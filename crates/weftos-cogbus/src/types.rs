//! Broker types shared with the server and the daemon: refusals, the
//! registration of an instance, `cogbus list` rows and the per-connection
//! [`Session`].

use std::collections::BTreeMap;
use std::sync::Arc;

use clawft_types::cog_topics::TopicClass;
use serde::Serialize;
use tokio::sync::{Notify, mpsc, watch};

use crate::proto::{Delivery, ErrorCode, Event, InputSpec, OutputSpec};
use crate::queue::SubQueue;

/// A refused request.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{code:?}: {message}")]
pub struct BusError {
    /// Machine code sent to the client.
    pub code: ErrorCode,
    /// Human reason sent to the client.
    pub message: String,
    /// Who was refused what, set by [`refuse`]; read by
    /// [`Broker::record_refusal`](crate::Broker::record_refusal).
    pub(crate) ctx: Option<Box<RefusalCtx>>,
}

/// Equal when code and message are; the refusal context is not compared.
impl PartialEq for BusError {
    fn eq(&self, o: &Self) -> bool {
        self.code == o.code && self.message == o.message
    }
}

impl Eq for BusError {}

/// The instance, action and topic a refusal was about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RefusalCtx {
    pub instance: String,
    pub action: String,
    pub topic: String,
}

impl BusError {
    /// Error with `code`.
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            ctx: None,
        }
    }
}

/// Mark `e` as a refusal of `action` on `topic` by `instance` and hand it
/// back. The connection that carried the request records it
/// ([`Broker::record_refusal`](crate::Broker::record_refusal)): governance
/// counts, the activity log, the rate limit and repeated-refusal stop.
pub fn refuse(instance: &str, action: &str, topic: &str, mut e: BusError) -> BusError {
    tracing::debug!(target: "cogbus", instance, action, topic, code = ?e.code, reason = %e.message, "cogbus refusal");
    e.ctx = Some(Box::new(RefusalCtx {
        instance: instance.to_string(),
        action: action.to_string(),
        topic: topic.to_string(),
    }));
    e
}

/// What the placement told the broker about one instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceSpec {
    /// Instance id on this node.
    pub instance_id: String,
    /// Cog id.
    pub cog_id: String,
    /// Project that placed it; `None` is the node's own bucket.
    pub project_id: Option<String>,
    /// Uid the runtime started it as; `None` skips the peer check.
    pub uid: Option<u32>,
    /// Manifest `[[outputs]]`.
    pub outputs: Vec<OutputSpec>,
    /// Manifest `[[inputs]]`.
    pub inputs: Vec<InputSpec>,
}

/// Broker tunables.
#[derive(Debug, Clone)]
pub struct BrokerConfig {
    /// Node id stamped on every delivery.
    pub node_id: String,
    /// Messages a `drop-oldest` reading subscriber may have pending.
    pub queue_len: usize,
    /// Silence on an authenticated connection before the broker pings it.
    pub idle_ping_ms: u64,
    /// Time a ping has to be answered (any frame counts) before the
    /// connection is closed.
    pub pong_timeout_ms: u64,
    /// Subscriptions (readings and streams) one connection may hold.
    pub max_subs_per_conn: usize,
    /// Connections one instance may have open.
    pub max_conns_per_instance: usize,
    /// Connections the node holds that have not yet said hello.
    pub max_prehello_conns: usize,
    /// Of those, how many one peer uid may hold.
    pub max_prehello_per_uid: usize,
    /// Connections the node holds in all (descriptor protection).
    pub max_conns: usize,
    /// Time a new connection has to say hello.
    pub hello_timeout_ms: u64,
}

impl BrokerConfig {
    /// Defaults for `node_id`: 256 pending readings, ping after 60 s of
    /// silence and close 15 s later, 64 subscriptions per connection and 8
    /// connections per instance.
    pub fn new(node_id: impl Into<String>) -> Self {
        Self {
            node_id: node_id.into(),
            queue_len: 256,
            idle_ping_ms: 60_000,
            pong_timeout_ms: 15_000,
            max_subs_per_conn: 64,
            max_conns_per_instance: 8,
            max_prehello_conns: 64,
            max_prehello_per_uid: 8,
            max_conns: 512,
            hello_timeout_ms: 5_000,
        }
    }
}

/// One row of `cogbus list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TopicRow {
    /// `<kind>/<source_id>`.
    pub topic: String,
    /// Schema id.
    pub schema: String,
    /// Size class.
    pub class: TopicClass,
    /// Instance writing it (`None` once that instance is gone).
    pub writer: Option<String>,
    /// Project the topic belongs to.
    pub project_id: Option<String>,
    /// Live subscriptions matching it.
    pub subscribers: usize,
    /// Messages published.
    pub messages: u64,
    /// Messages dropped across its subscribers.
    pub drops: u64,
}

/// A connection's place in its instance's connection count. Dropping it,
/// on any path (a failed reply, a cancelled task, a panic), gives the slot
/// back.
pub(crate) struct Slot {
    pub(crate) conns: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, std::collections::HashSet<u64>>>>,
    pub(crate) instance: String,
    pub(crate) conn: u64,
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut g = self.conns.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(open) = g.get_mut(&self.instance) {
            open.remove(&self.conn);
            if open.is_empty() {
                g.remove(&self.instance);
            }
        }
    }
}

/// An authenticated connection's view of the broker.
pub struct Session {
    pub(crate) instance_id: String,
    pub(crate) project_id: Option<String>,
    pub(crate) declared: Option<(Vec<OutputSpec>, Vec<InputSpec>)>,
    pub(crate) subs: BTreeMap<u64, Arc<SubQueue>>,
    pub(crate) wake: Arc<Notify>,
    pub(crate) revoked: watch::Receiver<bool>,
    /// Unique per connection; service providers and callers are tracked by it.
    pub(crate) conn: u64,
    /// Frames the broker pushes to this connection outside a request/answer
    /// (calls for a provider, late answers for a caller).
    pub(crate) push: mpsc::UnboundedSender<Event>,
    pub(crate) pushed: Option<mpsc::UnboundedReceiver<Event>>,
    /// Releases the connection's slot when the session goes.
    pub(crate) _slot: Slot,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("instance_id", &self.instance_id)
            .field("project_id", &self.project_id)
            .field("subs", &self.subs.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl Session {
    /// Unique per connection, shared with the stream registry.
    pub fn conn(&self) -> u64 {
        self.conn
    }

    /// The instance this connection speaks for.
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    /// Woken when any of this session's subscriptions has messages.
    pub fn wake(&self) -> Arc<Notify> {
        self.wake.clone()
    }

    /// Changes (or closes) when the instance is revoked.
    pub fn revoked(&self) -> watch::Receiver<bool> {
        self.revoked.clone()
    }

    /// The receiver of frames pushed outside request/answer (once); the
    /// connection sends each as it arrives.
    pub fn take_pushed(&mut self) -> Option<mpsc::UnboundedReceiver<Event>> {
        self.pushed.take()
    }

    /// Pending deliveries of every subscription, per subscription in order.
    pub fn drain(&self) -> Vec<Delivery> {
        self.subs.values().flat_map(|q| q.drain()).collect()
    }
}

pub(crate) fn token_hash(token: &str) -> [u8; 32] {
    *blake3::hash(token.as_bytes()).as_bytes()
}

/// `[a-z0-9][a-z0-9._-]*`, at most 64 chars (the envelope's `source_id`).
pub fn valid_source_id(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"._-".contains(c))
}
