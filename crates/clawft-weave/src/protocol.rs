//! Wire protocol for daemon <-> client communication.
//!
//! Core types (`Request`, `Response`, path helpers) are re-exported from
//! `clawft-rpc`. This module adds the typed domain-specific result structs
//! used by daemon dispatch and weaver commands.

use serde::{Deserialize, Serialize};

// Re-export core protocol types from clawft-rpc.
pub use clawft_rpc::{
    Request, Response, log_path, pid_path, runtime_dir, runtime_paths, socket_path,
};

// ── Typed result structs ───────────────────────────────────

/// Build provenance stamp reported by the running daemon (installer DX).
///
/// The daemon fills these from the git stamp baked into `weaver` at build
/// time (see `crates/clawft-weave/build.rs`). The CLI compares `sha`
/// against its own baked `BUILD_GIT_HASH` and warns on divergence, so a
/// stale installed binary talking to a freshly-built daemon (or the
/// reverse) is surfaced instead of silently misbehaving.
///
/// `Default` (all-empty) is what older daemons that predate this field
/// deserialize to, via the `#[serde(default)]` on `KernelStatusResult`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BuildStamp {
    /// Git short hash with an optional `-dirty` suffix (e.g. `14145f16-dirty`).
    pub sha: String,
    /// UTC build timestamp (e.g. `2026-07-05T17:20:00Z`).
    pub timestamp: String,
    /// Package version (e.g. `0.6.20`).
    pub version: String,
}

/// Result of `kernel.status`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KernelStatusResult {
    pub state: String,
    pub uptime_secs: f64,
    pub process_count: usize,
    pub service_count: usize,
    pub max_processes: u32,
    pub health_check_interval_secs: u64,
    /// Build provenance of the running daemon. `#[serde(default)]` keeps
    /// the wire format backward-compatible: responses from older daemons
    /// (no `build` key) deserialize with an all-empty stamp.
    #[serde(default)]
    pub build: BuildStamp,
    /// The daemon's `kernel.handshake` payload (profile, roles, runtime
    /// root, bound project, user id and key id), folded in so one call
    /// shows where a client landed. Absent from older daemons.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handshake: Option<clawft_rpc::handshake::Handshake>,
    /// Health of the services a `project`-profile kernel takes from its
    /// parent (ADR-103 Phase 2 F). Absent on every other profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared_services: Option<SharedServicesHealth>,
    /// Where the certified-leaf TCP and discovery sockets are bound; absent
    /// when no leaf listener is running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leaf_listeners: Option<LeafListeners>,
    /// Every bound certified-leaf listener when `leaf_listen_addr` names
    /// several; `leaf_listeners` is the first of these.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub leaf_listener_list: Vec<LeafListeners>,
}

/// Bound addresses of the certified-leaf listeners (separate from the main
/// mesh listener when `kernel.mesh.leaf_listen_addr` is set).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeafListeners {
    pub tcp: String,
    pub discovery: String,
}

/// `parent` when the user daemon answered last time, `down` otherwise. There
/// is no third state: a down service fails closed, it never runs locally.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedServicesHealth {
    pub embeddings: String,
    pub llm: String,
    pub voice: String,
}

/// A single process entry for `kernel.ps`.
///
/// # Row-id contract (WEFT-416)
///
/// `row_id` is the stable unique key for substrate per-row
/// Replace/Remove deltas. Prefer it over `pid` alone: `kernel.ps`
/// merges registered services as virtual rows that share the daemon
/// OS pid, so pid is not unique in the merged list.
///
/// Conventions:
/// - process-table rows → `pid:{pid}`
/// - service-as-process rows → `svc:{name}`
/// - agent.list rows → `pid:{pid}`
///
/// Older daemons may omit `row_id` (serde default empty); clients
/// should fall back via [`process_row_id_from_fields`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessInfo {
    /// Stable unique row key for substrate list-by-id deltas (WEFT-416).
    #[serde(default)]
    pub row_id: String,
    pub pid: u64,
    pub agent_id: String,
    pub state: String,
    pub memory_bytes: u64,
    pub cpu_time_ms: u64,
    pub parent_pid: Option<u64>,
}

/// Build a process-table / agent `row_id` (`pid:{pid}`).
pub fn process_row_id_for_pid(pid: u64) -> String {
    format!("pid:{pid}")
}

/// Build a service-as-process `row_id` (`svc:{name}`) for virtual
/// rows that `kernel.ps` merges in under the daemon OS pid.
pub fn process_row_id_for_service(name: &str) -> String {
    format!("svc:{name}")
}

/// Derive a process `row_id` when the daemon omitted it (legacy
/// wire shape). Prefer `pid:{pid}` when `agent_id` does not look like
/// a service-virtual row; callers that know the row is a service
/// should use [`process_row_id_for_service`] instead.
pub fn process_row_id_from_fields(pid: u64, agent_id: &str) -> String {
    // Service-as-process rows historically set agent_id = service
    // name and shared the daemon pid. Without an explicit row_id we
    // cannot tell them apart from a real process at that pid, so we
    // use a composite that stays unique either way.
    format!("{pid}:{agent_id}")
}

