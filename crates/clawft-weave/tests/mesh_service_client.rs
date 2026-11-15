//! P3-U: the user daemon as a client of the machine mesh service, driven
//! against mesh-local's loopback test server (no root, no fixed ports, per-test
//! tempdirs). The real service (package S) is not involved.
#![cfg(all(unix, feature = "mesh"))]

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use clawft_kernel::a2a::RemoteForwarder;
use clawft_kernel::error::KernelResult;
use clawft_kernel::gate::{GateBackend, GovernanceGate};
use clawft_kernel::governance::{GovernanceBranch, GovernanceRule, RuleSeverity};
use clawft_kernel::ipc::{KernelMessage, MessagePayload, MessageTarget};
use clawft_kernel::mesh_delivery::{LocalDelivery, PeerCtx};
use clawft_kernel::mesh_ipc::Scope as KScope;
use clawft_mesh_local::client::RegisterParams;
use clawft_mesh_local::proto::{
    Deliver, Frame, Message, PeerInfo, SERVICE_ID_FLAG, Scope, VerdictRequest, VerdictSubject,
};
use clawft_mesh_local::testing::{TestServer, TestServerConfig};
use clawft_mesh_local::{ClientConfig, InjectedPeer, node_id_from_pubkey};
use clawft_types::config::{MeshConfig, MeshServicePolicy};
use clawft_weave::mesh_local_chain::{ChainQueue, ChainSink, KIND_ANCHOR, KIND_BOUND};
use clawft_weave::mesh_local_glue::{
    LinkDeps, Resolved, ServiceEndpoint, ServiceLink, Timings, build_endpoint_in, resolve, resolve_with, spawn,
};
use clawft_weave::mesh_state::MeshStateCell;
use ed25519_dalek::SigningKey;
use serde_json::Value;

const SERVICE_UID: u32 = 4242;
const MY_UID: u32 = 501;

fn key(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}

fn mesh_cfg(policy: MeshServicePolicy) -> MeshConfig {
    MeshConfig { enabled: true, service: policy, ..MeshConfig::default() }
}

fn server_at(path: &Path, tweak: impl FnOnce(&mut TestServerConfig)) -> TestServer {
    let mut sc = TestServerConfig::new(key(100), Arc::new(InjectedPeer::uid(MY_UID)));
    tweak(&mut sc);
    TestServer::start(path, sc).unwrap()
}

fn endpoint(sock: &Path, record_from: &TestServer, pin: &Path) -> ServiceEndpoint {
    let mut client = ClientConfig::new(sock, record_from.service_record(SERVICE_UID));
    client.server_peer = Some(Arc::new(InjectedPeer::uid(SERVICE_UID)));
    client.own_uid = Some(MY_UID);
    client.machine_pin = Some(pin.to_path_buf());
    client.deadline = Duration::from_secs(3);
    client.build_sha = "daemon-sha".into();
    let user_key = key(1);
    let user_id = node_id_from_pubkey(&user_key.verifying_key().to_bytes());
    ServiceEndpoint {
        client,
        user_key,
        register: RegisterParams {
            projects: vec![],
            topic_prefixes: vec![format!("user/{user_id}/")],
            capabilities: vec!["a2a".into()],
            version: "test".into(),
            accept_from: vec![],
        },
    }
}

fn user_id() -> String {
    node_id_from_pubkey(&key(1).verifying_key().to_bytes())
}

#[derive(Default)]
struct Recorder {
    got: Mutex<Vec<(String, Option<KScope>, KernelMessage)>>,
}

#[async_trait]
impl LocalDelivery for Recorder {
    async fn deliver(&self, from: &PeerCtx, scope: Option<&KScope>, msg: KernelMessage) -> KernelResult<()> {
        self.got.lock().unwrap().push((from.peer_id.clone(), scope.cloned(), msg));
        Ok(())
    }
}

struct GatedSink {
    open: Arc<AtomicBool>,
    got: Arc<Mutex<Vec<(String, Value)>>>,
}

impl ChainSink for GatedSink {
    fn append(&self, kind: &str, payload: Value) -> Result<(), String> {
        if !self.open.load(Ordering::SeqCst) {
            return Err("chain unavailable".into());
        }
        self.got.lock().unwrap().push((kind.to_owned(), payload));
        Ok(())
    }
}

