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
    /// number. `skip`: ignored.
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
}

/// `POST /v1/cos/utterance` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserUtterance {
    pub text: String,
    pub source: Source,
}

impl UserUtterance {
    pub fn typed(text: impl Into<String>) -> Self {
        Self { text: text.into(), source: Source::Typed }
    }

    pub fn voice(text: impl Into<String>) -> Self {
        Self { text: text.into(), source: Source::Voice }
    }

    pub fn touch(text: impl Into<String>) -> Self {
        Self { text: text.into(), source: Source::Touch }
    }
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
}
