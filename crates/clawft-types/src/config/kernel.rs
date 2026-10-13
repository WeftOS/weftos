//! Kernel configuration types.
//!
//! These types are defined in `clawft-types` so they can be embedded
//! in the root [`Config`](super::Config) without creating a circular
//! dependency with `clawft-kernel`.

use serde::{Deserialize, Serialize};

/// Default maximum number of concurrent processes.
fn default_max_processes() -> u32 {
    64
}

/// Default health check interval in seconds.
fn default_health_check_interval_secs() -> u64 {
    30
}

/// Kernel is enabled by default.
fn default_enabled() -> bool {
    true
}

/// Default product display brand (white-label token). WEFT-176.
pub const DEFAULT_BRAND: &str = "WeftOS";

fn default_brand() -> String {
    DEFAULT_BRAND.to_string()
}

/// Process-wide brand slot (set from loaded config at boot / CLI entry).
fn process_brand_slot() -> &'static std::sync::RwLock<String> {
    use std::sync::{OnceLock, RwLock};
    static SLOT: OnceLock<RwLock<String>> = OnceLock::new();
    SLOT.get_or_init(|| RwLock::new(DEFAULT_BRAND.to_string()))
}

/// Install the process-wide product brand (call after config load).
///
/// Empty / whitespace values fall back to [`DEFAULT_BRAND`].
pub fn install_brand(brand: &str) {
    let value = {
        let t = brand.trim();
        if t.is_empty() {
            DEFAULT_BRAND
        } else {
            t
        }
    };
    if let Ok(mut guard) = process_brand_slot().write() {
        *guard = value.to_string();
    }
}

/// Process-wide display brand for white-label surfaces (CLI help, Discord
/// identify, boot banner). Defaults to [`DEFAULT_BRAND`] until
/// [`install_brand`] is called.
///
/// Prefer [`KernelConfig::brand`] / [`super::Config::brand`] when a
/// loaded config is in hand; use this free function on hot paths that
/// only need the installed process token (e.g. Discord gateway identify).
pub fn brand() -> String {
    process_brand_slot()
        .read()
        .map(|g| g.clone())
        .unwrap_or_else(|_| DEFAULT_BRAND.to_string())
}

/// Reset process brand to [`DEFAULT_BRAND`].
///
/// Intended for tests in this crate and dependents (Discord identify,
/// CLI help). Safe to call from production — equivalent to
/// `install_brand(DEFAULT_BRAND)`.
pub fn reset_brand_for_test() {
    install_brand(DEFAULT_BRAND);
}

/// Cluster networking configuration for distributed WeftOS nodes.
///
/// Controls the ruvector-powered clustering layer that coordinates
/// native nodes. Browser/edge nodes join via WebSocket to a
/// coordinator and do not need this configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterNetworkConfig {
    /// Number of replica copies for each shard (default: 3).
    #[serde(default = "default_replication_factor", alias = "replicationFactor")]
    pub replication_factor: usize,

    /// Total number of shards in the cluster (default: 64).
    #[serde(default = "default_shard_count", alias = "shardCount")]
    pub shard_count: u32,

    /// Interval between heartbeat checks in seconds (default: 5).
    #[serde(default = "default_cluster_heartbeat", alias = "heartbeatIntervalSecs")]
    pub heartbeat_interval_secs: u64,

    /// Timeout before marking a node offline in seconds (default: 30).
    #[serde(default = "default_node_timeout", alias = "nodeTimeoutSecs")]
    pub node_timeout_secs: u64,

    /// Whether to enable DAG-based consensus (default: true).
    #[serde(default = "default_enable_consensus", alias = "enableConsensus")]
    pub enable_consensus: bool,

    /// Minimum nodes required for quorum (default: 2).
    #[serde(default = "default_min_quorum", alias = "minQuorumSize")]
    pub min_quorum_size: usize,

    /// Seed node addresses for discovery (coordinator addresses).
    #[serde(default, alias = "seedNodes")]
    pub seed_nodes: Vec<String>,

    /// Human-readable display name for this node.
    #[serde(default, alias = "nodeName")]
    pub node_name: Option<String>,
}

fn default_replication_factor() -> usize {
    3
}
fn default_shard_count() -> u32 {
    64
}
fn default_cluster_heartbeat() -> u64 {
    5
}
fn default_node_timeout() -> u64 {
    30
}
fn default_enable_consensus() -> bool {
    true
}
fn default_min_quorum() -> usize {
    2
}

impl Default for ClusterNetworkConfig {
    fn default() -> Self {
        Self {
            replication_factor: default_replication_factor(),
            shard_count: default_shard_count(),
            heartbeat_interval_secs: default_cluster_heartbeat(),
            node_timeout_secs: default_node_timeout(),
            enable_consensus: default_enable_consensus(),
            min_quorum_size: default_min_quorum(),
            seed_nodes: Vec::new(),
            node_name: None,
        }
    }
}

/// Kernel subsystem configuration.
///
/// Embedded in the root `Config` under the `kernel` key. All fields
/// have sensible defaults so that existing configuration files parse
/// without errors.
///
/// # Example JSON
///
/// ```json
/// {
///   "kernel": {
///     "enabled": false,
///     "max_processes": 128,
///     "health_check_interval_secs": 15
///   }
/// }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KernelConfig {
    /// Whether the kernel subsystem is enabled.
    ///
    /// When `false`, kernel subsystems do not activate unless explicitly
    /// invoked via `weave kernel` CLI commands. Defaults to `true`.
    #[serde(default = "default_enabled")]
    pub enabled: bool,

    /// Maximum number of concurrent processes in the process table.
    #[serde(default = "default_max_processes", alias = "maxProcesses")]
    pub max_processes: u32,

    /// Interval (in seconds) between periodic health checks.
    #[serde(
        default = "default_health_check_interval_secs",
        alias = "healthCheckIntervalSecs"
    )]
    pub health_check_interval_secs: u64,

    /// Product display brand for white-label deployments (WEFT-176).
    ///
    /// Used by Discord identify (`browser` / `device`), CLI help text,
    /// boot banners, and the web UI header. Defaults to `"WeftOS"`.
    /// Binary/crate names are unchanged — this is display-only.
    #[serde(default = "default_brand")]
    pub brand: String,

    /// Cluster networking configuration (native coordinator nodes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cluster: Option<ClusterNetworkConfig>,

    /// Local chain configuration (exochain feature).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chain: Option<ChainConfig>,

    /// Resource tree configuration (exochain feature).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        alias = "resourceTree"
    )]
    pub resource_tree: Option<ResourceTreeConfig>,

    /// Vector search backend configuration (ECC feature).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vector: Option<VectorConfig>,

    /// Spatial BVH index configuration (ECC feature, ADR-056 / WEFT-718).
    ///
    /// Defaults to disabled (`enabled = false`) when present; omit the
    /// section entirely for no spatial subsystem registration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spatial: Option<SpatialConfig>,

    /// Per-user profile namespace configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profiles: Option<ProfilesConfig>,

    /// Time-windowed pairing configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pairing: Option<PairingConfig>,

    /// Mesh networking configuration (K6 transport layer).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mesh: Option<MeshConfig>,

    /// Stream-window chain anchor configuration.
    ///
    /// When enabled, the kernel subscribes to every topic matching
    /// one of the configured prefixes/globs and chain-appends a
    /// `stream.window_commit` event every `window_secs` summarising
    /// the window: BLAKE3 of concatenated message bytes, message
    /// count, byte count, first+last tick, and owning agent_id (when
    /// known). This gives verifiers a tamper-evident anchor without
    /// putting raw frames on-chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<AnchorConfig>,

    /// Optional TCP relay for the daemon's JSON-RPC socket.
    ///
    /// When enabled, the daemon also listens on a TCP port and
    /// transparently forwards every accepted connection to the local
    /// unix socket via in-process byte-copy. Clients speak the exact
    /// same line-delimited JSON-RPC protocol. All auth/policy stays
    /// in the unix-socket handler path — the TCP side is a byte
    /// conduit only. Intended for cross-boundary callers (Windows
    /// side of WSL, remote bridges) that cannot open `AF_UNIX`.
    #[serde(default, skip_serializing_if = "Option::is_none", alias = "ipcTcp")]
    pub ipc_tcp: Option<IpcTcpConfig>,

    /// Optional LLM endpoint configuration.
    ///
    /// Sets the daemon's `llm.prompt` / `agent.chat` upstream when no
    /// `LLM_SERVICE_URL` / `LLM_MODEL` env vars are present. Env vars
    /// always win over this block — the env path is for one-off
    /// experiments; the config block is the durable home.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm: Option<LlmEndpointConfig>,

    /// Optional agent-anchoring configuration.
    ///
    /// Controls whether successful `agent.chat` turns are mirrored into
    /// the witness chain, the HNSW vector index, and the causal graph.
    /// All flags default to `false` so the substrate JSONL archive
    /// behaviour stays the only side-effect unless an operator opts in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentAnchorConfig>,

    /// Governance policy (ADR-103 D12): what requests outside any project
    /// may do. Defaults to `read_only`.
    #[serde(default)]
    pub governance: super::governance::GovernanceConfig,

    /// Kernel profile (ADR-103 A6). `"project"` marks a per-project child
    /// kernel supervised by the user daemon; absent keeps the existing
    /// behaviour. The user-daemon profile is process state, not config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<KernelProfile>,

    /// Which heavy services a `project`-profile kernel takes from its parent
    /// (the user daemon) instead of running itself. Only read when
    /// `profile = "project"`.
    #[serde(default, skip_serializing_if = "SharedServicesConfig::is_default")]
    pub shared_services: SharedServicesConfig,
}

