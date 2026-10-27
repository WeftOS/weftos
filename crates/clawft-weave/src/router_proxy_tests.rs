//! The router end to end on loopback: a hyper test upstream (echo, health,
//! `/processes`, and a byte-echo on any `Upgrade`), the router started with
//! `start_with` on an ephemeral port, temp dirs only.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use http_body_util::{BodyExt, Empty, Full};
use hyper::body::{Bytes, Incoming};
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::router_cfg::RouterConfig;
use crate::router_sources::{Candidate, DirsSource, PORTS_FILE};
use crate::router_state::{RouterHandle, start_with};

type Body = http_body_util::combinators::BoxBody<Bytes, hyper::Error>;

fn full(s: String) -> Body {
    Full::new(Bytes::from(s)).map_err(|never| match never {}).boxed()
}

async fn upstream_handler(mut req: Request<Incoming>) -> Result<Response<Body>, hyper::Error> {
    let path = req.uri().path().to_owned();
    if req.headers().contains_key(hyper::header::UPGRADE) {
        let on = hyper::upgrade::on(&mut req);
        tokio::spawn(async move {
            if let Ok(up) = on.await {
                let mut io = TokioIo::new(up);
                let mut buf = [0u8; 1024];
                while let Ok(n) = io.read(&mut buf).await {
                    if n == 0 || io.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            }
        });
        let resp = Response::builder()
            .status(StatusCode::SWITCHING_PROTOCOLS)
            .header("upgrade", "websocket")
            .header("connection", "Upgrade")
            .body(Empty::<Bytes>::new().map_err(|never| match never {}).boxed())
            .unwrap();
        return Ok(resp);
    }
    if path == "/processes" {
        return Ok(Response::new(full(r#"{"data":[{"name":"web","status":"Running","is_ready":"Ready"}]}"#.into())));
    }
    if path == "/health" {
        return Ok(Response::new(full("ok".into())));
    }
    let h = |k: &str| req.headers().get(k).and_then(|v| v.to_str().ok()).unwrap_or("").to_owned();
    let mut echo = serde_json::json!({
        "path": req.uri().path_and_query().map(|p| p.as_str().to_owned()),
        "method": req.method().as_str(),
        "host": h("host"), "xfh": h("x-forwarded-host"), "xfp": h("x-forwarded-proto"), "xfpre": h("x-forwarded-prefix"),
        "xff": h("x-forwarded-for"), "connection": h("connection"),
        "login": h("tailscale-user-login"), "name": h("tailscale-user-name"), "forged": h("tailscale-forged"),
    });
    let body = req.into_body().collect().await?.to_bytes();
    echo["body_len"] = serde_json::json!(body.len());
    Ok(Response::builder().header("content-type", "application/json").header("x-upstream", "echo").body(full(echo.to_string())).unwrap())
}

async fn upstream() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((s, _)) = l.accept().await {
            tokio::spawn(async move {
                let svc = hyper::service::service_fn(upstream_handler);
                let _ = hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(s), svc).with_upgrades().await;
            });
        }
    });
    port
}

async fn closed_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap().port()
}

struct Rig {
    h: Arc<RouterHandle>,
    dir: tempfile::TempDir,
    up: u16,
}

impl Rig {
    fn write(&self, yaml: &str) {
        let proj = self.dir.path().join("app");
        std::fs::create_dir_all(proj.join("compose")).unwrap();
        std::fs::write(proj.join(PORTS_FILE), yaml).unwrap();
    }
}

async fn rig(yaml: impl FnOnce(u16) -> String) -> Rig {
    let up = upstream().await;
    let dir = tempfile::tempdir().unwrap();
    let proj = dir.path().join("app");
    std::fs::create_dir_all(proj.join("compose")).unwrap();
    std::fs::write(proj.join(PORTS_FILE), yaml(up)).unwrap();
    let cfg = RouterConfig { enabled: true, listen: "127.0.0.1:0".into(), poll_secs: 1, health_timeout_ms: 500 };
    let source = DirsSource::new(vec![Candidate { name: "app".into(), dir: proj, ulid: None }]);
    let h = start_with(cfg, Box::new(source), Duration::from_millis(50)).await.unwrap();
    Rig { h, dir, up }
}

