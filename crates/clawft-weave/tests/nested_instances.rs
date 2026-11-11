//! D10 tests. Source-only until the lead releases the compiler lane.
#![cfg(all(unix, feature = "exochain"))]
use clawft_kernel::parent_policy::export_rules;
use clawft_types::config::{
    Config,
    nested::NestedRegistration,
    overlay::{Limits, OverlayFile},
};
use clawft_weave::{
    nested_boot::{BootContract, consume_generation},
    nested_supervisor::NestedSupervisor,
};
use ed25519_dalek::SigningKey;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const ID: &str = "01JB8Z3Q0V6X9KQ4M2N7T5R1WD";
/// The test's private root. On drop (also while unwinding from a panic) it
/// SIGKILLs any process whose working directory lies under it, so a failed test
/// leaves no nested daemon or project kernel behind. Only processes whose cwd
/// is under this root are touched.
struct Root(tempfile::TempDir);
impl Root {
    fn path(&self) -> &Path {
        self.0.path()
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let Ok(root) = self.0.path().canonicalize() else { return };
        let Ok(out) = std::process::Command::new("lsof")
            .args(["-a", "-d", "cwd", "-F", "pn"])
            .output()
        else {
            return;
        };
        let me = std::process::id();
        let mut pid = None;
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            if let Some(p) = line.strip_prefix('p') {
                pid = p.parse::<u32>().ok();
            } else if let (Some(n), Some(pid)) = (line.strip_prefix('n'), pid)
                && pid != me
                && Path::new(n).starts_with(&root)
            {
                unsafe { libc::kill(pid as i32, libc::SIGKILL) };
            }
        }
    }
}

fn dir() -> Root {
    // No live HOME and no /tmp. For real daemon tests choose a short docs path
    // with D10_TEST_ROOT (macOS has a 104-byte Unix socket path limit).
    let root = std::env::var_os("D10_TEST_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/test-runs/d10");
            // Sockets nest ~45 bytes below the root (`n…/<16 hex>/h/r/kernel.sock`). A deep
            // checkout (a worktree) overflows the ~104-byte SUN_LEN, and so does the macOS
            // per-user temp dir, so fall back to a short cache dir. Never the live ~/.weftos.
            if repo.as_os_str().len() + 50 > 100 {
                PathBuf::from(std::env::var_os("HOME").expect("HOME")).join(".cache/wd10")
            } else {
                repo
            }
        });
    std::fs::create_dir_all(&root).unwrap();
    Root(
        tempfile::Builder::new()
            .prefix("n")
            .tempdir_in(root)
            .unwrap(),
    )
}
fn master() -> SigningKey {
    SigningKey::from_bytes(&[42; 32])
}
fn policy(version: u64) -> clawft_kernel::parent_policy::ParentPolicy {
    export_rules(
        Vec::new(),
        0.7,
        false,
        &Limits {
            max_processes: Some(128),
            spawn_budget: Some(4),
            ..Default::default()
        },
        &master(),
        version,
        chrono::Utc::now(),
    )
    .unwrap()
}
fn contract(sup: &NestedSupervisor) -> BootContract {
    serde_json::from_slice(&std::fs::read(sup.instance_dir(ID).join("boot.json")).unwrap()).unwrap()
}
fn sup(root: &Path, exe: &Path) -> NestedSupervisor {
    NestedSupervisor::new(root.into(), exe.into(), master(), true, 0).unwrap()
}

