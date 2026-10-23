//! Mesh accept loop and seed-connect loop (P3-K0).
//!
//! Moved out of `boot.rs` unchanged in behaviour so a machine mesh
//! service can reuse them. [`serve_listener`] owns the per-connection
//! Noise responder handshake and the bidirectional pump;
//! [`connect_seeds`] dials the configured seed peers as Noise initiator.

use std::sync::Arc;

use crate::ipc::MessageTarget;
use crate::mesh::{MeshTransport, TransportListener};
use crate::mesh_admit::{
    AdmitContext, AdmitHello, Admission, DialIdentity, AdmissionGate, ChannelBinding, ChannelKind, HelloFailure,
    PeerClass, PeerLimits, Refusal,
};
use crate::mesh_assess::{AssessmentEnvelope, AssessmentTransport};
use crate::mesh_delivery::PeerCtx;
use crate::mesh_ipc::{MeshIpcEnvelope, Scope};
use crate::mesh_limits::{IpSlot, Limits, HANDSHAKE_TIMEOUT, MAX_CONNECTIONS, ROUTE_CHECK};
use crate::mesh_noise::{
    noise_static_public, EncryptedChannel, NoiseChannel, NoiseConfig, PassthroughChannel,
};
use crate::mesh_runtime::{MeshRuntime, RouteTally};
use crate::mesh_leaf::{Prepare, MAX_LEAF_FRAME};
use weftos_leaf_types::link::{parse_parent_scope, SignedPublish, FRAME_MAGIC, ACK_MAGIC};

/// Build the transport for a `kernel.mesh.transport` name.
///
/// `seed_peer` is set when selecting for an outbound seed connection; it
/// only changes the wording of the QUIC-unavailable fallback log.
pub fn transport_for(name: &str, seed_peer: Option<&str>) -> Box<dyn MeshTransport> {
    match name {
        "ws" | "websocket" => Box::new(crate::mesh_ws::WsTransport),
        #[cfg(feature = "quic")]
        "quic" => Box::new(crate::mesh_quic::QuicTransport),
        #[cfg(not(feature = "quic"))]
        "quic" => {
            match seed_peer {
                Some(peer) => tracing::error!(
                    peer = %peer,
                    "seed peer uses quic but binary built without `quic` feature; falling back to tcp"
                ),
                None => tracing::error!(
                    "mesh transport=quic requested but clawft-kernel built without `quic` feature; falling back to tcp"
                ),
            }
            Box::new(crate::mesh_tcp::TcpTransport)
        }
        _ => {
            let _ = seed_peer;
            Box::new(crate::mesh_tcp::TcpTransport)
        }
    }
}

/// Accept connections on `listener` forever, pumping each through
/// `runtime`. Spawn this on a task; it never returns.
///
/// `listen_addr` is only the fallback for the "listener started" log when
/// the listener cannot report its bound address. `gate` decides, on each
/// connection's first frame, whether the peer is admitted (P3-K1);
/// [`AllowAll`](crate::mesh_admit::AllowAll) keeps the pre-admission
/// behaviour.
pub async fn serve_listener(
    runtime: Arc<MeshRuntime>,
    listener: Box<dyn TransportListener>,
    noise: Option<Arc<NoiseConfig>>,
    transport_name: &str,
    listen_addr: &str,
    gate: Arc<dyn AdmissionGate>,
) {
    serve_listener_with(
        runtime,
        listener,
        noise,
        transport_name,
        listen_addr,
        gate,
        Limits::default(),
    )
    .await;
}

/// [`serve_listener`] with explicit [`Limits`] (tests use short ones).
pub async fn serve_listener_with(
    runtime: Arc<MeshRuntime>,
    mut listener: Box<dyn TransportListener>,
    noise: Option<Arc<NoiseConfig>>,
    transport_name: &str,
    listen_addr: &str,
    gate: Arc<dyn AdmissionGate>,
    limits: Limits,
) {
    let bind = listener
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| listen_addr.to_owned());
    tracing::info!(
        transport = transport_name,
        addr = %bind,
        noise = noise.is_some(),
        "mesh listener started"
    );

    // Connection tasks live in a JoinSet so aborting this future (dropping
    // the set) also stops every connection it spawned, and a semaphore caps
    // concurrent connections.
    let mut conns = tokio::task::JoinSet::new();
    if let Some(ingress) = runtime.leaf_ingress().cloned()
        && let Ok(mesh_addr) = bind.parse::<std::net::SocketAddr>() {
            // One endpoint per configured address, all sharing this runtime's
            // single LeafIngress (one registry, keys, replay and ACK state).
            let specs = runtime.leaf_bind_specs();
            let plans: Vec<_> = if specs.is_empty() {
                vec![crate::mesh_leaf_bind::LeafBindSpec::resolve(None, mesh_addr)]
            } else {
                specs.iter().map(|s| crate::mesh_leaf_bind::LeafBindSpec::resolve(Some(s), mesh_addr)).collect()
            };
            let wildcard = runtime.leaf_discovery_any();
            let mut wildcard_ports = std::collections::HashSet::new();
            for plan in plans {
                match plan {
                    Err(e) => tracing::error!(error = %e, "certified leaf listener unavailable; no leaf discovery started"),
                    Ok(plan) => {
                        let rt = Arc::clone(&runtime);
                        let ingress_for_endpoint = Arc::clone(&ingress);
                        // One wildcard discovery socket per discovery port,
                        // shared by every endpoint that uses it.
                        if wildcard && wildcard_ports.insert(plan.discovery.port()) {
                            let rt = Arc::clone(&runtime);
                            let ingress = Arc::clone(&ingress);
                            let port = plan.discovery.port();
                            conns.spawn(async move { run_wildcard_discovery(rt, ingress, port).await });
                        }
                        conns.spawn(async move { run_leaf_endpoint(rt, ingress_for_endpoint, plan, limits, wildcard).await });
                    }
                }
            }
    }
    let slots = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    let per_ip = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    loop {
        while conns.try_join_next().is_some() {}
        match listener.accept().await {
            Ok((stream, peer_addr)) => {
                let Ok(permit) = Arc::clone(&slots).try_acquire_owned() else {
                    tracing::warn!(peer = %peer_addr, max = MAX_CONNECTIONS,
                        "mesh connection cap reached, dropping");
                    continue;
                };
                // The per-IP cap applies under every admission mode: the
                // global cap alone lets one host starve the rest.
                let Some(ip_slot) = IpSlot::acquire(&per_ip, peer_addr.ip(), limits.per_ip) else {
                    tracing::warn!(peer = %peer_addr, max = limits.per_ip,
                        "mesh per-IP connection cap reached, dropping");
                    continue;
                };
                let rt = Arc::clone(&runtime);
                let nc = noise.clone();
                let gate = Arc::clone(&gate);
                conns.spawn(async move {
                    serve_connection(rt, stream, peer_addr, nc, gate, limits, false).await;
                    drop(permit);
                    drop(ip_slot);
                });
            }
            Err(e) => {
                tracing::warn!(error = %e, "mesh accept error");
            }
        }
    }
}