/// Kernel profile selector (`kernel.profile`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KernelProfile {
    /// A per-project child kernel of the user daemon.
    Project,
}

/// Where a shared service runs for a `project`-profile kernel.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SharedServiceMode {
    /// Call the user daemon; fail closed when it is down (never fall back
    /// to a local copy).
    #[default]
    Parent,
}

/// `[kernel.shared_services]`: every service defaults to `parent`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedServicesConfig {
    /// Embedding model.
    #[serde(default)]
    pub embeddings: SharedServiceMode,
    /// LLM provider access (and its API keys).
    #[serde(default)]
    pub llm: SharedServiceMode,
    /// Voice pipeline.
    #[serde(default)]
    pub voice: SharedServiceMode,
}

impl SharedServicesConfig {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

impl Default for KernelConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_processes: default_max_processes(),
            health_check_interval_secs: default_health_check_interval_secs(),
            brand: default_brand(),
            cluster: None,
            chain: None,
            resource_tree: None,
            vector: None,
            spatial: None,
            profiles: None,
            pairing: None,
            mesh: None,
            anchor: None,
            ipc_tcp: None,
            llm: None,
            agent: None,
            governance: Default::default(),
            profile: None,
            shared_services: Default::default(),
        }
    }
}

impl KernelConfig {
    /// Config for a one-shot inspection boot (`kernel status|ps|services|boot`
    /// without `--foreground`): identical, but no network listener is bound,
    /// so it cannot collide with a running daemon's mesh port, and the local
    /// chain is disabled, so it never takes `chain.lock`, never adopts the
    /// legacy chain and never appends to a real chain (ADR-103 Phase 0).
    #[must_use]
    pub fn for_inspection(mut self) -> Self {
        if let Some(mesh) = self.mesh.as_mut() {
            mesh.enabled = false;
        }
        let mut chain = self.chain.take().unwrap_or_default();
        chain.enabled = false;
        self.chain = Some(chain);
        self
    }

    /// Display brand token; empty / whitespace values fall back to
    /// [`DEFAULT_BRAND`].
    pub fn brand(&self) -> &str {
        let t = self.brand.trim();
        if t.is_empty() {
            DEFAULT_BRAND
        } else {
            t
        }
    }
}

// ── Agent-anchor configuration ──────────────────────────────────────────

/// Operator-set agent-anchor flags for `agent.chat`.
///
/// Lives under `[kernel.agent]`. When any flag is true, a successful
/// chat turn produces side-effects beyond the substrate JSONL archive
/// (`_derived/chat/<conv>/turns/<ulid>`):
///
/// - `chain` → append `agent.chat.turn` to the witness chain. The turn
///   is also embedded and semantically indexed into its conversation's
///   `SessionView` via the L2 session tier attached alongside the chain.
/// - `causal` → add a causal node per turn and link it to the previous
///   turn in the same conversation (Explorer "Causal graph" ticks).
///
/// Example:
///
/// ```toml
/// [kernel.agent]
/// anchor_chain  = true
/// anchor_causal = true
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentAnchorConfig {
    /// Append a `agent.chat.turn` event to the witness chain on every
    /// successful turn.
    #[serde(default, alias = "anchorChain")]
    pub anchor_chain: bool,

    /// Add a causal-graph node per turn and link `prev_turn → this_turn`
    /// within the same conversation.
    #[serde(default, alias = "anchorCausal")]
    pub anchor_causal: bool,

    /// Host the daemon-side multiplexed `TalkModeLoop` and wire the text
    /// ImpulseSource (M2 D7). When true, each anchored turn is registered with
    /// the loop and an `EndOfUtterance` is emitted so the turn commits
    /// Frontier→Committed on the shared forest.
    ///
    /// Structurally requires `anchor_chain` (for the global `chain_seq`) and
    /// `anchor_causal` (for the causal graph + cross-refs). The daemon logs a
    /// warning and treats this as inert if those prerequisites are off — it is
    /// deliberately independent of [`any_enabled`](Self::any_enabled) so the
    /// daemon validates the prerequisites rather than folding this flag into
    /// the anchor side-effect set.
    #[serde(default, alias = "talkLoop")]
    pub talk_loop: bool,

    /// Idle timeout in seconds before the daemon reaper ends an inactive
    /// talk-loop conversation (runs the postmortem/promote path, then evicts
    /// the loop's per-conversation state). Only consulted when [`talk_loop`] is
    /// on. `None` ⇒ 1800s (30 min).
    ///
    /// [`talk_loop`]: Self::talk_loop
    #[serde(
        default,
        alias = "talkLoopIdleSecs",
        skip_serializing_if = "Option::is_none"
    )]
    pub talk_loop_idle_secs: Option<u64>,

    /// Voice Wave 2 §W2.1: the non-blocking voice→agent loop. When true, a
    /// `user` turn recorded via `agent.turn.record` is routed through the
    /// interrupt router: idle ⇒ a text reply is generated (`agent.chat`
    /// through the daemon service, capture never blocks on it); busy ⇒ the
    /// utterance is classified as STOP / Refine / Backchannel / Queue and
    /// executed against the in-flight turn (cancel→prune→witness,
    /// cancel-and-resubmit-with-amendment + `Contradicts`).
    ///
    /// Structurally requires [`talk_loop`] (busy-state + forest closures ride
    /// the loop); the daemon treats this as inert and logs a warning when the
    /// prerequisites are off, mirroring the `talk_loop` precedent.
    ///
    /// [`talk_loop`]: Self::talk_loop
    #[serde(default, alias = "voiceLoop")]
    pub voice_loop: bool,

    /// Agent-initiated subagent spawning (M4 D1/D5). Maps 1:1 onto
    /// `clawft_service_agent::SubagentConfig`, which the daemon builds from
    /// this block at agent-service boot. Absent ⇒ the defaults below (spawning
    /// enabled, 5 concurrent per conversation, depth cap 3, 120s per-child
    /// timeout), matching the spawner's own `Default`.
    #[serde(default)]
    pub subagents: SubagentsConfig,

    /// Retained-output review gate (WEFT-653 D10 / WEFT-654). `mode =
    /// "review"` holds each successful turn's memory promote (and, where the
    /// register-early machinery exists, its forest commit) as a PENDING
    /// PROPOSAL until `agent.proposal.accept` (= promote, witnessed) or
    /// `agent.proposal.discard` (= rollback + witnessed prune). A proposal
    /// past `timeout_secs` is DISCARDED, never silently committed
    /// (fail-closed, deny-by-default parity). NOTE: the loop's cow lineage is
    /// global, so a pending proposal holds the WHOLE loop — new dispatches
    /// fail fast with a typed error until the reviewer decides. Absent ⇒
    /// `auto` (today's behavior: promote on finalize).
    #[serde(default)]
    pub proposal: ProposalConfig,

    /// Turn classification & labeling (ADR-067 P2, classification-design §D6).
    /// Gates the `TurnClassifier` the L2 tier runs at `index_turn` so every
    /// committed turn node carries a 4-axis `classification` blob. Absent ⇒
    /// `mode = off` (classification disabled, no per-turn cost), matching the
    /// conservative `talk_loop=false` precedent.
    #[serde(default)]
    pub classification: ClassificationConfig,
}

/// See [`AgentAnchorConfig::proposal`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProposalConfig {
    /// `auto` (default) or `review`.
    #[serde(default)]
    pub mode: ProposalMode,
    /// Seconds a pending proposal may wait before being auto-DISCARDED
    /// (fail-closed). `None` ⇒ 600.
    #[serde(
        default,
        alias = "timeoutSecs",
        skip_serializing_if = "Option::is_none"
    )]
    pub timeout_secs: Option<u64>,
}

/// See [`AgentAnchorConfig::proposal`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProposalMode {
    /// Commit/promote on finalize — today's behavior.
    #[default]
    Auto,
    /// Hold each successful turn as a pending proposal for accept/discard.
    Review,
}

impl AgentAnchorConfig {
    /// True if at least one anchor side-effect is enabled.
    pub fn any_enabled(&self) -> bool {
        self.anchor_chain || self.anchor_causal
    }
}

fn default_subagents_enabled() -> bool {
    true
}

fn default_subagents_max_per_conv() -> u32 {
    5
}

fn default_subagents_max_depth() -> u32 {
    3
}

fn default_subagents_timeout_secs() -> u64 {
    120
}