async fn send<B>(addr: SocketAddr, req: Request<B>) -> (StatusCode, hyper::HeaderMap, String)
where
    B: hyper::body::Body + Send + 'static,
    B::Data: Send,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let s = TcpStream::connect(addr).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(s)).await.unwrap();
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let resp = sender.send_request(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    (status, headers, String::from_utf8_lossy(&body).into_owned())
}

fn get(path: &str) -> Request<Empty<Bytes>> {
    Request::builder().uri(path).header("host", "machine.example.ts.net").body(Empty::new()).unwrap()
}

fn basic(up: u16) -> String {
    let mut y = format!("project: app\nclaims:\n  - {{ port: {up}, use: process-compose-http }}\nroutes:\n");
    y += &format!("  - {{ prefix: /app, port: {up}, health: /health }}\n");
    y += &format!("  - {{ prefix: /app/deep, port: {up} }}\n");
    y
}

#[tokio::test]
async fn proxies_with_forwarded_headers_and_keeps_the_prefix() {
    let r = rig(basic).await;
    let mut req = get("/app/page?x=1");
    req.headers_mut().insert("x-forwarded-for", "100.64.0.9".parse().unwrap());
    let (status, headers, body) = send(r.h.bound, req).await;
    assert_eq!(status, 200);
    assert_eq!(headers.get("x-upstream").unwrap(), "echo");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["path"], "/app/page?x=1", "prefix kept, query kept");
    assert_eq!(v["host"], "machine.example.ts.net", "Host passes through as Tailscale Serve sends it");
    assert_eq!(v["xfh"], "machine.example.ts.net");
    assert_eq!(v["xfp"], "https");
    assert_eq!(v["xfpre"], "/app");
    assert_eq!(v["xff"], "100.64.0.9, 127.0.0.1", "the tailnet client from Serve is kept; this hop is appended");
    assert_eq!(v["connection"], "", "hop-by-hop headers are not forwarded");
}

#[tokio::test]
async fn longest_prefix_wins_and_a_body_streams_through() {
    let r = rig(basic).await;
    let (_, _, body) = send(r.h.bound, get("/app/deep/x")).await;
    assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap()["xfpre"], "/app/deep");
    let (_, _, body) = send(r.h.bound, get("/app/deeper")).await;
    assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap()["xfpre"], "/app");
    let payload = vec![b'z'; 300_000];
    let post = Request::builder().method("POST").uri("/app/upload").header("host", "m.example.ts.net").body(Full::new(Bytes::from(payload))).unwrap();
    let (status, _, body) = send(r.h.bound, post).await;
    assert_eq!(status, 200);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap()["body_len"], 300_000);
}

#[tokio::test]
async fn unmatched_paths_are_404_unless_a_default_route_holds_the_root() {
    let r = rig(basic).await;
    let (status, _, body) = send(r.h.bound, get("/elsewhere")).await;
    assert_eq!(status, 404);
    assert!(body.contains("No project routes") && body.contains("/_weftos/"), "{body}");
    let (status, headers, _) = send(r.h.bound, get("/")).await;
    assert_eq!(status, 302, "with no default route, / goes to the index");
    assert_eq!(headers.get("location").and_then(|v| v.to_str().ok()), Some("/_weftos/"));
    r.write(&format!("project: app\nroutes:\n  - {{ prefix: /app, port: {}, default: true }}\n", r.up));
    r.h.reload();
    let (status, _, body) = send(r.h.bound, get("/")).await;
    assert_eq!(status, 200);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap()["xfpre"], "/", "root traffic carries prefix /");
    let (status, _, _) = send(r.h.bound, get("/_weftos/routes.json")).await;
    assert_eq!(status, 200, "the index is never shadowed by the default route");
}