fn fast() -> Timings {
    Timings {
        anchor_every: Duration::from_millis(100),
        backoff: (Duration::from_millis(20), Duration::from_millis(100)),
        deadline: Duration::from_secs(2),
    }
}

async fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    for _ in 0..400 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for: {what}");
}

fn deps(
    delivery: Arc<dyn LocalDelivery>,
    gate: Option<Arc<dyn GateBackend>>,
    chain: Arc<ChainQueue>,
    state: Arc<MeshStateCell>,
) -> LinkDeps {
    LinkDeps { delivery, gate, chain, project_env: None, state, timings: fast() }
}

type Got = Arc<Mutex<Vec<(String, Value)>>>;

fn open_chain() -> (Arc<ChainQueue>, Got) {
    let got = Arc::new(Mutex::new(Vec::new()));
    let q = ChainQueue::new(GatedSink { open: Arc::new(AtomicBool::new(true)), got: got.clone() });
    (Arc::new(q), got)
}

async fn linked(
    dir: &Path,
    tweak: impl FnOnce(&mut TestServerConfig),
) -> (TestServer, Box<ServiceLink>) {
    let sock = dir.join("mesh.sock");
    let server = server_at(&sock, tweak);
    let ep = endpoint(&sock, &server, &dir.join("mesh/machine.pub"));
    match resolve(&mesh_cfg(MeshServicePolicy::Auto), Ok(Some(ep))).await.unwrap() {
        Resolved::Service(l) => (server, l),
        _ => panic!("expected service mode"),
    }
}

fn kernel_msg(node_id: &str) -> KernelMessage {
    KernelMessage::new(
        0,
        MessageTarget::RemoteNode {
            node_id: node_id.into(),
            target: Box::new(MessageTarget::Service("health".into())),
        },
        MessagePayload::Text("hello".into()),
    )
}

#[tokio::test]
async fn service_mode_boot_registers_and_shows_the_handshake() {
    let dir = tempfile::tempdir().unwrap();
    let (server, link) = linked(dir.path(), |_| {}).await;
    // The daemon's node id is the service's, with no signing key held here.
    let id = link.identity().unwrap();
    assert!(id.is_service());
    assert_eq!(id.public_key(), server.machine_pubkey());
    assert_eq!(id.node_id, node_id_from_pubkey(&server.machine_pubkey()));
    assert!(id.signing_key().is_err());
    assert_eq!(server.registrations().len(), 1, "registered at resolve time");
    assert_eq!(server.registrations()[0].1.build_sha, "daemon-sha");

    let state = Arc::new(MeshStateCell::new());
    let (chain, _) = open_chain();
    let h = spawn(link, deps(Arc::new(Recorder::default()), None, chain, state.clone()));
    let s = state.get().expect("state published");
    assert_eq!(s.mode, "service");
    assert_eq!(s.state.as_deref(), Some("connected"));
    assert_eq!(s.service_node_id.as_deref(), Some(id.node_id.as_str()));
    assert_eq!(s.proto, Some(2));
    assert!(s.cert_serial.is_some() && s.cert_not_after.is_some());
    assert_eq!(s.summary(), "service (connected)");
    assert!(state.is_service());
    h.shutdown().await;
}

#[tokio::test]
async fn collapsed_when_no_socket_or_nobody_listening() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("mesh.sock");
    let server = server_at(&sock, |_| {});
    let ep = endpoint(&sock, &server, &dir.path().join("pin"));
    drop(server); // the socket path is gone: connect fails with NotFound
    let r = resolve(&mesh_cfg(MeshServicePolicy::Auto), Ok(Some(ep))).await.unwrap();
    assert!(matches!(r, Resolved::Collapsed));
    // No socket at all.
    let r = resolve(&mesh_cfg(MeshServicePolicy::Auto), Ok(None)).await.unwrap();
    assert!(matches!(r, Resolved::Collapsed));
}

#[tokio::test]
async fn off_never_probes_even_with_a_live_service() {
    let dir = tempfile::tempdir().unwrap();
    let (server, link) = linked(dir.path(), |_| {}).await;
    drop(link);
    let before = server.registrations().len();
    let ep = endpoint(&dir.path().join("mesh.sock"), &server, &dir.path().join("pin"));
    let off = mesh_cfg(MeshServicePolicy::Off);
    assert!(matches!(resolve(&off, Ok(Some(ep))).await.unwrap(), Resolved::Collapsed));
    assert_eq!(server.registrations().len(), before, "no connection was made");
}

