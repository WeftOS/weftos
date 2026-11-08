use std::sync::Mutex;

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use weftos_store::audit::MemSink;
use weftos_store::service::{Config, Parties};
use weftos_store::{Access, ChainSink, ExposureGate, MemBackend, Principal, Source, StoreName};

use super::*;

struct Open;
impl ExposureGate for Open {
    fn register(&self, _: &str, _: &str) {}
    fn check(&self, _: &str) -> Result<(), String> {
        Ok(())
    }
}
struct NoChain;
impl ChainSink for NoChain {
    fn record(&self, _: &str, _: Value) {}
}

struct Rig {
    svc: Arc<StoreService>,
    sink: Arc<MemSink>,
    be: Arc<MemBackend>,
    rx: Receiver,
    store: StoreName,
}

async fn rig() -> Rig {
    let be = Arc::new(MemBackend::new());
    let sink = Arc::new(MemSink::default());
    let svc = Arc::new(
        StoreService::open(Config { node_id: "pg-1".into(), state_path: None, clock: || 1_000 }, be.clone(), be.clone(), Arc::new(Open), sink.clone(), Arc::new(NoChain)).unwrap(),
    );
    let store = StoreName::node("weft", "pg-1").unwrap();
    svc.create_store(&store, "daemon", Parties::default()).unwrap();
    svc.grant(&store, Principal::parse("cog:c1").unwrap(), Access::Write, "op").unwrap();
    let rx = serve(svc.clone(), "127.0.0.1:0".parse().unwrap(), "t".repeat(64)).await.unwrap();
    Rig { svc, sink, be, rx, store }
}

#[derive(Default)]
struct Wakes(Mutex<Vec<(String, String, String)>>);
impl WakeSink for Wakes {
    fn notified(&self, store: &str, bucket: &str, body: &str) {
        self.0.lock().unwrap().push((store.into(), bucket.into(), body.into()));
    }
}
struct Boom;
impl WakeSink for Boom {
    fn notified(&self, _: &str, _: &str, _: &str) {
        panic!("a bad sink");
    }
}

fn notify_body(bucket: &str, key: &str, event: &str) -> String {
    format!(r#"{{"EventName":"{event}","Key":"{bucket}/{key}","Records":[{{"eventVersion":"2.1","eventName":"{event}","userIdentity":{{"principalId":"AK1"}},"s3":{{"bucket":{{"name":"{bucket}"}},"object":{{"key":"{key}","size":6}}}}}}]}}"#)
}

async fn rig_with_wake(w: Arc<dyn WakeSink>) -> Rig {
    let mut r = rig().await;
    r.rx = serve_with(r.svc.clone(), "127.0.0.1:0".parse().unwrap(), "t".repeat(64), Some(w)).await.unwrap();
    r
}

/// One raw HTTP/1.1 request; returns the status code.
async fn post(rx: &Receiver, path: &str, method: &str, body: &[u8]) -> u16 {
    let mut s = TcpStream::connect(rx.addr).await.unwrap();
    let head = format!("{method} {path} HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
    s.write_all(head.as_bytes()).await.unwrap();
    s.write_all(body).await.unwrap();
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out).await;
    let text = String::from_utf8_lossy(&out);
    text.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0)
}

fn key_of(r: &Rig) -> String {
    r.be.keys_of("cog:c1").pop().expect("the cog's key")
}

