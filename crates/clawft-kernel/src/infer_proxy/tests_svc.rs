//! Svc-backed roles: the OpenAI to `infer.v1` translator, the loopback
//! proxy with a stand-in broker service, and the mesh path (exposure and
//! refusal hold exactly as for an HTTP-served role).

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde_json::{Value, json};

use super::listener::{InferProxy, OccupiedPolicy, Started};
use super::mesh_forward::{ServeGate, Served, serve_infer};
use super::support::*;
use super::svc::{MAX_IMAGE_BYTES, SvcBackend, SvcImage, SvcRoute, chat_to_infer, decode_image, infer_to_chat};
use super::table::PlacementTable;
use super::types::*;
use super::upstream::Upstream;
use super::wire;
use crate::mesh_framing::{FrameType, MeshFrame, write_frame};
use crate::mesh_test_support::connected_pair;

const MODEL: &str = "qwen3-vl-4b";
const TOPIC: &str = "svc.infer/venq-npu";

fn png(extra: usize) -> Vec<u8> {
    let mut v = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    v.extend(std::iter::repeat_n(7u8, extra));
    v
}

fn data_uri(mime: &str, bytes: &[u8]) -> String {
    format!("data:{mime};base64,{}", B64.encode(bytes))
}

fn chat(content: Value) -> Vec<u8> {
    serde_json::to_vec(&json!({"model": "whatever", "messages": [{"role": "user", "content": content}]})).unwrap()
}

fn msgs(m: Value, extra: Value) -> Vec<u8> {
    let mut v = json!({"messages": m});
    for (k, x) in extra.as_object().cloned().unwrap_or_default() {
        v[k] = x;
    }
    serde_json::to_vec(&v).unwrap()
}

fn refused(r: Result<super::svc::Translated, ProxyError>) -> ProxyError {
    r.expect_err("must be refused")
}

#[test]
fn text_only_chat_becomes_text_with_the_role_model() {
    let t = chat_to_infer(&chat(json!("hello there")), MODEL).unwrap();
    assert_eq!(t.body, json!({"model": MODEL, "text": "hello there", "max_tokens": 512}));
    assert!(t.image.is_none());
}

#[test]
fn system_messages_are_concatenated_and_the_last_user_message_is_the_text() {
    let b = msgs(
        json!([
            {"role": "system", "content": "be brief"},
            {"role": "user", "content": "first"},
            {"role": "assistant", "content": "ok"},
            {"role": "developer", "content": [{"type": "text", "text": "answer in French"}]},
            {"role": "user", "content": [{"type": "text", "text": "line one"}, {"type": "text", "text": "line two"}]}
        ]),
        json!({}),
    );
    let t = chat_to_infer(&b, MODEL).unwrap();
    assert_eq!(t.body["system"], "be brief\nanswer in French");
    assert_eq!(t.body["text"], "line one\nline two");
}

#[test]
fn an_image_data_uri_is_decoded_and_kept_apart_from_the_body() {
    let bytes = png(100);
    let b = chat(json!([
        {"type": "text", "text": "what is this"},
        {"type": "image_url", "image_url": {"url": data_uri("image/png", &bytes)}}
    ]));
    let t = chat_to_infer(&b, MODEL).unwrap();
    assert_eq!(t.image, Some(SvcImage { media_type: "image/png", bytes }));
    assert!(t.body.get("image_blob").is_none() && t.body.get("image_b64").is_none());
    // An image alone is a request too.
    let only = chat(json!([{"type": "image_url", "image_url": {"url": data_uri("image/jpeg", &[0xff, 0xd8, 0xff, 1])}}]));
    let t = chat_to_infer(&only, MODEL).unwrap();
    assert!(t.body.get("text").is_none());
    assert_eq!(t.image.unwrap().media_type, "image/jpeg");
}

#[test]
fn http_image_urls_are_refused_and_never_fetched() {
    for u in ["http://127.0.0.1:9/a.png", "https://example.com/a.png", "HTTPS://example.com/a.png"] {
        let e = refused(chat_to_infer(&chat(json!([{"type": "image_url", "image_url": {"url": u}}])), MODEL));
        assert!(matches!(&e, ProxyError::BadRequest(m) if m.contains("not fetched")), "{u}: {e}");
        assert_eq!(e.status(), 400);
    }
}

