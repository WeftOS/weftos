//! HTTP client wrapper for the Telegram Bot API.
//!
//! [`TelegramClient`] provides typed methods for the subset of the
//! Telegram Bot API used by the channel plugin: `getMe`, `getUpdates`,
//! and `sendMessage`.

use reqwest::Client;
use tracing::{debug, trace};

use clawft_types::error::ChannelError;

use super::types::{Message, SendMessageRequest, TelegramResponse, Update, User};

/// HTTP client for the Telegram Bot API.
///
/// Wraps a [`reqwest::Client`] and the bot token to provide typed
/// request methods. The base URL can be overridden for testing.
pub struct TelegramClient {
    /// Shared HTTP client.
    http: Client,
    /// Base URL: `https://api.telegram.org/bot{token}` by default.
    base_url: String,
}

impl TelegramClient {
    /// Create a new client with the given bot token.
    pub fn new(token: String) -> Self {
        let base_url = format!("https://api.telegram.org/bot{token}");
        Self {
            http: Client::new(),
            base_url,
        }
    }

    /// Create a client pointing at a custom base URL (for testing).
    #[cfg(test)]
    pub fn with_base_url(_token: String, base_url: String) -> Self {
        Self {
            http: Client::new(),
            base_url,
        }
    }

    /// Return the base URL used for API requests.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Fetch new updates using long polling.
    ///
    /// `offset` is the ID of the first update to return; `timeout` is the
    /// long-poll timeout in seconds (0 for non-blocking).
    pub async fn get_updates(
        &self,
        offset: Option<i64>,
        timeout: u64,
    ) -> Result<Vec<Update>, ChannelError> {
        let mut url = format!("{}/getUpdates?timeout={timeout}", self.base_url);
        if let Some(off) = offset {
            url.push_str(&format!("&offset={off}"));
        }

        trace!(url = %url, "polling for updates");

        let resp = self
            .http
            // The URL embeds the bot token and the error Display includes it,
            // so every transport error below goes through without_url().
            .get(&url)
            .send()
            .await
            .map_err(|e| ChannelError::ConnectionFailed(e.without_url().to_string()))?;

        let body: TelegramResponse<Vec<Update>> = resp
            .json()
            .await
            .map_err(|e| ChannelError::ReceiveFailed(e.without_url().to_string()))?;

        if !body.ok {
            let desc = body.description.unwrap_or_else(|| "unknown error".into());
            return Err(ChannelError::ReceiveFailed(desc));
        }

        let updates = body.result.unwrap_or_default();
        debug!(count = updates.len(), "received updates");
        Ok(updates)
    }

    /// Send a text message to a chat.
    ///
    /// Returns the sent [`Message`] on success.
    pub async fn send_message(
        &self,
        chat_id: i64,
        text: &str,
        reply_to: Option<i64>,
    ) -> Result<Message, ChannelError> {
        let url = format!("{}/sendMessage", self.base_url);

        let req = SendMessageRequest {
            chat_id,
            text: text.to_owned(),
            parse_mode: None,
            reply_to_message_id: reply_to,
        };

        debug!(chat_id, "sending message");

        let resp = self
            .http
            .post(&url)
            .json(&req)
            .send()
            .await
            .map_err(|e| ChannelError::SendFailed(e.without_url().to_string()))?;

        let body: TelegramResponse<Message> = resp
            .json()
            .await
            .map_err(|e| ChannelError::SendFailed(e.without_url().to_string()))?;

        if !body.ok {
            let desc = body.description.unwrap_or_else(|| "unknown error".into());
            return Err(ChannelError::SendFailed(desc));
        }

        body.result
            .ok_or_else(|| ChannelError::SendFailed("missing result in response".into()))
    }

    /// Verify the bot token by calling the `getMe` endpoint.
    ///
    /// Returns the bot's [`User`] info on success.
    pub async fn get_me(&self) -> Result<User, ChannelError> {
        let url = format!("{}/getMe", self.base_url);

        debug!("verifying bot token");

        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| ChannelError::ConnectionFailed(e.without_url().to_string()))?;

        let body: TelegramResponse<User> = resp
            .json()
            .await
            .map_err(|e| ChannelError::AuthFailed(e.without_url().to_string()))?;

        if !body.ok {
            let desc = body.description.unwrap_or_else(|| "unauthorized".into());
            return Err(ChannelError::AuthFailed(desc));
        }

        body.result
            .ok_or_else(|| ChannelError::AuthFailed("missing result in response".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_construction() {
        let client = TelegramClient::new("123:ABC".into());
        assert_eq!(client.base_url(), "https://api.telegram.org/bot123:ABC");
    }

    #[test]
    fn custom_base_url() {
        let client = TelegramClient::with_base_url("tok".into(), "http://localhost:9999".into());
        assert_eq!(client.base_url(), "http://localhost:9999");
    }

    const DUMMY_TOKEN: &str = "123456:DUMMY-not-a-real-token";

    fn assert_no_token(err: &ChannelError) {
        let shown = format!("{err} / {err:?}");
        assert!(!shown.contains(DUMMY_TOKEN), "token leaked: {shown}");
        assert!(!shown.contains("DUMMY"), "token fragment leaked: {shown}");
    }

    #[tokio::test]
    async fn connect_errors_do_not_contain_token() {
        // Port 1 on loopback refuses connections; no network involved.
        let base = format!("http://127.0.0.1:1/bot{DUMMY_TOKEN}");
        let client = TelegramClient::with_base_url(DUMMY_TOKEN.into(), base);
        assert_no_token(&client.get_me().await.unwrap_err());
        assert_no_token(&client.get_updates(None, 0).await.unwrap_err());
        assert_no_token(&client.send_message(1, "hi", None).await.unwrap_err());
    }

    #[tokio::test]
    async fn decode_errors_do_not_contain_token() {
        use wiremock::matchers::any;
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;
        let base = format!("{}/bot{DUMMY_TOKEN}", server.uri());
        let client = TelegramClient::with_base_url(DUMMY_TOKEN.into(), base);
        assert_no_token(&client.get_me().await.unwrap_err());
        assert_no_token(&client.get_updates(None, 0).await.unwrap_err());
        assert_no_token(&client.send_message(1, "hi", None).await.unwrap_err());
    }

    // NOTE: Live HTTP tests are not included because they would require a
    // real Telegram bot token or an HTTP mock server. The URL construction
    // and error-mapping logic is validated through the type-level tests
    // above and through the integration-level channel tests.
}
