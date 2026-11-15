//! Daemon-level tests for the project certificate RPCs (ADR-103 A7,
//! Phase 2 package C): capability, token and relay refusal on the real
//! dispatch path, and the owner's register/show/rekey/revoke flow against
//! a tempdir manifest store (never the real `HOME`).

use std::sync::Arc;
use std::time::Duration;

use clawft_kernel::boot::Kernel;
use clawft_platform::NativePlatform;
use clawft_types::config::{ChainConfig, Config, KernelConfig};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream, UnixListener, UnixStream};
use tokio::sync::{RwLock, watch};

type KernelRef = Arc<RwLock<Kernel<NativePlatform>>>;

struct Daemon {
    _tmp: tempfile::TempDir,
    sock: std::path::PathBuf,
    kernel: KernelRef,
    _shutdown: watch::Sender<bool>,
}

/// One run root for the whole binary, pinned before any daemon exists, so no
/// test (whatever order libtest runs them in) can resolve the real
/// `~/.weftos/run` for a revoke marker or a child run dir.
fn run_root() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    let dir = DIR.get_or_init(|| tempfile::tempdir().unwrap()).path();
    clawft_weave::project_cert_rpc::init_run_root(dir.to_path_buf());
    dir
}

/// `untrusted`: serve every connection as a peer that is not our uid.
async fn spawn(untrusted: bool) -> Daemon {
    run_root();
    let tmp = tempfile::tempdir().unwrap();
    let sock = tmp.path().join("kernel.sock");
    let kcfg = KernelConfig {
        chain: Some(ChainConfig::isolated_in(
            &tempfile::tempdir().unwrap().keep(),
        )),
        ..KernelConfig::default()
    };
    let kernel = Kernel::boot(Config::default(), kcfg, Arc::new(NativePlatform::new()))
        .await
        .expect("boot");
    let kernel: KernelRef = Arc::new(RwLock::new(kernel));
    let listener = UnixListener::bind(&sock).unwrap();
    let (tx, _rx) = watch::channel(false);
    let (k, t) = (Arc::clone(&kernel), tx.clone());
    tokio::spawn(async move {
        while let Ok((s, _)) = listener.accept().await {
            tokio::spawn(clawft_weave::daemon::handle_connection_peer(
                s,
                Arc::clone(&k),
                t.clone(),
                untrusted,
            ));
        }
    });
    tokio::time::sleep(Duration::from_millis(30)).await;
    Daemon {
        _tmp: tmp,
        sock,
        kernel,
        _shutdown: tx,
    }
}

async fn exchange<R, W>(r: R, mut w: W, method: &str, params: Value, auth: Option<&str>) -> Value
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut req = json!({ "id": "t", "proto": 1, "method": method, "params": params });
    if let Some(a) = auth {
        req["auth"] = json!(a);
    }
    w.write_all(format!("{req}\n").as_bytes()).await.unwrap();
    let mut line = String::new();
    BufReader::new(r).read_line(&mut line).await.unwrap();
    serde_json::from_str(line.trim()).unwrap()
}

async fn call(d: &Daemon, method: &str, params: Value, auth: Option<&str>) -> Value {
    let (r, w) = UnixStream::connect(&d.sock).await.unwrap().into_split();
    exchange(r, w, method, params, auth).await
}

/// Same call, but through the sanitising TCP relay.
async fn call_via_relay(d: &Daemon, method: &str, params: Value, auth: Option<&str>) -> Value {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let sock = d.sock.clone();
    tokio::spawn(async move {
        let (tcp, _) = l.accept().await.unwrap();
        let unix = UnixStream::connect(sock).await.unwrap();
        let _ = clawft_weave::relay_auth::relay(tcp, unix).await;
    });
    let (r, w) = TcpStream::connect(addr).await.unwrap().into_split();
    exchange(r, w, method, params, auth).await
}

fn denied(v: &Value) -> bool {
    v["ok"] == false
        && v["error"]
            .as_str()
            .unwrap_or("")
            .contains("permission denied")
}

const CERT_METHODS: [&str; 5] = [
    "project.cert.show",
    "project.cert.challenge",
    "project.identity.repair",
    "project.rekey",
    "project.revoke",
];

#[tokio::test]
async fn anonymous_relay_and_untrusted_peer_are_denied() {
    let d = spawn(false).await;
    for m in CERT_METHODS {
        assert!(denied(&call(&d, m, json!({}), None).await), "{m} anonymous");
        let r = call_via_relay(&d, m, json!({}), Some("admin")).await;
        assert!(denied(&r), "{m} via relay: {r}");
    }
    let u = spawn(true).await;
    for m in CERT_METHODS {
        assert!(denied(&call(&u, m, json!({}), Some("admin")).await), "{m} untrusted peer");
    }
}

#[tokio::test]
async fn a_token_cannot_manage_project_certificates() {
    let d = spawn(false).await;
    let v = call(&d, "auth.token.issue", json!({}), Some("admin")).await;
    let secret = v["result"]["secret"].as_str().unwrap().to_owned();
    for m in CERT_METHODS {
        let r = call(&d, m, json!({}), Some(&secret)).await;
        assert_eq!(r["error_kind"], "token_cannot_manage_tokens", "{m}: {r}");
        let r = call_via_relay(&d, m, json!({}), Some(&secret)).await;
        assert_eq!(r["error_kind"], "token_cannot_manage_tokens", "{m} via relay: {r}");
    }
}