/// A single service entry for `kernel.services`.
///
/// `state` is the user-facing lifecycle string ("running" / "stopped"
/// / "failed" / etc.) derived from the service's [`SystemService::
/// health_check`] return value. `health` is the raw health-probe
/// label kept for backward compatibility with older clients.
///
/// `pid` is the OS process the service runs in; for in-process
/// services (the default) that's the daemon's own PID. `restarts`
/// counts `service.restart` invocations since the daemon booted.
/// `uptime_ms` is approximate — measured from kernel boot time for
/// services that boot with the kernel.
///
/// # Row-id contract (WEFT-416)
///
/// `row_id` equals `name` (service names are unique in the registry).
/// Substrate emits per-row deltas at
/// `substrate/kernel/services/by-name/{row_id}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceInfo {
    /// Stable unique row key — equals [`Self::name`] (WEFT-416).
    #[serde(default)]
    pub row_id: String,
    pub name: String,
    pub service_type: String,
    pub state: String,
    pub health: String,
    pub pid: Option<u64>,
    pub restarts: u64,
    pub uptime_ms: u64,
    /// Optional operator-facing metrics / detail from
    /// [`clawft_kernel::SystemService::health_detail`] (WEFT-333).
    /// Empty when the service provides none. Backward-compatible:
    /// older daemons omit the key; clients default to `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// A single log entry for `kernel.logs` / `kernel.logs_stream`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub timestamp: String,
    pub phase: String,
    pub level: String,
    pub message: String,
    /// Monotonic sequence from the daemon event log (WEFT-434).
    /// Absent / zero on legacy payloads that predate the field.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub seq: u64,
}

fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}

impl LogEntry {
    /// Build a wire log entry from a kernel boot event.
    pub fn from_boot_event(e: &clawft_kernel::BootEvent) -> Self {
        Self {
            timestamp: e.timestamp.to_rfc3339(),
            phase: e.phase.tag().to_owned(),
            level: format!("{:?}", e.level).to_lowercase(),
            message: e.message.clone(),
            seq: e.seq,
        }
    }
}

/// Parameters for `kernel.logs` and `kernel.logs_stream`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogsParams {
    /// Number of recent entries to return (0 = all for `kernel.logs`;
    /// for `kernel.logs_stream`, 0 means stream-only with no initial
    /// tail replay).
    #[serde(default)]
    pub count: usize,
    /// Minimum level filter: "debug", "info", "warn", "error".
    #[serde(default)]
    pub level: Option<String>,
}

impl Default for LogsParams {
    fn default() -> Self {
        Self {
            count: 50,
            level: None,
        }
    }
}

// ── Cluster result types ──────────────────────────────────

/// Result of `cluster.status`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterStatusResult {
    pub total_nodes: usize,
    pub healthy_nodes: usize,
    pub total_shards: usize,
    pub active_shards: usize,
    pub consensus_enabled: bool,
}

/// A single node entry for `cluster.nodes`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterNodeInfo {
    pub node_id: String,
    pub name: String,
    pub platform: String,
    pub state: String,
    pub address: Option<String>,
    /// RFC 3339 time the membership last heard a verified join, recovery or announce
    /// from the peer (per-pong liveness is `fleet.snapshot` `mesh.last_seen`); empty
    /// when the membership
    /// has no record of the peer.
    #[serde(alias = "last_seen")]
    pub last_announce: String,
}

impl ClusterNodeInfo {
    /// Row for `cluster.nodes` from a membership entry. `peer` is the full
    /// record (`None` if it vanished between the list and the lookup).
    pub fn from_peer(
        id: &str,
        state: &impl std::fmt::Display,
        platform: &impl std::fmt::Display,
        peer: Option<clawft_kernel::PeerNode>,
    ) -> Self {
        Self {
            node_id: id.to_owned(),
            name: peer.as_ref().map_or_else(|| id.to_owned(), |p| p.name.clone()),
            platform: platform.to_string(),
            state: state.to_string(),
            last_announce: peer.as_ref().map_or_else(String::new, |p| {
                p.last_heartbeat.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
            }),
            address: peer.and_then(|p| p.address),
        }
    }
}

/// A single shard entry for `cluster.shards`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterShardInfo {
    pub shard_id: u32,
    pub primary_node: String,
    pub replica_nodes: Vec<String>,
    pub vector_count: usize,
    pub status: String,
}

/// Parameters for `cluster.join`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterJoinParams {
    /// Address of the node to join (for native nodes).
    #[serde(default)]
    pub address: Option<String>,
    /// Platform type: "native", "browser", "edge", "wasi".
    #[serde(default = "default_platform")]
    pub platform: String,
    /// Node display name.
    #[serde(default)]
    pub name: Option<String>,
}

fn default_platform() -> String {
    "native".into()
}

/// Parameters for `cluster.leave`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterLeaveParams {
    /// Node ID to remove from the cluster.
    pub node_id: String,
}

// ── Chain result types ────────────────────────────────────

/// Result of `chain.status`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainStatusResult {
    pub chain_id: u32,
    pub sequence: u64,
    pub event_count: usize,
    pub checkpoint_count: usize,
    pub events_since_checkpoint: u64,
    pub last_hash: String,
}

/// A single chain event for `chain.local`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainEventInfo {
    pub sequence: u64,
    pub chain_id: u32,
    pub timestamp: String,
    pub source: String,
    pub kind: String,
    pub hash: String,
    /// Condensed payload summary (e.g. "pid=2 agent=worker-1").
    #[serde(default)]
    pub detail: String,
}

/// Parameters for `chain.local`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainLocalParams {
    /// Number of recent events to return (0 = all).
    #[serde(default)]
    pub count: usize,
}

/// Result of `chain.verify`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainVerifyResult {
    pub valid: bool,
    pub event_count: usize,
    pub errors: Vec<String>,
    /// Ed25519 signature verification: None=unsigned, Some(true)=valid, Some(false)=invalid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature_verified: Option<bool>,
}

// ── Resource tree result types ────────────────────────────

/// Result of `resource.stats`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceStatsResult {
    pub total_nodes: usize,
    pub root_hash: String,
    pub namespaces: usize,
    pub services: usize,
    pub agents: usize,
    pub devices: usize,
}

