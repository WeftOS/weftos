//! ADR-117 end to end, no hardware: the real daemon dispatch (`display.*`
//! over its Unix socket), the real certified-leaf listener and discovery,
//! an emulated edge pad (`WireLeaf`: signed journal, same `DisplayService`
//! as the firmware), the real display client, and the console host.
#![cfg(all(unix, feature = "mesh"))]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clawft_kernel::boot::Kernel;
use clawft_kernel::mesh_admit::AllowAll;
use clawft_kernel::mesh_leaf::LeafIngress;
use clawft_kernel::mesh_leaf_bind::LeafBindSpec;
use clawft_kernel::mesh_runtime::MeshRuntime;
use clawft_kernel::mesh_tcp::TcpTransport;
use clawft_kernel::MeshTransport;
use clawft_platform::NativePlatform;
use clawft_types::config::{AgentDefaults, AgentsConfig, Config, KernelConfig};
use ed25519_dalek::SigningKey;
use tokio::net::UnixListener;
use weftos_cos_console::host::{Host, HostConfig};
use weftos_display_client::scene::{
    px, CursorHint, HitShape, InputRegion, Layer, Node, NodeId, Primitive, Rgba, SceneOp, Style, Transform,
};
use weftos_display_client::wire_leaf::{Parent, WireLeaf, WireLeafConfig};
use weftos_display_client::{DisplayError, DisplayGrant, DisplaySession, DisplayTarget, DisplaySize, LeafEndpoint, ObjectEvent};
use weftos_leaf_types::link::LeafCertificate;

const WAIT: Duration = Duration::from_secs(15);

fn kernel_config() -> KernelConfig {
    KernelConfig {
        governance: Default::default(),
        profile: None,
        shared_services: Default::default(),
        enabled: true,
        max_processes: 64,
        health_check_interval_secs: 5,
        brand: "WeftOS".to_string(),
        cluster: None,
        chain: Some(clawft_types::config::ChainConfig::isolated_in(&tempfile::tempdir().unwrap().keep())),
        resource_tree: None,
        vector: None,
        spatial: None,
        profiles: None,
        pairing: None,
        mesh: None,
        anchor: None,
        ipc_tcp: None,
        llm: None,
        agent: None,
    }
}

fn config() -> Config {
    Config {
        agents: AgentsConfig {
            defaults: AgentDefaults {
                workspace: "~/.clawft/workspace".into(),
                model: "test/model".into(),
                max_tokens: 1024,
                temperature: 0.5,
                max_tool_iterations: 5,
                memory_window: 10,
            },
            ..AgentsConfig::default()
        },
        ..Config::default()
    }
}

struct World {
    _dir: tempfile::TempDir,
    socket: PathBuf,
    leaf_id: String,
    cert_cbor: Vec<u8>,
    seed: [u8; 32],
    tcp: std::net::SocketAddr,
    discovery: std::net::SocketAddr,
    _tasks: Vec<tokio::task::JoinHandle<()>>,
}

