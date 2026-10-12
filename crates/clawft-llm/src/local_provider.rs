//! Local/Hermes-compatible LLM provider.
//!
//! [`LocalProvider`] connects to any OpenAI-compatible local inference server
//! (Ollama, vLLM, llama.cpp, LM Studio, Hermes). It wraps
//! [`OpenAiCompatProvider`] with key-optional behavior and sensible defaults
//! for air-gapped / local deployments.
//!
//! # Supported backends
//!
//! | Backend   | Default base URL                    | API key required |
//! |-----------|-------------------------------------|------------------|
//! | Ollama    | `http://localhost:11434/v1`          | No               |
//! | vLLM      | `http://localhost:8000/v1`           | No               |
//! | llama.cpp | `http://localhost:8080/v1`           | No               |
//! | LM Studio | `http://localhost:1234/v1`           | No               |
//! | Hermes    | `http://localhost:11434/v1` (Ollama) | No               |
//!
//! All backends expose the standard `/v1/chat/completions` endpoint.

use async_trait::async_trait;
use tokio::sync::mpsc;
use tracing::debug;

use std::collections::HashMap;
use std::time::Duration;

use crate::config::LlmProviderConfig;
use crate::error::{ProviderError, Result};
use crate::hermes::{self, ReasoningMode};
use crate::provider::Provider;
use crate::sse::parse_sse_line;
use crate::types::{ChatRequest, ChatResponse, StreamChunk};

/// Default base URL for Ollama's OpenAI-compatible endpoint.
pub const OLLAMA_DEFAULT_BASE_URL: &str = "http://localhost:11434/v1";

/// Default base URL for vLLM.
pub const VLLM_DEFAULT_BASE_URL: &str = "http://localhost:8000/v1";

/// Default base URL for llama.cpp server.
pub const LLAMACPP_DEFAULT_BASE_URL: &str = "http://localhost:8080/v1";

/// Default base URL for LM Studio.
pub const LMSTUDIO_DEFAULT_BASE_URL: &str = "http://localhost:1234/v1";

/// Default model for local providers.
pub const DEFAULT_LOCAL_MODEL: &str = "llama3.2";

/// Default base URL for the ADR-060 Hermes serving recipe (`bin/serve-llamacpp`,
/// OpenAI endpoint on port 8090).
///
/// WEFT-604: must stay equal to
/// `clawft_types::config::DEFAULT_LOCAL_LLM_API_BASE`.
pub const HERMES_SERVING_BASE_URL: &str = clawft_types::config::DEFAULT_LOCAL_LLM_API_BASE;

/// Default model id for the ADR-060 Hermes serving recipe.
///
/// WEFT-604: must stay equal to
/// `clawft_types::config::DEFAULT_LOCAL_LLM_MODEL`.
pub const HERMES_DEFAULT_MODEL: &str = clawft_types::config::DEFAULT_LOCAL_LLM_MODEL;

/// Default explicit context window for the Hermes loop (ADR-060: cap to the live
/// window, NOT the 512K native; never rely on a silent 4k default).
pub const HERMES_DEFAULT_NUM_CTX: u32 = 32768;

/// Default timeout for local providers (5 minutes -- local inference can be slow).
const DEFAULT_LOCAL_TIMEOUT_SECS: u64 = 300;

/// A local LLM provider that connects to OpenAI-compatible local inference servers.
///
/// Unlike cloud providers, local providers:
/// - Do not require an API key (but accept one if the server is configured with auth)
/// - Use longer default timeouts (local inference can be slow on CPU)
/// - Default to `http://localhost:11434/v1` (Ollama)
///
/// # Construction
///
/// ```rust,ignore
/// use clawft_llm::LocalProvider;
///
/// // Minimal -- connects to Ollama on default port
/// let provider = LocalProvider::ollama();
///
/// // Custom endpoint
/// let provider = LocalProvider::new(
///     "http://192.168.1.100:8000/v1".to_string(),
///     "my-local-model".to_string(),
///     None, // no API key
/// );
/// ```
pub struct LocalProvider {
    config: LlmProviderConfig,
    http: reqwest::Client,
    api_key: Option<String>,
    /// Explicit context window injected as `options.num_ctx` (ADR-060: no silent
    /// 4k truncation). `None` leaves the server's launch-time window in charge.
    num_ctx: Option<u32>,
    /// Default per-turn Hermes `<think>` reasoning mode for [`Provider::complete`].
    /// Per-turn overrides go through [`LocalProvider::complete_turn`].
    reasoning: ReasoningMode,
}