#[tokio::test]
async fn nested_private_state_env_cap_signature_and_replay() {
    let root = dir();
    assert!(
        NestedSupervisor::new(
            root.path().join("disabled"),
            env!("CARGO_BIN_EXE_weaver").into(),
            master(),
            false,
            0
        )
        .is_err()
    );
    let sup = sup(
        &root.path().join("n"),
        Path::new(env!("CARGO_BIN_EXE_weaver")),
    );
    let cap: OverlayFile =
        serde_json::from_value(json!({"limits": {"max_processes":64, "spawn_budget":2}})).unwrap();
    let inner = sup
        .register(ID, Config::default(), policy(10), cap)
        .await
        .unwrap();
    let c = contract(&sup);
    c.verify(
        &master().verifying_key().to_bytes(),
        chrono::Utc::now().timestamp(),
    )
    .unwrap();
    assert_ne!(
        inner,
        clawft_types::project::cert::key_id(&master().verifying_key().to_bytes())
    );
    assert_eq!(c.policy.limits.max_processes, Some(64));
    assert_eq!(c.policy.limits.spawn_budget, Some(2));
    let key = std::fs::read(c.home.join(".weftos/user.key")).unwrap();
    assert_ne!(key, master().to_bytes());
    assert_eq!(key, std::fs::read(c.runtime.join("node.key")).unwrap());
    let cfg: Config = serde_json::from_slice(&std::fs::read(&c.config).unwrap()).unwrap();
    let mesh = cfg.kernel.mesh.unwrap();
    assert!(!mesh.enabled && !mesh.discovery && mesh.seed_peers.is_empty());
    assert_eq!(mesh.service, clawft_types::config::MeshServicePolicy::Off);
    assert_eq!(
        cfg.kernel.chain.unwrap().checkpoint_path.unwrap(),
        c.home.join(".weftos/chain/chain.json").to_str().unwrap()
    );
    let command = sup.command(&c);
    let std = command.as_std();
    let args: Vec<_> = std
        .get_args()
        .map(|s| s.to_string_lossy().into_owned())
        .collect();
    assert!(args.windows(2).any(|a| a == ["--profile", "user"]));
    assert!(!args.iter().any(|a| a == "--project"));
    let env: std::collections::BTreeMap<_, _> = std
        .get_envs()
        .filter_map(|(k, v)| {
            v.map(|v| {
                (
                    k.to_string_lossy().into_owned(),
                    v.to_string_lossy().into_owned(),
                )
            })
        })
        .collect();
    assert_eq!(env["HOME"], c.home.to_string_lossy());
    for poison in [
        "WEFTOS_MESH_SOCKET",
        "WEFTOS_PROJECT_ID",
        "WEAVER_PROFILE",
        "OPENAI_API_KEY",
        "DYLD_INSERT_LIBRARIES",
    ] {
        assert!(!env.contains_key(poison));
    }
    let mut tampered = c.clone();
    tampered.instance.depth += 1;
    assert!(
        tampered
            .verify(
                &master().verifying_key().to_bytes(),
                chrono::Utc::now().timestamp()
            )
            .is_err()
    );
    assert!(
        c.verify(
            &SigningKey::from_bytes(&[99; 32]).verifying_key().to_bytes(),
            chrono::Utc::now().timestamp()
        )
        .is_err()
    );
    assert!(
        c.verify(&master().verifying_key().to_bytes(), c.expires_at)
            .is_err()
    );
    consume_generation(&c).unwrap();
    assert!(consume_generation(&c).is_err());
    assert!(sup.refresh_policy(ID, policy(9)).await.is_err());
    let looser: OverlayFile =
        serde_json::from_value(json!({"limits":{"max_processes":129}})).unwrap();
    assert!(
        sup.register(
            "01JB8Z3Q0V6X9KQ4M2N7T5R1WE",
            Config::default(),
            policy(11),
            looser
        )
        .await
        .is_err()
    );
    let grant = NestedRegistration::Collapsed {
        inner_key_id: "0".repeat(32),
        listen: "127.0.0.1:9490".parse().unwrap(),
        genesis_hash: "a".repeat(64),
        peers: vec![format!("127.0.0.1:9489#{}", "b".repeat(32))],
    };
    assert!(sup.grant(ID, grant).await.is_err());
}

#[tokio::test]
async fn nested_registry_recovery_preserves_identity_and_revoke() {
    let root = dir();
    let path = root.path().join("n");
    let first = sup(&path, Path::new(env!("CARGO_BIN_EXE_weaver")));
    first
        .register(ID, Config::default(), policy(20), OverlayFile::default())
        .await
        .unwrap();
    let before = contract(&first);
    drop(first);
    let second = sup(&path, Path::new(env!("CARGO_BIN_EXE_weaver")));
    assert_eq!(contract(&second).inner_pubkey, before.inner_pubkey);
    let pk = clawft_types::project::canon::hex_decode::<32>(&before.inner_pubkey).unwrap();
    second
        .grant(
            ID,
            NestedRegistration::Collapsed {
                inner_key_id: clawft_types::project::cert::key_id(&pk),
                listen: "127.0.0.1:9491".parse().unwrap(),
                genesis_hash: "a".repeat(64),
                peers: vec![format!("127.0.0.1:9489#{}", "b".repeat(32))],
            },
        )
        .await
        .unwrap();
    assert!(contract(&second).generation > before.generation);
    second.revoke(ID).await.unwrap();
    drop(second);
    let third = sup(&path, Path::new(env!("CARGO_BIN_EXE_weaver")));
    assert!(third.start(ID).await.is_err());
    assert!(third.grant(ID, NestedRegistration::Isolated).await.is_err());
}

