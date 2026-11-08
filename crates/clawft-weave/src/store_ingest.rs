//! The daemon's receiver for RustFS's audit stream and bucket notifications (ADR-126 S1), so a
//! request made straight to RustFS with a principal's own key reaches the activity log.
//!
//! RustFS POSTs both streams to one URL, `http://127.0.0.1:<port>/store/ingest/<token>/audit` and
//! `.../notify`; `weftos-store` turns each body into activity-log events (and cache invalidations).
//!
//! - **Off by default.** It runs only when `WEFTOS_STORE_INGEST_ADDR` is set (for example
//!   `127.0.0.1:7788`; port 0 picks one) and the object store is the RustFS backend.
//! - **Loopback only.** A non-loopback address is refused: RustFS and the daemon share a host.
//! - **Authenticated by a secret in the path.** RustFS's webhook `auth_token` is not verified
//!   against 1.0.4, so the secret is part of the URL. It is 256 random bits in `store-ingest-token`
//!   (0600) under the daemon's runtime dir; the full URLs are written to `store-ingest-urls` (0600).
//!   A wrong token is a 404, the same as any other path.
//! - **Bounded.** POST only, a body of at most 1 MiB, 10 s to deliver it.
//! - Records for buckets that are not this daemon's stores are dropped, and so is the daemon's own
//!   root-key traffic (the governed path already logged it): see `StoreService::event_from`.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use http_body_util::{BodyExt, Full, Limited};
use hyper::body::{Bytes, Incoming};
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use weftos_store::StoreService;

/// The most a webhook body may carry.
pub const MAX_BODY: usize = 1024 * 1024;
const BODY_TIME: Duration = Duration::from_secs(10);

/// Told about every bucket notification that concerns one of this daemon's stores, after it has
/// been logged. This is the hook for the follower wake-ups of ADR-126 §4a (S2): an implementation
/// calls `weftos_store::wake::registry_hints(body, bucket)` and, when that is non-empty, publishes a
/// wake message on the project's mesh relay topic. A wake is only a hint, so losing one is fine.
///
/// `notified` runs on a blocking thread, once per store bucket the body touches; keep it quick and do
/// not hold the receiver up on the network (hand off to a channel). A panic in it is caught and
/// logged, and does not change the answer to RustFS.
pub trait WakeSink: Send + Sync {
    /// `store` is the `weftos://` name, `bucket` its bucket, `body` the whole notification JSON.
    fn notified(&self, store: &str, bucket: &str, body: &str);
}

/// Where a running receiver listens and the secret it checks.
#[derive(Debug, Clone)]
pub struct Receiver {
    pub addr: SocketAddr,
    pub token: String,
}

impl Receiver {
    /// `http://<addr>/store/ingest/<token>/<stream>`.
    pub fn url(&self, stream: &str) -> String {
        format!("http://{}/store/ingest/{}/{stream}", self.addr, self.token)
    }
}

/// Write a file only its owner can read, through a temp file and a rename.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    let mut f = o.open(&tmp)?;
    f.write_all(bytes).and_then(|()| f.sync_all())?;
    std::fs::rename(&tmp, path)
}

/// Read the receiver's token from `dir`, making it (0600) the first time.
pub fn load_token(dir: &Path) -> Result<String, String> {
    let p = dir.join("store-ingest-token");
    if let Ok(t) = std::fs::read_to_string(&p) {
        let t = t.trim().to_owned();
        if t.len() >= 32 && t.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Ok(t);
        }
    }
    let t: String = rand::random::<[u8; 32]>().iter().map(|b| format!("{b:02x}")).collect();
    write_private(&p, t.as_bytes()).map_err(|e| format!("{}: {e}", p.display()))?;
    Ok(t)
}

/// Start the receiver on `addr` (loopback). Returns where it listens.
pub async fn serve(svc: Arc<StoreService>, addr: SocketAddr, token: String) -> Result<Receiver, String> {
    serve_with(svc, addr, token, None).await
}

/// [`serve`], with a [`WakeSink`] to tell about notifications for this daemon's stores.
pub async fn serve_with(svc: Arc<StoreService>, addr: SocketAddr, token: String, wake: Option<Arc<dyn WakeSink>>) -> Result<Receiver, String> {
    if !addr.ip().is_loopback() {
        return Err(format!("store ingest address {addr}: loopback only"));
    }
    let listener = TcpListener::bind(addr).await.map_err(|e| format!("store ingest bind {addr}: {e}"))?;
    let addr = listener.local_addr().map_err(|e| e.to_string())?;
    let rx = Receiver { addr, token: token.clone() };
    let token: Arc<str> = token.into();
    tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(x) => x,
                Err(e) => {
                    tracing::warn!(error = %e, "store ingest accept failed");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            };
            let (svc, token, wake) = (svc.clone(), token.clone(), wake.clone());
            tokio::spawn(async move {
                let f = hyper::service::service_fn(move |req| handle(req, svc.clone(), token.clone(), wake.clone()));
                if let Err(e) = hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(stream), f).await {
                    tracing::debug!(error = %e, "store ingest connection ended");
                }
            });
        }
    });
    Ok(rx)
}

