//! The child kernel's link to its parent, the user daemon (ADR-103 Phase 2 F).
//!
//! A `project`-profile kernel holds no embedding model, no voice pipeline and
//! no provider API key. It reaches them through the user daemon's
//! `shared.*` RPCs ([`crate::shared_rpc`]):
//!
//! * [`ParentLink`]: one JSON-lines call per connection over the parent's
//!   unix socket, with a bounded reconnect backoff, a health flag per
//!   service and a background monitor.
//! * [`RemoteEmbedder`]: an `Embedder` / `EmbeddingProvider` over
//!   `shared.embed`.
//! * [`ParentLlmBackend`]: the [`LlmBackend`] a project kernel's
//!   `LlmClient` runs on, over `shared.llm.chat` and `shared.llm.models`.
//!
//! **Fail closed.** When the parent is down, unauthenticated or refuses, the
//! call returns a typed error ([`PARENT_UNAVAILABLE_KIND`] for the first two).
//! There is no fallback to a local model, a local key or a hash embedder, and
//! none must be added: the whole point is that the secrets stay in the user
//! tier.
//!
//! The parent socket, the project token and the project id come from
//! `<run dir>/spawn.json`, which the user daemon writes (0600) when it
//! spawns the child:
//!
//! ```json
//! {"parent_socket": "/Users/x/.weftos/run/kernel.sock",
//!  "project_id": "01JB8Z3Q0V6X9KQ4M2N7T5R1WD",
//!  "project_token": "wft_..."}
//! ```
//!
//! `parent_socket` defaults to `<run dir>/../kernel.sock`. The token is
//! scoped to the one project; every request also names that project in
//! `Request.project`. The token is never logged ([`ParentLink`]'s `Debug`
//! redacts it).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clawft_rpc::{DaemonClient, Request};
use clawft_types::project::SPAWN_TTL_SECS;
use clawft_types::project::token_consts::{
    PROJECT_TOKEN_REFRESH_SECS, PROJECT_TOKEN_TTL_SECS, TOKEN_REFRESH_METHOD,
};
use serde::Deserialize;
use serde_json::Value;

use crate::protocol::SharedServicesHealth;

/// `error_kind` when the parent cannot be reached or the link has no
/// credentials. Callers branch on it; nothing may substitute a local result.
pub const PARENT_UNAVAILABLE_KIND: &str = "parent_unavailable";

/// Bound on dialing the parent socket: a wedged parent must not wedge a call.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Least seconds between refresh attempts.
const REFRESH_RETRY_SECS: u64 = 30;
/// Bound on the token refresh call.
const REFRESH_TIMEOUT: Duration = Duration::from_secs(10);

/// The link's project token and when it stops working.
#[derive(Default)]
struct TokenState {
    secret: Option<String>,
    /// Unix seconds of the last refresh attempt (success or not), so a
    /// refusing parent is not asked on every call.
    last_attempt_unix: u64,
    /// Unix seconds; `None` means the link does not track expiry (a token
    /// given to [`ParentLink::new`]) and never refreshes by itself.
    expires_unix: Option<u64>,
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Which shared service a call is for (selects the health flag and timeout).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Service {
    Embeddings,
    Llm,
    Voice,
}

impl Service {
    fn timeout(self) -> Duration {
        match self {
            Service::Embeddings => Duration::from_secs(60),
            // A long generation on a large local model (matches
            // `LlmConfig::request_timeout`).
            Service::Llm => Duration::from_secs(330),
            Service::Voice => Duration::from_secs(10),
        }
    }
}

/// Why a parent call did not produce a result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParentError {
    /// Down, unreachable, timed out, or the link has no credentials.
    Unavailable(String),
    /// The parent answered with an error (`rate_limited`,
    /// `budget_exceeded`, `project_scope_mismatch`, ...).
    Refused { kind: String, message: String },
}

impl ParentError {
    /// The `error_kind` for this error.
    pub fn kind(&self) -> &str {
        match self {
            ParentError::Unavailable(_) => PARENT_UNAVAILABLE_KIND,
            ParentError::Refused { kind, .. } => kind,
        }
    }
}

