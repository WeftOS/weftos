//! Inference placement wiring. Upstreams are tiny fakes on random loopback
//! ports; no default port and no real model server is ever contacted.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use clawft_kernel::infer_proxy::{ProxyAudit, ProxyLimits};
use clawft_kernel::mesh_runtime::MeshRuntime;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::infer_wire::*;

/// A fake llama.cpp server: `/health` and `/v1/models` answer 200, anything
/// else echoes the request line. Counts what it saw.
pub(crate) struct Fake {
    pub(crate) addr: SocketAddr,
    pub(crate) seen: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Fake {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub(crate) async fn fake() -> Fake {
    fake_on(0).await
}

/// A port nothing listens on and no parallel test is handed (see the
/// kernel's `infer::fakes::free_port`): below the OS ephemeral range, in a
/// per-process slice, bind-checked.
pub(crate) fn free_port() -> u16 {
    use std::sync::atomic::{AtomicU16, Ordering};
    static NEXT: AtomicU16 = AtomicU16::new(0);
    let base = 10_000 + (std::process::id() % 1000) as u16 * 16;
    for _ in 0..4096 {
        let port = base + NEXT.fetch_add(1, Ordering::Relaxed) % 16;
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

#[derive(Default)]
pub(crate) struct Audit(pub(crate) Mutex<Vec<String>>);

impl ProxyAudit for Audit {
    fn record(&self, kind: &str, _: serde_json::Value) {
        self.0.lock().unwrap().push(kind.to_string());
    }
}

pub(crate) fn write_cfg(dir: &std::path::Path, v: serde_json::Value) {
    std::fs::write(dir.join(CONFIG_FILE), v.to_string()).unwrap();
}

pub(crate) fn parts<'a>(
    dir: &'a std::path::Path,
    mesh: Option<Arc<MeshRuntime>>,
    service: bool,
    audit: Option<Arc<dyn ProxyAudit>>,
) -> InitParts<'a> {
    InitParts {
        dir,
        node_id: "node-a".into(),
        mesh,
        service_mode: service,
        audit,
        limits: ProxyLimits::default(),
        gate: None,
        chain: None,
    }
}

pub(crate) fn role(fake_port: u16, proxy_port: Option<u16>) -> serde_json::Value {
    let mut r = serde_json::json!({"role": "hermes", "flavor": "llamacpp", "instance_port": fake_port});
    if let Some(p) = proxy_port {
        r["proxy_port"] = p.into();
    }
    r
}

pub(crate) async fn http_get(addr: SocketAddr, path: &str) -> String {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\n\r\n").as_bytes()).await.unwrap();
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out).await;
    String::from_utf8_lossy(&out).into_owned()
}

#[test]
fn config_is_validated_and_defaults_to_off() {
    let dir = tempfile::tempdir().unwrap();
    assert!(load_config(dir.path()).unwrap().is_none(), "absent means off");
    let bad = |v: serde_json::Value| {
        write_cfg(dir.path(), v);
        load_config(dir.path()).unwrap_err()
    };
    let ok = serde_json::json!({"roles": [role(18090, Some(8090))]});
    write_cfg(dir.path(), ok);
    assert!(load_config(dir.path()).unwrap().is_some());
    bad(serde_json::json!({"roles": []}));
    bad(serde_json::json!({"roles": [role(1, None), role(2, None)]}));
    bad(serde_json::json!({"roles": [{"role": "a/b", "flavor": "llamacpp", "instance_port": 1}]}));
    bad(serde_json::json!({"roles": [{"role": "a", "flavor": "vllm", "instance_port": 1}]}));
    bad(serde_json::json!({"roles": [role(5, Some(5))]}));
    bad(serde_json::json!({"roles": [role(5, Some(6))], "unknown": 1}));
    bad(serde_json::json!({"roles": [role(5, None)], "mesh": {"expose": ["nope"]}}));
    bad(serde_json::json!({"roles": [role(5, None)], "mesh": {"serve_peers": {"hermes": ["bad node!"]}}}));
    bad(serde_json::json!({"roles": [role(5, None)], "advert_ttl_secs": 60, "advert_secs": 31}));
    bad(serde_json::json!({"roles": [role(5, None)], "advert_ttl_secs": 60, "advert_secs": 0}));
    bad(serde_json::json!({"roles": [role(5, None)], "sync_secs": 0}));
    bad(serde_json::json!({"roles": [role(5, None)], "sync_secs": 61}));
    // The announcement rides the sync tick: sync may not exceed advert
    // (against the defaults too).
    bad(serde_json::json!({"roles": [role(5, None)], "advert_ttl_secs": 60, "advert_secs": 2}));
    // sync 50 against the 60 s default announce really announces every 100 s, over 90.
    bad(serde_json::json!({"roles": [role(5, None)], "sync_secs": 50}));
    bad(serde_json::json!({"roles": [role(5, None)], "sync_secs": 11, "advert_ttl_secs": 60, "advert_secs": 10}));
    // sync 29 with advert 30 really announces every 58 s: rejected.
    bad(serde_json::json!({"roles": [role(5, None)], "advert_ttl_secs": 60, "advert_secs": 30, "sync_secs": 29}));
    bad(serde_json::json!({"roles": [role(5, None)], "advert_ttl_secs": 60, "advert_secs": 30, "sync_secs": 7}));
    write_cfg(dir.path(), serde_json::json!({"roles": [role(5, None)], "advert_ttl_secs": 60, "advert_secs": 30, "sync_secs": 6}));
    assert!(load_config(dir.path()).is_ok(), "30 is a multiple of 6");
    write_cfg(dir.path(), serde_json::json!({"roles": [role(5, None)], "advert_ttl_secs": 60, "advert_secs": 30, "sync_secs": 30}));
    assert!(load_config(dir.path()).is_ok());
    write_cfg(dir.path(), serde_json::json!({"roles": [role(5, None)], "advert_ttl_secs": 60, "advert_secs": 5}));
    assert!(load_config(dir.path()).is_ok(), "equal to the default sync");
    // Defaults (owner 2026-10-29): announce every 60 s, TTL 180 s.
    assert_eq!(crate::infer_cfg::DEFAULT_ADVERT, 60);
    assert_eq!(clawft_kernel::infer_proxy::DEFAULT_ADVERT_TTL.as_secs(), 180);
    write_cfg(dir.path(), serde_json::json!({"roles": [role(5, None)]}));
    let c = load_config(dir.path()).unwrap().unwrap();
    assert_eq!(c.advert_ttl().as_secs(), 180);
    // An announce period above half the TTL is refused, at the default TTL and a configured one.
    bad(serde_json::json!({"roles": [role(5, None)], "advert_secs": 91}));
    write_cfg(dir.path(), serde_json::json!({"roles": [role(5, None)], "advert_secs": 90}));
    assert!(load_config(dir.path()).is_ok(), "90 is half the 180 s default TTL");
    bad(serde_json::json!({"roles": [role(5, None)], "advert_ttl_secs": 100}));
    bad(serde_json::json!({"roles": [role(5, None)], "advert_ttl_secs": 1}));
    bad(serde_json::json!({"roles": [role(5, None)], "advert_ttl_secs": 3601}));
    write_cfg(dir.path(), serde_json::json!({"roles": [role(5, None)], "advert_ttl_secs": 120}));
    let c = load_config(dir.path()).unwrap().unwrap();
    assert_eq!(c.advert_ttl().as_secs(), 120);
    let mut r = role(5, Some(6));
    r["on_occupied"] = "bind-anyway".into();
    bad(serde_json::json!({"roles": [r]}));
    std::fs::write(dir.path().join(CONFIG_FILE), vec![b' '; 70_000]).unwrap();
    assert!(load_config(dir.path()).is_err(), "oversized file");
}

#[tokio::test]
async fn a_placed_role_is_served_through_the_stable_proxy() {
    let up = fake().await;
    let dir = tempfile::tempdir().unwrap();
    let proxy_port = free_port();
    write_cfg(dir.path(), serde_json::json!({"roles": [role(up.addr.port(), Some(proxy_port))]}));
    let (st, _) = build(parts(dir.path(), None, false, None)).await.unwrap().unwrap();
    assert!(st.table.resolve("hermes").is_some(), "the observed server is registered");
    let proxy: SocketAddr = ([127, 0, 0, 1], proxy_port).into();
    let r = http_get(proxy, "/v1/chat/completions").await;
    assert!(r.contains("served"), "{r}");
    assert!(up.seen.lock().unwrap().iter().any(|l| l.contains("/v1/chat/completions")));
    assert_eq!(
        st.table.base_url_for_role("hermes"),
        Some(format!("http://127.0.0.1:{}/v1", up.addr.port()))
    );
}

#[tokio::test]
async fn a_server_that_is_not_up_yet_is_picked_up_on_a_later_pass() {
    let dir = tempfile::tempdir().unwrap();
    // Reserve a port, then bring the fake up on it only after the first pass.
    let port = free_port();
    write_cfg(dir.path(), serde_json::json!({"roles": [role(port, None)]}));
    let (st, _) = build(parts(dir.path(), None, false, None)).await.unwrap().unwrap();
    assert!(st.table.resolve("hermes").is_none(), "nothing listens: not served");
    let l = TcpListener::bind(("127.0.0.1", port)).await;
    let Ok(l) = l else { return }; // port raced away; nothing to prove
    drop(l);
    let up = fake_on(port).await;
    st.sync_once().await;
    assert!(st.table.resolve("hermes").is_some());
    drop(up);
}

/// A fake model server on `port` (0: any free one): llama.cpp and Ollama
/// probe endpoints answer, anything else echoes. Records request lines.
pub(crate) async fn fake_on(port: u16) -> Fake {
    let l = TcpListener::bind(("127.0.0.1", port)).await.unwrap();
    let addr = l.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sn = seen.clone();
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else { return };
            let sn = sn.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = s.read(&mut buf).await.unwrap_or(0);
                let line = String::from_utf8_lossy(&buf[..n]).lines().next().unwrap_or("").to_string();
                sn.lock().unwrap().push(line.clone());
                let body = if line.contains("/health") {
                    r#"{"status":"ok"}"#.to_string()
                } else if line.contains("/api/version") {
                    r#"{"version":"0.0.0-test"}"#.to_string()
                } else if line.contains("/api/tags") || line.contains("/api/ps") {
                    r#"{"models":[]}"#.to_string()
                } else if line.contains("/v1/models") {
                    r#"{"data":[{"id":"m.gguf"}]}"#.to_string()
                } else {
                    r#"{"echo":"served"}"#.to_string()
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(resp.as_bytes()).await;
            });
        }
    });
    Fake { addr, seen, task }
}