#[test]
fn two_images_are_refused_with_a_clear_400() {
    let u = data_uri("image/png", &png(4));
    let b = chat(json!([
        {"type": "image_url", "image_url": {"url": u}},
        {"type": "image_url", "image_url": {"url": u}}
    ]));
    let e = refused(chat_to_infer(&b, MODEL));
    assert_eq!(e.status(), 400);
    assert!(e.to_string().contains("at most one image"), "{e}");
}

#[test]
fn data_uri_parameters_are_ignored_but_base64_is_required() {
    let b64 = B64.encode(png(8));
    let ok = decode_image(&format!("data:image/png;charset=utf-8;base64,{b64}")).unwrap();
    assert_eq!(ok.media_type, "image/png");
    assert!(decode_image(&format!("data:image/png;name=a.png;charset=utf-8;base64,{b64}")).is_ok());
    assert!(decode_image(&format!("data:IMAGE/PNG;BASE64,{b64}")).is_ok());
    // No base64 parameter: refused even when other parameters are present.
    assert!(decode_image(&format!("data:image/png;charset=utf-8,{b64}")).is_err());
    assert!(decode_image(&format!("data:;base64,{b64}")).is_err(), "empty mime");
}

#[test]
fn only_the_last_user_message_images_count() {
    let u = data_uri("image/png", &png(4));
    let m = json!([
        {"role": "user", "content": [{"type": "text", "text": "old"}, {"type": "image_url", "image_url": {"url": u}},
                                      {"type": "image_url", "image_url": {"url": u}}]},
        {"role": "assistant", "content": "a picture"},
        {"role": "user", "content": "and now text only"}
    ]);
    let t = chat_to_infer(&msgs(m, json!({})), MODEL).unwrap();
    assert!(t.image.is_none(), "an image in an earlier turn is ignored (even two of them)");
    assert_eq!(t.body["text"], "and now text only");
    // The last turn's image is the one sent.
    let last = png(9);
    let m = json!([
        {"role": "user", "content": [{"type": "image_url", "image_url": {"url": u}}]},
        {"role": "user", "content": [{"type": "text", "text": "this"}, {"type": "image_url", "image_url": {"url": data_uri("image/png", &last)}}]}
    ]);
    let t = chat_to_infer(&msgs(m, json!({})), MODEL).unwrap();
    assert_eq!(t.image.unwrap().bytes, last);
}

#[test]
fn busy_is_a_429() {
    assert_eq!(ProxyError::Busy("x".into()).status(), 429);
}

#[test]
fn an_oversize_image_is_refused() {
    let big = png(MAX_IMAGE_BYTES);
    let e = decode_image(&data_uri("image/png", &big)).unwrap_err();
    assert!(matches!(e, ProxyError::TooLarge(_)), "{e}");
    assert_eq!(e.status(), 413);
    // Just under the cap is fine.
    assert!(decode_image(&data_uri("image/png", &png(MAX_IMAGE_BYTES - 8))).is_ok());
}

#[test]
fn other_mime_types_and_mismatched_content_are_refused() {
    for mime in ["image/gif", "image/webp", "text/plain"] {
        let e = decode_image(&data_uri(mime, &png(4))).unwrap_err();
        assert_eq!(e.status(), 400, "{mime}");
    }
    assert!(decode_image(&data_uri("image/png", b"not a png")).is_err(), "content must match the declared type");
    assert!(decode_image("data:image/png,rawbytes").is_err(), "base64 only");
    assert!(decode_image("data:image/png;base64,@@@").is_err());
    assert!(decode_image("file:///etc/passwd").is_err());
}

#[test]
fn streaming_and_malformed_requests_are_400() {
    let e = refused(chat_to_infer(&msgs(json!([{"role": "user", "content": "x"}]), json!({"stream": true})), MODEL));
    assert!(e.to_string().contains("streaming is not supported"), "{e}");
    assert!(chat_to_infer(&msgs(json!([{"role": "user", "content": "x"}]), json!({"stream": false})), MODEL).is_ok());
    for bad in [&b"nope"[..], br#"[]"#, br#"{"messages":[]}"#, br#"{"messages":[{"role":"assistant","content":"x"}]}"#] {
        assert_eq!(refused(chat_to_infer(bad, MODEL)).status(), 400);
    }
    let tool = chat(json!([{"type": "input_audio", "input_audio": {}}]));
    assert_eq!(refused(chat_to_infer(&tool, MODEL)).status(), 400);
}