/// Operator config for agent-initiated subagent spawning (M4 D5).
///
/// Lives under `[kernel.agent.subagents]`. The daemon converts this into the
/// `clawft_service_agent::SubagentConfig` the `DaemonSubagentSpawner` enforces
/// (`max_per_conv` concurrency guard, `max_depth` WEFT-180 cap, `timeout` per
/// child). Every field is `#[serde(default = ...)]` so a partial or absent
/// block fills from the defaults rather than failing to deserialize.
///
/// ```toml
/// [kernel.agent.subagents]
/// enabled       = true
/// max_per_conv  = 5
/// max_depth     = 3
/// timeout_secs  = 120
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentsConfig {
    /// Master switch. When false the spawner refuses every `agent_spawn`
    /// (the tool returns a runtime error) even if the tools are registered.
    #[serde(default = "default_subagents_enabled")]
    pub enabled: bool,

    /// Max concurrent live children per parent conversation (concurrency
    /// guard, design D5). Mirrors the raw-`spawn` tool's cap.
    #[serde(default = "default_subagents_max_per_conv", alias = "maxPerConv")]
    pub max_per_conv: u32,

    /// Spawn recursion depth cap (WEFT-180, design D5). A spawn past this
    /// depth is denied before dispatch.
    #[serde(default = "default_subagents_max_depth", alias = "maxDepth")]
    pub max_depth: u32,

    /// Per-child dispatch timeout in seconds; on timeout the child conv is
    /// cancelled and the task fails.
    #[serde(default = "default_subagents_timeout_secs", alias = "timeoutSecs")]
    pub timeout_secs: u64,

    /// Default for the tool's `notify_on_complete` flag (proactive completion
    /// injection, design D3.2). Flag-gated and deferred past M4 core; carried
    /// here so the config surface is stable.
    #[serde(default, alias = "notifyOnComplete")]
    pub notify_on_complete: bool,

    /// Narrow, opt-in governance grant for `agent_spawn` (D6). The daemon's
    /// chat gate blocks any tool whose effect magnitude exceeds its 0.8
    /// threshold, and `agent_spawn`'s effect (~0.93) is deliberately above it
    /// so a spawn always forces a decision. Because the `GovernanceEngine`
    /// evaluates pure magnitude — it never consults the action string — there
    /// is no config-only way to permit *just* `agent_spawn` without globally
    /// loosening the threshold. When `true`, the daemon adds a per-action
    /// exemption for exactly `tool.agent_spawn` to the chat gate: the spawn is
    /// permitted, but the decision is still evaluated and witnessed (as a
    /// `governance.grant`) so the audit trail shows the grant was exercised.
    /// Default `false` — spawning stays gated until an operator opts in.
    #[serde(default, alias = "governanceGrant")]
    pub governance_grant: bool,
}

impl Default for SubagentsConfig {
    fn default() -> Self {
        Self {
            enabled: default_subagents_enabled(),
            max_per_conv: default_subagents_max_per_conv(),
            max_depth: default_subagents_max_depth(),
            timeout_secs: default_subagents_timeout_secs(),
            notify_on_complete: false,
            governance_grant: false,
        }
    }
}

impl SubagentsConfig {
    /// Per-child dispatch timeout as a [`Duration`](std::time::Duration) — the
    /// shape the spawner's `SubagentConfig.timeout` field wants.
    pub fn timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.timeout_secs)
    }
}

// ── Turn classification configuration ───────────────────────────────────

fn default_classification_queue_bound() -> usize {
    256
}

/// Turn-classification tier selector (classification-design §D6).
///
/// - `Off` (default) — no classifier is attached; turn nodes carry no
///   `classification` blob and the graph view stays inert. Non-ECC /
///   cost-sensitive deployments pay nothing.
/// - `Keyword` — the synchronous keyword tier runs inside `index_turn`
///   (microseconds of CPU); every committed turn gets a full 4-axis blob.
/// - `Full` — keyword tier plus the async LLM enrichment queue (Phase B):
///   after commit, a cheap-model round-trip refines the blob off the turn path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClassificationMode {
    /// No classification (default).
    #[default]
    Off,
    /// Synchronous keyword tier only.
    Keyword,
    /// Keyword tier + async LLM enrichment (Phase B).
    Full,
}

/// Operator config for turn classification (ADR-067 P2, design §D6).
///
/// Lives under `[kernel.agent.classification]`. The daemon reads `mode` at
/// agent-service boot: when it is not `Off`, it constructs a
/// `KeywordTurnClassifier` and attaches it to the `SessionTier`. Mirrors
/// [`SubagentsConfig`] — every field is `#[serde(default)]` so a partial or
/// absent block fills from the defaults rather than failing to deserialize.
///
/// ```toml
/// [kernel.agent.classification]
/// mode           = "keyword"     # off | keyword | full   (default: off)
/// model_override = "haiku-3.5"   # only consulted when mode = full
/// queue_bound    = 256           # async enrich queue depth; full ⇒ drop-oldest
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClassificationConfig {
    /// Which tier to run. `Off` (default) attaches no classifier.
    #[serde(default)]
    pub mode: ClassificationMode,

    /// Cheap-model override for the async LLM tier. Only consulted when
    /// `mode = full`; `None` ⇒ the daemon's default classifier model.
    #[serde(
        default,
        alias = "modelOverride",
        skip_serializing_if = "Option::is_none"
    )]
    pub model_override: Option<String>,

    /// Async enrichment queue depth (Phase B). A full queue drops the oldest
    /// job (best-effort) so `index_turn` never blocks. Only consulted when
    /// `mode = full`.
    #[serde(default = "default_classification_queue_bound", alias = "queueBound")]
    pub queue_bound: usize,
}

impl Default for ClassificationConfig {
    fn default() -> Self {
        Self {
            mode: ClassificationMode::default(),
            model_override: None,
            queue_bound: default_classification_queue_bound(),
        }
    }
}

impl ClassificationConfig {
    /// True when a classifier should be attached (any tier other than `Off`).
    pub fn is_enabled(&self) -> bool {
        self.mode != ClassificationMode::Off
    }

    /// True when the async LLM enrichment tier is on (`mode = full`). Gates the
    /// daemon's Phase-B enrich queue + drain task (design §D4).
    pub fn is_full(&self) -> bool {
        self.mode == ClassificationMode::Full
    }
}

// ── LLM endpoint configuration ──────────────────────────────────────────

/// Operator-set LLM endpoint for `llm.prompt` / `agent.chat`.
///
/// Lives under `[kernel.llm]`. Env vars `LLM_SERVICE_URL` and
/// `LLM_MODEL` override these fields when present. When this block is
/// absent and no env vars are set, the daemon falls back to:
///
/// - `OPENROUTER_API_KEY` set: OpenRouter takeover (defaults to the
///   OpenRouter base URL + a free-tier reasoning model, attaches
///   bearer auth).
/// - Otherwise: ADR-060 local Hermes at
///   [`crate::config::DEFAULT_LOCAL_LLM_SERVICE_URL`] (`:8090`) with
///   model [`crate::config::DEFAULT_LOCAL_LLM_MODEL`].
///
/// Example:
///
/// ```toml
/// [kernel.llm]
/// service_url = "http://127.0.0.1:8090"
/// model = "hermes-4.3-36b"
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LlmEndpointConfig {
    /// Base URL of the OpenAI-compat chat-completions server. The
    /// client appends `/v1/chat/completions` (or `/chat/completions`
    /// when the base already ends with `/v1`). Setting this disables
    /// the OpenRouter takeover even when `OPENROUTER_API_KEY` is in
    /// the environment, so the local endpoint receives no bearer auth
    /// or `HTTP-Referer` / `X-Title` headers.
    #[serde(default, skip_serializing_if = "Option::is_none", alias = "serviceUrl")]
    pub service_url: Option<String>,

    /// Model name sent in the `model` field of the request body. For
    /// llama.cpp this should match the server's `--alias` (or its
    /// default `local` when no alias is set).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

// ── IPC TCP relay configuration ─────────────────────────────────────────

/// Configuration for the optional TCP relay in front of the daemon's
/// unix-socket JSON-RPC.
///
/// Paired with [`crate::config::KernelConfig::ipc_tcp`]. When enabled,
/// the daemon binds `listen_addr` and forwards each accepted TCP
/// connection to the local unix socket via in-process byte-copy. No
/// protocol translation: clients speak the same line-delimited
/// JSON-RPC as unix-socket clients.
///
/// # Security (WEFT-481)
///
/// - `listen_addr` defaults to loopback (`127.0.0.1:9471`). Setting it
///   to `0.0.0.0` exposes the daemon RPC on every interface; the
///   daemon refuses to bind a non-loopback address unless `bearer`
///   is set, so anonymous broadcast can never happen by accident.
/// - `bearer`, when set, gates every TCP connection. The client must
///   send a `Bearer: <token>\n` line as the first line on the wire
///   before any JSON-RPC request. Mismatch closes the connection.
/// - Connections from non-loopback peers are dropped immediately
///   when `bearer` is unset, regardless of `listen_addr`.
///
/// # Example TOML
///
/// ```toml
/// [kernel.ipc_tcp]
/// enabled = true
/// listen_addr = "127.0.0.1:9471"
/// bearer = "deadbeef..."     # required when listen_addr is non-loopback
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcTcpConfig {
    /// Master switch. Default: false.
    #[serde(default)]
    pub enabled: bool,

    /// Address to bind. Loopback-only by default so cross-boundary
    /// callers must explicitly opt into a broader interface.
    #[serde(default = "default_ipc_tcp_listen_addr", alias = "listenAddr")]
    pub listen_addr: String,

    /// Optional shared bearer token. When set, every TCP connection
    /// must send `Bearer: <token>\n` as the first wire line before
    /// any JSON-RPC request, or the connection is closed. Required
    /// when `listen_addr` is non-loopback. WEFT-481.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer: Option<String>,
}

impl Default for IpcTcpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            listen_addr: default_ipc_tcp_listen_addr(),
            bearer: None,
        }
    }
}

fn default_ipc_tcp_listen_addr() -> String {
    "127.0.0.1:9471".to_string()
}