#[tokio::test]
async fn websocket_upgrade_is_tunnelled_both_ways() {
    let r = rig(basic).await;
    let s = TcpStream::connect(r.h.bound).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(s)).await.unwrap();
    tokio::spawn(async move {
        let _ = conn.with_upgrades().await;
    });
    let req = Request::builder()
        .uri("/app/_next/webpack-hmr")
        .header("host", "machine.example.ts.net")
        .header("upgrade", "websocket")
        .header("connection", "Upgrade")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
        .header("sec-websocket-version", "13")
        .body(Empty::<Bytes>::new())
        .unwrap();
    let resp = sender.send_request(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SWITCHING_PROTOCOLS);
    assert_eq!(resp.headers().get("upgrade").unwrap(), "websocket");
    let mut io = TokioIo::new(hyper::upgrade::on(resp).await.unwrap());
    io.write_all(b"ping through the router").await.unwrap();
    let mut buf = vec![0u8; 64];
    let n = tokio::time::timeout(Duration::from_secs(3), io.read(&mut buf)).await.unwrap().unwrap();
    assert_eq!(&buf[..n], b"ping through the router");
}

#[tokio::test]
async fn a_down_upstream_gets_a_502_naming_the_project() {
    let dead = closed_port().await;
    let r = rig(|_| format!("project: sleepy\nroutes:\n  - {{ prefix: /sleepy, port: {dead} }}\n")).await;
    let (status, headers, body) = send(r.h.bound, get("/sleepy/")).await;
    assert_eq!(status, 502);
    assert!(headers.get("content-type").unwrap().to_str().unwrap().starts_with("text/html"));
    assert!(body.contains("<strong>sleepy</strong>") && body.contains(&format!("127.0.0.1:{dead}")) && body.contains("not running"), "{body}");
}

#[tokio::test]
async fn the_index_lists_routes_health_and_process_compose_state() {
    let r = rig(basic).await;
    let (status, _, body) = send(r.h.bound, get("/_weftos/routes.json")).await;
    assert_eq!(status, 200);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["enabled"], true);
    let deep = &v["routes"][0];
    assert_eq!(deep["prefix"], "/app/deep");
    assert_eq!(deep["health"]["state"], "unknown", "no health path declared");
    let app = &v["routes"][1];
    assert_eq!(app["health"]["state"], "ok", "{app}");
    assert_eq!(v["projects"][0]["slug"], "app");
    assert_eq!(v["projects"][0]["process_compose"]["running"], 1, "{}", v["projects"][0]);
    assert_eq!(v["projects"][0]["process_compose"]["processes"][0]["name"], "web");
    let (status, _, html) = send(r.h.bound, get("/_weftos/")).await;
    assert_eq!(status, 200);
    assert!(html.contains("href=\"/app/\"") && html.contains("1/1 running") && html.contains("routes.json"), "{html}");
}

fn restricted(up: u16) -> String {
    format!("{}  - {{ prefix: /app/admin, port: {up}, allow: [alice@example.com, bob@example.com] }}\n", basic(up))
}

fn with_login(path: &str, login: Option<&str>) -> Request<Empty<Bytes>> {
    let mut req = get(path);
    if let Some(l) = login {
        req.headers_mut().insert("tailscale-user-login", l.parse().unwrap());
    }
    req
}

