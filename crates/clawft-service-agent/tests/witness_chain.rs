//! Integration: WitnessRecord assertions in the chat path (Phase F3).
//!
//! Drives an `AgentService::dispatch` end-to-end against:
//!   - a stub LlmTransport that returns one tool-use turn followed by
//!     a final-text turn,
//!   - a real `clawft-kernel::GovernanceGate` (NOT a stub) wrapped in
//!     `KernelEffectGate`,
//!   - a `ChainManager` capturing every witness entry the gate emits.
//!
//! What this test proves:
//!
//!   1. Every `gate.check` invocation produces exactly one `WitnessEntry`
//!      on the chain (kernel's `governance.{permit,defer,deny}` event
//!      kind — pinned in `crates/clawft-kernel/src/gate.rs:418-431`).
//!   2. The witness chain verifies (`ChainManager::verify_witness`
//!      returns `Ok(_)`), so the SHAKE-256 entries chain by hash.
//!   3. Permit decisions and Deny decisions both produce witnesses
//!      (Permit not just silent).
//!   4. The witness payload carries the `agent_id`, `action`
//!      (`"tool.{name}"`), and the 5D effect vector the gate received.
//!   5. Multi-tool turns produce one witness per tool call, all
//!      chain-linked.
//!
//! What is **not** covered: the substrate-sink writes (covered by C3's
//! tests), the kernel-gate Permit/Defer/Deny mapping in isolation
//! (covered by D2's tests), and the dispatch lock/cancel (covered by
//! C1's tests). F3 only proves the audit chain is exercised
//! end-to-end through the production wiring, not just the kernel-side
//! gate in isolation.
//!
//! The test mirrors `loop_core::tests::make_agent_loop` (~30 lines
//! reproduced inline; F3 plan note "don't make `make_agent_loop`
//! public — copy the helper") so the integration is hermetic — no
//! network, no daemon process, no kernel boot, no
//! `clawft-service-llm::LlmClient`, no writes to the user's
//! `~/.clawft/workspace`. The kernel's `GovernanceGate` and
//! `ChainManager` are both real instances; only the LLM transport
//! and pipeline stages are stubbed.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use async_trait::async_trait;
use clawft_core::agent::context::ContextBuilder;
use clawft_core::agent::loop_core::AgentLoop;
use clawft_core::agent::memory::MemoryStore;
use clawft_core::agent::skills::SkillsLoader;
use clawft_core::bus::MessageBus;
use clawft_core::pipeline::permissions::PermissionResolver;
use clawft_core::pipeline::traits::{
    AssembledContext, ChatRequest, ContextAssembler, LearningBackend, LearningSignal, LlmTransport,
    ModelRouter, Pipeline, PipelineRegistry, QualityScore, QualityScorer, ResponseOutcome,
    RoutingDecision, TaskClassifier, TaskProfile, TaskType, Trajectory, TransportRequest,
};
use clawft_core::tools::registry::{Tool, ToolError, ToolRegistry};
use clawft_kernel::chain::ChainManager;
use clawft_kernel::gate::{GateBackend, GovernanceGate};
use clawft_kernel::governance::{GovernanceBranch, GovernanceRule, RuleSeverity};
use clawft_platform::NativePlatform;
use clawft_service_agent::{AgentChatMessage, AgentChatParams, AgentService, KernelEffectGate};
use clawft_types::config::{AgentDefaults, AgentsConfig};
use clawft_types::provider::{ContentBlock, LlmResponse, StopReason, Usage};

// ── Stub pipeline stages ────────────────────────────────────────────
//
// Mirrors the inline mocks in `loop_core::tests` (production keeps
// those private; ~30 lines reproduced here per the F3 plan note about
// not leaking test infrastructure into the public API).

struct StubClassifier;
impl TaskClassifier for StubClassifier {
    fn classify(&self, _request: &ChatRequest) -> TaskProfile {
        TaskProfile {
            task_type: TaskType::Chat,
            complexity: 0.3,
            keywords: vec![],
        }
    }
}

