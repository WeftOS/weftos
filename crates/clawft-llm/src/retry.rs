//! Exponential backoff retry logic for LLM provider calls.
//!
//! [`RetryPolicy`] wraps any [`Provider`] and automatically retries failed
//! requests with configurable exponential backoff. Retries are applied to
//! transient errors (HTTP 429, 500, 502, 503, 504) and network failures.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tracing::{debug, warn};

use tokio::sync::mpsc;

use crate::eml_retry::{RetryModel, error_ordinal};
use crate::error::{ProviderError, Result};
use crate::provider::Provider;
use crate::types::{ChatRequest, ChatResponse, StreamChunk};

/// Configuration for retry behavior.
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// Maximum number of retry attempts (default: 3).
    pub max_retries: u32,
    /// Base delay between retries (default: 1 second).
    pub base_delay: Duration,
    /// Maximum delay between retries (default: 30 seconds).
    pub max_delay: Duration,
    /// Jitter factor: random 0..jitter_fraction of the delay is added (default: 0.25).
    pub jitter_fraction: f64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(30),
            jitter_fraction: 0.25,
        }
    }
}

/// Determines whether a [`ProviderError`] should be retried.
pub fn is_retryable(err: &ProviderError) -> bool {
    match err {
        ProviderError::RateLimited { .. } => true,
        ProviderError::Timeout => true,
        ProviderError::Http(_) => true,
        ProviderError::ServerError { status, .. } => (500..=599).contains(status),
        ProviderError::RequestFailed(_) => false,
        // Preserves the old behaviour for a down local server: fail fast.
        ProviderError::Connect(_) => false,
        ProviderError::AuthFailed(_)
        | ProviderError::ModelNotFound(_)
        | ProviderError::NotConfigured(_)
        | ProviderError::InvalidResponse(_)
        | ProviderError::Json(_)
        | ProviderError::AllProvidersExhausted { .. } => false,
    }
}

/// Calculate delay for attempt `n` (0-indexed) with exponential backoff + jitter.
///
/// The delay is `min(base_delay * 2^n, max_delay)` plus a random jitter of
/// `0..jitter_fraction * delay`.
pub fn compute_delay(config: &RetryConfig, attempt: u32) -> Duration {
    let exp = 2u64.saturating_pow(attempt);
    let base_ms = config.base_delay.as_millis() as u64;
    let raw_ms = base_ms.saturating_mul(exp);
    let capped_ms = raw_ms.min(config.max_delay.as_millis() as u64);

    // Add deterministic-ish jitter: use attempt number to vary
    // For real randomness we use a simple LCG seeded from attempt + current time nanos.
    let jitter_max_ms = (capped_ms as f64 * config.jitter_fraction) as u64;
    let jitter_ms = if jitter_max_ms > 0 {
        // Simple pseudo-random using system time nanoseconds
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos() as u64;
        seed % (jitter_max_ms + 1)
    } else {
        0
    };

    Duration::from_millis(capped_ms + jitter_ms)
}

/// A provider wrapper that retries transient failures with exponential backoff.
///
/// # Example
///
/// ```rust,ignore
/// use clawft_llm::{OpenAiCompatProvider, LlmProviderConfig};
/// use clawft_llm::retry::{RetryPolicy, RetryConfig};
///
/// let inner = OpenAiCompatProvider::new(config);
/// let provider = RetryPolicy::new(inner, RetryConfig::default());
/// // Calls to provider.complete() will now automatically retry on transient errors.
/// ```
pub struct RetryPolicy<P> {
    inner: P,
    config: RetryConfig,
    /// Optional EML retry model (Finding #6). When `Some`, every retry
    /// delay is predicted by the model and every outcome is recorded
    /// for online training. Shared via `Arc<Mutex<…>>` so the same
    /// model can learn from every provider wrapped by `RetryPolicy`.
    retry_model: Option<Arc<Mutex<RetryModel>>>,
}

impl<P: Provider> RetryPolicy<P> {
    /// Wrap a provider with retry logic.
    pub fn new(inner: P, config: RetryConfig) -> Self {
        Self {
            inner,
            config,
            retry_model: None,
        }
    }