/// A single resource node for `resource.tree` / `resource.inspect`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceNodeInfo {
    pub id: String,
    pub kind: String,
    pub parent: Option<String>,
    pub children: Vec<String>,
    pub metadata: serde_json::Value,
    pub merkle_hash: String,
    /// Optional 6-dimension scoring vector (present when scoring exists).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scoring: Option<ResourceScoreResult>,
}

/// Parameters for `resource.inspect`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceInspectParams {
    /// Resource path to inspect.
    pub path: String,
}

// ── Agent result types ───────────────────────────────────

/// Parameters for `agent.spawn`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSpawnParams {
    /// Unique identifier for the agent.
    pub agent_id: String,
    /// Optional parent PID.
    #[serde(default)]
    pub parent_pid: Option<u64>,
}

/// Result of `agent.spawn`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSpawnResult {
    pub pid: u64,
    pub agent_id: String,
}

/// Parameters for `agent.stop`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentStopParams {
    /// PID of the agent to stop.
    pub pid: u64,
    /// Whether to stop gracefully (default: true).
    #[serde(default = "default_true")]
    pub graceful: bool,
}

fn default_true() -> bool {
    true
}

/// Parameters for `agent.restart`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRestartParams {
    /// PID of the agent to restart.
    pub pid: u64,
}

/// Full inspection result for `agent.inspect`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentInspectResult {
    pub pid: u64,
    pub agent_id: String,
    pub state: String,
    pub memory_bytes: u64,
    pub cpu_time_ms: u64,
    pub messages_sent: u64,
    pub tool_calls: u64,
    pub topics: Vec<String>,
    pub parent_pid: Option<u64>,
    pub can_spawn: bool,
    pub can_ipc: bool,
    pub can_exec_tools: bool,
    pub can_network: bool,
}

/// Parameters for `agent.send`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSendParams {
    /// Target PID.
    pub pid: u64,
    /// Text message to send.
    pub message: String,
}

// ── Chain export types ───────────────────────────────────

/// Parameters for `chain.export`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainExportParams {
    /// Export format: "json" or "rvf".
    #[serde(default = "default_export_format")]
    pub format: String,
    /// Output file path (daemon-side, used for "rvf" format).
    #[serde(default)]
    pub output: Option<String>,
}

fn default_export_format() -> String {
    "json".into()
}

// ── chain.append (WEFT-324) ────────────────────────────────
//
// Canonical definitions live in `clawft_service_agent::protocol` so the
// agent service and the weave daemon share one wire shape. Re-export
// here for callers that already import chain types from this module.

/// Witness record payload for `chain.append` (WEFT-324).
pub use clawft_service_agent::protocol::WitnessRecord;
/// Parameters for `chain.append` (WEFT-324).
pub use clawft_service_agent::protocol::ChainAppendParams;
/// Result of `chain.append` (WEFT-324).
pub use clawft_service_agent::protocol::ChainAppendResult;

// ── Cron result types ────────────────────────────────────

/// Parameters for `cron.add`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronAddParams {
    /// Human-readable name for the job.
    pub name: String,
    /// Fire every N seconds.
    pub interval_secs: u64,
    /// Command payload to send.
    pub command: String,
    /// Target agent PID (optional).
    #[serde(default)]
    pub target_pid: Option<u64>,
}

/// Parameters for `cron.remove`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronRemoveParams {
    /// Job ID to remove.
    pub id: String,
}

/// A single cron job entry for `cron.list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronJobInfo {
    pub id: String,
    pub name: String,
    pub interval_secs: u64,
    pub command: String,
    pub target_pid: Option<u64>,
    pub enabled: bool,
    pub fire_count: u64,
    pub last_fired: Option<String>,
    pub created_at: String,
}

// ── IPC result types ─────────────────────────────────────

/// A topic entry for `ipc.topics`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcTopicInfo {
    pub topic: String,
    pub subscriber_count: usize,
    pub subscribers: Vec<u64>,
}

/// Parameters for `ipc.subscribe`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcSubscribeParams {
    /// PID to subscribe.
    pub pid: u64,
    /// Topic name.
    pub topic: String,
}

/// Parameters for `ipc.publish`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcPublishParams {
    /// Topic name.
    pub topic: String,
    /// Message payload (text or JSON string).
    pub message: String,
    /// Optional caller identity (agent_id returned by `agent.register`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_id: Option<String>,
    /// Optional Ed25519 signature (hex) of
    /// `blake3(topic || 0x00 || message || 0x00 || ts || 0x00 || actor_id)`
    /// computed with the agent's registered key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// Optional nonce / timestamp (unix millis). Part of the signed
    /// message so replays of past signatures fail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts: Option<u64>,
}

// ── Substrate RPC ─────────────────────────────────────────────

/// Parameters for `substrate.read`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubstrateReadParams {
    /// Substrate path (e.g. `"substrate/test/ping"`).
    pub path: String,
    /// Caller agent_id (required for capture-tier paths).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_id: Option<String>,
}

/// Result of `substrate.read`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubstrateReadResult {
    /// Current value at the path (None if never written).
    pub value: Option<serde_json::Value>,
    /// Monotonic tick for the path.
    pub tick: u64,
    /// Declared sensitivity level as a short lowercase string.
    pub sensitivity: String,
}

