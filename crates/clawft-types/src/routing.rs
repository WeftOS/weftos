//! Routing and permission configuration types.
//!
//! Defines the config schema for the TieredRouter (Level 1) and its
//! permission system. All types support both `snake_case` and `camelCase`
//! field names in JSON via `#[serde(alias)]`. Unknown fields are silently
//! ignored for forward compatibility.
//!
//! When the `routing` section is absent from config, `RoutingConfig::default()`
//! produces settings equivalent to the existing `StaticRouter` (Level 0).
//!
//! # Validation boundary (CONS-006 / WEFT-36)
//!
//! **Serde-time (this crate):** structural only — field types, `#[serde(default)]`,
//! and typed enums such as [`TierSelectionStrategy`]. Do not add
//! `deserialize_with` range / uniqueness / cross-field checks here.
//!
//! **Post-load (`clawft_core::routing_validation`):** single-pass semantic and
//! security validation (`validate_routing_config`, `validate_workspace_ceiling`).
//! Call after deserialize / merge and before `TieredRouter` construction.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

// ── TierSelectionStrategy ────────────────────────────────────────────────

/// Strategy for selecting a model within a tier.
///
/// Controls how the router picks among multiple models in a single tier.
/// Serializes/deserializes as snake_case strings (e.g., `"preference_order"`).
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TierSelectionStrategy {
    /// Use models in the order listed (first available wins).
    PreferenceOrder,
    /// Rotate through models in round-robin fashion.
    RoundRobin,
    /// Pick the cheapest model in the tier.
    LowestCost,
    /// Pick a random model from the tier.
    Random,
}

// ── RoutingConfig ────────────────────────────────────────────────────────

/// Top-level routing configuration.
///
/// Added to the root `Config` struct alongside `agents`, `channels`, etc.
/// When absent from JSON, defaults to `mode = "static"` (Level 0 StaticRouter).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingConfig {
    /// Routing mode: `"static"` (default, Level 0) or `"tiered"` (Level 1).
    #[serde(default = "default_routing_mode")]
    pub mode: String,

    /// Model tier definitions, ordered cheapest to most expensive.
    /// Only used when `mode = "tiered"`.
    #[serde(default)]
    pub tiers: Vec<ModelTierConfig>,

    /// Model selection strategy within a tier.
    #[serde(default, alias = "selectionStrategy")]
    pub selection_strategy: Option<TierSelectionStrategy>,

    /// Fallback model when all tiers/budgets are exhausted.
    /// Format: `"provider/model"` (e.g., `"groq/llama-3.1-8b"`).
    #[serde(default, alias = "fallbackModel")]
    pub fallback_model: Option<String>,

    /// Permission level definitions and per-user/channel overrides.
    #[serde(default)]
    pub permissions: PermissionsConfig,

    /// Escalation behavior settings.
    #[serde(default)]
    pub escalation: EscalationConfig,

    /// Global cost budget settings.
    #[serde(default, alias = "costBudgets")]
    pub cost_budgets: CostBudgetConfig,

    /// Rate limiting settings.
    #[serde(default, alias = "rateLimiting")]
    pub rate_limiting: RateLimitConfig,

    /// Pre-LLM `ContextRouter` selector
    /// (`docs/plans/agent-core-v1.md` Phase E1).
    ///
    /// Decides which `ContextRouter` impl the daemon attaches to its
    /// chat path. The router runs *before* the LLM call to nudge
    /// classification (`complexity_hint`) and surface an `archetype`
    /// label; it never picks a model. See
    /// `docs/research/rvf-context-router.md` for the contract.
    ///
    /// Supported values:
    ///
    /// - `"null"` (default) — v0 [`NullRouter`]: no-op, current
    ///   behaviour preserved bit-for-bit.
    /// - `"llm-classifier"` — v1 [`LlmClassifierRouter`]: round-trips
    ///   the user's message against the daemon's `LlmClient` and reads
    ///   back `{archetype, complexity}`. v1 → v2 promotion gate (7-day
    ///   fallback rate < 25%) is policy, not code.
    /// - `"embedding"` — v2 `EmbeddingRouter` (Phase E2): builds an
    ///   in-memory `ruvector-diskann@2.1` index over hand-authored
    ///   skill descriptors at boot, retrieves top-K nearest skills per
    ///   turn via the crate-local `Embedder` trait (production:
    ///   `ApiEmbedder` against an OpenAI-compat `/embeddings`, or a local
    ///   real model; with neither configured the router is unavailable,
    ///   there is no hash floor).
    /// - `"hybrid"` — v2.5 `HybridRouter` (Phase E3): chains a primary
    ///   (often v2 `EmbeddingRouter` or v3 `MicroLoraRouter`) with a
    ///   fallback (often v1 `LlmClassifierRouter`). Empty primary →
    ///   fallback. Optional sona skill rerank when `hybrid-rerank` is on.
    /// - `"micro-lora"` — v3 `MicroLoraRouter` (WEFT-45/338): **in-tree**,
    ///   unlimited hashed patterns + rank-r complexity residual. Does
    ///   **not** use ruvllm-wasm (avoids the historical 11-pattern cap).
    ///
    /// [`NullRouter`]: ../../clawft_core/agent/context_router/struct.NullRouter.html
    /// [`LlmClassifierRouter`]: ../../clawft_core/agent/context_router/llm_classifier/struct.LlmClassifierRouter.html
    #[serde(default = "default_context_router", alias = "contextRouter")]
    pub context_router: String,

    /// Maximum permission level a workspace config may grant (WEFT-47).
    ///
    /// Used by workspace ceiling validation (`validate_workspace_ceiling`)
    /// so operators can raise (or lower) the hard ceiling without patching
    /// code. Default is `1` (user level) — matching the historical constant
    /// that blocked workspaces from granting admin (`2`). Missing field in
    /// JSON/TOML deserializes to this default for backward compatibility.
    #[serde(
        default = "default_max_grantable_level",
        alias = "maxGrantableLevel"
    )]
    pub max_grantable_level: u8,
}

