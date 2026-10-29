//! A role served by a cogbus service (ADR-101 update 2026-10-28).
//!
//! `{"svc": {"topic": "svc.infer/venq-npu", "model": "qwen3-vl-4b"}}` makes an
//! `infer.<role>` answered by a provider of the `infer.v1` service on this
//! node's cog data plane (the Venq's NPU cog) instead of a loopback HTTP
//! server. The proxy keeps its OpenAI-compatible face: an OpenAI chat request
//! is translated to `infer.v1` ([`chat_to_infer`]), called through a
//! [`SvcBackend`] (the daemon's in-process broker session, governed like any
//! other `cogbus.call`), and the reply is translated back ([`infer_to_chat`]).
//!
//! What it carries, by design and not by omission:
//! - `POST /v1/chat/completions` with text and at most one image given as a
//!   `data:image/png|jpeg;base64,` URL, and `GET /v1/models`. Nothing else
//!   (completions, embeddings): 501.
//! - No streaming: `stream: true` is a 400, never a silent downgrade.
//! - The system messages become `system`, the last user message's text parts
//!   become `text`. Earlier turns and assistant/tool messages are not sent:
//!   `infer.v1` is a single-shot request.
//! - Images are taken from the last user message only; an image in an earlier
//!   turn is ignored (the history is not sent).
//! - An `http(s)` image URL is refused, never fetched (the daemon must not be
//!   made to dial a client-named host).
//! - An image travels as a broker blob (`image_blob`), because a service call
//!   body is capped at 64 KiB; the blob is owned by the proxy principal and
//!   expires on its own.

use std::time::Instant;

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde_json::{Map, Value, json};

use super::types::{Method, ProxyAudit, ProxyError, ProxyRequest, ResponseSink};

/// Largest decoded image `infer.v1` accepts (`clawft_types::cog_topics`).
pub const MAX_IMAGE_BYTES: usize = 4 * 1024 * 1024;
/// Largest `text` or `system`, bytes (`infer.v1`).
pub const MAX_TEXT: usize = 32 * 1024;
/// Most tokens `infer.v1` takes.
pub const MAX_TOKENS: u64 = 4096;
/// Tokens asked for when the client names none.
pub const DEFAULT_TOKENS: u64 = 512;

/// Where a role's calls go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SvcRoute {
    /// `svc.infer/<source_id>`.
    pub topic: String,
    /// The model id of the service's catalog the role uses; also what
    /// replies and `/v1/models` call the model.
    pub model: String,
}

/// A decoded image bound for a blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SvcImage {
    /// `image/png` or `image/jpeg`.
    pub media_type: &'static str,
    /// The decoded bytes.
    pub bytes: Vec<u8>,
}

/// The client side of the service call. The daemon implements it over its
/// in-process broker; it owns identity, governance and the blob upload.
#[async_trait]
pub trait SvcBackend: Send + Sync + 'static {
    /// Call `topic` with the `infer.v1` `body`. When `image` is present it is
    /// put as a blob and `image_blob` set in the body first. `caller` names
    /// who asked (`local:<addr>` or `mesh:<node>`) for the activity trail; the
    /// principal on the bus is the proxy's. Returns the reply body.
    async fn call(
        &self,
        topic: &str,
        body: Value,
        image: Option<SvcImage>,
        caller: &str,
    ) -> Result<Value, ProxyError>;
}

/// An OpenAI chat request as `infer.v1`.
#[derive(Debug, Clone, PartialEq)]
pub struct Translated {
    /// The `infer.v1` body (no `image_blob` yet).
    pub body: Value,
    /// The image to put as a blob, if the request carried one.
    pub image: Option<SvcImage>,
}

fn bad(m: impl Into<String>) -> ProxyError {
    ProxyError::BadRequest(m.into())
}

/// Text of a message `content`: a string, or the `text` parts of an array.
/// Images are returned separately. Any other part type is refused.
fn parts(content: &Value, images: &mut Vec<String>, allow_images: bool) -> Result<String, ProxyError> {
    match content {
        Value::Null => Ok(String::new()),
        Value::String(s) => Ok(s.clone()),
        Value::Array(a) => {
            let mut text: Vec<&str> = Vec::new();
            for p in a {
                match p.get("type").and_then(Value::as_str) {
                    Some("text") => text.push(p.get("text").and_then(Value::as_str).ok_or_else(|| bad("a text part has no text"))?),
                    Some("image_url") => {
                        if !allow_images {
                            return Err(bad("an image is accepted in a user message only"));
                        }
                        let u = p.get("image_url");
                        let url = u.and_then(|u| u.get("url")).or(u).and_then(Value::as_str);
                        images.push(url.ok_or_else(|| bad("an image_url part has no url"))?.to_string());
                    }
                    Some(other) => return Err(bad(format!("content part type {other:?} is not supported"))),
                    None => return Err(bad("a content part has no type")),
                }
            }
            Ok(text.join("\n"))
        }
        _ => Err(bad("message content is a string or an array of parts")),
    }
}