/// Parameters for `substrate.publish`.
///
/// Under the node-identity write gate, every publish must be
/// attributed to a **node** — the physical thing that produced the
/// data. The caller provides:
///
/// - `node_id` — registered via `node.register`; deterministically
///   derived from the signing pubkey.
/// - `node_signature` — Ed25519 signature over
///   `node_publish_payload(path, serialized_value, node_ts, node_id)`
///   (see [`clawft_kernel::node_publish_payload`]).
/// - `node_ts` — monotonic timestamp (unix millis) the signature was
///   generated at.
///
/// The path must sit under `substrate/<node_id>/...` (node-private
/// tier) — the gate rejects writes outside that prefix. Unsigned
/// publishes are rejected outright; there is no anonymous-publish
/// bring-up bypass.
///
/// `actor_id` / `signature` / `ts` are kept on the wire for future
/// reuse when the Actions pipeline ships (an Actor performing an
/// Action will sign with their own key alongside the node key); they
/// are accepted but ignored by the current gate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubstratePublishParams {
    /// Substrate path to publish under.
    pub path: String,
    /// Value to Replace into the path.
    pub value: serde_json::Value,
    /// Node-id of the publisher (registered via `node.register`).
    /// Required under the new gate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    /// Ed25519 signature (hex/base64) over
    /// `node_publish_payload(path, value_bytes, node_ts, node_id)`.
    /// Required when `node_id` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_signature: Option<String>,
    /// Monotonic nonce (unix millis) the node signature was generated
    /// at. Required when `node_id` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_ts: Option<u64>,
    /// **Reserved for the Actions pipeline.** Actor identity (UUID
    /// from `agent.register`). Unused by the publish gate today —
    /// Actor-signed Actions are a future addition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_id: Option<String>,
    /// **Reserved for the Actions pipeline.** Actor signature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// **Reserved for the Actions pipeline.** Actor signature
    /// timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts: Option<u64>,
}

/// Parameters for `node.identity`.
///
/// Empty params — caller asks "who is this daemon, what's its node-id."
/// The reply lets remote nodes (the ESP32 firmware especially) build
/// control-path prefixes without hardcoding the daemon's id.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NodeIdentityParams {}

/// Result of `node.identity` — minimal facts about the daemon's
/// own node identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeIdentityResult {
    /// The daemon's deterministic node-id (`hex(SHA-256(pubkey)[..16])`,
    /// ADR-103 D11).
    pub node_id: String,
    /// Friendly label (always `"daemon"` for this implementation).
    pub label: String,
    /// ISO-8601 timestamp the daemon registered itself at boot.
    pub registered_at: String,
}

/// Parameters for `control.set_enabled`.
///
/// Flips a daemon-managed enable flag and republishes the matching
/// `substrate/<authority-node>/control/<kind>s/<target>` intent so
/// downstream subscribers (GUI, firmware) observe the change.
///
/// `kind` is `"service"` or `"sensor"`. `target` is a slug that
/// matches the substrate-path tail beneath `control/<kind>s/`:
///
/// - service: bare name, e.g. `"whisper"`
/// - sensor:  `<target-node>/<sensor-tail>`, e.g.
///   `"n-bfc4cd/mic/pcm_chunk"`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlSetEnabledParams {
    /// `"service"` or `"sensor"`.
    pub kind: String,
    /// Target slug (matches the substrate path tail).
    pub target: String,
    /// Desired state.
    pub enabled: bool,
    /// Optional human-readable label echoed into the published
    /// intent. Defaults to empty when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Result of `control.set_enabled`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlSetEnabledResult {
    /// Substrate path the new intent was published at.
    pub path: String,
    /// Echo of the new state.
    pub enabled: bool,
}

/// One entry in the `control.list` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlListEntry {
    /// `"service"` or `"sensor"`.
    pub kind: String,
    /// Target slug.
    pub target: String,
    /// Current state.
    pub enabled: bool,
}

/// Result of `control.list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlListResult {
    /// All registered control flags.
    pub entries: Vec<ControlListEntry>,
}

/// One message in an `llm.prompt` conversation.
///
/// `role` is one of `"system"`, `"user"`, `"assistant"`. The daemon
/// passes this through to the underlying chat-completions endpoint
/// without validation; the server rejects unknown roles.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmPromptMessage {
    /// Role: `system` / `user` / `assistant`.
    pub role: String,
    /// Message content.
    pub content: String,
}

/// Parameters for `llm.prompt`.
///
/// The daemon-side LLM service is intentionally minimal in this
/// iteration: a single round-trip request that returns the full
/// completion. Streaming is deferred — when the chat window grows a
/// per-token UI it lands as `llm.prompt_stream` mirroring
/// `substrate.subscribe`'s connection-takeover pattern, not as a
/// breaking change to this RPC.
///
/// At least one of `prompt` (a bare user-turn convenience) or
/// `messages` (full conversation) must be provided. When both are
/// present, `messages` wins and `prompt` is ignored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmPromptParams {
    /// Convenience: a bare user prompt. Treated as a single
    /// `[{role:"user", content: prompt}]` conversation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Full conversation. Overrides `prompt` when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub messages: Option<Vec<LlmPromptMessage>>,
    /// Optional system prompt prepended to the conversation. Ignored
    /// when `messages` already starts with a `system` role.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    /// Sampling temperature. `None` uses the daemon's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Hard cap on generated tokens. `None` uses the daemon's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
}

/// Result of `llm.prompt`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmPromptResult {
    /// Assistant completion text.
    pub completion: String,
    /// Why generation stopped (`"stop"`, `"length"`, etc.). May be
    /// absent on servers that omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
    /// Tokens consumed by the prompt (0 if the server omits usage).
    pub prompt_tokens: u32,
    /// Tokens generated (0 if the server omits usage).
    pub completion_tokens: u32,
    /// Echoed model name (best-effort; may be absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

// ── llm.models (WEFT-256) ───────────────────────────────────
//
// Enumeration surface for the chat panel's model/provider chip strip.
// The daemon always returns at least the configured default so the UI
// has a stable selection even when upstream `/v1/models` is unreachable.

