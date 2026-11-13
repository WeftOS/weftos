//! Chat-window panel for the Explorer.
//!
//! Renders a scrollable conversation against the daemon's `llm.prompt`
//! RPC. Triggers when the selected substrate value matches a chat
//! sentinel:
//!
//! ```json
//! { "kind": "chat", "model": "local" }
//! ```
//!
//! The daemon publishes one such sentinel at boot under
//! `substrate/<daemon-node>/ui/chat` so this panel has a stable mount
//! point in the substrate tree.
//!
//! ## Wire shape
//!
//! Each user turn fires `llm.prompt` with the **full** conversation so
//! far (system + every user/assistant turn). We don't trust the daemon
//! to preserve session state — the panel is the source of truth.
//!
//! ## Scope cuts (deliberate)
//!
//! - Streaming lands via `agent.chat_stream` (WEFT-253): the panel fires
//!   that verb and polls `substrate/_derived/chat/<conv>/stream` for
//!   progressive frames while the RPC is in flight. Mid-generation
//!   LLM token callbacks are a daemon-side follow-up; today the daemon
//!   cascades the final assistant text as typewriter frames before
//!   returning, and the panel renders the draft bubble live.
//! - Conversation history is **in-memory only** for the panel lifetime
//!   (close / Explorer re-select resets). On-disk rehydrate via
//!   `substrate.list` is a follow-up; WEFT-254 ships the multi-session
//!   sidebar with in-memory slots.
//! - **Model / provider chip strip** (WEFT-256): selectable chips
//!   populated from `llm.models`. Selection rides on
//!   `agent.chat` / `agent.chat_stream` as `metadata.model` and
//!   persists across panel re-paints via egui memory.
//!
//! ## Shipped UX (this file)
//!
//! - **Multi-conversation sidebar** (WEFT-254): list sessions by id +
//!   last-active age + first user-message snippet; **New** creates a
//!   fresh slot; click switches active history + stream.
//! - **Markdown rendering** in assistant bubbles via `egui_commonmark`
//!   so code blocks / lists / headers don't render as raw markdown.
//!   WEFT-252.
//! - **System-prompt affordance**: collapsible textarea above the
//!   message field. The text rides on the `system` field and is sent
//!   alongside `agent.chat` params (the daemon merges it after the
//!   workspace identity prompt). WEFT-255.
//! - **Heartbeat label** below the input that mirrors the substrate
//!   `derived/chat/<conv_id>/status` payload while a request is in
//!   flight, replacing the blocking spinner. WEFT-257.
//! - **Identity-drift warning**: when the response's `identity_source`
//!   isn't a recognised stable source (e.g. `"docs-fallback"`),
//!   surface a non-dismissable warning chip above the input. WEFT-259.
//! - **Model / provider switcher** in a chip strip above the history
//!   (WEFT-256).
//! - **ThreadDock** (WEFT-284): when the panel hosts ≥2 parallel agent
//!   threads (swarm / supervisor pair), a column-per-agent dock paints
//!   above the active transcript so streams never interleave. Idle
//!   single-conversation chat keeps the classic layout.

use std::sync::Arc;

use eframe::egui;
use egui_commonmark::{CommonMarkCache, CommonMarkViewer};
use serde_json::Value;

use crate::canon::{AgentThread, CanonWidget, ThreadDock, ThreadDockState, ThreadPhase};
use crate::live::{self, Command, Live, ReplyRx};

/// Shape-match priority for the chat sentinel. Higher than
/// `control_toggle` (25) and `workshop` (30) so a chat sentinel
/// dispatched from the same selection slot wins decisively.
pub const PRIORITY: u32 = 40;

/// Sampling temperature passed to the LLM service. Conservative — chat
/// quality over creative drift; the daemon's own default also lives in
/// this range, so this is a soft repeat-of-default that lets the panel
/// override later without a daemon change.
const DEFAULT_TEMPERATURE: f32 = 0.4;

/// Hard cap on generated tokens per turn. Matches the daemon's default
/// (512); explicit here so a daemon-side bump doesn't silently change
/// chat-window behaviour.
const DEFAULT_MAX_TOKENS: u32 = 512;

/// How often (ms) the panel re-polls `substrate.read` for the
/// progressive stream frame while a turn is in flight. Lower = snappier
/// typewriter, higher = fewer RPCs. 120 ms is well under the daemon's
/// 4 ms cascade step × chunk size without flooding the socket.
const STREAM_POLL_INTERVAL_MS: f64 = 120.0;

/// Fixed width (px) of the WEFT-254 multi-conversation sidebar strip.
const CONV_SIDEBAR_WIDTH: f32 = 196.0;
/// Height reserved under the history scroll area (input, buttons, footer,
/// spacers and the history frame's margin).
const CHAT_BELOW_HISTORY_H: f32 = 175.0;
/// Smallest history height; below this the panel is too short to be useful.
const MIN_HISTORY_H: f32 = 60.0;

/// Max characters shown for the first-message snippet in the sidebar.
const SNIPPET_MAX_CHARS: usize = 48;

/// egui memory key for the WEFT-256 model selection. Shared across the
/// sidebar Chat app and the Explorer sentinel panel so picking a model
/// in either surface survives re-select / panel reopen within the
/// session (and across egui persistence when the host enables it).
const MODEL_SELECTION_MEM_ID: &str = "weft.chat.selected_model";

/// How often (ms) we re-issue `llm.models` when the previous fetch
/// failed or returned empty. Successes cache for the panel lifetime
/// (until Clear / selection reset).
const MODELS_RETRY_INTERVAL_MS: f64 = 5_000.0;

/// Shape predicate. Returns [`PRIORITY`] when `value` looks like a
/// chat sentinel:
///
/// ```json
/// { "kind": "chat", ... }
/// ```
///
/// Strict on `kind == "chat"` — we don't probe other shapes so an
/// arbitrary object with a `model` key doesn't false-match.
pub fn matches(value: &Value) -> u32 {
    let Some(obj) = value.as_object() else {
        return 0;
    };
    let kind_ok = obj
        .get("kind")
        .and_then(Value::as_str)
        .map(|s| s == "chat")
        .unwrap_or(false);
    if kind_ok { PRIORITY } else { 0 }
}

/// One conversation turn. Mirrors `LlmPromptMessage` on the wire but
/// kept as a local type so the panel doesn't depend on `clawft-weave`
/// (which would pull tokio + the daemon-side world into the GUI crate
/// for free).
#[derive(Debug, Clone, PartialEq)]
pub struct ChatMessage {
    /// Role: `system`, `user`, or `assistant`. `error` is a UI-only
    /// pseudo-role rendered as a red bubble; it's filtered out of the
    /// wire payload before sending.
    pub role: String,
    /// Message content.
    pub content: String,
}

impl ChatMessage {
    fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: content.into(),
        }
    }
    fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".into(),
            content: content.into(),
        }
    }
    fn error(content: impl Into<String>) -> Self {
        Self {
            role: "error".into(),
            content: content.into(),
        }
    }
}

/// Extract a WEFT-334 `error_kind` from a transport error string.
///
/// `native_live::call` prefixes typed failures as
/// `"[<kind>] <method>: <message>"` so the panel can branch without
/// holding the full RPC envelope. Returns `Some("timeout" |
/// "gate_deny" | "llm_error" | …)` when present.
pub fn chat_error_kind(err: &str) -> Option<&str> {
    let rest = err.strip_prefix('[')?;
    let (kind, _) = rest.split_once(']')?;
    if kind.is_empty() || kind.chars().any(char::is_whitespace) {
        return None;
    }
    Some(kind)
}

/// Panel state. Owned by the [`Explorer`](super::Explorer) and reset
/// whenever the selection moves (via `Explorer::on_select`) so a stale
/// conversation doesn't reappear when the user re-selects the chat
/// sentinel after navigating elsewhere.
#[derive(Default)]
pub struct ChatView {
    /// Optional system prompt prepended to every wire payload. WEFT-255
    /// surfaces this via a collapsible textarea above the message
    /// field. The daemon-side concierge owns the workspace identity
    /// prompt; this `system` is layered on top of it (not in place of)
    /// so the panel cannot hijack the workspace persona.
    pub system: Option<String>,
    /// Whether the system-prompt editor is expanded in the UI. Persists
    /// across paints within the panel lifetime; reset to false when the
    /// selection moves.
    pub system_expanded: bool,
    /// Full conversation. Wire payload always starts here (with `system`
    /// prepended if set) — the daemon does not preserve session state.
    pub history: Vec<ChatMessage>,
    /// Draft input text bound to the multiline `TextEdit`.
    pub draft: String,
    /// Stable conversation id for this panel session. Used both to
    /// identify the conversation in `agent.chat` params (so per-conv
    /// state on the daemon-side `AgentService` lines up across turns)
    /// and to derive the substrate heartbeat path
    /// `substrate/_derived/chat/<conv_id>/status`. Lazily minted on
    /// first submit so an empty/idle panel doesn't burn an id.
    conv_id: Option<String>,
    /// Last `identity_source` echoed by `agent.chat`. Drives the
    /// drift-warning chip (WEFT-259). `None` while no successful turn
    /// has landed; `Some("clawft")` is the stable, non-warning case.
    last_identity_source: Option<String>,
    /// Markdown renderer cache. WEFT-252 — `CommonMarkCache` retains
    /// parsed AST per source string across paints, so re-rendering an
    /// existing assistant turn doesn't re-parse the markdown each
    /// frame. Cleared implicitly when the panel state is reset on
    /// selection change (the cache is rebuilt on demand).
    cache: CommonMarkCache,
    /// Pending `agent.chat_stream` reply channel. `Some` while a request
    /// is in flight; the input + Send button disable in that window so
    /// the user can't fire a second request against `llama-server`'s
    /// single in-flight slot (the service crate's semaphore would
    /// serialize them anyway, but UI feedback is clearer).
    pending: Option<ReplyRx>,
    /// Accumulated assistant draft from progressive stream frames
    /// (WEFT-253). `Some` while a turn is in flight and at least one
    /// non-empty frame has landed; rendered as a live assistant bubble
    /// that grows as tokens arrive. Cleared when the final RPC reply
    /// lands (the bubble is committed into `history`).
    streaming_draft: Option<String>,
    /// Last stream-frame `phase` (`thinking` / `generating` / `done` /
    /// `error`). Drives the in-flight heartbeat label so the user sees
    /// more than a static "waiting for completion…".
    stream_phase: Option<String>,
    /// Highest stream `seq` applied. Frames with a lower-or-equal seq
    /// are ignored so a late substrate.read can't rewind the draft.
    stream_seq: u64,
    /// In-flight `substrate.read` for the stream path. At most one poll
    /// is outstanding so we don't queue a flood of reads.
    stream_poll: Option<ReplyRx>,
    /// `live::now_ms()` of the last stream-poll submit. Throttles the
    /// poll cadence to [`STREAM_POLL_INTERVAL_MS`].
    last_stream_poll_ms: f64,
    /// Currently selected model id (WEFT-256). When set, rides on
    /// `agent.chat` / `agent.chat_stream` as `metadata.model`. Seeded
    /// from the sentinel, `llm.models` default, or egui memory.
    selected_model: Option<String>,
    /// Catalog from the last successful `llm.models` response (or a
    /// sentinel fallback). Empty only on a brand-new view.
    available_models: Vec<ModelChip>,
    /// True once a live `llm.models` response has been applied. Keeps
    /// the sentinel-seeded fallback from suppressing the real catalog
    /// fetch on subsequent paints.
    models_catalog_live: bool,
    /// Pending `llm.models` reply. At most one outstanding.
    models_poll: Option<ReplyRx>,
    /// `live::now_ms()` of the last models-fetch submit (success or
    /// failure). Used to throttle retries after a failed probe.
    last_models_poll_ms: f64,
    /// Last models-fetch error (if any). Surfaced as a muted hint on
    /// the chip strip; not a chat error bubble.
    models_error: Option<String>,
}

