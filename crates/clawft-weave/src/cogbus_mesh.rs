//! This daemon's mesh topics (ADR-122 section 14): the broker's
//! [`MeshTopicHub`] over the kernel's own mesh connections.
//!
//! - Frames: `TopicRequest` 0x12 and `TopicData` 0x13 travel raw on the peer
//!   connection ([`MeshRuntime::try_send_frame`], [`MeshRuntime::set_frame_sink`]).
//!   The runtime hands over only frames from peers admission verified; the hub
//!   serves only peers whose grant qualifies (an enforced-admission full node,
//!   the inference proxy's standard).
//! - Exposures: each topic is the ADR-120 exposure `topic.<kind>/<source_id>`
//!   in the daemon's registry ([`crate::expose_rt::global`]). It is served
//!   only while it is on, its audience is not `local`, and that audience
//!   covers the peer: `paired` and `project:<id>` need the peer to be a paired
//!   placement target (a project's membership is not known per peer on this
//!   node, so `project:` is never wider than `paired`), `nodes:` names it. The
//!   operator's `rate_per_min` and `max_request_bytes` (as bytes per second)
//!   can only lower the cog's own `[mesh_limits]`.
//! - Every subscribe, refusal and end is an exposure activity record.
//! - Streams (B6d) need the broker socket: the hub's taps and proxies are
//!   internal instances that connect to it.
//! - `weaver expose on topic.*` is refused unless the cog writing it declared
//!   `[mesh_limits]` ([`check_expose`]).
//!
//! Service mode (ADR-103): the daemon does not own the mesh there, so there is
//! no runtime to hang the frames on and mesh topics are off, as inference
//! serving is.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use clawft_kernel::exposure::{Audience, ExposureKind, ExposureRegistry, ExposureState, NewActivity};
use clawft_kernel::infer_proxy::types::qualifies;
use clawft_kernel::mesh_delivery::PeerCtx;
use clawft_kernel::mesh_framing::{FrameType, MeshFrame};
use clawft_kernel::mesh_runtime::{MeshRuntime, PeerFrameSink};
use clawft_types::placement::TrustTier;
use weftos_cogbus::Broker;
use weftos_cogbus::mesh::{
    ExposureView, FrameKind, MeshAudience, MeshConfig, MeshLink, MeshRecord, MeshTopicHub, PeerInfo, TopicExposures,
};

/// How often each peer is told what it may subscribe to.
pub const ADVERT_EVERY: Duration = Duration::from_secs(10);

static HUB: OnceLock<Arc<MeshTopicHub>> = OnceLock::new();

/// The running hub, once [`start`] ran.
pub fn hub() -> Option<&'static Arc<MeshTopicHub>> {
    HUB.get()
}

/// End remote subscriptions of exposure `id` at once (`weaver expose off`).
pub fn withdraw(id: &str) {
    if let Some(h) = hub() {
        h.withdraw(id);
    }
}

/// May exposure `id` be switched on? A topic only when the cog writing it
/// declared its own `[mesh_limits]` (ADR-122 section 14); anything else, yes.
pub fn check_expose_on(broker: &Broker, id: &str) -> Result<(), String> {
    match id.strip_prefix("topic.") {
        Some(topic) => broker.topic_limits_declared(topic),
        None => Ok(()),
    }
}

/// [`check_expose_on`] against this daemon's broker.
pub fn check_expose(id: &str) -> Result<(), String> {
    if !id.starts_with("topic.") {
        return Ok(());
    }
    let broker = hub()
        .map(|h| h.broker().clone())
        .or_else(|| crate::cogbus_serve::runtime().map(|r| r.broker.clone()))
        .ok_or("the cogbus is not running on this daemon, so no cog's limits are known")?;
    check_expose_on(&broker, id)
}

/// After an operator change to `id`: end its remote subscriptions at once if
/// it is not on any more; a narrowed audience or cleared `--raw` ends the
/// subscriptions it no longer covers at the hub's next pump.
pub fn withdraw_if_narrowed(id: &str) {
    let off = crate::expose_rt::global().and_then(|r| r.get(id)).is_none_or(|e| e.state != ExposureState::On);
    if off {
        withdraw(id);
    }
    if let Some(h) = hub() {
        h.pump();
    }
}

struct RtLink(Arc<MeshRuntime>);

impl MeshLink for RtLink {
    fn send(&self, node: &str, kind: FrameKind, payload: Vec<u8>) -> bool {
        let frame_type = match kind {
            FrameKind::Request => FrameType::TopicRequest,
            FrameKind::Data => FrameType::TopicData,
        };
        self.0.try_send_frame(node, &MeshFrame { frame_type, payload })
    }
}

struct Frames(Arc<MeshTopicHub>, FrameKind);

impl PeerFrameSink for Frames {
    fn on_peer_frame(&self, ctx: &PeerCtx, frame: MeshFrame) {
        let peer = PeerInfo { node_id: ctx.peer_id.clone(), admitted: ctx.grant().as_ref().is_some_and(qualifies) };
        self.0.on_frame(&peer, self.1, &frame.payload);
    }
}

/// Is `node` paired with this one (a placement target at tier `paired` or
/// `pinned`)?
pub type PairedFn = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// The exposure registry as the hub sees it.
pub struct RegistryExposures {
    reg: Option<Arc<ExposureRegistry>>,
    paired: PairedFn,
}