/// One selectable model/provider entry from `llm.models`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LlmModelEntry {
    /// Wire model id (e.g. `hermes-4.3-36b`, `local/hermes-4.3-36b`,
    /// `openrouter/…`). Carried through `agent.chat` metadata as
    /// `metadata.model` when the user selects it.
    pub id: String,
    /// Logical provider bucket (`local`, `openrouter`, `openai`, …).
    pub provider: String,
    /// Human-facing label for the chip strip (often equals `id`).
    pub label: String,
    /// True when this entry is the daemon's configured default.
    #[serde(default)]
    pub is_default: bool,
    /// True when the id came from a live `/v1/models` probe (vs
    /// config-only fallback). Informational for the UI.
    #[serde(default)]
    pub live: bool,
}

/// Result of `llm.models` — WEFT-256 model/provider enumeration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmModelsResult {
    /// Configured default model (always present when the LLM client
    /// is wired).
    pub default_model: String,
    /// Provider label for the default endpoint (`local` for
    /// llama-server, gateway name when base_url looks hosted).
    pub default_provider: String,
    /// Upstream base URL (informational; may be empty when LLM is
    /// not initialized).
    pub base_url: String,
    /// Selectable models. Always includes at least `default_model`
    /// when the client is available.
    pub models: Vec<LlmModelEntry>,
    /// Optional diagnostic when the live `/v1/models` probe failed
    /// (config-only fallback was used).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe_error: Option<String>,
}

// ── Agent chat (Concierge) ─────────────────────────────────
//
// `agent.chat` runs through `clawft-service-agent::AgentService`
// against the per-conv mutex map; the daemon's dispatch is a thin
// translator that converts these wire types into the service's
// `InboundMessage` shape.
//
// The wire-format types live in `clawft_types::agent_chat` (WEFT-498)
// so neither `clawft-weave` nor `clawft-service-agent` has to depend on
// the other for serde. Re-exports below preserve the pre-WEFT-498 import
// paths (`clawft_weave::protocol::AgentChatParams`, etc.).

pub use clawft_types::agent_chat::{
    AgentChatDeferDecideParams, AgentChatDeferDecideResult, AgentChatError, AgentChatMessage,
    AgentChatParams, AgentChatResult, AgentChatStreamFrame, AgentChatToolCall,
    AgentTurnRecordParams, AgentTurnRecordResult, DEFER_DECISION_ALLOW, DEFER_DECISION_CANCEL,
    DEFER_DECISION_DENY, DEFER_DEFAULT_TIMEOUT_MS, DeferPromptEvent, DeferUserDecision,
    RecordedTurn, STREAM_PHASE_AWAITING_DEFER, SpawnedTaskSummary, chat_defer_path,
    chat_stream_path,
};

// ── Agent proposal (WEFT-654 review gate, D9-D11) ──────────
//
// `agent.proposal.{list,accept,discard}` — served against the single
// daemon-hosted `AgentLoop` (via `daemon_agent_loop()`, not
// `AgentService`/`AgentLoopHandle`, which doesn't expose the review-gate
// methods). These types are local to `clawft-weave`: unlike `agent.chat`'s
// wire types, nothing outside the daemon needs to share this shape.

/// One parked proposal in `agent.proposal.list`'s result. The loop's cow
/// lineage is global, so today there is at most one — the list shape stays
/// forward-compatible with per-conversation holds (design D9-D11 Phase 2
/// preview).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentProposalInfo {
    /// The held turn's bracket label (e.g. `turn:agent.chat:conv-id`).
    pub label: String,
    /// How long the proposal has been parked, in whole seconds.
    pub age_secs: u64,
}

/// Result of `agent.proposal.list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentProposalListResult {
    pub pending: Vec<AgentProposalInfo>,
}

/// Params for `agent.proposal.discard`. `reason` defaults to
/// `"operator discard"` when omitted; the timeout sweep (idle reaper) calls
/// `discard_pending` directly with `"timeout"` rather than going through
/// this RPC.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentProposalDiscardParams {
    #[serde(default)]
    pub reason: Option<String>,
}

/// Result of `agent.proposal.accept` / `agent.proposal.discard` — echoes
/// the decided proposal's label back so the caller can confirm which hold
/// was resolved.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentProposalDecisionResult {
    pub label: String,
}

// ── Terminal RPCs ─────────────────────────────────────────
//
// PTY-backed shell sessions hosted in the daemon. The egui Explorer
// terminal panel (and the future Cursor webview terminal, and any
// remote-SSH surface that ships) consume these. The architectural
// reason terminals live in the daemon — not in the surface — is so a
// single shell session is observable from any number of surfaces and
// survives a surface restart within the daemon's lifetime.
//
// Wire shape:
//
// - `terminal.spawn  { rows, cols, shell?, cwd? }
//      → { session_id, rows, cols, shell, cwd }`
// - `terminal.write  { session_id, data }   // data is base64`
//      → `{ ok: true }`
// - `terminal.resize { session_id, rows, cols }`
//      → `{ ok: true }`
// - `terminal.close  { session_id }`
//      → `{ ok: true }`
//
// Output is published to substrate at
// `substrate/<daemon-node>/derived/terminal/<session_id>` as
// `{ data: <base64>, ts_ms: <u64>, exit?: bool }` chunks. Surfaces
// subscribe via the existing substrate.read poll cascade — no
// special-case streaming RPC.

/// Parameters for `terminal.spawn`.
///
/// All fields optional; `rows` and `cols` default to a 24×80 cell PTY
/// when 0. `shell` falls back to `$SHELL` → `/bin/bash` → `/bin/sh`.
/// `cwd` falls back to the daemon's cwd when missing or non-existent.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TerminalSpawnParams {
    /// Initial PTY rows (cells). 0 / missing → service default.
    #[serde(default)]
    pub rows: u16,
    /// Initial PTY cols (cells). 0 / missing → service default.
    #[serde(default)]
    pub cols: u16,
    /// Shell binary path. Empty / missing → auto-detect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
    /// Initial cwd. Empty / missing or non-existent → daemon cwd.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