impl LocalProvider {
    /// Create a local provider with explicit parameters.
    ///
    /// - `base_url`: The OpenAI-compatible API base URL.
    /// - `model`: Default model name to use.
    /// - `api_key`: Optional API key (most local servers don't require one).
    pub fn new(base_url: String, model: String, api_key: Option<String>) -> Self {
        Self::from_config(
            LlmProviderConfig {
                name: "local".into(),
                base_url,
                api_key_env: "LOCAL_LLM_API_KEY".into(),
                model_prefix: Some("local/".into()),
                default_model: Some(model),
                headers: HashMap::new(),
                timeout_secs: Some(DEFAULT_LOCAL_TIMEOUT_SECS),
            },
            api_key,
        )
    }

    /// Create a local provider from a full [`LlmProviderConfig`].
    ///
    /// The API key is optional -- if `api_key` is `None` and the environment
    /// variable is not set, requests will be sent without an `Authorization`
    /// header.
    pub fn from_config(config: LlmProviderConfig, api_key: Option<String>) -> Self {
        let timeout_secs = config.timeout_secs.unwrap_or(DEFAULT_LOCAL_TIMEOUT_SECS);
        Self {
            http: reqwest::ClientBuilder::new()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(timeout_secs))
                .build()
                .expect("failed to build reqwest client"),
            config,
            api_key,
            num_ctx: None,
            reasoning: ReasoningMode::Default,
        }
    }

    /// Create a provider pre-configured for the ADR-060 Hermes serving recipe.
    ///
    /// Points at `bin/serve-llamacpp`'s OpenAI endpoint (port 8090) with the
    /// Hermes default model and an explicit 32k context window so the loop is
    /// never silently truncated to 4k. Reasoning mode defaults to
    /// [`ReasoningMode::Default`] (let the server template decide); set it
    /// per-turn with [`LocalProvider::complete_turn`].
    pub fn hermes_serving() -> Self {
        let mut provider = Self::new(
            HERMES_SERVING_BASE_URL.into(),
            HERMES_DEFAULT_MODEL.into(),
            None,
        );
        provider.config.name = "hermes".into();
        provider.num_ctx = Some(HERMES_DEFAULT_NUM_CTX);
        provider
    }

    /// Classify a failed send. Only a failure to connect (refused, DNS,
    /// connect timeout) is typed as [`ProviderError::Connect`]: nothing
    /// reached the server, so trying another endpoint is safe. A timeout of
    /// the whole request is a server that accepted and then wedged; it is a
    /// plain, non-retryable failure so neither `RetryPolicy` nor a placed
    /// fallback waits it out a second time. Messages carry the base URL.
    fn send_error(&self, e: reqwest::Error) -> ProviderError {
        let (connect, timeout) = (e.is_connect(), e.is_timeout());
        let base = &self.config.base_url;
        let detail = e.without_url();
        if connect {
            ProviderError::Connect(format!("failed to connect to local LLM server at {base}: {detail}"))
        } else if timeout {
            ProviderError::RequestFailed(format!("local LLM server at {base} timed out: {detail}"))
        } else {
            ProviderError::RequestFailed(format!("request to local LLM server at {base} failed: {detail}"))
        }
    }

    /// Set the explicit context window injected as `options.num_ctx`.
    pub fn with_num_ctx(mut self, num_ctx: u32) -> Self {
        self.num_ctx = Some(num_ctx);
        self
    }

    /// Set the default per-turn reasoning mode used by [`Provider::complete`].
    pub fn with_reasoning(mut self, reasoning: ReasoningMode) -> Self {
        self.reasoning = reasoning;
        self
    }

    /// The configured explicit context window, if any.
    pub fn num_ctx(&self) -> Option<u32> {
        self.num_ctx
    }

    /// The default per-turn reasoning mode.
    pub fn reasoning(&self) -> ReasoningMode {
        self.reasoning
    }

    /// Execute a chat completion with an explicit per-turn reasoning mode.
    ///
    /// This is the Hermes-aware completion path (ADR-060): it injects the
    /// explicit `num_ctx` and the `enable_thinking` toggle into the request
    /// body, then normalizes the response so `<think>` reasoning is stripped and
    /// `<tool_call>` blocks are recovered into native `tool_calls` when the
    /// server template did not already parse them. `<think>` content never
    /// reaches the parsed `tool_calls`.
    ///
    /// [`Provider::complete`] delegates here with the provider's default
    /// reasoning mode; call this directly to override it per turn (on for
    /// planning, off for tool execution).
    pub async fn complete_turn(
        &self,
        request: &ChatRequest,
        reasoning: ReasoningMode,
    ) -> Result<ChatResponse> {
        let url = self.completions_url();

        debug!(
            provider = %self.config.name,
            model = %request.model,
            messages = request.messages.len(),
            url = %url,
            num_ctx = ?self.num_ctx,
            reasoning = ?reasoning,
            "sending local chat completion request"
        );

        // Serialize, then augment with the Hermes serving knobs (num_ctx +
        // enable_thinking) that have no field on the shared ChatRequest type.
        let mut body = serde_json::to_value(request)?;
        hermes::augment_request_body(&mut body, self.num_ctx, reasoning);

        let mut req = self
            .http
            .post(&url)
            .header("Content-Type", "application/json");

        if let Some(ref key) = self.resolve_api_key() {
            req = req.header("Authorization", format!("Bearer {key}"));
        }

        for (k, v) in &self.config.headers {
            req = req.header(k.as_str(), v.as_str());
        }

        let response = req.json(&body).send().await.map_err(|e| self.send_error(e))?;

        let status = response.status();

        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();

            if status.as_u16() == 404 {
                return Err(ProviderError::ModelNotFound(format!(
                    "model '{}' not found on local server ({}): {}",
                    request.model, self.config.base_url, body
                )));
            }

            let code = status.as_u16();
            if (500..=599).contains(&code) {
                return Err(ProviderError::ServerError { status: code, body });
            }

            return Err(ProviderError::RequestFailed(format!(
                "HTTP {status}: {body}"
            )));
        }

        let mut chat_response: ChatResponse = response.json().await.map_err(|e| {
            ProviderError::InvalidResponse(format!("failed to parse local response: {e}"))
        })?;

        // Normalize each choice: strip <think>, recover <tool_call> blocks.
        for choice in &mut chat_response.choices {
            let recovered = hermes::normalize_message(&mut choice.message);
            if recovered && choice.finish_reason.as_deref() != Some("tool_calls") {
                choice.finish_reason = Some("tool_calls".to_string());
            }
        }

        debug!(
            provider = %self.config.name,
            model = %chat_response.model,
            choices = chat_response.choices.len(),
            "local chat completion response received"
        );

        Ok(chat_response)
    }

    /// Create a provider pre-configured for Ollama on the default port.
    pub fn ollama() -> Self {
        Self::new(
            OLLAMA_DEFAULT_BASE_URL.into(),
            DEFAULT_LOCAL_MODEL.into(),
            None,
        )
    }

    /// Create a provider pre-configured for vLLM on the default port.
    pub fn vllm(model: String) -> Self {
        let mut provider = Self::new(VLLM_DEFAULT_BASE_URL.into(), model, None);
        provider.config.name = "vllm".into();
        provider
    }

    /// Create a provider pre-configured for llama.cpp server on the default port.
    pub fn llamacpp() -> Self {
        let mut provider = Self::new(
            LLAMACPP_DEFAULT_BASE_URL.into(),
            DEFAULT_LOCAL_MODEL.into(),
            None,
        );
        provider.config.name = "llamacpp".into();
        provider
    }

    /// Create a provider pre-configured for LM Studio on the default port.
    pub fn lmstudio(model: String) -> Self {
        let mut provider = Self::new(LMSTUDIO_DEFAULT_BASE_URL.into(), model, None);
        provider.config.name = "lmstudio".into();
        provider
    }

    /// Returns the provider configuration.
    pub fn config(&self) -> &LlmProviderConfig {
        &self.config
    }

    /// Returns the chat completions endpoint URL.
    fn completions_url(&self) -> String {
        let base = self.config.base_url.trim_end_matches('/');
        format!("{base}/chat/completions")
    }

    /// Returns the models listing endpoint URL.
    fn models_url(&self) -> String {
        let base = self.config.base_url.trim_end_matches('/');
        format!("{base}/models")
    }

    /// Resolve the API key, returning `None` if no key is available.
    ///
    /// Unlike cloud providers, missing keys are not an error for local providers.
    fn resolve_api_key(&self) -> Option<String> {
        if let Some(ref key) = self.api_key {
            return Some(key.clone());
        }
        std::env::var(&self.config.api_key_env).ok()
    }

    /// List available models on the local server.
    ///
    /// Calls the `/v1/models` endpoint and returns model IDs.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] if the server is unreachable or returns
    /// an unexpected response format.
    pub async fn list_models(&self) -> Result<Vec<String>> {
        let url = self.models_url();

        debug!(
            provider = %self.config.name,
            url = %url,
            "listing local models"
        );

        let mut req = self.http.get(&url);

        if let Some(ref key) = self.resolve_api_key() {
            req = req.header("Authorization", format!("Bearer {key}"));
        }

        for (k, v) in &self.config.headers {
            req = req.header(k.as_str(), v.as_str());
        }

        let response = req.send().await.map_err(|e| self.send_error(e))?;

        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(ProviderError::RequestFailed(format!(
                "model listing failed: {body}"
            )));
        }

        let body: serde_json::Value = response.json().await.map_err(|e| {
            ProviderError::InvalidResponse(format!("failed to parse models response: {e}"))
        })?;

        // OpenAI format: {"data": [{"id": "model-name", ...}, ...]}
        // Ollama format: {"models": [{"name": "model-name", ...}, ...]}
        let models = if let Some(data) = body.get("data").and_then(|d| d.as_array()) {
            data.iter()
                .filter_map(|m| m.get("id").and_then(|id| id.as_str()))
                .map(String::from)
                .collect()
        } else if let Some(models) = body.get("models").and_then(|m| m.as_array()) {
            models
                .iter()
                .filter_map(|m| {
                    m.get("name")
                        .or_else(|| m.get("id"))
                        .and_then(|n| n.as_str())
                })
                .map(String::from)
                .collect()
        } else {
            Vec::new()
        };

        debug!(
            provider = %self.config.name,
            count = models.len(),
            "local models listed"
        );

        Ok(models)
    }
}

