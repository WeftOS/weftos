//! Browser WASM transport for LLM API calls.
//!
//! This module provides a browser-compatible HTTP client for calling LLM APIs
//! directly from WebAssembly. It handles:
//!
//! - **CORS proxy routing**: When a `cors_proxy` URL is configured on a
//!   [`ProviderConfig`](clawft_types::config::ProviderConfig), all requests
//!   are routed through the proxy to avoid browser CORS restrictions.
//!
//! - **Browser-direct headers**: When `browser_direct` is enabled (e.g. for
//!   Anthropic's direct browser access), the required
//!   `anthropic-dangerous-direct-browser-access: true` header is added.
//!
//! - **Streaming**: Uses `reqwest`'s `bytes_stream()` (compiled with the
//!   `wasm` feature) to consume SSE streams in the browser.
//!
//! # Architecture
//!
//! The [`BrowserLlmClient`] wraps a `reqwest::Client` and a
//! [`LlmProviderConfig`] from `clawft-llm`. It delegates to the platform
//! [`ProviderConfig`](clawft_types::config::ProviderConfig) for browser-specific
//! fields (`browser_direct`, `cors_proxy`).
//!
//! ```text
//!   ┌──────────────┐
//!   │ BrowserLlm   │ ── complete() ──▶ POST /chat/completions
//!   │ Client       │ ── complete_stream() ──▶ SSE stream
//!   └──────┬───────┘
//!          │ resolve_url()
//!          ▼
//!    CORS proxy OR direct URL
//! ```

use futures_util::StreamExt;
use reqwest::header::HeaderMap;
use tracing::{debug, trace, warn};

use crate::config::LlmProviderConfig;
use crate::error::{ProviderError, Result};
use crate::sse::parse_sse_line;
use crate::types::{ChatRequest, ChatResponse, StreamChunk};

/// Resolve the final URL for a browser request.
///
/// If `cors_proxy` is set on the provider config, the request is routed
/// through the proxy by prepending the proxy URL. Otherwise, the original
/// URL is returned unchanged.
///
/// # Arguments
///
/// * `base_url` - The provider's base URL (e.g. `https://api.openai.com/v1`).
/// * `path` - The API path to append (e.g. `/chat/completions`).
/// * `cors_proxy` - Optional CORS proxy URL.
///
/// # Examples
///
/// ```rust,ignore
/// // Without proxy
/// let url = resolve_url("https://api.openai.com/v1", "/chat/completions", None);
/// assert_eq!(url, "https://api.openai.com/v1/chat/completions");
///
/// // With proxy
/// let url = resolve_url(
///     "https://api.openai.com/v1",
///     "/chat/completions",
///     Some("https://proxy.example.com"),
/// );
/// assert_eq!(url, "https://proxy.example.com/https://api.openai.com/v1/chat/completions");
/// ```
pub fn resolve_url(base_url: &str, path: &str, cors_proxy: Option<&str>) -> String {
    let base = base_url.trim_end_matches('/');
    let path = path.trim_start_matches('/');
    let direct_url = format!("{base}/{path}");

    match cors_proxy {
        Some(proxy) => {
            let proxy = proxy.trim_end_matches('/');
            format!("{proxy}/{direct_url}")
        }
        None => direct_url,
    }
}

/// Add browser-specific headers to the request.
///
/// When `browser_direct` is true (indicating the provider supports direct
/// browser access without a CORS proxy), this adds the required opt-in
/// header. Currently this is specific to Anthropic's API, which requires
/// the `anthropic-dangerous-direct-browser-access: true` header.
///
/// # Arguments
///
/// * `headers` - Mutable reference to the header map to modify.
/// * `browser_direct` - Whether the provider supports direct browser access.
/// * `provider_name` - The provider name (used to determine which headers to add).
pub fn add_browser_headers(headers: &mut HeaderMap, browser_direct: bool, provider_name: &str) {
    if browser_direct {
        // Anthropic requires this header for direct browser access.
        // Other providers may need different headers in the future.
        if provider_name == "anthropic"
            || provider_name.contains("anthropic")
            || provider_name.contains("claude")
        {
            if let Ok(val) = "true".parse() {
                headers.insert("anthropic-dangerous-direct-browser-access", val);
            }
        }
    }
}

