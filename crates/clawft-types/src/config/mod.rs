//! Configuration schema types.
//!
//! A faithful port of `nanobot/config/schema.py`. All structs support
//! both `snake_case` and `camelCase` field names in JSON via `#[serde(alias)]`.
//! Unknown fields are silently ignored for forward compatibility by default.
//! Opt-in top-level strict mode: [`Config::from_json_str_strict`] /
//! env `WEFT_CONFIG_DENY_UNKNOWN=1` (WEFT-20).
//!
//! # Module Structure
//!
//! - [`channels`] -- Chat channel configurations (Telegram, Slack, Discord, etc.)
//! - [`policies`] -- Security policy configurations (command execution, URL safety)

pub mod adaptive_silence;
pub mod chain_paths;
pub mod channels;
pub mod governance;
pub mod nested;
pub mod kernel;
pub mod local_llm;
pub mod overlay;
#[cfg(test)]
mod overlay_tests;
pub mod personality;
pub mod plugins;
pub mod policies;
pub mod skills;
pub mod voice;
pub mod voice_metrics;

// Re-export channel types at the config level for backward compatibility.
pub use adaptive_silence::{AdaptiveSilenceConfig, AdaptiveSilenceTimeout};
pub use channels::*;
pub use governance::*;
pub use kernel::*;
pub use local_llm::*;
pub use personality::*;
pub use plugins::*;
pub use policies::*;
pub use skills::*;
pub use voice::*;
pub use voice_metrics::*;

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::agent_routing::AgentRoutingConfig;
use crate::delegation::DelegationConfig;
use crate::routing::RoutingConfig;
use crate::secret::SecretString;

/// Shared default function: returns `true`.
pub(crate) fn default_true() -> bool {
    true
}

// ── Root config ──────────────────────────────────────────────────────────

/// Root configuration for the clawft framework.
///
/// Mirrors the Python `Config(BaseSettings)` class.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    /// Nested user-instance authority (ADR-103 D10).
    #[serde(default, skip_serializing_if = "nested::WeaveConfig::is_default")]
    pub weave: nested::WeaveConfig,
    /// Agent defaults and per-agent overrides.
    #[serde(default)]
    pub agents: AgentsConfig,

    /// Chat channel configurations (Telegram, Slack, Discord, etc.).
    #[serde(default)]
    pub channels: ChannelsConfig,

    /// LLM provider credentials and settings.
    #[serde(default)]
    pub providers: ProvidersConfig,

    /// Gateway / HTTP server settings.
    #[serde(default)]
    pub gateway: GatewayConfig,

    /// Tool configurations (web search, exec, MCP servers).
    #[serde(default)]
    pub tools: ToolsConfig,

    /// Task delegation routing configuration.
    #[serde(default)]
    pub delegation: DelegationConfig,

    /// Tiered routing and permission configuration.
    #[serde(default)]
    pub routing: RoutingConfig,

    /// Multi-agent / doctor routing table (WEFT-197).
    #[serde(default)]
    pub agent_routing: AgentRoutingConfig,

    /// Voice pipeline configuration (STT, TTS, VAD, wake word).
    #[serde(default)]
    pub voice: VoiceConfig,

    /// Kernel subsystem configuration (WeftOS).
    #[serde(default)]
    pub kernel: KernelConfig,

    /// Pipeline stage selection (scorer, learner backends).
    #[serde(default)]
    pub pipeline: PipelineConfig,

    /// Per-plugin runtime configuration including voice sub-permission
    /// grants (WEFT-556 / SC-10).
    #[serde(default)]
    pub plugins: PluginsConfig,

    /// Skill discovery and autonomous skill-generation settings (WEFT-67).
    #[serde(default)]
    pub skills: SkillsConfig,
}

/// Known top-level keys of [`Config`] (snake_case + camelCase aliases).
///
/// Used by WEFT-20 opt-in `deny_unknown` mode. Nested objects still ignore
/// unknown keys for forward compatibility.
pub const CONFIG_TOP_LEVEL_KEYS: &[&str] = &[
    "weave",
    "agents",
    "channels",
    "providers",
    "gateway",
    "tools",
    "delegation",
    "routing",
    "agent_routing",
    "agentRouting",
    "voice",
    "kernel",
    "pipeline",
    "plugins",
    "skills",
];

/// Whether to reject unknown **top-level** config keys (WEFT-20).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DenyUnknown {
    /// Silent ignore (default; forward-compat).
    #[default]
    No,
    /// Reject unknown top-level keys with [`ConfigParseError::UnknownField`].
    Yes,
}

/// Errors from [`Config::from_json_str`] / strict parse helpers.
#[derive(Debug, thiserror::Error)]
pub enum ConfigParseError {
    /// JSON syntax or type mismatch from serde.
    #[error("config JSON parse error: {0}")]
    Serde(#[from] serde_json::Error),
    /// Opt-in deny-unknown: stray top-level key (WEFT-20).
    #[error("unknown top-level config key `{0}` (set WEFT_CONFIG_DENY_UNKNOWN only when you want this check)")]
    UnknownField(String),
}

impl Config {
    /// Parse config JSON with default behaviour (unknown top-level keys ignored).
    pub fn from_json_str(s: &str) -> Result<Self, ConfigParseError> {
        Self::from_json_str_with(s, DenyUnknown::No)
    }

    /// Parse config JSON rejecting unknown top-level keys (WEFT-20).
    pub fn from_json_str_strict(s: &str) -> Result<Self, ConfigParseError> {
        Self::from_json_str_with(s, DenyUnknown::Yes)
    }

    /// Parse config JSON with explicit [`DenyUnknown`] policy.
    pub fn from_json_str_with(s: &str, deny: DenyUnknown) -> Result<Self, ConfigParseError> {
        let value: serde_json::Value = serde_json::from_str(s)?;
        if deny == DenyUnknown::Yes
            && let serde_json::Value::Object(map) = &value
        {
            for key in map.keys() {
                if !CONFIG_TOP_LEVEL_KEYS.contains(&key.as_str()) {
                    return Err(ConfigParseError::UnknownField(key.clone()));
                }
            }
        }
        Ok(serde_json::from_value(value)?)
    }

    /// True when env requests strict top-level key checking (WEFT-20).
    ///
    /// Honours `WEFT_CONFIG_DENY_UNKNOWN` or legacy `CLAWFT_CONFIG_DENY_UNKNOWN`.
    /// Values `1`, `true`, `yes` (case-insensitive) enable strict mode.
    pub fn deny_unknown_from_env() -> DenyUnknown {
        for var in ["WEFT_CONFIG_DENY_UNKNOWN", "CLAWFT_CONFIG_DENY_UNKNOWN"] {
            if let Ok(v) = std::env::var(var) {
                let t = v.trim();
                if t == "1" || t.eq_ignore_ascii_case("true") || t.eq_ignore_ascii_case("yes") {
                    return DenyUnknown::Yes;
                }
            }
        }
        DenyUnknown::No
    }
}

// ── Pipeline ────────────────────────────────────────────────────────────

/// Pipeline stage backend selection.
///
/// Allows selecting which scorer and learner implementations to use.
/// Defaults to `"noop"` for backward compatibility.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineConfig {
    /// Quality scorer backend: `"noop"` (default) or `"fitness"`.
    #[serde(default = "default_scorer")]
    pub scorer: String,

    /// Learning backend: `"noop"` (default) or `"trajectory"`.
    #[serde(default = "default_learner")]
    pub learner: String,
}

fn default_scorer() -> String {
    "noop".into()
}

fn default_learner() -> String {
    "noop".into()
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            scorer: default_scorer(),
            learner: default_learner(),
        }
    }
}

impl Config {
    /// Product display brand (WEFT-176 white-label token).
    ///
    /// Delegates to [`KernelConfig::brand`]; defaults to
    /// [`DEFAULT_BRAND`] (`"WeftOS"`).
    pub fn brand(&self) -> &str {
        self.kernel.brand()
    }

    /// Install this config's brand as the process-wide display token.
    ///
    /// Call after loading config so Discord identify, CLI help, and
    /// boot banners pick up a custom brand without threading `Config`
    /// through every call site.
    pub fn install_brand(&self) {
        install_brand(self.brand());
    }

    /// Get the expanded workspace path.
    ///
    /// On native targets (with the `native` feature), this expands `~/` prefixes
    /// using `dirs::home_dir()`. On WASM or when `native` is disabled, `~/`
    /// prefixes are left unexpanded.
    pub fn workspace_path(&self) -> PathBuf {
        let raw = &self.agents.defaults.workspace;
        #[cfg(feature = "native")]
        if let Some(rest) = raw.strip_prefix("~/")
            && let Some(home) = dirs::home_dir()
        {
            return home.join(rest);
        }
        PathBuf::from(raw)
    }

