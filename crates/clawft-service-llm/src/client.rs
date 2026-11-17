//! HTTP client for an OpenAI-compatible chat-completions endpoint
//! (typically a local `llama-server`).
//!
//! Mirrors `clawft-service-whisper`'s `client.rs` shape:
//!
//! - [`LlmClient::health`]   — `GET /health` once.
//! - [`LlmClient::complete`] — `POST /v1/chat/completions` with the
//!   in-flight semaphore (permits=1) so we never pipeline a second
//!   request against `llama-server`'s single-batch core and stall on
//!   the OS accept backlog.
//!
//! No substrate or kernel knowledge — that lives in `clawft-weave`.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::Semaphore;
use tracing::{debug, warn};

use crate::{DEFAULT_LLM_MODEL, DEFAULT_LLM_SERVICE_URL, LLM_SERVICE_URL_ENV};

/// Configuration for [`LlmClient`].
///
/// `Debug` is hand-written so the bearer token in `api_key` never reaches
/// logs or panic messages.
#[derive(Clone)]
pub struct LlmConfig {
    /// Base URL of the LLM service (e.g. `http://127.0.0.1:8090`).
    /// No trailing slash.
    pub base_url: String,
    /// Model name to send in the request body. `llama-server` ignores
    /// this for routing (it serves whatever model it loaded), but it's
    /// echoed in the response and shows up in logs.
    pub model: String,
    /// Request timeout for a single completion. Generation can take
    /// many seconds for large outputs; default 300s covers a single
    /// long generation on a moderately quantized 35B at low throughput
    /// and matches the panel-side `LLM_TIMEOUT_MS` per-method bucket.
    pub request_timeout: Duration,
    /// How long [`LlmClient::wait_for_healthy`] will poll before
    /// giving up.
    pub health_deadline: Duration,
    /// Default sampling temperature when the per-call value is `None`.
    pub default_temperature: f32,
    /// Default `max_tokens` when the per-call value is `None`.
    /// Set deliberately conservative so a forgotten cap doesn't pin
    /// the server for minutes on a single request.
    pub default_max_tokens: u32,
    /// Optional bearer token. When `Some`, sent as
    /// `Authorization: Bearer <token>`. Required for OpenRouter (and any
    /// other hosted OpenAI-compat API); left `None` for local
    /// `llama-server` so the wire shape stays byte-identical to the
    /// pre-OpenRouter path.
    pub api_key: Option<String>,
    /// Optional `HTTP-Referer` header. OpenRouter uses this for app
    /// attribution in its dashboard; safe to leave `None` for local
    /// servers.
    pub referer: Option<String>,
    /// Optional `X-Title` header. OpenRouter shows this alongside the
    /// referer for attribution.
    pub app_title: Option<String>,
}

impl std::fmt::Debug for LlmConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmConfig")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("request_timeout", &self.request_timeout)
            .field("health_deadline", &self.health_deadline)
            .field("default_temperature", &self.default_temperature)
            .field("default_max_tokens", &self.default_max_tokens)
            .field("api_key", &self.api_key.as_ref().map(|_| "***"))
            .field("referer", &self.referer)
            .field("app_title", &self.app_title)
            .finish()
    }
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_LLM_SERVICE_URL.to_string(),
            model: DEFAULT_LLM_MODEL.to_string(),
            request_timeout: Duration::from_secs(300),
            health_deadline: Duration::from_secs(10),
            default_temperature: 0.2,
            default_max_tokens: 512,
            api_key: None,
            referer: None,
            app_title: None,
        }
    }
}

impl LlmConfig {
    /// Build a config honoring the `LLM_SERVICE_URL` env var.
    ///
    /// Falls back to [`DEFAULT_LLM_SERVICE_URL`] if unset or empty.
    pub fn from_env() -> Self {
        let base_url = std::env::var(LLM_SERVICE_URL_ENV)
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_LLM_SERVICE_URL.to_string());
        let model = std::env::var(crate::LLM_MODEL_ENV)
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_LLM_MODEL.to_string());
        Self {
            base_url,
            model,
            ..Default::default()
        }
    }
}

/// One content block inside a multimodal / structured message.
///
/// OpenAI vision uses `type: "text"` / `type: "image_url"`; Anthropic-
/// style payloads use `type: "text"` / `type: "image"` (with a `source`
/// object). Unknown block kinds are preserved so providers that add
/// new block types do not fail deserialization — consumers that only
/// care about text should call [`MessageContent::as_text`], which
/// joins the `text` fields of every block that carries one.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ContentBlock {
    /// Block kind: `"text"`, `"image_url"`, `"image"`, etc.
    #[serde(rename = "type")]
    pub kind: String,
    /// Present on `type: "text"` blocks (and occasionally on others).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Present on OpenAI `type: "image_url"` blocks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_url: Option<ImageUrl>,
    /// Present on Anthropic-style `type: "image"` blocks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<serde_json::Value>,
}

/// OpenAI vision `image_url` payload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ImageUrl {
    /// HTTPS URL or `data:` URI for the image.
    pub url: String,
    /// Optional detail hint (`"auto"`, `"low"`, `"high"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Message content as it appears on the wire: a plain string, an array
/// of content blocks (vision / multimodal), or JSON `null`.
///
/// Most daemon paths only need the flattened text view — use
/// [`MessageContent::as_text`] / [`MessageContent::into_text`]. The
/// block form is retained so callers that care about images (or that
/// re-serialize the original shape) can still reach it.
#[derive(Debug, Clone, PartialEq)]
pub enum MessageContent {
    /// Plain text content (the historical / default shape).
    Text(String),
    /// Multimodal or structured content blocks.
    Blocks(Vec<ContentBlock>),
}

impl Default for MessageContent {
    fn default() -> Self {
        Self::Text(String::new())
    }
}

impl MessageContent {
    /// Flatten to plain text.
    ///
    /// - [`MessageContent::Text`] → the string as-is (including empty).
    /// - [`MessageContent::Blocks`] → join non-empty `text` fields of
    ///   every block, separated by newlines. Image-only blocks
    ///   contribute nothing, so a pure-image array yields `""`.
    pub fn as_text(&self) -> std::borrow::Cow<'_, str> {
        match self {
            MessageContent::Text(s) => std::borrow::Cow::Borrowed(s.as_str()),
            MessageContent::Blocks(blocks) => {
                let mut parts: Vec<&str> = Vec::new();
                for b in blocks {
                    if let Some(t) = b.text.as_deref()
                        && !t.is_empty()
                    {
                        parts.push(t);
                    }
                }
                std::borrow::Cow::Owned(parts.join("\n"))
            }
        }
    }

    /// Consume and return the flattened text (see [`Self::as_text`]).
    pub fn into_text(self) -> String {
        match self {
            MessageContent::Text(s) => s,
            MessageContent::Blocks(blocks) => {
                let mut parts: Vec<&str> = Vec::new();
                for b in &blocks {
                    if let Some(t) = b.text.as_deref()
                        && !t.is_empty()
                    {
                        parts.push(t);
                    }
                }
                parts.join("\n")
            }
        }
    }

    /// `true` when there is no textual content (empty string, empty
    /// blocks, or image-only blocks).
    pub fn is_empty(&self) -> bool {
        match self {
            MessageContent::Text(s) => s.is_empty(),
            MessageContent::Blocks(blocks) => blocks.iter().all(|b| {
                b.text
                    .as_deref()
                    .map(|t| t.is_empty())
                    .unwrap_or(true)
            }),
        }
    }

    /// Borrow the block list when content arrived as an array.
    pub fn blocks(&self) -> Option<&[ContentBlock]> {
        match self {
            MessageContent::Blocks(b) => Some(b.as_slice()),
            MessageContent::Text(_) => None,
        }
    }
}