fn default_routing_mode() -> String {
    "static".into()
}

/// Default for [`RoutingConfig::context_router`]: keep v0 NullRouter
/// live so existing deployments see no behaviour change. Operators
/// flip to `"llm-classifier"` to enable the Phase E1 router.
fn default_context_router() -> String {
    "null".into()
}

/// Default workspace permission ceiling: user level (`1`).
///
/// Matches the historical `DEFAULT_MAX_GRANTABLE_LEVEL` constant that
/// lived in `routing_validation` before WEFT-47 promoted it to config.
pub fn default_max_grantable_level() -> u8 {
    1
}

impl Default for RoutingConfig {
    fn default() -> Self {
        Self {
            mode: default_routing_mode(),
            tiers: Vec::new(),
            selection_strategy: None,
            fallback_model: None,
            permissions: PermissionsConfig::default(),
            escalation: EscalationConfig::default(),
            cost_budgets: CostBudgetConfig::default(),
            rate_limiting: RateLimitConfig::default(),
            context_router: default_context_router(),
            max_grantable_level: default_max_grantable_level(),
        }
    }
}

// ── ModelTierConfig ──────────────────────────────────────────────────────

/// A named group of models at a similar cost/capability level.
///
/// Tiers are ordered from cheapest to most expensive in the `tiers` array.
/// Complexity ranges may overlap intentionally -- the router picks the
/// highest-quality tier the user is allowed and can afford.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelTierConfig {
    /// Tier name (e.g., `"free"`, `"standard"`, `"premium"`, `"elite"`).
    pub name: String,

    /// Models available in this tier, in preference order.
    /// Format: `"provider/model"` (e.g., `"anthropic/claude-haiku-3.5"`).
    #[serde(default)]
    pub models: Vec<String>,

    /// Complexity range this tier covers: `[min, max]` where each is 0.0-1.0.
    #[serde(default = "default_complexity_range", alias = "complexityRange")]
    pub complexity_range: [f32; 2],

    /// Approximate cost per 1K tokens (blended input/output) in USD.
    #[serde(default, alias = "costPer1kTokens")]
    pub cost_per_1k_tokens: f64,

    /// Maximum context tokens supported by models in this tier.
    #[serde(default = "default_tier_max_context", alias = "maxContextTokens")]
    pub max_context_tokens: usize,
}

fn default_complexity_range() -> [f32; 2] {
    [0.0, 1.0]
}

fn default_tier_max_context() -> usize {
    8192
}

impl Default for ModelTierConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            models: Vec::new(),
            complexity_range: default_complexity_range(),
            cost_per_1k_tokens: 0.0,
            max_context_tokens: default_tier_max_context(),
        }
    }
}

// ── PermissionsConfig ────────────────────────────────────────────────────

/// Container for permission level defaults and per-user/channel overrides.
///
/// The three built-in levels (`zero_trust`, `user`, `admin`) are named fields.
/// Per-user and per-channel overrides use HashMaps keyed by sender ID or
/// channel name.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PermissionsConfig {
    /// Level 0 (zero-trust) permission defaults.
    #[serde(default)]
    pub zero_trust: PermissionLevelConfig,

    /// Level 1 (user) permission defaults.
    #[serde(default)]
    pub user: PermissionLevelConfig,

    /// Level 2 (admin) permission defaults.
    #[serde(default)]
    pub admin: PermissionLevelConfig,

    /// Per-user permission overrides, keyed by sender ID.
    #[serde(default)]
    pub users: HashMap<String, PermissionLevelConfig>,

    /// Per-channel permission overrides, keyed by channel name.
    #[serde(default)]
    pub channels: HashMap<String, PermissionLevelConfig>,
}

// ── PermissionLevelConfig ────────────────────────────────────────────────