/// One sequential test: it flips the process-wide user-daemon profile.
#[tokio::test]
async fn owner_flow_on_the_user_daemon() {
    let d = spawn(false).await;
    let home = tempfile::tempdir().unwrap();
    let mdir = home.path().join(".weftos/projects");
    clawft_weave::project_rpc::init_manifests_dir(mdir.clone());
    let root = home.path().join("proj");
    std::fs::create_dir_all(&root).unwrap();

    // Not the user daemon yet: refused, nothing certified.
    let r = call(&d, "project.cert.show", json!({"id": "01JB8Z3Q0V6X9KQ4M2N7T5R1WD"}), Some("admin")).await;
    assert_eq!(r["error_kind"], "cert_unavailable", "{r}");

    // An explicit run root: nothing resolves the real `~/.weftos/run`.
    clawft_weave::user_daemon::enter_at(run_root());
    let reg = call(&d, "project.register", json!({"root": root, "name": "demo"}), Some("admin")).await;
    assert_eq!(reg["ok"], true, "{reg}");
    let id = reg["result"]["project"]["id"].as_str().unwrap().to_owned();
    let canon_root = std::path::PathBuf::from(reg["result"]["project"]["root"].as_str().unwrap());

    let shown = call(&d, "project.cert.show", json!({"id": id}), Some("admin")).await;
    assert_eq!(shown["result"]["certified"], false, "{shown}");

    // Certify through the function package H's mesh.register calls.
    use clawft_kernel::project_identity as ident;
    use clawft_types::project::cert::PopOp;
    let user_kid = {
        let k = d.kernel.read().await;
        let uk = k.chain_manager().unwrap().signing_key_clone().unwrap();
        clawft_types::project::cert::key_id(&uk.verifying_key().to_bytes())
    };
    let key = ed25519_dalek::SigningKey::from_bytes(&[2u8; 32]);
    let nonce = clawft_weave::project_cert_rpc::issue_challenge(&id).unwrap();
    let req = clawft_weave::project_cert_rpc::RegisterRequest {
        project_id: id.clone(),
        project_pubkey: key.verifying_key().to_bytes(),
        root_sha256: clawft_weave::project_cert_rpc::root_sha256(&canon_root),
        spawn: clawft_weave::project_cert_rpc::SpawnInfo { pid: 1, exe_sha: "ab".repeat(32) },
        pop_sig: ident::pop_sign(&key, PopOp::Register, &user_kid, &nonce, &id).unwrap(),
        nonce: clawft_weave::project_cert_rpc::claim_nonce(&nonce, &id).unwrap(),
    };
    let ctx = clawft_weave::rpc_ext::ExtCtx {
        kernel: Arc::clone(&d.kernel),
        auth: None,
        project: None,
        verified_project: None,
        caps: clawft_weave::capability::CallerCapabilities::anonymous(),
        connection: None,
    };
    let issued = clawft_weave::project_cert_rpc::issue_for_register(&ctx, req).await.unwrap();
    assert!(issued.new);
    assert!(mdir.join(format!("{id}.cert.json")).is_file());
    let shown = call(&d, "project.cert.show", json!({"id": id}), Some("admin")).await;
    assert_eq!(shown["result"]["certified"], true, "{shown}");
    assert_eq!(shown["result"]["cert"]["project_key_id"], issued.cert.project_key_id.as_str());

    // Rekey, then revoke.
    let key2 = ed25519_dalek::SigningKey::from_bytes(&[3u8; 32]);
    let ch = call(&d, "project.cert.challenge", json!({"id": id}), Some("admin")).await;
    assert_eq!(ch["ok"], true, "{ch}");
    assert_eq!(ch["result"]["user_key_id"], user_kid.as_str());
    let n2 = ch["result"]["nonce"].as_str().unwrap().to_owned();
    let sig2 = ident::pop_sign(&key2, PopOp::Rekey, &user_kid, &n2, &id).unwrap();
    let rk = call(
        &d,
        "project.rekey",
        json!({"id": id, "new_pubkey": ident::hex(&key2.verifying_key().to_bytes()),
               "nonce": n2, "pop_sig": ident::hex(&sig2), "reason": "test"}),
        Some("admin"),
    )
    .await;
    assert_eq!(rk["ok"], true, "{rk}");
    assert_eq!(rk["result"]["cert"]["serial"], 2);
    let rv = call(&d, "project.revoke", json!({"id": id, "reason": "done"}), Some("admin")).await;
    assert_eq!(rv["ok"], true, "{rv}");
    assert!(!mdir.join(format!("{id}.cert.json")).exists());

    // The user chain recorded all three, and no private bytes.
    let events = d.kernel.read().await.chain_manager().unwrap().tail_from(0);
    let kinds: Vec<&str> = events.iter().filter(|e| e.source == "user.projects").map(|e| e.kind.as_str()).collect();
    assert_eq!(kinds, ["project.register", "project.rekey", "project.revoke"]);

    // The source is reserved: a Write caller cannot forge identity events.
    let forged = call(
        &d,
        "chain.append",
        json!({"source": "user.projects", "record": {"kind": "project.revoke", "entries": [],
            "hash_before": "", "hash_after": "", "ts": "2026-10-01T00:00:00Z"}}),
        Some("admin"),
    )
    .await;
    assert_eq!(forged["error_kind"], "reserved_source", "{forged}");
    // Review M3: the kernel's own sources too (the rollback floor, genesis
    // and supervisor events are read back by kind).
    for source in ["governance", "project", "project.supervisor"] {
        let forged = call(
            &d,
            "chain.append",
            json!({"source": source, "record": {"kind": "governance.overlay.applied", "entries": [],
                "hash_before": "", "hash_after": "", "ts": "2026-10-01T00:00:00Z"}}),
            Some("admin"),
        )
        .await;
        assert_eq!(forged["error_kind"], "reserved_source", "{source}: {forged}");
    }
    let dump = serde_json::to_string(&events).unwrap();
    assert!(!dump.contains(&"02".repeat(32)) && !dump.contains(&"03".repeat(32)));
    clawft_weave::user_daemon::leave();
}