/// Browser-compatible LLM client for WASM targets.
///
/// Wraps `reqwest::Client` (compiled with the `wasm` feature) and adds
/// browser-specific behavior: CORS proxy routing, direct-browser-access
/// headers, and SSE streaming via the Fetch API.
///
/// # Construction
///
/// ```rust,ignore
/// use clawft_llm::browser_transport::BrowserLlmClient;
/// use clawft_llm::config::LlmProviderConfig;
///
/// let config = LlmProviderConfig { /* ... */ };
/// let client = BrowserLlmClient::new(config, false, None);
/// ```
pub struct BrowserLlmClient {
    config: LlmProviderConfig,
    http: reqwest::Client,
    api_key: Option<String>,
    /// Whether the provider supports direct browser access.
    browser_direct: bool,
    /// Optional CORS proxy URL.
    cors_proxy: Option<String>,
}

impl BrowserLlmClient {
    /// Create a new browser LLM client.
    ///
    /// # Arguments
    ///
    /// * `config` - The LLM provider configuration (base URL, headers, etc.).
    /// * `browser_direct` - Whether the provider supports direct browser access.
    /// * `cors_proxy` - Optional CORS proxy URL for routing requests.
    pub fn new(
        config: LlmProviderConfig,
        browser_direct: bool,
        cors_proxy: Option<String>,
    ) -> Self {
        Self {
            http: reqwest::Client::new(),
            config,
            api_key: None,
            browser_direct,
            cors_proxy,
        }
    }

    /// Create a new browser LLM client with an explicit API key.
    pub fn with_api_key(
        config: LlmProviderConfig,
        api_key: String,
        browser_direct: bool,
        cors_proxy: Option<String>,
    ) -> Self {
        Self {
            http: reqwest::Client::new(),
            config,
            api_key: Some(api_key),
            browser_direct,
            cors_proxy,
        }
    }

    /// Returns the provider configuration.
    pub fn config(&self) -> &LlmProviderConfig {
        &self.config
    }

    /// Returns the provider name.
    pub fn name(&self) -> &str {
        &self.config.name
    }

    /// Returns the chat completions endpoint URL (with CORS proxy applied if configured).
    fn completions_url(&self) -> String {
        resolve_url(
            &self.config.base_url,
            "chat/completions",
            self.cors_proxy.as_deref(),
        )
    }

    /// Resolve the API key: explicit key > environment variable.
    ///
    /// Note: In browser environments, `std::env::var` will not work. The
    /// API key must be provided explicitly via [`with_api_key`](Self::with_api_key).
    fn resolve_api_key(&self) -> Result<String> {
        if let Some(ref key) = self.api_key {
            return Ok(key.clone());
        }
        // In browser, env vars are not available; require explicit key.
        Err(ProviderError::NotConfigured(format!(
            "browser mode requires an explicit API key (env var {} not available in WASM)",
            self.config.api_key_env
        )))
    }

    /// Execute a non-streaming chat completion request.
    pub async fn complete(&self, request: &ChatRequest) -> Result<ChatResponse> {
        let api_key = self.resolve_api_key()?;
        let url = self.completions_url();

        debug!(
            provider = %self.config.name,
            model = %request.model,
            messages = request.messages.len(),
            browser = true,
            "sending browser chat completion request"
        );

        let mut headers = HeaderMap::new();
        add_browser_headers(&mut headers, self.browser_direct, &self.config.name);

        let mut req = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {api_key}"))
            .header("Content-Type", "application/json")
            .headers(headers);

        for (k, v) in &self.config.headers {
            req = req.header(k.as_str(), v.as_str());
        }

        let response = req.json(request).send().await?;
        let status = response.status();

        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(classify_error(status.as_u16(), &body, &request.model));
        }

        let chat_response: ChatResponse = response.json().await.map_err(|e| {
            ProviderError::InvalidResponse(format!("failed to parse response: {e}"))
        })?;

        debug!(
            provider = %self.config.name,
            model = %chat_response.model,
            choices = chat_response.choices.len(),
            browser = true,
            "browser chat completion response received"
        );