/// One certified-leaf address: bind its TCP listener, then serve it and its
/// discovery socket. A failed bind (for example a tailnet interface that is
/// not up yet) is logged and retried, and never affects the other addresses.
async fn run_leaf_endpoint(
    runtime: Arc<MeshRuntime>,
    ingress: Arc<crate::mesh_leaf::LeafIngress>,
    plan: crate::mesh_leaf_bind::LeafBind,
    limits: Limits,
    wildcard_discovery: bool,
) {
    let mut warned = false;
    let leaf_listener = loop {
        match crate::mesh_tcp::TcpTransport.listen(&plan.tcp.to_string()).await {
            Ok(l) => break l,
            Err(e) => {
                if !warned {
                    tracing::error!(addr = %plan.tcp, error = %e, "certified leaf listener cannot bind; other leaf addresses continue, retrying");
                    warned = true;
                }
                tokio::time::sleep(runtime.leaf_bind_retry()).await;
            }
        }
    };
    let tcp = leaf_listener.local_addr().unwrap_or(plan.tcp);
    let discovery = plan.discovery;
    runtime.note_leaf_bound(crate::mesh_leaf_bind::LeafBind { tcp, discovery });
    tracing::info!(addr = %tcp, discovery = %discovery, "dedicated certified leaf listener started");
    let discover = async move {
        // Wildcard mode answers on one shared socket (run_wildcard_discovery).
        if wildcard_discovery { return; }
        if let Err(e) = ingress.serve_discovery(discovery, tcp.port()).await {
            tracing::error!(error = %e, "leaf discovery stopped");
        }
    };
    tokio::join!(serve_signed_leaf_listener(runtime, leaf_listener, limits), discover);
}

/// `leaf_discovery_any`: discovery UDP on `0.0.0.0:<port>`. Replies advertise a
/// bound leaf TCP listener whose discovery port is `port`, chosen by the
/// probe's subnet. A busy port is retried like a leaf address that cannot bind.
async fn run_wildcard_discovery(runtime: Arc<MeshRuntime>, ingress: Arc<crate::mesh_leaf::LeafIngress>, port: u16) {
    let mut warned = false;
    loop {
        let rt = Arc::clone(&runtime);
        let bounds = move || rt.leaf_bounds().into_iter().filter(|b| b.discovery.port() == port).map(|b| b.tcp).collect();
        match crate::mesh_leaf::serve_wildcard_discovery(&ingress, port, bounds).await {
            Ok(()) => return,
            Err(e) => {
                if !warned {
                    tracing::error!(port, error = %e, "wildcard leaf discovery stopped; retrying");
                    warned = true;
                }
                tokio::time::sleep(runtime.leaf_bind_retry()).await;
            }
        }
    }
}

/// Separate bounded TCP ingress for WLF1 only. The ordinary mesh listener
/// keeps its configured Noise requirement; no plaintext fallback is added
/// to the node-to-node port.
async fn serve_signed_leaf_listener(runtime: Arc<MeshRuntime>, mut listener: Box<dyn TransportListener>, limits: Limits) {
    let slots = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    let per_ip = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let mut conns = tokio::task::JoinSet::new();
    loop {
        while conns.try_join_next().is_some() {}
        match listener.accept().await {
            Ok((stream, peer)) => {
                let Ok(permit) = Arc::clone(&slots).try_acquire_owned() else { continue };
                let Some(ip_slot) = IpSlot::acquire(&per_ip, peer.ip(), limits.per_ip) else { continue };
                let rt = Arc::clone(&runtime);
                conns.spawn(async move {
                    serve_connection(rt, stream, peer, None, Arc::new(crate::mesh_admit::AllowAll), limits, true).await;
                    drop(permit);
                    drop(ip_slot);
                });
            }
            Err(e) => tracing::warn!(error = %e, "certified leaf accept error"),
        }
    }
}

/// What admission granted this connection.
pub(crate) struct Active {
    /// Verified node id; later envelopes must carry it as `source_node`.
    pub(crate) bound: Option<String>,
    pub(crate) limits: PeerLimits,
    pub(crate) trust_scope: bool,
    /// Admitted = hello verified AND the gate enforced AND said Admit.
    /// `bound` alone is only key possession, not membership.
    pub(crate) admitted: bool,
    pub(crate) class: PeerClass,
    pub(crate) remote_static: Option<Vec<u8>>,
    /// This connection must present a signed WLF1 frame for every input.
    pub(crate) signed_leaf: bool,
    pub(crate) leaf_cert: Option<weftos_leaf_types::link::LeafCertificate>,
}