impl IpcTcpConfig {
    /// Returns true when `listen_addr` parses to a non-loopback
    /// address (any-interface or routable). WEFT-481 uses this to
    /// refuse to bind without a bearer token.
    pub fn is_non_loopback(&self) -> bool {
        use std::net::SocketAddr;
        match self.listen_addr.parse::<SocketAddr>() {
            Ok(addr) => !addr.ip().is_loopback(),
            // If we can't parse it, treat as non-loopback so the
            // daemon refuses to bind rather than guessing.
            Err(_) => true,
        }
    }
}

// ── Stream-window anchor configuration ──────────────────────────────────

/// Configuration for the kernel's stream-window chain anchor.
///
/// Paired with [`crate::config::KernelConfig::anchor`]. When enabled,
/// every window_secs seconds the anchor emits a `stream.window_commit`
/// chain event summarising all traffic on topics matching one of
/// `topics`.
///
/// # Example TOML
///
/// ```toml
/// [kernel.anchor]
/// enabled = true
/// topics = ["sensor.*"]
/// window_secs = 2
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnchorConfig {
    /// Master switch. Default: false.
    #[serde(default)]
    pub enabled: bool,

    /// Topic patterns to anchor. Each entry is either an exact topic
    /// name or a single-segment wildcard like `"sensor.*"` which
    /// matches any topic sharing the literal `"sensor."` prefix.
    #[serde(default)]
    pub topics: Vec<String>,

    /// Rolling window duration in seconds. Default: 2.
    #[serde(default = "default_anchor_window_secs")]
    pub window_secs: u64,
}

fn default_anchor_window_secs() -> u64 {
    2
}

impl Default for AnchorConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            topics: Vec::new(),
            window_secs: default_anchor_window_secs(),
        }
    }
}

// ── Mesh networking configuration ──────────────────────────────────────

/// `kernel.mesh.leaf_listen_addr`: one address string, or a list of them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum LeafListenAddr {
    One(String),
    Many(Vec<String>),
}

impl LeafListenAddr {
    /// The configured entries, in order. A list may be empty; callers refuse that.
    pub fn entries(&self) -> Vec<&str> {
        match self {
            Self::One(s) => vec![s.as_str()],
            Self::Many(v) => v.iter().map(String::as_str).collect(),
        }
    }
}

impl From<&str> for LeafListenAddr {
    fn from(s: &str) -> Self {
        Self::One(s.to_owned())
    }
}

impl From<String> for LeafListenAddr {
    fn from(s: String) -> Self {
        Self::One(s)
    }
}

/// Configuration for the K6 mesh transport layer.
///
/// Controls whether the mesh listener is started, what transport to use,
/// and where to bind. When enabled, the kernel spawns a `MeshRuntime`
/// that accepts peer connections and wires them into the A2A router.
///
/// # Example TOML
///
/// ```toml
/// [kernel.mesh]
/// enabled = true
/// transport = "quic"          # "tcp" | "ws" | "quic" (WEFT-118 / ADR-026)
/// listen_addr = "0.0.0.0:9489"
/// noise = true                # Noise XX over the transport (snow)
/// seed_peers = ["quic://10.0.0.2:9489"]
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeshConfig {
    /// Whether the mesh transport is active. Default: false.
    #[serde(default)]
    pub enabled: bool,

    /// Transport backend: `"tcp"` (default), `"ws"` / `"websocket"`, or
    /// `"quic"` (quinn; WEFT-118 / ADR-026). QUIC requires the
    /// `clawft-kernel` `quic` feature (enabled in default builds).
    /// Addresses for QUIC peers use the `quic://host:port` scheme.
    #[serde(default = "default_mesh_transport")]
    pub transport: String,

    /// Address to bind the mesh listener on (default `127.0.0.1` on port
    /// [`DEFAULT_MESH_PORT`], "the weave"; ADR-103 D1). Peers on other
    /// machines need an explicit non-loopback address. `listen` is
    /// accepted as an alias. When `enabled`, a failed bind aborts boot.
    #[serde(default = "default_mesh_listen_addr", alias = "listen")]
    pub listen_addr: String,

    /// Enable peer discovery via Kademlia DHT.
    #[serde(default)]
    pub discovery: bool,

    /// Seed peers to connect to on startup. Each entry is an address, or
    /// `address#node-id` to pin the node id the seed must claim (a seed sends
    /// no hello, so without a pin it is bound to the first id it names).
    #[serde(default)]
    pub seed_peers: Vec<String>,

    /// Enable Noise Protocol encryption on mesh connections.
    /// When true, all peer connections use Noise XX handshake
    /// (Noise_XX_25519_ChaChaPoly_SHA256). Default: false.
    #[serde(default)]
    pub noise: bool,

    /// Path to Ed25519 private key for Noise handshake.
    /// If absent, a ephemeral key is generated at boot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub noise_key_path: Option<String>,

    /// Peer admission policy on the mesh listener (ADR-103, P3-K1).
    ///
    /// - `off`: no policy, but a peer that sends an `AdmitHello` still has
    ///   it consumed and verified, and its `source_node` is bound to the
    ///   verified key.
    /// - `observe` (default): checks and records would-be refusals, never
    ///   refuses, never marks a peer admitted. Takes effect only once
    ///   `genesis_hash` is pinned.
    /// - `enforce`: refuses unsigned, plaintext, wrong-genesis, revoked and
    ///   verdict-denied peers. Needs `genesis_hash`, and a governance gate
    ///   unless `admission_open_membership` is set.
    ///
    /// For every peer that is not *admitted* (anything but `enforce`
    /// accepting a verified hello) the listener strips the envelope's
    /// `src_scope`.
    #[serde(default)]
    pub admission: MeshAdmissionMode,

    /// Cluster genesis hash (64 hex chars) peers must present in their
    /// `AdmitHello`. Required for `admission = "enforce"`. This is a
    /// cluster label, not a credential: anyone who knows it can present it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub genesis_hash: Option<String>,

    /// Mesh nonce (64 hex chars = 32 random bytes), the same on every node of
    /// the mesh, written by the operator next to `genesis_hash` (ADR-106
    /// section 3). With the genesis pin it derives the mesh id a Seed binding,
    /// checkout grant and hash approval are for. It is not a secret.
    /// Generate one with `weaver mesh nonce generate`. Absent: the licence
    /// path stays inert and behaves like `ManifestPolicy`; a changed nonce
    /// orphans the binding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mesh_nonce: Option<String>,

    /// Allow `admission = "enforce"` without a governance gate, i.e. admit
    /// every peer that presents a valid hello for the right genesis. Off by
    /// default: without it enforce refuses everyone when no gate exists.
    #[serde(default)]
    pub admission_open_membership: bool,

    /// Most concurrent inbound mesh connections from one source IP
    /// (IPv6 is counted per /64). Applies under every `admission` mode.
    /// Default: 64.
    #[serde(default = "default_mesh_max_connections_per_ip")]
    pub max_connections_per_ip: usize,

    /// Seconds an inbound connection may stay silent before its first
    /// frame. Applies under every `admission` mode. Default: 10.
    #[serde(default = "default_mesh_first_frame_timeout_secs")]
    pub first_frame_timeout_secs: u64,

    /// Whether this daemon uses the machine mesh service (ADR-103 D2,
    /// P3-U). See [`MeshServicePolicy`].
    #[serde(default)]
    pub service: MeshServicePolicy,

    /// Override for the machine mesh service socket. Absent: the
    /// `WEFTOS_MESH_SOCKET` environment variable, else
    /// `/var/run/weftos/mesh.sock`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_socket: Option<String>,

    /// Where the certified-leaf listeners (WLF1 TCP, and the leaf
    /// discovery UDP socket) bind, kept apart from `listen_addr` so a
    /// machine can accept leaves on its LAN while the main mesh listener
    /// stays on loopback. An IP (`192.0.2.10`) uses `<ip>:<main port + 2>`
    /// for the leaf TCP listener and `<ip>:<main port + 1>` for discovery;
    /// `ip:port` (`192.0.2.10:9600`) sets the leaf TCP port, with discovery
    /// at that port minus one. Absent keeps the old behaviour: the main
    /// listener's IP. A wildcard (`0.0.0.0`, `::`) needs
    /// `leaf_listen_any = true`. Binding wider never changes who is
    /// admitted: every leaf still needs its certificate and signatures.
    ///
    /// A list (`["192.0.2.10", "100.64.0.5"]`) binds one leaf listener and
    /// discovery socket per entry, all feeding the same admission. Each
    /// entry follows the rules above; duplicates are refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leaf_listen_addr: Option<LeafListenAddr>,

    /// Opt-in that allows a wildcard `leaf_listen_addr`. Default: false.
    #[serde(default)]
    pub leaf_listen_any: bool,

    /// Bind the certified-leaf discovery UDP socket on the wildcard address
    /// (`0.0.0.0:<discovery port>`) so broadcast and unicast probes reach it
    /// on any interface. The TCP leaf listeners stay on `leaf_listen_addr`,
    /// and the reply advertises the listener on the probe's subnet.
    /// Default: false.
    #[serde(default)]
    pub leaf_discovery_any: bool,
}

/// How the daemon relates to the machine mesh service (`kernel.mesh.service`).
///
/// - `auto` (default): use the service when its socket answers and verifies,
///   otherwise run the mesh collapsed in this process. A service that answers
///   but fails verification (machine key changed, bind conflict, wrong
///   server uid) is a boot failure, never a silent fallback.
/// - `required`: boot fails when no service answers.
/// - `off`: never probe; collapsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MeshServicePolicy {
    /// Use the service when present, else collapsed.
    #[default]
    Auto,
    /// The service must be present.
    Required,
    /// Never use the service.
    Off,
}