/// Kernel + mesh runtime with the certified-leaf listener + daemon socket.
async fn world(tag: u8) -> World {
    let dir = tempfile::tempdir().unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let kernel = Kernel::boot(config(), kernel_config(), Arc::new(NativePlatform::new())).await.expect("kernel boot");
    let router = kernel.a2a_router().clone();
    let kernel = Arc::new(tokio::sync::RwLock::new(kernel));

    // The enrolled leaf.
    let parent = SigningKey::from_bytes(&[tag; 32]);
    let machine = SigningKey::from_bytes(&[tag + 1; 32]);
    let leaf = SigningKey::from_bytes(&[tag + 2; 32]);
    let user_id = clawft_types::project::cert::key_id(&parent.verifying_key().to_bytes());
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let cert = LeafCertificate::issue(
        &parent,
        format!("user:{user_id}"),
        machine.verifying_key().to_bytes(),
        leaf.verifying_key().to_bytes(),
        1,
        now - 1,
        now + 3600,
        vec!["input.publish".into(), "push.subscribe".into()],
    )
    .unwrap();
    let leaf_id = cert.leaf_id();
    let ingress = LeafIngress::open(dir.path(), machine).unwrap();
    std::fs::write(
        dir.path().join("registry").join(format!("{leaf_id}.json")),
        serde_json::json!({"node_id": leaf_id, "cert": cert}).to_string(),
    )
    .unwrap();

    // Mesh runtime wired to the kernel router, with the leaf listeners on loopback.
    let mut rt = MeshRuntime::new("srv".into());
    rt.set_local_router(router.clone());
    let rt = Arc::new(rt);
    router.set_mesh_runtime(rt.clone());
    rt.set_leaf_ingress(Arc::new(ingress)).ok().unwrap();
    let tcp_port = {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        probe.local_addr().unwrap().port()
    };
    assert!(rt.set_leaf_bind_spec(LeafBindSpec::parse(&format!("127.0.0.1:{tcp_port}"), false).unwrap()));
    let main = TcpTransport.listen("127.0.0.1:0").await.unwrap();
    let serve = tokio::spawn(clawft_kernel::mesh_serve::serve_listener(
        Arc::clone(&rt),
        main,
        None,
        "tcp",
        "x",
        Arc::new(AllowAll),
    ));
    let bound = loop {
        if let Some(b) = rt.leaf_bound() {
            break b.clone();
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };

    // The daemon's RPC socket, served by the real connection handler.
    let socket = dir.path().join("kernel.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let (shutdown_tx, _rx) = tokio::sync::watch::channel(false);
    let accept = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(clawft_weave::daemon::handle_connection(stream, Arc::clone(&kernel), shutdown_tx.clone()));
        }
    });

    World {
        socket,
        leaf_id,
        cert_cbor: weftos_leaf_types::encode(&cert).unwrap(),
        seed: [tag + 2; 32],
        tcp: bound.tcp,
        discovery: bound.discovery,
        _dir: dir,
        _tasks: vec![serve, accept],
    }
}

impl World {
    fn pad(&self, parent: Parent) -> WireLeaf {
        WireLeaf::start(WireLeafConfig {
            seed: self.seed,
            cert_cbor: self.cert_cbor.clone(),
            parent,
            state_file: None,
            size: DisplaySize::PAD,
        })
        .expect("emulated pad")
    }

    fn target(&self) -> DisplayTarget {
        DisplayTarget::Leaf(LeafEndpoint {
            name: format!("weftos://srv/nodes/{}/services/display", self.leaf_id),
            leaf_id: self.leaf_id.clone(),
            display: 0,
            daemon: Some(self.socket.clone()),
        })
    }
}

fn button(id: u32, x: i32, y: i32, w: i32, h: i32) -> Node {
    let mut n = Node::new(NodeId::from_raw(id), Layer::Widget, Primitive::Rect { w: px(w), h: px(h), radius_q8: 0 });
    n.transform = Transform::translate(px(x), px(y));
    n.style = Style::filled(Rgba::opaque(0x22, 0x88, 0x44));
    n.input = Some(InputRegion { shape: HitShape::Aabb { w: px(w), h: px(h) }, cursor_hint: CursorHint::Pointer, capture: true });
    n
}

/// Run blocking client work off the async runtime that serves the daemon.
async fn client<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    tokio::task::spawn_blocking(f).await.unwrap()
}