#[tokio::test]
async fn required_without_a_service_fails_boot_with_the_reason() {
    let e = resolve(&mesh_cfg(MeshServicePolicy::Required), Ok(None)).await.err().unwrap();
    assert!(e.contains("required") && e.contains("no machine mesh service"), "{e}");
}

#[tokio::test]
async fn machine_pin_mismatch_is_fatal_under_auto_and_required() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("mesh.sock");
    let server = server_at(&sock, |_| {});
    let pin = dir.path().join("mesh/machine.pub");
    std::fs::create_dir_all(pin.parent().unwrap()).unwrap();
    // A pin for some other machine key.
    let other: String = key(9).verifying_key().to_bytes().iter().map(|b| format!("{b:02x}")).collect();
    std::fs::write(&pin, other).unwrap();
    for policy in [MeshServicePolicy::Auto, MeshServicePolicy::Required] {
        let ep = endpoint(&sock, &server, &pin);
        let e = resolve(&mesh_cfg(policy), Ok(Some(ep))).await.err().expect("must refuse");
        assert!(e.contains("machine_key_changed"), "{e}");
    }
}

#[tokio::test]
async fn a_server_running_as_the_wrong_uid_is_refused_not_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("mesh.sock");
    let server = server_at(&sock, |_| {});
    let mut ep = endpoint(&sock, &server, &dir.path().join("pin"));
    ep.client.server_peer = Some(Arc::new(InjectedPeer::uid(777)));
    let e = resolve(&mesh_cfg(MeshServicePolicy::Auto), Ok(Some(ep))).await.err().unwrap();
    assert!(e.contains("refusing"), "{e}");
    // An unusable service.json (or user key) surfaces as a refusal too.
    let e = resolve(&mesh_cfg(MeshServicePolicy::Auto), Err("service.json unreadable".into()))
        .await
        .err()
        .unwrap();
    assert!(e.contains("service.json unreadable"), "{e}");
}

#[tokio::test]
async fn service_restart_reregisters_and_refreshes_the_cert() {
    let dir = tempfile::tempdir().unwrap();
    let (server, link) = linked(dir.path(), |c| c.cert_ttl_s = 3).await;
    let state = Arc::new(MeshStateCell::new());
    let (chain, _) = open_chain();
    let h = spawn(link, deps(Arc::new(Recorder::default()), None, chain, state.clone()));
    let first = state.get().unwrap().cert_serial.unwrap();
    wait_until("cert renewal", || state.get().unwrap().cert_serial.unwrap() > first).await;

    let sock = dir.path().join("mesh.sock");
    drop(server);
    wait_until("reconnecting", || state.get().unwrap().state.as_deref() == Some("reconnecting")).await;
    // Remote traffic fails fast while the link is down; the daemon is fine.
    let e = h.forwarder.forward(&node_id_from_pubkey(&[7; 32]), kernel_msg("x")).await.unwrap_err();
    assert!(e.to_string().contains("reconnecting"), "{e}");

    let server2 = server_at(&sock, |c| c.cert_ttl_s = 3);
    wait_until("reconnected", || state.get().unwrap().state.as_deref() == Some("connected")).await;
    assert_eq!(server2.registrations().len(), 1, "re-registered after the restart");
    assert_eq!(server2.registrations()[0].1.user_pubkey, key(1).verifying_key().to_bytes());
    let serial = state.get().unwrap().cert_serial.unwrap();
    wait_until("cert refreshed on the new service", || {
        state.get().unwrap().cert_serial.unwrap() > serial
    })
    .await;
    h.shutdown().await;
}