/// Decode a `data:image/png|jpeg;base64,...` URL. Anything else is refused,
/// notably `http(s)` (never fetched).
pub fn decode_image(url: &str) -> Result<SvcImage, ProxyError> {
    let lower = url.get(..8).unwrap_or(url).to_ascii_lowercase();
    if lower.starts_with("http:") || lower.starts_with("https:") {
        return Err(bad("image URLs are not fetched: send the image as a data: URI"));
    }
    let rest = url.strip_prefix("data:").ok_or_else(|| bad("an image is a data:image/png|jpeg;base64, URL"))?;
    let (meta, data) = rest.split_once(',').ok_or_else(|| bad("malformed data: URI"))?;
    // `<mime>[;param=value]*;base64`: the mime is the first segment, a bare
    // `base64` parameter is required, other parameters (charset) are ignored.
    let mut segs = meta.split(';');
    let mime = segs.next().unwrap_or("").trim();
    if !segs.any(|p| p.trim().eq_ignore_ascii_case("base64")) {
        return Err(bad("a data: URI image must be base64"));
    }
    let (media_type, magic): (&'static str, &[u8]) = match mime.to_ascii_lowercase().as_str() {
        "image/png" => ("image/png", &[0x89, b'P', b'N', b'G']),
        "image/jpeg" => ("image/jpeg", &[0xff, 0xd8, 0xff]),
        other => return Err(bad(format!("image type {other:?} is not supported (image/png or image/jpeg)"))),
    };
    if data.len() > MAX_IMAGE_BYTES / 3 * 4 + 8 {
        return Err(ProxyError::TooLarge(format!("image over {MAX_IMAGE_BYTES} bytes")));
    }
    let bytes = B64.decode(data.trim()).map_err(|_| bad("image data is not valid base64"))?;
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(ProxyError::TooLarge(format!("image over {MAX_IMAGE_BYTES} bytes")));
    }
    if !bytes.starts_with(magic) {
        return Err(bad(format!("image data is not a {media_type}")));
    }
    Ok(SvcImage { media_type, bytes })
}

fn token_limit(o: &Map<String, Value>) -> Result<u64, ProxyError> {
    let v = o.get("max_completion_tokens").filter(|v| !v.is_null()).or_else(|| o.get("max_tokens"));
    match v {
        None | Some(Value::Null) => Ok(DEFAULT_TOKENS),
        Some(v) => {
            let n = v.as_u64().or_else(|| v.as_f64().filter(|f| f.is_finite() && *f >= 0.0).map(|f| f as u64));
            Ok(n.ok_or_else(|| bad("max_tokens is a number"))?.clamp(1, MAX_TOKENS))
        }
    }
}

/// OpenAI `/v1/chat/completions` body to `infer.v1`.
pub fn chat_to_infer(body: &[u8], model: &str) -> Result<Translated, ProxyError> {
    let v: Value = serde_json::from_slice(body).map_err(|_| bad("request body must be JSON"))?;
    let o = v.as_object().ok_or_else(|| bad("request body must be a JSON object"))?;
    if o.get("stream").and_then(Value::as_bool) == Some(true) {
        return Err(bad("streaming is not supported for svc roles: send stream=false"));
    }
    let msgs = o.get("messages").and_then(Value::as_array).filter(|m| !m.is_empty()).ok_or_else(|| bad("messages is a non-empty array"))?;
    let mut system: Vec<String> = Vec::new();
    let mut last_user: Option<(String, Vec<String>)> = None;
    for m in msgs {
        let role = m.get("role").and_then(Value::as_str).ok_or_else(|| bad("a message has no role"))?;
        let content = m.get("content").unwrap_or(&Value::Null);
        match role {
            "system" | "developer" => {
                let t = parts(content, &mut Vec::new(), false)?;
                if !t.is_empty() {
                    system.push(t);
                }
            }
            // Each user message replaces the last: only its images count.
            "user" => {
                let mut imgs = Vec::new();
                let t = parts(content, &mut imgs, true)?;
                last_user = Some((t, imgs));
            }
            // History: not part of a single-shot request.
            _ => {}
        }
    }
    let (text, images) = last_user.ok_or_else(|| bad("there is no user message"))?;
    if images.len() > 1 {
        return Err(bad(format!("{} images sent: a svc role takes at most one image per request", images.len())));
    }
    let image = images.first().map(|u| decode_image(u)).transpose()?;
    if text.trim().is_empty() && image.is_none() {
        return Err(bad("the last user message has no text and no image"));
    }
    let system = system.join("\n");
    if text.len() > MAX_TEXT || system.len() > MAX_TEXT {
        return Err(ProxyError::TooLarge(format!("text or system over {MAX_TEXT} bytes")));
    }
    let mut out = Map::new();
    out.insert("model".into(), json!(model));
    if !text.trim().is_empty() {
        out.insert("text".into(), json!(text));
    }
    if !system.is_empty() {
        out.insert("system".into(), json!(system));
    }
    out.insert("max_tokens".into(), json!(token_limit(o)?));
    Ok(Translated { body: Value::Object(out), image })
}

