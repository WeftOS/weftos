//! Agent-to-agent IPC protocol.
//!
//! The [`A2ARouter`] provides direct PID-to-PID messaging with
//! capability-checked routing, per-agent inboxes, and request-response
//! patterns with timeout support. It integrates with the
//! [`TopicRouter`] for pub/sub delivery.
//!
//! # Message Flow
//!
//! ```text
//! Agent A (PID 1)       A2ARouter          Agent B (PID 7)
//!      |                    |                    |
//!      |-- send(msg) ------>|                    |
//!      |                    |-- check_scope ---->|
//!      |                    |<-- Ok -------------|
//!      |                    |-- inbox.send(7) -->|
//!      |                    |                    |-- recv msg
//! ```

use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, warn};

use crate::capability::CapabilityChecker;
use crate::error::{KernelError, KernelResult};
use crate::ipc::{KernelMessage, MessageTarget};
use crate::process::{Pid, ProcessState, ProcessTable};
use crate::service::ServiceRegistry;
use crate::topic::{SubscriberSink, TopicRouter};

#[cfg(feature = "exochain")]
use crate::chain::ChainManager;

#[cfg(feature = "mesh")]
use crate::mesh_ipc::MeshIpcEnvelope;
#[cfg(feature = "mesh")]
use crate::mesh_runtime::MeshRuntime;

/// Default inbox channel capacity per agent.
const DEFAULT_INBOX_CAPACITY: usize = 1024;

/// Maximum serialized message size (16 MiB) -- prevents DoS via oversized payloads.
///
/// This mirrors [`crate::mesh::MAX_MESSAGE_SIZE`]. Enforcement happens at the
/// mesh boundary in `mesh_framing.rs` where raw bytes are received from remote
/// nodes. Within a single kernel, messages travel as typed `KernelMessage`
/// structs over `mpsc` channels, so the size limit is not checked on the
/// in-process hot path.
#[allow(dead_code)]
const MAX_A2A_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

/// A pending request awaiting a correlated response.
struct PendingRequest {
    /// Sender to deliver the response on.
    response_tx: oneshot::Sender<KernelMessage>,
    /// When the request was sent (for timeout tracking / diagnostics).
    #[allow(dead_code)]
    sent_at: Instant,
}

/// Carries messages for remote nodes when the mesh runs outside this kernel
/// (service mode, ADR-103 P3-U): the router hands every
/// [`MessageTarget::RemoteNode`] message to it instead of a [`MeshRuntime`].
#[cfg(feature = "mesh")]
#[async_trait::async_trait]
pub trait RemoteForwarder: Send + Sync + 'static {
    /// Forward `msg` to `node_id`.
    async fn forward(&self, node_id: &str, msg: KernelMessage) -> KernelResult<()>;
}

/// Agent-to-agent message router.
///
/// Manages per-agent inboxes (bounded `mpsc` channels), validates
/// IPC scope through the capability checker, and routes messages
/// to their targets (direct PID, topic, broadcast, service).
pub struct A2ARouter {
    /// Process table for state validation.
    process_table: Arc<ProcessTable>,

    /// Capability checker for IPC scope enforcement.
    capability_checker: Arc<CapabilityChecker>,

    /// Topic router for pub/sub delivery.
    topic_router: Arc<TopicRouter>,

    /// Service registry for service-based routing (D1, D19, K2.1).
    service_registry: Option<Arc<ServiceRegistry>>,

    /// Per-agent inboxes: PID -> sender half of inbox channel.
    inboxes: DashMap<Pid, mpsc::Sender<KernelMessage>>,

    /// Pending request-response tracking: request_id -> PendingRequest.
    pending_requests: DashMap<String, PendingRequest>,

    /// Optional routing-time gate backend (C4 dual-layer governance).
    ///
    /// Uses `OnceLock` to support post-construction wiring from boot
    /// (the governance gate is created after the A2ARouter is wrapped
    /// in `Arc`). `with_gate()` and `set_gate()` both target this field.
    #[cfg(feature = "exochain")]
    gate: std::sync::OnceLock<Arc<dyn crate::gate::GateBackend>>,

    /// Dead letter queue for undeliverable messages (os-patterns).
    #[cfg(feature = "os-patterns")]
    dead_letter_queue: std::sync::OnceLock<Arc<crate::dead_letter::DeadLetterQueue>>,

    /// Optional mesh runtime for cross-node message delivery (K6).
    #[cfg(feature = "mesh")]
    mesh_runtime: std::sync::OnceLock<Arc<MeshRuntime>>,

    /// Remote-node forwarder used instead of `mesh_runtime` in service mode.
    #[cfg(feature = "mesh")]
    remote_forwarder: std::sync::OnceLock<Arc<dyn RemoteForwarder>>,
}

impl A2ARouter {
    /// Create a new A2A router.
    pub fn new(
        process_table: Arc<ProcessTable>,
        capability_checker: Arc<CapabilityChecker>,
        topic_router: Arc<TopicRouter>,
    ) -> Self {
        Self {
            process_table,
            capability_checker,
            topic_router,
            service_registry: None,
            inboxes: DashMap::new(),
            pending_requests: DashMap::new(),
            #[cfg(feature = "exochain")]
            gate: std::sync::OnceLock::new(),
            #[cfg(feature = "os-patterns")]
            dead_letter_queue: std::sync::OnceLock::new(),
            #[cfg(feature = "mesh")]
            mesh_runtime: std::sync::OnceLock::new(),
            #[cfg(feature = "mesh")]
            remote_forwarder: std::sync::OnceLock::new(),
        }
    }

    /// Attach a service registry for service-based routing (D1, D19).
    pub fn with_service_registry(mut self, registry: Arc<ServiceRegistry>) -> Self {
        self.service_registry = Some(registry);
        self
    }

    /// Attach a routing-time gate for dual-layer governance (C4).
    ///
    /// When set, every message routed through `send()` is checked against
    /// the gate *before* inbox delivery. A `Deny` decision blocks the
    /// message; `Defer` still delivers (the handler-time gate decides).
    #[cfg(feature = "exochain")]
    pub fn with_gate(self, gate: Arc<dyn crate::gate::GateBackend>) -> Self {
        let _ = self.gate.set(gate);
        self
    }

    /// Set the routing-time gate after construction (for boot wiring).
    ///
    /// This allows the governance gate to be attached after the router
    /// is already wrapped in an `Arc`, since the `OnceLock` provides
    /// interior mutability for the first (and only) write.
    #[cfg(feature = "exochain")]
    pub fn set_gate(&self, gate: Arc<dyn crate::gate::GateBackend>) {
        let _ = self.gate.set(gate);
    }

    /// Set the dead letter queue after construction (for boot wiring).
    ///
    /// Like `set_gate()`, uses `OnceLock` for interior mutability so
    /// the DLQ can be attached after the router is wrapped in `Arc`.
    #[cfg(feature = "os-patterns")]
    pub fn set_dead_letter_queue(&self, dlq: Arc<crate::dead_letter::DeadLetterQueue>) {
        let _ = self.dead_letter_queue.set(dlq);
    }

    /// Get the dead letter queue (if configured).
    #[cfg(feature = "os-patterns")]
    pub fn dead_letter_queue(&self) -> Option<&Arc<crate::dead_letter::DeadLetterQueue>> {
        self.dead_letter_queue.get()
    }

    /// Attach the mesh runtime for cross-node message delivery (K6).
    ///
    /// Uses `OnceLock` so the runtime can be attached after the router
    /// is already wrapped in `Arc`.
    #[cfg(feature = "mesh")]
    pub fn set_mesh_runtime(&self, runtime: Arc<MeshRuntime>) {
        let _ = self.mesh_runtime.set(runtime);
    }

    /// Attach the forwarder that carries remote-node messages when the mesh
    /// is owned by the machine mesh service (P3-U). Ignored once set.
    #[cfg(feature = "mesh")]
    pub fn set_remote_forwarder(&self, forwarder: Arc<dyn RemoteForwarder>) {
        let _ = self.remote_forwarder.set(forwarder);
    }

    /// Get the mesh runtime (if configured).
    #[cfg(feature = "mesh")]
    pub fn mesh_runtime(&self) -> Option<&Arc<MeshRuntime>> {
        self.mesh_runtime.get()
    }

    /// Get the service registry (if configured).
    pub fn service_registry(&self) -> Option<&Arc<ServiceRegistry>> {
        self.service_registry.as_ref()
    }

    /// Create an inbox for a process.
    ///
    /// Returns the receiver half that the agent should poll for
    /// incoming messages. The sender half is stored internally
    /// for routing.
    ///
    /// If an inbox already exists for this PID, the old one is
    /// replaced (existing messages are lost).
    pub fn create_inbox(&self, pid: Pid) -> mpsc::Receiver<KernelMessage> {
        let (tx, rx) = mpsc::channel(DEFAULT_INBOX_CAPACITY);
        self.inboxes.insert(pid, tx);
        debug!(pid, "created inbox");
        rx
    }

    /// Remove an inbox (used during process cleanup).
    pub fn remove_inbox(&self, pid: Pid) {
        self.inboxes.remove(&pid);
        debug!(pid, "removed inbox");
    }

