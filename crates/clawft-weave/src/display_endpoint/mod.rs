//! The daemon side of a display endpoint (ADR-117 D1/D2).
//!
//! An app (the console host, over the `display.*` RPC) opens a session on
//! a leaf's display; this module turns that into `SessionFrame`s on the
//! leaf's push topic (host -> leaf) and folds the leaf's `.input`
//! publishes back into per-session event queues (leaf -> host). One leaf
//! has one display and one holder. The wire format and the leaf-side
//! rules live in `weftos_leaf_scene::session`, shared with the firmware.
//!
//! Grants go through [`GrantPolicy`], checked on open and again on every
//! op and poll, so a revoked subject loses the screen on its next call.

mod chunk;
mod policy;
mod sink;
mod state;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use weftos_leaf_scene::session::{SessionFrame, MAX_PUSH_CBOR};
use weftos_leaf_scene::SceneEnvelope;

pub use chunk::chunk_ops;
pub use policy::{GrantPolicy, GrantRequest, RevocablePolicy};
pub use sink::{FrameSink, RouterSink};
use state::LeafSlot;

/// Timing and bounds. Tests shorten them.
#[derive(Debug, Clone)]
pub struct EndpointConfig {
    /// How long `open` waits for the leaf's `Opened` / `Refused`.
    pub open_timeout: Duration,
    /// Empty `Ops` sent to each live session this often, so the leaf's
    /// idle timer (45 s) never fires on a healthy session.
    pub keepalive: Duration,
    /// A session nobody has polled or written to for this long is closed.
    pub client_idle: Duration,
    /// Events kept per session; the oldest are dropped past it.
    pub max_events: usize,
}

impl Default for EndpointConfig {
    fn default() -> Self {
        Self {
            open_timeout: Duration::from_secs(5),
            keepalive: Duration::from_secs(10),
            client_idle: Duration::from_secs(60),
            max_events: 256,
        }
    }
}

/// Why a call failed. `kind()` is the RPC `error_kind`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndpointError {
    /// The policy refused (or revoked) the grant.
    Denied(String),
    /// The leaf answered `Refused` (busy, no such display).
    Refused(String),
    /// The leaf cannot be reached or did not answer.
    Unreachable(String),
    /// No live session with that token.
    NoSession,
    /// The session ended; carries why.
    Ended(String),
    /// The request itself is malformed or over a bound.
    Bad(String),
}

impl EndpointError {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Denied(_) => "display_denied",
            Self::Refused(_) => "display_refused",
            Self::Unreachable(_) => "display_unreachable",
            Self::NoSession => "display_no_session",
            Self::Ended(_) => "display_ended",
            Self::Bad(_) => "display_bad_request",
        }
    }
}

impl std::fmt::Display for EndpointError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Denied(w) => write!(f, "display grant denied: {w}"),
            Self::Refused(w) => write!(f, "the leaf refused the session: {w}"),
            Self::Unreachable(w) => write!(f, "display leaf unreachable: {w}"),
            Self::NoSession => write!(f, "no such display session"),
            Self::Ended(w) => write!(f, "display session ended: {w}"),
            Self::Bad(w) => write!(f, "bad display request: {w}"),
        }
    }
}

impl std::error::Error for EndpointError {}

/// What `open` hands back.
#[derive(Debug, Clone, Serialize)]
pub struct OpenReply {
    /// The daemon's token for the session; every later call names it.
    pub session: u64,
    pub leaf_id: String,
    pub width: u16,
    pub height: u16,
    pub render_caps: u32,
    pub touch: bool,
    pub ptt: bool,
    pub max_nodes: u32,
    pub max_ops_per_sec: u32,
    pub image_budget_bytes: u32,
}

/// One event for the app, as the RPC carries it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PollEvent {
    /// A hit-tested pointer event; `cbor_b64` is an `InputEnvelope`.
    Input { cbor_b64: String },
    Sensor { name: String, pressed: Option<bool>, value: Option<f64> },
}