fn count(u: &Value, names: &[&str]) -> Option<u64> {
    names.iter().find_map(|n| u.get(*n).and_then(Value::as_u64))
}

/// `infer.v1` reply to an OpenAI `chat.completion`.
pub fn infer_to_chat(reply: &Value, model: &str, id: &str, created: u64) -> Result<Value, ProxyError> {
    let text = reply.get("text").and_then(Value::as_str).ok_or_else(|| ProxyError::Upstream("the service reply carries no text".into()))?;
    let mut out = json!({
        "id": id,
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": [{"index": 0, "message": {"role": "assistant", "content": text}, "finish_reason": "stop"}],
    });
    if let Some(u) = reply.get("usage").filter(|u| u.is_object()) {
        let p = count(u, &["prompt_tokens", "input_tokens"]);
        let c = count(u, &["completion_tokens", "output_tokens"]);
        if p.is_some() || c.is_some() {
            let (p, c) = (p.unwrap_or(0), c.unwrap_or(0));
            out["usage"] = json!({"prompt_tokens": p, "completion_tokens": c, "total_tokens": count(u, &["total_tokens"]).unwrap_or(p + c)});
        }
    }
    Ok(out)
}

/// `GET /v1/models` for a svc role.
pub fn models_list(model: &str) -> Value {
    json!({"object": "list", "data": [{"id": model, "object": "model", "owned_by": "weftos"}]})
}

async fn send(sink: &mut dyn ResponseSink, v: &Value) -> Result<(), ProxyError> {
    let bytes = serde_json::to_vec(v).map_err(|e| ProxyError::Upstream(e.to_string()))?;
    sink.head(200, Some("application/json")).await?;
    sink.chunk(&bytes).await
}

/// Answer `req` for a svc role into `sink`. `caller` is recorded with the
/// call; the audit trail gets one entry per call (role, caller, topic,
/// whether an image went, size, outcome), never the prompt.
pub async fn serve_svc(
    backend: &dyn SvcBackend,
    route: &SvcRoute,
    req: &ProxyRequest,
    caller: &str,
    sink: &mut dyn ResponseSink,
    audit: Option<&dyn ProxyAudit>,
) -> Result<(), ProxyError> {
    let path = req.path.split('?').next().unwrap_or("");
    match (req.method, path) {
        (Method::Get, "/v1/models") => send(sink, &models_list(&route.model)).await,
        (Method::Post, "/v1/chat/completions") => {
            let started = Instant::now();
            let t = match chat_to_infer(&req.body, &route.model) {
                Ok(t) => t,
                Err(e) => {
                    if let Some(a) = audit {
                        let mut reason = e.to_string();
                        reason.truncate(200);
                        a.record(
                            "infer.svc.call",
                            json!({"role": req.role, "caller": caller, "topic": route.topic, "status": e.status(),
                                   "bytes_in": req.body.len(), "outcome": "rejected", "reason": reason}),
                        );
                    }
                    return Err(e);
                }
            };
            let had_image = t.image.is_some();
            let res = backend.call(&route.topic, t.body, t.image, caller).await;
            if let Some(a) = audit {
                a.record(
                    "infer.svc.call",
                    json!({"role": req.role, "caller": caller, "topic": route.topic, "image": had_image,
                           "bytes_in": req.body.len(), "ms": started.elapsed().as_millis() as u64,
                           "outcome": match &res { Ok(_) => "ok".to_string(), Err(e) => e.to_string() }}),
                );
            }
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
            let id = format!("chatcmpl-{}", uuid::Uuid::new_v4().simple());
            send(sink, &infer_to_chat(&res?, &route.model, &id, now)?).await
        }
        (Method::Post, "/v1/completions" | "/v1/embeddings") => Err(ProxyError::Unsupported(
            "this role is served by an inference service: chat completions and models only".into(),
        )),
        _ => Err(ProxyError::Unsupported("a svc role serves POST /v1/chat/completions and GET /v1/models".into())),
    }
}