/// Mesh admission policy (see [`MeshConfig::admission`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MeshAdmissionMode {
    /// No admission checks.
    Off,
    /// Check and record, never refuse.
    #[default]
    Observe,
    /// Refuse peers that fail admission.
    Enforce,
}

fn default_mesh_transport() -> String {
    "tcp".to_owned()
}

/// Default mesh listener port ("the weave", ADR-103 D1).
pub const DEFAULT_MESH_PORT: u16 = 9489;

fn default_mesh_max_connections_per_ip() -> usize {
    64
}

fn default_mesh_first_frame_timeout_secs() -> u64 {
    10
}

/// Loopback by default (the same rule as `gateway.host`, ADR-102): a node
/// that peers dial from other machines names the address it listens on
/// (`listen_addr = "0.0.0.0:9489"` or a specific interface).
fn default_mesh_listen_addr() -> String {
    format!("127.0.0.1:{DEFAULT_MESH_PORT}")
}

impl Default for MeshConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            transport: default_mesh_transport(),
            listen_addr: default_mesh_listen_addr(),
            discovery: false,
            seed_peers: vec![],
            noise: false,
            noise_key_path: None,
            admission: MeshAdmissionMode::default(),
            genesis_hash: None,
            mesh_nonce: None,
            admission_open_membership: false,
            max_connections_per_ip: default_mesh_max_connections_per_ip(),
            first_frame_timeout_secs: default_mesh_first_frame_timeout_secs(),
            service: MeshServicePolicy::default(),
            service_socket: None,
            leaf_listen_addr: None,
            leaf_discovery_any: false,
            leaf_listen_any: false,
        }
    }
}

// ── Profile namespace configuration ─────────────────────────────────────

/// Per-user profile namespace configuration.
///
/// When enabled, each profile gets its own isolated vector storage
/// directory under `storage_path`.
///
/// # Example TOML
///
/// ```toml
/// [kernel.profiles]
/// enabled = true
/// storage_path = ".weftos/profiles"
/// default_profile = "default"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfilesConfig {
    /// Whether profile namespaces are enabled.
    #[serde(default = "default_profiles_enabled")]
    pub enabled: bool,

    /// Base directory for profile data.
    #[serde(default = "default_profiles_storage_path")]
    pub storage_path: String,

    /// Default profile to activate on boot.
    #[serde(default = "default_profile_name")]
    pub default_profile: String,
}

fn default_profiles_enabled() -> bool {
    true
}

fn default_profiles_storage_path() -> String {
    ".weftos/profiles".to_owned()
}

fn default_profile_name() -> String {
    "default".to_owned()
}

impl Default for ProfilesConfig {
    fn default() -> Self {
        Self {
            enabled: default_profiles_enabled(),
            storage_path: default_profiles_storage_path(),
            default_profile: default_profile_name(),
        }
    }
}

// ── Time-windowed pairing configuration ─────────────────────────────────

/// Configuration for time-windowed mesh pairing.
///
/// Controls where paired host data is persisted and the default
/// enrollment window duration.
///
/// # Example TOML
///
/// ```toml
/// [kernel.pairing]
/// persist_path = ".weftos/runtime/paired_hosts.json"
/// default_window_secs = 30
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairingConfig {
    /// Path to the paired hosts persistence file.
    #[serde(default = "default_pairing_persist_path")]
    pub persist_path: String,

    /// Default enrollment window duration in seconds.
    #[serde(default = "default_pairing_window_secs")]
    pub default_window_secs: u64,
}

fn default_pairing_persist_path() -> String {
    ".weftos/runtime/paired_hosts.json".to_owned()
}

fn default_pairing_window_secs() -> u64 {
    30
}

impl Default for PairingConfig {
    fn default() -> Self {
        Self {
            persist_path: default_pairing_persist_path(),
            default_window_secs: default_pairing_window_secs(),
        }
    }
}

/// Local chain configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainConfig {
    /// Whether the local chain is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Maximum events before auto-checkpoint.
    #[serde(default = "default_checkpoint_interval", alias = "checkpointInterval")]
    pub checkpoint_interval: u64,

    /// Chain ID (0 = local node chain).
    #[serde(default)]
    pub chain_id: u32,

    /// Path to the chain checkpoint file for persistence across restarts.
    /// If `None`, defaults to `chain.json` under the resolved runtime root
    /// (see [`crate::runtime_paths`]).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        alias = "checkpointPath"
    )]
    pub checkpoint_path: Option<String>,

    /// External chain-head anchoring (ADR-041 / WEFT-137).
    ///
    /// When set, the kernel builds an anchoring controller that periodically
    /// (or on demand) anchors the ExoChain head hash to a non-mock ledger
    /// backend (file ledger by default; external HTTP stub when a target
    /// ledger is configured).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        alias = "externalAnchor"
    )]
    pub external_anchor: Option<ChainExternalAnchorConfig>,
}

fn default_true() -> bool {
    true
}
fn default_checkpoint_interval() -> u64 {
    1000
}

impl Default for ChainConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            checkpoint_interval: default_checkpoint_interval(),
            chain_id: 0,
            checkpoint_path: None,
            external_anchor: None,
        }
    }
}

// ── External chain-head anchoring (ADR-041 / WEFT-137) ──────────────────

/// Backend selection for the kernel `ChainAnchor` trait (ADR-041).
///
/// `File` is the useful default until an external public ledger
/// (OpenTimestamps / Ethereum / etc.) is chosen for a deployment.
/// `External` is a wired stub that persists intent locally and records
/// an endpoint for future HTTP/gRPC submission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ChainAnchorBackend {
    /// No external anchoring (controller not built even if config present).
    #[default]
    None,
    /// Always-succeeding mock (tests / dry-run).
    Mock,
    /// Local append-only hash-linked ledger file (production-useful default).
    File,
    /// External ledger stub with real config wiring (endpoint + intent log).
    External,
    /// A project kernel anchors its chain head to the user daemon
    /// (`project.anchor.submit`, ADR-103 A7). Needs the project key and a
    /// parent transport, so project boot builds it, not [`ChainAnchorBackend`]
    /// config alone.
    Parent,
}

/// Configuration for chain-head external anchoring beyond MockAnchor.
///
/// Example TOML:
/// ```toml
/// [kernel.chain.external_anchor]
/// backend = "file"
/// ledger_path = "~/.clawft/chain/anchors.jsonl"
/// min_interval_secs = 300
/// min_events_between = 100
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainExternalAnchorConfig {
    /// Ledger backend selection.
    #[serde(default)]
    pub backend: ChainAnchorBackend,

    /// Path for the file ledger (File) or intent log (External).
    ///
    /// When `None` and backend is `File`/`External`, the kernel derives
    /// `chain/anchors.jsonl` under the resolved runtime root (native).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        alias = "ledgerPath"
    )]
    pub ledger_path: Option<String>,

    /// Optional HTTP/gRPC endpoint for the External backend.
    ///
    /// When unset, External still writes a durable intent log so operators
    /// can audit pending submissions. Network POST is best-effort once an
    /// endpoint is set (stub records the intent + endpoint binding today).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,

    /// Minimum wall-clock seconds between successful anchors (0 = no time gate).
    #[serde(
        default = "default_anchor_min_interval_secs",
        alias = "minIntervalSecs"
    )]
    pub min_interval_secs: u64,

    /// Minimum ExoChain event sequence delta between anchors (0 = no event gate).
    #[serde(
        default = "default_anchor_min_events",
        alias = "minEventsBetween"
    )]
    pub min_events_between: u64,
}

fn default_anchor_min_interval_secs() -> u64 {
    300
}

fn default_anchor_min_events() -> u64 {
    100
}

impl Default for ChainExternalAnchorConfig {
    fn default() -> Self {
        Self {
            backend: ChainAnchorBackend::File,
            ledger_path: None,
            endpoint: None,
            min_interval_secs: default_anchor_min_interval_secs(),
            min_events_between: default_anchor_min_events(),
        }
    }
}

impl ChainExternalAnchorConfig {
    /// Effective ledger path, expanding a missing path to the default location.
    ///
    /// Precedence: explicit `ledger_path`, then the resolved runtime root's
    /// `chain/` (see [`crate::runtime_paths`]).
    pub fn effective_ledger_path(&self) -> Option<String> {
        if let Some(ref p) = self.ledger_path {
            return Some(p.clone());
        }
        #[cfg(feature = "native")]
        {
            Some(
                crate::runtime_paths::RuntimePaths::resolve()
                    .anchors_ledger()
                    .to_string_lossy()
                    .into_owned(),
            )
        }
        #[cfg(not(feature = "native"))]
        {
            None
        }
    }
}

impl ChainConfig {
    /// A default chain config whose checkpoint (and therefore RVF, signing
    /// key and tree checkpoint) lives under `dir`. For tests, probes and
    /// demos that must never touch the operator chain.
    pub fn isolated_in(dir: &std::path::Path) -> Self {
        Self {
            checkpoint_path: Some(
                dir.join(super::chain_paths::CHAIN_CHECKPOINT_FILE)
                    .to_string_lossy()
                    .into_owned(),
            ),
            ..Self::default()
        }
    }