/// What `poll` hands back.
#[derive(Debug, Clone, Default, Serialize)]
pub struct PollReply {
    pub events: Vec<PollEvent>,
    /// Batches the leaf refused whole (the session continues).
    pub rejected: Vec<String>,
    /// Set once the session is over; the app closes its side.
    pub ended: Option<String>,
    /// Events dropped for want of room since the last poll.
    pub dropped: u64,
}

pub struct DisplayEndpoint {
    inner: Arc<Inner>,
}

pub(crate) struct Inner {
    sink: Arc<dyn FrameSink>,
    policy: Arc<dyn GrantPolicy>,
    cfg: EndpointConfig,
    slots: Mutex<HashMap<String, Arc<LeafSlot>>>,
    next_token: Mutex<u64>,
}

impl DisplayEndpoint {
    pub fn new(sink: Arc<dyn FrameSink>, policy: Arc<dyn GrantPolicy>, cfg: EndpointConfig) -> Self {
        let inner = Arc::new(Inner { sink, policy, cfg, slots: Mutex::new(HashMap::new()), next_token: Mutex::new(1) });
        let weak = Arc::downgrade(&inner);
        let tick = inner.cfg.keepalive.min(Duration::from_secs(1)).max(Duration::from_millis(20));
        tokio::spawn(async move {
            let mut last_keepalive = Instant::now();
            loop {
                tokio::time::sleep(tick).await;
                let Some(inner) = weak.upgrade() else { return };
                let keepalive_due = last_keepalive.elapsed() >= inner.cfg.keepalive;
                if keepalive_due {
                    last_keepalive = Instant::now();
                }
                inner.maintain(keepalive_due).await;
            }
        });
        Self { inner }
    }

    /// The subject of the live session this endpoint holds on a leaf, if any.
    pub fn holder(&self, leaf_id: &str) -> Option<String> {
        let slots = self.inner.slots.lock().unwrap_or_else(|e| e.into_inner());
        slots.get(leaf_id).and_then(|s| s.holder())
    }

    /// Open (or re-open, idempotently) a session on a leaf's display.
    pub async fn open(
        &self,
        leaf_id: &str,
        subject: &str,
        resource: &str,
        proof: Vec<u8>,
        display: u8,
    ) -> Result<OpenReply, EndpointError> {
        check_leaf_id(leaf_id)?;
        check_open_fields(subject, resource, &proof)?;
        self.inner
            .policy
            .admit(&GrantRequest { subject: subject.into(), resource: resource.into(), leaf_id: leaf_id.into(), proof: proof.clone() })
            .map_err(EndpointError::Denied)?;
        let slot = self.inner.slot(leaf_id);
        self.inner.open(&slot, leaf_id, subject, proof, display).await
    }

    /// Send scene ops for a session, in as many frames as they need.
    pub async fn ops(&self, token: u64, envelope: SceneEnvelope) -> Result<(), EndpointError> {
        let (slot, leaf_id) = self.inner.find(token)?;
        self.inner.ops(&slot, &leaf_id, token, envelope).await
    }

    /// Everything the leaf has reported since the last poll.
    pub async fn poll(&self, token: u64, max: usize) -> Result<PollReply, EndpointError> {
        let (slot, leaf_id) = self.inner.find(token)?;
        self.inner.poll(&slot, &leaf_id, token, max).await
    }

    /// Release the screen. Idempotent: an unknown or ended token is fine.
    pub async fn close(&self, token: u64) -> Result<(), EndpointError> {
        let Ok((slot, leaf_id)) = self.inner.find(token) else { return Ok(()) };
        self.inner.close(&slot, &leaf_id, token, "closed by the app").await;
        Ok(())
    }

    /// Feed one frame the leaf published on its `.input` topic.
    pub fn on_leaf_frame(&self, leaf_id: &str, frame: SessionFrame) {
        let slot = self.inner.slot(leaf_id);
        slot.on_frame(frame, self.inner.cfg.max_events);
    }