async fn rpc(socket: &Path, method: &str, params: Value) -> Value {
    rpc_scoped(socket, method, params, None).await
}

async fn rpc_scoped(socket: &Path, method: &str, params: Value, project: Option<&str>) -> Value {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let mut stream = tokio::net::UnixStream::connect(socket).await.unwrap();
    stream
        .write_all(
            format!(
                "{}\n",
                json!({"proto":1,"id":"d10","auth":"admin","method":method,"params":params,"project":project})
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut line = String::new();
    // `project.start` answers only once the supervisor has seen the child
    // kernel ready, and the supervisor's own readiness window is 30 s. A
    // client deadline shorter than that fires on a loaded machine while the
    // call is still legitimately in flight, so the reply is waited for past
    // the server's window; every other method answers immediately.
    let deadline = if method.starts_with("project.") { 90 } else { 15 };
    tokio::time::timeout(
        std::time::Duration::from_secs(deadline),
        BufReader::new(stream).read_line(&mut line),
    )
    .await
    .unwrap()
    .unwrap();
    serde_json::from_str(&line).unwrap()
}

/// Requires the integrated binary. This is a real user daemon supervising a real
/// project kernel, not a fake process answering just the readiness handshake.
#[tokio::test]
async fn nested_user_supervises_project_and_restarts_with_private_identity() {
    let root = dir();
    let sup = sup(root.path(), Path::new(env!("CARGO_BIN_EXE_weaver")));
    let config: Config = serde_json::from_value(
        json!({"kernel":{"llm":{"service_url":"http://127.0.0.1:0","model":"d10-test"}}}),
    )
    .unwrap();
    sup.register(ID, config, policy(30), OverlayFile::default())
        .await
        .unwrap();
    let c = contract(&sup);
    let project = c.home.join("p");
    std::fs::create_dir(&project).unwrap();
    let manifest = clawft_types::project::adopt_or_init(
        &project,
        &c.home.join(".weftos/projects"),
        Some("inner-project"),
    )
    .unwrap();
    sup.start(ID).await.unwrap();
    let sock = c.runtime.join("kernel.sock");
    let h = rpc(&sock, "kernel.handshake", json!({})).await;
    assert_eq!(h["result"]["profile"], "user");
    assert_eq!(h["result"]["depth"], 1);
    let signed_before = std::fs::read(c.runtime.join("parent-policy.json")).unwrap();
    sup.refresh_policy(ID, policy(31)).await.unwrap();
    let repeated = sup.start(ID).await.unwrap();
    assert_eq!(repeated["started"], false);
    assert_eq!(repeated["pid"], h["result"]["pid"]);
    assert_eq!(
        signed_before,
        std::fs::read(c.runtime.join("parent-policy.json")).unwrap()
    );
    let reloaded = rpc(&sock, "governance.reload", json!({})).await;
    assert_eq!(reloaded["ok"], true, "{reloaded}");

    let started = rpc(&sock, "project.start", json!({"id":manifest.id})).await;
    assert_eq!(started["ok"], true, "{started}");
    let child_socket = c.runtime.join(&manifest.id).join("kernel.sock");
    let child = rpc(&child_socket, "kernel.handshake", json!({})).await;
    assert_eq!(child["result"]["project_id"], manifest.id);
    assert!(project.join(".weftos/project.cert.json").exists());
    let inner_key = std::fs::read(c.home.join(".weftos/user.key")).unwrap();
    sup.stop(ID).await.unwrap();
    assert!(
        tokio::net::UnixStream::connect(&child_socket)
            .await
            .is_err(),
        "inner projects must stop with inner user"
    );
    // The chain persists as `chain.rvf` beside the configured `chain.json` checkpoint path
    // when signed (kernel boot prefers RVF); either file is a saved chain.
    let chain = c.home.join(".weftos/chain/chain.json");
    // The child writes its chain while it shuts down, which can finish a
    // moment after `stop` returns on a loaded host: wait for it, bounded.
    let saved = |p: &std::path::Path| p.exists() || p.with_extension("rvf").exists();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while !saved(&chain) && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(saved(&chain), "inner chain was not saved");
    sup.start(ID).await.unwrap();
    assert_eq!(
        std::fs::read(c.home.join(".weftos/user.key")).unwrap(),
        inner_key
    );
    let again = rpc(&sock, "kernel.handshake", json!({})).await;
    assert_eq!(again["result"]["user_key_id"], h["result"]["user_key_id"]);
    sup.revoke(ID).await.unwrap();
    assert!(tokio::net::UnixStream::connect(&sock).await.is_err());
    assert!(sup.start(ID).await.is_err());
}

#[cfg(feature = "mesh")]
#[tokio::test]
async fn nested_granted_listener_refuses_port_collision_and_closes_on_revoke() {
    let root = dir();
    let sup = sup(root.path(), Path::new(env!("CARGO_BIN_EXE_weaver")));
    let config: Config = serde_json::from_value(
        json!({"kernel":{"llm":{"service_url":"http://127.0.0.1:0","model":"d10-test"}}}),
    )
    .unwrap();
    let inner_key_id = sup
        .register(ID, config, policy(40), OverlayFile::default())
        .await
        .unwrap();
    let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let listen = held.local_addr().unwrap();
    let peer = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let peer_addr = peer.local_addr().unwrap();
    drop(peer);
    sup.grant(
        ID,
        NestedRegistration::Collapsed {
            inner_key_id,
            listen,
            genesis_hash: "a".repeat(64),
            peers: vec![format!("{peer_addr}#{}", "b".repeat(32))],
        },
    )
    .await
    .unwrap();
    assert!(
        sup.start(ID).await.is_err(),
        "required listener collision must refuse boot"
    );
    drop(held);
    sup.start(ID).await.unwrap();
    let connection = tokio::net::TcpStream::connect(listen).await.unwrap();
    sup.revoke(ID).await.unwrap();
    assert!(
        tokio::net::TcpStream::connect(listen).await.is_err(),
        "revoked listener still reachable"
    );
    use tokio::io::AsyncReadExt;
    let mut connection = connection;
    let mut bytes = [0u8; 1024];
    // Drain handshake bytes if any; revocation must terminate existing sessions.
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            match connection.read(&mut bytes).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    })
    .await
    .expect("revocation left an existing connection open");
}