#[tokio::test]
async fn verdict_round_trip_through_a_real_gate_rule() {
    let dir = tempfile::tempdir().unwrap();
    let (server, link) = linked(dir.path(), |_| {}).await;
    // Deny cluster.join outright; everything else is permitted.
    let gate: Arc<dyn GateBackend> = Arc::new(GovernanceGate::new(0.5, false).add_rule(GovernanceRule {
        id: "no-cluster-join".into(),
        description: "refuse every cluster join".into(),
        branch: GovernanceBranch::Judicial,
        severity: RuleSeverity::Blocking,
        active: true,
        reference_url: None,
        sop_category: None,
        rule_type: Default::default(),
        action_selector: Some("cluster.join".into()),
        tool_selector: None,
        force_on_match: true,
        principal_selector: None,
    }));
    let (chain, _) = open_chain();
    let h = spawn(link, deps(Arc::new(Recorder::default()), Some(gate), chain, Arc::new(MeshStateCell::new())));

    let peer = PeerInfo {
        node_id: node_id_from_pubkey(&[5; 32]),
        pubkey: "k".into(),
        platform: "linux".into(),
        capabilities: vec![],
        genesis_hash: String::new(),
        chain_seq: 3,
    };
    let ask = |n: u64, subject| {
        Frame::with_id(
            SERVICE_ID_FLAG | n,
            Message::VerdictRequest(VerdictRequest { subject, peer: peer.clone(), topic: None }),
        )
    };
    assert_eq!(server.push(ask(1, VerdictSubject::ClusterJoin)), 1);
    assert_eq!(server.push(ask(2, VerdictSubject::PeerAdmit)), 1);
    wait_until("both verdicts", || server.verdict_replies().len() == 2).await;
    let replies = server.verdict_replies();
    let by_id = |n: u64| replies.iter().find(|r| r.0 == SERVICE_ID_FLAG | n).unwrap().clone();
    let (_, allow, reason) = by_id(1);
    assert!(!allow, "the rule denies cluster.join");
    assert!(!reason.is_empty());
    assert!(by_id(2).1, "peer.admit has no matching rule");
    h.shutdown().await;
}

#[tokio::test]
async fn without_a_gate_every_verdict_is_denied() {
    let dir = tempfile::tempdir().unwrap();
    let (server, link) = linked(dir.path(), |_| {}).await;
    let (chain, _) = open_chain();
    let h = spawn(link, deps(Arc::new(Recorder::default()), None, chain, Arc::new(MeshStateCell::new())));
    let peer = PeerInfo {
        node_id: node_id_from_pubkey(&[5; 32]),
        pubkey: "k".into(),
        platform: String::new(),
        capabilities: vec![],
        genesis_hash: String::new(),
        chain_seq: 0,
    };
    server.push(Frame::with_id(
        SERVICE_ID_FLAG | 9,
        Message::VerdictRequest(VerdictRequest { subject: VerdictSubject::PeerAdmit, peer, topic: None }),
    ));
    wait_until("verdict", || !server.verdict_replies().is_empty()).await;
    assert!(!server.verdict_replies()[0].1);
    h.shutdown().await;
}

#[tokio::test]
async fn anchors_survive_an_outage_and_flush_after_reconnect() {
    let dir = tempfile::tempdir().unwrap();
    let (server, link) = linked(dir.path(), |_| {}).await;
    server.set_journal_head(5, "aa");
    let open = Arc::new(AtomicBool::new(false));
    let got = Arc::new(Mutex::new(Vec::new()));
    let chain = Arc::new(ChainQueue::new(GatedSink { open: open.clone(), got: got.clone() }));
    let state = Arc::new(MeshStateCell::new());
    let h = spawn(link, deps(Arc::new(Recorder::default()), None, chain.clone(), state.clone()));
    // The chain refuses everything: the binding record and anchor 5 queue up.
    wait_until("anchor 5 queued", || chain.pending() >= 2).await;
    assert!(got.lock().unwrap().is_empty());

    // The service goes away; then the chain recovers during the outage.
    let sock = dir.path().join("mesh.sock");
    drop(server);
    wait_until("reconnecting", || state.get().unwrap().state.as_deref() == Some("reconnecting")).await;
    open.store(true, Ordering::SeqCst);
    wait_until("queued events flushed during the outage", || chain.pending() == 0).await;

    // After the restart the new head is anchored too, after the old one.
    let server2 = server_at(&sock, |_| {});
    server2.set_journal_head(6, "bb");
    wait_until("anchor 6", || {
        got.lock().unwrap().iter().any(|(k, p)| k == KIND_ANCHOR && p["seq"] == 6)
    })
    .await;
    let events = got.lock().unwrap().clone();
    let seqs: Vec<u64> = events
        .iter()
        .filter(|(k, _)| k == KIND_ANCHOR)
        .map(|(_, p)| p["seq"].as_u64().unwrap())
        .collect();
    assert_eq!(seqs, vec![5, 6], "no anchor lost, order kept");
    assert!(
        events.iter().filter(|(k, _)| k == KIND_BOUND).count() >= 2,
        "a binding record per (re)connection"
    );
    assert!(events.iter().all(|(_, p)| p["node_id"].is_string() || p["cert_serial"].is_number()));
    h.shutdown().await;
}