/// Configuration for a single permission level or override.
///
/// When used as a level default, all fields are meaningful. When used as
/// a per-user or per-channel override, only specified fields apply --
/// the rest inherit from the user's resolved level defaults.
///
/// All fields are `Option` to support partial overrides.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PermissionLevelConfig {
    /// Permission level (0 = zero_trust, 1 = user, 2 = admin).
    #[serde(default)]
    pub level: Option<u8>,

    /// Maximum model tier this user can access.
    #[serde(default, alias = "maxTier")]
    pub max_tier: Option<String>,

    /// Explicit model allowlist. Empty = all models in allowed tiers.
    #[serde(default, alias = "modelAccess")]
    pub model_access: Option<Vec<String>>,

    /// Explicit model denylist. Checked after allowlist.
    #[serde(default, alias = "modelDenylist")]
    pub model_denylist: Option<Vec<String>>,

    /// Tool names this user can invoke. `["*"]` = all tools.
    #[serde(default, alias = "toolAccess")]
    pub tool_access: Option<Vec<String>>,

    /// Tool names explicitly denied even if tool_access allows.
    #[serde(default, alias = "toolDenylist")]
    pub tool_denylist: Option<Vec<String>>,

    /// Maximum input context tokens.
    #[serde(default, alias = "maxContextTokens")]
    pub max_context_tokens: Option<usize>,

    /// Maximum output tokens per response.
    #[serde(default, alias = "maxOutputTokens")]
    pub max_output_tokens: Option<usize>,

    /// Rate limit in requests per minute. 0 = unlimited.
    #[serde(default, alias = "rateLimit")]
    pub rate_limit: Option<u32>,

    /// Whether SSE streaming responses are allowed.
    #[serde(default, alias = "streamingAllowed")]
    pub streaming_allowed: Option<bool>,

    /// Whether complexity-based escalation to a higher tier is allowed.
    #[serde(default, alias = "escalationAllowed")]
    pub escalation_allowed: Option<bool>,

    /// Complexity threshold (0.0-1.0) above which escalation triggers.
    #[serde(default, alias = "escalationThreshold")]
    pub escalation_threshold: Option<f32>,

    /// Whether the user can manually override model selection.
    #[serde(default, alias = "modelOverride")]
    pub model_override: Option<bool>,

    /// Daily cost budget in USD. 0.0 = unlimited.
    #[serde(default, alias = "costBudgetDailyUsd")]
    pub cost_budget_daily_usd: Option<f64>,

    /// Monthly cost budget in USD. 0.0 = unlimited.
    #[serde(default, alias = "costBudgetMonthlyUsd")]
    pub cost_budget_monthly_usd: Option<f64>,

    /// Extensible custom permission dimensions.
    #[serde(default, alias = "customPermissions")]
    pub custom_permissions: Option<HashMap<String, serde_json::Value>>,
}

// ── UserPermissions ──────────────────────────────────────────────────────

/// Resolved user permission capabilities.
///
/// This is the **runtime** permission object produced by layering:
/// built-in defaults + level config + workspace config + user override +
/// channel override. Unlike `PermissionLevelConfig` (which uses `Option`
/// for partial overrides), all fields here are concrete values.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserPermissions {
    /// Permission level (0 = zero_trust, 1 = user, 2 = admin).
    #[serde(default)]
    pub level: u8,

    /// Maximum model tier this user can access.
    #[serde(default, alias = "maxTier")]
    pub max_tier: String,

    /// Explicit model allowlist. Empty = all models in allowed tiers.
    #[serde(default, alias = "modelAccess")]
    pub model_access: Vec<String>,

    /// Explicit model denylist.
    #[serde(default, alias = "modelDenylist")]
    pub model_denylist: Vec<String>,

    /// Tool names this user can invoke. `["*"]` = all tools.
    #[serde(default, alias = "toolAccess")]
    pub tool_access: Vec<String>,

    /// Tool names explicitly denied.
    #[serde(default, alias = "toolDenylist")]
    pub tool_denylist: Vec<String>,

    /// Maximum input context tokens.
    #[serde(default = "default_max_context_tokens", alias = "maxContextTokens")]
    pub max_context_tokens: usize,

    /// Maximum output tokens per response.
    #[serde(default = "default_max_output_tokens", alias = "maxOutputTokens")]
    pub max_output_tokens: usize,

    /// Rate limit in requests per minute. 0 = unlimited.
    #[serde(default = "default_rate_limit", alias = "rateLimit")]
    pub rate_limit: u32,

    /// Whether SSE streaming responses are allowed.
    #[serde(default, alias = "streamingAllowed")]
    pub streaming_allowed: bool,

    /// Whether complexity-based escalation is allowed.
    #[serde(default, alias = "escalationAllowed")]
    pub escalation_allowed: bool,

    /// Complexity threshold (0.0-1.0) above which escalation triggers.
    #[serde(
        default = "default_escalation_threshold",
        alias = "escalationThreshold"
    )]
    pub escalation_threshold: f32,

    /// Whether the user can manually override model selection.
    #[serde(default, alias = "modelOverride")]
    pub model_override: bool,

    /// Daily cost budget in USD. 0.0 = unlimited.
    /// Zero-trust default: $0.10/day (see design doc Section 2.2).
    #[serde(
        default = "default_cost_budget_daily_usd",
        alias = "costBudgetDailyUsd"
    )]
    pub cost_budget_daily_usd: f64,

    /// Monthly cost budget in USD. 0.0 = unlimited.
    /// Zero-trust default: $2.00/month (see design doc Section 2.2).
    #[serde(
        default = "default_cost_budget_monthly_usd",
        alias = "costBudgetMonthlyUsd"
    )]
    pub cost_budget_monthly_usd: f64,

    /// Extensible custom permission dimensions.
    #[serde(default, alias = "customPermissions")]
    pub custom_permissions: HashMap<String, serde_json::Value>,
}