    /// Returns the effective checkpoint path.
    ///
    /// If `checkpoint_path` is set, returns it. Otherwise, when
    /// `chain.json` under the resolved runtime root (`WEFTOS_RUNTIME_DIR`,
    /// else the project's `.weftos/runtime`, else legacy `~/.clawft`;
    /// requires the `native` feature). The RVF file, signing key and tree
    /// checkpoint are derived from this path by extension. See
    /// [`crate::runtime_paths`].
    pub fn effective_checkpoint_path(&self) -> Option<String> {
        if self.checkpoint_path.is_some() {
            return self.checkpoint_path.clone();
        }
        #[cfg(feature = "native")]
        {
            Some(
                crate::runtime_paths::RuntimePaths::resolve()
                    .chain_checkpoint()
                    .to_string_lossy()
                    .into_owned(),
            )
        }
        #[cfg(not(feature = "native"))]
        {
            None
        }
    }
}

/// Resource tree configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceTreeConfig {
    /// Whether the resource tree is enabled.
    #[serde(default = "default_true_rt")]
    pub enabled: bool,

    /// Path to checkpoint file (None = in-memory only).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        alias = "checkpointPath"
    )]
    pub checkpoint_path: Option<String>,
}

fn default_true_rt() -> bool {
    true
}

impl Default for ResourceTreeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            checkpoint_path: None,
        }
    }
}

// ── Spatial BVH configuration (ADR-056 / WEFT-718) ───────────────────────

/// Which spatial index backend to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SpatialBackendKind {
    /// In-memory BVH (only variant in Phase C).
    #[default]
    Bvh,
}

/// How many derived BVH branches to retain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BranchRetentionPolicy {
    /// Keep the last N derived branches (plus main).
    KeepLast(usize),
}

impl Default for BranchRetentionPolicy {
    fn default() -> Self {
        Self::KeepLast(64)
    }
}

/// Spatial / BVH index configuration under `[kernel.spatial]`.
///
/// See ADR-056 and `.planning/bvh-spatial-index/PLAN.md` Phase C.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpatialConfig {
    /// Whether the spatial service is registered at boot.
    ///
    /// Default: `false` (opt-in). When `false`, ECC boot skips BVH init.
    #[serde(default)]
    pub enabled: bool,

    /// Backend kind (only `bvh` today).
    #[serde(default)]
    pub backend: SpatialBackendKind,

    /// Soft max leaves per branch (store-full guard).
    #[serde(default = "default_spatial_max_leaves")]
    pub max_leaves: usize,

    /// Pending mutations before a forced phase seal (Phase D).
    #[serde(default = "default_spatial_phase_commit_threshold")]
    pub phase_commit_threshold: usize,

    /// Branch retention policy.
    #[serde(default)]
    pub branch_retention: BranchRetentionPolicy,
}

fn default_spatial_max_leaves() -> usize {
    1_000_000
}

fn default_spatial_phase_commit_threshold() -> usize {
    4096
}

impl Default for SpatialConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            backend: SpatialBackendKind::Bvh,
            max_leaves: default_spatial_max_leaves(),
            phase_commit_threshold: default_spatial_phase_commit_threshold(),
            branch_retention: BranchRetentionPolicy::default(),
        }
    }
}

// ── Vector search backend configuration ──────────────────────────────────

/// Which vector search backend to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum VectorBackendKind {
    /// In-memory HNSW (default, fast, suitable for <1M vectors).
    #[default]
    Hnsw,
    /// SSD-backed DiskANN (large scale, 1M+ vectors).
    DiskAnn,
    /// Hot HNSW cache + cold DiskANN store.
    Hybrid,
}

/// HNSW-specific vector configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorHnswConfig {
    /// ef_construction parameter for index building.
    #[serde(default = "default_ef_construction")]
    pub ef_construction: usize,

    /// Number of bi-directional links per node (M parameter).
    #[serde(default = "default_m")]
    pub m: usize,

    /// Maximum number of elements the index can hold.
    #[serde(default = "default_max_elements")]
    pub max_elements: usize,
}

fn default_ef_construction() -> usize {
    200
}
fn default_m() -> usize {
    16
}
fn default_max_elements() -> usize {
    100_000
}

impl Default for VectorHnswConfig {
    fn default() -> Self {
        Self {
            ef_construction: default_ef_construction(),
            m: default_m(),
            max_elements: default_max_elements(),
        }
    }
}

/// DiskANN-specific vector configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorDiskAnnConfig {
    /// Maximum number of points the index can hold.
    #[serde(default = "default_diskann_max_points")]
    pub max_points: usize,

    /// Vector dimensionality.
    #[serde(default = "default_diskann_dimensions")]
    pub dimensions: usize,

    /// Number of neighbors per node in the DiskANN graph.
    #[serde(default = "default_diskann_num_neighbors")]
    pub num_neighbors: usize,

    /// Size of the search candidate list.
    #[serde(default = "default_diskann_search_list_size")]
    pub search_list_size: usize,

    /// Directory path for SSD-backed data files.
    #[serde(default = "default_diskann_data_path")]
    pub data_path: String,

    /// Whether to use product quantization for compression.
    #[serde(default = "default_diskann_use_pq")]
    pub use_pq: bool,

    /// Number of PQ sub-quantizer chunks.
    #[serde(default = "default_diskann_pq_num_chunks")]
    pub pq_num_chunks: usize,
}

fn default_diskann_max_points() -> usize {
    10_000_000
}
fn default_diskann_dimensions() -> usize {
    384
}
fn default_diskann_num_neighbors() -> usize {
    64
}
fn default_diskann_search_list_size() -> usize {
    100
}
fn default_diskann_data_path() -> String {
    ".weftos/diskann".to_owned()
}
fn default_diskann_use_pq() -> bool {
    true
}
fn default_diskann_pq_num_chunks() -> usize {
    48
}

impl Default for VectorDiskAnnConfig {
    fn default() -> Self {
        Self {
            max_points: default_diskann_max_points(),
            dimensions: default_diskann_dimensions(),
            num_neighbors: default_diskann_num_neighbors(),
            search_list_size: default_diskann_search_list_size(),
            data_path: default_diskann_data_path(),
            use_pq: default_diskann_use_pq(),
            pq_num_chunks: default_diskann_pq_num_chunks(),
        }
    }
}

/// Eviction policy for the hybrid backend's hot tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum VectorEvictionPolicy {
    /// Least Recently Used.
    #[default]
    Lru,
}

/// Hybrid backend-specific configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorHybridConfig {
    /// Maximum number of vectors in the hot (HNSW) tier.
    #[serde(default = "default_hybrid_hot_capacity")]
    pub hot_capacity: usize,

    /// Access count threshold before a cold vector is promoted to hot.
    #[serde(default = "default_hybrid_promotion_threshold")]
    pub promotion_threshold: u32,

    /// Eviction policy when the hot tier is full.
    #[serde(default)]
    pub eviction_policy: VectorEvictionPolicy,
}

fn default_hybrid_hot_capacity() -> usize {
    50_000
}
fn default_hybrid_promotion_threshold() -> u32 {
    3
}

impl Default for VectorHybridConfig {
    fn default() -> Self {
        Self {
            hot_capacity: default_hybrid_hot_capacity(),
            promotion_threshold: default_hybrid_promotion_threshold(),
            eviction_policy: VectorEvictionPolicy::default(),
        }
    }
}

/// Unified vector search backend configuration.
///
/// Controls which backend is used for the ECC cognitive substrate's
/// vector search layer.
///
/// # Example TOML
///
/// ```toml
/// [kernel.vector]
/// backend = "hybrid"
/// # Fail boot (instead of warning) if `backend` needs the `diskann`
/// # cargo feature and the binary wasn't built with it.
/// strict = false
///
/// [kernel.vector.hnsw]
/// ef_construction = 200
/// max_elements = 100000
///
/// [kernel.vector.diskann]
/// max_points = 10000000
/// data_path = ".weftos/diskann"
///
/// [kernel.vector.hybrid]
/// hot_capacity = 50000
/// promotion_threshold = 3
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VectorConfig {
    /// Which backend to use.
    #[serde(default)]
    pub backend: VectorBackendKind,

    /// When `true`, boot fails with an error instead of silently falling
    /// back to the brute-force stub if `backend` is `DiskAnn`/`Hybrid` but
    /// the kernel was not compiled with the `diskann` cargo feature
    /// (default: `false` — warn and continue with the degraded stub).
    ///
    /// See `clawft_kernel::vector_diskann` module docs (WEFT-656).
    #[serde(default)]
    pub strict: bool,

    /// HNSW-specific settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hnsw: Option<VectorHnswConfig>,

    /// DiskANN-specific settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diskann: Option<VectorDiskAnnConfig>,

    /// Hybrid-specific settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hybrid: Option<VectorHybridConfig>,

    /// Logarithmic quantization settings (KG-011).
    ///
    /// Runtime lives in `clawft-kernel::vector_quantization` (**in-tree**;
    /// no longer blocked on RuVector#352).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_quantized: Option<LogQuantizedStubConfig>,

    /// Unified distance kernel settings (KG-012).
    ///
    /// Runtime lives in `clawft-kernel::vector_quantization` (**in-tree**).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub simd_distance: Option<SimdDistanceStubConfig>,
}

/// Serializable config for LogQuantized (KG-011).
///
/// Full implementation lives in `clawft-kernel::vector_quantization`
/// (WeftOS first-party; vendor-independent).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogQuantizedStubConfig {
    /// Whether logarithmic quantization is enabled.
    #[serde(default)]
    pub enabled: bool,
    /// Compression ratio (default: 4).
    #[serde(default = "default_log_quantized_compression_ratio")]
    pub compression_ratio: usize,
}