fn poll_until(s: &mut DisplaySession, mut want: impl FnMut(&ObjectEvent) -> bool) -> Option<ObjectEvent> {
    let end = std::time::Instant::now() + WAIT;
    while std::time::Instant::now() < end {
        if let Some(ev) = s.poll_event() {
            if want(&ev) {
                return Some(ev);
            }
        } else {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    None
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scene_ops_reach_the_pad_and_taps_and_ptt_come_back() {
    let w = world(81).await;
    let pad = w.pad(Parent::Discover(w.discovery)); // exercises signed discovery too
    assert!(pad.wait_for(WAIT, |p| p.is_subscribed()), "the pad never subscribed: {:?}", pad.log());
    let sim = pad.sim();
    let target = w.target();
    let (socket, leaf_id) = (w.socket.clone(), w.leaf_id.clone());
    let pad = Arc::new(pad);
    let pad2 = Arc::clone(&pad);

    client(move || {
        let mut s = DisplaySession::open(target, DisplayGrant::dev("e2e-app")).expect("open a session on the pad");
        assert_eq!((s.size().width, s.size().height), (800, 480));
        assert!(s.capabilities().input.touch);
        assert_eq!(s.capabilities().input.buttons, vec!["ptt".to_string()]);
        assert!(pad2.session_active() && pad2.session_subject().as_deref() == Some("e2e-app"));
        let v = rpc(&socket, "display.status", serde_json::json!({"leaf_id": leaf_id}), "read");
        assert_eq!(v["result"]["connected"], true, "{v}");
        assert_eq!(v["result"]["session"], "e2e-app", "{v}");

        // Scene ops land on the pad's scene.
        s.apply(&[SceneOp::Clear, SceneOp::Insert(button(7, 100, 100, 120, 60))]).expect("apply");
        assert!(pad2.wait_for(WAIT, |p| p.sim().has_node(NodeId::from_raw(7))), "node 7 never reached the pad");
        assert_eq!(sim.pixel(150, 120), (0x22, 0x88, 0x44), "and it was drawn");

        // A tap on the pad comes back as an object event.
        pad2.tap(150, 120);
        let ev = poll_until(&mut s, |e| matches!(e, ObjectEvent::Tap { .. })).expect("a Tap event");
        assert_eq!(ev, ObjectEvent::Tap { node: NodeId::from_raw(7) });

        // A drag too, with the hit-test done on the pad.
        pad2.touch_down(150, 120);
        pad2.touch_move(160, 130);
        pad2.touch_up(160, 130);
        let ev = poll_until(&mut s, |e| matches!(e, ObjectEvent::Drag { .. })).expect("a Drag event");
        assert!(matches!(ev, ObjectEvent::Drag { node, dx: 10, dy: 10 } if node == NodeId::from_raw(7)), "{ev:?}");

        // The button: ptt.down / ptt.up.
        pad2.ptt(true);
        pad2.ptt(false);
        let down = poll_until(&mut s, |e| matches!(e, ObjectEvent::Sensor { .. })).expect("ptt.down");
        assert_eq!(down, ObjectEvent::ptt(true));
        let up = poll_until(&mut s, |e| matches!(e, ObjectEvent::Sensor { .. })).expect("ptt.up");
        assert_eq!(up, ObjectEvent::ptt(false));

        // Closing clears the session's objects on the pad.
        s.close().unwrap();
        assert!(pad2.wait_for(WAIT, |p| !p.session_active() && p.sim().node_count() == 0), "the pad kept the session's nodes");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_app_is_refused_and_a_revoked_one_is_cut_off() {
    let w = world(91).await;
    let pad = w.pad(Parent::Direct(w.tcp));
    assert!(pad.wait_for(WAIT, |p| p.is_subscribed()), "{:?}", pad.log());
    let (t1, t2, socket) = (w.target(), w.target(), w.socket.clone());
    let pad = Arc::new(pad);
    let pad2 = Arc::clone(&pad);

    client(move || {
        let mut first = DisplaySession::open(t1, DisplayGrant::dev("first")).unwrap();
        match DisplaySession::open(t2, DisplayGrant::dev("second")) {
            Err(DisplayError::Backend(why)) => assert!(why.contains("display busy: held by first"), "{why}"),
            other => panic!("expected the leaf's refusal, got {:?}", other.map(|_| ())),
        }

        // An operator revokes the subject (Admin): its next op is refused
        // and the pad clears its objects.
        first.apply(&[SceneOp::Insert(button(3, 10, 10, 50, 50))]).unwrap();
        assert!(pad2.wait_for(WAIT, |p| p.sim().has_node(NodeId::from_raw(3))));
        rpc_admin(&socket, "display.revoke", serde_json::json!({"subject": "first"}));
        let err = first.apply(&[SceneOp::Insert(button(4, 80, 10, 50, 50))]).unwrap_err();
        assert!(matches!(err, DisplayError::Denied(_)), "{err:?}");
        assert!(pad2.wait_for(WAIT, |p| !p.session_active() && p.sim().node_count() == 0), "the pad still holds the revoked session");
    })
    .await;
}

fn rpc(socket: &std::path::Path, method: &str, params: serde_json::Value, auth: &str) -> serde_json::Value {
    use std::io::{BufRead, Write};
    let mut s = std::os::unix::net::UnixStream::connect(socket).unwrap();
    let req = serde_json::json!({"id": "t", "proto": 1, "method": method, "params": params, "auth": auth});
    s.write_all(format!("{req}\n").as_bytes()).unwrap();
    let mut line = String::new();
    std::io::BufReader::new(s).read_line(&mut line).unwrap();
    serde_json::from_str(line.trim()).unwrap()
}

fn rpc_admin(socket: &std::path::Path, method: &str, params: serde_json::Value) {
    let v = rpc(socket, method, params, "admin");
    assert_eq!(v["ok"], true, "{v}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_write_caller_cannot_revoke_and_an_unconnected_leaf_is_unreachable() {
    let w = world(101).await;
    // No pad running: nothing subscribed to the push topic.
    let target = w.target();
    let socket = w.socket.clone();
    let leaf_id = w.leaf_id.clone();
    client(move || {
        match DisplaySession::open(target, DisplayGrant::dev("a")) {
            Err(DisplayError::Backend(why)) => assert!(why.contains("not connected"), "{why}"),
            other => panic!("expected unreachable, got {:?}", other.map(|_| ())),
        }
        // `display.revoke` is an Admin act: Write is refused.
        let v = rpc(&socket, "display.revoke", serde_json::json!({"subject": "x"}), "write");
        assert_eq!(v["ok"], false);
        assert!(v["error"].as_str().unwrap().contains("permission denied"), "{v}");
        // `display.status` is a plain read, and says the leaf is not connected.
        let v = rpc(&socket, "display.status", serde_json::json!({"leaf_id": leaf_id}), "read");
        assert_eq!((v["ok"].as_bool(), v["result"]["connected"].as_bool()), (Some(true), Some(false)), "{v}");
        assert!(v["result"]["session"].is_null());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_console_host_drives_the_pad_and_ptt_reaches_it() {
    let w = world(111).await;
    let pad = w.pad(Parent::Direct(w.tcp));
    assert!(pad.wait_for(WAIT, |p| p.is_subscribed()), "{:?}", pad.log());
    let target = w.target();
    let pad = Arc::new(pad);
    let pad2 = Arc::clone(&pad);

    client(move || {
        let mut session = DisplaySession::open(target, DisplayGrant::dev(weftos_cos_console::frame::SUBJECT)).unwrap();
        // An agent that is not there: the host still draws and shows Blocked.
        let cfg = HostConfig { agent: "http://127.0.0.1:9".into(), typing: false, ..HostConfig::default() };
        let mut host = Host::open(&mut session, cfg).expect("console on the pad");
        let mut now = 0u32;
        let mut step = |host: &mut Host<'_>| {
            now += 33;
            host.step(now).expect("step");
        };
        step(&mut host);

        // The console scene is on the pad: chrome, the status strip.
        assert!(pad2.wait_for(WAIT, |p| p.sim().node_count() > 20), "console nodes: {}", pad2.sim().node_count());
        // The status strip's push-to-talk dot (node 204) is hidden until the
        // button is held.
        let ptt_dot = |p: &WireLeaf| {
            p.sim().snapshot().nodes.into_iter().find(|n| n.id == NodeId::from_raw(204)).map(|n| n.style.visible)
        };
        assert_eq!(ptt_dot(&pad2), Some(false), "the status strip's ptt dot is on the pad, unlit");

        // Hold the pad's button: the host sees ptt.down and the strip lights.
        pad2.ptt(true);
        let end = std::time::Instant::now() + WAIT;
        let mut lit = false;
        while std::time::Instant::now() < end && !lit {
            step(&mut host);
            lit = ptt_dot(&pad2) == Some(true);
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(lit, "ptt.down never reached the console host");
        pad2.ptt(false);
        let end = std::time::Instant::now() + WAIT;
        let mut dark = false;
        while std::time::Instant::now() < end && !dark {
            step(&mut host);
            dark = ptt_dot(&pad2) == Some(false);
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(dark, "ptt.up never reached the console host");
    })
    .await;
}