    /// Decode the carrier of a message published on `mesh.leaf.<id>.input`
    /// and feed it. Returns false for messages that are not session frames
    /// (plain touch events), so the caller can leave those alone.
    pub fn on_leaf_message(&self, leaf_id: &str, message: &serde_json::Value) -> bool {
        let Some(text) = carrier_text(message) else { return false };
        match SessionFrame::from_carrier(text.as_bytes()) {
            Some(Ok(frame)) => {
                self.on_leaf_frame(leaf_id, frame);
                true
            }
            Some(Err(e)) => {
                tracing::warn!(leaf = leaf_id, error = %e, "display: undecodable session frame from leaf");
                true
            }
            None => false,
        }
    }

    /// Subscribe to `mesh.leaf.<id>.input` on the topic router and feed
    /// the endpoint from it. Idempotent per leaf.
    pub fn attach_input(&self, topics: &Arc<clawft_kernel::TopicRouter>, leaf_id: &str) {
        let slot = self.inner.slot(leaf_id);
        if !slot.mark_attached() {
            return;
        }
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(256);
        let topic = format!("mesh.leaf.{leaf_id}.input");
        topics.subscribe_sink(&topic, clawft_kernel::SubscriberSink::ExternalStream(tx));
        let me = DisplayEndpoint { inner: Arc::clone(&self.inner) };
        let leaf = leaf_id.to_owned();
        tokio::spawn(async move {
            while let Some(line) = rx.recv().await {
                let Ok(msg) = serde_json::from_slice::<serde_json::Value>(&line) else { continue };
                if let Some(payload) = msg.get("payload") {
                    me.on_leaf_message(&leaf, payload);
                }
            }
        });
    }
}

fn check_leaf_id(id: &str) -> Result<(), EndpointError> {
    if id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()) {
        Ok(())
    } else {
        Err(EndpointError::Bad("leaf_id must be 32 lowercase hex characters (as `weaver leaf provision` prints it)".into()))
    }
}

/// Bounds on what an app may put in `Open`: the subject is shown to other
/// apps (`display busy: held by ...`) and logged, and the whole frame must
/// fit the pad's receive buffer.
fn check_open_fields(subject: &str, resource: &str, proof: &[u8]) -> Result<(), EndpointError> {
    let bad = |why: &str| Err(EndpointError::Bad(why.into()));
    if subject.is_empty() || subject.chars().count() > 64 || subject.chars().any(char::is_control) {
        return bad("subject must be 1 to 64 printable characters");
    }
    if resource.len() > 256 || resource.chars().any(char::is_control) {
        return bad("resource must be at most 256 printable characters");
    }
    if proof.len() > 2048 {
        return bad("proof must be at most 2048 bytes");
    }
    Ok(())
}

/// The carrier text inside a `KernelMessage` payload: `{"Json": {...}}`
/// or `{"Text": "..."}`; a bare object is accepted too.
fn carrier_text(payload: &serde_json::Value) -> Option<String> {
    if let Some(json) = payload.get("Json") {
        return Some(json.to_string());
    }
    if let Some(text) = payload.get("Text").and_then(|t| t.as_str()) {
        return Some(text.to_owned());
    }
    payload.is_object().then(|| payload.to_string())
}

impl Inner {
    fn slot(&self, leaf_id: &str) -> Arc<LeafSlot> {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        Arc::clone(slots.entry(leaf_id.to_owned()).or_insert_with(|| Arc::new(LeafSlot::default())))
    }

    fn find(&self, token: u64) -> Result<(Arc<LeafSlot>, String), EndpointError> {
        let slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        slots
            .iter()
            .find(|(_, s)| s.has_token(token))
            .map(|(id, s)| (Arc::clone(s), id.clone()))
            .ok_or(EndpointError::NoSession)
    }

    async fn send(&self, leaf_id: &str, frame: &SessionFrame) -> Result<(), EndpointError> {
        let carrier = frame.to_carrier(leaf_id).map_err(|e| EndpointError::Bad(format!("cannot encode frame: {e}")))?;
        self.sink.push(leaf_id, carrier).await.map_err(EndpointError::Unreachable)
    }