impl std::fmt::Display for ParentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParentError::Unavailable(m) => write!(f, "{PARENT_UNAVAILABLE_KIND}: {m}"),
            ParentError::Refused { kind, message } => write!(f, "{kind}: {message}"),
        }
    }
}

impl std::error::Error for ParentError {}

/// Reconnect policy: `attempts` connects, sleeping `initial`, doubled up to
/// `max`, between them. Kept short so a stopped parent fails a call in well
/// under a second; the monitor does the long-haul retrying.
#[derive(Debug, Clone, Copy)]
pub struct Backoff {
    pub attempts: u32,
    pub initial: Duration,
    pub max: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            attempts: 3,
            initial: Duration::from_millis(100),
            max: Duration::from_secs(1),
        }
    }
}

/// What `spawn.json` carries for this module. Unknown keys are ignored (the
/// supervisor adds its own).
#[derive(Debug, Default, Deserialize)]
struct LooseSpawnFile {
    #[serde(default)]
    parent_socket: Option<PathBuf>,
    #[serde(default)]
    project_id: Option<String>,
    #[serde(default)]
    project_token: Option<String>,
}

/// Client to the user daemon. Cheap to share (`Arc`); every call opens its
/// own connection, so a long LLM call never blocks an embedding.
pub struct ParentLink {
    socket: PathBuf,
    project_id: Option<String>,
    token: Mutex<TokenState>,
    /// Set when the link could not be configured; every call then fails
    /// closed with this text.
    config_error: Option<String>,
    backoff: Backoff,
    embeddings_up: AtomicBool,
    llm_up: AtomicBool,
    voice_up: AtomicBool,
}

impl std::fmt::Debug for ParentLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParentLink")
            .field("socket", &self.socket)
            .field("project_id", &self.project_id)
            .field("token", &"<redacted>")
            .field("config_error", &self.config_error)
            .finish()
    }
}

impl ParentLink {
    /// A link with explicit credentials.
    pub fn new(socket: PathBuf, project_id: String, token: String) -> Self {
        Self::build(socket, Some(project_id), Some(token), None)
    }

    /// A link that fails every call closed with `reason` (no `spawn.json`,
    /// unreadable, or no token). The kernel still boots; its shared services
    /// report `down`.
    pub fn unconfigured(reason: impl Into<String>) -> Self {
        Self::build(PathBuf::new(), None, None, Some(reason.into()))
    }

    fn build(
        socket: PathBuf,
        project_id: Option<String>,
        token: Option<String>,
        config_error: Option<String>,
    ) -> Self {
        Self {
            socket,
            project_id,
            token: Mutex::new(TokenState { secret: token, last_attempt_unix: 0, expires_unix: None }),
            config_error,
            backoff: Backoff::default(),
            embeddings_up: AtomicBool::new(false),
            llm_up: AtomicBool::new(false),
            voice_up: AtomicBool::new(false),
        }
    }

    /// Replace the reconnect policy (tests).
    pub fn with_backoff(mut self, backoff: Backoff) -> Self {
        self.backoff = backoff;
        self
    }

    /// A link for a child that has just consumed its `spawn.json` (the
    /// token was issued at most [`SPAWN_TTL_SECS`] ago). The token is
    /// short-lived ([`PROJECT_TOKEN_TTL_SECS`]); the link renews it with
    /// `project.token.refresh` when [`PROJECT_TOKEN_REFRESH_SECS`] are left,
    /// before every call and from the monitor.
    pub fn new_from_spawn(socket: PathBuf, project_id: String, token: String) -> Self {
        let link = Self::new(socket, project_id, token);
        let expires = now_unix() + PROJECT_TOKEN_TTL_SECS - SPAWN_TTL_SECS;
        link.with_token_expiry(expires)
    }

    /// Track the current token's expiry (tests).
    pub fn with_token_expiry(self, expires_unix: u64) -> Self {
        self.token.lock().expect("token lock poisoned").expires_unix = Some(expires_unix);
        self
    }