fn default_log_quantized_compression_ratio() -> usize {
    4
}

impl Default for LogQuantizedStubConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            compression_ratio: default_log_quantized_compression_ratio(),
        }
    }
}

/// Serializable config for unified distance (KG-012).
///
/// Full implementation lives in `clawft-kernel::vector_quantization`
/// (WeftOS first-party; vendor-independent).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SimdDistanceStubConfig {
    /// Whether the unified distance kernel is enabled.
    #[serde(default)]
    pub enabled: bool,
    /// Whether to pad vectors to power-of-two length for alignment.
    /// May increase memory for odd dimensions — enable only when useful.
    #[serde(default)]
    pub pad_to_power_of_two: bool,
}

#[cfg(test)]
mod tests {
    #[test]
    fn mesh_listens_on_loopback_unless_configured() {
        assert_eq!(super::MeshConfig::default().listen_addr, "127.0.0.1:9489");
        let none: super::MeshConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(none.listen_addr, "127.0.0.1:9489", "an absent key is loopback");
        let set: super::MeshConfig = serde_json::from_str(r#"{"listen": "0.0.0.0:9489"}"#).unwrap();
        assert_eq!(set.listen_addr, "0.0.0.0:9489", "explicit config is unchanged");
    }

    #[test]
    fn leaf_listen_addr_is_optional_and_parses() {
        let none: super::MeshConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(none.leaf_listen_addr, None);
        assert!(!none.leaf_listen_any);
        let ip: super::MeshConfig = serde_json::from_str(r#"{"leaf_listen_addr": "192.0.2.10"}"#).unwrap();
        assert_eq!(ip.leaf_listen_addr, Some("192.0.2.10".into()));
        let both: super::MeshConfig =
            serde_json::from_str(r#"{"leaf_listen_addr": "192.0.2.10:9600", "leaf_listen_any": true}"#).unwrap();
        assert_eq!(both.leaf_listen_addr, Some("192.0.2.10:9600".into()));
        assert!(both.leaf_listen_any);
        let toml_cfg: super::MeshConfig = toml::from_str("leaf_listen_addr = \"192.0.2.10\"").unwrap();
        assert_eq!(toml_cfg.leaf_listen_addr, Some("192.0.2.10".into()));
        let list: super::MeshConfig =
            toml::from_str("leaf_listen_addr = [\"192.0.2.10\", \"100.64.0.5\"]").unwrap();
        let l = list.leaf_listen_addr.unwrap();
        assert_eq!(l.entries(), vec!["192.0.2.10", "100.64.0.5"]);
        let j: super::MeshConfig = serde_json::from_str(r#"{"leaf_listen_addr": ["192.0.2.10"]}"#).unwrap();
        assert_eq!(j.leaf_listen_addr.unwrap().entries(), vec!["192.0.2.10"]);
        assert!(toml::from_str::<super::MeshConfig>("leaf_listen_addr = 5").is_err());
    }

    #[test]
    fn for_inspection_disables_mesh_listener() {
        let kc = super::KernelConfig {
            mesh: Some(super::MeshConfig {
                enabled: true,
                ..super::MeshConfig::default()
            }),
            ..super::KernelConfig::default()
        }
        .for_inspection();
        assert!(!kc.mesh.unwrap().enabled);
        assert!(super::KernelConfig::default().for_inspection().mesh.is_none());
    }

    #[test]
    fn for_inspection_disables_the_chain() {
        let kc = super::KernelConfig::default().for_inspection();
        assert!(!kc.chain.expect("chain config set").enabled);
        let kc = super::KernelConfig {
            chain: Some(super::ChainConfig {
                enabled: true,
                ..super::ChainConfig::default()
            }),
            ..super::KernelConfig::default()
        }
        .for_inspection();
        assert!(!kc.chain.unwrap().enabled);
    }

    use super::*;

    #[test]
    fn default_kernel_config() {
        let cfg = KernelConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.max_processes, 64);
        assert_eq!(cfg.health_check_interval_secs, 30);
    }

    #[test]
    fn deserialize_empty() {
        let cfg: KernelConfig = serde_json::from_str("{}").unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.max_processes, 64);
    }

    #[test]
    fn deserialize_camel_case() {
        let json = r#"{"maxProcesses": 128, "healthCheckIntervalSecs": 15}"#;
        let cfg: KernelConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.max_processes, 128);
        assert_eq!(cfg.health_check_interval_secs, 15);
    }

    #[test]
    fn brand_defaults_to_weftos() {
        let cfg = KernelConfig::default();
        assert_eq!(cfg.brand(), DEFAULT_BRAND);
        assert_eq!(cfg.brand, "WeftOS");
        let empty: KernelConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(empty.brand(), DEFAULT_BRAND);
    }

    #[test]
    fn brand_accessor_is_configurable() {
        let mut cfg = KernelConfig {
            brand: "Valtech Agentic Mesh".into(),
            ..KernelConfig::default()
        };
        assert_eq!(cfg.brand(), "Valtech Agentic Mesh");
        cfg.brand = "   ".into();
        assert_eq!(cfg.brand(), DEFAULT_BRAND, "whitespace falls back");
        let json = r#"{"brand": "Acme OS"}"#;
        let parsed: KernelConfig = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.brand(), "Acme OS");
    }

    #[test]
    fn process_brand_install_and_reset() {
        install_brand("Custom Brand");
        assert_eq!(brand(), "Custom Brand");
        install_brand("");
        assert_eq!(brand(), DEFAULT_BRAND);
        reset_brand_for_test();
        assert_eq!(brand(), DEFAULT_BRAND);
    }

    #[test]
    fn serde_roundtrip() {
        let cfg = KernelConfig {
            enabled: true,
            max_processes: 256,
            health_check_interval_secs: 10,
            brand: "WeftOS".into(),
            cluster: None,
            chain: None,
            resource_tree: None,
            vector: None,
            spatial: None,
            profiles: None,
            pairing: None,
            mesh: None,
            anchor: None,
            ipc_tcp: None,
            llm: None,
            agent: None,
            governance: Default::default(),
            profile: None,
            shared_services: Default::default(),
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let restored: KernelConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.enabled, cfg.enabled);
        assert_eq!(restored.max_processes, cfg.max_processes);
        assert_eq!(restored.brand(), cfg.brand());
    }

    #[test]
    fn old_configs_load_without_profile_or_shared_services() {
        let cfg: KernelConfig = serde_json::from_str(r#"{"enabled": true}"#).unwrap();
        assert_eq!(cfg.profile, None);
        assert_eq!(cfg.shared_services, SharedServicesConfig::default());
        // The new fields add nothing to a serialised default config.
        let json = serde_json::to_value(KernelConfig::default()).unwrap();
        assert!(json.get("profile").is_none());
        assert!(json.get("shared_services").is_none());
    }

    #[test]
    fn project_profile_and_shared_services_parse() {
        let cfg: KernelConfig = serde_json::from_str(
            r#"{"profile": "project", "shared_services": {"embeddings": "parent", "llm": "parent", "voice": "parent"}}"#,
        )
        .unwrap();
        assert_eq!(cfg.profile, Some(KernelProfile::Project));
        assert_eq!(cfg.shared_services.voice, SharedServiceMode::Parent);
        let back = serde_json::to_value(&cfg).unwrap();
        assert_eq!(back["profile"], "project");
        // A local mode is not accepted (fail closed, no local fallback).
        assert!(
            serde_json::from_str::<KernelConfig>(r#"{"shared_services": {"llm": "local"}}"#)
                .is_err()
        );
    }

    #[test]
    fn profiles_config_defaults() {
        let cfg = ProfilesConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.storage_path, ".weftos/profiles");
        assert_eq!(cfg.default_profile, "default");
    }

    #[test]
    fn profiles_config_deserialize() {
        let json =
            r#"{"enabled": false, "storage_path": "/tmp/profiles", "default_profile": "admin"}"#;
        let cfg: ProfilesConfig = serde_json::from_str(json).unwrap();
        assert!(!cfg.enabled);
        assert_eq!(cfg.storage_path, "/tmp/profiles");
        assert_eq!(cfg.default_profile, "admin");
    }

    #[test]
    fn pairing_config_defaults() {
        let cfg = PairingConfig::default();
        assert_eq!(cfg.persist_path, ".weftos/runtime/paired_hosts.json");
        assert_eq!(cfg.default_window_secs, 30);
    }

    #[test]
    fn pairing_config_deserialize() {
        let json = r#"{"persist_path": "/opt/pairing.json", "default_window_secs": 60}"#;
        let cfg: PairingConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.persist_path, "/opt/pairing.json");
        assert_eq!(cfg.default_window_secs, 60);
    }