impl RegistryExposures {
    /// Over `reg` (tests), or the daemon's global registry when `None`.
    pub fn new(reg: Option<Arc<ExposureRegistry>>, paired: PairedFn) -> Self {
        Self { reg, paired }
    }

    fn registry(&self) -> Option<Arc<ExposureRegistry>> {
        self.reg.clone().or_else(crate::expose_rt::global)
    }
}

/// Paired per the daemon's placement plane, once it is built.
pub fn plane_paired() -> PairedFn {
    Arc::new(|node: &str| {
        crate::workload_place_rpc::plane_if_built()
            .is_some_and(|p| p.targets().iter().any(|t| t.node_id == node && t.tier >= TrustTier::Paired))
    })
}

impl TopicExposures for RegistryExposures {
    fn serve(&self, id: &str, peer: &str) -> Result<ExposureView, String> {
        let reg = self.registry().ok_or("the exposure registry is not running")?;
        let e = reg.get(id).ok_or("not registered")?;
        if e.kind != ExposureKind::Topic {
            return Err("not a topic exposure".into());
        }
        // A topic is announced exactly while it is on for a remote audience:
        // the hub offers it in every advert then.
        reg.set_announced(id, e.state == ExposureState::On && e.audience.is_remote());
        reg.check_serve(id).map_err(|r| r.reason().to_string())?;
        let audience = match &e.audience {
            Audience::Local => return Err("audience is local".into()),
            Audience::Paired if (self.paired)(peer) => MeshAudience::Paired,
            Audience::Project(p) if (self.paired)(peer) => MeshAudience::Project(p.clone()),
            Audience::Nodes(n) if n.iter().any(|x| x == peer) => MeshAudience::Nodes(n.clone()),
            Audience::Paired | Audience::Project(_) => return Err("this node is not paired".into()),
            Audience::Nodes(_) => return Err("this node is not in the audience".into()),
        };
        Ok(ExposureView {
            audience,
            raw: e.raw,
            rate_per_min: e.limits.rate_per_min,
            bytes_per_sec: e.limits.max_request_bytes,
        })
    }

    fn record(&self, r: MeshRecord) {
        if let Some(reg) = self.registry() {
            reg.record(NewActivity {
                exposure: r.exposure,
                caller_node: r.caller_node,
                caller_user: None,
                bytes_in: 0,
                bytes_out: r.bytes_out,
                duration_ms: r.duration_ms,
                outcome: r.outcome,
            });
        }
    }
}

/// A hub over `broker` and `rt` with its frame sinks registered on `rt`.
pub fn build(broker: Arc<Broker>, rt: Arc<MeshRuntime>, exposures: Arc<dyn TopicExposures>) -> Arc<MeshTopicHub> {
    let hub = MeshTopicHub::new(broker, Arc::new(RtLink(rt.clone())), exposures, MeshConfig::default());
    rt.set_frame_sink(FrameType::TopicRequest, Arc::new(Frames(hub.clone(), FrameKind::Request)));
    rt.set_frame_sink(FrameType::TopicData, Arc::new(Frames(hub.clone(), FrameKind::Data)));
    hub
}

/// [`build`] once for the daemon (a second call returns the first hub).
fn install(broker: Arc<Broker>, rt: Arc<MeshRuntime>, exposures: Arc<dyn TopicExposures>) -> Arc<MeshTopicHub> {
    HUB.get_or_init(|| build(broker, rt, exposures)).clone()
}

/// Verified peers of `rt`, the ones adverts go to.
pub fn verified_peers(rt: Arc<MeshRuntime>) -> Arc<dyn Fn() -> Vec<PeerInfo> + Send + Sync> {
    Arc::new(move || {
        rt.peer_ids()
            .into_iter()
            .filter(|p| rt.peer_verified(p))
            .map(|node_id| PeerInfo { node_id, admitted: true })
            .collect()
    })
}

/// Install and run the hub for the daemon's life: pump, adverts to verified
/// peers, and a peer that leaves ends its subscriptions both ways.
pub fn start(broker: Arc<Broker>, rt: Arc<MeshRuntime>, socket: Option<std::path::PathBuf>) {
    if hub().is_some() {
        return;
    }
    let hub = install(broker, rt.clone(), Arc::new(RegistryExposures::new(None, plane_paired())));
    match socket {
        Some(p) => hub.set_socket(p),
        None => tracing::warn!(target: "cogbus", "no broker socket: stream topics do not cross the mesh"),
    }
    tokio::spawn(hub.clone().run(verified_peers(rt.clone()), ADVERT_EVERY));
    let mut rx = rt.subscribe_peer_events();
    tokio::spawn(async move {
        use tokio::sync::broadcast::error::RecvError;
        loop {
            match rx.recv().await {
                Ok(clawft_kernel::mesh_discovery::MeshPeerEvent::Left { node_id }) => hub.peer_down(&node_id),
                Ok(_) | Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => break,
            }
        }
    });
    tracing::info!(target: "cogbus", "mesh topics on (frames 0x12/0x13)");
}

#[cfg(test)]
#[path = "cogbus_mesh_tests.rs"]
mod tests;