        Ok(chat_response)
    }

    /// Execute a streaming chat completion request with a callback.
    ///
    /// Unlike the native `Provider::complete_stream` which uses
    /// `tokio::sync::mpsc`, this method uses a callback since `tokio` is
    /// not available in browser WASM. The callback receives each
    /// [`StreamChunk`] as it arrives. Return `true` to continue receiving
    /// chunks, or `false` to stop the stream early.
    pub async fn complete_stream_callback<F>(
        &self,
        request: &ChatRequest,
        mut on_chunk: F,
    ) -> Result<()>
    where
        F: FnMut(StreamChunk) -> bool,
    {
        let api_key = self.resolve_api_key()?;
        let url = self.completions_url();

        debug!(
            provider = %self.config.name,
            model = %request.model,
            messages = request.messages.len(),
            browser = true,
            "sending browser streaming chat completion request"
        );

        let mut stream_request = request.clone();
        stream_request.stream = Some(true);

        let mut headers = HeaderMap::new();
        add_browser_headers(&mut headers, self.browser_direct, &self.config.name);

        let mut req = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {api_key}"))
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream")
            .headers(headers);

        for (k, v) in &self.config.headers {
            req = req.header(k.as_str(), v.as_str());
        }

        let response = req.json(&stream_request).send().await?;
        let status = response.status();

        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(classify_error(status.as_u16(), &body, &request.model));
        }

        // Read the SSE stream via bytes_stream (works on both native and WASM).
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
                    Err(e) => {
                        warn!(
                            provider = %self.config.name,
                            error = %e,
                            "SSE parse error, skipping line"
                        );
                        continue;
                    }
                };

                for chunk in chunks {
                    trace!(
                        provider = %self.config.name,
                        chunk = ?chunk,
                        "browser streaming chunk"
                    );
                    if !on_chunk(chunk) {
                        debug!(
                            provider = %self.config.name,
                            "browser stream consumer signalled stop"
                        );
                        return Ok(());
                    }
                }
            }
        }

        // Process any remaining data in the buffer.
        if !buffer.trim().is_empty() {
            if let Ok(chunks) = parse_sse_line(&buffer) {
                for chunk in chunks {
                    let _ = on_chunk(chunk);
                }
            }
        }

        debug!(
            provider = %self.config.name,
            browser = true,
            "browser streaming complete"
        );

        Ok(())
    }
}

/// Classify an HTTP error status code into a [`ProviderError`].
fn classify_error(status: u16, body: &str, model: &str) -> ProviderError {
    match status {
        429 => ProviderError::RateLimited {
            retry_after_ms: 1000,
        },
        401 | 403 => ProviderError::AuthFailed(body.to_string()),
        404 => ProviderError::ModelNotFound(format!("model '{model}': {body}")),
        500..=599 => ProviderError::ServerError {
            status,
            body: body.to_string(),
        },
        _ => ProviderError::RequestFailed(format!("HTTP {status}: {body}")),
    }
}

impl std::fmt::Debug for BrowserLlmClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrowserLlmClient")
            .field("name", &self.config.name)
            .field("base_url", &self.config.base_url)
            .field("browser_direct", &self.browser_direct)
            .field("cors_proxy", &self.cors_proxy)
            .field("api_key", &self.api_key.as_ref().map(|_| "***"))
            .finish()
    }
}

// ── Browser-compatible sleep utility ────────────────────────────────────

