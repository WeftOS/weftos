//! Microsoft Teams channel adapter implementation.
//!
//! Implements [`ChannelAdapter`] for Microsoft Teams via the Bot
//! Framework. The adapter performs three real network operations:
//!
//! 1. **OAuth2 client-credentials** against
//!    `https://login.microsoftonline.com/{tenant}/oauth2/v2.0/token`
//!    with scope `https://api.botframework.com/.default`. Tokens are
//!    cached with a 60-second safety margin before `expires_in`.
//! 2. **Outbound activity POST** to
//!    `{service_url}/v3/conversations/{conversationId}/activities`
//!    with `Authorization: Bearer <token>`. The response's `id` is
//!    returned to the caller.
//! 3. **Inbound webhook** -- an axum HTTP listener bound to
//!    `webhook_bind` accepts Bot Framework activity POSTs, decodes the
//!    JWT presented in `Authorization: Bearer ...` (RS256 signature,
//!    issuer, audience and expiry verified against the Bot Framework
//!    JWKS, see [`super::jwt`]), and publishes message-typed activities on the
//!    [`ChannelAdapterHost`] message bus.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use clawft_plugin::error::PluginError;
use clawft_plugin::message::MessagePayload;
use clawft_plugin::traits::{ChannelAdapter, ChannelAdapterHost};

use super::jwt::{HttpJwksSource, TeamsJwtVerifier};
use super::types::TeamsAdapterConfig;

/// Cached OAuth2 access token entry.
#[derive(Clone, Debug)]
struct CachedToken {
    access_token: String,
    /// Wall-clock instant after which we should refresh proactively.
    refresh_after: Instant,
}

/// Microsoft Teams channel adapter.
pub struct TeamsChannelAdapter {
    config: TeamsAdapterConfig,
    http: reqwest::Client,
    token: Mutex<Option<CachedToken>>,
}

/// Bot Framework token endpoint response shape.
#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    /// Lifetime in seconds.
    #[serde(default = "default_expires_in")]
    expires_in: u64,
}

fn default_expires_in() -> u64 {
    3600
}

/// Bot Framework activity (subset; only the fields we read or write).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Activity {
    #[serde(default, rename = "type")]
    pub activity_type: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub service_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<ActivityParty>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation: Option<ConversationAccount>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct ActivityParty {
    #[serde(default)]
    pub id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ConversationAccount {
    #[serde(default)]
    pub id: String,
}

/// Token-endpoint response carrying just the message id we return.
#[derive(Debug, Deserialize)]
struct ActivityPostResponse {
    id: String,
}

impl TeamsChannelAdapter {
    /// Create a new Teams channel adapter.
    pub fn new(config: TeamsAdapterConfig) -> Self {
        Self {
            config,
            http: reqwest::Client::new(),
            token: Mutex::new(None),
        }
    }

    /// Check if a user principal name is in the allow list.
    pub fn is_user_allowed(&self, upn: &str) -> bool {
        if self.config.allowed_users.is_empty() {
            return true;
        }
        let upn_lower = upn.to_lowercase();
        self.config
            .allowed_users
            .iter()
            .any(|u| u.to_lowercase() == upn_lower)
    }

    /// Acquire a Bot Framework access token, refreshing the cache when
    /// the previous token is missing or within 60 seconds of expiry.
    async fn access_token(&self) -> Result<String, PluginError> {
        let now = Instant::now();
        {
            let cache = self.token.lock().await;
            if let Some(t) = cache.as_ref()
                && now < t.refresh_after
            {
                return Ok(t.access_token.clone());
            }
        }

        let endpoint = self.config.token_endpoint();
        debug!(endpoint = %endpoint, "teams: requesting bot framework access token");

        let form = [
            ("grant_type", "client_credentials"),
            ("client_id", self.config.app_id.as_str()),
            ("client_secret", self.config.app_password.expose()),
            ("scope", "https://api.botframework.com/.default"),
        ];

        let resp = self
            .http
            .post(&endpoint)
            .form(&form)
            .send()
            .await
            .map_err(|e| {
                PluginError::ExecutionFailed(format!("teams: token request failed: {e}"))
            })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(PluginError::ExecutionFailed(format!(
                "teams: token endpoint returned {status}: {body}"
            )));
        }

        let parsed: TokenResponse = resp.json().await.map_err(|e| {
            PluginError::ExecutionFailed(format!("teams: malformed token response: {e}"))
        })?;

        let lifetime = Duration::from_secs(parsed.expires_in);
        // 60s safety margin so we refresh before MS rejects the token.
        let margin = Duration::from_secs(60);
        let refresh_after = now + lifetime.saturating_sub(margin);

        let cached = CachedToken {
            access_token: parsed.access_token.clone(),
            refresh_after,
        };
        *self.token.lock().await = Some(cached);
        Ok(parsed.access_token)
    }

    /// POST an outbound activity to the Bot Framework conversations
    /// endpoint and return the response's `id`.
    async fn post_activity(
        &self,
        service_url: &str,
        conversation_id: &str,
        text: &str,
    ) -> Result<String, PluginError> {
        let token = self.access_token().await?;
        let base = service_url.trim_end_matches('/');
        let url = format!("{base}/v3/conversations/{conversation_id}/activities");
        let body = Activity {
            activity_type: "message".into(),
            text: text.into(),
            ..Default::default()
        };

        debug!(
            url = %url,
            convo = %conversation_id,
            content_len = text.len(),
            "teams: posting outbound activity"
        );

        let resp = self
            .http
            .post(&url)
            .bearer_auth(&token)
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                PluginError::ExecutionFailed(format!("teams: activity POST failed: {e}"))
            })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(PluginError::ExecutionFailed(format!(
                "teams: activity endpoint returned {status}: {body}"
            )));
        }

        let parsed: ActivityPostResponse = resp.json().await.map_err(|e| {
            PluginError::ExecutionFailed(format!("teams: malformed activity response: {e}"))
        })?;
        Ok(parsed.id)
    }
}