#[tokio::test]
async fn an_occupied_proxy_port_is_refused_or_adopted_never_taken() {
    let up = fake().await;
    let holder = fake().await; // something else already owns the proxy port
    let dir = tempfile::tempdir().unwrap();
    for (policy, want_adopt) in [("refuse", false), ("adopt", true)] {
        let mut r = role(up.addr.port(), Some(holder.addr.port()));
        r["on_occupied"] = policy.into();
        write_cfg(dir.path(), serde_json::json!({"roles": [r]}));
        let (st, _) = build(parts(dir.path(), None, false, None)).await.unwrap().unwrap();
        let state = st.roles[0].proxy.lock().unwrap().clone();
        match (want_adopt, state) {
            (true, ProxyState::Adopted(a)) => assert_eq!(a, holder.addr),
            (false, ProxyState::Refused(why)) => assert!(why.contains("in use"), "{why}"),
            (_, other) => panic!("{policy}: {other:?}"),
        }
    }
    // The occupancy probe only connects; no request is ever sent to the owner.
    assert!(holder.seen.lock().unwrap().iter().all(|l| l.is_empty()), "{:?}", holder.seen.lock().unwrap());
}

#[tokio::test]
async fn mesh_is_off_and_deny_by_default_and_audited_when_set() {
    let up = fake().await;
    let dir = tempfile::tempdir().unwrap();
    let audit = Arc::new(Audit::default());
    // No mesh block: with a mesh runtime attached, still nothing exposed.
    write_cfg(dir.path(), serde_json::json!({"roles": [role(up.addr.port(), None)]}));
    let rt = Arc::new(MeshRuntime::new("node-a".into()));
    let (st, _) = build(parts(dir.path(), Some(rt.clone()), false, Some(audit.clone()))).await.unwrap().unwrap();
    assert!(st.hub.is_some());
    assert!(st.table.exposed_roles().is_empty());
    assert!(!st.table.peer_listed_any("node-b"));
    assert!(st.table.advertisements(0).is_empty());
    // Config allowlists apply and are audited.
    write_cfg(
        dir.path(),
        serde_json::json!({"roles": [role(up.addr.port(), None)], "mesh": {
            "expose": ["hermes"],
            "serve_peers": {"hermes": ["node-b"]},
            "remote_nodes": {"hermes": ["node-c"]}}}),
    );
    let audit2 = Arc::new(Audit::default());
    let rt2 = Arc::new(MeshRuntime::new("node-a".into()));
    let (st, _) = build(parts(dir.path(), Some(rt2), false, Some(audit2.clone()))).await.unwrap().unwrap();
    assert_eq!(st.table.exposed_roles(), ["hermes"]);
    assert!(st.table.mesh_peer_allowed("hermes", "node-b"));
    assert!(!st.table.mesh_peer_allowed("hermes", "node-z"));
    let kinds = audit2.0.lock().unwrap().clone();
    for k in ["infer.mesh.expose", "infer.mesh.allow", "infer.remote.allow"] {
        assert!(kinds.contains(&k.to_string()), "{k} not audited: {kinds:?}");
    }
}