#[test]
fn max_tokens_is_clamped_and_max_completion_tokens_wins() {
    let tokens = |extra: Value| chat_to_infer(&msgs(json!([{"role": "user", "content": "x"}]), extra), MODEL).unwrap().body["max_tokens"].clone();
    assert_eq!(tokens(json!({"max_tokens": 100})), 100);
    assert_eq!(tokens(json!({"max_tokens": 100000})), 4096);
    assert_eq!(tokens(json!({"max_tokens": 0})), 1);
    assert_eq!(tokens(json!({"max_tokens": 10, "max_completion_tokens": 20})), 20);
}

#[test]
fn the_reply_maps_to_a_chat_completion_with_usage_when_present() {
    let r = json!({"text": "a cat", "usage": {"input_tokens": 12, "output_tokens": 3}, "timings": {"total_ms": 900}});
    let v = infer_to_chat(&r, MODEL, "chatcmpl-1", 5).unwrap();
    assert_eq!(v["object"], "chat.completion");
    assert_eq!(v["model"], MODEL);
    assert_eq!(v["choices"][0]["message"], json!({"role": "assistant", "content": "a cat"}));
    assert_eq!(v["usage"], json!({"prompt_tokens": 12, "completion_tokens": 3, "total_tokens": 15}));
    assert!(infer_to_chat(&json!({"text": "x"}), MODEL, "i", 0).unwrap().get("usage").is_none());
    assert!(infer_to_chat(&json!({"nope": 1}), MODEL, "i", 0).is_err());
}

/// A stand-in for the broker session: sets `image_blob` the way the real
/// backend does and echoes the `infer.v1` body it would have sent.
type Call = (String, Value, Option<SvcImage>, String);

#[derive(Default)]
struct Echo {
    calls: Mutex<Vec<Call>>,
}

#[async_trait]
impl SvcBackend for Echo {
    async fn call(&self, topic: &str, mut body: Value, image: Option<SvcImage>, caller: &str) -> Result<Value, ProxyError> {
        if let Some(i) = &image {
            body["image_blob"] = json!({"hash": "abc", "size": i.bytes.len(), "media_type": i.media_type});
        }
        self.calls.lock().unwrap().push((topic.into(), body.clone(), image, caller.into()));
        Ok(json!({"text": body.to_string(), "usage": {"prompt_tokens": 4, "completion_tokens": 2}}))
    }
}

fn svc_table(id: &str, echo: &Arc<Echo>) -> Arc<PlacementTable> {
    let t = Arc::new(PlacementTable::new(id, None, None));
    t.set_svc_backend(echo.clone());
    t.register_local_svc("vision", SvcRoute { topic: TOPIC.into(), model: MODEL.into() }).unwrap();
    t
}

async fn start_proxy(t: &Arc<PlacementTable>, audit: Option<Arc<Audit>>) -> InferProxy {
    let a = audit.map(|a| a as Arc<dyn ProxyAudit>);
    match InferProxy::start("vision", "127.0.0.1:0".parse().unwrap(), OccupiedPolicy::Refuse, t.clone(), small_limits(), a)
        .await
        .unwrap()
    {
        Started::Running(p) => p,
        Started::Adopted(_) => panic!("adopted"),
    }
}