#[async_trait]
impl ChannelAdapter for TeamsChannelAdapter {
    fn name(&self) -> &str {
        "teams"
    }

    fn display_name(&self) -> &str {
        "Microsoft Teams"
    }

    fn supports_threads(&self) -> bool {
        true
    }

    fn supports_media(&self) -> bool {
        false
    }

    async fn start(
        &self,
        host: Arc<dyn ChannelAdapterHost>,
        cancel: CancellationToken,
    ) -> Result<(), PluginError> {
        info!("Teams channel adapter starting");

        if self.config.tenant_id.is_empty() {
            return Err(PluginError::LoadFailed(
                "teams adapter: tenant_id is required".into(),
            ));
        }
        if self.config.app_id.is_empty() {
            return Err(PluginError::LoadFailed(
                "teams adapter: app_id (or legacy client_id) is required".into(),
            ));
        }
        if self.config.app_password.is_empty() {
            return Err(PluginError::LoadFailed(
                "teams adapter: app_password (or legacy client_secret) is required".into(),
            ));
        }

        if self.config.webhook_bind.is_empty() {
            // Inbound listener disabled. Outbound-only mode is a
            // legitimate configuration (e.g. a worker that only
            // replies to messages routed in via another channel).
            warn!(
                "teams adapter: webhook_bind is empty; running in \
                 outbound-only mode (no inbound activity listener)"
            );
            cancel.cancelled().await;
            info!("Teams channel adapter shutting down");
            return Ok(());
        }

        // Bind axum listener.
        let bind_addr: std::net::SocketAddr = self.config.webhook_bind.parse().map_err(|e| {
            PluginError::LoadFailed(format!(
                "teams adapter: invalid webhook_bind {:?}: {e}",
                self.config.webhook_bind
            ))
        })?;

        // Fails (rather than accepting every token) if app_id is unusable.
        let verifier = TeamsJwtVerifier::new(
            &self.config.app_id,
            Box::new(HttpJwksSource::new(self.http.clone())),
        )
        .map_err(|e| PluginError::LoadFailed(format!("teams adapter: {e}")))?;

        let allowed = self.config.allowed_users.clone();
        let host_for_handler = host.clone();
        let app = build_router(host_for_handler, allowed, Arc::new(verifier));

        let listener = tokio::net::TcpListener::bind(bind_addr)
            .await
            .map_err(|e| {
                PluginError::LoadFailed(format!("teams adapter: bind {bind_addr} failed: {e}"))
            })?;

        info!(addr = %bind_addr, "teams: inbound webhook listening");

        let cancel_for_serve = cancel.clone();
        let serve = axum::serve(listener, app).with_graceful_shutdown(async move {
            cancel_for_serve.cancelled().await;
        });

        if let Err(e) = serve.await {
            error!(error = %e, "teams: webhook server exited with error");
            return Err(PluginError::ExecutionFailed(format!(
                "teams: webhook server failed: {e}"
            )));
        }

        info!("Teams channel adapter shutting down");
        Ok(())
    }