fn reply(status: StatusCode, body: &'static str) -> Response<Full<Bytes>> {
    let mut r = Response::new(Full::new(Bytes::from_static(body.as_bytes())));
    *r.status_mut() = status;
    r
}

/// Constant-time comparison, so a wrong token leaks nothing about a right one.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

async fn handle(req: Request<Incoming>, svc: Arc<StoreService>, token: Arc<str>, wake: Option<Arc<dyn WakeSink>>) -> Result<Response<Full<Bytes>>, std::convert::Infallible> {
    // /store/ingest/<token>/<stream>
    let path = req.uri().path().to_owned();
    let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let stream = match parts.as_slice() {
        ["store", "ingest", t, "audit"] if same(t, &token) => "audit",
        ["store", "ingest", t, "notify"] if same(t, &token) => "notify",
        _ => return Ok(reply(StatusCode::NOT_FOUND, "not found")),
    };
    if req.method() != Method::POST {
        return Ok(reply(StatusCode::METHOD_NOT_ALLOWED, "POST only"));
    }
    let body = match tokio::time::timeout(BODY_TIME, Limited::new(req.into_body(), MAX_BODY).collect()).await {
        Ok(Ok(b)) => b.to_bytes(),
        Ok(Err(_)) => return Ok(reply(StatusCode::PAYLOAD_TOO_LARGE, "body too large")),
        Err(_) => return Ok(reply(StatusCode::REQUEST_TIMEOUT, "body timed out")),
    };
    let Ok(text) = String::from_utf8(body.to_vec()) else { return Ok(reply(StatusCode::BAD_REQUEST, "not utf-8")) };
    // The service calls are quick but take locks and may reach a backend: not on the reactor.
    let done = tokio::task::spawn_blocking(move || ingest(&svc, stream, &text, wake.as_deref())).await.unwrap_or(false);
    Ok(if done { reply(StatusCode::OK, "ok") } else { reply(StatusCode::BAD_REQUEST, "not a RustFS record") })
}

/// Hand one body to the service. False if it was not a record of that stream.
fn ingest(svc: &StoreService, stream: &str, body: &str, wake: Option<&dyn WakeSink>) -> bool {
    match stream {
        "audit" => {
            // One JSON object, or newline-separated ones.
            let mut any = false;
            for line in body.lines().filter(|l| !l.trim().is_empty()) {
                any |= svc.ingest_audit_line(line);
            }
            any
        }
        _ => {
            let recs = weftos_store::audit::parse_notification(body);
            if recs.is_empty() {
                return false;
            }
            svc.ingest_notification(body);
            if let Some(w) = wake {
                tell(svc, w, &recs, body);
            }
            true
        }
    }
}

/// Tell the sink once per store whose bucket the notification names. Records for buckets that are
/// not this daemon's stores are not passed on, as with the activity log.
fn tell(svc: &StoreService, w: &dyn WakeSink, recs: &[weftos_store::audit::Raw], body: &str) {
    let mut seen = std::collections::BTreeSet::new();
    for name in svc.store_names() {
        let Ok(parsed) = weftos_store::StoreName::parse(&name) else { continue };
        let bucket = parsed.bucket();
        if recs.iter().any(|r| r.bucket == bucket) && seen.insert(bucket.clone()) {
            let called = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| w.notified(&name, &bucket, body)));
            if called.is_err() {
                tracing::warn!(store = %name, "store ingest wake sink panicked; ignored");
            }
        }
    }
}

/// Start the receiver if `WEFTOS_STORE_INGEST_ADDR` is set. Writes the URLs to `dir/store-ingest-urls`.
pub async fn start_from_env(svc: Arc<StoreService>, dir: &Path) -> Result<Option<Receiver>, String> {
    start_from_env_with(svc, dir, None).await
}

/// [`start_from_env`], with a [`WakeSink`] (the daemon passes one once the mesh relay can carry wake messages).
pub async fn start_from_env_with(svc: Arc<StoreService>, dir: &Path, wake: Option<Arc<dyn WakeSink>>) -> Result<Option<Receiver>, String> {
    let Some(a) = std::env::var("WEFTOS_STORE_INGEST_ADDR").ok().filter(|v| !v.is_empty()) else { return Ok(None) };
    let addr: SocketAddr = a.parse().map_err(|e| format!("WEFTOS_STORE_INGEST_ADDR {a:?}: {e}"))?;
    let rx = serve_with(svc, addr, load_token(dir)?, wake).await?;
    let urls = format!("{}\n{}\n", rx.url("audit"), rx.url("notify"));
    write_private(&dir.join("store-ingest-urls"), urls.as_bytes()).map_err(|e| e.to_string())?;
    tracing::info!(addr = %rx.addr, "store ingest listening (URLs in store-ingest-urls)");
    Ok(Some(rx))
}

#[cfg(test)]
#[path = "store_ingest_tests.rs"]
mod tests;