#[async_trait]
impl Provider for LocalProvider {
    fn name(&self) -> &str {
        &self.config.name
    }

    async fn complete(&self, request: &ChatRequest) -> Result<ChatResponse> {
        // Delegate to the Hermes-aware path with the provider's default
        // reasoning mode; per-turn overrides use `complete_turn` directly.
        self.complete_turn(request, self.reasoning).await
    }

    async fn complete_stream(
        &self,
        request: &ChatRequest,
        tx: mpsc::Sender<StreamChunk>,
    ) -> Result<()> {
        let url = self.completions_url();

        debug!(
            provider = %self.config.name,
            model = %request.model,
            messages = request.messages.len(),
            "sending local streaming chat completion request"
        );

        let mut stream_request = request.clone();
        stream_request.stream = Some(true);

        let mut req = self
            .http
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream");

        if let Some(ref key) = self.resolve_api_key() {
            req = req.header("Authorization", format!("Bearer {key}"));
        }

        for (k, v) in &self.config.headers {
            req = req.header(k.as_str(), v.as_str());
        }

        // Same Hermes-aware body as the non-streaming path: the explicit
        // context window and the per-turn reasoning switch apply to a
        // streamed turn too.
        let mut body = serde_json::to_value(&stream_request).map_err(|e| ProviderError::InvalidResponse(e.to_string()))?;
        hermes::augment_request_body(&mut body, self.num_ctx, self.reasoning);
        let response = req.json(&body).send().await.map_err(|e| self.send_error(e))?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            if status.as_u16() == 404 {
                return Err(ProviderError::ModelNotFound(format!(
                    "model '{}' not found on local server: {}",
                    request.model, body
                )));
            }
            let code = status.as_u16();
            if (500..=599).contains(&code) {
                return Err(ProviderError::ServerError { status: code, body });
            }
            return Err(ProviderError::RequestFailed(format!(
                "HTTP {status}: {body}"
            )));
        }

