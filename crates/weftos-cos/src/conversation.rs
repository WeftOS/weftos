//! The in-process conversation API (contract §3). The HTTP/SSE server in
//! [`crate::server`] is a thin shell over this trait, and the console host on
//! the same machine can call it directly.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::CosError;
use crate::events::Event;

/// Identifier of one user turn; returned by `utter` and stamped on its events.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TurnId(pub String);

impl TurnId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().simple().to_string())
    }
}

impl Default for TurnId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for TurnId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The endpoint of the agent host's own microphone, push-to-talk and console.
pub const HOST_ENDPOINT: &str = "host";

/// Whether `name` is a usable endpoint id: 1 to 32 characters of
/// `a-z 0-9 _ -`.
pub fn valid_endpoint_name(name: &str) -> bool {
    (1..=32).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// Where an utterance came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Voice,
    Typed,
    /// A tap on a guide view (survey button, checklist item, step control).
    Touch,
}

/// `POST /v1/cos/answer` body: a touch interaction with a guide view.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GuideAnswer {
    pub kind: AnswerKind,
    /// The checklist, steps or ask id the interaction belongs to.
    pub id: String,
    /// `answer`: the 1-based choice number. `toggle`: the item id, or
    /// `{item_id, done}`. `step`: `"next"`, `"back"` or a 1-based step
    /// number. `skip` and `back`: ignored.
    #[serde(default)]
    pub value: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AnswerKind {
    Answer,
    Toggle,
    Step,
    Skip,
    /// Re-ask the previous survey question (Q1: re-show Q1).
    Back,
}

/// `POST /v1/cos/utterance` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserUtterance {
    pub text: String,
    pub source: Source,
    /// Which front end the turn came from (`host` when absent). The reply's
    /// final `message` names it, and only `host` replies are spoken by the
    /// agent process itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
}

impl UserUtterance {
    pub fn typed(text: impl Into<String>) -> Self {
        Self { text: text.into(), source: Source::Typed, endpoint: None }
    }

    pub fn voice(text: impl Into<String>) -> Self {
        Self { text: text.into(), source: Source::Voice, endpoint: None }
    }

    /// The same utterance attributed to `endpoint`.
    pub fn from_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = Some(endpoint.into());
        self
    }

    pub fn touch(text: impl Into<String>) -> Self {
        Self { text: text.into(), source: Source::Touch, endpoint: None }
    }
}

/// Turn messages kept for `GET /v1/cos/turns`.
pub const TURN_HISTORY_CAP: usize = 200;
/// Default and maximum `limit` of `GET /v1/cos/turns`.
pub const TURNS_DEFAULT_LIMIT: usize = 20;
pub const TURNS_MAX_LIMIT: usize = 100;

/// One final message of a turn, as `GET /v1/cos/turns` returns it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnMessage {
    pub turn_id: TurnId,
    /// `user` or `assistant`.
    pub role: String,
    /// The endpoint the turn belongs to.
    pub endpoint: String,
    /// Redacted, as on the event stream.
    pub text: String,
    /// RFC 3339, UTC.
    pub ts: String,
}

/// The agent as the console sees it: utterances in, events out.
#[async_trait]
pub trait Conversation: Send + Sync {
    /// Accept an utterance and return its turn id at once; the turn runs in
    /// the background and reports through [`Conversation::events`].
    async fn utter(&self, utterance: UserUtterance) -> Result<TurnId, CosError>;

    /// Accept a touch interaction with a guide view. A survey answer or a
    /// skip becomes a `touch` user turn; a toggle or step changes the view
    /// and re-emits it without a model turn. Returns a turn id either way.
    async fn answer(&self, answer: GuideAnswer) -> Result<TurnId, CosError>;

    /// Subscribe to the event stream. Each subscriber sees events emitted
    /// after it subscribed.
    fn events(&self) -> broadcast::Receiver<Event>;

    /// The most recent final user and assistant messages, oldest first.
    /// `limit` is already validated (1..=[`TURNS_MAX_LIMIT`]).
    fn recent_turns(&self, limit: usize) -> Vec<TurnMessage>;
}