fn default_cost_budget_daily_usd() -> f64 {
    0.10
}

fn default_cost_budget_monthly_usd() -> f64 {
    2.00
}

fn default_max_context_tokens() -> usize {
    4096
}

fn default_max_output_tokens() -> usize {
    1024
}

fn default_rate_limit() -> u32 {
    10
}

fn default_escalation_threshold() -> f32 {
    1.0
}

impl Default for UserPermissions {
    /// Returns zero-trust defaults. All values are restrictive.
    /// `cost_budget_daily_usd` = $0.10, `cost_budget_monthly_usd` = $2.00
    /// per design doc Section 2.2 (NOT 0.0, which would mean unlimited).
    fn default() -> Self {
        Self {
            level: 0,
            max_tier: "free".into(),
            model_access: Vec::new(),
            model_denylist: Vec::new(),
            tool_access: Vec::new(),
            tool_denylist: Vec::new(),
            max_context_tokens: default_max_context_tokens(),
            max_output_tokens: default_max_output_tokens(),
            rate_limit: default_rate_limit(),
            streaming_allowed: false,
            escalation_allowed: false,
            escalation_threshold: default_escalation_threshold(),
            model_override: false,
            cost_budget_daily_usd: default_cost_budget_daily_usd(),
            cost_budget_monthly_usd: default_cost_budget_monthly_usd(),
            custom_permissions: HashMap::new(),
        }
    }
}

// ── AuthContext ───────────────────────────────────────────────────────────

/// Authentication context threaded through the request pipeline.
///
/// Attached to `ChatRequest` by the agent loop after resolving the sender's
/// identity from channel authentication. When absent, the router defaults
/// to zero-trust permissions.
///
/// `Default` returns zero-trust values (empty sender_id, empty channel,
/// zero-trust permissions). For CLI use, call `AuthContext::cli_default()`
/// which sets sender_id="local", channel="cli", and admin permissions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthContext {
    /// Unique sender identifier (platform-specific).
    /// Telegram: user ID, Slack: user ID, Discord: user ID, CLI: `"local"`.
    #[serde(default, alias = "senderId")]
    pub sender_id: String,

    /// Channel name the request originated from.
    #[serde(default)]
    pub channel: String,

    /// Resolved permissions for this sender.
    #[serde(default)]
    pub permissions: UserPermissions,
}

impl Default for AuthContext {
    /// Returns zero-trust defaults: empty sender_id, empty channel,
    /// zero-trust permissions. Unauthenticated requests get minimal access.
    fn default() -> Self {
        Self {
            sender_id: String::new(),
            channel: String::new(),
            permissions: UserPermissions::default(),
        }
    }
}

impl AuthContext {
    /// Convenience constructor for CLI use. Sets `sender_id = "local"`,
    /// `channel = "cli"`, and admin-level permissions. Callers must
    /// explicitly opt into CLI privileges -- this is NOT the Default.
    pub fn cli_default() -> Self {
        Self {
            sender_id: "local".into(),
            channel: "cli".into(),
            permissions: UserPermissions {
                level: 2,
                max_tier: "elite".into(),
                tool_access: vec!["*".into()],
                max_context_tokens: 200_000,
                max_output_tokens: 16_384,
                rate_limit: 0,
                streaming_allowed: true,
                escalation_allowed: true,
                escalation_threshold: 0.0,
                model_override: true,
                cost_budget_daily_usd: 0.0,
                cost_budget_monthly_usd: 0.0,
                ..UserPermissions::default()
            },
        }
    }
}

// ── EscalationConfig ─────────────────────────────────────────────────────

/// Controls complexity-based escalation to higher model tiers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EscalationConfig {
    /// Whether escalation is enabled globally.
    #[serde(default)]
    pub enabled: bool,

    /// Default complexity threshold for escalation (0.0-1.0).
    #[serde(default = "default_global_escalation_threshold")]
    pub threshold: f32,

    /// Maximum number of tiers a request can escalate beyond the user's `max_tier`.
    #[serde(default = "default_max_escalation_tiers", alias = "maxEscalationTiers")]
    pub max_escalation_tiers: u32,
}

fn default_global_escalation_threshold() -> f32 {
    0.6
}

fn default_max_escalation_tiers() -> u32 {
    1
}

impl Default for EscalationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            threshold: default_global_escalation_threshold(),
            max_escalation_tiers: default_max_escalation_tiers(),
        }
    }
}

// ── CostBudgetConfig ─────────────────────────────────────────────────────

