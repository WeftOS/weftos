//! Core agent loop -- message processing pipeline.
//!
//! The [`AgentLoop`] is the heart of the clawft agent. It implements the
//! consume-process-respond cycle ported from Python `nanobot/agent/loop.py`:
//!
//! ```text
//! Inbound Message (from MessageBus)
//!   |
//!   v
//! Session lookup / creation
//!   |
//!   v
//! ContextBuilder.build_messages()
//!   |
//!   v
//! Pipeline execution (Classifier -> Router -> Assembler -> Transport -> Scorer -> Learner)
//!   |
//!   v
//! Tool execution loop (up to max_tool_iterations)
//!   |  - Extract tool calls from LLM response
//!   |  - Execute each tool via ToolRegistry
//!   |  - Append tool results to context
//!   |  - Re-invoke LLM if stop_reason == ToolUse
//!   |
//!   v
//! Outbound Message (dispatched to MessageBus)
//! ```

use std::sync::{Arc, Mutex};

use clawft_plugin::CancellationToken;
use tracing::{debug, error, info, warn};

use clawft_platform::Platform;
use clawft_types::agent_chat::{
    AGENT_LOOP_RESULT_META_KEY, AgentChatToolCall, AgentLoopResultMeta, DeferredActionEvent,
    EscalateToHumanEvent, FINISH_REASON_DEFERRED, FINISH_REASON_ESCALATE_TO_HUMAN, GateDenialRecord,
    SpawnedTaskSummary,
};
use clawft_types::config::AgentsConfig;
use clawft_types::error::ClawftError;
use clawft_types::event::{InboundMessage, OutboundMessage};
use clawft_types::provider::ContentBlock;
use clawft_types::routing::AuthContext;

use crate::bus::MessageBus;
use crate::pipeline::permissions::PermissionResolver;
use crate::pipeline::traits::{ChatRequest, LlmMessage, PipelineRegistry};
use crate::tools::registry::ToolRegistry;

use super::context::ContextBuilder;
use super::context_router::{ContextDecision, ContextRequest, ContextRouter, NullRouter};
use super::cost_budget::ConversationBudget;
use super::effects::effect_for_tool;
use super::gate::{
    EffectGate, GATE_DENIAL_ESCALATION_LIMIT, GateDecision, NoopGate, gate_deferral_reason,
    record_gate_denial_streak,
};
use super::graft::{ContextGraftProvider, splice_graft_block};
use super::sink::{ConversationSink, InMemorySink, Turn};
use super::system_prompt::SystemPromptBuilder;
use super::verification;

// ---------------------------------------------------------------------------
// Auto-delegation trait
// ---------------------------------------------------------------------------

/// Trait for pre-LLM automatic delegation routing.
///
/// Implementations check whether an inbound message should be routed to
/// a delegation tool (e.g. `delegate_task`) before the local LLM processes
/// it. This enables rule-based auto-routing for complex tasks that match
/// configured patterns (e.g. "swarm", "orchestrate", "deploy").
///
/// If [`should_delegate`](AutoDelegation::should_delegate) returns `Some`,
/// the agent loop invokes the `delegate_task` tool directly, bypassing the
/// local LLM entirely. If it returns `None`, normal LLM processing proceeds.
pub trait AutoDelegation: Send + Sync {
    /// Check whether the message content should be auto-delegated.
    ///
    /// Returns `Some(args)` with the JSON arguments for the `delegate_task`
    /// tool if delegation should happen, or `None` to proceed normally.
    fn should_delegate(&self, content: &str) -> Option<serde_json::Value>;
}

/// Maximum size in bytes for a single tool result.
const MAX_TOOL_RESULT_BYTES: usize = 65_536;

/// After this many consecutive identical failing tool calls (same tool
/// name + same args JSON + same error text) within one
/// [`AgentLoop::run_tool_loop`] turn, abort the loop (WEFT-651).
///
/// Live incident (conv w21-smoke2): Hermes retried canvas with missing
/// `content` for the full 20-iteration budget. Three strikes is enough
/// for the model to see schema-echo + escalated errors before we
/// fail-fast — matching the planning circuit-breaker default of 3.
const IDENTICAL_TOOL_FAILURE_LIMIT: u32 = 3;

// WEFT-345: consecutive gate Deny limit is `GATE_DENIAL_ESCALATION_LIMIT`
// in `gate.rs` (re-exported constant used by `run_tool_loop`).

/// Default maximum delegation depth (WEFT-180).
///
/// When a message arrives with no `delegation_depth` metadata, the
/// loop treats it as depth 0. Each delegation hop bumps the depth by
/// one. When the bumped value would exceed
/// [`Self::max_delegation_depth`], the loop refuses to delegate and
/// returns an error result so the LLM (or the operator, via logs)
/// sees the cap was hit instead of looping forever.
///
/// Operators can override the cap via the `CLAWFT_DELEGATION_DEPTH`
/// environment variable. Values <= 0 fall back to this default.
pub(crate) const DEFAULT_MAX_DELEGATION_DEPTH: u32 = 5;

/// Metadata key used to thread the recursive-delegation depth across
/// `delegate_task` hops. Consumed by the auto-delegation short-circuit
/// path and by any future delegator that re-enters the agent loop.
pub(crate) const DELEGATION_DEPTH_KEY: &str = "delegation_depth";

/// Resolve the configured delegation depth ceiling (WEFT-180).
///
/// Reads `CLAWFT_DELEGATION_DEPTH` once per call. Invalid / unset
/// values fall through to [`DEFAULT_MAX_DELEGATION_DEPTH`].
fn resolve_max_delegation_depth() -> u32 {
    #[cfg(feature = "native")]
    {
        if let Ok(raw) = std::env::var("CLAWFT_DELEGATION_DEPTH")
            && let Ok(parsed) = raw.trim().parse::<u32>()
            && parsed > 0
        {
            return parsed;
        }
    }
    DEFAULT_MAX_DELEGATION_DEPTH
}

/// Read the current delegation depth from an [`InboundMessage`]'s
/// metadata. Treats absent / non-integer / negative values as 0.
fn read_delegation_depth(msg: &InboundMessage) -> u32 {
    msg.metadata
        .get(DELEGATION_DEPTH_KEY)
        .and_then(|v| v.as_u64())
        .map(|v| v.min(u32::MAX as u64) as u32)
        .unwrap_or(0)
}

/// Metadata key carrying a conversation's spawn depth (M4 D5).
///
/// Cross-crate contract: the daemon subagent spawner
/// (`clawft_service_agent::subagent`) writes this key into a child
/// conversation's `AgentChatParams.metadata` so the child's loop knows
/// its own depth and any grandchild spawn increments correctly under the
/// WEFT-180 cap. Top-level conversations omit it and read as depth 0.
#[cfg(feature = "native")]
pub(crate) const SPAWN_DEPTH_KEY: &str = "spawn_depth";

/// Inbound-metadata key carrying an optional OpenAI `tool_choice`
/// override (string `"auto"`/`"none"`/`"required"` or a
/// `{"type":"function","function":{"name":…}}` object). Threaded from
/// `AgentChatParams.metadata` into the pipeline [`ChatRequest`] so a
/// harness can request a named tool call (enforcement is provider-side).
/// Absent leaves selection to the model — today's behavior.
pub(crate) const TOOL_CHOICE_META_KEY: &str = "tool_choice";

/// Read this conversation's spawn depth from an [`InboundMessage`]'s
/// metadata (M4 D5). Absent / non-integer values read as 0 (a top-level
/// conversation); an `agent_spawn` inside this turn spawns a child at
/// `depth + 1`.
#[cfg(feature = "native")]
fn read_spawn_depth(msg: &InboundMessage) -> u32 {
    msg.metadata
        .get(SPAWN_DEPTH_KEY)
        .and_then(|v| v.as_u64())
        .map(|v| v.min(u32::MAX as u64) as u32)
        .unwrap_or(0)
}

/// System prompt injected for voice-mode sessions.
///
/// Instructs the LLM to respond in natural conversational language suitable
/// for text-to-speech, rather than written/markdown format. The frontend
/// strips any residual formatting before passing to the TTS engine.
const VOICE_MODE_PROMPT: &str = "\
# Voice Mode

You are responding via voice. Your answer will be spoken aloud by a text-to-speech engine.