impl From<String> for MessageContent {
    fn from(s: String) -> Self {
        MessageContent::Text(s)
    }
}

impl From<&str> for MessageContent {
    fn from(s: &str) -> Self {
        MessageContent::Text(s.to_string())
    }
}

impl From<Vec<ContentBlock>> for MessageContent {
    fn from(blocks: Vec<ContentBlock>) -> Self {
        MessageContent::Blocks(blocks)
    }
}

impl std::fmt::Display for MessageContent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.as_text())
    }
}

impl PartialEq<str> for MessageContent {
    fn eq(&self, other: &str) -> bool {
        self.as_text() == other
    }
}

impl PartialEq<&str> for MessageContent {
    fn eq(&self, other: &&str) -> bool {
        self.as_text() == *other
    }
}

impl PartialEq<String> for MessageContent {
    fn eq(&self, other: &String) -> bool {
        self.as_text() == other.as_str()
    }
}

impl Serialize for MessageContent {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        match self {
            MessageContent::Text(s) => ser.serialize_str(s),
            MessageContent::Blocks(blocks) => blocks.serialize(ser),
        }
    }
}

impl<'de> Deserialize<'de> for MessageContent {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        // Accept string | array | null | missing (via Option).
        // Nemotron/OpenRouter emit null on tool-call turns; OpenAI
        // vision and Anthropic-style APIs emit content-block arrays.
        let value = Option::<serde_json::Value>::deserialize(d)?;
        match value {
            None | Some(serde_json::Value::Null) => Ok(MessageContent::Text(String::new())),
            Some(serde_json::Value::String(s)) => Ok(MessageContent::Text(s)),
            Some(serde_json::Value::Array(arr)) => {
                let blocks: Vec<ContentBlock> =
                    serde_json::from_value(serde_json::Value::Array(arr))
                        .map_err(serde::de::Error::custom)?;
                Ok(MessageContent::Blocks(blocks))
            }
            Some(other) => Err(serde::de::Error::custom(format!(
                "content must be string, array of content blocks, or null; got {other}"
            ))),
        }
    }
}

/// One message in a chat completion conversation.
///
/// `role` is one of `"system"`, `"user"`, `"assistant"`, `"tool"`. We
/// don't gate on the value here — the server validates and rejects.
///
/// Tool-call additions:
/// - When the assistant emits tool calls, `content` may be empty and
///   `tool_calls` carries the structured calls.
/// - When relaying a tool result back to the model, `role` is `"tool"`,
///   `content` is the tool's stringified result, and `tool_call_id`
///   matches the originating call's id.
///
/// Content shape: providers emit string, array-of-blocks (vision), or
/// `null` (Nemotron tool-calls). All three deserialize into
/// [`MessageContent`]; use [`MessageContent::as_text`] when only the
/// flattened string is needed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    /// Role: `system` / `user` / `assistant` / `tool`.
    pub role: String,
    /// Message content. Empty text for tool-call-only assistant
    /// turns. Some providers emit JSON `null` here instead of `""`
    /// when the assistant turn is purely a tool call; both become
    /// [`MessageContent::Text`]`("")`. Vision / multimodal providers
    /// emit an array of content blocks → [`MessageContent::Blocks`].
    #[serde(default)]
    pub content: MessageContent,
    /// Tool calls produced by the assistant. Present on assistant
    /// messages whose `finish_reason` is `"tool_calls"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    /// On `role: "tool"` messages, the id of the tool call this is a
    /// response to. Required by the OpenAI-compat schema for tool
    /// results to be accepted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    /// Convenience constructor for a system prompt.
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: MessageContent::Text(content.into()),
            tool_calls: None,
            tool_call_id: None,
        }
    }
    /// Convenience constructor for a user turn.
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: MessageContent::Text(content.into()),
            tool_calls: None,
            tool_call_id: None,
        }
    }
    /// Convenience constructor for an assistant turn.
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".into(),
            content: MessageContent::Text(content.into()),
            tool_calls: None,
            tool_call_id: None,
        }
    }
    /// Convenience constructor for a `role: "tool"` reply that closes
    /// out a tool call from a previous assistant turn.
    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".into(),
            content: MessageContent::Text(content.into()),
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
        }
    }

    /// Multimodal user (or other) message with structured content blocks.
    pub fn with_blocks(role: impl Into<String>, blocks: Vec<ContentBlock>) -> Self {
        Self {
            role: role.into(),
            content: MessageContent::Blocks(blocks),
            tool_calls: None,
            tool_call_id: None,
        }
    }
}

/// A tool definition the model is allowed to call. OpenAI-compatible
/// `{"type":"function","function":{...}}` shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tool {
    /// Always `"function"` for the OpenAI-compat schema we target.
    #[serde(rename = "type")]
    pub kind: String,
    /// The function specification.
    pub function: ToolFunction,
}

impl Tool {
    /// Build a `function`-typed tool from a name, description, and
    /// JSON-schema parameter spec.
    pub fn function(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: serde_json::Value,
    ) -> Self {
        Self {
            kind: "function".into(),
            function: ToolFunction {
                name: name.into(),
                description: description.into(),
                parameters,
            },
        }
    }
}

/// The function spec attached to a [`Tool`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolFunction {
    /// Tool name. Must be a valid identifier on the server side.
    pub name: String,
    /// Free-text description; the model uses this to decide when to
    /// call.
    #[serde(default)]
    pub description: String,
    /// JSON-schema describing the call's arguments. The model emits
    /// values matching this schema in the `arguments` string of the
    /// returned [`ToolCall`].
    pub parameters: serde_json::Value,
}

/// One tool call emitted by the assistant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    /// Server-assigned identifier; echo back in the matching
    /// [`ChatMessage::tool`] reply.
    pub id: String,
    /// Always `"function"` for the OpenAI-compat schema.
    #[serde(default = "default_tool_call_kind", rename = "type")]
    pub kind: String,
    /// The function being called.
    pub function: ToolCallFunction,
}

fn default_tool_call_kind() -> String {
    "function".to_string()
}

/// The function name + JSON-stringified arguments emitted in a
/// [`ToolCall`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallFunction {
    /// Tool name the assistant chose.
    pub name: String,
    /// JSON-encoded arguments. The model returns this as a string per
    /// the OpenAI schema; callers must `serde_json::from_str` to parse.
    pub arguments: String,
}