impl Active {
    /// The connection identity handed to the runtime and to delivery.
    fn peer_ctx(&self) -> PeerCtx {
        PeerCtx {
            peer_id: self.bound.clone().unwrap_or_default(),
            node_verified: self.admitted,
            class: self.class,
            remote_static: self.remote_static.clone(),
            src_scope: None,
        }
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Run admission on a connection's first frame. Returns the terms and the
/// frame to process normally (`None` when the frame was the hello).
async fn admit_first_frame(
    channel: &dyn EncryptedChannel,
    kind: ChannelKind,
    gate: &dyn AdmissionGate,
    data: Vec<u8>,
) -> Result<(Active, Option<Vec<u8>>), Refusal> {
    let legacy = AdmitContext { class: PeerClass::Legacy, channel: kind };
    let binding = channel.handshake_hash().map(|h| ChannelBinding {
        handshake_hash: h,
        remote_static: channel.remote_static_key(),
    });
    let (decision, bound, rest) = match AdmitHello::parse_frame(&data) {
        None => (
            gate.admit_unverified(&HelloFailure::Missing, &legacy).await,
            None,
            Some(data),
        ),
        Some(Err(f)) => (gate.admit_unverified(&f, &legacy).await, None, None),
        Some(Ok(hello)) => match hello.verify(binding.as_ref(), unix_now()) {
            Ok(v) => {
                let ctx = AdmitContext { class: v.class(), channel: kind };
                (gate.admit(&v, &ctx).await, Some(v.node_id), None)
            }
            Err(f) => (gate.admit_unverified(&f, &legacy).await, None, None),
        },
    };
    match decision {
        Admission::Refuse(r) => Err(r),
        Admission::Admit(g) => Ok((
            Active {
                admitted: g.admitted && bound.is_some(),
                trust_scope: g.trust_scope && g.admitted && bound.is_some(),
                limits: g.limits,
                bound,
                class: g.class,
                remote_static: channel.remote_static_key().map(<[u8]>::to_vec),
                signed_leaf: false,
                leaf_cert: None,
            },
            rest,
        )),
    }
}

/// The node id a frame says it comes from: an envelope's `source_node`, or an
/// assessment frame's. `None` for anything else.
pub(crate) fn claimed_source(data: &[u8]) -> Option<String> {
    if let Ok(env) = MeshIpcEnvelope::from_bytes(data) {
        return Some(env.source_node);
    }
    AssessmentTransport::try_extract_payload(data)
        .ok()
        .flatten()
        .and_then(|p| AssessmentEnvelope::from_bytes(&p).ok())
        .map(|e| e.source_node)
}

/// Apply post-admission rules to one frame. `None` drops it.
///
/// - `source_node` must equal the verified node id (envelopes and
///   assessment frames alike).
/// - Leaf limits: only `substrate/<own-id>/...` topics and `mesh.subscribe`.
/// - `src_scope` is stripped unless admission verified the peer.
pub(crate) fn screen_frame(data: Vec<u8>, act: &Active) -> Option<Vec<u8>> {
    match MeshIpcEnvelope::from_bytes(&data) {
        Ok(mut env) => {
            // The old ESP32 plaintext `ipc.publish` path must not remain a
            // leaf-write bypass while rollout runs in observe mode.
            if !act.admitted {
                let protected = match (&env.message.target, &env.message.payload) {
                    (MessageTarget::Topic(t), _) if t.starts_with("substrate/") || t.starts_with("mesh.leaf.") => true,
                    (MessageTarget::Topic(t), crate::ipc::MessagePayload::Json(v)) if t == "ipc.publish" || t == "mesh.subscribe" =>
                        v.get("topic").and_then(|t| t.as_str()).is_some_and(|t| t.starts_with("mesh.leaf.") || t.starts_with("substrate/")),
                    _ => false,
                };
                if protected {
                    tracing::warn!("dropping unsigned legacy leaf frame");
                    return None;
                }
            }
            if let Some(id) = &act.bound
                && &env.source_node != id
            {
                tracing::warn!(claimed = %env.source_node, verified = %id,
                    "dropping envelope: source_node differs from admitted node id");
                return None;
            }
            if act.limits == PeerLimits::Leaf {
                // The established, Noise-authenticated CAP_LEAF grant may
                // publish only to its own substrate prefix. WLF1 is required
                // for the newer certified input/announce path and has its
                // own per-publish replay check in the pump.
                let id = act.bound.as_deref().unwrap_or_default();
                let allowed = matches!(&env.message.target, MessageTarget::Topic(t)
                    if t == "mesh.subscribe" || t.starts_with(&format!("substrate/{id}/")));
                if !allowed {
                    tracing::warn!(node = id, "dropping leaf envelope outside substrate/<id>/");
                    return None;
                }
            }
            if env.src_scope.is_some() && !act.trust_scope {
                env.src_scope = None;
                return env.to_bytes().ok();
            }
            Some(data)
        }
        Err(_) => {
            if act.limits == PeerLimits::Leaf {
                return None;
            }
            if let Some(id) = &act.bound {
                let src = AssessmentTransport::try_extract_payload(&data)
                    .ok()
                    .flatten()
                    .and_then(|p| AssessmentEnvelope::from_bytes(&p).ok())
                    .map(|e| e.source_node);
                if src.is_some_and(|s| &s != id) {
                    tracing::warn!(verified = %id, "dropping assessment frame: source_node mismatch");
                    return None;
                }
            }
            Some(data)
        }
    }
}

/// Decode only the leaf's own publish/subscription shape. The outer signed
/// target must agree with the inner routed topic; a signature over one topic
/// cannot authorize another topic hidden inside JSON.
fn leaf_routed_envelope(frame: &SignedPublish) -> Option<Vec<u8>> {
    let id = frame.cert.leaf_id();
    let mut env = MeshIpcEnvelope::from_bytes(&frame.payload).ok()?;
    if env.source_node != id { return None; }
    // A certificate binds the parent scope, but the envelope's tenant
    // claims are caller-controlled and cannot be inferred from that string.
    // Never pass those claims to the delivery authorizer.
    env.src_scope = None;
    let (user_id, project_id) = parse_parent_scope(&frame.cert.parent_scope).ok()?;
    env.dest_scope = Some(Scope { user_id: user_id.to_owned(), project_id: project_id.map(str::to_owned) });
    let crate::ipc::MessagePayload::Json(v) = &env.message.payload else { return None };
    let inner_topic = v.get("topic")?.as_str()?;
    match (&env.message.target, frame.target.as_str()) {
        (MessageTarget::Topic(t), "mesh.subscribe") if t == "mesh.subscribe"
            && inner_topic == weftos_leaf_types::push_topic(&id) => env.to_bytes().ok(),
        (MessageTarget::Topic(t), target) if t == "ipc.publish" && inner_topic == target
            && (target == format!("mesh.leaf.{id}.input") || target == format!("mesh.leaf.{id}.announce")) => {
            let message = v.get("message")?.as_str()?;
            let payload = match serde_json::from_str::<serde_json::Value>(message) {
                Ok(json) => crate::ipc::MessagePayload::Json(json),
                Err(_) => crate::ipc::MessagePayload::Text(message.to_owned()),
            };
            env.message.target = MessageTarget::Topic(target.to_owned());
            env.message.payload = payload;
            env.to_bytes().ok()
        }
        _ => None,
    }
}

/// One accepted connection: optional Noise responder handshake, admission
/// on the first frame, then the bidirectional pump until either side
/// closes.
async fn serve_connection(
    rt: Arc<MeshRuntime>,
    stream: Box<dyn crate::mesh::MeshStream>,
    peer_addr: std::net::SocketAddr,
    nc: Option<Arc<NoiseConfig>>,
    gate: Arc<dyn AdmissionGate>,
    limits: Limits,
    leaf_only: bool,
) {
    tracing::info!(
        peer = %peer_addr,
        noise = nc.is_some(),
        "mesh peer connected"
    );

    // Optionally wrap in Noise encryption.
    let channel: Box<dyn EncryptedChannel> = match &nc {
        Some(cfg) => match tokio::time::timeout(
            HANDSHAKE_TIMEOUT,
            NoiseChannel::respond(stream, cfg),
        )
        .await
        .unwrap_or_else(|_| Err(crate::mesh::MeshError::Handshake("handshake timed out".into())))
        {
            Ok(ch) => {
                tracing::info!(peer = %peer_addr, "noise handshake complete");
                Box::new(ch)
            }
            Err(e) => {
                tracing::warn!(peer = %peer_addr, error = %e, "noise handshake failed, dropping");
                return;
            }
        },
        None => Box::new(PassthroughChannel::new(stream)),
    };
    let kind = if nc.is_some() { ChannelKind::Noise } else { ChannelKind::Passthrough };
    let (out_tx, out_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(256);
    pump(&rt, channel, kind, &peer_addr.to_string(), &*gate, limits, out_tx, out_rx, None, RouteTally::default(), false, leaf_only).await;
}

/// The bidirectional pump shared by accepted and dialled connections.
///
/// The kernel pushes frames into `out_tx` (via `MeshRuntime::send_to_peer`)
/// and this task drains `out_rx` back through the encrypted stream. This is
/// what lets the topic forwarder in `A2ARouter` deliver pushes to inbound
/// leaf peers that subscribed via `mesh.subscribe`. `active` is `None` until
/// the first frame has been through admission; a dialled connection starts
/// with `active` set (we chose the peer, so there is no admission step; it is
/// not *admitted* in the membership sense) and its route already counted in
/// `tally`. Nested seed dials instead install `active` only after reciprocal
/// cryptographic admission, and retain the real gate for live revocation.
#[allow(clippy::too_many_arguments)]
async fn pump(
    rt: &Arc<MeshRuntime>,
    mut channel: Box<dyn EncryptedChannel>,
    kind: ChannelKind,
    peer_addr: &str,
    gate: &dyn AdmissionGate,
    limits: Limits,
    out_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    mut out_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    mut active: Option<Active>,
    tally: RouteTally,
    dialled: bool,
    leaf_only: bool,
) {
    // Removes this connection's routes however the pump ends, including
    // when the task is aborted mid-await.
    let _routes = ConnRoutes { rt: Arc::clone(rt), tx: out_tx.clone() };
    // Bidirectional loop. `handle_incoming_from` auto-registers the peer
    // by `envelope.source_node` on first arrival so the kernel can route
    // back.
    //
    // Cancel-safety (ADR-010 / WEFT-18): both racing futures must be
    // cancel-safe.
    // - `out_rx.recv()` — tokio mpsc, cancel-safe.
    // - `channel.recv_encrypted()` → `MeshStream::recv` — contract on
    //   `MeshStream` requires cancel-safe framing (`TcpMeshStream` keeps
    //   partial progress).
    // Losing either race must not drop a complete message already removed
    // from its source, and must not desync the TCP length-prefix stream.
    // Admission runs in the handler of a completed `recv`, never inside a
    // raced future.
    let strict = gate.strict();
    // Set once this connection carries a route; when the route is removed
    // from under it (`disconnect_peer`, e.g. `weaver mesh peer revoke`, or a
    // replacement connection), the connection is closed instead of lingering
    // and re-registering on its next frame.
    let mut routed = tally.live() > 0;
    let mut route_check = tokio::time::interval(ROUTE_CHECK);
    route_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The silence timers run from the last frame in either direction; the
    // route check's own ticks must not reset them.
    let mut last_activity = tokio::time::Instant::now();
    // Frames dropped on a dialled connection for speaking as an id other than
    // the one it is bound to (or claiming a routed one). A seed that keeps
    // doing it is cut off and redialled with backoff, not left spamming.
    let mut mismatched = 0u32;
    loop {
        // Every mode bounds the silence before the first frame; strict gates
        // also bound how long an admitted peer may stay silent. Lenient gates
        // keep the pre-admission behaviour (no idle limit) so slow legacy
        // leaves still work.
        // A dialled connection (which starts with `active` set) bounds
        // inbound silence so a half-open seed is detected and redialled.
        let limit = if active.is_none() {
            Some(limits.first_frame)
        } else {
            (strict || dialled).then_some(limits.idle)
        };
        let timer = async {
            match limit {
                Some(d) => tokio::time::sleep_until(last_activity + d).await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            _ = timer => {
                tracing::warn!(peer = %peer_addr, "mesh connection timed out, dropping");
                break;
            }
            _ = route_check.tick() => {
                if routed && tally.live() == 0 {
                    tracing::info!(peer = %peer_addr, "mesh route removed (disconnect or revocation), closing");
                    break;
                }
                if let Some(id) = active.as_ref().and_then(|a| a.bound.as_deref())
                    && gate.is_revoked(id)
                {
                    tracing::warn!(peer = %peer_addr, node = id, "mesh peer revoked, closing");
                    break;
                }
                if let Some(cert) = active.as_ref().and_then(|a| a.leaf_cert.as_ref())
                    && !rt.leaf_ingress().is_some_and(|ingress| ingress.still_enrolled(cert, unix_now())) {
                    tracing::warn!(peer = %peer_addr, node = %cert.leaf_id(), "certified leaf enrollment expired or revoked; closing route");
                    break;
                }
            }
            inbound = channel.recv_encrypted() => match inbound {
                Ok(data) => {
                    last_activity = tokio::time::Instant::now();
                    if leaf_only && !data.starts_with(FRAME_MAGIC) {
                        tracing::warn!(peer = %peer_addr, "non-certified frame on leaf-only listener");
                        break;
                    }
                    if data.starts_with(FRAME_MAGIC) {
                        if data.len() > MAX_LEAF_FRAME { break; }
                        let Some(ingress) = rt.leaf_ingress() else {
                            tracing::warn!(peer = %peer_addr, "signed leaf frame but no registry installed");
                            break;
                        };
                        let Ok(frame) = weftos_leaf_types::decode::<SignedPublish>(&data[FRAME_MAGIC.len()..]) else { break };
                        let id = frame.cert.leaf_id();
                        if active.as_ref().is_some_and(|a| !a.signed_leaf || a.bound.as_deref() != Some(&id)
                            || a.leaf_cert.as_ref() != Some(&frame.cert)) { break; }
                        let _guard = ingress.lock.lock().await;
                        let prepared = match ingress.prepare(&frame, unix_now()) {
                            Ok(p) => p,
                            Err(e) => { tracing::warn!(peer = %peer_addr, node = %id, error = %e, "leaf frame refused"); break; }
                        };
                        let Some(routed_frame) = leaf_routed_envelope(&frame) else {
                            tracing::warn!(peer = %peer_addr, node = %id, "leaf payload does not match signed target");
                            break;
                        };
                        if active.is_none() {
                            if !rt.register_authenticated_as(id.clone(), out_tx.clone(), true, PeerClass::Leaf, &tally) {
                                tracing::warn!(peer = %peer_addr, node = %id, "leaf route refused");
                                break;
                            }
                            active = Some(Active {
                                bound: Some(id.clone()), limits: PeerLimits::Leaf, trust_scope: false,
                                admitted: true, class: PeerClass::Leaf, remote_static: None, signed_leaf: true,
                                leaf_cert: Some(frame.cert.clone()),
                            });
                            routed = true;
                        }
                        // A duplicate subscribe after reconnect is a safe
                        // control-plane refresh: route subscriptions are
                        // connection-local, unlike the durable publish floor.
                        if prepared == Prepare::New || frame.target == "mesh.subscribe" {
                            let ctx = active.as_ref().expect("set above").peer_ctx();
                            if let Err(e) = rt.handle_incoming_tallied(&routed_frame, out_tx.clone(), Some(&ctx), Some(&tally)).await {
                                tracing::warn!(peer = %peer_addr, node = %id, error = %e, "leaf delivery failed");
                                break;
                            }
                            if prepared == Prepare::New
                                && let Err(e) = ingress.commit(&frame) {
                                tracing::error!(node = %id, error = %e, "leaf replay floor commit failed; no ACK");
                                break;
                            }
                        }
                        let ack = ingress.ack(&frame);
                        let Ok(mut ack_bytes) = weftos_leaf_types::encode(&ack) else { break };
                        let mut wire = Vec::with_capacity(ACK_MAGIC.len() + ack_bytes.len());
                        wire.extend_from_slice(ACK_MAGIC);
                        wire.append(&mut ack_bytes);
                        if !write_frame(&mut *channel, &wire, limits.write_stall, peer_addr).await { break; }
                        continue;
                    }
                    if active.as_ref().is_some_and(|a| a.signed_leaf) {
                        tracing::warn!(peer = %peer_addr, "signed leaf sent an unsigned frame");
                        break;
                    }
                    let reciprocal = active.is_none() && serde_json::from_slice::<serde_json::Value>(&data)
                        .ok().is_some_and(|v| v.get("reciprocal").and_then(|v| v.as_bool()) == Some(true));
                    let frame = match active {
                        Some(_) => Some(data),
                        None => match admit_first_frame(&*channel, kind, gate, data).await {
                            Ok((act, rest)) => {
                                tracing::info!(peer = %peer_addr,
                                    node = act.bound.as_deref().unwrap_or("-"), "mesh peer admitted");
                                // Key-authenticated: join now, keyed by the
                                // verified id, not on the first envelope.
                                // Admitted peers only: a key-valid peer under
                                // off/observe is not a member, so it keeps the
                                // first-envelope path.
                                if act.admitted
                                    && let Some(id) = act.bound.as_deref()
                                    && !rt.register_authenticated_as(
                                        id.to_owned(), out_tx.clone(), true, act.class, &tally)
                                {
                                    tracing::warn!(peer = %peer_addr, node = id,
                                        "route refused for admitted peer, closing");
                                    break;
                                }
                                if reciprocal {
                                    let Some(auth) = rt.authentication() else { break };
                                    let Some(hash) = channel.handshake_hash() else { break };
                                    if !act.admitted { break; }
                                    let reply = auth.identity.hello(hash, &auth.noise_static, unix_now());
                                    if !write_frame(&mut *channel, &reply.to_bytes(), limits.write_stall, peer_addr).await { break; }
                                }
                                active = Some(act);
                                rest
                            }
                            Err(r) => {
                                tracing::warn!(peer = %peer_addr, code = r.code, detail = %r.detail,
                                    "mesh peer refused, dropping");
                                break;
                            }
                        },
                    };
                    let Some(frame) = frame else { continue };
                    if dialled
                        && let Some(act) = active.as_mut()
                        && act.bound.is_none()
                    {
                        // The seed's id is not authenticated, so bind the
                        // connection to the first id it claims (in an
                        // envelope or an assessment frame) and never let it
                        // claim an id another connection already routes. A
                        // frame that names no id cannot be attributed to
                        // anyone: it is dropped, not passed on unbound.
                        let Some(claimed) = claimed_source(&frame) else {
                            tracing::warn!(peer = %peer_addr,
                                "dialled peer sent a frame naming no node id before binding, dropping");
                            continue;
                        };
                        if rt.route_is_foreign(&claimed, &out_tx) {
                            tracing::warn!(peer = %peer_addr, claimed = %claimed,
                                "dialled peer claims an id routed elsewhere, dropping frame");
                            mismatched += 1;
                            if mismatched >= MAX_ID_MISMATCHES {
                                tracing::warn!(peer = %peer_addr, "dialled peer keeps claiming routed ids, closing");
                                break;
                            }
                            continue;
                        }
                        act.bound = Some(claimed);
                    }
                    let Some(act) = active.as_ref() else { break };
                    // A revocation after admission: the next frame must not
                    // re-register the route.
                    if let Some(id) = act.bound.as_deref()
                        && gate.is_revoked(id)
                    {
                        tracing::warn!(peer = %peer_addr, node = id, "mesh peer revoked, closing");
                        break;
                    }
                    let screened = screen_frame(frame, act);
                    if screened.is_none() && dialled && act.bound.is_some() {
                        // `screen_frame` has logged this one; the connection
                        // is cut after a few so the log cannot be flooded.
                        mismatched += 1;
                        if mismatched >= MAX_ID_MISMATCHES {
                            tracing::warn!(peer = %peer_addr, node = act.bound.as_deref().unwrap_or("-"),
                                "dialled peer keeps speaking for other ids, closing");
                            break;
                        }
                    }
                    if let Some(frame) = screened {
                        let ctx = act.peer_ctx();
                        if let Err(e) = rt.handle_incoming_tallied(&frame, out_tx.clone(), Some(&ctx), Some(&tally)).await {
                            tracing::debug!(error = %e, "mesh message handling error");
                        }
                        routed = routed || tally.live() > 0;
                    }
                }
                Err(_) => break,
            },
            outbound = out_rx.recv() => match outbound {
                Some(data) => {
                    if let Some(cert) = active.as_ref().and_then(|a| a.leaf_cert.as_ref())
                        && !rt.leaf_ingress().is_some_and(|ingress| ingress.still_enrolled(cert, unix_now())) {
                        tracing::warn!(peer = %peer_addr, node = %cert.leaf_id(), "certified leaf revoked before outbound push; closing route");
                        break;
                    }
                    if !dialled {
                        last_activity = tokio::time::Instant::now();
                    }
                    if !write_frame(&mut *channel, &data, limits.write_stall, peer_addr).await {
                        break;
                    }
                }
                None => break,
            },
        }
    }
}

/// Write one frame, giving up after `limit`. False: close the connection
/// (the write failed, or the peer stopped reading and the frame never left).
/// A cancelled write can leave a partial frame on the wire; the connection
/// is closed right after, so the stream is never reused out of sync.
async fn write_frame(channel: &mut dyn EncryptedChannel, data: &[u8], limit: std::time::Duration, peer: &str) -> bool {
    match tokio::time::timeout(limit, channel.send_encrypted(data)).await {
        Ok(Ok(())) => true,
        Ok(Err(_)) => false,
        Err(_) => {
            tracing::warn!(peer, timeout_s = limit.as_secs(), "mesh peer stopped reading; closing stalled connection");
            false
        }
    }
}

/// Drops every route that still points at one connection (a verified route
/// must not outlive its connection) when the pump ends or is aborted.
struct ConnRoutes {
    rt: Arc<MeshRuntime>,
    tx: tokio::sync::mpsc::Sender<Vec<u8>>,
}

impl Drop for ConnRoutes {
    fn drop(&mut self) {
        self.rt.disconnect_channel(&self.tx);
    }
}

/// Frames a dialled connection may have dropped for naming the wrong id
/// before it is closed.
const MAX_ID_MISMATCHES: u32 = 3;

/// First reconnect delay; doubles per consecutive failure up to
/// [`SEED_BACKOFF_MAX`]. Each wait is jittered to 50-100% of its nominal
/// value so a fleet restarted together does not redial in lockstep.
const SEED_BACKOFF_BASE: std::time::Duration = std::time::Duration::from_secs(1);
const SEED_BACKOFF_MAX: std::time::Duration = std::time::Duration::from_secs(60);
/// Inbound silence after which a dialled connection is considered half-open.
const SEED_IDLE: std::time::Duration = std::time::Duration::from_secs(300);
/// A connection that stayed up this long resets the backoff.
const SEED_STABLE: std::time::Duration = std::time::Duration::from_secs(30);

/// Backoff for the `failures`-th consecutive failure (0-based), jittered.
pub(crate) fn seed_backoff(
    failures: u32,
    base: std::time::Duration,
    max: std::time::Duration,
) -> std::time::Duration {
    let nominal = base.saturating_mul(1u32 << failures.min(16)).min(max);
    let jitter = rand::random::<f64>() * 0.5 + 0.5;
    nominal.mul_f64(jitter)
}

/// Retry timing for a seed dial loop (tests shorten it).
#[derive(Clone, Copy)]
pub(crate) struct SeedTiming {
    pub base: std::time::Duration,
    pub max: std::time::Duration,
    pub stable: std::time::Duration,
    /// Inbound silence after which a dialled connection is dropped and redialled.
    pub idle: std::time::Duration,
}

impl Default for SeedTiming {
    fn default() -> Self {
        Self { base: SEED_BACKOFF_BASE, max: SEED_BACKOFF_MAX, stable: SEED_STABLE, idle: SEED_IDLE }
    }
}

/// One configured seed: the address to dial and, when the operator pinned
/// it, the node id the seed must claim. Written `addr` or `addr#node-id`.
///
/// A seed sends no hello, so its id is otherwise whatever it claims first.
/// A pin binds the connection to that id from the start: frames naming any
/// other id are dropped, so a seed cannot speak for (or impersonate while
/// offline) another node. The pin is the operator's word, not a proof: the
/// seed's peer is still held as unverified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedSpec {
    /// Address to dial (`host:port`, or a URL for the transport).
    pub addr: String,
    /// Node id the seed is expected to be, if pinned.
    pub node_id: Option<String>,
}

/// Parse one `seed_peers` entry (`addr` or `addr#node-id`).
pub fn parse_seed(entry: &str) -> Result<SeedSpec, String> {
    let entry = entry.trim();
    let (addr, id) = match entry.rsplit_once('#') {
        Some((a, i)) => (a, Some(i)),
        None => (entry, None),
    };
    if addr.is_empty() || addr.chars().any(|c| c.is_whitespace() || c.is_control() || c == '#') {
        return Err(format!("seed address {addr:?} is not usable"));
    }
    let node_id = match id {
        None => None,
        Some(i) if i.is_empty() || i.len() > 128 => {
            return Err(format!("seed {addr}: pinned node id must be 1 to 128 characters"));
        }
        Some(i) if !i.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-')) => {
            return Err(format!("seed {addr}: pinned node id has characters a node id cannot"));
        }
        Some(i) => Some(i.to_owned()),
    };
    Ok(SeedSpec { addr: addr.to_owned(), node_id })
}

/// The seeds to dial: parsed, unusable entries skipped with a warning, and
/// one entry per address (the first wins; a different pin on a repeat is
/// reported, not honoured).
pub fn seed_specs(entries: &[String]) -> Vec<SeedSpec> {
    let mut seen: std::collections::HashMap<String, Option<String>> = Default::default();
    let mut out = Vec::new();
    for e in entries {
        match parse_seed(e) {
            Err(why) => tracing::warn!(seed = %e.split('#').next().unwrap_or(""), "{why}; seed skipped"),
            Ok(spec) => match seen.get(&spec.addr) {
                None => {
                    seen.insert(spec.addr.clone(), spec.node_id.clone());
                    out.push(spec);
                }
                Some(first) if *first != spec.node_id => tracing::warn!(peer = %spec.addr,
                    "seed listed twice with different pinned ids; the first is used"),
                Some(_) => {}
            },
        }
    }
    out
}

/// Dial each seed peer in its own task (Noise initiator when `noise` is
/// set) and register it with `runtime`. Returns immediately.
///
/// Entries are `addr` or `addr#node-id` (see [`SeedSpec`]).
///
/// Connections are bidirectional (inbound frames from the seed reach the
/// runtime) and are redialled with jittered exponential backoff when they
/// drop. The returned handles run until aborted; abort them to stop
/// dialling and close the connections.
pub fn connect_seeds(
    runtime: &Arc<MeshRuntime>,
    seed_peers: &[String],
    transport_name: &str,
    noise: Option<Arc<NoiseConfig>>,
    identity: Option<Arc<DialIdentity>>,
) -> Vec<tokio::task::JoinHandle<()>> {
    connect_seeds_with(runtime, seed_peers, transport_name, noise, identity, SeedTiming::default())
}

pub(crate) fn connect_seeds_with(
    runtime: &Arc<MeshRuntime>,
    seed_peers: &[String],
    transport_name: &str,
    noise: Option<Arc<NoiseConfig>>,
    identity: Option<Arc<DialIdentity>>,
    timing: SeedTiming,
) -> Vec<tokio::task::JoinHandle<()>> {
    seed_specs(seed_peers)
        .into_iter()
        .map(|SeedSpec { addr, node_id: expected }| {
            // Weak: an idle dial loop does not keep a dropped runtime alive
            // (an established connection still holds it until it ends).
            let rt = Arc::downgrade(runtime);
            let transport_name = transport_name.to_owned();
            let nc = noise.clone();
            let identity = identity.clone();
            tokio::spawn(async move {
                let mut failures = 0u32;
                loop {
                    let Some(rt) = rt.upgrade() else { return };
                    let started = tokio::time::Instant::now();
                    dial_seed_once(&rt, &addr, expected.as_deref(), &transport_name, nc.clone(), identity.clone(), timing.idle).await;
                    drop(rt);
                    if started.elapsed() >= timing.stable {
                        failures = 0;
                    }
                    let wait = seed_backoff(failures, timing.base, timing.max);
                    failures = failures.saturating_add(1);
                    tracing::info!(peer = %addr, retry_in_ms = wait.as_millis() as u64,
                        "seed connection down, will redial");
                    tokio::time::sleep(wait).await;
                }
            })
        })
        .collect()
}

/// One dial of a seed: connect, handshake, hello, then pump until the
/// connection ends. Returns when it is over (for any reason).
async fn dial_seed_once(
    rt: &Arc<MeshRuntime>,
    addr: &str,
    expected: Option<&str>,
    transport_name: &str,
    nc: Option<Arc<NoiseConfig>>,
    identity: Option<Arc<DialIdentity>>,
    idle: std::time::Duration,
) {
    let auth = rt.authentication();
    let strict_seed = crate::mesh_admit_gate::nested_peer_ceiling_active()
        || auth.is_some_and(|a| a.require_authenticated_seeds);
    if strict_seed && (expected.is_none() || nc.is_none() || identity.is_none()
        || !auth.is_some_and(|a| a.gate.strict())) {
        tracing::warn!(peer = %addr, "nested seed refused: missing pin, Noise, identity or enforcing gate");
        return;
    }
    let transport = transport_for(transport_name, Some(addr));
    let stream = match transport.connect(addr).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(peer = %addr, error = %e, "failed to connect to seed peer");
            return;
        }
    };
    // Optionally wrap in Noise encryption (as initiator).
    let mut channel: Box<dyn EncryptedChannel> = match &nc {
        Some(cfg) => match tokio::time::timeout(HANDSHAKE_TIMEOUT, NoiseChannel::initiate(stream, cfg))
            .await
            .unwrap_or_else(|_| {
                Err(crate::mesh::MeshError::Handshake("handshake timed out".into()))
            }) {
            Ok(ch) => {
                tracing::info!(peer = %addr, "noise handshake complete (initiator)");
                Box::new(ch)
            }
            Err(e) => {
                tracing::warn!(peer = %addr, error = %e, "noise handshake failed");
                return;
            }
        },
        None => Box::new(PassthroughChannel::new(stream)),
    };
    let kind = if nc.is_some() { ChannelKind::Noise } else { ChannelKind::Passthrough };