fn put_record(key: &str, bucket: &str) -> String {
    format!(r#"{{"EventName":"s3:ObjectCreated:Put","Key":"{bucket}/s3:PutObject","Records":[{{"event":"s3:ObjectCreated:Put","api":{{"name":"s3:PutObject","bucket":"{bucket}","object":"k/obj.txt","status":"success","status_code":200}},"accessKey":"{key}"}}]}}"#)
}

#[tokio::test]
async fn a_direct_s3_put_by_a_cogs_key_reaches_the_activity_log() {
    let r = rig().await;
    let before = r.sink.events().len();
    let path = format!("/store/ingest/{}/audit", r.rx.token);
    assert_eq!(post(&r.rx, &path, "POST", put_record(&key_of(&r), &r.store.bucket()).as_bytes()).await, 200);
    let ev = &r.sink.events()[before..];
    assert_eq!(ev.len(), 1);
    assert_eq!((ev[0].principal.as_str(), ev[0].op.as_str(), ev[0].source), ("cog:c1", "PutObject", Source::RustfsAudit));
    assert_eq!(ev[0].store, r.store.to_string());
    assert_eq!(ev[0].key.as_deref(), Some("k/obj.txt"));
}

#[tokio::test]
async fn the_daemons_own_traffic_and_other_buckets_are_not_logged() {
    let r = rig().await;
    let before = r.sink.events().len();
    let path = format!("/store/ingest/{}/audit", r.rx.token);
    // The root key (not a principal's key): the governed path logged this already.
    assert_eq!(post(&r.rx, &path, "POST", put_record("rootakb06cee4ae43f", &r.store.bucket()).as_bytes()).await, 200);
    // A bucket that is not ours, even with a known key.
    assert_eq!(post(&r.rx, &path, "POST", put_record(&key_of(&r), "someone-elses-bucket").as_bytes()).await, 200);
    assert_eq!(r.sink.events().len(), before, "nothing was logged for either");
}

#[tokio::test]
async fn a_notification_invalidates_the_cache_and_is_logged_for_a_known_key() {
    let r = rig().await;
    let seen = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
    let s2 = seen.clone();
    r.svc.set_invalidator(Arc::new(move |st, k| s2.lock().unwrap().push((st.to_owned(), k.to_owned()))));
    let body = format!(
        r#"{{"EventName":"s3:ObjectCreated:Put","Key":"b/c%2Fobj.txt","Records":[{{"eventName":"s3:ObjectCreated:Put","userIdentity":{{"principalId":"{}"}},"s3":{{"bucket":{{"name":"{}"}},"object":{{"key":"c%2Fobj.txt","size":6}}}}}}]}}"#,
        key_of(&r),
        r.store.bucket()
    );
    assert_eq!(post(&r.rx, &format!("/store/ingest/{}/notify", r.rx.token), "POST", body.as_bytes()).await, 200);
    assert_eq!(seen.lock().unwrap().as_slice(), &[(r.store.to_string(), "c/obj.txt".to_string())]);
}

#[tokio::test]
async fn only_the_right_path_method_size_and_body_are_served() {
    let r = rig().await;
    let ok = format!("/store/ingest/{}/audit", r.rx.token);
    assert_eq!(post(&r.rx, "/store/ingest/wrong-token/audit", "POST", b"{}").await, 404, "a wrong token is a plain 404");
    assert_eq!(post(&r.rx, "/store/ingest/audit", "POST", b"{}").await, 404);
    assert_eq!(post(&r.rx, "/store/ingest/x/other", "POST", b"{}").await, 404);
    assert_eq!(post(&r.rx, &ok, "GET", b"").await, 405);
    assert_eq!(post(&r.rx, &ok, "POST", b"not json at all").await, 400);
    assert_eq!(post(&r.rx, &ok, "POST", &vec![b'x'; MAX_BODY + 1]).await, 413);
}

#[tokio::test]
async fn only_loopback_is_accepted_and_the_token_is_private() {
    let r = rig().await;
    assert!(serve(r.svc.clone(), "0.0.0.0:0".parse().unwrap(), "t".repeat(64)).await.unwrap_err().contains("loopback"));
    assert!(serve(r.svc.clone(), "192.0.2.1:0".parse().unwrap(), "t".repeat(64)).await.is_err());
    let d = tempfile::tempdir().unwrap();
    let t = load_token(d.path()).unwrap();
    assert_eq!(t.len(), 64);
    assert_eq!(load_token(d.path()).unwrap(), t, "stable across restarts");
    #[cfg(unix)]
    assert_eq!(std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(d.path().join("store-ingest-token")).unwrap().permissions()) & 0o777, 0o600);
    assert!(same(&t, &t) && !same(&t, &"0".repeat(64)) && !same("a", "ab"));
}