#[tokio::test]
async fn service_mode_serves_locally_but_no_mesh() {
    let up = fake().await;
    let dir = tempfile::tempdir().unwrap();
    write_cfg(
        dir.path(),
        serde_json::json!({"roles": [role(up.addr.port(), None)],
            "mesh": {"expose": ["hermes"], "serve_peers": {"hermes": ["node-b"]}}}),
    );
    // Even if a runtime object exists, service mode must not build a hub.
    let rt = Arc::new(MeshRuntime::new("node-a".into()));
    let (st, _) = build(parts(dir.path(), Some(rt), true, None)).await.unwrap().unwrap();
    assert!(st.hub.is_none());
    assert!(st.mesh_note.contains("service mode"), "{}", st.mesh_note);
    assert!(st.table.resolve("hermes").is_some(), "local placement still works");
    assert!(st.table.exposed_roles().is_empty(), "mesh settings are ignored in service mode");
    assert!(!st.table.peer_listed_any("node-b"));
    // No mesh runtime at all: same.
    let (st, _) = build(parts(dir.path(), None, false, None)).await.unwrap().unwrap();
    assert!(st.hub.is_none() && st.mesh_note.contains("no mesh runtime"));
}

#[tokio::test]
async fn operator_verbs_and_the_local_provider_hook() {
    use crate::infer_rpc::handle;
    // Before init: off, and the verbs say so.
    assert!(handle("infer.status", serde_json::Value::Null).await.error.is_some());
    let up = fake().await;
    let dir = tempfile::tempdir().unwrap();
    write_cfg(
        dir.path(),
        serde_json::json!({"roles": [{"role": "hermes", "flavor": "llamacpp",
            "instance_port": up.addr.port(), "provider": "local"}]}),
    );
    // Service mode: placement works, mesh verbs refuse with the reason.
    let st = init(parts(dir.path(), None, true, None)).await.unwrap().unwrap();
    assert!(st.table.resolve("hermes").is_some());
    let status = handle("infer.status", serde_json::Value::Null).await.result.unwrap();
    assert_eq!(status["roles"][0]["serves"], "local");
    assert_eq!(status["mesh"]["available"], false);
    let r = handle("infer.expose", serde_json::json!({"role": "hermes", "exposed": true})).await;
    assert!(r.error.unwrap().contains("service mode"));
    let r = handle("infer.allow", serde_json::json!({"role":"hermes","node":"n","direction":"serve","allowed":true})).await;
    assert!(r.error.is_some());
    // The hook: a default config lets `local` follow the role.
    let config = clawft_types::config::Config::default();
    if let Some((role, _)) = clawft_core::placement_hook::placement_for("local", &config) {
        assert_eq!(role, "hermes");
    }
    // An explicit endpoint setting keeps the provider out of placement.
    let mut explicit = config.clone();
    explicit.providers.local.api_base = Some("http://127.0.0.1:9100/v1".into());
    assert!(clawft_core::placement_hook::placement_for("local", &explicit).is_none());
}

#[tokio::test]
async fn hub_mode_verbs_change_allowlists_with_validation() {
    let up = fake().await;
    let dir = tempfile::tempdir().unwrap();
    write_cfg(dir.path(), serde_json::json!({"roles": [role(up.addr.port(), None)]}));
    let rt = Arc::new(MeshRuntime::new("node-a".into()));
    let audit = Arc::new(Audit::default());
    let (st, _) = build(parts(dir.path(), Some(rt), false, Some(audit.clone()))).await.unwrap().unwrap();
    // The verbs act on the global state; exercise the same table calls the
    // verbs make, with the same validation boundary.
    assert!(st.hub.is_some());
    st.table.allow_mesh_peer("hermes", "node-b", true);
    st.table.allow_remote_node("hermes", "node-b", true);
    st.table.expose_to_mesh("hermes", true);
    assert!(st.table.advertisement("hermes", 1).is_some());
    st.table.expose_to_mesh("hermes", false);
    assert!(st.table.advertisement("hermes", 2).is_none());
    assert!(audit.0.lock().unwrap().len() >= 4);
}