#[tokio::test]
async fn an_image_chat_through_the_proxy_arrives_as_image_blob_and_the_reply_maps_back() {
    let echo = Arc::new(Echo::default());
    let t = svc_table("venq", &echo);
    assert_eq!(t.resolve("vision"), Some(Target::Svc { topic: TOPIC.into(), model: MODEL.into() }));
    let audit = Arc::new(Audit::default());
    let p = start_proxy(&t, Some(audit.clone())).await;
    let bytes = png(300);
    let body = String::from_utf8(chat(json!([
        {"type": "text", "text": "describe"},
        {"type": "image_url", "image_url": {"url": data_uri("image/png", &bytes)}}
    ])))
    .unwrap();
    let resp = raw(p.addr(), &post(p.addr(), "/v1/chat/completions", &body)).await;
    assert_eq!(status(&resp), 200, "{resp}");
    let v: Value = serde_json::from_str(body_of(&resp)).unwrap();
    assert_eq!(v["model"], MODEL);
    assert_eq!(v["usage"]["total_tokens"], 6);
    let sent: Value = serde_json::from_str(v["choices"][0]["message"]["content"].as_str().unwrap()).unwrap();
    assert_eq!(sent["image_blob"], json!({"hash": "abc", "size": bytes.len(), "media_type": "image/png"}));
    assert_eq!(sent["text"], "describe");
    assert_eq!(sent["model"], MODEL, "the role model, not the client's");
    {
        let calls = echo.calls.lock().unwrap();
        assert_eq!(calls[0].0, TOPIC);
        assert_eq!(calls[0].2.as_ref().unwrap().bytes, bytes);
        assert!(calls[0].3.starts_with("local:127.0.0.1:"), "{}", calls[0].3);
    }
    assert!(audit.kinds().contains(&"infer.svc.call".to_string()));
    // Models list, and a refused streaming request.
    let m = raw(p.addr(), &get(p.addr(), "/v1/models")).await;
    assert!(body_of(&m).contains(MODEL), "{m}");
    let s = raw(p.addr(), &post(p.addr(), "/v1/chat/completions", r#"{"stream":true,"messages":[{"role":"user","content":"x"}]}"#)).await;
    assert_eq!(status(&s), 400, "{s}");
    {
        // A rejected request is audited with the caller and the reason, never the prompt.
        let rec = audit.0.lock().unwrap();
        let (k, v) = rec.iter().find(|(_, v)| v["outcome"] == "rejected").expect("rejected request audited");
        assert_eq!(k, "infer.svc.call");
        assert_eq!(v["status"], 400);
        assert!(v["caller"].as_str().unwrap().starts_with("local:"));
        assert!(v["reason"].as_str().unwrap().contains("streaming"));
        assert!(!v.to_string().contains("describe"), "no prompt in the audit record");
    }
    let e = raw(p.addr(), &post(p.addr(), "/v1/embeddings", r#"{"input":"x"}"#)).await;
    assert_eq!(status(&e), 501, "{e}");
    assert_eq!(echo.calls.lock().unwrap().len(), 1, "refused requests never reach the service");
}

fn body_of(resp: &str) -> &str {
    body(resp)
}

#[tokio::test]
async fn a_role_whose_service_backend_is_missing_is_unavailable_not_a_panic() {
    let t = Arc::new(PlacementTable::new("venq", None, None));
    t.register_local_svc("vision", SvcRoute { topic: TOPIC.into(), model: MODEL.into() }).unwrap();
    let p = start_proxy(&t, None).await;
    let resp = raw(p.addr(), &post(p.addr(), "/v1/chat/completions", r#"{"messages":[{"role":"user","content":"x"}]}"#)).await;
    assert_eq!(status(&resp), 503, "{resp}");
}

async fn mesh_request(t: &Arc<PlacementTable>, peer: &str, body: &str) -> (Served, Vec<wire::Resp>) {
    let (mut client, mut server) = connected_pair().await.unwrap();
    let u = Upstream::new(small_limits()).unwrap();
    let tt = t.clone();
    let p = trusted_peer(peer);
    let h = tokio::spawn(async move { serve_infer(&mut server, &p, tt.as_ref(), &u, &ServeGate::default(), None).await.unwrap() });
    let req = ProxyRequest {
        role: "vision".into(), method: Method::Post, path: "/v1/chat/completions".into(),
        content_type: Some("application/json".into()), accept: None, authorization: None, body: body.as_bytes().to_vec(),
    };
    write_frame(&mut client, &MeshFrame { frame_type: FrameType::InferRequest, payload: wire::encode_request(&req).unwrap() }).await.unwrap();
    let mut got = Vec::new();
    while let Ok(f) = crate::mesh_framing::read_frame(&mut client).await {
        let r = wire::decode_resp(&f.payload).unwrap();
        let done = matches!(r, wire::Resp::End | wire::Resp::Error(_));
        got.push(r);
        if done {
            break;
        }
    }
    (h.await.unwrap(), got)
}

#[tokio::test]
async fn a_svc_role_is_advertised_and_served_to_an_allowed_peer_with_the_model_pinned() {
    let echo = Arc::new(Echo::default());
    let t = svc_table("venq", &echo);
    t.expose_to_mesh("vision", true);
    t.allow_mesh_peer("vision", "mac", true);
    let ad = t.advertisement("vision", 1).expect("announced like an HTTP-served role");
    assert_eq!(ad.name, "infer.vision");
    assert_eq!(ad.metadata.get("runtime").map(String::as_str), Some("svc"));
    assert_eq!(ad.metadata.get("model").map(String::as_str), Some(MODEL));
    assert!(t.exposures().get("infer.vision").is_some());
    let body = r#"{"model":"someone-elses-model","keep_alive":"1h","messages":[{"role":"user","content":"hi"}]}"#;
    let (served, frames) = mesh_request(&t, "mac", body).await;
    assert_eq!(served, Served::Ok);
    assert!(frames.iter().any(|f| matches!(f, wire::Resp::Head { status: 200, .. })));
    let calls = echo.calls.lock().unwrap();
    assert_eq!(calls[0].1["model"], MODEL);
    assert_eq!(calls[0].3, "mesh:mac");
}

#[tokio::test]
async fn a_svc_role_that_is_not_exposed_or_not_allowed_is_refused_and_never_called() {
    let echo = Arc::new(Echo::default());
    let t = svc_table("venq", &echo);
    let body = r#"{"messages":[{"role":"user","content":"hi"}]}"#;
    // Registered but never exposed: not announced, not served.
    t.allow_mesh_peer("vision", "mac", true);
    assert!(t.advertisement("vision", 1).is_none());
    let (served, _) = mesh_request(&t, "mac", body).await;
    assert!(matches!(served, Served::Failed(_)), "{served:?}");
    // Exposed, but the peer is not on the allowlist.
    t.expose_to_mesh("vision", true);
    let (served, _) = mesh_request(&t, "stranger", body).await;
    assert!(matches!(served, Served::Failed(_)), "{served:?}");
    // Switched off again (ADR-120): refused at once.
    t.expose_to_mesh("vision", false);
    let (served, _) = mesh_request(&t, "mac", body).await;
    assert!(matches!(served, Served::Failed(_)), "{served:?}");
    assert!(echo.calls.lock().unwrap().is_empty(), "no refused request reached the service");
}

#[tokio::test]
async fn an_image_chat_from_a_consumer_node_is_answered_by_the_svc_node_over_the_mesh() {
    let echo = Arc::new(Echo::default());
    let b = svc_table("venq", &echo);
    b.expose_to_mesh("vision", true);
    b.allow_mesh_peer("vision", "node-a", true);
    let mesh = FakeMesh::new("node-a");
    let a = Arc::new(PlacementTable::new("node-a", Some(mesh.clone() as Arc<dyn MeshDialer>), None));
    mesh.add_node("venq", b.clone(), true);
    a.allow_remote_node("vision", "venq", true);
    assert!(a.ingest_advertisement("venq", &b.advertisement("vision", 100).unwrap()));
    assert_eq!(a.resolve("vision"), Some(Target::Remote { node_id: "venq".into() }));
    let p = start_proxy(&a, None).await;
    let body = String::from_utf8(chat(json!([
        {"type": "text", "text": "what"},
        {"type": "image_url", "image_url": {"url": data_uri("image/jpeg", &[0xff, 0xd8, 0xff, 9, 9])}}
    ])))
    .unwrap();
    let resp = raw(p.addr(), &post(p.addr(), "/v1/chat/completions", &body)).await;
    assert_eq!(status(&resp), 200, "{resp}");
    let v: Value = serde_json::from_str(body_of(&resp)).unwrap();
    let sent: Value = serde_json::from_str(v["choices"][0]["message"]["content"].as_str().unwrap()).unwrap();
    assert_eq!(sent["image_blob"]["media_type"], "image/jpeg");
    assert_eq!(echo.calls.lock().unwrap()[0].3, "mesh:node-a");
    // The 2 MiB mesh cap applies to the forwarded chat request, base64 included.
    let big = String::from_utf8(chat(json!([{"type": "image_url", "image_url": {"url": data_uri("image/png", &png(2 * 1024 * 1024))}}]))).unwrap();
    let resp = raw(p.addr(), &post(p.addr(), "/v1/chat/completions", &big)).await;
    assert_eq!(status(&resp), 413, "{}", &resp[..resp.len().min(200)]);
}