    async fn open(
        &self,
        slot: &Arc<LeafSlot>,
        leaf_id: &str,
        subject: &str,
        proof: Vec<u8>,
        display: u8,
    ) -> Result<OpenReply, EndpointError> {
        // One open at a time per leaf, so two apps cannot interleave.
        let _gate = slot.open_gate.lock().await;
        if let Some(live) = slot.live_for(subject) {
            return Ok(live);
        }
        let rx = slot.expect_open();
        let frame = SessionFrame::Open { subject: subject.into(), display, proof };
        if let Err(e) = self.send(leaf_id, &frame).await {
            slot.cancel_open();
            return Err(e);
        }
        let answer = tokio::time::timeout(self.cfg.open_timeout, rx).await;
        slot.cancel_open();
        match answer {
            Err(_) => Err(EndpointError::Unreachable(format!(
                "no answer from leaf {leaf_id} within {:?} (is it connected and subscribed?)",
                self.cfg.open_timeout
            ))),
            Ok(Err(_)) => Err(EndpointError::Unreachable("open was cancelled".into())),
            Ok(Ok(Err(reason))) => Err(EndpointError::Refused(reason)),
            Ok(Ok(Ok(opened))) => {
                let token = {
                    let mut n = self.next_token.lock().unwrap_or_else(|e| e.into_inner());
                    let t = *n;
                    *n += 1;
                    t
                };
                Ok(slot.install(token, subject, opened, leaf_id))
            }
        }
    }

    async fn ops(&self, slot: &Arc<LeafSlot>, leaf_id: &str, token: u64, env: SceneEnvelope) -> Result<(), EndpointError> {
        let (session, subject) = slot.touch_live(token)?;
        if !self.policy.still_valid(&subject, leaf_id) {
            self.close(slot, leaf_id, token, "grant revoked").await;
            return Err(EndpointError::Denied(format!("grant for {subject} was revoked")));
        }
        let chunks = chunk_ops(&env.ops, MAX_PUSH_CBOR).map_err(EndpointError::Bad)?;
        let last = chunks.len().saturating_sub(1);
        // Hold the send lock across the batch so another caller's frames
        // cannot land between fragments.
        let _order = slot.send_lock.lock().await;
        for (i, ops) in chunks.into_iter().enumerate() {
            let seq = slot.next_seq(token)?;
            let frame = SessionFrame::Ops { session, seq, more: i != last, envelope: SceneEnvelope::new(env.display_id, ops) };
            self.send(leaf_id, &frame).await?;
        }
        Ok(())
    }

    async fn poll(&self, slot: &Arc<LeafSlot>, leaf_id: &str, token: u64, max: usize) -> Result<PollReply, EndpointError> {
        let (_, subject) = slot.touch(token)?;
        if !self.policy.still_valid(&subject, leaf_id) {
            self.close(slot, leaf_id, token, "grant revoked").await;
            return Err(EndpointError::Denied(format!("grant for {subject} was revoked")));
        }
        slot.drain(token, max.clamp(1, 1024))
    }

    async fn close(&self, slot: &Arc<LeafSlot>, leaf_id: &str, token: u64, why: &str) {
        let Some(session) = slot.end(token, why) else { return };
        let _ = self.send(leaf_id, &SessionFrame::Close { session }).await;
    }

    /// Keepalives for live sessions and the reaper for abandoned ones.
    async fn maintain(&self, keepalive_due: bool) {
        let slots: Vec<(String, Arc<LeafSlot>)> =
            self.slots.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|(k, v)| (k.clone(), Arc::clone(v))).collect();
        for (leaf_id, slot) in slots {
            for live in slot.snapshot() {
                if live.idle_for() > self.cfg.client_idle {
                    self.close(&slot, &leaf_id, live.token, "the app went quiet").await;
                } else if keepalive_due {
                    let _order = slot.send_lock.lock().await;
                    if let Ok(seq) = slot.next_seq(live.token) {
                        let frame = SessionFrame::Ops { session: live.session, seq, more: false, envelope: SceneEnvelope::new(0, Vec::new()) };
                        let _ = self.send(&leaf_id, &frame).await;
                    }
                }
            }
        }
    }
}