/// One chip in the WEFT-256 model/provider strip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelChip {
    /// Wire model id (sent as `metadata.model`).
    pub id: String,
    /// Provider bucket (`local`, `openrouter`, …).
    pub provider: String,
    /// Human label (usually equals `id`).
    pub label: String,
    /// Daemon's configured default.
    pub is_default: bool,
}

impl ChatView {
    /// Whether a request is currently in flight.
    pub fn is_in_flight(&self) -> bool {
        self.pending.is_some()
    }

    /// Build the JSON params for `agent.chat` from the current history.
    ///
    /// Matches `AgentChatParams { messages, temperature, max_tokens,
    /// conv_id }`. When [`Self::system`] is set (WEFT-255) it rides on
    /// the request as a `system` user-prompt message at the head of
    /// `messages` — the daemon-side concierge prepends its own identity
    /// prompt before this, so panel-side `system` is layered on top
    /// (additional context) rather than replacing the workspace persona.
    ///
    /// `conv_id` is included whenever it has been minted (i.e. once the
    /// first turn has been submitted; the very first call to this fn
    /// during `submit_draft` mints it). Both Phase A (ephemeral default)
    /// and Phase C (panel-supplied) call sites keep working.
    ///
    /// Filters UI-only `error` pseudo-roles out of the wire payload.
    ///
    /// WEFT-256: when [`Self::selected_model`] is set, it rides under
    /// `metadata.model` so the daemon agent loop / tiered router can
    /// honor the panel chip-strip selection (see WEFT-31 model_override).
    pub fn build_request_params(&self, next_user: &str) -> Value {
        let mut messages: Vec<Value> = Vec::new();
        if let Some(system) = self.system.as_deref()
            && !system.trim().is_empty()
        {
            messages.push(serde_json::json!({
                "role": "system",
                "content": system,
            }));
        }
        for m in self.history.iter().filter(|m| m.role != "error") {
            messages.push(serde_json::json!({
                "role": m.role,
                "content": m.content,
            }));
        }
        messages.push(serde_json::json!({
            "role": "user",
            "content": next_user,
        }));
        let mut obj = serde_json::Map::new();
        obj.insert("messages".into(), Value::Array(messages));
        obj.insert("temperature".into(), serde_json::json!(DEFAULT_TEMPERATURE));
        obj.insert("max_tokens".into(), serde_json::json!(DEFAULT_MAX_TOKENS));
        if let Some(id) = self.conv_id.as_deref() {
            obj.insert("conv_id".into(), serde_json::json!(id));
        }
        if let Some(model) = self.selected_model.as_deref()
            && !model.trim().is_empty()
        {
            let mut meta = serde_json::Map::new();
            meta.insert("model".into(), Value::String(model.to_owned()));
            obj.insert("metadata".into(), Value::Object(meta));
        }
        Value::Object(obj)
    }

    /// Currently selected model id (WEFT-256), if any.
    pub fn selected_model(&self) -> Option<&str> {
        self.selected_model.as_deref()
    }

    /// Available model chips (WEFT-256), after `llm.models` lands.
    pub fn available_models(&self) -> &[ModelChip] {
        &self.available_models
    }

    /// Select a model by id. Persists into egui memory when a `Ui` is
    /// later painted (see [`paint_model_chip_strip`]); pure state
    /// update here so unit tests can drive it without egui.
    pub fn select_model(&mut self, model_id: impl Into<String>) {
        let id = model_id.into();
        if id.trim().is_empty() {
            return;
        }
        self.selected_model = Some(id);
    }