    async fn send(&self, target: &str, payload: &MessagePayload) -> Result<String, PluginError> {
        let content = payload.as_text().ok_or_else(|| {
            PluginError::ExecutionFailed("teams: only text payloads supported".into())
        })?;

        if self.config.tenant_id.is_empty() {
            return Err(PluginError::ExecutionFailed(
                "teams: tenant_id not configured".into(),
            ));
        }
        if self.config.service_url.is_empty() {
            return Err(PluginError::ExecutionFailed(
                "teams: service_url not configured (set per-conversation \
                 from inbound activity, or supply in adapter config)"
                    .into(),
            ));
        }

        self.post_activity(&self.config.service_url, target, content)
            .await
    }
}

// ---------------------------------------------------------------------------
// Inbound webhook
// ---------------------------------------------------------------------------

/// State shared with the axum handler.
#[derive(Clone)]
struct WebhookState {
    host: Arc<dyn ChannelAdapterHost>,
    allowed_users: Vec<String>,
    verifier: Arc<TeamsJwtVerifier>,
}

/// Build the axum router that handles inbound Bot Framework activities.
fn build_router(
    host: Arc<dyn ChannelAdapterHost>,
    allowed_users: Vec<String>,
    verifier: Arc<TeamsJwtVerifier>,
) -> axum::Router {
    use axum::routing::post;
    let state = WebhookState {
        host,
        allowed_users,
        verifier,
    };
    axum::Router::new()
        .route("/api/messages", post(handle_activity))
        .with_state(state)
}

