//! HTTP surface (DESIGN.md §3 + capture-protocol-v1 image-set ingest).

use crate::state::SharedState;
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderValue, Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use clawft_splat_pipeline::{
    JobRecord, SESSION_DIR_NAME, is_session_dir, normalize_session_root, now_ms,
};
use serde::Deserialize;
use serde_json::json;
use std::sync::atomic::Ordering;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

/// Browser harness viewer runs on another origin (e.g. :8765). Allow GET/HEAD
/// cross-origin fetches for job JSON + artifacts (SplatMesh `fetch`).
async fn cors_layer(req: axum::extract::Request, next: Next) -> Response {
    // Preflight for Range / Content-Type from some loaders.
    if req.method() == Method::OPTIONS {
        let mut res = StatusCode::NO_CONTENT.into_response();
        insert_cors(res.headers_mut());
        res.headers_mut().insert(
            header::ACCESS_CONTROL_ALLOW_METHODS,
            HeaderValue::from_static("GET, HEAD, POST, OPTIONS"),
        );
        res.headers_mut().insert(
            header::ACCESS_CONTROL_ALLOW_HEADERS,
            HeaderValue::from_static("content-type, range, authorization"),
        );
        res.headers_mut().insert(
            header::ACCESS_CONTROL_MAX_AGE,
            HeaderValue::from_static("86400"),
        );
        return res;
    }

    let mut res = next.run(req).await;
    insert_cors(res.headers_mut());
    // Expose size headers so range clients can read them if needed.
    res.headers_mut().insert(
        header::ACCESS_CONTROL_EXPOSE_HEADERS,
        HeaderValue::from_static("content-length, content-type, accept-ranges"),
    );
    res
}

fn insert_cors(headers: &mut axum::http::HeaderMap) {
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
}

pub fn router(state: SharedState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/jobs", post(create_job).get(list_jobs))
        .route("/v1/jobs/{id}", get(get_job))
        .route("/v1/jobs/{id}/log/{stage}", get(get_log))
        .route("/v1/jobs/{id}/artifacts/{name}", get(get_artifact))
        .layer(middleware::from_fn(cors_layer))
        .with_state(state)
}