#[tokio::test]
async fn nested_policy_noop_preserves_signed_boot_and_policy_bytes() {
    let root = dir();
    let sup = sup(root.path(), Path::new(env!("CARGO_BIN_EXE_weaver")));
    sup.register(ID, Config::default(), policy(50), OverlayFile::default())
        .await
        .unwrap();
    let c = contract(&sup);
    let paths = [
        c.config.clone(),
        c.config.with_file_name("boot.json"),
        c.runtime.join("parent-policy.json"),
    ];
    let before: Vec<_> = paths.iter().map(|p| std::fs::read(p).unwrap()).collect();
    sup.refresh_policy(ID, policy(51)).await.unwrap();
    let after: Vec<_> = paths.iter().map(|p| std::fs::read(p).unwrap()).collect();
    assert_eq!(
        before, after,
        "no-op export changed the live contract signature/generation"
    );
}

#[tokio::test]
async fn recovered_nested_stop_and_revoke_refuse_live_unowned_endpoint() {
    let root = dir();
    let first = sup(root.path(), Path::new(env!("CARGO_BIN_EXE_weaver")));
    first
        .register(ID, Config::default(), policy(60), OverlayFile::default())
        .await
        .unwrap();
    let c = contract(&first);
    drop(first);
    let recovered = sup(root.path(), Path::new(env!("CARGO_BIN_EXE_weaver")));
    let paths = clawft_types::runtime_paths::RuntimePaths::at(&c.runtime);
    let lock = clawft_weave::instance_lock::InstanceLock::acquire(&paths).unwrap();
    assert!(
        recovered.stop(ID).await.is_err(),
        "held runtime was declared stopped after recovery"
    );
    let policy_path = c.runtime.join("parent-policy.json");
    let before = std::fs::read(&policy_path).unwrap();
    let tighter = export_rules(
        Vec::new(),
        0.6,
        false,
        &Limits {
            max_processes: Some(64),
            spawn_budget: Some(2),
            ..Default::default()
        },
        &master(),
        61,
        chrono::Utc::now(),
    )
    .unwrap();
    assert!(
        recovered.refresh_policy(ID, tighter).await.is_err(),
        "rewrote policy before proving recovered child stopped"
    );
    assert_eq!(before, std::fs::read(&policy_path).unwrap());
    assert!(
        recovered.revoke(ID).await.is_err(),
        "held runtime was declared revoked complete"
    );
    drop(lock);
    // A process not following the runtime-lock protocol is still not ours to
    // unlink or signal. Use a short socket root supplied via D10_TEST_ROOT.
    let listener = tokio::net::UnixListener::bind(paths.socket()).unwrap();
    assert!(recovered.stop(ID).await.is_err());
    assert!(recovered.revoke(ID).await.is_err());
    assert!(
        tokio::net::UnixStream::connect(paths.socket())
            .await
            .is_ok(),
        "live unowned endpoint was unlinked"
    );
    drop(listener);
    recovered.stop(ID).await.unwrap();
    assert!(
        !paths.socket().exists(),
        "proven-stale socket was not cleaned up"
    );
}