/// Asynchronous delay that works in both native and browser environments.
///
/// * **wasm32 + browser** (WEFT-390): real wall-clock wait via
///   [`gloo_timers::future::TimeoutFuture`] so retry/backoff paths
///   actually pause the event loop instead of spinning.
/// * **Native** (when the `browser` feature is exercised under host
///   tests alongside `native`): `tokio::time::sleep`.
/// * **Other**: single yield to the executor (no busy-wait).
pub async fn browser_delay(duration: std::time::Duration) {
    #[cfg(all(target_arch = "wasm32", feature = "browser"))]
    {
        // Cap at u32::MAX ms (~49 days) — fine for backoff budgets.
        let ms = duration.as_millis().min(u128::from(u32::MAX)) as u32;
        gloo_timers::future::TimeoutFuture::new(ms).await;
    }
    #[cfg(all(not(target_arch = "wasm32"), feature = "native"))]
    {
        tokio::time::sleep(duration).await;
    }
    #[cfg(all(not(target_arch = "wasm32"), not(feature = "native")))]
    {
        let _ = duration;
        futures_util::future::ready(()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn test_config() -> LlmProviderConfig {
        LlmProviderConfig {
            name: "test-provider".into(),
            base_url: "https://api.example.com/v1".into(),
            api_key_env: "TEST_API_KEY".into(),
            model_prefix: Some("test/".into()),
            default_model: Some("test-model".into()),
            headers: HashMap::new(),
            timeout_secs: None,
        }
    }

    // ── resolve_url tests ───────────────────────────────────────────────

    #[test]
    fn resolve_url_without_proxy() {
        let url = resolve_url("https://api.openai.com/v1", "chat/completions", None);
        assert_eq!(url, "https://api.openai.com/v1/chat/completions");
    }

    #[test]
    fn resolve_url_with_proxy() {
        let url = resolve_url(
            "https://api.openai.com/v1",
            "chat/completions",
            Some("https://proxy.example.com"),
        );
        assert_eq!(
            url,
            "https://proxy.example.com/https://api.openai.com/v1/chat/completions"
        );
    }

    #[test]
    fn resolve_url_strips_trailing_slash_from_base() {
        let url = resolve_url("https://api.example.com/v1/", "chat/completions", None);
        assert_eq!(url, "https://api.example.com/v1/chat/completions");
    }

    #[test]
    fn resolve_url_strips_leading_slash_from_path() {
        let url = resolve_url("https://api.example.com/v1", "/chat/completions", None);
        assert_eq!(url, "https://api.example.com/v1/chat/completions");
    }

    #[test]
    fn resolve_url_strips_trailing_slash_from_proxy() {
        let url = resolve_url(
            "https://api.example.com/v1",
            "chat/completions",
            Some("https://proxy.example.com/"),
        );
        assert_eq!(
            url,
            "https://proxy.example.com/https://api.example.com/v1/chat/completions"
        );
    }

    // ── add_browser_headers tests ───────────────────────────────────────

    #[test]
    fn add_browser_headers_anthropic_direct() {
        let mut headers = HeaderMap::new();
        add_browser_headers(&mut headers, true, "anthropic");
        assert_eq!(
            headers
                .get("anthropic-dangerous-direct-browser-access")
                .unwrap(),
            "true"
        );
    }

    #[test]
    fn add_browser_headers_anthropic_name_contains() {
        let mut headers = HeaderMap::new();
        add_browser_headers(&mut headers, true, "my-anthropic-proxy");
        assert!(headers.contains_key("anthropic-dangerous-direct-browser-access"));
    }

    #[test]
    fn add_browser_headers_claude_name() {
        let mut headers = HeaderMap::new();
        add_browser_headers(&mut headers, true, "claude-provider");
        assert!(headers.contains_key("anthropic-dangerous-direct-browser-access"));
    }

    #[test]
    fn add_browser_headers_non_anthropic() {
        let mut headers = HeaderMap::new();
        add_browser_headers(&mut headers, true, "openai");
        assert!(!headers.contains_key("anthropic-dangerous-direct-browser-access"));
    }

    #[test]
    fn add_browser_headers_not_direct() {
        let mut headers = HeaderMap::new();
        add_browser_headers(&mut headers, false, "anthropic");
        assert!(!headers.contains_key("anthropic-dangerous-direct-browser-access"));
    }

    // ── BrowserLlmClient construction tests ─────────────────────────────

    #[test]
    fn client_new() {
        let client = BrowserLlmClient::new(test_config(), false, None);
        assert_eq!(client.name(), "test-provider");
        assert!(client.api_key.is_none());
        assert!(!client.browser_direct);
        assert!(client.cors_proxy.is_none());
    }

    #[test]
    fn client_with_api_key() {
        let client = BrowserLlmClient::with_api_key(
            test_config(),
            "sk-browser-test".into(),
            true,
            Some("https://proxy.example.com".into()),
        );
        assert_eq!(client.api_key.as_deref(), Some("sk-browser-test"));
        assert!(client.browser_direct);
        assert_eq!(
            client.cors_proxy.as_deref(),
            Some("https://proxy.example.com")
        );
    }

    #[test]
    fn client_completions_url_no_proxy() {
        let client = BrowserLlmClient::new(test_config(), false, None);
        assert_eq!(
            client.completions_url(),
            "https://api.example.com/v1/chat/completions"
        );
    }

    #[test]
    fn client_completions_url_with_proxy() {
        let client = BrowserLlmClient::new(
            test_config(),
            false,
            Some("https://cors.example.com".into()),
        );
        assert_eq!(
            client.completions_url(),
            "https://cors.example.com/https://api.example.com/v1/chat/completions"
        );
    }

    #[test]
    fn client_resolve_api_key_explicit() {
        let client =
            BrowserLlmClient::with_api_key(test_config(), "sk-explicit".into(), false, None);
        let key = client.resolve_api_key().unwrap();
        assert_eq!(key, "sk-explicit");
    }

    #[test]
    fn client_resolve_api_key_missing_returns_not_configured() {
        let client = BrowserLlmClient::new(test_config(), false, None);
        let err = client.resolve_api_key().unwrap_err();
        assert!(matches!(err, ProviderError::NotConfigured(_)));
        assert!(err.to_string().contains("browser mode"));
    }

    #[test]
    fn debug_hides_api_key() {
        let client = BrowserLlmClient::with_api_key(
            test_config(),
            "sk-secret-browser-key".into(),
            false,
            None,
        );
        let debug_str = format!("{:?}", client);
        assert!(!debug_str.contains("sk-secret-browser-key"));
        assert!(debug_str.contains("***"));
    }

    // ── classify_error tests ────────────────────────────────────────────

    #[test]
    fn classify_error_429() {
        let err = classify_error(429, "rate limited", "gpt-4");
        assert!(matches!(err, ProviderError::RateLimited { .. }));
    }

    #[test]
    fn classify_error_401() {
        let err = classify_error(401, "unauthorized", "gpt-4");
        assert!(matches!(err, ProviderError::AuthFailed(_)));
    }

    #[test]
    fn classify_error_403() {
        let err = classify_error(403, "forbidden", "gpt-4");
        assert!(matches!(err, ProviderError::AuthFailed(_)));
    }

    #[test]
    fn classify_error_404() {
        let err = classify_error(404, "not found", "gpt-5-turbo");
        assert!(matches!(err, ProviderError::ModelNotFound(_)));
        assert!(err.to_string().contains("gpt-5-turbo"));
    }

    #[test]
    fn classify_error_500() {
        let err = classify_error(500, "internal error", "gpt-4");
        assert!(matches!(
            err,
            ProviderError::ServerError { status: 500, .. }
        ));
    }

    #[test]
    fn classify_error_503() {
        let err = classify_error(503, "service unavailable", "gpt-4");
        assert!(matches!(
            err,
            ProviderError::ServerError { status: 503, .. }
        ));
    }

    #[test]
    fn classify_error_other() {
        let err = classify_error(418, "I'm a teapot", "gpt-4");
        assert!(matches!(err, ProviderError::RequestFailed(_)));
        assert!(err.to_string().contains("418"));
    }

    // ── Backward compatibility tests ────────────────────────────────────

    #[test]
    fn llm_provider_config_parses_with_defaults() {
        let json = r#"{
            "name": "test",
            "base_url": "https://api.example.com",
            "api_key_env": "TEST_KEY"
        }"#;
        let config: LlmProviderConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.name, "test");
        assert!(config.model_prefix.is_none());
        assert!(config.timeout_secs.is_none());
    }

    #[test]
    fn provider_config_browser_fields_default() {
        // Verify the types-level ProviderConfig still has browser fields with defaults
        let json = r#"{}"#;
        let cfg: clawft_types::config::ProviderConfig = serde_json::from_str(json).unwrap();
        assert!(!cfg.browser_direct);
        assert!(cfg.cors_proxy.is_none());
    }

    #[test]
    fn provider_config_browser_fields_roundtrip() {
        let json = r#"{"browserDirect": true, "corsProxy": "https://proxy.dev"}"#;
        let cfg: clawft_types::config::ProviderConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.browser_direct);
        assert_eq!(cfg.cors_proxy.as_deref(), Some("https://proxy.dev"));
    }

    // ── browser_delay test ──────────────────────────────────────────────

    #[tokio::test]
    async fn browser_delay_completes() {
        // Should complete immediately (no actual delay in non-WASM builds).
        browser_delay(std::time::Duration::from_millis(100)).await;
    }
}