    /// Send a message, routing it to the appropriate target.
    ///
    /// Validates that the sender exists and is running, checks
    /// IPC scope via the capability checker, then delivers the
    /// message to the target.
    ///
    /// # Routing
    ///
    /// - `Process(pid)`: delivers directly to the target's inbox
    /// - `Topic(name)`: publishes to all topic subscribers
    /// - `Broadcast`: delivers to all inboxes except the sender
    /// - `Service(name)`: logs a warning (service routing is a
    ///   future extension)
    /// - `Kernel`: logs a warning (kernel messages are internal)
    ///
    /// # Errors
    ///
    /// Returns `KernelError::ProcessNotFound` if the sender PID
    /// is not in the process table, or `KernelError::CapabilityDenied`
    /// if the sender's IPC scope does not permit the target.
    pub async fn send(&self, msg: KernelMessage) -> KernelResult<()> {
        let from = msg.from;

        // Validate sender exists and is running
        let sender = self
            .process_table
            .get(from)
            .ok_or(KernelError::ProcessNotFound { pid: from })?;

        if !matches!(
            sender.state,
            ProcessState::Running | ProcessState::Suspended
        ) {
            return Err(KernelError::Ipc(format!(
                "sender PID {from} is not running (state: {})",
                sender.state
            )));
        }

        // C4: Routing-time gate check (first layer of dual-layer governance).
        // A Deny blocks the message before it reaches any inbox. Defer
        // still delivers — the handler-time gate makes the final call.
        #[cfg(feature = "exochain")]
        if let Some(gate) = self.gate.get() {
            let action = match &msg.payload {
                crate::ipc::MessagePayload::ToolCall { name, .. } => format!("tool.{name}"),
                crate::ipc::MessagePayload::Signal(_) => "ipc.signal".to_string(),
                _ => "ipc.send".to_string(),
            };
            use crate::governance::{GateEffectKind, with_effect_context};
            let mut base = serde_json::json!({
                "from": from,
                "target": format!("{:?}", msg.target),
                "layer": "routing",
            });
            // The kernel's own topic publishes (daemon leaf pushes) get one
            // quota bucket per topic instead of sharing principal 0's.
            if from == 0
                && let MessageTarget::Topic(t) = &msg.target
            {
                base["quota_key"] = serde_json::Value::String(format!("0:{t}"));
            }
            let context = with_effect_context(
                base,
                &GateEffectKind::A2aRouting.effect_vector(),
            );
            match gate.check(&from.to_string(), &action, &context) {
                crate::gate::GateDecision::Deny { reason, .. } => {
                    return Err(KernelError::CapabilityDenied {
                        pid: from,
                        action,
                        reason: format!("routing gate denied: {reason}"),
                    });
                }
                crate::gate::GateDecision::Defer { .. }
                | crate::gate::GateDecision::Permit { .. } => {
                    // Permitted or deferred — continue to delivery.
                }
            }
        }

        // Route based on target
        match &msg.target {
            MessageTarget::Process(target_pid) => {
                // Check IPC scope
                self.capability_checker
                    .check_ipc_target(from, *target_pid)?;

                self.deliver_to_inbox(*target_pid, msg).await
            }
            MessageTarget::Topic(topic) => {
                // WEFT-150: enforce IpcScope topic grants on publish
                // (mirrors check_ipc_target for Process). Kernel PID 0
                // defaults to IpcScope::All; Restricted / Topic scopes
                // and browser Restricted are blocked from non-public
                // topics (mesh.leaf.* is not public).
                self.capability_checker.check_ipc_topic(from, topic)?;

                let sinks = self.topic_router.live_sinks(topic);
                let mut delivered = 0u32;

                // Serialize the message once for external streaming
                // subscribers; only build this if at least one
                // external sink exists so the fast path is unaffected.
                let mut external_line: Option<Vec<u8>> = None;
                if sinks
                    .iter()
                    .any(|(_, s)| matches!(s, SubscriberSink::ExternalStream(_)))
                    && let Ok(mut bytes) = serde_json::to_vec(&msg)
                {
                    bytes.push(b'\n');
                    external_line = Some(bytes);
                }

                for (_id, sink) in &sinks {
                    match sink {
                        SubscriberSink::PidInbox(sub_pid) => {
                            if *sub_pid != from {
                                let msg_clone = msg.clone();
                                if self.deliver_to_inbox(*sub_pid, msg_clone).await.is_ok() {
                                    delivered += 1;
                                }
                            }
                        }
                        SubscriberSink::ExternalStream(tx) => {
                            if let Some(line) = external_line.as_ref() {
                                // Best-effort delivery; a full or closed
                                // channel drops the message for that
                                // client (the daemon will prune on the
                                // next publish via live_sinks).
                                match tx.try_send(line.clone()) {
                                    Ok(()) => {
                                        delivered += 1;
                                    }
                                    Err(e) => {
                                        warn!(topic, error = %e, "external stream drop");
                                    }
                                }
                            }
                        }
                    }
                }

                // Forward to mesh peers that registered a subscription for
                // this topic via a `mesh.subscribe` control envelope.
                #[cfg(feature = "mesh")]
                if let Some(runtime) = self.mesh_runtime.get() {
                    let peer_ids = runtime.peers_for_topic(topic);
                    let mut mesh_delivered = 0u32;
                    for peer_id in &peer_ids {
                        let envelope = MeshIpcEnvelope::new(
                            runtime.node_id().to_string(),
                            peer_id.clone(),
                            msg.clone(),
                        );
                        match runtime.send_to_peer(peer_id, envelope).await {
                            Ok(()) => mesh_delivered += 1,
                            Err(e) => {
                                warn!(
                                    topic,
                                    peer = %peer_id,
                                    error = %e,
                                    "failed to forward topic to mesh peer"
                                );
                            }
                        }
                    }
                    if mesh_delivered > 0 {
                        debug!(from, topic, mesh_delivered, "forwarded topic to mesh peers");
                    }
                }

                debug!(from, topic, delivered, "published to topic");
                Ok(())
            }
            MessageTarget::Broadcast => {
                let mut delivered = 0u32;
                let pids: Vec<Pid> = self.inboxes.iter().map(|entry| *entry.key()).collect();

                for pid in pids {
                    if pid != from {
                        // Check IPC scope for each target
                        if self.capability_checker.check_ipc_target(from, pid).is_ok() {
                            let msg_clone = msg.clone();
                            if self.deliver_to_inbox(pid, msg_clone).await.is_ok() {
                                delivered += 1;
                            }
                        }
                    }
                }
                debug!(from, delivered, "broadcast sent");
                Ok(())
            }
            MessageTarget::Service(name) => {
                let name = name.clone();
                self.route_to_service(from, &name, msg).await
            }
            MessageTarget::ServiceMethod { service, .. } => {
                let service_name = service.clone();
                self.route_to_service(from, &service_name, msg).await
            }
            MessageTarget::Kernel => {
                debug!(from, "kernel message routing not yet implemented");
                Ok(())
            }
            MessageTarget::RemoteNode { node_id, .. } => {
                #[cfg(feature = "mesh")]
                {
                    if let Some(forwarder) = self.remote_forwarder.get() {
                        let node_id = node_id.clone();
                        debug!(from, %node_id, "routing message to remote node via mesh service");
                        return forwarder.forward(&node_id, msg).await;
                    }
                    if let Some(runtime) = self.mesh_runtime.get() {
                        let node_id = node_id.clone();
                        debug!(from, %node_id, "routing message to remote node via mesh");
                        let envelope = MeshIpcEnvelope::new(
                            runtime.node_id().to_string(),
                            node_id.clone(),
                            msg,
                        );
                        runtime.send_to_peer(&node_id, envelope).await.map_err(|e| {
                            KernelError::Mesh(format!(
                                "failed to send to remote node '{node_id}': {e}"
                            ))
                        })
                    } else {
                        debug!(from, %node_id, "remote node routing: no mesh runtime attached");
                        Err(KernelError::Mesh(format!(
                            "remote routing to node '{node_id}' not yet implemented"
                        )))
                    }
                }
                #[cfg(not(feature = "mesh"))]
                {
                    debug!(from, %node_id, "remote node routing not available (mesh feature disabled)");
                    Err(KernelError::Mesh(format!(
                        "remote routing to node '{node_id}' not yet implemented"
                    )))
                }
            }
        }
    }

    /// Deliver a message to a specific PID's inbox.
    ///
    /// If the inbox does not exist or is full, the message is routed to
    /// the dead letter queue (when os-patterns is enabled) and an error
    /// is returned.
    async fn deliver_to_inbox(&self, pid: Pid, msg: KernelMessage) -> KernelResult<()> {
        // Clone the sender so we release the DashMap read lock before
        // any potential remove() call (which needs a write lock on the
        // same shard — holding both would deadlock).
        let tx = match self.inboxes.get(&pid) {
            Some(tx) => tx.clone(),
            None => {
                warn!(pid, "no inbox for PID, dead-lettering");
                #[cfg(feature = "os-patterns")]
                if let Some(dlq) = self.dead_letter_queue.get() {
                    dlq.intake(
                        msg,
                        crate::dead_letter::DeadLetterReason::TargetNotFound { pid },
                    );
                }
                return Err(KernelError::Ipc(format!("no inbox for PID {pid}")));
            }
        };

        match tx.try_send(msg) {
            Ok(()) => {
                debug!(pid, "message delivered to inbox");
                Ok(())
            }
            Err(mpsc::error::TrySendError::Full(msg)) => {
                let _rejected_msg = msg;
                warn!(pid, "inbox full, dead-lettering");
                #[cfg(feature = "os-patterns")]
                if let Some(dlq) = self.dead_letter_queue.get() {
                    dlq.intake(
                        rejected_msg,
                        crate::dead_letter::DeadLetterReason::InboxFull { pid },
                    );
                }
                Err(KernelError::Ipc(format!("inbox full for PID {pid}")))
            }
            Err(mpsc::error::TrySendError::Closed(msg)) => {
                let _rejected_msg = msg;
                warn!(pid, "inbox closed, removing and dead-lettering");
                self.inboxes.remove(&pid);
                #[cfg(feature = "os-patterns")]
                if let Some(dlq) = self.dead_letter_queue.get() {
                    dlq.intake(
                        rejected_msg,
                        crate::dead_letter::DeadLetterReason::AgentExited { pid },
                    );
                }
                Err(KernelError::Ipc(format!("inbox closed for PID {pid}")))
            }
        }
    }