/// Tool-choice strategy. `Auto` (default) lets the model decide;
/// `None` disables tool calls; `Required` forces one tool call;
/// `Function(name)` pins to a specific tool.
#[derive(Debug, Clone)]
pub enum ToolChoice {
    /// Server default — equivalent to omitting `tool_choice`.
    Auto,
    /// Disallow tool calls (model must respond with content only).
    None,
    /// Require any tool call.
    Required,
    /// Force a specific tool by name.
    Function(String),
}

impl serde::Serialize for ToolChoice {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        match self {
            ToolChoice::Auto => ser.serialize_str("auto"),
            ToolChoice::None => ser.serialize_str("none"),
            ToolChoice::Required => ser.serialize_str("required"),
            ToolChoice::Function(name) => {
                use serde::ser::SerializeMap;
                let mut m = ser.serialize_map(Some(2))?;
                m.serialize_entry("type", "function")?;
                m.serialize_entry("function", &serde_json::json!({ "name": name }))?;
                m.end()
            }
        }
    }
}

/// Wire shape for `GET /v1/models` response (WEFT-256).
#[derive(Debug, Clone, Deserialize)]
struct ModelsListResponse {
    #[serde(default)]
    data: Vec<ModelsListEntry>,
}

#[derive(Debug, Clone, Deserialize)]
struct ModelsListEntry {
    #[serde(default)]
    id: String,
}

/// Wire shape for `POST /v1/chat/completions` request body.
#[derive(Debug, Clone, Serialize)]
pub struct ChatRequest {
    /// Model name (echoed; not used for routing on llama-server).
    pub model: String,
    /// Conversation so far. The last entry should normally be `user`.
    pub messages: Vec<ChatMessage>,
    /// Sampling temperature.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Hard cap on generated tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Streaming flag. Always `false` for [`LlmClient::complete`];
    /// streaming would land as a separate method that flips this to
    /// `true` and parses SSE.
    pub stream: bool,
    /// Tools the model is allowed to call. Omitted when empty so we
    /// stay byte-compatible with the no-tools wire shape that existed
    /// before tool support landed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,
    /// Strategy for choosing among the supplied tools.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
}

/// One choice in a chat completion response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatChoice {
    /// The assistant message produced for this choice.
    pub message: ChatMessage,
    /// `"stop"`, `"length"`, etc. Optional because some servers omit
    /// it on early returns.
    #[serde(default)]
    pub finish_reason: Option<String>,
}

/// Token-usage block from the response.
///
/// All fields default to 0 because some OpenAI-compat servers omit
/// the block entirely when token counts aren't tracked.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChatUsage {
    /// Tokens in the prompt.
    #[serde(default)]
    pub prompt_tokens: u32,
    /// Tokens generated.
    #[serde(default)]
    pub completion_tokens: u32,
    /// Sum.
    #[serde(default)]
    pub total_tokens: u32,
    /// Per-prompt detail block; carries `cached_tokens` from
    /// llama-server's slot prefix cache. Server-optional, defaults
    /// to all-zeros when omitted.
    #[serde(default)]
    pub prompt_tokens_details: ChatUsagePromptDetails,
}

/// Optional `usage.prompt_tokens_details` block. Surfaces slot prefix
/// cache hit counts so callers can verify cache reuse.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChatUsagePromptDetails {
    /// Tokens served from the slot prefix cache.
    #[serde(default)]
    pub cached_tokens: u32,
}

/// llama-server's `timings` block — server-specific, carries token-rate
/// metrics. Absent on stricter OpenAI-compat backends.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChatTimings {
    /// Sustained generation rate during this call (tokens/sec).
    #[serde(default)]
    pub predicted_per_second: f32,
    /// Sustained prompt-processing rate (tokens/sec).
    #[serde(default)]
    pub prompt_per_second: f32,
}

/// Wire shape for `POST /v1/chat/completions` response body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatResponse {
    /// One or more completion choices. Practically always one for
    /// llama-server.
    pub choices: Vec<ChatChoice>,
    /// Token-usage block. May be absent on some servers.
    #[serde(default)]
    pub usage: ChatUsage,
    /// Echoed model name (best-effort; some servers omit).
    #[serde(default)]
    pub model: Option<String>,
    /// llama-server-specific timings block; server-optional.
    #[serde(default)]
    pub timings: Option<ChatTimings>,
}

/// Errors emitted by the LLM HTTP client.
#[derive(Debug, Error)]
pub enum LlmError {
    /// Underlying HTTP transport failure (DNS, connect, timeout, TLS).
    #[error("llm http transport: {0}")]
    Transport(String),
    /// Service returned 5xx (idempotent at low T — safe to retry once).
    #[error("llm service {status}: {body}")]
    Server {
        /// HTTP status code.
        status: u16,
        /// Response body (plain text from the server).
        body: String,
    },
    /// Service returned 4xx (malformed request — don't retry).
    #[error("llm client error {status}: {body}")]
    ClientError {
        /// HTTP status code.
        status: u16,
        /// Response body.
        body: String,
    },
    /// Service returned 503 `{"status":"loading model"}` — retry with
    /// backoff, mirrors whisper's contract.
    #[error("llm service loading")]
    Loading,
    /// Response body was not JSON in the expected shape.
    #[error("llm response malformed: {0}")]
    Malformed(String),
    /// Response carried an empty `choices` array — we have nothing to
    /// surface back to the caller.
    #[error("llm response had no choices")]
    NoChoices,
}

impl LlmError {
    /// Whether a caller should retry automatically.
    pub fn is_retriable(&self) -> bool {
        matches!(
            self,
            LlmError::Transport(_) | LlmError::Server { .. } | LlmError::Loading
        )
    }
}

/// A replacement transport for [`LlmClient`] (ADR-103 Phase 2 package F).
///
/// A client built with [`LlmClient::with_backend`] never touches HTTP and
/// never reads `config.api_key`: `complete*`, `list_models` and `health` all
/// go to the backend. The per-project kernel uses this to reach the model
/// through its parent daemon, so provider credentials stay in the user tier.
/// The in-flight permit and per-call defaults still apply.
#[async_trait::async_trait]
pub trait LlmBackend: Send + Sync + std::fmt::Debug {
    /// Execute one chat completion.
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, LlmError>;
    /// Model ids the backend offers.
    async fn list_models(&self) -> Result<Vec<String>, LlmError>;
    /// One readiness probe: `Ok(true)` ready, `Ok(false)` loading.
    async fn health(&self) -> Result<bool, LlmError>;
}

/// HTTP client for the LLM service.
#[derive(Debug, Clone)]
pub struct LlmClient {
    config: LlmConfig,
    backend: Option<Arc<dyn LlmBackend>>,
    http: reqwest::Client,
    /// Backpressure: permits=1 matches `llama-server`'s single-batch
    /// processing core. We enforce it client-side so we never pipeline
    /// a second request against the server's busy slot.
    in_flight: Arc<Semaphore>,
}