    /// Apply an `llm.models` success payload (WEFT-256). Pure over the
    /// value so tests can drive it without RPC plumbing.
    ///
    /// - Parses `models[]` into [`ModelChip`]s (falls back to a single
    ///   chip from `default_model` when the array is empty/missing).
    /// - Seeds [`Self::selected_model`] from the catalog default when
    ///   the panel has no selection yet.
    pub fn on_models_ok(&mut self, response: &Value) {
        self.models_error = None;
        self.models_catalog_live = true;
        let mut chips: Vec<ModelChip> = response
            .get("models")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| {
                        let id = v.get("id").and_then(Value::as_str)?.to_owned();
                        if id.is_empty() {
                            return None;
                        }
                        let provider = v
                            .get("provider")
                            .and_then(Value::as_str)
                            .unwrap_or("local")
                            .to_owned();
                        let label = v
                            .get("label")
                            .and_then(Value::as_str)
                            .unwrap_or(id.as_str())
                            .to_owned();
                        let is_default = v
                            .get("is_default")
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                        Some(ModelChip {
                            id,
                            provider,
                            label,
                            is_default,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        if chips.is_empty()
            && let Some(def) = response
                .get("default_model")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        {
            let provider = response
                .get("default_provider")
                .and_then(Value::as_str)
                .unwrap_or("local")
                .to_owned();
            chips.push(ModelChip {
                id: def.to_owned(),
                provider,
                label: def.to_owned(),
                is_default: true,
            });
        }

        self.available_models = chips;

        // Seed selection: keep an already-chosen id when still present;
        // otherwise pick the default chip (or the first entry).
        let still_valid = self
            .selected_model
            .as_deref()
            .is_some_and(|sel| self.available_models.iter().any(|c| c.id == sel));
        if !still_valid {
            let pick = self
                .available_models
                .iter()
                .find(|c| c.is_default)
                .or_else(|| self.available_models.first())
                .map(|c| c.id.clone());
            if let Some(id) = pick {
                self.selected_model = Some(id);
            }
        }
    }

    /// Record an `llm.models` failure. Does not clear a previous
    /// catalog so a transient probe blip keeps the last known chips.
    pub fn on_models_err(&mut self, err: &str) {
        self.models_error = Some(err.to_owned());
    }

    /// State-machine entry: a successful `agent.chat` /
    /// `agent.chat_stream` response landed. Appends the assistant text
    /// to history and clears the in-flight + streaming state. Pure
    /// function over [`Value`] so tests can drive it without any RPC
    /// plumbing.
    ///
    /// Tool-call rendering as collapsible bubbles lands in commit 9
    /// (plan §11.4); the spike just shows the final assistant text.
    /// Both old `{ completion: "..." }` and new
    /// `{ assistant_text: "..." }` shapes are accepted to make rolling
    /// the wasm bundle and the daemon independently safe.
    pub fn on_response_ok(&mut self, response: &Value) {
        let text = response
            .get("assistant_text")
            .or_else(|| response.get("completion"))
            .and_then(Value::as_str)
            .unwrap_or("(empty completion)");
        self.history.push(ChatMessage::assistant(text.to_string()));
        // Capture identity_source for the WEFT-259 drift-warning chip.
        // The daemon injects this at the wire boundary; if absent, we
        // intentionally clear the cached value so a previously-sticky
        // warning doesn't haunt subsequent turns.
        self.last_identity_source = response
            .get("identity_source")
            .and_then(Value::as_str)
            .map(str::to_owned);
        self.clear_stream_state();
    }

    /// Apply a progressive stream frame (WEFT-253). Ignores frames with
    /// `seq <= self.stream_seq` so a late poll can't rewind the draft.
    /// Pure over the frame value so unit tests can drive it without
    /// substrate plumbing.
    pub fn on_stream_frame(&mut self, frame: &Value) {
        let seq = frame.get("seq").and_then(Value::as_u64).unwrap_or(0);
        if seq < self.stream_seq {
            return;
        }
        // Equal seq still applies phase/text updates (idempotent
        // Replace of the same frame); only strictly-older are dropped.
        if seq > self.stream_seq {
            self.stream_seq = seq;
        }
        if let Some(phase) = frame.get("phase").and_then(Value::as_str) {
            self.stream_phase = Some(phase.to_owned());
        }
        if let Some(text) = frame.get("text").and_then(Value::as_str)
            && !text.is_empty()
        {
            // Empty text leaves a prior draft alone (thinking frames
            // carry `text: ""` and must not flash a blank bubble or
            // rewind a growing draft on a late re-read).
            self.streaming_draft = Some(text.to_owned());
        }
        if frame.get("done").and_then(Value::as_bool).unwrap_or(false)
            && let Some(err) = frame.get("error").and_then(Value::as_str)
        {
            // Error frames surface via the final RPC reply too; stash
            // the message so the heartbeat label can show it until the
            // oneshot lands.
            self.stream_phase = Some(format!("error: {err}"));
        }
    }

    /// Clear every WEFT-253 streaming field. Called when the final RPC
    /// reply lands (ok or err) so the next turn starts clean.
    fn clear_stream_state(&mut self) {
        self.pending = None;
        self.streaming_draft = None;
        self.stream_phase = None;
        self.stream_seq = 0;
        self.stream_poll = None;
        self.last_stream_poll_ms = 0.0;
    }

    /// Substrate path for the progressive stream frame, or `None` if
    /// no conv_id has been minted yet. Sibling of [`Self::heartbeat_path`].
    pub fn stream_path(&self) -> Option<String> {
        self.conv_id
            .as_deref()
            .map(|id| format!("substrate/_derived/chat/{id}/stream"))
    }

    /// Current streaming draft text (accumulated assistant tokens).
    pub fn streaming_draft(&self) -> Option<&str> {
        self.streaming_draft.as_deref()
    }

    /// Last stream phase label, if any.
    pub fn stream_phase(&self) -> Option<&str> {
        self.stream_phase.as_deref()
    }

    /// Whether the most recent response indicates a suspicious
    /// identity source (e.g. `"docs-fallback"`) that should warn the
    /// user. WEFT-259.
    ///
    /// Returns `Some(source)` when a warning chip should render. Treats
    /// `"clawft"` (the canonical workspace-loaded identity) as the only
    /// stable source today; anything else — including unknown future
    /// values — is surfaced so a regression in the daemon's identity
    /// loader can't silently degrade the chat experience.
    pub fn identity_warning(&self) -> Option<&str> {
        match self.last_identity_source.as_deref() {
            None => None,
            Some("clawft") => None,
            Some(other) => Some(other),
        }
    }

    /// State-machine entry: an `agent.chat_stream` / `agent.chat`
    /// request failed. Appends a UI-only `error` bubble and clears the
    /// in-flight + streaming state.
    ///
    /// WEFT-334: when the transport prefixes a structured kind
    /// (`[timeout] …`, `[gate_deny] …`, `[llm_error] …` — see
    /// [`chat_error_kind`]), the full string is still shown; callers
    /// that want kind-specific UI (retry affordance, policy link, …)
    /// should call [`chat_error_kind`] first.
    pub fn on_response_err(&mut self, err: &str) {
        self.history.push(ChatMessage::error(err.to_string()));
        self.clear_stream_state();
    }

    /// Drain the pending reply channel and any in-flight stream poll.
    /// Called once per paint before rendering so the UI always reflects
    /// the freshest server state.
    fn drain_reply(&mut self) {
        if let Some(rx) = self.pending.as_mut() {
            match live::try_recv_reply(rx) {
                live::TryReply::Done(Ok(value)) => self.on_response_ok(&value),
                live::TryReply::Done(Err(err)) => self.on_response_err(&err),
                live::TryReply::Closed => {
                    self.on_response_err("transport closed before reply");
                }
                live::TryReply::Empty => { /* still in flight */ }
            }
        }
        // Drain a completed llm.models fetch (WEFT-256).
        let models_result = self.models_poll.as_mut().map(live::try_recv_reply);
        match models_result {
            Some(live::TryReply::Done(Ok(value))) => {
                self.models_poll = None;
                self.on_models_ok(&value);
            }
            Some(live::TryReply::Done(Err(err))) => {
                self.models_poll = None;
                self.on_models_err(&err);
            }
            Some(live::TryReply::Closed) => {
                self.models_poll = None;
                self.on_models_err("transport closed before models reply");
            }
            Some(live::TryReply::Empty) | None => {}
        }
        // Drain a completed stream-path substrate.read without holding
        // `self.stream_poll` across the `on_stream_frame` borrow.
        let stream_result = self.stream_poll.as_mut().map(live::try_recv_reply);
        match stream_result {
            Some(live::TryReply::Done(Ok(value))) => {
                self.stream_poll = None;
                // substrate.read returns `{ value, tick, sensitivity }`.
                // The progressive frame lives under `value`.
                if let Some(frame) = value.get("value").filter(|v| !v.is_null()) {
                    self.on_stream_frame(frame);
                }
            }
            Some(live::TryReply::Done(Err(_))) | Some(live::TryReply::Closed) => {
                // Transient read failure — drop the slot so the next
                // paint re-submits. Don't surface as a chat error.
                self.stream_poll = None;
            }
            Some(live::TryReply::Empty) | None => {}
        }
    }

    /// Fire `llm.models` once (or retry after failure) so the chip
    /// strip has a catalog (WEFT-256). Idempotent while a poll is
    /// outstanding or a successful live catalog is already cached.
    fn poll_models_if_needed(&mut self, live: &Arc<Live>) {
        if self.models_poll.is_some() {
            return;
        }
        // Live catalog cache hit — do not re-fetch every paint.
        if self.models_catalog_live && self.models_error.is_none() {
            return;
        }
        let now = live::now_ms();
        if self.last_models_poll_ms > 0.0
            && (now - self.last_models_poll_ms) < MODELS_RETRY_INTERVAL_MS
        {
            return;
        }
        let (tx, rx) = live::reply_channel();
        self.models_poll = Some(rx);
        self.last_models_poll_ms = now;
        live.submit(Command::Raw {
            method: "llm.models".into(),
            params: serde_json::json!({}),
            reply: Some(tx),
        });
    }

    /// While a turn is in flight, keep the progressive stream path
    /// warm via `substrate.read`. Throttled and at-most-one outstanding
    /// so we don't starve the raw-RPC slot during long turns.
    fn poll_stream_if_needed(&mut self, live: &Arc<Live>) {
        if !self.is_in_flight() {
            return;
        }
        if self.stream_poll.is_some() {
            return;
        }
        let Some(path) = self.stream_path() else {
            return;
        };
        let now = live::now_ms();
        if self.last_stream_poll_ms > 0.0
            && (now - self.last_stream_poll_ms) < STREAM_POLL_INTERVAL_MS
        {
            return;
        }
        // Prefer any frame already mirrored into the local substrate
        // snapshot (native adapter path / prior relay) before spending
        // an RPC. Cheap and keeps the typewriter smooth at 60 fps when
        // the path is already warm.
        if let Some(frame) = live.substrate_snapshot().get(&path).cloned() {
            self.on_stream_frame(&frame);
        }
        let (tx, rx) = live::reply_channel();
        self.stream_poll = Some(rx);
        self.last_stream_poll_ms = now;
        live.submit(Command::Raw {
            method: "substrate.read".into(),
            params: serde_json::json!({ "path": path }),
            reply: Some(tx),
        });
    }

    /// Mint a stable conversation id for this panel session. Idempotent
    /// — once set, subsequent calls reuse the same id so the daemon's
    /// per-conv `AgentService` state lines up across turns and the
    /// substrate heartbeat path stays stable for WEFT-257's label.
    ///
    /// Format mirrors `clawft_types::agent_chat::default_conv_id` —
    /// `panel-<ts_ms>-<rng>` — so it's visually distinct from the
    /// daemon's auto-minted ephemeral ids in logs and substrate paths.
    fn ensure_conv_id(&mut self) -> &str {
        if self.conv_id.is_none() {
            // `now_ms` is monotonic-from-app-start on native and
            // performance.now() on wasm — good enough for a panel-local
            // id. We don't need cross-process uniqueness; the daemon
            // namespaces by node_id under the hood.
            let ts = live::now_ms() as u64;
            // Cheap pseudo-random suffix from the address of `self` —
            // collisions across panel sessions are inert (the daemon
            // treats each as a fresh conv) and we avoid adding `rand`
            // to this crate's wasm bundle just for this.
            let salt = (self as *const _ as usize) as u64 & 0xFFFF;
            self.conv_id = Some(format!("panel-{ts:013}-{salt:04x}"));
        }
        // Safe: the `if` above ensures `Some`.
        self.conv_id.as_deref().expect("conv_id minted")
    }

    /// Substrate path for this conversation's heartbeat / status frame,
    /// or `None` if no conv_id has been minted yet (i.e. no turn has
    /// been submitted in this panel lifetime). Used by the WEFT-257
    /// heartbeat label.
    pub fn heartbeat_path(&self) -> Option<String> {
        self.conv_id
            .as_deref()
            .map(|id| format!("substrate/_derived/chat/{id}/status"))
    }

    /// Submit the current `draft` as a user turn and fire the RPC.
    /// No-op if the draft is empty/whitespace or a request is already
    /// in flight.
    fn submit_draft(&mut self, live: &Arc<Live>) {
        if self.is_in_flight() {
            return;
        }
        let text = self.draft.trim().to_string();
        if text.is_empty() {
            return;
        }
        // Mint conv_id before serialising params so the request carries
        // the stable id and the substrate heartbeat path is well-known
        // for the in-flight render (WEFT-257).
        self.ensure_conv_id();
        let params = self.build_request_params(&text);
        self.history.push(ChatMessage::user(text));
        self.draft.clear();

        // Reset streaming draft so a previous turn's partial text
        // never bleeds into this one.
        self.streaming_draft = None;
        self.stream_phase = Some("thinking".into());
        self.stream_seq = 0;
        self.stream_poll = None;
        self.last_stream_poll_ms = 0.0;

        let (tx, rx) = live::reply_channel();
        self.pending = Some(rx);
        // WEFT-253: prefer agent.chat_stream so progressive frames land
        // on the substrate stream path while the turn runs. Final
        // response shape is identical to agent.chat (plus optional
        // stream_path / streamed echo fields the panel ignores).
        live.submit(Command::Raw {
            method: "agent.chat_stream".into(),
            params,
            reply: Some(tx),
        });
    }
}

// ── WEFT-254 multi-conversation panel ───────────────────────────────

/// One in-memory conversation slot for the multi-conversation sidebar.
///
/// Holds a full [`ChatView`] (history, stream state, pending RPC) plus
/// sidebar metadata. Persistence is panel-lifetime only; substrate
/// rehydrate is a follow-up.
#[derive(Default)]
pub struct ChatSession {
    /// Always-present local key for egui widget ids and the sidebar
    /// row before the daemon `conv_id` is minted on first submit.
    local_id: String,
    /// `live::now_ms()` when the slot was created.
    created_ms: f64,
    /// `live::now_ms()` of the last user submit or completed turn.
    last_active_ms: f64,
    /// Per-conversation UI + wire state machine.
    view: ChatView,
}

impl ChatSession {
    fn fresh() -> Self {
        let now = live::now_ms();
        Self {
            local_id: mint_local_session_id(),
            created_ms: now,
            last_active_ms: now,
            view: ChatView::default(),
        }
    }

    /// Id shown in the sidebar: daemon `conv_id` once minted, else the
    /// local draft key.
    pub fn display_id(&self) -> &str {
        self.view
            .conv_id
            .as_deref()
            .unwrap_or(self.local_id.as_str())
    }

    /// First user-message snippet for the sidebar row, or a placeholder
    /// when the conversation has no user turns yet.
    pub fn first_snippet(&self) -> String {
        self.view
            .history
            .iter()
            .find(|m| m.role == "user")
            .map(|m| truncate_snippet(&m.content, SNIPPET_MAX_CHARS))
            .unwrap_or_else(|| "New conversation".into())
    }

    /// Last-active wall clock from panel epoch (`live::now_ms`).
    pub fn last_active_ms(&self) -> f64 {
        self.last_active_ms
    }

    /// Created-at wall clock from panel epoch.
    pub fn created_ms(&self) -> f64 {
        self.created_ms
    }

    /// Borrow the underlying single-conversation view.
    pub fn view(&self) -> &ChatView {
        &self.view
    }

    fn touch(&mut self) {
        self.last_active_ms = live::now_ms();
    }
}

/// Multi-conversation chat panel (WEFT-254).
///
/// Owns one or more [`ChatSession`] slots in memory. The active session
/// drives the main transcript; the left strip lists every slot so the
/// user can switch concurrent threads or revisit earlier history without
/// losing in-flight stream state on other slots.
pub struct ChatPanel {
    sessions: Vec<ChatSession>,
    active: usize,
    /// WEFT-284 — parallel agent columns. Visible when
    /// [`ThreadDockState::is_parallel`] (len ≥ 2). Independent of the
    /// multi-conversation sidebar (WEFT-254): sessions are alternate
    /// chats; threads are concurrent agents on one swarm turn.
    thread_dock: ThreadDockState,
}

impl Default for ChatPanel {
    fn default() -> Self {
        Self::new()
    }
}

impl ChatPanel {
    /// Panel with a single empty conversation selected.
    pub fn new() -> Self {
        Self {
            sessions: vec![ChatSession::fresh()],
            active: 0,
            thread_dock: ThreadDockState::new(),
        }
    }

    /// WEFT-284 — parallel agent thread dock (read-only).
    pub fn thread_dock(&self) -> &ThreadDockState {
        &self.thread_dock
    }

    /// WEFT-284 — parallel agent thread dock (mutable host/tests).
    pub fn thread_dock_mut(&mut self) -> &mut ThreadDockState {
        &mut self.thread_dock
    }

    /// Attach or replace an agent column. Returns the column index.
    ///
    /// When ≥2 columns are present, [`paint`](paint) shows the
    /// ThreadDock above the active conversation transcript.
    pub fn upsert_agent_thread(&mut self, thread: AgentThread) -> usize {
        if let Some(idx) = self
            .thread_dock
            .threads()
            .iter()
            .position(|t| t.id == thread.id)
        {
            self.thread_dock.threads_mut()[idx] = thread;
            idx
        } else {
            self.thread_dock.push(thread)
        }
    }

    /// Feed an accumulated stream draft into a named agent column
    /// (WEFT-253 frame shape → ThreadDock column). Sets phase to
    /// Streaming when text is non-empty.
    pub fn feed_agent_stream(&mut self, id: &str, text: &str, phase: Option<ThreadPhase>) -> bool {
        let ok = self.thread_dock.set_stream_text(id, text);
        if !ok {
            return false;
        }
        let p = phase.unwrap_or(if text.is_empty() {
            ThreadPhase::Thinking
        } else {
            ThreadPhase::Streaming
        });
        let _ = self.thread_dock.set_phase(id, p);
        true
    }

    /// True when the ThreadDock should paint (≥2 agent columns).
    pub fn shows_thread_dock(&self) -> bool {
        self.thread_dock.is_parallel()
    }

    /// Drop all agent columns (returns to classic single-stream chat).
    pub fn clear_agent_threads(&mut self) {
        self.thread_dock.clear();
    }

    /// Number of in-memory conversation slots.
    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    /// True when no sessions exist (should not happen after [`Self::new`]).
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// Index of the active conversation.
    pub fn active_index(&self) -> usize {
        self.active
    }

    /// All sessions in creation order (sidebar lists newest-active
    /// first at paint time without mutating this order).
    pub fn sessions(&self) -> &[ChatSession] {
        &self.sessions
    }

    /// Active session view (read-only).
    pub fn active_view(&self) -> &ChatView {
        &self.sessions[self.active].view
    }

    /// Active session view (mutable) — for tests and host plumbing.
    pub fn active_view_mut(&mut self) -> &mut ChatView {
        &mut self.sessions[self.active].view
    }

    /// Create a fresh conversation and select it. Returns the new index.
    pub fn new_conversation(&mut self) -> usize {
        self.sessions.push(ChatSession::fresh());
        self.active = self.sessions.len() - 1;
        self.active
    }

    /// Select conversation `idx` if in range. No-op on OOB.
    pub fn select(&mut self, idx: usize) {
        if idx < self.sessions.len() {
            self.active = idx;
        }
    }

    /// Drain pending RPC replies + stream polls for **every** session
    /// so a background turn that finishes while another is selected
    /// still lands in that session's history.
    fn drain_all(&mut self, live: &Arc<Live>) {
        for session in &mut self.sessions {
            let hist_before = session.view.history.len();
            let inflight_before = session.view.is_in_flight();
            session.view.drain_reply();
            if session.view.is_in_flight() {
                session.view.poll_stream_if_needed(live);
            }
            // Touch last-active when a turn completes or history grows.
            if session.view.history.len() != hist_before
                || (inflight_before && !session.view.is_in_flight())
            {
                session.touch();
            }
        }
        // Defensive: never leave active out of range.
        if self.active >= self.sessions.len() {
            self.active = self.sessions.len().saturating_sub(1);
        }
    }
}

/// Mint a local session key (distinct from daemon `panel-…` conv ids).
fn mint_local_session_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let ts = live::now_ms() as u64;
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("local-{ts:013}-{n:04}")
}

/// Truncate a user message for the sidebar snippet line.
fn truncate_snippet(s: &str, max_chars: usize) -> String {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return "New conversation".into();
    }
    let mut out = String::new();
    for (i, ch) in trimmed.chars().enumerate() {
        if i >= max_chars {
            out.push('…');
            break;
        }
        // Collapse internal newlines so the sidebar row stays one line.
        if ch == '\n' || ch == '\r' {
            out.push(' ');
        } else {
            out.push(ch);
        }
    }
    out
}

/// Human-readable age from a panel-epoch timestamp (`live::now_ms`).
fn format_age_label(last_active_ms: f64) -> String {
    let age_ms = (live::now_ms() - last_active_ms).max(0.0);
    let secs = (age_ms / 1000.0) as u64;
    if secs < 5 {
        "just now".into()
    } else if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

/// Render the chat panel. `path` is the substrate sentinel path (used
/// only for the muted footer hint). `value` is the sentinel value
/// (used only to surface the model name).
///
/// Layout (WEFT-254): left conversation list + right active transcript.
pub fn paint(
    ui: &mut egui::Ui,
    path: &str,
    value: &Value,
    panel: &mut ChatPanel,
    live: &Arc<Live>,
) {
    panel.drain_all(live);

    ui.horizontal_top(|ui| {
        // ── conversation sidebar ──────────────────────────────────
        ui.allocate_ui_with_layout(
            egui::vec2(CONV_SIDEBAR_WIDTH, ui.available_height()),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                paint_conversation_sidebar(ui, panel);
            },
        );

        ui.separator();

        // ── active conversation body ──────────────────────────────
        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), ui.available_height()),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                paint_active_conversation(ui, path, value, panel, live);
            },
        );
    });
}