    /// Route a message to a named service via the ServiceRegistry.
    ///
    /// Resolves the service name to an owning agent PID, then delivers
    /// the message to that agent's inbox.
    async fn route_to_service(
        &self,
        from: Pid,
        service_name: &str,
        msg: KernelMessage,
    ) -> KernelResult<()> {
        let registry = self.service_registry.as_ref().ok_or_else(|| {
            KernelError::Ipc(format!(
                "no service registry configured; cannot route to service '{service_name}'"
            ))
        })?;

        let target_pid = registry
            .resolve_target(service_name)
            .ok_or_else(|| KernelError::Ipc(format!("service not found: '{service_name}'")))?;

        // Check IPC scope
        self.capability_checker.check_ipc_target(from, target_pid)?;

        self.deliver_to_inbox(target_pid, msg).await
    }

    /// Send a message with chain-event logging.
    ///
    /// This mirrors `KernelIpc::send_checked` but for the A2ARouter:
    /// every routed message is logged as an `ipc.send` chain event with
    /// sender, target, payload type, and message ID — forming a
    /// tamper-evident IPC audit trail in the exochain.
    ///
    /// When the `exochain` feature is disabled this is equivalent to
    /// a plain `send()`.
    #[cfg(feature = "exochain")]
    pub async fn send_checked(
        &self,
        msg: KernelMessage,
        chain: Option<&ChainManager>,
    ) -> KernelResult<()> {
        // Log the IPC event before delivery so the chain records intent
        // even if the inbox is full or closed.
        if let Some(cm) = chain {
            cm.append(
                "ipc",
                "ipc.send",
                Some(serde_json::json!({
                    "from": msg.from,
                    "target": format!("{:?}", msg.target),
                    "payload_type": msg.payload.type_name(),
                    "msg_id": msg.id,
                })),
            );
        }
        self.send(msg).await
    }

    /// Get the topic router.
    pub fn topic_router(&self) -> &Arc<TopicRouter> {
        &self.topic_router
    }

    /// Get the number of active inboxes.
    pub fn inbox_count(&self) -> usize {
        self.inboxes.len()
    }

    /// Check whether a PID has an inbox.
    pub fn has_inbox(&self, pid: Pid) -> bool {
        self.inboxes.contains_key(&pid)
    }