/// Result of `terminal.spawn`. Echoes the resolved parameters back
/// so the surface can render an accurate "shell · path" header
/// without needing a separate query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalSpawnResult {
    /// Opaque session id. Surfaces stash this and pass it back in
    /// every subsequent `terminal.*` call. Format is `t-<12-hex>`.
    pub session_id: String,
    /// Effective rows the PTY was opened with.
    pub rows: u16,
    /// Effective cols the PTY was opened with.
    pub cols: u16,
    /// Resolved shell path that was spawned.
    pub shell: String,
    /// Resolved cwd the shell was started in.
    pub cwd: String,
    /// Substrate path the surface should subscribe to for output
    /// chunks. Convenience — equal to
    /// `substrate/<daemon-node>/derived/terminal/<session_id>`. Saves
    /// the surface from having to know the daemon's node-id ahead of
    /// time.
    pub output_path: String,
}

/// Parameters for `terminal.write`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalWriteParams {
    /// Target session.
    pub session_id: String,
    /// Bytes to write to the PTY, base64-encoded. Base64 because the
    /// JSON-RPC line carrier doesn't support raw bytes; the bytes
    /// commonly include `\r`, `\n`, and 0x1B escape sequences.
    pub data: String,
}

/// Result of `terminal.write` (and the other side-effect terminal
/// RPCs). Tiny success ack — error cases come back as the standard
/// RPC `{ "error": "..." }` envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalAck {
    /// Always `true` on success.
    pub ok: bool,
}

/// Parameters for `terminal.resize`. Cells, not pixels — applications
/// inside the shell read `TIOCGWINSZ` in cells.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalResizeParams {
    /// Target session.
    pub session_id: String,
    /// New row count.
    pub rows: u16,
    /// New column count.
    pub cols: u16,
}

/// Parameters for `terminal.close`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalCloseParams {
    /// Target session. Closing an unknown / already-closed session
    /// is a no-op success — surfaces unmount and re-mount without
    /// having to remember whether spawn ever succeeded.
    pub session_id: String,
}

/// Substrate value shape the daemon publishes for each terminal
/// output chunk. Documented as a typed struct so a future surface
/// (or a `vte`-based viewer) has one source of truth for the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalChunk {
    /// Bytes the child wrote, base64-encoded. Empty when `exit` is
    /// `true`.
    pub data: String,
    /// Wall-clock ms when the chunk was emitted (or 0 on clock
    /// failure — strictly informational).
    pub ts_ms: u64,
    /// `true` on the final chunk after the child exited (or the
    /// surface called `terminal.close`). After this no more chunks
    /// arrive for the session.
    #[serde(default)]
    pub exit: bool,
}

/// Parameters for `substrate.canonical_publish_payload`.
///
/// Diagnostic RPC — runs the daemon's value-canonicalization +
/// payload-build path and returns the exact bytes the verifier
/// would feed to `Ed25519::verify(...)`. **No signature is
/// checked, no actual publish happens.** Lets a remote node (or a
/// firmware Claude) compute the same bytes locally and diff before
/// shipping a real signed publish.
///
/// See `clawft_kernel::node_publish_payload` for the layout. The
/// only kernel-side transform that's not 1:1-with-the-wire is the
/// re-serialization of `value` through `serde_json::Value`, which
/// alphabetizes object keys (`BTreeMap`-backed under
/// `serde_json/preserve_order: off`). The returned
/// `canonical_value_json` field surfaces that exact byte sequence
/// so callers can direct-compare.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubstrateCanonicalPublishPayloadParams {
    /// Substrate path (same as `substrate.publish.path`).
    pub path: String,
    /// Value (same as `substrate.publish.value`).
    pub value: serde_json::Value,
    /// Node id this publish would be attributed to.
    pub node_id: String,
    /// Timestamp (unix or boot-relative ms — opaque nonce).
    pub node_ts: u64,
}

/// Result of `substrate.canonical_publish_payload`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubstrateCanonicalPublishPayloadResult {
    /// Hex-encoded full payload the verifier would feed to
    /// `Ed25519::verify(...)`. Exactly the bytes a node should
    /// sign with its private key.
    pub payload_hex: String,
    /// Total length of the payload in bytes.
    pub payload_len: usize,
    /// The re-serialized `value` JSON the daemon would embed in
    /// the payload. Equal to `serde_json::to_vec(&params.value)`
    /// — keys come back alphabetically because the workspace
    /// doesn't enable `serde_json/preserve_order`. Use this to
    /// directly compare against your own buffer.
    pub canonical_value_json: String,
}

/// Parameters for `substrate.notify`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubstrateNotifyParams {
    /// Substrate path to pulse.
    pub path: String,
    /// Caller agent_id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_id: Option<String>,
}

/// Default `depth` used by `substrate.list` when the client omits it.
///
/// Matches the Explorer MVP contract (Phase 1 §3.1) — a lazy tree that
/// expands one level per click.
pub const SUBSTRATE_LIST_DEFAULT_DEPTH: u32 = 1;

/// Parameters for `substrate.list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubstrateListParams {
    /// Substrate path prefix (e.g. `"substrate/sensor"`). Empty or `"/"`
    /// lists from the root.
    pub prefix: String,
    /// How many levels below `prefix` to enumerate. Defaults to 1.
    /// A value of 0 returns only the prefix node itself (if it carries
    /// a value).
    #[serde(default = "default_list_depth")]
    pub depth: u32,
    /// Caller agent_id (required for capture-tier prefixes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_id: Option<String>,
}