/// Left strip: New + scrollable session rows (id, age, snippet).
fn paint_conversation_sidebar(ui: &mut egui::Ui, panel: &mut ChatPanel) {
    ui.set_min_width(CONV_SIDEBAR_WIDTH - 8.0);
    ui.label(
        egui::RichText::new("Conversations")
            .small()
            .strong()
            .color(crate::theming::palette::CG_EDGE_DEFAULT),
    );
    ui.add_space(4.0);

    if ui
        .add_sized(
            [ui.available_width(), 28.0],
            egui::Button::new(
                egui::RichText::new("+ New conversation")
                    .small()
                    .color(crate::theming::palette::TEXT_ACTION_BLUE),
            ),
        )
        .clicked()
    {
        panel.new_conversation();
    }

    ui.add_space(4.0);
    ui.separator();
    ui.add_space(2.0);

    // Paint order: most-recently-active first, without reordering the
    // underlying vec (select still uses stable indices).
    let mut order: Vec<usize> = (0..panel.sessions.len()).collect();
    order.sort_by(|&a, &b| {
        panel.sessions[b]
            .last_active_ms
            .partial_cmp(&panel.sessions[a].last_active_ms)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let active = panel.active;
    let mut clicked: Option<usize> = None;

    egui::ScrollArea::vertical()
        .id_salt("chat-conv-sidebar-scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for idx in order {
                let session = &panel.sessions[idx];
                let selected = idx == active;
                let id_label = session.display_id();
                // Shorten long panel-/local- ids for the row title.
                let id_short = if id_label.len() > 22 {
                    format!("{}…", &id_label[..20])
                } else {
                    id_label.to_owned()
                };
                let age = format_age_label(session.last_active_ms);
                let snippet = session.first_snippet();
                let inflight = session.view.is_in_flight();

                let fill = if selected {
                    crate::theming::palette::ROW_SELECTED_FILL
                } else {
                    crate::theming::palette::ROW_IDLE_FILL
                };
                let stroke = if selected {
                    egui::Stroke::new(1.0, crate::theming::palette::ROW_SELECTED_STROKE)
                } else {
                    egui::Stroke::new(1.0, crate::theming::palette::ROW_IDLE_STROKE)
                };

                let frame = egui::Frame::new()
                    .fill(fill)
                    .stroke(stroke)
                    .inner_margin(egui::Margin::symmetric(8, 6))
                    .corner_radius(4.0);

                let resp = frame
                    .show(ui, |ui| {
                        ui.set_min_width(ui.available_width());
                        ui.horizontal(|ui| {
                            if inflight {
                                ui.label(
                                    egui::RichText::new("●")
                                        .small()
                                        .color(crate::theming::palette::TEXT_LIVE_DOT),
                                );
                            } else if selected {
                                ui.label(
                                    egui::RichText::new("▸")
                                        .small()
                                        .color(crate::theming::palette::TEXT_SELECTED_MARK),
                                );
                            } else {
                                ui.label(
                                    egui::RichText::new(" ")
                                        .small()
                                        .color(egui::Color32::TRANSPARENT),
                                );
                            }
                            ui.label(
                                egui::RichText::new(id_short)
                                    .small()
                                    .monospace()
                                    .strong()
                                    .color(if selected {
                                        crate::theming::palette::ROW_SELECTED_TEXT
                                    } else {
                                        crate::theming::palette::ROW_IDLE_TEXT
                                    }),
                            );
                        });
                        ui.label(
                            egui::RichText::new(&age)
                                .small()
                                .italics()
                                .color(crate::theming::palette::TEXT_META_FAINT),
                        );
                        ui.label(
                            egui::RichText::new(&snippet)
                                .small()
                                .color(crate::theming::palette::TEXT_SNIPPET),
                        );
                    })
                    .response
                    .interact(egui::Sense::click())
                    .on_hover_text(format!("{id_label}\n{age}"));

                if resp.clicked() {
                    clicked = Some(idx);
                }
                ui.add_space(3.0);
            }
        });

    if let Some(idx) = clicked {
        panel.select(idx);
    }
}

/// Right pane: model chip, history, input for the active session.
fn paint_active_conversation(
    ui: &mut egui::Ui,
    path: &str,
    value: &Value,
    panel: &mut ChatPanel,
    live: &Arc<Live>,
) {
    // WEFT-284: when ≥2 agent columns are attached, paint the
    // column-per-thread dock above the classic transcript so parallel
    // agent streams never collapse into one scroll.
    if panel.shows_thread_dock() {
        ui.label(
            egui::RichText::new("parallel agents")
                .small()
                .color(crate::theming::palette::TEXT_SECTION_BLUE),
        );
        let _ = ThreadDock::new("chat.thread_dock", &mut panel.thread_dock)
            .min_height(140.0)
            .max_height(200.0)
            .show_close(true)
            .tooltip("ThreadDock — per-agent parallel output (WEFT-284)")
            .show(ui);
        ui.add_space(6.0);
        ui.separator();
        ui.add_space(4.0);
    }

    // Pull session-local bits we need for labels before mutably
    // borrowing the view for the rest of the body.
    let session_id_label = panel.sessions[panel.active].display_id().to_owned();
    let system_salt = panel.sessions[panel.active].local_id.clone();

    // Scoped mut borrow of the active ChatView for the remainder.
    let session = &mut panel.sessions[panel.active];
    let view = &mut session.view;

    // WEFT-256: restore / seed model selection and refresh catalog.
    restore_model_selection(ui, view);
    seed_model_from_sentinel(view, value);
    view.poll_models_if_needed(live);

    let model = view
        .selected_model()
        .or_else(|| {
            value
                .as_object()
                .and_then(|o| o.get("model"))
                .and_then(Value::as_str)
        })
        .unwrap_or("local");

    ui.label(
        egui::RichText::new(format!("chat · {model}"))
            .color(crate::theming::palette::TEXT_SUBTLE)
            .small(),
    );
    ui.add_space(2.0);
    ui.horizontal(|ui| {
        ui.heading("Local LLM");
        ui.label(
            egui::RichText::new(format!("· {session_id_label}"))
                .small()
                .monospace()
                .color(crate::theming::palette::TEXT_SESSION_ID),
        );
    });
    ui.add_space(4.0);
    // WEFT-256: model / provider chip strip.
    paint_model_chip_strip(ui, view);
    persist_model_selection(ui, view);
    ui.add_space(2.0);

    // WEFT-259: identity-drift / binding-thread mismatch warning. Lives
    // above the input so the user always sees it before composing the
    // next turn — not buried under the scroll area.
    paint_identity_warning(ui, view);

    // WEFT-255: system-prompt affordance. Collapsible header above the
    // history so the persona/instructions can be tweaked without
    // crowding the message field. Initial state is collapsed.
    paint_system_editor(ui, view, &system_salt);

    // Reserve room for everything below the history so the input row and
    // Send button stay inside the window: multiline input (3 rows), the
    // Send/Clear row, the spacers, separator and path footer, plus the
    // history frame's own margin and stroke.
    let history_h = (ui.available_height() - CHAT_BELOW_HISTORY_H).max(MIN_HISTORY_H);

    // WEFT-252: paint_history needs `&mut view` (CommonMarkCache lives
    // there). Splitting the closure body into a free fn keeps borrow
    // shapes clean.
    egui::Frame::group(ui.style())
        .inner_margin(egui::Margin::same(6))
        .show(ui, |ui| {
            egui::ScrollArea::vertical()
                .id_salt(format!("chat-history-{system_salt}"))
                .auto_shrink([false, false])
                .stick_to_bottom(true)
                .max_height(history_h)
                .show(ui, |ui| {
                    paint_history(ui, view);
                });
        });

    ui.add_space(6.0);

    // WEFT-257: heartbeat label replaces the in-history spinner. Reads
    // `substrate/_derived/chat/<conv_id>/status` and surfaces the
    // status string + age in seconds. Shown only while a request is
    // in flight (otherwise stale status would mislead the user).
    if view.is_in_flight() {
        paint_heartbeat(ui, view, live);
    }

    // Input row. Disabled while a request is in flight; Enter submits,
    // Shift+Enter inserts a newline (egui's default for multiline +
    // explicit `desired_rows`).
    let mut did_submit = false;
    ui.add_enabled_ui(!view.is_in_flight(), |ui| {
        let response = ui.add(
            egui::TextEdit::multiline(&mut view.draft)
                .id_salt(format!("chat-draft-{system_salt}"))
                .desired_rows(3)
                .desired_width(f32::INFINITY)
                .hint_text("Type a message — Enter to send, Shift+Enter for newline"),
        );

        let enter_pressed = response.has_focus()
            && ui.input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.shift);

        ui.horizontal(|ui| {
            let send_clicked = ui.button("Send").clicked();
            if (send_clicked || enter_pressed) && !view.draft.trim().is_empty() {
                view.submit_draft(live);
                did_submit = true;
            }
            if !view.history.is_empty() && ui.button("Clear").clicked() && !view.is_in_flight() {
                view.history.clear();
            }
        });
    });
    if did_submit {
        session.touch();
    }

    ui.add_space(4.0);
    ui.separator();
    ui.label(
        egui::RichText::new(format!("path: {path}"))
            .small()
            .monospace()
            .color(crate::theming::palette::TEXT_LABEL_FAINT),
    );
}