    // Introduce ourselves before anything else is sent. Only a Noise session
    // has a handshake hash to bind the hello to; plaintext dials send nothing
    // (the far side's gate decides whether that is acceptable).
    if let (Some(id), Some(cfg), Some(hash)) =
        (&identity, &nc, channel.handshake_hash().map(<[u8]>::to_vec))
    {
        match noise_static_public(&cfg.local_private_key) {
            Some(stat) => {
                let hello = id.hello(&hash, &stat, unix_now());
                let hello_bytes = if strict_seed {
                    // This flag only asks for a response; the response itself is
                    // signed over this Noise session and goes through admission.
                    let mut wire = serde_json::to_value(&hello).expect("hello serializes");
                    wire["reciprocal"] = serde_json::Value::Bool(true);
                    serde_json::to_vec(&wire).expect("hello serializes")
                } else { hello.to_bytes() };
                if channel.send_encrypted(&hello_bytes).await.is_err() {
                    tracing::warn!(peer = %addr, "failed to send admission hello");
                    return;
                }
            }
            None => tracing::warn!(peer = %addr, "cannot derive noise static key"),
        }
    }

    if strict_seed {
        let gate = &*auth.expect("checked above").gate;
        let admission = async {
            let frame = channel.recv_encrypted().await.map_err(|e| e.to_string())?;
            let (active, rest) = admit_first_frame(&*channel, kind, gate, frame).await
                .map_err(|e| e.detail)?;
            if !active.admitted || active.bound.as_deref() != expected || rest.is_some() {
                return Err("seed did not prove the granted identity under admission".to_owned());
            }
            Ok(active)
        };
        let active = match tokio::time::timeout(HANDSHAKE_TIMEOUT, admission).await {
            Ok(Ok(active)) => active,
            _ => { tracing::warn!(peer = %addr, "reciprocal seed admission failed"); return; }
        };
        let (out_tx, out_rx) = tokio::sync::mpsc::channel(256);
        let tally = RouteTally::default();
        let id = active.bound.as_ref().expect("verified above");
        if !rt.register_authenticated_as(id.clone(), out_tx.clone(), true, active.class, &tally) { return; }
        // This address alias is installed only AFTER key/session verification and
        // admission, so even the first queued outbound payload cannot leak.
        rt.add_peer_tallied(addr.to_owned(), out_tx.clone(), &tally);
        pump(rt, channel, kind, addr, gate, Limits { idle, ..Limits::default() },
            out_tx, out_rx, Some(active), tally, true, false).await;
        return;
    }

