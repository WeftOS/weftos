//! Mesh topics as ADR-120 exposures (ADR-122 section 14, phases B6b-B6e).
//!
//! One [`MeshTopicHub`] per node does both halves:
//!
//! - **source** ([`source`]): a peer's `subscribe` becomes a broker-internal
//!   reader of one topic, served only if, in order, the peer is admitted, the
//!   writer declared `[mesh_limits]`, the topic's exposure covers the peer
//!   ([`TopicExposures::serve`]), governance allows `cogbus.mesh.subscribe`
//!   for `mesh:<node>`, and the raw rule holds. Readings go out only against
//!   the subscriber's credit and inside the cog's limits; past either they are
//!   dropped and counted, so the subscriber sees a `seq` gap.
//! - **subscriber** (`Broker::subscribe` with a `weftos://` name, in the
//!   broker's `mesh_ops`): a proxy topic `<kind>/<source_id>` written by
//!   `@mesh:<node>`, one mesh subscription per proxy topic, credit renewal,
//!   resubscribe with backoff, and teardown when its last local reader goes.
//!
//! The hub has no network of its own: the daemon hands it frames from
//! admitted peers ([`MeshTopicHub::on_frame`]) and gives it a [`MeshLink`]
//! that sends raw `TopicRequest` 0x12 / `TopicData` 0x13 frames on the peer
//! connection. Tests link two hubs in one process.

mod fetch;
pub mod source;
pub mod wire;

#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;

use crate::broker::Broker;
pub use wire::{Advert, Credit, FrameKind, TopicData, TopicReq};

/// Exposure id of a topic (ADR-122 section 14).
pub fn exposure_id(topic: &str) -> String {
    format!("topic.{topic}")
}

/// The broker-internal writer of a proxy topic fed from `node`.
pub fn proxy_writer(node: &str) -> String {
    format!("@mesh:{node}")
}

/// The governance principal of a remote subscriber.
pub fn mesh_principal(node: &str) -> String {
    format!("mesh:{node}")
}

/// A raw topic: a stream or blob class, or a raw media schema (section 14).
pub fn is_raw(class: clawft_types::cog_topics::TopicClass, schema: &str) -> bool {
    use clawft_types::cog_topics::TopicClass;
    matches!(class, TopicClass::Stream | TopicClass::Blob)
        || matches!(schema, "iq.cs16.v1" | "pcm.s16le.v1" | "video.frame-ref.v1")
}

/// The peer a frame came from, as the connection established it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerInfo {
    /// Verified node id.
    pub node_id: String,
    /// An enforced-admission full node (the inference proxy's standard).
    pub admitted: bool,
}

/// Sends frames to peers. Never blocks: a full or closed link refuses and the
/// caller counts the frame as dropped.
pub trait MeshLink: Send + Sync {
    /// Send one payload as a `kind` frame to `node`; false if it was not sent.
    fn send(&self, node: &str, kind: FrameKind, payload: Vec<u8>) -> bool;
}

/// Who an exposure is served to (ADR-120 audiences other than `local`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MeshAudience {
    /// Paired peers.
    Paired,
    /// Named nodes.
    Nodes(Vec<String>),
    /// A project's members; the topic counts as that project's remotely.
    Project(String),
}

/// What an exposure allows one peer, when it is served to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExposureView {
    /// Its audience.
    pub audience: MeshAudience,
    /// The operator allowed raw subscribes (`weaver expose on --raw`).
    pub raw: bool,
    /// Operator's per-subscriber readings per minute, if set.
    pub rate_per_min: Option<u32>,
    /// Operator's per-subscriber bytes per second, if set.
    pub bytes_per_sec: Option<u64>,
}

/// One exposure activity record (ADR-120 section 5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeshRecord {
    /// `topic.<kind>/<source_id>`.
    pub exposure: String,
    /// The peer.
    pub caller_node: String,
    /// Bytes sent to it.
    pub bytes_out: u64,
    /// How long it was served.
    pub duration_ms: u64,
    /// `ok`, `refused:<code> <reason>`, `ended:<reason>`.
    pub outcome: String,
}

/// The node's exposure registry, as the hub needs it.
pub trait TopicExposures: Send + Sync {
    /// The exposure `id` as served to `peer` now, or why it is not.
    fn serve(&self, id: &str, peer: &str) -> Result<ExposureView, String>;
    /// Record one request (chained by the registry when it is a refusal).
    fn record(&self, r: MeshRecord);
}

/// Hub tunables.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeshConfig {
    /// Readings granted per credit window.
    pub window: u64,
    /// Stream bytes granted per credit window.
    pub window_bytes: u64,
    /// How often a quiet subscription renews its credit.
    pub renew_every: Duration,
    /// How long an advert stays valid without a new one.
    pub advert_ttl: Duration,
    /// Longest wait between resubscribe attempts.
    pub max_backoff: Duration,
    /// Remote subscriptions this node serves in all.
    pub max_served: usize,
    /// Queue length of a source-side reader.
    pub queue_len: usize,
    /// How often the run loop pumps without a wake.
    pub tick: Duration,
}

impl Default for MeshConfig {
    fn default() -> Self {
        Self {
            window: 64,
            window_bytes: 1 << 20,
            renew_every: Duration::from_secs(10),
            advert_ttl: Duration::from_secs(30),
            max_backoff: Duration::from_secs(30),
            max_served: 256,
            queue_len: 256,
            tick: Duration::from_millis(50),
        }
    }
}