#[tokio::test]
async fn an_allow_list_admits_listed_logins_case_insensitively_and_forwards_the_identity() {
    let r = rig(restricted).await;
    let (status, _, body) = send(r.h.bound, with_login("/app/admin/x", Some("Alice@Example.COM"))).await;
    assert_eq!(status, 200, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["login"], "Alice@Example.COM", "the login goes upstream as Serve sent it");
    let (status, _, _) = send(r.h.bound, with_login("/app/admin", Some("bob@example.com"))).await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn a_login_not_listed_and_a_missing_login_get_the_same_403_naming_the_route() {
    let r = rig(restricted).await;
    let (status, headers, denied) = send(r.h.bound, with_login("/app/admin/x", Some("mallory@example.com"))).await;
    assert_eq!(status, 403);
    assert!(headers.get("content-type").unwrap().to_str().unwrap().starts_with("text/html"));
    assert!(denied.contains("<code>/app/admin/</code>") && denied.contains("<strong>app</strong>"), "{denied}");
    assert!(!denied.contains("alice") && !denied.contains("bob") && !denied.contains("mallory"), "the list and the caller stay out of the page: {denied}");
    let (status, _, missing) = send(r.h.bound, with_login("/app/admin/x", None)).await;
    assert_eq!(status, 403, "a tagged device (no login header) is refused");
    assert_eq!(missing, denied, "one uniform page");
    let (status, _, empty) = send(r.h.bound, with_login("/app/admin/x", Some("  "))).await;
    assert_eq!((status, empty == denied), (StatusCode::FORBIDDEN, true));
    // A prefix match on a longer open route is not shadowed: `/app/administrator` is `/app`.
    let (status, _, _) = send(r.h.bound, with_login("/app/administrator", None)).await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn a_route_without_allow_stays_open_and_forged_tailscale_headers_are_stripped() {
    let r = rig(restricted).await;
    let (status, _, body) = send(r.h.bound, with_login("/app/page", None)).await;
    assert_eq!(status, 200);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap()["login"], "");
    let mut req = with_login("/app/page", Some("carol@example.com"));
    req.headers_mut().insert("tailscale-user-name", "Carol".parse().unwrap());
    req.headers_mut().insert("tailscale-forged", "1".parse().unwrap());
    req.headers_mut().insert("Tailscale-Other-Thing", "2".parse().unwrap());
    let (status, _, body) = send(r.h.bound, req).await;
    assert_eq!(status, 200);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["login"], "carol@example.com");
    assert_eq!(v["name"], "Carol");
    assert_eq!(v["forged"], "", "a Tailscale-* header Serve does not set is dropped");
    let (_, _, idx) = send(r.h.bound, get("/_weftos/routes.json")).await;
    let v: serde_json::Value = serde_json::from_str(&idx).unwrap();
    let admin = v["routes"].as_array().unwrap().iter().find(|x| x["prefix"] == "/app/admin").cloned().unwrap();
    assert_eq!(admin["restricted"], true);
    assert!(admin.get("allow").is_none() && !idx.contains("alice"), "the index never lists logins: {idx}");
    let (_, _, html) = send(r.h.bound, get("/_weftos/")).await;
    assert!(html.contains("(restricted)"), "{html}");
}

#[tokio::test]
async fn a_changed_ports_file_is_picked_up_by_the_poller() {
    let r = rig(basic).await;
    let gen0 = r.h.generation();
    let (status, _, _) = send(r.h.bound, get("/second/")).await;
    assert_eq!(status, 404);
    tokio::time::sleep(Duration::from_millis(20)).await;
    r.write(&format!("{}  - {{ prefix: /second, port: {} }}\n", basic(r.up), r.up));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while r.h.generation() == gen0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(r.h.generation() > gen0, "poller did not reload after the mtime change");
    let (status, _, body) = send(r.h.bound, get("/second/x")).await;
    assert_eq!(status, 200);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap()["xfpre"], "/second");
    assert!(!r.h.reload_if_changed(), "an unchanged source does not reload again");
}

#[tokio::test]
async fn stop_accepting_closes_the_listener() {
    // Daemon shutdown closes the router first (card 1a919ae6), while later
    // steps may still be running.
    let r = rig(basic).await;
    let (status, _, _) = send(r.h.bound, get("/app/x")).await;
    assert_eq!(status, StatusCode::OK);
    r.h.stop_accepting();
    for _ in 0..200 {
        if TcpStream::connect(r.h.bound).await.is_err() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("router still accepting after stop_accepting");
}