/// Global cost budget settings.
///
/// System-wide limits that apply regardless of individual user budgets.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostBudgetConfig {
    /// Global daily spending limit in USD. 0.0 = unlimited.
    #[serde(default, alias = "globalDailyLimitUsd")]
    pub global_daily_limit_usd: f64,

    /// Global monthly spending limit in USD. 0.0 = unlimited.
    #[serde(default, alias = "globalMonthlyLimitUsd")]
    pub global_monthly_limit_usd: f64,

    /// Whether to persist cost tracking data to disk.
    #[serde(default, alias = "trackingPersistence")]
    pub tracking_persistence: bool,

    /// Hour (UTC) at which daily budgets reset. 0 = midnight UTC.
    #[serde(default, alias = "resetHourUtc")]
    pub reset_hour_utc: u8,
}

impl Default for CostBudgetConfig {
    fn default() -> Self {
        Self {
            global_daily_limit_usd: 0.0,
            global_monthly_limit_usd: 0.0,
            tracking_persistence: false,
            reset_hour_utc: 0,
        }
    }
}

// ── RateLimitConfig ──────────────────────────────────────────────────────

/// Rate limiting configuration.
///
/// Controls the sliding-window rate limiter that enforces per-user request
/// limits. The window size and strategy are global; per-user limits are
/// defined in `PermissionLevelConfig.rate_limit`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitConfig {
    /// Window size in seconds for rate limit calculations.
    #[serde(default = "default_window_seconds", alias = "windowSeconds")]
    pub window_seconds: u32,

    /// Rate limiting strategy: `"sliding_window"` (default) or `"fixed_window"`.
    #[serde(default = "default_rate_limit_strategy")]
    pub strategy: String,

    /// Global rate limit in requests per minute across ALL users.
    /// 0 = unlimited (no global cap). Checked before per-user limits.
    #[serde(default, alias = "globalRateLimitRpm")]
    pub global_rate_limit_rpm: u32,
}

fn default_window_seconds() -> u32 {
    60
}