/// Process-wide handle that can be replaced when env rotates (WEFT-343).
///
/// The daemon stores one of these in a `OnceLock` so `control.set_enabled`
/// (and any other refresh path) can re-read env/config and swap the
/// inner [`LlmClient`] without tearing down call-sites that already hold
/// the outer `Arc`.
pub type SharedLlmClient = Arc<tokio::sync::RwLock<LlmClient>>;

/// Wrap a freshly-built [`LlmClient`] for shared, swappable use.
pub fn share_llm_client(client: LlmClient) -> SharedLlmClient {
    Arc::new(tokio::sync::RwLock::new(client))
}

impl LlmClient {
    /// Build a client with the supplied config.
    pub fn new(config: LlmConfig) -> Result<Self, LlmError> {
        let http = reqwest::Client::builder()
            .timeout(config.request_timeout)
            .build()
            .map_err(|e| LlmError::Transport(e.to_string()))?;
        Ok(Self {
            config,
            backend: None,
            http,
            in_flight: Arc::new(Semaphore::new(1)),
        })
    }

    /// Build a client whose transport is `backend` instead of HTTP.
    pub fn with_backend(
        config: LlmConfig,
        backend: Arc<dyn LlmBackend>,
    ) -> Result<Self, LlmError> {
        let mut client = Self::new(config)?;
        client.backend = Some(backend);
        Ok(client)
    }

    /// Take the single in-flight slot without waiting: `None` while another
    /// call holds it. A caller that must never queue behind (or in front of)
    /// the daemon's own turns holds this across [`Self::complete_unchecked`]
    /// instead of calling [`Self::complete_with_tools`].
    pub fn try_slot(&self) -> Option<tokio::sync::OwnedSemaphorePermit> {
        Arc::clone(&self.in_flight).try_acquire_owned().ok()
    }

    /// Read-only accessor; used by the daemon's wiring + structured logs.
    pub fn config(&self) -> &LlmConfig {
        &self.config
    }

    /// Hit `GET /health` exactly once.
    ///
    /// Returns `Ok(true)` on 200, `Ok(false)` on 503 (loading model),
    /// or `Err` on transport / other failure.
    pub async fn health(&self) -> Result<bool, LlmError> {
        if let Some(backend) = &self.backend {
            return backend.health().await;
        }
        let url = format!("{}/health", self.config.base_url);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| LlmError::Transport(e.to_string()))?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| LlmError::Transport(e.to_string()))?;
        if status.is_success() {
            Ok(true)
        } else if status.as_u16() == 503 {
            Ok(false)
        } else {
            Err(LlmError::Server {
                status: status.as_u16(),
                body,
            })
        }
    }

    /// Poll `/health` until it returns ready or the deadline elapses.
    ///
    /// Returns `Ok(true)` when ready, `Ok(false)` on timeout — **not**
    /// an error, so the daemon can degrade gracefully (RPC calls will
    /// then surface a clear "service not ready" error per request).
    pub async fn wait_for_healthy(&self) -> bool {
        let start = tokio::time::Instant::now();
        let mut backoff = Duration::from_millis(200);
        while start.elapsed() < self.config.health_deadline {
            match self.health().await {
                Ok(true) => return true,
                Ok(false) => debug!("llm: service still loading model"),
                Err(e) => debug!(error = %e, "llm: health check failed"),
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(2));
        }
        warn!(
            base_url = %self.config.base_url,
            deadline_ms = self.config.health_deadline.as_millis() as u64,
            "llm: health probe timeout — service will run in degraded mode"
        );
        false
    }

    /// POST a chat completion request and return the parsed response.
    ///
    /// Acquires the in-flight semaphore so concurrent callers serialize
    /// rather than racing the server's single-batch slot.
    ///
    /// The caller controls `temperature` and `max_tokens` per call;
    /// passing `None` uses the config defaults. No tools are sent —
    /// for tool-call-capable completions use
    /// [`Self::complete_with_tools`].
    pub async fn complete(
        &self,
        messages: Vec<ChatMessage>,
        temperature: Option<f32>,
        max_tokens: Option<u32>,
    ) -> Result<ChatResponse, LlmError> {
        self.complete_with_tools(messages, Vec::new(), None, temperature, max_tokens, None)
            .await
    }

    /// Tool-call-capable variant of [`Self::complete`]. Pass an empty
    /// `tools` vec to behave identically to `complete`.
    ///
    /// `tool_choice` controls whether the model is forced to call a
    /// tool ([`ToolChoice::Required`] / [`ToolChoice::Function`]),
    /// allowed to choose ([`ToolChoice::Auto`] — the server default,
    /// equivalent to passing `None`), or forbidden from calling
    /// ([`ToolChoice::None`]).
    ///
    /// `model` (WEFT-256): optional per-call model override. When
    /// `Some` and non-empty, it replaces the configured default in the
    /// request body so a panel chip-strip selection actually reaches
    /// the upstream. `None` keeps [`LlmConfig::model`].
    pub async fn complete_with_tools(
        &self,
        messages: Vec<ChatMessage>,
        tools: Vec<Tool>,
        tool_choice: Option<ToolChoice>,
        temperature: Option<f32>,
        max_tokens: Option<u32>,
        model: Option<&str>,
    ) -> Result<ChatResponse, LlmError> {
        let _permit = self
            .in_flight
            .acquire()
            .await
            .map_err(|_| LlmError::Transport("llm in-flight semaphore closed".into()))?;
        self.complete_unchecked(messages, tools, tool_choice, temperature, max_tokens, model)
            .await
    }

    /// Build the chat-completions URL from `base_url`.
    ///
    /// Tolerates either convention for `base_url`:
    /// - bare API root (`http://127.0.0.1:8111`, `https://openrouter.ai/api`)
    ///   — appends `/v1/chat/completions`.
    /// - v1 root (`https://openrouter.ai/api/v1`) — appends just
    ///   `/chat/completions` so callers who paste OpenRouter's
    ///   "OpenAI base URL" string still get the right endpoint.
    fn chat_completions_url(&self) -> String {
        let base = self.config.base_url.trim_end_matches('/');
        if let Some(stripped) = base.strip_suffix("/v1") {
            format!("{stripped}/v1/chat/completions")
        } else {
            format!("{base}/v1/chat/completions")
        }
    }

    /// Build the OpenAI-compat `/v1/models` URL (WEFT-256 enumeration).
    fn models_url(&self) -> String {
        let base = self.config.base_url.trim_end_matches('/');
        if let Some(stripped) = base.strip_suffix("/v1") {
            format!("{stripped}/v1/models")
        } else {
            format!("{base}/v1/models")
        }
    }

    /// List model ids from the upstream `/v1/models` endpoint (WEFT-256).
    ///
    /// Returns ids only. On non-2xx or transport failure returns
    /// [`LlmError`] so the daemon can fall back to the configured
    /// default without failing the RPC.
    pub async fn list_models(&self) -> Result<Vec<String>, LlmError> {
        if let Some(backend) = &self.backend {
            return backend.list_models().await;
        }
        let url = self.models_url();
        let mut req = self.http.get(&url);
        if let Some(key) = self.config.api_key.as_deref() {
            req = req.bearer_auth(key);
        }
        if let Some(referer) = self.config.referer.as_deref() {
            req = req.header("HTTP-Referer", referer);
        }
        if let Some(title) = self.config.app_title.as_deref() {
            req = req.header("X-Title", title);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| LlmError::Transport(e.to_string()))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| LlmError::Transport(e.to_string()))?;
        if !status.is_success() {
            return Err(LlmError::Server {
                status: status.as_u16(),
                body: text,
            });
        }
        let parsed: ModelsListResponse = serde_json::from_str(&text).map_err(|e| {
            LlmError::Transport(format!("llm /v1/models parse failed: {e}; body={text}"))
        })?;
        Ok(parsed
            .data
            .into_iter()
            .map(|m| m.id)
            .filter(|id| !id.is_empty())
            .collect())
    }

    /// Send the request without acquiring the permit. Public for test
    /// harnesses that exercise the wire format without serialization;
    /// production code paths MUST use [`Self::complete`] or
    /// [`Self::complete_with_tools`].
    ///
    /// `model` (WEFT-256): optional per-call override; see
    /// [`Self::complete_with_tools`].
    pub async fn complete_unchecked(
        &self,
        messages: Vec<ChatMessage>,
        tools: Vec<Tool>,
        tool_choice: Option<ToolChoice>,
        temperature: Option<f32>,
        max_tokens: Option<u32>,
        model: Option<&str>,
    ) -> Result<ChatResponse, LlmError> {
        let url = self.chat_completions_url();

        let model_name = model
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(self.config.model.as_str())
            .to_string();

        let body = ChatRequest {
            model: model_name,
            messages,
            temperature: Some(temperature.unwrap_or(self.config.default_temperature)),
            max_tokens: Some(max_tokens.unwrap_or(self.config.default_max_tokens)),
            stream: false,
            tools: if tools.is_empty() { None } else { Some(tools) },
            tool_choice,
        };

        if let Some(backend) = &self.backend {
            let parsed = backend.chat(body).await?;
            if parsed.choices.is_empty() {
                return Err(LlmError::NoChoices);
            }
            return Ok(parsed);
        }

        let mut req = self.http.post(&url).json(&body);
        if let Some(key) = self.config.api_key.as_deref() {
            req = req.bearer_auth(key);
        }
        if let Some(referer) = self.config.referer.as_deref() {
            req = req.header("HTTP-Referer", referer);
        }
        if let Some(title) = self.config.app_title.as_deref() {
            req = req.header("X-Title", title);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| LlmError::Transport(e.to_string()))?;

        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| LlmError::Transport(e.to_string()))?;

        if status.as_u16() == 503 {
            return Err(LlmError::Loading);
        }
        if !status.is_success() {
            let code = status.as_u16();
            if (400..500).contains(&code) {
                return Err(LlmError::ClientError {
                    status: code,
                    body: text,
                });
            }
            return Err(LlmError::Server {
                status: code,
                body: text,
            });
        }

        // OpenRouter's free-tier endpoints (Nemotron, llama, etc.)
        // pad the response with keep-alive whitespace lines while the
        // request is queued, then sometimes return an error envelope
        // (`{"error":{"message":...,"code":...}}`) at status 200 when
        // the upstream provider eventually fails. Direct parsing as
        // ChatResponse in that case yields a confusing
        // `missing field 'choices' at line 571 column 58` — no hint
        // that the upstream actually reported an error. Try the error
        // shape first and surface it as ClientError so the caller (and
        // the user) sees the real reason.
        if let Ok(env) = serde_json::from_str::<OpenRouterErrorEnvelope>(&text) {
            return Err(LlmError::ClientError {
                status: env.error.code.unwrap_or(status.as_u16()),
                body: env.error.message,
            });
        }
        let parsed: ChatResponse = serde_json::from_str(&text).map_err(|e| {
            // Trim the body in the error message so a 571-line
            // keep-alive-padded response doesn't blow out the log.
            let preview: String = text.chars().take(512).collect();
            LlmError::Malformed(format!("body was not ChatResponse JSON ({e}): {preview}"))
        })?;
        if parsed.choices.is_empty() {
            return Err(LlmError::NoChoices);
        }
        Ok(parsed)
    }
}