struct StubRouter;
#[async_trait]
impl ModelRouter for StubRouter {
    async fn route(&self, _request: &ChatRequest, _profile: &TaskProfile) -> RoutingDecision {
        RoutingDecision {
            provider: "test".into(),
            model: "test-model".into(),
            reason: "stub".into(),
            ..Default::default()
        }
    }
    fn update(&self, _d: &RoutingDecision, _o: &ResponseOutcome) {}
}

struct StubAssembler;
#[async_trait]
impl ContextAssembler for StubAssembler {
    async fn assemble(&self, request: &ChatRequest, _profile: &TaskProfile) -> AssembledContext {
        AssembledContext {
            messages: request.messages.clone(),
            token_estimate: 100,
            truncated: false,
        }
    }
}

struct StubScorer;
impl QualityScorer for StubScorer {
    fn score(&self, _req: &ChatRequest, _resp: &LlmResponse) -> QualityScore {
        QualityScore {
            overall: 1.0,
            relevance: 1.0,
            coherence: 1.0,
        }
    }
}

struct StubLearner;
impl LearningBackend for StubLearner {
    fn record(&self, _t: &Trajectory) {}
    fn adapt(&self, _s: &LearningSignal) {}
}

// ── Stub LLM transport ──────────────────────────────────────────────

/// Returns a single tool-use call (configurable name + input) on the
/// first invocation, then plain text on subsequent invocations.
struct OneToolThenTextTransport {
    tool_name: String,
    tool_input: serde_json::Value,
    final_text: String,
    call_count: AtomicUsize,
}