#[tokio::test]
async fn inbound_delivery_reaches_the_router_and_other_users_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (server, link) = linked(dir.path(), |_| {}).await;
    let rec = Arc::new(Recorder::default());
    let (chain, _) = open_chain();
    let h = spawn(link, deps(rec.clone(), None, chain, Arc::new(MeshStateCell::new())));
    let deliver = |user: &str, text: &str| {
        let mut m = kernel_msg("x");
        m.payload = MessagePayload::Text(text.into());
        Frame::new(Message::Deliver(Deliver {
            source_node: node_id_from_pubkey(&[8; 32]),
            source_cert: None,
            scope: Scope { user_id: user.into(), project_id: None },
            envelope_id: "e".into(),
            message: serde_json::to_value(&m).unwrap(),
            origin: None,
        }))
    };
    server.push(deliver(&"f".repeat(32), "not for us"));
    server.push(deliver(&user_id(), "for us"));
    wait_until("delivery", || !rec.got.lock().unwrap().is_empty()).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    {
        let got = rec.got.lock().unwrap();
        assert_eq!(
            got.len(),
            1,
            "the other user's frame never reaches the router"
        );
        assert_eq!(got[0].0, node_id_from_pubkey(&[8; 32]));
        assert_eq!(got[0].1.as_ref().unwrap().user_id, user_id());
        assert!(matches!(&got[0].2.payload, MessagePayload::Text(t) if t == "for us"));
    }
    h.shutdown().await;
}

#[tokio::test]
async fn remote_node_messages_are_forwarded_as_send() {
    let dir = tempfile::tempdir().unwrap();
    let (server, link) = linked(dir.path(), |_| {}).await;
    let (chain, _) = open_chain();
    let h = spawn(link, deps(Arc::new(Recorder::default()), None, chain, Arc::new(MeshStateCell::new())));
    let remote = node_id_from_pubkey(&[7; 32]);
    h.forwarder.forward(&remote, kernel_msg(&remote)).await.unwrap();
    let sent = server.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0, format!("weft://{remote}/_/_"));
    assert_eq!(sent[0].1["payload"]["Text"], "hello");
    // A malformed node id is an error, not a send.
    assert!(h.forwarder.forward("not-a-node", kernel_msg("x")).await.is_err());
    assert_eq!(server.sent().len(), 1);
    h.shutdown().await;
}

/// A listener bound then dropped leaves a socket file nobody serves.
fn stale_socket(path: &Path) {
    drop(std::os::unix::net::UnixListener::bind(path).unwrap());
}

#[tokio::test]
async fn a_refusing_socket_is_retried_then_collapses_loudly() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("mesh.sock");
    let proto = server_at(&dir.path().join("proto.sock"), |_| {});
    stale_socket(&sock);
    let ep = endpoint(&sock, &proto, &dir.path().join("pin"));
    let t = std::time::Instant::now();
    let r = resolve_with(
        &mesh_cfg(MeshServicePolicy::Auto),
        Ok(Some(ep)),
        &[Duration::from_millis(60), Duration::from_millis(60)],
    )
    .await
    .unwrap();
    assert!(matches!(r, Resolved::Collapsed));
    assert!(t.elapsed() >= Duration::from_millis(120), "both retries were taken");
    // Under `required` the same situation is a boot failure.
    let ep = endpoint(&sock, &proto, &dir.path().join("pin"));
    assert!(resolve_with(&mesh_cfg(MeshServicePolicy::Required), Ok(Some(ep)), &[]).await.is_err());
}

#[tokio::test]
async fn a_service_that_comes_up_during_the_retries_is_used() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("mesh.sock");
    let proto = server_at(&dir.path().join("proto.sock"), |_| {});
    stale_socket(&sock);
    let ep = endpoint(&sock, &proto, &dir.path().join("pin"));
    let late = sock.clone();
    let starter = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        std::fs::remove_file(&late).unwrap();
        server_at(&late, |_| {})
    });
    let retries = [Duration::from_millis(100); 8];
    let r = resolve_with(&mesh_cfg(MeshServicePolicy::Auto), Ok(Some(ep)), &retries).await.unwrap();
    assert!(matches!(r, Resolved::Service(_)));
    drop(starter.await.unwrap());
}