// ---- against a real RustFS ------------------------------------------------------------------
// WEFTOS_STORE_TEST_RUSTFS_BIN is the rustfs binary; the test starts its own THROWAWAY instance on
// loopback with the audit and notify env of docs/guides/rustfs-audit-deploy.md, pointed at this
// receiver, and sends a request straight to RustFS with a cog's own key. Run with `--ignored`.
#[test]
#[ignore = "needs a rustfs binary: WEFTOS_STORE_TEST_RUSTFS_BIN"]
fn live_a_request_straight_to_rustfs_reaches_the_activity_log() {
    use std::process::{Command, Stdio};
    use weftos_store::{AdminBackend, ObjectBackend, RustfsBackend, RustfsConfig};

    let bin = std::env::var("WEFTOS_STORE_TEST_RUSTFS_BIN").expect("WEFTOS_STORE_TEST_RUSTFS_BIN");
    let free = || std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let (p_rfs, p_rx) = (free(), free());
    let d = tempfile::tempdir().unwrap();
    for sub in ["data", "aq", "nq", "secrets"] {
        std::fs::create_dir(d.path().join(sub)).unwrap();
    }
    let (access, secret) = ("liveroot".to_owned() + &ulid::Ulid::new().to_string()[..8].to_lowercase(), ulid::Ulid::new().to_string() + &ulid::Ulid::new().to_string());
    let cfg = RustfsConfig {
        endpoint: format!("http://127.0.0.1:{p_rfs}"),
        region: "us-east-1".into(),
        root_access_key: access.clone(),
        root_secret_key: secret.clone(),
        secrets_dir: d.path().join("secrets"),
        timeout: Duration::from_secs(20),
    };
    let be = Arc::new(RustfsBackend::new(cfg.clone()).unwrap());
    let sink = Arc::new(MemSink::default());
    let svc = Arc::new(StoreService::open(Config { node_id: "pg-live".into(), state_path: None, clock: || 1_000 }, be.clone(), be.clone(), Arc::new(Open), sink.clone(), Arc::new(NoChain)).unwrap());
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let rx = rt.block_on(serve(svc.clone(), format!("127.0.0.1:{p_rx}").parse().unwrap(), "f".repeat(64))).unwrap();

    let origin = format!("http://127.0.0.1:{p_rx}");
    let mut child = Command::new(bin)
        .args(["server", "--address", &format!("127.0.0.1:{p_rfs}")])
        .arg(d.path().join("data"))
        .env("RUSTFS_ACCESS_KEY", &access)
        .env("RUSTFS_SECRET_KEY", &secret)
        .env("RUSTFS_OUTBOUND_ALLOW_ORIGINS", &origin)
        .env("RUSTFS_AUDIT_ENABLE", "true")
        .env("RUSTFS_AUDIT_WEBHOOK_ENABLE_PRIMARY", "on")
        .env("RUSTFS_AUDIT_WEBHOOK_ENDPOINT_PRIMARY", rx.url("audit"))
        .env("RUSTFS_AUDIT_WEBHOOK_QUEUE_DIR_PRIMARY", d.path().join("aq"))
        .env("RUSTFS_NOTIFY_ENABLE", "true")
        .env("RUSTFS_NOTIFY_WEBHOOK_ENABLE_PRIMARY", "on")
        .env("RUSTFS_NOTIFY_WEBHOOK_ENDPOINT_PRIMARY", rx.url("notify"))
        .env("RUSTFS_NOTIFY_WEBHOOK_QUEUE_DIR_PRIMARY", d.path().join("nq"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let up = (0..60).any(|_| {
            std::thread::sleep(Duration::from_millis(500));
            be.bucket_exists("probe-up").is_ok()
        });
        assert!(up, "rustfs did not come up");
        let store = StoreName::project("weft", &ulid::Ulid::new().to_string()).unwrap();
        svc.create_store(&store, "daemon", Parties::default()).unwrap();
        let cog = Principal::parse("cog:live").unwrap();
        svc.grant(&store, cog.clone(), Access::Write, "op").unwrap();
        be.put_bucket_notification(&store.bucket(), "primary").unwrap();

        // Straight to RustFS with the cog's own key: the service never sees this request.
        let key_id = RustfsBackend::key_id_for("cog:live");
        let direct = RustfsBackend::new(RustfsConfig { root_access_key: key_id.clone(), root_secret_key: std::fs::read_to_string(be.secret_path(&key_id)).unwrap(), ..cfg.clone() }).unwrap();
        direct.put(&store.bucket(), "direct/obj.txt", b"hello", false).unwrap();

        let want = |src: Source| sink.events().iter().any(|e| e.principal == "cog:live" && e.source == src && e.key.as_deref() == Some("direct/obj.txt") && e.store == store.to_string());
        let ok = (0..60).any(|_| {
            std::thread::sleep(Duration::from_millis(500));
            want(Source::RustfsAudit) && want(Source::Notification)
        });
        assert!(ok, "events seen: {:?}", sink.events().iter().map(|e| (e.principal.clone(), e.op.clone(), e.source)).collect::<Vec<_>>());
        // The daemon's own root-key requests (create_bucket, put_bucket_notification) are not repeated.
        assert!(!sink.events().iter().any(|e| e.source == Source::RustfsAudit && e.principal == access));
    }));
    let _ = child.kill();
    let _ = child.wait();
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