/// OpenRouter error-envelope wire shape. Used to disambiguate a 200
/// response that actually carries an upstream-provider failure.
#[derive(Debug, Deserialize)]
struct OpenRouterErrorEnvelope {
    error: OpenRouterError,
}

#[derive(Debug, Deserialize)]
struct OpenRouterError {
    message: String,
    #[serde(default)]
    code: Option<u16>,
}

#[cfg(test)]
mod tests {
    #[test]
    fn llm_config_debug_redacts_api_key() {
        let cfg = LlmConfig {
            api_key: Some("sk-dummy-not-a-real-key-12345".to_string()),
            ..LlmConfig::default()
        };
        let dbg = format!("{cfg:?}");
        assert!(!dbg.contains("sk-dummy"), "config Debug leaked key: {dbg}");
        assert!(dbg.contains("***"), "expected redaction marker: {dbg}");
        let client = LlmClient::new(cfg).expect("client builds");
        let dbg = format!("{client:?}");
        assert!(!dbg.contains("sk-dummy"), "client Debug leaked key: {dbg}");
        let none = format!("{:?}", LlmConfig::default());
        assert!(none.contains("api_key: None"), "{none}");
    }

    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_config(url: String) -> LlmConfig {
        LlmConfig {
            base_url: url,
            request_timeout: Duration::from_secs(5),
            health_deadline: Duration::from_secs(2),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn health_ok_when_service_ready() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/health"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"status":"ok"}"#))
            .mount(&server)
            .await;
        let client = LlmClient::new(test_config(server.uri())).unwrap();
        assert!(client.health().await.unwrap());
    }

    #[tokio::test]
    async fn health_false_when_loading() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/health"))
            .respond_with(
                ResponseTemplate::new(503).set_body_string(r#"{"status":"loading model"}"#),
            )
            .mount(&server)
            .await;
        let client = LlmClient::new(test_config(server.uri())).unwrap();
        assert!(!client.health().await.unwrap());
    }

    #[tokio::test]
    async fn health_errors_on_5xx_non_503() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/health"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        let client = LlmClient::new(test_config(server.uri())).unwrap();
        let err = client.health().await.unwrap_err();
        assert!(matches!(err, LlmError::Server { .. }));
    }

    #[tokio::test]
    async fn wait_for_healthy_returns_false_on_timeout() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/health"))
            .respond_with(
                ResponseTemplate::new(503).set_body_string(r#"{"status":"loading model"}"#),
            )
            .mount(&server)
            .await;
        let client = LlmClient::new(test_config(server.uri())).unwrap();
        let ok = client.wait_for_healthy().await;
        assert!(!ok);
    }

