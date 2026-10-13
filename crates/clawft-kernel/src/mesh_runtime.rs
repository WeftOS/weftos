//! Mesh runtime orchestrator for WeftOS node-to-node communication (K6).
//!
//! The [`MeshRuntime`] wires together transport connections, serialization
//! via [`MeshIpcEnvelope`], and the local [`A2ARouter`] so that a
//! `RemoteNode` message target actually delivers across the network.

use std::sync::{Arc, Mutex};

use dashmap::DashMap;
use tracing::{debug, warn};

use crate::a2a::A2ARouter;
use crate::error::{KernelError, KernelResult};
use crate::ipc::{KernelMessage, MessagePayload, MessageTarget};
use crate::mesh_assess::AssessmentTransport;
use crate::mesh_chain::{ChainSyncRequest, ChainSyncResponse};
use crate::mesh_discovery::{MeshPeerEvent, MeshPeerEventBus};
use crate::mesh_heartbeat::{ClockSource, HeartbeatConfig, HeartbeatTracker, MeshClockSync};
use crate::mesh_delivery::{LocalDelivery, PeerCtx};
use crate::mesh_ipc::MeshIpcEnvelope;
use crate::mesh_kad::KademliaTable;

/// Control topic carrying signed node facts between peers (card
/// mesh-placement-03). Consumed by the runtime: handed to the installed
/// [`PeerControlSink`], never routed to local subscribers.
pub const FACTS_TOPIC: &str = "mesh.node_facts";

/// Control topic carrying signed artifact revocations (card
/// mesh-placement-25). Handled like [`FACTS_TOPIC`].
pub const REVOKE_TOPIC: &str = "mesh.artifact.revoke";

/// Control topic carrying signed Seed bindings (ADR-106). Handled like
/// [`FACTS_TOPIC`].
pub const COG_BINDING_TOPIC: &str = "mesh.cog.binding";

/// Control topic carrying signed checkout grants and operator approvals
/// (ADR-106). Handled like [`FACTS_TOPIC`].
pub const COG_GRANT_TOPIC: &str = "mesh.cog.grant";

/// Control topic carrying licence catch-up sync requests and responses
/// (ADR-106 section 5.5). Handled like [`FACTS_TOPIC`].
pub const COG_SYNC_TOPIC: &str = "mesh.cog.sync";

/// Control topic carrying inference adverts and forwarded requests between
/// peers (card mesh-placement-19). Handled like [`FACTS_TOPIC`]; see
/// `infer_proxy::hub`.
pub const INFER_TOPIC: &str = "mesh.infer";

/// Control topic carrying liveness ping/pong between verified peers
/// (see [`crate::mesh_liveness`]). Handled like [`FACTS_TOPIC`].
pub const PING_TOPIC: &str = "mesh.ping";

/// Control topics the runtime consumes instead of routing locally.
const CONTROL_TOPICS: [&str; 7] = [
    FACTS_TOPIC,
    REVOKE_TOPIC,
    COG_BINDING_TOPIC,
    COG_GRANT_TOPIC,
    COG_SYNC_TOPIC,
    INFER_TOPIC,
    PING_TOPIC,
];

/// Receiver of a runtime control topic (one of `CONTROL_TOPICS`).
///
/// The sink decides what to trust: it is handed the connection's
/// authenticated identity ([`PeerCtx`]) and returns payloads to send back
/// to the same peer (for example a re-send request).
pub trait PeerControlSink: Send + Sync + 'static {
    /// Handle one control payload from `ctx.peer_id`; return replies.
    ///
    /// `conn` identifies the connection the frame arrived on (0 when the
    /// caller has none): one connection can claim many source ids while
    /// unverified, so per-connection limits key on this, not on `ctx`.
    fn on_peer_control(
        &self,
        ctx: &PeerCtx,
        conn: u64,
        payload: &serde_json::Value,
    ) -> Vec<serde_json::Value>;
}

/// A handle to a connected peer, holding the sender half of an mpsc
/// channel whose receiver is read by a background write loop.
pub struct PeerConnection {
    /// Remote node identifier.
    pub node_id: String,
    /// When the connection was established.
    pub connected_at: chrono::DateTime<chrono::Utc>,
    /// Sender for outbound serialized messages.
    pub sender: tokio::sync::mpsc::Sender<Vec<u8>>,
    /// True when this route was registered by a connection whose node id
    /// admission verified. A verified route cannot be taken over by an
    /// unverified connection claiming the same id.
    pub verified: bool,
    /// The admitted class of the connection (`Node`, `Leaf`, ...); `Legacy`
    /// for a route no admission verdict classed.
    pub class: crate::mesh_admit::PeerClass,
    /// Live-route accounting for the serving connection (None for routes
    /// added without one). Dropping the route, by removal or replacement,
    /// decrements it.
    _tally: Option<RouteGuard>,
}

/// What [`MeshRuntime::peer_details`] reports for one connected peer.
#[derive(Debug, Clone)]
pub struct PeerDetail {
    /// Remote node identifier.
    pub node_id: String,
    /// Admitted class of the connection.
    pub class: crate::mesh_admit::PeerClass,
    /// Admission verified the node id.
    pub verified: bool,
    /// Verified and class `node` (see [`MeshRuntime::peer_licensed`]).
    pub licensed: bool,
    /// When the connection was established.
    pub connected_at: chrono::DateTime<chrono::Utc>,
    /// Heartbeat tracker's view, when discovery is attached and tracks it.
    pub heartbeat: Option<crate::mesh_heartbeat::HeartbeatState>,
    /// Last counted liveness pong from this peer (real liveness), when the
    /// liveness service runs and the peer has answered.
    pub last_seen: Option<std::time::SystemTime>,
    /// Smoothed ping round-trip time in milliseconds, same condition.
    pub rtt_ms: Option<f64>,
    /// Pings that timed out since the last counted pong.
    pub missed_pongs: Option<u32>,
    /// Load the peer attached to its last pong (peer-claimed, unsigned).
    pub load: Option<crate::mesh_load::LoadSample>,
}

/// Count of live routes registered by one serving connection. Lets the
/// connection learn in O(1), instead of scanning the peer map, that its
/// routes were removed (`disconnect_peer`, revocation) or replaced.
#[derive(Clone, Default)]
pub struct RouteTally(Arc<std::sync::atomic::AtomicUsize>);

impl RouteTally {
    /// Routes currently pointing at this connection.
    pub fn live(&self) -> usize {
        self.0.load(std::sync::atomic::Ordering::Acquire)
    }
}

/// Held by a `PeerConnection`; decrements its tally when the route drops.
struct RouteGuard(Arc<std::sync::atomic::AtomicUsize>);

impl RouteGuard {
    fn new(t: &RouteTally) -> Self {
        t.0.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        Self(Arc::clone(&t.0))
    }
}

impl Drop for RouteGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

/// Discovery state for mesh peer lookup and health tracking.
pub struct DiscoveryState {
    /// Kademlia routing table for peer lookup.
    pub kademlia: Mutex<KademliaTable>,
    /// Known peer addresses: node_id -> socket addr string.
    pub peer_addresses: DashMap<String, String>,
    /// Heartbeat tracker for failure detection.
    pub heartbeat: Mutex<HeartbeatTracker>,
}

/// Admission material owned by this runtime. Nested seed dials must use this
/// gate after verifying a reciprocal hello; a configured node-id is not proof.
pub struct MeshAuthentication {
    pub gate: Arc<dyn crate::mesh_admit::AdmissionGate>,
    pub identity: Arc<crate::mesh_admit::DialIdentity>,
    pub noise_static: [u8; 32],
    pub require_authenticated_seeds: bool,
}

/// The mesh runtime orchestrates transport, connections, and message bridging.
///
/// It maintains a set of active peer connections (keyed by node ID) and
/// provides the plumbing to:
/// 1. Send a [`MeshIpcEnvelope`] to a connected peer.
/// 2. Receive an envelope from a peer and inject it into the local
///    [`A2ARouter`].
pub struct MeshRuntime {
    /// Installed ADR-103 leaf certificate and replay gate.
    leaf_ingress: std::sync::OnceLock<Arc<crate::mesh_leaf::LeafIngress>>,
    /// Configured leaf bind (absent: derived from the main listener).
    leaf_bind_specs: std::sync::OnceLock<Vec<crate::mesh_leaf_bind::LeafBindSpec>>,
    /// Where the leaf listeners actually bound, in the order they came up.
    leaf_bound: std::sync::Mutex<Vec<crate::mesh_leaf_bind::LeafBind>>,
    /// Milliseconds between retries of a leaf address that failed to bind.
    leaf_bind_retry_ms: std::sync::atomic::AtomicU64,
    authentication: std::sync::OnceLock<MeshAuthentication>,
    /// Local node identifier.
    node_id: String,
    /// Active peer connections: node_id -> PeerConnection.
    peers: DashMap<String, PeerConnection>,
    /// Reference to the local A2A router for injecting remote messages.
    local_router: Option<Arc<dyn LocalDelivery>>,
    /// Optional discovery state (Kademlia + heartbeat).
    discovery: Option<DiscoveryState>,
    /// Mesh time synchronization state.
    clock: std::sync::Mutex<MeshClockSync>,
    /// Late-bound chain manager. When set (via
    /// [`set_chain_manager`]), every successful `handle_incoming`
    /// appends a `peer.envelope` event to the ExoChain so mesh
    /// activity is auditable via `weaver chain local`.
    #[cfg(feature = "exochain")]
    chain_manager: std::sync::OnceLock<Arc<crate::chain::ChainManager>>,
    /// Peer topic subscription registry: topic → set of peer node IDs.
    ///
    /// Populated when a peer sends a `mesh.subscribe` control envelope.
    /// Consulted by the A2A router's Topic handler to forward published
    /// messages to remote nodes that subscribed to the same topic.
    mesh_subscriptions: DashMap<String, Vec<String>>,
    /// Assessment mesh transport (WEFT-117 / K6.6). When set, inbound
    /// `FrameType::AssessmentSync` frames are demuxed into the transport
    /// instead of being treated as MeshIpcEnvelope JSON.
    assessment_transport: std::sync::OnceLock<Arc<AssessmentTransport>>,
    /// Peer join/leave/health event bus (WEFT-120).
    ///
    /// `ClusterService` / `ClusterMembership` subscribe so cluster
    /// membership tracks live mesh state.
    peer_events: MeshPeerEventBus,
    /// Sinks for control topics (`mesh.node_facts`, `mesh.artifact.revoke`).
    control_sinks: DashMap<String, Arc<dyn PeerControlSink>>,
    /// Connection ids for control-topic limits, keyed by outbound channel.
    conn_ids: Mutex<Vec<(tokio::sync::mpsc::WeakSender<Vec<u8>>, u64)>>,
    conn_seq: std::sync::atomic::AtomicU64,
    /// Whether admission is `enforce`. Only then does a peer whose node id
    /// was not verified stop counting as a cluster member; under `observe`
    /// and `off` (the shipped default is `observe`) every peer counts, as
    /// before. Set by the owner of the admission mode and updated when the
    /// mode changes.
    enforcing: std::sync::atomic::AtomicBool,
    /// Liveness ping/pong service, once started ([`MeshRuntime::start_liveness`]).
    liveness: std::sync::OnceLock<Arc<crate::mesh_liveness::Liveness>>,
}

impl MeshRuntime {
    /// Install the service-owned leaf gate before the listener starts.
    pub fn set_leaf_ingress(&self, ingress: Arc<crate::mesh_leaf::LeafIngress>) -> Result<(), Arc<crate::mesh_leaf::LeafIngress>> {
        self.leaf_ingress.set(ingress)
    }

    pub fn leaf_ingress(&self) -> Option<&Arc<crate::mesh_leaf::LeafIngress>> {
        self.leaf_ingress.get()
    }