fn default_list_depth() -> u32 {
    SUBSTRATE_LIST_DEFAULT_DEPTH
}

/// One child entry in the `substrate.list` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubstrateListChild {
    /// Full substrate path of this child.
    pub path: String,
    /// `true` if this path has a published value (vs. a pure internal
    /// node that exists only because it sits above value-bearing
    /// descendants).
    pub has_value: bool,
    /// Count of descendants under `path` that themselves carry a value.
    pub child_count: u32,
}

/// Result of `substrate.list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubstrateListResult {
    /// Children enumerated under the requested prefix (sorted by path).
    pub children: Vec<SubstrateListChild>,
    /// Global substrate tick at the moment the list was taken.
    pub tick: u64,
}

/// Parameters for `substrate.subscribe`.
///
/// Same wire shape as [`IpcSubscribeStreamParams`] — takes over the
/// connection after the initial ack line. One JSON line per update
/// (`{"path":..,"tick":..,"kind":"publish|notify","value":..}`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubstrateSubscribeParams {
    /// Substrate path to subscribe to.
    pub path: String,
    /// Tick to resume from (reserved; M1.5 streams live updates only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_tick: Option<u64>,
    /// Caller agent_id (required for capture-tier paths).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_id: Option<String>,
    /// Optional signature — same scheme as `ipc.subscribe_stream`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// Optional nonce.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts: Option<u64>,
}

/// Parameters for `agent.register`.
///
/// The caller provides a human-readable `name`, a 32-byte Ed25519
/// `pubkey`, and a 64-byte `proof` signature. `proof` is the
/// Ed25519 signature over
/// `b"register\0" || name || b"\0" || pubkey || b"\0" || ts_le`
/// (see `clawft_kernel::register_payload`), which binds the
/// registration to the specific key and a fresh nonce.
///
/// Binary fields (`pubkey`, `proof`) accept either a hex string or a
/// base64 string; parser is permissive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRegisterParams {
    /// Human-readable display name.
    pub name: String,
    /// Ed25519 public key bytes (hex or base64; 32 bytes decoded).
    pub pubkey: String,
    /// Ed25519 signature bytes (hex or base64; 64 bytes decoded).
    pub proof: String,
    /// Monotonic timestamp (unix millis) the proof was generated at.
    pub ts: u64,
}

/// Result of `agent.register`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRegisterResult {
    /// Freshly-assigned agent identifier (UUID v4).
    pub agent_id: String,
    /// Echo of the supplied name.
    pub name: String,
}

/// Parameters for `node.register`.
///
/// A **node** is a physical thing in the mesh (an ESP32 leaf, the
/// daemon host, a future kernel-class peer). Registration is
/// proof-of-possession: the caller signs
/// `b"node.register\0" || pubkey || b"\0" || ts_le || b"\0" || label`
/// (see [`clawft_kernel::node_publish_payload`]'s sibling
/// [`clawft_kernel::node_registry::node_register_payload`]) so a
/// hostile client cannot register someone else's key.
///
/// The node-id is **derived deterministically** from the pubkey
/// (`hex(SHA-256(pubkey)[..16])` per
/// ADR-103 D11), so re-running
/// the registration with the same key returns the same id. Distinct
/// from `agent.register` whose `agent_id` is a fresh UUID.
///
/// Binary fields (`pubkey`, `proof`) accept either a hex string or
/// a base64 string; parser is permissive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeRegisterParams {
    /// Optional human-readable label, e.g. `"esp32-workbench"`.
    /// Stored in the registry as a convenience copy; authoritative
    /// label lives at `substrate/<node-id>/meta/label`. May be empty.
    #[serde(default)]
    pub label: String,
    /// Ed25519 public key bytes (hex or base64; 32 bytes decoded).
    pub pubkey: String,
    /// Ed25519 signature bytes (hex or base64; 64 bytes decoded).
    pub proof: String,
    /// Monotonic timestamp (unix millis) the proof was generated at.
    pub ts: u64,
}

/// Result of `node.register`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeRegisterResult {
    /// The deterministic node-id derived from the pubkey.
    pub node_id: String,
    /// Echo of the supplied label (may be empty).
    pub label: String,
}

/// Parameters for `ipc.subscribe_stream`.
///
/// After a successful ack, the daemon keeps the connection open and
/// forwards every matching publish as one JSON line per message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcSubscribeStreamParams {
    /// Topic name to subscribe to.
    pub topic: String,
    /// Resume marker — reserved for future use (requires a topic
    /// ring-buffer; M1.5 streams live publishes only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_tick: Option<u64>,
    /// Caller identity (agent_id returned by `agent.register`).
    ///
    /// Required when the target topic is declared `Capture` or
    /// higher sensitivity on the substrate side.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_id: Option<String>,
    /// Optional Ed25519 signature (hex) of
    /// `blake3(topic || 0x00 || ts || 0x00 || actor_id)` for
    /// authenticated subscriptions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// Optional nonce / timestamp (unix millis).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts: Option<u64>,
}

// ── Resource scoring types ───────────────────────────────

/// Parameters for `resource.score`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceScoreParams {
    /// Resource path to score.
    pub path: String,
}

/// Scoring result for `resource.score`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceScoreResult {
    pub path: String,
    pub trust: f32,
    pub performance: f32,
    pub difficulty: f32,
    pub reward: f32,
    pub reliability: f32,
    pub velocity: f32,
    pub composite: f32,
}

/// Parameters for `resource.rank`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceRankParams {
    /// Number of top-ranked nodes to return.
    #[serde(default = "default_rank_count")]
    pub count: usize,
}