    #[tokio::test]
    async fn wait_for_healthy_returns_true_when_ready() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/health"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"status":"ok"}"#))
            .mount(&server)
            .await;
        let client = LlmClient::new(test_config(server.uri())).unwrap();
        assert!(client.wait_for_healthy().await);
    }

    #[tokio::test]
    async fn complete_happy_path_returns_assistant_text() {
        let server = MockServer::start().await;
        let body = r#"{
            "id": "chatcmpl-1",
            "object": "chat.completion",
            "created": 0,
            "model": "Qwen3.6-35B",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "hello back"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7}
        }"#;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;
        let client = LlmClient::new(test_config(server.uri())).unwrap();
        let r = client
            .complete(vec![ChatMessage::user("hi")], None, None)
            .await
            .unwrap();
        assert_eq!(r.choices.len(), 1);
        assert_eq!(r.choices[0].message.role, "assistant");
        assert_eq!(r.choices[0].message.content, "hello back");
        assert_eq!(r.usage.completion_tokens, 2);
    }

    #[tokio::test]
    async fn complete_handles_missing_usage_block() {
        // Some OpenAI-compat servers omit `usage`; we must default it.
        let server = MockServer::start().await;
        let body = r#"{
            "choices": [{
                "message": {"role": "assistant", "content": "ok"}
            }]
        }"#;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;
        let client = LlmClient::new(test_config(server.uri())).unwrap();
        let r = client
            .complete(vec![ChatMessage::user("hi")], None, None)
            .await
            .unwrap();
        assert_eq!(r.choices[0].message.content, "ok");
        assert_eq!(r.usage.total_tokens, 0);
    }

    #[tokio::test]
    async fn complete_empty_choices_is_explicit_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"choices": []}"#))
            .mount(&server)
            .await;
        let client = LlmClient::new(test_config(server.uri())).unwrap();
        let err = client
            .complete(vec![ChatMessage::user("hi")], None, None)
            .await
            .unwrap_err();
        assert!(matches!(err, LlmError::NoChoices));
    }

    #[tokio::test]
    async fn complete_bubbles_loading() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(503).set_body_string(r#"{"status":"loading model"}"#),
            )
            .mount(&server)
            .await;
        let client = LlmClient::new(test_config(server.uri())).unwrap();
        let err = client
            .complete(vec![ChatMessage::user("hi")], None, None)
            .await
            .unwrap_err();
        assert!(matches!(err, LlmError::Loading));
        assert!(err.is_retriable());
    }

    #[tokio::test]
    async fn complete_client_error_not_retriable() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(400).set_body_string(r#"{"error":"bad request"}"#))
            .mount(&server)
            .await;
        let client = LlmClient::new(test_config(server.uri())).unwrap();
        let err = client
            .complete(vec![ChatMessage::user("hi")], None, None)
            .await
            .unwrap_err();
        assert!(matches!(err, LlmError::ClientError { status: 400, .. }));
        assert!(!err.is_retriable());
    }

    #[tokio::test]
    async fn complete_server_error_retriable() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(502).set_body_string("bad gateway"))
            .mount(&server)
            .await;
        let client = LlmClient::new(test_config(server.uri())).unwrap();
        let err = client
            .complete(vec![ChatMessage::user("hi")], None, None)
            .await
            .unwrap_err();
        assert!(err.is_retriable());
    }

    #[tokio::test]
    async fn complete_malformed_json_fails() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;
        let client = LlmClient::new(test_config(server.uri())).unwrap();
        let err = client
            .complete(vec![ChatMessage::user("hi")], None, None)
            .await
            .unwrap_err();
        assert!(matches!(err, LlmError::Malformed(_)));
    }

    #[tokio::test]
    async fn in_flight_semaphore_is_one() {
        let client = LlmClient::new(test_config("http://unused".into())).unwrap();
        assert_eq!(client.in_flight.available_permits(), 1);
    }

    #[test]
    fn config_from_env_uses_env_var() {
        // SAFETY: set_var/remove_var are unsafe on Rust 2024 due to
        // races with reader threads; this test is single-threaded.
        unsafe { std::env::set_var(LLM_SERVICE_URL_ENV, "http://other.host:9000") };
        let c = LlmConfig::from_env();
        assert_eq!(c.base_url, "http://other.host:9000");
        unsafe { std::env::remove_var(LLM_SERVICE_URL_ENV) };
        let c2 = LlmConfig::from_env();
        assert_eq!(c2.base_url, DEFAULT_LLM_SERVICE_URL);
    }

    #[test]
    fn chat_message_role_constructors() {
        assert_eq!(ChatMessage::system("s").role, "system");
        assert_eq!(ChatMessage::user("u").role, "user");
        assert_eq!(ChatMessage::assistant("a").role, "assistant");
    }

    #[test]
    fn chat_message_tool_constructor() {
        let m = ChatMessage::tool("call_42", "result text");
        assert_eq!(m.role, "tool");
        assert_eq!(m.content, "result text");
        assert_eq!(m.tool_call_id.as_deref(), Some("call_42"));
        assert!(m.tool_calls.is_none());
    }

    #[test]
    fn chat_request_serializes_tools_when_present() {
        let req = ChatRequest {
            model: "qwen".into(),
            messages: vec![ChatMessage::user("hi")],
            temperature: None,
            max_tokens: None,
            stream: false,
            tools: Some(vec![Tool::function(
                "list_files",
                "List files in a directory",
                serde_json::json!({"type":"object","properties":{}}),
            )]),
            tool_choice: Some(ToolChoice::Auto),
        };
        let v: serde_json::Value = serde_json::to_value(&req).unwrap();
        assert!(v.get("tools").is_some(), "tools should serialize");
        assert_eq!(v["tools"][0]["type"], "function");
        assert_eq!(v["tools"][0]["function"]["name"], "list_files");
        assert_eq!(v["tool_choice"], "auto");
    }

    #[test]
    fn chat_request_omits_tools_when_none() {
        let req = ChatRequest {
            model: "qwen".into(),
            messages: vec![ChatMessage::user("hi")],
            temperature: None,
            max_tokens: None,
            stream: false,
            tools: None,
            tool_choice: None,
        };
        let v: serde_json::Value = serde_json::to_value(&req).unwrap();
        assert!(
            v.get("tools").is_none(),
            "wire shape must stay byte-compat with the no-tools case"
        );
        assert!(v.get("tool_choice").is_none());
    }

    #[test]
    fn tool_choice_function_serializes_as_object() {
        let v = serde_json::to_value(ToolChoice::Function("read_file".into())).unwrap();
        assert_eq!(v["type"], "function");
        assert_eq!(v["function"]["name"], "read_file");
    }

    #[test]
    fn tool_choice_string_variants_serialize() {
        assert_eq!(serde_json::to_value(ToolChoice::Auto).unwrap(), "auto");
        assert_eq!(serde_json::to_value(ToolChoice::None).unwrap(), "none");
        assert_eq!(
            serde_json::to_value(ToolChoice::Required).unwrap(),
            "required"
        );
    }

    #[tokio::test]
    async fn complete_with_tools_returns_tool_calls() {
        // Server returns a tool-call response: assistant message with
        // empty content and a `tool_calls` array, finish_reason
        // "tool_calls".
        let server = MockServer::start().await;
        let body = r#"{
            "id": "chatcmpl-tool-1",
            "model": "Qwen3.6-35B",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [{
                        "id": "call_abc",
                        "type": "function",
                        "function": {
                            "name": "list_files",
                            "arguments": "{\"path\":\"/tmp\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        }"#;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;
        let client = LlmClient::new(test_config(server.uri())).unwrap();
        let r = client
            .complete_with_tools(
                vec![ChatMessage::user("list /tmp")],
                vec![Tool::function(
                    "list_files",
                    "List files in a directory",
                    serde_json::json!({"type":"object"}),
                )],
                Some(ToolChoice::Auto),
                None,
                None,
                None,
            )
            .await
            .unwrap();
        let calls = r.choices[0]
            .message
            .tool_calls
            .as_ref()
            .expect("tool_calls present");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_abc");
        assert_eq!(calls[0].kind, "function");
        assert_eq!(calls[0].function.name, "list_files");
        assert!(calls[0].function.arguments.contains("/tmp"));
        assert_eq!(r.choices[0].finish_reason.as_deref(), Some("tool_calls"));
    }

    #[tokio::test]
    async fn complete_with_tools_accepts_null_content() {
        // OpenRouter via some upstreams (e.g. Nvidia Nemotron) returns
        // `"content": null` on tool-call turns instead of `"content": ""`.
        // The wire shape is OpenAI-compatible; the deserializer must
        // coerce null -> "" so the agent loop can keep going.
        let server = MockServer::start().await;
        let body = r#"{
            "id": "gen-1-null-content",
            "model": "nvidia/nemotron-3-super-120b-a12b-20230311:free",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "chatcmpl-tool-null-1",
                        "type": "function",
                        "function": {
                            "name": "memory_read",
                            "arguments": "{\"query\":\"\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        }"#;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;
        let client = LlmClient::new(test_config(server.uri())).unwrap();
        let r = client
            .complete_with_tools(
                vec![ChatMessage::user("how complete is the project?")],
                vec![Tool::function(
                    "memory_read",
                    "Read project memory",
                    serde_json::json!({"type":"object"}),
                )],
                Some(ToolChoice::Auto),
                None,
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(r.choices[0].message.content, "");
        let calls = r.choices[0]
            .message
            .tool_calls
            .as_ref()
            .expect("tool_calls present");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "memory_read");
        assert_eq!(r.choices[0].finish_reason.as_deref(), Some("tool_calls"));
    }

    #[tokio::test]
    async fn list_models_parses_openai_shape() {
        // WEFT-256: daemon enumeration probes /v1/models.
        let server = MockServer::start().await;
        let body = r#"{
            "object": "list",
            "data": [
                {"id": "hermes-4.3-36b", "object": "model"},
                {"id": "other-model", "object": "model"}
            ]
        }"#;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;
        let client = LlmClient::new(test_config(server.uri())).unwrap();
        let ids = client.list_models().await.unwrap();
        assert_eq!(ids, vec!["hermes-4.3-36b", "other-model"]);
    }

    #[tokio::test]
    async fn complete_with_tools_honors_model_override() {
        // WEFT-256: panel chip-strip selection must reach the wire body.
        use wiremock::matchers::{body_partial_json, method, path};
        let server = MockServer::start().await;
        let body = r#"{"choices":[{"message":{"role":"assistant","content":"ok"}}]}"#;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(body_partial_json(serde_json::json!({
                "model": "panel-selected-model"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;
        let client = LlmClient::new(test_config(server.uri())).unwrap();
        let r = client
            .complete_with_tools(
                vec![ChatMessage::user("hi")],
                Vec::new(),
                None,
                None,
                None,
                Some("panel-selected-model"),
            )
            .await
            .unwrap();
        assert_eq!(r.choices[0].message.content, "ok");
    }

    #[tokio::test]
    async fn complete_attaches_bearer_and_attribution_headers() {
        // OpenRouter path: when api_key/referer/app_title are set,
        // they ride on the request as `Authorization: Bearer …`,
        // `HTTP-Referer: …`, `X-Title: …`. Local-llama path
        // (everything `None`) sends none of these — covered by all
        // the existing tests that expect byte-compat wire shape.
        use wiremock::matchers::{header, method, path};
        let server = MockServer::start().await;
        let body = r#"{"choices":[{"message":{"role":"assistant","content":"ok"}}]}"#;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header("authorization", "Bearer sk-or-test"))
            .and(header("http-referer", "https://example.test/app"))
            .and(header("x-title", "WeftOS test"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;
        let cfg = LlmConfig {
            base_url: server.uri(),
            api_key: Some("sk-or-test".into()),
            referer: Some("https://example.test/app".into()),
            app_title: Some("WeftOS test".into()),
            request_timeout: Duration::from_secs(5),
            health_deadline: Duration::from_secs(2),
            ..LlmConfig::default()
        };
        let client = LlmClient::new(cfg).unwrap();
        let r = client
            .complete(vec![ChatMessage::user("hi")], None, None)
            .await
            .expect("matched mock means headers were present");
        assert_eq!(r.choices[0].message.content, "ok");
    }

    #[test]
    fn chat_completions_url_handles_both_base_url_conventions() {
        // Bare API root → appends /v1/chat/completions.
        let bare = LlmClient::new(LlmConfig {
            base_url: "http://127.0.0.1:8111".into(),
            ..test_config("unused".into())
        })
        .unwrap();
        assert_eq!(
            bare.chat_completions_url(),
            "http://127.0.0.1:8111/v1/chat/completions",
        );
        // Trailing slash → trimmed.
        let slash = LlmClient::new(LlmConfig {
            base_url: "http://127.0.0.1:8111/".into(),
            ..test_config("unused".into())
        })
        .unwrap();
        assert_eq!(
            slash.chat_completions_url(),
            "http://127.0.0.1:8111/v1/chat/completions",
        );
        // OpenRouter "OpenAI base URL" with /v1 already in it →
        // strip and re-append so we don't double up.
        let v1 = LlmClient::new(LlmConfig {
            base_url: "https://openrouter.ai/api/v1".into(),
            ..test_config("unused".into())
        })
        .unwrap();
        assert_eq!(
            v1.chat_completions_url(),
            "https://openrouter.ai/api/v1/chat/completions",
        );
    }

    #[tokio::test]
    async fn tool_role_message_round_trips_with_id() {
        let server = MockServer::start().await;
        let body = r#"{
            "choices": [{
                "message": {"role":"assistant","content":"done"},
                "finish_reason":"stop"
            }]
        }"#;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;
        let client = LlmClient::new(test_config(server.uri())).unwrap();
        // Round-trip a tool-result message through the request.
        let r = client
            .complete(
                vec![
                    ChatMessage::user("list /tmp"),
                    ChatMessage::tool("call_abc", "[\"a\",\"b\"]"),
                ],
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(r.choices[0].message.content, "done");
    }

    // ── MessageContent: string | blocks | null ─────────────────────────

    #[test]
    fn message_content_deserializes_plain_string() {
        let m: ChatMessage = serde_json::from_value(serde_json::json!({
            "role": "assistant",
            "content": "hello back"
        }))
        .unwrap();
        assert!(matches!(m.content, MessageContent::Text(_)));
        assert_eq!(m.content.as_text(), "hello back");
        assert_eq!(m.content, "hello back");
    }

    #[test]
    fn message_content_deserializes_null_as_empty_text() {
        // Nemotron / OpenRouter tool-call turns.
        let m: ChatMessage = serde_json::from_value(serde_json::json!({
            "role": "assistant",
            "content": null,
            "tool_calls": []
        }))
        .unwrap();
        assert_eq!(m.content, MessageContent::Text(String::new()));
        assert!(m.content.is_empty());
        assert_eq!(m.content.as_text(), "");
    }

    #[test]
    fn message_content_deserializes_missing_as_empty_text() {
        let m: ChatMessage = serde_json::from_value(serde_json::json!({
            "role": "assistant"
        }))
        .unwrap();
        assert!(m.content.is_empty());
    }

    #[test]
    fn message_content_deserializes_openai_vision_blocks() {
        // OpenAI vision API: array of text + image_url blocks.
        let m: ChatMessage = serde_json::from_value(serde_json::json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "What is in this image?"},
                {
                    "type": "image_url",
                    "image_url": {
                        "url": "https://example.com/cat.png",
                        "detail": "auto"
                    }
                }
            ]
        }))
        .unwrap();
        let blocks = m.content.blocks().expect("blocks present");
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].kind, "text");
        assert_eq!(blocks[0].text.as_deref(), Some("What is in this image?"));
        assert_eq!(blocks[1].kind, "image_url");
        assert_eq!(
            blocks[1].image_url.as_ref().map(|u| u.url.as_str()),
            Some("https://example.com/cat.png")
        );
        assert_eq!(
            blocks[1].image_url.as_ref().and_then(|u| u.detail.as_deref()),
            Some("auto")
        );
        // Flatten joins text parts only.
        assert_eq!(m.content.as_text(), "What is in this image?");
    }

    #[test]
    fn message_content_deserializes_anthropic_style_blocks() {
        // Anthropic-style: type "image" with a source object + text.
        let m: ChatMessage = serde_json::from_value(serde_json::json!({
            "role": "user",
            "content": [
                {
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": "image/jpeg",
                        "data": "/9j/4AAQSkZJRg=="
                    }
                },
                {"type": "text", "text": "Describe this photo."}
            ]
        }))
        .unwrap();
        let blocks = m.content.blocks().expect("blocks present");
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].kind, "image");
        assert!(blocks[0].source.is_some());
        assert_eq!(blocks[0].source.as_ref().unwrap()["type"], "base64");
        assert_eq!(blocks[1].kind, "text");
        assert_eq!(m.content.as_text(), "Describe this photo.");
    }

    #[test]
    fn message_content_flattens_multiple_text_blocks() {
        let m: ChatMessage = serde_json::from_value(serde_json::json!({
            "role": "assistant",
            "content": [
                {"type": "text", "text": "line one"},
                {"type": "text", "text": "line two"}
            ]
        }))
        .unwrap();
        assert_eq!(m.content.as_text(), "line one\nline two");
        assert_eq!(m.content.into_text(), "line one\nline two");
    }

    #[test]
    fn message_content_image_only_blocks_flatten_to_empty() {
        let m: ChatMessage = serde_json::from_value(serde_json::json!({
            "role": "user",
            "content": [
                {
                    "type": "image_url",
                    "image_url": {"url": "data:image/png;base64,abc"}
                }
            ]
        }))
        .unwrap();
        assert!(m.content.is_empty());
        assert_eq!(m.content.as_text(), "");
    }

    #[test]
    fn message_content_serializes_text_as_string() {
        let m = ChatMessage::user("hi");
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["content"], "hi");
        assert!(v["content"].is_string());
    }

    #[test]
    fn message_content_serializes_blocks_as_array() {
        let m = ChatMessage::with_blocks(
            "user",
            vec![
                ContentBlock {
                    kind: "text".into(),
                    text: Some("see this".into()),
                    image_url: None,
                    source: None,
                },
                ContentBlock {
                    kind: "image_url".into(),
                    text: None,
                    image_url: Some(ImageUrl {
                        url: "https://example.com/x.png".into(),
                        detail: None,
                    }),
                    source: None,
                },
            ],
        );
        let v = serde_json::to_value(&m).unwrap();
        assert!(v["content"].is_array());
        assert_eq!(v["content"][0]["type"], "text");
        assert_eq!(v["content"][0]["text"], "see this");
        assert_eq!(v["content"][1]["type"], "image_url");
        assert_eq!(v["content"][1]["image_url"]["url"], "https://example.com/x.png");
    }

    #[tokio::test]
    async fn complete_accepts_openai_vision_response_content_blocks() {
        // Some OpenAI-compat vision endpoints return assistant content
        // as a single text block array rather than a bare string.
        let server = MockServer::start().await;
        let body = r#"{
            "id": "chatcmpl-vision-1",
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": [
                        {"type": "text", "text": "A calico cat on a windowsill."}
                    ]
                },
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 100, "completion_tokens": 12, "total_tokens": 112}
        }"#;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;
        let client = LlmClient::new(test_config(server.uri())).unwrap();
        let r = client
            .complete(vec![ChatMessage::user("describe")], None, None)
            .await
            .unwrap();
        assert!(matches!(
            r.choices[0].message.content,
            MessageContent::Blocks(_)
        ));
        assert_eq!(
            r.choices[0].message.content.as_text(),
            "A calico cat on a windowsill."
        );
        // PartialEq<&str> flattens for consumers that compare text.
        assert_eq!(
            r.choices[0].message.content,
            "A calico cat on a windowsill."
        );
    }

    #[tokio::test]
    async fn complete_accepts_anthropic_style_content_blocks_in_response() {
        let server = MockServer::start().await;
        let body = r#"{
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": [
                        {"type": "text", "text": "I see a red bicycle."}
                    ]
                },
                "finish_reason": "stop"
            }]
        }"#;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;
        let client = LlmClient::new(test_config(server.uri())).unwrap();
        let r = client
            .complete(vec![ChatMessage::user("what do you see?")], None, None)
            .await
            .unwrap();
        assert_eq!(r.choices[0].message.content.as_text(), "I see a red bicycle.");
    }

    /// WEFT-343: a `SharedLlmClient` swap must replace the inner client
    /// so subsequent readers observe the new config (e.g. rotated API
    /// key / URL after `control.set_enabled` refresh).
    #[tokio::test]
    async fn shared_client_swap_replaces_inner() {
        let shared = share_llm_client(LlmClient::new(test_config("http://old.example".into())).unwrap());
        assert_eq!(
            shared.read().await.config().base_url,
            "http://old.example"
        );

        let replacement =
            LlmClient::new(test_config("http://new.example".into())).unwrap();
        *shared.write().await = replacement;

        assert_eq!(
            shared.read().await.config().base_url,
            "http://new.example"
        );
        // Outer Arc identity is stable — call-sites holding clones still
        // see the swap.
        let clone = Arc::clone(&shared);
        assert_eq!(clone.read().await.config().base_url, "http://new.example");
    }
}