        // Read the SSE stream line by line
        use futures_util::StreamExt;
        let mut byte_stream = response.bytes_stream();
        let mut buffer = String::new();

        while let Some(chunk_result) = byte_stream.next().await {
            let bytes = chunk_result
                .map_err(|e| ProviderError::RequestFailed(format!("stream read error: {e}")))?;

            let text = String::from_utf8_lossy(&bytes);
            buffer.push_str(&text);

            while let Some(newline_pos) = buffer.find('\n') {
                let line = buffer[..newline_pos].to_string();
                buffer = buffer[newline_pos + 1..].to_string();

                let chunks = match parse_sse_line(&line) {
                    Ok(c) => c,
                    Err(_) => continue,
                };

                for chunk in chunks {
                    if tx.send(chunk).await.is_err() {
                        debug!(
                            provider = %self.config.name,
                            "stream receiver dropped, stopping"
                        );
                        return Ok(());
                    }
                }
            }
        }

        if !buffer.trim().is_empty()
            && let Ok(chunks) = parse_sse_line(&buffer)
        {
            for chunk in chunks {
                let _ = tx.send(chunk).await;
            }
        }

        debug!(
            provider = %self.config.name,
            "local streaming complete"
        );

        Ok(())
    }
}

impl std::fmt::Debug for LocalProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalProvider")
            .field("name", &self.config.name)
            .field("base_url", &self.config.base_url)
            .field("default_model", &self.config.default_model)
            .field("api_key", &self.api_key.as_ref().map(|_| "***"))
            .finish()
    }
}

