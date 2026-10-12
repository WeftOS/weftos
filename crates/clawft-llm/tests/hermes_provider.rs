//! ADR-060 task 0.2: `LocalProvider` Hermes serving path.
//!
//! Two layers of coverage:
//!
//! - **Mock HTTP (always runs):** stands up a wiremock server to prove the
//!   provider (1) injects the explicit `num_ctx` and `enable_thinking` toggle
//!   into the request body, and (2) round-trips a Hermes `<tool_call>` response
//!   into native OpenAI `tool_calls` while stripping `<think>` reasoning — the
//!   load-bearing 0.2 behaviors, verified without a live model.
//! - **Live (`#[ignore]`):** a real tool-calling round-trip against the
//!   `bin/serve-llamacpp` endpoint on :8090. Run with `--ignored` once 0.1 is
//!   unblocked (model served).

use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use clawft_llm::ReasoningMode;
use clawft_llm::local_provider::LocalProvider;
use clawft_llm::provider::Provider;
use clawft_llm::types::{ChatMessage, ChatRequest};

/// A request body with a tool available, like a real tool-using turn.
fn tool_request(model: &str) -> ChatRequest {
    ChatRequest {
        model: model.into(),
        messages: vec![ChatMessage::user("What's the weather in SF?")],
        max_tokens: Some(256),
        temperature: Some(0.0),
        tools: vec![serde_json::json!({
            "type": "function",
            "function": {
                "name": "get_weather",
                "parameters": {
                    "type": "object",
                    "properties": {"city": {"type": "string"}},
                    "required": ["city"]
                }
            }
        })],
        stream: None,
        tool_choice: None,
    }
}

// ── Mock: streaming carries the same serving knobs ──────────────────────

#[tokio::test]
async fn streamed_turns_carry_num_ctx_and_the_reasoning_switch() {
    let server = MockServer::start().await;
    let sse = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\n\
               data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
               data: [DONE]\n\n";
    // The mock only matches if the streaming body has stream, num_ctx and enable_thinking.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_partial_json(serde_json::json!({
            "stream": true,
            "options": {"num_ctx": 32768},
            "chat_template_kwargs": {"enable_thinking": false}
        })))
        .respond_with(ResponseTemplate::new(200).insert_header("content-type", "text/event-stream").set_body_string(sse))
        .expect(1)
        .mount(&server)
        .await;

    let provider = LocalProvider::new(server.uri(), "hermes".into(), None).with_num_ctx(32768).with_reasoning(ReasoningMode::Off);
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    provider.complete_stream(&tool_request("hermes"), tx).await.unwrap();
    let mut text = String::new();
    while let Some(chunk) = rx.recv().await {
        if let clawft_llm::StreamChunk::TextDelta { text: t } = chunk {
            text.push_str(&t);
        }
    }
    assert_eq!(text, "ok");
}

// ── Mock: request-body augmentation ─────────────────────────────────────

#[tokio::test]
async fn injects_num_ctx_and_reasoning_off_into_body() {
    let server = MockServer::start().await;

    let body = serde_json::json!({
        "id": "r1",
        "model": "hermes",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "ok"},
            "finish_reason": "stop"
        }],
        "usage": null
    });

    // The mock only matches if num_ctx and enable_thinking=false reach the wire.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_partial_json(serde_json::json!({
            "options": {"num_ctx": 32768},
            "chat_template_kwargs": {"enable_thinking": false}
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(&body))
        .expect(1)
        .mount(&server)
        .await;

    let provider = LocalProvider::new(server.uri(), "hermes".into(), None).with_num_ctx(32768);

    // Per-turn override: tool-execution turn => reasoning off.
    let resp = provider
        .complete_turn(&tool_request("hermes"), ReasoningMode::Off)
        .await
        .unwrap();
    assert_eq!(resp.id, "r1");
}