fn default_rate_limit_strategy() -> String {
    "sliding_window".into()
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            window_seconds: default_window_seconds(),
            strategy: default_rate_limit_strategy(),
            global_rate_limit_rpm: 0,
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const TIERED_FIXTURE_PATH: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/config_tiered.json"
    );

    fn load_tiered_fixture() -> crate::config::Config {
        let content = std::fs::read_to_string(TIERED_FIXTURE_PATH)
            .expect("config_tiered.json fixture should exist");
        serde_json::from_str(&content).expect("tiered fixture should deserialize")
    }

    #[test]
    fn routing_config_defaults() {
        let cfg = RoutingConfig::default();
        assert_eq!(cfg.mode, "static");
        assert!(cfg.tiers.is_empty());
        assert!(cfg.selection_strategy.is_none());
        assert!(cfg.fallback_model.is_none());
        assert!(!cfg.escalation.enabled);
        assert_eq!(cfg.max_grantable_level, 1);
    }

    #[test]
    fn model_tier_config_defaults() {
        let cfg = ModelTierConfig::default();
        assert!(cfg.name.is_empty());
        assert!(cfg.models.is_empty());
        assert_eq!(cfg.complexity_range, [0.0, 1.0]);
        assert_eq!(cfg.cost_per_1k_tokens, 0.0);
        assert_eq!(cfg.max_context_tokens, 8192);
    }

    #[test]
    fn permission_level_config_defaults() {
        let cfg = PermissionLevelConfig::default();
        assert!(cfg.level.is_none());
        assert!(cfg.max_tier.is_none());
        assert!(cfg.tool_access.is_none());
        assert!(cfg.rate_limit.is_none());
        assert!(cfg.streaming_allowed.is_none());
    }

    #[test]
    fn user_permissions_defaults() {
        let perms = UserPermissions::default();
        assert_eq!(perms.level, 0);
        assert_eq!(perms.max_tier, "free");
        assert!(perms.tool_access.is_empty());
        assert_eq!(perms.max_context_tokens, 4096);
        assert_eq!(perms.max_output_tokens, 1024);
        assert_eq!(perms.rate_limit, 10);
        assert!(!perms.streaming_allowed);
        assert!(!perms.escalation_allowed);
        assert!((perms.escalation_threshold - 1.0).abs() < f32::EPSILON);
        assert!(!perms.model_override);
        assert!((perms.cost_budget_daily_usd - 0.10).abs() < f64::EPSILON);
        assert!((perms.cost_budget_monthly_usd - 2.00).abs() < f64::EPSILON);
        assert!(perms.custom_permissions.is_empty());
    }

    #[test]
    fn auth_context_defaults() {
        let ctx = AuthContext::default();
        assert!(ctx.sender_id.is_empty());
        assert!(ctx.channel.is_empty());
        assert_eq!(ctx.permissions.level, 0);
        assert!((ctx.permissions.cost_budget_daily_usd - 0.10).abs() < f64::EPSILON);
        assert!((ctx.permissions.cost_budget_monthly_usd - 2.00).abs() < f64::EPSILON);
    }

    #[test]
    fn auth_context_cli_default() {
        let ctx = AuthContext::cli_default();
        assert_eq!(ctx.sender_id, "local");
        assert_eq!(ctx.channel, "cli");
        assert_eq!(ctx.permissions.level, 2);
        assert_eq!(ctx.permissions.max_tier, "elite");
        assert_eq!(ctx.permissions.tool_access, vec!["*"]);
        assert_eq!(ctx.permissions.max_context_tokens, 200_000);
        assert_eq!(ctx.permissions.max_output_tokens, 16_384);
        assert_eq!(ctx.permissions.rate_limit, 0);
        assert!(ctx.permissions.streaming_allowed);
        assert!(ctx.permissions.escalation_allowed);
        assert!((ctx.permissions.escalation_threshold - 0.0).abs() < f32::EPSILON);
        assert!(ctx.permissions.model_override);
        assert_eq!(ctx.permissions.cost_budget_daily_usd, 0.0);
        assert_eq!(ctx.permissions.cost_budget_monthly_usd, 0.0);
    }

    #[test]
    fn tier_selection_strategy_serde() {
        let json = serde_json::to_string(&TierSelectionStrategy::PreferenceOrder).unwrap();
        assert_eq!(json, "\"preference_order\"");

        let json = serde_json::to_string(&TierSelectionStrategy::RoundRobin).unwrap();
        assert_eq!(json, "\"round_robin\"");

        let json = serde_json::to_string(&TierSelectionStrategy::LowestCost).unwrap();
        assert_eq!(json, "\"lowest_cost\"");

        let json = serde_json::to_string(&TierSelectionStrategy::Random).unwrap();
        assert_eq!(json, "\"random\"");

        let strategy: TierSelectionStrategy = serde_json::from_str("\"preference_order\"").unwrap();
        assert_eq!(strategy, TierSelectionStrategy::PreferenceOrder);

        let strategy: TierSelectionStrategy = serde_json::from_str("\"round_robin\"").unwrap();
        assert_eq!(strategy, TierSelectionStrategy::RoundRobin);

        let result = serde_json::from_str::<TierSelectionStrategy>("\"invalid_strategy\"");
        assert!(result.is_err());

        // WEFT-36 / CONS-006: nested on RoutingConfig — serde-time path only;
        // post-load validation is never reached for unknown strategies.
        let nested = serde_json::from_str::<RoutingConfig>(
            r#"{ "mode": "tiered", "selection_strategy": "invalid_strategy" }"#,
        );
        assert!(
            nested.is_err(),
            "unknown selection_strategy must fail RoutingConfig deserialize"
        );
    }

    #[test]
    fn escalation_config_defaults() {
        let cfg = EscalationConfig::default();
        assert!(!cfg.enabled);
        assert!((cfg.threshold - 0.6).abs() < f32::EPSILON);
        assert_eq!(cfg.max_escalation_tiers, 1);
    }

    #[test]
    fn cost_budget_config_defaults() {
        let cfg = CostBudgetConfig::default();
        assert_eq!(cfg.global_daily_limit_usd, 0.0);
        assert_eq!(cfg.global_monthly_limit_usd, 0.0);
        assert!(!cfg.tracking_persistence);
        assert_eq!(cfg.reset_hour_utc, 0);
    }

    #[test]
    fn rate_limit_config_defaults() {
        let cfg = RateLimitConfig::default();
        assert_eq!(cfg.window_seconds, 60);
        assert_eq!(cfg.strategy, "sliding_window");
        assert_eq!(cfg.global_rate_limit_rpm, 0);
    }

    #[test]
    fn deserialize_full_tiered_config() {
        let cfg = load_tiered_fixture();
        let routing = &cfg.routing;

        assert_eq!(routing.mode, "tiered");

        assert_eq!(routing.tiers.len(), 4);
        assert_eq!(routing.tiers[0].name, "free");
        assert_eq!(routing.tiers[0].models.len(), 2);
        assert_eq!(routing.tiers[0].complexity_range, [0.0, 0.3]);
        assert_eq!(routing.tiers[0].cost_per_1k_tokens, 0.0);
        assert_eq!(routing.tiers[0].max_context_tokens, 8192);

        assert_eq!(routing.tiers[1].name, "standard");
        assert_eq!(routing.tiers[2].name, "premium");
        assert_eq!(routing.tiers[3].name, "elite");
        assert_eq!(routing.tiers[3].cost_per_1k_tokens, 0.05);
        assert_eq!(routing.tiers[3].max_context_tokens, 200000);

        assert_eq!(
            routing.selection_strategy,
            Some(TierSelectionStrategy::PreferenceOrder)
        );
        assert_eq!(routing.fallback_model.as_deref(), Some("groq/llama-3.1-8b"));

        let zt = &routing.permissions.zero_trust;
        assert_eq!(zt.level, Some(0));
        assert_eq!(zt.max_tier.as_deref(), Some("free"));
        assert_eq!(zt.tool_access.as_ref().map(|v| v.len()), Some(0));
        assert_eq!(zt.max_context_tokens, Some(4096));
        assert_eq!(zt.max_output_tokens, Some(1024));
        assert_eq!(zt.rate_limit, Some(10));
        assert_eq!(zt.streaming_allowed, Some(false));
        assert_eq!(zt.escalation_allowed, Some(false));

        let u = &routing.permissions.user;
        assert_eq!(u.level, Some(1));
        assert_eq!(u.max_tier.as_deref(), Some("standard"));
        assert_eq!(u.tool_access.as_ref().map(|v| v.len()), Some(7));
        assert_eq!(u.streaming_allowed, Some(true));
        assert_eq!(u.escalation_allowed, Some(true));

        let a = &routing.permissions.admin;
        assert_eq!(a.level, Some(2));
        assert_eq!(a.max_tier.as_deref(), Some("elite"));
        assert_eq!(a.model_override, Some(true));
        assert_eq!(a.rate_limit, Some(0));

        assert!(routing.permissions.users.contains_key("alice_telegram_123"));
        assert_eq!(
            routing.permissions.users["alice_telegram_123"].level,
            Some(2)
        );
        assert!(routing.permissions.users.contains_key("bob_discord_456"));
        assert_eq!(routing.permissions.users["bob_discord_456"].level, Some(1));
        assert_eq!(
            routing.permissions.users["bob_discord_456"].cost_budget_daily_usd,
            Some(2.00)
        );

        assert_eq!(routing.permissions.channels["cli"].level, Some(2));
        assert_eq!(routing.permissions.channels["telegram"].level, Some(1));
        assert_eq!(routing.permissions.channels["discord"].level, Some(0));

        assert!(routing.escalation.enabled);
        assert!((routing.escalation.threshold - 0.6).abs() < f32::EPSILON);
        assert_eq!(routing.escalation.max_escalation_tiers, 1);

        assert_eq!(routing.cost_budgets.global_daily_limit_usd, 50.0);
        assert_eq!(routing.cost_budgets.global_monthly_limit_usd, 500.0);
        assert!(routing.cost_budgets.tracking_persistence);
        assert_eq!(routing.cost_budgets.reset_hour_utc, 0);

        assert_eq!(routing.rate_limiting.window_seconds, 60);
        assert_eq!(routing.rate_limiting.strategy, "sliding_window");
        assert_eq!(routing.rate_limiting.global_rate_limit_rpm, 0);
    }

    #[test]
    fn serde_roundtrip_routing_config() {
        let cfg = load_tiered_fixture();
        let json = serde_json::to_string(&cfg.routing).unwrap();
        let restored: RoutingConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.mode, cfg.routing.mode);
        assert_eq!(restored.tiers.len(), cfg.routing.tiers.len());
        assert_eq!(restored.tiers[0].name, cfg.routing.tiers[0].name);
        assert_eq!(restored.fallback_model, cfg.routing.fallback_model);
        assert_eq!(
            restored.escalation.max_escalation_tiers,
            cfg.routing.escalation.max_escalation_tiers
        );
    }

    #[test]
    fn camel_case_aliases() {
        let json = r#"{
            "mode": "tiered",
            "selectionStrategy": "round_robin",
            "fallbackModel": "groq/llama-3.1-8b",
            "costBudgets": {
                "globalDailyLimitUsd": 25.0,
                "globalMonthlyLimitUsd": 250.0,
                "trackingPersistence": true,
                "resetHourUtc": 6
            },
            "rateLimiting": {
                "windowSeconds": 120
            },
            "escalation": {
                "maxEscalationTiers": 2
            }
        }"#;
        let cfg: RoutingConfig = serde_json::from_str(json).unwrap();
        assert_eq!(
            cfg.selection_strategy,
            Some(TierSelectionStrategy::RoundRobin)
        );
        assert_eq!(cfg.fallback_model.as_deref(), Some("groq/llama-3.1-8b"));
        assert_eq!(cfg.cost_budgets.global_daily_limit_usd, 25.0);
        assert_eq!(cfg.cost_budgets.global_monthly_limit_usd, 250.0);
        assert!(cfg.cost_budgets.tracking_persistence);
        assert_eq!(cfg.cost_budgets.reset_hour_utc, 6);
        assert_eq!(cfg.rate_limiting.window_seconds, 120);
        assert_eq!(cfg.escalation.max_escalation_tiers, 2);
    }

    #[test]
    fn unknown_fields_ignored() {
        let json = r#"{
            "mode": "tiered",
            "future_field": "should be ignored",
            "escalation": {
                "enabled": true,
                "unknown_nested": 42
            },
            "tiers": [{
                "name": "test",
                "models": [],
                "complexity_range": [0.0, 1.0],
                "cost_per_1k_tokens": 0.0,
                "some_future_field": true
            }]
        }"#;
        let cfg: RoutingConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.mode, "tiered");
        assert!(cfg.escalation.enabled);
        assert_eq!(cfg.tiers.len(), 1);
        assert_eq!(cfg.tiers[0].name, "test");
    }

    #[test]
    fn empty_routing_section() {
        let json = r#"{}"#;
        let cfg: RoutingConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.mode, "static");
        assert!(cfg.tiers.is_empty());
        assert!(cfg.selection_strategy.is_none());
        assert!(cfg.fallback_model.is_none());
        assert!(!cfg.escalation.enabled);
        // WEFT-47: missing max_grantable_level deserializes to historical default.
        assert_eq!(cfg.max_grantable_level, default_max_grantable_level());
    }

    #[test]
    fn max_grantable_level_serde_and_alias() {
        // Explicit snake_case.
        let cfg: RoutingConfig =
            serde_json::from_str(r#"{ "max_grantable_level": 2 }"#).unwrap();
        assert_eq!(cfg.max_grantable_level, 2);

        // camelCase alias.
        let cfg: RoutingConfig =
            serde_json::from_str(r#"{ "maxGrantableLevel": 0 }"#).unwrap();
        assert_eq!(cfg.max_grantable_level, 0);

        // Missing field → default 1 (backward-compat).
        let cfg: RoutingConfig = serde_json::from_str(r#"{ "mode": "tiered" }"#).unwrap();
        assert_eq!(cfg.max_grantable_level, 1);
    }

    #[test]
    fn backward_compat_no_routing() {
        let json = r#"{
            "agents": { "defaults": { "model": "deepseek/deepseek-chat" } },
            "providers": { "anthropic": { "apiKey": "test" } }
        }"#;
        let cfg: crate::config::Config = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.agents.defaults.model, "deepseek/deepseek-chat");
        assert_eq!(cfg.routing.mode, "static");
        assert!(cfg.routing.tiers.is_empty());
    }

    #[test]
    fn per_user_partial_override() {
        let json = r#"{
            "users": {
                "alice": { "level": 2 },
                "bob": { "level": 1, "cost_budget_daily_usd": 3.50 }
            }
        }"#;
        let cfg: PermissionsConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.users["alice"].level, Some(2));
        assert!(cfg.users["alice"].max_tier.is_none());
        assert!(cfg.users["alice"].tool_access.is_none());
        assert_eq!(cfg.users["bob"].level, Some(1));
        assert_eq!(cfg.users["bob"].cost_budget_daily_usd, Some(3.50));
    }

    #[test]
    fn complexity_range_array() {
        let json = r#"{
            "name": "test",
            "models": ["provider/model"],
            "complexity_range": [0.3, 0.7],
            "cost_per_1k_tokens": 0.005
        }"#;
        let tier: ModelTierConfig = serde_json::from_str(json).unwrap();
        assert_eq!(tier.complexity_range, [0.3, 0.7]);
        assert_eq!(tier.name, "test");
        assert_eq!(tier.models, vec!["provider/model"]);
        assert_eq!(tier.cost_per_1k_tokens, 0.005);
    }

    // ── Phase F: Auth context safety tests ──────────────────────────

    /// F-17: UserPermissions::default() returns zero-trust (level 0).
    /// This verifies that any unknown/unconfigured user gets the safest defaults.
    #[test]
    fn test_defaults_for_level_unknown_returns_zero_trust() {
        let perms = UserPermissions::default();
        assert_eq!(
            perms.level, 0,
            "default permissions should be zero-trust (level 0)"
        );
        assert_eq!(perms.max_tier, "free", "zero-trust should have 'free' tier");
        assert!(
            perms.tool_access.is_empty(),
            "zero-trust should have no tool access"
        );
        assert!(
            !perms.streaming_allowed,
            "zero-trust should not allow streaming"
        );
        assert!(
            !perms.escalation_allowed,
            "zero-trust should not allow escalation"
        );
        assert!(
            !perms.model_override,
            "zero-trust should not allow model override"
        );
    }

    /// F-extra: AuthContext::default() returns zero-trust with empty identity.
    #[test]
    fn test_auth_context_default_is_zero_trust() {
        let ctx = AuthContext::default();
        assert!(
            ctx.sender_id.is_empty(),
            "default sender_id should be empty"
        );
        assert!(ctx.channel.is_empty(), "default channel should be empty");
        assert_eq!(
            ctx.permissions.level, 0,
            "default permissions should be level 0"
        );
    }

    /// F-extra: AuthContext::cli_default() returns admin with correct identity.
    #[test]
    fn test_auth_context_cli_default_is_admin() {
        let ctx = AuthContext::cli_default();
        assert_eq!(ctx.sender_id, "local");
        assert_eq!(ctx.channel, "cli");
        assert_eq!(
            ctx.permissions.level, 2,
            "CLI default should be admin (level 2)"
        );
        assert_eq!(ctx.permissions.max_tier, "elite");
        assert!(ctx.permissions.tool_access.contains(&"*".to_string()));
        assert_eq!(
            ctx.permissions.rate_limit, 0,
            "CLI admin should have no rate limit"
        );
        assert!(ctx.permissions.streaming_allowed);
        assert!(ctx.permissions.escalation_allowed);
        assert!(ctx.permissions.model_override);
    }
}