    /// Wrap a provider with retry logic and enable the EML
    /// [`RetryModel`]. Per-attempt delay comes from
    /// [`RetryModel::delay_ms`], which falls back to
    /// [`compute_delay`] until the model is trained. Retry outcomes
    /// (success, exhaustion) are recorded back into the model so it
    /// can learn the optimal backoff per error-type × attempt × hour.
    pub fn with_model(inner: P, config: RetryConfig, model: Arc<Mutex<RetryModel>>) -> Self {
        Self {
            inner,
            config,
            retry_model: Some(model),
        }
    }

    /// Returns a reference to the retry configuration.
    pub fn retry_config(&self) -> &RetryConfig {
        &self.config
    }

    /// Returns a reference to the inner provider.
    pub fn inner(&self) -> &P {
        &self.inner
    }

    /// Returns the optional EML retry model handle, if wired.
    pub fn retry_model(&self) -> Option<&Arc<Mutex<RetryModel>>> {
        self.retry_model.as_ref()
    }

    /// Compute the delay for this attempt. Prefers the EML model if
    /// one is wired; otherwise falls back to the hardcoded
    /// exponential backoff in [`compute_delay`].
    ///
    /// Uses the `RateLimited` suggested delay as a floor whenever
    /// the server explicitly asked us to wait longer.
    fn effective_delay(&self, err: &ProviderError, attempt: u32) -> Duration {
        let base = match self.retry_model.as_ref() {
            Some(model) => {
                let ms = model
                    .lock()
                    .map(|m| m.delay_ms(err, attempt))
                    .unwrap_or_else(|_| {
                        // Poisoned lock — recover gracefully and fall
                        // back to the hardcoded config.
                        compute_delay(&self.config, attempt).as_millis() as u64
                    });
                Duration::from_millis(ms)
            }
            None => compute_delay(&self.config, attempt),
        };
        match err {
            ProviderError::RateLimited { retry_after_ms } => {
                base.max(Duration::from_millis(*retry_after_ms))
            }
            _ => base,
        }
    }

    /// Record the outcome of a retry attempt into the EML model, if
    /// one is wired. Takes the pre-computed error ordinal instead of
    /// a `&ProviderError` so the retry loop can stash it before the
    /// error is moved into `last_err`.
    fn record_outcome_by_ordinal(
        &self,
        ordinal: f64,
        attempt: u32,
        delay: Duration,
        succeeded: bool,
    ) {
        let Some(model) = self.retry_model.as_ref() else {
            return;
        };
        let Ok(mut m) = model.lock() else {
            return;
        };
        m.record_by_ordinal(ordinal, attempt, delay.as_millis() as u64, succeeded);
    }
}

#[async_trait]
impl<P: Provider> Provider for RetryPolicy<P> {
    fn name(&self) -> &str {
        self.inner.name()
    }

    async fn complete(&self, request: &ChatRequest) -> Result<ChatResponse> {
        let mut last_err = None;
        // Stash the previous attempt's (ordinal, attempt, delay) so
        // we can credit the model when the next attempt succeeds or
        // exhausts. ProviderError isn't Clone, so we snapshot the
        // ordinal before the error is moved into `last_err`.
        let mut last_retry: Option<(f64, u32, Duration)> = None;

        for attempt in 0..=self.config.max_retries {
            match self.inner.complete(request).await {
                Ok(response) => {
                    if attempt > 0 {
                        debug!(
                            provider = %self.inner.name(),
                            attempt,
                            "request succeeded after retry"
                        );
                        if let Some((ord, prev_attempt, prev_delay)) = last_retry.take() {
                            self.record_outcome_by_ordinal(ord, prev_attempt, prev_delay, true);
                        }
                    }
                    return Ok(response);
                }
                Err(err) => {
                    if !is_retryable(&err) || attempt == self.config.max_retries {
                        if let Some((ord, prev_attempt, prev_delay)) = last_retry.take() {
                            self.record_outcome_by_ordinal(ord, prev_attempt, prev_delay, false);
                        }
                        return Err(err);
                    }

                    let delay = self.effective_delay(&err, attempt);
                    let ord = error_ordinal(&err);

                    warn!(
                        provider = %self.inner.name(),
                        attempt,
                        delay_ms = delay.as_millis() as u64,
                        error = %err,
                        "retrying after transient error"
                    );

                    tokio::time::sleep(delay).await;
                    last_retry = Some((ord, attempt, delay));
                    last_err = Some(err);
                }
            }
        }

        // This should not be reachable, but handle it defensively
        Err(last_err.unwrap_or(ProviderError::RequestFailed(
            "retry loop exhausted without error".into(),
        )))
    }