#[tokio::test]
async fn reasoning_on_injects_enable_thinking_true() {
    let server = MockServer::start().await;

    let body = serde_json::json!({
        "id": "r2",
        "model": "hermes",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "planning done"},
            "finish_reason": "stop"
        }],
        "usage": null
    });

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_partial_json(serde_json::json!({
            "chat_template_kwargs": {"enable_thinking": true}
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(&body))
        .expect(1)
        .mount(&server)
        .await;

    let provider = LocalProvider::new(server.uri(), "hermes".into(), None);
    let resp = provider
        .complete_turn(&tool_request("hermes"), ReasoningMode::On)
        .await
        .unwrap();
    assert_eq!(resp.id, "r2");
}

// ── Mock: <tool_call> + <think> round-trip ──────────────────────────────

#[tokio::test]
async fn recovers_tool_call_from_content_and_strips_think() {
    let server = MockServer::start().await;

    // A Hermes-style response where the server template did NOT parse the
    // tool call — it sits in `content`, preceded by a <think> block that even
    // *mentions* a (wrong) tool call. Only the real call must be recovered, and
    // <think> must never leak into the parsed arguments.
    let raw_content = concat!(
        "<think>The user wants weather. I could call <tool_call>",
        r#"{"name":"WRONG","arguments":{}}</tool_call> but I'll do it right.</think>"#,
        r#"<tool_call>{"name":"get_weather","arguments":{"city":"SF"}}</tool_call>"#
    );

    let body = serde_json::json!({
        "id": "r3",
        "model": "hermes",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": raw_content},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 20, "completion_tokens": 30, "total_tokens": 50}
    });

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&body))
        .expect(1)
        .mount(&server)
        .await;

    let provider = LocalProvider::new(server.uri(), "hermes".into(), None);
    let resp = provider.complete(&tool_request("hermes")).await.unwrap();

    let choice = &resp.choices[0];
    // finish_reason fixed up to tool_calls since calls were recovered.
    assert_eq!(choice.finish_reason.as_deref(), Some("tool_calls"));

    let calls = choice
        .message
        .tool_calls
        .as_ref()
        .expect("tool calls should be recovered from content");
    assert_eq!(
        calls.len(),
        1,
        "only the real call, not the <think> mention"
    );
    assert_eq!(calls[0].function.name, "get_weather");

    // The load-bearing guarantee: <think> never reaches the parsed tool call.
    assert!(!calls[0].function.arguments.contains("WRONG"));
    assert!(!calls[0].function.arguments.to_lowercase().contains("think"));
    let parsed: serde_json::Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
    assert_eq!(parsed["city"], "SF");

    // Reasoning-only remainder collapses to no content.
    assert!(choice.message.content.is_none());
}

#[tokio::test]
async fn native_tool_calls_pass_through_untouched() {
    let server = MockServer::start().await;

    // Server template already parsed the call (the --jinja happy path).
    let body = serde_json::json!({
        "id": "r4",
        "model": "hermes",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "<think>reasoning</think>",
                "tool_calls": [{
                    "id": "call_native",
                    "type": "function",
                    "function": {"name": "get_weather", "arguments": "{\"city\":\"SF\"}"}
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": null
    });

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&body))
        .expect(1)
        .mount(&server)
        .await;

    let provider = LocalProvider::new(server.uri(), "hermes".into(), None);
    let resp = provider.complete(&tool_request("hermes")).await.unwrap();

    let calls = resp.choices[0].message.tool_calls.as_ref().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].id, "call_native");
    // Content was reasoning-only -> stripped to None.
    assert!(resp.choices[0].message.content.is_none());
}

// ── Live round-trip (gated) ─────────────────────────────────────────────

/// Live tool-calling round-trip against the ADR-060 serving endpoint.
///
/// BLOCKED on task 0.1 (Hermes-4.3-36B GGUF served on :8090). Run once served:
/// `cargo test -p clawft-llm --test hermes_provider -- --ignored`.
#[tokio::test]
#[ignore = "requires a live Hermes server on 127.0.0.1:8090 (task 0.1)"]
async fn live_hermes_tool_call_round_trip() {
    let provider = LocalProvider::hermes_serving();

    let resp = provider
        .complete_turn(
            &tool_request(clawft_llm::local_provider::HERMES_DEFAULT_MODEL),
            ReasoningMode::Off,
        )
        .await
        .expect("live Hermes endpoint should respond");

    let choice = &resp.choices[0];
    let calls = choice
        .message
        .tool_calls
        .as_ref()
        .expect("Hermes should emit a tool call for a weather query");
    assert!(!calls.is_empty());
    assert_eq!(calls[0].function.name, "get_weather");

    // <think> must never appear in any parsed tool-call arguments.
    for call in calls {
        assert!(
            !call.function.arguments.contains("<think>"),
            "tool-call arguments leaked <think>: {}",
            call.function.arguments
        );
        // Arguments must be valid JSON.
        serde_json::from_str::<serde_json::Value>(&call.function.arguments)
            .expect("tool-call arguments must be valid JSON");
    }
}