    let (out_tx, out_rx) = tokio::sync::mpsc::channel(256);
    // A pinned id that another connection already routes is not ours to take.
    if let Some(id) = expected
        && rt.route_is_foreign(id, &out_tx)
    {
        tracing::warn!(peer = %addr, pinned = %id,
            "pinned seed id is routed through another connection, not dialling");
        return;
    }
    let tally = RouteTally::default();
    rt.add_peer_tallied(addr.to_owned(), out_tx.clone(), &tally);
    tracing::info!(peer = %addr, noise = nc.is_some(), "connected to seed peer");
    // No hello comes back from a seed, so `bound` is the id the operator
    // pinned for it, or unset until its first frame names an id (see the
    // pump); frames then go through `screen_frame` and the runtime's identity
    // rules like any other.
    let active = Active {
        bound: expected.map(str::to_owned),
        limits: PeerLimits::None,
        trust_scope: false,
        admitted: false,
        class: PeerClass::Legacy,
        remote_static: channel.remote_static_key().map(<[u8]>::to_vec),
        signed_leaf: false,
        leaf_cert: None,
    };
    let limits = Limits { idle, ..Limits::default() };
    pump(rt, channel, kind, addr, &crate::mesh_admit::AllowAll, limits, out_tx, out_rx, Some(active), tally, true, false).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::KernelResult;
    use crate::ipc::{KernelMessage, MessagePayload, MessageTarget};
    use crate::mesh_delivery::{LocalDelivery, PeerCtx};
    use crate::mesh_ipc::{MeshIpcEnvelope, Scope};
    use std::sync::Mutex;