    async fn complete_stream(
        &self,
        request: &ChatRequest,
        tx: mpsc::Sender<StreamChunk>,
    ) -> Result<()> {
        // Streaming retry with buffer-then-commit: each attempt writes to a
        // temporary channel. Only on success do we forward chunks to the real
        // sender, preventing partial output from failed attempts.
        let mut last_err = None;
        let mut last_retry: Option<(f64, u32, Duration)> = None;

        for attempt in 0..=self.config.max_retries {
            let (attempt_tx, mut attempt_rx) = mpsc::channel::<StreamChunk>(256);

            // Drain the attempt while it runs: a bounded channel nobody reads
            // until the call returns stalls any stream longer than its
            // capacity (a reasoning model sends well over 256 chunks).
            let drain = async {
                let mut buffered = Vec::new();
                while let Some(chunk) = attempt_rx.recv().await {
                    buffered.push(chunk);
                }
                buffered
            };
            let (result, buffered) = tokio::join!(self.inner.complete_stream(request, attempt_tx), drain);

            match result {
                Ok(()) => {
                    if attempt > 0 {
                        debug!(
                            provider = %self.inner.name(),
                            attempt,
                            "streaming request succeeded after retry"
                        );
                        if let Some((ord, prev_attempt, prev_delay)) = last_retry.take() {
                            self.record_outcome_by_ordinal(ord, prev_attempt, prev_delay, true);
                        }
                    }
                    // Forward buffered chunks to the real sender.
                    for chunk in buffered {
                        if tx.send(chunk).await.is_err() {
                            break;
                        }
                    }
                    return Ok(());
                }
                Err(err) => {
                    // Discard buffered partial chunks from this failed attempt.
                    drop(buffered);

                    if !is_retryable(&err) || attempt == self.config.max_retries {
                        if let Some((ord, prev_attempt, prev_delay)) = last_retry.take() {
                            self.record_outcome_by_ordinal(ord, prev_attempt, prev_delay, false);
                        }
                        return Err(err);
                    }

                    let delay = self.effective_delay(&err, attempt);
                    let ord = error_ordinal(&err);

                    warn!(
                        provider = %self.inner.name(),
                        attempt,
                        delay_ms = delay.as_millis() as u64,
                        error = %err,
                        "retrying streaming request after transient error"
                    );

                    tokio::time::sleep(delay).await;
                    last_retry = Some((ord, attempt, delay));
                    last_err = Some(err);
                }
            }
        }

        Err(last_err.unwrap_or(ProviderError::RequestFailed(
            "streaming retry loop exhausted without error".into(),
        )))
    }
}