    /// Get the expanded workspace path with an explicit home directory.
    ///
    /// This is the browser-friendly variant that does not depend on `dirs`.
    /// Pass `None` for `home` to skip `~/` expansion.
    pub fn workspace_path_with_home(&self, home: Option<&std::path::Path>) -> PathBuf {
        let raw = &self.agents.defaults.workspace;
        if let Some(rest) = raw.strip_prefix("~/")
            && let Some(home) = home
        {
            return home.join(rest);
        }
        PathBuf::from(raw)
    }
}

// ── Agents ───────────────────────────────────────────────────────────────

/// Agent configuration container.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentsConfig {
    /// Default settings applied to all agents.
    #[serde(default)]
    pub defaults: AgentDefaults,

    /// Root directory for agent identity files and daemon workspace paths
    /// (WEFT-83 / MW-5).
    ///
    /// When set, the daemon loads `<workspace_root>/.clawft/{SOUL.md,IDENTITY.md}`
    /// and roots file tools / sandbox paths there instead of the process CWD.
    /// When `None` (default), falls back to `std::env::current_dir()` for
    /// back-compat — same behaviour as pre-WEFT-83 daemons.
    ///
    /// Distinct from [`AgentDefaults::workspace`] (nanobot-style working
    /// directory for file tools inside the agent loop) and from the skills
    /// catalog root. Accepts absolute paths or `~/…` (expanded on native).
    ///
    /// TOML/JSON: `agents.workspace_root` / `agents.workspaceRoot`.
    #[serde(default, alias = "workspaceRoot")]
    pub workspace_root: Option<PathBuf>,

    /// Per-conversation cost circuit-breaker (WEFT-322).
    ///
    /// Caps the cumulative spend for a single `conv_id` so a confused
    /// agent loop on a permission prompt cannot burn the daily budget
    /// in one turn. The agent loop checks this BEFORE issuing each
    /// LLM call; on trip the conversation is marked `circuit_open` in
    /// the budget store and all subsequent calls fail-fast until
    /// reset via `agent.chat.reset_budget`.
    #[serde(default, alias = "costBudget")]
    pub cost_budget: CostBudgetConfig,

    /// Per-turn COW memory checkpointing (WEFT-616 Phase 2).
    ///
    /// When enabled, [`AgentLoop::handle_turn`](../../clawft_core/agent/loop_core/struct.AgentLoop.html#method.handle_turn)
    /// (clawft-core, `rvf` feature) checkpoints a `clawft-cow-memory`
    /// `BranchableMemory` before each turn, promotes it on success, and
    /// rolls it back on turn failure. Off by default: `enabled: false`
    /// means the loop's `cow_memory` handle stays absent and turn
    /// behavior is unchanged from before this option existed.
    #[serde(default, alias = "cowMemory")]
    pub cow_memory: CowMemoryConfig,

    /// Binding-thread integrity policy (WEFT-342 / agent-core-v1.1).
    ///
    /// Controls whether a missing compile-time `BINDING_THREAD_EXCERPT`
    /// in loaded `SOUL.md` hard-refuses the turn (`deny`, default) or
    /// only annotates the system prompt (`warn_only`, legacy v1).
    /// Evaluated every turn via `gate.check("soul.binding_thread_intact", …)`.
    ///
    /// TOML/JSON: `agents.binding_thread_mode` / `agents.bindingThreadMode`.
    #[serde(default, alias = "bindingThreadMode")]
    pub binding_thread_mode: BindingThreadMode,

    /// Retrieved long-term memory with citation feedback (WEFT-732, RMM
    /// retrospective). Off by default: the full `MEMORY.md` is injected as
    /// before. When enabled and a retriever is built in, the context gets
    /// at most `top_m` retrieved snippets tagged `[m1]`…, the model cites
    /// the ids it used, and cited / ignored ids reward the reranker (never
    /// the retriever). See `docs/research/rmm-reflective-memory-management.md`.
    ///
    /// TOML/JSON: `agents.memory_recall` / `agents.memoryRecall`.
    #[serde(default, alias = "memoryRecall")]
    pub memory_recall: MemoryRecallConfig,

    /// Distil finished conversation turns into long-term memory (WEFT-347) and, with
    /// `topic_merge`, organize them as topic nodes with merge-or-insert (WEFT-733, RMM
    /// prospective). Off by default; when on, the agent loop runs it after each turn on the
    /// conversation sink whenever its cadence is due.
    ///
    /// TOML/JSON: `agents.memory_consolidation` / `agents.memoryConsolidation`.
    #[serde(default, alias = "memoryConsolidation")]
    pub memory_consolidation: MemoryConsolidationConfig,
}

/// Memory consolidation settings (WEFT-347 / WEFT-733). See [`AgentsConfig::memory_consolidation`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MemoryConsolidationConfig {
    /// Run the consolidator from the agent loop. Default `false`.
    #[serde(default)]
    pub enabled: bool,
    /// Consolidate after this many new turns in a conversation.
    #[serde(default = "default_consolidate_every_k", alias = "everyKTurns")]
    pub every_k_turns: usize,
    /// Merge facts into topic nodes instead of appending blocks (WEFT-733). Default `true`.
    #[serde(default = "default_true_bool", alias = "topicMerge")]
    pub topic_merge: bool,
}

fn default_consolidate_every_k() -> usize {
    10
}
fn default_true_bool() -> bool {
    true
}

impl Default for MemoryConsolidationConfig {
    fn default() -> Self {
        Self { enabled: false, every_k_turns: default_consolidate_every_k(), topic_merge: true }
    }
}

/// Retrieved-memory settings (WEFT-732). See [`AgentsConfig::memory_recall`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MemoryRecallConfig {
    /// Turn retrieved memory on. Default `false` (full `MEMORY.md` dump).
    #[serde(default)]
    pub enabled: bool,
    /// Candidates the frozen retriever returns (paper default 20).
    #[serde(default = "default_recall_top_k", alias = "topK")]
    pub top_k: usize,
    /// Snippets injected after rerank (paper default 5).
    #[serde(default = "default_recall_top_m", alias = "topM")]
    pub top_m: usize,
    /// Directory of the embedding model bundle (nomic-embed-text-v1.5:
    /// `onnx/model.onnx` + `tokenizer.json`). Unset = the documented search path
    /// (`$WEFTOS_EMBED_MODEL_DIR`, `.weftos/models/nomic-embed-text-v1.5`,
    /// `~/.weftos/models/nomic-embed-text-v1.5`). If no model is found, recall
    /// is OFF (the full `MEMORY.md` is injected) and an error is logged; it never
    /// falls back to hashed retrieval.
    #[serde(default, alias = "modelDir")]
    pub model_dir: Option<String>,
    /// Opt in to LEXICAL retrieval (shared words only, not semantic). Default
    /// `false`. Also enabled process-wide by `WEFTOS_EMBEDDING_MODE=lexical`.
    #[serde(default)]
    pub lexical: bool,
}

fn default_recall_top_k() -> usize {
    20
}
fn default_recall_top_m() -> usize {
    5
}

impl Default for MemoryRecallConfig {
    fn default() -> Self {
        Self { enabled: false, top_k: default_recall_top_k(), top_m: default_recall_top_m(), model_dir: None, lexical: false }
    }
}

/// Policy for binding-thread integrity checks (WEFT-342).
///
/// - [`Deny`](Self::Deny) — default; mismatch → hard refuse the turn
///   (`GateDecision::Deny { reason: "binding-thread mismatch" }`).
/// - [`WarnOnly`](Self::WarnOnly) — legacy v1; annotate prompt + `warn!`
///   log, agent continues in degraded mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BindingThreadMode {
    /// Hard refuse on mismatch (v1.1 default).
    #[default]
    Deny,
    /// Annotate prompt + warn log only (legacy).
    WarnOnly,
}

impl BindingThreadMode {
    /// Stable string label for logs and gate context.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Deny => "deny",
            Self::WarnOnly => "warn_only",
        }
    }

    /// `true` when mismatch must abort the turn.
    pub const fn is_deny(self) -> bool {
        matches!(self, Self::Deny)
    }
}