    #[test]
    fn kernel_config_with_profiles_and_pairing() {
        let json = r#"{"profiles": {"enabled": true}, "pairing": {"default_window_secs": 45}}"#;
        let cfg: KernelConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.profiles.is_some());
        assert!(cfg.profiles.unwrap().enabled);
        assert!(cfg.pairing.is_some());
        assert_eq!(cfg.pairing.unwrap().default_window_secs, 45);
    }

    #[test]
    fn vector_config_defaults() {
        let cfg = VectorConfig::default();
        assert_eq!(cfg.backend, VectorBackendKind::Hnsw);
        assert!(cfg.hnsw.is_none());
        assert!(cfg.diskann.is_none());
        assert!(cfg.hybrid.is_none());
        assert!(cfg.log_quantized.is_none());
        assert!(cfg.simd_distance.is_none());
    }

    #[test]
    fn vector_config_deserialize_hybrid() {
        let json =
            r#"{"backend": "hybrid", "hybrid": {"hot_capacity": 1000, "promotion_threshold": 5}}"#;
        let cfg: VectorConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.backend, VectorBackendKind::Hybrid);
        let h = cfg.hybrid.unwrap();
        assert_eq!(h.hot_capacity, 1000);
        assert_eq!(h.promotion_threshold, 5);
    }

    #[test]
    fn vector_config_deserialize_diskann() {
        let json = r#"{"backend": "diskann", "diskann": {"max_points": 5000000}}"#;
        let cfg: VectorConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.backend, VectorBackendKind::DiskAnn);
        let d = cfg.diskann.unwrap();
        assert_eq!(d.max_points, 5_000_000);
    }

    #[test]
    fn kernel_config_with_vector() {
        let json = r#"{"vector": {"backend": "hnsw"}}"#;
        let cfg: KernelConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.vector.is_some());
        assert_eq!(cfg.vector.unwrap().backend, VectorBackendKind::Hnsw);
    }

    #[test]
    fn spatial_config_defaults() {
        let cfg = SpatialConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.backend, SpatialBackendKind::Bvh);
        assert_eq!(cfg.max_leaves, 1_000_000);
        assert_eq!(cfg.phase_commit_threshold, 4096);
        assert_eq!(cfg.branch_retention, BranchRetentionPolicy::KeepLast(64));
    }

    #[test]
    fn kernel_config_with_spatial() {
        let json = r#"{"spatial": {"enabled": true, "backend": "bvh", "max_leaves": 1000}}"#;
        let cfg: KernelConfig = serde_json::from_str(json).unwrap();
        let s = cfg.spatial.unwrap();
        assert!(s.enabled);
        assert_eq!(s.backend, SpatialBackendKind::Bvh);
        assert_eq!(s.max_leaves, 1000);
    }

    #[test]
    fn log_quantized_stub_config_defaults() {
        let cfg = LogQuantizedStubConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.compression_ratio, 4);
    }

    #[test]
    fn log_quantized_stub_config_deserialize() {
        let json = r#"{"enabled": true, "compression_ratio": 8}"#;
        let cfg: LogQuantizedStubConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.compression_ratio, 8);
    }

    #[test]
    fn simd_distance_stub_config_defaults() {
        let cfg = SimdDistanceStubConfig::default();
        assert!(!cfg.enabled);
        assert!(!cfg.pad_to_power_of_two);
    }

    #[test]
    fn simd_distance_stub_config_deserialize() {
        let json = r#"{"enabled": true, "pad_to_power_of_two": true}"#;
        let cfg: SimdDistanceStubConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.enabled);
        assert!(cfg.pad_to_power_of_two);
    }

    #[test]
    fn vector_config_with_shaal_stubs() {
        let json = r#"{
            "backend": "hnsw",
            "log_quantized": {"enabled": true, "compression_ratio": 16},
            "simd_distance": {"enabled": true, "pad_to_power_of_two": true}
        }"#;
        let cfg: VectorConfig = serde_json::from_str(json).unwrap();
        let lq = cfg.log_quantized.unwrap();
        assert!(lq.enabled);
        assert_eq!(lq.compression_ratio, 16);
        let sd = cfg.simd_distance.unwrap();
        assert!(sd.enabled);
        assert!(sd.pad_to_power_of_two);
    }

    #[test]
    fn subagents_config_defaults_match_spawner() {
        // Must mirror clawft_service_agent::SubagentConfig::default().
        let cfg = SubagentsConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.max_per_conv, 5);
        assert_eq!(cfg.max_depth, 3);
        assert_eq!(cfg.timeout_secs, 120);
        assert!(!cfg.notify_on_complete);
        assert_eq!(cfg.timeout(), std::time::Duration::from_secs(120));
    }

    #[test]
    fn subagents_config_absent_block_uses_defaults() {
        // An agent block with no `subagents` key still yields the defaults.
        let json = r#"{"talk_loop": true}"#;
        let cfg: AgentAnchorConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.talk_loop);
        assert!(cfg.subagents.enabled);
        assert_eq!(cfg.subagents.max_depth, 3);
    }

    #[test]
    fn subagents_config_partial_block_fills_missing_fields() {
        // A partial block overrides only what it sets; the rest default.
        let json = r#"{"subagents": {"max_depth": 1, "enabled": false}}"#;
        let cfg: AgentAnchorConfig = serde_json::from_str(json).unwrap();
        assert!(!cfg.subagents.enabled);
        assert_eq!(cfg.subagents.max_depth, 1);
        assert_eq!(cfg.subagents.max_per_conv, 5); // untouched → default
        assert_eq!(cfg.subagents.timeout_secs, 120); // untouched → default
    }

    #[test]
    fn subagents_config_camel_case_aliases() {
        let json = r#"{"maxPerConv": 8, "maxDepth": 2, "timeoutSecs": 60, "notifyOnComplete": true}"#;
        let cfg: SubagentsConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.max_per_conv, 8);
        assert_eq!(cfg.max_depth, 2);
        assert_eq!(cfg.timeout_secs, 60);
        assert!(cfg.notify_on_complete);
    }

    #[test]
    fn classification_config_defaults_to_off() {
        // No block ⇒ disabled, default queue bound, no cost.
        let cfg = ClassificationConfig::default();
        assert_eq!(cfg.mode, ClassificationMode::Off);
        assert!(!cfg.is_enabled());
        assert!(cfg.model_override.is_none());
        assert_eq!(cfg.queue_bound, 256);
    }

    #[test]
    fn classification_config_absent_block_uses_defaults() {
        // An agent block with no `classification` key ⇒ mode = off.
        let json = r#"{"anchor_causal": true}"#;
        let cfg: AgentAnchorConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.anchor_causal);
        assert_eq!(cfg.classification.mode, ClassificationMode::Off);
        assert!(!cfg.classification.is_enabled());
        assert_eq!(cfg.classification.queue_bound, 256);
    }

    #[test]
    fn classification_config_partial_block_fills_missing_fields() {
        // A partial block overrides only what it sets; the rest default.
        let json = r#"{"classification": {"mode": "keyword"}}"#;
        let cfg: AgentAnchorConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.classification.mode, ClassificationMode::Keyword);
        assert!(cfg.classification.is_enabled());
        assert!(cfg.classification.model_override.is_none()); // untouched → default
        assert_eq!(cfg.classification.queue_bound, 256); // untouched → default
    }

    #[test]
    fn classification_config_full_mode_round_trip() {
        let json =
            r#"{"mode": "full", "model_override": "haiku-3.5", "queue_bound": 512}"#;
        let cfg: ClassificationConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.mode, ClassificationMode::Full);
        assert!(cfg.is_enabled());
        assert_eq!(cfg.model_override.as_deref(), Some("haiku-3.5"));
        assert_eq!(cfg.queue_bound, 512);
    }

    #[test]
    fn classification_config_camel_case_aliases() {
        let json = r#"{"mode": "full", "modelOverride": "haiku-3.5", "queueBound": 128}"#;
        let cfg: ClassificationConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.mode, ClassificationMode::Full);
        assert_eq!(cfg.model_override.as_deref(), Some("haiku-3.5"));
        assert_eq!(cfg.queue_bound, 128);
    }

    // ── WEFT-137 external chain-head anchor config ──────────────────

    #[test]
    fn chain_external_anchor_defaults() {
        let cfg = ChainExternalAnchorConfig::default();
        assert_eq!(cfg.backend, ChainAnchorBackend::File);
        assert_eq!(cfg.min_interval_secs, 300);
        assert_eq!(cfg.min_events_between, 100);
        assert!(cfg.ledger_path.is_none());
        assert!(cfg.endpoint.is_none());
    }

    #[test]
    fn chain_config_external_anchor_serde() {
        let json = r#"{
            "enabled": true,
            "external_anchor": {
                "backend": "file",
                "ledger_path": "/tmp/anchors.jsonl",
                "min_interval_secs": 60,
                "min_events_between": 10
            }
        }"#;
        let cfg: ChainConfig = serde_json::from_str(json).unwrap();
        let anchor = cfg.external_anchor.expect("present");
        assert_eq!(anchor.backend, ChainAnchorBackend::File);
        assert_eq!(anchor.ledger_path.as_deref(), Some("/tmp/anchors.jsonl"));
        assert_eq!(anchor.min_interval_secs, 60);
        assert_eq!(anchor.min_events_between, 10);
    }

    #[test]
    fn chain_external_anchor_camel_case_and_external_backend() {
        let json = r#"{
            "backend": "external",
            "ledgerPath": "/var/lib/weftos/intent.jsonl",
            "endpoint": "https://ledger.example/anchor",
            "minIntervalSecs": 0,
            "minEventsBetween": 0
        }"#;
        let cfg: ChainExternalAnchorConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.backend, ChainAnchorBackend::External);
        assert_eq!(
            cfg.ledger_path.as_deref(),
            Some("/var/lib/weftos/intent.jsonl")
        );
        assert_eq!(
            cfg.endpoint.as_deref(),
            Some("https://ledger.example/anchor")
        );
        assert_eq!(cfg.min_interval_secs, 0);
        assert_eq!(cfg.min_events_between, 0);
    }
}