/// Both halves of the node's mesh topics.
pub struct MeshTopicHub {
    pub(crate) broker: Arc<Broker>,
    pub(crate) link: Arc<dyn MeshLink>,
    pub(crate) exposures: Arc<dyn TopicExposures>,
    pub(crate) cfg: MeshConfig,
    pub(crate) src: source::Source,
    pub(crate) fetches: fetch::Fetches,
    pub(crate) wake: Arc<Notify>,
}

impl MeshTopicHub {
    /// A hub over `broker`; the broker learns of it so `weftos://`
    /// subscribes reach the mesh.
    pub fn new(
        broker: Arc<Broker>,
        link: Arc<dyn MeshLink>,
        exposures: Arc<dyn TopicExposures>,
        cfg: MeshConfig,
    ) -> Arc<Self> {
        let wake = broker.mesh_wake();
        broker.mesh_configure(&cfg);
        Arc::new(Self { broker, link, exposures, cfg, src: source::Source::default(), fetches: Default::default(), wake })
    }

    /// The broker.
    pub fn broker(&self) -> &Arc<Broker> {
        &self.broker
    }

    /// One frame from `peer`. A peer that is not admitted is ignored without
    /// a reply, as the inference proxy does.
    pub fn on_frame(&self, peer: &PeerInfo, kind: FrameKind, payload: &[u8]) {
        if !peer.admitted {
            tracing::debug!(target: "cogbus", peer = %peer.node_id, "topic frame from a peer that is not admitted; dropped");
            return;
        }
        match kind {
            FrameKind::Request => match wire::decode::<TopicReq>(payload) {
                Ok((req, _)) => self.on_request(&peer.node_id, req),
                Err(e) => tracing::debug!(target: "cogbus", peer = %peer.node_id, error = %e, "bad topic request"),
            },
            FrameKind::Data => match wire::decode::<TopicData>(payload) {
                Ok((d, tail)) => self.on_data(&peer.node_id, d, tail),
                Err(e) => tracing::debug!(target: "cogbus", peer = %peer.node_id, error = %e, "bad topic data"),
            },
        }
        self.wake.notify_one();
    }

    pub(crate) fn send_data(&self, node: &str, d: &TopicData, tail: &[u8]) -> bool {
        self.link.send(node, FrameKind::Data, wire::encode(d, tail))
    }

    pub(crate) fn send_req(&self, node: &str, r: &TopicReq) -> bool {
        self.link.send(node, FrameKind::Request, wire::encode(r, &[]))
    }

    fn on_data(&self, node: &str, d: TopicData, tail: &[u8]) {
        match d {
            TopicData::Advert { topics, ttl_s } => {
                self.broker.mesh_advert(node, topics, Duration::from_secs(ttl_s.clamp(1, 600)));
            }
            TopicData::Reading { sub, seq, t_ns, body } => {
                match self.broker.mesh_reading(node, sub, seq, t_ns, body) {
                    Some(0) => {}
                    Some(n) => {
                        let credit = Credit { n, bytes: 0 };
                        self.send_req(node, &TopicReq::Credit { sub, credit });
                    }
                    // Not ours (any more): tell the source to stop.
                    None => {
                        self.send_req(node, &TopicReq::Unsubscribe { sub });
                    }
                }
            }
            TopicData::End { sub, reason } => self.broker.mesh_ended(node, sub, &reason),
            TopicData::Error { sub, code, message } => {
                self.broker.mesh_ended(node, sub, &format!("refused:{code} {message}"))
            }
            TopicData::Blob { req, size, eof } => self.on_blob(node, req, size, eof, tail),
            TopicData::BlobError { req, code, message } => self.on_blob_error(node, req, &code, &message),
            TopicData::Chunk { .. } => {
                tracing::debug!(target: "cogbus", peer = node, "stream chunk for a stream this node does not carry yet");
            }
        }
    }

    /// Send the subscriber side's due subscribes, renewals and unsubscribes,
    /// and serve the source side's pending readings.
    pub fn pump(&self) {
        for a in self.broker.mesh_due(std::time::Instant::now()) {
            let (node, req) = a;
            self.send_req(&node, &req);
        }
        self.pump_source();
        self.start_fetches();
        self.serve_fetches();
    }

    /// The link to `node` went down: its subscriptions here end and ours
    /// to it resubscribe once it is back.
    pub fn peer_down(&self, node: &str) {
        self.end_peer(node, "link down");
        self.broker.mesh_peer_down(node);
    }

    /// Run [`pump`](Self::pump) on every wake and at least every tick, and
    /// [`announce`](Self::announce) to `peers()` every `advert_every`.
    pub async fn run(self: Arc<Self>, peers: Arc<dyn Fn() -> Vec<PeerInfo> + Send + Sync>, advert_every: Duration) {
        let mut next_advert = tokio::time::Instant::now();
        loop {
            if tokio::time::Instant::now() >= next_advert {
                self.announce(&peers());
                next_advert = tokio::time::Instant::now() + advert_every;
            }
            self.pump();
            tokio::select! {
                _ = self.wake.notified() => {}
                _ = tokio::time::sleep(self.cfg.tick) => {}
            }
        }
    }
}