/// HealthReport-shaped snapshot (weftos HEALTHCHECK-CONTRACT.md) plus
/// service-specific queue/job counters.
async fn healthz(State(state): State<SharedState>) -> Json<serde_json::Value> {
    Json(json!({
        "status": "healthy",
        "uptime_s": state.started.elapsed().as_secs(),
        "last_publish_ts": now_ms(),
        "queue_depth": state.queue_depth.load(Ordering::Relaxed),
        "jobs_done": state.jobs_done.load(Ordering::Relaxed),
        "jobs_failed": state.jobs_failed.load(Ordering::Relaxed),
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

#[derive(Deserialize)]
struct CreateJobJson {
    /// Local path to a video already on this machine.
    source_path: Option<String>,
    /// Remote URL splatd downloads (e.g. a Supabase Storage signed URL).
    source_url: Option<String>,
    /// Absolute path to a weft.capture.v1 session directory (frames/ + poses.jsonl).
    session_path: Option<String>,
    /// Webhook POSTed the terminal JobRecord (shasta-os-020 awaitArtifact).
    callback_url: Option<String>,
}

/// POST /v1/jobs — video or image-set session.
///
/// Video:
/// - multipart field `video`
/// - JSON `{ "source_path" | "source_url", "callback_url"? }`
///
/// Image-set (weft.capture.v1):
/// - multipart field `session` (ZIP of session package)
/// - JSON `{ "session_path": "/abs/path/to/session_dir", "callback_url"? }`
async fn create_job(
    State(state): State<SharedState>,
    headers: axum::http::HeaderMap,
    body: axum::body::Body,
) -> Response {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let job_id = Uuid::new_v4().to_string();
    let dirs = state.job_dirs(&job_id);
    if let Err(e) = dirs.create_all() {
        return err(StatusCode::INTERNAL_SERVER_ERROR, &format!("workspace: {e}"));
    }

    let (source, callback_url) = if content_type.starts_with("multipart/form-data") {
        let boundary = match multer::parse_boundary(&content_type) {
            Ok(b) => b,
            Err(_) => return err(StatusCode::BAD_REQUEST, "missing multipart boundary"),
        };
        let mut mp = multer::Multipart::new(body.into_data_stream(), boundary);
        match save_upload(&mut mp, &dirs.root).await {
            Ok(v) => v,
            Err(e) => return err(StatusCode::BAD_REQUEST, &e),
        }
    } else {
        let bytes = match axum::body::to_bytes(body, 1 << 20).await {
            Ok(b) => b,
            Err(e) => return err(StatusCode::BAD_REQUEST, &format!("body: {e}")),
        };
        let req: CreateJobJson = match serde_json::from_slice(&bytes) {
            Ok(r) => r,
            Err(e) => return err(StatusCode::BAD_REQUEST, &format!("json: {e}")),
        };
        if let Some(session_path) = req.session_path {
            if let Err(e) = materialize_session_path(&session_path, &dirs.root) {
                return err(StatusCode::BAD_REQUEST, &format!("session_path: {e}"));
            }
            (session_path, req.callback_url)
        } else if let Some(path) = req.source_path {
            let src = std::path::PathBuf::from(&path);
            let ext = src.extension().and_then(|e| e.to_str()).unwrap_or("mov");
            let dst = dirs.root.join(format!("input.{ext}"));
            if let Err(e) = std::fs::copy(&src, &dst) {
                return err(StatusCode::BAD_REQUEST, &format!("source_path {path}: {e}"));
            }
            (path, req.callback_url)
        } else if let Some(url) = req.source_url {
            if let Err(e) = download(&url, &dirs.root).await {
                return err(StatusCode::BAD_GATEWAY, &format!("source_url: {e}"));
            }
            (url, req.callback_url)
        } else {
            return err(
                StatusCode::BAD_REQUEST,
                "provide source_path, source_url, or session_path",
            );
        }
    };

    let rec = JobRecord::new(job_id.clone(), source, callback_url, now_ms());
    let _ = rec.save(&dirs.root);
    state.jobs.lock().await.insert(job_id.clone(), rec);
    state.queue_depth.fetch_add(1, Ordering::Relaxed);
    if state.tx.send(job_id.clone()).is_err() {
        return err(StatusCode::INTERNAL_SERVER_ERROR, "worker gone");
    }

    (StatusCode::ACCEPTED, Json(json!({ "job_id": job_id }))).into_response()
}

async fn list_jobs(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let jobs = state.jobs.lock().await;
    let mut items: Vec<_> = jobs
        .values()
        .map(|r| {
            json!({
                "job_id": r.job_id, "status": r.status,
                "stage": r.stage, "created_ts_ms": r.created_ts_ms,
            })
        })
        .collect();
    items.sort_by_key(|v| std::cmp::Reverse(v["created_ts_ms"].as_u64()));
    Json(json!({ "jobs": items }))
}

async fn get_job(State(state): State<SharedState>, Path(id): Path<String>) -> Response {
    match state.jobs.lock().await.get(&id) {
        Some(rec) => Json(rec.clone()).into_response(),
        None => err(StatusCode::NOT_FOUND, "no such job"),
    }
}

async fn get_log(
    State(state): State<SharedState>,
    Path((id, stage)): Path<(String, String)>,
) -> Response {
    if !valid_name(&stage) {
        return err(StatusCode::BAD_REQUEST, "bad stage name");
    }
    let path = state.job_dirs(&id).logs().join(format!("{stage}.log"));
    match tokio::fs::read_to_string(&path).await {
        Ok(text) => ([(header::CONTENT_TYPE, "text/plain")], text).into_response(),
        Err(_) => err(StatusCode::NOT_FOUND, "no log for that job/stage"),
    }
}

async fn get_artifact(
    State(state): State<SharedState>,
    Path((id, name)): Path<(String, String)>,
) -> Response {
    if !valid_name(&name) {
        return err(StatusCode::BAD_REQUEST, "bad artifact name");
    }
    let path = state.job_dirs(&id).artifacts().join(&name);
    match tokio::fs::File::open(&path).await {
        Ok(file) => {
            let stream = tokio_util::io::ReaderStream::new(file);
            let content_type = match path.extension().and_then(|e| e.to_str()) {
                Some("json") => "application/json",
                Some("sog") | Some("ply") | Some("splat") | Some("spz") => "application/octet-stream",
                _ => "application/octet-stream",
            };
            // CORS applied by router layer; keep Content-Type + streaming body.
            (
                [(header::CONTENT_TYPE, content_type.to_string())],
                axum::body::Body::from_stream(stream),
            )
                .into_response()
        }
        Err(_) => err(StatusCode::NOT_FOUND, "no such artifact"),
    }
}

// ---- helpers ---------------------------------------------------------

fn err(code: StatusCode, msg: &str) -> Response {
    (code, Json(json!({ "error": msg }))).into_response()
}

/// Artifact/stage names: single path segment, no traversal.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains(['/', '\\'])
        && !name.contains("..")
        && name.len() < 128
}

async fn save_upload(
    mp: &mut multer::Multipart<'_>,
    job_root: &std::path::Path,
) -> Result<(String, Option<String>), String> {
    let mut source = None;
    let mut callback_url = None;
    let mut saw_video = false;
    let mut saw_session = false;

    while let Some(mut field) = mp.next_field().await.map_err(|e| e.to_string())? {
        match field.name().unwrap_or("") {
            "video" => {
                if saw_session {
                    return Err("multipart cannot include both `video` and `session`".into());
                }
                let ext = field
                    .file_name()
                    .and_then(|n| std::path::Path::new(n).extension())
                    .and_then(|e| e.to_str())
                    .unwrap_or("mov")
                    .to_ascii_lowercase();
                let dst = job_root.join(format!("input.{ext}"));
                let mut file = tokio::fs::File::create(&dst).await.map_err(|e| e.to_string())?;
                while let Some(chunk) = field.chunk().await.map_err(|e| e.to_string())? {
                    file.write_all(&chunk).await.map_err(|e| e.to_string())?;
                }
                file.flush().await.map_err(|e| e.to_string())?;
                source = Some("upload".to_string());
                saw_video = true;
            }
            "session" => {
                if saw_video {
                    return Err("multipart cannot include both `video` and `session`".into());
                }
                // Save ZIP then extract into job_root/session/.
                let zip_path = job_root.join("session_upload.zip");
                let mut file = tokio::fs::File::create(&zip_path)
                    .await
                    .map_err(|e| e.to_string())?;
                while let Some(chunk) = field.chunk().await.map_err(|e| e.to_string())? {
                    file.write_all(&chunk).await.map_err(|e| e.to_string())?;
                }
                file.flush().await.map_err(|e| e.to_string())?;
                drop(file);

                let session_dir = job_root.join(SESSION_DIR_NAME);
                extract_session_zip(&zip_path, &session_dir).map_err(|e| e.to_string())?;
                let _ = std::fs::remove_file(&zip_path);
                if !is_session_dir(&session_dir) {
                    return Err(
                        "session zip did not contain frames/ with images (weft.capture.v1)".into(),
                    );
                }
                source = Some("session-upload".to_string());
                saw_session = true;
            }
            "callback_url" => {
                callback_url = Some(field.text().await.map_err(|e| e.to_string())?);
            }
            _ => {}
        }
    }
    source
        .map(|s| (s, callback_url))
        .ok_or_else(|| {
            "multipart must include a `video` field or a `session` (zip) field".to_string()
        })
}

/// Copy a local session directory into `job_root/session/`.
fn materialize_session_path(session_path: &str, job_root: &std::path::Path) -> Result<(), String> {
    let src = std::path::PathBuf::from(session_path);
    if !src.is_dir() {
        return Err(format!("not a directory: {session_path}"));
    }
    if !is_session_dir(&src) {
        return Err(format!(
            "not a weft.capture.v1 session (need frames/ with images): {session_path}"
        ));
    }
    let dest = job_root.join(SESSION_DIR_NAME);
    copy_dir_recursive(&src, &dest).map_err(|e| e.to_string())?;
    normalize_session_root(&dest).map_err(|e| e.to_string())?;
    if !is_session_dir(&dest) {
        return Err("session materialize failed validation".into());
    }
    Ok(())
}

fn copy_dir_recursive(src: &std::path::Path, dest: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dest)?;
    for ent in std::fs::read_dir(src)? {
        let ent = ent?;
        let from = ent.path();
        let to = dest.join(ent.file_name());
        if from.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

fn extract_session_zip(zip_path: &std::path::Path, dest: &std::path::Path) -> Result<(), String> {
    std::fs::create_dir_all(dest).map_err(|e| e.to_string())?;
    let file = std::fs::File::open(zip_path).map_err(|e| e.to_string())?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| e.to_string())?;
        // Symlink entries are refused: extraction only writes regular files.
        if entry.is_symlink() {
            return Err(format!("zip symlink entry refused: {:?}", entry.name()));
        }
        let name = entry
            .enclosed_name()
            .ok_or_else(|| format!("unsafe zip path: {:?}", entry.name()))?
            .to_path_buf();
        // Skip macOS resource forks / junk.
        if name.components().any(|c| {
            matches!(c, std::path::Component::Normal(s) if s == "__MACOSX" || s.to_string_lossy().starts_with("._"))
        }) {
            continue;
        }
        let outpath = dest.join(&name);
        if entry.is_dir() {
            std::fs::create_dir_all(&outpath).map_err(|e| e.to_string())?;
        } else {
            if let Some(parent) = outpath.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            let mut outfile = std::fs::File::create(&outpath).map_err(|e| e.to_string())?;
            std::io::copy(&mut entry, &mut outfile).map_err(|e| e.to_string())?;
        }
    }
    normalize_session_root(dest).map_err(|e| e.to_string())?;
    Ok(())
}

async fn download(url: &str, job_root: &std::path::Path) -> anyhow::Result<()> {
    let resp = reqwest::get(url).await?.error_for_status()?;
    // Extension from the URL path, default mov.
    let ext = url
        .split('?')
        .next()
        .and_then(|p| p.rsplit('.').next())
        .filter(|e| e.len() <= 4 && e.chars().all(|c| c.is_ascii_alphanumeric()))
        .unwrap_or("mov")
        .to_ascii_lowercase();
    let dst = job_root.join(format!("input.{ext}"));
    let mut file = tokio::fs::File::create(&dst).await?;
    use futures_util::StreamExt;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        file.write_all(&chunk?).await?;
    }
    file.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_tiny_jpeg(path: &std::path::Path) {
        // Minimal JPEG SOI/EOI — enough for is_image_path.
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::write(path, [0xFF, 0xD8, 0xFF, 0xD9]).unwrap();
    }

    #[test]
    fn materialize_session_path_ok() {
        let tmp = std::env::temp_dir().join(format!(
            "splatd-session-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let session = tmp.join("src_session");
        write_tiny_jpeg(&session.join("frames/000001.jpg"));
        let mut poses = std::fs::File::create(session.join("poses.jsonl")).unwrap();
        writeln!(
            poses,
            r#"{{"frame_id":1,"t_ns":1,"path":"frames/000001.jpg","quat_wxyz":[1,0,0,0]}}"#
        )
        .unwrap();

        let job = tmp.join("job");
        std::fs::create_dir_all(&job).unwrap();
        materialize_session_path(session.to_str().unwrap(), &job).unwrap();
        assert!(is_session_dir(&job.join(SESSION_DIR_NAME)));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn extract_zip_with_wrapper_dir() {
        let tmp = std::env::temp_dir().join(format!(
            "splatd-zip-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let pack = tmp.join("pack");
        write_tiny_jpeg(&pack.join("session-demo/frames/a.jpg"));
        std::fs::write(
            pack.join("session-demo/poses.jsonl"),
            r#"{"frame_id":1,"t_ns":1,"path":"frames/a.jpg","quat_wxyz":[1,0,0,0]}"#,
        )
        .unwrap();

        let zip_path = tmp.join("s.zip");
        {
            let file = std::fs::File::create(&zip_path).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            zip.add_directory("session-demo/", opts).unwrap();
            zip.add_directory("session-demo/frames/", opts).unwrap();
            zip.start_file("session-demo/frames/a.jpg", opts).unwrap();
            zip.write_all(&[0xFF, 0xD8, 0xFF, 0xD9]).unwrap();
            zip.start_file("session-demo/poses.jsonl", opts).unwrap();
            zip.write_all(
                br#"{"frame_id":1,"t_ns":1,"path":"frames/a.jpg","quat_wxyz":[1,0,0,0]}"#,
            )
            .unwrap();
            zip.finish().unwrap();
        }

        let dest = tmp.join("out");
        extract_session_zip(&zip_path, &dest).unwrap();
        assert!(is_session_dir(&dest), "expected peeled session at {dest:?}");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn extract_zip_refuses_symlink_and_traversal() {
        let tmp = std::env::temp_dir().join(format!(
            "splatd-zip-refuse-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);

        let link_zip = tmp.join("link.zip");
        {
            let mut zip = zip::ZipWriter::new(std::fs::File::create(&link_zip).unwrap());
            zip.add_symlink("session/evil", "/etc/passwd", opts).unwrap();
            zip.finish().unwrap();
        }
        let err = extract_session_zip(&link_zip, &tmp.join("out-link")).unwrap_err();
        assert!(err.contains("symlink"), "unexpected error: {err}");
        assert!(!tmp.join("out-link/session/evil").exists());

        let trav_zip = tmp.join("trav.zip");
        {
            let mut zip = zip::ZipWriter::new(std::fs::File::create(&trav_zip).unwrap());
            zip.start_file("../escape.txt", opts).unwrap();
            zip.write_all(b"x").unwrap();
            zip.finish().unwrap();
        }
        let err = extract_session_zip(&trav_zip, &tmp.join("out-trav")).unwrap_err();
        assert!(err.contains("unsafe zip path"), "unexpected error: {err}");
        assert!(!tmp.join("escape.txt").exists());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