impl AgentsConfig {
    /// Resolve the agent workspace root for identity loading and daemon
    /// workspace paths (WEFT-83).
    ///
    /// * If [`Self::workspace_root`] is set, return it with optional `~/`
    ///   expansion (native feature).
    /// * Otherwise return `std::env::current_dir()`.
    ///
    /// Callers that already hold a preferred CWD override can pass it via
    /// [`Self::resolve_workspace_root_or`].
    pub fn resolve_workspace_root(&self) -> std::io::Result<PathBuf> {
        self.resolve_workspace_root_or(None)
    }

    /// Like [`Self::resolve_workspace_root`], but uses `fallback` instead of
    /// `current_dir()` when the config key is unset. Useful in tests and when
    /// the daemon has already resolved CWD.
    pub fn resolve_workspace_root_or(
        &self,
        fallback: Option<PathBuf>,
    ) -> std::io::Result<PathBuf> {
        if let Some(raw) = self.workspace_root.as_ref() {
            return Ok(expand_agent_workspace_root(raw));
        }
        match fallback {
            Some(p) => Ok(p),
            None => std::env::current_dir(),
        }
    }
}

/// Expand `~/` on a configured workspace_root path (native only).
fn expand_agent_workspace_root(raw: &std::path::Path) -> PathBuf {
    #[cfg(feature = "native")]
    let s = raw.to_string_lossy();
    #[cfg(feature = "native")]
    if let Some(rest) = s.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    // Also handle bare "~"
    #[cfg(feature = "native")]
    if s == "~"
        && let Some(home) = dirs::home_dir()
    {
        return home;
    }
    raw.to_path_buf()
}

/// Per-turn COW memory checkpointing config (WEFT-616 Phase 2). See
/// [`AgentsConfig::cow_memory`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CowMemoryConfig {
    /// Whether per-turn COW memory checkpointing is enabled. Default
    /// `false` — zero behavior change until an operator opts in.
    #[serde(default)]
    pub enabled: bool,

    /// Directory for the `BranchableMemory` lineage's `.rvf` files.
    /// Only consulted when `enabled` is `true`.
    #[serde(default = "default_cow_memory_path")]
    pub path: String,

    /// Whether a turn's user/assistant exchange is embedded and ingested
    /// into the checkpointed `working` node before `promote`/`rollback`
    /// (WEFT-616 Phase 3 write-routing). Default `true` — the checkpoint
    /// bracket exists to protect real writes, and ingesting the turn's own
    /// exchange is the first (and currently only) source of those writes.
    /// Only consulted when `enabled` is also `true`; set `false` to keep
    /// the bracket active (still protecting whatever tools/graphify write
    /// directly into the lineage) without the automatic exchange ingest.
    #[serde(default = "default_ingest_turns", alias = "ingestTurns")]
    pub ingest_turns: bool,

    /// Checkpoint cadence (WEFT-652, cubecow event-level snapshots).
    /// `turn` (default): one checkpoint per turn — the Phase-2 bracket.
    /// `tool`: additionally checkpoint at every tool-call boundary inside
    /// the turn (each loop iteration), so any mid-turn point is a rollback
    /// target; subagent spawns are tool calls, so this covers spawn
    /// boundaries too. Mid-turn checkpoints collapse at the turn's promote.
    /// ~8ms per checkpoint (fsync-dominated) — trivial next to an LLM call.
    #[serde(default)]
    pub cadence: CowCadence,
}

/// See [`CowMemoryConfig::cadence`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CowCadence {
    /// One checkpoint per turn (Phase 2 behavior).
    #[default]
    Turn,
    /// Turn checkpoint + one per tool-call boundary (cubecow event-level).
    Tool,
}

fn default_cow_memory_path() -> String {
    "~/.clawft/workspace/cow_memory".into()
}

fn default_ingest_turns() -> bool {
    true
}

impl Default for CowMemoryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            path: default_cow_memory_path(),
            ingest_turns: default_ingest_turns(),
            cadence: CowCadence::default(),
        }
    }
}

/// Per-conversation cost circuit-breaker config (WEFT-322).
///
/// The agent loop tracks cumulative tokens, USD spend, and iteration
/// count for each `conv_id`. When any cap is exceeded the conversation
/// is marked `circuit_open` and subsequent `agent.chat` calls return
/// [`ClawftError::ConversationBudgetExceeded`](crate::error::ClawftError::ConversationBudgetExceeded)
/// without invoking the LLM. The state survives daemon restarts via
/// the substrate-backed budget store at
/// `derived/chat/<conv_id>/budget.json`.
///
/// Defaults are sized for free-tier OpenRouter use:
/// 200 000 tokens, $1.00, 30 iterations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostBudgetConfig {
    /// Max cumulative input+output tokens per conversation. Default `200_000`.
    #[serde(default = "default_max_tokens_per_conv", alias = "maxTokensPerConv")]
    pub max_tokens_per_conv: u64,

    /// Max cumulative USD spend per conversation. Default `1.00`.
    #[serde(default = "default_max_usd_per_conv", alias = "maxUsdPerConv")]
    pub max_usd_per_conv: f64,

    /// Max cumulative LLM iterations (round-trips) per conversation.
    /// Default `30`. This counts every `pipeline.complete` call inside
    /// `run_tool_loop`, summed across every `handle_turn` for the conv.
    #[serde(
        default = "default_max_iterations_per_conv",
        alias = "maxIterationsPerConv"
    )]
    pub max_iterations_per_conv: u32,
}

fn default_max_tokens_per_conv() -> u64 {
    200_000
}
fn default_max_usd_per_conv() -> f64 {
    1.00
}
fn default_max_iterations_per_conv() -> u32 {
    30
}

impl Default for CostBudgetConfig {
    fn default() -> Self {
        Self {
            max_tokens_per_conv: default_max_tokens_per_conv(),
            max_usd_per_conv: default_max_usd_per_conv(),
            max_iterations_per_conv: default_max_iterations_per_conv(),
        }
    }
}

/// Default agent settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDefaults {
    /// Working directory for agent file operations.
    #[serde(default = "default_workspace")]
    pub workspace: String,

    /// Default LLM model identifier.
    #[serde(default = "default_model")]
    pub model: String,

    /// Maximum tokens in a single LLM response.
    #[serde(default = "default_max_tokens", alias = "maxTokens")]
    pub max_tokens: i32,

    /// Sampling temperature.
    #[serde(default = "default_temperature")]
    pub temperature: f64,

    /// Maximum tool-use iterations per turn.
    #[serde(default = "default_max_tool_iterations", alias = "maxToolIterations")]
    pub max_tool_iterations: i32,

    /// Number of recent messages to include in context.
    #[serde(default = "default_memory_window", alias = "memoryWindow")]
    pub memory_window: i32,
}

fn default_workspace() -> String {
    "~/.nanobot/workspace".into()
}
fn default_model() -> String {
    // WEFT-604 / ADR-060: local Hermes is the zero-config default so
    // `weft agent` reaches a freshly-served :8090 endpoint without a
    // cloud API key. Operators who want cloud set agents.defaults.model
    // (or OPENROUTER_API_KEY for the daemon OpenRouter takeover).
    DEFAULT_LOCAL_LLM_MODEL_ROUTED.into()
}
fn default_max_tokens() -> i32 {
    8192
}
fn default_temperature() -> f64 {
    0.7
}
fn default_max_tool_iterations() -> i32 {
    20
}
fn default_memory_window() -> i32 {
    50
}

impl Default for AgentDefaults {
    fn default() -> Self {
        Self {
            workspace: default_workspace(),
            model: default_model(),
            max_tokens: default_max_tokens(),
            temperature: default_temperature(),
            max_tool_iterations: default_max_tool_iterations(),
            memory_window: default_memory_window(),
        }
    }
}

// ── Providers ────────────────────────────────────────────────────────────

/// LLM provider credentials.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProviderConfig {
    /// API key for authentication.
    #[serde(default, alias = "apiKey")]
    pub api_key: SecretString,

    /// Base URL override (e.g. for proxies).
    #[serde(default, alias = "apiBase", alias = "baseUrl")]
    pub api_base: Option<String>,

    /// Custom HTTP headers (e.g. `APP-Code` for AiHubMix).
    #[serde(default, alias = "extraHeaders")]
    pub extra_headers: Option<HashMap<String, String>>,

    /// Whether this provider supports direct browser access (no CORS proxy needed).
    #[serde(default, alias = "browserDirect")]
    pub browser_direct: bool,

    /// CORS proxy URL for browser-mode API calls (e.g. "https://proxy.example.com").
    #[serde(default, alias = "corsProxy")]
    pub cors_proxy: Option<String>,
}