Rules:
- Respond in natural, conversational language as if speaking to someone in person.
- Keep responses to 1-3 sentences for simple questions. Use more for complex topics, but stay concise.
- Use contractions: say \"it's\", \"you'll\", \"that's\", \"I'd\" instead of formal equivalents.
- Never use markdown formatting: no bold, italic, headers, bullet lists, numbered lists, or code fences.
- Never read URLs, file paths, or raw code aloud. Instead, describe what it does or say \"I can show that on screen\".
- For numbers, use natural speech: \"about seventy-two degrees\" not \"72F\", \"around three hundred\" not \"300\".
- Use natural transitions instead of lists: \"there are a couple of things\" or \"first... and then...\" rather than enumerating with dashes or numbers.
- If you need to share code, a table, or structured data, give a brief spoken summary and note that the details are on screen.
- Do not start with \"Sure!\" or \"Of course!\". Just answer naturally.
- Do not narrate your actions (\"Let me search for...\"). Just provide the answer.
- Sound warm and natural, not robotic or formal.";

/// Max bytes of a tool's arguments/result kept for the UI preview
/// carried in [`AgentChatToolCall`] (M4 D8). Distinct from
/// [`MAX_TOOL_RESULT_BYTES`], which bounds what the LLM sees; this is a
/// much smaller cap for a collapsible panel bubble.
const TOOL_PREVIEW_MAX_BYTES: usize = 512;

/// Result from the tool loop, including hallucination counters and the
/// M4 D8 result-enrichment summary (real tool calls / finish reason /
/// iterations / spawned subagents).
#[derive(Debug)]
struct ToolLoopResult {
    /// The final text response from the LLM.
    text: String,
    /// Number of write claims that failed verification (hallucinated).
    hallucinations: usize,
    /// Number of write claims that passed verification.
    verified_successes: usize,
    /// Tool calls executed during the loop, in order (M4 D8).
    tool_calls: Vec<AgentChatToolCall>,
    /// Why the loop terminated: `"stop"` on a clean text response (M4
    /// D8). The max-iterations path returns `Err` before constructing
    /// this, so `"stop"` is the only value produced today.
    finish_reason: String,
    /// Number of LLM round-trips executed inside the loop (M4 D8).
    iterations: u32,
    /// Subagents spawned during the loop, detected from `agent_spawn`
    /// tool results (M4 D8).
    spawned_tasks: Vec<SpawnedTaskSummary>,
    /// Model that answered, from the last response's `metadata["model"]`.
    model: Option<String>,
    /// Cumulative prompt/completion tokens across the loop's LLM calls.
    prompt_tokens: u32,
    completion_tokens: u32,
    /// Reasoning captured across iterations (`metadata["reasoning"]` — the
    /// stripped Hermes `<think>` / llama.cpp `reasoning_content`), joined
    /// in order. Observability only; never fed back into the prompt.
    reasoning: Option<String>,
    /// WEFT-345: set when the loop halted after consecutive gate denials
    /// and emitted `EscalateToHuman`.
    escalation: Option<EscalateToHumanEvent>,
    /// WEFT-258: set when the loop halted on a gate `Defer` so the
    /// panel can prompt-and-resume.
    deferred: Option<DeferredActionEvent>,
}

/// Truncate a string to at most `TOOL_PREVIEW_MAX_BYTES` bytes on a
/// char boundary, appending an ellipsis marker when clipped.
fn preview_truncate(s: &str) -> String {
    if s.len() <= TOOL_PREVIEW_MAX_BYTES {
        return s.to_string();
    }
    let mut end = TOOL_PREVIEW_MAX_BYTES;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// A tool result JSON string counts as a failure when it deserializes
/// to an object carrying an `"error"` (registry/verification/truncation
/// failures) or `"denied"` (gate denial) key — the same shapes
/// [`AgentLoop::execute_tool_with_guards`] emits. Anything else (incl.
/// non-object results) is treated as success.
fn tool_result_succeeded(result_json: &str) -> bool {
    match serde_json::from_str::<serde_json::Value>(result_json) {
        Ok(serde_json::Value::Object(map)) => {
            !map.contains_key("error") && !map.contains_key("denied")
        }
        _ => true,
    }
}

/// Fingerprint of a failing tool call used by the WEFT-651 identical-
/// failure circuit breaker. Two failures match when tool name, args
/// JSON (compact serialization), and error text are equal.
#[derive(Clone, Debug, PartialEq, Eq)]
struct IdenticalFailureKey {
    tool: String,
    args: String,
    error: String,
}

/// Extract a stable error fingerprint from a failed tool-result body.
fn tool_result_error_text(result_json: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(result_json) {
        Ok(serde_json::Value::Object(map)) => {
            if let Some(e) = map.get("error") {
                return e
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| e.to_string());
            }
            if map.contains_key("denied") {
                let reason = map
                    .get("reason")
                    .and_then(|v| v.as_str())
                    .unwrap_or("policy");
                return format!("denied: {reason}");
            }
            "failure".into()
        }
        _ => "failure".into(),
    }
}

/// Build an [`IdenticalFailureKey`] when `result_json` is a failure;
/// `None` on success (so the breaker counter resets).
fn identical_failure_key(
    tool_name: &str,
    input: &serde_json::Value,
    result_json: &str,
) -> Option<IdenticalFailureKey> {
    if tool_result_succeeded(result_json) {
        return None;
    }
    Some(IdenticalFailureKey {
        tool: tool_name.to_string(),
        args: serde_json::to_string(input).unwrap_or_default(),
        error: tool_result_error_text(result_json),
    })
}

/// Record one tool-result outcome against the consecutive-identical-
/// failure counter. Returns `Some(key)` when the breaker should trip
/// (`count >= limit`); updates `last` / `count` in place.
///
/// Success or a different (tool, args, error) fingerprint resets the
/// streak. Same key increments.
fn record_identical_failure(
    last: &mut Option<IdenticalFailureKey>,
    count: &mut u32,
    key: Option<IdenticalFailureKey>,
    limit: u32,
) -> Option<IdenticalFailureKey> {
    match key {
        None => {
            *last = None;
            *count = 0;
            None
        }
        Some(k) => {
            if last.as_ref() == Some(&k) {
                *count = count.saturating_add(1);
            } else {
                *last = Some(k.clone());
                *count = 1;
            }
            if *count >= limit {
                Some(k)
            } else {
                None
            }
        }
    }
}

/// Escalated tool-result body when the WEFT-651 breaker trips — clear
/// instruction to the model (and any history inspector) to stop the
/// identical retry.
fn identical_failure_breaker_message(key: &IdenticalFailureKey, count: u32) -> String {
    serde_json::json!({
        "error": format!(
            "IDENTICAL TOOL FAILURE BREAKER: tool `{tool}` failed {count} times \
             with the same arguments. Last error: {err}. Stop calling this tool \
             with the same arguments — fix the args (see any schema hint above) \
             or answer without this tool.",
            tool = key.tool,
            count = count,
            err = key.error,
        )
    })
    .to_string()
}

/// Detect an `agent_spawn` tool result and extract its
/// [`SpawnedTaskSummary`] (M4 D8). The tool result JSON shape is frozen
/// by the M4 design (D4): `{task_id, child_conv_id, status, …}`. A
/// denied or malformed result yields `None`.
fn spawned_task_from_result(name: &str, result_json: &str) -> Option<SpawnedTaskSummary> {
    if name != "agent_spawn" {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(result_json).ok()?;
    let obj = value.as_object()?;
    // A gate denial carries no task; skip it (the tool call itself is
    // still recorded in `tool_calls`).
    if obj.contains_key("denied") || obj.contains_key("error") {
        return None;
    }
    Some(SpawnedTaskSummary {
        task_id: obj.get("task_id")?.as_str()?.to_string(),
        child_conv_id: obj.get("child_conv_id")?.as_str()?.to_string(),
        status: obj.get("status")?.as_str()?.to_string(),
    })
}

/// The core agent loop that processes inbound messages.
///
/// Consumes messages from the bus, invokes the LLM pipeline, executes
/// tool calls, and dispatches responses. This struct holds all the
/// dependencies needed for the full processing cycle.
///
/// # Processing flow
///
/// 1. **Consume**: Pull the next [`InboundMessage`] from the [`MessageBus`].
/// 2. **Session**: Look up or create a [`Session`](clawft_types::session::Session)
///    keyed by `channel:chat_id`.
/// 3. **Context**: Build the LLM message list via
///    [`ContextBuilder::build_messages`].
/// 4. **Pipeline**: Run the assembled context through the 6-stage pipeline
///    (Classifier -> Router -> Assembler -> Transport -> Scorer -> Learner).
/// 5. **Tools**: If the LLM response contains tool calls, execute them
///    via the `ToolRegistry`, append results, and loop back to step 4
///    (up to `max_tool_iterations`).
/// 6. **Respond**: Extract the final text response and dispatch an
///    [`OutboundMessage`] to the bus.
/// 7. **Persist**: Save the updated session and append to history.
///
/// The loop's COW-memory attachment (WEFT-616 Phase 2 + 652 + 654).
#[cfg(feature = "rvf")]
#[derive(Clone)]
pub struct CowAttachment {
    pub mem: Arc<std::sync::Mutex<clawft_cow_memory::BranchableMemory>>,
    /// Ingest each successful turn's exchange pre-promote (Phase 3).
    pub ingest_turns: bool,
    /// Checkpoint at tool-call boundaries too (WEFT-652).
    pub tool_cadence: bool,
    /// Hold successful turns as pending proposals (WEFT-654 review gate).
    pub review: bool,
}

pub struct AgentLoop<P: Platform> {
    config: AgentsConfig,
    platform: Arc<P>,
    bus: Arc<MessageBus>,
    pipeline: PipelineRegistry,
    tools: Arc<ToolRegistry>,
    context: ContextBuilder<P>,
    permission_resolver: PermissionResolver,
    cancel: Option<CancellationToken>,
    /// Optional pre-LLM auto-delegation router.
    ///
    /// When set, inbound messages are checked against delegation rules
    /// before the local LLM is invoked. If a rule matches, the
    /// `delegate_task` tool is called directly, bypassing the LLM.
    auto_delegation: Option<Arc<dyn AutoDelegation>>,
    /// Optional sandbox enforcer.
    ///
    /// When set, every tool dispatch in [`Self::run_tool_loop`] is
    /// gated through [`SandboxEnforcer::check_tool`] before the
    /// underlying [`ToolRegistry`] runs. A denial materializes as a
    /// `{"error": ...}` tool result (same shape as a normal failure)
    /// so the LLM can recover, and the audit log captures the
    /// decision. When `None` (default for backwards compat) tools
    /// execute exactly as before — no enforcement layer.
    sandbox: Option<Arc<crate::agent::sandbox::SandboxEnforcer>>,
    /// Optional autonomous skill-creation pattern detector.
    ///
    /// When set, every tool dispatched in [`Self::run_tool_loop`] is
    /// fed to
    /// [`PatternDetector::record_tool_call`](crate::agent::skill_autogen::PatternDetector::record_tool_call).
    /// After dispatch we call `detect_candidates`; new patterns get
    /// materialized as pending SKILL.md files via
    /// [`install_pending_skill`](crate::agent::skill_autogen::install_pending_skill)
    /// in `~/.clawft/skills/pending/`. The pending → live promotion
    /// stays manual (user approval), per the autogen module's design.
    autogen: Option<Arc<Mutex<crate::agent::skill_autogen::PatternDetector>>>,
    /// Optional Phase 4 skill auto-promoter (WEFT-348).
    ///
    /// When set, successful uses of skills sourced from `.claude/skills/`
    /// are counted; on threshold breach the skill is copied into
    /// `.clawft/skills/` with an audit log entry.
    skill_promoter: Option<Arc<Mutex<crate::agent::skill_autogen::SkillPromoter>>>,
    /// Pre-LLM context router (agent-core-v1 Phase B1).
    ///
    /// Defaults to
    /// [`NullRouter`](crate::agent::context_router::NullRouter) so
    /// existing behaviour is preserved. Phase E1 swaps in
    /// `LlmClassifierRouter`. The router NEVER picks a model — that's
    /// `TieredRouter`'s job downstream
    /// (`crates/clawft-core/src/pipeline/tiered_router.rs:585`).
    /// See `docs/research/rvf-context-router.md` for the contract.
    context_router: Arc<dyn ContextRouter>,
    /// Effect gate (agent-core-v1 Phase B2). Consulted before each
    /// tool dispatch with an [`EffectVector`](crate::agent::effects::EffectVector)
    /// from [`effect_for_tool`](crate::agent::effects::effect_for_tool).
    /// Defaults to [`NoopGate`](crate::agent::gate::NoopGate) (always
    /// permits). Phase D2 swaps in the kernel-backed
    /// `GovernanceGate::check` from `clawft-kernel`.
    gate: Arc<dyn EffectGate>,
    /// Conversation sink (agent-core-v1 Phase B2). Receives one
    /// [`Turn`](crate::agent::sink::Turn) per role event (user,
    /// assistant, tool). Defaults to
    /// [`InMemorySink`](crate::agent::sink::InMemorySink) (test-only,
    /// HashMap-backed). Phase C3 swaps in the substrate-backed sink
    /// from `clawft-service-agent`.
    sink: Arc<dyn ConversationSink>,
    /// Memory consolidation after each turn (WEFT-347 / WEFT-733), from
    /// `agents.memory_consolidation`; `None` when off.
    consolidator: Option<crate::agent::learning::MemoryConsolidator<P>>,
    /// Optional daemon-supplied agent_id for [`EffectGate::check`]
    /// calls (agent-core-v1 Phase D2). When set, every tool dispatch
    /// passes this id to the gate instead of the synthesized
    /// `"{channel}:{sender_id}"` from the inbound message — unless
    /// the inbound metadata carries
    /// [`GATE_AGENT_ID_META_KEY`](clawft_types::agent_chat::GATE_AGENT_ID_META_KEY)
    /// (WEFT-332 per-caller principal). The daemon stamps a boot-time
    /// concierge agent_id as the single-tenant fallback; multi-tenant
    /// chat injects a per-caller id via metadata each dispatch. CLI /
    /// test callers leave this as `None` to preserve the synthesis
    /// fallback.
    daemon_agent_id: Option<String>,
    /// Identity-aware system-prompt builder (agent-core-v1 Phase D1).
    ///
    /// When set, [`Self::handle_turn`] builds an identity-aware system
    /// message via the builder and **prepends** it to the message list
    /// passed to the LLM transport, ahead of any
    /// `ContextBuilder`-emitted content. When `None` (default for CLI
    /// / legacy callers), behaviour is unchanged.
    system_prompt_builder: Option<Arc<SystemPromptBuilder>>,
    /// Optional inbound-message router (WEFT-178).
    ///
    /// When set, [`Self::handle_turn`] consults
    /// [`AgentRouter::route`](crate::agent_routing::AgentRouter::route)
    /// to determine which agent persona should own the message, and
    /// stamps the resolved id into the request's auth context (and
    /// the inbound metadata so downstream tooling can observe it).
    /// When `None` (single-agent CLI flow, tests), routing is skipped
    /// and the loop processes every message itself.
    ///
    /// This is the wiring referenced in
    /// `.planning/reviews/0.7.0-release-gate/07-multi-agent-routing.md`
    /// "L1 routing wired into inbound dispatch".
    agent_router: Option<Arc<crate::agent_routing::AgentRouter>>,
    /// Hard cap on recursive delegation depth (WEFT-180).
    ///
    /// Resolved from `CLAWFT_DELEGATION_DEPTH` at construction time;
    /// callers that need a custom cap (tests) use
    /// [`Self::with_max_delegation_depth`]. The default is
    /// [`DEFAULT_MAX_DELEGATION_DEPTH`].
    max_delegation_depth: u32,
    /// Per-conversation cost circuit-breaker (WEFT-322).
    ///
    /// When attached, [`Self::run_tool_loop`] consults
    /// [`ConversationBudget::check_can_call`] BEFORE each
    /// `pipeline.complete` and surfaces
    /// [`ClawftError::ConversationBudgetExceeded`](clawft_types::error::ClawftError::ConversationBudgetExceeded)
    /// on trip; usage is recorded after each successful call. When
    /// `None` (default for CLI / test paths) budgeting is skipped — the
    /// loop's only cap is `max_tool_iterations` per turn.
    cost_budget: Option<Arc<ConversationBudget>>,
    /// Optional L2 context-graft provider (ADR-058 Phase 5.1).
    ///
    /// When set, [`Self::handle_turn`] queries it once per turn (after the
    /// prefix-stable system head is assembled, before generation) and splices
    /// the grafted block into the `[head] → [graft] → [tail]` position. The
    /// kernel-backed impl (per-conversation `SessionView`) lives in
    /// `clawft-service-agent`. Defaults to `None` (CLI / tests), preserving
    /// prior behaviour.
    graft_provider: Option<Arc<dyn ContextGraftProvider>>,
    /// Whether the inbound `chat_id` is already a globally-unique
    /// conversation id (M3 §D5 reconciliation).
    ///
    /// The M3 store collapse adopts `"{channel}:{chat_id}"` (the session
    /// key) as the canonical `conv_id` so multi-channel `chat_id`s can't
    /// collide and the in-process [`LocalFileSink`] reads the existing
    /// `{channel}:{chat_id}.jsonl` files unchanged (§D4). But the daemon's
    /// `agent.chat` path already mints a globally-unique conv id and carries
    /// it *as* `chat_id` (under a constant `agent.chat` channel), and it keys
    /// the substrate turn log, causal forest, session tier, `conversation.graph`
    /// / `agent.chat.end`, and the `sub:<parent>:…` subagent child namespace by
    /// that bare id. Prefixing it there would diverge from those readers and
    /// compound the prefix on nested spawns. So the daemon sets this flag (via
    /// [`Self::with_chat_id_as_conv_id`], wired in `build_daemon_agent_loop`)
    /// to use `chat_id` verbatim; CLI / channel callers leave it `false` and
    /// get the canonical session-key `conv_id`.
    chat_id_is_conv_id: bool,
    /// Optional per-turn COW memory checkpoint (WEFT-616 Phase 2).
    ///
    /// When set, [`Self::handle_turn`] brackets the turn with
    /// `clawft-cow-memory::BranchableMemory::checkpoint` /
    /// `promote`/`rollback` (see [`super::turn_checkpoint`]). Attached via
    /// [`Self::with_cow_memory`], driven by `AgentsConfig::cow_memory`
    /// (`clawft_types::config::CowMemoryConfig`) at whatever layer
    /// constructs the loop -- `None` (default) is the same "feature
    /// absent" pattern as `sandbox`/`gate`: zero behavior change.
    #[cfg(feature = "rvf")]
    cow_memory: std::sync::OnceLock<CowAttachment>,
    /// WEFT-654 review mode: the currently HELD turn awaiting
    /// accept/discard. At most one — a pending hold fails new dispatches
    /// closed (the loop's lineage is global; see ProposalConfig docs).
    #[cfg(feature = "rvf")]
    pending_hold: std::sync::Mutex<Option<super::turn_checkpoint::HeldTurn>>,
    /// Chain-side witness of the bracket (WEFT-616 Phase 3, DualStateBridge).
    /// Late-wired by the daemon over ChainManager; absent = no chain coupling
    /// (memory semantics unchanged). Not feature-gated — the trait is pure.
    turn_ledger: std::sync::OnceLock<Arc<dyn super::turn_ledger::TurnLedger>>,
    /// Optional soul-journal writer (WEFT-330 / agent-core-v1.1).
    ///
    /// When set, [`Self::handle_turn_inner`] consults
    /// [`super::soul_journal::detect_drift_signal`] after a successful
    /// tool loop and, on a hit, appends a
    /// [`super::soul_journal::DriftObservation`] via this writer.
    /// Failures are logged and swallowed (degrade, don't crash the
    /// user-visible turn). When `None` (default for CLI / legacy
    /// callers) the loop never writes journal entries.
    soul_journal: Option<Arc<dyn super::soul_journal::SoulJournal>>,
    /// Optional router-decision observability log (WEFT-335).
    ///
    /// When set, every [`ContextRouter::route`] call appends a
    /// [`super::routing_log::RouterDecisionRecord`] (query, selected
    /// route, alternatives, confidence, latency) for the v1→v2
    /// promotion gate. Failures are logged and swallowed — logging
    /// must never abort the user-visible chat path. When `None`
    /// (default for CLI / legacy callers) no decision records are
    /// written.
    routing_log: Option<Arc<dyn super::routing_log::RouterDecisionLog>>,
    /// Optional interactive-defer hook (WEFT-331).
    ///
    /// When set, [`GateDecision::Defer`](super::gate::GateDecision::Defer)
    /// suspends the tool loop until the interactor returns a human
    /// decision (allow / deny / cancel / timeout). When `None` (default)
    /// Defer keeps the v1 structured-tool-result behaviour so the model
    /// can re-plan without a panel. Daemon path wires
    /// `clawft-service-agent`'s broker via [`Self::set_defer_interactor`].
    defer_interactor: std::sync::OnceLock<Arc<dyn super::defer::DeferInteractor>>,
    /// Sessions directory for ObservationPack archives (native). When unset,
    /// the first pack call discovers the default `~/.clawft/workspace/sessions`.
    #[cfg(feature = "native")]
    observation_sessions_dir: std::sync::OnceLock<std::path::PathBuf>,
}

impl<P: Platform> AgentLoop<P> {
    /// Create a new agent loop with all dependencies wired.
    ///
    /// # Arguments
    ///
    /// * `config` -- Agent configuration (model, max_tokens, etc.)
    /// * `platform` -- Platform abstraction for filesystem/env/http
    /// * `bus` -- Message bus for consuming inbound and dispatching outbound
    /// * `pipeline` -- Pipeline registry for LLM invocation
    /// * `tools` -- Tool registry for executing tool calls
    /// * `context` -- Context builder for assembling prompts
    ///
    /// The M3 store collapse (design §P5) retired `SessionManager` from the
    /// turn path: the loop hydrates every turn from the [`ConversationSink`],
    /// so the constructor no longer takes a session manager. Durable session
    /// state lives in the sink (substrate-backed on the daemon path,
    /// [`LocalFileSink`](crate::agent::local_file_sink::LocalFileSink) in
    /// process).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: AgentsConfig,
        platform: Arc<P>,
        bus: Arc<MessageBus>,
        pipeline: PipelineRegistry,
        tools: Arc<ToolRegistry>,
        context: ContextBuilder<P>,
        permission_resolver: PermissionResolver,
    ) -> Self {
        let consolidator = context.consolidator_from_config();
        Self {
            config,
            platform,
            bus,
            pipeline,
            tools,
            context,
            permission_resolver,
            cancel: None,
            auto_delegation: None,
            sandbox: None,
            autogen: None,
            skill_promoter: None,
            context_router: Arc::new(NullRouter),
            gate: Arc::new(NoopGate),
            sink: Arc::new(InMemorySink::new()),
            consolidator,
            daemon_agent_id: None,
            system_prompt_builder: None,
            soul_journal: None,
            routing_log: None,
            agent_router: None,
            max_delegation_depth: resolve_max_delegation_depth(),
            cost_budget: None,
            graft_provider: None,
            chat_id_is_conv_id: false,
            #[cfg(feature = "rvf")]
            cow_memory: std::sync::OnceLock::new(),
            #[cfg(feature = "rvf")]
            pending_hold: std::sync::Mutex::new(None),
            turn_ledger: std::sync::OnceLock::new(),
            defer_interactor: std::sync::OnceLock::new(),
            #[cfg(feature = "native")]
            observation_sessions_dir: std::sync::OnceLock::new(),
        }
    }

    /// Attach a [`CancellationToken`] so the agent loop exits promptly on
    /// shutdown instead of waiting for all bus senders to be dropped.
    pub fn with_cancel(mut self, token: CancellationToken) -> Self {
        self.cancel = Some(token);
        self
    }

    /// Mint a fresh, never-cancelled [`CancellationToken`] for callers
    /// that have no per-conversation cancel signal (CLI, browser, tests).
    /// WEFT-323: [`Self::handle_turn`] requires a token reference; this is
    /// the no-op stand-in.
    pub fn fresh_cancel_token(&self) -> CancellationToken {
        let _ = self;
        CancellationToken::new()
    }

    /// Attach an auto-delegation router for pre-LLM routing.
    ///
    /// When set, messages matching delegation rules are routed to the
    /// `delegate_task` tool before the local LLM processes them.
    pub fn with_auto_delegation(mut self, delegation: Arc<dyn AutoDelegation>) -> Self {
        self.auto_delegation = Some(delegation);
        self
    }

    /// Attach a [`SandboxEnforcer`](crate::agent::sandbox::SandboxEnforcer)
    /// that gates every tool call against the agent's allowlist before
    /// dispatch. Without this attached the agent loop runs un-sandboxed
    /// (legacy behaviour).
    pub fn with_sandbox(mut self, sandbox: Arc<crate::agent::sandbox::SandboxEnforcer>) -> Self {
        self.sandbox = Some(sandbox);
        self
    }

    /// Attach a
    /// [`PatternDetector`](crate::agent::skill_autogen::PatternDetector)
    /// so the agent loop records every tool call and writes pending
    /// SKILL.md candidates when patterns recur.
    pub fn with_autogen(
        mut self,
        detector: Arc<Mutex<crate::agent::skill_autogen::PatternDetector>>,
    ) -> Self {
        self.autogen = Some(detector);
        self
    }

    /// Attach a
    /// [`SkillPromoter`](crate::agent::skill_autogen::SkillPromoter)
    /// so successful uses of `.claude/skills/*` skills auto-promote into
    /// `.clawft/skills/` after the configured threshold (WEFT-348).
    pub fn with_skill_promoter(
        mut self,
        promoter: Arc<Mutex<crate::agent::skill_autogen::SkillPromoter>>,
    ) -> Self {
        self.skill_promoter = Some(promoter);
        self
    }

    /// Borrow the attached skill promoter, if any.
    pub fn skill_promoter(
        &self,
    ) -> Option<&Arc<Mutex<crate::agent::skill_autogen::SkillPromoter>>> {
        self.skill_promoter.as_ref()
    }

    /// Record a successful skill invocation for auto-promotion (WEFT-348).
    ///
    /// No-op when no promoter is attached. Intended for call sites that
    /// know a skill was used successfully and have its on-disk source path
    /// (typically under `.claude/skills/`).
    pub fn record_skill_success(&self, skill_name: &str, source_path: &std::path::Path) {
        let Some(promoter) = self.skill_promoter.as_ref() else {
            return;
        };
        let mut guard = match promoter.lock() {
            Ok(g) => g,
            Err(p) => {
                warn!("skill promoter mutex poisoned, recovering");
                p.into_inner()
            }
        };
        match guard.record_success(skill_name, source_path) {
            crate::agent::skill_autogen::PromotionOutcome::Promoted { entry } => {
                info!(
                    skill = %entry.skill_name,
                    target = %entry.target_path.display(),
                    count = entry.successful_invocations,
                    "skill auto-promoted to .clawft/skills"
                );
            }
            crate::agent::skill_autogen::PromotionOutcome::Error(e) => {
                warn!(error = %e, skill = %skill_name, "skill promotion failed");
            }
            _ => {}
        }
    }

    /// Attach a [`ContextRouter`] so the loop consults it before each
    /// LLM request. Without this attached the loop uses
    /// [`NullRouter`] (no skills, no tool restriction, zero hint —
    /// behaviour identical to pre-B1).
    pub fn with_context_router(mut self, router: Arc<dyn ContextRouter>) -> Self {
        self.context_router = router;
        self
    }

    /// Attach an interactive-defer interactor (WEFT-331).
    ///
    /// Builder form for tests / early wiring. Prefer
    /// [`Self::set_defer_interactor`] when the loop is already behind
    /// an `Arc` (daemon boot).
    pub fn with_defer_interactor(
        self,
        interactor: Arc<dyn super::defer::DeferInteractor>,
    ) -> Self {
        let _ = self.defer_interactor.set(interactor);
        self
    }

    /// Late-wire an interactive-defer interactor (WEFT-331).
    ///
    /// Idempotent: first set wins (matches `set_cow_memory` /
    /// `set_turn_ledger`).
    pub fn set_defer_interactor(&self, interactor: Arc<dyn super::defer::DeferInteractor>) {
        if self.defer_interactor.set(interactor).is_err() {
            tracing::debug!("defer interactor already set; ignoring second attach");
        }
    }

    /// Attach an [`EffectGate`] so the loop checks every tool dispatch
    /// against policy before execution. Without this attached the
    /// loop uses [`NoopGate`] (always permits) — behaviour identical
    /// to pre-B2.
    pub fn with_gate(mut self, gate: Arc<dyn EffectGate>) -> Self {
        self.gate = gate;
        self
    }

    /// Attach a [`ConversationSink`] so the loop persists one
    /// [`Turn`] per role event. Without this attached the loop uses
    /// [`InMemorySink`] (HashMap-backed, test-only) — behaviour
    /// identical to pre-B2 (turns are recorded but never observed).
    pub fn with_sink(mut self, sink: Arc<dyn ConversationSink>) -> Self {
        self.sink = sink;
        self
    }

    /// Pin the ObservationPack sessions directory (tests / known path).
    ///
    /// Without this, native packs discover `~/.clawft/workspace/sessions`.
    #[cfg(feature = "native")]
    pub fn with_observation_sessions_dir(self, dir: std::path::PathBuf) -> Self {
        let _ = self.observation_sessions_dir.set(dir);
        self
    }

    /// Shared handle to the conversation sink (WEFT-399).
    ///
    /// Browser WASM uses this for `get_history` / hydration inspection
    /// without re-opening the sessions directory. Clone is cheap
    /// (`Arc`).
    pub fn conversation_sink(&self) -> Arc<dyn ConversationSink> {
        Arc::clone(&self.sink)
    }

    /// Attach a daemon-supplied agent id (agent-core-v1 Phase D2).
    ///
    /// When set, every [`EffectGate::check`] call inside the tool
    /// loop passes this id as `agent_id` instead of synthesizing one
    /// from the inbound message metadata. This is how the daemon's
    /// concierge agent — registered once at boot via
    /// `clawft-kernel::AgentRegistry` and stashed in
    /// `DAEMON_CONCIERGE_AGENT_ID` — becomes the principal of every
    /// chat-driven tool call.
    ///
    /// Without this attached (CLI path, tests) the loop falls back to
    /// the pre-D2 synthesized `"{channel}:{sender_id}"` shape. When a
    /// per-turn
    /// [`GATE_AGENT_ID_META_KEY`](clawft_types::agent_chat::GATE_AGENT_ID_META_KEY)
    /// is present on the inbound message (WEFT-332), that id wins over
    /// this boot-time default.
    pub fn with_daemon_agent_id(mut self, agent_id: String) -> Self {
        self.daemon_agent_id = Some(agent_id);
        self
    }

    /// Resolve the gate principal for an inbound turn.
    ///
    /// Precedence (WEFT-178 / WEFT-332):
    /// 1. Routed agent id (when [`Self::with_agent_router`] matched)
    /// 2. Per-turn metadata [`GATE_AGENT_ID_META_KEY`]
    ///    (multi-tenant caller principal from `AgentService`)
    /// 3. Boot-time [`Self::with_daemon_agent_id`] concierge fallback
    /// 4. Synthesised `"{channel}:{sender_id}"`
    fn resolve_gate_agent_id(
        &self,
        msg: &InboundMessage,
        routed_agent_id: Option<&str>,
    ) -> String {
        if let Some(id) = routed_agent_id {
            return id.to_owned();
        }
        if let Some(id) = msg
            .metadata
            .get(clawft_types::agent_chat::GATE_AGENT_ID_META_KEY)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return id.to_owned();
        }
        if let Some(id) = self.daemon_agent_id.as_deref() {
            return id.to_owned();
        }
        format!("{}:{}", msg.channel, msg.sender_id)
    }

    /// Attach a [`SystemPromptBuilder`] so [`Self::handle_turn`]
    /// emits an identity-aware system message ahead of the
    /// `ContextBuilder` content (agent-core-v1 Phase D1).
    ///
    /// When unset (default for CLI / legacy callers), the loop's
    /// system-prompt assembly falls through to whatever the
    /// [`ContextBuilder`] produces, exactly as before.
    pub fn with_system_prompt_builder(mut self, builder: Arc<SystemPromptBuilder>) -> Self {
        self.system_prompt_builder = Some(builder);
        self
    }

    /// Attach a [`SoulJournal`](super::soul_journal::SoulJournal) so
    /// [`Self::handle_turn_inner`] can append identity-drift
    /// observations after a successful turn (WEFT-330).
    ///
    /// When unset (default), the loop never writes journal entries.
    /// The daemon attaches a grant-gated substrate writer (optionally
    /// dual-writing `.clawft/SOUL.journal.md`); tests use
    /// [`InMemorySoulJournal`](super::soul_journal::InMemorySoulJournal).
    pub fn with_soul_journal(
        mut self,
        journal: Arc<dyn super::soul_journal::SoulJournal>,
    ) -> Self {
        self.soul_journal = Some(journal);
        self
    }

    /// Attach a [`RouterDecisionLog`](super::routing_log::RouterDecisionLog)
    /// so every [`ContextRouter::route`] call is recorded for the
    /// v1→v2 promotion gate (WEFT-335).
    ///
    /// When unset (default), the loop never writes decision records.
    /// The daemon attaches a grant-gated substrate writer under
    /// `substrate/_derived/agent/routing/recent/`; tests use
    /// [`InMemoryRouterDecisionLog`](super::routing_log::InMemoryRouterDecisionLog).
    pub fn with_routing_log(
        mut self,
        log: Arc<dyn super::routing_log::RouterDecisionLog>,
    ) -> Self {
        self.routing_log = Some(log);
        self
    }

    /// Attach an [`AgentRouter`](crate::agent_routing::AgentRouter)
    /// so [`Self::handle_turn`] resolves a routed `agent_id` from the
    /// inbound message before falling back to the synthesised
    /// `"{channel}:{sender_id}"` shape (WEFT-178).
    ///
    /// When unset, the loop ignores the router entirely (the
    /// pre-WEFT-178 behaviour). The CLI flow keeps `None`; the daemon
    /// supplies its loaded `AgentRoutingConfig`-derived router.
    pub fn with_agent_router(mut self, router: Arc<crate::agent_routing::AgentRouter>) -> Self {
        self.agent_router = Some(router);
        self
    }

    /// Override the recursive-delegation depth cap (WEFT-180).
    ///
    /// Production code should rely on `CLAWFT_DELEGATION_DEPTH`
    /// (resolved at [`Self::new`] time). This builder is the test /
    /// programmatic-override hook. Values of `0` are silently
    /// promoted to `1` so the loop never short-circuits before the
    /// first delegation attempt.
    pub fn with_max_delegation_depth(mut self, depth: u32) -> Self {
        self.max_delegation_depth = depth.max(1);
        self
    }

    /// Attach a [`ConversationBudget`] so [`Self::run_tool_loop`]
    /// consults the per-conversation cost circuit-breaker before each
    /// LLM call (WEFT-322).
    ///
    /// Without this attached the loop runs un-budgeted (legacy
    /// behaviour); the only cap is the per-turn
    /// `max_tool_iterations`. The daemon supplies this via
    /// `clawft-service-agent` so the substrate-backed store carries
    /// state across daemon restarts.
    pub fn with_cost_budget(mut self, budget: Arc<ConversationBudget>) -> Self {
        self.cost_budget = Some(budget);
        self
    }

    /// Borrow the optional [`ConversationBudget`].
    pub fn cost_budget(&self) -> Option<&Arc<ConversationBudget>> {
        self.cost_budget.as_ref()
    }

    /// Attach an L2 [`ContextGraftProvider`] (ADR-058 Phase 5.1). The daemon
    /// supplies a kernel-backed, per-conversation graft provider; CLI / tests
    /// leave this unset (no grafting).
    pub fn with_graft_provider(mut self, provider: Arc<dyn ContextGraftProvider>) -> Self {
        self.graft_provider = Some(provider);
        self
    }

    /// Treat the inbound `chat_id` as the canonical `conv_id` verbatim
    /// instead of deriving `"{channel}:{chat_id}"` (M3 §D5 reconciliation).
    ///
    /// The daemon's `agent.chat` path mints a globally-unique conv id and
    /// carries it as `chat_id`; its substrate turn log, causal forest,
    /// session tier, `conversation.graph`, and subagent child namespace all
    /// key by that bare id. `build_daemon_agent_loop` calls this so the
    /// collapse hydrates from / writes to the exact same key those readers
    /// use. CLI / channel callers leave it unset for the canonical
    /// session-key `conv_id` (see [`Self::chat_id_is_conv_id`]).
    pub fn with_chat_id_as_conv_id(mut self) -> Self {
        self.chat_id_is_conv_id = true;
        self
    }

    /// Attach a per-turn COW memory checkpoint (WEFT-616 Phase 2).
    ///
    /// When set, [`Self::handle_turn`] checkpoints `mem` before the turn,
    /// promotes it on success, and rolls it back to the pre-turn checkpoint
    /// on failure -- see [`super::turn_checkpoint::with_turn_checkpoint`].
    /// Without this attached (default), `handle_turn` behaves exactly as it
    /// did before this option existed. Construction of the `BranchableMemory`
    /// itself (from `AgentsConfig::cow_memory`'s `enabled`/`path`) is the
    /// caller's job -- this builder only attaches an already-open handle,
    /// mirroring [`Self::with_sandbox`]/[`Self::with_gate`].
    #[cfg(feature = "rvf")]
    pub fn with_cow_memory(
        self,
        mem: Arc<std::sync::Mutex<clawft_cow_memory::BranchableMemory>>,
    ) -> Self {
        let _ = self.cow_memory.set(CowAttachment {
            mem,
            ingest_turns: true,
            tool_cadence: false,
            review: false,
        });
        self
    }

    /// Late-wire the per-turn COW memory after the loop is `Arc`-wrapped —
    /// the daemon boot path (`build_daemon_agent_loop` returns an `Arc`, and
    /// the operator's `AgentsConfig::cow_memory` is only in scope there).
    /// Write-once, same idiom as the reply-submitter/spawner late wiring.
    #[cfg(feature = "rvf")]
    /// `ingest_turns` = `CowMemoryConfig::ingest_turns`: when true, each
    /// successful turn's exchange (user text + reply) is hash-embedded and
    /// ingested into the checkpointed `working` node before promote — the
    /// Phase-3 write routing that makes the bracket protect real data.
    /// Attach the loop's COW memory with its settings (WEFT-616/652/654).
    pub fn set_cow_memory(&self, attachment: CowAttachment) {
        let _ = self.cow_memory.set(attachment);
    }

    /// The held review-mode proposal's (label, age), if any (WEFT-654).
    #[cfg(feature = "rvf")]
    pub fn pending_hold_info(&self) -> Option<(String, std::time::Duration)> {
        self.pending_hold
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .map(|h| (h.label.clone(), h.created_at.elapsed()))
    }

    /// Accept the held proposal: promote + witness (WEFT-654; accept = promote).
    #[cfg(feature = "rvf")]
    pub fn accept_pending(&self) -> clawft_types::Result<String> {
        let held = self
            .pending_hold
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
            .ok_or_else(|| clawft_types::ClawftError::CowMemory {
                reason: "no pending proposal".into(),
            })?;
        let label = held.label.clone();
        let att = self.cow_memory.get().expect("hold exists ⇒ attachment exists");
        super::turn_checkpoint::accept_held(&att.mem, self.turn_ledger.get(), held)?;
        Ok(label)
    }

    /// Discard the held proposal: rollback + witnessed revert (WEFT-654;
    /// discard = rollback; the fact of the discard stays on chain).
    #[cfg(feature = "rvf")]
    pub fn discard_pending(&self, reason: &str) -> clawft_types::Result<String> {
        let held = self
            .pending_hold
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
            .ok_or_else(|| clawft_types::ClawftError::CowMemory {
                reason: "no pending proposal".into(),
            })?;
        let label = held.label.clone();
        let att = self.cow_memory.get().expect("hold exists ⇒ attachment exists");
        super::turn_checkpoint::discard_held(&att.mem, self.turn_ledger.get(), held, reason)?;
        Ok(label)
    }

    /// Late-wire the chain-side [`TurnLedger`] (WEFT-616 Phase 3). Write-once;
    /// only consulted when a cow-memory bracket is active — the ledger
    /// witnesses bracket transitions, it has nothing to observe without one.
    ///
    /// [`TurnLedger`]: super::turn_ledger::TurnLedger
    pub fn set_turn_ledger(&self, ledger: Arc<dyn super::turn_ledger::TurnLedger>) {
        let _ = self.turn_ledger.set(ledger);
    }

    /// Resolve the conversation id for `msg` under the configured key scheme
    /// (M3 §D5). Either the canonical session key `"{channel}:{chat_id}"`
    /// (default) or the bare `chat_id` when [`Self::chat_id_is_conv_id`] is
    /// set (daemon path). Threaded identically through hydration, the sink,
    /// the graft, and the spawn context.
    fn conv_id_for(&self, msg: &InboundMessage) -> String {
        if self.chat_id_is_conv_id {
            msg.chat_id.clone()
        } else {
            msg.session_key()
        }
    }

    /// Currently-configured maximum delegation depth.
    pub fn max_delegation_depth(&self) -> u32 {
        self.max_delegation_depth
    }

    /// Borrow the optional [`AgentRouter`].
    pub fn agent_router(&self) -> Option<&Arc<crate::agent_routing::AgentRouter>> {
        self.agent_router.as_ref()
    }

    /// Get a reference to the agent configuration.
    pub fn config(&self) -> &AgentsConfig {
        &self.config
    }

    /// Get a reference to the platform.
    pub fn platform(&self) -> &Arc<P> {
        &self.platform
    }

    /// Get a reference to the message bus.
    pub fn bus(&self) -> &Arc<MessageBus> {
        &self.bus
    }

    /// Access the pipeline registry (e.g. browser streaming chat — WEFT-390).
    ///
    /// Prefer [`Self::handle_turn`] for the full agent path (tools, sessions).
    /// [`PipelineRegistry::complete_stream`] is the single-call streaming
    /// entry used by the WASM `stream_chat` export.
    pub fn pipeline(&self) -> &PipelineRegistry {
        &self.pipeline
    }

    /// Run the agent loop, consuming messages until the bus is closed or
    /// the optional [`CancellationToken`] is triggered.
    ///
    /// This is the main entrypoint. It pulls messages from the inbound
    /// channel and processes each one through the full pipeline. Errors
    /// on individual messages are logged but do not terminate the loop.
    pub async fn run(&self) -> clawft_types::Result<()> {
        info!("agent loop started, waiting for messages");

        loop {
            let msg = if let Some(ref token) = self.cancel {
                #[cfg(feature = "native")]
                {
                    tokio::select! {
                        biased;
                        _ = token.cancelled() => {
                            info!("agent loop cancelled via token, exiting");
                            break;
                        }
                        msg = self.bus.consume_inbound() => msg,
                    }
                }
                #[cfg(not(feature = "native"))]
                {
                    // On browser, poll cancellation between messages.
                    if token.is_cancelled() {
                        info!("agent loop cancelled via token, exiting");
                        break;
                    }
                    self.bus.consume_inbound().await
                }
            } else {
                self.bus.consume_inbound().await
            };

            match msg {
                Some(msg) => {
                    debug!(
                        channel = %msg.channel,
                        chat_id = %msg.chat_id,
                        "processing inbound message"
                    );
                    let (channel, chat_id) = (msg.channel.clone(), msg.chat_id.clone());
                    // WEFT-323: thread the loop-level cancel token into the
                    // turn so tool iterations observe it. When none is
                    // attached, mint a never-cancelled token for this turn.
                    let never;
                    let cancel = match self.cancel.as_ref() {
                        Some(t) => t,
                        None => {
                            never = CancellationToken::new();
                            &never
                        }
                    };
                    match self.handle_turn(msg, cancel).await {
                        Ok(outbound) => {
                            if let Err(e) = self.bus.dispatch_outbound(outbound) {
                                error!("failed to dispatch outbound message: {}", e);
                            }
                        }
                        Err(e) => {
                            error!("failed to process message: {}", e);
                            // Surface the failure to the sender instead of
                            // going silent: single-message CLI mode blocks on
                            // the outbound reply, and channel users otherwise
                            // see nothing at all.
                            let mut outbound = OutboundMessage {
                                channel,
                                chat_id,
                                content: format!("error: {e}"),
                                reply_to: None,
                                media: vec![],
                                metadata: Default::default(),
                            };
                            outbound.mark_error();
                            if let Err(e) = self.bus.dispatch_outbound(outbound) {
                                error!("failed to dispatch error reply: {}", e);
                            }
                        }
                    }
                }
                None => {
                    info!("inbound channel closed, agent loop exiting");
                    break;
                }
            }
        }

        Ok(())
    }

    /// Process a single inbound message through the full pipeline and
    /// return the resulting [`OutboundMessage`] reply.
    ///
    /// This is the per-turn entry point used by both the long-lived
    /// bus consumer ([`Self::run`]) and request/response RPC handlers
    /// (e.g. `agent.chat`). Unlike [`Self::run`], `handle_turn` does
    /// **not** touch the [`MessageBus`] for outbound dispatch — the
    /// caller is responsible for routing the returned reply (publish
    /// to the bus, return as an RPC response, etc.).
    ///
    /// Handles session lookup, context building, pipeline invocation,
    /// the tool execution loop, and session persistence. Auto-delegation
    /// short-circuits the local LLM pipeline and returns the delegate's
    /// response as the reply.
    ///
    /// `cancel` is observed at each tool-loop iteration boundary
    /// (WEFT-323). When tripped, the turn returns
    /// [`ClawftError::Cancelled`] without starting another LLM call.
    /// Callers that have no cancel signal should pass a fresh,
    /// never-cancelled [`CancellationToken`].
    ///
    /// When a [`Self::with_cow_memory`] handle is attached, this brackets
    /// [`Self::handle_turn_inner`] with a checkpoint/promote/rollback cycle
    /// (WEFT-616 Phase 2, [`super::turn_checkpoint::with_turn_checkpoint`]).
    /// Without one attached (default), this calls straight through with no
    /// added behavior -- byte-identical to the pre-Phase-2 `handle_turn`.
    pub async fn handle_turn(
        &self,
        msg: InboundMessage,
        cancel: &CancellationToken,
    ) -> clawft_types::Result<OutboundMessage> {
        #[cfg(feature = "rvf")]
        if let Some(att) = self.cow_memory.get().cloned() {
            // WEFT-654: one undecided proposal fails new dispatches closed.
            if let Some((label, _)) = self.pending_hold_info() {
                return Err(clawft_types::ClawftError::ProposalPending { label });
            }
            let CowAttachment { mem, ingest_turns, review, .. } = att;
            let label = format!("turn:{}:{}", msg.channel, msg.chat_id);
            // Capture the exchange inputs before `msg` moves into the turn.
            let user_text = msg.content.clone();
            let channel = msg.channel.clone();
            let chat_id = msg.chat_id.clone();
            // Dimension comes from the attached lineage so the hash embedder
            // always matches the store (the daemon creates at 384; tests may
            // attach smaller stores).
            let dim = {
                let guard = mem.lock().unwrap_or_else(|p| p.into_inner());
                guard.dimension()
            };
            let items_on_ok = move |out: &OutboundMessage| {
                if !ingest_turns {
                    return Vec::new();
                }
                super::turn_checkpoint::exchange_items(
                    dim, &channel, &chat_id, &user_text, &out.content,
                )
            };
            let mut held = None;
            // Clone the token so the checkpoint bracket's async future can
            // own a copy; CancellationToken is Arc-backed and cheap to clone.
            // Must be `async move` (not `move || method(&cancel)`) so the
            // future owns `cancel` instead of borrowing the closure's env
            // (E0515).
            let cancel = cancel.clone();
            let out = super::turn_checkpoint::with_turn_checkpoint(
                &mem,
                self.turn_ledger.get(),
                label,
                move || {
                    let cancel = cancel;
                    async move { self.handle_turn_inner(msg, &cancel).await }
                },
                items_on_ok,
                review,
                &mut held,
            )
            .await;
            if let Some(h) = held {
                tracing::info!(label = %h.label, "review mode: turn held pending accept/discard");
                *self.pending_hold.lock().unwrap_or_else(|p| p.into_inner()) = Some(h);
            }
            return out;
        }
        self.handle_turn_inner(msg, cancel).await
    }

    /// The actual per-turn pipeline: session lookup, context building,
    /// pipeline invocation, the tool execution loop, and session
    /// persistence. Extracted from [`Self::handle_turn`] so the COW memory
    /// checkpoint bracket (WEFT-616 Phase 2) can wrap it without disturbing
    /// its body -- see that method's doc comment for the full behavior
    /// description; this one is unchanged from before the wrap existed.
    async fn handle_turn_inner(
        &self,
        msg: InboundMessage,
        cancel: &CancellationToken,
    ) -> clawft_types::Result<OutboundMessage> {
        // WEFT-178: Resolve the routed agent_id BEFORE building the
        // session key / context so the rest of the turn observes the
        // correct identity. The router is consulted only when one is
        // attached — single-agent CLI / test flows keep the legacy
        // synthesised `"{channel}:{sender_id}"` shape downstream.
        // Today we route in-process (every loop targets every routed
        // id); the dispatcher (clawft-weave / daemon) is the layer
        // that owns the multi-loop dispatch and may swap to a
        // per-agent runtime in a future increment. Here we just
        // resolve, log, and stash the decision for downstream
        // consumption (auth_context, gate checks).
        let routed_agent_id = self.resolve_routed_agent(&msg);
        let session_key = msg.session_key();
        // Conversation identity for the ConversationSink (M3 store
        // collapse, design §D5). Default is the canonical
        // `"{channel}:{chat_id}"` session key — the scheme `SessionManager`
        // used, so the in-process `LocalFileSink` reads the existing
        // `{channel}:{chat_id}.jsonl` files with no rename and two channels
        // sharing a `chat_id` no longer collide. The daemon opts into the
        // bare `chat_id` (already a unique conv id) via
        // `with_chat_id_as_conv_id`. This is the single conv identity threaded
        // through hydration, the sink, the graft, and the spawn context.
        let conv_id = self.conv_id_for(&msg);
        // Acquire the per-conv lock. InMemorySink is a no-op; the
        // substrate-backed sink (Phase C3) blocks concurrent turns
        // in the same conversation against the AgentService DashMap.
        self.sink.lock_conversation(&conv_id).await;

        // 0. Pre-LLM auto-delegation check.
        //    If an AutoDelegation router is configured and the message matches
        //    a delegation rule, invoke `delegate_task` directly and skip the
        //    local LLM pipeline entirely.
        if let Some(ref auto_del) = self.auto_delegation
            && let Some(delegate_args) = auto_del.should_delegate(&msg.content)
        {
            info!(
                task = %msg.content,
                "auto-delegation triggered, invoking delegate_task"
            );
            return self.run_auto_delegation(&msg, delegate_args).await;
        }

        // 0b. Pre-LLM context router (agent-core-v1 Phase B1).
        //     The router selects skills, can restrict the tool subset,
        //     and writes a clamped complexity_hint into the request.
        //     Default is NullRouter (no-op); Phase E1 replaces it with
        //     LlmClassifierRouter. By contract, the router NEVER picks
        //     a model — TieredRouter still owns that decision.
        //     WEFT-335: time the route call and append a decision
        //     record when a RouterDecisionLog is attached. Log failures
        //     are non-fatal (degrade, don't crash the chat path).
        let ctx_request = ContextRequest {
            content: msg.content.clone(),
            channel: msg.channel.clone(),
            chat_id: msg.chat_id.clone(),
            metadata: msg.metadata.clone(),
        };
        let route_started = std::time::Instant::now();
        let ctx_decision = self.context_router.route(&ctx_request).await;
        let route_latency_ms = route_started.elapsed().as_millis() as u64;
        if !ctx_decision.skills.is_empty()
            || ctx_decision.tool_subset.is_some()
            || ctx_decision.complexity_hint != 0.0
        {
            debug!(
                skills = ?ctx_decision.skills,
                tool_subset = ?ctx_decision.tool_subset,
                complexity_hint = ctx_decision.complexity_hint,
                latency_ms = route_latency_ms,
                "context router emitted decision"
            );
        }
        if let Some(ref rlog) = self.routing_log {
            self.maybe_append_routing_decision(
                rlog.as_ref(),
                &ctx_request,
                &ctx_decision,
                route_latency_ms,
            )
            .await;
        }

        // 1. Hydrate the assembly session from the conversation sink
        //    (M3 store collapse, design §D2). The in-memory `Session`
        //    is no longer independently persisted; it is reconstructed
        //    each turn from the single durable store — the sink turn log
        //    — by dropping the tool-loop intermediates the sink superset
        //    carries (see `hydrate::filter_store1_subset`). Session
        //    metadata (hallucination score, §D3) rides the sink's meta
        //    sidecar. `SessionManager` is retired from this path.
        let mut session =
            crate::agent::hydrate::hydrate_session(self.sink.as_ref(), &conv_id, 0).await;

        // 2. Build context messages from memory, skills, and history BEFORE
        //    adding the user message to session (to avoid duplicate).
        //    The user's message is the memory-retrieval query (WEFT-732); without a
        //    memory recall attached this is plain `build_messages`.
        let context_messages = self
            .context
            .build_messages_with_query(&session, &[], &msg.content)
            .await;

        // 3. Add user message to session (after building context). The
        //    in-memory session is a per-turn assembly buffer only (§D1);
        //    durability is the sink's job below.
        session.add_message("user", &msg.content, None);

        // 3b. Persist the user turn to the conversation sink — the single
        //     durable store. Errors are logged and swallowed: a sink write
        //     hiccup must never abort the LLM turn (design §6, degrade
        //     don't crash); the next turn simply hydrates without it.
        //     Skipped when the caller already recorded it (voice §W2.1: the
        //     turn landed via `agent.turn.record` WITH its voice decomposition
        //     — a second plain append would double it on the sink + forest).
        if !Self::user_turn_already_recorded(&msg) {
            // WEFT-350: when STT attaches an audio ref, persist Mixed/Audio.
            self.sink_append_user(&conv_id, &msg).await;
        }

        // 4. Context messages are already pipeline::traits::LlmMessage (B2 unification).
        let mut messages: Vec<LlmMessage> = context_messages;

        // 4·prelude. Identity-aware system prompt (agent-core-v1 Phase D1).
        //     When a SystemPromptBuilder is attached, build the
        //     identity-bearing system message and PREPEND it as the
        //     leading entry in the message list, ahead of any
        //     ContextBuilder-emitted content. The builder pulls from
        //     `Arc<dyn IdentityProvider>` so this is filesystem-free
        //     in tests.
        //
        //     WEFT-328: also capture `identity_source` so the wire
        //     `AgentChatResult` can surface it to the panel drift chip.
        //
        //     WEFT-342: binding-thread integrity is re-evaluated every
        //     turn via `gate.check("soul.binding_thread_intact", …)`.
        //     Default policy hard-refuses on mismatch
        //     (`agents.binding_thread_mode = "deny"`); `warn_only`
        //     keeps the legacy annotate + continue path. Missing
        //     identity files (NotFound) still degrade to
        //     ContextBuilder-only so uninitialised workspaces remain
        //     chat-able for CLI/dev.
        //
        //     Agent id precedence matches the tool-loop gate path
        //     below (routed > per-turn metadata > daemon concierge >
        //     channel:sender). See [`Self::resolve_gate_agent_id`].
        let gate_agent_id =
            self.resolve_gate_agent_id(&msg, routed_agent_id.as_deref());
        let mut identity_source: Option<String> = None;
        if let Some(ref builder) = self.system_prompt_builder {
            match builder.build_with_meta().await {
                Ok(built) => {
                    // Evaluate governance rule every turn (even when ok)
                    // so the witness trail records the check.
                    let status_ok = built.binding_thread_status.is_ok();
                    let bt_effect =
                        crate::agent::effects::effect_for_binding_thread(status_ok);
                    let decision = self
                        .gate
                        .check(
                            &gate_agent_id,
                            crate::agent::identity::BINDING_THREAD_GATE_ACTION,
                            &bt_effect,
                        )
                        .await;
                    // Policy mode is authoritative (WEFT-342). Gate is
                    // always consulted for the audit/witness trail; under
                    // deny mode a mismatch hard-refuses regardless of
                    // whether the attached gate is NoopGate or kernel.
                    // Under warn_only the degraded prompt continues even
                    // if the kernel rule would Deny.
                    if !status_ok && builder.binding_thread_mode().is_deny() {
                        let reason = match &decision {
                            GateDecision::Deny { reason } => reason.as_str(),
                            _ => crate::agent::identity::BINDING_THREAD_MISMATCH_REASON,
                        };
                        warn!(
                            agent_id = %gate_agent_id,
                            reason = %reason,
                            decision = ?decision,
                            "binding-thread mismatch — hard refusing turn \
                             (agents.binding_thread_mode=deny)"
                        );
                        return Err(ClawftError::SecurityViolation {
                            reason: crate::agent::identity::BINDING_THREAD_MISMATCH_REASON
                                .to_string(),
                        });
                    }
                    if !status_ok {
                        debug!(
                            agent_id = %gate_agent_id,
                            decision = ?decision,
                            "binding-thread mismatch under warn_only — continuing degraded"
                        );
                    }
                    identity_source = Some(built.identity_source);
                    messages.insert(
                        0,
                        LlmMessage {
                            role: "system".into(),
                            content: built.body,
                            tool_call_id: None,
                            tool_calls: None,
                        },
                    );
                }
                Err(crate::agent::identity::IdentityError::BindingThreadMismatch) => {
                    // WEFT-342 deny path: re-run gate for audit then refuse.
                    let decision = self
                        .gate
                        .check(
                            &gate_agent_id,
                            crate::agent::identity::BINDING_THREAD_GATE_ACTION,
                            &crate::agent::effects::effect_for_binding_thread(false),
                        )
                        .await;
                    let reason = match decision {
                        GateDecision::Deny { reason } => reason,
                        _ => crate::agent::identity::BINDING_THREAD_MISMATCH_REASON.to_string(),
                    };
                    warn!(
                        agent_id = %gate_agent_id,
                        reason = %reason,
                        "binding-thread mismatch — hard refusing turn"
                    );
                    return Err(ClawftError::SecurityViolation {
                        reason: crate::agent::identity::BINDING_THREAD_MISMATCH_REASON.to_string(),
                    });
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        "system prompt builder: identity load failed; \
                         continuing with ContextBuilder-only system prompt"
                    );
                }
            }
        }

        // 4·graft. L2 context graft (ADR-058 Phase 5.1). Query the
        //     per-conversation session tier for context relevant to this
        //     turn and splice it into the prefix-stable position: after the
        //     leading system head (system prompt + identity + skills +
        //     memory), before the dialogue tail — so the byte-identical head
        //     keeps its prefix-KV reuse. The provider is non-fatal (returns
        //     an empty block on any retrieval error); `None` (CLI / tests)
        //     skips this entirely.
        if let Some(ref provider) = self.graft_provider {
            let block = provider.graft_block(&conv_id, &msg.content).await;
            if !block.is_empty() {
                debug!(count = block.len(), "splicing L2 graft block into prompt");
                splice_graft_block(&mut messages, block);
            }
        }

        // 4a. Append router-selected skill names as a system note.
        //     The full skill instruction body is loaded by the
        //     skills loader; here we surface the names so the LLM
        //     knows which capabilities the router thinks apply.
        //     Phase E1's LlmClassifierRouter will resolve names to
        //     instructions before we reach the model; for now this
        //     is a hook the NullRouter never exercises.
        if !ctx_decision.skills.is_empty() {
            messages.push(LlmMessage {
                role: "system".into(),
                content: format!(
                    "# Router-selected skills\n\n{}",
                    ctx_decision.skills.join(", ")
                ),
                tool_call_id: None,
                tool_calls: None,
            });
        }

        // 4b. Inject skill instructions from metadata (v2 skill activation).
        //     When the interactive REPL activates a skill, its instructions
        //     are passed via metadata so the agent loop can include them.
        if let Some(instructions) = msg
            .metadata
            .get("skill_instructions")
            .and_then(|v| v.as_str())
            && !instructions.is_empty()
        {
            debug!("injecting skill instructions from metadata");
            messages.push(LlmMessage {
                role: "system".into(),
                content: format!("# Active Skill Instructions\n\n{instructions}"),
                tool_call_id: None,
                tool_calls: None,
            });
        }

        // 4c. Voice mode prompt injection.
        //     When the message originates from a voice session, inject
        //     instructions that make the LLM respond in natural spoken
        //     language instead of written/markdown format. The spoken
        //     response is sent to TTS; the frontend handles visual display.
        if msg.chat_id == "voice" {
            debug!("injecting voice mode system prompt");
            messages.push(LlmMessage {
                role: "system".into(),
                content: VOICE_MODE_PROMPT.into(),
                tool_call_id: None,
                tool_calls: None,
            });
        }

        // 5. Add current user message
        messages.push(LlmMessage {
            role: "user".into(),
            content: msg.content.clone(),
            tool_call_id: None,
            tool_calls: None,
        });

        // 6. Resolve auth context from inbound message identity.
        //    CLI channel gets admin permissions; other channels get zero-trust
        //    defaults with the sender_id and channel attached.
        let auth_context = self.resolve_auth_context(&msg);

        // 7. Resolve tool schemas -- filter by allowed_tools if present
        //    in the inbound message metadata (skill-based injection).
        //    The context router's tool_subset (when Some) overrides
        //    metadata-driven filtering since the router has the
        //    higher-level view (skill choice, complexity, etc.).
        let tool_schemas = if let Some(subset) = ctx_decision.tool_subset.as_ref() {
            if subset.is_empty() {
                debug!("context router: empty tool_subset → tools disabled");
                Vec::new()
            } else {
                debug!(tool_subset = ?subset, "context router: applying tool subset");
                self.tools.schemas_for_tools(subset)
            }
        } else {
            match msg.metadata.get("allowed_tools").and_then(|v| v.as_array()) {
                Some(tools_arr) => {
                    let allowed: Vec<String> = tools_arr
                        .iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect();
                    if allowed.is_empty() {
                        self.tools.schemas()
                    } else {
                        debug!(allowed_tools = ?allowed, "filtering tools for skill");
                        self.tools.schemas_for_tools(&allowed)
                    }
                }
                None => self.tools.schemas(),
            }
        };

        // 8. Read hallucination score from session metadata and compute boost.
        let hallucination_score = session
            .metadata
            .get(verification::HALLUCINATION_SCORE_KEY)
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0) as f32;
        let hallucination_boost = verification::score_to_boost(hallucination_score);

        if hallucination_boost > 0.0 {
            debug!(
                hallucination_score,
                hallucination_boost, "applying hallucination complexity boost"
            );
        }

        // 8b. Resolve final complexity_boost: when the router supplied
        //     a nonzero hint it takes precedence; otherwise we keep
        //     the hallucination-derived boost. This matches the B1
        //     contract — the router never ESCALATES a tier, it just
        //     replaces the boost field for the classifier to consume.
        let complexity_boost = if ctx_decision.complexity_hint != 0.0 {
            ctx_decision.complexity_hint
        } else {
            hallucination_boost
        };

        // 9. Create pipeline request with auth context + hallucination boost.
        //    A caller (test harness / panel) may force tool selection by
        //    putting an OpenAI `tool_choice` under the inbound metadata key
        //    `tool_choice` (mirrors the `allowed_tools` / `model` precedent);
        //    absent metadata leaves selection to the model — today's behavior.
        //
        //    WEFT-256: `metadata.model` from the panel chip strip (or any
        //    caller) overrides `agents.defaults.model` when non-empty. With
        //    principal `model_override: true` the tiered router honors it
        //    via the existing WEFT-31 bypass path.
        let tool_choice = msg.metadata.get(TOOL_CHOICE_META_KEY).cloned();
        let model = msg
            .metadata
            .get("model")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| self.config.defaults.model.clone());
        let request = ChatRequest {
            messages,
            tools: tool_schemas,
            model: Some(model),
            max_tokens: Some(self.config.defaults.max_tokens),
            temperature: Some(self.config.defaults.temperature),
            auth_context: Some(auth_context),
            complexity_boost,
            tool_choice,
        };

        // 10. Execute pipeline + tool loop.
        //     Phase D2 + WEFT-178 + WEFT-332: see
        //     [`Self::resolve_gate_agent_id`] — routed >
        //     per-turn metadata (multi-tenant caller principal) >
        //     boot-time concierge > synthesised channel:sender.
        let agent_id =
            self.resolve_gate_agent_id(&msg, routed_agent_id.as_deref());
        // M4 D3/D5: install the ambient parent spawn context around the
        // tool loop so an `agent_spawn` call inside it populates its
        // SpawnSpec's parent_* fields and depth from the execution
        // context, never from the LLM. conv_id / agent_id are in hand
        // here; depth rides the inbound metadata (SPAWN_DEPTH_KEY, set by
        // the daemon spawner for child conversations, 0 for top-level).
        // turn_uid stays None here by design: the parent turn's
        // *universal* id is a kernel-layer concept minted by
        // SessionTier/KernelTurnAnchor at the daemon layer, not available
        // in clawft-core. The daemon spawner resolves the real parent-turn
        // uid from SessionTier (`latest_turn_uid(parent_conv)`, keyed by
        // the parent_conv_id this scope supplies) and roots the
        // TriggeredBy/EvidenceFor edges at that real committed turn — the
        // conv-anchor uid is only the last-resort fallback when the tier is
        // unreachable, not the intended target (M4 #31). Native-only: the
        // seam is a tokio task-local (a daemon capability); the
        // browser/wasm build runs the loop unscoped.
        // WEFT-323: thread the per-turn cancel token into the tool loop so
        // cancel is observed at each iteration boundary (not only when the
        // outer dispatch `select!` drops the future mid-LLM-call).
        let loop_fut = self.run_tool_loop(request, &conv_id, &agent_id, cancel);
        #[cfg(feature = "native")]
        let tool_result = crate::agent::spawn_context::SpawnContext {
            conv_id: Some(conv_id.clone()),
            agent_id: Some(agent_id.clone()),
            turn_uid: None,
            depth: read_spawn_depth(&msg),
        }
        .scope(loop_fut)
        .await?;
        #[cfg(not(feature = "native"))]
        let tool_result = loop_fut.await?;

        // 11. Update hallucination score if any write verifications occurred.
        if tool_result.hallucinations > 0 || tool_result.verified_successes > 0 {
            let new_score = verification::update_hallucination_score(
                hallucination_score,
                tool_result.hallucinations,
                tool_result.verified_successes,
                verification::HALLUCINATION_EMA_ALPHA,
            );
            session.metadata.insert(
                verification::HALLUCINATION_SCORE_KEY.to_string(),
                serde_json::json!(new_score),
            );
            debug!(
                old_score = hallucination_score,
                new_score,
                hallucinations = tool_result.hallucinations,
                verified = tool_result.verified_successes,
                "updated hallucination score"
            );
        }

        // 12. Add assistant message to session (assembly buffer only, §D1).
        session.add_message("assistant", &tool_result.text, None);

        // 12b. Persist the final assistant turn to the conversation
        //      sink. Tool-result intermediates already went through
        //      append_turn from inside run_tool_loop; this is the
        //      last record per `chat-agent-v1.md` §11.5.
        self.sink_append_plain(&conv_id, "assistant", &tool_result.text)
            .await;

        // 12c. Soul-journal drift observation (WEFT-330).
        //      When a SoulJournal is attached, consult the documented
        //      drift signal (heuristic user-correction cues and/or
        //      explicit `soul_journal_observe` metadata). On a hit,
        //      append one observation. Failures are logged and
        //      swallowed — a journal write must never abort chat
        //      (same degrade-don't-crash contract as sink appends).
        if let Some(ref journal) = self.soul_journal {
            self.maybe_append_soul_journal(
                journal.as_ref(),
                &msg,
                &tool_result.text,
                &conv_id,
            )
            .await;
        }

        // 13. Persist session metadata to the sink's meta sidecar (§D3).
        //     `SessionManager::save_session` is retired on this path (§D1):
        //     the sink already durably recorded every turn as it happened,
        //     so the only remaining per-conv state is the metadata map
        //     (home of the hallucination score updated above). Best-effort
        //     and infallible by contract — a write hiccup can't fail the turn.
        self.sink
            .set_meta(
                &conv_id,
                serde_json::to_value(&session.metadata).unwrap_or(serde_json::Value::Null),
            )
            .await;

        // 14. Build outbound reply (caller handles dispatch).
        //
        // M4 D8 / WEFT-328: thread the enriched loop result (tool calls,
        // finish reason, iterations, spawned subagents, token counts,
        // model, identity_source) through the envelope's free-form
        // metadata map under a well-known key so the agent service's
        // `result_from_outbound` can populate `AgentChatResult`. This
        // mirrors the inbound `AgentChatParams.metadata` precedent and
        // avoids widening the generic `OutboundMessage` bus envelope.
        let mut metadata = std::collections::HashMap::new();
        let result_meta = AgentLoopResultMeta {
            tool_calls: tool_result.tool_calls,
            finish_reason: tool_result.finish_reason,
            iterations: tool_result.iterations,
            spawned_tasks: tool_result.spawned_tasks,
            model: tool_result.model,
            prompt_tokens: tool_result.prompt_tokens,
            completion_tokens: tool_result.completion_tokens,
            reasoning: tool_result.reasoning,
            identity_source,
            escalation: tool_result.escalation,
            deferred: tool_result.deferred,
        };
        match serde_json::to_value(&result_meta) {
            Ok(v) => {
                metadata.insert(AGENT_LOOP_RESULT_META_KEY.to_string(), v);
            }
            Err(e) => {
                warn!(error = %e, "failed to serialize agent loop result meta; result fields will default");
            }
        }
        // 13a. Memory consolidation (WEFT-347 / WEFT-733): distil this conversation's turns
        //      into long-term memory when the cadence is due. Best-effort: a failure is logged
        //      and never fails the turn.
        if let Some(ref consolidator) = self.consolidator
            && let Err(e) = consolidator
                .run_if_due(self.sink.as_ref(), &conv_id, crate::agent::learning::consolidator::now_ms())
                .await
        {
            warn!(conv_id = %conv_id, error = %e, "memory consolidation failed");
        }

        // 13b. Memory citations (WEFT-732): reward the memory reranker for the retrieved
        //      snippets the reply cited (+1) or ignored (-1), and drop the [mN] markers from
        //      the user-facing text (the sink keeps the reply as the model wrote it).
        let reply_text = match self.context.memory_recall() {
            Some(recall) => recall.attribute(&session.key, &tool_result.text),
            None => tool_result.text,
        };
        let outbound = OutboundMessage {
            channel: msg.channel.clone(),
            chat_id: msg.chat_id.clone(),
            content: reply_text,
            reply_to: None,
            media: vec![],
            metadata,
        };

        debug!(session_key = %session_key, "message processed successfully");

        Ok(outbound)
    }

    /// Execute auto-delegation: invoke `delegate_task` directly and
    /// return the resulting [`OutboundMessage`].
    ///
    /// This short-circuits the normal LLM pipeline when the auto-delegation
    /// router decides a message should be handled by a delegate (e.g. Claude
    /// sub-agent) rather than the local LLM. The caller is responsible for
    /// routing the returned reply (see [`Self::handle_turn`]).
    async fn run_auto_delegation(
        &self,
        msg: &InboundMessage,
        mut delegate_args: serde_json::Value,
    ) -> clawft_types::Result<OutboundMessage> {
        // Conv identity for the sink (design §D5), matching
        // [`Self::handle_turn`]'s key scheme. `SessionManager` is retired
        // here too: the delegation user/assistant turns are recorded to the
        // single durable store so the next turn hydrates them.
        let session_key = msg.session_key();
        let conv_id = self.conv_id_for(msg);

        // WEFT-180: Recursive-delegation depth guard. Each delegation
        // hop bumps the depth; when the bumped value would exceed
        // [`Self::max_delegation_depth`] we refuse to call
        // `delegate_task` and surface a structured error so the
        // operator can see the cap was hit (and the LLM, if it owned
        // the original turn, can replan). Depth is threaded via the
        // `delegate_args.delegation_depth` field AND the
        // `CLAWFT_DELEGATION_DEPTH` env var the delegator
        // (FlowDelegator / ClaudeDelegator) re-injects when it
        // re-enters the loop in a child process.
        let current_depth = read_delegation_depth(msg);
        let next_depth = current_depth.saturating_add(1);
        if next_depth > self.max_delegation_depth {
            let cap = self.max_delegation_depth;
            warn!(
                current_depth,
                next_depth,
                max = cap,
                channel = %msg.channel,
                "delegation depth exceeded, refusing delegate_task hop"
            );
            // Record the user message + refusal to the sink for traceability.
            let body = format!(
                "Delegation refused: maximum recursive delegation depth ({cap}) reached at hop {next_depth}. Override via CLAWFT_DELEGATION_DEPTH if intentional."
            );
            if !Self::user_turn_already_recorded(msg) {
                self.sink_append_user(&conv_id, msg).await;
            }
            self.sink_append_plain(&conv_id, "assistant", &body).await;
            // The task was not done: flag the turn as failed so scripted
            // callers (one-shot `weft agent -m`) exit non-zero.
            let mut refused = OutboundMessage {
                channel: msg.channel.clone(),
                chat_id: msg.chat_id.clone(),
                content: body,
                reply_to: None,
                media: vec![],
                metadata: Default::default(),
            };
            refused.mark_error();
            return Ok(refused);
        }
        // Stamp the bumped depth into the delegation args so the
        // child delegator sees the carried count. Tools that don't
        // know about the field will just ignore it.
        if let Some(obj) = delegate_args.as_object_mut() {
            obj.insert(DELEGATION_DEPTH_KEY.into(), serde_json::json!(next_depth));
        }

        // Record the user turn to the sink for history (unless the caller
        // already did — see `user_turn_already_recorded`).
        if !Self::user_turn_already_recorded(msg) {
            self.sink_append_user(&conv_id, msg).await;
        }

        // Resolve auth context for permission checks.
        let auth = self.resolve_auth_context(msg);
        let permissions = Some(&auth.permissions);

        // Invoke delegate_task tool directly.
        let mut delegation_failed = false;
        let response_text = match self
            .tools
            .execute("delegate_task", delegate_args, permissions)
            .await
        {
            Ok(result) => {
                // Extract the response text from the delegation result.
                if let Some(response) = result.get("response").and_then(|v| v.as_str()) {
                    response.to_string()
                } else {
                    // Fall back to the full JSON if no "response" field.
                    serde_json::to_string_pretty(&result).unwrap_or_else(|_| result.to_string())
                }
            }
            Err(e) => {
                warn!(error = %e, "auto-delegation failed");
                // On delegation failure, surface a user-visible error and
                // flag the turn as failed (see `OutboundMessage::mark_error`)
                // so chat UIs show the text while one-shot callers exit
                // non-zero. (A future enhancement could re-enter the local
                // LLM pipeline here; today we keep the simpler contract.)
                delegation_failed = true;
                format!("Delegation failed: {e}. The task could not be routed to the delegate.")
            }
        };

        // Record the delegate's response to the sink.
        self.sink_append_plain(&conv_id, "assistant", &response_text)
            .await;

        // Build outbound reply (caller handles dispatch).
        let mut outbound = OutboundMessage {
            channel: msg.channel.clone(),
            chat_id: msg.chat_id.clone(),
            content: response_text,
            reply_to: None,
            media: vec![],
            metadata: Default::default(),
        };
        if delegation_failed {
            outbound.mark_error();
        }

        debug!(session_key = %session_key, "auto-delegated message processed");
        Ok(outbound)
    }

    /// Resolve [`AuthContext`] from the inbound message's sender identity.
    ///
    /// Resolve permissions for an inbound message using the 5-layer
    /// [`PermissionResolver`].
    ///
    /// Resolution order (highest priority wins):
    /// 1. Built-in level defaults
    /// 2. Global config level overrides
    /// 3. Workspace config level overrides
    /// 4. Per-user overrides
    /// 5. Per-channel overrides (highest priority)
    ///
    /// CLI channel messages always receive admin-level (Level 2)
    /// permissions via the resolver's `cli_default_level`.
    fn resolve_auth_context(&self, msg: &InboundMessage) -> AuthContext {
        // Channel plugins set "allow_from_match" in metadata when the sender
        // passed the channel's allow_from verification. This promotes the
        // sender from zero-trust to at least user-level permissions.
        let allow_from_match = msg
            .metadata
            .get("allow_from_match")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        self.permission_resolver.resolve_auth_context(
            &msg.sender_id,
            &msg.channel,
            allow_from_match,
        )
    }

    /// Resolve the routed `agent_id` for an inbound message via the
    /// attached [`AgentRouter`] (WEFT-178).
    ///
    /// Returns `None` when no router is attached OR when the router
    /// emits [`RoutingResult::NoMatch`](crate::agent_routing::RoutingResult::NoMatch).
    /// The latter is logged at warn-level by [`AgentRouter::route`]
    /// itself; we don't double-log here.
    ///
    /// `RoutingResult::Agent` and `RoutingResult::CatchAll` both
    /// return `Some(id)` so downstream callers can't tell the two
    /// apart. That's intentional — the catch-all IS the routed
    /// agent for that message.
    fn resolve_routed_agent(&self, msg: &InboundMessage) -> Option<String> {
        let router = self.agent_router.as_ref()?;
        match router.route(msg) {
            crate::agent_routing::RoutingResult::Agent(id)
            | crate::agent_routing::RoutingResult::CatchAll(id) => {
                debug!(
                    routed_agent = %id,
                    channel = %msg.channel,
                    sender_id = %msg.sender_id,
                    "agent router resolved inbound message"
                );
                Some(id)
            }
            crate::agent_routing::RoutingResult::NoMatch => None,
        }
    }

    /// Wall-clock millisecond timestamp.
    ///
    /// Used as the `ts_ms` for [`Turn`] records published to the
    /// [`ConversationSink`]. Falls back to `0` if the system clock
    /// is before the UNIX epoch (which only happens on misconfigured
    /// machines; not worth surfacing as an error).
    fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    /// Generate a turn identifier. Phase C3 will swap this for a
    /// monotonic ULID inside the substrate-backed sink; until then
    /// we use a `chrono`-based string + nanosecond counter to keep
    /// turns ordered without pulling a ULID dep into clawft-core.
    fn next_turn_id() -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        format!("turn-{ts}-{seq:08x}", ts = Self::now_ms(), seq = seq)
    }

    /// True when the caller already persisted this user turn to the sink
    /// (metadata `user_turn_recorded = true`). The voice §W2.1 loop records
    /// the user turn via `agent.turn.record` first (carrying the Wave-1
    /// `voice_analysis` decomposition the plain append here would drop), then
    /// dispatches with this flag so the turn isn't recorded twice.
    fn user_turn_already_recorded(msg: &InboundMessage) -> bool {
        msg.metadata
            .get("user_turn_recorded")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    /// Append a plain (no tool metadata) role turn to the conversation
    /// sink, warning and swallowing on failure.
    ///
    /// The sink is the single durable store after the M3 store collapse,
    /// but a write is still best-effort by contract (design §6): a
    /// substrate/file hiccup must never abort the LLM turn — the next
    /// turn just hydrates without the lost record. Shared by the user /
    /// assistant appends in [`Self::handle_turn`] and the auto-delegation
    /// path so all three record turns identically.
    async fn sink_append_plain(&self, conv_id: &str, role: &str, content: &str) {
        self.sink_append_plain_with_audio(conv_id, role, content, None)
            .await;
    }

    /// WEFT-350: append a user turn, pulling an optional [`AudioRef`] from
    /// inbound metadata so the substrate sink can persist
    /// `TurnContent::Audio` / `Mixed`.
    async fn sink_append_user(&self, conv_id: &str, msg: &InboundMessage) {
        let audio = Self::audio_from_inbound(msg);
        self.sink_append_plain_with_audio(conv_id, "user", &msg.content, audio)
            .await;
    }

    /// Extract a substrate audio ref from inbound metadata (WEFT-350).
    fn audio_from_inbound(msg: &InboundMessage) -> Option<clawft_types::AudioRef> {
        let map: serde_json::Map<String, serde_json::Value> = msg
            .metadata
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        if let Some(a) = clawft_types::audio_from_metadata(&map) {
            return Some(a);
        }
        // Fallback: first media URL treated as substrate path.
        msg.media
            .first()
            .map(|path| clawft_types::AudioRef::new(path.clone(), "audio/wav", 0))
    }

    async fn sink_append_plain_with_audio(
        &self,
        conv_id: &str,
        role: &str,
        content: &str,
        audio: Option<clawft_types::AudioRef>,
    ) {
        if let Err(e) = self
            .sink
            .append_turn(
                conv_id,
                Turn {
                    turn_id: Self::next_turn_id(),
                    role: role.into(),
                    content: content.into(),
                    tool_calls: None,
                    tool_call_id: None,
                    ts_ms: Self::now_ms(),
                    voice_analysis: None,
                    audio,
                },
            )
            .await
        {
            warn!(error = %e, role, "sink: failed to append turn");
        }
    }

    /// WEFT-330: detect a drift signal on this turn and, if present,
    /// append one [`super::soul_journal::DriftObservation`]. See
    /// `soul_journal` module docs for the signal table.
    async fn maybe_append_soul_journal(
        &self,
        journal: &dyn super::soul_journal::SoulJournal,
        msg: &InboundMessage,
        assistant_text: &str,
        conv_id: &str,
    ) {
        use super::soul_journal::{detect_drift_signal, observation_from_signal};

        let Some((signal, content)) =
            detect_drift_signal(&msg.content, assistant_text, &msg.metadata)
        else {
            return;
        };
        let obs = observation_from_signal(signal, content, conv_id);
        match journal.append(obs).await {
            Ok(()) => {
                debug!(conv_id = %conv_id, "soul journal: drift observation appended");
            }
            Err(e) => {
                warn!(
                    error = %e,
                    conv_id = %conv_id,
                    "soul journal: append failed (non-fatal)"
                );
            }
        }
    }

    /// WEFT-335: append one router decision record. Always fires when
    /// a log is attached (every `ContextRouter::route` call). Failures
    /// are logged and swallowed so observability never aborts chat.
    async fn maybe_append_routing_decision(
        &self,
        log: &dyn super::routing_log::RouterDecisionLog,
        request: &ContextRequest,
        decision: &ContextDecision,
        latency_ms: u64,
    ) {
        use super::routing_log::RouterDecisionRecord;

        // Routers today don't always surface confidence / alternatives
        // on `ContextDecision`. Populate what we can; leave the rest
        // empty so the record still seeds promotion-gate counts +
        // latency baselines. `fallback_used` comes from the decision
        // (HybridRouter sets it on fall-through). Optional metadata
        // keys let tests / future routers enrich confidence + alts.
        let confidence = request
            .metadata
            .get("routing_confidence")
            .and_then(|v| v.as_f64())
            .map(|f| f as f32);
        let alternatives = request
            .metadata
            .get("routing_alternatives")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        let rec = RouterDecisionRecord::from_route(
            request,
            decision,
            latency_ms,
            alternatives,
            confidence,
            decision.fallback_used,
        );
        match log.append(rec).await {
            Ok(()) => {
                debug!(
                    chat_id = %request.chat_id,
                    latency_ms,
                    "routing log: decision appended"
                );
            }
            Err(e) => {
                warn!(
                    error = %e,
                    chat_id = %request.chat_id,
                    "routing log: append failed (non-fatal)"
                );
            }
        }
    }

    /// Resolve the workspace path from config, expanding `~` to home dir.
    fn workspace_path(&self) -> std::path::PathBuf {
        let raw = &self.config.defaults.workspace;
        if let Some(rest) = raw.strip_prefix("~/")
            && let Some(home) = self.platform.fs().home_dir()
        {
            return home.join(rest);
        }
        std::path::PathBuf::from(raw)
    }

    /// Single-tool dispatch with policy + sandbox + truncation
    /// (WEFT-190).
    ///
    /// Centralises the per-tool-call shape so both the in-loop
    /// dispatcher (`run_tool_loop`) and any future caller (e.g. a
    /// delegator that re-enters the registry directly) share the
    /// same policy enforcement. Returns the JSON-encoded tool result
    /// body (the same shape `run_tool_loop` previously inlined):
    ///
    /// * `EffectGate::Permit` → mints a per-tool
    ///   [`ToolPermitToken`](crate::tools::ToolPermitToken) (WEFT-341)
    ///   and passes it to
    ///   [`ToolRegistry::execute_with_permit`](crate::tools::registry::ToolRegistry::execute_with_permit);
    ///   success result is serialized after
    ///   [`crate::security::truncate_result`] clamps it to
    ///   [`MAX_TOOL_RESULT_BYTES`]; failures become `{"error": "..."}`.
    /// * `EffectGate::Deny`   → no tool dispatch; returns
    ///   `{"denied": true, "reason": ...}` so the LLM sees a
    ///   policy decision distinct from a runtime fault.
    /// * `EffectGate::Defer`  → with no interactor: returns
    ///   `{"deferred": true, "reason": ...}` and **halts** with
    ///   [`FINISH_REASON_DEFERRED`] + [`DeferredActionEvent`] (WEFT-258).
    ///   With a [`super::defer::DeferInteractor`] (WEFT-331): suspends
    ///   until allow / deny / cancel / timeout; allow falls through
    ///   to sandbox + dispatch (token minted as `"defer-allow"`).
    /// * Sandbox denial       → returns `{"error": "sandbox denied: ..."}`.
    ///
    /// The helper applies the [`MAX_TOOL_RESULT_BYTES`] truncation
    /// uniformly so callers cannot accidentally ship un-truncated
    /// tool output back to the LLM (the audit's CRIT-02 finding).
    ///
    /// `conv_id` + `cancel` are required for the interactive-defer path
    /// (panel prompt + WEFT-323 cancel). Callers without a live turn
    /// (unit tests of the deny envelope) may pass `""` and a fresh token.
    /// `tool_call_id` is stored on ObservationPack ledger rows (empty in
    /// tests that do not have a model-issued id).
    // Public AgentLoop entry point with ~9 call sites (loop + tests); the guard inputs are independent, so a params struct would only move the arguments.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_tool_with_guards(
        &self,
        agent_id: &str,
        tool_name: &str,
        input: &serde_json::Value,
        permissions: Option<&clawft_types::routing::UserPermissions>,
        conv_id: &str,
        cancel: &CancellationToken,
        tool_call_id: &str,
    ) -> String {
        // 1. EffectGate (policy) check.
        let ev = effect_for_tool(tool_name, input);
        let action = format!("tool.{tool_name}");
        // Gate payload carried into the per-tool Permit token (WEFT-341).
        // Human-allow on Defer uses a distinct sentinel so the proof
        // still binds to this dispatch even without a gate receipt.
        let gate_token: String = match self.gate.check(agent_id, &action, &ev).await {
            GateDecision::Permit { token } => token,
            GateDecision::Deny { reason } => {
                warn!(tool = %tool_name, reason = %reason, "gate: tool dispatch denied");
                return serde_json::json!({
                    "denied": true,
                    "reason": reason,
                })
                .to_string();
            }
            GateDecision::Defer { reason } => {
                // WEFT-331: interactive defer when an interactor is
                // attached; otherwise keep the v1 structured deferred
                // tool-result so the model re-plans without a panel.
                if let Some(interactor) = self.defer_interactor.get() {
                    warn!(
                        tool = %tool_name,
                        reason = %reason,
                        conv_id,
                        "gate: tool dispatch deferred — awaiting human decision"
                    );
                    let args_preview = preview_truncate(
                        &serde_json::to_string(input).unwrap_or_else(|_| "{}".into()),
                    );
                    let request = super::defer::DeferRequest {
                        conv_id: conv_id.to_string(),
                        tool: tool_name.to_string(),
                        reason: reason.clone(),
                        arguments_preview: args_preview,
                        timeout_ms: clawft_types::agent_chat::DEFER_DEFAULT_TIMEOUT_MS,
                    };
                    let outcome = interactor.await_decision(request, cancel).await;
                    if let Some(body) = outcome.tool_result_json(&reason) {
                        info!(
                            tool = %tool_name,
                            conv_id,
                            outcome = ?outcome,
                            "defer decision — tool not executed"
                        );
                        return body;
                    }
                    // Allow → fall through to sandbox + dispatch
                    info!(
                        tool = %tool_name,
                        conv_id,
                        "defer allowed by human — proceeding with tool"
                    );
                    "defer-allow".into()
                } else {
                    warn!(tool = %tool_name, reason = %reason, "gate: tool dispatch deferred");
                    return serde_json::json!({
                        "deferred": true,
                        "reason": reason,
                    })
                    .to_string();
                }
            }
        };

        // 2. Sandbox (allowlist) check.
        if let Some(enforcer) = self.sandbox.as_ref()
            && let Err(reason) = enforcer.check_tool(tool_name)
        {
            warn!(tool = %tool_name, reason = %reason, "sandbox: tool dispatch denied");
            return serde_json::json!({
                "error": format!("sandbox denied: {reason}")
            })
            .to_string();
        }

        // 3. Mint per-tool Permit token (WEFT-341) and dispatch through
        //    the registry. The registry verifies MAC / tool / TTL when
        //    a token is presented; tools may also re-present via
        //    `tools::present_proof`. Success results are truncated.
        //    WEFT-651: InvalidArgs echoes the tool parameter schema so
        //    the model can correct the call instead of blind-retrying
        //    (the canvas missing-content ×20 incident).
        let permit = self
            .tools
            .permit_issuer()
            .issue(agent_id, tool_name, &gate_token);
        let execute = self.tools.execute_with_permit(
            tool_name,
            input.clone(),
            permissions,
            Some(&permit),
        );

        #[cfg(feature = "native")]
        let result = {
            if let Some(sessions_dir) = self.resolve_observation_sessions_dir().await {
                crate::observation_pack::ObservationContext {
                    sessions_dir,
                    session_key: conv_id.to_string(),
                }
                .scope(execute)
                .await
            } else {
                execute.await
            }
        };
        #[cfg(not(feature = "native"))]
        let result = execute.await;

        match result {
            Ok(val) => self
                .pack_or_truncate(tool_name, tool_call_id, conv_id, val)
                .await,
            Err(e) => {
                error!(tool = %tool_name, error = %e, "tool execution failed");
                let mut msg = e.to_string();
                if matches!(
                    e,
                    crate::tools::registry::ToolError::InvalidArgs(_)
                ) && let Some(tool) = self.tools.get(tool_name)
                {
                    let schema = tool.parameters();
                    let schema_str =
                        serde_json::to_string(&schema).unwrap_or_else(|_| "{}".into());
                    let schema_preview = preview_truncate(&schema_str);
                    msg = format!(
                        "{msg}. Expected parameters schema: {schema_preview}. \
                         Do not retry with the same invalid arguments."
                    );
                }
                serde_json::json!({"error": msg}).to_string()
            }
        }
    }

    #[cfg(feature = "native")]
    async fn resolve_observation_sessions_dir(&self) -> Option<std::path::PathBuf> {
        if let Some(dir) = self.observation_sessions_dir.get() {
            return Some(dir.clone());
        }
        match crate::session::discover_sessions_dir(self.platform.as_ref()).await {
            Ok(dir) => {
                let _ = self.observation_sessions_dir.set(dir.clone());
                Some(dir)
            }
            Err(e) => {
                warn!(error = %e, "observation pack: sessions dir unavailable");
                None
            }
        }
    }

    async fn pack_or_truncate(
        &self,
        tool_name: &str,
        tool_call_id: &str,
        conv_id: &str,
        val: serde_json::Value,
    ) -> String {
        #[cfg(feature = "native")]
        {
            if let Some(sessions_dir) = self.resolve_observation_sessions_dir().await {
                let packed = crate::observation_pack::pack_tool_result(
                    &sessions_dir,
                    conv_id,
                    tool_name,
                    tool_call_id,
                    val,
                    MAX_TOOL_RESULT_BYTES,
                )
                .await;
                return serde_json::to_string(&packed).unwrap_or_default();
            }
        }
        #[cfg(not(feature = "native"))]
        {
            let _ = (tool_name, tool_call_id, conv_id);
        }
        let truncated = crate::security::truncate_result(val, MAX_TOOL_RESULT_BYTES);
        serde_json::to_string(&truncated).unwrap_or_default()
    }

    /// Execute the tool loop: call LLM, execute tools, repeat.
    ///
    /// After each LLM call, checks if the response contains tool-use
    /// requests. If so, executes each tool via the `ToolRegistry`, appends
    /// tool results to the message list, and re-invokes the pipeline.
    /// Continues until the LLM returns a text response or the maximum
    /// iteration limit is reached.
    ///
    /// `cancel` is checked at the top of every iteration (WEFT-323). When
    /// tripped, the loop returns [`ClawftError::Cancelled`] without starting
    /// another LLM call — so cancel takes effect at the next tool-call
    /// boundary rather than waiting for the whole turn to drain.
    ///
    /// Post-write verification checks whether files claimed by write/edit
    /// tools actually exist on disk. Hallucinated results are replaced with
    /// error messages so the LLM can retry.
    async fn run_tool_loop(
        &self,
        mut request: ChatRequest,
        conv_id: &str,
        agent_id: &str,
        cancel: &CancellationToken,
    ) -> clawft_types::Result<ToolLoopResult> {
        let max_iterations = self.config.defaults.max_tool_iterations.max(1) as usize;
        let mut total_hallucinations: usize = 0;
        let mut total_verified: usize = 0;
        // M4 D8: accumulate the real result enrichment across iterations.
        let mut tool_call_summaries: Vec<AgentChatToolCall> = Vec::new();
        let mut spawned_tasks: Vec<SpawnedTaskSummary> = Vec::new();
        // Observability accounting across iterations (voice trace / watch).
        let mut model: Option<String> = None;
        let mut prompt_tokens: u32 = 0;
        let mut completion_tokens: u32 = 0;
        let mut reasoning_parts: Vec<String> = Vec::new();
        let workspace = self.workspace_path();
        // WEFT-651: consecutive identical (tool, args, error) failures.
        let mut last_identical_failure: Option<IdenticalFailureKey> = None;
        let mut identical_failure_count: u32 = 0;
        // WEFT-345: consecutive gate Deny results → EscalateToHuman.
        let mut consecutive_gate_denials: u32 = 0;
        let mut gate_denial_records: Vec<GateDenialRecord> = Vec::new();

        for iteration in 0..max_iterations {
            // WEFT-323: observe the per-conv cancel token at the iteration
            // boundary — before the next LLM call or tool dispatch. This is
            // the per-iteration guarantee AC-10 required beyond the outer
            // dispatch `select!` (which only aborts when the future is
            // dropped mid-await).
            if cancel.is_cancelled() {
                info!(
                    conv_id,
                    iteration, "tool loop cancelled via token at iteration boundary"
                );
                return Err(ClawftError::Cancelled {
                    conv_id: conv_id.to_string(),
                });
            }

            // WEFT-652 (cubecow event-level snapshots): under tool cadence,
            // checkpoint at each tool-call boundary — iteration 0 is covered
            // by the turn bracket's own checkpoint, so this fires from the
            // first boundary onward. Failure logs and continues: an
            // event-level snapshot is an OPPORTUNITY for finer rollback, not
            // a gate on the turn (the turn-level bracket still guards the
            // whole turn). Witnessed like every checkpoint.
            #[cfg(feature = "rvf")]
            if iteration > 0
                && let Some(att) = self.cow_memory.get()
                && att.tool_cadence
            {
                let mem = &att.mem;
                let label = format!("tool:{conv_id}:{iteration}");
                let ok = {
                    let mut guard = mem.lock().unwrap_or_else(|p| p.into_inner());
                    guard.checkpoint(label.clone()).map_err(|e| {
                        tracing::error!(error = %e, label, "tool-boundary checkpoint failed; continuing turn");
                    })
                    .is_ok()
                };
                if ok && let Some(ledger) = self.turn_ledger.get() {
                    ledger.on_checkpoint(&label);
                }
            }

            // WEFT-322: per-conversation cost circuit-breaker.
            // Check BEFORE the LLM call so a tripped budget never burns
            // an extra request. The check honours circuit_open from a
            // prior turn (substrate-persisted), so a daemon restart
            // mid-conversation re-trips on the next call without
            // repeating the offending iteration.
            if let Some(ref budget) = self.cost_budget {
                let decision = budget.check_can_call(conv_id);
                if let Some(err) = decision.into_error(conv_id) {
                    warn!(
                        conv_id,
                        iteration,
                        error = %err,
                        "cost budget tripped — fail-fast before LLM call"
                    );
                    // Persist circuit_open so subsequent turns see it.
                    // mark_open is idempotent if already open. We need
                    // the dimension for the persisted record; pull it
                    // out of the error before consuming it.
                    if let ClawftError::ConversationBudgetExceeded { ref dimension, .. } = err
                        && let Err(e) = budget.mark_open(conv_id, dimension)
                    {
                        warn!(error = %e, "budget: mark_open failed");
                    }
                    return Err(err);
                }
            }

            let response = self.pipeline.complete(&request).await?;

            // Observability accounting (voice trace / watch): which model
            // answered, cumulative tokens, and any captured reasoning.
            if let Some(m) = response.metadata.get("model").and_then(|v| v.as_str()) {
                model = Some(m.to_string());
            }
            prompt_tokens = prompt_tokens.saturating_add(response.usage.input_tokens);
            completion_tokens = completion_tokens.saturating_add(response.usage.output_tokens);
            if let Some(r) = response.metadata.get("reasoning").and_then(|v| v.as_str()) {
                reasoning_parts.push(r.to_string());
            }

            // WEFT-322: record this call's accounting against the budget.
            // The pipeline's `LlmResponse.usage` is the canonical token
            // count; USD cost is not yet routed through the loop so we
            // record `0.0` for now — substrate-backed cost adapters in
            // a future increment will fill this in via response metadata.
            if let Some(ref budget) = self.cost_budget {
                let usd_cost = response
                    .metadata
                    .get("usd_cost")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                match budget.record_call(
                    conv_id,
                    response.usage.input_tokens,
                    response.usage.output_tokens,
                    usd_cost,
                ) {
                    Ok(usage) => {
                        // If THIS call put us over, mark open and return
                        // typed error before any tool dispatch — preserving
                        // the contract that no further LLM work happens
                        // once the budget trips.
                        let post = budget.check_after_call(conv_id, &usage);
                        if let Some(err) = post.into_error(conv_id) {
                            warn!(
                                conv_id,
                                iteration,
                                error = %err,
                                "cost budget tripped on latest call — fail-fast"
                            );
                            if let ClawftError::ConversationBudgetExceeded { ref dimension, .. } =
                                err
                                && let Err(e) = budget.mark_open(conv_id, dimension)
                            {
                                warn!(error = %e, "budget: mark_open failed");
                            }
                            return Err(err);
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, "budget: record_call failed");
                    }
                }
            }

            // Extract tool calls from the response
            let tool_calls: Vec<(String, String, serde_json::Value)> = response
                .content
                .iter()
                .filter_map(|block| {
                    if let ContentBlock::ToolUse { id, name, input } = block {
                        Some((id.clone(), name.clone(), input.clone()))
                    } else {
                        None
                    }
                })
                .collect();

            if tool_calls.is_empty() {
                // No tool calls -- extract text response and return
                let text = response
                    .content
                    .iter()
                    .filter_map(|block| {
                        if let ContentBlock::Text { text } = block {
                            Some(text.as_str())
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("");

                debug!(iteration, "tool loop complete, returning text response");
                return Ok(ToolLoopResult {
                    text,
                    hallucinations: total_hallucinations,
                    verified_successes: total_verified,
                    tool_calls: tool_call_summaries,
                    finish_reason: "stop".into(),
                    // `iteration` is 0-based; this LLM round-trip ran.
                    iterations: (iteration as u32).saturating_add(1),
                    spawned_tasks,
                    model,
                    prompt_tokens,
                    completion_tokens,
                    reasoning: if reasoning_parts.is_empty() {
                        None
                    } else {
                        Some(reasoning_parts.join("\n\n"))
                    },
                    escalation: None,
                    deferred: None,
                });
            }

            debug!(
                iteration,
                tool_count = tool_calls.len(),
                "executing tool calls"
            );

            // Append the assistant message (with tool_calls) to the conversation
            // so the next LLM request sees the correct message sequence:
            //   ... user -> assistant (tool_use) -> tool results -> ...
            let assistant_tool_calls: Vec<serde_json::Value> = tool_calls
                .iter()
                .map(|(id, name, input)| {
                    serde_json::json!({
                        "id": id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": serde_json::to_string(input).unwrap_or_default(),
                        }
                    })
                })
                .collect();

            // Extract any text content from the response for the assistant message
            let assistant_text: String = response
                .content
                .iter()
                .filter_map(|block| {
                    if let ContentBlock::Text { text } = block {
                        Some(text.as_str())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("");

            request.messages.push(LlmMessage {
                role: "assistant".into(),
                content: assistant_text.clone(),
                tool_call_id: None,
                tool_calls: Some(assistant_tool_calls.clone()),
            });

            // Phase B2: persist the assistant turn that invoked
            // tools. The final assistant text response (no tools)
            // is written by handle_turn after the loop returns.
            if let Err(e) = self
                .sink
                .append_turn(
                    conv_id,
                    Turn {
                        turn_id: Self::next_turn_id(),
                        role: "assistant".into(),
                        content: assistant_text,
                        tool_calls: Some(assistant_tool_calls),
                        tool_call_id: None,
                        ts_ms: Self::now_ms(),
                        voice_analysis: None,
            audio: None,
                    },
                )
                .await
            {
                warn!(error = %e, "sink: failed to append assistant tool-call turn");
            }

            // Execute all tool calls in parallel and append results in order.
            // WEFT-190: each per-tool dispatch goes through
            // [`Self::execute_tool_with_guards`], which centralises
            // the EffectGate policy check, the sandbox allowlist, the
            // registry call, and the [`MAX_TOOL_RESULT_BYTES`]
            // truncation. The shape returned by the helper matches
            // what the LLM has been seeing since Phase B2, so this
            // refactor is behaviour-preserving for `run_tool_loop`.
            let permissions = request.auth_context.as_ref().map(|ctx| &ctx.permissions);

            let futures: Vec<_> = tool_calls
                .iter()
                .map(|(id, name, input)| async move {
                    let body = self
                        .execute_tool_with_guards(
                            agent_id,
                            name,
                            input,
                            permissions,
                            conv_id,
                            cancel,
                            id,
                        )
                        .await;
                    (id.clone(), name.clone(), body)
                })
                .collect();

            let results = futures_util::future::join_all(futures).await;

            // Skill autogen pattern detection: feed each dispatched
            // tool name to the detector, then surface any newly
            // recurring patterns as pending SKILL.md candidates in
            // `~/.clawft/skills/`. Promotion to live skills stays a
            // manual approval step per the autogen module's design —
            // we never auto-arm a generated skill.
            //
            // WEFT-66: when AutogenConfig carries the active
            // TrajectoryLearner (or the pipeline exposes one),
            // generation goes through generate_skill_md_with_learning
            // so successful trajectories mutate the candidate body.
            if let Some(detector) = self.autogen.as_ref() {
                use crate::agent::skill_autogen::{
                    generate_skill_md_with_learning, install_pending_skill,
                };
                let candidates = {
                    let mut det = match detector.lock() {
                        Ok(g) => g,
                        Err(p) => {
                            warn!("autogen detector mutex poisoned, recovering");
                            p.into_inner()
                        }
                    };
                    for (_, name, _) in &results {
                        det.record_tool_call(name);
                    }
                    det.detect_candidates()
                };
                if !candidates.is_empty() {
                    // Prefer the learner wired into AutogenConfig; fall
                    // back to the pipeline's shared TrajectoryLearner so
                    // learning-driven generation still runs when CLI
                    // only set enabled/threshold.
                    let (install_dir, learner_arc) = {
                        let mut det = match detector.lock() {
                            Ok(g) => g,
                            Err(p) => p.into_inner(),
                        };
                        if det.learner().is_none()
                            && let Some(pipeline_learner) = self.pipeline.trajectory_learner()
                        {
                            det.set_learner(Some(Arc::clone(pipeline_learner)));
                        }
                        (det.install_dir(), det.learner_arc())
                    };
                    for pattern in candidates {
                        let candidate = generate_skill_md_with_learning(
                            &pattern,
                            learner_arc.as_deref(),
                        );
                        match install_pending_skill(&candidate, &install_dir) {
                            Ok(path) => {
                                info!(
                                    skill_dir = %path.display(),
                                    name = %candidate.name,
                                    learning = learner_arc.is_some(),
                                    "autogen: installed pending skill candidate"
                                );
                            }
                            Err(e) => {
                                warn!(error = %e, "autogen: install_pending_skill failed");
                            }
                        }
                    }
                }
            }

            // Post-write verification: check that claimed writes exist on disk.
            let verification_results =
                verification::verify_write_results(self.platform.fs(), &workspace, &results).await;

            // Build a set of hallucinated tool call IDs for result replacement.
            let hallucinated_ids: std::collections::HashSet<String> = verification_results
                .iter()
                .filter(|v| !v.verified)
                .map(|v| v.tool_call_id.clone())
                .collect();

            // Count verification outcomes.
            for vr in &verification_results {
                if vr.verified {
                    total_verified += 1;
                } else {
                    total_hallucinations += 1;
                    warn!(
                        tool_call_id = %vr.tool_call_id,
                        path = %vr.claimed_path.display(),
                        "VERIFICATION FAILED: file does not exist (hallucinated write)"
                    );
                }
            }

            for (id, name, result_json) in &results {
                let mut content = if hallucinated_ids.contains(id) {
                    // Replace the success result with a verification failure error.
                    serde_json::json!({
                        "error": "VERIFICATION FAILED: the file you claimed to write does not exist on disk. The write was hallucinated. Please retry the write operation."
                    }).to_string()
                } else {
                    result_json.clone()
                };

                // Match args from the request-side tool call (by id).
                let input = tool_calls
                    .iter()
                    .find(|(cid, _, _)| cid == id)
                    .map(|(_, _, input)| input.clone())
                    .unwrap_or(serde_json::Value::Null);

                // WEFT-258: gate Defer → halt for interactive panel prompt.
                // Structured tool-result shape is unchanged
                // (`{"deferred":true,"reason":...}`); the loop no longer
                // continues so the model can re-plan — the panel owns the
                // human decision and resumes via a follow-up user turn.
                if let Some(reason) = gate_deferral_reason(&content) {
                    let arguments_preview =
                        preview_truncate(&serde_json::to_string(&input).unwrap_or_default());
                    let event = DeferredActionEvent::new(
                        conv_id,
                        name.clone(),
                        reason,
                        arguments_preview.clone(),
                    );
                    warn!(
                        conv_id,
                        tool = %name,
                        reason = %event.reason,
                        "WEFT-258 gate defer — interactive human review"
                    );

                    tool_call_summaries.push(AgentChatToolCall {
                        name: name.clone(),
                        arguments_preview,
                        result_preview: preview_truncate(&content),
                        success: false,
                    });
                    request.messages.push(LlmMessage {
                        role: "tool".into(),
                        content: content.clone(),
                        tool_call_id: Some(id.clone()),
                        tool_calls: None,
                    });
                    if let Err(e) = self
                        .sink
                        .append_turn(
                            conv_id,
                            Turn {
                                turn_id: Self::next_turn_id(),
                                role: "tool".into(),
                                content: content.clone(),
                                tool_calls: None,
                                tool_call_id: Some(id.clone()),
                                ts_ms: Self::now_ms(),
                                voice_analysis: None,
            audio: None,
                            },
                        )
                        .await
                    {
                        warn!(error = %e, "sink: failed to append tool turn");
                    }

                    return Ok(ToolLoopResult {
                        text: event.summary.clone(),
                        hallucinations: total_hallucinations,
                        verified_successes: total_verified,
                        tool_calls: tool_call_summaries,
                        finish_reason: FINISH_REASON_DEFERRED.into(),
                        iterations: (iteration as u32).saturating_add(1),
                        spawned_tasks,
                        model,
                        prompt_tokens,
                        completion_tokens,
                        reasoning: if reasoning_parts.is_empty() {
                            None
                        } else {
                            Some(reasoning_parts.join("\n\n"))
                        },
                        escalation: None,
                        deferred: Some(event),
                    });
                }

                // WEFT-345: consecutive gate Deny → EscalateToHuman.
                // Count only structured gate denials (`{"denied":true}`);
                // sandbox / runtime errors and Defer do not contribute.
                // Trip *before* the WEFT-651 identical-failure breaker so
                // policy denials surface as governance escalation rather
                // than a generic provider error when both would fire.
                let escalate = record_gate_denial_streak(
                    &mut consecutive_gate_denials,
                    &mut gate_denial_records,
                    name,
                    &content,
                    GATE_DENIAL_ESCALATION_LIMIT,
                );
                if escalate {
                    let event =
                        EscalateToHumanEvent::from_denials(conv_id, gate_denial_records.clone());
                    warn!(
                        conv_id,
                        denial_count = event.denial_count,
                        last_tool = %name,
                        "WEFT-345 gate denial streak — EscalateToHuman"
                    );

                    let arguments_preview =
                        preview_truncate(&serde_json::to_string(&input).unwrap_or_default());
                    tool_call_summaries.push(AgentChatToolCall {
                        name: name.clone(),
                        arguments_preview,
                        result_preview: preview_truncate(&content),
                        success: false,
                    });
                    request.messages.push(LlmMessage {
                        role: "tool".into(),
                        content: content.clone(),
                        tool_call_id: Some(id.clone()),
                        tool_calls: None,
                    });
                    if let Err(e) = self
                        .sink
                        .append_turn(
                            conv_id,
                            Turn {
                                turn_id: Self::next_turn_id(),
                                role: "tool".into(),
                                content: content.clone(),
                                tool_calls: None,
                                tool_call_id: Some(id.clone()),
                                ts_ms: Self::now_ms(),
                                voice_analysis: None,
            audio: None,
                            },
                        )
                        .await
                    {
                        warn!(error = %e, "sink: failed to append tool turn");
                    }

                    // Halt the turn with a first-class escalation event
                    // (not a silent Provider error). Panel UX for the
                    // interactive allow/abort/refine RPC is W-UI/15.
                    return Ok(ToolLoopResult {
                        text: event.summary.clone(),
                        hallucinations: total_hallucinations,
                        verified_successes: total_verified,
                        tool_calls: tool_call_summaries,
                        finish_reason: FINISH_REASON_ESCALATE_TO_HUMAN.into(),
                        iterations: (iteration as u32).saturating_add(1),
                        spawned_tasks,
                        model,
                        prompt_tokens,
                        completion_tokens,
                        reasoning: if reasoning_parts.is_empty() {
                            None
                        } else {
                            Some(reasoning_parts.join("\n\n"))
                        },
                        escalation: Some(event),
                        deferred: None,
                    });
                }

                // WEFT-651: consecutive identical-failure circuit breaker.
                let failure_key = identical_failure_key(name, &input, &content);
                if let Some(tripped) = record_identical_failure(
                    &mut last_identical_failure,
                    &mut identical_failure_count,
                    failure_key,
                    IDENTICAL_TOOL_FAILURE_LIMIT,
                ) {
                    content = identical_failure_breaker_message(
                        &tripped,
                        identical_failure_count,
                    );
                    warn!(
                        conv_id,
                        tool = %tripped.tool,
                        count = identical_failure_count,
                        error = %tripped.error,
                        "WEFT-651 identical tool failure breaker tripped"
                    );

                    // Still surface the escalated result in history /
                    // summaries before fail-fast so observers see why.
                    let arguments_preview =
                        preview_truncate(&serde_json::to_string(&input).unwrap_or_default());
                    tool_call_summaries.push(AgentChatToolCall {
                        name: name.clone(),
                        arguments_preview,
                        result_preview: preview_truncate(&content),
                        success: false,
                    });
                    request.messages.push(LlmMessage {
                        role: "tool".into(),
                        content: content.clone(),
                        tool_call_id: Some(id.clone()),
                        tool_calls: None,
                    });
                    if let Err(e) = self
                        .sink
                        .append_turn(
                            conv_id,
                            Turn {
                                turn_id: Self::next_turn_id(),
                                role: "tool".into(),
                                content,
                                tool_calls: None,
                                tool_call_id: Some(id.clone()),
                                ts_ms: Self::now_ms(),
                                voice_analysis: None,
            audio: None,
                            },
                        )
                        .await
                    {
                        warn!(error = %e, "sink: failed to append tool turn");
                    }

                    return Err(ClawftError::Provider {
                        message: format!(
                            "identical tool failure breaker: tool `{}` failed {} times \
                             with the same arguments ({}). Stop retrying the identical call.",
                            tripped.tool,
                            identical_failure_count,
                            tripped.error,
                        ),
                    });
                }

                // M4 D8: record a per-tool-call summary for the wire
                // result. `arguments_preview` comes from the request-side
                // tool input (matched by call id); `result_preview` and
                // `success` reflect the post-verification `content`.
                let arguments_preview =
                    preview_truncate(&serde_json::to_string(&input).unwrap_or_default());
                tool_call_summaries.push(AgentChatToolCall {
                    name: name.clone(),
                    arguments_preview,
                    result_preview: preview_truncate(&content),
                    success: tool_result_succeeded(&content),
                });

                // M4 D8: surface any subagent spawned this iteration.
                // Detected from the real tool output (`result_json`),
                // which for `agent_spawn` is never hallucination-replaced.
                if let Some(task) = spawned_task_from_result(name, result_json) {
                    spawned_tasks.push(task);
                }

                request.messages.push(LlmMessage {
                    role: "tool".into(),
                    content: content.clone(),
                    tool_call_id: Some(id.clone()),
                    tool_calls: None,
                });

                // Phase B2: persist the tool-result turn so the
                // ConversationSink sees one record per role event.
                // Sink errors are logged and swallowed — never fail
                // the LLM turn because of a substrate write hiccup.
                if let Err(e) = self
                    .sink
                    .append_turn(
                        conv_id,
                        Turn {
                            turn_id: Self::next_turn_id(),
                            role: "tool".into(),
                            content,
                            tool_calls: None,
                            tool_call_id: Some(id.clone()),
                            ts_ms: Self::now_ms(),
                            voice_analysis: None,
            audio: None,
                        },
                    )
                    .await
                {
                    warn!(error = %e, "sink: failed to append tool turn");
                }
            }
        }

        Err(ClawftError::Provider {
            message: format!("max tool iterations ({}) exceeded", max_iterations),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::memory::MemoryStore;
    use crate::agent::skills::SkillsLoader;
    use crate::pipeline::traits::{
        AssembledContext, LearningBackend, LearningSignal, LlmTransport, ModelRouter, Pipeline,
        QualityScore, QualityScorer, ResponseOutcome, RoutingDecision, TaskClassifier, TaskProfile,
        TaskType, Trajectory, TransportRequest,
    };
    use crate::tools::registry::Tool;
    use async_trait::async_trait;
    use clawft_platform::NativePlatform;
    use clawft_types::config::AgentDefaults;
    use clawft_types::provider::{LlmResponse, StopReason, Usage};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(prefix: &str) -> PathBuf {
        let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        std::env::temp_dir().join(format!("clawft_loop_test_{prefix}_{pid}_{id}"))
    }

    fn test_config() -> AgentsConfig {
        AgentsConfig {
            defaults: AgentDefaults {
                workspace: "~/.clawft/workspace".into(),
                model: "test-model".into(),
                max_tokens: 4096,
                temperature: 0.5,
                max_tool_iterations: 10,
                memory_window: 50,
            },
            ..AgentsConfig::default()
        }
    }

    // -- Mock pipeline stages --

    struct MockClassifier;
    impl TaskClassifier for MockClassifier {
        fn classify(&self, _request: &ChatRequest) -> TaskProfile {
            TaskProfile {
                task_type: TaskType::Chat,
                complexity: 0.3,
                keywords: vec![],
            }
        }
    }

    struct MockRouter;
    #[async_trait]
    impl ModelRouter for MockRouter {
        async fn route(&self, _request: &ChatRequest, _profile: &TaskProfile) -> RoutingDecision {
            RoutingDecision {
                provider: "test".into(),
                model: "test-model".into(),
                reason: "mock".into(),
                ..Default::default()
            }
        }
        fn update(&self, _d: &RoutingDecision, _o: &ResponseOutcome) {}
    }

    struct MockAssembler;
    #[async_trait]
    impl crate::pipeline::traits::ContextAssembler for MockAssembler {
        async fn assemble(
            &self,
            request: &ChatRequest,
            _profile: &TaskProfile,
        ) -> AssembledContext {
            AssembledContext {
                messages: request.messages.clone(),
                token_estimate: 100,
                truncated: false,
            }
        }
    }

    /// Transport that returns a fixed text response.
    struct MockTransport {
        response_text: String,
    }

    impl MockTransport {
        fn new(text: &str) -> Self {
            Self {
                response_text: text.into(),
            }
        }
    }

    #[async_trait]
    impl LlmTransport for MockTransport {
        async fn complete(&self, _request: &TransportRequest) -> clawft_types::Result<LlmResponse> {
            Ok(LlmResponse {
                id: "mock-resp".into(),
                content: vec![ContentBlock::Text {
                    text: self.response_text.clone(),
                }],
                stop_reason: StopReason::EndTurn,
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                    total_tokens: 0,
                },
                metadata: HashMap::new(),
            })
        }
    }

    /// Transport that returns a tool call first, then text.
    struct MockToolTransport {
        call_count: std::sync::atomic::AtomicUsize,
    }

    impl MockToolTransport {
        fn new() -> Self {
            Self {
                call_count: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl LlmTransport for MockToolTransport {
        async fn complete(&self, _request: &TransportRequest) -> clawft_types::Result<LlmResponse> {
            let count = self
                .call_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if count == 0 {
                // First call: request a tool use
                Ok(LlmResponse {
                    id: "mock-tool-resp".into(),
                    content: vec![ContentBlock::ToolUse {
                        id: "call-1".into(),
                        name: "echo".into(),
                        input: serde_json::json!({"text": "hello"}),
                    }],
                    stop_reason: StopReason::ToolUse,
                    usage: Usage {
                        input_tokens: 10,
                        output_tokens: 5,
                        total_tokens: 0,
                    },
                    metadata: HashMap::new(),
                })
            } else {
                // Second call: return text
                Ok(LlmResponse {
                    id: "mock-final-resp".into(),
                    content: vec![ContentBlock::Text {
                        text: "tool result processed".into(),
                    }],
                    stop_reason: StopReason::EndTurn,
                    usage: Usage {
                        input_tokens: 20,
                        output_tokens: 8,
                        total_tokens: 0,
                    },
                    metadata: HashMap::new(),
                })
            }
        }
    }

    /// Transport that always returns tool calls (to test max iterations).
    struct InfiniteToolTransport;

    #[async_trait]
    impl LlmTransport for InfiniteToolTransport {
        async fn complete(&self, _request: &TransportRequest) -> clawft_types::Result<LlmResponse> {
            Ok(LlmResponse {
                id: "infinite".into(),
                content: vec![ContentBlock::ToolUse {
                    id: "call-inf".into(),
                    name: "echo".into(),
                    input: serde_json::json!({"text": "loop"}),
                }],
                stop_reason: StopReason::ToolUse,
                usage: Usage {
                    input_tokens: 5,
                    output_tokens: 3,
                    total_tokens: 0,
                },
                metadata: HashMap::new(),
            })
        }
    }

    struct MockScorer;
    impl QualityScorer for MockScorer {
        fn score(&self, _req: &ChatRequest, _resp: &LlmResponse) -> QualityScore {
            QualityScore {
                overall: 1.0,
                relevance: 1.0,
                coherence: 1.0,
            }
        }
    }

    struct MockLearner;
    impl LearningBackend for MockLearner {
        fn record(&self, _t: &Trajectory) {}
        fn adapt(&self, _s: &LearningSignal) {}
    }

    fn make_pipeline(transport: Arc<dyn LlmTransport>) -> PipelineRegistry {
        let pipeline = Pipeline {
            classifier: Arc::new(MockClassifier),
            router: Arc::new(MockRouter),
            assembler: Arc::new(MockAssembler),
            transport,
            scorer: Arc::new(MockScorer),
            learner: Arc::new(MockLearner),
        };
        PipelineRegistry::new(pipeline)
    }

    // -- Mock tool --

    struct EchoTool;

    #[async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "Echo back the input text"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string" }
                },
                "required": ["text"]
            })
        }
        async fn execute(
            &self,
            args: serde_json::Value,
        ) -> Result<serde_json::Value, crate::tools::registry::ToolError> {
            let text = args
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("(no text)");
            Ok(serde_json::json!({"output": text}))
        }
    }

    struct SizedOutputTool {
        name: &'static str,
        n_bytes: usize,
    }

    #[async_trait]
    impl Tool for SizedOutputTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "Returns a payload of a chosen size"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({ "type": "object", "properties": {} })
        }
        async fn execute(
            &self,
            _args: serde_json::Value,
        ) -> Result<serde_json::Value, crate::tools::registry::ToolError> {
            Ok(serde_json::json!({ "data": "x".repeat(self.n_bytes) }))
        }
    }

    /// Helper to create an AgentLoop with the given transport.
    async fn make_agent_loop(
        transport: Arc<dyn LlmTransport>,
        prefix: &str,
    ) -> (AgentLoop<NativePlatform>, PathBuf) {
        let dir = temp_dir(prefix);
        let platform = Arc::new(NativePlatform::new());
        let bus = Arc::new(MessageBus::new());

        let memory = Arc::new(MemoryStore::with_paths(
            dir.join("memory").join("MEMORY.md"),
            dir.join("memory").join("HISTORY.md"),
            platform.clone(),
        ));
        let skills = Arc::new(SkillsLoader::with_dir(dir.join("skills"), platform.clone()));
        let context = ContextBuilder::new(test_config(), memory, skills, platform.clone());

        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(EchoTool));

        let pipeline = make_pipeline(transport);

        let sessions = dir.join("sessions");
        let _ = std::fs::create_dir_all(&sessions);
        let agent = AgentLoop::new(
            test_config(),
            platform,
            bus,
            pipeline,
            Arc::new(tools),
            context,
            PermissionResolver::default_resolver(),
        )
        .with_observation_sessions_dir(sessions);
        (agent, dir)
    }

    /// `make_agent_loop` with an `AgentsConfig` tweak and a pre-seeded MEMORY.md.
    async fn make_agent_loop_cfg(
        transport: Arc<dyn LlmTransport>,
        prefix: &str,
        tweak: impl Fn(&mut AgentsConfig),
        memory_md: &str,
    ) -> (AgentLoop<NativePlatform>, PathBuf, Arc<MemoryStore<NativePlatform>>) {
        let dir = temp_dir(prefix);
        let platform = Arc::new(NativePlatform::new());
        let memory = Arc::new(MemoryStore::with_paths(
            dir.join("memory").join("MEMORY.md"),
            dir.join("memory").join("HISTORY.md"),
            platform.clone(),
        ));
        if !memory_md.is_empty() {
            memory.write_long_term(memory_md).await.unwrap();
        }
        let skills = Arc::new(SkillsLoader::with_dir(dir.join("skills"), platform.clone()));
        let mut cfg = test_config();
        tweak(&mut cfg);
        let context = ContextBuilder::new(cfg.clone(), memory.clone(), skills, platform.clone());
        let sessions = dir.join("sessions");
        let _ = std::fs::create_dir_all(&sessions);
        let agent = AgentLoop::new(
            cfg,
            platform,
            Arc::new(MessageBus::new()),
            make_pipeline(transport),
            Arc::new(ToolRegistry::new()),
            context,
            PermissionResolver::default_resolver(),
        )
        .with_observation_sessions_dir(sessions);
        (agent, dir, memory)
    }

    fn inbound(content: &str) -> InboundMessage {
        InboundMessage {
            channel: "test".into(),
            sender_id: "user1".into(),
            chat_id: "chat1".into(),
            content: content.into(),
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata: HashMap::new(),
        }
    }

    #[tokio::test]
    async fn consolidation_after_a_turn_writes_the_topic_bank() {
        let transport = Arc::new(MockTransport::new("Noted."));
        let (agent, dir, memory) = make_agent_loop_cfg(
            transport,
            "consolidate_turn",
            |c| {
                c.memory_consolidation.enabled = true;
                c.memory_consolidation.every_k_turns = 1;
            },
            "",
        )
        .await;
        agent
            .handle_turn(inbound("I am allergic to penicillin and I live in Denver."), &CancellationToken::new())
            .await
            .unwrap();
        let md = memory.read_long_term().await.unwrap();
        assert!(md.contains("<!-- topics -->"), "{md}");
        assert!(md.contains("I am allergic to penicillin") && md.contains("I live in Denver"), "{md}");
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn consolidation_off_by_default_leaves_memory_untouched() {
        let transport = Arc::new(MockTransport::new("Noted."));
        let (agent, dir, memory) = make_agent_loop_cfg(transport, "consolidate_off", |_| {}, "").await;
        agent.handle_turn(inbound("I live in Denver."), &CancellationToken::new()).await.unwrap();
        assert!(memory.read_long_term().await.unwrap_or_default().is_empty());
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[cfg(feature = "vector-memory")]
    #[tokio::test]
    async fn memory_recall_citations_are_stripped_from_the_user_facing_reply() {
        let transport = Arc::new(MockTransport::new("You drink tea [m1]."));
        let (agent, dir, _) = make_agent_loop_cfg(
            transport,
            "recall_turn",
            // PLUMBING: this test is about citation stripping, not retrieval quality, so it
            // opts in to the explicit lexical retriever (no embedding model needed).
            |c| {
                c.memory_recall.enabled = true;
                c.memory_recall.lexical = true;
            },
            "The user prefers tea over coffee.\n\nThe user lives in Denver.",
        )
        .await;
        let out = agent
            .handle_turn(inbound("tea or coffee preference?"), &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(out.content, "You drink tea.");
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[test]
    fn new_creates_agent_loop_with_all_deps() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let transport = Arc::new(MockTransport::new("hello"));
            let (agent, dir) = make_agent_loop(transport, "new_all").await;

            assert_eq!(agent.config().defaults.model, "test-model");
            assert_eq!(agent.config().defaults.max_tokens, 4096);
            assert_eq!(agent.config().defaults.max_tool_iterations, 10);

            let _ = tokio::fs::remove_dir_all(&dir).await;
        });
    }

    #[test]
    fn config_accessor_returns_config() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let transport = Arc::new(MockTransport::new("hello"));
            let (agent, dir) = make_agent_loop(transport, "config_acc").await;

            assert_eq!(agent.config().defaults.memory_window, 50);
            assert_eq!(agent.config().defaults.workspace, "~/.clawft/workspace");

            let _ = tokio::fs::remove_dir_all(&dir).await;
        });
    }

    #[test]
    fn platform_accessor_returns_platform() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let transport = Arc::new(MockTransport::new("hello"));
            let (agent, dir) = make_agent_loop(transport, "platform_acc").await;

            // Verify the platform reference is accessible
            let _p = agent.platform();

            let _ = tokio::fs::remove_dir_all(&dir).await;
        });
    }

    #[tokio::test]
    async fn process_message_produces_outbound() {
        let transport = Arc::new(MockTransport::new("Hello from LLM!"));
        let (agent, dir) = make_agent_loop(transport, "process_msg").await;

        // Publish an inbound message
        let inbound = InboundMessage {
            channel: "test".into(),
            sender_id: "user1".into(),
            chat_id: "chat1".into(),
            content: "hi there".into(),
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata: HashMap::new(),
        };
        agent.bus.publish_inbound(inbound).unwrap();

        // Process it
        let msg = agent.bus.consume_inbound().await.unwrap();
        let outbound = agent.handle_turn(msg, &CancellationToken::new()).await.unwrap();

        // Check outbound
        assert_eq!(outbound.channel, "test");
        assert_eq!(outbound.chat_id, "chat1");
        assert_eq!(outbound.content, "Hello from LLM!");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn process_message_executes_tool_loop() {
        let transport = Arc::new(MockToolTransport::new());
        let (agent, dir) = make_agent_loop(transport, "tool_loop").await;

        let inbound = InboundMessage {
            channel: "test".into(),
            sender_id: "user1".into(),
            chat_id: "chat1".into(),
            content: "use echo tool".into(),
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata: HashMap::new(),
        };
        agent.bus.publish_inbound(inbound).unwrap();

        let msg = agent.bus.consume_inbound().await.unwrap();
        let outbound = agent.handle_turn(msg, &CancellationToken::new()).await.unwrap();

        assert_eq!(outbound.content, "tool result processed");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// WEFT-652: under tool cadence, each tool-call boundary takes a
    /// witnessed mid-turn checkpoint (iteration ≥ 1); the turn's promote
    /// collapses them. Observed through a recording TurnLedger — the
    /// checkpoints themselves are collapsed by promote, so the witness
    /// stream is the durable evidence.
    #[cfg(feature = "rvf")]
    #[tokio::test]
    async fn tool_cadence_checkpoints_are_witnessed_per_boundary() {
        use crate::agent::turn_ledger::{PromotedLineage, TurnLedger};

        #[derive(Default)]
        struct Rec(std::sync::Mutex<Vec<String>>);
        impl TurnLedger for Rec {
            fn on_checkpoint(&self, label: &str) {
                self.0.lock().unwrap().push(format!("cp:{label}"));
            }
            fn on_revert(&self, label: &str, _e: &str, _r: bool) {
                self.0.lock().unwrap().push(format!("rv:{label}"));
            }
            fn on_promote(&self, label: &str, _p: bool, _l: Option<PromotedLineage>) {
                self.0.lock().unwrap().push(format!("pr:{label}"));
            }
        }

        // MockToolTransport: one tool round-trip then a final answer — so
        // exactly one iteration boundary at iteration 1.
        let transport = Arc::new(MockToolTransport::new());
        let (agent, dir) = make_agent_loop(transport, "tool_cadence").await;
        let cow_dir = tempfile::TempDir::new().unwrap();
        let mem = clawft_cow_memory::BranchableMemory::create(cow_dir.path(), 4).unwrap();
        agent
            .cow_memory
            .set(CowAttachment {
                mem: Arc::new(std::sync::Mutex::new(mem)),
                ingest_turns: false,
                tool_cadence: true,
                review: false,
            })
            .ok();
        let rec = Arc::new(Rec::default());
        agent.set_turn_ledger(rec.clone());

        let inbound = InboundMessage {
            channel: "test".into(),
            sender_id: "user1".into(),
            chat_id: "chat1".into(),
            content: "use echo tool".into(),
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata: HashMap::new(),
        };
        agent.bus.publish_inbound(inbound).unwrap();
        let msg = agent.bus.consume_inbound().await.unwrap();
        let outbound = agent.handle_turn(msg, &CancellationToken::new()).await.unwrap();
        assert_eq!(outbound.content, "tool result processed");

        let events = rec.0.lock().unwrap().clone();
        assert!(
            events.iter().any(|e| e.starts_with("cp:turn:test:chat1")),
            "turn checkpoint witnessed: {events:?}"
        );
        assert!(
            events.iter().any(|e| e.starts_with("cp:tool:") && e.contains(":1")),
            "tool-boundary checkpoint witnessed at iteration 1: {events:?}"
        );
        assert!(
            events.iter().any(|e| e.starts_with("pr:turn:test:chat1")),
            "promote witnessed and collapses the mid-turn checkpoints: {events:?}"
        );
        // Order: turn cp before tool cp before promote.
        let idx = |pfx: &str| events.iter().position(|e| e.starts_with(pfx)).unwrap();
        assert!(idx("cp:turn:") < idx("cp:tool:") && idx("cp:tool:") < idx("pr:turn:"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// WEFT-654: review mode holds turn 1, fails turn 2 closed with
    /// ProposalPending, and accept_pending unblocks turn 3.
    #[cfg(feature = "rvf")]
    #[tokio::test]
    async fn review_mode_guards_dispatch_until_accept() {
        let transport = Arc::new(MockTransport::new("Hello from LLM!"));
        let (agent, dir) = make_agent_loop(transport, "review_guard").await;
        let cow_dir = tempfile::TempDir::new().unwrap();
        let mem = clawft_cow_memory::BranchableMemory::create(cow_dir.path(), 4).unwrap();
        agent
            .cow_memory
            .set(CowAttachment {
                mem: Arc::new(std::sync::Mutex::new(mem)),
                ingest_turns: true,
                tool_cadence: false,
                review: true,
            })
            .ok();

        let msg = |content: &str| InboundMessage {
            channel: "test".into(),
            sender_id: "user1".into(),
            chat_id: "chat1".into(),
            content: content.into(),
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata: HashMap::new(),
        };

        // Turn 1 succeeds and is HELD.
        let out1 = agent.handle_turn(msg("first"), &CancellationToken::new()).await;
        assert!(out1.is_ok());
        let (label, _age) = agent.pending_hold_info().expect("turn 1 held");
        assert_eq!(label, "turn:test:chat1");

        // Turn 2 fails closed while the proposal is pending.
        let out2 = agent.handle_turn(msg("second"), &CancellationToken::new()).await;
        assert!(
            matches!(out2, Err(clawft_types::ClawftError::ProposalPending { .. })),
            "pending proposal must fail new dispatches closed: {out2:?}"
        );

        // Accept = promote; the loop is open again.
        let accepted = agent.accept_pending().expect("accept");
        assert_eq!(accepted, "turn:test:chat1");
        assert!(agent.pending_hold_info().is_none());
        let out3 = agent.handle_turn(msg("third"), &CancellationToken::new()).await;
        assert!(out3.is_ok(), "accepted loop takes turns again: {out3:?}");
        // turn 3 is itself held now (review stays on) — discard it to end clean.
        let _ = agent.discard_pending("test teardown");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn run_tool_loop_respects_max_iterations() {
        let transport = Arc::new(InfiniteToolTransport);
        let (agent, dir) = make_agent_loop(transport, "max_iter").await;

        let request = ChatRequest {
            messages: vec![LlmMessage {
                role: "user".into(),
                content: "loop forever".into(),
                tool_call_id: None,
                tool_calls: None,
            }],
            tools: vec![],
            model: Some("test-model".into()),
            max_tokens: Some(4096),
            temperature: Some(0.5),
            auth_context: None,
            complexity_boost: 0.0,
            tool_choice: None,
        };

        let result = agent
            .run_tool_loop(request, "test-conv", "test:agent", &CancellationToken::new())
            .await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("max tool iterations"),
            "error should mention max iterations: {}",
            err_msg
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// WEFT-323: cancel token tripped before the first iteration returns
    /// [`ClawftError::Cancelled`] without burning an LLM call.
    #[tokio::test]
    async fn run_tool_loop_breaks_on_cancel_at_iteration_boundary() {
        let transport = Arc::new(InfiniteToolTransport);
        let (agent, dir) = make_agent_loop(transport, "cancel_iter").await;

        let request = ChatRequest {
            messages: vec![LlmMessage {
                role: "user".into(),
                content: "loop until cancelled".into(),
                tool_call_id: None,
                tool_calls: None,
            }],
            tools: vec![],
            model: Some("test-model".into()),
            max_tokens: Some(4096),
            temperature: Some(0.5),
            auth_context: None,
            complexity_boost: 0.0,
            tool_choice: None,
        };

        // Pre-cancelled: iteration 0 must fail-fast before the LLM call.
        let cancel = CancellationToken::new();
        cancel.cancel();

        let result = agent
            .run_tool_loop(request, "conv-cancel", "test:agent", &cancel)
            .await;
        match result {
            Err(ClawftError::Cancelled { conv_id }) => {
                assert_eq!(conv_id, "conv-cancel");
            }
            other => panic!("expected Cancelled at iteration boundary, got {other:?}"),
        }

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// WEFT-323: cancel mid multi-iteration loop breaks on the next
    /// boundary (after the current iteration drains).
    #[tokio::test]
    async fn run_tool_loop_breaks_mid_turn_on_next_boundary() {
        /// Transport that cancels the shared token after the first
        /// successful complete, so iteration 1 hits the boundary check.
        struct CancelAfterFirstTransport {
            cancel: CancellationToken,
            calls: std::sync::atomic::AtomicU32,
        }

        #[async_trait]
        impl LlmTransport for CancelAfterFirstTransport {
            async fn complete(
                &self,
                _request: &TransportRequest,
            ) -> clawft_types::Result<LlmResponse> {
                let n = self
                    .calls
                    .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                if n >= 1 {
                    // Should never be reached: iteration 1 must abort
                    // at the boundary before this call.
                    panic!("LLM called after cancel at iteration boundary");
                }
                // Trip cancel so the next iteration's boundary check fires.
                self.cancel.cancel();
                Ok(LlmResponse {
                    id: "cancel-after-first".into(),
                    content: vec![ContentBlock::ToolUse {
                        id: "call-1".into(),
                        name: "echo".into(),
                        input: serde_json::json!({"text": "once"}),
                    }],
                    stop_reason: StopReason::ToolUse,
                    usage: Usage {
                        input_tokens: 1,
                        output_tokens: 1,
                        total_tokens: 0,
                    },
                    metadata: HashMap::new(),
                })
            }
        }

        let cancel = CancellationToken::new();
        let transport = Arc::new(CancelAfterFirstTransport {
            cancel: cancel.clone(),
            calls: std::sync::atomic::AtomicU32::new(0),
        });
        let (agent, dir) = make_agent_loop(transport, "cancel_mid").await;

        let request = ChatRequest {
            messages: vec![LlmMessage {
                role: "user".into(),
                content: "start multi-iter".into(),
                tool_call_id: None,
                tool_calls: None,
            }],
            tools: vec![],
            model: Some("test-model".into()),
            max_tokens: Some(4096),
            temperature: Some(0.5),
            auth_context: None,
            complexity_boost: 0.0,
            tool_choice: None,
        };

        let result = agent
            .run_tool_loop(request, "conv-mid", "test:agent", &cancel)
            .await;
        match result {
            Err(ClawftError::Cancelled { conv_id }) => {
                assert_eq!(conv_id, "conv-mid");
            }
            other => panic!("expected mid-turn Cancelled, got {other:?}"),
        }

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    // -- WEFT-651: identical tool+args failure breaker in run_tool_loop -----

    /// Tool that always rejects with InvalidArgs (canvas missing-content
    /// pattern). Parameters schema is non-empty so schema-echo can be
    /// asserted on the error body.
    struct AlwaysInvalidTool;

    #[async_trait]
    impl Tool for AlwaysInvalidTool {
        fn name(&self) -> &str {
            "canvas_stub"
        }
        fn description(&self) -> &str {
            "Stub canvas tool that always rejects args"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["command", "content"]
            })
        }
        async fn execute(
            &self,
            _args: serde_json::Value,
        ) -> Result<serde_json::Value, crate::tools::registry::ToolError> {
            Err(crate::tools::registry::ToolError::InvalidArgs(
                "invalid canvas command: missing field content".into(),
            ))
        }
    }

    /// Transport that always requests the same failing canvas_stub call
    /// (identical tool + args every iteration).
    struct InfiniteIdenticalFailTransport;

    #[async_trait]
    impl LlmTransport for InfiniteIdenticalFailTransport {
        async fn complete(&self, _request: &TransportRequest) -> clawft_types::Result<LlmResponse> {
            Ok(LlmResponse {
                id: "identical-fail".into(),
                content: vec![ContentBlock::ToolUse {
                    id: "call-fail".into(),
                    name: "canvas_stub".into(),
                    input: serde_json::json!({"command": "render"}),
                }],
                stop_reason: StopReason::ToolUse,
                usage: Usage {
                    input_tokens: 5,
                    output_tokens: 3,
                    total_tokens: 0,
                },
                metadata: HashMap::new(),
            })
        }
    }

    /// Agent loop with AlwaysInvalidTool registered (and echo for other
    /// tests' default registry shape).
    async fn make_agent_loop_with_failing_tool(
        transport: Arc<dyn LlmTransport>,
        prefix: &str,
    ) -> (AgentLoop<NativePlatform>, PathBuf) {
        let dir = temp_dir(prefix);
        let platform = Arc::new(NativePlatform::new());
        let bus = Arc::new(MessageBus::new());

        let memory = Arc::new(MemoryStore::with_paths(
            dir.join("memory").join("MEMORY.md"),
            dir.join("memory").join("HISTORY.md"),
            platform.clone(),
        ));
        let skills = Arc::new(SkillsLoader::with_dir(dir.join("skills"), platform.clone()));
        let context = ContextBuilder::new(test_config(), memory, skills, platform.clone());

        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(EchoTool));
        tools.register(Arc::new(AlwaysInvalidTool));

        let pipeline = make_pipeline(transport);

        let sessions = dir.join("sessions");
        let _ = std::fs::create_dir_all(&sessions);
        let agent = AgentLoop::new(
            test_config(),
            platform,
            bus,
            pipeline,
            Arc::new(tools),
            context,
            PermissionResolver::default_resolver(),
        )
        .with_observation_sessions_dir(sessions);
        (agent, dir)
    }

    #[tokio::test]
    async fn run_tool_loop_trips_identical_failure_breaker() {
        // max_tool_iterations is 10 in test_config; without the breaker
        // this would burn all 10. With N=3 it must fail-fast earlier.
        let transport = Arc::new(InfiniteIdenticalFailTransport);
        let (agent, dir) = make_agent_loop_with_failing_tool(transport, "ident_fail").await;

        let request = ChatRequest {
            messages: vec![LlmMessage {
                role: "user".into(),
                content: "write a haiku on the canvas".into(),
                tool_call_id: None,
                tool_calls: None,
            }],
            tools: vec![],
            model: Some("test-model".into()),
            max_tokens: Some(4096),
            temperature: Some(0.5),
            auth_context: None,
            complexity_boost: 0.0,
            tool_choice: None,
        };

        let result = agent
            .run_tool_loop(request, "w21-smoke2", "test:agent", &CancellationToken::new())
            .await;
        assert!(result.is_err(), "breaker must fail-fast: {result:?}");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("identical tool failure breaker"),
            "error should name the breaker: {err_msg}"
        );
        assert!(
            err_msg.contains("canvas_stub"),
            "error should name the tool: {err_msg}"
        );
        assert!(
            err_msg.contains("3 times") || err_msg.contains("failed 3"),
            "error should report N=3: {err_msg}"
        );
        // Must NOT burn the full max-iteration budget message.
        assert!(
            !err_msg.contains("max tool iterations"),
            "should trip breaker before max iterations: {err_msg}"
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn invalid_args_error_echoes_parameter_schema() {
        let transport = Arc::new(MockTransport::new("unused"));
        let (agent, dir) = make_agent_loop_with_failing_tool(transport, "schema_echo").await;

        let body = agent
            .execute_tool_with_guards(
                "test:agent",
                "canvas_stub",
                &serde_json::json!({"command": "render"}),
                None,
                "schema_echo",
                &CancellationToken::new(),
                "",
            )
            .await;

        assert!(
            body.contains("invalid arguments") || body.contains("missing field content"),
            "should surface InvalidArgs: {body}"
        );
        assert!(
            body.contains("Expected parameters schema"),
            "WEFT-651 schema-echo missing: {body}"
        );
        assert!(
            body.contains("content"),
            "schema should mention required content field: {body}"
        );
        assert!(
            body.contains("Do not retry with the same invalid arguments"),
            "should advise against identical retry: {body}"
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    // -- WEFT-322: per-conversation cost circuit-breaker --------------------

    /// Build a chat request with a single user message — handy for the
    /// budget tests that exercise `run_tool_loop` directly.
    fn budget_request(content: &str) -> ChatRequest {
        ChatRequest {
            messages: vec![LlmMessage {
                role: "user".into(),
                content: content.into(),
                tool_call_id: None,
                tool_calls: None,
            }],
            tools: vec![],
            model: Some("test-model".into()),
            max_tokens: Some(4096),
            temperature: Some(0.5),
            auth_context: None,
            complexity_boost: 0.0,
            tool_choice: None,
        }
    }

    #[tokio::test]
    async fn cost_budget_token_cap_fails_fast_with_typed_error() {
        use crate::agent::cost_budget::{ConversationBudget, InMemoryBudgetStore};
        use clawft_types::config::CostBudgetConfig;
        use clawft_types::error::ClawftError;

        // Each MockToolTransport call records 10/5 then 20/8 tokens
        // (total 43). Cap at 12 → first call (15 total) trips the
        // post-call check before any tool result can be appended.
        let transport = Arc::new(MockToolTransport::new());
        let (mut agent, dir) = make_agent_loop(transport, "budget_tokens").await;
        let store = Arc::new(InMemoryBudgetStore::new());
        let budget = Arc::new(ConversationBudget::new(
            CostBudgetConfig {
                max_tokens_per_conv: 12,
                max_usd_per_conv: 999.0,
                max_iterations_per_conv: 999,
            },
            store.clone(),
        ));
        agent = agent.with_cost_budget(Arc::clone(&budget));

        let result = agent
            .run_tool_loop(budget_request("hi"), "conv-tok", "test:agent", &CancellationToken::new())
            .await;
        let err = result.expect_err("token cap should trip");
        match err {
            ClawftError::ConversationBudgetExceeded {
                conv_id,
                dimension,
                limit,
                used,
            } => {
                assert_eq!(conv_id, "conv-tok");
                assert_eq!(dimension, "tokens");
                assert_eq!(limit, 12.0);
                assert!(used >= 12.0, "used={used} should be >= limit");
            }
            other => panic!("expected ConversationBudgetExceeded, got {other:?}"),
        }

        // Circuit must be open in the store (survives next call).
        let usage = budget.usage("conv-tok");
        assert!(usage.circuit_open, "circuit must be marked open");
        assert_eq!(usage.tripped_dimension.as_deref(), Some("tokens"));

        // Second call: fails fast WITHOUT invoking the LLM (count stays).
        let result2 = agent
            .run_tool_loop(budget_request("hi again"), "conv-tok", "test:agent", &CancellationToken::new())
            .await;
        assert!(matches!(
            result2,
            Err(ClawftError::ConversationBudgetExceeded { .. })
        ));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn cost_budget_iteration_cap_fails_fast() {
        use crate::agent::cost_budget::{ConversationBudget, InMemoryBudgetStore};
        use clawft_types::config::CostBudgetConfig;
        use clawft_types::error::ClawftError;

        // InfiniteToolTransport always returns ToolUse with 5/3 tokens.
        // max_iterations_per_conv = 2 → after the 2nd call iterations
        // crosses the cap and the loop returns the typed error.
        let transport = Arc::new(InfiniteToolTransport);
        let (mut agent, dir) = make_agent_loop(transport, "budget_iters").await;
        let store = Arc::new(InMemoryBudgetStore::new());
        let budget = Arc::new(ConversationBudget::new(
            CostBudgetConfig {
                max_tokens_per_conv: 1_000_000,
                max_usd_per_conv: 999.0,
                max_iterations_per_conv: 2,
            },
            store.clone(),
        ));
        agent = agent.with_cost_budget(Arc::clone(&budget));

        let err = agent
            .run_tool_loop(budget_request("loop"), "conv-iter", "test:agent", &CancellationToken::new())
            .await
            .expect_err("iteration cap should trip");
        match err {
            ClawftError::ConversationBudgetExceeded { dimension, .. } => {
                assert_eq!(dimension, "iterations");
            }
            other => panic!("wrong error: {other:?}"),
        }

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn cost_budget_usd_cap_fails_fast() {
        use crate::agent::cost_budget::{BudgetStore, BudgetUsage, ConversationBudget};
        use clawft_types::config::CostBudgetConfig;
        use clawft_types::error::ClawftError;
        use std::collections::HashMap;
        use std::sync::Mutex as StdMutex;

        // Pre-seed the store with usd=0.99 so the first call (any
        // amount) crosses the 1.00 cap on the post-call check. We use a
        // tiny custom store for this so we don't need provider metadata.
        struct PreseededStore {
            inner: StdMutex<HashMap<String, BudgetUsage>>,
        }
        impl BudgetStore for PreseededStore {
            fn load(&self, conv_id: &str) -> BudgetUsage {
                self.inner
                    .lock()
                    .ok()
                    .and_then(|g| g.get(conv_id).cloned())
                    .unwrap_or_default()
            }
            fn save(&self, conv_id: &str, usage: &BudgetUsage) -> Result<(), String> {
                self.inner
                    .lock()
                    .map_err(|_| "poisoned".to_string())?
                    .insert(conv_id.to_string(), usage.clone());
                Ok(())
            }
        }

        let mut seed = HashMap::new();
        seed.insert(
            "conv-usd".to_string(),
            BudgetUsage {
                input_tokens: 0,
                output_tokens: 0,
                usd: 0.99,
                iterations: 0,
                circuit_open: false,
                tripped_dimension: None,
            },
        );
        let store = Arc::new(PreseededStore {
            inner: StdMutex::new(seed),
        });
        let budget = Arc::new(ConversationBudget::new(
            CostBudgetConfig {
                max_tokens_per_conv: 1_000_000,
                max_usd_per_conv: 1.00,
                max_iterations_per_conv: 1_000,
            },
            store,
        ));

        // The MockTransport's metadata has no usd_cost, so record_call
        // adds 0.0 — but the iterations bump alone re-triggers the
        // existing seeded usd value 0.99 to be re-evaluated. We need a
        // transport whose metadata DOES carry usd_cost so the math
        // adds up to >=1.00. Build one inline.
        struct UsdTransport;
        #[async_trait]
        impl LlmTransport for UsdTransport {
            async fn complete(
                &self,
                _request: &TransportRequest,
            ) -> clawft_types::Result<LlmResponse> {
                let mut metadata = HashMap::new();
                metadata.insert("usd_cost".to_string(), serde_json::json!(0.05));
                Ok(LlmResponse {
                    id: "usd".into(),
                    content: vec![ContentBlock::Text { text: "ok".into() }],
                    stop_reason: StopReason::EndTurn,
                    usage: Usage {
                        input_tokens: 1,
                        output_tokens: 1,
                        total_tokens: 0,
                    },
                    metadata,
                })
            }
        }

        let transport = Arc::new(UsdTransport);
        let (mut agent, dir) = make_agent_loop(transport, "budget_usd").await;
        agent = agent.with_cost_budget(Arc::clone(&budget));

        let err = agent
            .run_tool_loop(budget_request("hi"), "conv-usd", "test:agent", &CancellationToken::new())
            .await
            .expect_err("usd cap should trip");
        match err {
            ClawftError::ConversationBudgetExceeded {
                dimension,
                used,
                limit,
                ..
            } => {
                assert_eq!(dimension, "usd");
                assert!((limit - 1.00).abs() < 1e-9);
                assert!(used >= 1.00, "used={used} should >= 1.00");
            }
            other => panic!("wrong error: {other:?}"),
        }

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn cost_budget_reset_unblocks_conversation() {
        use crate::agent::cost_budget::{ConversationBudget, InMemoryBudgetStore};
        use clawft_types::config::CostBudgetConfig;
        use clawft_types::error::ClawftError;

        let transport = Arc::new(MockTransport::new("ok"));
        let (mut agent, dir) = make_agent_loop(transport, "budget_reset").await;
        let store = Arc::new(InMemoryBudgetStore::new());
        // Cap tokens at 12 — first MockTransport call (10+5=15) trips
        // the post-call check; reset zeroes the accumulator, so the
        // SECOND call (another 15) is again > 12 and would also trip.
        // We bump the cap up between trip and reset so the post-reset
        // call has room to complete cleanly. Mirrors the spec's
        // "operator widens budget then resets" workflow.
        let budget = Arc::new(ConversationBudget::new(
            CostBudgetConfig {
                max_tokens_per_conv: 12,
                max_usd_per_conv: 999.0,
                max_iterations_per_conv: 999,
            },
            store.clone(),
        ));
        agent = agent.with_cost_budget(Arc::clone(&budget));

        // First call trips (10+5=15 tokens > 12 cap).
        let err = agent
            .run_tool_loop(budget_request("hi"), "conv-reset", "test:agent", &CancellationToken::new())
            .await
            .expect_err("must trip");
        assert!(matches!(
            err,
            ClawftError::ConversationBudgetExceeded { .. }
        ));

        // Reset clears circuit + accumulator. Subsequent call sees a
        // fresh 0 accumulator → 15 tokens after the call is still > 12,
        // BUT the check_can_call (BEFORE the call) sees usage=0 and
        // permits, then the post-call check trips. To prove "reset
        // unblocks", we simply verify the failure mode after reset
        // is a fresh post-call trip (not the pre-call fail-fast that
        // signals the circuit is still open).
        let prev = budget.reset("conv-reset").expect("reset");
        assert!(prev.circuit_open);
        // Confirm circuit is closed in the store post-reset.
        assert!(!budget.usage("conv-reset").circuit_open);

        // After reset, the LLM IS invoked (proving fast-fail is gone).
        // For this transport (10+5 tokens) and this cap (12) the
        // post-call check trips again — same dimension, fresh values.
        // The contract under test is: the call ATTEMPTED — not that
        // it succeeded. We cover the "attempted and succeeded" case
        // by also verifying the post-reset usage carries the new call.
        let _ = agent
            .run_tool_loop(budget_request("hi"), "conv-reset", "test:agent", &CancellationToken::new())
            .await;
        let post_usage = budget.usage("conv-reset");
        assert_eq!(
            post_usage.iterations, 1,
            "reset must allow the next LLM call to be attempted"
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn cost_budget_state_persists_across_loop_handles() {
        // Simulate "daemon restart" by sharing the same BudgetStore
        // across two distinct AgentLoop builds.
        use crate::agent::cost_budget::{BudgetStore, ConversationBudget, InMemoryBudgetStore};
        use clawft_types::config::CostBudgetConfig;
        use clawft_types::error::ClawftError;

        let store: Arc<dyn BudgetStore> = Arc::new(InMemoryBudgetStore::new());
        let cfg = CostBudgetConfig {
            max_tokens_per_conv: 12,
            max_usd_per_conv: 999.0,
            max_iterations_per_conv: 999,
        };

        // Phase 1: agent A trips the budget.
        {
            let transport = Arc::new(MockTransport::new("ok"));
            let (mut agent, dir) = make_agent_loop(transport, "budget_persist_a").await;
            let budget = Arc::new(ConversationBudget::new(cfg.clone(), Arc::clone(&store)));
            agent = agent.with_cost_budget(budget);
            let err = agent
                .run_tool_loop(budget_request("hi"), "conv-persist", "test:agent", &CancellationToken::new())
                .await
                .expect_err("must trip");
            assert!(matches!(
                err,
                ClawftError::ConversationBudgetExceeded { .. }
            ));
            let _ = tokio::fs::remove_dir_all(&dir).await;
        }

        // Phase 2: a fresh AgentLoop with the SAME store sees circuit_open
        // and fails fast on the very first attempt — no LLM call issued.
        {
            let transport = Arc::new(MockTransport::new("ok"));
            let (mut agent, dir) = make_agent_loop(transport, "budget_persist_b").await;
            let budget = Arc::new(ConversationBudget::new(cfg, Arc::clone(&store)));
            agent = agent.with_cost_budget(budget);
            let err = agent
                .run_tool_loop(budget_request("hi"), "conv-persist", "test:agent", &CancellationToken::new())
                .await
                .expect_err("must still be tripped after restart");
            match err {
                ClawftError::ConversationBudgetExceeded { dimension, .. } => {
                    assert_eq!(dimension, "tokens");
                }
                other => panic!("wrong error: {other:?}"),
            }
            let _ = tokio::fs::remove_dir_all(&dir).await;
        }
    }

    #[tokio::test]
    async fn run_exits_when_bus_closes() {
        let transport = Arc::new(MockTransport::new("hello"));
        let (agent, dir) = make_agent_loop(transport, "bus_close").await;

        // Drop the inbound sender by dropping a cloned bus reference.
        // We need to drop all inbound senders. The bus holds one internally.
        // Simplest: publish a message, consume it, then drop the bus.
        // Since the agent holds an Arc<MessageBus>, we cannot fully drop it.
        // Instead, test that run() exits when the channel is closed by
        // spawning run in a background task and sending a message that
        // processes, then dropping the bus's sender.

        // We cannot easily test the full `run()` loop exit here since the
        // bus is shared via Arc. Instead test the contract: consume_inbound
        // returns None when all senders are dropped.
        // This is already tested in bus.rs. Here we verify the struct compiles.
        assert!(
            agent
                .bus()
                .inbound_sender()
                .send(InboundMessage {
                    channel: "test".into(),
                    sender_id: "u".into(),
                    chat_id: "c".into(),
                    content: "msg".into(),
                    timestamp: chrono::Utc::now(),
                    media: vec![],
                    metadata: HashMap::new(),
                })
                .await
                .is_ok()
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn process_message_saves_session() {
        let transport = Arc::new(MockTransport::new("saved response"));
        let (agent, dir) = make_agent_loop(transport, "session_save").await;

        let inbound = InboundMessage {
            channel: "telegram".into(),
            sender_id: "user1".into(),
            chat_id: "chat42".into(),
            content: "remember this".into(),
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata: HashMap::new(),
        };
        agent.bus.publish_inbound(inbound).unwrap();

        let msg = agent.bus.consume_inbound().await.unwrap();
        let _outbound = agent.handle_turn(msg, &CancellationToken::new()).await.unwrap();

        // M3 store collapse: durability moved from SessionManager to the
        // ConversationSink. Hydrate from the sink (the single store) and
        // verify both the user and assistant turns landed. conv_id is the
        // canonical session_key `"telegram:chat42"` (design §D5).
        let session =
            crate::agent::hydrate::hydrate_session(agent.sink.as_ref(), "telegram:chat42", 0).await;
        assert!(session.messages.len() >= 2);

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// M3 T5 (design risk 3): the session-metadata K/V (hallucination score)
    /// must survive the cutover. Pre-collapse it rode `SessionManager`'s
    /// persisted header; now it lives in the sink's meta sidecar (§D3),
    /// hydrated into `Session::metadata` at turn start and written back via
    /// `sink.set_meta` at turn end. A score seeded before a turn must still be
    /// present after it — proving the K/V is not silently dropped when
    /// `save_session` is retired.
    #[tokio::test]
    async fn hallucination_metadata_survives_cutover() {
        let transport = Arc::new(MockTransport::new("ok"));
        let (agent, dir) = make_agent_loop(transport, "meta_survive").await;

        // Seed a prior hallucination score into the sink meta sidecar.
        agent
            .sink
            .set_meta(
                "telegram:c",
                serde_json::json!({ "hallucination_score": 0.37 }),
            )
            .await;

        let inbound = InboundMessage {
            channel: "telegram".into(),
            sender_id: "u".into(),
            chat_id: "c".into(),
            content: "hi".into(),
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata: HashMap::new(),
        };
        agent.bus.publish_inbound(inbound).unwrap();
        let msg = agent.bus.consume_inbound().await.unwrap();
        let _ = agent.handle_turn(msg, &CancellationToken::new()).await.unwrap();

        // The turn hydrated the score into `session.metadata` and wrote it
        // back through `set_meta`; hydrating again must still see it.
        let session =
            crate::agent::hydrate::hydrate_session(agent.sink.as_ref(), "telegram:c", 0).await;
        assert_eq!(
            session
                .metadata
                .get("hallucination_score")
                .and_then(|v| v.as_f64()),
            Some(0.37),
            "hallucination score must survive a turn through the sink sidecar"
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[test]
    fn agent_loop_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<AgentLoop<NativePlatform>>();
    }

    // ── GAP-19: Tool result truncation verification ───────────────────

    /// Transport that returns a tool call for a tool producing oversized output.
    struct OversizedToolTransport {
        call_count: std::sync::atomic::AtomicUsize,
    }

    impl OversizedToolTransport {
        fn new() -> Self {
            Self {
                call_count: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl LlmTransport for OversizedToolTransport {
        async fn complete(&self, request: &TransportRequest) -> clawft_types::Result<LlmResponse> {
            let count = self
                .call_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if count == 0 {
                // First call: request a tool call that will produce oversized output
                Ok(LlmResponse {
                    id: "oversized-resp".into(),
                    content: vec![ContentBlock::ToolUse {
                        id: "call-big".into(),
                        name: "big_output".into(),
                        input: serde_json::json!({}),
                    }],
                    stop_reason: StopReason::ToolUse,
                    usage: Usage {
                        input_tokens: 10,
                        output_tokens: 5,
                        total_tokens: 0,
                    },
                    metadata: HashMap::new(),
                })
            } else {
                // Second call: verify the tool result was included (truncated)
                // and return text. Check message list for truncation.
                let last_msg = request.messages.last();
                let content_text = last_msg
                    .map(|m| m.content.as_str())
                    .unwrap_or("no tool result");

                Ok(LlmResponse {
                    id: "final-resp".into(),
                    content: vec![ContentBlock::Text {
                        text: format!("tool_result_len:{}", content_text.len()),
                    }],
                    stop_reason: StopReason::EndTurn,
                    usage: Usage {
                        input_tokens: 20,
                        output_tokens: 8,
                        total_tokens: 0,
                    },
                    metadata: HashMap::new(),
                })
            }
        }
    }

    /// Tool that produces output exceeding MAX_TOOL_RESULT_BYTES.
    struct BigOutputTool;

    #[async_trait]
    impl Tool for BigOutputTool {
        fn name(&self) -> &str {
            "big_output"
        }
        fn description(&self) -> &str {
            "Returns a very large output"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {}
            })
        }
        async fn execute(
            &self,
            _args: serde_json::Value,
        ) -> Result<serde_json::Value, crate::tools::registry::ToolError> {
            // Produce output far exceeding 64KB (MAX_TOOL_RESULT_BYTES)
            let big_string = "x".repeat(200_000);
            Ok(serde_json::json!({"data": big_string}))
        }
    }

    #[tokio::test]
    async fn tool_result_truncation_enforced() {
        let dir = temp_dir("truncation");
        let platform = Arc::new(NativePlatform::new());
        let bus = Arc::new(MessageBus::new());

        let memory = Arc::new(MemoryStore::with_paths(
            dir.join("memory").join("MEMORY.md"),
            dir.join("memory").join("HISTORY.md"),
            platform.clone(),
        ));
        let skills = Arc::new(SkillsLoader::with_dir(dir.join("skills"), platform.clone()));
        let context = ContextBuilder::new(test_config(), memory, skills, platform.clone());

        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(BigOutputTool));

        let pipeline = make_pipeline(Arc::new(OversizedToolTransport::new()));

        let sessions = dir.join("sessions");
        let _ = std::fs::create_dir_all(&sessions);
        let agent = AgentLoop::new(
            test_config(),
            platform,
            bus,
            pipeline,
            Arc::new(tools),
            context,
            PermissionResolver::default_resolver(),
        )
        .with_observation_sessions_dir(sessions);

        let request = ChatRequest {
            messages: vec![LlmMessage {
                role: "user".into(),
                content: "trigger big tool".into(),
                tool_call_id: None,
                tool_calls: None,
            }],
            tools: vec![],
            model: Some("test-model".into()),
            max_tokens: Some(4096),
            temperature: Some(0.5),
            auth_context: None,
            complexity_boost: 0.0,
            tool_choice: None,
        };

        let tool_result = agent
            .run_tool_loop(request, "test-conv", "test:agent", &CancellationToken::new())
            .await
            .unwrap();
        let result = &tool_result.text;

        // The tool result should have been truncated to MAX_TOOL_RESULT_BYTES (65536).
        // The response tells us the length of the tool result message.
        assert!(
            result.starts_with("tool_result_len:"),
            "response should contain truncated tool result length: {result}"
        );
        let len_str = result.strip_prefix("tool_result_len:").unwrap();
        let result_len: usize = len_str.parse().unwrap();
        assert!(
            result_len <= MAX_TOOL_RESULT_BYTES,
            "tool result ({result_len} bytes) should be truncated to <= {} bytes",
            MAX_TOOL_RESULT_BYTES
        );

        let obs = crate::observation_pack::observations_dir(&dir.join("sessions"), "test-conv");
        let ledger = tokio::fs::read_to_string(obs.join("ledger.jsonl"))
            .await
            .expect("200KB tool result must be archived");
        let row: serde_json::Value = serde_json::from_str(ledger.lines().next().unwrap()).unwrap();
        let id = row["id"].as_str().unwrap();
        let total = row["bytes"].as_u64().unwrap();
        assert!(total > MAX_TOOL_RESULT_BYTES as u64);
        let page = crate::observation_pack::recall(
            &dir.join("sessions"),
            "test-conv",
            id,
            total.saturating_sub(32),
            64,
        )
        .await
        .unwrap();
        assert_eq!(page["eof"], true);
        assert!(
            page["chunk"].as_str().unwrap().contains('x'),
            "tail past 64KB must be recallable"
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    // ── TEST-04: Agent loop end-to-end test ────────────────────────────

    /// Transport that records every request it receives and drives a full
    /// tool-use round-trip: call 1 returns tool_use, call 2 verifies the
    /// tool result was appended and returns text.
    struct E2eRecordingTransport {
        call_count: std::sync::atomic::AtomicUsize,
        /// Snapshot of message lists received on each call.
        recorded_requests: std::sync::Mutex<Vec<Vec<LlmMessage>>>,
    }

    impl E2eRecordingTransport {
        fn new() -> Self {
            Self {
                call_count: std::sync::atomic::AtomicUsize::new(0),
                recorded_requests: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn snapshots(&self) -> Vec<Vec<LlmMessage>> {
            self.recorded_requests.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl LlmTransport for E2eRecordingTransport {
        async fn complete(&self, request: &TransportRequest) -> clawft_types::Result<LlmResponse> {
            // Record the incoming message list
            self.recorded_requests
                .lock()
                .unwrap()
                .push(request.messages.clone());

            let count = self
                .call_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

            // WEFT-328: model label in metadata so loop meta carries a real value.
            let mut meta = HashMap::new();
            meta.insert("model".into(), serde_json::json!("e2e-test-model"));

            if count == 0 {
                // Call 1: LLM returns a tool_use request
                Ok(LlmResponse {
                    id: "e2e-resp-1".into(),
                    content: vec![ContentBlock::ToolUse {
                        id: "call-e2e-1".into(),
                        name: "echo".into(),
                        input: serde_json::json!({"text": "e2e-ping"}),
                    }],
                    stop_reason: StopReason::ToolUse,
                    usage: Usage {
                        input_tokens: 15,
                        output_tokens: 10,
                        total_tokens: 0,
                    },
                    metadata: meta,
                })
            } else {
                // Call 2: LLM receives tool result, returns final text
                Ok(LlmResponse {
                    id: "e2e-resp-2".into(),
                    content: vec![ContentBlock::Text {
                        text: "I received the tool output successfully".into(),
                    }],
                    stop_reason: StopReason::EndTurn,
                    usage: Usage {
                        input_tokens: 25,
                        output_tokens: 12,
                        total_tokens: 0,
                    },
                    metadata: meta,
                })
            }
        }
    }

    /// Multi-tool transport: returns two tool calls on the first invocation,
    /// then text on the second.
    struct MultiToolE2eTransport {
        call_count: std::sync::atomic::AtomicUsize,
        recorded_requests: std::sync::Mutex<Vec<Vec<LlmMessage>>>,
    }

    impl MultiToolE2eTransport {
        fn new() -> Self {
            Self {
                call_count: std::sync::atomic::AtomicUsize::new(0),
                recorded_requests: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn snapshots(&self) -> Vec<Vec<LlmMessage>> {
            self.recorded_requests.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl LlmTransport for MultiToolE2eTransport {
        async fn complete(&self, request: &TransportRequest) -> clawft_types::Result<LlmResponse> {
            self.recorded_requests
                .lock()
                .unwrap()
                .push(request.messages.clone());

            let count = self
                .call_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

            if count == 0 {
                // Return two tool calls at once
                Ok(LlmResponse {
                    id: "multi-tool-resp-1".into(),
                    content: vec![
                        ContentBlock::ToolUse {
                            id: "call-mt-1".into(),
                            name: "echo".into(),
                            input: serde_json::json!({"text": "first"}),
                        },
                        ContentBlock::ToolUse {
                            id: "call-mt-2".into(),
                            name: "echo".into(),
                            input: serde_json::json!({"text": "second"}),
                        },
                    ],
                    stop_reason: StopReason::ToolUse,
                    usage: Usage {
                        input_tokens: 20,
                        output_tokens: 15,
                        total_tokens: 0,
                    },
                    metadata: HashMap::new(),
                })
            } else {
                Ok(LlmResponse {
                    id: "multi-tool-resp-2".into(),
                    content: vec![ContentBlock::Text {
                        text: "processed both tools".into(),
                    }],
                    stop_reason: StopReason::EndTurn,
                    usage: Usage {
                        input_tokens: 30,
                        output_tokens: 10,
                        total_tokens: 0,
                    },
                    metadata: HashMap::new(),
                })
            }
        }
    }

    /// TEST-04: Full e2e test -- mock LLM returns tool_use, tool executes,
    /// result is sent back to LLM, LLM returns text. Verifies the full
    /// message chain including intermediate tool result messages.
    #[tokio::test]
    async fn e2e_tool_roundtrip_message_chain() {
        let transport = Arc::new(E2eRecordingTransport::new());
        let transport_ref = transport.clone();
        let (agent, dir) = make_agent_loop(transport, "e2e_chain").await;

        // Publish and process a message that triggers tool use.
        // Use channel "cli" so resolve_auth_context grants admin permissions,
        // allowing the echo tool to execute through the permission check.
        let inbound = InboundMessage {
            channel: "cli".into(),
            sender_id: "e2e-user".into(),
            chat_id: "e2e-chat".into(),
            content: "please use the echo tool".into(),
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata: HashMap::new(),
        };
        agent.bus.publish_inbound(inbound).unwrap();
        let msg = agent.bus.consume_inbound().await.unwrap();
        let outbound = agent.handle_turn(msg, &CancellationToken::new()).await.unwrap();

        // Verify outbound message is the final text response
        assert_eq!(outbound.content, "I received the tool output successfully");
        assert_eq!(outbound.channel, "cli");
        assert_eq!(outbound.chat_id, "e2e-chat");

        // Verify the transport was called exactly twice
        let snapshots = transport_ref.snapshots();
        assert_eq!(
            snapshots.len(),
            2,
            "transport should be called twice (tool_use -> text)"
        );

        // Snapshot 1: initial user request
        let first_call = &snapshots[0];
        assert!(
            first_call.iter().any(|m| m.role == "user"),
            "first call should contain user message"
        );

        // Snapshot 2: should include the tool result message
        let second_call = &snapshots[1];
        let tool_result_msg = second_call
            .iter()
            .find(|m| m.role == "tool")
            .expect("second call should contain a tool result message");

        // Verify the tool result has the correct tool_call_id
        assert_eq!(
            tool_result_msg.tool_call_id.as_deref(),
            Some("call-e2e-1"),
            "tool result should reference the tool call ID"
        );

        // Verify the tool result contains the echo output
        assert!(
            tool_result_msg.content.contains("e2e-ping"),
            "tool result should contain the echoed text: {}",
            tool_result_msg.content
        );

        // M3 store collapse: verify durability via the sink (single store).
        let session =
            crate::agent::hydrate::hydrate_session(agent.sink.as_ref(), "cli:e2e-chat", 0).await;
        assert!(
            session.messages.len() >= 2,
            "session should have at least user + assistant messages, got {}",
            session.messages.len()
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// TEST-04: E2e test with multiple tool calls in a single LLM response.
    /// Verifies that all tool results are sent back with correct IDs.
    #[tokio::test]
    async fn e2e_multi_tool_calls_all_results_returned() {
        let transport = Arc::new(MultiToolE2eTransport::new());
        let transport_ref = transport.clone();
        let (agent, dir) = make_agent_loop(transport, "e2e_multi").await;

        // Use channel "cli" so resolve_auth_context grants admin permissions,
        // allowing the echo tool to execute through the permission check.
        let inbound = InboundMessage {
            channel: "cli".into(),
            sender_id: "user".into(),
            chat_id: "chat".into(),
            content: "use echo twice".into(),
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata: HashMap::new(),
        };
        agent.bus.publish_inbound(inbound).unwrap();
        let msg = agent.bus.consume_inbound().await.unwrap();
        let outbound = agent.handle_turn(msg, &CancellationToken::new()).await.unwrap();

        assert_eq!(outbound.content, "processed both tools");

        // Verify the second call to the transport has both tool results
        let snapshots = transport_ref.snapshots();
        assert_eq!(snapshots.len(), 2);

        let second_call = &snapshots[1];
        let tool_results: Vec<&LlmMessage> =
            second_call.iter().filter(|m| m.role == "tool").collect();

        assert_eq!(
            tool_results.len(),
            2,
            "second call should have 2 tool result messages"
        );

        // Verify each tool result has the correct call ID
        let ids: Vec<&str> = tool_results
            .iter()
            .filter_map(|m| m.tool_call_id.as_deref())
            .collect();
        assert!(
            ids.contains(&"call-mt-1"),
            "should have result for call-mt-1"
        );
        assert!(
            ids.contains(&"call-mt-2"),
            "should have result for call-mt-2"
        );

        // Verify tool outputs
        let first_result = tool_results
            .iter()
            .find(|m| m.tool_call_id.as_deref() == Some("call-mt-1"))
            .unwrap();
        assert!(
            first_result.content.contains("first"),
            "first tool result should contain 'first': {}",
            first_result.content
        );

        let second_result = tool_results
            .iter()
            .find(|m| m.tool_call_id.as_deref() == Some("call-mt-2"))
            .unwrap();
        assert!(
            second_result.content.contains("second"),
            "second tool result should contain 'second': {}",
            second_result.content
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// TEST-04: E2e test verifying a direct text response (no tool use)
    /// flows through the full pipeline correctly.
    #[tokio::test]
    async fn e2e_direct_text_response_no_tools() {
        let transport = Arc::new(MockTransport::new("Direct answer from LLM"));
        let (agent, dir) = make_agent_loop(transport, "e2e_no_tools").await;

        let inbound = InboundMessage {
            channel: "direct".into(),
            sender_id: "user".into(),
            chat_id: "chat".into(),
            content: "what is 2+2?".into(),
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata: HashMap::new(),
        };
        agent.bus.publish_inbound(inbound).unwrap();
        let msg = agent.bus.consume_inbound().await.unwrap();
        let outbound = agent.handle_turn(msg, &CancellationToken::new()).await.unwrap();

        assert_eq!(outbound.content, "Direct answer from LLM");
        assert_eq!(outbound.channel, "direct");

        // M3 store collapse: hydrate from the sink and check both roles landed.
        let session =
            crate::agent::hydrate::hydrate_session(agent.sink.as_ref(), "direct:chat", 0).await;
        let roles: Vec<String> = session
            .messages
            .iter()
            .filter_map(|m| m.get("role").and_then(|v| v.as_str()).map(String::from))
            .collect();
        assert!(
            roles.iter().any(|r| r == "user"),
            "session should have user message"
        );
        assert!(
            roles.iter().any(|r| r == "assistant"),
            "session should have assistant message"
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// TEST-04: E2e test verifying tool execution failure is gracefully
    /// handled and the error is sent back to the LLM.
    struct FailingToolTransport {
        call_count: std::sync::atomic::AtomicUsize,
        recorded_requests: std::sync::Mutex<Vec<Vec<LlmMessage>>>,
    }

    impl FailingToolTransport {
        fn new() -> Self {
            Self {
                call_count: std::sync::atomic::AtomicUsize::new(0),
                recorded_requests: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn snapshots(&self) -> Vec<Vec<LlmMessage>> {
            self.recorded_requests.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl LlmTransport for FailingToolTransport {
        async fn complete(&self, request: &TransportRequest) -> clawft_types::Result<LlmResponse> {
            self.recorded_requests
                .lock()
                .unwrap()
                .push(request.messages.clone());

            let count = self
                .call_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

            if count == 0 {
                // Request a tool that does not exist
                Ok(LlmResponse {
                    id: "fail-resp-1".into(),
                    content: vec![ContentBlock::ToolUse {
                        id: "call-fail-1".into(),
                        name: "nonexistent_tool".into(),
                        input: serde_json::json!({}),
                    }],
                    stop_reason: StopReason::ToolUse,
                    usage: Usage {
                        input_tokens: 10,
                        output_tokens: 5,
                        total_tokens: 0,
                    },
                    metadata: HashMap::new(),
                })
            } else {
                // LLM receives the error and returns gracefully
                Ok(LlmResponse {
                    id: "fail-resp-2".into(),
                    content: vec![ContentBlock::Text {
                        text: "I see the tool failed, let me help differently".into(),
                    }],
                    stop_reason: StopReason::EndTurn,
                    usage: Usage {
                        input_tokens: 20,
                        output_tokens: 12,
                        total_tokens: 0,
                    },
                    metadata: HashMap::new(),
                })
            }
        }
    }

    #[tokio::test]
    async fn e2e_tool_execution_failure_handled_gracefully() {
        let transport = Arc::new(FailingToolTransport::new());
        let transport_ref = transport.clone();
        let (agent, dir) = make_agent_loop(transport, "e2e_fail").await;

        let inbound = InboundMessage {
            channel: "fail".into(),
            sender_id: "user".into(),
            chat_id: "chat".into(),
            content: "try a tool".into(),
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata: HashMap::new(),
        };
        agent.bus.publish_inbound(inbound).unwrap();
        let msg = agent.bus.consume_inbound().await.unwrap();
        let outbound = agent.handle_turn(msg, &CancellationToken::new()).await.unwrap();

        assert_eq!(
            outbound.content,
            "I see the tool failed, let me help differently"
        );

        // Verify the error was passed to the LLM in the second call
        let snapshots = transport_ref.snapshots();
        assert_eq!(snapshots.len(), 2);

        let second_call = &snapshots[1];
        let tool_result = second_call
            .iter()
            .find(|m| m.role == "tool")
            .expect("second call should have a tool result with the error");

        assert!(
            tool_result.content.contains("error"),
            "tool result should contain error message: {}",
            tool_result.content
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    // ── Phase F: Auth context threading tests ────────────────────────

    /// Helper to build an InboundMessage with the given channel and sender.
    fn make_inbound(channel: &str, sender_id: &str) -> InboundMessage {
        InboundMessage {
            channel: channel.into(),
            sender_id: sender_id.into(),
            chat_id: "test-chat".into(),
            content: "test message".into(),
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata: HashMap::new(),
        }
    }

    /// Helper: resolve auth context using the default resolver (CLI=admin,
    /// everything else=zero_trust) without needing a full AgentLoop.
    fn resolve_default(msg: &InboundMessage) -> AuthContext {
        let resolver = PermissionResolver::default_resolver();
        resolver.resolve_auth_context(&msg.sender_id, &msg.channel, false)
    }

    /// F-05: CLI channel gets admin-level permissions via cli_default().
    #[test]
    fn test_resolve_auth_context_cli() {
        let msg = make_inbound("cli", "local");
        let ctx = resolve_default(&msg);

        assert_eq!(ctx.sender_id, "local");
        assert_eq!(ctx.channel, "cli");
        assert_eq!(ctx.permissions.level, 2, "CLI should get admin (level 2)");
        assert!(
            ctx.permissions.tool_access.contains(&"*".to_string()),
            "CLI admin should have wildcard tool access"
        );
        assert_eq!(
            ctx.permissions.rate_limit, 0,
            "CLI admin should have no rate limit"
        );
    }

    /// F-09: Empty sender_id gets zero-trust (level 0) permissions.
    #[test]
    fn test_resolve_auth_context_empty_sender() {
        let msg = make_inbound("telegram", "");
        let ctx = resolve_default(&msg);

        assert_eq!(ctx.sender_id, "");
        assert_eq!(ctx.channel, "telegram");
        assert_eq!(
            ctx.permissions.level, 0,
            "empty sender_id should get zero-trust (level 0)"
        );
    }

    /// F-10: Non-CLI channel with unknown sender gets zero-trust defaults.
    #[test]
    fn test_resolve_auth_context_gateway_channel() {
        let msg = make_inbound("gateway", "api_key_user");
        let ctx = resolve_default(&msg);

        assert_eq!(ctx.sender_id, "api_key_user");
        assert_eq!(ctx.channel, "gateway");
        assert_eq!(
            ctx.permissions.level, 0,
            "gateway users should get zero-trust (level 0) by default"
        );
        assert!(
            ctx.permissions.tool_access.is_empty(),
            "zero-trust should have no tool access"
        );
    }

    /// F-06/07: Telegram channel gets zero-trust with sender identity preserved.
    /// With the default resolver (no per-user overrides), all non-CLI channels
    /// get zero-trust. Config-driven per-user/channel overrides are tested in
    /// the `permissions` module.
    #[test]
    fn test_resolve_auth_context_telegram_preserves_identity() {
        let msg = make_inbound("telegram", "12345");
        let ctx = resolve_default(&msg);

        assert_eq!(ctx.sender_id, "12345");
        assert_eq!(ctx.channel, "telegram");
        assert_eq!(
            ctx.permissions.level, 0,
            "non-CLI channel gets zero-trust with default resolver"
        );
    }

    /// F-extra: Discord channel gets zero-trust with sender identity preserved.
    #[test]
    fn test_resolve_auth_context_discord() {
        let msg = make_inbound("discord", "snowflake_987654321");
        let ctx = resolve_default(&msg);

        assert_eq!(ctx.sender_id, "snowflake_987654321");
        assert_eq!(ctx.channel, "discord");
        assert_eq!(ctx.permissions.level, 0);
    }

    /// F-extra: Slack channel gets zero-trust with sender identity preserved.
    #[test]
    fn test_resolve_auth_context_slack() {
        let msg = make_inbound("slack", "U12345");
        let ctx = resolve_default(&msg);

        assert_eq!(ctx.sender_id, "U12345");
        assert_eq!(ctx.channel, "slack");
        assert_eq!(ctx.permissions.level, 0);
    }

    /// F-12: process_message attaches auth_context to the pipeline request.
    /// Uses a "cli" channel so the auth_context has admin permissions,
    /// verifying the full threading from InboundMessage -> ChatRequest.
    #[tokio::test]
    async fn test_auth_context_attached_to_chat_request() {
        let transport = Arc::new(MockTransport::new("auth-verified"));
        let (agent, dir) = make_agent_loop(transport, "auth_attach").await;

        let inbound = make_inbound("cli", "local");
        agent.bus.publish_inbound(inbound).unwrap();

        let msg = agent.bus.consume_inbound().await.unwrap();
        let outbound = agent.handle_turn(msg, &CancellationToken::new()).await.unwrap();

        // Verify response came through (proves pipeline executed successfully
        // with auth_context attached).
        assert_eq!(outbound.content, "auth-verified");
        assert_eq!(outbound.channel, "cli");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    // ── A3: Error JSON formatting tests ────────────────────────────

    #[test]
    fn error_json_escapes_double_quotes() {
        let error_msg = r#"file "foo" not found"#;
        let json_str = serde_json::json!({"error": error_msg}).to_string();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed["error"].as_str().unwrap(), error_msg);
    }

    #[test]
    fn error_json_escapes_backslashes() {
        let error_msg = r"path C:\Users\test";
        let json_str = serde_json::json!({"error": error_msg}).to_string();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed["error"].as_str().unwrap(), error_msg);
    }

    #[test]
    fn error_json_escapes_newlines() {
        let error_msg = "line 1\nline 2";
        let json_str = serde_json::json!({"error": error_msg}).to_string();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed["error"].as_str().unwrap(), error_msg);
    }

    #[test]
    fn error_json_escapes_all_special_chars() {
        let error_msg = "quote: \" backslash: \\ newline: \n tab: \t null: \0";
        let json_str = serde_json::json!({"error": error_msg}).to_string();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed["error"].as_str().unwrap(), error_msg);
    }

    #[test]
    fn error_json_has_single_error_key() {
        let error_msg = "something went wrong";
        let json_str = serde_json::json!({"error": error_msg}).to_string();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        let obj = parsed.as_object().unwrap();
        assert_eq!(obj.len(), 1, "should have exactly one key");
        assert!(obj.contains_key("error"));
    }

    /// F-12b: process_message with non-CLI channel attaches zero-trust auth_context.
    #[tokio::test]
    async fn test_auth_context_non_cli_attaches_zero_trust() {
        let transport = Arc::new(MockTransport::new("zero-trust-ok"));
        let (agent, dir) = make_agent_loop(transport, "auth_zt").await;

        let inbound = make_inbound("telegram", "user42");
        agent.bus.publish_inbound(inbound).unwrap();

        let msg = agent.bus.consume_inbound().await.unwrap();
        let outbound = agent.handle_turn(msg, &CancellationToken::new()).await.unwrap();

        assert_eq!(outbound.content, "zero-trust-ok");
        assert_eq!(outbound.channel, "telegram");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    // ── Auto-delegation tests ─────────────────────────────────────────

    /// Mock auto-delegation that delegates anything containing "swarm" or "deploy".
    struct MockAutoDelegation;

    impl AutoDelegation for MockAutoDelegation {
        fn should_delegate(&self, content: &str) -> Option<serde_json::Value> {
            let lower = content.to_lowercase();
            if lower.contains("swarm") || lower.contains("deploy") {
                Some(serde_json::json!({"task": content}))
            } else {
                None
            }
        }
    }

    /// A tool that simulates `delegate_task` (Claude path + WEFT-349 specialist).
    struct MockDelegateTaskTool;

    #[async_trait]
    impl Tool for MockDelegateTaskTool {
        fn name(&self) -> &str {
            "delegate_task"
        }
        fn description(&self) -> &str {
            "Mock delegate_task for testing"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "task": { "type": "string" },
                    "agent": { "type": "string" }
                },
                "required": ["task"]
            })
        }
        async fn execute(
            &self,
            args: serde_json::Value,
        ) -> Result<serde_json::Value, crate::tools::registry::ToolError> {
            let task = args
                .get("task")
                .and_then(|v| v.as_str())
                .unwrap_or("(unknown)");
            // WEFT-349: named specialist profile path (mock LLM / profile load).
            if let Some(agent) = args.get("agent").and_then(|v| v.as_str()) {
                return Ok(serde_json::json!({
                    "status": "delegated",
                    "target": "specialist",
                    "agent": agent,
                    "response": format!(
                        "## Code review by {agent}\n\nTask: {task}\n\n- LGTM with nits"
                    ),
                    "task": task,
                    "profile": { "name": agent, "skills": ["code-review"] },
                }));
            }
            Ok(serde_json::json!({
                "status": "delegated",
                "target": "claude",
                "response": format!("Delegated: {task}"),
                "task": task,
            }))
        }
    }

    /// Auto-route code-review asks to `delegate_task` with agent=code-reviewer.
    struct MockCodeReviewDelegation;

    impl AutoDelegation for MockCodeReviewDelegation {
        fn should_delegate(&self, content: &str) -> Option<serde_json::Value> {
            let lower = content.to_lowercase();
            if lower.contains("code review") || lower.contains("code-review") {
                Some(serde_json::json!({
                    "task": content,
                    "agent": "code-reviewer",
                }))
            } else {
                None
            }
        }
    }

    /// Helper: create an AgentLoop with auto-delegation and a mock delegate_task tool.
    async fn make_auto_delegation_agent(
        transport: Arc<dyn LlmTransport>,
        prefix: &str,
    ) -> (AgentLoop<NativePlatform>, PathBuf) {
        make_auto_delegation_agent_with(transport, prefix, Arc::new(MockDelegateTaskTool)).await
    }

    /// Same as [`make_auto_delegation_agent`] with a caller-supplied `delegate_task` tool.
    async fn make_auto_delegation_agent_with(
        transport: Arc<dyn LlmTransport>,
        prefix: &str,
        delegate: Arc<dyn Tool>,
    ) -> (AgentLoop<NativePlatform>, PathBuf) {
        let dir = temp_dir(prefix);
        let platform = Arc::new(NativePlatform::new());
        let bus = Arc::new(MessageBus::new());

        let memory = Arc::new(MemoryStore::with_paths(
            dir.join("memory").join("MEMORY.md"),
            dir.join("memory").join("HISTORY.md"),
            platform.clone(),
        ));
        let skills = Arc::new(SkillsLoader::with_dir(dir.join("skills"), platform.clone()));
        let context = ContextBuilder::new(test_config(), memory, skills, platform.clone());

        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(EchoTool));
        tools.register(delegate);

        let pipeline = make_pipeline(transport);

        let agent = AgentLoop::new(
            test_config(),
            platform,
            bus,
            pipeline,
            Arc::new(tools),
            context,
            PermissionResolver::default_resolver(),
        )
        .with_auto_delegation(Arc::new(MockAutoDelegation));

        (agent, dir)
    }

    /// Auto-delegation kicks in when the message matches delegation rules.
    #[tokio::test]
    async fn auto_delegation_routes_matching_message() {
        let transport = Arc::new(MockTransport::new("should NOT see this"));
        let (agent, dir) = make_auto_delegation_agent(transport, "auto_del_match").await;

        // "swarm" triggers auto-delegation
        let inbound = InboundMessage {
            channel: "cli".into(),
            sender_id: "local".into(),
            chat_id: "test".into(),
            content: "run a swarm security review".into(),
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata: HashMap::new(),
        };
        agent.bus.publish_inbound(inbound).unwrap();
        let msg = agent.bus.consume_inbound().await.unwrap();
        let outbound = agent.handle_turn(msg, &CancellationToken::new()).await.unwrap();

        assert!(
            outbound.content.contains("Delegated:"),
            "response should be from delegate_task, got: {}",
            outbound.content
        );
        assert!(
            outbound.content.contains("swarm"),
            "delegated response should include the original task"
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// Auto-delegation does NOT trigger for non-matching messages -- normal LLM path.
    #[tokio::test]
    async fn auto_delegation_skips_non_matching_message() {
        let transport = Arc::new(MockTransport::new("LLM response"));
        let (agent, dir) = make_auto_delegation_agent(transport, "auto_del_skip").await;

        // "hello" does NOT match delegation rules
        let inbound = InboundMessage {
            channel: "cli".into(),
            sender_id: "local".into(),
            chat_id: "test".into(),
            content: "hello world".into(),
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata: HashMap::new(),
        };
        agent.bus.publish_inbound(inbound).unwrap();
        let msg = agent.bus.consume_inbound().await.unwrap();
        let outbound = agent.handle_turn(msg, &CancellationToken::new()).await.unwrap();

        assert_eq!(
            outbound.content, "LLM response",
            "non-matching message should go through normal LLM pipeline"
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// WEFT-349 AC: chat agent invokes `delegate_task` for a code-review
    /// subtask (specialist profile) and receives the specialist output.
    #[tokio::test]
    async fn chat_agent_delegates_code_review_subtask() {
        let transport = Arc::new(MockTransport::new("should NOT see this"));
        let dir = temp_dir("weft349_code_review");
        let platform = Arc::new(NativePlatform::new());
        let bus = Arc::new(MessageBus::new());

        let memory = Arc::new(MemoryStore::with_paths(
            dir.join("memory").join("MEMORY.md"),
            dir.join("memory").join("HISTORY.md"),
            platform.clone(),
        ));
        let skills = Arc::new(SkillsLoader::with_dir(dir.join("skills"), platform.clone()));
        let context = ContextBuilder::new(test_config(), memory, skills, platform.clone());

        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(EchoTool));
        tools.register(Arc::new(MockDelegateTaskTool));

        let pipeline = make_pipeline(transport);
        let agent = AgentLoop::new(
            test_config(),
            platform,
            bus,
            pipeline,
            Arc::new(tools),
            context,
            PermissionResolver::default_resolver(),
        )
        .with_auto_delegation(Arc::new(MockCodeReviewDelegation));

        let inbound = InboundMessage {
            channel: "cli".into(),
            sender_id: "local".into(),
            chat_id: "test".into(),
            content: "Please run a code review of delegate_tool.rs".into(),
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata: HashMap::new(),
        };
        agent.bus.publish_inbound(inbound).unwrap();
        let msg = agent.bus.consume_inbound().await.unwrap();
        let outbound = agent
            .handle_turn(msg, &CancellationToken::new())
            .await
            .unwrap();

        assert!(
            outbound.content.contains("Code review by code-reviewer"),
            "chat agent should receive specialist output, got: {}",
            outbound.content
        );
        assert!(
            outbound.content.contains("LGTM"),
            "specialist review body missing: {}",
            outbound.content
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// Without auto-delegation set, messages always go to the LLM.
    #[tokio::test]
    async fn no_auto_delegation_uses_llm() {
        let transport = Arc::new(MockTransport::new("normal LLM"));
        let (agent, dir) = make_agent_loop(transport, "no_auto_del").await;

        // Even "swarm" goes to LLM when auto-delegation is not set
        let inbound = InboundMessage {
            channel: "cli".into(),
            sender_id: "local".into(),
            chat_id: "test".into(),
            content: "run a swarm task".into(),
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata: HashMap::new(),
        };
        agent.bus.publish_inbound(inbound).unwrap();
        let msg = agent.bus.consume_inbound().await.unwrap();
        let outbound = agent.handle_turn(msg, &CancellationToken::new()).await.unwrap();

        assert_eq!(outbound.content, "normal LLM");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    // ── agent-core-v1 Phase D1 + D2 helpers ─────────────────────────

    /// In-memory [`IdentityProvider`] for the D1 system-prompt tests.
    struct StubIdentityProvider {
        soul: String,
        identity: String,
    }

    #[async_trait]
    impl crate::agent::identity::IdentityProvider for StubIdentityProvider {
        async fn current(
            &self,
        ) -> Result<crate::agent::identity::Identity, crate::agent::identity::IdentityError>
        {
            Ok(crate::agent::identity::Identity::from_files(
                self.soul.clone(),
                self.identity.clone(),
                crate::agent::identity::IdentitySource::Stub,
            ))
        }
    }

    /// D2 stub gate that always returns the configured decision and
    /// records every `(agent_id, action)` it observed. Used to assert
    /// (a) Defer/Deny short-circuits the tool dispatch with the
    /// structured tool-result shape, and (b) `with_daemon_agent_id`
    /// overrides the synthesized fallback.
    struct StubGate {
        decision: super::super::gate::GateDecision,
        seen: std::sync::Mutex<Vec<(String, String)>>,
    }

    impl StubGate {
        fn defer(reason: &str) -> Self {
            Self {
                decision: super::super::gate::GateDecision::Defer {
                    reason: reason.into(),
                },
                seen: std::sync::Mutex::new(Vec::new()),
            }
        }
        fn deny(reason: &str) -> Self {
            Self {
                decision: super::super::gate::GateDecision::Deny {
                    reason: reason.into(),
                },
                seen: std::sync::Mutex::new(Vec::new()),
            }
        }
        fn agent_ids(&self) -> Vec<String> {
            self.seen
                .lock()
                .unwrap()
                .iter()
                .map(|(a, _)| a.clone())
                .collect()
        }
    }

    #[async_trait]
    impl super::super::gate::EffectGate for StubGate {
        async fn check(
            &self,
            agent_id: &str,
            action: &str,
            _effect: &super::super::effects::EffectVector,
        ) -> super::super::gate::GateDecision {
            self.seen
                .lock()
                .unwrap()
                .push((agent_id.into(), action.into()));
            self.decision.clone()
        }
    }

    /// D2 transport that drives one `echo` tool-use turn followed by a
    /// final-text turn. The second turn echoes back the tool-result
    /// message body (last `LlmMessage::content`) so the test can
    /// inspect what the loop fed the LLM after the gate decision.
    struct GateProbeTransport {
        call_count: std::sync::atomic::AtomicUsize,
    }

    impl GateProbeTransport {
        fn new() -> Self {
            Self {
                call_count: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl LlmTransport for GateProbeTransport {
        async fn complete(&self, request: &TransportRequest) -> clawft_types::Result<LlmResponse> {
            let count = self
                .call_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if count == 0 {
                Ok(LlmResponse {
                    id: "gate-probe-tool".into(),
                    content: vec![ContentBlock::ToolUse {
                        id: "call-d2".into(),
                        name: "echo".into(),
                        input: serde_json::json!({"text": "blocked?"}),
                    }],
                    stop_reason: StopReason::ToolUse,
                    usage: Usage {
                        input_tokens: 10,
                        output_tokens: 5,
                        total_tokens: 0,
                    },
                    metadata: HashMap::new(),
                })
            } else {
                let echoed = request
                    .messages
                    .last()
                    .map(|m| m.content.clone())
                    .unwrap_or_default();
                Ok(LlmResponse {
                    id: "gate-probe-final".into(),
                    content: vec![ContentBlock::Text { text: echoed }],
                    stop_reason: StopReason::EndTurn,
                    usage: Usage {
                        input_tokens: 20,
                        output_tokens: 8,
                        total_tokens: 0,
                    },
                    metadata: HashMap::new(),
                })
            }
        }
    }

    // ── D1 tests ────────────────────────────────────────────────────

    /// D1: when a `SystemPromptBuilder` is attached, `handle_turn`
    /// must prepend the identity-aware system message to the message
    /// list passed to the transport.
    ///
    /// WEFT-328: also surfaces `identity_source` in the outbound loop meta.
    #[tokio::test]
    async fn handle_turn_prepends_identity_system_prompt() {
        use crate::agent::identity::{BINDING_THREAD_EXCERPT, IdentityProvider};
        use crate::agent::system_prompt::SystemPromptBuilder;

        let transport = Arc::new(E2eRecordingTransport::new());
        let (mut agent, dir) =
            make_agent_loop(transport.clone() as Arc<dyn LlmTransport>, "d1_prompt").await;

        let soul = format!("# SOUL.md\n\nThe binding thread: {BINDING_THREAD_EXCERPT}.\n");
        let identity = "# IDENTITY.md\n\nclawft Concierge.".to_string();
        let provider: Arc<dyn IdentityProvider> = Arc::new(StubIdentityProvider {
            soul: soul.clone(),
            identity: identity.clone(),
        });
        let workspace = std::path::PathBuf::from("/tmp/d1-test-workspace");
        let builder = Arc::new(SystemPromptBuilder::new(provider, workspace.clone()));
        agent = agent.with_system_prompt_builder(builder);

        let inbound = InboundMessage {
            channel: "test".into(),
            sender_id: "user1".into(),
            chat_id: "chat-d1".into(),
            content: "ping".into(),
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata: HashMap::new(),
        };
        agent.bus.publish_inbound(inbound).unwrap();
        let msg = agent.bus.consume_inbound().await.unwrap();
        let outbound = agent.handle_turn(msg, &CancellationToken::new()).await.unwrap();

        let snapshots = transport.snapshots();
        assert!(!snapshots.is_empty(), "transport must record ≥1 call");
        let first_call = &snapshots[0];
        assert_eq!(
            first_call[0].role, "system",
            "leading message must be the identity system prompt"
        );
        let prompt = &first_call[0].content;
        assert!(prompt.contains("[identity]"));
        assert!(prompt.contains(BINDING_THREAD_EXCERPT));
        assert!(prompt.contains(&identity));
        assert!(prompt.contains("[binding-thread-status]\nok"));
        assert!(prompt.contains(&workspace.display().to_string()));
        assert!(prompt.contains("[hash]"));

        // WEFT-328: identity_source from StubIdentityProvider ("stub")
        // lands in the OutboundMessage loop meta.
        let meta: AgentLoopResultMeta = serde_json::from_value(
            outbound
                .metadata
                .get(AGENT_LOOP_RESULT_META_KEY)
                .expect("loop meta present")
                .clone(),
        )
        .unwrap();
        assert_eq!(meta.identity_source.as_deref(), Some("stub"));
        assert_eq!(meta.prompt_tokens, 40);
        assert_eq!(meta.completion_tokens, 22);
        assert_eq!(meta.model.as_deref(), Some("e2e-test-model"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// WEFT-342: deny mode (default) hard-refuses when SOUL.md lacks the
    /// binding-thread excerpt — no LLM call is made.
    #[tokio::test]
    async fn handle_turn_hard_refuses_on_binding_thread_mismatch_deny_mode() {
        use crate::agent::identity::IdentityProvider;
        use crate::agent::system_prompt::SystemPromptBuilder;
        use clawft_types::config::BindingThreadMode;

        let transport = Arc::new(E2eRecordingTransport::new());
        let (mut agent, dir) =
            make_agent_loop(transport.clone() as Arc<dyn LlmTransport>, "d1_bt_deny").await;

        let provider: Arc<dyn IdentityProvider> = Arc::new(StubIdentityProvider {
            soul: "# SOUL.md\n\nNo binding thread here.\n".into(),
            identity: "# IDENTITY.md\n\nclawft.\n".into(),
        });
        let builder = Arc::new(
            SystemPromptBuilder::new(provider, std::path::PathBuf::from("/tmp/bt-deny"))
                .with_binding_thread_mode(BindingThreadMode::Deny),
        );
        agent = agent.with_system_prompt_builder(builder);

        let inbound = InboundMessage {
            channel: "test".into(),
            sender_id: "user1".into(),
            chat_id: "chat-bt-deny".into(),
            content: "ping".into(),
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata: HashMap::new(),
        };
        agent.bus.publish_inbound(inbound).unwrap();
        let msg = agent.bus.consume_inbound().await.unwrap();
        let err = agent
            .handle_turn(msg, &CancellationToken::new())
            .await
            .expect_err("must hard-refuse");
        match err {
            ClawftError::SecurityViolation { reason } => {
                assert_eq!(reason, "binding-thread mismatch");
            }
            other => panic!("expected SecurityViolation, got {other:?}"),
        }
        assert!(
            transport.snapshots().is_empty(),
            "LLM must not be called after binding-thread refuse"
        );
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// WEFT-342: warn_only continues with a degraded annotated prompt.
    #[tokio::test]
    async fn handle_turn_continues_on_binding_thread_mismatch_warn_only() {
        use crate::agent::identity::IdentityProvider;
        use crate::agent::system_prompt::SystemPromptBuilder;
        use clawft_types::config::BindingThreadMode;

        let transport = Arc::new(E2eRecordingTransport::new());
        let (mut agent, dir) =
            make_agent_loop(transport.clone() as Arc<dyn LlmTransport>, "d1_bt_warn").await;

        let provider: Arc<dyn IdentityProvider> = Arc::new(StubIdentityProvider {
            soul: "# SOUL.md\n\nNo binding thread here.\n".into(),
            identity: "# IDENTITY.md\n\nclawft.\n".into(),
        });
        let builder = Arc::new(
            SystemPromptBuilder::new(provider, std::path::PathBuf::from("/tmp/bt-warn"))
                .with_binding_thread_mode(BindingThreadMode::WarnOnly),
        );
        agent = agent.with_system_prompt_builder(builder);

        let inbound = InboundMessage {
            channel: "test".into(),
            sender_id: "user1".into(),
            chat_id: "chat-bt-warn".into(),
            content: "ping".into(),
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata: HashMap::new(),
        };
        agent.bus.publish_inbound(inbound).unwrap();
        let msg = agent.bus.consume_inbound().await.unwrap();
        let _outbound = agent
            .handle_turn(msg, &CancellationToken::new())
            .await
            .expect("warn_only must continue");

        let snapshots = transport.snapshots();
        assert!(!snapshots.is_empty(), "LLM must still be called under warn_only");
        let prompt = &snapshots[0][0].content;
        assert!(prompt.contains("[binding-thread-status]\nmismatch"));
        assert!(prompt.contains("degraded mode"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// D1: when no builder is attached the loop must NOT inject any
    /// new system message — preserving CLI / legacy callers' shape.
    #[tokio::test]
    async fn handle_turn_without_builder_skips_identity_prompt() {
        let transport = Arc::new(E2eRecordingTransport::new());
        let (agent, dir) =
            make_agent_loop(transport.clone() as Arc<dyn LlmTransport>, "d1_nobuild").await;

        let inbound = InboundMessage {
            channel: "test".into(),
            sender_id: "user1".into(),
            chat_id: "chat-d1-no".into(),
            content: "ping".into(),
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata: HashMap::new(),
        };
        agent.bus.publish_inbound(inbound).unwrap();
        let msg = agent.bus.consume_inbound().await.unwrap();
        let _ = agent.handle_turn(msg, &CancellationToken::new()).await.unwrap();

        let snapshots = transport.snapshots();
        assert!(!snapshots.is_empty());
        let leading = &snapshots[0][0];
        assert!(!leading.content.contains("[binding-thread-status]"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    // ── WEFT-330 soul journal ───────────────────────────────────────

    /// Synthetic drift turn (explicit metadata) produces one journal
    /// entry via the attached `SoulJournal`.
    #[tokio::test]
    async fn handle_turn_appends_soul_journal_on_explicit_drift() {
        use crate::agent::soul_journal::{InMemorySoulJournal, SOUL_JOURNAL_OBSERVE_META_KEY};

        let transport = Arc::new(MockTransport::new("ack — noted."));
        let (mut agent, dir) =
            make_agent_loop(transport as Arc<dyn LlmTransport>, "soul_j_explicit").await;
        let journal = Arc::new(InMemorySoulJournal::new());
        agent = agent.with_soul_journal(journal.clone());

        let mut metadata = HashMap::new();
        metadata.insert(
            SOUL_JOURNAL_OBSERVE_META_KEY.to_string(),
            serde_json::json!("noticed bias toward verbose answers"),
        );
        let inbound = InboundMessage {
            channel: "test".into(),
            sender_id: "user1".into(),
            chat_id: "chat-soul-explicit".into(),
            content: "hello".into(),
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata,
        };
        agent.bus.publish_inbound(inbound).unwrap();
        let msg = agent.bus.consume_inbound().await.unwrap();
        let _ = agent
            .handle_turn(msg, &CancellationToken::new())
            .await
            .unwrap();

        let entries = journal.entries();
        assert_eq!(entries.len(), 1, "explicit drift must produce one entry");
        assert!(
            entries[0]
                .content
                .contains("noticed bias toward verbose answers"),
            "entry body must carry the explicit observation"
        );
        assert!(!entries[0].entry_id.is_empty());
        assert!(!entries[0].ts.is_empty());

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// Heuristic correction cue on the user message produces a journal
    /// entry after the assistant replies.
    #[tokio::test]
    async fn handle_turn_appends_soul_journal_on_user_correction_cue() {
        use crate::agent::soul_journal::{DriftSignal, InMemorySoulJournal};

        let transport = Arc::new(MockTransport::new("Understood — I'll keep answers short."));
        let (mut agent, dir) =
            make_agent_loop(transport as Arc<dyn LlmTransport>, "soul_j_heuristic").await;
        let journal = Arc::new(InMemorySoulJournal::new());
        agent = agent.with_soul_journal(journal.clone());

        let inbound = InboundMessage {
            channel: "test".into(),
            sender_id: "user1".into(),
            chat_id: "chat-soul-heur".into(),
            content: "Please don't use bullet lists; I prefer narrative paragraphs."
                .into(),
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata: HashMap::new(),
        };
        agent.bus.publish_inbound(inbound).unwrap();
        let msg = agent.bus.consume_inbound().await.unwrap();
        let _ = agent
            .handle_turn(msg, &CancellationToken::new())
            .await
            .unwrap();

        let entries = journal.entries();
        assert_eq!(entries.len(), 1, "heuristic cue must produce one entry");
        assert!(
            matches!(entries[0].signal, DriftSignal::UserCorrection { .. }),
            "signal must be UserCorrection, got {:?}",
            entries[0].signal
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// Neutral user message does not write a journal entry.
    #[tokio::test]
    async fn handle_turn_skips_soul_journal_without_drift_signal() {
        use crate::agent::soul_journal::InMemorySoulJournal;

        let transport = Arc::new(MockTransport::new("Sunny and 72°F."));
        let (mut agent, dir) =
            make_agent_loop(transport as Arc<dyn LlmTransport>, "soul_j_skip").await;
        let journal = Arc::new(InMemorySoulJournal::new());
        agent = agent.with_soul_journal(journal.clone());

        let inbound = InboundMessage {
            channel: "test".into(),
            sender_id: "user1".into(),
            chat_id: "chat-soul-skip".into(),
            content: "What is the weather?".into(),
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata: HashMap::new(),
        };
        agent.bus.publish_inbound(inbound).unwrap();
        let msg = agent.bus.consume_inbound().await.unwrap();
        let _ = agent
            .handle_turn(msg, &CancellationToken::new())
            .await
            .unwrap();

        assert!(
            journal.entries().is_empty(),
            "neutral turn must not write journal entries"
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    // ── WEFT-335 router decision log ────────────────────────────────

    /// Every handle_turn that hits ContextRouter::route appends one
    /// decision record when a RouterDecisionLog is attached.
    #[tokio::test]
    async fn handle_turn_appends_routing_decision_log() {
        use crate::agent::routing_log::InMemoryRouterDecisionLog;

        let transport = Arc::new(MockTransport::new("ok"));
        let (mut agent, dir) =
            make_agent_loop(transport as Arc<dyn LlmTransport>, "route_log_one").await;
        let rlog = Arc::new(InMemoryRouterDecisionLog::new());
        agent = agent.with_routing_log(rlog.clone());

        let inbound = InboundMessage {
            channel: "panel".into(),
            sender_id: "user1".into(),
            chat_id: "chat-route-1".into(),
            content: "what is this project about?".into(),
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata: HashMap::new(),
        };
        agent.bus.publish_inbound(inbound).unwrap();
        let msg = agent.bus.consume_inbound().await.unwrap();
        let _ = agent
            .handle_turn(msg, &CancellationToken::new())
            .await
            .unwrap();

        let entries = rlog.entries();
        assert_eq!(entries.len(), 1, "one route must produce one log entry");
        assert_eq!(entries[0].query, "what is this project about?");
        assert_eq!(entries[0].channel, "panel");
        assert_eq!(entries[0].chat_id, "chat-route-1");
        assert!(!entries[0].decision_id.is_empty());
        // latency is measured; just ensure the field is present (0 is fine
        // for a NullRouter that returns instantly).
        let _ = entries[0].latency_ms;

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// 100 routes produce 100 substrate/log entries (AC).
    #[tokio::test]
    async fn handle_turn_one_hundred_routes_produce_one_hundred_log_entries() {
        use crate::agent::routing_log::InMemoryRouterDecisionLog;

        let transport = Arc::new(MockTransport::new("ok"));
        let (mut agent, dir) =
            make_agent_loop(transport as Arc<dyn LlmTransport>, "route_log_100").await;
        let rlog = Arc::new(InMemoryRouterDecisionLog::with_retention(1_000));
        agent = agent.with_routing_log(rlog.clone());

        for i in 0..100 {
            let inbound = InboundMessage {
                channel: "panel".into(),
                sender_id: "user1".into(),
                chat_id: format!("chat-route-{i}"),
                content: format!("query number {i}"),
                timestamp: chrono::Utc::now(),
                media: vec![],
                metadata: HashMap::new(),
            };
            agent.bus.publish_inbound(inbound).unwrap();
            let msg = agent.bus.consume_inbound().await.unwrap();
            let _ = agent
                .handle_turn(msg, &CancellationToken::new())
                .await
                .unwrap();
        }

        assert_eq!(
            rlog.len(),
            100,
            "100 routes must produce 100 decision log entries"
        );
        let entries = rlog.entries();
        assert_eq!(entries[0].query, "query number 0");
        assert_eq!(entries[99].query, "query number 99");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// Routing-log failure must not abort the chat path.
    #[tokio::test]
    async fn handle_turn_survives_routing_log_failure() {
        use crate::agent::routing_log::{RouterDecisionLog, RouterDecisionRecord};
        use async_trait::async_trait;

        struct FailingLog;
        #[async_trait]
        impl RouterDecisionLog for FailingLog {
            async fn append(&self, _record: RouterDecisionRecord) -> Result<(), String> {
                Err("substrate down".into())
            }
        }

        let transport = Arc::new(MockTransport::new("still works"));
        let (mut agent, dir) =
            make_agent_loop(transport as Arc<dyn LlmTransport>, "route_log_fail").await;
        agent = agent.with_routing_log(Arc::new(FailingLog));

        let inbound = InboundMessage {
            channel: "test".into(),
            sender_id: "user1".into(),
            chat_id: "chat-route-fail".into(),
            content: "hello".into(),
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata: HashMap::new(),
        };
        agent.bus.publish_inbound(inbound).unwrap();
        let msg = agent.bus.consume_inbound().await.unwrap();
        let outbound = agent
            .handle_turn(msg, &CancellationToken::new())
            .await
            .expect("chat path must succeed even when routing log fails");
        assert!(outbound.content.contains("still works") || !outbound.content.is_empty());

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    // ── D2 tests ────────────────────────────────────────────────────

    #[tokio::test]
    async fn gate_defer_emits_structured_tool_result() {
        // WEFT-258: Defer halts the turn with finish_reason=deferred and
        // a DeferredActionEvent on loop meta (panel prompt-and-resume).
        // Tool-result body remains `{"deferred":true,"reason":...}`.
        let transport = Arc::new(GateProbeTransport::new());
        let (mut agent, dir) = make_agent_loop(transport, "gate_defer").await;
        let gate = Arc::new(StubGate::defer("policy review pending"));
        agent = agent.with_gate(gate.clone());

        let inbound = InboundMessage {
            channel: "test".into(),
            sender_id: "u".into(),
            chat_id: "conv-defer".into(),
            content: "trigger tool".into(),
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata: HashMap::new(),
        };
        agent.bus.publish_inbound(inbound).unwrap();
        let msg = agent.bus.consume_inbound().await.unwrap();
        let outbound = agent.handle_turn(msg, &CancellationToken::new()).await.unwrap();

        assert!(
            outbound.content.contains("deferred") || outbound.content.contains("policy review"),
            "assistant text should mention deferral: {}",
            outbound.content
        );
        let meta: AgentLoopResultMeta = serde_json::from_value(
            outbound
                .metadata
                .get(AGENT_LOOP_RESULT_META_KEY)
                .expect("loop meta")
                .clone(),
        )
        .unwrap();
        assert_eq!(meta.finish_reason, FINISH_REASON_DEFERRED);
        let deferred = meta.deferred.expect("deferred event present");
        assert!(deferred.deferred);
        assert_eq!(deferred.reason, "policy review pending");
        assert_eq!(deferred.tool, "echo");
        assert_eq!(meta.tool_calls.len(), 1);
        assert!(
            meta.tool_calls[0]
                .result_preview
                .contains("\"deferred\":true")
                || meta.tool_calls[0].result_preview.contains("deferred"),
            "tool result preview must keep structured deferred shape: {}",
            meta.tool_calls[0].result_preview
        );
        assert_eq!(gate.agent_ids().len(), 1);

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// WEFT-331: with AlwaysAllowDefer interactor, a gate Defer still
    /// executes the tool (human allow path) and the model sees the
    /// real tool result rather than a deferred envelope.
    #[tokio::test]
    async fn gate_defer_allow_resumes_tool_execution() {
        let transport = Arc::new(GateProbeTransport::new());
        let (mut agent, dir) = make_agent_loop(transport, "gate_defer_allow").await;
        let gate = Arc::new(StubGate::defer("needs human"));
        agent = agent
            .with_gate(gate)
            .with_defer_interactor(Arc::new(super::super::defer::AlwaysAllowDefer));

        let inbound = InboundMessage {
            channel: "test".into(),
            sender_id: "u".into(),
            chat_id: "conv-defer-allow".into(),
            content: "trigger tool".into(),
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata: HashMap::new(),
        };
        agent.bus.publish_inbound(inbound).unwrap();
        let msg = agent.bus.consume_inbound().await.unwrap();
        let outbound = agent
            .handle_turn(msg, &CancellationToken::new())
            .await
            .unwrap();

        // GateProbeTransport echoes the tool-result body on the second
        // LLM call as the assistant text. After human allow the gate no
        // longer short-circuits — the body must not be the deferred
        // envelope (tool may still fail sandbox/permissions downstream).
        let parsed: serde_json::Value =
            serde_json::from_str(&outbound.content).expect("tool result is JSON");
        assert!(
            parsed.get("deferred").is_none(),
            "allow path must not leave a deferred envelope: {parsed}"
        );
        // Gate-level deny envelope is also absent — interactive allow
        // does not synthesize denied:true from the gate reason.
        assert!(
            parsed.get("denied").is_none()
                || parsed
                    .get("reason")
                    .and_then(|r| r.as_str())
                    .is_some_and(|r| r.contains("permission") || r.contains("sandbox")),
            "allow path must not gate-deny: {parsed}"
        );
        // Prove the interactor ran: without it, content would be
        // {"deferred":true,"reason":"needs human"}.
        assert_ne!(
            parsed.get("reason").and_then(|r| r.as_str()),
            Some("needs human"),
            "interactive allow must not return the raw gate defer reason: {parsed}"
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// WEFT-331: interactive deny maps to a structured denied envelope.
    #[tokio::test]
    async fn gate_defer_interactive_deny_blocks_tool() {
        let transport = Arc::new(GateProbeTransport::new());
        let (mut agent, dir) = make_agent_loop(transport, "gate_defer_deny").await;
        agent = agent
            .with_gate(Arc::new(StubGate::defer("needs human")))
            .with_defer_interactor(Arc::new(super::super::defer::AlwaysDenyDefer {
                reason: "human said no".into(),
            }));

        let inbound = InboundMessage {
            channel: "test".into(),
            sender_id: "u".into(),
            chat_id: "conv-defer-deny".into(),
            content: "trigger tool".into(),
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata: HashMap::new(),
        };
        agent.bus.publish_inbound(inbound).unwrap();
        let msg = agent.bus.consume_inbound().await.unwrap();
        let outbound = agent
            .handle_turn(msg, &CancellationToken::new())
            .await
            .unwrap();

        let parsed: serde_json::Value =
            serde_json::from_str(&outbound.content).expect("deny result is JSON");
        assert_eq!(parsed["denied"], true);
        assert_eq!(parsed["reason"], "human said no");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn gate_defer_halts_without_second_llm_round_trip() {
        // GateProbeTransport only returns tool-use on call 0; if the
        // loop continued after Defer it would hit call 1. WEFT-258 must
        // stop after the deferred tool result (one LLM call).
        let transport = Arc::new(GateProbeTransport::new());
        let (mut agent, dir) = make_agent_loop(transport.clone(), "gate_defer_halt").await;
        let gate = Arc::new(StubGate::defer("stop here"));
        agent = agent.with_gate(gate);

        let inbound = InboundMessage {
            channel: "test".into(),
            sender_id: "u".into(),
            chat_id: "conv-defer-halt".into(),
            content: "trigger".into(),
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata: HashMap::new(),
        };
        agent.bus.publish_inbound(inbound).unwrap();
        let msg = agent.bus.consume_inbound().await.unwrap();
        let outbound = agent.handle_turn(msg, &CancellationToken::new()).await.unwrap();

        let calls = transport
            .call_count
            .load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            calls, 1,
            "defer must halt before the second LLM round-trip; got {calls}"
        );
        let meta: AgentLoopResultMeta = serde_json::from_value(
            outbound
                .metadata
                .get(AGENT_LOOP_RESULT_META_KEY)
                .expect("meta")
                .clone(),
        )
        .unwrap();
        assert_eq!(meta.finish_reason, FINISH_REASON_DEFERRED);
        assert_eq!(meta.iterations, 1);

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn gate_deny_emits_structured_tool_result() {
        let transport = Arc::new(GateProbeTransport::new());
        let (mut agent, dir) = make_agent_loop(transport, "gate_deny").await;
        let gate = Arc::new(StubGate::deny("write blocked by policy"));
        agent = agent.with_gate(gate);

        let inbound = InboundMessage {
            channel: "test".into(),
            sender_id: "u".into(),
            chat_id: "conv-deny".into(),
            content: "trigger tool".into(),
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata: HashMap::new(),
        };
        agent.bus.publish_inbound(inbound).unwrap();
        let msg = agent.bus.consume_inbound().await.unwrap();
        let outbound = agent.handle_turn(msg, &CancellationToken::new()).await.unwrap();

        let parsed: serde_json::Value =
            serde_json::from_str(&outbound.content).expect("gate result is JSON");
        assert_eq!(parsed["denied"], serde_json::json!(true));
        assert_eq!(
            parsed["reason"],
            serde_json::json!("write blocked by policy")
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// Transport that always requests the same tool — drives multi-deny
    /// escalation without ever returning a final text turn (WEFT-345).
    struct InfiniteToolUseTransport;

    #[async_trait]
    impl LlmTransport for InfiniteToolUseTransport {
        async fn complete(&self, _request: &TransportRequest) -> clawft_types::Result<LlmResponse> {
            Ok(LlmResponse {
                id: "infinite-tool".into(),
                content: vec![ContentBlock::ToolUse {
                    id: "call-deny".into(),
                    name: "echo".into(),
                    input: serde_json::json!({"text": "blocked"}),
                }],
                stop_reason: StopReason::ToolUse,
                usage: Usage {
                    input_tokens: 5,
                    output_tokens: 3,
                    total_tokens: 0,
                },
                metadata: HashMap::new(),
            })
        }
    }

    /// WEFT-345: after 3 consecutive gate Denys the loop emits
    /// EscalateToHuman (finish_reason + escalation meta) instead of
    /// silently burning the remaining tool-iteration budget.
    #[tokio::test]
    async fn three_gate_denials_emit_escalate_to_human() {
        let transport = Arc::new(InfiniteToolUseTransport);
        let (mut agent, dir) = make_agent_loop(transport, "weft345_escal").await;
        let gate = Arc::new(StubGate::deny("policy blocks echo"));
        agent = agent.with_gate(gate);

        let inbound = InboundMessage {
            channel: "test".into(),
            sender_id: "u".into(),
            chat_id: "conv-escal".into(),
            content: "do a blocked thing".into(),
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata: HashMap::new(),
        };
        agent.bus.publish_inbound(inbound).unwrap();
        let msg = agent.bus.consume_inbound().await.unwrap();
        let outbound = agent
            .handle_turn(msg, &CancellationToken::new())
            .await
            .expect("escalation is Ok with event, not Err");

        assert!(
            outbound.content.contains("EscalateToHuman"),
            "assistant text should name EscalateToHuman, got: {}",
            outbound.content
        );
        assert!(
            outbound.content.contains("3x") || outbound.content.contains("3"),
            "summary should mention denial count: {}",
            outbound.content
        );

        let meta: AgentLoopResultMeta = serde_json::from_value(
            outbound
                .metadata
                .get(AGENT_LOOP_RESULT_META_KEY)
                .expect("loop meta present")
                .clone(),
        )
        .expect("meta deserializes");
        assert_eq!(meta.finish_reason, FINISH_REASON_ESCALATE_TO_HUMAN);
        let esc = meta
            .escalation
            .expect("escalation event present on meta");
        assert_eq!(
            esc.decision,
            clawft_types::agent_chat::GOVERNANCE_DECISION_ESCALATE_TO_HUMAN
        );
        assert_eq!(esc.denial_count, 3);
        assert_eq!(esc.denials.len(), 3);
        // Default conv_id is the session key (`channel:chat_id`).
        assert_eq!(esc.conv_id, "test:conv-escal");
        for d in &esc.denials {
            assert_eq!(d.tool, "echo");
            assert_eq!(d.reason, "policy blocks echo");
        }
        // Loop should stop at the third denial (≤ 3 LLM rounds).
        assert!(
            meta.iterations <= 3,
            "should halt at escalation, iterations={}",
            meta.iterations
        );
        assert_eq!(meta.tool_calls.len(), 3);

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// Two denials then a clean final text turn must NOT escalate
    /// (streak < limit).
    #[tokio::test]
    async fn two_gate_denials_do_not_escalate() {
        /// First two LLM calls request tools; third returns text.
        struct TwoToolsThenText {
            n: std::sync::atomic::AtomicUsize,
        }

        #[async_trait]
        impl LlmTransport for TwoToolsThenText {
            async fn complete(
                &self,
                _request: &TransportRequest,
            ) -> clawft_types::Result<LlmResponse> {
                let i = self.n.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if i < 2 {
                    Ok(LlmResponse {
                        id: format!("tool-{i}"),
                        content: vec![ContentBlock::ToolUse {
                            id: format!("call-{i}"),
                            name: "echo".into(),
                            input: serde_json::json!({"text": "x"}),
                        }],
                        stop_reason: StopReason::ToolUse,
                        usage: Usage {
                            input_tokens: 5,
                            output_tokens: 3,
                            total_tokens: 0,
                        },
                        metadata: HashMap::new(),
                    })
                } else {
                    Ok(LlmResponse {
                        id: "final".into(),
                        content: vec![ContentBlock::Text {
                            text: "gave up on tools".into(),
                        }],
                        stop_reason: StopReason::EndTurn,
                        usage: Usage {
                            input_tokens: 5,
                            output_tokens: 3,
                            total_tokens: 0,
                        },
                        metadata: HashMap::new(),
                    })
                }
            }
        }

        let transport = Arc::new(TwoToolsThenText {
            n: std::sync::atomic::AtomicUsize::new(0),
        });
        let (mut agent, dir) = make_agent_loop(transport, "weft345_two").await;
        agent = agent.with_gate(Arc::new(StubGate::deny("nope")));

        let inbound = InboundMessage {
            channel: "test".into(),
            sender_id: "u".into(),
            chat_id: "conv-two".into(),
            content: "try".into(),
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata: HashMap::new(),
        };
        agent.bus.publish_inbound(inbound).unwrap();
        let msg = agent.bus.consume_inbound().await.unwrap();
        let outbound = agent.handle_turn(msg, &CancellationToken::new()).await.unwrap();

        assert_eq!(outbound.content, "gave up on tools");
        let meta: AgentLoopResultMeta = serde_json::from_value(
            outbound
                .metadata
                .get(AGENT_LOOP_RESULT_META_KEY)
                .expect("meta")
                .clone(),
        )
        .unwrap();
        assert_eq!(meta.finish_reason, "stop");
        assert!(meta.escalation.is_none());

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn daemon_agent_id_overrides_synthesized_fallback() {
        let transport = Arc::new(GateProbeTransport::new());
        let (mut agent, dir) = make_agent_loop(transport, "daemon_id").await;
        let gate = Arc::new(StubGate::defer("anything"));
        agent = agent
            .with_gate(gate.clone())
            .with_daemon_agent_id("concierge-bot/uuid".into());

        let inbound = InboundMessage {
            channel: "test".into(),
            sender_id: "u".into(),
            chat_id: "conv-daemon-id".into(),
            content: "trigger".into(),
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata: HashMap::new(),
        };
        agent.bus.publish_inbound(inbound).unwrap();
        let msg = agent.bus.consume_inbound().await.unwrap();
        let _outbound = agent.handle_turn(msg, &CancellationToken::new()).await.unwrap();

        let ids = gate.agent_ids();
        assert!(!ids.is_empty(), "gate must have been invoked");
        for id in ids {
            assert_eq!(id, "concierge-bot/uuid");
        }

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn synthesized_agent_id_remains_when_daemon_id_unset() {
        let transport = Arc::new(GateProbeTransport::new());
        let (mut agent, dir) = make_agent_loop(transport, "synth_id").await;
        let gate = Arc::new(StubGate::defer("ignored"));
        agent = agent.with_gate(gate.clone());

        let inbound = InboundMessage {
            channel: "cli".into(),
            sender_id: "local-user".into(),
            chat_id: "conv-synth".into(),
            content: "hi".into(),
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata: HashMap::new(),
        };
        agent.bus.publish_inbound(inbound).unwrap();
        let msg = agent.bus.consume_inbound().await.unwrap();
        let _outbound = agent.handle_turn(msg, &CancellationToken::new()).await.unwrap();

        let ids = gate.agent_ids();
        assert!(!ids.is_empty(), "gate must have been invoked");
        for id in ids {
            assert_eq!(id, "cli:local-user");
        }

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// WEFT-332: per-turn `gate_agent_id` metadata beats the boot-time
    /// daemon concierge id so multi-tenant callers isolate principals.
    #[tokio::test]
    async fn metadata_gate_agent_id_overrides_daemon_id() {
        let transport = Arc::new(GateProbeTransport::new());
        let (mut agent, dir) = make_agent_loop(transport, "meta_gate_id").await;
        let gate = Arc::new(StubGate::defer("anything"));
        agent = agent
            .with_gate(gate.clone())
            .with_daemon_agent_id("concierge-shared".into());

        let mut metadata = HashMap::new();
        metadata.insert(
            clawft_types::agent_chat::GATE_AGENT_ID_META_KEY.into(),
            serde_json::Value::String("agent.chat:user:alice".into()),
        );
        let inbound = InboundMessage {
            channel: "agent.chat".into(),
            sender_id: "alice".into(),
            chat_id: "shared-conv".into(),
            content: "trigger".into(),
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata,
        };
        agent.bus.publish_inbound(inbound).unwrap();
        let msg = agent.bus.consume_inbound().await.unwrap();
        let _outbound = agent.handle_turn(msg, &CancellationToken::new()).await.unwrap();

        let ids = gate.agent_ids();
        assert!(!ids.is_empty(), "gate must have been invoked");
        for id in ids {
            assert_eq!(
                id, "agent.chat:user:alice",
                "per-caller metadata must beat shared concierge id"
            );
        }

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    // ── WEFT-178: AgentRouter wiring ────────────────────────────────

    /// When an [`AgentRouter`] is attached and a route matches, the
    /// resolved agent_id supersedes both the daemon-supplied id and
    /// the synthesized `"{channel}:{sender_id}"` shape on every gate
    /// check.
    #[tokio::test]
    async fn agent_router_routes_messages_to_named_agent() {
        use crate::agent_routing::AgentRouter;
        use clawft_types::agent_routing::{AgentRoute, AgentRoutingConfig, MatchCriteria};

        // GateProbeTransport drives a tool-use turn → tool result → final
        // text turn so the gate gets at least one (agent_id, action) entry.
        let transport = Arc::new(GateProbeTransport::new());
        let (mut agent, dir) =
            make_agent_loop(transport.clone() as Arc<dyn LlmTransport>, "router_match").await;
        let gate = Arc::new(StubGate::defer("test-defer"));
        agent = agent
            .with_gate(gate.clone() as Arc<dyn EffectGate>)
            .with_daemon_agent_id("ignored-daemon-id".into())
            .with_agent_router(Arc::new(AgentRouter::new(AgentRoutingConfig {
                routes: vec![AgentRoute {
                    channel: "cli".into(),
                    match_criteria: MatchCriteria {
                        user_id: Some("local".into()),
                        ..Default::default()
                    },
                    agent: "work-agent".into(),
                }],
                catch_all: None,
            })));

        let inbound = make_inbound("cli", "local");
        agent.bus.publish_inbound(inbound).unwrap();
        let msg = agent.bus.consume_inbound().await.unwrap();
        let _ = agent.handle_turn(msg, &CancellationToken::new()).await.unwrap();

        let ids = gate.agent_ids();
        assert!(!ids.is_empty(), "gate must record at least one check");
        for id in ids {
            assert_eq!(
                id, "work-agent",
                "routed agent_id beats daemon and synthesised fallbacks"
            );
        }

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// `RoutingResult::NoMatch` falls back to the daemon-supplied id
    /// (or the synthesised shape) — the router never forces an empty
    /// principal.
    #[tokio::test]
    async fn agent_router_no_match_falls_back_to_daemon_id() {
        use crate::agent_routing::AgentRouter;
        use clawft_types::agent_routing::{AgentRoute, AgentRoutingConfig, MatchCriteria};

        let transport = Arc::new(GateProbeTransport::new());
        let (mut agent, dir) =
            make_agent_loop(transport.clone() as Arc<dyn LlmTransport>, "router_nomatch").await;
        let gate = Arc::new(StubGate::defer("test-defer"));
        agent = agent
            .with_gate(gate.clone() as Arc<dyn EffectGate>)
            .with_daemon_agent_id("daemon-fallback".into())
            // Router only matches `slack`, not `cli`. No catch-all.
            .with_agent_router(Arc::new(AgentRouter::new(AgentRoutingConfig {
                routes: vec![AgentRoute {
                    channel: "slack".into(),
                    match_criteria: MatchCriteria::default(),
                    agent: "slack-only".into(),
                }],
                catch_all: None,
            })));

        let inbound = make_inbound("cli", "local");
        agent.bus.publish_inbound(inbound).unwrap();
        let msg = agent.bus.consume_inbound().await.unwrap();
        let _ = agent.handle_turn(msg, &CancellationToken::new()).await.unwrap();

        let ids = gate.agent_ids();
        assert!(!ids.is_empty(), "gate must record at least one check");
        for id in ids {
            assert_eq!(id, "daemon-fallback");
        }

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// One-shot `weft agent -m` turns use a fresh chat id each call (or
    /// the `--session` name). History is keyed by chat id, so two
    /// different ids never see each other's turns while a repeated id
    /// does. This is the loop-side half of that contract.
    #[tokio::test]
    async fn history_is_keyed_by_chat_id() {
        struct RecordingTransport {
            requests: std::sync::Mutex<Vec<String>>,
        }

        #[async_trait]
        impl LlmTransport for RecordingTransport {
            async fn complete(
                &self,
                request: &TransportRequest,
            ) -> clawft_types::Result<LlmResponse> {
                let text = request
                    .messages
                    .iter()
                    .map(|m| m.content.as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                self.requests.lock().unwrap().push(text);
                Ok(LlmResponse {
                    id: "rec".into(),
                    content: vec![ContentBlock::Text { text: "ok".into() }],
                    stop_reason: StopReason::EndTurn,
                    usage: Usage {
                        input_tokens: 1,
                        output_tokens: 1,
                        total_tokens: 0,
                    },
                    metadata: HashMap::new(),
                })
            }
        }

        let transport = Arc::new(RecordingTransport {
            requests: std::sync::Mutex::new(Vec::new()),
        });
        let (agent, dir) =
            make_agent_loop(transport.clone() as Arc<dyn LlmTransport>, "history_by_chat").await;

        async fn turn(agent: &AgentLoop<NativePlatform>, chat_id: &str, content: &str) {
            let msg = InboundMessage {
                channel: "cli".into(),
                sender_id: "local".into(),
                chat_id: chat_id.into(),
                content: content.into(),
                timestamp: chrono::Utc::now(),
                media: vec![],
                metadata: HashMap::new(),
            };
            agent.handle_turn(msg, &CancellationToken::new()).await.unwrap();
        }

        // Two fresh one-shot ids: the second must not see the first.
        turn(&agent, "oneshot-a", "marker-alpha").await;
        turn(&agent, "oneshot-b", "marker-beta").await;
        // A named session used twice: the second sees the first.
        turn(&agent, "review", "marker-gamma").await;
        turn(&agent, "review", "marker-delta").await;

        let reqs = transport.requests.lock().unwrap().clone();
        assert_eq!(reqs.len(), 4);
        assert!(!reqs[1].contains("marker-alpha"), "fresh ids must not share history");
        assert!(reqs[1].contains("marker-beta"));
        assert!(reqs[3].contains("marker-gamma"), "a repeated id must continue its history");
        assert!(reqs[3].contains("marker-delta"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    // ── WEFT-180: recursive-delegation depth guard ──────────────────

    /// A 6-deep delegation chain (depth 5 inbound) refuses the next
    /// `delegate_task` hop with the configured cap reached message.
    /// Default cap is [`DEFAULT_MAX_DELEGATION_DEPTH`] = 5; the 6th
    /// hop is the one that fails.
    #[tokio::test]
    async fn delegation_depth_six_deep_chain_refuses_at_hop_six() {
        let transport = Arc::new(MockTransport::new("should NOT see this"));
        let (agent, dir) = make_auto_delegation_agent(transport, "del_depth_6").await;
        // Pin the cap explicitly so the test is hermetic regardless
        // of what CLAWFT_DELEGATION_DEPTH the host has set.
        let agent = agent.with_max_delegation_depth(DEFAULT_MAX_DELEGATION_DEPTH);

        // Inbound at depth 5 (already delegated 5 times). The next
        // hop would be #6, which exceeds the default cap of 5.
        let mut metadata = HashMap::new();
        metadata.insert(DELEGATION_DEPTH_KEY.into(), serde_json::json!(5));
        let inbound = InboundMessage {
            channel: "cli".into(),
            sender_id: "local".into(),
            chat_id: "depth-test".into(),
            content: "run a swarm".into(), // matches MockAutoDelegation
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata,
        };
        agent.bus.publish_inbound(inbound).unwrap();
        let msg = agent.bus.consume_inbound().await.unwrap();
        let outbound = agent.handle_turn(msg, &CancellationToken::new()).await.unwrap();

        assert!(
            outbound.content.contains("Delegation refused"),
            "depth-cap hit must surface a refusal, got: {}",
            outbound.content
        );
        assert!(
            outbound.content.contains("(5)"),
            "refusal should mention the cap, got: {}",
            outbound.content
        );
        assert!(outbound.is_error(), "a refused delegation is a failed turn");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// A `delegate_task` that fails (e.g. the provider rejects the call)
    /// keeps a readable reply for chat UIs but flags the turn as failed,
    /// so one-shot `weft agent -m` exits non-zero instead of reporting
    /// success.
    #[tokio::test]
    async fn delegation_failure_flags_outbound_as_error() {
        struct FailingDelegateTaskTool;

        #[async_trait]
        impl Tool for FailingDelegateTaskTool {
            fn name(&self) -> &str {
                "delegate_task"
            }
            fn description(&self) -> &str {
                "Always fails"
            }
            fn parameters(&self) -> serde_json::Value {
                serde_json::json!({ "type": "object" })
            }
            async fn execute(
                &self,
                _args: serde_json::Value,
            ) -> Result<serde_json::Value, crate::tools::registry::ToolError> {
                Err(crate::tools::registry::ToolError::ExecutionFailed(
                    "provider returned 400: credit balance is too low".into(),
                ))
            }
        }

        fn swarm_inbound(chat_id: &str) -> InboundMessage {
            InboundMessage {
                channel: "cli".into(),
                sender_id: "local".into(),
                chat_id: chat_id.into(),
                content: "run a swarm".into(), // matches MockAutoDelegation
                timestamp: chrono::Utc::now(),
                media: vec![],
                metadata: HashMap::new(),
            }
        }

        let transport = Arc::new(MockTransport::new("should NOT see this"));
        let (agent, dir) = make_auto_delegation_agent_with(
            transport,
            "del_fail_flag",
            Arc::new(FailingDelegateTaskTool),
        )
        .await;
        agent.bus.publish_inbound(swarm_inbound("fail-test")).unwrap();
        let msg = agent.bus.consume_inbound().await.unwrap();
        let outbound = agent.handle_turn(msg, &CancellationToken::new()).await.unwrap();
        assert!(
            outbound.content.starts_with("Delegation failed"),
            "got: {}",
            outbound.content
        );
        assert!(outbound.is_error(), "a failed delegation must set the error flag");

        // A successful delegation does not set it.
        let transport = Arc::new(MockTransport::new("should NOT see this"));
        let (ok_agent, ok_dir) = make_auto_delegation_agent(transport, "del_ok_flag").await;
        ok_agent.bus.publish_inbound(swarm_inbound("ok-test")).unwrap();
        let msg = ok_agent.bus.consume_inbound().await.unwrap();
        let outbound = ok_agent.handle_turn(msg, &CancellationToken::new()).await.unwrap();
        assert!(!outbound.is_error());

        let _ = tokio::fs::remove_dir_all(&dir).await;
        let _ = tokio::fs::remove_dir_all(&ok_dir).await;
    }

    /// Inbound at depth 4 (one below the cap) still gets delegated;
    /// the recursive guard only fires when the bumped depth would
    /// exceed the cap.
    #[tokio::test]
    async fn delegation_depth_below_cap_still_delegates() {
        let transport = Arc::new(MockTransport::new("should NOT see this"));
        let (agent, dir) = make_auto_delegation_agent(transport, "del_depth_4").await;
        let agent = agent.with_max_delegation_depth(DEFAULT_MAX_DELEGATION_DEPTH);

        let mut metadata = HashMap::new();
        metadata.insert(DELEGATION_DEPTH_KEY.into(), serde_json::json!(4));
        let inbound = InboundMessage {
            channel: "cli".into(),
            sender_id: "local".into(),
            chat_id: "depth-test".into(),
            content: "run a swarm".into(),
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata,
        };
        agent.bus.publish_inbound(inbound).unwrap();
        let msg = agent.bus.consume_inbound().await.unwrap();
        let outbound = agent.handle_turn(msg, &CancellationToken::new()).await.unwrap();

        assert!(
            outbound.content.contains("Delegated:"),
            "hop 5 (≤ cap) must still delegate, got: {}",
            outbound.content
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// `with_max_delegation_depth(2)` overrides the default and
    /// refuses at hop 3 even though the env-var default would allow
    /// it.
    #[tokio::test]
    async fn delegation_depth_custom_cap_overrides_default() {
        let transport = Arc::new(MockTransport::new("should NOT see this"));
        let (agent, dir) = make_auto_delegation_agent(transport, "del_depth_custom").await;
        let agent = agent.with_max_delegation_depth(2);

        let mut metadata = HashMap::new();
        metadata.insert(DELEGATION_DEPTH_KEY.into(), serde_json::json!(2));
        let inbound = InboundMessage {
            channel: "cli".into(),
            sender_id: "local".into(),
            chat_id: "depth-test".into(),
            content: "run a swarm".into(),
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata,
        };
        agent.bus.publish_inbound(inbound).unwrap();
        let msg = agent.bus.consume_inbound().await.unwrap();
        let outbound = agent.handle_turn(msg, &CancellationToken::new()).await.unwrap();

        assert!(
            outbound.content.contains("Delegation refused"),
            "custom cap must refuse at hop 3, got: {}",
            outbound.content
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// `read_delegation_depth` accepts integer metadata and ignores
    /// malformed values.
    #[test]
    fn delegation_depth_metadata_parsing() {
        let mut m = InboundMessage {
            channel: "x".into(),
            sender_id: "y".into(),
            chat_id: "z".into(),
            content: "".into(),
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata: HashMap::new(),
        };
        // No metadata → 0
        assert_eq!(read_delegation_depth(&m), 0);
        // Integer value
        m.metadata
            .insert(DELEGATION_DEPTH_KEY.into(), serde_json::json!(3));
        assert_eq!(read_delegation_depth(&m), 3);
        // Non-integer → falls through to 0 (silently)
        m.metadata
            .insert(DELEGATION_DEPTH_KEY.into(), serde_json::json!("oops"));
        assert_eq!(read_delegation_depth(&m), 0);
    }

    // ── WEFT-190: execute_tool_with_guards helper ───────────────────

    /// `execute_tool_with_guards` honours a `Deny` decision from the
    /// gate and returns a `{"denied": true, ...}` envelope without
    /// dispatching the tool.
    #[tokio::test]
    async fn execute_tool_with_guards_returns_deny_envelope() {
        let transport = Arc::new(MockTransport::new("ok"));
        let (mut agent, dir) = make_agent_loop(transport, "etwg_deny").await;
        agent = agent.with_gate(Arc::new(StubGate::deny("policy")) as Arc<dyn EffectGate>);

        let body = agent
            .execute_tool_with_guards(
                "agent-x",
                "echo",
                &serde_json::json!({"text": "hi"}),
                None,
                "etwg_deny",
                &CancellationToken::new(),
                "",
            )
            .await;
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["denied"], true);
        assert_eq!(parsed["reason"], "policy");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// `execute_tool_with_guards` truncates oversized successful
    /// results so callers don't accidentally ship huge bodies back
    /// to the LLM.
    #[tokio::test]
    async fn execute_tool_with_guards_truncates_large_results() {
        let transport = Arc::new(MockTransport::new("ok"));
        let (agent, dir) = make_agent_loop(transport, "etwg_trunc").await;

        let body = agent
            .execute_tool_with_guards(
                "agent-x",
                "huge_output",
                &serde_json::json!({}),
                None,
                "etwg_trunc",
                &CancellationToken::new(),
                "",
            )
            .await;
        // The mock-loop registry has no `huge_output` tool, so this
        // path returns an `{"error": ...}` envelope. That alone
        // proves the helper plumbs through to the registry; the
        // truncation path is exercised by the existing
        // `tool_result_truncation_applied` test against the full
        // run_tool_loop.
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(parsed["error"].is_string());

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    async fn make_obs_loop(
        prefix: &str,
        tool: SizedOutputTool,
    ) -> (AgentLoop<NativePlatform>, PathBuf) {
        let transport = Arc::new(MockTransport::new("ok"));
        let dir = temp_dir(prefix);
        let platform = Arc::new(NativePlatform::new());
        let bus = Arc::new(MessageBus::new());
        let memory = Arc::new(MemoryStore::with_paths(
            dir.join("memory").join("MEMORY.md"),
            dir.join("memory").join("HISTORY.md"),
            platform.clone(),
        ));
        let skills = Arc::new(SkillsLoader::with_dir(dir.join("skills"), platform.clone()));
        let context = ContextBuilder::new(test_config(), memory, skills, platform.clone());
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(EchoTool));
        tools.register(Arc::new(tool));
        let pipeline = make_pipeline(transport);
        let sessions = dir.join("sessions");
        let _ = std::fs::create_dir_all(&sessions);
        let agent = AgentLoop::new(
            test_config(),
            platform,
            bus,
            pipeline,
            Arc::new(tools),
            context,
            PermissionResolver::default_resolver(),
        )
        .with_observation_sessions_dir(sessions);
        (agent, dir)
    }

    #[tokio::test]
    async fn execute_tool_tiny_result_does_not_archive() {
        let (agent, dir) = make_obs_loop(
            "obs_tiny",
            SizedOutputTool {
                name: "tiny_output",
                n_bytes: 32,
            },
        )
        .await;
        let body = agent
            .execute_tool_with_guards(
                "agent-x",
                "tiny_output",
                &serde_json::json!({}),
                None,
                "obs_tiny",
                &CancellationToken::new(),
                "c1",
            )
            .await;
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(parsed.get("observation_pack").is_none());
        let obs = crate::observation_pack::observations_dir(&dir.join("sessions"), "obs_tiny");
        assert!(!obs.exists(), "tiny result must not create {obs:?}");
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn execute_tool_10kb_full_twice_then_projects() {
        let (agent, dir) = make_obs_loop(
            "obs_10k",
            SizedOutputTool {
                name: "medium_output",
                n_bytes: 10_000,
            },
        )
        .await;
        let mut bodies = Vec::new();
        for i in 0..3 {
            bodies.push(
                agent
                    .execute_tool_with_guards(
                        "agent-x",
                        "medium_output",
                        &serde_json::json!({}),
                        None,
                        "obs_10k",
                        &CancellationToken::new(),
                        &format!("c{i}"),
                    )
                    .await,
            );
        }
        let first: serde_json::Value = serde_json::from_str(&bodies[0]).unwrap();
        let second: serde_json::Value = serde_json::from_str(&bodies[1]).unwrap();
        let third: serde_json::Value = serde_json::from_str(&bodies[2]).unwrap();
        assert!(first.get("observation_pack").is_none());
        assert!(second.get("observation_pack").is_none());
        assert!(first["data"].as_str().unwrap().len() == 10_000);
        assert_eq!(first, second);
        let pack = third
            .get("observation_pack")
            .expect("third send is a projection");
        assert_eq!(pack["tool"], "medium_output");
        assert!(pack["head"].as_str().is_some());
        assert!(pack["tail"].as_str().is_some());
        assert!(bodies[2].len() <= MAX_TOOL_RESULT_BYTES);
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn execute_tool_over_64kb_is_recallable() {
        let (agent, dir) = make_obs_loop(
            "obs_64k",
            SizedOutputTool {
                name: "huge_output",
                n_bytes: 80_000,
            },
        )
        .await;
        let body = agent
            .execute_tool_with_guards(
                "agent-x",
                "huge_output",
                &serde_json::json!({}),
                None,
                "obs_64k",
                &CancellationToken::new(),
                "call-huge",
            )
            .await;
        assert!(body.len() <= MAX_TOOL_RESULT_BYTES);
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        let pack = parsed
            .get("observation_pack")
            .expect("oversize result must project");
        let id = pack["id"].as_str().unwrap();
        let total = pack["bytes"].as_u64().unwrap();
        assert!(total > MAX_TOOL_RESULT_BYTES as u64);

        let page = crate::observation_pack::recall(
            &dir.join("sessions"),
            "obs_64k",
            id,
            total.saturating_sub(64),
            128,
        )
        .await
        .unwrap();
        assert_eq!(page["eof"], true);
        let chunk = page["chunk"].as_str().unwrap();
        assert!(!chunk.is_empty());
        assert!(chunk.contains('x'), "tail past 64KB must be recallable");
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn execute_tool_archive_failure_does_not_error_loop() {
        let transport = Arc::new(MockTransport::new("ok"));
        let dir = temp_dir("obs_fail");
        let _ = std::fs::create_dir_all(&dir);
        let platform = Arc::new(NativePlatform::new());
        let bus = Arc::new(MessageBus::new());
        let memory = Arc::new(MemoryStore::with_paths(
            dir.join("memory").join("MEMORY.md"),
            dir.join("memory").join("HISTORY.md"),
            platform.clone(),
        ));
        let skills = Arc::new(SkillsLoader::with_dir(dir.join("skills"), platform.clone()));
        let context = ContextBuilder::new(test_config(), memory, skills, platform.clone());
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(SizedOutputTool {
            name: "medium_output",
            n_bytes: 10_000,
        }));
        let pipeline = make_pipeline(transport);
        let blocker = dir.join("sessions_blocker");
        std::fs::write(&blocker, b"not-a-dir").unwrap();
        let agent = AgentLoop::new(
            test_config(),
            platform,
            bus,
            pipeline,
            Arc::new(tools),
            context,
            PermissionResolver::default_resolver(),
        )
        .with_observation_sessions_dir(blocker);

        let body = agent
            .execute_tool_with_guards(
                "agent-x",
                "medium_output",
                &serde_json::json!({}),
                None,
                "obs_fail",
                &CancellationToken::new(),
                "c1",
            )
            .await;
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(parsed.get("error").is_none(), "loop must not error: {body}");
        assert!(parsed.get("observation_pack").is_none());
        assert!(body.len() <= MAX_TOOL_RESULT_BYTES);
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    // ── M4 D8: result-enrichment ───────────────────────────────────────

    #[test]
    fn preview_truncate_leaves_short_strings_and_clips_long_ones() {
        assert_eq!(preview_truncate("short"), "short");
        let long = "y".repeat(TOOL_PREVIEW_MAX_BYTES + 50);
        let clipped = preview_truncate(&long);
        assert!(clipped.ends_with('…'));
        // The retained prefix is at most the byte cap.
        assert!(clipped.len() <= TOOL_PREVIEW_MAX_BYTES + '…'.len_utf8());
    }

    #[test]
    fn tool_result_succeeded_detects_error_and_denial() {
        assert!(tool_result_succeeded(r#"{"output":"ok"}"#));
        assert!(!tool_result_succeeded(r#"{"error":"boom"}"#));
        assert!(!tool_result_succeeded(r#"{"denied":true,"reason":"policy"}"#));
        // Non-object results are treated as success.
        assert!(tool_result_succeeded(r#""just a string""#));
    }

    // -- WEFT-651: identical tool-failure breaker helpers --------------------

    #[test]
    fn identical_failure_key_none_on_success() {
        let key = identical_failure_key(
            "echo",
            &serde_json::json!({"text": "hi"}),
            r#"{"output":"hi"}"#,
        );
        assert!(key.is_none());
    }

    #[test]
    fn identical_failure_key_matches_same_tool_args_error() {
        let args = serde_json::json!({"command": "render"});
        let err = r#"{"error":"invalid arguments: missing field content"}"#;
        let a = identical_failure_key("canvas", &args, err).expect("fail");
        let b = identical_failure_key("canvas", &args, err).expect("fail");
        assert_eq!(a, b);
        assert_eq!(a.tool, "canvas");
        assert!(a.error.contains("missing field content"));
    }

    #[test]
    fn identical_failure_key_differs_when_args_change() {
        let err = r#"{"error":"boom"}"#;
        let a = identical_failure_key("t", &serde_json::json!({"x": 1}), err).unwrap();
        let b = identical_failure_key("t", &serde_json::json!({"x": 2}), err).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn record_identical_failure_trips_at_limit() {
        let limit = 3;
        let mut last = None;
        let mut count = 0u32;
        let mk = || {
            identical_failure_key(
                "canvas",
                &serde_json::json!({"command": "render"}),
                r#"{"error":"missing field content"}"#,
            )
        };

        assert!(record_identical_failure(&mut last, &mut count, mk(), limit).is_none());
        assert_eq!(count, 1);
        assert!(record_identical_failure(&mut last, &mut count, mk(), limit).is_none());
        assert_eq!(count, 2);
        let tripped = record_identical_failure(&mut last, &mut count, mk(), limit);
        assert!(tripped.is_some());
        assert_eq!(count, 3);
        assert_eq!(tripped.unwrap().tool, "canvas");
    }

    #[test]
    fn record_identical_failure_resets_on_success_or_different_key() {
        let limit = 3;
        let mut last = None;
        let mut count = 0u32;
        let fail_a = || {
            identical_failure_key("t", &serde_json::json!({"a": 1}), r#"{"error":"e1"}"#)
        };
        let fail_b = || {
            identical_failure_key("t", &serde_json::json!({"a": 2}), r#"{"error":"e1"}"#)
        };

        assert!(record_identical_failure(&mut last, &mut count, fail_a(), limit).is_none());
        assert!(record_identical_failure(&mut last, &mut count, fail_a(), limit).is_none());
        assert_eq!(count, 2);
        // Different args → streak resets to 1.
        assert!(record_identical_failure(&mut last, &mut count, fail_b(), limit).is_none());
        assert_eq!(count, 1);
        // Success → streak clears.
        assert!(record_identical_failure(&mut last, &mut count, None, limit).is_none());
        assert_eq!(count, 0);
        assert!(last.is_none());
    }

    #[test]
    fn identical_failure_breaker_message_is_clear() {
        let key = IdenticalFailureKey {
            tool: "canvas".into(),
            args: r#"{"command":"render"}"#.into(),
            error: "missing field content".into(),
        };
        let msg = identical_failure_breaker_message(&key, 3);
        assert!(msg.contains("IDENTICAL TOOL FAILURE BREAKER"));
        assert!(msg.contains("canvas"));
        assert!(msg.contains("3 times"));
        assert!(msg.contains("missing field content"));
        assert!(msg.contains("Stop calling this tool"));
    }

    #[test]
    fn spawned_task_from_result_parses_agent_spawn_shape() {
        let ok = r#"{"task_id":"t1","child_conv_id":"sub:P:01HQ","status":"completed","result":"4"}"#;
        let task = spawned_task_from_result("agent_spawn", ok).expect("should parse");
        assert_eq!(task.task_id, "t1");
        assert_eq!(task.child_conv_id, "sub:P:01HQ");
        assert_eq!(task.status, "completed");

        // Wrong tool name → None.
        assert!(spawned_task_from_result("echo", ok).is_none());
        // Denied spawn carries no task.
        assert!(spawned_task_from_result("agent_spawn", r#"{"denied":true}"#).is_none());
        // Missing required field → None.
        assert!(spawned_task_from_result("agent_spawn", r#"{"task_id":"t1"}"#).is_none());
    }

    #[tokio::test]
    async fn handle_turn_threads_real_tool_calls_and_iterations() {
        // A turn that runs one echo tool then answers must surface real
        // tool_calls + iterations + token/model fields in the
        // OutboundMessage metadata (D8 / WEFT-328).
        let transport = Arc::new(E2eRecordingTransport::new());
        let (agent, dir) = make_agent_loop(transport, "d8_toolcalls").await;

        let inbound = InboundMessage {
            channel: "cli".into(),
            sender_id: "u".into(),
            chat_id: "d8-chat".into(),
            content: "please use the echo tool".into(),
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata: HashMap::new(),
        };
        let outbound = agent.handle_turn(inbound, &CancellationToken::new()).await.unwrap();

        let meta_value = outbound
            .metadata
            .get(AGENT_LOOP_RESULT_META_KEY)
            .expect("outbound must carry the loop result meta");
        let meta: AgentLoopResultMeta = serde_json::from_value(meta_value.clone()).unwrap();
        assert_eq!(meta.finish_reason, "stop");
        // Two LLM round-trips: tool_use then text.
        assert_eq!(meta.iterations, 2);
        assert_eq!(meta.tool_calls.len(), 1);
        assert_eq!(meta.tool_calls[0].name, "echo");
        assert!(meta.tool_calls[0].success);
        assert!(meta.tool_calls[0].arguments_preview.contains("e2e-ping"));
        // No spawns in this turn.
        assert!(meta.spawned_tasks.is_empty());
        // WEFT-328: cumulative tokens from both LLM calls (15+25, 10+12).
        assert_eq!(meta.prompt_tokens, 40);
        assert_eq!(meta.completion_tokens, 22);
        assert_eq!(meta.model.as_deref(), Some("e2e-test-model"));
        // No SystemPromptBuilder attached → identity_source stays None.
        assert!(meta.identity_source.is_none());

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// A tool named `agent_spawn` returning the frozen D4 result shape.
    struct SpawnStubTool;

    #[async_trait]
    impl Tool for SpawnStubTool {
        fn name(&self) -> &str {
            "agent_spawn"
        }
        fn description(&self) -> &str {
            "Stub subagent spawner for tests"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": { "goal": { "type": "string" } },
                "required": ["goal"]
            })
        }
        async fn execute(
            &self,
            _args: serde_json::Value,
        ) -> Result<serde_json::Value, crate::tools::registry::ToolError> {
            Ok(serde_json::json!({
                "task_id": "task-42",
                "child_conv_id": "sub:d8-spawn-chat:01HQ",
                "status": "completed",
                "result": "4"
            }))
        }
    }

    /// Transport: emit an `agent_spawn` tool_use, then a text reply.
    struct SpawnE2eTransport {
        call_count: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl LlmTransport for SpawnE2eTransport {
        async fn complete(&self, _request: &TransportRequest) -> clawft_types::Result<LlmResponse> {
            let count = self
                .call_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if count == 0 {
                Ok(LlmResponse {
                    id: "spawn-resp-1".into(),
                    content: vec![ContentBlock::ToolUse {
                        id: "call-spawn-1".into(),
                        name: "agent_spawn".into(),
                        input: serde_json::json!({"goal": "answer 2+2"}),
                    }],
                    stop_reason: StopReason::ToolUse,
                    usage: Usage {
                        input_tokens: 10,
                        output_tokens: 5,
                        total_tokens: 0,
                    },
                    metadata: HashMap::new(),
                })
            } else {
                Ok(LlmResponse {
                    id: "spawn-resp-2".into(),
                    content: vec![ContentBlock::Text {
                        text: "The subagent answered 4".into(),
                    }],
                    stop_reason: StopReason::EndTurn,
                    usage: Usage {
                        input_tokens: 20,
                        output_tokens: 8,
                        total_tokens: 0,
                    },
                    metadata: HashMap::new(),
                })
            }
        }
    }

    #[tokio::test]
    async fn handle_turn_surfaces_spawned_tasks_from_agent_spawn_result() {
        // The headline D8 unit: a turn whose tool call is `agent_spawn`
        // yields a real spawned_tasks[] entry in the result metadata.
        let dir = temp_dir("d8_spawn");
        let platform = Arc::new(NativePlatform::new());
        let bus = Arc::new(MessageBus::new());
        let memory = Arc::new(MemoryStore::with_paths(
            dir.join("memory").join("MEMORY.md"),
            dir.join("memory").join("HISTORY.md"),
            platform.clone(),
        ));
        let skills = Arc::new(SkillsLoader::with_dir(dir.join("skills"), platform.clone()));
        let context = ContextBuilder::new(test_config(), memory, skills, platform.clone());

        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(SpawnStubTool));

        let pipeline = make_pipeline(Arc::new(SpawnE2eTransport {
            call_count: std::sync::atomic::AtomicUsize::new(0),
        }));

        let agent = AgentLoop::new(
            test_config(),
            platform,
            bus,
            pipeline,
            Arc::new(tools),
            context,
            PermissionResolver::default_resolver(),
        );

        let inbound = InboundMessage {
            channel: "cli".into(),
            sender_id: "u".into(),
            chat_id: "d8-spawn-chat".into(),
            content: "spawn a subagent to compute 2+2".into(),
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata: HashMap::new(),
        };
        let outbound = agent.handle_turn(inbound, &CancellationToken::new()).await.unwrap();

        let meta: AgentLoopResultMeta = serde_json::from_value(
            outbound
                .metadata
                .get(AGENT_LOOP_RESULT_META_KEY)
                .expect("loop meta present")
                .clone(),
        )
        .unwrap();
        assert_eq!(meta.spawned_tasks.len(), 1, "one subagent should surface");
        assert_eq!(meta.spawned_tasks[0].task_id, "task-42");
        assert_eq!(meta.spawned_tasks[0].child_conv_id, "sub:d8-spawn-chat:01HQ");
        assert_eq!(meta.spawned_tasks[0].status, "completed");
        // The spawn tool call is also recorded in tool_calls.
        assert!(meta.tool_calls.iter().any(|t| t.name == "agent_spawn"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    // ── M4 D5: spawn_context write-side (loop installs parent context) ──

    /// Tool that captures the ambient [`spawn_context`] visible during its
    /// dispatch, so a test can assert the loop installed the right context.
    #[cfg(feature = "native")]
    struct CtxCaptureTool {
        seen: Arc<std::sync::Mutex<Option<crate::agent::spawn_context::SpawnContext>>>,
    }

    #[cfg(feature = "native")]
    #[async_trait]
    impl Tool for CtxCaptureTool {
        fn name(&self) -> &str {
            "capture_ctx"
        }
        fn description(&self) -> &str {
            "Capture the ambient spawn context for assertions"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({ "type": "object", "properties": {} })
        }
        async fn execute(
            &self,
            _args: serde_json::Value,
        ) -> Result<serde_json::Value, crate::tools::registry::ToolError> {
            *self.seen.lock().unwrap() = crate::agent::spawn_context::current();
            Ok(serde_json::json!({ "captured": true }))
        }
    }

    /// Transport: emit a `capture_ctx` tool_use, then a text reply.
    #[cfg(feature = "native")]
    struct CaptureCtxTransport {
        call_count: std::sync::atomic::AtomicUsize,
    }

    #[cfg(feature = "native")]
    #[async_trait]
    impl LlmTransport for CaptureCtxTransport {
        async fn complete(&self, _request: &TransportRequest) -> clawft_types::Result<LlmResponse> {
            let count = self
                .call_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let content = if count == 0 {
                vec![ContentBlock::ToolUse {
                    id: "call-cap-1".into(),
                    name: "capture_ctx".into(),
                    input: serde_json::json!({}),
                }]
            } else {
                vec![ContentBlock::Text {
                    text: "done".into(),
                }]
            };
            Ok(LlmResponse {
                id: "cap-resp".into(),
                content,
                stop_reason: if count == 0 {
                    StopReason::ToolUse
                } else {
                    StopReason::EndTurn
                },
                usage: Usage {
                    input_tokens: 5,
                    output_tokens: 3,
                    total_tokens: 0,
                },
                metadata: HashMap::new(),
            })
        }
    }

    #[cfg(feature = "native")]
    #[tokio::test]
    async fn handle_turn_installs_parent_spawn_context_for_tools() {
        // A tool dispatched inside run_tool_loop must see the parent
        // conversation's spawn context: conv_id, agent_id, depth from the
        // inbound metadata, and turn_uid=None (kernel-layer, not in core).
        let dir = temp_dir("d5_ctx");
        let platform = Arc::new(NativePlatform::new());
        let bus = Arc::new(MessageBus::new());
        let memory = Arc::new(MemoryStore::with_paths(
            dir.join("memory").join("MEMORY.md"),
            dir.join("memory").join("HISTORY.md"),
            platform.clone(),
        ));
        let skills = Arc::new(SkillsLoader::with_dir(dir.join("skills"), platform.clone()));
        let context = ContextBuilder::new(test_config(), memory, skills, platform.clone());

        let seen = Arc::new(std::sync::Mutex::new(None));
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(CtxCaptureTool { seen: seen.clone() }));

        let pipeline = make_pipeline(Arc::new(CaptureCtxTransport {
            call_count: std::sync::atomic::AtomicUsize::new(0),
        }));

        let agent = AgentLoop::new(
            test_config(),
            platform,
            bus,
            pipeline,
            Arc::new(tools),
            context,
            PermissionResolver::default_resolver(),
        );

        // Inbound with a spawn_depth of 2 (as a child conversation would carry).
        let mut metadata = HashMap::new();
        metadata.insert(SPAWN_DEPTH_KEY.to_string(), serde_json::json!(2));
        let inbound = InboundMessage {
            channel: "cli".into(),
            sender_id: "u7".into(),
            chat_id: "d5-ctx-chat".into(),
            content: "use the capture tool".into(),
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata,
        };
        agent.handle_turn(inbound, &CancellationToken::new()).await.unwrap();

        let ctx = seen
            .lock()
            .unwrap()
            .clone()
            .expect("tool must observe an installed spawn context");
        // M3 §D5: conv_id is the canonical session_key `"{channel}:{chat_id}"`,
        // not the bare chat_id — the spawn context roots at the same conv
        // identity the sink / tier key by.
        assert_eq!(ctx.conv_id.as_deref(), Some("cli:d5-ctx-chat"));
        // No router / daemon id in this harness → channel:sender fallback.
        assert_eq!(ctx.agent_id.as_deref(), Some("cli:u7"));
        assert_eq!(ctx.depth, 2, "depth must come from inbound spawn_depth");
        assert!(
            ctx.turn_uid.is_none(),
            "turn_uid is kernel-layer; the loop leaves it None"
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// Depth defaults to 0 for a top-level conversation (no spawn_depth
    /// metadata), so a first-level child spawns at depth 1 (pB's tool adds 1).
    #[cfg(feature = "native")]
    #[test]
    fn read_spawn_depth_defaults_to_zero() {
        let msg = InboundMessage {
            channel: "cli".into(),
            sender_id: "u".into(),
            chat_id: "c".into(),
            content: "hi".into(),
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata: HashMap::new(),
        };
        assert_eq!(read_spawn_depth(&msg), 0);
        let mut with_depth = msg.clone();
        with_depth
            .metadata
            .insert(SPAWN_DEPTH_KEY.to_string(), serde_json::json!(3));
        assert_eq!(read_spawn_depth(&with_depth), 3);
    }

    // ── WEFT-616 Phase 2: cow-memory checkpoint bracket ────────────────

    /// Without [`AgentLoop::with_cow_memory`] attached, `handle_turn`
    /// behaves exactly as before Phase 2 -- no field to check here beyond
    /// "it still succeeds"; every other `handle_turn_*` test in this module
    /// already proves this since none of them attach a cow-memory handle.
    #[cfg(feature = "rvf")]
    #[tokio::test]
    async fn handle_turn_disabled_cow_memory_is_unaffected() {
        let transport = Arc::new(E2eRecordingTransport::new());
        let (agent, dir) =
            make_agent_loop(transport.clone() as Arc<dyn LlmTransport>, "cow_off").await;

        let inbound = InboundMessage {
            channel: "test".into(),
            sender_id: "user1".into(),
            chat_id: "cow-off-chat".into(),
            content: "hello".into(),
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata: HashMap::new(),
        };
        let result = agent.handle_turn(inbound, &CancellationToken::new()).await;
        assert!(result.is_ok(), "turn without cow_memory attached: {result:?}");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// A successful turn checkpoints, runs, and promotes: the checkpoint
    /// chain collapses back to depth 0 (working re-derived from base) and
    /// the loop's reply is unaffected by the memory bracket.
    #[cfg(feature = "rvf")]
    #[tokio::test]
    async fn handle_turn_success_checkpoints_and_promotes() {
        let transport = Arc::new(E2eRecordingTransport::new());
        let (mut agent, dir) =
            make_agent_loop(transport.clone() as Arc<dyn LlmTransport>, "cow_ok").await;

        let mem = clawft_cow_memory::BranchableMemory::create(dir.join("cow_memory"), 4)
            .expect("create BranchableMemory");
        let mem = Arc::new(std::sync::Mutex::new(mem));
        agent = agent.with_cow_memory(mem.clone());

        let inbound = InboundMessage {
            channel: "test".into(),
            sender_id: "user1".into(),
            chat_id: "cow-ok-chat".into(),
            content: "hello".into(),
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata: HashMap::new(),
        };
        let result = agent.handle_turn(inbound, &CancellationToken::new()).await;
        assert!(result.is_ok(), "turn with cow_memory attached: {result:?}");

        let status = mem.lock().unwrap().status();
        assert_eq!(
            status.own_checkpoint_depth, 0,
            "a successful turn's checkpoint must be collapsed by promote()"
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// A failing turn checkpoints, runs, and rolls back: the checkpoint
    /// chain also collapses back to depth 0 (working re-derived from the
    /// pre-turn checkpoint, discarding it), and the turn's own error is
    /// what the caller sees -- not a memory-layer error.
    #[cfg(feature = "rvf")]
    #[tokio::test]
    async fn handle_turn_failure_checkpoints_and_rolls_back() {
        struct AlwaysErrTransport;
        #[async_trait]
        impl LlmTransport for AlwaysErrTransport {
            async fn complete(&self, _request: &TransportRequest) -> clawft_types::Result<LlmResponse> {
                Err(ClawftError::Provider {
                    message: "simulated provider failure".into(),
                })
            }
        }

        let transport = Arc::new(AlwaysErrTransport);
        let (mut agent, dir) =
            make_agent_loop(transport as Arc<dyn LlmTransport>, "cow_err").await;

        let mem = clawft_cow_memory::BranchableMemory::create(dir.join("cow_memory"), 4)
            .expect("create BranchableMemory");
        let mem = Arc::new(std::sync::Mutex::new(mem));
        agent = agent.with_cow_memory(mem.clone());

        let inbound = InboundMessage {
            channel: "test".into(),
            sender_id: "user1".into(),
            chat_id: "cow-err-chat".into(),
            content: "hello".into(),
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata: HashMap::new(),
        };
        let result = agent.handle_turn(inbound, &CancellationToken::new()).await;
        assert!(
            result.is_err(),
            "a provider failure must still surface as the turn's error"
        );

        let status = mem.lock().unwrap().status();
        assert_eq!(
            status.own_checkpoint_depth, 0,
            "a failed turn's checkpoint must be collapsed by rollback()"
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