/// Create an [`LlmProviderConfig`] for a local/Ollama provider.
///
/// This is a convenience function for use in the builtin provider list.
pub fn local_provider_config(
    name: &str,
    base_url: &str,
    default_model: &str,
    prefix: &str,
) -> LlmProviderConfig {
    LlmProviderConfig {
        name: name.into(),
        base_url: base_url.into(),
        api_key_env: "LOCAL_LLM_API_KEY".into(),
        model_prefix: Some(prefix.into()),
        default_model: Some(default_model.into()),
        headers: HashMap::new(),
        timeout_secs: Some(DEFAULT_LOCAL_TIMEOUT_SECS),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ChatMessage;

    // ── Construction tests ──────────────────────────────────────────

    #[test]
    fn ollama_defaults() {
        let provider = LocalProvider::ollama();
        assert_eq!(provider.name(), "local");
        assert_eq!(provider.config().base_url, OLLAMA_DEFAULT_BASE_URL);
        assert_eq!(
            provider.config().default_model.as_deref(),
            Some(DEFAULT_LOCAL_MODEL)
        );
        assert!(provider.api_key.is_none());
    }

    #[test]
    fn vllm_defaults() {
        let provider = LocalProvider::vllm("mistral-7b".into());
        assert_eq!(provider.name(), "vllm");
        assert_eq!(provider.config().base_url, VLLM_DEFAULT_BASE_URL);
        assert_eq!(
            provider.config().default_model.as_deref(),
            Some("mistral-7b")
        );
    }

    #[test]
    fn llamacpp_defaults() {
        let provider = LocalProvider::llamacpp();
        assert_eq!(provider.name(), "llamacpp");
        assert_eq!(provider.config().base_url, LLAMACPP_DEFAULT_BASE_URL);
    }

    #[test]
    fn lmstudio_defaults() {
        let provider = LocalProvider::lmstudio("phi-3".into());
        assert_eq!(provider.name(), "lmstudio");
        assert_eq!(provider.config().base_url, LMSTUDIO_DEFAULT_BASE_URL);
        assert_eq!(provider.config().default_model.as_deref(), Some("phi-3"));
    }

    #[test]
    fn hermes_serving_defaults() {
        let provider = LocalProvider::hermes_serving();
        assert_eq!(provider.name(), "hermes");
        assert_eq!(provider.config().base_url, HERMES_SERVING_BASE_URL);
        assert_eq!(
            provider.config().default_model.as_deref(),
            Some(HERMES_DEFAULT_MODEL)
        );
        // Explicit window so the loop is never silently truncated to 4k.
        assert_eq!(provider.num_ctx(), Some(HERMES_DEFAULT_NUM_CTX));
        assert_eq!(provider.reasoning(), ReasoningMode::Default);
    }

    #[test]
    fn with_num_ctx_and_reasoning_builders() {
        let provider = LocalProvider::ollama()
            .with_num_ctx(16384)
            .with_reasoning(ReasoningMode::Off);
        assert_eq!(provider.num_ctx(), Some(16384));
        assert_eq!(provider.reasoning(), ReasoningMode::Off);
    }

    #[test]
    fn custom_local_endpoint() {
        let provider = LocalProvider::new(
            "http://192.168.1.50:9000/v1".into(),
            "hermes-3-llama-3.1-8b".into(),
            Some("custom-key".into()),
        );
        assert_eq!(provider.config().base_url, "http://192.168.1.50:9000/v1");
        assert_eq!(
            provider.config().default_model.as_deref(),
            Some("hermes-3-llama-3.1-8b")
        );
        assert_eq!(provider.api_key.as_deref(), Some("custom-key"));
    }

    // ── URL construction ────────────────────────────────────────────

    #[test]
    fn completions_url_construction() {
        let provider = LocalProvider::ollama();
        assert_eq!(
            provider.completions_url(),
            "http://localhost:11434/v1/chat/completions"
        );
    }

    #[test]
    fn completions_url_strips_trailing_slash() {
        let provider = LocalProvider::new("http://localhost:11434/v1/".into(), "test".into(), None);
        assert_eq!(
            provider.completions_url(),
            "http://localhost:11434/v1/chat/completions"
        );
    }

    #[test]
    fn models_url_construction() {
        let provider = LocalProvider::ollama();
        assert_eq!(provider.models_url(), "http://localhost:11434/v1/models");
    }

    // ── API key resolution ──────────────────────────────────────────

    #[test]
    fn resolve_api_key_none_when_missing() {
        let provider = LocalProvider::ollama();
        // No explicit key and env var unlikely to be set
        // This is the key difference from cloud providers -- no error
        let key = provider.resolve_api_key();
        // We can't assert None because LOCAL_LLM_API_KEY might be set,
        // but at minimum it should not panic or error
        let _ = key;
    }

    #[test]
    fn resolve_api_key_explicit() {
        let provider = LocalProvider::new(
            OLLAMA_DEFAULT_BASE_URL.into(),
            "test".into(),
            Some("my-local-key".into()),
        );
        assert_eq!(provider.resolve_api_key(), Some("my-local-key".into()));
    }

    // ── Request formatting ──────────────────────────────────────────

    #[test]
    fn request_body_format() {
        let request = ChatRequest::new(
            "llama3.2",
            vec![
                ChatMessage::system("You are a helpful assistant."),
                ChatMessage::user("What is Rust?"),
            ],
        );
        let json = serde_json::to_value(&request).unwrap();

        assert_eq!(json["model"], "llama3.2");
        assert_eq!(json["messages"].as_array().unwrap().len(), 2);
        assert_eq!(json["messages"][0]["role"], "system");
        assert_eq!(json["messages"][1]["role"], "user");
        assert_eq!(json["messages"][1]["content"], "What is Rust?");
        // stream should not be present by default
        assert!(json.get("stream").is_none());
    }

    #[test]
    fn request_body_with_stream() {
        let mut request = ChatRequest::new("llama3.2", vec![ChatMessage::user("Hello")]);
        request.stream = Some(true);
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["stream"], true);
    }

    #[test]
    fn request_body_with_options() {
        let request = ChatRequest {
            model: "hermes-3-llama-3.1-8b".into(),
            messages: vec![ChatMessage::user("test")],
            max_tokens: Some(2048),
            temperature: Some(0.7),
            tools: Vec::new(),
            stream: None,
            tool_choice: None,
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["model"], "hermes-3-llama-3.1-8b");
        assert_eq!(json["max_tokens"], 2048);
        assert_eq!(json["temperature"], 0.7);
    }

    // ── Response parsing ────────────────────────────────────────────

    #[test]
    fn parse_ollama_response() {
        // Ollama returns standard OpenAI format via /v1 endpoint
        let json = r#"{
            "id": "chatcmpl-123",
            "object": "chat.completion",
            "created": 1677652288,
            "model": "llama3.2",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "Rust is a systems programming language."
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 12,
                "completion_tokens": 8,
                "total_tokens": 20
            }
        }"#;

        let response: ChatResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.model, "llama3.2");
        assert_eq!(response.choices.len(), 1);
        assert_eq!(
            response.choices[0].message.content.as_deref(),
            Some("Rust is a systems programming language.")
        );
        assert_eq!(response.choices[0].finish_reason.as_deref(), Some("stop"));
        let usage = response.usage.unwrap();
        assert_eq!(usage.input_tokens, 12);
        assert_eq!(usage.output_tokens, 8);
        assert_eq!(usage.total_tokens, 20);
    }

    #[test]
    fn parse_vllm_response() {
        // vLLM returns standard OpenAI format
        let json = r#"{
            "id": "cmpl-abc123",
            "object": "chat.completion",
            "created": 1700000000,
            "model": "NousResearch/Hermes-3-Llama-3.1-8B",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "Hello! How can I help you today?"
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 5,
                "completion_tokens": 9,
                "total_tokens": 14
            }
        }"#;

        let response: ChatResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.model, "NousResearch/Hermes-3-Llama-3.1-8B");
        assert_eq!(
            response.choices[0].message.content.as_deref(),
            Some("Hello! How can I help you today?")
        );
    }

    #[test]
    fn parse_response_without_usage() {
        // Some local servers omit usage stats
        let json = r#"{
            "id": "chatcmpl-local",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "Hi!"
                },
                "finish_reason": "stop"
            }],
            "usage": null,
            "model": "llama3.2"
        }"#;

        let response: ChatResponse = serde_json::from_str(json).unwrap();
        assert!(response.usage.is_none());
        assert_eq!(response.choices[0].message.content.as_deref(), Some("Hi!"));
    }

    #[test]
    fn parse_response_with_tool_calls() {
        // Hermes models support function calling
        let json = r#"{
            "id": "chatcmpl-hermes",
            "model": "hermes-3-llama-3.1-8b",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "tool_calls": [{
                        "id": "call_001",
                        "type": "function",
                        "function": {
                            "name": "get_weather",
                            "arguments": "{\"city\": \"San Francisco\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {
                "prompt_tokens": 20,
                "completion_tokens": 15,
                "total_tokens": 35
            }
        }"#;

        let response: ChatResponse = serde_json::from_str(json).unwrap();
        let tool_calls = response.choices[0].message.tool_calls.as_ref().unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].function.name, "get_weather");
        assert!(tool_calls[0].function.arguments.contains("San Francisco"));
    }

    // ── Model listing response parsing ──────────────────────────────

    #[test]
    fn parse_openai_models_response() {
        let json = r#"{
            "data": [
                {"id": "llama3.2", "object": "model"},
                {"id": "codellama", "object": "model"},
                {"id": "hermes-3", "object": "model"}
            ]
        }"#;
        let body: serde_json::Value = serde_json::from_str(json).unwrap();
        let models: Vec<String> = body
            .get("data")
            .and_then(|d| d.as_array())
            .unwrap()
            .iter()
            .filter_map(|m| m.get("id").and_then(|id| id.as_str()))
            .map(String::from)
            .collect();
        assert_eq!(models, vec!["llama3.2", "codellama", "hermes-3"]);
    }

    #[test]
    fn parse_ollama_models_response() {
        // Ollama's native /api/tags returns a different format, but
        // the /v1/models endpoint uses the OpenAI format
        let json = r#"{
            "models": [
                {"name": "llama3.2:latest"},
                {"name": "hermes-3:8b"}
            ]
        }"#;
        let body: serde_json::Value = serde_json::from_str(json).unwrap();
        let models: Vec<String> = body
            .get("models")
            .and_then(|m| m.as_array())
            .unwrap()
            .iter()
            .filter_map(|m| m.get("name").and_then(|n| n.as_str()))
            .map(String::from)
            .collect();
        assert_eq!(models, vec!["llama3.2:latest", "hermes-3:8b"]);
    }

    // ── Debug output ────────────────────────────────────────────────

    #[test]
    fn debug_hides_api_key() {
        let provider = LocalProvider::new(
            OLLAMA_DEFAULT_BASE_URL.into(),
            "test".into(),
            Some("secret-key-123".into()),
        );
        let debug_str = format!("{:?}", provider);
        assert!(!debug_str.contains("secret-key-123"));
        assert!(debug_str.contains("***"));
    }

    #[test]
    fn debug_shows_none_for_missing_key() {
        let provider = LocalProvider::ollama();
        let debug_str = format!("{:?}", provider);
        assert!(debug_str.contains("None"));
        assert!(debug_str.contains("localhost:11434"));
    }

    // ── Config helper ───────────────────────────────────────────────

    #[test]
    fn local_provider_config_helper() {
        let config = local_provider_config(
            "ollama",
            OLLAMA_DEFAULT_BASE_URL,
            DEFAULT_LOCAL_MODEL,
            "local/",
        );
        assert_eq!(config.name, "ollama");
        assert_eq!(config.base_url, "http://localhost:11434/v1");
        assert_eq!(config.default_model.as_deref(), Some("llama3.2"));
        assert_eq!(config.model_prefix.as_deref(), Some("local/"));
        assert_eq!(config.timeout_secs, Some(300));
    }

    // ── Timeout configuration ───────────────────────────────────────

    #[test]
    fn default_timeout_is_300s() {
        let provider = LocalProvider::ollama();
        assert_eq!(
            provider.config().timeout_secs,
            Some(DEFAULT_LOCAL_TIMEOUT_SECS)
        );
    }

    #[test]
    fn custom_timeout() {
        let config = LlmProviderConfig {
            name: "local".into(),
            base_url: OLLAMA_DEFAULT_BASE_URL.into(),
            api_key_env: "LOCAL_LLM_API_KEY".into(),
            model_prefix: Some("local/".into()),
            default_model: Some("test".into()),
            headers: HashMap::new(),
            timeout_secs: Some(60),
        };
        let provider = LocalProvider::from_config(config, None);
        assert_eq!(provider.config().timeout_secs, Some(60));
    }

    // ── Streaming request format ────────────────────────────────────

    #[test]
    fn stream_request_sets_stream_true() {
        let mut request = ChatRequest::new("llama3.2", vec![ChatMessage::user("Hi")]);
        request.stream = Some(true);
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["stream"], true);
        assert_eq!(json["model"], "llama3.2");
    }

    // ── SSE parsing for local providers ─────────────────────────────

    #[test]
    fn parse_local_sse_text_delta() {
        use crate::sse::parse_sse_line;
        let line = r#"data: {"id":"chatcmpl-local","choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}]}"#;
        let chunks = parse_sse_line(line).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(
            chunks[0],
            StreamChunk::TextDelta {
                text: "Hello".into()
            }
        );
    }

    #[test]
    fn parse_local_sse_done() {
        use crate::sse::parse_sse_line;
        let line = r#"data: {"id":"chatcmpl-local","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#;
        let chunks = parse_sse_line(line).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(
            chunks[0],
            StreamChunk::Done {
                finish_reason: Some("stop".into()),
                usage: None,
            }
        );
    }

    #[test]
    fn parse_local_sse_with_usage() {
        use crate::sse::parse_sse_line;
        // Ollama includes usage in the final chunk
        let line = r#"data: {"id":"chatcmpl-local","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":20,"total_tokens":30}}"#;
        let chunks = parse_sse_line(line).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(
            chunks[0],
            StreamChunk::Done {
                finish_reason: Some("stop".into()),
                usage: Some(crate::types::Usage {
                    input_tokens: 10,
                    output_tokens: 20,
                    total_tokens: 30,
                }),
            }
        );
    }
}