#[tokio::test]
async fn a_notification_for_one_of_our_stores_wakes_the_sink_once_with_the_whole_body() {
    let w = Arc::new(Wakes::default());
    let r = rig_with_wake(w.clone()).await;
    let path = format!("/store/ingest/{}/notify", r.rx.token);
    let body = notify_body(&r.store.bucket(), "registry/wt-a.json", "s3:ObjectCreated:Put");
    assert_eq!(post(&r.rx, &path, "POST", body.as_bytes()).await, 200);
    let got = w.0.lock().unwrap().clone();
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!((got[0].0.as_str(), got[0].1.as_str()), (r.store.to_string().as_str(), r.store.bucket().as_str()));
    assert_eq!(got[0].2, body, "the sink gets the notification as it came, to decide for itself (registry_hints)");
}

#[tokio::test]
async fn notifications_for_other_buckets_garbage_and_the_audit_stream_do_not_wake_the_sink() {
    let w = Arc::new(Wakes::default());
    let r = rig_with_wake(w.clone()).await;
    let notify = format!("/store/ingest/{}/notify", r.rx.token);
    let audit = format!("/store/ingest/{}/audit", r.rx.token);
    assert_eq!(post(&r.rx, &notify, "POST", notify_body("someone-elses-bucket", "k", "s3:ObjectCreated:Put").as_bytes()).await, 200);
    assert_eq!(post(&r.rx, &notify, "POST", b"not json").await, 400);
    assert_eq!(post(&r.rx, &audit, "POST", put_record(&key_of(&r), &r.store.bucket()).as_bytes()).await, 200);
    assert_eq!(post(&r.rx, &format!("/store/ingest/{}/notify", "x".repeat(64)), "POST", notify_body(&r.store.bucket(), "k", "s3:ObjectCreated:Put").as_bytes()).await, 404, "wrong token");
    assert!(w.0.lock().unwrap().is_empty(), "{:?}", w.0.lock().unwrap());
}

#[tokio::test]
async fn a_sink_that_panics_does_not_change_what_rustfs_is_told() {
    let r = rig_with_wake(Arc::new(Boom)).await;
    let path = format!("/store/ingest/{}/notify", r.rx.token);
    assert_eq!(post(&r.rx, &path, "POST", notify_body(&r.store.bucket(), "k", "s3:ObjectCreated:Put").as_bytes()).await, 200);
    // The receiver still serves the next request.
    assert_eq!(post(&r.rx, &path, "POST", notify_body(&r.store.bucket(), "k2", "s3:ObjectCreated:Put").as_bytes()).await, 200);
}

#[tokio::test]
async fn refusals_posted_to_the_audit_url_suspend_the_key_through_the_service() {
    let r = rig().await;
    r.svc.set_denial_rule(Some(weftos_store::service::DenialRule { max: 3, window_ms: 60_000 }));
    let key = key_of(&r);
    let denied = |status: u16| format!(r#"{{"EventName":"s3:GetObject","Key":"x","Records":[{{"accessKey":"{key}","api":{{"name":"s3:GetObject","bucket":"{}","object":"x","status_code":{status}}}}}]}}"#, r.store.bucket());
    let path = format!("/store/ingest/{}/audit", r.rx.token);
    for _ in 0..2 {
        assert_eq!(post(&r.rx, &path, "POST", denied(403).as_bytes()).await, 200);
    }
    assert_eq!(r.be.key_enabled(&key), Some(true));
    assert_eq!(post(&r.rx, &path, "POST", denied(403).as_bytes()).await, 200);
    assert_eq!(r.be.key_enabled(&key), Some(false), "the third refusal disabled the key at the backend, by way of suspend_principal");
    // A wrong token reaches nothing, so it can neither log nor count.
    assert_eq!(post(&r.rx, "/store/ingest/nope/audit", "POST", denied(403).as_bytes()).await, 404);
}