impl<P: std::fmt::Debug> std::fmt::Debug for RetryPolicy<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RetryPolicy")
            .field("inner", &self.inner)
            .field("config", &self.config)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ChatMessage, ChatResponse, Choice, Usage};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A mock provider that fails a configurable number of times before succeeding.
    struct MockProvider {
        name: String,
        fail_count: AtomicU32,
        fail_with: fn(u32) -> ProviderError,
    }

    impl MockProvider {
        fn new(name: &str, failures: u32, fail_with: fn(u32) -> ProviderError) -> Self {
            Self {
                name: name.into(),
                fail_count: AtomicU32::new(failures),
                fail_with,
            }
        }

        fn success_response() -> ChatResponse {
            ChatResponse {
                id: "resp-1".into(),
                choices: vec![Choice {
                    index: 0,
                    message: ChatMessage::assistant("Hello!"),
                    finish_reason: Some("stop".into()),
                }],
                usage: Some(Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                    total_tokens: 15,
                }),
                model: "test-model".into(),
            }
        }
    }

    #[async_trait]
    impl Provider for MockProvider {
        fn name(&self) -> &str {
            &self.name
        }

        async fn complete(&self, _request: &ChatRequest) -> Result<ChatResponse> {
            let remaining = self.fail_count.load(Ordering::SeqCst);
            if remaining > 0 {
                self.fail_count.fetch_sub(1, Ordering::SeqCst);
                return Err((self.fail_with)(remaining));
            }
            Ok(Self::success_response())
        }
    }

    fn test_request() -> ChatRequest {
        ChatRequest::new("test-model", vec![ChatMessage::user("Hi")])
    }

    fn fast_retry_config() -> RetryConfig {
        RetryConfig {
            max_retries: 3,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(10),
            jitter_fraction: 0.0,
        }
    }

    #[test]
    fn default_retry_config() {
        let cfg = RetryConfig::default();
        assert_eq!(cfg.max_retries, 3);
        assert_eq!(cfg.base_delay, Duration::from_secs(1));
        assert_eq!(cfg.max_delay, Duration::from_secs(30));
        assert!((cfg.jitter_fraction - 0.25).abs() < f64::EPSILON);
    }

    #[test]
    fn is_retryable_rate_limited() {
        assert!(is_retryable(&ProviderError::RateLimited {
            retry_after_ms: 1000,
        }));
    }

    #[test]
    fn is_retryable_timeout() {
        assert!(is_retryable(&ProviderError::Timeout));
    }

    #[test]
    fn is_retryable_server_errors() {
        assert!(is_retryable(&ProviderError::ServerError {
            status: 500,
            body: "internal".into(),
        }));
        assert!(is_retryable(&ProviderError::ServerError {
            status: 502,
            body: "bad gateway".into(),
        }));
        assert!(is_retryable(&ProviderError::ServerError {
            status: 503,
            body: "unavailable".into(),
        }));
        assert!(is_retryable(&ProviderError::ServerError {
            status: 504,
            body: "timeout".into(),
        }));
    }

    #[test]
    fn is_not_retryable_auth() {
        assert!(!is_retryable(&ProviderError::AuthFailed("bad key".into())));
    }

    #[test]
    fn is_not_retryable_model_not_found() {
        assert!(!is_retryable(&ProviderError::ModelNotFound("gpt-5".into())));
    }

    #[test]
    fn is_not_retryable_not_configured() {
        assert!(!is_retryable(&ProviderError::NotConfigured(
            "missing key".into()
        )));
    }

    #[test]
    fn is_not_retryable_invalid_response() {
        assert!(!is_retryable(&ProviderError::InvalidResponse(
            "bad json".into()
        )));
    }

    #[test]
    fn is_not_retryable_client_error() {
        assert!(!is_retryable(&ProviderError::RequestFailed(
            "HTTP 400: bad request".into()
        )));
    }

    #[test]
    fn is_retryable_server_error_variant() {
        assert!(is_retryable(&ProviderError::ServerError {
            status: 500,
            body: "internal server error".into(),
        }));
        // 4xx via ServerError should not be retryable
        assert!(!is_retryable(&ProviderError::ServerError {
            status: 400,
            body: "bad request".into(),
        }));
    }

    #[test]
    fn compute_delay_exponential() {
        let config = RetryConfig {
            max_retries: 5,
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(60),
            jitter_fraction: 0.0,
        };
        // attempt 0: 100ms * 2^0 = 100ms
        let d0 = compute_delay(&config, 0);
        assert_eq!(d0.as_millis(), 100);

        // attempt 1: 100ms * 2^1 = 200ms
        let d1 = compute_delay(&config, 1);
        assert_eq!(d1.as_millis(), 200);

        // attempt 2: 100ms * 2^2 = 400ms
        let d2 = compute_delay(&config, 2);
        assert_eq!(d2.as_millis(), 400);
    }

    #[test]
    fn compute_delay_capped() {
        let config = RetryConfig {
            max_retries: 10,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(5),
            jitter_fraction: 0.0,
        };
        // attempt 5: 1s * 32 = 32s, but capped at 5s
        let d = compute_delay(&config, 5);
        assert_eq!(d.as_millis(), 5000);
    }

    #[test]
    fn compute_delay_with_jitter_bounded() {
        let config = RetryConfig {
            max_retries: 3,
            base_delay: Duration::from_millis(1000),
            max_delay: Duration::from_secs(30),
            jitter_fraction: 0.25,
        };
        // attempt 0: base = 1000ms, jitter max = 250ms
        // total should be in [1000, 1250]
        for _ in 0..20 {
            let d = compute_delay(&config, 0);
            let ms = d.as_millis();
            assert!(ms >= 1000, "delay {ms} < 1000");
            assert!(ms <= 1250, "delay {ms} > 1250");
        }
    }

    #[tokio::test]
    async fn retry_succeeds_first_try() {
        let mock = MockProvider::new("test", 0, |_| ProviderError::Timeout);
        let provider = RetryPolicy::new(mock, fast_retry_config());

        let resp = provider.complete(&test_request()).await.unwrap();
        assert_eq!(resp.choices[0].message.content.as_deref(), Some("Hello!"));
    }

    #[tokio::test]
    async fn retry_succeeds_after_transient_failures() {
        let mock = MockProvider::new("test", 2, |_| ProviderError::ServerError {
            status: 503,
            body: "unavailable".into(),
        });
        let provider = RetryPolicy::new(mock, fast_retry_config());

        let resp = provider.complete(&test_request()).await.unwrap();
        assert_eq!(resp.choices[0].message.content.as_deref(), Some("Hello!"));
    }

    #[tokio::test]
    async fn retry_exhausted_returns_last_error() {
        let mock = MockProvider::new("test", 10, |_| ProviderError::ServerError {
            status: 500,
            body: "error".into(),
        });
        let config = RetryConfig {
            max_retries: 2,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(5),
            jitter_fraction: 0.0,
        };
        let provider = RetryPolicy::new(mock, config);

        let err = provider.complete(&test_request()).await.unwrap_err();
        assert!(matches!(err, ProviderError::ServerError { .. }));
    }

    #[tokio::test]
    async fn retry_does_not_retry_auth_errors() {
        let call_count = Arc::new(AtomicU32::new(0));

        struct CountingProvider {
            count: Arc<AtomicU32>,
        }

        #[async_trait]
        impl Provider for CountingProvider {
            fn name(&self) -> &str {
                "counting"
            }
            async fn complete(&self, _req: &ChatRequest) -> Result<ChatResponse> {
                self.count.fetch_add(1, Ordering::SeqCst);
                Err(ProviderError::AuthFailed("invalid key".into()))
            }
        }

        let provider = RetryPolicy::new(
            CountingProvider {
                count: call_count.clone(),
            },
            fast_retry_config(),
        );

        let err = provider.complete(&test_request()).await.unwrap_err();
        assert!(matches!(err, ProviderError::AuthFailed(_)));
        // Should only be called once -- no retries for auth errors
        assert_eq!(call_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn retry_rate_limited_uses_suggested_delay() {
        let mock = MockProvider::new("test", 1, |_| ProviderError::RateLimited {
            retry_after_ms: 5,
        });
        let config = RetryConfig {
            max_retries: 3,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(100),
            jitter_fraction: 0.0,
        };
        let provider = RetryPolicy::new(mock, config);

        let resp = provider.complete(&test_request()).await.unwrap();
        assert_eq!(resp.choices[0].message.content.as_deref(), Some("Hello!"));
    }

    #[test]
    fn retry_policy_name_delegates() {
        let mock = MockProvider::new("my-provider", 0, |_| ProviderError::Timeout);
        let provider = RetryPolicy::new(mock, RetryConfig::default());
        assert_eq!(provider.name(), "my-provider");
    }

    #[test]
    fn retry_config_accessor() {
        let mock = MockProvider::new("test", 0, |_| ProviderError::Timeout);
        let config = RetryConfig {
            max_retries: 5,
            ..RetryConfig::default()
        };
        let provider = RetryPolicy::new(mock, config);
        assert_eq!(provider.retry_config().max_retries, 5);
    }

    #[tokio::test]
    async fn retry_model_records_success_on_recovery() {
        // A wrapped EML model should see a successful-retry outcome
        // recorded once the next attempt succeeds. This verifies the
        // whole wiring without any training happening.
        let model = Arc::new(Mutex::new(RetryModel::new()));
        assert_eq!(model.lock().unwrap().training_sample_count(), 0);

        let mock = MockProvider::new("test", 1, |_| ProviderError::ServerError {
            status: 503,
            body: "unavailable".into(),
        });
        let provider = RetryPolicy::with_model(mock, fast_retry_config(), model.clone());
        // Sanity: the handle roundtrips.
        assert!(provider.retry_model().is_some());

        let resp = provider.complete(&test_request()).await.unwrap();
        assert_eq!(resp.choices[0].message.content.as_deref(), Some("Hello!"));

        // Exactly one record: the failed attempt whose next retry
        // succeeded.
        assert_eq!(model.lock().unwrap().training_sample_count(), 1);
    }

    #[tokio::test]
    async fn retry_model_records_failure_on_exhaustion() {
        let model = Arc::new(Mutex::new(RetryModel::new()));

        let mock = MockProvider::new("test", 10, |_| ProviderError::ServerError {
            status: 500,
            body: "error".into(),
        });
        let config = RetryConfig {
            max_retries: 2,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(5),
            jitter_fraction: 0.0,
        };
        let provider = RetryPolicy::with_model(mock, config, model.clone());

        let err = provider.complete(&test_request()).await.unwrap_err();
        assert!(matches!(err, ProviderError::ServerError { .. }));

        // Final attempt exhausted → the last pending retry was
        // recorded as failed. (The attempts before it don't record
        // until their successor attempt's outcome is known.)
        assert_eq!(model.lock().unwrap().training_sample_count(), 1);
    }

    /// Streams `chunks` text deltas; fails the first `fail_first` attempts
    /// after sending a partial chunk.
    struct StreamingProvider {
        chunks: usize,
        fail_first: AtomicU32,
    }

    #[async_trait]
    impl Provider for StreamingProvider {
        fn name(&self) -> &str {
            "streaming"
        }

        async fn complete(&self, _request: &ChatRequest) -> Result<ChatResponse> {
            Ok(MockProvider::success_response())
        }

        async fn complete_stream(&self, _request: &ChatRequest, tx: mpsc::Sender<StreamChunk>) -> Result<()> {
            if self.fail_first.load(Ordering::SeqCst) > 0 {
                self.fail_first.fetch_sub(1, Ordering::SeqCst);
                let _ = tx.send(StreamChunk::TextDelta { text: "partial".into() }).await;
                return Err(ProviderError::Timeout);
            }
            for i in 0..self.chunks {
                tx.send(StreamChunk::TextDelta { text: format!("{i} ") }).await.map_err(|_| ProviderError::Timeout)?;
            }
            Ok(())
        }
    }

    async fn collect_stream(provider: StreamingProvider) -> (Result<()>, Vec<StreamChunk>) {
        let policy = RetryPolicy::new(provider, fast_retry_config());
        let (tx, mut rx) = mpsc::channel(8);
        let request = test_request();
        let run = async { policy.complete_stream(&request, tx).await };
        let collect = async {
            let mut out = Vec::new();
            while let Some(c) = rx.recv().await {
                out.push(c);
            }
            out
        };
        tokio::join!(run, collect)
    }

    /// A stream longer than the attempt buffer (256) must not stall: reasoning
    /// models send well over that many chunks.
    #[tokio::test]
    async fn long_streams_complete_through_the_retry_buffer() {
        let result = tokio::time::timeout(Duration::from_secs(5), collect_stream(StreamingProvider { chunks: 1500, fail_first: AtomicU32::new(0) }))
            .await
            .expect("stream stalled");
        assert!(result.0.is_ok());
        assert_eq!(result.1.len(), 1500);
    }

    #[tokio::test]
    async fn a_failed_attempts_partial_chunks_are_discarded() {
        let (result, chunks) = collect_stream(StreamingProvider { chunks: 3, fail_first: AtomicU32::new(1) }).await;
        assert!(result.is_ok());
        assert_eq!(chunks.len(), 3);
        assert!(chunks.iter().all(|c| !matches!(c, StreamChunk::TextDelta { text } if text == "partial")));
    }
}