#[test]
fn an_uninspectable_socket_path_is_a_refusal_not_absence() {
    use std::os::unix::fs::PermissionsExt;
    if nix::unistd::geteuid().is_root() {
        return; // root can inspect anything
    }
    let dir = tempfile::tempdir().unwrap();
    let locked = dir.path().join("run");
    std::fs::create_dir(&locked).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let mut cfg = mesh_cfg(MeshServicePolicy::Auto);
    cfg.service_socket = Some(locked.join("mesh.sock").display().to_string());
    let r = build_endpoint_in(&cfg, dir.path(), "sha", dir.path());
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    let e = r.err().expect("EACCES must not read as 'no service'");
    assert!(e.contains("mesh.sock") && e.contains("group"), "{e}");
}

#[tokio::test]
async fn service_json_owned_by_someone_else_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("mesh.sock");
    let proto = server_at(&dir.path().join("proto.sock"), |_| {});
    let _l = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    // The file is owned by this uid, but the record names another service uid.
    let record = proto.service_record(nix::unistd::geteuid().as_raw() + 1);
    std::fs::write(dir.path().join("service.json"), serde_json::to_vec(&record).unwrap()).unwrap();
    let mut cfg = mesh_cfg(MeshServicePolicy::Auto);
    cfg.service_socket = Some(sock.display().to_string());
    let e = build_endpoint_in(&cfg, dir.path(), "sha", dir.path()).err().expect("refused");
    assert!(e.contains("owned by uid"), "{e}");
}

#[tokio::test]
async fn a_pending_legacy_chain_blocks_service_mode_instead_of_minting_a_key() {
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let sock = dir.path().join("mesh.sock");
    let proto = server_at(&dir.path().join("proto.sock"), |_| {});
    let _l = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    let me = nix::unistd::geteuid().as_raw();
    std::fs::write(dir.path().join("service.json"), serde_json::to_vec(&proto.service_record(me)).unwrap())
        .unwrap();
    std::fs::create_dir_all(home.path().join(".clawft")).unwrap();
    std::fs::write(home.path().join(".clawft/chain.json"), b"{}").unwrap();
    let mut cfg = mesh_cfg(MeshServicePolicy::Auto);
    cfg.service_socket = Some(sock.display().to_string());
    let e = build_endpoint_in(&cfg, home.path(), "sha", dir.path()).err().expect("refused");
    assert!(e.contains("weaver migrate user-chain"), "{e}");
    assert!(!home.path().join(".weftos/user.key").exists());
}

struct SlowInbox;

#[async_trait]
impl LocalDelivery for SlowInbox {
    async fn deliver(&self, _: &PeerCtx, _: Option<&KScope>, _: KernelMessage) -> KernelResult<()> {
        tokio::time::sleep(Duration::from_secs(3)).await;
        Ok(())
    }
}

#[tokio::test]
async fn a_slow_inbox_does_not_hold_up_verdicts_or_sends() {
    let dir = tempfile::tempdir().unwrap();
    let (server, link) = linked(dir.path(), |_| {}).await;
    let (chain, _) = open_chain();
    let h = spawn(link, deps(Arc::new(SlowInbox), None, chain, Arc::new(MeshStateCell::new())));
    let mut m = kernel_msg("x");
    m.payload = MessagePayload::Text("slow".into());
    for _ in 0..3 {
        server.push(Frame::new(Message::Deliver(Deliver {
            source_node: node_id_from_pubkey(&[8; 32]),
            source_cert: None,
            scope: Scope { user_id: user_id(), project_id: None },
            envelope_id: "e".into(),
            message: serde_json::to_value(&m).unwrap(),
            origin: None,
        })));
    }
    let peer = PeerInfo {
        node_id: node_id_from_pubkey(&[5; 32]),
        pubkey: "k".into(),
        platform: String::new(),
        capabilities: vec![],
        genesis_hash: String::new(),
        chain_seq: 0,
    };
    server.push(Frame::with_id(
        SERVICE_ID_FLAG | 3,
        Message::VerdictRequest(VerdictRequest { subject: VerdictSubject::PeerAdmit, peer, topic: None }),
    ));
    let t = std::time::Instant::now();
    wait_until("verdict answered while deliveries are stuck", || !server.verdict_replies().is_empty()).await;
    let remote = node_id_from_pubkey(&[7; 32]);
    h.forwarder.forward(&remote, kernel_msg(&remote)).await.unwrap();
    assert!(t.elapsed() < Duration::from_secs(2), "took {:?}", t.elapsed());
    h.shutdown().await;
}