/// Default browser provider-routing fallback order (WEFT-404).
///
/// Used when a model string has no matching builtin `model_prefix`.
/// Preserves the historical hard-coded chain for back-compat.
pub fn default_provider_fallback_order() -> Vec<String> {
    vec![
        "openrouter".into(),
        "openai".into(),
        "anthropic".into(),
        "groq".into(),
        "deepseek".into(),
        "gemini".into(),
        "xai".into(),
    ]
}

/// Configuration for all LLM providers.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProvidersConfig {
    /// Custom OpenAI-compatible endpoint.
    #[serde(default)]
    pub custom: ProviderConfig,

    /// Anthropic.
    #[serde(default)]
    pub anthropic: ProviderConfig,

    /// OpenAI.
    #[serde(default)]
    pub openai: ProviderConfig,

    /// OpenRouter gateway.
    #[serde(default)]
    pub openrouter: ProviderConfig,

    /// DeepSeek.
    #[serde(default)]
    pub deepseek: ProviderConfig,

    /// Groq.
    #[serde(default)]
    pub groq: ProviderConfig,

    /// Zhipu AI.
    #[serde(default)]
    pub zhipu: ProviderConfig,

    /// DashScope (Alibaba Cloud Qwen).
    #[serde(default)]
    pub dashscope: ProviderConfig,

    /// vLLM / local server.
    #[serde(default)]
    pub vllm: ProviderConfig,

    /// Google Gemini.
    #[serde(default)]
    pub gemini: ProviderConfig,

    /// Moonshot (Kimi).
    #[serde(default)]
    pub moonshot: ProviderConfig,

    /// MiniMax.
    #[serde(default)]
    pub minimax: ProviderConfig,

    /// AiHubMix gateway.
    #[serde(default)]
    pub aihubmix: ProviderConfig,

    /// OpenAI Codex (OAuth-based).
    #[serde(default)]
    pub openai_codex: ProviderConfig,

    /// xAI (Grok).
    #[serde(default)]
    pub xai: ProviderConfig,

    /// ElevenLabs (TTS).
    #[serde(default)]
    pub elevenlabs: ProviderConfig,

    /// Keyless local OpenAI-compat endpoint (`local/` model prefix).
    ///
    /// WEFT-604: honoured by `apply_config_overrides` so
    /// `[providers.local] api_base = "…"` is no longer silently dropped.
    #[serde(default)]
    pub local: ProviderConfig,

    /// Ollama OpenAI-compat endpoint (`ollama/` model prefix).
    ///
    /// WEFT-604: same as [`Self::local`] — overrides must apply or fail loud.
    #[serde(default)]
    pub ollama: ProviderConfig,

    /// Ordered list of provider names tried when a model has no matching
    /// builtin prefix (browser `resolve_provider` fallback, WEFT-404).
    ///
    /// Default: `openrouter` → `openai` → `anthropic` → `groq` →
    /// `deepseek` → `gemini` → `xai`. Empty / missing values resolve to
    /// that default via [`Self::effective_provider_fallback_order`].
    #[serde(
        default = "default_provider_fallback_order",
        alias = "providerFallbackOrder"
    )]
    pub provider_fallback_order: Vec<String>,
}

impl ProvidersConfig {
    /// Effective provider-routing fallback order (WEFT-404).
    ///
    /// Returns the configured list when non-empty; otherwise the
    /// historical default from [`default_provider_fallback_order`].
    pub fn effective_provider_fallback_order(&self) -> Vec<String> {
        if self.provider_fallback_order.is_empty() {
            default_provider_fallback_order()
        } else {
            self.provider_fallback_order.clone()
        }
    }

    /// Look up a named provider entry (borrowed).
    ///
    /// Unknown names fall through to [`Self::custom`] (same behaviour as
    /// the browser `user_provider_config` helper).
    pub fn provider_config(&self, name: &str) -> &ProviderConfig {
        match name {
            "anthropic" => &self.anthropic,
            "openai" => &self.openai,
            "openrouter" => &self.openrouter,
            "deepseek" => &self.deepseek,
            "groq" => &self.groq,
            "zhipu" => &self.zhipu,
            "dashscope" => &self.dashscope,
            "vllm" => &self.vllm,
            "gemini" => &self.gemini,
            "moonshot" => &self.moonshot,
            "minimax" => &self.minimax,
            "aihubmix" => &self.aihubmix,
            "openai_codex" | "openai-codex" => &self.openai_codex,
            "xai" => &self.xai,
            "elevenlabs" => &self.elevenlabs,
            "local" => &self.local,
            "ollama" => &self.ollama,
            _ => &self.custom,
        }
    }

    /// First provider name in `order` that has a non-empty API key.
    ///
    /// Used by browser provider routing when the model string has no
    /// matching prefix (WEFT-404).
    pub fn first_in_order_with_api_key<'a>(&'a self, order: &'a [String]) -> Option<&'a str> {
        order
            .iter()
            .find(|name| !self.provider_config(name).api_key.is_empty())
            .map(|s| s.as_str())
    }

    /// First provider in the effective fallback order with a non-empty API key.
    pub fn first_fallback_with_api_key(&self) -> Option<String> {
        let order = self.effective_provider_fallback_order();
        // Re-borrow from owned order: return owned name.
        order
            .into_iter()
            .find(|name| !self.provider_config(name).api_key.is_empty())
    }
}

// ── Gateway ──────────────────────────────────────────────────────────────

/// Gateway / HTTP server configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayConfig {
    /// Bind address.
    #[serde(default = "default_gateway_host")]
    pub host: String,

    /// Listen port.
    #[serde(default = "default_gateway_port")]
    pub port: u16,

    /// Heartbeat interval in minutes (0 = disabled).
    #[serde(default, alias = "heartbeatIntervalMinutes")]
    pub heartbeat_interval_minutes: u64,

    /// Heartbeat prompt text.
    #[serde(default = "default_heartbeat_prompt", alias = "heartbeatPrompt")]
    pub heartbeat_prompt: String,

    /// Port for the UI REST API (separate from gateway port).
    #[serde(default = "default_api_port", alias = "apiPort")]
    pub api_port: u16,

    /// Allowed CORS origins for the UI API.
    #[serde(default = "default_cors_origins", alias = "corsOrigins")]
    pub cors_origins: Vec<String>,

    /// Whether the REST/WS API is enabled.
    #[serde(default, alias = "apiEnabled")]
    pub api_enabled: bool,

    /// Allow the API on a non-loopback address over plain HTTP (ADR-102).
    ///
    /// The gateway has no TLS of its own. A bearer token grants the full
    /// API, so it is refused on a non-loopback bind unless TLS is
    /// terminated in front (reverse proxy) and this is set, or
    /// `--dangerously-plain-http` is passed.
    #[serde(default, alias = "dangerouslyPlainHttp")]
    pub dangerously_plain_http: bool,

    /// Directory holding the built cog-manager web console. When set, the
    /// gateway serves it under `/console/` (never at the root). `weft
    /// gateway --static-dir` overrides it.
    #[serde(default, alias = "staticDir")]
    pub static_dir: Option<String>,

    /// Extra origins the console page may `connect-src` to, on top of
    /// `'self'` (for example the project's Cog Host and Seeds). Each entry
    /// is a bare origin such as `http://192.168.1.50:8081`.
    #[serde(default, alias = "consoleConnectSrc")]
    pub console_connect_src: Vec<String>,

    /// Tailnet-identity token minting for the console (`POST
    /// /api/console/token`). Disabled by default.
    #[serde(default, alias = "tailnetIdentity")]
    pub tailnet_identity: TailnetIdentityConfig,
}

/// `gateway.tailnetIdentity`: mint short-lived read-only project tokens for
/// callers that Tailscale identifies as an allowlisted login.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TailnetIdentityConfig {
    /// Master switch. Off unless set.
    #[serde(default)]
    pub enabled: bool,

    /// Tailscale login names (`user@domain`) allowed to mint. Empty means
    /// nobody, even when `enabled`.
    #[serde(default, alias = "allowedLogins")]
    pub allowed_logins: Vec<String>,

    /// Lifetime of a minted token, seconds. Capped at the token
    /// authority's maximum.
    #[serde(default = "default_tailnet_ttl_secs", alias = "ttlSecs")]
    pub ttl_secs: u64,

    /// Path of the `tailscale` binary used for `whois`. Default: `tailscale`
    /// from `PATH`.
    #[serde(default, alias = "tailscaleBin")]
    pub tailscale_bin: Option<String>,
}

fn default_tailnet_ttl_secs() -> u64 {
    900
}

