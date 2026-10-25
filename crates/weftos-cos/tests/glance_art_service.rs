//! `glance_art::generate` against a fake image service: the request the
//! service sees (bearer token, steps, style, negative prompt) and the bitmap
//! that comes back.

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::post;
use serde_json::Value;
use weftos_cos::config::ArtConfig;
use weftos_cos::glance_art::{self, IMG_BYTES};

type Seen = Arc<Mutex<Vec<(Option<String>, Value)>>>;

fn png() -> Vec<u8> {
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, 512, 512);
    enc.set_color(png::ColorType::Grayscale);
    enc.set_depth(png::BitDepth::Eight);
    let mut w = enc.write_header().unwrap();
    w.write_image_data(&(0..512 * 512).map(|i| if (i % 512) < 256 { 0u8 } else { 255 }).collect::<Vec<_>>()).unwrap();
    drop(w);
    out
}

async fn handler(State(seen): State<Seen>, headers: HeaderMap, axum::Json(body): axum::Json<Value>) -> impl IntoResponse {
    let auth = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).map(String::from);
    let ok = auth.as_deref() == Some("Bearer tok-123");
    seen.lock().unwrap().push((auth, body));
    if ok { ([(header::CONTENT_TYPE, "image/png")], png()).into_response() } else { StatusCode::UNAUTHORIZED.into_response() }
}

async fn serve() -> (String, Seen) {
    let seen: Seen = Arc::default();
    let app = Router::new().route("/v1/generate", post(handler)).with_state(seen.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (url, seen)
}

fn token_file(dir: &tempfile::TempDir, mode: u32, body: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.path().join("tok");
    std::fs::write(&p, body).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
    p
}

#[tokio::test]
async fn sends_the_documented_request_and_dithers_the_png() {
    let (url, seen) = serve().await;
    let dir = tempfile::tempdir().unwrap();
    let cfg = ArtConfig { url: Some(url), token_file: token_file(&dir, 0o600, "tok-123\n") };
    let scene = glance_art::scene(3);
    let img = glance_art::generate(&cfg, &scene, 3).await.unwrap();
    assert_eq!(img.bitmap.len(), IMG_BYTES);
    assert_eq!(img.caption, scene.caption);
    // Left half black (ink), right half white.
    assert_eq!(img.bitmap[0], 0xff);
    assert_eq!(img.bitmap[31], 0x00);
    glance_art::validate_line(&img.to_line("09:00")).unwrap();

    let seen = seen.lock().unwrap();
    let (auth, body) = &seen[0];
    assert_eq!(auth.as_deref(), Some("Bearer tok-123"));
    assert_eq!(body["steps"], 12);
    assert!(body["prompt"].as_str().unwrap().ends_with(", woodcut, black ink on white paper, high contrast"));
    assert_eq!(body["negative_prompt"], "color, gray, shading, photo, blurry");
}

#[tokio::test]
async fn failures_are_errors_that_never_echo_the_token() {
    let (url, _seen) = serve().await;
    let dir = tempfile::tempdir().unwrap();
    // Wrong token: the service answers 401.
    let cfg = ArtConfig { url: Some(url.clone()), token_file: token_file(&dir, 0o600, "wrong-token") };
    let e = glance_art::generate(&cfg, &glance_art::scene(1), 1).await.unwrap_err();
    assert!(e.contains("401") && !e.contains("wrong-token"), "{e}");
    // World-readable token file: refused before any request.
    let cfg = ArtConfig { url: Some(url), token_file: token_file(&dir, 0o644, "tok-123") };
    let e = glance_art::generate(&cfg, &glance_art::scene(1), 1).await.unwrap_err();
    assert!(e.contains("mode 600") && !e.contains("tok-123"), "{e}");
    // No url: art is off.
    assert!(glance_art::generate(&ArtConfig { url: None, ..cfg.clone() }, &glance_art::scene(1), 1).await.is_err());
    // Service down: a clean error.
    let down = ArtConfig { url: Some("http://127.0.0.1:1".into()), token_file: token_file(&dir, 0o600, "tok-123") };
    assert!(glance_art::generate(&down, &glance_art::scene(1), 1).await.is_err());
}