/// Paint the conversation history. Split out from [`paint`] so the
/// `CommonMarkCache` borrow on `view` stays scoped to a single call.
fn paint_history(ui: &mut egui::Ui, view: &mut ChatView) {
    if view.history.is_empty() {
        ui.vertical_centered(|ui| {
            ui.add_space(24.0);
            ui.label(
                egui::RichText::new("(no messages yet — type below to start)")
                    .italics()
                    .color(crate::theming::palette::TEXT_SUBTLE),
            );
        });
        return;
    }
    // Index-loop so each iteration can re-borrow `view.cache` mutably
    // for assistant turns without aliasing `view.history`. Cloning the
    // role + content strings is cheap (small per turn) and avoids the
    // borrow-checker gymnastics of holding a `&ChatMessage` across the
    // CommonMarkViewer call.
    let len = view.history.len();
    for i in 0..len {
        let (role, content) = {
            let m = &view.history[i];
            (m.role.clone(), m.content.clone())
        };
        let msg = ChatMessage { role, content };
        paint_bubble(ui, &msg, &mut view.cache);
    }
    // WEFT-253: live streaming draft bubble. Grows as stream frames
    // arrive; replaced by the committed assistant history entry when
    // the final RPC reply lands (`on_response_ok` clears the draft).
    if let Some(draft) = view.streaming_draft.clone() {
        let msg = ChatMessage::assistant(draft);
        paint_bubble(ui, &msg, &mut view.cache);
        // Subtle caret so the user can tell this bubble is still
        // growing (vs a finished assistant turn above).
        ui.label(
            egui::RichText::new("▍")
                .small()
                .color(crate::theming::palette::TEXT_CURSOR),
        );
    }
}

/// Restore [`ChatView::selected_model`] from egui memory when the
/// panel state was reset (WEFT-256 persistence across panel reload /
/// re-select within the host session).
fn restore_model_selection(ui: &mut egui::Ui, view: &mut ChatView) {
    if view.selected_model.is_some() {
        return;
    }
    let id = egui::Id::new(MODEL_SELECTION_MEM_ID);
    if let Some(stored) = ui.ctx().data_mut(|d| d.get_persisted::<String>(id))
        && !stored.trim().is_empty()
    {
        view.selected_model = Some(stored);
    }
}

/// Persist the current selection into egui memory so a panel reload
/// (or a second Chat surface in the same process) reuses it.
fn persist_model_selection(ui: &mut egui::Ui, view: &ChatView) {
    if let Some(model) = view.selected_model.as_ref() {
        let id = egui::Id::new(MODEL_SELECTION_MEM_ID);
        ui.ctx().data_mut(|d| d.insert_persisted(id, model.clone()));
    }
}

/// Seed selection + a single fallback chip from the substrate
/// sentinel's `model` field when the catalog hasn't arrived yet.
fn seed_model_from_sentinel(view: &mut ChatView, value: &Value) {
    let sentinel_model = value
        .as_object()
        .and_then(|o| o.get("model"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let Some(model) = sentinel_model else {
        return;
    };
    if view.selected_model.is_none() {
        view.selected_model = Some(model.to_owned());
    }
    if view.available_models.is_empty() {
        view.available_models.push(ModelChip {
            id: model.to_owned(),
            provider: "local".into(),
            label: model.to_owned(),
            is_default: true,
        });
    }
}

/// Paint the WEFT-256 model / provider chip strip.
///
/// Chips are selectable; the active chip is filled. A muted hint shows
/// when `llm.models` is still loading or failed (catalog may still
/// show the sentinel fallback).
fn paint_model_chip_strip(ui: &mut egui::Ui, view: &mut ChatView) {
    let chips: Vec<ModelChip> = if view.available_models.is_empty() {
        // Extremely early paint before seed/fetch — show a placeholder
        // chip so the strip always occupies space.
        vec![ModelChip {
            id: view
                .selected_model
                .clone()
                .unwrap_or_else(|| "local".into()),
            provider: "local".into(),
            label: view
                .selected_model
                .clone()
                .unwrap_or_else(|| "local".into()),
            is_default: true,
        }]
    } else {
        view.available_models.clone()
    };

    ui.horizontal_wrapped(|ui| {
        ui.label(
            egui::RichText::new("model")
                .small()
                .color(crate::theming::palette::TEXT_LABEL_FAINT),
        );
        for chip in &chips {
            let selected = view.selected_model.as_deref() == Some(chip.id.as_str());
            let label = if chip.provider.is_empty() || chip.provider == "local" {
                chip.label.clone()
            } else {
                format!("{} · {}", chip.provider, chip.label)
            };
            let fill = if selected {
                crate::theming::palette::CHIP_ON_FILL
            } else {
                crate::theming::palette::CHIP_OFF_FILL
            };
            let stroke = if selected {
                egui::Stroke::new(1.0, crate::theming::palette::CHIP_ON_STROKE)
            } else {
                egui::Stroke::new(1.0, crate::theming::palette::CHIP_OFF_STROKE)
            };
            let text_color = if selected {
                crate::theming::palette::CHIP_ON_TEXT
            } else {
                crate::theming::palette::CHIP_OFF_TEXT
            };
            // Allocate a clickable chip rect, then paint into it.
            let font = egui::FontId::proportional(11.0);
            let text_size = ui.fonts_mut(|f| {
                let galley = f.layout_no_wrap(label.clone(), font.clone(), text_color);
                galley.size()
            });
            let pad = egui::vec2(16.0, 6.0);
            let desired = text_size + pad;
            let (rect, response) = ui.allocate_exact_size(desired, egui::Sense::click());
            if ui.is_rect_visible(rect) {
                let painter = ui.painter();
                painter.rect(rect, 10.0, fill, stroke, egui::StrokeKind::Inside);
                painter.text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    &label,
                    font,
                    text_color,
                );
            }
            let response = response.on_hover_text(format!(
                "provider: {}{}\nclick to select for the next turn",
                chip.provider,
                if chip.is_default { " (default)" } else { "" }
            ));
            if response.clicked() && !view.is_in_flight() {
                view.select_model(chip.id.clone());
                persist_model_selection(ui, view);
            }
        }
        if view.models_poll.is_some() {
            ui.label(
                egui::RichText::new("loading…")
                    .small()
                    .italics()
                    .color(crate::theming::palette::TEXT_HINT_FAINT),
            );
        } else if let Some(err) = view.models_error.as_deref() {
            ui.label(
                egui::RichText::new(format!("catalog: {err}"))
                    .small()
                    .color(crate::theming::palette::TEXT_WARN_MUTED),
            )
            .on_hover_text("llm.models probe failed; using last known / sentinel catalog");
        }
    });
    // Keep memory warm even when selection came from restore/seed.
    persist_model_selection(ui, view);
    ui.add_space(4.0);
}

/// Paint the WEFT-255 system-prompt editor. Collapsible so it doesn't
/// crowd the panel when the user just wants to chat. Edits flow into
/// `view.system`; an empty/whitespace string is treated as `None` on
/// the wire (see [`ChatView::build_request_params`]).
///
/// `id_salt` is the session `local_id` so multi-conversation panels
/// don't share collapsible open-state across sessions (WEFT-254).
fn paint_system_editor(ui: &mut egui::Ui, view: &mut ChatView, id_salt: &str) {
    egui::CollapsingHeader::new(
        egui::RichText::new("system prompt (optional)")
            .small()
            .color(crate::theming::palette::TEXT_DISCLOSURE),
    )
    .id_salt(format!("chat-system-prompt-{id_salt}"))
    .default_open(view.system_expanded)
    .show(ui, |ui| {
        view.system_expanded = true;
        let mut text = view.system.clone().unwrap_or_default();
        let resp = ui.add(
            egui::TextEdit::multiline(&mut text)
                .id_salt(format!("chat-system-text-{id_salt}"))
                .desired_rows(2)
                .desired_width(f32::INFINITY)
                .hint_text("Extra context layered on top of the workspace identity prompt."),
        );
        if resp.changed() {
            view.system = if text.trim().is_empty() {
                None
            } else {
                Some(text)
            };
        }
    });
    ui.add_space(4.0);
}

/// Paint the WEFT-259 identity-drift warning chip. No-op when the most
/// recent response carried a recognised stable `identity_source`.
fn paint_identity_warning(ui: &mut egui::Ui, view: &ChatView) {
    let Some(source) = view.identity_warning() else {
        return;
    };
    egui::Frame::new()
        .fill(crate::theming::palette::DRIFT_BANNER_FILL)
        .inner_margin(egui::Margin::symmetric(8, 4))
        .corner_radius(4.0)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("identity drift")
                        .strong()
                        .color(crate::theming::palette::DRIFT_TITLE),
                );
                ui.label(
                    egui::RichText::new(format!("source = `{source}`"))
                        .monospace()
                        .small()
                        .color(crate::theming::palette::DRIFT_SOURCE),
                );
            });
            ui.label(
                egui::RichText::new(
                    "Workspace identity not loaded from the canonical source. \
                     Conversations may use a fallback persona; check the \
                     daemon logs and `IDENTITY.md`.",
                )
                .small()
                .color(crate::theming::palette::DRIFT_BODY),
            );
        });
    ui.add_space(4.0);
}