impl Default for TailnetIdentityConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            allowed_logins: Vec::new(),
            ttl_secs: default_tailnet_ttl_secs(),
            tailscale_bin: None,
        }
    }
}

fn default_gateway_host() -> String {
    // Loopback by default (ADR-102): LAN exposure must be an explicit choice.
    "127.0.0.1".into()
}
fn default_gateway_port() -> u16 {
    18790
}
fn default_heartbeat_prompt() -> String {
    "heartbeat".into()
}
fn default_api_port() -> u16 {
    18789
}
fn default_cors_origins() -> Vec<String> {
    vec!["http://localhost:5173".into()]
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            host: default_gateway_host(),
            port: default_gateway_port(),
            heartbeat_interval_minutes: 0,
            heartbeat_prompt: default_heartbeat_prompt(),
            api_port: default_api_port(),
            cors_origins: default_cors_origins(),
            api_enabled: false,
            dangerously_plain_http: false,
            static_dir: None,
            console_connect_src: Vec::new(),
            tailnet_identity: TailnetIdentityConfig::default(),
        }
    }
}

// ── Tools ────────────────────────────────────────────────────────────────

/// Tools configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ToolsConfig {
    /// Web tools (search, etc.).
    #[serde(default)]
    pub web: WebToolsConfig,

    /// Shell exec tool settings.
    #[serde(default, rename = "exec")]
    pub exec_tool: ExecToolConfig,

    /// Whether to restrict all tool access to the workspace directory.
    #[serde(default, alias = "restrictToWorkspace")]
    pub restrict_to_workspace: bool,

    /// MCP server connections.
    #[serde(default, alias = "mcpServers")]
    pub mcp_servers: HashMap<String, MCPServerConfig>,

    /// Command execution policy (allowlist/denylist).
    #[serde(default, alias = "commandPolicy")]
    pub command_policy: CommandPolicyConfig,

    /// URL safety policy (SSRF protection).
    #[serde(default, alias = "urlPolicy")]
    pub url_policy: UrlPolicyConfig,

    /// Tools allowed by `weft mcp-server` over the wire.
    ///
    /// Each entry is a glob pattern (`*`, `?`). When the list is empty
    /// (default), all tools registered in the daemon are exposed —
    /// preserves prior behavior for upgrades. When non-empty, only
    /// matching tools are visible to MCP clients and other tools are
    /// rejected with `PermissionDenied` before execution.
    ///
    /// Used by [`PermissionFilter::from_patterns`] in
    /// `crates/clawft-services/src/mcp/middleware.rs` (WEFT-189).
    #[serde(default, alias = "allowedTools")]
    pub allowed_tools: Vec<String>,
}

/// Web tools configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WebToolsConfig {
    /// Search engine settings.
    #[serde(default)]
    pub search: WebSearchConfig,
}

/// Web search tool configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebSearchConfig {
    /// Search API key (e.g. Brave Search).
    #[serde(default, alias = "apiKey")]
    pub api_key: SecretString,

    /// Maximum number of search results.
    #[serde(default = "default_max_results", alias = "maxResults")]
    pub max_results: u32,
}

fn default_max_results() -> u32 {
    5
}

impl Default for WebSearchConfig {
    fn default() -> Self {
        Self {
            api_key: SecretString::default(),
            max_results: default_max_results(),
        }
    }
}

/// Shell exec tool configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecToolConfig {
    /// Command timeout in seconds.
    #[serde(default = "default_exec_timeout")]
    pub timeout: u32,
}

fn default_exec_timeout() -> u32 {
    60
}

impl Default for ExecToolConfig {
    fn default() -> Self {
        Self {
            timeout: default_exec_timeout(),
        }
    }
}

/// MCP server connection configuration (stdio or HTTP).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MCPServerConfig {
    /// Command to run (for stdio transport, e.g. `"npx"`).
    #[serde(default)]
    pub command: String,

    /// Command arguments (for stdio transport).
    #[serde(default)]
    pub args: Vec<String>,

    /// Extra environment variables (for stdio transport).
    #[serde(default)]
    pub env: HashMap<String, String>,

    /// Streamable HTTP endpoint URL (for HTTP transport).
    #[serde(default)]
    pub url: String,

    /// If true, MCP session is created but tools are NOT registered in ToolRegistry.
    /// Infrastructure servers (claude-flow, claude-code) should be internal.
    #[serde(default = "default_true", alias = "internalOnly")]
    pub internal_only: bool,
}

impl Default for MCPServerConfig {
    fn default() -> Self {
        Self {
            command: String::new(),
            args: Vec::new(),
            env: HashMap::new(),
            url: String::new(),
            internal_only: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Path to the test fixture config.
    const FIXTURE_PATH: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/config.json"
    );

    fn load_fixture() -> Config {
        let content =
            std::fs::read_to_string(FIXTURE_PATH).expect("fixture config.json should exist");
        serde_json::from_str(&content).expect("fixture should deserialize")
    }

    #[test]
    fn deserialize_fixture() {
        let cfg = load_fixture();

        // Agents
        assert_eq!(
            cfg.agents.defaults.model,
            DEFAULT_LOCAL_LLM_MODEL_ROUTED
        );
        assert_eq!(cfg.agents.defaults.max_tokens, 8192);
        assert_eq!(cfg.agents.defaults.temperature, 0.7);
        assert_eq!(cfg.agents.defaults.max_tool_iterations, 20);
        assert_eq!(cfg.agents.defaults.memory_window, 50);

        // Channels
        assert!(cfg.channels.telegram.enabled);
        assert_eq!(cfg.channels.telegram.token.expose(), "test-bot-token-123");
        assert_eq!(cfg.channels.telegram.allow_from, vec!["user1", "user2"]);
        assert!(!cfg.channels.slack.enabled);
        assert!(!cfg.channels.discord.enabled);

        // Providers
        assert_eq!(cfg.providers.anthropic.api_key.expose(), "sk-ant-test-key");
        assert_eq!(cfg.providers.openrouter.api_key.expose(), "sk-or-test-key");
        assert_eq!(
            cfg.providers.openrouter.api_base.as_deref(),
            Some("https://openrouter.ai/api/v1")
        );
        assert!(cfg.providers.deepseek.api_key.is_empty());

        // Gateway
        assert_eq!(cfg.gateway.host, "0.0.0.0");
        assert_eq!(cfg.gateway.port, 18790);

        // Tools
        assert_eq!(cfg.tools.web.search.max_results, 5);
        assert_eq!(cfg.tools.exec_tool.timeout, 60);
        assert!(!cfg.tools.restrict_to_workspace);
        assert!(cfg.tools.mcp_servers.contains_key("test-server"));
        let mcp = &cfg.tools.mcp_servers["test-server"];
        assert_eq!(mcp.command, "npx");
        assert_eq!(mcp.args, vec!["-y", "test-mcp-server"]);
    }

    #[test]
    fn camel_case_aliases() {
        // The fixture uses camelCase (maxTokens, allowFrom, etc.)
        // This test is essentially the same as deserialize_fixture
        // but focuses on alias correctness.
        let cfg = load_fixture();
        assert_eq!(cfg.agents.defaults.max_tokens, 8192); // maxTokens
        assert_eq!(cfg.agents.defaults.max_tool_iterations, 20); // maxToolIterations
        assert_eq!(cfg.agents.defaults.memory_window, 50); // memoryWindow
        assert_eq!(cfg.channels.telegram.allow_from, vec!["user1", "user2"]); // allowFrom
    }

    #[test]
    fn default_values_for_missing_fields() {
        let json = r#"{}"#;
        let cfg: Config = serde_json::from_str(json).unwrap();

        // Agent defaults
        assert_eq!(cfg.agents.defaults.workspace, "~/.nanobot/workspace");
        assert_eq!(
            cfg.agents.defaults.model,
            DEFAULT_LOCAL_LLM_MODEL_ROUTED
        );
        assert_eq!(cfg.agents.defaults.max_tokens, 8192);
        assert!((cfg.agents.defaults.temperature - 0.7).abs() < f64::EPSILON);
        assert_eq!(cfg.agents.defaults.max_tool_iterations, 20);
        assert_eq!(cfg.agents.defaults.memory_window, 50);

        // Channel defaults
        assert!(!cfg.channels.telegram.enabled);
        assert!(cfg.channels.telegram.token.is_empty());
        assert!(!cfg.channels.slack.enabled);
        assert_eq!(cfg.channels.slack.mode, "socket");
        assert!(!cfg.channels.discord.enabled);
        assert_eq!(cfg.channels.discord.intents, 37377);

        // Gateway defaults
        assert_eq!(cfg.gateway.host, "127.0.0.1");
        assert_eq!(cfg.gateway.port, 18790);

        // Tool defaults
        assert_eq!(cfg.tools.exec_tool.timeout, 60);
        assert_eq!(cfg.tools.web.search.max_results, 5);

        // WEFT-176 brand default
        assert_eq!(cfg.brand(), DEFAULT_BRAND);
        assert_eq!(cfg.kernel.brand(), "WeftOS");
    }

    #[test]
    fn config_brand_installs_process_token() {
        let mut cfg = Config::default();
        cfg.kernel.brand = "Valtech Agentic Mesh".into();
        assert_eq!(cfg.brand(), "Valtech Agentic Mesh");
        cfg.install_brand();
        assert_eq!(brand(), "Valtech Agentic Mesh");
        reset_brand_for_test();
        assert_eq!(brand(), DEFAULT_BRAND);
    }

    #[test]
    fn serde_roundtrip() {
        let cfg = load_fixture();
        let json = serde_json::to_string(&cfg).unwrap();
        let restored: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.agents.defaults.model, cfg.agents.defaults.model);
        assert_eq!(restored.gateway.port, cfg.gateway.port);
        // SecretString serializes to "" for security, so after round-trip
        // the restored api_key is empty (by design).
        assert!(restored.providers.anthropic.api_key.is_empty());
    }