fn default_rank_count() -> usize {
    10
}

/// A ranked resource entry for `resource.rank`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceRankEntry {
    pub path: String,
    pub score: f32,
}

// ── Assessment result types ──────────────────────────────

/// Parameters for `assess.run`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssessRunParams {
    /// Assessment scope: "full", "commit", "ci", or "dependency".
    #[serde(default = "default_assess_scope")]
    pub scope: String,
    /// Output format: "table", "json", or "github-annotations".
    #[serde(default = "default_assess_format")]
    pub format: String,
    /// Working directory to assess (defaults to ".").
    #[serde(default)]
    pub dir: Option<String>,
}

fn default_assess_scope() -> String {
    "full".into()
}
fn default_assess_format() -> String {
    "table".into()
}

/// Parameters for `assess.link`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssessLinkParams {
    /// Peer name / alias.
    pub name: String,
    /// Peer location (URL, path, or address).
    pub location: String,
}

/// Parameters for `assess.compare`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssessCompareParams {
    /// Peer name to compare against.
    pub peer: String,
}

// ── Assessment mesh result types ────────────────────────────

/// Result of `assess.mesh.status`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssessMeshStatusResult {
    pub mesh_enabled: bool,
    pub node_id: Option<String>,
    pub project_name: Option<String>,
    pub peer_count: usize,
    pub peers: Vec<AssessMeshPeerInfo>,
}

/// A single peer entry in the mesh status response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssessMeshPeerInfo {
    pub node_id: String,
    pub project_name: String,
    pub last_assessment: Option<String>,
    pub finding_count: usize,
    pub analyzer_count: usize,
    pub last_gossip: String,
}

/// Result of `assess.mesh.gossip`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssessMeshGossipResult {
    pub sent: bool,
    pub message: String,
}

// ── Custody attestation ─────────────────────────────────────────────

/// Result of `custody.attest`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustodyAttestResult {
    pub device_id: String,
    pub epoch: u64,
    pub chain_head: String,
    pub chain_depth: u64,
    pub vector_count: u64,
    pub content_hash: String,
    pub timestamp: u64,
    pub signature: String,
}

// ── Host revocation ─────────────────────────────────────────────────

/// Parameters for `mesh.revoke`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeshRevokeParams {
    pub host_id: String,
    #[serde(default)]
    pub reason: String,
}

/// Parameters for `mesh.unrevoke`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeshUnrevokeParams {
    pub host_id: String,
}

/// A single entry in the revocation list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RevokedHostInfo {
    pub host_id: String,
    pub revoked_at: u64,
    pub reason: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_roundtrip() {
        let req = Request::new("kernel.status");
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.method, "kernel.status");
    }

    #[test]
    fn response_success_roundtrip() {
        let resp = Response::success(serde_json::json!({"state": "running"}));
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: Response = serde_json::from_str(&json).unwrap();
        assert!(parsed.ok);
        assert_eq!(parsed.result.unwrap()["state"], "running");
    }

    #[test]
    fn response_error_roundtrip() {
        let resp = Response::error("not found");
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: Response = serde_json::from_str(&json).unwrap();
        assert!(!parsed.ok);
        assert_eq!(parsed.error.unwrap(), "not found");
    }

    #[test]
    fn request_with_params() {
        let req = Request::with_params(
            "agent.spawn",
            serde_json::json!({"name": "worker-1", "role": "processor"}),
        );
        assert_eq!(req.params["name"], "worker-1");
    }

    #[test]
    fn socket_path_not_empty() {
        let path = socket_path();
        assert!(path.to_string_lossy().contains("kernel.sock"));
    }

    #[test]
    fn agent_chat_params_omitted_conv_id_gets_default() {
        // Legacy panel wire format (no `conv_id` field) must still
        // deserialize cleanly; the default fills in an ephemeral id.
        let json = r#"{"messages":[{"role":"user","content":"hi"}]}"#;
        let params: AgentChatParams = serde_json::from_str(json).unwrap();
        assert!(
            params.conv_id.starts_with("ephemeral-"),
            "default conv_id must be ephemeral-shaped, got {:?}",
            params.conv_id
        );
        assert_eq!(params.messages.len(), 1);
    }

    #[test]
    fn agent_chat_params_explicit_conv_id_round_trips() {
        let json = r#"{"messages":[],"conv_id":"01HQ123ABCXYZ"}"#;
        let params: AgentChatParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.conv_id, "01HQ123ABCXYZ");
    }

    #[test]
    fn agent_chat_params_default_conv_ids_are_distinct() {
        // Successive default-id calls within the same millisecond must
        // not collide — the atomic counter component differentiates.
        let a = clawft_types::agent_chat::default_conv_id();
        let b = clawft_types::agent_chat::default_conv_id();
        assert_ne!(a, b);
    }

    #[test]
    fn agent_chat_wire_and_service_types_are_identical() {
        // Post-WEFT-498 sanity: clawft_weave::protocol::AgentChatParams
        // and clawft_service_agent::AgentChatParams must be the same
        // type (both re-export from clawft_types::agent_chat). If a
        // future contributor reintroduces a duplicate, the assertion
        // below stops compiling.
        fn _assert_same<T>(_: T, _: T) {}
        let p = AgentChatParams {
            messages: Vec::new(),
            temperature: None,
            max_tokens: None,
            conv_id: "x".into(),
            metadata: None,
            caller_id: None,
        };
        let q: clawft_service_agent::AgentChatParams = AgentChatParams {
            messages: Vec::new(),
            temperature: None,
            max_tokens: None,
            conv_id: "y".into(),
            metadata: None,
            caller_id: None,
        };
        _assert_same(p, q);
    }
}