    /// Send a request and wait for a correlated response with timeout.
    ///
    /// The request message is sent normally, but its `id` is registered
    /// as a pending request. When a response arrives with a matching
    /// `correlation_id`, it is delivered to the returned future instead
    /// of the sender's inbox.
    ///
    /// # Errors
    ///
    /// Returns `KernelError::Timeout` if no response arrives within the
    /// specified duration, or `KernelError::Ipc` if the response channel
    /// is closed before a response arrives.
    pub async fn request(
        &self,
        msg: KernelMessage,
        timeout: Duration,
    ) -> KernelResult<KernelMessage> {
        let request_id = msg.id.clone();
        let (tx, rx) = oneshot::channel();

        // Register pending request before sending so there is no race.
        self.pending_requests.insert(
            request_id.clone(),
            PendingRequest {
                response_tx: tx,
                sent_at: Instant::now(),
            },
        );

        // Send the message; clean up on failure.
        if let Err(e) = self.send(msg).await {
            self.pending_requests.remove(&request_id);
            return Err(e);
        }

        // Wait for response with timeout.
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(_)) => {
                self.pending_requests.remove(&request_id);
                Err(KernelError::Ipc("response channel closed".into()))
            }
            Err(_) => {
                self.pending_requests.remove(&request_id);
                Err(KernelError::Timeout {
                    operation: format!("request {request_id}"),
                    duration_ms: timeout.as_millis() as u64,
                })
            }
        }
    }

    /// Try to complete a pending request with a correlated response.
    ///
    /// If the message has a `correlation_id` that matches a pending
    /// request, the response is delivered to the waiting future and
    /// `true` is returned. Otherwise returns `false`.
    pub fn try_complete_request(&self, msg: KernelMessage) -> bool {
        if let Some(ref corr_id) = msg.correlation_id
            && let Some((_, pending)) = self.pending_requests.remove(corr_id)
        {
            let _ = pending.response_tx.send(msg);
            return true;
        }
        false
    }

    /// Get the number of pending requests.
    pub fn pending_request_count(&self) -> usize {
        self.pending_requests.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::AgentCapabilities;
    use crate::ipc::MessagePayload;
    use crate::process::{ProcessEntry, ResourceUsage};
    use tokio_util::sync::CancellationToken;

    fn setup_router(
        agent_count: usize,
    ) -> (A2ARouter, Vec<Pid>, Vec<mpsc::Receiver<KernelMessage>>) {
        let table = Arc::new(ProcessTable::new(64));
        let mut pids = Vec::new();

        for i in 0..agent_count {
            let entry = ProcessEntry {
                pid: 0,
                agent_id: format!("agent-{i}"),
                state: ProcessState::Running,
                capabilities: AgentCapabilities::default(),
                resource_usage: ResourceUsage::default(),
                cancel_token: CancellationToken::new(),
                parent_pid: None,
            };
            let pid = table.insert(entry).unwrap();
            pids.push(pid);
        }

        let checker = Arc::new(CapabilityChecker::new(table.clone()));
        let topic_router = Arc::new(TopicRouter::new(table.clone()));
        let router = A2ARouter::new(table, checker, topic_router);

        let mut receivers = Vec::new();
        for &pid in &pids {
            let rx = router.create_inbox(pid);
            receivers.push(rx);
        }

        (router, pids, receivers)
    }

    #[tokio::test]
    async fn direct_message_delivery() {
        let (router, pids, mut receivers) = setup_router(2);

        let msg = KernelMessage::text(pids[0], MessageTarget::Process(pids[1]), "hello");
        router.send(msg).await.unwrap();

        let received = receivers[1].try_recv().unwrap();
        assert_eq!(received.from, pids[0]);
        assert!(matches!(
            received.payload,
            MessagePayload::Text(ref t) if t == "hello"
        ));
    }

    #[tokio::test]
    async fn message_to_self_works() {
        let (router, pids, mut receivers) = setup_router(1);

        let msg = KernelMessage::text(pids[0], MessageTarget::Process(pids[0]), "self-msg");
        router.send(msg).await.unwrap();

        let received = receivers[0].try_recv().unwrap();
        assert!(matches!(
            received.payload,
            MessagePayload::Text(ref t) if t == "self-msg"
        ));
    }

    #[tokio::test]
    async fn broadcast_delivers_to_all_except_sender() {
        let (router, pids, mut receivers) = setup_router(3);

        let msg = KernelMessage::text(pids[0], MessageTarget::Broadcast, "broadcast");
        router.send(msg).await.unwrap();

        // Sender should not receive
        assert!(receivers[0].try_recv().is_err());

        // Others should receive
        let r1 = receivers[1].try_recv().unwrap();
        assert!(matches!(
            r1.payload,
            MessagePayload::Text(ref t) if t == "broadcast"
        ));
        let r2 = receivers[2].try_recv().unwrap();
        assert!(matches!(
            r2.payload,
            MessagePayload::Text(ref t) if t == "broadcast"
        ));
    }

    #[tokio::test]
    async fn topic_publish_delivers_to_subscribers() {
        let (router, pids, mut receivers) = setup_router(3);

        // Subscribe pids[1] and pids[2] to "build"
        router.topic_router().subscribe(pids[1], "build");
        router.topic_router().subscribe(pids[2], "build");

        let msg = KernelMessage::text(pids[0], MessageTarget::Topic("build".into()), "build done");
        router.send(msg).await.unwrap();

        // Sender not subscribed, should not receive
        assert!(receivers[0].try_recv().is_err());

        // Subscribers should receive
        assert!(receivers[1].try_recv().is_ok());
        assert!(receivers[2].try_recv().is_ok());
    }

    #[tokio::test]
    async fn topic_publish_delivers_to_external_stream() {
        let (router, pids, _receivers) = setup_router(1);

        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(8);
        let _id = router
            .topic_router()
            .subscribe_sink("events", SubscriberSink::ExternalStream(tx));

        let msg = KernelMessage::text(pids[0], MessageTarget::Topic("events".into()), "hi");
        router.send(msg).await.unwrap();

        // External stream receives a JSON line terminated by '\n'.
        let line = rx
            .try_recv()
            .expect("external stream should receive message");
        assert!(line.ends_with(b"\n"));
        let trimmed = std::str::from_utf8(&line[..line.len() - 1]).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(trimmed).unwrap();
        assert_eq!(parsed["from"], pids[0]);
    }

    #[tokio::test]
    async fn topic_publish_fans_out_to_pid_and_external() {
        let (router, pids, mut receivers) = setup_router(2);
        router.topic_router().subscribe(pids[1], "events");

        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(8);
        router
            .topic_router()
            .subscribe_sink("events", SubscriberSink::ExternalStream(tx));

        let msg = KernelMessage::text(pids[0], MessageTarget::Topic("events".into()), "both");
        router.send(msg).await.unwrap();

        assert!(receivers[1].try_recv().is_ok(), "pid subscriber missed");
        assert!(rx.try_recv().is_ok(), "external subscriber missed");
    }

    #[tokio::test]
    async fn topic_publish_excludes_sender_if_subscribed() {
        let (router, pids, mut receivers) = setup_router(2);

        // Both subscribe
        router.topic_router().subscribe(pids[0], "build");
        router.topic_router().subscribe(pids[1], "build");

        let msg = KernelMessage::text(pids[0], MessageTarget::Topic("build".into()), "done");
        router.send(msg).await.unwrap();

        // Sender should not receive their own publish
        assert!(receivers[0].try_recv().is_err());
        // Other subscriber should receive
        assert!(receivers[1].try_recv().is_ok());
    }

    #[tokio::test]
    async fn send_from_nonexistent_pid_fails() {
        let (router, _pids, _receivers) = setup_router(1);

        let msg = KernelMessage::text(999, MessageTarget::Process(1), "hello");
        let result = router.send(msg).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn send_to_pid_without_inbox_fails() {
        let table = Arc::new(ProcessTable::new(64));

        // Create sender (running)
        let sender_entry = ProcessEntry {
            pid: 0,
            agent_id: "sender".to_owned(),
            state: ProcessState::Running,
            capabilities: AgentCapabilities::default(),
            resource_usage: ResourceUsage::default(),
            cancel_token: CancellationToken::new(),
            parent_pid: None,
        };
        let sender_pid = table.insert(sender_entry).unwrap();

        // Create target (running) but don't create inbox
        let target_entry = ProcessEntry {
            pid: 0,
            agent_id: "target".to_owned(),
            state: ProcessState::Running,
            capabilities: AgentCapabilities::default(),
            resource_usage: ResourceUsage::default(),
            cancel_token: CancellationToken::new(),
            parent_pid: None,
        };
        let target_pid = table.insert(target_entry).unwrap();

        let checker = Arc::new(CapabilityChecker::new(table.clone()));
        let topic_router = Arc::new(TopicRouter::new(table.clone()));
        let router = A2ARouter::new(table, checker, topic_router);

        // Create inbox only for sender
        let _rx = router.create_inbox(sender_pid);

        let msg = KernelMessage::text(sender_pid, MessageTarget::Process(target_pid), "hello");
        let result = router.send(msg).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn ipc_scope_restricts_messaging() {
        let table = Arc::new(ProcessTable::new(64));

        // Create sender with restricted IPC scope
        use crate::capability::IpcScope;
        let sender_entry = ProcessEntry {
            pid: 0,
            agent_id: "restricted".to_owned(),
            state: ProcessState::Running,
            capabilities: AgentCapabilities {
                ipc_scope: IpcScope::Restricted(vec![]), // No allowed PIDs
                ..Default::default()
            },
            resource_usage: ResourceUsage::default(),
            cancel_token: CancellationToken::new(),
            parent_pid: None,
        };
        let sender_pid = table.insert(sender_entry).unwrap();

        // Create target
        let target_entry = ProcessEntry {
            pid: 0,
            agent_id: "target".to_owned(),
            state: ProcessState::Running,
            capabilities: AgentCapabilities::default(),
            resource_usage: ResourceUsage::default(),
            cancel_token: CancellationToken::new(),
            parent_pid: None,
        };
        let target_pid = table.insert(target_entry).unwrap();

        let checker = Arc::new(CapabilityChecker::new(table.clone()));
        let topic_router = Arc::new(TopicRouter::new(table.clone()));
        let router = A2ARouter::new(table, checker, topic_router);
        let _rx1 = router.create_inbox(sender_pid);
        let _rx2 = router.create_inbox(target_pid);

        let msg = KernelMessage::text(sender_pid, MessageTarget::Process(target_pid), "blocked");
        let result = router.send(msg).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn ipc_scope_none_blocks_all() {
        let table = Arc::new(ProcessTable::new(64));

        use crate::capability::IpcScope;
        let sender_entry = ProcessEntry {
            pid: 0,
            agent_id: "no-ipc".to_owned(),
            state: ProcessState::Running,
            capabilities: AgentCapabilities {
                can_ipc: false,
                ipc_scope: IpcScope::None,
                ..Default::default()
            },
            resource_usage: ResourceUsage::default(),
            cancel_token: CancellationToken::new(),
            parent_pid: None,
        };
        let sender_pid = table.insert(sender_entry).unwrap();

        let target_entry = ProcessEntry {
            pid: 0,
            agent_id: "target".to_owned(),
            state: ProcessState::Running,
            capabilities: AgentCapabilities::default(),
            resource_usage: ResourceUsage::default(),
            cancel_token: CancellationToken::new(),
            parent_pid: None,
        };
        let target_pid = table.insert(target_entry).unwrap();

        let checker = Arc::new(CapabilityChecker::new(table.clone()));
        let topic_router = Arc::new(TopicRouter::new(table.clone()));
        let router = A2ARouter::new(table, checker, topic_router);
        let _rx1 = router.create_inbox(sender_pid);
        let _rx2 = router.create_inbox(target_pid);

        let msg = KernelMessage::text(sender_pid, MessageTarget::Process(target_pid), "blocked");
        let result = router.send(msg).await;
        assert!(result.is_err());
    }

    #[test]
    fn create_and_remove_inbox() {
        let table = Arc::new(ProcessTable::new(64));
        let checker = Arc::new(CapabilityChecker::new(table.clone()));
        let topic_router = Arc::new(TopicRouter::new(table.clone()));
        let router = A2ARouter::new(table, checker, topic_router);

        let _rx = router.create_inbox(42);
        assert!(router.has_inbox(42));
        assert_eq!(router.inbox_count(), 1);

        router.remove_inbox(42);
        assert!(!router.has_inbox(42));
        assert_eq!(router.inbox_count(), 0);
    }

    #[tokio::test]
    async fn tool_call_message_routes() {
        let (router, pids, mut receivers) = setup_router(2);

        let msg = KernelMessage::tool_call(
            pids[0],
            MessageTarget::Process(pids[1]),
            "read_file",
            serde_json::json!({"path": "/test"}),
        );
        router.send(msg).await.unwrap();

        let received = receivers[1].try_recv().unwrap();
        assert!(matches!(
            received.payload,
            MessagePayload::ToolCall { ref name, .. } if name == "read_file"
        ));
    }

    #[tokio::test]
    async fn tool_result_message_routes() {
        let (router, pids, mut receivers) = setup_router(2);

        let msg = KernelMessage::tool_result(
            pids[1],
            MessageTarget::Process(pids[0]),
            "call-1",
            serde_json::json!({"content": "data"}),
        );
        router.send(msg).await.unwrap();

        let received = receivers[0].try_recv().unwrap();
        assert!(matches!(
            received.payload,
            MessagePayload::ToolResult { ref call_id, .. } if call_id == "call-1"
        ));
    }

    #[cfg(feature = "exochain")]
    #[tokio::test]
    async fn send_checked_logs_chain_event() {
        let (router, pids, mut receivers) = setup_router(2);

        let chain = crate::chain::ChainManager::new(0, 1000);
        let initial_seq = chain.sequence();

        let msg = KernelMessage::text(pids[0], MessageTarget::Process(pids[1]), "audited");
        router.send_checked(msg, Some(&chain)).await.unwrap();

        // Message should still be delivered
        let received = receivers[1].try_recv().unwrap();
        assert!(matches!(
            received.payload,
            MessagePayload::Text(ref t) if t == "audited"
        ));

        // Chain should have a new ipc.send event
        assert_eq!(chain.sequence(), initial_seq + 1);
        let events = chain.tail(1);
        assert_eq!(events[0].kind, "ipc.send");
        assert_eq!(events[0].source, "ipc");
        let payload = events[0].payload.as_ref().unwrap();
        assert_eq!(payload["from"], pids[0]);
        assert_eq!(payload["payload_type"], "text");
    }

    #[cfg(feature = "exochain")]
    #[tokio::test]
    async fn send_checked_without_chain_still_delivers() {
        let (router, pids, mut receivers) = setup_router(2);

        let msg = KernelMessage::text(pids[0], MessageTarget::Process(pids[1]), "no-chain");
        router.send_checked(msg, None).await.unwrap();

        let received = receivers[1].try_recv().unwrap();
        assert!(matches!(
            received.payload,
            MessagePayload::Text(ref t) if t == "no-chain"
        ));
    }

    /// WEFT-150: Restricted IpcScope cannot publish to leaf-push topics.
    #[tokio::test]
    async fn restricted_scope_cannot_publish_leaf_push_topic() {
        use crate::capability::IpcScope;
        use crate::process::ProcessTable;

        let table = Arc::new(ProcessTable::new(16));
        let entry = ProcessEntry {
            pid: 0,
            agent_id: "browser-like".into(),
            state: ProcessState::Running,
            capabilities: AgentCapabilities {
                ipc_scope: IpcScope::Restricted(vec![]),
                ..AgentCapabilities::default()
            },
            resource_usage: ResourceUsage::default(),
            cancel_token: CancellationToken::new(),
            parent_pid: None,
        };
        let pid = table.insert(entry).unwrap();
        let checker = Arc::new(CapabilityChecker::new(table.clone()));
        let topic_router = Arc::new(TopicRouter::new(table.clone()));
        let router = A2ARouter::new(table, checker, topic_router);
        router.create_inbox(pid);

        let leaf_topic = "mesh.leaf.deadbeef.push";
        let msg = KernelMessage::text(pid, MessageTarget::Topic(leaf_topic.into()), "leaf");
        let err = router
            .send(msg)
            .await
            .expect_err("Restricted must not publish mesh.leaf.*.push");
        assert!(
            matches!(err, KernelError::CapabilityDenied { .. }),
            "expected CapabilityDenied, got {err:?}"
        );

        // Public topics remain allowed under Restricted.
        let ok_msg =
            KernelMessage::text(pid, MessageTarget::Topic("public.events".into()), "ok");
        router
            .send(ok_msg)
            .await
            .expect("Restricted may publish public.*");
    }

    /// WEFT-150: default (IpcScope::All) agents can publish leaf-push topics.
    #[tokio::test]
    async fn all_scope_can_publish_leaf_push_topic() {
        let (router, pids, _receivers) = setup_router(1);
        let msg = KernelMessage::text(
            pids[0],
            MessageTarget::Topic("mesh.leaf.abc123.push".into()),
            "scene",
        );
        router
            .send(msg)
            .await
            .expect("IpcScope::All may publish leaf-push topics");
    }

    #[tokio::test]
    async fn rvf_payload_routes() {
        let (router, pids, mut receivers) = setup_router(2);

        let msg = KernelMessage::new(
            pids[0],
            MessageTarget::Process(pids[1]),
            MessagePayload::Rvf {
                segment_type: 0x40,
                data: vec![0xCA, 0xFE],
            },
        );
        router.send(msg).await.unwrap();

        let received = receivers[1].try_recv().unwrap();
        assert!(matches!(
            received.payload,
            MessagePayload::Rvf {
                segment_type: 0x40,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn closed_inbox_auto_removed() {
        let (router, pids, receivers) = setup_router(2);

        // Drop receiver for pids[1] — closes the inbox channel
        drop(receivers);

        assert!(router.has_inbox(pids[1]));

        // Sending should fail with "inbox closed" and auto-remove the entry
        let msg = KernelMessage::text(pids[0], MessageTarget::Process(pids[1]), "gone");
        let result = router.send(msg).await;
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("inbox closed"),
            "expected 'inbox closed', got: {err_msg}"
        );

        // Inbox should have been removed automatically
        assert!(!router.has_inbox(pids[1]));
    }

    #[tokio::test]
    async fn inbox_overflow_returns_error() {
        // Create a router with 2 agents, but use a small inbox
        // We can't change DEFAULT_INBOX_CAPACITY, so we fill a normal inbox.
        // Instead, create a custom channel with capacity 2 for a targeted test.
        let table = Arc::new(ProcessTable::new(64));

        let sender_entry = ProcessEntry {
            pid: 0,
            agent_id: "sender".to_owned(),
            state: ProcessState::Running,
            capabilities: AgentCapabilities::default(),
            resource_usage: ResourceUsage::default(),
            cancel_token: CancellationToken::new(),
            parent_pid: None,
        };
        let sender_pid = table.insert(sender_entry).unwrap();

        let target_entry = ProcessEntry {
            pid: 0,
            agent_id: "target".to_owned(),
            state: ProcessState::Running,
            capabilities: AgentCapabilities::default(),
            resource_usage: ResourceUsage::default(),
            cancel_token: CancellationToken::new(),
            parent_pid: None,
        };
        let target_pid = table.insert(target_entry).unwrap();

        let checker = Arc::new(CapabilityChecker::new(table.clone()));
        let topic_router = Arc::new(TopicRouter::new(table.clone()));
        let router = A2ARouter::new(table, checker, topic_router);

        // Create sender inbox normally
        let _rx_sender = router.create_inbox(sender_pid);

        // Manually insert a tiny-capacity channel for target (capacity=2)
        let (tx, _rx_target) = mpsc::channel(2);
        router.inboxes.insert(target_pid, tx);

        // Fill it up
        let m1 = KernelMessage::text(sender_pid, MessageTarget::Process(target_pid), "msg1");
        let m2 = KernelMessage::text(sender_pid, MessageTarget::Process(target_pid), "msg2");
        router.send(m1).await.unwrap();
        router.send(m2).await.unwrap();

        // Third message should fail — inbox full
        let m3 = KernelMessage::text(sender_pid, MessageTarget::Process(target_pid), "overflow");
        let result = router.send(m3).await;
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("inbox full"),
            "expected 'inbox full', got: {err_msg}"
        );

        // Inbox should still exist (not removed on full, only on closed)
        assert!(router.has_inbox(target_pid));
    }

    #[tokio::test]
    async fn concurrent_sends_to_same_pid() {
        let (router, pids, mut receivers) = setup_router(4);
        let router = Arc::new(router);
        let target = pids[0];

        // 3 senders each send a message to the same target concurrently
        let mut handles = Vec::new();
        for &sender_pid in &pids[1..] {
            let r = Arc::clone(&router);
            let msg = KernelMessage::text(
                sender_pid,
                MessageTarget::Process(target),
                format!("from-{sender_pid}"),
            );
            handles.push(tokio::spawn(async move { r.send(msg).await }));
        }

        // All sends should succeed
        for h in handles {
            h.await.unwrap().unwrap();
        }

        // Target should have received all 3 messages
        let mut received = Vec::new();
        while let Ok(msg) = receivers[0].try_recv() {
            if let MessagePayload::Text(t) = &msg.payload {
                received.push(t.clone());
            }
        }
        assert_eq!(received.len(), 3);
        // All senders represented (order may vary)
        for &sender_pid in &pids[1..] {
            assert!(
                received.iter().any(|t| t == &format!("from-{sender_pid}")),
                "missing message from PID {sender_pid}"
            );
        }
    }

    #[tokio::test]
    async fn send_from_non_running_process_fails() {
        let table = Arc::new(ProcessTable::new(64));

        // Create sender in Exited state
        let sender_entry = ProcessEntry {
            pid: 0,
            agent_id: "exited-sender".to_owned(),
            state: ProcessState::Exited(0),
            capabilities: AgentCapabilities::default(),
            resource_usage: ResourceUsage::default(),
            cancel_token: CancellationToken::new(),
            parent_pid: None,
        };
        let sender_pid = table.insert(sender_entry).unwrap();

        // Create target in Running state
        let target_entry = ProcessEntry {
            pid: 0,
            agent_id: "target".to_owned(),
            state: ProcessState::Running,
            capabilities: AgentCapabilities::default(),
            resource_usage: ResourceUsage::default(),
            cancel_token: CancellationToken::new(),
            parent_pid: None,
        };
        let target_pid = table.insert(target_entry).unwrap();

        let checker = Arc::new(CapabilityChecker::new(table.clone()));
        let topic_router = Arc::new(TopicRouter::new(table.clone()));
        let router = A2ARouter::new(table, checker, topic_router);
        let _rx1 = router.create_inbox(sender_pid);
        let _rx2 = router.create_inbox(target_pid);

        let msg = KernelMessage::text(sender_pid, MessageTarget::Process(target_pid), "from-dead");
        let result = router.send(msg).await;
        assert!(result.is_err(), "send from non-Running process should fail");
    }

    // ── Service routing tests (K2.1 T3: D19 + D1) ──────────────

    fn setup_router_with_registry(
        agent_count: usize,
    ) -> (
        A2ARouter,
        Vec<Pid>,
        Vec<mpsc::Receiver<KernelMessage>>,
        Arc<crate::service::ServiceRegistry>,
    ) {
        let table = Arc::new(ProcessTable::new(64));
        let mut pids = Vec::new();

        for i in 0..agent_count {
            let entry = ProcessEntry {
                pid: 0,
                agent_id: format!("agent-{i}"),
                state: ProcessState::Running,
                capabilities: AgentCapabilities::default(),
                resource_usage: ResourceUsage::default(),
                cancel_token: CancellationToken::new(),
                parent_pid: None,
            };
            let pid = table.insert(entry).unwrap();
            pids.push(pid);
        }

        let checker = Arc::new(CapabilityChecker::new(table.clone()));
        let topic_router = Arc::new(TopicRouter::new(table.clone()));
        let registry = Arc::new(crate::service::ServiceRegistry::new());
        let router =
            A2ARouter::new(table, checker, topic_router).with_service_registry(registry.clone());

        let mut receivers = Vec::new();
        for &pid in &pids {
            let rx = router.create_inbox(pid);
            receivers.push(rx);
        }

        (router, pids, receivers, registry)
    }

    #[tokio::test]
    async fn route_to_service_by_name() {
        let (router, pids, mut receivers, registry) = setup_router_with_registry(2);

        // Register a service owned by pids[1]
        registry
            .register_entry(crate::service::ServiceEntry {
                name: "auth".into(),
                owner_pid: Some(pids[1]),
                endpoint: crate::service::ServiceEndpoint::AgentInbox(pids[1]),
                audit_level: crate::service::ServiceAuditLevel::Full,
                registered_at: chrono::Utc::now(),
            })
            .unwrap();

        let msg = KernelMessage::text(pids[0], MessageTarget::Service("auth".into()), "validate");
        router.send(msg).await.unwrap();

        let received = receivers[1].try_recv().unwrap();
        assert_eq!(received.from, pids[0]);
        assert!(matches!(
            received.payload,
            MessagePayload::Text(ref t) if t == "validate"
        ));
    }

    #[tokio::test]
    async fn route_service_method() {
        let (router, pids, mut receivers, registry) = setup_router_with_registry(2);

        registry
            .register_entry(crate::service::ServiceEntry {
                name: "auth".into(),
                owner_pid: Some(pids[1]),
                endpoint: crate::service::ServiceEndpoint::AgentInbox(pids[1]),
                audit_level: crate::service::ServiceAuditLevel::Full,
                registered_at: chrono::Utc::now(),
            })
            .unwrap();

        let msg = KernelMessage::text(
            pids[0],
            MessageTarget::ServiceMethod {
                service: "auth".into(),
                method: "validate_token".into(),
            },
            "token-123",
        );
        router.send(msg).await.unwrap();

        let received = receivers[1].try_recv().unwrap();
        assert_eq!(received.from, pids[0]);
        assert!(matches!(
            received.target,
            MessageTarget::ServiceMethod { ref service, ref method }
            if service == "auth" && method == "validate_token"
        ));
    }

    #[tokio::test]
    async fn service_not_found_returns_error() {
        let (router, pids, _receivers, _registry) = setup_router_with_registry(2);

        let msg = KernelMessage::text(
            pids[0],
            MessageTarget::Service("nonexistent".into()),
            "hello",
        );
        let result = router.send(msg).await;
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("service not found"),
            "expected 'service not found', got: {err_msg}"
        );
    }

    #[tokio::test]
    async fn service_entry_registration() {
        let registry = crate::service::ServiceRegistry::new();

        let entry = crate::service::ServiceEntry {
            name: "cache".into(),
            owner_pid: Some(42),
            endpoint: crate::service::ServiceEndpoint::AgentInbox(42),
            audit_level: crate::service::ServiceAuditLevel::Full,
            registered_at: chrono::Utc::now(),
        };
        registry.register_entry(entry).unwrap();

        let retrieved = registry.get_entry("cache").unwrap();
        assert_eq!(retrieved.name, "cache");
        assert_eq!(retrieved.owner_pid, Some(42));
    }

    #[tokio::test]
    async fn service_entry_with_audit_level() {
        let registry = crate::service::ServiceRegistry::new();

        let entry = crate::service::ServiceEntry {
            name: "metrics".into(),
            owner_pid: Some(10),
            endpoint: crate::service::ServiceEndpoint::AgentInbox(10),
            audit_level: crate::service::ServiceAuditLevel::GateOnly,
            registered_at: chrono::Utc::now(),
        };
        registry.register_entry(entry).unwrap();

        let retrieved = registry.get_entry("metrics").unwrap();
        assert_eq!(
            retrieved.audit_level,
            crate::service::ServiceAuditLevel::GateOnly
        );
    }

    #[tokio::test]
    async fn resolve_target_finds_owner_pid() {
        let registry = crate::service::ServiceRegistry::new();

        let entry = crate::service::ServiceEntry {
            name: "search".into(),
            owner_pid: Some(77),
            endpoint: crate::service::ServiceEndpoint::AgentInbox(77),
            audit_level: crate::service::ServiceAuditLevel::Full,
            registered_at: chrono::Utc::now(),
        };
        registry.register_entry(entry).unwrap();

        assert_eq!(registry.resolve_target("search"), Some(77));
        assert_eq!(registry.resolve_target("nonexistent"), None);
    }

    #[tokio::test]
    async fn service_without_registry_returns_error() {
        // Router without service registry should fail on Service target
        let (router_no_reg, pids, _receivers) = setup_router(2);

        let msg = KernelMessage::text(pids[0], MessageTarget::Service("missing".into()), "hello");
        let result = router_no_reg.send(msg).await;
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("no service registry"),
            "expected 'no service registry', got: {err_msg}"
        );
    }

    #[tokio::test]
    async fn external_service_no_pid_returns_error() {
        let (router, pids, _receivers, registry) = setup_router_with_registry(2);

        // Register external service with no owner_pid
        registry
            .register_entry(crate::service::ServiceEntry {
                name: "redis".into(),
                owner_pid: None,
                endpoint: crate::service::ServiceEndpoint::External {
                    url: "redis://localhost:6379".into(),
                },
                audit_level: crate::service::ServiceAuditLevel::GateOnly,
                registered_at: chrono::Utc::now(),
            })
            .unwrap();

        let msg = KernelMessage::text(pids[0], MessageTarget::Service("redis".into()), "ping");
        let result = router.send(msg).await;
        assert!(
            result.is_err(),
            "external service with no PID should fail routing"
        );
    }

    // ── Request-response tests ──────────────────────────────────

    #[tokio::test]
    async fn request_response_completes() {
        let (router, pids, mut receivers) = setup_router(2);
        let router = Arc::new(router);
        let router2 = Arc::clone(&router);

        // Spawn a responder that reads from pids[1]'s inbox and replies.
        let from_pid = pids[0];
        let to_pid = pids[1];
        tokio::spawn(async move {
            let msg = receivers[1].recv().await.unwrap();
            // Build a correlated response back to the sender.
            let reply = KernelMessage::with_correlation(
                to_pid,
                MessageTarget::Process(from_pid),
                MessagePayload::Text("pong".into()),
                msg.id.clone(),
            );
            router2.try_complete_request(reply);
        });

        let request = KernelMessage::text(from_pid, MessageTarget::Process(to_pid), "ping");
        let response = router
            .request(request, Duration::from_secs(5))
            .await
            .unwrap();
        assert!(matches!(
            response.payload,
            MessagePayload::Text(ref t) if t == "pong"
        ));
        assert_eq!(router.pending_request_count(), 0);
    }

    #[tokio::test]
    async fn request_response_timeout() {
        let (router, pids, _receivers) = setup_router(2);

        let request = KernelMessage::text(pids[0], MessageTarget::Process(pids[1]), "no-reply");
        let result = router.request(request, Duration::from_millis(50)).await;
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("timeout"),
            "expected timeout error, got: {err_msg}"
        );
        assert_eq!(router.pending_request_count(), 0);
    }

    #[tokio::test]
    async fn try_complete_request_matching() {
        let (router, pids, _receivers) = setup_router(2);

        // Manually register a pending request.
        let (tx, rx) = oneshot::channel();
        let request_id = "req-42".to_string();
        router.pending_requests.insert(
            request_id.clone(),
            PendingRequest {
                response_tx: tx,
                sent_at: Instant::now(),
            },
        );

        // Build a response with matching correlation_id.
        let reply = KernelMessage::with_correlation(
            pids[1],
            MessageTarget::Process(pids[0]),
            MessagePayload::Text("reply".into()),
            request_id,
        );
        let completed = router.try_complete_request(reply);
        assert!(
            completed,
            "try_complete_request should return true for matching id"
        );

        let response = rx.await.unwrap();
        assert!(matches!(
            response.payload,
            MessagePayload::Text(ref t) if t == "reply"
        ));
    }

    #[tokio::test]
    async fn try_complete_request_no_match() {
        let (router, pids, _receivers) = setup_router(2);

        // No pending requests registered.
        let reply = KernelMessage::with_correlation(
            pids[1],
            MessageTarget::Process(pids[0]),
            MessagePayload::Text("orphan".into()),
            "nonexistent-id".into(),
        );
        let completed = router.try_complete_request(reply);
        assert!(
            !completed,
            "try_complete_request should return false for non-matching id"
        );

        // Also test message with no correlation_id.
        let plain = KernelMessage::text(pids[1], MessageTarget::Process(pids[0]), "plain");
        assert!(!router.try_complete_request(plain));
    }

    #[tokio::test]
    async fn pending_request_count_tracks_correctly() {
        let (router, _pids, _receivers) = setup_router(2);
        assert_eq!(router.pending_request_count(), 0);

        // Insert two pending requests.
        let (tx1, _rx1) = oneshot::channel();
        let (tx2, _rx2) = oneshot::channel();
        router.pending_requests.insert(
            "req-a".into(),
            PendingRequest {
                response_tx: tx1,
                sent_at: Instant::now(),
            },
        );
        assert_eq!(router.pending_request_count(), 1);

        router.pending_requests.insert(
            "req-b".into(),
            PendingRequest {
                response_tx: tx2,
                sent_at: Instant::now(),
            },
        );
        assert_eq!(router.pending_request_count(), 2);

        // Complete one via try_complete_request.
        let reply = KernelMessage::with_correlation(
            1,
            MessageTarget::Process(2),
            MessagePayload::Text("done".into()),
            "req-a".into(),
        );
        router.try_complete_request(reply);
        assert_eq!(router.pending_request_count(), 1);

        // Remove the other manually.
        router.pending_requests.remove("req-b");
        assert_eq!(router.pending_request_count(), 0);
    }

    // ── C4: Routing-time gate tests ────────────────────────────────

    #[cfg(feature = "exochain")]
    #[tokio::test]
    async fn routing_gate_denies_message() {
        struct DenyGate;
        impl crate::gate::GateBackend for DenyGate {
            fn check(
                &self,
                _agent: &str,
                _action: &str,
                _ctx: &serde_json::Value,
            ) -> crate::gate::GateDecision {
                crate::gate::GateDecision::Deny {
                    reason: "blocked by policy".into(),
                    receipt: None,
                }
            }
        }

        let (router, pids, _receivers) = setup_router(2);
        let router = router.with_gate(Arc::new(DenyGate));

        let msg = KernelMessage::text(pids[0], MessageTarget::Process(pids[1]), "hello");
        let result = router.send(msg).await;
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("routing gate denied"),
            "expected 'routing gate denied', got: {err_msg}"
        );
    }

    #[cfg(feature = "exochain")]
    #[tokio::test]
    async fn routing_gate_quota_key_is_per_topic_for_kernel_sender_only() {
        use std::sync::Mutex;
        #[derive(Default)]
        struct Rec {
            seen: Mutex<Vec<(String, Option<String>)>>,
        }
        struct RecGate(Arc<Rec>);
        impl crate::gate::GateBackend for RecGate {
            fn check(
                &self,
                agent: &str,
                _action: &str,
                ctx: &serde_json::Value,
            ) -> crate::gate::GateDecision {
                let q = ctx.get("quota_key").and_then(|v| v.as_str()).map(str::to_owned);
                self.0.seen.lock().unwrap().push((agent.to_owned(), q));
                crate::gate::GateDecision::Permit { token: None }
            }
        }

        let (router, pids, _rx) = setup_router(1);
        let rec = Arc::new(Rec::default());
        let router = router.with_gate(Arc::new(RecGate(rec.clone())));
        // The kernel's own entry, as the daemon has it.
        router
            .process_table
            .insert_with_pid(ProcessEntry {
                pid: 0,
                agent_id: "kernel".into(),
                state: ProcessState::Running,
                capabilities: AgentCapabilities::default(),
                resource_usage: ResourceUsage::default(),
                cancel_token: CancellationToken::new(),
                parent_pid: None,
            })
            .unwrap();

        let _ = router.send(KernelMessage::text(0, MessageTarget::Topic("mesh.leaf.a.push".into()), "x")).await;
        let _ = router.send(KernelMessage::text(0, MessageTarget::Process(pids[0]), "x")).await;
        let _ = router.send(KernelMessage::text(pids[0], MessageTarget::Topic("t".into()), "x")).await;

        let got = rec.seen.lock().unwrap().clone();
        assert_eq!(got.len(), 3, "{got:?}");
        assert_eq!(got[0], ("0".to_string(), Some("0:mesh.leaf.a.push".to_string())));
        assert_eq!(got[1], ("0".to_string(), None), "kernel to a process keeps the shared key");
        assert_eq!(got[2].1, None, "other senders keep their own key");
    }

    #[cfg(feature = "exochain")]
    #[tokio::test]
    async fn routing_gate_permits_message() {
        struct PermitGate;
        impl crate::gate::GateBackend for PermitGate {
            fn check(
                &self,
                _agent: &str,
                _action: &str,
                _ctx: &serde_json::Value,
            ) -> crate::gate::GateDecision {
                crate::gate::GateDecision::Permit { token: None }
            }
        }

        let (router, pids, mut receivers) = setup_router(2);
        let router = router.with_gate(Arc::new(PermitGate));

        let msg = KernelMessage::text(pids[0], MessageTarget::Process(pids[1]), "hello");
        router.send(msg).await.unwrap();

        let received = receivers[1].try_recv().unwrap();
        assert_eq!(received.from, pids[0]);
        assert!(matches!(
            received.payload,
            MessagePayload::Text(ref t) if t == "hello"
        ));
    }

    #[cfg(feature = "exochain")]
    #[tokio::test]
    async fn routing_gate_defer_still_delivers() {
        struct DeferGate;
        impl crate::gate::GateBackend for DeferGate {
            fn check(
                &self,
                _agent: &str,
                _action: &str,
                _ctx: &serde_json::Value,
            ) -> crate::gate::GateDecision {
                crate::gate::GateDecision::Defer {
                    reason: "pending review".into(),
                }
            }
        }

        let (router, pids, mut receivers) = setup_router(2);
        let router = router.with_gate(Arc::new(DeferGate));

        let msg = KernelMessage::text(pids[0], MessageTarget::Process(pids[1]), "deferred");
        router.send(msg).await.unwrap(); // should succeed (defer = deliver)
        assert!(receivers[1].try_recv().is_ok());
    }

    // ── W5: Test hardening — additional coverage ─────────────────

    #[tokio::test]
    async fn multiple_messages_queued_in_order() {
        let (router, pids, mut receivers) = setup_router(2);

        for i in 0..5 {
            let msg =
                KernelMessage::text(pids[0], MessageTarget::Process(pids[1]), format!("msg-{i}"));
            router.send(msg).await.unwrap();
        }

        // Messages should arrive in FIFO order
        for i in 0..5 {
            let received = receivers[1].try_recv().unwrap();
            let expected = format!("msg-{i}");
            assert!(
                matches!(received.payload, MessagePayload::Text(ref t) if t == &expected),
                "expected '{expected}' but got {:?}",
                received.payload,
            );
        }
    }

    #[tokio::test]
    async fn unsubscribe_stops_topic_delivery() {
        let (router, pids, mut receivers) = setup_router(3);

        router.topic_router().subscribe(pids[1], "events");
        router.topic_router().subscribe(pids[2], "events");

        // Unsubscribe pids[1]
        router.topic_router().unsubscribe(pids[1], "events");

        let msg = KernelMessage::text(
            pids[0],
            MessageTarget::Topic("events".into()),
            "after-unsub",
        );
        router.send(msg).await.unwrap();

        // pids[1] should NOT receive (unsubscribed)
        assert!(
            receivers[1].try_recv().is_err(),
            "unsubscribed agent should not receive"
        );

        // pids[2] should still receive
        let received = receivers[2].try_recv().unwrap();
        assert!(matches!(
            received.payload,
            MessagePayload::Text(ref t) if t == "after-unsub"
        ));
    }

    #[tokio::test]
    async fn publish_to_empty_topic_succeeds() {
        let (router, pids, _receivers) = setup_router(2);

        // Publish to a topic with no subscribers — should succeed with no error
        let msg = KernelMessage::text(
            pids[0],
            MessageTarget::Topic("empty-topic".into()),
            "nobody-listening",
        );
        let result = router.send(msg).await;
        assert!(result.is_ok(), "publish to empty topic should succeed");
    }

    #[tokio::test]
    async fn broadcast_with_zero_other_agents() {
        let (router, pids, mut receivers) = setup_router(1);

        let msg = KernelMessage::text(pids[0], MessageTarget::Broadcast, "alone");
        let result = router.send(msg).await;
        assert!(result.is_ok(), "broadcast with only sender should succeed");

        // Sender should not receive their own broadcast
        assert!(receivers[0].try_recv().is_err());
    }

    #[tokio::test]
    async fn ipc_scope_restricted_allows_listed_pids() {
        let table = Arc::new(ProcessTable::new(64));

        use crate::capability::IpcScope;
        // Create target first so we know its PID for the restricted list
        let target_entry = ProcessEntry {
            pid: 0,
            agent_id: "target".to_owned(),
            state: ProcessState::Running,
            capabilities: AgentCapabilities::default(),
            resource_usage: ResourceUsage::default(),
            cancel_token: CancellationToken::new(),
            parent_pid: None,
        };
        let target_pid = table.insert(target_entry).unwrap();

        // Create sender with restricted IPC scope that includes the target
        let sender_entry = ProcessEntry {
            pid: 0,
            agent_id: "restricted-sender".to_owned(),
            state: ProcessState::Running,
            capabilities: AgentCapabilities {
                ipc_scope: IpcScope::Restricted(vec![target_pid]),
                ..Default::default()
            },
            resource_usage: ResourceUsage::default(),
            cancel_token: CancellationToken::new(),
            parent_pid: None,
        };
        let sender_pid = table.insert(sender_entry).unwrap();

        let checker = Arc::new(CapabilityChecker::new(table.clone()));
        let topic_router = Arc::new(TopicRouter::new(table.clone()));
        let router = A2ARouter::new(table, checker, topic_router);
        let _rx_sender = router.create_inbox(sender_pid);
        let mut rx_target = router.create_inbox(target_pid);

        let msg = KernelMessage::text(sender_pid, MessageTarget::Process(target_pid), "allowed");
        let result = router.send(msg).await;
        assert!(result.is_ok(), "restricted sender should reach allowed PID");

        let received = rx_target.try_recv().unwrap();
        assert!(matches!(
            received.payload,
            MessagePayload::Text(ref t) if t == "allowed"
        ));
    }

    #[tokio::test]
    async fn ipc_scope_restricted_blocks_unlisted_pids() {
        let table = Arc::new(ProcessTable::new(64));

        use crate::capability::IpcScope;
        let target_entry = ProcessEntry {
            pid: 0,
            agent_id: "target".to_owned(),
            state: ProcessState::Running,
            capabilities: AgentCapabilities::default(),
            resource_usage: ResourceUsage::default(),
            cancel_token: CancellationToken::new(),
            parent_pid: None,
        };
        let target_pid = table.insert(target_entry).unwrap();

        // Restricted to PID 999 (not the target)
        let sender_entry = ProcessEntry {
            pid: 0,
            agent_id: "restricted-sender".to_owned(),
            state: ProcessState::Running,
            capabilities: AgentCapabilities {
                ipc_scope: IpcScope::Restricted(vec![999]),
                ..Default::default()
            },
            resource_usage: ResourceUsage::default(),
            cancel_token: CancellationToken::new(),
            parent_pid: None,
        };
        let sender_pid = table.insert(sender_entry).unwrap();

        let checker = Arc::new(CapabilityChecker::new(table.clone()));
        let topic_router = Arc::new(TopicRouter::new(table.clone()));
        let router = A2ARouter::new(table, checker, topic_router);
        let _rx_sender = router.create_inbox(sender_pid);
        let _rx_target = router.create_inbox(target_pid);

        let msg = KernelMessage::text(sender_pid, MessageTarget::Process(target_pid), "blocked");
        let result = router.send(msg).await;
        assert!(
            result.is_err(),
            "restricted sender should be blocked from unlisted PID"
        );
    }

    #[test]
    fn create_inbox_returns_receiver() {
        let table = Arc::new(ProcessTable::new(64));
        let checker = Arc::new(CapabilityChecker::new(table.clone()));
        let topic_router = Arc::new(TopicRouter::new(table.clone()));
        let router = A2ARouter::new(table, checker, topic_router);

        let rx = router.create_inbox(10);
        assert!(router.has_inbox(10));
        assert_eq!(router.inbox_count(), 1);
        drop(rx);
    }

    #[test]
    fn create_inbox_replaces_existing() {
        let table = Arc::new(ProcessTable::new(64));
        let checker = Arc::new(CapabilityChecker::new(table.clone()));
        let topic_router = Arc::new(TopicRouter::new(table.clone()));
        let router = A2ARouter::new(table, checker, topic_router);

        let _rx1 = router.create_inbox(10);
        assert_eq!(router.inbox_count(), 1);

        // Replace — old receiver is invalidated
        let _rx2 = router.create_inbox(10);
        assert_eq!(router.inbox_count(), 1);
        assert!(router.has_inbox(10));
    }

    #[test]
    fn remove_nonexistent_inbox_is_noop() {
        let table = Arc::new(ProcessTable::new(64));
        let checker = Arc::new(CapabilityChecker::new(table.clone()));
        let topic_router = Arc::new(TopicRouter::new(table.clone()));
        let router = A2ARouter::new(table, checker, topic_router);

        // Should not panic
        router.remove_inbox(999);
        assert_eq!(router.inbox_count(), 0);
    }

    #[test]
    fn inbox_count_tracks_multiple_inboxes() {
        let table = Arc::new(ProcessTable::new(64));
        let checker = Arc::new(CapabilityChecker::new(table.clone()));
        let topic_router = Arc::new(TopicRouter::new(table.clone()));
        let router = A2ARouter::new(table, checker, topic_router);

        let _rx1 = router.create_inbox(1);
        let _rx2 = router.create_inbox(2);
        let _rx3 = router.create_inbox(3);
        assert_eq!(router.inbox_count(), 3);

        router.remove_inbox(2);
        assert_eq!(router.inbox_count(), 2);
        assert!(!router.has_inbox(2));
        assert!(router.has_inbox(1));
        assert!(router.has_inbox(3));
    }

    #[tokio::test]
    async fn send_to_kernel_target_succeeds() {
        let (router, pids, _receivers) = setup_router(1);

        let msg = KernelMessage::text(pids[0], MessageTarget::Kernel, "kernel-msg");
        let result = router.send(msg).await;
        assert!(
            result.is_ok(),
            "kernel target routing should succeed (even if no-op)"
        );
    }

    #[tokio::test]
    async fn send_to_remote_node_fails() {
        let (router, pids, _receivers) = setup_router(1);

        let msg = KernelMessage::text(
            pids[0],
            MessageTarget::RemoteNode {
                node_id: "remote-1".into(),
                target: Box::new(MessageTarget::Process(42)),
            },
            "remote-msg",
        );
        let result = router.send(msg).await;
        assert!(
            result.is_err(),
            "remote node routing should fail (not yet implemented)"
        );
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("remote routing"),
            "expected remote routing error, got: {err_msg}"
        );
    }

    #[tokio::test]
    async fn suspended_process_can_still_send() {
        let table = Arc::new(ProcessTable::new(64));

        let sender_entry = ProcessEntry {
            pid: 0,
            agent_id: "suspended-sender".to_owned(),
            state: ProcessState::Suspended,
            capabilities: AgentCapabilities::default(),
            resource_usage: ResourceUsage::default(),
            cancel_token: CancellationToken::new(),
            parent_pid: None,
        };
        let sender_pid = table.insert(sender_entry).unwrap();

        let target_entry = ProcessEntry {
            pid: 0,
            agent_id: "target".to_owned(),
            state: ProcessState::Running,
            capabilities: AgentCapabilities::default(),
            resource_usage: ResourceUsage::default(),
            cancel_token: CancellationToken::new(),
            parent_pid: None,
        };
        let target_pid = table.insert(target_entry).unwrap();

        let checker = Arc::new(CapabilityChecker::new(table.clone()));
        let topic_router = Arc::new(TopicRouter::new(table.clone()));
        let router = A2ARouter::new(table, checker, topic_router);
        let _rx_sender = router.create_inbox(sender_pid);
        let mut rx_target = router.create_inbox(target_pid);

        let msg = KernelMessage::text(
            sender_pid,
            MessageTarget::Process(target_pid),
            "from-suspended",
        );
        let result = router.send(msg).await;
        assert!(
            result.is_ok(),
            "suspended process should be allowed to send"
        );

        let received = rx_target.try_recv().unwrap();
        assert!(matches!(
            received.payload,
            MessagePayload::Text(ref t) if t == "from-suspended"
        ));
    }

    #[tokio::test]
    async fn topic_multiple_subscribers_receive_same_message() {
        let (router, pids, mut receivers) = setup_router(5);

        // Subscribe pids[1..5] to the topic
        for &pid in &pids[1..] {
            router.topic_router().subscribe(pid, "news");
        }

        let msg = KernelMessage::text(pids[0], MessageTarget::Topic("news".into()), "breaking");
        router.send(msg).await.unwrap();

        // All subscribers should receive the same payload
        for (i, rx) in receivers[1..].iter_mut().enumerate() {
            let received = rx.try_recv().unwrap_or_else(|_| {
                panic!(
                    "subscriber {} (pid {}) should have received message",
                    i + 1,
                    pids[i + 1]
                )
            });
            assert!(matches!(
                received.payload,
                MessagePayload::Text(ref t) if t == "breaking"
            ));
            assert_eq!(received.from, pids[0]);
        }
    }

    #[tokio::test]
    async fn broadcast_skips_scope_restricted_targets() {
        let table = Arc::new(ProcessTable::new(64));

        use crate::capability::IpcScope;
        // Sender with restricted scope (empty list = nobody allowed)
        let sender_entry = ProcessEntry {
            pid: 0,
            agent_id: "restricted-broadcaster".to_owned(),
            state: ProcessState::Running,
            capabilities: AgentCapabilities {
                ipc_scope: IpcScope::Restricted(vec![]),
                ..Default::default()
            },
            resource_usage: ResourceUsage::default(),
            cancel_token: CancellationToken::new(),
            parent_pid: None,
        };
        let sender_pid = table.insert(sender_entry).unwrap();

        let target_entry = ProcessEntry {
            pid: 0,
            agent_id: "target".to_owned(),
            state: ProcessState::Running,
            capabilities: AgentCapabilities::default(),
            resource_usage: ResourceUsage::default(),
            cancel_token: CancellationToken::new(),
            parent_pid: None,
        };
        let target_pid = table.insert(target_entry).unwrap();

        let checker = Arc::new(CapabilityChecker::new(table.clone()));
        let topic_router = Arc::new(TopicRouter::new(table.clone()));
        let router = A2ARouter::new(table, checker, topic_router);
        let _rx_sender = router.create_inbox(sender_pid);
        let mut rx_target = router.create_inbox(target_pid);

        let msg = KernelMessage::text(sender_pid, MessageTarget::Broadcast, "restricted-broadcast");
        let result = router.send(msg).await;
        assert!(
            result.is_ok(),
            "broadcast itself should succeed even if all targets are blocked"
        );

        // Target should NOT receive (scope blocks it)
        assert!(
            rx_target.try_recv().is_err(),
            "restricted broadcast should not reach any target"
        );
    }

    #[test]
    fn service_registry_accessor() {
        let table = Arc::new(ProcessTable::new(64));
        let checker = Arc::new(CapabilityChecker::new(table.clone()));
        let topic_router = Arc::new(TopicRouter::new(table.clone()));

        let router = A2ARouter::new(table.clone(), checker.clone(), topic_router.clone());
        assert!(
            router.service_registry().is_none(),
            "no registry by default"
        );

        let registry = Arc::new(crate::service::ServiceRegistry::new());
        let router = router.with_service_registry(registry);
        assert!(
            router.service_registry().is_some(),
            "registry should be present after with_service_registry"
        );
    }

    #[test]
    fn topic_router_accessor() {
        let table = Arc::new(ProcessTable::new(64));
        let checker = Arc::new(CapabilityChecker::new(table.clone()));
        let topic_router = Arc::new(TopicRouter::new(table.clone()));
        let expected = Arc::clone(&topic_router);

        let router = A2ARouter::new(table, checker, topic_router);
        assert!(Arc::ptr_eq(router.topic_router(), &expected));
    }

    #[tokio::test]
    async fn json_payload_routes() {
        let (router, pids, mut receivers) = setup_router(2);

        let msg = KernelMessage::new(
            pids[0],
            MessageTarget::Process(pids[1]),
            MessagePayload::Json(serde_json::json!({"key": "value"})),
        );
        router.send(msg).await.unwrap();

        let received = receivers[1].try_recv().unwrap();
        assert!(matches!(
            received.payload,
            MessagePayload::Json(ref v) if v["key"] == "value"
        ));
    }

    #[tokio::test]
    async fn signal_payload_routes() {
        use crate::ipc::KernelSignal;
        let (router, pids, mut receivers) = setup_router(2);

        let msg = KernelMessage::signal(
            pids[0],
            MessageTarget::Process(pids[1]),
            KernelSignal::Shutdown,
        );
        router.send(msg).await.unwrap();

        let received = receivers[1].try_recv().unwrap();
        assert!(matches!(
            received.payload,
            MessagePayload::Signal(KernelSignal::Shutdown)
        ));
    }

    #[tokio::test]
    async fn request_to_nonexistent_target_cleans_up_pending() {
        let table = Arc::new(ProcessTable::new(64));

        let sender_entry = ProcessEntry {
            pid: 0,
            agent_id: "sender".to_owned(),
            state: ProcessState::Running,
            capabilities: AgentCapabilities::default(),
            resource_usage: ResourceUsage::default(),
            cancel_token: CancellationToken::new(),
            parent_pid: None,
        };
        let sender_pid = table.insert(sender_entry).unwrap();

        // Create target in process table but no inbox
        let target_entry = ProcessEntry {
            pid: 0,
            agent_id: "no-inbox-target".to_owned(),
            state: ProcessState::Running,
            capabilities: AgentCapabilities::default(),
            resource_usage: ResourceUsage::default(),
            cancel_token: CancellationToken::new(),
            parent_pid: None,
        };
        let target_pid = table.insert(target_entry).unwrap();

        let checker = Arc::new(CapabilityChecker::new(table.clone()));
        let topic_router = Arc::new(TopicRouter::new(table.clone()));
        let router = A2ARouter::new(table, checker, topic_router);
        let _rx_sender = router.create_inbox(sender_pid);
        // Intentionally no inbox for target

        let msg = KernelMessage::text(sender_pid, MessageTarget::Process(target_pid), "request");
        let result = router.request(msg, Duration::from_millis(100)).await;
        assert!(result.is_err(), "request to PID without inbox should fail");
        assert_eq!(
            router.pending_request_count(),
            0,
            "pending request should be cleaned up on send failure"
        );
    }

    #[tokio::test]
    async fn service_method_not_found_returns_error() {
        let (router, pids, _receivers, _registry) = setup_router_with_registry(2);

        let msg = KernelMessage::text(
            pids[0],
            MessageTarget::ServiceMethod {
                service: "nonexistent-svc".into(),
                method: "do_thing".into(),
            },
            "call",
        );
        let result = router.send(msg).await;
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("service not found"),
            "expected 'service not found' for missing ServiceMethod target, got: {err_msg}"
        );
    }
}