impl OneToolThenTextTransport {
    fn new(tool_name: &str, tool_input: serde_json::Value, final_text: &str) -> Self {
        Self {
            tool_name: tool_name.into(),
            tool_input,
            final_text: final_text.into(),
            call_count: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl LlmTransport for OneToolThenTextTransport {
    async fn complete(&self, _request: &TransportRequest) -> clawft_types::Result<LlmResponse> {
        let n = self.call_count.fetch_add(1, Ordering::Relaxed);
        if n == 0 {
            Ok(LlmResponse {
                id: "stub-tool-resp".into(),
                content: vec![ContentBlock::ToolUse {
                    id: "call-1".into(),
                    name: self.tool_name.clone(),
                    input: self.tool_input.clone(),
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
                id: "stub-final-resp".into(),
                content: vec![ContentBlock::Text {
                    text: self.final_text.clone(),
                }],
                stop_reason: StopReason::EndTurn,
                usage: Usage {
                    input_tokens: 12,
                    output_tokens: 6,
                    total_tokens: 0,
                },
                metadata: HashMap::new(),
            })
        }
    }
}

/// Returns two tool-use calls in one assistant turn, then plain text.
struct TwoToolsThenTextTransport {
    call_count: AtomicUsize,
}

impl TwoToolsThenTextTransport {
    fn new() -> Self {
        Self {
            call_count: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl LlmTransport for TwoToolsThenTextTransport {
    async fn complete(&self, _request: &TransportRequest) -> clawft_types::Result<LlmResponse> {
        let n = self.call_count.fetch_add(1, Ordering::Relaxed);
        if n == 0 {
            Ok(LlmResponse {
                id: "stub-multi-resp".into(),
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
                id: "stub-multi-final".into(),
                content: vec![ContentBlock::Text {
                    text: "both processed".into(),
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

// ── Stub tools ──────────────────────────────────────────────────────

/// Echo tool — registered under the name `"echo"`. The `effect_for_tool`
/// table treats `"echo"` as the all-zero vector (unknown tool), so under
/// `GovernanceGate::open()` every call Permits.
struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "Echo back the text"
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "text": { "type": "string" } },
            "required": ["text"],
        })
    }
    async fn execute(&self, args: serde_json::Value) -> Result<serde_json::Value, ToolError> {
        let t = args
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("(none)");
        Ok(serde_json::json!({ "output": t }))
    }
}

/// Read-file tool — registered under the name `"read_file"` so
/// [`effect_for_tool`](clawft_core::agent::effects::effect_for_tool)
/// returns `privacy: 0.1` (magnitude 0.1). With
/// `GovernanceGate::new(0.0, false)` + a Blocking rule, that exceeds
/// the threshold and yields a `Deny` + `governance.deny` chain entry.
/// The execute impl returns a stub success — irrelevant because the
/// gate denies before the tool ever runs.
struct ReadFileTool;

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }
    fn description(&self) -> &str {
        "Read a file from the workspace (stub)"
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"],
        })
    }
    async fn execute(&self, _args: serde_json::Value) -> Result<serde_json::Value, ToolError> {
        Ok(serde_json::json!({ "content": "stub" }))
    }
}

// ── Test helpers ────────────────────────────────────────────────────

/// Per-test-process counter used to derive unique temp dirs for
/// `SessionManager` / `MemoryStore` / `SkillsLoader`. Mirrors
/// `loop_core::tests::TEST_COUNTER`.
static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_dir(prefix: &str) -> PathBuf {
    let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    std::env::temp_dir().join(format!("clawft_f3_witness_{prefix}_{pid}_{id}"))
}

/// Test-only `AgentsConfig` mirroring `loop_core::tests::test_config`.
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

/// Build a `Pipeline` whose only "real" stage is the supplied
/// transport — every other stage is a no-op stub.
fn make_pipeline(transport: Arc<dyn LlmTransport>) -> PipelineRegistry {
    PipelineRegistry::new(Pipeline {
        classifier: Arc::new(StubClassifier),
        router: Arc::new(StubRouter),
        assembler: Arc::new(StubAssembler),
        transport,
        scorer: Arc::new(StubScorer),
        learner: Arc::new(StubLearner),
    })
}

/// Build a hermetic agent loop wired with the given transport, gate,
/// and registered tools.
///
/// Mirrors `clawft_core::agent::loop_core::tests::make_agent_loop`
/// (~30 lines reproduced per the F3 plan note). Every filesystem
/// dependency points at a unique temp dir so the test does not touch
/// the user's `~/.clawft/workspace`; every LLM dependency is the
/// stub above so no network is required.
fn make_loop(
    prefix: &str,
    transport: Arc<dyn LlmTransport>,
    gate: Arc<dyn clawft_core::agent::gate::EffectGate>,
    tools: Vec<Arc<dyn Tool>>,
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

    let mut tool_registry = ToolRegistry::new();
    for t in tools {
        tool_registry.register(t);
    }

    let pipeline = make_pipeline(transport);

    let agent = AgentLoop::new(
        test_config(),
        platform,
        bus,
        pipeline,
        Arc::new(tool_registry),
        context,
        PermissionResolver::default_resolver(),
    )
    .with_gate(gate);

    (agent, dir)
}

/// Construct an `AgentChatParams` with the given conv_id and a single
/// user message. Uses fresh ULID-shaped conv_ids so concurrent test
/// runs don't share session files in `~/.clawft/sessions/`.
fn params_for(conv_id: &str, content: &str) -> AgentChatParams {
    AgentChatParams {
        messages: vec![AgentChatMessage {
            role: "user".into(),
            content: content.into(),
                    audio: None,
        }],
        temperature: None,
        max_tokens: None,
        conv_id: conv_id.into(),
        metadata: None,
        caller_id: None,
    }
}

// ── Tests ───────────────────────────────────────────────────────────

#[tokio::test]
async fn permit_dispatch_appends_witness_entry() {
    // GovernanceGate::open() permits everything. `echo` maps to the
    // zero EffectVector. Expect exactly one `governance.permit` event
    // appended to the chain after one tool dispatch.
    let chain = Arc::new(ChainManager::new(0, 1000));
    let baseline = chain.len();

    let governance: Arc<dyn GateBackend> =
        Arc::new(GovernanceGate::open().with_chain(Arc::clone(&chain)));
    let kernel_gate = Arc::new(KernelEffectGate::new(governance));

    let transport: Arc<dyn LlmTransport> = Arc::new(OneToolThenTextTransport::new(
        "echo",
        serde_json::json!({"text": "hello"}),
        "ok",
    ));

    let (agent, dir) = make_loop("permit", transport, kernel_gate, vec![Arc::new(EchoTool)]);
    let svc = AgentService::new(Arc::new(agent));

    let conv_id = format!("f3-permit-{}", ulid::Ulid::new());
    let result = svc
        .dispatch(params_for(&conv_id, "use echo"))
        .await
        .expect("dispatch must succeed under an open gate");

    // The stub LLM returned "ok" once the tool came back.
    assert_eq!(result.assistant_text, "ok");

    // Exactly one new chain event since the dispatch — the
    // `governance.permit` entry for the single `echo` tool call.
    assert_eq!(
        chain.len(),
        baseline + 1,
        "permit dispatch must append exactly one chain event"
    );

    let tail = chain.tail(1);
    let last = tail.last().expect("tail must contain the new event");
    assert_eq!(last.source, "governance");
    assert_eq!(last.kind, "governance.permit");

    let payload = last.payload.as_ref().expect("permit event has a payload");
    assert_eq!(payload["action"], serde_json::json!("tool.echo"));
    // Agent id falls back to "{channel}:{sender_id}" because no
    // daemon_agent_id is set on the loop. The service hard-codes
    // both: channel=`agent.chat`, sender=`panel`.
    assert_eq!(payload["agent_id"], serde_json::json!("agent.chat:panel"));
    // Effect vector is the zero baseline for unknown tools.
    let effect = &payload["effect"];
    for key in ["risk", "fairness", "privacy", "novelty", "security"] {
        assert!(
            effect.get(key).is_some(),
            "permit payload effect missing `{key}`"
        );
        assert_eq!(
            effect[key].as_f64(),
            Some(0.0),
            "echo's effect[{key}] must be 0.0"
        );
    }
    // threshold_exceeded must be false for an open gate (threshold=1.0).
    assert_eq!(payload["threshold_exceeded"], serde_json::json!(false));

    // The witness chain must verify — every emitted event also adds
    // a SHAKE-256 witness entry, hash-linked.
    let verified = chain.verify_witness().expect("witness chain verifies");
    assert_eq!(
        verified,
        chain.witness_count(),
        "every chain event must produce one verified witness entry"
    );

    let _ = tokio::fs::remove_dir_all(&dir).await;
}

#[tokio::test]
async fn deny_dispatch_appends_deny_witness() {
    // `read_file` → privacy=0.1 (magnitude 0.1). `risk_threshold=0.0`
    // → 0.1 > 0.0 → threshold_exceeded=true. With a Blocking rule and
    // `human_approval_required=false`, the engine returns Deny.
    let chain = Arc::new(ChainManager::new(0, 1000));
    let baseline = chain.len();

    let governance: Arc<dyn GateBackend> = Arc::new(
        GovernanceGate::new(0.0, false)
            .with_chain(Arc::clone(&chain))
            .add_rule(GovernanceRule {
                id: "f3-deny-test".into(),
                description: "block any non-zero effect for the F3 test".into(),
                branch: GovernanceBranch::Judicial,
                severity: RuleSeverity::Blocking,
                active: true,
                reference_url: None,
                sop_category: None,
                rule_type: Default::default(),
                action_selector: None,
                tool_selector: None,
                force_on_match: false,
                principal_selector: None,
            }),
    );
    let kernel_gate = Arc::new(KernelEffectGate::new(governance));

    let transport: Arc<dyn LlmTransport> = Arc::new(OneToolThenTextTransport::new(
        "read_file",
        serde_json::json!({"path": "/tmp/x"}),
        "denied-final",
    ));

    let (agent, dir) = make_loop("deny", transport, kernel_gate, vec![Arc::new(ReadFileTool)]);
    let svc = AgentService::new(Arc::new(agent));

    let conv_id = format!("f3-deny-{}", ulid::Ulid::new());
    let result = svc
        .dispatch(params_for(&conv_id, "read it"))
        .await
        .expect("dispatch must complete — deny surfaces as a tool result, not a turn error");

    // Loop runs one tool turn (denied) then the LLM returns text.
    assert_eq!(result.assistant_text, "denied-final");

    assert_eq!(
        chain.len(),
        baseline + 1,
        "deny dispatch must append exactly one chain event"
    );

    let tail = chain.tail(1);
    let last = tail.last().expect("tail must contain the deny event");
    assert_eq!(last.source, "governance");
    assert_eq!(
        last.kind, "governance.deny",
        "deny path emits the kernel's `governance.deny` event kind"
    );

    let payload = last.payload.as_ref().expect("deny event has a payload");
    assert_eq!(payload["action"], serde_json::json!("tool.read_file"));
    assert_eq!(payload["threshold_exceeded"], serde_json::json!(true));
    assert!(
        payload["reason"].as_str().is_some(),
        "kernel deny payload includes a `reason` string"
    );
    // The `read_file` baseline EffectVector exposes privacy=0.1.
    assert_eq!(payload["effect"]["privacy"].as_f64(), Some(0.1));

    // Witness chain still verifies after a deny.
    let verified = chain.verify_witness().expect("witness chain verifies");
    assert_eq!(verified, chain.witness_count());

    let _ = tokio::fs::remove_dir_all(&dir).await;
}

#[tokio::test]
async fn multi_tool_call_emits_one_witness_per_tool() {
    // The stub returns two `echo` tool-use blocks in a single
    // assistant turn. The loop iterates both before re-prompting the
    // LLM. Expect two `governance.permit` events appended.
    let chain = Arc::new(ChainManager::new(0, 1000));
    let baseline = chain.len();

    let governance: Arc<dyn GateBackend> =
        Arc::new(GovernanceGate::open().with_chain(Arc::clone(&chain)));
    let kernel_gate = Arc::new(KernelEffectGate::new(governance));

    let transport: Arc<dyn LlmTransport> = Arc::new(TwoToolsThenTextTransport::new());

    let (agent, dir) = make_loop("multi", transport, kernel_gate, vec![Arc::new(EchoTool)]);
    let svc = AgentService::new(Arc::new(agent));

    let conv_id = format!("f3-multi-{}", ulid::Ulid::new());
    let result = svc
        .dispatch(params_for(&conv_id, "fan out"))
        .await
        .expect("dispatch must succeed under an open gate");

    assert_eq!(result.assistant_text, "both processed");

    // Exactly two new chain events — one per tool call.
    assert_eq!(
        chain.len(),
        baseline + 2,
        "two tool calls must produce two chain events (one per gate.check)"
    );

    let tail = chain.tail(2);
    assert_eq!(tail.len(), 2);
    for event in &tail {
        assert_eq!(event.source, "governance");
        assert_eq!(event.kind, "governance.permit");
        assert_eq!(
            event.payload.as_ref().unwrap()["action"],
            serde_json::json!("tool.echo")
        );
    }

    // Hash-linkage: each event's `prev_hash` matches the prior event's
    // `hash`. Walk the full tail and verify the chain locally.
    let all = chain.tail(0);
    for window in all.windows(2) {
        let prev = &window[0];
        let curr = &window[1];
        assert_eq!(
            curr.prev_hash, prev.hash,
            "chain link: event at seq {} must reference seq {}'s hash",
            curr.sequence, prev.sequence
        );
    }

    // And the cryptographic witness chain still verifies as a whole.
    let verified = chain.verify_witness().expect("witness chain verifies");
    assert_eq!(verified, chain.witness_count());

    let _ = tokio::fs::remove_dir_all(&dir).await;
}