    #[derive(Default)]
    struct Recorder {
        got: Mutex<Vec<(Option<Scope>, KernelMessage)>>,
    }

    #[async_trait::async_trait]
    impl LocalDelivery for Recorder {
        async fn deliver(
            &self,
            _from: &PeerCtx,
            scope: Option<&Scope>,
            msg: KernelMessage,
        ) -> KernelResult<()> {
            self.got.lock().unwrap().push((scope.cloned(), msg));
            Ok(())
        }
    }

    #[tokio::test]
    async fn serve_listener_delivers_scoped_envelope_to_local_delivery() {
        let rec = Arc::new(Recorder::default());
        let mut rt = MeshRuntime::new("node-b".into());
        rt.set_local_delivery(rec.clone());
        let rt = Arc::new(rt);

        let transport = crate::mesh_tcp::TcpTransport;
        let listener = transport.listen("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(serve_listener(
            Arc::clone(&rt),
            listener,
            None,
            "tcp",
            "127.0.0.1:0",
            Arc::new(crate::mesh_admit::AllowAll),
        ));

        let mut client = transport.connect(&addr.to_string()).await.unwrap();
        let msg = KernelMessage::text(0, MessageTarget::Topic("t.scoped".into()), "hi");
        let mut env = MeshIpcEnvelope::new("node-a".into(), "node-b".into(), msg);
        env.dest_scope = Some(Scope {
            user_id: "a".repeat(32),
            project_id: None,
        });
        client.send(&env.to_bytes().unwrap()).await.unwrap();

        for _ in 0..100 {
            if !rec.got.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        server.abort();

        let got = rec.got.lock().unwrap();
        assert_eq!(got.len(), 1, "envelope not delivered");
        assert_eq!(got[0].0.as_ref().unwrap().user_id, "a".repeat(32));
        assert!(matches!(&got[0].1.payload, MessagePayload::Text(s) if s == "hi"));
        // The accept loop registered the sender by source_node.
        assert!(rt.peer_ids().contains(&"node-a".to_string()));
    }
}

#[cfg(test)]
#[path = "mesh_serve_tests.rs"]
mod serve_tests;

#[cfg(test)]
#[path = "mesh_nested_dial_tests.rs"]
mod nested_dial_tests;

#[cfg(test)]
#[path = "mesh_serve_stall_tests.rs"]
mod stall_tests;