/// Paint the WEFT-257 heartbeat label below the input. Reads the
/// substrate status frame at `substrate/_derived/chat/<conv_id>/status`
/// (a `{ status, payload, ts_ms }` blob written by the daemon's
/// `SubstrateConversationSink`) and renders the status word plus an
/// age in seconds since the last heartbeat tick.
///
/// Falls back to a neutral "waiting…" label when the status frame
/// isn't published yet (very first turn, or daemon-side sink not
/// wired) so the user always sees *something* moving in the in-flight
/// window.
fn paint_heartbeat(ui: &mut egui::Ui, view: &ChatView, live: &Arc<Live>) {
    let snap = live.substrate_snapshot();
    let frame = view
        .heartbeat_path()
        .as_deref()
        .and_then(|p| snap.get(p).cloned());

    // Prefer the progressive stream phase (WEFT-253) when present —
    // "generating" / "thinking" is more informative than a generic
    // heartbeat "alive". Fall back to the status-frame word, then to
    // "waiting".
    let stream_phase = view.stream_phase().map(str::to_owned);
    let (status_word, age_label) = match frame.as_ref() {
        Some(v) => {
            let status = stream_phase.clone().unwrap_or_else(|| {
                v.get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("alive")
                    .to_owned()
            });
            let age = v
                .get("ts_ms")
                .and_then(Value::as_f64)
                .map(|ts| (live::now_ms() - ts).max(0.0) / 1000.0)
                .map(|secs| format!("{secs:.1}s ago"))
                .unwrap_or_default();
            (status, age)
        }
        None => (
            stream_phase.unwrap_or_else(|| "waiting".into()),
            String::new(),
        ),
    };

    let progress_hint = if view.streaming_draft().is_some() {
        "streaming…"
    } else {
        "waiting for completion…"
    };

    ui.horizontal(|ui| {
        // Subtle pulsing dot replaces the spinner — it's an unambiguous
        // "we're alive" signal but doesn't grab the eye the way a
        // spinning gear does. Hue cycles through a narrow blue-green
        // band per second so a stalled paint loop becomes visible.
        let t = (live::now_ms() / 1000.0).fract() as f32;
        let pulse = 120 + ((1.0 - t) * 80.0) as u8;
        let dot_color = crate::theming::palette::heartbeat_dot(pulse);
        let (rect, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
        ui.painter().circle_filled(rect.center(), 4.0, dot_color);

        ui.label(
            egui::RichText::new(format!("status: {status_word}"))
                .small()
                .color(crate::theming::palette::TEXT_STATUS),
        );
        if !age_label.is_empty() {
            ui.label(
                egui::RichText::new(format!("({age_label})"))
                    .small()
                    .italics()
                    .color(crate::theming::palette::TEXT_HINT_FAINT),
            );
        }
        ui.label(
            egui::RichText::new(progress_hint)
                .small()
                .italics()
                .color(crate::theming::palette::TEXT_PROGRESS),
        );
    });
    // Request a repaint at the stream-poll cadence so the typewriter
    // draft and age counter tick even when the user isn't moving the
    // mouse. egui only repaints on input by default — without this,
    // the label stalls and tokens freeze on screen.
    ui.ctx()
        .request_repaint_after(std::time::Duration::from_millis(100));
    ui.add_space(2.0);
}

/// Paint one history entry as a chat bubble. Roles render distinctly:
///
/// - `system`   — dim italic, full width (informational; not a turn).
/// - `user`     — right-aligned, subtle bg, plain text.
/// - `assistant`— left-aligned, subtle bg, **markdown** via
///   `egui_commonmark` (WEFT-252) so code blocks / lists / headers
///   render correctly. The cache is supplied by the caller so the
///   parsed AST is reused across paints rather than re-parsed each
///   frame.
/// - `error`    — left-aligned, red bg + red strong text. UI-only role.
fn paint_bubble(ui: &mut egui::Ui, msg: &ChatMessage, md_cache: &mut CommonMarkCache) {
    match msg.role.as_str() {
        "system" => {
            ui.label(
                egui::RichText::new(format!("[system] {}", msg.content))
                    .italics()
                    .small()
                    .color(crate::theming::palette::TEXT_SYSTEM_MSG),
            );
            ui.add_space(2.0);
        }
        "user" => {
            ui.with_layout(egui::Layout::top_down(egui::Align::RIGHT), |ui| {
                egui::Frame::new()
                    .fill(crate::theming::palette::BUBBLE_USER_FILL)
                    .inner_margin(egui::Margin::symmetric(8, 4))
                    .corner_radius(6.0)
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new(&msg.content)
                                .color(crate::theming::palette::BUBBLE_USER_TEXT),
                        );
                    });
            });
            ui.add_space(4.0);
        }
        "assistant" => {
            ui.with_layout(egui::Layout::top_down(egui::Align::LEFT), |ui| {
                egui::Frame::new()
                    .fill(crate::theming::palette::BUBBLE_ASSISTANT_FILL)
                    .inner_margin(egui::Margin::symmetric(8, 4))
                    .corner_radius(6.0)
                    .show(ui, |ui| {
                        // WEFT-252: markdown rendering. The
                        // `id_salt` is unique-per-bubble (combined
                        // hash of content + role) so collapsible
                        // sections and copy buttons inside markdown
                        // don't share state across bubbles.
                        CommonMarkViewer::new().max_image_width(Some(480)).show(
                            ui,
                            md_cache,
                            &msg.content,
                        );
                    });
            });
            ui.add_space(4.0);
        }
        "error" => {
            ui.with_layout(egui::Layout::top_down(egui::Align::LEFT), |ui| {
                egui::Frame::new()
                    .fill(crate::theming::palette::BUBBLE_ERROR_FILL)
                    .inner_margin(egui::Margin::symmetric(8, 4))
                    .corner_radius(6.0)
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new(format!("error: {}", msg.content))
                                .strong()
                                .color(crate::theming::palette::BUBBLE_ERROR_TEXT),
                        );
                    });
            });
            ui.add_space(4.0);
        }
        other => {
            // Unknown role — render as plain text so we never silently
            // drop content if the wire shape grows.
            ui.label(format!("[{other}] {}", msg.content));
            ui.add_space(2.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn colsnap_chat_panel() {
        let live = crate::live::Live::test_stub(crate::live::Snapshot::default());
        // Conversation ids embed `now_ms()` and a global counter: compare colours only.
        crate::snap_support::render_checked_colours(
            "chat_panel",
            crate::theming::apply,
            vec![],
            || {
                let live = live.clone();
                let mut panel = ChatPanel::new();
                {
                    let v = panel.active_view_mut();
                    v.history.push(ChatMessage::user("hello there"));
                    v.history.push(ChatMessage::assistant(
                        "Hi. Here is **bold**, `code`, and a list:\n\n- one\n- two\n\n```\nfn x() {}\n```",
                    ));
                    v.history.push(ChatMessage::user("trigger an error"));
                    v.history.push(ChatMessage::error(
                        "[transport] agent.chat: daemon unreachable",
                    ));
                    v.draft = "draft text".into();
                    v.system = Some("You are a test.".into());
                }
                panel.new_conversation();
                panel
                    .active_view_mut()
                    .history
                    .push(ChatMessage::user("second thread"));
                panel.select(0);
                let sentinel = json!({ "kind": "chat", "model": "local" });
                move |ui: &mut egui::Ui| {
                    paint(ui, "substrate/chat/test", &sentinel, &mut panel, &live)
                }
            },
        );
    }

    // colsnap-states:begin
    // Interaction and streaming states of the chat panel. In-flight scenarios
    // mask the heartbeat dot (G channel comes from the wall clock); every other
    // colour is compared exactly. See docs/development/gui-colour-snapshot.md.
    fn colsnap_heartbeat_dot(hex: &str) -> bool {
        hex.starts_with("50")
            && hex.ends_with("c8ff")
            && u8::from_str_radix(&hex[2..4], 16).is_ok_and(|g| (120..=200).contains(&g))
    }

    fn colsnap_chat_prep_stream(panel: &mut ChatPanel, frame: Value) {
        let v = panel.active_view_mut();
        let (tx, rx) = live::reply_channel();
        // Keep the sender alive so the reply stays pending (turn in flight).
        std::mem::forget(tx);
        v.pending = Some(rx);
        v.on_stream_frame(&frame);
    }

    fn colsnap_chat_states(
        name: &str,
        script: &crate::snap_support::Script,
        in_flight: bool,
        prep: impl Fn(&mut ChatPanel),
    ) -> String {
        let live = crate::live::Live::test_stub(crate::live::Snapshot::default());
        let mk = || {
            let live = live.clone();
            let mut panel = ChatPanel::new();
            {
                let v = panel.active_view_mut();
                v.history.push(ChatMessage::user("hello there"));
                v.history
                    .push(ChatMessage::assistant("Hi, here is **bold** and `code`."));
                v.draft = "draft text".into();
            }
            prep(&mut panel);
            let sentinel = json!({ "kind": "chat", "model": "local" });
            move |ui: &mut egui::Ui| paint(ui, "substrate/chat/test", &sentinel, &mut panel, &live)
        };
        if in_flight {
            crate::snap_support::render_script_masked(
                name,
                crate::theming::apply,
                script,
                mk,
                colsnap_heartbeat_dot,
            )
        } else {
            crate::snap_support::render_script_colours(name, crate::theming::apply, script, mk)
        }
    }

    #[test]
    fn colsnap_state_chat_streaming() {
        use crate::snap_support::Script;
        let none = Script::new();
        let st = |n: &str, frame: Value| {
            colsnap_chat_states(&format!("state_chat_stream_{n}"), &none, true, |p| {
                colsnap_chat_prep_stream(p, frame.clone())
            })
        };
        st(
            "thinking",
            json!({ "phase": "thinking", "text": "", "seq": 1, "done": false }),
        );
        st(
            "partial_plain",
            json!({ "phase": "generating", "text": "Streaming a reply that is still", "seq": 2, "done": false }),
        );
        st(
            "partial_markdown",
            json!({ "phase": "generating", "text": "Streaming **bold** and `co", "seq": 2, "done": false }),
        );
        st(
            "partial_list",
            json!({ "phase": "generating", "text": "Steps:\n\n- first\n- second\n- thi", "seq": 3, "done": false }),
        );
        st(
            "partial_open_fence",
            json!({ "phase": "generating", "text": "Here:\n\n```rust\nfn main() {\n    println!(\"hi", "seq": 3, "done": false }),
        );
        st(
            "partial_long",
            json!({ "phase": "generating", "text": "word ".repeat(300), "seq": 4, "done": false }),
        );
        st(
            "done",
            json!({ "phase": "done", "text": "Final text.", "seq": 5, "done": true }),
        );
        st(
            "error",
            json!({ "phase": "error", "text": "partial", "seq": 2, "done": true, "error": "boom" }),
        );
    }

    #[test]
    fn colsnap_state_chat_interaction() {
        use crate::snap_support::{Script, probe, text_origin, text_target};
        let live = crate::live::Live::test_stub(crate::live::Snapshot::default());
        let base = probe(crate::theming::apply, &Script::new(), || {
            let live = live.clone();
            let mut panel = ChatPanel::new();
            {
                let v = panel.active_view_mut();
                v.history.push(ChatMessage::user("hello there"));
                v.history
                    .push(ChatMessage::assistant("Hi, here is **bold** and `code`."));
                v.draft = "draft text".into();
            }
            let sentinel = json!({ "kind": "chat", "model": "local" });
            move |ui: &mut egui::Ui| paint(ui, "substrate/chat/test", &sentinel, &mut panel, &live)
        });
        let send = text_target(&base, "\"Send\"");
        let clear = text_target(&base, "\"Clear\"");
        let draft = text_origin(&base, "draft text").expect("draft") + egui::vec2(10.0, 6.0);
        let idle =
            |n: &str, s: Script| colsnap_chat_states(&format!("state_chat_{n}"), &s, false, |_| {});
        idle("plain", Script::new());
        idle("hover_send", Script::new().hover(send));
        idle("press_send", Script::new().press(send));
        idle("hover_clear", Script::new().hover(clear));
        idle("press_clear", Script::new().press(clear));
        idle("focus_draft", Script::new().click(draft));
        idle("typed_draft", Script::new().click(draft).text("more"));
        idle(
            "select_all_draft",
            Script::new()
                .click(draft)
                .key(egui::Key::A, egui::Modifiers::COMMAND),
        );
        for n in 1..=8usize {
            let mut s = Script::new();
            for _ in 0..n {
                s = s.key(egui::Key::Tab, egui::Modifiers::NONE);
            }
            idle(&format!("tab_{n:02}"), s);
        }
        // Click Send: the draft is submitted and the turn goes in flight.
        colsnap_chat_states(
            "state_chat_click_send",
            &Script::new().click(send),
            true,
            |_| {},
        );
    }
    // colsnap-states:end

    #[test]
    fn matches_returns_positive_for_chat_sentinel() {
        let v = json!({ "kind": "chat", "model": "local" });
        assert_eq!(matches(&v), PRIORITY);
    }

    #[test]
    fn matches_returns_positive_without_model_field() {
        // Sentinel must work even if the daemon hasn't filled in the
        // optional `model` field — `kind` alone is the contract.
        let v = json!({ "kind": "chat" });
        assert_eq!(matches(&v), PRIORITY);
    }

    #[test]
    fn matches_returns_zero_for_other_shapes() {
        // Control intent — different `kind`.
        let control = json!({
            "enabled": true,
            "kind": "service",
            "target": "llm",
        });
        assert_eq!(matches(&control), 0);

        // Workshop — no `kind` at all.
        let workshop = json!({
            "title": "x",
            "panels": [],
        });
        assert_eq!(matches(&workshop), 0);

        // Plain scalars + arrays.
        assert_eq!(matches(&Value::Null), 0);
        assert_eq!(matches(&json!(42)), 0);
        assert_eq!(matches(&json!("chat")), 0);
        assert_eq!(matches(&json!([1, 2, 3])), 0);

        // String `kind` but wrong value.
        assert_eq!(matches(&json!({ "kind": "Chat" })), 0);
        assert_eq!(matches(&json!({ "kind": "chatroom" })), 0);

        // Non-string `kind`.
        assert_eq!(matches(&json!({ "kind": 7 })), 0);
    }

    #[test]
    fn priority_beats_control_toggle_and_workshop() {
        // Sanity that the dispatch order in `explorer/mod.rs` lands on
        // chat first when a value (somehow) shape-matches multiple
        // primitives.
        const { assert!(PRIORITY > 30) };
        const { assert!(PRIORITY > 25) };
    }

    #[test]
    fn serializes_messages_to_expected_wire_shape() {
        let mut view = ChatView::default();
        view.history.push(ChatMessage::user("hi"));
        view.history.push(ChatMessage::assistant("hello"));
        // Error bubbles must NOT show up on the wire.
        view.history.push(ChatMessage::error("transport blew up"));

        let params = view.build_request_params("how are you?");
        let messages = params
            .get("messages")
            .and_then(Value::as_array)
            .expect("messages key");

        assert_eq!(messages.len(), 3, "error role filtered, new user appended");
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"], "hi");
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["content"], "hello");
        assert_eq!(messages[2]["role"], "user");
        assert_eq!(messages[2]["content"], "how are you?");

        // Required tuning knobs land in the params with the panel's
        // explicit defaults so a daemon-side default change can't drift
        // chat behaviour silently.
        assert!(params.get("temperature").is_some());
        assert!(params.get("max_tokens").is_some());
        // No `prompt` field — we always send `messages`.
        assert!(params.get("prompt").is_none());
        // No `system` unless explicitly set.
        assert!(params.get("system").is_none());
    }

    #[test]
    fn system_prompt_rides_at_head_of_messages() {
        // WEFT-255: when set, the panel's `system` field is sent as a
        // `{ role: "system", content: ... }` message at index 0 so the
        // daemon-side concierge can layer it on top of the workspace
        // identity prompt. There's no top-level `system` field — the
        // wire shape is uniformly `messages`.
        let mut view = ChatView {
            system: Some("you are concise".into()),
            ..Default::default()
        };
        view.history.push(ChatMessage::user("hi"));
        let params = view.build_request_params("again");
        // Top-level `system` still absent — we never split the prompt
        // into a sibling field.
        assert!(params.get("system").is_none());

        let messages = params["messages"].as_array().expect("messages");
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "you are concise");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"], "hi");
        assert_eq!(messages[2]["role"], "user");
        assert_eq!(messages[2]["content"], "again");
    }

    #[test]
    fn whitespace_only_system_prompt_is_dropped_from_wire() {
        let mut view = ChatView {
            system: Some("   \n\t".into()),
            ..Default::default()
        };
        view.history.push(ChatMessage::user("hi"));
        let params = view.build_request_params("again");
        let messages = params["messages"].as_array().expect("messages");
        // No system frame — the trim-empty guard kicks in before we
        // emit one.
        assert!(messages.iter().all(|m| m["role"] != "system"));
    }

    #[test]
    fn build_params_includes_conv_id_after_first_submit_mints_one() {
        let mut view = ChatView::default();
        // Before any submit, conv_id is None and isn't on the wire.
        let params = view.build_request_params("first");
        assert!(params.get("conv_id").is_none());

        // After mint, the wire payload carries it so the daemon's
        // per-conv state lines up across turns.
        view.ensure_conv_id();
        let params = view.build_request_params("second");
        let id = params
            .get("conv_id")
            .and_then(Value::as_str)
            .expect("conv_id");
        assert!(id.starts_with("panel-"), "got {id:?}");
    }

    #[test]
    fn ensure_conv_id_is_idempotent() {
        let mut view = ChatView::default();
        let a = view.ensure_conv_id().to_owned();
        let b = view.ensure_conv_id().to_owned();
        assert_eq!(a, b, "second call must not re-mint");
    }

    #[test]
    fn heartbeat_path_is_none_until_first_turn() {
        let mut view = ChatView::default();
        assert!(view.heartbeat_path().is_none());
        view.ensure_conv_id();
        let p = view.heartbeat_path().expect("path after mint");
        assert!(p.starts_with("substrate/_derived/chat/"));
        assert!(p.ends_with("/status"));
    }

    #[test]
    fn identity_warning_clear_for_canonical_source() {
        let mut view = ChatView::default();
        view.on_response_ok(&json!({
            "assistant_text": "hi",
            "identity_source": "clawft",
        }));
        assert_eq!(view.identity_warning(), None);
    }

    #[test]
    fn identity_warning_fires_for_docs_fallback() {
        let mut view = ChatView::default();
        view.on_response_ok(&json!({
            "assistant_text": "hi",
            "identity_source": "docs-fallback",
        }));
        assert_eq!(view.identity_warning(), Some("docs-fallback"));
    }

    #[test]
    fn identity_warning_fires_for_unknown_source() {
        // Forward-compat: any unrecognised source value warns. A future
        // daemon adding a new identity loader without touching the
        // panel's allowlist would otherwise silently degrade.
        let mut view = ChatView::default();
        view.on_response_ok(&json!({
            "assistant_text": "hi",
            "identity_source": "experimental",
        }));
        assert_eq!(view.identity_warning(), Some("experimental"));
    }

    #[test]
    fn identity_warning_silent_when_field_absent() {
        // Non-loop / older producers may omit the field (C1 default).
        // Absence is not a warning — only an explicit non-canonical
        // value is. WEFT-328 populates it on the normal agent-loop path.
        let mut view = ChatView::default();
        view.on_response_ok(&json!({
            "assistant_text": "hi",
        }));
        assert_eq!(view.identity_warning(), None);
    }

    #[test]
    fn ok_response_accepts_assistant_text_field() {
        // `agent.chat` returns `assistant_text`; the panel must accept it
        // alongside the legacy `completion` field for cross-rev safety.
        // WEFT-328: full payload with real token/model/identity values.
        let mut view = ChatView::default();
        view.history.push(ChatMessage::user("hi"));
        let response = json!({
            "assistant_text": "hello from the concierge",
            "tool_calls": [{
                "name": "echo",
                "arguments_preview": "{\"text\":\"hi\"}",
                "result_preview": "hi",
                "success": true
            }],
            "finish_reason": "stop",
            "iterations": 2,
            "prompt_tokens": 40,
            "completion_tokens": 22,
            "model": "local",
            "identity_source": "clawft",
        });
        view.on_response_ok(&response);
        assert_eq!(view.history.len(), 2);
        assert_eq!(view.history[1].content, "hello from the concierge");
        // Identity source captured for the drift chip (canonical → no warn).
        assert_eq!(view.identity_warning(), None);
    }

    #[test]
    fn appends_assistant_message_on_ok_response() {
        let mut view = ChatView::default();
        view.history.push(ChatMessage::user("hi"));

        // Mock the wire shape an `llm.prompt` success returns: the
        // `Response::success` body is the `LlmPromptResult` JSON.
        let response = json!({
            "completion": "hello back",
            "finish_reason": "stop",
            "prompt_tokens": 5,
            "completion_tokens": 3,
            "model": "local",
        });
        view.on_response_ok(&response);

        assert_eq!(view.history.len(), 2);
        assert_eq!(view.history[1].role, "assistant");
        assert_eq!(view.history[1].content, "hello back");
        assert!(!view.is_in_flight());
    }

    #[test]
    fn ok_response_with_missing_completion_yields_placeholder() {
        let mut view = ChatView::default();
        view.on_response_ok(&json!({ "completion_tokens": 0 }));
        assert_eq!(view.history.len(), 1);
        assert_eq!(view.history[0].role, "assistant");
        assert!(view.history[0].content.contains("empty"));
    }

    #[test]
    fn appends_error_bubble_on_err_response() {
        let mut view = ChatView::default();
        view.history.push(ChatMessage::user("hi"));
        view.on_response_err("llm.prompt: llm service is disabled");

        assert_eq!(view.history.len(), 2);
        assert_eq!(view.history[1].role, "error");
        assert!(view.history[1].content.contains("llm service is disabled"));
        assert!(!view.is_in_flight());
    }

    #[test]
    fn chat_error_kind_branches_timeout_gate_llm() {
        // WEFT-334: panel can branch on the three primary variants.
        assert_eq!(
            chat_error_kind("[timeout] agent.chat: operation timed out: llm_call"),
            Some("timeout")
        );
        assert_eq!(
            chat_error_kind("[gate_deny] agent.chat: security violation: path"),
            Some("gate_deny")
        );
        assert_eq!(
            chat_error_kind("[llm_error] agent.chat: provider error: 502"),
            Some("llm_error")
        );
        // Legacy string-only errors have no kind.
        assert_eq!(chat_error_kind("agent.chat: agent service not wired"), None);
    }

    // ── WEFT-253 stream frame state machine ──────────────────────

    #[test]
    fn stream_path_mirrors_heartbeat_layout() {
        let mut view = ChatView::default();
        assert!(view.stream_path().is_none());
        view.ensure_conv_id();
        let p = view.stream_path().expect("path after mint");
        assert!(p.starts_with("substrate/_derived/chat/"));
        assert!(p.ends_with("/stream"));
        // Sibling of the status heartbeat path (same conv id).
        let status = view.heartbeat_path().expect("status");
        assert_eq!(
            p.trim_end_matches("/stream"),
            status.trim_end_matches("/status")
        );
    }

    #[test]
    fn stream_frame_grows_draft_and_ignores_stale_seq() {
        let mut view = ChatView::default();
        view.on_stream_frame(&json!({
            "phase": "thinking",
            "text": "",
            "seq": 0,
            "done": false,
        }));
        assert_eq!(view.stream_phase(), Some("thinking"));
        assert_eq!(
            view.streaming_draft(),
            None,
            "empty thinking keeps draft None"
        );

        view.on_stream_frame(&json!({
            "phase": "generating",
            "text": "hel",
            "seq": 1,
            "done": false,
        }));
        assert_eq!(view.streaming_draft(), Some("hel"));
        assert_eq!(view.stream_phase(), Some("generating"));

        view.on_stream_frame(&json!({
            "phase": "generating",
            "text": "hello",
            "seq": 2,
            "done": false,
        }));
        assert_eq!(view.streaming_draft(), Some("hello"));

        // Stale lower seq must not rewind the draft.
        view.on_stream_frame(&json!({
            "phase": "generating",
            "text": "he",
            "seq": 1,
            "done": false,
        }));
        assert_eq!(view.streaming_draft(), Some("hello"));
    }

    #[test]
    fn stream_state_clears_on_final_ok() {
        let mut view = ChatView::default();
        view.on_stream_frame(&json!({
            "phase": "generating",
            "text": "partial",
            "seq": 3,
            "done": false,
        }));
        assert!(view.streaming_draft().is_some());
        view.on_response_ok(&json!({
            "assistant_text": "partial final",
            "identity_source": "clawft",
        }));
        assert_eq!(view.streaming_draft(), None);
        assert_eq!(view.stream_phase(), None);
        assert_eq!(view.stream_seq, 0);
        assert_eq!(
            view.history.last().map(|m| m.content.as_str()),
            Some("partial final")
        );
    }

    #[test]
    fn stream_state_clears_on_err() {
        let mut view = ChatView::default();
        view.on_stream_frame(&json!({
            "phase": "generating",
            "text": "x",
            "seq": 1,
            "done": false,
        }));
        view.on_response_err("agent.chat_stream: cancelled");
        assert_eq!(view.streaming_draft(), None);
        assert!(!view.is_in_flight());
    }

    // ── WEFT-254 multi-conversation sidebar ──────────────────────

    #[test]
    fn panel_starts_with_one_empty_session() {
        let panel = ChatPanel::new();
        assert_eq!(panel.len(), 1);
        assert_eq!(panel.active_index(), 0);
        assert!(panel.active_view().history.is_empty());
        assert_eq!(panel.sessions()[0].first_snippet(), "New conversation");
        assert!(panel.sessions()[0].display_id().starts_with("local-"));
    }

    #[test]
    fn new_conversation_appends_and_selects() {
        let mut panel = ChatPanel::new();
        let a = panel.active_index();
        panel
            .active_view_mut()
            .history
            .push(ChatMessage::user("first thread"));
        let b = panel.new_conversation();
        assert_eq!(panel.len(), 2);
        assert_eq!(b, 1);
        assert_eq!(panel.active_index(), b);
        assert_ne!(a, b);
        // New session is empty; previous keeps its history.
        assert!(panel.active_view().history.is_empty());
        assert_eq!(panel.sessions()[0].view().history.len(), 1);
        assert_eq!(panel.sessions()[0].first_snippet(), "first thread");
    }

    #[test]
    fn select_switches_active_history() {
        let mut panel = ChatPanel::new();
        panel
            .active_view_mut()
            .history
            .push(ChatMessage::user("alpha"));
        panel.new_conversation();
        panel
            .active_view_mut()
            .history
            .push(ChatMessage::user("beta"));
        assert_eq!(panel.active_view().history[0].content, "beta");

        panel.select(0);
        assert_eq!(panel.active_index(), 0);
        assert_eq!(panel.active_view().history[0].content, "alpha");

        panel.select(1);
        assert_eq!(panel.active_view().history[0].content, "beta");

        // OOB is a no-op.
        panel.select(99);
        assert_eq!(panel.active_index(), 1);
    }

    #[test]
    fn select_preserves_per_session_stream_state() {
        // Switching away mid-stream must not clear the background
        // session's draft; the active stream path follows the selected
        // session only.
        let mut panel = ChatPanel::new();
        panel.active_view_mut().ensure_conv_id();
        panel.active_view_mut().on_stream_frame(&json!({
            "phase": "generating",
            "text": "partial A",
            "seq": 2,
            "done": false,
        }));
        assert_eq!(panel.active_view().streaming_draft(), Some("partial A"));

        panel.new_conversation();
        assert_eq!(panel.active_view().streaming_draft(), None);
        // Session 0 still holds its draft.
        assert_eq!(
            panel.sessions()[0].view().streaming_draft(),
            Some("partial A")
        );
        assert!(panel.sessions()[0].view().stream_path().is_some());
        assert!(panel.active_view().stream_path().is_none());

        panel.select(0);
        assert_eq!(panel.active_view().streaming_draft(), Some("partial A"));
    }

    #[test]
    fn snippet_truncates_and_collapses_newlines() {
        let mut panel = ChatPanel::new();
        let long = "hello\nworld ".repeat(10);
        panel
            .active_view_mut()
            .history
            .push(ChatMessage::user(long));
        let snip = panel.sessions()[0].first_snippet();
        assert!(!snip.contains('\n'));
        assert!(snip.ends_with('…'), "got {snip:?}");
        assert!(snip.chars().count() <= SNIPPET_MAX_CHARS + 1);
    }

    #[test]
    fn display_id_prefers_minted_conv_id() {
        let mut panel = ChatPanel::new();
        let local = panel.sessions()[0].display_id().to_owned();
        assert!(local.starts_with("local-"));
        panel.active_view_mut().ensure_conv_id();
        let display = panel.sessions()[0].display_id().to_owned();
        assert!(display.starts_with("panel-"), "got {display:?}");
        assert_ne!(display, local);
    }

    #[test]
    fn truncate_snippet_helpers() {
        assert_eq!(truncate_snippet("  hi  ", 48), "hi");
        assert_eq!(truncate_snippet("", 48), "New conversation");
        assert_eq!(truncate_snippet("a\nb", 48), "a b");
        let s = truncate_snippet("abcdefghij", 5);
        assert_eq!(s, "abcde…");
    }

    #[test]
    fn touch_updates_last_active_ordering_input() {
        let mut panel = ChatPanel::new();
        let t0 = panel.sessions()[0].last_active_ms();
        panel.new_conversation();
        // Force-touch session 0 so it would sort first if we re-sort.
        panel.sessions[0].touch();
        assert!(panel.sessions()[0].last_active_ms() >= t0);
        assert_eq!(panel.active_index(), 1);
    }

    // ── WEFT-256 model / provider chip strip ─────────────────

    #[test]
    fn models_ok_builds_chips_and_seeds_default() {
        let mut view = ChatView::default();
        view.on_models_ok(&json!({
            "default_model": "hermes-4.3-36b",
            "default_provider": "local",
            "base_url": "http://127.0.0.1:8090",
            "models": [
                {
                    "id": "hermes-4.3-36b",
                    "provider": "local",
                    "label": "hermes-4.3-36b",
                    "is_default": true,
                    "live": true
                },
                {
                    "id": "other-model",
                    "provider": "local",
                    "label": "other-model",
                    "is_default": false,
                    "live": true
                }
            ]
        }));
        assert_eq!(view.available_models().len(), 2);
        assert_eq!(view.selected_model(), Some("hermes-4.3-36b"));
        assert!(view.available_models()[0].is_default);
    }

    #[test]
    fn models_ok_keeps_existing_selection_when_still_valid() {
        let mut view = ChatView::default();
        view.select_model("other-model");
        view.on_models_ok(&json!({
            "default_model": "hermes-4.3-36b",
            "default_provider": "local",
            "base_url": "http://127.0.0.1:8090",
            "models": [
                {"id": "hermes-4.3-36b", "provider": "local", "label": "h", "is_default": true},
                {"id": "other-model", "provider": "local", "label": "o", "is_default": false}
            ]
        }));
        assert_eq!(view.selected_model(), Some("other-model"));
    }

    #[test]
    fn models_ok_falls_back_to_default_model_when_array_empty() {
        let mut view = ChatView::default();
        view.on_models_ok(&json!({
            "default_model": "hermes-4.3-36b",
            "default_provider": "local",
            "base_url": "http://127.0.0.1:8090",
            "models": []
        }));
        assert_eq!(view.available_models().len(), 1);
        assert_eq!(view.selected_model(), Some("hermes-4.3-36b"));
    }

    #[test]
    fn build_params_includes_metadata_model_when_selected() {
        let mut view = ChatView::default();
        view.select_model("hermes-4.3-36b");
        let params = view.build_request_params("hi");
        assert_eq!(
            params
                .get("metadata")
                .and_then(|m| m.get("model"))
                .and_then(Value::as_str),
            Some("hermes-4.3-36b")
        );
    }

    #[test]
    fn build_params_omits_metadata_when_no_model_selected() {
        let view = ChatView::default();
        let params = view.build_request_params("hi");
        assert!(params.get("metadata").is_none());
    }

    #[test]
    fn models_err_does_not_clear_catalog() {
        let mut view = ChatView::default();
        view.on_models_ok(&json!({
            "default_model": "hermes-4.3-36b",
            "default_provider": "local",
            "base_url": "http://127.0.0.1:8090",
            "models": [
                {"id": "hermes-4.3-36b", "provider": "local", "label": "h", "is_default": true}
            ]
        }));
        view.on_models_err("timeout");
        assert_eq!(view.available_models().len(), 1);
        assert!(view.models_error.as_deref().unwrap().contains("timeout"));
    }

    // ── WEFT-284 ThreadDock composition ──────────────────────────

    #[test]
    fn thread_dock_hidden_until_two_agents() {
        let mut panel = ChatPanel::new();
        assert!(!panel.shows_thread_dock());
        panel.upsert_agent_thread(
            AgentThread::new("swarm/coder", "coder").phase(ThreadPhase::Streaming),
        );
        assert!(!panel.shows_thread_dock());
        panel.upsert_agent_thread(
            AgentThread::new("swarm/reviewer", "reviewer").phase(ThreadPhase::Thinking),
        );
        assert!(panel.shows_thread_dock());
        assert_eq!(panel.thread_dock().len(), 2);
    }

    #[test]
    fn parallel_agent_streams_do_not_interleave() {
        let mut panel = ChatPanel::new();
        panel.upsert_agent_thread(AgentThread::new("a", "coder"));
        panel.upsert_agent_thread(AgentThread::new("b", "reviewer"));
        assert!(panel.feed_agent_stream("a", "hello\nfrom coder", Some(ThreadPhase::Streaming)));
        assert!(panel.feed_agent_stream("b", "hello\nfrom reviewer", Some(ThreadPhase::Streaming)));
        // Grow each stream independently.
        assert!(panel.feed_agent_stream(
            "a",
            "hello\nfrom coder\nmore coder",
            Some(ThreadPhase::Streaming)
        ));
        let a = panel.thread_dock().get("a").unwrap();
        let b = panel.thread_dock().get("b").unwrap();
        assert_eq!(a.lines, ["hello", "from coder", "more coder"]);
        assert_eq!(b.lines, ["hello", "from reviewer"]);
        assert!(!a.lines.iter().any(|l| l.contains("reviewer")));
        assert!(!b.lines.iter().any(|l| l.contains("coder")));
        assert_eq!(a.phase, ThreadPhase::Streaming);
        assert_eq!(b.phase, ThreadPhase::Streaming);
    }

    #[test]
    fn upsert_replaces_same_id_and_clear_resets() {
        let mut panel = ChatPanel::new();
        panel.upsert_agent_thread(AgentThread::new("a", "coder").phase(ThreadPhase::Idle));
        panel.upsert_agent_thread(AgentThread::new("b", "reviewer"));
        assert_eq!(panel.thread_dock().len(), 2);
        panel.upsert_agent_thread(
            AgentThread::new("a", "coder-v2")
                .phase(ThreadPhase::Done)
                .lines(["done"]),
        );
        assert_eq!(panel.thread_dock().len(), 2);
        assert_eq!(panel.thread_dock().get("a").unwrap().label, "coder-v2");
        assert_eq!(
            panel.thread_dock().get("a").unwrap().phase,
            ThreadPhase::Done
        );
        panel.clear_agent_threads();
        assert!(!panel.shows_thread_dock());
        assert!(panel.thread_dock().is_empty());
    }
}