    /// Install the configured leaf bind before the listener starts.
    pub fn set_leaf_bind_spec(&self, spec: crate::mesh_leaf_bind::LeafBindSpec) -> bool {
        self.set_leaf_bind_specs(vec![spec])
    }

    /// Install every configured leaf bind (one listener each).
    pub fn set_leaf_bind_specs(&self, specs: Vec<crate::mesh_leaf_bind::LeafBindSpec>) -> bool {
        self.leaf_bind_specs.set(specs).is_ok()
    }

    /// The configured leaf binds; empty means derive from the main listener.
    pub fn leaf_bind_specs(&self) -> &[crate::mesh_leaf_bind::LeafBindSpec] {
        self.leaf_bind_specs.get().map(Vec::as_slice).unwrap_or(&[])
    }

    /// The first bound certified-leaf TCP and discovery pair, or `None`
    /// when no leaf listener is running.
    pub fn leaf_bound(&self) -> Option<crate::mesh_leaf_bind::LeafBind> {
        self.leaf_bounds().into_iter().next()
    }

    /// Every bound certified-leaf listener.
    pub fn leaf_bounds(&self) -> Vec<crate::mesh_leaf_bind::LeafBind> {
        self.leaf_bound.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// How long a leaf address that failed to bind waits before the next try.
    pub(crate) fn leaf_bind_retry(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.leaf_bind_retry_ms.load(std::sync::atomic::Ordering::Relaxed))
    }

    #[cfg(test)]
    pub(crate) fn set_leaf_bind_retry(&self, d: std::time::Duration) {
        self.leaf_bind_retry_ms.store(d.as_millis() as u64, std::sync::atomic::Ordering::Relaxed);
    }

    pub(crate) fn note_leaf_bound(&self, bound: crate::mesh_leaf_bind::LeafBind) {
        let mut v = self.leaf_bound.lock().unwrap_or_else(|e| e.into_inner());
        if !v.contains(&bound) {
            v.push(bound);
        }
    }
    pub fn set_authentication(&self, auth: MeshAuthentication) -> bool {
        self.authentication.set(auth).is_ok()
    }

    pub fn authentication(&self) -> Option<&MeshAuthentication> {
        self.authentication.get()
    }

    /// Start the liveness ping/pong between verified peers (idempotent).
    pub fn start_liveness(self: &Arc<Self>, cfg: crate::mesh_liveness::LivenessConfig) {
        let lv = crate::mesh_liveness::Liveness::new(self, cfg);
        if self.liveness.set(lv.clone()).is_ok() {
            lv.start();
        }
    }

    /// The liveness service, when started.
    pub fn liveness(&self) -> Option<&Arc<crate::mesh_liveness::Liveness>> {
        self.liveness.get()
    }

    /// Say whether admission is `enforce` (see the field). Takes effect for
    /// the peer events emitted after the call.
    pub fn set_enforcing(&self, enforcing: bool) {
        self.enforcing.store(enforcing, std::sync::atomic::Ordering::SeqCst);
    }