#[tokio::test]
async fn nested_owned_pipe_cascades_when_deny_all_rejects_shutdown_rpc() {
    let root = dir();
    let sup = sup(root.path(), Path::new(env!("CARGO_BIN_EXE_weaver")));
    let config: Config = serde_json::from_value(json!({"kernel": {
        "governance": {"outside_project":"deny_all"},
        "llm": {"service_url":"http://127.0.0.1:0", "model":"d10-test"}
    }}))
    .unwrap();
    sup.register(ID, config, policy(70), OverlayFile::default())
        .await
        .unwrap();
    let c = contract(&sup);
    let project = c.home.join("p");
    std::fs::create_dir(&project).unwrap();
    let manifest = clawft_types::project::adopt_or_init(
        &project,
        &c.home.join(".weftos/projects"),
        Some("deny-all-inner-project"),
    )
    .unwrap();
    sup.start(ID).await.unwrap();
    let socket = c.runtime.join("kernel.sock");
    // The owner explicitly scopes this lifecycle request to its registered
    // project. The unscoped shutdown below must still fail under deny_all.
    let started = rpc_scoped(
        &socket,
        "project.start",
        json!({"id":manifest.id}),
        Some(&manifest.id),
    )
    .await;
    assert_eq!(started["ok"], true, "{started}");
    let child_socket = c.runtime.join(&manifest.id).join("kernel.sock");
    assert!(tokio::net::UnixStream::connect(&child_socket).await.is_ok());
    let denied = rpc(&socket, "kernel.shutdown", json!({})).await;
    assert_eq!(denied["ok"], false, "deny_all was weakened: {denied}");
    assert_eq!(denied["error_kind"], "project_required", "{denied}");
    tokio::time::timeout(std::time::Duration::from_secs(20), sup.stop(ID))
        .await
        .expect("owned shutdown exceeded its bound")
        .unwrap();
    assert!(tokio::net::UnixStream::connect(&socket).await.is_err());
    assert!(
        tokio::net::UnixStream::connect(&child_socket)
            .await
            .is_err(),
        "inner projects must stop with inner user even when shutdown RPC is denied"
    );
}

fn pid_gone(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) }
}

/// The nested instance dies without any cleanup (SIGKILL: the test process
/// dying, a nextest kill). Its project kernel, in its own process group, must
/// still exit within a bound, by its stdin liveness pipe closing.
#[tokio::test]
async fn nested_sigkill_reaps_supervised_project_child() {
    let root = dir();
    let sup = sup(root.path(), Path::new(env!("CARGO_BIN_EXE_weaver")));
    let config: Config = serde_json::from_value(json!({"kernel": {
        "llm": {"service_url":"http://127.0.0.1:0", "model":"d10-test"}
    }}))
    .unwrap();
    sup.register(ID, config, policy(71), OverlayFile::default())
        .await
        .unwrap();
    let c = contract(&sup);
    let project = c.home.join("p");
    std::fs::create_dir(&project).unwrap();
    let manifest = clawft_types::project::adopt_or_init(
        &project,
        &c.home.join(".weftos/projects"),
        Some("sigkill-inner-project"),
    )
    .unwrap();
    let nested = sup.start(ID).await.unwrap()["pid"].as_i64().unwrap() as i32;
    let sock = c.runtime.join("kernel.sock");
    let started = rpc(&sock, "project.start", json!({"id":manifest.id})).await;
    assert_eq!(started["ok"], true, "{started}");
    let child_socket = c.runtime.join(&manifest.id).join("kernel.sock");
    let child_pid = rpc(&child_socket, "kernel.handshake", json!({})).await["result"]["pid"]
        .as_i64()
        .expect("project kernel pid") as i32;
    assert_ne!(child_pid, nested);
    assert!(!pid_gone(child_pid));
    unsafe { libc::kill(nested, libc::SIGKILL) };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(25);
    while !pid_gone(child_pid) {
        assert!(
            std::time::Instant::now() < deadline,
            "project kernel {child_pid} outlived its SIGKILLed nested instance"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}