    /// Renew the project token now. The old token stays in place when the
    /// parent is down or refuses (the call that needed it then fails closed
    /// on its own).
    pub async fn refresh_token(&self) -> Result<(), ParentError> {
        if let Some(why) = &self.config_error {
            return Err(ParentError::Unavailable(why.clone()));
        }
        let (Some(token), Some(project)) = (self.current_token(), &self.project_id) else {
            return Err(ParentError::Unavailable("link has no project token".into()));
        };
        let mut client = self.connect().await?;
        let mut req = Request::with_params(TOKEN_REFRESH_METHOD, serde_json::json!({ "id": project }))
            .with_auth(token);
        req.project = Some(project.clone());
        let resp = tokio::time::timeout(REFRESH_TIMEOUT, client.call(req))
            .await
            .map_err(|_| ParentError::Unavailable("token refresh timed out".into()))?
            .map_err(|e| ParentError::Unavailable(format!("token refresh: {e}")))?;
        if !resp.ok {
            return Err(ParentError::Refused {
                kind: resp.error_kind.unwrap_or_else(|| "parent_error".into()),
                message: resp.error.unwrap_or_default(),
            });
        }
        let new = resp
            .result
            .as_ref()
            .and_then(|r| r.get("token"))
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| ParentError::Refused {
                kind: "parent_error".into(),
                message: "token refresh reply has no token".into(),
            })?;
        let mut st = self.token.lock().expect("token lock poisoned");
        st.secret = Some(new.to_owned());
        st.expires_unix = Some(now_unix() + PROJECT_TOKEN_TTL_SECS);
        Ok(())
    }

    /// Refresh when the token is within [`PROJECT_TOKEN_REFRESH_SECS`] of
    /// expiry. Errors are swallowed: the next call decides what to do.
    pub async fn maybe_refresh(&self) {
        let due = {
            let mut st = self.token.lock().expect("token lock poisoned");
            let now = now_unix();
            let due = st.expires_unix.is_some_and(|e| now + PROJECT_TOKEN_REFRESH_SECS >= e)
                && now >= st.last_attempt_unix + REFRESH_RETRY_SECS;
            if due {
                st.last_attempt_unix = now;
            }
            due
        };
        if due && let Err(e) = self.refresh_token().await {
            tracing::warn!(error = %e, "project token refresh failed");
        }
    }

    fn current_token(&self) -> Option<String> {
        self.token.lock().expect("token lock poisoned").secret.clone()
    }

    /// Read `spawn.json` at `path`. A missing, unreadable or token-less file
    /// gives an [`unconfigured`](Self::unconfigured) link, never a panic or a
    /// partially trusted one.
    pub fn from_spawn_json(path: &Path) -> Self {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) => return Self::unconfigured(format!("{}: {e}", path.display())),
        };
        let spawn: LooseSpawnFile = match serde_json::from_str(&text) {
            Ok(s) => s,
            Err(e) => return Self::unconfigured(format!("{}: {e}", path.display())),
        };
        let socket = spawn.parent_socket.or_else(|| {
            path.parent()
                .and_then(Path::parent)
                .map(|root| root.join("kernel.sock"))
        });
        match (socket, spawn.project_id, spawn.project_token) {
            (Some(socket), Some(id), Some(token)) if !token.trim().is_empty() => {
                Self::new(socket, id, token)
            }
            _ => Self::unconfigured(format!(
                "{}: needs project_id and project_token",
                path.display()
            )),
        }
    }

    /// The project this link speaks for.
    pub fn project_id(&self) -> Option<&str> {
        self.project_id.as_deref()
    }

    fn flag(&self, service: Service) -> &AtomicBool {
        match service {
            Service::Embeddings => &self.embeddings_up,
            Service::Llm => &self.llm_up,
            Service::Voice => &self.voice_up,
        }
    }

    fn mark_all(&self, up: bool) {
        for s in [Service::Embeddings, Service::Llm, Service::Voice] {
            self.flag(s).store(up, Ordering::Relaxed);
        }
    }

    /// `parent` or `down` for each shared service, for `kernel.status`.
    pub fn health(&self) -> SharedServicesHealth {
        let word = |s| if self.flag(s).load(Ordering::Relaxed) { "parent" } else { "down" };
        SharedServicesHealth {
            embeddings: word(Service::Embeddings).into(),
            llm: word(Service::Llm).into(),
            voice: word(Service::Voice).into(),
        }
    }

    async fn connect(&self) -> Result<DaemonClient, ParentError> {
        let mut delay = self.backoff.initial;
        for attempt in 0..self.backoff.attempts.max(1) {
            let dial = tokio::time::timeout(CONNECT_TIMEOUT, DaemonClient::connect_path(&self.socket));
            if let Ok(Some(client)) = dial.await {
                return Ok(client);
            }
            if attempt + 1 < self.backoff.attempts {
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(self.backoff.max);
            }
        }
        Err(ParentError::Unavailable(format!(
            "cannot connect to the user daemon at {}",
            self.socket.display()
        )))
    }

    /// Call `method` on the parent for `service`. Fails closed.
    pub async fn call(
        &self,
        service: Service,
        method: &str,
        params: Value,
    ) -> Result<Value, ParentError> {
        let result = self.call_inner(service, method, params).await;
        match &result {
            Err(ParentError::Unavailable(_)) => self.flag(service).store(false, Ordering::Relaxed),
            _ => self.flag(service).store(true, Ordering::Relaxed),
        }
        result
    }

    async fn call_inner(
        &self,
        service: Service,
        method: &str,
        params: Value,
    ) -> Result<Value, ParentError> {
        if let Some(why) = &self.config_error {
            return Err(ParentError::Unavailable(why.clone()));
        }
        // `DaemonClient::call` attaches an implicit `admin` scope to a
        // request with no `auth`; never let that happen from a child.
        self.maybe_refresh().await;
        let (Some(token), Some(project)) = (self.current_token(), &self.project_id) else {
            return Err(ParentError::Unavailable("link has no project token".into()));
        };
        let mut client = self.connect().await?;
        let mut req = Request::with_params(method, params).with_auth(token);
        req.project = Some(project.clone());
        let resp = tokio::time::timeout(service.timeout(), client.call(req))
            .await
            .map_err(|_| ParentError::Unavailable(format!("{method} timed out")))?
            .map_err(|e| ParentError::Unavailable(format!("{method}: {e}")))?;
        if resp.ok {
            Ok(resp.result.unwrap_or(Value::Null))
        } else {
            Err(ParentError::Refused {
                kind: resp.error_kind.unwrap_or_else(|| "parent_error".into()),
                message: resp.error.unwrap_or_default(),
            })
        }
    }

    /// One liveness probe (`kernel.handshake`); sets every service flag.
    pub async fn probe(&self) -> bool {
        let up = self
            .call_inner(Service::Voice, "kernel.handshake", Value::Null)
            .await
            .is_ok();
        self.mark_all(up);
        up
    }

    /// Probe in the background: every 15 s while up, with exponential
    /// backoff (1 s to 30 s) while down. Ends when the last `Arc` is dropped.
    /// Needs a tokio runtime; without one the flags only move with calls.
    pub fn spawn_monitor(self: &Arc<Self>) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let weak = Arc::downgrade(self);
        handle.spawn(async move {
            let mut down_for = Duration::from_secs(1);
            loop {
                let Some(link) = weak.upgrade() else { return };
                let up = link.probe().await;
                drop(link);
                let wait = if up {
                    down_for = Duration::from_secs(1);
                    Duration::from_secs(15)
                } else {
                    let w = down_for;
                    down_for = (down_for * 2).min(Duration::from_secs(30));
                    w
                };
                tokio::time::sleep(wait).await;
            }
        });
    }
}

pub use crate::parent_services::{ParentLlmBackend, RemoteEmbedder};

#[cfg(test)]
#[path = "parent_link_tests.rs"]
mod tests;