/// Handle a single inbound activity POST.
async fn handle_activity(
    axum::extract::State(state): axum::extract::State<WebhookState>,
    headers: axum::http::HeaderMap,
    axum::Json(activity): axum::Json<Activity>,
) -> axum::http::StatusCode {
    // Extract Authorization: Bearer <jwt>; reject otherwise.
    let auth = match headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        Some(s) => s,
        None => {
            warn!("teams webhook: rejected activity without Authorization");
            return axum::http::StatusCode::UNAUTHORIZED;
        }
    };
    let token = auth.strip_prefix("Bearer ").unwrap_or(auth);
    if let Err(e) = state.verifier.verify(token).await {
        warn!(error = %e, "teams webhook: rejected activity with invalid JWT");
        return axum::http::StatusCode::UNAUTHORIZED;
    }

    if activity.activity_type != "message" {
        debug!(kind = %activity.activity_type, "teams webhook: ignoring non-message activity");
        return axum::http::StatusCode::OK;
    }

    let from = activity.from.clone().unwrap_or_default();
    if !state.allowed_users.is_empty() {
        let allowed = state
            .allowed_users
            .iter()
            .any(|u| u.eq_ignore_ascii_case(&from.id) || u.eq_ignore_ascii_case(&from.name));
        if !allowed {
            warn!(sender = %from.id, "teams webhook: rejected activity from disallowed sender");
            return axum::http::StatusCode::FORBIDDEN;
        }
    }

    let convo = activity.conversation.clone().unwrap_or_default();
    let mut metadata: HashMap<String, serde_json::Value> = HashMap::new();
    if !activity.service_url.is_empty() {
        metadata.insert(
            "service_url".into(),
            serde_json::Value::String(activity.service_url.clone()),
        );
    }
    if !activity.id.is_empty() {
        metadata.insert(
            "activity_id".into(),
            serde_json::Value::String(activity.id.clone()),
        );
    }

    let payload = MessagePayload::text(activity.text.clone());

    if let Err(e) = state
        .host
        .deliver_inbound("teams", &from.id, &convo.id, payload, metadata)
        .await
    {
        error!(error = %e, "teams webhook: deliver_inbound failed");
        return axum::http::StatusCode::INTERNAL_SERVER_ERROR;
    }

    axum::http::StatusCode::OK
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    use clawft_types::secret::SecretString;

    use super::super::jwt_tests::fixture_verifier_and_token;

    fn make_config() -> TeamsAdapterConfig {
        TeamsAdapterConfig {
            tenant_id: "test-tenant".into(),
            app_id: "test-client".into(),
            app_password: SecretString::new("test-secret"),
            ..Default::default()
        }
    }

    #[test]
    fn name_is_teams() {
        let adapter = TeamsChannelAdapter::new(make_config());
        assert_eq!(adapter.name(), "teams");
    }

    #[test]
    fn display_name() {
        let adapter = TeamsChannelAdapter::new(make_config());
        assert_eq!(adapter.display_name(), "Microsoft Teams");
    }

    #[test]
    fn supports_threads_yes() {
        let adapter = TeamsChannelAdapter::new(make_config());
        assert!(adapter.supports_threads());
    }

    #[test]
    fn supports_media_no() {
        let adapter = TeamsChannelAdapter::new(make_config());
        assert!(!adapter.supports_media());
    }

    #[test]
    fn user_filtering() {
        let mut config = make_config();
        config.allowed_users = vec!["admin@company.com".into()];
        let adapter = TeamsChannelAdapter::new(config);

        assert!(adapter.is_user_allowed("admin@company.com"));
        assert!(adapter.is_user_allowed("ADMIN@COMPANY.COM"));
        assert!(!adapter.is_user_allowed("stranger@evil.com"));
    }

    #[test]
    fn empty_allow_list_allows_all() {
        let adapter = TeamsChannelAdapter::new(make_config());
        assert!(adapter.is_user_allowed("anyone@company.com"));
    }

    #[tokio::test]
    async fn send_non_text_fails() {
        let adapter = TeamsChannelAdapter::new(make_config());
        let payload = MessagePayload::structured(serde_json::json!({}));
        let result = adapter.send("convo-1", &payload).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn send_without_service_url_fails() {
        let adapter = TeamsChannelAdapter::new(make_config());
        let payload = MessagePayload::text("hi");
        let err = adapter.send("convo-1", &payload).await.unwrap_err();
        assert!(err.to_string().contains("service_url"));
    }

    #[tokio::test]
    async fn start_validates_tenant_id() {
        let mut config = make_config();
        config.tenant_id = String::new();
        let adapter = TeamsChannelAdapter::new(config);

        let host = Arc::new(MockHost::default());
        let cancel = CancellationToken::new();
        let result = adapter.start(host, cancel).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("tenant_id"));
    }

    #[tokio::test]
    async fn start_validates_app_id() {
        let mut config = make_config();
        config.app_id = String::new();
        let adapter = TeamsChannelAdapter::new(config);

        let host = Arc::new(MockHost::default());
        let cancel = CancellationToken::new();
        let result = adapter.start(host, cancel).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("app_id"));
    }

    #[tokio::test]
    async fn start_validates_app_password() {
        let mut config = make_config();
        config.app_password = SecretString::default();
        let adapter = TeamsChannelAdapter::new(config);

        let host = Arc::new(MockHost::default());
        let cancel = CancellationToken::new();
        let result = adapter.start(host, cancel).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("app_password"));
    }

    #[tokio::test]
    async fn start_outbound_only_shuts_down_on_cancel() {
        let adapter = TeamsChannelAdapter::new(make_config());
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        let host = Arc::new(MockHost::default());
        let handle = tokio::spawn(async move { adapter.start(host, cancel_clone).await });

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        cancel.cancel();
        let result = handle.await.unwrap();
        assert!(result.is_ok());
    }

    // -- Integration tests against wiremock for the OAuth + send paths --

    /// Full token endpoint round-trip.
    #[tokio::test]
    async fn token_endpoint_round_trip_against_wiremock() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth2/v2.0/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "token_type": "Bearer",
                "expires_in": 3600,
                "ext_expires_in": 3600,
                "access_token": "test-bearer-token-001"
            })))
            .mount(&server)
            .await;

        let mut config = make_config();
        config.oauth_token_url = format!("{}/oauth2/v2.0/token", server.uri());

        let adapter = TeamsChannelAdapter::new(config);
        let token = adapter
            .access_token()
            .await
            .expect("token fetch must succeed");
        assert_eq!(token, "test-bearer-token-001");

        // Second call should hit the cache rather than the server -- but
        // wiremock will serve again if reached. Just assert the value
        // is identical.
        let token2 = adapter.access_token().await.unwrap();
        assert_eq!(token, token2);
    }

    /// Outbound activity POST round-trip: the adapter must acquire a
    /// token, POST to `{service}/v3/conversations/{convo}/activities`,
    /// and surface the response's `id`.
    #[tokio::test]
    async fn send_endpoint_round_trip_against_wiremock() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/oauth2/v2.0/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "send-test-token",
                "expires_in": 3600
            })))
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/v3/conversations/convo-42/activities"))
            .and(header("authorization", "Bearer send-test-token"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "id": "1632432598347"
            })))
            .mount(&server)
            .await;

        let mut config = make_config();
        config.oauth_token_url = format!("{}/oauth2/v2.0/token", server.uri());
        config.service_url = server.uri();

        let adapter = TeamsChannelAdapter::new(config);
        let payload = MessagePayload::text("hello teams");
        let id = adapter
            .send("convo-42", &payload)
            .await
            .expect("send must succeed");
        assert_eq!(id, "1632432598347");
    }

    /// Inbound activity POST: the adapter's webhook handler must
    /// publish the activity on the [`ChannelAdapterHost`] message bus.
    #[tokio::test]
    async fn inbound_activity_publishes_on_message_bus() {
        let host = Arc::new(MockHost::default());
        let (verifier, jwt) = fixture_verifier_and_token();
        let app = build_router(host.clone(), Vec::new(), Arc::new(verifier));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        // A real RS256 token verified against a stubbed JWKS.

        let activity = serde_json::json!({
            "type": "message",
            "id": "act-1",
            "text": "hi from teams",
            "serviceUrl": "https://smba.example.com/amer/",
            "from": {"id": "user-7", "name": "Alice"},
            "conversation": {"id": "convo-77"}
        });

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://{addr}/api/messages"))
            .header("authorization", format!("Bearer {jwt}"))
            .json(&activity)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);

        // Confirm host saw the message.
        let delivered = host.delivered.lock().unwrap().clone();
        assert_eq!(delivered.len(), 1);
        let (channel, sender, chat, text) = &delivered[0];
        assert_eq!(channel, "teams");
        assert_eq!(sender, "user-7");
        assert_eq!(chat, "convo-77");
        assert_eq!(text, "hi from teams");

        server_handle.abort();
    }

    #[tokio::test]
    async fn inbound_activity_rejects_missing_authorization() {
        let host = Arc::new(MockHost::default());
        let (verifier, _) = fixture_verifier_and_token();
        let app = build_router(host.clone(), Vec::new(), Arc::new(verifier));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://{addr}/api/messages"))
            .json(&serde_json::json!({"type":"message","text":"x"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 401);
        assert!(host.delivered.lock().unwrap().is_empty());

        server_handle.abort();
    }

    /// An unsigned / HS256 / garbage bearer token must be refused with 401
    /// and nothing may reach the message bus.
    #[tokio::test]
    async fn inbound_activity_rejects_unverifiable_tokens() {
        let host = Arc::new(MockHost::default());
        let (verifier, _) = fixture_verifier_and_token();
        let app = build_router(host.clone(), Vec::new(), Arc::new(verifier));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let client = reqwest::Client::new();
        for bad in [
            "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiJ4In0.",
            "eyJhbGciOiJub25lIiwidHlwIjoiSldUIn0.eyJzdWIiOiIxIn0.",
            "not-a-jwt",
        ] {
            let resp = client
                .post(format!("http://{addr}/api/messages"))
                .header("authorization", format!("Bearer {bad}"))
                .json(&serde_json::json!({"type":"message","text":"x"}))
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status().as_u16(), 401, "token {bad:?}");
        }
        assert!(host.delivered.lock().unwrap().is_empty());
        server_handle.abort();
    }

    // -- Mock host ---------------------------------------------------

    #[derive(Default)]
    struct MockHost {
        #[allow(clippy::type_complexity)]
        delivered: StdMutex<Vec<(String, String, String, String)>>,
    }

    #[async_trait]
    impl ChannelAdapterHost for MockHost {
        async fn deliver_inbound(
            &self,
            channel: &str,
            sender_id: &str,
            chat_id: &str,
            payload: MessagePayload,
            _metadata: HashMap<String, serde_json::Value>,
        ) -> Result<(), PluginError> {
            let text = payload.as_text().unwrap_or_default().to_string();
            self.delivered.lock().unwrap().push((
                channel.into(),
                sender_id.into(),
                chat_id.into(),
                text,
            ));
            Ok(())
        }
    }
}