    #[test]
    fn unknown_fields_ignored() {
        let json = r#"{
            "agents": { "defaults": { "model": "test" } },
            "unknown_top_level": true,
            "channels": {
                "telegram": { "enabled": false, "some_future_field": 42 }
            },
            "providers": {
                "anthropic": { "apiKey": "k", "newField": "x" }
            }
        }"#;
        let cfg: Config = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.agents.defaults.model, "test");
        assert!(!cfg.channels.telegram.enabled);
        assert_eq!(cfg.providers.anthropic.api_key.expose(), "k");
        // WEFT-20 default path: helper also ignores.
        let cfg2 = Config::from_json_str(json).unwrap();
        assert_eq!(cfg2.agents.defaults.model, "test");
    }

    /// WEFT-20: strict mode rejects unknown top-level keys.
    #[test]
    fn unknown_fields_rejected_when_deny_unknown() {
        let json = r#"{
            "agents": { "defaults": { "model": "test" } },
            "unknown_top_level": true
        }"#;
        let err = Config::from_json_str_strict(json).unwrap_err();
        match err {
            ConfigParseError::UnknownField(k) => assert_eq!(k, "unknown_top_level"),
            other => panic!("expected UnknownField, got {other:?}"),
        }
        // Known keys still parse under strict mode.
        let ok = Config::from_json_str_strict(
            r#"{ "agents": { "defaults": { "model": "m" } } }"#,
        )
        .unwrap();
        assert_eq!(ok.agents.defaults.model, "m");
    }

    #[test]
    fn unknown_channel_plugins_in_extra() {
        let json = r#"{
            "channels": {
                "telegram": { "enabled": true },
                "my_custom_channel": { "url": "wss://custom.io" }
            }
        }"#;
        let cfg: Config = serde_json::from_str(json).unwrap();
        assert!(cfg.channels.telegram.enabled);
        assert!(cfg.channels.extra.contains_key("my_custom_channel"));
    }

    #[test]
    fn workspace_path_expansion() {
        let mut cfg = Config::default();
        cfg.agents.defaults.workspace = "~/.clawft/workspace".into();
        let path = cfg.workspace_path();
        // Should not start with "~" after expansion
        assert!(!path.to_string_lossy().starts_with('~'));
    }

    #[test]
    fn skills_autogen_defaults_disabled() {
        let cfg = Config::default();
        assert!(!cfg.skills.autogen.enabled);
        assert_eq!(cfg.skills.autogen.threshold, 3);
        assert_eq!(cfg.skills.autogen.max_pending, 10);
        // WEFT-348: promotion defaults on with threshold 10.
        assert!(cfg.skills.promotion.enabled);
        assert_eq!(cfg.skills.promotion.threshold, 10);
    }

    #[test]
    fn skills_autogen_deserializes_from_json() {
        let json = r#"{
            "skills": {
                "autogen": { "enabled": true, "threshold": 5, "max_pending": 8 }
            }
        }"#;
        let cfg: Config = serde_json::from_str(json).unwrap();
        assert!(cfg.skills.autogen.enabled);
        assert_eq!(cfg.skills.autogen.threshold, 5);
        assert_eq!(cfg.skills.autogen.max_pending, 8);
        assert!(cfg.skills.promotion.enabled);
        assert_eq!(cfg.skills.promotion.threshold, 10);
    }

    #[test]
    fn skills_promotion_deserializes_from_json() {
        let json = r#"{
            "skills": {
                "promotion": { "enabled": false, "threshold": 20 }
            }
        }"#;
        let cfg: Config = serde_json::from_str(json).unwrap();
        assert!(!cfg.skills.promotion.enabled);
        assert_eq!(cfg.skills.promotion.threshold, 20);
    }

    #[test]
    fn workspace_root_defaults_to_none() {
        let cfg = Config::default();
        assert!(cfg.agents.workspace_root.is_none());
        // Unset key → resolve uses the provided fallback (simulates CWD).
        let fallback = PathBuf::from("/tmp/daemon-cwd");
        let resolved = cfg
            .agents
            .resolve_workspace_root_or(Some(fallback.clone()))
            .unwrap();
        assert_eq!(resolved, fallback);
    }

    #[test]
    fn binding_thread_mode_defaults_to_deny() {
        // WEFT-342: v1.1 default is hard-refuse, not legacy warn-only.
        let cfg = Config::default();
        assert_eq!(cfg.agents.binding_thread_mode, BindingThreadMode::Deny);
        assert!(cfg.agents.binding_thread_mode.is_deny());

        let snake = r#"{ "agents": { "binding_thread_mode": "warn_only" } }"#;
        let cfg: Config = serde_json::from_str(snake).unwrap();
        assert_eq!(cfg.agents.binding_thread_mode, BindingThreadMode::WarnOnly);

        let camel = r#"{ "agents": { "bindingThreadMode": "deny" } }"#;
        let cfg: Config = serde_json::from_str(camel).unwrap();
        assert_eq!(cfg.agents.binding_thread_mode, BindingThreadMode::Deny);
    }

    #[test]
    fn workspace_root_deserialize_snake_and_camel() {
        let snake = r#"{ "agents": { "workspace_root": "/home/user/proj-a" } }"#;
        let cfg: Config = serde_json::from_str(snake).unwrap();
        assert_eq!(
            cfg.agents.workspace_root.as_deref(),
            Some(std::path::Path::new("/home/user/proj-a"))
        );

        let camel = r#"{ "agents": { "workspaceRoot": "/home/user/proj-b" } }"#;
        let cfg: Config = serde_json::from_str(camel).unwrap();
        assert_eq!(
            cfg.agents.workspace_root.as_deref(),
            Some(std::path::Path::new("/home/user/proj-b"))
        );

        let resolved = cfg
            .agents
            .resolve_workspace_root_or(Some(PathBuf::from("/should-not-use")))
            .unwrap();
        assert_eq!(resolved, PathBuf::from("/home/user/proj-b"));
    }

    #[test]
    fn workspace_root_prefers_config_over_fallback() {
        // WEFT-83: two configured workspaces resolve independently of CWD.
        let a = AgentsConfig {
            workspace_root: Some(PathBuf::from("/workspaces/alpha")),
            ..AgentsConfig::default()
        };
        let b = AgentsConfig {
            workspace_root: Some(PathBuf::from("/workspaces/beta")),
            ..AgentsConfig::default()
        };

        let cwd = PathBuf::from("/tmp");
        assert_eq!(
            a.resolve_workspace_root_or(Some(cwd.clone())).unwrap(),
            PathBuf::from("/workspaces/alpha")
        );
        assert_eq!(
            b.resolve_workspace_root_or(Some(cwd)).unwrap(),
            PathBuf::from("/workspaces/beta")
        );
    }

    #[test]
    fn provider_config_with_extra_headers() {
        let json = r#"{
            "apiKey": "test",
            "extraHeaders": { "X-Custom": "value" }
        }"#;
        let cfg: ProviderConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.api_key.expose(), "test");
        let headers = cfg.extra_headers.unwrap();
        assert_eq!(headers["X-Custom"], "value");
    }

    /// WEFT-404: default fallback order matches the historical hard-coded chain.
    #[test]
    fn provider_fallback_order_default_preserves_historical() {
        let expected = [
            "openrouter",
            "openai",
            "anthropic",
            "groq",
            "deepseek",
            "gemini",
            "xai",
        ];
        assert_eq!(default_provider_fallback_order(), expected);

        // Rust Default leaves the field empty; effective_* restores default.
        let cfg = ProvidersConfig::default();
        assert!(cfg.provider_fallback_order.is_empty());
        assert_eq!(cfg.effective_provider_fallback_order(), expected);

        // Missing key on partial deserialize → serde default fills the list.
        let json = r#"{ "openai": { "apiKey": "sk-test" } }"#;
        let cfg: ProvidersConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.provider_fallback_order, expected);
    }

    /// WEFT-404: custom order is data-driven (snake_case + camelCase).
    #[test]
    fn provider_fallback_order_custom_selects_first_with_key() {
        let snake = r#"{
            "provider_fallback_order": ["gemini", "openai", "anthropic"],
            "openai": { "apiKey": "sk-openai" },
            "anthropic": { "apiKey": "sk-ant" }
        }"#;
        let cfg: ProvidersConfig = serde_json::from_str(snake).unwrap();
        assert_eq!(
            cfg.provider_fallback_order,
            vec!["gemini", "openai", "anthropic"]
        );
        // gemini has no key → openai is first with a key.
        assert_eq!(
            cfg.first_fallback_with_api_key().as_deref(),
            Some("openai")
        );

        let camel = r#"{
            "providerFallbackOrder": ["xai", "groq", "openrouter"],
            "xai": { "apiKey": "xai-key" },
            "groq": { "apiKey": "groq-key" }
        }"#;
        let cfg: ProvidersConfig = serde_json::from_str(camel).unwrap();
        assert_eq!(cfg.provider_fallback_order, vec!["xai", "groq", "openrouter"]);
        assert_eq!(cfg.first_fallback_with_api_key().as_deref(), Some("xai"));

        // Nested under root Config.
        let root = r#"{
            "providers": {
                "providerFallbackOrder": ["anthropic", "openai"],
                "openai": { "apiKey": "sk-oai" }
            }
        }"#;
        let root_cfg: Config = serde_json::from_str(root).unwrap();
        assert_eq!(
            root_cfg.providers.effective_provider_fallback_order(),
            vec!["anthropic", "openai"]
        );
        // anthropic has no key → openai.
        assert_eq!(
            root_cfg
                .providers
                .first_fallback_with_api_key()
                .as_deref(),
            Some("openai")
        );
    }

    #[test]
    fn email_config_defaults() {
        let cfg = EmailConfig::default();
        assert_eq!(cfg.imap_port, 993);
        assert!(cfg.imap_use_ssl);
        assert_eq!(cfg.smtp_port, 587);
        assert!(cfg.smtp_use_tls);
        assert!(!cfg.smtp_use_ssl);
        assert!(cfg.auto_reply_enabled);
        assert_eq!(cfg.poll_interval_seconds, 30);
        assert!(cfg.mark_seen);
        assert_eq!(cfg.max_body_chars, 12000);
        assert_eq!(cfg.subject_prefix, "Re: ");
    }

    #[test]
    fn mochat_config_defaults() {
        let cfg = MochatConfig::default();
        assert_eq!(cfg.base_url, "https://mochat.io");
        assert_eq!(cfg.socket_path, "/socket.io");
        assert_eq!(cfg.socket_reconnect_delay_ms, 1000);
        assert_eq!(cfg.socket_max_reconnect_delay_ms, 10000);
        assert_eq!(cfg.socket_connect_timeout_ms, 10000);
        assert_eq!(cfg.refresh_interval_ms, 30000);
        assert_eq!(cfg.watch_timeout_ms, 25000);
        assert_eq!(cfg.watch_limit, 100);
        assert_eq!(cfg.retry_delay_ms, 500);
        assert_eq!(cfg.max_retry_attempts, 0);
        assert_eq!(cfg.reply_delay_mode, "non-mention");
        assert_eq!(cfg.reply_delay_ms, 120000);
    }

    #[test]
    fn slack_dm_config_defaults() {
        let cfg = SlackDMConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.policy, "open");
    }

    #[test]
    fn gateway_heartbeat_defaults() {
        let cfg = GatewayConfig::default();
        assert_eq!(cfg.heartbeat_interval_minutes, 0);
        assert_eq!(cfg.heartbeat_prompt, "heartbeat");
    }

    #[test]
    fn gateway_plain_http_is_off_by_default_and_parses_both_spellings() {
        assert!(!GatewayConfig::default().dangerously_plain_http);
        let a: GatewayConfig = serde_json::from_str(r#"{"dangerously_plain_http": true}"#).unwrap();
        let b: GatewayConfig = serde_json::from_str(r#"{"dangerouslyPlainHttp": true}"#).unwrap();
        assert!(a.dangerously_plain_http && b.dangerously_plain_http);
        assert!(!serde_json::from_str::<GatewayConfig>("{}").unwrap().dangerously_plain_http);
    }

    #[test]
    fn gateway_heartbeat_from_json() {
        let json = r#"{
            "host": "0.0.0.0",
            "port": 8080,
            "heartbeatIntervalMinutes": 15,
            "heartbeatPrompt": "status check"
        }"#;
        let cfg: GatewayConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.heartbeat_interval_minutes, 15);
        assert_eq!(cfg.heartbeat_prompt, "status check");
    }

    #[test]
    fn gateway_heartbeat_disabled_by_default() {
        let json = r#"{"host": "0.0.0.0"}"#;
        let cfg: GatewayConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.heartbeat_interval_minutes, 0);
        assert_eq!(cfg.heartbeat_prompt, "heartbeat");
    }

    #[test]
    fn mcp_server_config_roundtrip() {
        let cfg = MCPServerConfig {
            command: "npx".into(),
            args: vec!["-y".into(), "test-server".into()],
            env: {
                let mut m = HashMap::new();
                m.insert("API_KEY".into(), "secret".into());
                m
            },
            url: String::new(),
            internal_only: false,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let restored: MCPServerConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.command, "npx");
        assert_eq!(restored.args.len(), 2);
        assert_eq!(restored.env["API_KEY"], "secret");
        assert!(!restored.internal_only);
    }

    #[test]
    fn command_policy_config_defaults() {
        let config: CommandPolicyConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(config.mode, "allowlist");
        assert!(config.allowlist.is_empty());
        assert!(config.denylist.is_empty());
    }

    #[test]
    fn command_policy_config_custom() {
        let json = r#"{"mode": "denylist", "allowlist": ["echo", "ls"], "denylist": ["rm -rf /"]}"#;
        let config: CommandPolicyConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.mode, "denylist");
        assert_eq!(config.allowlist, vec!["echo", "ls"]);
        assert_eq!(config.denylist, vec!["rm -rf /"]);
    }

    #[test]
    fn url_policy_config_defaults() {
        let config: UrlPolicyConfig = serde_json::from_str("{}").unwrap();
        assert!(config.enabled);
        assert!(!config.allow_private);
        assert!(config.allowed_domains.is_empty());
        assert!(config.blocked_domains.is_empty());
    }

    #[test]
    fn url_policy_config_custom() {
        let json = r#"{"enabled": false, "allowPrivate": true, "allowedDomains": ["internal.corp"], "blockedDomains": ["evil.com"]}"#;
        let config: UrlPolicyConfig = serde_json::from_str(json).unwrap();
        assert!(!config.enabled);
        assert!(config.allow_private);
        assert_eq!(config.allowed_domains, vec!["internal.corp"]);
        assert_eq!(config.blocked_domains, vec!["evil.com"]);
    }

    #[test]
    fn tools_config_includes_policies() {
        let json = r#"{"commandPolicy": {"mode": "denylist"}, "urlPolicy": {"enabled": false}}"#;
        let config: ToolsConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.command_policy.mode, "denylist");
        assert!(!config.url_policy.enabled);
    }

    #[test]
    fn tools_config_policies_default_when_absent() {
        let config: ToolsConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(config.command_policy.mode, "allowlist");
        assert!(config.url_policy.enabled);
    }

    // ── Step 0: Three-workstream config field tests ──────────────────────

    #[test]
    fn voice_config_defaults() {
        let cfg: VoiceConfig = serde_json::from_str("{}").unwrap();
        assert!(!cfg.enabled);
        // ADR-074 / WEFT-689: product default mode is xai_s2s (fails open to local).
        assert_eq!(cfg.mode, VoiceMode::XaiS2s);
        assert_eq!(cfg.xai.api_key_env, DEFAULT_XAI_API_KEY_ENV);
        assert_eq!(cfg.xai.realtime_model, DEFAULT_XAI_REALTIME_MODEL);
        assert_eq!(cfg.xai.endpoint, DEFAULT_XAI_REALTIME_ENDPOINT);
        assert_eq!(cfg.audio.sample_rate, 16000);
        assert_eq!(cfg.audio.chunk_size, 512);
        assert_eq!(cfg.audio.channels, 1);
        assert!(cfg.audio.input_device.is_none());
        assert!(cfg.audio.output_device.is_none());
        // SC-2 / WEFT-223: default audio_retention is none (no raw audio retained).
        assert_eq!(cfg.audio_retention, AudioRetention::None);
        assert!(cfg.audio_retention.is_none());
        assert!(!cfg.audio_retention.allows_disk_write());
        assert!(!cfg.audio_retention.allows_session_hold());
        assert!(cfg.stt.enabled);
        assert_eq!(cfg.stt.model, "sherpa-onnx-streaming-zipformer-en-20M");
        assert!(cfg.stt.language.is_empty());
        assert!(cfg.tts.enabled);
        assert_eq!(cfg.tts.model, "vits-piper-en_US-amy-medium");
        assert!(cfg.tts.voice.is_empty());
        assert!((cfg.tts.speed - 1.0).abs() < f32::EPSILON);
        assert!((cfg.vad.threshold - 0.5).abs() < f32::EPSILON);
        assert_eq!(cfg.vad.silence_timeout_ms, 1500);
        assert_eq!(cfg.vad.min_speech_ms, 250);
        // WEFT-230: adaptive silence defaults
        assert!(cfg.vad.adaptive.enabled);
        assert_eq!(cfg.vad.adaptive.min_ms, 500);
        assert_eq!(cfg.vad.adaptive.max_ms, 3_000);
        assert_eq!(cfg.vad.adaptive.window_size, 8);
        assert!(!cfg.wake.enabled);
        assert_eq!(cfg.wake.phrase, "hey weft");
        assert!((cfg.wake.sensitivity - 0.5).abs() < f32::EPSILON);
        assert!(cfg.wake.model_path.is_none());
        assert!(!cfg.cloud_fallback.enabled);
        assert!(cfg.cloud_fallback.stt_provider.is_empty());
        assert!(cfg.cloud_fallback.tts_provider.is_empty());
        // SC-6 / WEFT-225 + SC-8 / WEFT-226 defaults
        assert_eq!(cfg.confirmation.timeout_seconds, 10);
        assert!(cfg.confirmation.anti_replay_nonce);
        assert!(cfg.confirmation.transcription_echo);
        assert_eq!(cfg.rate_limit.commands_per_minute, 10);
        assert_eq!(cfg.rate_limit.wake_activations_per_minute, 5);
        assert_eq!(cfg.rate_limit.fail_threshold, 3);
        assert_eq!(cfg.rate_limit.post_fail_cooldown_seconds, 30);
    }

    #[test]
    fn voice_audio_retention_serde() {
        // Default omitted key → None
        let cfg: VoiceConfig = serde_json::from_str(r#"{"enabled":true}"#).unwrap();
        assert_eq!(cfg.audio_retention, AudioRetention::None);

        let session: VoiceConfig =
            serde_json::from_str(r#"{"audio_retention":"session"}"#).unwrap();
        assert_eq!(session.audio_retention, AudioRetention::Session);
        assert!(session.audio_retention.allows_session_hold());
        assert!(!session.audio_retention.allows_disk_write());

        let persist: VoiceConfig =
            serde_json::from_str(r#"{"audioRetention":"persist"}"#).unwrap();
        assert_eq!(persist.audio_retention, AudioRetention::Persist);
        assert!(persist.audio_retention.allows_disk_write());

        // Round-trip lowercase wire form
        let json = serde_json::to_string(&AudioRetention::Session).unwrap();
        assert_eq!(json, "\"session\"");
        let back: AudioRetention = serde_json::from_str(&json).unwrap();
        assert_eq!(back, AudioRetention::Session);
    }

    #[test]
    fn gateway_api_fields_defaults() {
        let cfg = GatewayConfig::default();
        assert_eq!(cfg.api_port, 18789);
        assert_eq!(cfg.cors_origins, vec!["http://localhost:5173"]);
        assert!(!cfg.api_enabled);
    }

    #[test]
    fn provider_browser_fields_defaults() {
        let cfg: ProviderConfig = serde_json::from_str("{}").unwrap();
        assert!(!cfg.browser_direct);
        assert!(cfg.cors_proxy.is_none());
    }

    #[test]
    fn provider_base_url_alias() {
        let json = r#"{"baseUrl": "https://example.com"}"#;
        let cfg: ProviderConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.api_base.as_deref(), Some("https://example.com"));
    }

    #[test]
    fn config_with_voice_section() {
        let json = r#"{"voice": {"enabled": true}}"#;
        let cfg: Config = serde_json::from_str(json).unwrap();
        assert!(cfg.voice.enabled);
        // Sub-structs should still be default
        assert_eq!(cfg.voice.audio.sample_rate, 16000);
        assert!(cfg.voice.stt.enabled);
    }

    #[test]
    fn config_with_all_new_fields() {
        let json = r#"{
            "voice": {
                "enabled": true,
                "audio": { "sampleRate": 48000, "chunkSize": 1024, "channels": 2 },
                "stt": { "model": "custom-stt", "language": "zh" },
                "tts": { "model": "custom-tts", "voice": "alloy", "speed": 1.5 },
                "vad": { "threshold": 0.8, "silenceTimeoutMs": 2000, "minSpeechMs": 500 },
                "wake": { "enabled": true, "phrase": "ok clawft", "sensitivity": 0.7 },
                "cloudFallback": { "enabled": true, "sttProvider": "whisper", "ttsProvider": "elevenlabs" }
            },
            "gateway": {
                "host": "127.0.0.1",
                "port": 9000,
                "apiPort": 9001,
                "corsOrigins": ["http://localhost:3000", "https://app.example.com"],
                "apiEnabled": true
            },
            "providers": {
                "openai": {
                    "apiKey": "sk-test",
                    "baseUrl": "https://api.openai.com/v1",
                    "browserDirect": true,
                    "corsProxy": "https://proxy.example.com"
                }
            }
        }"#;
        let cfg: Config = serde_json::from_str(json).unwrap();

        // Voice
        assert!(cfg.voice.enabled);
        assert_eq!(cfg.voice.audio.sample_rate, 48000);
        assert_eq!(cfg.voice.audio.chunk_size, 1024);
        assert_eq!(cfg.voice.audio.channels, 2);
        assert_eq!(cfg.voice.stt.model, "custom-stt");
        assert_eq!(cfg.voice.stt.language, "zh");
        assert_eq!(cfg.voice.tts.model, "custom-tts");
        assert_eq!(cfg.voice.tts.voice, "alloy");
        assert!((cfg.voice.tts.speed - 1.5).abs() < f32::EPSILON);
        assert!((cfg.voice.vad.threshold - 0.8).abs() < f32::EPSILON);
        assert_eq!(cfg.voice.vad.silence_timeout_ms, 2000);
        assert_eq!(cfg.voice.vad.min_speech_ms, 500);
        assert!(cfg.voice.wake.enabled);
        assert_eq!(cfg.voice.wake.phrase, "ok clawft");
        assert!((cfg.voice.wake.sensitivity - 0.7).abs() < f32::EPSILON);
        assert!(cfg.voice.cloud_fallback.enabled);
        assert_eq!(cfg.voice.cloud_fallback.stt_provider, "whisper");
        assert_eq!(cfg.voice.cloud_fallback.tts_provider, "elevenlabs");

        // Gateway new fields
        assert_eq!(cfg.gateway.api_port, 9001);
        assert_eq!(
            cfg.gateway.cors_origins,
            vec!["http://localhost:3000", "https://app.example.com"]
        );
        assert!(cfg.gateway.api_enabled);

        // Provider browser fields
        assert!(cfg.providers.openai.browser_direct);
        assert_eq!(
            cfg.providers.openai.cors_proxy.as_deref(),
            Some("https://proxy.example.com")
        );
        assert_eq!(
            cfg.providers.openai.api_base.as_deref(),
            Some("https://api.openai.com/v1")
        );
    }
}