    /// Whether admission is `enforce`.
    pub fn enforcing(&self) -> bool {
        self.enforcing.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Whether a connection counts as a cluster member: admission verified
    /// its node id, or admission is not enforcing.
    fn counts_as_member(&self, verified: bool) -> bool {
        verified || !self.enforcing()
    }

    /// Create a new mesh runtime for the given local node.
    pub fn new(node_id: String) -> Self {
        Self {
            leaf_ingress: std::sync::OnceLock::new(),
            leaf_bind_specs: std::sync::OnceLock::new(),
            leaf_bound: std::sync::Mutex::new(Vec::new()),
            leaf_bind_retry_ms: std::sync::atomic::AtomicU64::new(15_000),
            authentication: std::sync::OnceLock::new(),
            node_id,
            peers: DashMap::new(),
            local_router: None,
            discovery: None,
            clock: std::sync::Mutex::new(MeshClockSync::new(ClockSource::Local)),
            #[cfg(feature = "exochain")]
            chain_manager: std::sync::OnceLock::new(),
            mesh_subscriptions: DashMap::new(),
            assessment_transport: std::sync::OnceLock::new(),
            peer_events: MeshPeerEventBus::new(),
            control_sinks: DashMap::new(),
            conn_ids: Mutex::new(Vec::new()),
            conn_seq: std::sync::atomic::AtomicU64::new(0),
            enforcing: std::sync::atomic::AtomicBool::new(false),
            liveness: std::sync::OnceLock::new(),
        }
    }

    /// Create a mesh runtime with discovery state initialized.
    ///
    /// The `kademlia_id` is used as the local key in the Kademlia routing
    /// table. Heartbeat tracking starts with default configuration.
    pub fn with_discovery(node_id: String, kademlia_id: [u8; 32]) -> Self {
        Self {
            leaf_ingress: std::sync::OnceLock::new(),
            leaf_bind_specs: std::sync::OnceLock::new(),
            leaf_bound: std::sync::Mutex::new(Vec::new()),
            leaf_bind_retry_ms: std::sync::atomic::AtomicU64::new(15_000),
            authentication: std::sync::OnceLock::new(),
            node_id,
            peers: DashMap::new(),
            local_router: None,
            discovery: Some(DiscoveryState {
                kademlia: Mutex::new(KademliaTable::new(kademlia_id)),
                peer_addresses: DashMap::new(),
                heartbeat: Mutex::new(HeartbeatTracker::new(HeartbeatConfig::default())),
            }),
            clock: std::sync::Mutex::new(MeshClockSync::new(ClockSource::Local)),
            #[cfg(feature = "exochain")]
            chain_manager: std::sync::OnceLock::new(),
            mesh_subscriptions: DashMap::new(),
            assessment_transport: std::sync::OnceLock::new(),
            peer_events: MeshPeerEventBus::new(),
            control_sinks: DashMap::new(),
            conn_ids: Mutex::new(Vec::new()),
            conn_seq: std::sync::atomic::AtomicU64::new(0),
            enforcing: std::sync::atomic::AtomicBool::new(false),
            liveness: std::sync::OnceLock::new(),
        }
    }

    /// Stable id of the connection behind `outbound` (1-based; 0 means none).
    fn conn_id(&self, outbound: &tokio::sync::mpsc::Sender<Vec<u8>>) -> u64 {
        let mut ids = self.conn_ids.lock().unwrap_or_else(|p| p.into_inner());
        ids.retain(|(w, _)| w.upgrade().is_some());
        if let Some((_, id)) = ids
            .iter()
            .find(|(w, _)| w.upgrade().is_some_and(|t| t.same_channel(outbound)))
        {
            return *id;
        }
        let id = self.conn_seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        ids.push((outbound.downgrade(), id));
        id
    }

    /// Install the sink for the control topic `topic` (first call wins).
    /// Only the topics in `CONTROL_TOPICS` are control topics.
    pub fn set_control_sink(&self, topic: &str, sink: Arc<dyn PeerControlSink>) {
        self.control_sinks.entry(topic.to_string()).or_insert(sink);
    }

    /// Subscribe to live mesh peer membership / health events (WEFT-120).
    pub fn subscribe_peer_events(&self) -> tokio::sync::broadcast::Receiver<MeshPeerEvent> {
        self.peer_events.subscribe()
    }

    /// Shared peer-event bus (for boot wiring and tests).
    pub fn peer_event_bus(&self) -> &MeshPeerEventBus {
        &self.peer_events
    }

    /// Publish a peer event (used by external discovery / tests).
    pub fn emit_peer_event(&self, event: MeshPeerEvent) {
        self.peer_events.emit(event);
    }

    /// Attach the assessment transport used for AssessmentSync demux (WEFT-117).
    ///
    /// Idempotent (first call wins). Safe after the runtime is wrapped in
    /// [`Arc`] so boot can wire the transport after mesh listener spawn.
    pub fn set_assessment_transport(&self, transport: Arc<AssessmentTransport>) {
        let _ = self.assessment_transport.set(transport);
    }

    /// Shared assessment transport, if registered.
    pub fn assessment_transport(&self) -> Option<Arc<AssessmentTransport>> {
        self.assessment_transport.get().cloned()
    }

    /// Broadcast raw encoded frame bytes to every connected peer.
    ///
    /// Used by the assessment gossip/diff push loop. Failures on individual
    /// peers are logged and skipped (best-effort fan-out).
    pub async fn broadcast_raw(&self, data: &[u8]) -> usize {
        let mut sent = 0usize;
        for entry in self.peers.iter() {
            let peer_id = entry.key().clone();
            let sender = entry.sender.clone();
            match sender.send(data.to_vec()).await {
                Ok(()) => sent += 1,
                Err(_) => {
                    warn!(peer = %peer_id, "assessment broadcast: peer send channel closed");
                }
            }
        }
        sent
    }

    /// Drain pending assessment broadcast (diff/gossip) and push to all peers.
    ///
    /// Returns the number of peers that received the frame, or 0 if nothing
    /// was pending / transport not wired.
    pub async fn push_pending_assessment(&self) -> usize {
        let Some(transport) = self.assessment_transport.get() else {
            return 0;
        };
        let Some(bytes) = transport.drain_pending() else {
            return 0;
        };
        self.broadcast_raw(&bytes).await
    }

    /// Drive one assessment gossip tick using the published local report.
    pub async fn assessment_gossip_tick(&self) -> usize {
        let Some(transport) = self.assessment_transport.get() else {
            return 0;
        };
        let report = transport.latest_report();
        let Some(bytes) = transport.gossip_tick(report.as_ref()) else {
            return 0;
        };
        self.broadcast_raw(&bytes).await
    }

    /// Return this node's identifier.
    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    /// Attach the local A2A router for incoming message injection.
    pub fn set_local_router(&mut self, router: Arc<A2ARouter>) {
        self.local_router = Some(router);
    }

    /// Attach any [`LocalDelivery`] sink for inbound messages (a mesh
    /// service delivers to tenants instead of an in-kernel router).
    pub fn set_local_delivery(&mut self, delivery: Arc<dyn LocalDelivery>) {
        self.local_router = Some(delivery);
    }

    // ── Peer topic subscription registry ─────────────────────────

    /// Register a remote peer as a subscriber for the given topic.
    ///
    /// Called from [`handle_incoming`] when a `mesh.subscribe` control
    /// envelope is received. De-duplicates: if the peer is already
    /// registered for this topic, the call is a no-op.
    pub fn register_peer_topic(&self, topic: &str, peer_node_id: &str) {
        let mut entry = self
            .mesh_subscriptions
            .entry(topic.to_string())
            .or_default();
        if !entry.contains(&peer_node_id.to_string()) {
            entry.push(peer_node_id.to_string());
            debug!(topic, peer = %peer_node_id, "registered peer topic subscription");
        }
    }

    /// Remove a peer's subscription for the given topic.
    ///
    /// Called when a peer disconnects or sends an explicit unsubscribe.
    pub fn unregister_peer_topic(&self, topic: &str, peer_node_id: &str) {
        if let Some(mut subs) = self.mesh_subscriptions.get_mut(topic) {
            subs.retain(|id| id != peer_node_id);
        }
    }

    /// Remove all topic subscriptions for the given peer.
    ///
    /// Called when `disconnect_peer` is invoked so stale entries are
    /// cleaned up and future publishes don't attempt dead channels.
    pub fn unregister_all_peer_topics(&self, peer_node_id: &str) {
        for mut entry in self.mesh_subscriptions.iter_mut() {
            entry.retain(|id| id != peer_node_id);
        }
    }

    /// Return the list of peer node IDs subscribed to the given topic.
    ///
    /// Used by the A2A router's Topic handler to forward published
    /// messages to remote nodes.
    pub fn peers_for_topic(&self, topic: &str) -> Vec<String> {
        self.mesh_subscriptions
            .get(topic)
            .map(|subs| subs.clone())
            .unwrap_or_default()
    }

    /// Attach the ExoChain manager for auditable peer-envelope logging.
    ///
    /// Idempotent (first call wins; subsequent calls are no-ops). Uses
    /// interior mutability so this can be called after the runtime has
    /// been wrapped in [`Arc`], which matches the boot-sequence ordering
    /// where the chain manager is constructed after the mesh listener.
    #[cfg(feature = "exochain")]
    pub fn set_chain_manager(&self, cm: Arc<crate::chain::ChainManager>) {
        let _ = self.chain_manager.set(cm);
    }

    /// Register a peer connection using an already-established channel.
    ///
    /// This is the low-level entry point used after a TCP (or other
    /// transport) connection has been set up and the node-ID exchange
    /// has completed. Higher-level helpers like `connect_peer` build
    /// on top of this.
    pub fn add_peer(&self, node_id: String, sender: tokio::sync::mpsc::Sender<Vec<u8>>) {
        self.register_peer(node_id, sender, false, None);
    }

    /// True when `node_id` is routed through some channel other than `tx`.
    pub fn route_is_foreign(&self, node_id: &str, tx: &tokio::sync::mpsc::Sender<Vec<u8>>) -> bool {
        self.peers.get(node_id).is_some_and(|p| !p.sender.same_channel(tx))
    }

    /// [`add_peer`](Self::add_peer) that counts the route in `tally`, so the
    /// owning connection can tell in O(1) when it is removed or replaced.
    pub fn add_peer_tallied(
        &self,
        node_id: String,
        sender: tokio::sync::mpsc::Sender<Vec<u8>>,
        tally: &RouteTally,
    ) {
        self.register_peer(node_id, sender, false, Some(tally));
    }

    /// Register the route of a connection whose node id admission just
    /// authenticated, before any envelope arrives, so the peer joins the
    /// cluster when its handshake completes rather than on its first
    /// application frame. `verified` is the admission verdict (enforce);
    /// returns false when refused (see [`register_peer`](Self::register_peer)).
    /// Test-only: production registers with the admitted class
    /// ([`register_authenticated_as`](Self::register_authenticated_as)).
    #[cfg(test)]
    pub fn register_authenticated(
        &self,
        node_id: String,
        sender: tokio::sync::mpsc::Sender<Vec<u8>>,
        verified: bool,
        tally: &RouteTally,
    ) -> bool {
        self.register_authenticated_as(node_id, sender, verified, crate::mesh_admit::PeerClass::Node, tally)
    }

    /// [`register_authenticated`](Self::register_authenticated) with the
    /// admitted class of the connection, so outbound decisions that need a
    /// full node ([`peer_licensed`](Self::peer_licensed)) can tell a leaf.
    pub fn register_authenticated_as(
        &self,
        node_id: String,
        sender: tokio::sync::mpsc::Sender<Vec<u8>>,
        verified: bool,
        class: crate::mesh_admit::PeerClass,
        tally: &RouteTally,
    ) -> bool {
        self.register_peer_classed(node_id, sender, verified, class, Some(tally))
    }

    /// Register or refresh a route. Returns false when refused: an
    /// unverified connection may not replace a verified peer's route.
    ///
    /// Re-registering the *same* channel (every inbound envelope does) is
    /// a no-op; only a genuinely new channel replaces the route and emits
    /// `Recovered`.
    fn register_peer(
        &self,
        node_id: String,
        sender: tokio::sync::mpsc::Sender<Vec<u8>>,
        verified: bool,
        tally: Option<&RouteTally>,
    ) -> bool {
        let class = if verified {
            crate::mesh_admit::PeerClass::Node
        } else {
            crate::mesh_admit::PeerClass::Legacy
        };
        self.register_peer_classed(node_id, sender, verified, class, tally)
    }

    fn register_peer_classed(
        &self,
        node_id: String,
        sender: tokio::sync::mpsc::Sender<Vec<u8>>,
        verified: bool,
        class: crate::mesh_admit::PeerClass,
        tally: Option<&RouteTally>,
    ) -> bool {
        use dashmap::mapref::entry::Entry;
        debug!(peer = %node_id, "adding peer connection");
        let address = self
            .discovery
            .as_ref()
            .and_then(|d| d.peer_addresses.get(&node_id).map(|a| a.value().clone()));
        let conn = PeerConnection {
            node_id: node_id.clone(),
            connected_at: chrono::Utc::now(),
            sender,
            verified,
            class,
            _tally: tally.map(RouteGuard::new),
        };
        // Decide and write under the entry (shard) lock so an unverified
        // registration can never overwrite a verified one in a race.
        let was_known = match self.peers.entry(node_id.clone()) {
            Entry::Occupied(mut o) => {
                if o.get().sender.same_channel(&conn.sender) {
                    return true;
                }
                if o.get().verified && !verified {
                    warn!(peer = %node_id, "refusing route takeover of an admitted peer by an unverified connection");
                    return false;
                }
                if !verified {
                    // Unauthenticated claims can displace each other; a
                    // legitimate reconnect looks identical, so this is
                    // logged, not refused (ADR-103 A10: observe is not
                    // protection).
                    warn!(peer = %node_id,
                        "unverified connection replaced an unverified route; the previous connection will be closed");
                }
                o.insert(conn);
                true
            }
            Entry::Vacant(v) => {
                v.insert(conn);
                false
            }
        };
        // WEFT-120: reconnected peers emit Recovered; first connect → Joined.
        if was_known {
            self.peer_events.emit(MeshPeerEvent::Recovered {
                node_id,
                address,
                verified: self.counts_as_member(verified),
            });
        } else {
            self.peer_events.emit(MeshPeerEvent::Joined {
                node_id,
                address,
                platform: None,
                verified: self.counts_as_member(verified),
            });
        }
        true
    }

    /// Remove every route that still points at `tx` (a closed connection's
    /// outbound channel). Routes since replaced by a newer connection are
    /// left alone; `Left` is emitted only for routes actually removed.
    /// Returns the number removed.
    /// True when some route currently sends through `tx`'s channel.
    /// O(peers): diagnostics and tests only; connections use [`RouteTally`].
    pub fn routes_via(&self, tx: &tokio::sync::mpsc::Sender<Vec<u8>>) -> bool {
        self.peers.iter().any(|e| e.sender.same_channel(tx))
    }

    pub fn disconnect_channel(&self, tx: &tokio::sync::mpsc::Sender<Vec<u8>>) -> usize {
        let ids: Vec<String> = self
            .peers
            .iter()
            .filter(|e| e.sender.same_channel(tx))
            .map(|e| e.key().clone())
            .collect();
        let mut n = 0;
        for id in ids {
            if let Some((_, conn)) = self.peers.remove_if(&id, |_, p| p.sender.same_channel(tx)) {
                // Admitted routes lose their subscriptions with the
                // connection. Unadmitted (legacy) peers keep theirs, as
                // before: a leaf that reconnects must not have to
                // re-subscribe.
                if conn.verified {
                    self.unregister_all_peer_topics(&id);
                }
                self.peer_events.emit(MeshPeerEvent::Left { node_id: id });
                n += 1;
            }
        }
        n
    }

    /// Send a [`MeshIpcEnvelope`] to a connected peer.
    ///
    /// Serializes the envelope to JSON bytes and pushes them into the
    /// peer's outbound channel. Returns an error if the peer is not
    /// connected or the channel is closed/full.
    pub async fn send_to_peer(&self, node_id: &str, envelope: MeshIpcEnvelope) -> KernelResult<()> {
        let peer = self
            .peers
            .get(node_id)
            .ok_or_else(|| KernelError::Mesh(format!("peer not connected: {node_id}")))?;

        let bytes = envelope
            .to_bytes()
            .map_err(|e| KernelError::Mesh(format!("serialization error: {e}")))?;

        peer.sender
            .send(bytes)
            .await
            .map_err(|_| KernelError::Mesh(format!("send channel closed for peer {node_id}")))?;

        debug!(peer = %node_id, "sent envelope to peer");
        Ok(())
    }

    /// Build and send a message to a remote node.
    ///
    /// Wraps a [`KernelMessage`] in a [`MeshIpcEnvelope`] with the
    /// correct source/dest and sends it to the named peer.
    pub async fn route_to_remote(&self, node_id: &str, message: KernelMessage) -> KernelResult<()> {
        let envelope = MeshIpcEnvelope::new(self.node_id.clone(), node_id.to_string(), message);
        self.send_to_peer(node_id, envelope).await
    }

    /// Handle incoming raw bytes from a peer.
    ///
    /// First attempts AssessmentSync demux via the registered
    /// [`AssessmentTransport`] (WEFT-117). Non-assessment traffic is
    /// deserialized as a [`MeshIpcEnvelope`] and injected into the
    /// local A2A router. The message target is unwrapped from
    /// `RemoteNode` if present so that the local router delivers to
    /// the correct local process.
    pub async fn handle_incoming(&self, data: &[u8]) -> KernelResult<()> {
        if self.try_handle_assessment(data, None).await? {
            return Ok(());
        }
        let envelope = MeshIpcEnvelope::from_bytes(data)
            .map_err(|e| KernelError::Mesh(format!("deserialization error: {e}")))?;
        let ctx = PeerCtx::unauthenticated(envelope.source_node.clone());
        self.handle_envelope(envelope, &ctx, 0).await
    }

    /// Handle incoming bytes while auto-registering the sending peer.
    ///
    /// Used by the mesh accept loop: the kernel doesn't know the peer's
    /// node ID until the first envelope arrives, so we opportunistically
    /// register `envelope.source_node → outbound` before dispatching.
    /// Always (re)registers the channel: on reconnect the node_id is
    /// unchanged but the TCP connection — and thus the outbound channel
    /// — is new, and a stale entry would route every push to the dead
    /// channel of the previous connection. This is what wires
    /// `A2ARouter` topic forwarding to inbound leaf subscribers (they
    /// send `mesh.subscribe`, then the kernel can call `send_to_peer`
    /// back over this connection).
    pub async fn handle_incoming_from(
        &self,
        data: &[u8],
        outbound: tokio::sync::mpsc::Sender<Vec<u8>>,
    ) -> KernelResult<()> {
        self.handle_incoming_peer(data, outbound, None).await
    }

    /// [`handle_incoming_from`](Self::handle_incoming_from) with the
    /// connection's authenticated identity.
    ///
    /// With `peer = Some(ctx)` and `ctx.node_verified`, the route is
    /// registered under `ctx.peer_id` (never under the envelope's claim),
    /// an envelope whose `source_node` differs is rejected, and the
    /// route cannot later be taken over by an unverified connection. With
    /// `None` (or an unverified ctx) behaviour is the pre-admission one:
    /// the peer is registered under its claimed `source_node`, with a
    /// warning the first time that id is seen.
    pub async fn handle_incoming_peer(
        &self,
        data: &[u8],
        outbound: tokio::sync::mpsc::Sender<Vec<u8>>,
        peer: Option<&PeerCtx>,
    ) -> KernelResult<()> {
        self.handle_incoming_tallied(data, outbound, peer, None).await
    }

    /// [`handle_incoming_peer`](Self::handle_incoming_peer) that also counts
    /// the routes it registers in `tally` (the serving connection's O(1)
    /// "is my route still there" signal).
    pub async fn handle_incoming_tallied(
        &self,
        data: &[u8],
        outbound: tokio::sync::mpsc::Sender<Vec<u8>>,
        peer: Option<&PeerCtx>,
        tally: Option<&RouteTally>,
    ) -> KernelResult<()> {
        // AssessmentSync frames do not carry MeshIpcEnvelope source_node
        // for auto-registration. Demux first; reply (if any) goes on the
        // same outbound channel. Peer registration for assessment-only
        // peers happens when the first IPC envelope arrives, or via
        // explicit add_peer (seed / test harness).
        if self
            .try_handle_assessment(data, Some(outbound.clone()))
            .await?
        {
            return Ok(());
        }

        let envelope = MeshIpcEnvelope::from_bytes(data)
            .map_err(|e| KernelError::Mesh(format!("deserialization error: {e}")))?;

        let mut ctx = match peer {
            Some(c) => c.clone(),
            None => PeerCtx::unauthenticated(envelope.source_node.clone()),
        };
        if ctx.node_verified && envelope.source_node != ctx.peer_id {
            return Err(KernelError::Mesh(format!(
                "source_node {} does not match admitted node {}",
                envelope.source_node, ctx.peer_id
            )));
        }
        if !ctx.node_verified {
            // No authenticated identity: the claim is all there is.
            ctx.peer_id = envelope.source_node.clone();
            if !self.peers.contains_key(&ctx.peer_id) {
                warn!(peer = %ctx.peer_id,
                    "registering peer under an unauthenticated source_node claim");
            }
        }
        ctx.src_scope = envelope.src_scope.clone();

        // Always (re)register the peer's outbound channel. A leaf that
        // reconnects (daemon restart, wifi blip, socket idle-timeout)
        // keeps the same node_id but gets a fresh channel — the stale
        // entry must be replaced or `send_to_peer` delivers into the
        // dead channel of the dropped connection and the peer never
        // receives anything again.
        let conn = self.conn_id(&outbound);
        // The class rides with the route so a re-registered leaf (after
        // `remove_dead_peers` dropped it) stays a leaf, never a node.
        let class = if ctx.node_verified { ctx.class } else { crate::mesh_admit::PeerClass::Legacy };
        if !self.register_peer_classed(ctx.peer_id.clone(), outbound, ctx.node_verified, class, tally) {
            return Err(KernelError::Mesh(format!(
                "route for {} belongs to an admitted peer",
                ctx.peer_id
            )));
        }

        self.handle_envelope(envelope, &ctx, conn).await
    }

    /// Demux AssessmentSync frames into the registered transport.
    ///
    /// Returns `Ok(true)` when the bytes were assessment traffic (handled),
    /// `Ok(false)` when the caller should continue with IPC envelope path.
    async fn try_handle_assessment(
        &self,
        data: &[u8],
        reply_tx: Option<tokio::sync::mpsc::Sender<Vec<u8>>>,
    ) -> KernelResult<bool> {
        let Some(transport) = self.assessment_transport.get() else {
            return Ok(false);
        };
        match transport.try_handle_raw(data)? {
            Some(maybe_reply) => {
                if let Some(reply) = maybe_reply {
                    if let Some(tx) = reply_tx {
                        let _ = tx.send(reply).await;
                    } else if let Some(source) = AssessmentTransport::try_extract_payload(data)
                        .ok()
                        .flatten()
                        .and_then(|p| {
                            crate::mesh_assess::AssessmentEnvelope::from_bytes(&p)
                                .ok()
                                .map(|e| e.source_node)
                        })
                    {
                        // Prefer routing the reply to the known peer by source_node.
                        if let Some(peer) = self.peers.get(&source) {
                            let _ = peer.sender.send(reply).await;
                        }
                    }
                }
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn handle_envelope(
        &self,
        envelope: MeshIpcEnvelope,
        ctx: &PeerCtx,
        conn: u64,
    ) -> KernelResult<()> {
        debug!(
            from_node = %envelope.source_node,
            dest_node = %envelope.dest_node,
            "received mesh envelope"
        );

        // Auditable record: append a peer.envelope event to the local
        // chain before routing. Captures source_node (leaf identity),
        // dest_node, envelope_id, and the resolved topic/service when
        // known. The chain append is best-effort — a failure here must
        // not block message delivery.
        #[cfg(feature = "exochain")]
        if let Some(cm) = self.chain_manager.get() {
            let topic = match &envelope.message.target {
                MessageTarget::Topic(t) => Some(t.clone()),
                MessageTarget::Service(s) => Some(format!("service:{s}")),
                MessageTarget::ServiceMethod { service, method } => {
                    Some(format!("service:{service}#{method}"))
                }
                MessageTarget::RemoteNode { target, .. } => match target.as_ref() {
                    MessageTarget::Topic(t) => Some(t.clone()),
                    _ => None,
                },
                _ => None,
            };
            let payload = serde_json::json!({
                "source_node": envelope.source_node,
                "dest_node": envelope.dest_node,
                "envelope_id": envelope.envelope_id,
                "topic": topic,
                "hop_count": envelope.hop_count,
            });
            let _ = cm.append("mesh", "peer.envelope", Some(payload));
        }

        // Unwrap the RemoteNode wrapper so the local router sees the
        // inner target (Process, Service, Topic, etc.).
        let dest_scope = envelope.dest_scope;
        let mut message = envelope.message;
        if let MessageTarget::RemoteNode { target, .. } = message.target {
            message.target = *target;
        }

        // Intercept mesh.subscribe control envelopes before local routing.
        //
        // A peer sends `Topic("mesh.subscribe")` with a JSON payload of the
        // form `{"topic": "<topic-name>"}` to register interest. We record
        // the subscription and consume the message — it does not propagate
        // to the local router or any local subscribers. This is handled
        // before the local-router lookup: a subscribe is consumed here and
        // never routed, so it must succeed even on a runtime with no router
        // attached (e.g. a pure leaf-push relay, or before the kernel wires
        // its router in).
        if let MessageTarget::Topic(ref ctrl_topic) = message.target
            && ctrl_topic == "mesh.subscribe"
        {
            if let MessagePayload::Json(ref payload) = message.payload
                && let Some(topic) = payload.get("topic").and_then(|v| v.as_str())
            {
                // Same admitted-identity path as delivery: the subscriber
                // is `ctx.peer_id`, and a tenant-aware sink may veto.
                if let Some(router) = self.local_router.as_ref()
                    && !router
                        .authorize_subscribe(ctx, topic, dest_scope.as_ref())
                        .await
                {
                    warn!(from = %ctx.peer_id, topic, "mesh.subscribe refused by local delivery");
                    if ctx.node_verified && ctx.class == crate::mesh_admit::PeerClass::Leaf {
                        return Err(KernelError::Mesh("certified leaf subscription scope refused".into()));
                    }
                    return Ok(());
                }
                self.register_peer_topic(topic, &ctx.peer_id);
                return Ok(());
            }
            // Malformed subscribe — drop silently rather than routing to
            // local subscribers who have no idea what to do with it.
            warn!(
                from = %envelope.source_node,
                "received malformed mesh.subscribe envelope (missing 'topic' field)"
            );
            return Ok(());
        }

        // Control topics are handed to their sink with the connection's
        // authenticated identity and never routed.
        if let MessageTarget::Topic(ref t) = message.target
            && CONTROL_TOPICS.contains(&t.as_str())
        {
            let sink = self.control_sinks.get(t.as_str()).map(|s| s.clone());
            if let (Some(sink), MessagePayload::Json(payload)) = (sink, &message.payload) {
                for reply in sink.on_peer_control(ctx, conn, payload) {
                    let msg = KernelMessage::new(
                        0,
                        MessageTarget::Topic(t.clone()),
                        MessagePayload::Json(reply),
                    );
                    if let Err(e) = self.route_to_remote(&ctx.peer_id, msg).await {
                        warn!(peer = %ctx.peer_id, topic = %t, error = %e, "control reply failed");
                    }
                }
            }
            return Ok(());
        }

        // Everything else routes to the local kernel router.
        let router = self
            .local_router
            .as_ref()
            .ok_or_else(|| KernelError::Mesh("no local router attached to mesh runtime".into()))?;
        router.deliver(ctx, dest_scope.as_ref(), message).await
    }

    /// The connection id and verified flag of the current route to `node_id`
    /// (None when not connected). Lets a cache of per-connection facts check
    /// it still describes the connection in use.
    pub fn peer_route(&self, node_id: &str) -> Option<(u64, bool)> {
        let p = self.peers.get(node_id)?;
        Some((self.conn_id(&p.sender), p.verified))
    }

    /// Number of currently connected peers.
    pub fn peer_count(&self) -> usize {
        self.peers.len()
    }

    /// True when `node_id` is routed through a connection whose node id
    /// admission verified.
    pub fn peer_verified(&self, node_id: &str) -> bool {
        self.peers.get(node_id).is_some_and(|p| p.verified)
    }

    /// True when `node_id` is routed through a verified connection whose
    /// admitted class is `node`: the outbound twin of
    /// `mesh_artifact_tunnel::licensed_peer`. A verified leaf is not.
    pub fn peer_licensed(&self, node_id: &str) -> bool {
        self.peers
            .get(node_id)
            .is_some_and(|p| p.verified && p.class == crate::mesh_admit::PeerClass::Node)
    }

    /// List the node IDs of all connected peers.
    pub fn peer_ids(&self) -> Vec<String> {
        self.peers.iter().map(|entry| entry.key().clone()).collect()
    }

    /// Per-peer connection detail, sorted by node id (observability only).
    ///
    /// Round-trip time is deliberately absent: nothing in the runtime measures
    /// it (`PeerMetrics` is never fed), and a made-up zero would read as a
    /// perfect link.
    pub fn peer_details(&self) -> Vec<PeerDetail> {
        let mut out: Vec<PeerDetail> = self
            .peers
            .iter()
            .map(|e| {
                let p = e.value();
                PeerDetail {
                    node_id: e.key().clone(),
                    class: p.class,
                    verified: p.verified,
                    licensed: p.verified && p.class == crate::mesh_admit::PeerClass::Node,
                    connected_at: p.connected_at,
                    heartbeat: self
                        .discovery
                        .as_ref()
                        .and_then(|d| d.heartbeat.lock().ok()?.peer_state(e.key())),
                    last_seen: None,
                    rtt_ms: None,
                    missed_pongs: None,
                    load: None,
                }
            })
            .collect();
        out.sort_by(|a, b| a.node_id.cmp(&b.node_id));
        if let Some(lv) = self.liveness.get() {
            for d in &mut out {
                if let Some(l) = lv.peer(&d.node_id) {
                    d.last_seen = Some(l.last_seen);
                    d.rtt_ms = Some(l.rtt_ms);
                    d.missed_pongs = Some(l.missed);
                    d.load = l.load;
                }
            }
        }
        out
    }

    /// Disconnect a peer, dropping its send channel.
    ///
    /// Also cleans up any topic subscriptions the peer registered so
    /// future publishes don't attempt to send to a closed channel.
    pub fn disconnect_peer(&self, node_id: &str) {
        if self.peers.remove(node_id).is_some() {
            self.unregister_all_peer_topics(node_id);
            debug!(peer = %node_id, "disconnected peer");
            self.peer_events.emit(MeshPeerEvent::Left {
                node_id: node_id.to_owned(),
            });
        } else {
            warn!(peer = %node_id, "disconnect_peer: peer not found");
        }
    }

    /// Disconnect every connected peer (used by mesh service stop / shutdown).
    ///
    /// Returns the number of peers that were disconnected.
    pub fn disconnect_all_peers(&self) -> usize {
        let ids: Vec<String> = self.peer_ids();
        let n = ids.len();
        for id in &ids {
            self.disconnect_peer(id);
        }
        n
    }

    // ── Discovery integration ─────────────────────────────────────

    /// Register a peer's network address for future connection.
    ///
    /// Stored in the discovery state's address map. If discovery is not
    /// initialized this is a no-op.
    pub fn register_peer_address(&self, node_id: &str, addr: &str) {
        if let Some(ref disc) = self.discovery {
            disc.peer_addresses
                .insert(node_id.to_string(), addr.to_string());
        }
    }

    /// Return known peers from the discovery address map.
    ///
    /// Each entry is `(node_id, address)`. Returns an empty vec if
    /// discovery is not initialized.
    pub fn discover_peers(&self) -> Vec<(String, String)> {
        match self.discovery.as_ref() {
            Some(disc) => disc
                .peer_addresses
                .iter()
                .map(|entry| (entry.key().clone(), entry.value().clone()))
                .collect(),
            None => Vec::new(),
        }
    }

    /// Record a heartbeat from the given peer (marks it alive).
    ///
    /// If the peer is not yet tracked by the heartbeat system it will
    /// be added automatically.
    pub fn record_heartbeat(&self, node_id: &str) {
        if let Some(ref disc) = self.discovery {
            let mut hb = disc.heartbeat.lock().unwrap();
            let prior = hb.peer_state(node_id);
            if prior.is_none() {
                hb.add_peer(node_id.to_string());
            }
            hb.record_alive(node_id);
            // Emit health transitions for cluster membership (WEFT-120).
            use crate::mesh_heartbeat::HeartbeatState;
            match prior {
                Some(HeartbeatState::Suspect) | Some(HeartbeatState::Dead) => {
                    drop(hb);
                    self.peer_events.emit(MeshPeerEvent::Recovered {
                        node_id: node_id.to_owned(),
                        address: disc
                            .peer_addresses
                            .get(node_id)
                            .map(|a| a.value().clone()),
                        verified: self.counts_as_member(
                            self.peers.get(node_id).is_some_and(|p| p.verified),
                        ),
                    });
                }
                _ => {
                    drop(hb);
                    self.peer_events.emit(MeshPeerEvent::Alive {
                        node_id: node_id.to_owned(),
                    });
                }
            }
        }
    }

    // ── Time synchronization ──────────────────────────────────────

    /// Get the current mesh-synchronized time in microseconds since epoch.
    ///
    /// If time sync is active (synced from authority), returns the
    /// authority-aligned time. Otherwise returns local system time.
    pub fn mesh_time_us(&self) -> u64 {
        self.clock.lock().unwrap().mesh_time_us()
    }

    /// Get the clock uncertainty estimate in microseconds.
    pub fn clock_uncertainty_us(&self) -> u64 {
        self.clock.lock().unwrap().uncertainty_us
    }

    /// Get the current clock source quality.
    pub fn clock_source(&self) -> ClockSource {
        self.clock.lock().unwrap().local_source
    }

    /// Set the local clock source (e.g., after NTP sync is confirmed).
    pub fn set_clock_source(&self, source: ClockSource) {
        self.clock.lock().unwrap().local_source = source;
    }

    /// Process a time sync sample from a peer's heartbeat.
    pub fn sync_clock_from_peer(&self, peer_id: &str, peer_time_us: u64, peer_source: ClockSource) {
        let local_time = crate::mesh_heartbeat::system_time_us();
        self.clock
            .lock()
            .unwrap()
            .process_sync(peer_id, peer_time_us, peer_source, local_time);
    }

    /// Check if this node is the time authority.
    pub fn is_time_authority(&self) -> bool {
        self.clock.lock().unwrap().is_authority(&self.node_id)
    }

    /// Return node IDs of peers that heartbeat considers suspect or dead.
    pub fn check_peer_health(&self) -> Vec<String> {
        match self.discovery.as_ref() {
            Some(disc) => {
                let hb = disc.heartbeat.lock().unwrap();
                let mut unhealthy: Vec<String> = hb
                    .suspect_peers()
                    .into_iter()
                    .map(|s| s.to_string())
                    .collect();
                unhealthy.extend(hb.dead_peers().into_iter().map(|s| s.to_string()));
                unhealthy
            }
            None => Vec::new(),
        }
    }

    /// Disconnect peers that the heartbeat tracker considers dead.
    ///
    /// Emits [`MeshPeerEvent::Unreachable`] before the connection is
    /// dropped so cluster membership can mark partition/failure
    /// separately from a graceful [`MeshPeerEvent::Left`].
    pub fn remove_dead_peers(&self) {
        if let Some(ref disc) = self.discovery {
            let dead: Vec<String> = {
                let hb = disc.heartbeat.lock().unwrap();
                hb.dead_peers().into_iter().map(|s| s.to_string()).collect()
            };
            for node_id in &dead {
                self.peer_events.emit(MeshPeerEvent::Unreachable {
                    node_id: node_id.clone(),
                });
                // Disconnect without a second Left event: remove peer map
                // entry and topics, but treat Unreachable as the bus signal.
                if self.peers.remove(node_id.as_str()).is_some() {
                    self.unregister_all_peer_topics(node_id);
                    debug!(peer = %node_id, "removed dead peer (unreachable)");
                }
                disc.peer_addresses.remove(node_id.as_str());
            }
        }
    }

    /// Mutable access to discovery state (for tests and setup code).
    pub fn discovery_mut(&mut self) -> Option<&mut DiscoveryState> {
        self.discovery.as_mut()
    }

    /// Shared access to discovery state.
    pub fn discovery(&self) -> Option<&DiscoveryState> {
        self.discovery.as_ref()
    }

    // ── Chain sync (WEFT-105 / K6.4) ──────────────────────────────

    /// Build a serialized [`ChainSyncRequest`] for sending to a peer.
    ///
    /// The request asks for chain events starting after `from_seq`.
    /// When a chain manager is attached, `after_hash` is filled from the
    /// local tip at `from_seq` (or the head hash when `from_seq` is the tip).
    pub fn build_chain_sync_request(&self, from_seq: u64) -> Vec<u8> {
        let (after_hash, chain_id) = self.chain_tip_meta(from_seq);
        let req = ChainSyncRequest {
            chain_id,
            after_sequence: from_seq,
            after_hash,
            max_events: 256,
        };
        serde_json::to_vec(&req).unwrap_or_default()
    }

    /// Build a chain sync **response** from the local chain (server side).
    ///
    /// Returns `None` when no chain manager is attached.
    #[cfg(feature = "exochain")]
    pub fn build_chain_sync_response(
        &self,
        after_sequence: u64,
        max_events: u32,
    ) -> Option<ChainSyncResponse> {
        let cm = self.chain_manager.get()?;
        let mut events = cm.tail_from(after_sequence);
        let has_more = events.len() > max_events as usize;
        if has_more {
            events.truncate(max_events as usize);
        }
        let json_events: Vec<serde_json::Value> = events
            .iter()
            .filter_map(|e| serde_json::to_value(e).ok())
            .collect();
        Some(ChainSyncResponse {
            chain_id: cm.chain_id(),
            events: json_events,
            has_more,
            tip_sequence: cm.head_sequence(),
            tip_hash: cm
                .head_hash()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect(),
        })
    }

    /// Handle an incoming chain sync response by replaying remote events
    /// into the local [`ChainManager`] via [`ChainManager::append_signed`].
    ///
    /// Returns the number of events **newly applied** (idempotent skips
    /// are not counted). When no chain manager is attached, returns the
    /// number of events in the payload without applying them (legacy /
    /// transport-only callers). See [`Self::handle_chain_sync_outcome`] for
    /// the typed form, which also reports a stop at an authority event.
    ///
    /// Merge policy (ADR-089): linear catch-up only. A hash mismatch at
    /// the same sequence surfaces as a mesh error (fork) rather than
    /// silent overwrite — multi-parent merge commits are a later layer.
    pub fn handle_chain_sync_response(&self, data: &[u8]) -> KernelResult<usize> {
        self.handle_chain_sync_outcome(data).map(|o| o.applied)
    }

    /// [`Self::handle_chain_sync_response`] with a typed, non-fatal stop:
    /// replication halts cleanly at the first authority-bearing event
    /// (a reserved source or kind, see `chain::RESERVED_SOURCES`) and says
    /// where, instead of failing the batch. Verified replication of authority
    /// events is future work; skipping one would leave a sequence gap.
    pub fn handle_chain_sync_outcome(&self, data: &[u8]) -> KernelResult<ChainSyncOutcome> {
        let resp: ChainSyncResponse = serde_json::from_slice(data)
            .map_err(|e| KernelError::Mesh(format!("chain sync deserialize error: {e}")))?;

        #[cfg(feature = "exochain")]
        {
            if let Some(cm) = self.chain_manager.get() {
                let mut applied = 0usize;
                let mut stopped = None;
                for value in &resp.events {
                    let event: crate::chain::ChainEvent = serde_json::from_value(value.clone())
                        .map_err(|e| {
                            KernelError::Mesh(format!("chain sync event decode: {e}"))
                        })?;
                    let seq = event.sequence;
                    match cm.append_signed(event) {
                        Ok(_) => applied += 1,
                        Err(crate::chain::AppendSignedError::AlreadyPresent { .. }) => {}
                        Err(crate::chain::AppendSignedError::ReservedSource { event_source }) => {
                            warn!(seq, source = %event_source, "chain sync stopped at an authority event");
                            stopped = Some(StoppedAtAuthorityEvent { sequence: seq, source: event_source });
                            break;
                        }
                        Err(e) => {
                            return Err(KernelError::Mesh(format!(
                                "chain sync append_signed: {e}"
                            )));
                        }
                    }
                }
                debug!(
                    applied,
                    batch = resp.events.len(),
                    tip = resp.tip_sequence,
                    "chain sync response applied"
                );
                return Ok(ChainSyncOutcome { applied, stopped });
            }
        }

        Ok(ChainSyncOutcome { applied: resp.events.len(), stopped: None })
    }

    #[cfg(feature = "exochain")]
    fn chain_tip_meta(&self, from_seq: u64) -> (String, u32) {
        if let Some(cm) = self.chain_manager.get() {
            let hash = if from_seq == 0 && cm.head_sequence() == 0 {
                cm.head_hash()
            } else {
                cm.tail_from(from_seq.saturating_sub(1))
                    .first()
                    .map(|e| e.prev_hash)
                    .unwrap_or_else(|| cm.head_hash())
            };
            // Prefer the event at `from_seq` if present.
            let hash = cm
                .tail(0)
                .into_iter()
                .find(|e| e.sequence == from_seq)
                .map(|e| e.hash)
                .unwrap_or(hash);
            let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
            (hex, cm.chain_id())
        } else {
            (String::new(), 0)
        }
    }

    #[cfg(not(feature = "exochain"))]
    fn chain_tip_meta(&self, _from_seq: u64) -> (String, u32) {
        (String::new(), 0)
    }
}

/// Where chain replication stopped at an authority-bearing event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoppedAtAuthorityEvent {
    /// Sequence of the refused event.
    pub sequence: u64,
    /// Its source.
    pub source: String,
}

/// Result of applying a chain sync batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainSyncOutcome {
    /// Events newly applied.
    pub applied: usize,
    /// Set when replication halted at an authority event (not an error).
    pub stopped: Option<StoppedAtAuthorityEvent>,
}

// ── Tests ───────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::{AgentCapabilities, CapabilityChecker};
    use crate::ipc::{KernelMessage, MessagePayload, MessageTarget};
    use crate::mesh_heartbeat::HeartbeatState;
    use crate::mesh_ipc::MeshIpcEnvelope;
    use crate::process::{ProcessEntry, ProcessState, ProcessTable, ResourceUsage};
    use crate::topic::TopicRouter;
    use tokio_util::sync::CancellationToken;

    /// Helper: build a minimal A2ARouter with one registered process and return
    /// (router, pid, inbox_receiver).
    fn make_router_with_process() -> (
        Arc<A2ARouter>,
        crate::process::Pid,
        tokio::sync::mpsc::Receiver<KernelMessage>,
    ) {
        let table = Arc::new(ProcessTable::new(64));
        let entry = ProcessEntry {
            pid: 0,
            agent_id: "test-agent".into(),
            state: ProcessState::Running,
            capabilities: AgentCapabilities::default(),
            resource_usage: ResourceUsage::default(),
            cancel_token: CancellationToken::new(),
            parent_pid: None,
        };
        let pid = table.insert(entry).unwrap();
        let checker = Arc::new(CapabilityChecker::new(table.clone()));
        let topics = Arc::new(TopicRouter::new(table.clone()));
        let router = Arc::new(A2ARouter::new(table, checker, topics));
        let rx = router.create_inbox(pid);
        (router, pid, rx)
    }

    // ── Test 1: empty peer list on creation ─────────────────────

    #[test]
    fn new_runtime_has_no_peers() {
        let rt = MeshRuntime::new("node-local".into());
        assert_eq!(rt.peer_count(), 0);
        assert!(rt.peer_ids().is_empty());
        assert_eq!(rt.node_id(), "node-local");
    }

    // ── Test 2: add mock peer, verify peer_count ────────────────

    #[test]
    fn add_peer_increases_count() {
        let rt = MeshRuntime::new("local".into());
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        rt.add_peer("peer-1".into(), tx);
        assert_eq!(rt.peer_count(), 1);
        assert_eq!(rt.peer_ids(), vec!["peer-1".to_string()]);
    }

    // ── WEFT-120: peer-event emission ─────────────────────────────

    #[tokio::test]
    async fn add_peer_emits_joined_event() {
        use crate::mesh_discovery::MeshPeerEvent;
        let rt = MeshRuntime::new("local".into());
        let mut rx = rt.subscribe_peer_events();
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        rt.add_peer("peer-j".into(), tx);
        let ev = rx.recv().await.unwrap();
        match ev {
            MeshPeerEvent::Joined { node_id, .. } => assert_eq!(node_id, "peer-j"),
            other => panic!("expected Joined, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn disconnect_peer_emits_left_event() {
        use crate::mesh_discovery::MeshPeerEvent;
        let rt = MeshRuntime::new("local".into());
        let mut rx = rt.subscribe_peer_events();
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        rt.add_peer("peer-l".into(), tx);
        let _ = rx.recv().await.unwrap(); // Joined
        rt.disconnect_peer("peer-l");
        let ev = rx.recv().await.unwrap();
        assert_eq!(
            ev,
            MeshPeerEvent::Left {
                node_id: "peer-l".into()
            }
        );
    }

    #[tokio::test]
    async fn re_add_peer_emits_recovered() {
        use crate::mesh_discovery::MeshPeerEvent;
        let rt = MeshRuntime::new("local".into());
        let mut rx = rt.subscribe_peer_events();
        let (tx1, _rx1) = tokio::sync::mpsc::channel(16);
        rt.add_peer("peer-r".into(), tx1);
        let _ = rx.recv().await.unwrap();
        // Re-register same node_id (reconnect) without disconnecting first
        // keeps was_known=true → Recovered.
        let (tx2, _rx2) = tokio::sync::mpsc::channel(16);
        rt.add_peer("peer-r".into(), tx2);
        let ev = rx.recv().await.unwrap();
        match ev {
            MeshPeerEvent::Recovered { node_id, .. } => assert_eq!(node_id, "peer-r"),
            other => panic!("expected Recovered, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn remove_dead_peers_emits_unreachable() {
        use crate::mesh_discovery::MeshPeerEvent;
        use crate::mesh_heartbeat::{HeartbeatConfig, HeartbeatState};
        use std::time::Duration;

        let kad = [0u8; 32];
        let mut rt = MeshRuntime::with_discovery("local".into(), kad);
        let mut rx = rt.subscribe_peer_events();
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        rt.add_peer("dying".into(), tx);
        let _ = rx.recv().await.unwrap(); // Joined

        // Force peer to Dead via zero suspect timeout + two misses.
        {
            let disc = rt.discovery_mut().unwrap();
            *disc.heartbeat.lock().unwrap() =
                crate::mesh_heartbeat::HeartbeatTracker::new(HeartbeatConfig {
                    suspect_timeout: Duration::ZERO,
                    ..Default::default()
                });
            let mut hb = disc.heartbeat.lock().unwrap();
            hb.add_peer("dying".into());
            hb.record_miss("dying"); // Alive → Suspect
            hb.record_miss("dying"); // Suspect + zero timeout → Dead
            assert_eq!(hb.peer_state("dying"), Some(HeartbeatState::Dead));
        }

        rt.remove_dead_peers();
        let ev = rx.recv().await.unwrap();
        assert_eq!(
            ev,
            MeshPeerEvent::Unreachable {
                node_id: "dying".into()
            }
        );
        assert_eq!(rt.peer_count(), 0);
    }

    #[tokio::test]
    async fn mesh_runtime_events_drive_cluster_membership() {
        use crate::cluster::{ClusterConfig, ClusterMembership, NodeState};
        use crate::mesh_discovery::MeshPeerEvent;
        use std::sync::Arc;

        let membership = Arc::new(
            ClusterMembership::new(ClusterConfig::default())
                .with_min_peer_interval(std::time::Duration::ZERO),
        );
        let rt = MeshRuntime::new("local".into());
        // Unverified only exists under enforce; observe and off count everyone.
        rt.set_enforcing(true);
        membership.spawn_mesh_peer_listener(rt.subscribe_peer_events());

        // A legacy route (`add_peer`: a seed, a leaf, a test harness) is a
        // claimed id, held as Unverified; an admitted one is Active.
        let (utx, _urx) = tokio::sync::mpsc::channel(16);
        rt.add_peer("legacy-peer".into(), utx);
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        assert!(rt.register_authenticated("mesh-peer".into(), tx, true, &RouteTally::default()));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            membership.get_peer("legacy-peer").unwrap().state,
            NodeState::Unverified
        );
        assert_eq!(
            membership.get_peer("mesh-peer").unwrap().state,
            NodeState::Active
        );

        rt.emit_peer_event(MeshPeerEvent::Unreachable {
            node_id: "mesh-peer".into(),
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            membership.get_peer("mesh-peer").unwrap().state,
            NodeState::Unreachable
        );

        // Reconnect after partition.
        let (tx2, _rx2) = tokio::sync::mpsc::channel(16);
        // Still in peers map from first add — re-add emits Recovered.
        assert!(rt.register_authenticated("mesh-peer".into(), tx2, true, &RouteTally::default()));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            membership.get_peer("mesh-peer").unwrap().state,
            NodeState::Active
        );

        rt.disconnect_peer("mesh-peer");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(membership.get_peer("mesh-peer").is_none());
    }

    // ── Test 3: send envelope to peer (mock channel) ────────────

    #[tokio::test]
    async fn send_to_peer_delivers_serialized_bytes() {
        let rt = MeshRuntime::new("local".into());
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        rt.add_peer("peer-1".into(), tx);

        let msg = KernelMessage::text(0, MessageTarget::Broadcast, "hello");
        let envelope = MeshIpcEnvelope::new("local".into(), "peer-1".into(), msg);
        let expected_id = envelope.envelope_id.clone();

        rt.send_to_peer("peer-1", envelope).await.unwrap();

        let received = rx.recv().await.unwrap();
        let decoded = MeshIpcEnvelope::from_bytes(&received).unwrap();
        assert_eq!(decoded.envelope_id, expected_id);
        assert_eq!(decoded.source_node, "local");
        assert_eq!(decoded.dest_node, "peer-1");
    }

    // ── Test 4: receive envelope, inject into local router ──────

    #[tokio::test]
    async fn handle_incoming_injects_into_local_router() {
        let (router, pid, mut inbox_rx) = make_router_with_process();

        let mut rt = MeshRuntime::new("node-b".into());
        rt.set_local_router(router);

        // Build an envelope targeting a local process via RemoteNode wrapper
        let inner_target = MessageTarget::Process(pid);
        let remote_target = MessageTarget::RemoteNode {
            node_id: "node-b".into(),
            target: Box::new(inner_target),
        };
        let msg = KernelMessage::text(pid, remote_target, "from-remote");
        let envelope = MeshIpcEnvelope::new("node-a".into(), "node-b".into(), msg);
        let bytes = envelope.to_bytes().unwrap();

        rt.handle_incoming(&bytes).await.unwrap();

        let delivered = inbox_rx.try_recv().unwrap();
        assert!(matches!(delivered.target, MessageTarget::Process(p) if p == pid));
        match &delivered.payload {
            MessagePayload::Text(s) => assert_eq!(s, "from-remote"),
            other => panic!("expected Text payload, got: {other:?}"),
        }
    }

    // ── Test 5: disconnect peer ─────────────────────────────────

    #[test]
    fn disconnect_peer_removes_it() {
        let rt = MeshRuntime::new("local".into());
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        rt.add_peer("peer-1".into(), tx);
        assert_eq!(rt.peer_count(), 1);

        rt.disconnect_peer("peer-1");
        assert_eq!(rt.peer_count(), 0);
    }

    // ── Test 6: round-trip (serialize -> deserialize -> inject) ──

    #[tokio::test]
    async fn round_trip_send_receive() {
        let (router, pid, mut inbox_rx) = make_router_with_process();

        // "Node A" side: build the runtime and a mock peer channel
        let rt_a = MeshRuntime::new("node-a".into());
        let (tx, mut peer_rx) = tokio::sync::mpsc::channel(16);
        rt_a.add_peer("node-b".into(), tx);

        // Send from A to B
        let msg = KernelMessage::text(
            pid,
            MessageTarget::RemoteNode {
                node_id: "node-b".into(),
                target: Box::new(MessageTarget::Process(pid)),
            },
            "round-trip",
        );
        rt_a.route_to_remote("node-b", msg).await.unwrap();

        // "Node B" side: receive the bytes and inject
        let wire_bytes = peer_rx.recv().await.unwrap();
        let mut rt_b = MeshRuntime::new("node-b".into());
        rt_b.set_local_router(router);
        rt_b.handle_incoming(&wire_bytes).await.unwrap();

        let delivered = inbox_rx.try_recv().unwrap();
        match &delivered.payload {
            MessagePayload::Text(s) => assert_eq!(s, "round-trip"),
            other => panic!("expected Text payload, got: {other:?}"),
        }
    }

    // ── Test 7: node ID exchange metadata ───────────────────────

    #[test]
    fn peer_connection_stores_node_id_and_timestamp() {
        let rt = MeshRuntime::new("local".into());
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        rt.add_peer("remote-42".into(), tx);

        let entry = rt.peers.get("remote-42").unwrap();
        assert_eq!(entry.node_id, "remote-42");
        // connected_at should be very recent (within 1 second)
        let age = chrono::Utc::now() - entry.connected_at;
        assert!(age.num_seconds() < 2);
    }

    // ── Test 8: multiple peers ──────────────────────────────────

    #[test]
    fn multiple_peers_connected() {
        let rt = MeshRuntime::new("local".into());
        for i in 0..5 {
            let (tx, _rx) = tokio::sync::mpsc::channel(16);
            rt.add_peer(format!("peer-{i}"), tx);
        }
        assert_eq!(rt.peer_count(), 5);

        let mut ids = rt.peer_ids();
        ids.sort();
        assert_eq!(ids, vec!["peer-0", "peer-1", "peer-2", "peer-3", "peer-4"]);
    }

    // ── Test 9: send to unknown peer returns error ──────────────

    #[tokio::test]
    async fn send_to_unknown_peer_errors() {
        let rt = MeshRuntime::new("local".into());
        let msg = KernelMessage::text(0, MessageTarget::Broadcast, "hi");
        let envelope = MeshIpcEnvelope::new("local".into(), "ghost".into(), msg);

        let err = rt.send_to_peer("ghost", envelope).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("peer not connected"), "got: {msg}");
    }

    // ── Test 10: handle_incoming with malformed data ────────────

    #[tokio::test]
    async fn handle_incoming_malformed_data_errors() {
        let mut rt = MeshRuntime::new("local".into());
        let (router, _pid, _rx) = make_router_with_process();
        rt.set_local_router(router);

        let err = rt.handle_incoming(b"not valid json").await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("deserialization"), "got: {msg}");
    }

    // ── Test 11: handle_incoming without router errors ──────────

    #[tokio::test]
    async fn handle_incoming_without_router_errors() {
        let rt = MeshRuntime::new("local".into());
        let msg = KernelMessage::text(0, MessageTarget::Broadcast, "hi");
        let envelope = MeshIpcEnvelope::new("a".into(), "local".into(), msg);
        let bytes = envelope.to_bytes().unwrap();

        let err = rt.handle_incoming(&bytes).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no local router"), "got: {msg}");
    }

    // ── Test 12: disconnect nonexistent peer is safe ────────────

    #[test]
    fn disconnect_nonexistent_peer_is_noop() {
        let rt = MeshRuntime::new("local".into());
        rt.disconnect_peer("ghost"); // should not panic
        assert_eq!(rt.peer_count(), 0);
    }

    // ── Test 13: two-node message exchange ──────────────────────

    #[tokio::test]
    async fn two_nodes_exchange_messages() {
        // Create two MeshRuntime instances with different node IDs.
        let (router_b, pid_b, mut inbox_b) = make_router_with_process();

        let rt_a = MeshRuntime::new("node-a".into());
        let mut rt_b = MeshRuntime::new("node-b".into());
        rt_b.set_local_router(router_b);

        // Create channels to simulate network transport.
        let (a_to_b_tx, mut a_to_b_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let (b_to_a_tx, mut b_to_a_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);

        // Each node knows the other as a peer.
        rt_a.add_peer("node-b".into(), a_to_b_tx);
        rt_b.add_peer("node-a".into(), b_to_a_tx);

        // Node A sends a message targeting a process on node B.
        let msg_a = KernelMessage::text(
            pid_b,
            MessageTarget::RemoteNode {
                node_id: "node-b".into(),
                target: Box::new(MessageTarget::Process(pid_b)),
            },
            "hello-from-a",
        );
        rt_a.route_to_remote("node-b", msg_a).await.unwrap();

        // Simulate the wire: read from a_to_b channel, inject into rt_b.
        let wire_bytes = a_to_b_rx.recv().await.unwrap();
        rt_b.handle_incoming(&wire_bytes).await.unwrap();

        // The message should be in node B's local inbox.
        let delivered = inbox_b.try_recv().unwrap();
        match &delivered.payload {
            MessagePayload::Text(s) => assert_eq!(s, "hello-from-a"),
            other => panic!("expected Text, got: {other:?}"),
        }

        // Node B replies back to node A (we just verify send succeeds).
        let msg_b = KernelMessage::text(0, MessageTarget::Broadcast, "reply-from-b");
        rt_b.route_to_remote("node-a", msg_b).await.unwrap();

        let reply_bytes = b_to_a_rx.recv().await.unwrap();
        let reply_env = MeshIpcEnvelope::from_bytes(&reply_bytes).unwrap();
        assert_eq!(reply_env.source_node, "node-b");
        assert_eq!(reply_env.dest_node, "node-a");
    }

    // ── Test 14: discover_peers returns registered addresses ─────

    #[test]
    fn discover_peers_returns_registered_addresses() {
        let rt = MeshRuntime::with_discovery("node-x".into(), [0u8; 32]);
        rt.register_peer_address("peer-1", "10.0.0.1:9489");
        rt.register_peer_address("peer-2", "10.0.0.2:9489");
        rt.register_peer_address("peer-3", "10.0.0.3:9489");

        let mut peers = rt.discover_peers();
        peers.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(peers.len(), 3);
        assert_eq!(peers[0], ("peer-1".into(), "10.0.0.1:9489".into()));
        assert_eq!(peers[1], ("peer-2".into(), "10.0.0.2:9489".into()));
        assert_eq!(peers[2], ("peer-3".into(), "10.0.0.3:9489".into()));
    }

    #[test]
    fn peer_details_report_class_verified_licensed_and_heartbeat() {
        let rt = MeshRuntime::with_discovery("local".into(), [0u8; 32]);
        let tally = RouteTally::default();
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let (tx2, _rx2) = tokio::sync::mpsc::channel(1);
        rt.register_authenticated_as("b-leaf".into(), tx, true, crate::mesh_admit::PeerClass::Leaf, &tally);
        rt.register_authenticated_as("a-node".into(), tx2, true, crate::mesh_admit::PeerClass::Node, &tally);
        rt.record_heartbeat("a-node");
        let d = rt.peer_details();
        assert_eq!(d.iter().map(|p| p.node_id.as_str()).collect::<Vec<_>>(), ["a-node", "b-leaf"]);
        assert!(d[0].licensed && d[0].verified);
        assert_eq!(d[0].heartbeat, Some(crate::mesh_heartbeat::HeartbeatState::Alive));
        assert!(!d[1].licensed && d[1].verified && d[1].class.as_str() == "leaf");
        assert_eq!(d[1].heartbeat, None);
    }

    // ── Test 15: heartbeat tracking detects suspect peer ─────────

    #[test]
    fn heartbeat_tracking_detects_dead_peer() {
        let mut rt = MeshRuntime::with_discovery("local".into(), [0u8; 32]);

        // Configure heartbeat with zero suspect timeout for instant transition.
        {
            let disc = rt.discovery_mut().unwrap();
            let mut hb = disc.heartbeat.lock().unwrap();
            *hb = HeartbeatTracker::new(crate::mesh_heartbeat::HeartbeatConfig {
                suspect_timeout: std::time::Duration::from_secs(0),
                ..crate::mesh_heartbeat::HeartbeatConfig::default()
            });
            hb.add_peer("healthy".into());
            hb.add_peer("dying".into());
        }

        // Record heartbeat for healthy peer.
        rt.record_heartbeat("healthy");

        // Simulate misses for dying peer -> suspect -> dead.
        {
            let disc = rt.discovery_mut().unwrap();
            let mut hb = disc.heartbeat.lock().unwrap();
            hb.record_miss("dying");
            hb.record_miss("dying");
        }

        let unhealthy = rt.check_peer_health();
        assert!(unhealthy.contains(&"dying".to_string()));
        assert!(!unhealthy.contains(&"healthy".to_string()));
    }

    // ── Test 16: remove_dead_peers cleans connections ────────────

    #[test]
    fn remove_dead_peers_cleans_connections() {
        let mut rt = MeshRuntime::with_discovery("local".into(), [0u8; 32]);

        // Add a peer connection.
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        rt.add_peer("dead-peer".into(), tx);
        rt.register_peer_address("dead-peer", "10.0.0.99:9489");

        // Force the peer into Dead state.
        {
            let disc = rt.discovery_mut().unwrap();
            let mut hb = disc.heartbeat.lock().unwrap();
            *hb = HeartbeatTracker::new(crate::mesh_heartbeat::HeartbeatConfig {
                suspect_timeout: std::time::Duration::from_secs(0),
                ..crate::mesh_heartbeat::HeartbeatConfig::default()
            });
            hb.add_peer("dead-peer".into());
            hb.record_miss("dead-peer");
            hb.record_miss("dead-peer");
            assert_eq!(hb.peer_state("dead-peer"), Some(HeartbeatState::Dead));
        }

        assert_eq!(rt.peer_count(), 1);
        rt.remove_dead_peers();
        assert_eq!(rt.peer_count(), 0);
        assert!(rt.discover_peers().is_empty());
    }

    // ── Test 17: Kademlia routing finds closest peers ────────────

    #[test]
    fn kademlia_routing_finds_closest_peers() {
        let mut rt = MeshRuntime::with_discovery("local".into(), [0u8; 32]);

        let disc = rt.discovery_mut().unwrap();
        let mut kad = disc.kademlia.lock().unwrap();
        // Insert peers with different XOR distances from the local key [0;32].
        for i in 1..=5u8 {
            let mut peer_key = [0u8; 32];
            peer_key[0] = i;
            kad.add_peer(
                peer_key,
                crate::mesh_kad::DhtEntry {
                    key: format!("peer-{i}"),
                    node_id: format!("peer-{i}"),
                    address: format!("10.0.0.{i}:9489"),
                    platform: "linux".into(),
                    last_seen: 1000,
                    governance_genesis_prefix: "0000000000000000".into(),
                },
            );
        }

        // Find the 3 closest to the local key.
        let closest = kad.find_closest(&[0u8; 32], 3);
        assert_eq!(closest.len(), 3);
        // The closest by XOR distance to [0;32] should be the ones with
        // the smallest first byte, which maps to node_id strings that
        // start with the smallest byte values.
    }

    // ── Test 18: with_discovery initializes state ────────────────

    #[test]
    fn with_discovery_initializes_state() {
        let rt = MeshRuntime::with_discovery("disc-node".into(), [0xAB; 32]);
        assert_eq!(rt.node_id(), "disc-node");
        assert!(rt.discovery().is_some());

        let disc = rt.discovery().unwrap();
        assert_eq!(*disc.kademlia.lock().unwrap().local_key(), [0xAB; 32]);
        assert_eq!(disc.heartbeat.lock().unwrap().peer_count(), 0);
    }

    // ── Test 19: discover_peers empty without discovery ──────────

    #[test]
    fn discover_peers_empty_without_discovery() {
        let rt = MeshRuntime::new("plain-node".into());
        assert!(rt.discover_peers().is_empty());
        // These should be safe no-ops.
        rt.register_peer_address("x", "y");
        rt.record_heartbeat("x");
        assert!(rt.check_peer_health().is_empty());
    }

    // ── Test 20: chain sync request round-trip ───────────────────

    #[test]
    fn chain_sync_request_round_trip() {
        let rt = MeshRuntime::new("sync-node".into());
        let bytes = rt.build_chain_sync_request(42);

        let req: crate::mesh_chain::ChainSyncRequest = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(req.chain_id, 0);
        assert_eq!(req.after_sequence, 42);
        assert_eq!(req.max_events, 256);
    }

    // ── Test 21: chain sync response handling (no chain attached) ─

    #[test]
    fn chain_sync_response_handling() {
        let rt = MeshRuntime::new("sync-node".into());

        let resp = crate::mesh_chain::ChainSyncResponse {
            chain_id: 0,
            events: vec![
                serde_json::json!({"type": "write"}),
                serde_json::json!({"type": "delete"}),
            ],
            has_more: false,
            tip_sequence: 100,
            tip_hash: "abc".into(),
        };
        let data = serde_json::to_vec(&resp).unwrap();

        // Without a chain manager, events are counted but not applied.
        let count = rt.handle_chain_sync_response(&data).unwrap();
        assert_eq!(count, 2);
    }

    // ── Test 22: chain sync response malformed errors ────────────

    #[test]
    fn chain_sync_response_malformed_errors() {
        let rt = MeshRuntime::new("sync-node".into());
        let err = rt.handle_chain_sync_response(b"bad json").unwrap_err();
        assert!(err.to_string().contains("chain sync deserialize"));
    }

    // ── Test 22b: two-node chain convergence via append_signed ───

    /// WEFT-105: source node is ahead; sink shares genesis and catches up
    /// via chain sync response → both tips converge.
    #[cfg(feature = "exochain")]
    #[test]
    fn two_node_chain_sync_convergence() {
        use crate::chain::ChainManager;
        use std::sync::Arc;

        let source_cm = Arc::new(ChainManager::new(0, 1000));
        source_cm.append("mesh", "peer.join", Some(serde_json::json!({"id": "a"})));
        source_cm.append("mesh", "peer.join", Some(serde_json::json!({"id": "b"})));
        source_cm.append("kernel", "boot", None);

        // Sink starts from the same cluster genesis (shared identity).
        let genesis = source_cm.tail(0)[0].clone();
        let sink_cm = Arc::new(ChainManager::from_events(vec![genesis], 1000).unwrap());
        assert!(sink_cm.head_sequence() < source_cm.head_sequence());

        let source_rt = MeshRuntime::new("source".into());
        source_rt.set_chain_manager(source_cm.clone());
        let sink_rt = MeshRuntime::new("sink".into());
        sink_rt.set_chain_manager(sink_cm.clone());

        let after = sink_cm.head_sequence();
        let resp = source_rt
            .build_chain_sync_response(after, 256)
            .expect("source has chain");
        assert!(!resp.events.is_empty());
        let data = serde_json::to_vec(&resp).unwrap();

        let applied = sink_rt.handle_chain_sync_response(&data).unwrap();
        assert_eq!(applied, 3);
        assert_eq!(sink_cm.head_sequence(), source_cm.head_sequence());
        assert_eq!(sink_cm.head_hash(), source_cm.head_hash());

        // Second sync is idempotent — zero new applies.
        let resp2 = source_rt
            .build_chain_sync_response(sink_cm.head_sequence(), 256)
            .unwrap();
        assert!(resp2.events.is_empty());
    }

    /// Replication stops cleanly (typed, non-fatal) at the first authority
    /// event; what came before is applied, nothing after is.
    #[cfg(feature = "exochain")]
    #[test]
    fn chain_sync_stops_at_the_first_authority_event() {
        use crate::chain::ChainManager;
        use std::sync::Arc;

        let src = Arc::new(ChainManager::new(0, 1000));
        src.append("mesh", "peer.join", None);
        let forged = src.append("auth.token", "auth.token.issued", Some(serde_json::json!({})));
        src.append("mesh", "peer.join", None);
        let genesis = src.tail(0)[0].clone();
        let sink = Arc::new(ChainManager::from_events(vec![genesis], 1000).unwrap());
        let src_rt = MeshRuntime::new("s".into());
        src_rt.set_chain_manager(src.clone());
        let sink_rt = MeshRuntime::new("k".into());
        sink_rt.set_chain_manager(sink.clone());
        let resp = src_rt.build_chain_sync_response(sink.head_sequence(), 256).unwrap();
        let data = serde_json::to_vec(&resp).unwrap();
        let out = sink_rt.handle_chain_sync_outcome(&data).unwrap();
        assert_eq!(out.applied, 1);
        assert_eq!(
            out.stopped,
            Some(StoppedAtAuthorityEvent { sequence: forged.sequence, source: "auth.token".into() })
        );
        assert!(sink.tail(0).iter().all(|e| e.source != "auth.token"));
        assert_eq!(sink_rt.handle_chain_sync_response(&data).unwrap(), 0);
    }

    // ── Test 23: inbound peer auto-registered, topic forwarded ───

    /// End-to-end: a leaf peer sends a `mesh.subscribe` envelope over
    /// an inbound connection; the kernel auto-registers the peer, records
    /// the subscription, and then forwards a locally-published topic
    /// message back through the peer's outbound channel.
    #[tokio::test]
    async fn subscribe_then_topic_forwards_to_inbound_peer() {
        let (router, pid, _inbox) = make_router_with_process();

        // Wire a mesh runtime to the router so the Topic handler's
        // forwarder can see it.
        let mut rt = MeshRuntime::new("kernel".into());
        rt.set_local_router(router.clone());
        let rt = Arc::new(rt);
        router.set_mesh_runtime(rt.clone());

        // Inbound peer's outbound channel (what the accept loop would drain).
        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);

        // Leaf sends mesh.subscribe to register interest in topic "push.leaf-x".
        let sub_msg = KernelMessage::new(
            pid,
            MessageTarget::Topic("mesh.subscribe".into()),
            MessagePayload::Json(serde_json::json!({"topic": "push.leaf-x"})),
        );
        let sub_env = MeshIpcEnvelope::new("leaf-x".into(), "kernel".into(), sub_msg);
        let sub_bytes = sub_env.to_bytes().unwrap();

        rt.handle_incoming_from(&sub_bytes, out_tx.clone())
            .await
            .unwrap();

        // Peer should now be registered in both the peer table and the
        // subscription registry.
        assert!(
            rt.peer_ids().contains(&"leaf-x".to_string()),
            "inbound peer should be auto-registered by source_node"
        );
        assert_eq!(
            rt.peers_for_topic("push.leaf-x"),
            vec!["leaf-x".to_string()]
        );

        // Locally publish to the subscribed topic. The A2A router's Topic
        // handler should forward to the mesh peer via send_to_peer.
        let push = KernelMessage::new(
            pid,
            MessageTarget::Topic("push.leaf-x".into()),
            MessagePayload::Text("hello-leaf".into()),
        );
        router.send(push).await.unwrap();

        // The outbound drain should see the forwarded envelope.
        let forwarded_bytes =
            tokio::time::timeout(std::time::Duration::from_millis(500), out_rx.recv())
                .await
                .expect("forward should arrive within timeout")
                .expect("channel should not be closed");

        let forwarded = MeshIpcEnvelope::from_bytes(&forwarded_bytes).unwrap();
        assert_eq!(forwarded.source_node, "kernel");
        assert_eq!(forwarded.dest_node, "leaf-x");
        match &forwarded.message.target {
            MessageTarget::Topic(t) => assert_eq!(t, "push.leaf-x"),
            other => panic!("expected Topic target, got {other:?}"),
        }
        match &forwarded.message.payload {
            MessagePayload::Text(s) => assert_eq!(s, "hello-leaf"),
            other => panic!("expected Text payload, got {other:?}"),
        }

        // Disconnect should also clear the subscription, so a follow-up
        // publish has nowhere to go.
        rt.disconnect_peer("leaf-x");
        assert!(rt.peers_for_topic("push.leaf-x").is_empty());
    }

    // ── Test 24: reconnecting peer re-registers its outbound channel ─

    /// Regression: a leaf that reconnects keeps the same `source_node`
    /// but gets a fresh outbound channel. `handle_incoming_from` must
    /// replace the stale entry — otherwise `send_to_peer` delivers into
    /// the dead channel of the dropped connection and the leaf never
    /// receives anything again.
    #[tokio::test]
    async fn reconnecting_peer_replaces_stale_outbound_channel() {
        let rt = MeshRuntime::new("kernel".into());

        let sub_msg = KernelMessage::new(
            0,
            MessageTarget::Topic("mesh.subscribe".into()),
            MessagePayload::Json(serde_json::json!({"topic": "push.leaf-y"})),
        );
        let sub_env = MeshIpcEnvelope::new("leaf-y".into(), "kernel".into(), sub_msg);
        let sub_bytes = sub_env.to_bytes().unwrap();

        // First connection.
        let (old_tx, mut old_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
        rt.handle_incoming_from(&sub_bytes, old_tx).await.unwrap();

        // Reconnect: same node_id, brand-new channel (old connection dropped).
        let (new_tx, mut new_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
        rt.handle_incoming_from(&sub_bytes, new_tx).await.unwrap();

        // A push must land on the NEW channel, not the stale one.
        let push = KernelMessage::new(
            0,
            MessageTarget::Topic("push.leaf-y".into()),
            MessagePayload::Text("after-reconnect".into()),
        );
        let env = MeshIpcEnvelope::new("kernel".into(), "leaf-y".into(), push);
        rt.send_to_peer("leaf-y", env).await.unwrap();

        assert!(
            new_rx.try_recv().is_ok(),
            "push should be delivered on the reconnected channel"
        );
        assert!(
            old_rx.try_recv().is_err(),
            "stale channel from the dropped connection must not receive"
        );
    }

    // ── WEFT-117: AssessmentTransport demux + two-node diff push ─

    /// Two mock peers: node A pushes a FindingDiff through MeshRuntime
    /// demux; node B's AssessmentTransport updates peer state without
    /// requiring a full report exchange.
    #[tokio::test]
    async fn assessment_transport_demux_and_diff_push_two_nodes() {
        use crate::assessment::analyzer::diff_reports;
        use crate::assessment::mesh::MeshCoordinator;
        use crate::assessment::{AssessmentReport, AssessmentSummary, Finding};
        use crate::mesh_assess::AssessmentTransport;
        use chrono::Utc;

        fn report_with(findings: Vec<Finding>) -> AssessmentReport {
            AssessmentReport {
                timestamp: Utc::now(),
                scope: "full".into(),
                project: "/tmp/a".into(),
                files_scanned: 10,
                summary: AssessmentSummary {
                    total_files: 10,
                    coherence_score: 0.9,
                    ..Default::default()
                },
                findings,
                analyzers_run: vec!["complexity".into()],
            }
        }

        let coord_a = Arc::new(MeshCoordinator::new("node-a".into(), "proj-a".into()));
        let coord_b = Arc::new(MeshCoordinator::new("node-b".into(), "proj-b".into()));
        let transport_a = Arc::new(AssessmentTransport::new(coord_a.clone()));
        let transport_b = Arc::new(AssessmentTransport::new(coord_b.clone()));

        // Routers not required for assessment demux path.
        let rt_a = Arc::new(MeshRuntime::new("node-a".into()));
        let rt_b = Arc::new(MeshRuntime::new("node-b".into()));
        rt_a.set_assessment_transport(transport_a.clone());
        rt_b.set_assessment_transport(transport_b.clone());

        // Mock peer channels: A → B and B → A
        let (a_to_b_tx, mut a_to_b_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        let (b_to_a_tx, mut b_to_a_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        rt_a.add_peer("node-b".into(), a_to_b_tx);
        rt_b.add_peer("node-a".into(), b_to_a_tx.clone());

        let prev = report_with(vec![Finding {
            severity: "warning".into(),
            category: "size".into(),
            file: "a.rs".into(),
            line: Some(1),
            message: "old".into(),
        }]);
        let curr = report_with(vec![
            Finding {
                severity: "warning".into(),
                category: "size".into(),
                file: "a.rs".into(),
                line: Some(1),
                message: "old".into(),
            },
            Finding {
                severity: "warning".into(),
                category: "size".into(),
                file: "b.rs".into(),
                line: Some(2),
                message: "new finding".into(),
            },
        ]);
        let diff = diff_reports(&curr, &prev);
        assert_eq!(diff.findings_new.len(), 1);

        // A queues FindingDiff and drains via push_pending_assessment.
        coord_a.set_pending_broadcast(coord_a.build_finding_diff(&curr, &diff));
        transport_a.publish_report(curr.clone());
        let sent = rt_a.push_pending_assessment().await;
        assert_eq!(sent, 1, "diff should reach mock peer B channel");

        let frame_bytes = a_to_b_rx.try_recv().expect("B should receive encoded frame");

        // B demuxes AssessmentSync in the mesh event loop.
        rt_b.handle_incoming_from(&frame_bytes, b_to_a_tx.clone())
            .await
            .expect("assessment demux must succeed");

        let peers = transport_b.peer_states();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].node_id, "node-a");
        assert_eq!(peers[0].project_name, "proj-a");
        assert_eq!(peers[0].finding_count, 2);

        // JSON IPC still works when assessment transport is wired
        // (non-assessment traffic falls through).
        let ipc = MeshIpcEnvelope::new(
            "node-a".into(),
            "node-b".into(),
            KernelMessage::new(
                0,
                MessageTarget::Topic("mesh.subscribe".into()),
                MessagePayload::Json(serde_json::json!({"topic": "t1"})),
            ),
        );
        let ipc_bytes = ipc.to_bytes().unwrap();
        rt_b.handle_incoming_from(&ipc_bytes, b_to_a_tx)
            .await
            .unwrap();
        assert!(rt_b.peers_for_topic("t1").contains(&"node-a".to_string()));

        // Silence unused warning for b_to_a_rx if nothing was replied.
        let _ = b_to_a_rx.try_recv();
    }
}

#[cfg(test)]
#[path = "mesh_runtime_bind_tests.rs"]
mod bind_tests;
