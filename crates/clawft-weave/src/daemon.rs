//! Kernel daemon — persistent kernel process with local RPC transport.
//!
//! The daemon boots a [`Kernel`], then listens for JSON-RPC requests on
//! the platform-local transport (WEFT-559):
//!
//! - **Unix**: Unix domain socket (`kernel.sock` under the runtime dir)
//! - **Windows**: named pipe `\\.\pipe\clawft-kernel-<hash>` derived from
//!   the same logical path via [`clawft_rpc::pipe_name_for_path`]
//!
//! The kernel itself is platform-agnostic and could also be wrapped in
//! WebSocket, TCP, or `postMessage` for other environments.

use std::sync::{Arc, OnceLock};

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
#[cfg(unix)]
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::watch;
use tracing::{debug, error, info, warn};

use crate::control::{ControlFlags, ControlIntent, ControlKind};

/// Daemon-wide control state shared between the boot wiring (which
/// registers flags as services come up) and the `control.*` RPC
/// handlers (which read + flip them). Set once at daemon boot.
struct DaemonControlState {
    /// The daemon's own node-id, used as the authority that owns
    /// every control intent published by this daemon.
    daemon_node_id: String,
    /// Per-target enable flags, shared with services that consult
    /// them in their main loops.
    flags: ControlFlags,
}

static DAEMON_CONTROL: OnceLock<Arc<DaemonControlState>> = OnceLock::new();

fn daemon_control() -> Option<Arc<DaemonControlState>> {
    DAEMON_CONTROL.get().cloned()
}

/// Daemon-wide handle to the LLM HTTP client. Set at boot if the
/// service spawns successfully; `None` otherwise. The `llm.prompt`
/// handler reads this and returns a clean error when unset rather
/// than panicking.
///
/// WEFT-343: wrapped in `Arc<RwLock<_>>` so a mid-session env rotation
/// (e.g. `OPENROUTER_API_KEY`) can swap the inner client via
/// `control.set_enabled {kind:"service", target:"llm"}` without
/// rebuilding every agent-loop / adapter that holds a clone of the
/// outer `Arc`.
static DAEMON_LLM: OnceLock<clawft_service_llm::SharedLlmClient> = OnceLock::new();

pub(crate) fn daemon_llm() -> Option<clawft_service_llm::SharedLlmClient> {
    DAEMON_LLM.get().cloned()
}

/// WEFT-343: re-read env + `[kernel.llm]` and replace the inner
/// [`LlmClient`] under `DAEMON_LLM`. Call-sites that already hold the
/// shared handle observe the new client on their next read lock.
///
/// Returns `Err` when construction fails or when the client was never
/// wired at boot (agent loop / service registration already skipped —
/// a full re-wire is out of scope for this ticket).
async fn refresh_daemon_llm_from_env(
    kernel: &Arc<tokio::sync::RwLock<Kernel<NativePlatform>>>,
) -> Result<(), String> {
    let shared = daemon_llm().ok_or_else(|| {
        "llm service not initialized (cannot refresh — was not wired at boot)".to_string()
    })?;
    let cfg_llm = {
        let k = kernel.read().await;
        k.kernel_config().llm.clone()
    };
    let (new_client, resolved) =
        crate::llm_service::build_llm_client(cfg_llm.as_ref()).map_err(|e| e.to_string())?;

    let (old_url, old_model) = {
        let guard = shared.read().await;
        (
            guard.config().base_url.clone(),
            guard.config().model.clone(),
        )
    };
    *shared.write().await = new_client;
    info!(
        old_url = %old_url,
        old_model = %old_model,
        url = %resolved.config.base_url,
        model = %resolved.config.model,
        url_source = resolved.url_source,
        model_source = resolved.model_source,
        openrouter = resolved.using_openrouter,
        "llm client swapped after env refresh (WEFT-343)"
    );
    Ok(())
}

/// Swap the daemon's LLM client when the placed `local` role moved (ADR-101
/// section 5). Only a placement-sourced endpoint follows the table: an
/// explicit URL, config or the OpenRouter takeover do not change with it.
#[cfg(all(feature = "placement", unix))]
async fn refresh_llm_if_placement_changed(kernel: &Arc<tokio::sync::RwLock<Kernel<NativePlatform>>>) {
    let Some(shared) = daemon_llm() else { return };
    let cfg_llm = kernel.read().await.kernel_config().llm.clone();
    let resolved = crate::llm_service::resolve_llm_endpoint(cfg_llm.as_ref());
    if !matches!(resolved.url_source, "placement:local" | "default:local") {
        return;
    }
    let current = shared.read().await.config().base_url.clone();
    if current != resolved.config.base_url
        && let Err(e) = refresh_daemon_llm_from_env(kernel).await
    {
        warn!(error = %e, "llm client not refreshed after a placement change");
    }
}

/// Daemon-wide handle to the PTY-backed terminal manager. Set at boot;
/// the four `terminal.*` handlers read this. We don't register a
/// control flag for terminal — sessions are user-initiated (no
/// background traffic to gate) and the GUI's "close session" button is
/// the natural off-switch. If we ever grow auto-spawned sessions, the
/// control flag lives here next to `DAEMON_LLM`.
static DAEMON_TERMINAL: OnceLock<Arc<clawft_service_terminal::TerminalManager>> = OnceLock::new();

fn daemon_terminal() -> Option<Arc<clawft_service_terminal::TerminalManager>> {
    DAEMON_TERMINAL.get().cloned()
}

/// agent-core-v1 Phase D3 (the cutover): daemon-wide handle to the
/// agent service (`clawft-service-agent::AgentService`). Set at boot
/// when the LLM client succeeded — `None` if `DAEMON_LLM` couldn't be
/// wired.
///
/// Every `agent.chat` request unconditionally dispatches through this
/// handle. The C2 spike fallback was removed in D3 along with the
/// inline `handle_agent_chat` body; if the service didn't wire the
/// dispatch arm surfaces a typed "agent service not wired" error
/// instead of falling back. The temporary `agent-core-chat` Cargo
/// feature was removed in WEFT-497 after D3 soak (no `#[cfg]` gates
/// remained; the flag was an empty default-on toggle).
///
/// Generic param matches the blanket
/// `impl<P: Platform> AgentLoopHandle for AgentLoop<P>` so production
/// uses `AgentLoop<NativePlatform>` directly.
type DaemonAgentService =
    clawft_service_agent::AgentService<clawft_core::agent::loop_core::AgentLoop<NativePlatform>>;

static DAEMON_AGENT: OnceLock<Arc<DaemonAgentService>> = OnceLock::new();

fn daemon_agent() -> Option<Arc<DaemonAgentService>> {
    DAEMON_AGENT.get().cloned()
}

/// ADR-069 / WEFT-641: daemon-wide atom primary index (Panopticon).
/// Set at boot when the agent anchor is wired (chain path present); `None`
/// when anchors are off — absence is a supported mode (P3 removability).
/// Serves `atom.locate` / `atom.audit` RPCs; never load-bearing for turns.
static DAEMON_ATOM_REGISTRY: OnceLock<Arc<clawft_service_agent::AtomRegistry>> = OnceLock::new();

fn daemon_atom_registry() -> Option<Arc<clawft_service_agent::AtomRegistry>> {
    DAEMON_ATOM_REGISTRY.get().cloned()
}

/// WEFT-654: daemon-wide handle to the concrete [`AgentLoop`] behind
/// `DAEMON_AGENT`, stashed separately because `AgentLoopHandle` (the trait
/// `AgentService<H>` is generic over) doesn't expose the review-gate
/// surface (`pending_hold_info` / `accept_pending` / `discard_pending` —
/// see `clawft-core/src/agent/loop_core.rs`). Set once at boot alongside
/// `DAEMON_AGENT`, from the same `Arc` before it moves into
/// `AgentService::new`. The `agent.proposal.*` RPC arms read this instead
/// of `daemon_agent()`.
///
/// [`AgentLoop`]: clawft_core::agent::loop_core::AgentLoop
static DAEMON_AGENT_LOOP: OnceLock<Arc<clawft_core::agent::loop_core::AgentLoop<NativePlatform>>> =
    OnceLock::new();

/// WEFT-329: keep the identity notify watcher alive for the daemon
/// process lifetime. Dropping the handle would stop invalidation;
/// without it the provider stays in watch mode but never marks stale.
static DAEMON_IDENTITY_WATCHER: OnceLock<clawft_core::agent::identity_watcher::IdentityWatcherHandle> =
    OnceLock::new();

/// `pub(crate)` (mirrors [`daemon_agent`]) so `voice_loop.rs` can reach the
/// review-gate surface directly — WEFT-655 Gap A needs `pending_hold_info`
/// from `dispatch_reply`'s Ok arm, which `AgentLoopHandle` doesn't expose.
pub(crate) fn daemon_agent_loop(
) -> Option<Arc<clawft_core::agent::loop_core::AgentLoop<NativePlatform>>> {
    DAEMON_AGENT_LOOP.get().cloned()
}

/// WEFT-655: the review-gate hold label a turn mints, pinned to
/// `AgentLoop::handle_turn`'s `format!("turn:{}:{}", msg.channel,
/// msg.chat_id)` (`clawft-core/src/agent/loop_core.rs`) where `channel` is
/// always `AGENT_CHAT_CHANNEL` ("agent.chat") and `chat_id` is the `conv_id`
/// passed to `AgentService::dispatch` (`inbound_from_params`,
/// `clawft-service-agent/src/service.rs`). Every voice-loop dispatch goes
/// through that exact path (`agent.dispatch(params)` in
/// `voice_loop::dispatch_reply`), so this is the label to expect held for a
/// voice-originated reply. Pure fn, pinned against the literal string in the
/// `tests` module below so a rename on the core side breaks loudly here
/// instead of silently missing the Gap A park.
pub(crate) fn turn_hold_label(conv_id: &str) -> String {
    format!("turn:agent.chat:{conv_id}")
}

/// WEFT-655 Gap A: conv_id + attempt seq for a voice-originated reply whose
/// forest commit (`SessionTier::commit_reply_frontier`) was parked because
/// this SAME turn's memory promote is held as a pending review-mode
/// cow-memory proposal (`AgentLoop::handle_turn`, D9) — see `voice_loop`'s
/// module doc for the full forest/memory asymmetry this closes. Keyed by the
/// proposal's hold label ([`turn_hold_label`]) so the `agent.proposal.*` RPC
/// arms and the timeout sweep can find/clear the matching attempt without
/// threading anything through `clawft-service-agent`. Lazily initialized —
/// cheap and empty on every daemon that never parks an attempt (no voice
/// loop, or voice loop but never review mode).
static DAEMON_PARKED_VOICE_ATTEMPTS: OnceLock<Arc<dashmap::DashMap<String, (String, u64)>>> =
    OnceLock::new();

fn daemon_parked_voice_attempts() -> Arc<dashmap::DashMap<String, (String, u64)>> {
    DAEMON_PARKED_VOICE_ATTEMPTS
        .get_or_init(|| Arc::new(dashmap::DashMap::new()))
        .clone()
}

/// Park `(conv_id, seq)` under `label` (WEFT-655 Gap A). Overwrites any
/// stale entry for the same label — there is only ever one hold at a time
/// (D9 fails new dispatches closed while one is pending), so a second park
/// under the same label can't happen in practice; last-write-wins is safe
/// either way.
pub(crate) fn park_voice_attempt(label: &str, conv_id: &str, seq: u64) {
    daemon_parked_voice_attempts().insert(label.to_string(), (conv_id.to_string(), seq));
}

/// Take (remove) the parked attempt for `label`, if any — WEFT-655 Gap A.
fn take_parked_voice_attempt(label: &str) -> Option<(String, u64)> {
    daemon_parked_voice_attempts()
        .remove(label)
        .map(|(_, v)| v)
}

/// WEFT-655 Gap A / WEFT-673: after a held proposal resolves as ACCEPTED
/// (`agent.proposal.accept`), resolve any voice reply parked behind it —
/// commits the forest attempt `dispatch_reply`'s Ok arm deferred, clears
/// the voice loop's busy axis, and drains one queued utterance (WEFT-673
/// hold-drain seam — same order as ordinary finalize: commit/clear, then
/// `drain_next`). No-op when nothing was parked under `label` (the ordinary
/// text-chat / no-voice case).
fn resolve_parked_attempt_on_accept(label: &str) {
    let Some((conv, seq)) = take_parked_voice_attempt(label) else {
        return;
    };
    if let Some(tier) = daemon_agent().and_then(|a| a.session_tier().cloned()) {
        tier.commit_reply_frontier(&conv, seq);
    }
    if let Some(voice_loop) = DAEMON_VOICE_LOOP.get() {
        // Busy axis first, then drain — the drained resubmit must not see
        // the parked attempt as still in flight (WEFT-673).
        voice_loop.clear_parked_in_flight(&conv, seq);
        voice_loop.drain_after_hold_resolve(&conv);
    }
    info!(
        conv_id = %conv,
        seq,
        label = %label,
        "WEFT-655/673: parked voice attempt resolved on proposal accept \
         (forest committed + queue drain)"
    );
}

/// WEFT-655 Gap A / WEFT-673: after a held proposal resolves as DISCARDED
/// (operator `agent.proposal.discard` RPC arm, or the fail-closed timeout
/// sweep — [`run_proposal_timeout_sweep_tick`]), clear any voice reply
/// parked behind it the SAME way a bare STOP interrupt does: prune
/// tombstone + witnessed cancel (`SessionTier::emit_cancel_prune` /
/// `witness_cancel`), then clear the voice busy axis and drain one queued
/// utterance (WEFT-673). Unlike a STOP-driven `Cancelled` finalize — which
/// deliberately does **not** drain because the Refine resubmit that caused
/// the cancel drains on *its* finalize — a proposal discard has no
/// replacement attempt, so the queue would stall without an explicit drain.
/// No-op when nothing was parked under `label`.
fn cleanup_parked_attempt_on_discard(label: &str) {
    let Some((conv, seq)) = take_parked_voice_attempt(label) else {
        return;
    };
    if let Some(tier) = daemon_agent().and_then(|a| a.session_tier().cloned()) {
        let pruned = tier.emit_cancel_prune(&conv, Some(seq), None);
        tier.witness_cancel(&conv, pruned);
    }
    if let Some(voice_loop) = DAEMON_VOICE_LOOP.get() {
        voice_loop.clear_parked_in_flight(&conv, seq);
        // WEFT-673: discard has no Refine replacement — drain so queued
        // utterances do not wait for the next ordinary turn.
        voice_loop.drain_after_hold_resolve(&conv);
    }
    info!(
        conv_id = %conv,
        seq,
        label = %label,
        "WEFT-655/673: parked voice attempt cleaned up on proposal discard \
         (pruned + witnessed + queue drain)"
    );
}

/// Pure predicate for [`run_proposal_timeout_sweep_tick`]'s age check —
/// pulled out so the age-vs-timeout boundary is unit-testable without a real
/// loop/hold. Matches the original inline WEFT-654 D10 check (`age >
/// timeout`, not `>=` — a hold exactly at the deadline survives one more
/// tick).
fn proposal_past_timeout(age: std::time::Duration, timeout: std::time::Duration) -> bool {
    age > timeout
}

/// WEFT-654 D10 / WEFT-655 Gap B: one tick of the review-mode proposal
/// fail-closed timeout guard — discard a hold past
/// `[kernel.agent.proposal].timeout_secs`, never silently commit it, with
/// the same Gap A parked-attempt cleanup the operator-driven discard RPC arm
/// gets ([`cleanup_parked_attempt_on_discard`]). Shared between the M2
/// reaper's inline call (hosted only when `talk_loop` is on) and the
/// standalone sweep task hosted when it is not (see `run_daemon`'s boot
/// wiring) — a review-mode hold gets fail-closed timeout coverage either
/// way. No-op when nothing is held, or no loop/timeout is wired.
fn run_proposal_timeout_sweep_tick() {
    let (Some(loop_handle), Some(timeout)) = (daemon_agent_loop(), DAEMON_PROPOSAL_TIMEOUT.get())
    else {
        return;
    };
    let Some((label, age)) = loop_handle.pending_hold_info() else {
        return;
    };
    if !proposal_past_timeout(age, *timeout) {
        return;
    }
    match loop_handle.discard_pending("timeout") {
        Ok(_) => {
            cleanup_parked_attempt_on_discard(&label);
            warn!(
                label = %label,
                age_secs = age.as_secs(),
                timeout_secs = timeout.as_secs(),
                "proposal timeout sweep: proposal timed out — discarded (fail-closed)"
            );
        }
        Err(e) => warn!(
            label = %label,
            error = %e,
            "proposal timeout sweep: proposal timeout discard failed"
        ),
    }
}

/// WEFT-654: format `AgentLoop::pending_hold_info`'s `(label, age)` into the
/// `agent.proposal.list` wire shape. A pure function (no `AgentLoop`
/// dependency) so the list/forward-compat shape is unit-testable without
/// standing up a real loop — see the `tests` module below.
fn proposal_list_result(pending: Option<(String, std::time::Duration)>) -> AgentProposalListResult {
    AgentProposalListResult {
        pending: pending
            .into_iter()
            .map(|(label, age)| AgentProposalInfo {
                label,
                age_secs: age.as_secs(),
            })
            .collect(),
    }
}

/// WEFT-654: resolve `agent.proposal.discard`'s reason — the caller-supplied
/// string, or `"operator discard"` when omitted. Pulled out so the default
/// is unit-testable on its own (the timeout sweep bypasses this and calls
/// `discard_pending("timeout")` directly).
fn proposal_discard_reason(params: &AgentProposalDiscardParams) -> &str {
    params.reason.as_deref().unwrap_or("operator discard")
}

/// M4 Phase C.1/D5: daemon-wide handles to the subagent spawn substrate.
/// The `SpawnRegistry` backs the parent-end cascade (it knows a parent
/// conversation's still-running children); the `SubagentSpawner` trait object
/// performs the cancel (abort driver + cancel child turn + witness + release
/// the per-conv concurrency slot). Both are set once at boot alongside
/// `DAEMON_AGENT`, after the service `Arc` exists so the spawner's `Weak`
/// back-reference is wired (Arc-cycle break, design D2).
static DAEMON_SPAWN_REGISTRY: OnceLock<Arc<clawft_service_agent::SpawnRegistry>> = OnceLock::new();
static DAEMON_SUBAGENT_SPAWNER: OnceLock<Arc<dyn clawft_core::agent::spawn::SubagentSpawner>> =
    OnceLock::new();

/// M4 D5: fully end every still-running child of a parent conversation that is
/// itself ending — driven by `agent.chat.end` and the idle reaper (conversation
/// over ⇒ children die). Each child is torn down the way the parent is:
/// `spawner.cancel` aborts the detached driver, cancels the in-flight child
/// turn, releases the parent's live concurrency slot, and witnesses
/// `agent.cancel`; then the child's ephemeral L2 SessionTier view is dropped
/// (no postmortem — the child was torn down, not completed) and its talk-loop
/// state evicted, so the reaper never revisits it. A no-op when the spawn
/// substrate isn't wired or the parent has no live children.
///
/// NOTE: `agent.chat.cancel` deliberately does NOT call this — a turn-level
/// cancel must not destroy detached (`await:false`) children, which are
/// independent work the user fired off on purpose (see the `agent.chat.cancel`
/// arm). Only conversation-end (here) tears the whole spawn tree down.
async fn cascade_end_children(parent_conv: &str) {
    let (Some(registry), Some(spawner)) =
        (DAEMON_SPAWN_REGISTRY.get(), DAEMON_SUBAGENT_SPAWNER.get())
    else {
        return;
    };
    let children = registry.live_children(parent_conv);
    if children.is_empty() {
        return;
    }
    for task_id in &children {
        // Resolve the child conv before cancel — the record persists after the
        // terminal transition, so this is safe either way.
        let child_conv = registry.child_conv(task_id);
        if let Err(e) = spawner.cancel(task_id).await {
            warn!(
                parent = %parent_conv,
                task_id = %task_id,
                error = %e,
                "M4 cascade end: child cancel failed"
            );
        }
        if let Some(child_conv) = child_conv {
            if let Some(agent) = daemon_agent() {
                let _ = agent.end_conversation(&child_conv, None);
            }
            if let Some(talk_loop) = DAEMON_TALK_LOOP.get() {
                talk_loop.end_conversation(&child_conv);
            }
        }
    }
    info!(
        parent = %parent_conv,
        children = children.len(),
        "M4: ended spawned children on parent end"
    );
}

/// Phase B enrichment drain (classification-design §D4, plan B3). Pulls
/// [`clawft_service_agent::EnrichJob`]s from the bounded queue one at a time,
/// refines each committed turn's classification blob with the cheap-model
/// [`clawft_service_agent::EnrichmentClassifier`], and patches the node
/// (`tier → "llm"`) via [`clawft_kernel::causal::CausalGraph::merge_node_metadata`]
/// (B1). Serialises the LLM calls (one job in flight) and exits on daemon
/// shutdown. Every effect is best-effort: a backend/parse failure or a
/// vanished node leaves the durable keyword blob in place, never an error.
async fn run_enrich_drain(
    queue: Arc<clawft_service_agent::EnrichQueue>,
    enricher: Arc<clawft_service_agent::EnrichmentClassifier>,
    causal: Arc<clawft_kernel::causal::CausalGraph>,
    shutdown_rx: &mut tokio::sync::watch::Receiver<bool>,
) {
    loop {
        let job = tokio::select! {
            biased;
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    debug!("classification enrich drain shutting down");
                    return;
                }
                continue;
            }
            job = queue.recv() => job,
        };

        // `prev_topic` — the node's existing keyword topic, threaded so the
        // enricher can fall back to it when the model omits a topic (design §D2
        // continuity). Read live from the graph, matching the §D4 job shape
        // (which carries no topic).
        let prev_topic = causal.get_node(job.node_id).and_then(|n| {
            n.metadata
                .get("classification")
                .and_then(|c| c.get("topic"))
                .and_then(|t| t.as_str())
                .map(str::to_string)
        });

        // Cheap-model round-trip off the turn path. `enrich` returns `None` on
        // any backend/parse failure, in which case the node keeps its durable
        // keyword blob (best-effort refinement, never a regression).
        let Some(cv) = enricher.enrich(&job.text, prev_topic.as_deref()).await else {
            continue;
        };

        // Patch only the `classification` key (now `tier = "llm"`);
        // `merge_node_metadata` leaves the protected identity/lifecycle keys
        // (state, uid, chain_seq, text, conv_id) untouched (B1). The GUI shows
        // the refinement on its next `conversation.graph` poll.
        let mut patch = serde_json::Map::new();
        patch.insert("classification".to_string(), cv.to_metadata_value());
        if !causal.merge_node_metadata(job.node_id, &patch) {
            warn!(
                node_id = job.node_id,
                conv_id = %job.conv_id,
                "enrich drain: node vanished before its classification patch"
            );
        }
    }
}

/// Daemon-wide handle to the agent conversation sink, for the
/// `agent.turn.record` RPC — externally-produced turns (the `weft voice
/// talk` Talk-Mode loop) recorded through the SAME
/// `SubstrateConversationSink` + `KernelTurnAnchor` as `agent.chat`
/// turns, so voice exchanges land on the substrate JSONL and are
/// mirrored to the witness chain / HNSW / causal graph / session tier
/// per the `[kernel.agent]` anchor flags. Set at boot alongside
/// `DAEMON_AGENT`.
static DAEMON_TURN_SINK: OnceLock<Arc<dyn clawft_core::agent::sink::ConversationSink>> =
    OnceLock::new();

fn daemon_turn_sink() -> Option<Arc<dyn clawft_core::agent::sink::ConversationSink>> {
    DAEMON_TURN_SINK.get().cloned()
}

/// M2 D1/D6: daemon-wide handle to the multiplexed `TalkModeLoop` hosted at
/// boot when `[kernel.agent].talk_loop` is on. Stashed so the `agent.chat.end`
/// arm and the idle reaper can evict a conversation's loop state via
/// `TalkModeLoop::end_conversation`. Set once at boot alongside `DAEMON_AGENT`.
static DAEMON_TALK_LOOP: OnceLock<Arc<clawft_kernel::TalkModeLoop>> = OnceLock::new();

/// Voice Wave 2 §W2.1: the non-blocking voice→agent loop router. Set at boot
/// ONLY when `[kernel.agent].voice_loop` is on and its `talk_loop`
/// prerequisite holds — its presence IS the gate the `agent.turn.record` arm
/// consults before routing recorded user turns through the interrupt brain.
/// WEFT-652 AutoPause: the attached cow lineage + its chain witness, stashed
/// so the idle reaper can park the lineage when the daemon goes fully idle.
/// The bracket auto-resumes on the next turn (clawft-core turn_checkpoint).
static DAEMON_COW_MEMORY: OnceLock<
    Arc<std::sync::Mutex<clawft_core::clawft_cow_memory::BranchableMemory>>,
> = OnceLock::new();
static DAEMON_TURN_LEDGER: OnceLock<Arc<crate::turn_ledger::DaemonTurnLedger>> = OnceLock::new();

/// WEFT-654 D10: `[kernel.agent.proposal].timeout_secs` (default 600),
/// snapshotted as a `Duration` where `anchor_cfg` is read so the M2 idle
/// reaper — which runs outside that scope — can enforce the fail-closed
/// timeout guard without re-reading the kernel config. Set once regardless
/// of whether cow-memory/review actually attached; the reaper only acts on
/// it when `AgentLoop::pending_hold_info` returns `Some`, so an unused
/// timeout with no cow attachment is inert.
static DAEMON_PROPOSAL_TIMEOUT: OnceLock<std::time::Duration> = OnceLock::new();

/// WEFT-655 Gap B: `[kernel.agent.proposal].mode == "review"`, snapshotted
/// alongside [`DAEMON_PROPOSAL_TIMEOUT`] (same `anchor_cfg` scope,
/// out-of-scope by the time the M2 reaper / standalone sweep spawn below).
/// The reaper-hosting `if let` decides whether to spawn the standalone
/// timeout-sweep task by reading this rather than `anchor_cfg` directly.
static DAEMON_PROPOSAL_REVIEW_MODE: OnceLock<bool> = OnceLock::new();

static DAEMON_VOICE_LOOP: OnceLock<
    Arc<crate::voice_loop::VoiceLoop<clawft_core::agent::loop_core::AgentLoop<NativePlatform>>>,
> = OnceLock::new();

/// Phase B (classification-design §D4): the bounded async-enrichment queue and
/// its cheap-model classifier, wired at boot only when
/// `[kernel.agent.classification] mode = full`. `index_turn` (via the session
/// tier) enqueues into the queue; the drain task spawned in the run loop pulls
/// jobs, refines the blob with the classifier, and patches the node
/// (`tier → "llm"`). Kept out of the tier so the drain lives under the daemon's
/// shutdown cancel like `run_talk_loop`, not the request path.
static DAEMON_ENRICH_QUEUE: OnceLock<Arc<clawft_service_agent::EnrichQueue>> = OnceLock::new();
static DAEMON_ENRICH_CLASSIFIER: OnceLock<Arc<clawft_service_agent::EnrichmentClassifier>> =
    OnceLock::new();

/// M2 D6: per-conversation last-activity timestamps for the idle reaper.
/// Touched in the `agent.chat` / `agent.turn.record` arms; the reaper walks
/// `SessionTier::active_conversations()` and ends any conversation idle past
/// `[kernel.agent].talk_loop_idle_secs`. Kept out of the view (no cheap
/// wall-clock ts there) per plan §4.
static DAEMON_CONV_ACTIVITY: OnceLock<Arc<dashmap::DashMap<String, std::time::Instant>>> =
    OnceLock::new();

/// agent-core-v1 Phase D2 / WEFT-332: daemon-wide handle to the
/// concierge agent's kernel-issued `agent_id`.
///
/// Set at boot when the daemon registers a concierge principal in
/// `clawft-kernel::AgentRegistry::register` (right next to the
/// existing node registration). Used as the single-tenant fallback
/// when an `agent.chat` request has no `caller_id`. Multi-tenant
/// callers (WEFT-332) get a lazily-registered
/// `agent.chat:user:<caller_id>` principal via
/// [`clawft_service_agent::AgentService::with_caller_registry`]; that
/// UUID is stamped into inbound metadata and wins over this default
/// in `AgentLoop` gate checks. Also threaded into
/// `AgentLoop::with_daemon_agent_id` so requests without metadata
/// still use the concierge principal rather than the
/// `"{channel}:{sender_id}"` synthesis.
static DAEMON_CONCIERGE_AGENT_ID: OnceLock<String> = OnceLock::new();

/// Per-service restart counter. Bumped each time `service.restart`
/// fires; read by `kernel.services` to populate the `restarts` field.
/// Keyed by service name (`SystemService::name()`).
static SERVICE_RESTART_COUNTS: OnceLock<std::sync::Mutex<std::collections::HashMap<String, u64>>> =
    OnceLock::new();

fn service_restart_counts() -> &'static std::sync::Mutex<std::collections::HashMap<String, u64>> {
    SERVICE_RESTART_COUNTS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Read this process's resident set size in bytes from
/// `/proc/self/status` (Linux). Returns 0 on read or parse failure.
/// The kernel's in-process service registry doesn't track per-service
/// memory because services share the daemon's address space — the
/// daemon's RSS IS the aggregate kernel footprint, which is the
/// number worth showing on the kernel root row.
fn read_self_rss_bytes() -> u64 {
    #[cfg(target_os = "linux")]
    {
        if let Ok(s) = std::fs::read_to_string("/proc/self/status") {
            for line in s.lines() {
                if let Some(rest) = line.strip_prefix("VmRSS:") {
                    let kb = rest
                        .split_whitespace()
                        .next()
                        .and_then(|n| n.parse::<u64>().ok())
                        .unwrap_or(0);
                    return kb.saturating_mul(1024);
                }
            }
        }
    }
    0
}

#[allow(dead_code)] // Read by future RPCs (e.g. `agent.whoami`)
fn daemon_concierge_agent_id() -> Option<String> {
    DAEMON_CONCIERGE_AGENT_ID.get().cloned()
}

use clawft_kernel::{Kernel, KernelState};
use clawft_platform::NativePlatform;
use clawft_types::config::{Config, KernelConfig};

use crate::protocol::{
    self, AgentChatParams, AgentInspectResult, AgentProposalDecisionResult,
    AgentProposalDiscardParams, AgentProposalInfo, AgentProposalListResult, AgentRestartParams,
    AgentSendParams, AgentSpawnParams, AgentSpawnResult, AgentStopParams, AgentTurnRecordParams,
    ClusterJoinParams, ClusterLeaveParams, ClusterNodeInfo, ClusterStatusResult, CronAddParams,
    CronJobInfo, CronRemoveParams, IpcPublishParams, IpcSubscribeParams, IpcTopicInfo,
    KernelStatusResult, LogEntry, LogsParams, ProcessInfo, Request, Response, ServiceInfo,
};
#[cfg(feature = "exochain")]
use crate::protocol::{
    ChainAppendParams, ChainAppendResult, ChainEventInfo, ChainExportParams, ChainLocalParams,
    ChainStatusResult, ChainVerifyResult, ResourceInspectParams, ResourceNodeInfo,
    ResourceRankEntry, ResourceRankParams, ResourceScoreParams, ResourceScoreResult,
    ResourceStatsResult,
};

/// Spawn the daemon into the background.
///
/// Spawns `weaver kernel start --foreground` as a detached child process,
/// redirecting stdout/stderr to the kernel log file. Writes the child PID
/// to the PID file. The parent returns after the child is launched.
///
/// Works on Unix and Windows (WEFT-559). On Windows the child is created
/// detached so closing the parent console does not kill the daemon.
pub fn daemonize(
    config_override: Option<&str>,
    new_chain: bool,
    adopt_legacy_chain: bool,
) -> anyhow::Result<()> {
    use std::process::Command;

    let runtime_dir = protocol::runtime_dir();
    std::fs::create_dir_all(&runtime_dir)?;

    let log_path = protocol::log_path();
    let pid_path = protocol::pid_path();

    // Check if already running
    if pid_path.exists()
        && let Ok(pid_str) = std::fs::read_to_string(&pid_path)
    {
        if let Ok(pid) = pid_str.trim().parse::<u32>()
            && process_alive(pid)
        {
            anyhow::bail!("kernel already running (pid {pid})");
        }
        // Stale PID file
        let _ = std::fs::remove_file(&pid_path);
    }

    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let log_err = log_file.try_clone()?;

    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.args(["kernel", "start", "--foreground"]);
    if let Some(cfg) = config_override {
        cmd.args(["--config", cfg]);
    }
    if new_chain {
        cmd.arg("--new-chain");
    }
    if adopt_legacy_chain {
        cmd.arg("--adopt-legacy-chain");
    }
    if crate::user_daemon::is_active() {
        cmd.args(["--profile", crate::user_daemon::PROFILE_USER]);
    } else {
        // The parent already passed the legacy-daemon check (a project-rooted
        // daemon beside a user daemon); the foreground child must not refuse
        // it a second time.
        cmd.arg("--legacy-project-daemon");
    }

    // Windows: detach so the daemon outlives the spawning console.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP
        const FLAGS: u32 = 0x00000008 | 0x00000200;
        cmd.creation_flags(FLAGS);
    }

    let mut child = cmd
        .stdout(log_file)
        .stderr(log_err)
        .stdin(std::process::Stdio::null())
        .spawn()?;

    let pid = child.id();
    // The child writes kernel.pid itself once boot (runtime and chain locks)
    // succeeded; success is reported only when it serves.
    let ready =
        wait_for_daemon_ready(&mut child, &protocol::socket_path(), &pid_path, &log_path)?;
    if !ready {
        // Exit status 0: the daemon is alive and booting (a big chain can
        // take minutes); it is not a failure, but nothing is serving yet.
        println!(
            "WeftOS kernel still starting (pid {pid}); check `weaver kernel status` \
             (log: {})",
            log_path.display()
        );
        return Ok(());
    }

    println!("WeftOS kernel started (pid {pid})");
    #[cfg(unix)]
    println!("  Socket: {}", protocol::socket_path().display());
    #[cfg(windows)]
    println!(
        "  Pipe:   {}",
        clawft_rpc::pipe_name_for_path(protocol::socket_path())
    );
    println!("  Log:    {}", log_path.display());
    println!("  PID:    {}", pid_path.display());
    println!();
    println!("Use 'weaver kernel status' to check, 'weaver kernel attach' to view logs.");
    println!("Use 'weaver kernel stop' to shut down.");

    Ok(())
}

/// Last few non-empty lines of the daemon log, for failure messages.
fn log_tail(log_path: &std::path::Path) -> String {
    let Ok(text) = std::fs::read(log_path) else {
        return String::new();
    };
    let text = String::from_utf8_lossy(&text[text.len().saturating_sub(4096)..]).into_owned();
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(8)..].join("\n")
}

/// True once the daemon's endpoint accepts connections.
fn daemon_ready(socket: &std::path::Path, pid_path: &std::path::Path, want_pid: &str) -> bool {
    #[cfg(unix)]
    {
        // The child writes kernel.pid after boot; requiring our child's pid
        // keeps another live daemon's socket from counting as ours.
        std::fs::read_to_string(pid_path).is_ok_and(|s| s.trim() == want_pid)
            && std::os::unix::net::UnixStream::connect(socket).is_ok()
    }
    #[cfg(not(unix))]
    {
        // Named pipes have no sync probe here; the PID file is written only
        // after a successful boot, so it is the readiness marker.
        let _ = socket;
        std::fs::read_to_string(pid_path).is_ok_and(|s| s.trim() == want_pid)
    }
}

/// Wait for the freshly spawned daemon to become reachable, failing with the
/// tail of its log if it exits first (for example because another kernel
/// owns the runtime dir or the chain). Returns `true` once the daemon
/// serves and `false` if it is still starting after the timeout.
fn wait_for_daemon_ready(
    child: &mut std::process::Child,
    socket: &std::path::Path,
    pid_path: &std::path::Path,
    log_path: &std::path::Path,
) -> anyhow::Result<bool> {
    let want = child.id().to_string();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    while std::time::Instant::now() < deadline {
        if let Some(status) = child.try_wait()? {
            anyhow::bail!(
                "kernel exited during startup ({status}); last log lines from {}:\n{}",
                log_path.display(),
                log_tail(log_path)
            );
        }
        if daemon_ready(socket, pid_path, &want) {
            return Ok(true);
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Ok(false)
}

/// Return true if a process with the given PID appears to be alive.
fn process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        use std::process::Command;
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
    #[cfg(windows)]
    {
        use std::process::Command;
        // tasklist /FI "PID eq N" /NH prints a line when the process exists.
        let output = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output();
        match output {
            Ok(o) => {
                let s = String::from_utf8_lossy(&o.stdout);
                s.contains(&pid.to_string())
            }
            Err(_) => false,
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        false
    }
}

/// Build the v2 [`EmbeddingRouter`](clawft_core::agent::context_router::EmbeddingRouter),
/// or `None` (with a `tracing::warn!`) when any prerequisite fails.
///
/// Phase E2 introduced this construction inline in the `"embedding"`
/// arm; Phase E3 extracted it here so the new `"hybrid"` arm reuses
/// the exact same skill-discovery + embedder-selection + index-build
/// path. The helper:
///
/// 1. Discovers skills via the same workspace-walk + user-dir lookup
///    the CLI's `weft agent` uses.
/// 2. Selects [`ApiEmbedder::with_defaults`](clawft_core::embeddings::api_embedder::ApiEmbedder)
///    when `OPENAI_API_KEY` is set, else
///    [`ApiEmbedder::hash_only`](clawft_core::embeddings::api_embedder::ApiEmbedder)
///    as the deterministic offline floor.
/// 3. Builds the index. On any failure (empty registry, discover
///    error, embedder/index build error) it emits a `warn!` and
///    returns `None` so callers can fall back to the next router in
///    their preference chain (NullRouter for `"embedding"`,
///    LlmClassifierRouter for `"hybrid"`).
async fn build_embedding_router_or_warn(
    workspace: &std::path::Path,
) -> Option<Arc<dyn clawft_core::agent::context_router::ContextRouter>> {
    // Skill discovery: same paths the CLI uses (`weft agent`); see
    // `clawft-cli/src/commands/agent.rs::discover_skill_dirs`.
    let user_skill_dir = dirs::home_dir().map(|h| h.join(".clawft").join("skills"));
    let ws_skill_dir = {
        let mut dir = workspace;
        let mut found: Option<std::path::PathBuf> = None;
        loop {
            let candidate = dir.join(".clawft").join("skills");
            if candidate.is_dir() {
                found = Some(candidate);
                break;
            }
            match dir.parent() {
                Some(parent) => dir = parent,
                None => break,
            }
        }
        found
    };

    let skills_result = clawft_core::agent::skills_v2::SkillRegistry::discover(
        ws_skill_dir.as_deref(),
        user_skill_dir.as_deref(),
        Vec::new(),
    )
    .await;

    let skills = match skills_result {
        Ok(s) if s.is_empty() => {
            warn!(
                "agent-core: EmbeddingRouter requested but skill registry is empty; \
                 returning None so caller can fall back"
            );
            return None;
        }
        Ok(s) => s,
        Err(e) => {
            warn!(
                error = %e,
                "agent-core: skill discovery failed; EmbeddingRouter unavailable"
            );
            return None;
        }
    };

    // Pick ApiEmbedder when an API key is configured, else the hash
    // floor. ApiEmbedder itself collapses to its SHA-256 fallback
    // per-call when the API errors, so production stays robust.
    let embedder: Arc<dyn clawft_core::embeddings::Embedder> = if let Some(remote) =
        crate::project_profile::project_embedder().await
    {
        // Project profile: the parent embeds; no local model, no API key.
        // Never build the index on a guessed width: until the parent has
        // reported its own, the router is unavailable.
        if !remote.is_ready() {
            warn!("agent-core: EmbeddingRouter unavailable; the user daemon has not reported an embedding width");
            return None;
        }
        remote
    } else if std::env::var("OPENAI_API_KEY")
        .map(|s| !s.is_empty())
        .unwrap_or(false)
    {
        info!("agent-core: EmbeddingRouter using ApiEmbedder (OpenAI-compat /embeddings)");
        Arc::new(clawft_core::embeddings::api_embedder::ApiEmbedder::with_defaults())
    } else {
        info!(
            "agent-core: EmbeddingRouter using ApiEmbedder hash-only fallback \
             (no OPENAI_API_KEY set)"
        );
        Arc::new(clawft_core::embeddings::api_embedder::ApiEmbedder::hash_only(384))
    };

    match clawft_core::agent::context_router::EmbeddingRouter::new(embedder, &skills).await {
        Ok(r) => {
            info!(
                router = "embedding",
                skills = skills.len(),
                "agent-core: ContextRouter v2 (EmbeddingRouter) built"
            );
            Some(Arc::new(r) as Arc<dyn clawft_core::agent::context_router::ContextRouter>)
        }
        Err(e) => {
            warn!(error = %e, "agent-core: EmbeddingRouter build failed");
            None
        }
    }
}

/// Open an existing `BranchableMemory` lineage at `dir`, or create a fresh
/// one (WEFT-616 Phase 2). Open-then-create keeps restarts idempotent; the
/// 384 dimension matches the brain's embedding width (rvf_real's default).
fn open_or_create_cow_memory(
    dir: &std::path::Path,
) -> Result<clawft_core::clawft_cow_memory::BranchableMemory, String> {
    use clawft_core::clawft_cow_memory::BranchableMemory;
    match BranchableMemory::open(dir) {
        Ok(mem) => Ok(mem),
        Err(_) => BranchableMemory::create(dir, 384).map_err(|e| e.to_string()),
    }
}

/// Boot the kernel with the daemon's persisted node key so its mesh /
/// cluster node id equals `identity.node_id` (ADR-103 D11).
pub(crate) async fn boot_kernel_with_identity(
    config: Config,
    kernel_config: KernelConfig,
    platform: Arc<NativePlatform>,
    identity: &crate::node_identity::DaemonIdentity,
) -> clawft_kernel::KernelResult<Kernel<NativePlatform>> {
    match identity.signing_key() {
        Ok(key) => {
            Kernel::boot_with_node_key(config, kernel_config, platform, Some(key.to_bytes())).await
        }
        // Service mode: the node id is the mesh service's; there is no key
        // here to derive one from and none is generated.
        Err(_) => {
            Kernel::boot_in_service_mode(config, kernel_config, platform, identity.node_id.clone())
                .await
        }
    }
}

/// User daemon only: seed `~/.weftos/projects` from the legacy
/// `~/.clawft/workspaces.json` (idempotent; a missing file seeds nothing).
fn seed_user_projects() {
    let Some(home) = clawft_types::runtime_paths::home_dir() else {
        return;
    };
    match clawft_types::project::seed_from_workspaces(
        &home.join(".clawft").join("workspaces.json"),
        &crate::user_daemon::manifests_dir(&home),
    ) {
        Ok(r) => info!(
            created = r.created.len(),
            adopted = r.adopted.len(),
            missing = r.missing.len(),
            unchanged = r.unchanged.len(),
            skipped = r.skipped.len(),
            "seeded project manifests from workspaces.json"
        ),
        Err(e) => warn!(error = %e, "could not seed project manifests from workspaces.json"),
    }
}

/// Run the kernel daemon.
///
/// Boots the kernel, binds the platform-local RPC transport (Unix socket
/// or Windows named pipe), and serves requests until shutdown is
/// requested (via `kernel.shutdown` RPC or OS signal / Ctrl+C).
///
/// `global_routing` / `workspace_routing` (WEFT-10) are the split layers
/// from the config loader. Global is the PermissionResolver ceiling;
/// workspace is the optional overlay that will be clamped.
pub async fn run(
    mut config: Config,
    mut kernel_config: KernelConfig,
    // WEFT-10: unmerged global routing (ceiling for PermissionResolver).
    global_routing: clawft_types::routing::RoutingConfig,
    // WEFT-10: workspace overlay routing, when present.
    workspace_routing: Option<clawft_types::routing::RoutingConfig>,
) -> anyhow::Result<()> {
    // ADR-103 A6 seams: bodies live in `project_hooks` (no-ops until Phase 2 H/F).
    // For the `project` profile `pre_boot` does the spawn handshake, key and
    // registration (and fixes `RuntimePaths::resolve()`), so it must stay the
    // first thing here.
    if crate::user_daemon::project_profile_requested() {
        config.kernel.profile = Some(clawft_types::config::KernelProfile::Project);
        kernel_config.profile = Some(clawft_types::config::KernelProfile::Project);
        // A nested user instance owns its project kernels (D10): exit when it dies.
        #[cfg(unix)]
        if crate::parent_liveness::requested() {
            crate::parent_liveness::spawn_stdin_watcher();
        }
    }
    #[cfg(all(unix, feature = "exochain"))]
    let nested_master = config.weave.master;
    let mut pre_boot = crate::project_hooks::pre_boot(&mut config, &kernel_config).await?;
    crate::project_hooks::adjust_services(&mut config, &mut kernel_config);
    let paths = protocol::runtime_paths();
    let socket_path = paths.socket();
    // A systemd watchdog runs from unit start; keep it fed while booting.
    crate::liveness::boot_keepalive();

    // ADR-103 P0b: one kernel per runtime dir. Hold the advisory lock for
    // the daemon's whole lifetime; with it held nobody else is serving the
    // socket, so a leftover socket file can be reclaimed safely.
    let reexec = std::env::var_os(crate::boot_refusal::REEXEC_ENV).is_some();
    let instance_lock = if reexec {
        crate::instance_lock::InstanceLock::acquire_after_reexec(
            &paths,
            crate::boot_refusal::REEXEC_LOCK_WAIT,
        )?
    } else {
        crate::instance_lock::InstanceLock::acquire(&paths)?
    };
    info!(root = %paths.root().display(), "runtime dir locked");

    // WEFT-39: persist shared LLM RetryModel learned weights so the next
    // daemon start restores the curve instead of resetting to untrained.
    clawft_core::pipeline::persist_shared_retry_model();

    // Endpoint probe, with the lock held (platform-specific).
    #[cfg(unix)]
    crate::instance_lock::reclaim_stale_socket(&paths).await?;
    #[cfg(windows)]
    {
        // Named pipes have no filesystem node; dial the derived pipe name.
        if clawft_rpc::DaemonClient::connect_path(&socket_path)
            .await
            .is_some()
        {
            anyhow::bail!(
                "daemon already running (named pipe accepting: {})",
                clawft_rpc::pipe_name_for_path(&socket_path)
            );
        }
    }

    // Ensure parent directory exists (UDS parent; also holds PID/log files).
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // agent-core-v1 Phase E1: snapshot the ContextRouter selector
    // before `config` moves into `Kernel::boot`. The agent-service
    // wiring further below reads this to pick between v0 NullRouter
    // and v1 LlmClassifierRouter. See `Config.routing.context_router`
    // in `clawft-types::routing`.
    let context_router_choice = config.routing.context_router.clone();

    // Snapshot split routing layers before `config` moves into
    // `Kernel::boot`. Global is the PermissionResolver ceiling;
    // workspace is the overlay that will be clamped (WEFT-10).
    let agent_routing = global_routing;
    let agent_workspace_routing = workspace_routing;

    // WEFT-555 (M5-W): snapshot the voice-consumer section so it
    // survives `Kernel::boot` taking ownership of `config`. The voice
    // consumer wires further below, after the agent service is up.
    let voice_consumer_cfg = config.voice.consumer.clone();
    let voice_mic_pin = config.voice.mic_node_id.clone();

    // WEFT-616 Phase 2: snapshot the per-turn COW memory config; wired
    // onto the agent loop after `build_daemon_agent_loop` (late set —
    // the operator's loaded AgentsConfig is only in scope here).
    let cow_memory_cfg = config.agents.cow_memory.clone();

    // WEFT-83: snapshot agent.workspace_root before `config` moves into
    // Kernel::boot. When set, identity + tool workspace resolve from the
    // configured root rather than process CWD (systemd / multi-workspace).
    let agent_workspace_root = config.agents.workspace_root.clone();

    // WEFT-494 / ADR-070: snapshot MCP server configs before boot so the
    // live McpServerManager registry can be seeded after Kernel::boot.
    let mcp_servers_seed = config.tools.mcp_servers.clone();

    // Boot kernel
    let platform = NativePlatform::new();
    // The node key is loaded before boot so the kernel derives its mesh /
    // cluster node id from it (ADR-103 D11) instead of a per-boot UUID.
    let runtime_dir = paths.root().to_path_buf();
    // ADR-103 D14: decide the bound project once, from the paths this
    // daemon booted with; the handshake and the envelope gate read it.
    crate::handshake_rpc::init_bound(
        &paths,
        clawft_types::runtime_paths::home_dir()
            .map(|h| clawft_rpc::resolve::manifests_dir(&h))
            .as_deref(),
    );
    // ADR-103 D12: the scope gate verifies claimed projects against the
    // same manifest registry (`None` falls back to `~/.weftos/projects`).
    // Only the `--profile user` daemon defaults to `read_only`; every other
    // root keeps `allow_all` until migration (ADR-103 D12 amendment A6).
    crate::scope_gate::init(
        clawft_types::runtime_paths::home_dir().map(|h| clawft_rpc::resolve::manifests_dir(&h)),
        clawft_types::runtime_paths::user_profile_active(),
    );
    // A project kernel boots with its project key (node key == chain key),
    // already loaded and certified by `pre_boot`; it never talks to the
    // machine mesh service (its mesh is off, ADR-103 A7), so the P3-U mesh
    // decision below is skipped for it and `<runtime>/node.key` is not read.
    // Otherwise (ADR-103 P3-U) decide the mesh mode first: in service mode
    // the node id is the machine mesh service's and `node.key` is never read.
    #[cfg(all(unix, feature = "mesh"))]
    let (daemon_identity, mesh_link) = match pre_boot.take_identity() {
        Some(identity) => {
            crate::mesh_state::global().set(crate::mesh_state::plain("off"));
            (identity, None)
        }
        None => {
            let b = crate::mesh_boot::prepare(&kernel_config, &runtime_dir).await?;
            (b.identity, b.link)
        }
    };
    #[cfg(not(all(unix, feature = "mesh")))]
    let daemon_identity = match pre_boot.take_identity() {
        Some(identity) => identity,
        None => crate::node_identity::load_or_generate(&runtime_dir)
            .map_err(|e| anyhow::anyhow!("daemon identity bootstrap: {e}"))?,
    };
    // ADR-103 A6 (package I): a project-bound kernel attributes every
    // governance request to its own bound project.
    crate::caller_principal::attest_instance(&daemon_identity.node_id);
    let dashboard_gateway = config
        .gateway
        .api_enabled
        .then(|| format!("http://{}:{}", config.gateway.host, config.gateway.api_port));
    let kernel =
        boot_kernel_with_identity(config, kernel_config, Arc::new(platform), &daemon_identity)
            .await?;
    // The one-shot --new-chain / --adopt-legacy-chain requests have been
    // consumed by boot.
    clawft_kernel::chain_storage::request_new_chain(false);
    clawft_kernel::chain_storage::request_adopt_legacy_chain(false);
    crate::project_hooks::post_boot(&kernel, &pre_boot)?;
    let kernel = Arc::new(tokio::sync::RwLock::new(kernel));
    #[cfg(all(unix, feature = "exochain"))]
    crate::nested_rpc::init(nested_master, &kernel).await?;
    // Record this process as the live daemon only now that boot (which takes
    // the chain lock) has succeeded, so a refused boot leaves no stale pid.
    let _ = crate::instance_lock::write_pid_file(&protocol::pid_path(), std::process::id());
    // Boot succeeded. A launchd-started instance lifts the "do not restart"
    // sentinel (from an earlier refusal or clean exit) so launchd supervises
    // this run; a manual start leaves it, or launchd would launch its own,
    // refused, instance beside this one every throttle interval.
    if crate::boot_refusal::lift_refused_on_boot(&paths.refused(), crate::boot_refusal::service_managed())
        == crate::boot_refusal::RefusedLift::LeftParked
    {
        tracing::warn!("{}", crate::boot_refusal::parked_hint(&paths.refused()));
    }
    if crate::user_daemon::is_active() {
        seed_user_projects();
        crate::anchor_rpc::reconcile_startup(&kernel).await;
    }
    // Daemon-native dashboard reporter (`[dashboard]` in weave.toml; off by default).
    if crate::user_daemon::is_active()
        && let Some(home) = clawft_types::runtime_paths::home_dir()
    {
        crate::dashboard_rpc::start(&kernel, &home, dashboard_gateway).await;
    }
    // ADR-116 R1: the tailnet router (`[router]` in weave.toml; off by default).
    if crate::user_daemon::is_active()
        && let Some(home) = clawft_types::runtime_paths::home_dir()
    {
        crate::router_state::start(&home).await;
    }
    // ADR-103 P3-U: start the service link now that the router and gate exist.
    #[cfg(all(unix, feature = "mesh"))]
    let mesh_handle = match mesh_link {
        Some(link) => Some(crate::mesh_boot::start_link(link, &kernel).await),
        None => None,
    };

    // WEFT-494: seed live MCP registry + remember best-effort config path
    // for path-less mcp.reload (CLI after weft mcp add).
    crate::mcp_rpc::seed_from_config_servers(&mcp_servers_seed).await;
    if let Some(path) = crate::mcp_rpc::resolve_reload_path(None) {
        crate::mcp_rpc::set_mcp_config_path(path);
    }

    // Bootstrap daemon node identity. Loads `<runtime>/node.key`
    // (generates on first run, persists with 0600). Registers the
    // daemon's pubkey with the kernel's NodeRegistry so the substrate
    // publish gate can verify signatures and enforce the
    // `substrate/<node-id>/...` write prefix.
    // mesh-placement-06: persisted node-local workload catalog.
    #[cfg(feature = "exochain")]
    crate::workload_rpc::init_registry(&paths.workloads());
    // mesh-placement-12: the placement control plane signs with the node key;
    // in service mode the box key belongs to the mesh service, so placement and
    // the licence steward sign with a daemon-local control key instead
    // (ADR-106 phase 3, `placement_boot`).
    crate::fleet_labels::init(&runtime_dir);
    #[cfg(all(feature = "placement", unix))]
    crate::placement_boot::start(&kernel, &daemon_identity, &runtime_dir).await;
    // mesh-placement-03: probe, sign and cache this node's facts. In service
    // mode the service signs and advertises the machine's facts.
    #[cfg(any(feature = "mesh", feature = "exochain"))]
    match daemon_identity.signing_key() {
        Ok(key) => crate::node_facts_rpc::init(
            key.clone(),
            runtime_dir.clone(),
            kernel.read().await.cluster_membership().clone(),
            kernel.read().await.a2a_router().mesh_runtime().cloned(),
            Some(kernel.read().await.revocation_list().clone()),
        ),
        Err(e) => warn!(error = %e, "local node facts disabled (the mesh service advertises them)"),
    }
    // mesh-placement-12: serve this node's workload-host to the controllers
    // named in <runtime>/workload-host.json (off when the file is absent).
    #[cfg(all(feature = "placement", unix))]
    {
        let k = kernel.clone();
        tokio::spawn(async move {
            match crate::workload_place_rpc::start_serving(k).await {
                Ok(Some(addr)) => info!(%addr, "workload-host serving"),
                Ok(None) => {}
                Err(e) => error!(error = %e, "workload-host not served"),
            }
        });
    }
    // mesh-placement-19: inference placement (loopback proxy per role, the
    // adapters, the mesh hub). Off unless <runtime>/inference.json exists.
    #[cfg(all(feature = "placement", unix))]
    {
        let (audit, mesh) = {
            let k = kernel.read().await;
            let audit = k.chain_manager().cloned().map(|c| {
                Arc::new(clawft_kernel::infer_proxy::ChainAudit(c)) as Arc<dyn clawft_kernel::infer_proxy::ProxyAudit>
            });
            (audit, k.a2a_router().mesh_runtime().cloned())
        };
        // Managed roles and listening beyond loopback are governed: they need
        // the workload gate (which needs the chain). Without it they stay off.
        let (gate, chain) = {
            let chain = kernel.read().await.chain_manager().cloned();
            let gate = match crate::workload_gate::from_kernel(&kernel, &runtime_dir).await {
                Ok(g) => Some(g),
                Err(e) => {
                    warn!(error = %e, "inference: no workload gate, managed roles stay off");
                    None
                }
            };
            (gate, chain)
        };
        // ADR-120: the exposure registry exists before the inference table that registers into it.
        {
            let audit_fn: Option<clawft_kernel::exposure::AuditFn> = audit.clone().map(|a| {
                Arc::new(move |k: &str, v: serde_json::Value| a.record(k, v)) as clawft_kernel::exposure::AuditFn
            });
            let home = clawft_types::runtime_paths::home_dir();
            crate::expose_rt::start(&daemon_identity.node_id, &runtime_dir, home.as_deref(), audit_fn).await;
        }
        let parts = crate::infer_wire::InitParts {
            dir: &runtime_dir,
            node_id: daemon_identity.node_id.clone(),
            mesh,
            service_mode: daemon_identity.is_service(),
            audit,
            limits: Default::default(),
            gate,
            chain,
        };
        match crate::infer_wire::init(parts).await {
            Ok(Some(st)) => {
                // The kernel `llm` service follows the placed role.
                let mut rx = st.table_changes();
                let k = kernel.clone();
                tokio::spawn(async move {
                    while rx.changed().await.is_ok() {
                        refresh_llm_if_placement_changed(&k).await;
                    }
                });
            }
            Ok(None) => {}
            Err(e) => error!(error = %e, "inference placement not started"),
        }
    }
    {
        let k = kernel.read().await;
        let pubkey: [u8; 32] = daemon_identity.public_key();
        // In service mode `pubkey` is the machine's: this process cannot sign
        // for it, so the entry says it is attested by the service.
        let label = if daemon_identity.is_service() { "daemon (service-attested)" } else { "daemon" };
        k.node_registry().register(pubkey, Some(label.to_string()));
        info!(node_id = %daemon_identity.node_id, "daemon node registered");
        k.event_log().info(
            "node",
            format!("daemon node registered: {}", daemon_identity.node_id),
        );

        // Issue mesh-canonical write grants for every topic this
        // daemon will produce. Per `.planning/sensors/PIPELINE-PRIMITIVE-JOURNAL.md`
        // §R3.6 each `_derived/<topic>` subtree requires a separate
        // grant — this is the seam where they're stamped. MVP: the
        // daemon grants itself in-process; no signature, no
        // revocation. Future federated grants will require an issuer
        // signature checked at the RPC boundary.
        //
        // Topics included now (some are produced by services landing
        // in parallel work):
        // - `transcript`    — whisper STT output (this branch)
        // - `classify`      — speech/silence/keyword classifier (parallel)
        // - `terminal`      — terminal-output capture pipeline (parallel)
        // - `chat`          — agent.chat / agent.chat_stream per-turn
        //                     JSONL + heartbeat + progressive stream
        //                     (`derived/chat/<conv>/turns/<ulid>`,
        //                      `derived/chat/<conv>/status`,
        //                      `derived/chat/<conv>/stream` WEFT-253);
        //                     plan `agent-core-v1.md` Phase A2.
        // - `soul_journal`  — per-agent identity-drift observations
        //                     destined for `.clawft/SOUL.journal.md`.
        //                     The agent writes through the substrate,
        //                     not direct to disk; F2's `weaver soul
        //                     promote` reads back, diffs, and applies
        //                     to SOUL.md on confirmation. Plan
        //                     `agent-core-v1.md` Phase F1/F2.
        // - `agent`         — agent-core observability under
        //                     `derived/agent/...` (WEFT-335 router
        //                     decisions at `agent/routing/recent/<ulid>`;
        //                     future identity projection may share this
        //                     topic). TopicPrefix covers the subtree.
        //
        // Stamping all six here means the parallel branches don't
        // each need to touch this grant-issue path.
        for topic in [
            "transcript",
            "classify",
            "terminal",
            "chat",
            "soul_journal",
            "agent",
        ] {
            match k.node_registry().issue_derived_grant(
                daemon_identity.node_id.clone(),
                topic,
                clawft_kernel::GrantScope::TopicPrefix,
            ) {
                Ok(_) => {
                    info!(
                        node_id = %daemon_identity.node_id,
                        topic = %topic,
                        "derived-write grant issued"
                    );
                    k.event_log().info(
                        "node",
                        format!(
                            "derived-write grant: node={} topic={}",
                            daemon_identity.node_id, topic
                        ),
                    );
                }
                Err(e) => {
                    // Topic strings above are constants so this can't
                    // realistically fire — log + continue rather than
                    // abort daemon boot.
                    warn!(error = %e, topic = %topic, "derived-write grant issue failed");
                }
            }
        }
    }

    // Stash daemon-wide control state. Set once; the `control.*`
    // RPC handlers and the service-spawn path read it. Idempotent
    // re-set is impossible (OnceLock) — this must run exactly once
    // per daemon process.
    let control_flags = ControlFlags::new();
    {
        let _ = DAEMON_CONTROL.set(Arc::new(DaemonControlState {
            daemon_node_id: daemon_identity.node_id.clone(),
            flags: control_flags.clone(),
        }));
    }

    // Whisper STT + audio classifier. The mic source is chosen by
    // `crate::mic_source::supervise`: the operator pin (`WHISPER_INPUT_NODE_ID`
    // or `voice.mic_node_id`) wins; otherwise only a unique `sensor/mic`
    // publisher is auto-selected; several candidates are refused with a
    // warning. There is no hard-coded node id.
    let (mic_node_tx, mic_node_rx) = watch::channel(None::<String>);
    // A project kernel runs no voice pipeline (the user daemon does); with the
    // sender dropped the receiver just keeps its `None`.
    if crate::project_profile::mic_supervisor_enabled(crate::project_profile::is_project_profile()) {
        tokio::spawn(crate::mic_source::supervise(
            crate::mic_source::pin_from(voice_mic_pin.as_deref()),
            kernel.clone(),
            control_flags.clone(),
            daemon_identity.node_id.clone(),
            mic_node_tx,
        ));
    }

    // Spawn the LLM service handle. Unlike whisper this is a
    // request/response client — there's no background tokio task to
    // hold open, so we just construct the client (and one-shot
    // health probe in the background so a cold cache logs cleanly
    // without blocking boot).
    //
    // Pre-register the control flag so `control.set_enabled
    // {kind:"service", target:"llm"}` works the first time the
    // user toggles it from the GUI.
    let _llm_service_flag = control_flags.register(ControlKind::Service, "llm", true);
    {
        // LLM endpoint resolution — shared with the WEFT-343 refresh
        // path (`refresh_daemon_llm_from_env` / `llm_service::resolve_llm_endpoint`).
        //
        // Precedence (first hit wins):
        //   1. LLM_SERVICE_URL env — one-off override / experiments.
        //      Note: `main.rs` calls `dotenvy::dotenv()` early, so any
        //      `LLM_SERVICE_URL=` line in `./env` ALSO gets picked up
        //      here and silently shadows `[kernel.llm]` below.
        //   2. [kernel.llm].service_url in config — durable operator
        //      choice.
        //   3. OPENROUTER_API_KEY set AND no URL above — opt-in
        //      OpenRouter takeover (bearer auth + OpenRouter defaults).
        //   4. ADR-060 local Hermes defaults (127.0.0.1:8090, hermes-4.3-36b).
        let cfg_llm = {
            let k = kernel.read().await;
            k.kernel_config().llm.clone()
        };
        let resolved = crate::llm_service::resolve_llm_endpoint(cfg_llm.as_ref());
        let llm_url = resolved.config.base_url.clone();
        let llm_model = resolved.config.model.clone();
        let using_openrouter = resolved.using_openrouter;
        let url_source = resolved.url_source;
        let model_source = resolved.model_source;

        info!(
            url = %llm_url,
            url_source,
            model = %llm_model,
            model_source,
            openrouter = using_openrouter,
            "llm endpoint resolved"
        );
        // Warn when env (possibly from `./env` via dotenvy) shadows an
        // explicit `[kernel.llm]` value — intended precedence, but a
        // silent shadow was a recurring "panel keeps hitting OpenRouter"
        // footgun.
        if url_source == "env:LLM_SERVICE_URL"
            && cfg_llm
                .as_ref()
                .and_then(|c| c.service_url.as_ref())
                .is_some_and(|s| !s.is_empty())
        {
            warn!(
                env_url = %llm_url,
                "LLM_SERVICE_URL (possibly from ./env) shadows \
                 [kernel.llm].service_url — the env value wins; unset it \
                 to use the durable config value"
            );
        }
        if model_source == "env:LLM_MODEL"
            && cfg_llm
                .as_ref()
                .and_then(|c| c.model.as_ref())
                .is_some_and(|s| !s.is_empty())
        {
            warn!(
                env_model = %llm_model,
                "LLM_MODEL (possibly from ./env) shadows [kernel.llm].model \
                 — the env value wins; unset it to use the durable config value"
            );
        }

        match crate::llm_service::new_client(resolved.config) {
            Ok(client) => {
                // WEFT-343: shared swappable handle — outer Arc is
                // stable for the process lifetime; inner client can
                // be replaced on control.set_enabled refresh.
                let shared = clawft_service_llm::share_llm_client(client);
                // Background health probe — only meaningful for
                // local llama-server; OpenRouter has no `/health`
                // endpoint and would always fail the probe, so we
                // skip it when openrouter takeover is active.
                if !using_openrouter {
                    let probe = Arc::clone(&shared);
                    tokio::spawn(async move {
                        let guard = probe.read().await;
                        if guard.wait_for_healthy().await {
                            info!(
                                url = %guard.config().base_url,
                                "llm service: healthy"
                            );
                        } else {
                            warn!(
                                url = %guard.config().base_url,
                                "llm service: health probe failed at boot \
                                 (RPC will return a clean error per call)"
                            );
                        }
                    });
                }
                let _ = DAEMON_LLM.set(Arc::clone(&shared));

                // Register the LLM as a first-class kernel service so
                // it shows up in `kernel.services`, in the desktop
                // Services panel, and answers to
                // `service.{start,stop,restart}`. The same
                // `_llm_service_flag` we registered above is the
                // backing toggle — `service.stop` flips it off, after
                // which `handle_llm_prompt` returns the standard
                // disabled error. Registration also publishes a
                // `service.contract.register` chain event listing the
                // exposed methods, mirroring the governance treatment
                // every other kernel service gets at boot.
                {
                    let svc = Arc::new(crate::llm_service::LlmSystemService::new(
                        Arc::clone(&shared),
                        Arc::clone(&_llm_service_flag),
                    ));
                    let k = kernel.read().await;
                    let methods: Vec<String> = crate::llm_service::LLM_CONTRACT_METHODS
                        .iter()
                        .map(|s| (*s).to_string())
                        .collect();
                    let registered = {
                        #[cfg(feature = "exochain")]
                        {
                            match k.chain_manager() {
                                Some(cm) => {
                                    k.services()
                                        .register_with_contract(svc.clone(), methods, cm)
                                }
                                None => k.services().register(svc.clone()),
                            }
                        }
                        #[cfg(not(feature = "exochain"))]
                        {
                            let _ = methods;
                            k.services().register(svc.clone())
                        }
                    };
                    match registered {
                        Ok(()) => {
                            // Record a metadata entry too so callers
                            // resolving by name can see the upstream
                            // URL without poking at LlmClient internals.
                            let entry = clawft_kernel::ServiceEntry {
                                name: "llm".to_string(),
                                owner_pid: None,
                                endpoint: clawft_kernel::ServiceEndpoint::External {
                                    url: llm_url.clone(),
                                },
                                audit_level: clawft_kernel::ServiceAuditLevel::Full,
                                registered_at: chrono::Utc::now(),
                            };
                            if let Err(e) = k.services().register_entry(entry) {
                                warn!(
                                    error = %e,
                                    "llm service entry registration failed (the SystemService is still live)"
                                );
                            }
                        }
                        Err(e) => warn!(
                            error = %e,
                            "llm service registration failed — llm.prompt still works, but it won't appear in kernel.services"
                        ),
                    }
                }

                info!(
                    url = %llm_url,
                    model = %llm_model,
                    openrouter = using_openrouter,
                    "llm service handle wired",
                );
            }
            Err(e) => {
                warn!(
                    error = %e,
                    "llm client init failed — llm.prompt RPC will \
                     return 'service unavailable'"
                );
            }
        }
    }

    // agent-core-v1 Phase C2 / D3: wire
    // `clawft-service-agent::AgentService` into the daemon as a
    // long-lived handle. Mirrors the whisper/llm/terminal pattern.
    //
    // After D3 the dispatch arm at `agent.chat` calls into this
    // service unconditionally — the C2 spike fallback (`handle_agent_chat`)
    // is gone. If the LLM client failed to init, `DAEMON_AGENT`
    // stays `None` and the dispatch arm surfaces a typed error
    // ("agent service not wired") instead of falling back.
    //
    // Pre-register the control flag so
    // `control.set_enabled {kind:"service", target:"agent"}` works the
    // first time the user toggles it.
    let _agent_service_flag = control_flags.register(ControlKind::Service, "agent", true);
    if let Some(llm_for_agent) = DAEMON_LLM.get().cloned() {
        // WEFT-83: prefer `agents.workspace_root` when configured; fall
        // back to process CWD for back-compat (pre-config-key behaviour).
        let workspace = {
            let agents = clawft_types::config::AgentsConfig {
                workspace_root: agent_workspace_root,
                ..Default::default()
            };
            match agents.resolve_workspace_root() {
                Ok(p) => {
                    if agents.workspace_root.is_some() {
                        info!(
                            workspace = %p.display(),
                            "agent service: using agents.workspace_root"
                        );
                    }
                    p
                }
                Err(e) => {
                    warn!(error = %e, "agent service: workspace root unavailable, skipping wiring");
                    return Err(anyhow::anyhow!(
                        "agent service: workspace root unavailable: {e}"
                    ));
                }
            }
        };

        // WEFT-95: identity reads go through Platform::fs (NativePlatform).
        let identity_platform = Arc::new(NativePlatform::new());
        let identity_loader = Arc::new(clawft_core::agent::identity::IdentityLoader::new(
            &workspace,
            Arc::clone(&identity_platform),
        ));

        // M4 Phase C.1: construct the subagent spawn substrate BEFORE
        // register_all so the four spawn tools (agent_spawn / task_status /
        // task_result / agent_message) register against a live backend. The
        // service back-reference is late-wired below once the AgentService
        // `Arc` exists (Arc-cycle break, design D2 — same OnceLock shape as
        // DAEMON_AGENT). Caps are seeded from `[kernel.agent.subagents]` (D.1);
        // absent ⇒ the spawner defaults (enabled, 5 concurrent / depth 3 / 120s).
        let spawn_registry = Arc::new(clawft_service_agent::SpawnRegistry::new());
        let subagent_spawner: Arc<
            clawft_service_agent::DaemonSubagentSpawner<
                clawft_core::agent::loop_core::AgentLoop<NativePlatform>,
            >,
        > = {
            let k = kernel.read().await;
            // D.1: map the operator config block 1:1 onto the spawner's caps.
            let subagent_config = match k.kernel_config().agent.as_ref() {
                Some(a) => clawft_service_agent::SubagentConfig {
                    enabled: a.subagents.enabled,
                    max_per_conv: a.subagents.max_per_conv,
                    max_depth: a.subagents.max_depth,
                    // config owns the seconds→Duration conversion (D.1 helper)
                    timeout: a.subagents.timeout(),
                },
                None => clawft_service_agent::SubagentConfig::default(),
            };
            let mut sp = clawft_service_agent::DaemonSubagentSpawner::new(
                Arc::clone(&spawn_registry),
                subagent_config,
            );
            // D6 witnessing: record spawn lifecycle (spawn/complete/fail/cancel)
            // on the chain when it exists (ADR-033).
            if let Some(chain) = k.chain_manager().cloned() {
                sp = sp.with_chain(chain);
            }
            // D3 forest: draw cross-conv spawn edges into the SAME kernel-global
            // causal graph + cross-ref store the SessionTier dual-writes turns
            // into (they are the kernel's single shared instances), so the
            // ADR-067 graph view renders the spawn tree with no extra work.
            if let (Some(causal), Some(crossrefs)) =
                (k.ecc_causal().cloned(), k.ecc_crossrefs().cloned())
            {
                sp = sp.with_forest(clawft_service_agent::SubagentForest { causal, crossrefs });
            }
            Arc::new(sp)
        };

        let mut tool_registry = clawft_core::tools::registry::ToolRegistry::new();
        clawft_tools::register_all(
            &mut tool_registry,
            Arc::new(NativePlatform::new()),
            workspace.clone(),
            clawft_tools::security_policy::CommandPolicy::safe_defaults(),
            clawft_tools::url_safety::UrlPolicy::default(),
            clawft_tools::web_search::WebSearchConfig::default(),
            // M4 Phase C.1: the four subagent tools register against this
            // spawner. `None` on the in-process CLI path leaves them unregistered.
            Some(Arc::clone(&subagent_spawner)
                as Arc<dyn clawft_core::agent::spawn::SubagentSpawner>),
        );
        // WEFT-349: cross-agent delegation via `delegate_task` + agents/
        // specialist profiles. Spawner lets chat spawn specialists on the
        // local loop; Claude is optional (env / config key).
        {
            use clawft_tools::delegate_tool::{RegisterDelegateOpts, register_delegate_task};
            let user_agents = dirs::home_dir().map(|h| h.join(".clawft").join("agents"));
            let workspace_agents = {
                let clawft = workspace.join(".clawft").join("agents");
                if clawft.is_dir() {
                    Some(clawft)
                } else {
                    let top = workspace.join("agents");
                    if top.is_dir() {
                        Some(top)
                    } else {
                        None
                    }
                }
            };
            let del_cfg = clawft_types::delegation::DelegationConfig::default();
            register_delegate_task(
                &del_cfg,
                &mut tool_registry,
                RegisterDelegateOpts {
                    workspace_agents,
                    user_agents,
                    spawner: Some(
                        Arc::clone(&subagent_spawner)
                            as Arc<dyn clawft_core::agent::spawn::SubagentSpawner>,
                    ),
                    config_api_key: std::env::var("ANTHROPIC_API_KEY").ok().filter(|k| !k.is_empty()),
                },
            );
        }
        // Project profile: no voice tools (their default STT/TTS endpoints and
        // cloud fallbacks belong to the user daemon).
        let tool_registry = Arc::new(crate::project_profile::strip_voice_tools(tool_registry));
        // Stash the registry + spawner so the lifecycle paths (idle reaper,
        // agent.chat.end, agent.chat.cancel) can cascade-cancel a parent's
        // still-running children when the parent conversation ends (design D5).
        let _ = DAEMON_SPAWN_REGISTRY.set(Arc::clone(&spawn_registry));
        // ADR-121 O2: the node's instance registry follows the spawn registry and chains
        // starts, stops and failures on the daemon's chain.
        {
            let chain = kernel.read().await.chain_manager().cloned();
            let reg = crate::instance_registry::InstanceRegistry::new(chain);
            reg.add_source(Arc::new(crate::instance_sources::SubagentSource { registry: Arc::clone(&spawn_registry) }));
            if crate::instance_registry::install_global(Arc::clone(&reg)) {
                reg.spawn_sync_loop(std::time::Duration::from_secs(15));
            }
        }
        let _ = DAEMON_SUBAGENT_SPAWNER.set(
            Arc::clone(&subagent_spawner) as Arc<dyn clawft_core::agent::spawn::SubagentSpawner>,
        );

        // agent-core-v1 Phase C3: wire the substrate-backed
        // `ConversationSink` so per-turn JSONL lands at
        // `substrate/_derived/chat/<conv>/turns/<ulid>` and the
        // per-conv heartbeat publishes
        // `substrate/_derived/chat/<conv>/status`. The `chat`
        // derived-write grant issued at boot covers both paths.
        // ADR-058 Phase 5.1: alongside the sink we build the per-conversation
        // L2 session tier and surface it so the agent loop can graft from it.
        // M2 D7: hoist the anchor config so the post-block `talk_loop`
        // validation warning (which runs after the kernel read guard is
        // dropped) can consult it without re-locking.
        let anchor_cfg = {
            let k = kernel.read().await;
            k.kernel_config().agent.clone().unwrap_or_default()
        };
        // WEFT-654 D10: snapshot the proposal timeout for the M2 reaper
        // (out of `anchor_cfg`'s scope by the time the reaper spawns).
        let _ = DAEMON_PROPOSAL_TIMEOUT.set(std::time::Duration::from_secs(
            anchor_cfg.proposal.timeout_secs.unwrap_or(600),
        ));
        // WEFT-655 Gap B: same reasoning, for whether review mode is on —
        // the standalone timeout-sweep spawn point can't read `anchor_cfg`
        // directly either.
        let _ = DAEMON_PROPOSAL_REVIEW_MODE
            .set(anchor_cfg.proposal.mode == clawft_types::config::ProposalMode::Review);
        let (agent_sink, session_tier): (
            Arc<dyn clawft_core::agent::sink::ConversationSink>,
            Option<Arc<clawft_service_agent::SessionTier>>,
        ) = {
            let k = kernel.read().await;
            let substrate = k.substrate_service().clone();
            let node_registry = k.node_registry().clone();

            // [kernel.agent].anchor_* — when any flag is on, mirror
            // every successful turn into the witness chain / HNSW
            // index / causal graph so the Explorer KPIs and the
            // chain seq move with chat traffic. Each side-effect is
            // gated independently. With every flag off (default) the
            // sink falls back to the no-op anchor — pure substrate
            // JSONL behaviour, no extra work per turn.
            // `anchor_cfg` is hoisted above the block (M2 D7); reuse it here.
            let mut session_tier: Option<Arc<clawft_service_agent::SessionTier>> = None;
            let anchor: Arc<dyn clawft_service_agent::TurnAnchor> = if anchor_cfg.any_enabled() {
                let chain = anchor_cfg
                    .anchor_chain
                    .then(|| k.chain_manager().cloned())
                    .flatten();
                let causal = anchor_cfg
                    .anchor_causal
                    .then(|| k.ecc_causal().cloned())
                    .flatten();
                let crossrefs = anchor_cfg
                    .anchor_causal
                    .then(|| k.ecc_crossrefs().cloned())
                    .flatten();
                // ADR-062 §1.1: when both the witness chain and the causal graph
                // are enabled, the L2 tier owns the forest dual-write (causal
                // node + `Follows` lineage + speaker cross-ref, keyed by chain
                // sequence). Hand the anchor `None` for causal in that case so a
                // turn isn't double-added; the anchor keeps its own causal write
                // only in the legacy causal-without-chain (no-tier) configuration.
                let tier_owns_forest = chain.is_some() && causal.is_some() && crossrefs.is_some();
                let anchor_causal = if tier_owns_forest {
                    None
                } else {
                    causal.clone()
                };
                info!(
                    chain = chain.is_some(),
                    causal = causal.is_some(),
                    forest_join = tier_owns_forest,
                    "agent.chat anchors wired (mirrors turns to enabled stores)"
                );
                let mut anchor =
                    clawft_service_agent::KernelTurnAnchor::new(chain.clone(), anchor_causal);
                // ADR-069 / WEFT-641: atom primary index (Panopticon). Wired
                // whenever the chain is present (mint needs event.sequence).
                // Fire-and-forget at the anchor; RPC surface reads the same Arc.
                if chain.is_some() {
                    let registry = clawft_service_agent::AtomRegistry::shared();
                    anchor = anchor.with_atom_registry(registry.clone());
                    if DAEMON_ATOM_REGISTRY.set(registry).is_err() {
                        warn!("atom registry already set — reusing prior instance");
                    } else {
                        info!("ADR-069 atom registry wired (atom.locate / atom.audit)");
                    }
                }
                // ADR-058 Phase 5.1: the L2 tier needs the witness chain (its
                // sequence is the index key), so it lives only when chain
                // anchoring is enabled. The embedder is the ADR-059 Qwen3
                // provider when its weights are present, else the Mock fallback.
                if let Some(chain) = chain {
                    let embedder: Arc<dyn clawft_kernel::embedding::EmbeddingProvider> =
                        match crate::project_profile::project_embedding_provider().await {
                            // Project profile: remote only; never load a local model.
                            Some(remote) => remote,
                            None => Arc::from(clawft_kernel::embedding::select_embedding_provider(None)),
                        };
                    let mut tier = clawft_service_agent::SessionTier::new(embedder, chain, None);
                    // ADR-062 §1.1 forest join: dual-write turns into the
                    // kernel-global causal graph + cross-ref store and fuse
                    // lineage/cross-structure recall with cosine on graft.
                    if let (Some(causal), Some(crossrefs)) = (causal.clone(), crossrefs.clone()) {
                        tier = tier.with_forest(causal, crossrefs);
                    }
                    // ADR-067 P2 (classification-design §D6): attach the keyword
                    // turn classifier when `[kernel.agent.classification]` is on
                    // (mode != off), so every committed turn node carries a
                    // 4-axis `classification` blob + its text. Default off ⇒ no
                    // classifier, no per-turn cost. The classifier only does
                    // anything alongside the forest join above (it feeds
                    // `dual_write_turn`), so it is inert without `anchor_causal`.
                    if anchor_cfg.classification.is_enabled() {
                        tier = tier.with_classifier(Arc::new(
                            clawft_service_agent::KeywordTurnClassifier::new(),
                        ));
                        info!(
                            mode = ?anchor_cfg.classification.mode,
                            "turn classification enabled — committed turn nodes carry a \
                             4-axis classification blob (keyword tier)"
                        );
                    } else if anchor_cfg.talk_loop {
                        // Operator hint (design §D6): the ECC loop is on but the
                        // graph view is inert without classification.
                        info!(
                            "classification.mode=off while talk_loop is on — committed turn \
                             nodes carry no classification blob and the conversation.graph view \
                             stays inert; set [kernel.agent.classification] mode = \"keyword\" \
                             to populate it"
                        );
                    }
                    // Phase B (classification-design §D4): `mode = full` adds the
                    // async LLM enrichment tier on top of the keyword sync tier
                    // wired above. Stand up the bounded drop-oldest enrich queue,
                    // attach it to the tier (so `index_turn` enqueues committed
                    // turns), and build the cheap-model `EnrichmentClassifier`;
                    // both are stashed for the drain task spawned in the run loop.
                    // The queue-full path drops oldest, so the enqueue never
                    // stalls a turn. Requires an LLM backend — `full` without one
                    // warns and stays keyword-only (no queue ⇒ no enqueue).
                    if anchor_cfg.classification.is_full() {
                        match daemon_llm() {
                            Some(llm) => {
                                let queue =
                                    Arc::new(clawft_service_agent::EnrichQueue::new(
                                        anchor_cfg.classification.queue_bound,
                                    ));
                                tier = tier.with_enrich_queue(queue.clone());
                                let backend: Arc<
                                    dyn clawft_core::agent::context_router::Classifier,
                                > = llm;
                                let enricher = Arc::new(
                                    clawft_service_agent::EnrichmentClassifier::from_config(
                                        backend,
                                        &anchor_cfg.classification,
                                    ),
                                );
                                let _ = DAEMON_ENRICH_QUEUE.set(queue);
                                let _ = DAEMON_ENRICH_CLASSIFIER.set(enricher);
                                info!(
                                    queue_bound = anchor_cfg.classification.queue_bound,
                                    "classification.mode=full — async LLM enrichment queue \
                                     wired; committed turns refine off the turn path (tier=llm)"
                                );
                            }
                            None => {
                                warn!(
                                    "classification.mode=full but no LLM backend (DAEMON_LLM \
                                     unset) — enrichment disabled; turns keep the keyword blob"
                                );
                            }
                        }
                    }
                    let tier = Arc::new(tier);
                    // ADR-058 Phase 5.3: pre-warm the embedder at startup (one
                    // throwaway embed) so the first conversation's first graft
                    // doesn't pay the model's first-inference cost — mirrors the
                    // 6.2 STT pre-warm. Best-effort; cheap on the Mock fallback.
                    tier.warm().await;

                    // M2 D1/D6/D7: host the multiplexed TalkModeLoop when
                    // `talk_loop` is on AND the tier owns the forest (chain +
                    // causal + cross-refs). The loop is the sole consumer of
                    // `ecc_impulses`; each text turn's `EndOfUtterance` (emitted
                    // by `SessionTier::index_turn`) commits Frontier→Committed on
                    // the shared forest on the loop's next tick. The loop resolves
                    // per-conversation views through the tier itself
                    // (`ViewResolver`), so it commits the SAME view graft reads
                    // from (D4). Attached via `set_talk_loop` on the already-Arc'd
                    // tier to break the tier↔loop construction cycle (the loop
                    // needs the tier as its `Arc<dyn ViewResolver>`, so the tier
                    // must be Arc'd first).
                    if anchor_cfg.talk_loop && tier_owns_forest {
                        match (
                            k.ecc_impulses().cloned(),
                            k.ecc_tick().cloned(),
                            causal.clone(),
                            crossrefs.clone(),
                        ) {
                            (Some(impulses), Some(tick), Some(causal), Some(crossrefs)) => {
                                // Hand the loop a WEAK handle to the tier (P3's
                                // `weak_view_resolver`). The tier's OnceLock holds
                                // the loop strongly, so a strong resolver here would
                                // form a `tier ↔ loop` cycle that never drops; the
                                // Weak keeps loop→tier weak. The tier stays alive for
                                // the daemon's lifetime via the agent service's
                                // strong ref, so resolution always upgrades at
                                // runtime; if the tier is ever dropped the loop
                                // no-ops (D4 reaped-conversation contract).
                                let views =
                                    clawft_service_agent::SessionTier::weak_view_resolver(&tier);
                                let talk_loop = Arc::new(clawft_kernel::TalkModeLoop::new(
                                    impulses,
                                    causal,
                                    crossrefs,
                                    views,
                                    tick,
                                    clawft_kernel::TalkModeConfig::default(),
                                ));
                                tier.set_talk_loop(talk_loop.clone());
                                if let Err(e) = k.services().register(talk_loop.clone()) {
                                    warn!(error = %e, "M2: TalkModeLoop service registration failed");
                                }
                                let _ = DAEMON_TALK_LOOP.set(talk_loop);
                                info!(
                                    "M2: multiplexed TalkModeLoop hosted — text turns commit \
                                     Frontier→Committed on the shared forest"
                                );
                            }
                            _ => {
                                warn!(
                                    "M2: talk_loop=true but kernel ECC handles \
                                     (impulses/tick/causal/crossrefs) missing — talk_loop inert"
                                );
                            }
                        }
                    }

                    anchor = anchor.with_session_tier(tier.clone());
                    session_tier = Some(tier);
                    info!(
                        "agent.chat L2 session tier wired (index + graft active, embedder pre-warmed)"
                    );
                }
                Arc::new(anchor)
            } else {
                Arc::new(clawft_service_agent::NoopTurnAnchor)
            };

            let sink = Arc::new(
                clawft_service_agent::SubstrateConversationSink::with_anchor(
                    substrate,
                    node_registry,
                    daemon_identity.node_id.clone(),
                    anchor,
                ),
            );
            (sink, session_tier)
        };

        // M2 D7: if `talk_loop` was requested but never hosted (prerequisites
        // off, or the kernel's ECC subsystem is absent), say so once and carry
        // on with it inert — no partial wiring. Text turns still index and
        // graft; they simply stay `Frontier` (uncommitted-but-functional).
        if anchor_cfg.talk_loop && DAEMON_TALK_LOOP.get().is_none() {
            warn!(
                "M2: [kernel.agent].talk_loop=true but the loop was not hosted \
                 (requires anchor_chain + anchor_causal AND kernel ECC init); \
                 text turns will index but stay Frontier"
            );
        }
        // M2 D6: back the idle reaper's per-conversation last-activity map.
        let _ = DAEMON_CONV_ACTIVITY.set(Arc::new(dashmap::DashMap::new()));

        // Expose the sink to the `agent.turn.record` dispatch arm so
        // externally-produced turns (voice Talk-Mode) share the exact
        // substrate + anchor path as agent.chat turns.
        let _ = DAEMON_TURN_SINK.set(agent_sink.clone());

        // agent-core-v1 Phase D1 + WEFT-96: FileIdentityProvider loads
        // `.clawft/SOUL.md` + `.clawft/IDENTITY.md`; wrap with
        // JournalAwareIdentityProvider so pending substrate journal rows
        // are consulted (read-on-every-turn) without auto-promoting into
        // SOUL.md. Degrade-open on journal read errors so a substrate
        // blip never blocks chat.
        //
        // WEFT-329: start a notify watcher on `.clawft/` so identity
        // edits invalidate the cache; next turn re-reads. Always-on
        // when notify starts (cheap single-dir watch); fail-open to
        // default always-re-read if the watcher cannot be created.
        let identity_provider: Arc<dyn clawft_core::agent::identity::IdentityProvider> = {
            let file_provider = Arc::new(
                clawft_core::agent::identity::FileIdentityProvider::new(
                    &workspace,
                    Arc::clone(&identity_platform),
                ),
            );
            let watch_cfg =
                clawft_core::agent::identity_watcher::IdentityWatcherConfig::for_workspace(
                    &workspace,
                );
            match clawft_core::agent::identity_watcher::start_watching(
                watch_cfg,
                Arc::clone(&file_provider),
            ) {
                Ok(handle) => {
                    if DAEMON_IDENTITY_WATCHER.set(handle).is_ok() {
                        info!(
                            clawft = %workspace.join(".clawft").display(),
                            "agent-core: identity hot-reload watcher started (WEFT-329)"
                        );
                    } else {
                        warn!(
                            "agent-core: identity watcher handle already set; \
                             new handle dropped (watcher stopped)"
                        );
                    }
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        "agent-core: identity hot-reload watcher failed to start; \
                         FileIdentityProvider stays in always-re-read mode"
                    );
                }
            }
            let file_base: Arc<dyn clawft_core::agent::identity::IdentityProvider> =
                file_provider;
            let k = kernel.read().await;
            let journal_client = Arc::new(clawft_service_agent::KernelSubstrateClient::new(
                k.substrate_service().clone(),
                k.node_registry().clone(),
            ));
            let journal_reader: Arc<dyn clawft_core::agent::soul_journal::SoulJournalReader> =
                Arc::new(clawft_service_agent::SubstrateSoulJournalReader::new(
                    journal_client,
                ));
            info!(
                "agent-core: journal-aware identity provider attached \
                 (substrate soul_journal read-on-every-turn)"
            );
            Arc::new(clawft_core::agent::identity::JournalAwareIdentityProvider::new(
                file_base,
                journal_reader,
            ))
        };

        // agent-core-v1 Phase D2 / WEFT-332: register the boot-time
        // concierge principal in the kernel's AgentRegistry. Used as
        // the single-tenant fallback when `agent.chat` has no
        // `caller_id`. Multi-tenant callers register lazily via
        // `AgentService::with_caller_registry` under
        // `agent.chat:user:<caller_id>`. The agent reuses the
        // daemon's own pubkey (PoP is unnecessary for a
        // self-registration happening before the listener is up).
        let concierge_agent_id: String = {
            let k = kernel.read().await;
            // In service mode this is the machine's public key: the entry is
            // service-attested (this process cannot sign for that key). The
            // agent registry has no label field yet; tracked as a follow-up card.
            let pubkey: [u8; 32] = daemon_identity.public_key();
            let entry = k.agent_registry().register("concierge-bot".into(), pubkey);
            info!(
                agent_id = %entry.agent_id,
                name = %entry.name,
                "agent-core: concierge principal registered"
            );
            k.event_log().info(
                "agent",
                format!("concierge principal registered: {}", entry.agent_id),
            );
            entry.agent_id
        };
        let _ = DAEMON_CONCIERGE_AGENT_ID.set(concierge_agent_id.clone());

        // agent-core-v1 Phase D2: construct a GovernanceGate for the
        // chat path and wrap it in the `EffectGate` adapter so every
        // tool dispatch in `AgentLoop::run_tool_loop` produces an
        // audited Permit/Defer/Deny decision. Risk threshold 0.8
        // matches the daemon's existing per-spawn gate; chain logging
        // is wired when the exochain feature is on so witness chain
        // entries land alongside other governance events.
        let chat_gate: Arc<dyn clawft_core::agent::gate::EffectGate> = {
            use clawft_kernel::{
                GateBackend, GovernanceBranch, GovernanceGate, GovernanceRule, RuleSeverity,
            };
            let mut g = GovernanceGate::new(0.8, false)
                .add_rule(GovernanceRule {
                    id: "chat-tool-guard".into(),
                    description: "Block high-risk tool dispatches in agent.chat".into(),
                    branch: GovernanceBranch::Judicial,
                    severity: RuleSeverity::Blocking,
                    active: true,
                    reference_url: None,
                    sop_category: None,
                    rule_type: Default::default(),
                    action_selector: None,
                    tool_selector: None,
                    force_on_match: false,
                })
                // WEFT-342: hard-refuse on binding-thread mismatch each turn.
                .add_rule(clawft_kernel::governance::GovernanceRule::binding_thread(
                    "SOUL.md must contain the compile-time binding-thread excerpt",
                ));
            // D6: opt-in per-action grant so agent_spawn (~0.93 effect) clears
            // the 0.8 chat gate without globally loosening the threshold. Off
            // by default ([kernel.agent.subagents].governance_grant); the grant
            // is still evaluated and witnessed as a `governance.grant`.
            if anchor_cfg.subagents.governance_grant {
                g = g.exempt_action("tool.agent_spawn");
            }
            #[cfg(feature = "exochain")]
            {
                let k = kernel.read().await;
                if let Some(cm) = k.chain_manager() {
                    g = g.with_chain(cm.clone());
                }
            }
            // ADR-103 D8: a project kernel's tool gate is its effective
            // governance (signed parent policy + overlay), not the chat-only
            // rule set above, so an overlay deny also stops tool dispatch.
            #[cfg(feature = "exochain")]
            let overlay_gate = {
                let k = kernel.read().await;
                k.governance_overlay().and(k.governance_gate().cloned())
            };
            #[cfg(not(feature = "exochain"))]
            let overlay_gate: Option<Arc<dyn GateBackend>> = None;
            let backend: Arc<dyn GateBackend> = overlay_gate.unwrap_or_else(|| Arc::new(g));
            Arc::new(clawft_service_agent::KernelEffectGate::new(backend))
        };

        // agent-core-v1 Phase E1/E2: pick the ContextRouter implementation
        // from `config.routing.context_router`. v0 `"null"` (the
        // default) hands `None` to `build_daemon_agent_loop`, which
        // keeps `AgentLoop`'s built-in `NullRouter` live so behaviour
        // is bit-for-bit unchanged. v1 `"llm-classifier"` builds an
        // `LlmClassifierRouter` over the same `LlmClient` the daemon
        // already uses (so OpenRouter / local llama-server both keep
        // working without a second connection). v2 `"embedding"`
        // (Phase E2) builds an `EmbeddingRouter` over a
        // `ruvector-diskann@2.1` index of the discovered skill
        // descriptors. The embedder is `ApiEmbedder` (production —
        // OpenAI-compat `/embeddings`) when `OPENAI_API_KEY` is set,
        // else `HashEmbedder` as the deterministic offline floor.
        //
        // Per `docs/research/rvf-context-router.md`, the router NEVER
        // picks a model and NEVER escalates a tier — `TieredRouter`
        // owns the actual decision; the router only nudges
        // `complexity_hint` and surfaces an `archetype` label.
        let context_router: Option<Arc<dyn clawft_core::agent::context_router::ContextRouter>> =
            match context_router_choice.as_str() {
                "llm-classifier" => {
                    info!(
                        router = "llm-classifier",
                        "agent-core: ContextRouter v1 (LlmClassifierRouter) live"
                    );
                    Some(Arc::new(
                        clawft_core::agent::context_router::LlmClassifierRouter::new(
                            llm_for_agent.clone(),
                        ),
                    ))
                }
                "embedding" => {
                    // E2: build the v2 router via the shared helper. On
                    // any prerequisite failure (empty registry, discover
                    // error, embedder/index build error) the helper logs
                    // a `warn!` and returns None; we propagate that so the
                    // daemon falls back to the built-in NullRouter rather
                    // than crashing the boot path.
                    match build_embedding_router_or_warn(workspace.as_path()).await {
                        Some(r) => {
                            info!(
                                router = "embedding",
                                "agent-core: ContextRouter v2 (EmbeddingRouter) live"
                            );
                            Some(r)
                        }
                        None => {
                            warn!(
                                "agent-core: EmbeddingRouter unavailable; falling back to NullRouter"
                            );
                            None
                        }
                    }
                }
                "hybrid" => {
                    // E3: v2.5 plumbing. EmbeddingRouter (v2) primary,
                    // LlmClassifierRouter (v1) fallback on empty/low-
                    // confidence (the v2 router already collapses to
                    // ContextDecision::default() below its confidence
                    // threshold, so "empty primary" == "low-confidence
                    // primary" without any extra signaling).
                    //
                    // If the v2 primary fails to construct (no
                    // OPENAI_API_KEY *and* the hash floor still errors —
                    // unlikely but possible if skills discovery breaks),
                    // we degrade gracefully to the v1 classifier alone.
                    // If even the classifier can't be constructed (it
                    // can't fail today — `Arc::new(LlmClient.into())`),
                    // we fall through to NullRouter.
                    let v1: Arc<dyn clawft_core::agent::context_router::ContextRouter> = Arc::new(
                        clawft_core::agent::context_router::LlmClassifierRouter::new(
                            llm_for_agent.clone(),
                        ),
                    );
                    match build_embedding_router_or_warn(workspace.as_path()).await {
                        Some(emb) => {
                            info!(
                                router = "hybrid",
                                "agent-core: ContextRouter v2.5 (HybridRouter) live \
                             — EmbeddingRouter primary, LlmClassifierRouter fallback"
                            );
                            Some(
                                Arc::new(clawft_core::agent::context_router::HybridRouter::new(
                                    emb, v1,
                                ))
                                    as Arc<dyn clawft_core::agent::context_router::ContextRouter>,
                            )
                        }
                        None => {
                            warn!(
                                "hybrid router: embedding primary unavailable; \
                             using LlmClassifierRouter alone"
                            );
                            Some(v1)
                        }
                    }
                }
                "null" => {
                    info!(
                        router = "null",
                        "agent-core: ContextRouter v0 (NullRouter) live"
                    );
                    None
                }
                other => {
                    warn!(
                        requested = %other,
                        "agent-core: unknown context_router setting; falling back to NullRouter"
                    );
                    None
                }
            };

        // WEFT-330: agent-side SOUL.journal writer. Substrate publish is
        // grant-gated under `_derived/soul_journal/` (F1 stamped the
        // `soul_journal` DerivedWriteGrant at boot). File mirror keeps
        // `.clawft/SOUL.journal.md` in sync for offline inspection.
        let soul_journal: Option<Arc<dyn clawft_core::agent::soul_journal::SoulJournal>> = {
            use clawft_core::agent::soul_journal::{
                CompositeSoulJournal, FileSoulJournal, SoulJournal,
            };
            let k = kernel.read().await;
            let client = Arc::new(clawft_service_agent::KernelSubstrateClient::new(
                k.substrate_service().clone(),
                k.node_registry().clone(),
            ));
            let substrate_writer: Arc<dyn SoulJournal> =
                Arc::new(clawft_service_agent::SubstrateSoulJournal::new(
                    client,
                    daemon_identity.node_id.clone(),
                ));
            let file_mirror: Arc<dyn SoulJournal> =
                Arc::new(FileSoulJournal::for_workspace(&workspace));
            let composite: Arc<dyn SoulJournal> =
                Arc::new(CompositeSoulJournal::with_mirror(substrate_writer, file_mirror));
            info!(
                node_id = %daemon_identity.node_id,
                "agent-core: soul journal writer attached (substrate + file mirror)"
            );
            Some(composite)
        };

        // WEFT-335: log every ContextRouter::route decision under
        // `substrate/_derived/agent/routing/recent/<ulid>` (grant topic
        // `agent`, stamped at boot above). Seeds v1→v2 promotion gate.
        let routing_log: Option<
            Arc<dyn clawft_core::agent::routing_log::RouterDecisionLog>,
        > = {
            let k = kernel.read().await;
            let client = Arc::new(clawft_service_agent::KernelSubstrateClient::new(
                k.substrate_service().clone(),
                k.node_registry().clone(),
            ));
            let writer: Arc<dyn clawft_core::agent::routing_log::RouterDecisionLog> =
                Arc::new(clawft_service_agent::SubstrateRouterDecisionLog::new(
                    client,
                    daemon_identity.node_id.clone(),
                ));
            info!(
                node_id = %daemon_identity.node_id,
                "agent-core: router decision log attached (substrate/_derived/agent/routing/recent)"
            );
            Some(writer)
        };

        let agent_loop = clawft_core::bootstrap::build_daemon_agent_loop(
            llm_for_agent,
            tool_registry,
            identity_loader,
            &workspace,
            Some(concierge_agent_id.clone()),
            context_router,
            Some(chat_gate),
            Some(agent_sink),
            Some(identity_provider),
            // WEFT-10: global routing is the PermissionResolver ceiling.
            // Without this, channel_overrides is empty and every chat
            // principal hits zero_trust regardless of config.
            Some(&agent_routing),
            // WEFT-10: workspace overlay (clamped against global ceiling).
            agent_workspace_routing.as_ref(),
            // ADR-058 Phase 5.1: the L2 session tier as the loop's graft
            // provider (the same instance the turn anchor indexes into), so
            // each turn's prompt is augmented with recalled context. `None`
            // when chain anchoring is off — no grafting.
            session_tier.clone().map(|t| {
                let p: Arc<dyn clawft_core::agent::graft::ContextGraftProvider> = t;
                p
            }),
            soul_journal,
            routing_log,
        )
        .await;

        // WEFT-654: stash the concrete loop `Arc` before it moves into
        // `AgentService::new` below — `agent.proposal.*` RPC arms need the
        // review-gate methods (`pending_hold_info`/`accept_pending`/
        // `discard_pending`) that `AgentLoopHandle` doesn't expose.
        let _ = DAEMON_AGENT_LOOP.set(agent_loop.clone());

        // WEFT-616 Phase 2: attach the per-turn COW memory bracket when the
        // operator enabled it. Fail-open with a loud warning — a broken COW
        // store must not take the whole agent service down (the bracket is
        // isolation infrastructure, not the reply path).
        if cow_memory_cfg.enabled {
            // Tilde-expand via the same idiom the skill dir uses (dirs::home_dir).
            let path = if let Some(rest) = cow_memory_cfg.path.strip_prefix("~/") {
                dirs::home_dir()
                    .map(|h| h.join(rest).display().to_string())
                    .unwrap_or_else(|| cow_memory_cfg.path.clone())
            } else {
                cow_memory_cfg.path.clone()
            };
            match std::fs::create_dir_all(&path).map_err(|e| e.to_string()).and_then(|_| {
                open_or_create_cow_memory(std::path::Path::new(&path))
            }) {
                Ok(mem) => {
                    let cow_handle = std::sync::Arc::new(std::sync::Mutex::new(mem));
                    let _ = DAEMON_COW_MEMORY.set(cow_handle.clone());
                    // WEFT-654: `review` comes off the kernel-config anchor
                    // block (`anchor_cfg.proposal.mode`), not `cow_memory_cfg`
                    // — the review gate is a policy over the SAME lineage,
                    // configured in `[kernel.agent.proposal]` rather than
                    // `[agents.cow_memory]`.
                    agent_loop.set_cow_memory(clawft_core::agent::loop_core::CowAttachment {
                        mem: cow_handle,
                        ingest_turns: cow_memory_cfg.ingest_turns,
                        tool_cadence: cow_memory_cfg.cadence
                            == clawft_types::config::CowCadence::Tool,
                        review: anchor_cfg.proposal.mode
                            == clawft_types::config::ProposalMode::Review,
                    });
                    info!(
                        path = %path,
                        review = anchor_cfg.proposal.mode == clawft_types::config::ProposalMode::Review,
                        "cow memory wired (WEFT-616 Phase 2 / WEFT-654): per-turn checkpoint/promote/rollback active"
                    );

                    // WEFT-616 Phase 3: witness the bracket's transitions on
                    // the exochain (DualStateBridge, plan §7). A ledger with
                    // no cow-memory bracket to observe is pointless, so this
                    // only wires up alongside a successful cow-attach above,
                    // and only when a chain manager is actually present.
                    #[cfg(feature = "exochain")]
                    {
                        let chain = kernel.read().await.chain_manager().cloned();
                        match chain {
                            Some(chain) => {
                                let daemon_ledger =
                                    std::sync::Arc::new(crate::turn_ledger::DaemonTurnLedger::new(chain));
                                let _ = DAEMON_TURN_LEDGER.set(daemon_ledger.clone());
                                agent_loop.set_turn_ledger(daemon_ledger);
                                info!(
                                    "turn ledger wired (WEFT-616 Phase 3): cow-memory \
                                     checkpoint/revert/promote transitions witnessed on the \
                                     exochain"
                                );
                            }
                            None => {
                                warn!(
                                    "cow memory wired but no chain manager available — \
                                     turn bracket transitions will not be witnessed \
                                     (exochain anchoring is off)"
                                );
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!(path = %path, error = %e, "cow memory enabled but failed to open — turns run WITHOUT the checkpoint bracket");
                }
            }
        }

        // WEFT-331: interactive Defer broker. Publishes panel prompts on the
        // chat stream + dedicated defer substrate path; decisions arrive via
        // `agent.chat.defer_decide`. Attach the interactor to the loop before
        // the service wraps it so GateDecision::Defer suspends the turn.
        let defer_publisher: Arc<dyn clawft_service_agent::DeferPromptPublisher> =
            Arc::new(DaemonDeferPublisher {
                kernel: Arc::clone(&kernel),
            });
        let defer_broker = Arc::new(clawft_service_agent::InteractiveDeferBroker::new(
            defer_publisher,
            std::time::Duration::from_millis(
                clawft_types::agent_chat::DEFER_DEFAULT_TIMEOUT_MS,
            ),
        ));
        agent_loop.set_defer_interactor(defer_broker.clone());

        // ADR-058 Phase 5 deferred step 4: also hand the SAME tier to the
        // service so the `agent.chat.end` signal can drive conversation-end
        // promotion (the loop grafts from it; the service promotes from it).
        //
        // WEFT-332: hand the kernel AgentRegistry so multi-tenant
        // `agent.chat` callers get a lazily-registered principal
        // (`agent.chat:user:<caller_id>`) instead of sharing the boot
        // concierge UUID. Absent `caller_id` still falls back to the
        // concierge id registered above.
        let caller_registry = {
            let k = kernel.read().await;
            k.agent_registry().clone()
        };
        // Service mode: service-attested machine key, as for the concierge above.
        let caller_pubkey: [u8; 32] =
            daemon_identity.public_key();
        let mut service = clawft_service_agent::AgentService::new(agent_loop)
            .with_defer_broker(defer_broker)
            .with_caller_registry(
                caller_registry,
                caller_pubkey,
                Some(concierge_agent_id.clone()),
            );
        if let Some(tier) = session_tier {
            service = service.with_session_tier(tier);
        }
        let service = Arc::new(service);
        // M4 Phase C.1: late-wire the spawner's `Weak<AgentService>` back-reference
        // now the service `Arc` exists (Arc-cycle break, design D2). Idempotent
        // OnceLock set — mirrors the A2ARouter gate late-wiring. Without this the
        // spawner can't dispatch child conversations.
        subagent_spawner.set_service(Arc::downgrade(&service));
        // Voice Wave 2 §W2.1: wire the register-early/commit-late reply
        // submitter + the interrupt-routing loop. Gated on `voice_loop` with
        // the `talk_loop` prerequisite (busy state + forest closures ride the
        // hosted loop) — inert with a warning otherwise, mirroring the
        // talk_loop validation precedent.
        if anchor_cfg.voice_loop {
            if anchor_cfg.talk_loop && anchor_cfg.anchor_chain && anchor_cfg.anchor_causal {
                // Shared per-conv utterance queue (WEFT-650): the loop's
                // Queue arm pushes; the submitter drains one utterance on each
                // reply commit/failure. Both sides MUST share this Arc — the
                // 1-arg constructors build private queues and the drain never
                // fires.
                let voice_shared = Arc::new(crate::voice_loop::VoiceShared::default());
                service.set_reply_submitter(Arc::new(
                    crate::voice_loop::DaemonReplySubmitter::with_shared(
                        Arc::downgrade(&service),
                        voice_shared.clone(),
                    ),
                ));
                let _ = DAEMON_VOICE_LOOP.set(Arc::new(crate::voice_loop::VoiceLoop::with_shared(
                    Arc::downgrade(&service),
                    voice_shared,
                )));
                info!(
                    "voice loop wired (§W2.1): recorded user turns route through the \
                     interrupt router; idle turns generate text replies off-path"
                );
            } else {
                warn!(
                    "[kernel.agent] voice_loop=true requires talk_loop + anchor_chain + \
                     anchor_causal; voice loop is inert"
                );
            }
        }
        // WEFT-333: register agent.chat as a first-class kernel
        // SystemService so last-completion / in-flight / lock
        // contention surface in `kernel.services` and `weft status`.
        {
            let chat_svc = Arc::new(clawft_service_agent::AgentChatSystemService::new(
                service.metrics(),
                Arc::clone(&_agent_service_flag),
                service.shutting_down_flag(),
            ));
            let k = kernel.read().await;
            let methods: Vec<String> = clawft_service_agent::AGENT_CHAT_CONTRACT_METHODS
                .iter()
                .map(|s| (*s).to_string())
                .collect();
            let registered = {
                #[cfg(feature = "exochain")]
                {
                    match k.chain_manager() {
                        Some(cm) => k
                            .services()
                            .register_with_contract(chat_svc.clone(), methods, cm),
                        None => k.services().register(chat_svc.clone()),
                    }
                }
                #[cfg(not(feature = "exochain"))]
                {
                    let _ = methods;
                    k.services().register(chat_svc.clone())
                }
            };
            match registered {
                Ok(()) => {
                    let entry = clawft_kernel::ServiceEntry {
                        name: clawft_service_agent::AGENT_CHAT_SERVICE_NAME.to_string(),
                        owner_pid: None,
                        endpoint: clawft_kernel::ServiceEndpoint::External {
                            url: "rpc:agent.chat".to_string(),
                        },
                        audit_level: clawft_kernel::ServiceAuditLevel::Full,
                        registered_at: chrono::Utc::now(),
                    };
                    if let Err(e) = k.services().register_entry(entry) {
                        warn!(
                            error = %e,
                            "agent.chat service entry registration failed (the SystemService is still live)"
                        );
                    }
                    info!("agent.chat SystemService registered (WEFT-333)");
                }
                Err(e) => warn!(
                    error = %e,
                    "agent.chat SystemService registration failed — agent.chat still works, but it won't appear in kernel.services / weft status"
                ),
            }
        }

        let _ = DAEMON_AGENT.set(service);
        info!("agent service wired (agent.chat dispatches through clawft-service-agent)");
        info!("subagent spawner wired (agent_spawn dispatches child conversations)");
    } else {
        warn!(
            "agent service: DAEMON_LLM not set, skipping wiring \
             (agent.chat will return 'agent service not wired' until the LLM client comes up)"
        );
    }

    // Spawn the terminal manager. Empty registry — sessions appear on
    // demand via `terminal.spawn`. Construction is infallible (a
    // `DashMap::new()` under the hood); we still match on the result so
    // future fallible variants land cleanly.
    {
        let mgr = Arc::new(clawft_service_terminal::TerminalManager::new());
        let _ = DAEMON_TERMINAL.set(mgr);
        info!("terminal service wired (PTY-backed sessions hosted in daemon)");
    }

    // WEFT-555 (M5-W): voice transcript consumer. Subscribes to the
    // mesh-canonical transcript topic emitted by
    // `clawft-service-whisper` and routes each transcript into either
    // an `agent.chat` dispatch (default) or the daemon's RPC dispatch
    // (when the transcript starts with `voice.consumer.command_prefix`).
    //
    // Disabled by default; voice routing is opt-in via the config flag
    // until the 5 P0 voice security controls (WEFT-207..211) ship.
    let _voice_router_handle: Option<crate::voice_router::VoiceRouter> = {
        let kernel = Arc::clone(&kernel);
        let control_flags = control_flags.clone();
        let mut voice_consumer_cfg = voice_consumer_cfg.clone();
        let daemon_node_id = daemon_identity.node_id.clone();
        let mut mic_node_rx = mic_node_rx.clone();
        let auto_topic = voice_consumer_cfg.enabled && voice_consumer_cfg.transcript_topic.trim().is_empty();
        let build = async move {
            if !voice_consumer_cfg.enabled {
                info!("voice consumer: disabled by config (voice.consumer.enabled=false)");
                None
            } else if DAEMON_AGENT.get().is_none() {
                warn!(
                    "voice consumer: requested but agent service not wired; \
                     transcripts would have nowhere to land — skipping spawn"
                );
                None
            } else {
                // Empty topic = auto: follow the mic source node (env override or
                // the registered sensor/mic publisher), never a baked-in id.
                if voice_consumer_cfg.transcript_topic.trim().is_empty() {
                    let id = loop {
                        if let Some(id) = mic_node_rx.borrow().clone() {
                            break id;
                        }
                        if mic_node_rx.changed().await.is_err() {
                            return None;
                        }
                    };
                    voice_consumer_cfg.transcript_topic =
                        format!("substrate/_derived/transcript/{id}/mic");
                    info!(topic = %voice_consumer_cfg.transcript_topic, "voice consumer: topic resolved from mic source");
                }
                let substrate = {
                    let k = kernel.read().await;
                    k.substrate_service().clone()
                };
                let subscriber_id = daemon_node_id.clone();
                // SC-4: translate the config-side permission grid into the
                // router's typed VoicePermissions table. Out-of-range raw
                // levels (anything > 2) are clamped to Level 0 by
                // VoicePermissions::from_raw — a defensive default so a bad
                // YAML / TOML edit can never accidentally privilege a
                // principal.
                let perms_cfg = &voice_consumer_cfg.permissions;
                let permissions = crate::voice_router::VoicePermissions::from_raw(
                    perms_cfg.default_level,
                    perms_cfg
                        .principal_levels
                        .iter()
                        .map(|(k, v)| (k.clone(), *v)),
                    perms_cfg.safe_commands.iter().cloned(),
                );
                let router_cfg = crate::voice_router::VoiceRouterConfig {
                    transcript_topic: voice_consumer_cfg.transcript_topic.clone(),
                    chat_target_agent: voice_consumer_cfg.chat_target_agent.clone(),
                    conv_id: voice_consumer_cfg.conv_id.clone(),
                    command_prefix: voice_consumer_cfg.command_prefix.clone(),
                    subscriber_id: Some(subscriber_id.clone()),
                    permissions,
                };
                let chat_handler: Arc<dyn crate::voice_router::ChatHandler> =
                    Arc::new(DaemonAgentChatHandler);
                let cmd_kernel = Arc::clone(&kernel);
                let cmd_shutdown = control_flags.clone();
                let cmd_handler: Arc<dyn crate::voice_router::CommandHandler> =
                    Arc::new(DaemonCommandHandler {
                        kernel: cmd_kernel,
                        // Voice-routed commands cannot trigger daemon
                        // shutdown — the kernel.shutdown verb requires a
                        // separate control intent. Hand the handler a
                        // throwaway watch channel so its signature
                        // matches the daemon's `dispatch` arity.
                        shutdown_tx: watch::channel(false).0,
                        _control: cmd_shutdown,
                    });
                match crate::voice_router::VoiceRouter::spawn(
                    router_cfg,
                    |caller, path| {
                        substrate
                            .subscribe(caller, path)
                            .map(|(_id, rx)| rx)
                            .map_err(|e| format!("substrate subscribe: {e}"))
                    },
                    chat_handler,
                    cmd_handler,
                ) {
                    Ok(svc) => {
                        info!(
                            topic = %voice_consumer_cfg.transcript_topic,
                            chat_target = %voice_consumer_cfg.chat_target_agent,
                            command_prefix = %voice_consumer_cfg.command_prefix,
                            "voice consumer spawned"
                        );
                        Some(svc)
                    }
                    Err(e) => {
                        warn!(error = %e, "voice consumer failed to spawn");
                        None
                    }
                }
            }
        };
        if auto_topic {
            // Do not hold up boot waiting for a mic node to appear.
            tokio::spawn(async move {
                if let Some(router) = build.await {
                    let _router = router;
                    std::future::pending::<()>().await;
                }
            });
            None
        } else {
            build.await
        }
    };

    // Publish the top-level UI sentinel for the terminal panel. Lives
    // at `substrate/<daemon-node>/ui/terminal` — the egui Explorer's
    // terminal viewer shape-matches on `{ "kind": "terminal" }`. By
    // convention, every top-level surface (chat-window agent picks a
    // sibling path) publishes its sentinel under
    // `substrate/<daemon-node>/ui/<name>` so the Explorer tree
    // surfaces them as siblings without further coordination.
    {
        let k = kernel.read().await;
        let substrate = k.substrate_service();
        let path = format!("substrate/{}/ui/terminal", daemon_identity.node_id);
        let value = serde_json::json!({
            "kind": "terminal",
            "label": "Terminal",
            "updated_at_ms": crate::control::now_ms(),
        });
        if let Err(e) = substrate.publish_gated(Some(&daemon_identity.node_id), &path, value) {
            warn!(error = %e, path = %path, "terminal: ui sentinel publish failed");
        } else {
            debug!(path = %path, "terminal: ui sentinel published");
        }
    }

    // Publish initial control intents now that the daemon node is
    // registered and services are wired. The intents live under the
    // daemon's own prefix so `publish_gated` accepts them.
    {
        let k = kernel.read().await;
        let substrate = k.substrate_service();
        let initial = [
            (ControlKind::Service, "llm".to_string(), "Local LLM"),
            (ControlKind::Service, "agent".to_string(), "Agent service"),
        ];
        for (kind, target, label) in &initial {
            let intent = ControlIntent {
                enabled: control_flags
                    .get(*kind, target)
                    .map(|f| f.load(std::sync::atomic::Ordering::SeqCst))
                    .unwrap_or(true),
                kind: *kind,
                target: target.clone(),
                label: (*label).to_string(),
                updated_at_ms: crate::control::now_ms(),
            };
            let path = crate::control::intent_path(&daemon_identity.node_id, *kind, target);
            if let Err(e) =
                substrate.publish_gated(Some(&daemon_identity.node_id), &path, intent.to_value())
            {
                warn!(error = %e, path = %path, "control: initial intent publish failed");
            } else {
                debug!(path = %path, "control: initial intent published");
            }
        }

        // Publish the chat-window sentinel so the GUI Explorer's chat
        // panel has a stable substrate mount point. Shape:
        // `{ "kind": "chat", "model": "<llama-server model name>" }`.
        // The model field is informational — the daemon's `llm.prompt`
        // handler picks the actual model server-side; this just makes
        // the choice visible in the Explorer's tree label and chat
        // header.
        let chat_path = format!("substrate/{}/ui/chat", daemon_identity.node_id,);
        let model_name = match daemon_llm() {
            Some(c) => c.read().await.config().model.clone(),
            None => clawft_service_llm::DEFAULT_LLM_MODEL.to_string(),
        };
        let chat_sentinel = serde_json::json!({
            "kind": "chat",
            "model": model_name,
        });
        if let Err(e) =
            substrate.publish_gated(Some(&daemon_identity.node_id), &chat_path, chat_sentinel)
        {
            warn!(error = %e, path = %chat_path, "ui: chat sentinel publish failed");
        } else {
            debug!(path = %chat_path, "ui: chat sentinel published");
        }
    }

    // Print boot banner. Lead with the build-id so every run makes
    // which binary is executing visible in the first stdout line —
    // disambiguates "is this the freshly-rebuilt daemon?" without
    // needing to grep `weaver --version` separately.
    {
        let k = kernel.read().await;
        println!(
            "weaver {} · git {} · built {}",
            env!("CARGO_PKG_VERSION"),
            env!("BUILD_GIT_HASH"),
            env!("BUILD_TIMESTAMP"),
        );
        info!(
            version = env!("CARGO_PKG_VERSION"),
            git = env!("BUILD_GIT_HASH"),
            built = env!("BUILD_TIMESTAMP"),
            "weaver build identity"
        );
        print!("{}", clawft_kernel::console::boot_banner());
        print!("{}", k.boot_log().format_all());
    }

    // Bind local transport (Unix UDS or Windows named pipe).
    #[cfg(unix)]
    let listener = {
        let listener = UnixListener::bind(&socket_path)?;
        info!(path = %socket_path.display(), "daemon listening");
        println!("Daemon listening on {}", socket_path.display());
        {
            let k = kernel.read().await;
            k.event_log()
                .info("daemon", format!("listening on {}", socket_path.display()));
        }
        listener
    };
    #[cfg(unix)]
    let child_listener = if crate::user_daemon::is_active() {
        let child_socket = crate::user_daemon::child_socket_path(paths.root());
        // The daemon instance lock is held, so reclaiming a stale endpoint
        // cannot replace another live daemon's listener.
        let bound = crate::user_daemon::bind_child_socket(paths.root())?;
        info!(path = %child_socket.display(), "child-only RPC endpoint listening");
        Some((child_socket, bound))
    } else {
        None
    };
    #[cfg(windows)]
    let mut pipe_server = {
        let pipe = clawft_rpc::pipe_name_for_path(&socket_path);
        let server = clawft_rpc::named_pipe::create_listener(&socket_path).map_err(|e| {
            anyhow::anyhow!("failed to create named-pipe listener `{pipe}`: {e}")
        })?;
        info!(pipe = %pipe, "daemon listening");
        println!("Daemon listening on {pipe}");
        {
            let k = kernel.read().await;
            k.event_log()
                .info("daemon", format!("listening on {pipe}"));
        }
        server
    };

    // Stream-window anchor: start configured topic anchors so every
    // `window_secs` window emits a `stream.window_commit` chain event
    // summarising traffic for audit. Each anchor subscribes via the
    // a2a topic router and holds its own BLAKE3 + counters.
    #[cfg(feature = "exochain")]
    let mut stream_anchors: Vec<clawft_kernel::TopicAnchor> = Vec::new();
    #[cfg(feature = "exochain")]
    {
        let k = kernel.read().await;
        if let Some(cfg) = k.kernel_config().anchor.as_ref()
            && cfg.enabled
            && !cfg.topics.is_empty()
        {
            let window = std::time::Duration::from_secs(cfg.window_secs.max(1));
            let a2a = k.a2a_router().clone();
            let chain = k.chain_manager().cloned();
            for pattern in &cfg.topics {
                // Exact topics are wired directly; wildcard patterns
                // like "sensor.*" are not expanded here — the
                // config-writer provides the exact topic prefix
                // they want to anchor, and the anchor currently
                // subscribes by literal name. A wildcard anchor
                // watchdog is a follow-up (M1.6+).
                let topic = pattern.clone();
                let anchor = clawft_kernel::StreamWindowAnchor::start_topic(
                    Arc::clone(&a2a),
                    chain.clone(),
                    topic.clone(),
                    window,
                );
                stream_anchors.push(anchor);
            }
            info!(
                anchors = stream_anchors.len(),
                window_ms = window.as_millis() as u64,
                "stream-window anchors started"
            );
        }
    }

    // Shutdown signal
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    #[cfg(all(unix, feature = "mesh"))]
    if let Some(h) = mesh_handle {
        crate::mesh_boot::stop_on_shutdown(h, shutdown_rx.clone());
    }

    // Managed inference servers started by this daemon stop with it.
    #[cfg(all(feature = "placement", unix))]
    {
        let mut rx = shutdown_rx.clone();
        tokio::spawn(async move {
            while !*rx.borrow() {
                if rx.changed().await.is_err() {
                    break;
                }
            }
            if let Some(st) = crate::infer_wire::state() {
                st.shutdown().await;
            }
        });
    }

    // Cron tick loop — fires overdue jobs every second
    let cron_kernel = Arc::clone(&kernel);
    let mut cron_shutdown_rx = shutdown_rx.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    let k = cron_kernel.read().await;
                    let cron = k.cron_service();
                    let tick_result = cron.tick();

                    // Dispatch fired jobs via chain-logged A2ARouter
                    for job_id in &tick_result.fired {
                        if let Some(job) = cron.job_snapshot(job_id)
                            && let Some(target_pid) = job.target_pid
                        {
                            // Log cron.fire event first (records scheduling intent)
                            #[cfg(feature = "exochain")]
                            if let Some(cm) = k.chain_manager() {
                                cm.append(
                                    "cron",
                                    "cron.fire",
                                    Some(serde_json::json!({
                                        "job_id": job.id,
                                        "name": job.name,
                                        "fire_count": job.fire_count,
                                        "target_pid": job.target_pid,
                                    })),
                                );
                            }

                            // Dispatch via send_checked (logs ipc.send in chain)
                            let msg = clawft_kernel::KernelMessage::new(
                                0,
                                clawft_kernel::MessageTarget::Process(target_pid),
                                clawft_kernel::MessagePayload::Json(serde_json::json!({
                                    "cmd": job.command,
                                    "cron_job_id": job.id,
                                    "cron_job_name": job.name,
                                })),
                            );
                            let a2a = k.a2a_router().clone();

                            #[cfg(feature = "exochain")]
                            {
                                let chain = k.chain_manager();
                                if let Err(e) = a2a.send_checked(msg, chain.map(|c| c.as_ref())).await {
                                    warn!(job_id = %job.id, error = %e, "cron: failed to send to target");
                                }
                            }
                            #[cfg(not(feature = "exochain"))]
                            {
                                if let Err(e) = a2a.send(msg).await {
                                    warn!(job_id = %job.id, error = %e, "cron: failed to send to target");
                                }
                            }
                        }
                    }
                }
                _ = cron_shutdown_rx.changed() => {
                    if *cron_shutdown_rx.borrow() {
                        debug!("cron tick loop shutting down");
                        break;
                    }
                }
            }
        }
    });

    // Chain event bridge — drains non-kernel chain events and forwards to
    // ChainManager (WEFT-597). Producers reach the pending buffer either via
    // `chain_event!` / `push_chain_event` or via `ChainEventLayer` (installed
    // in weaver `main.rs`) for tracing-only call sites.
    #[cfg(feature = "exochain")]
    {
        let bridge_kernel = Arc::clone(&kernel);
        let mut bridge_shutdown_rx = shutdown_rx.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        let k = bridge_kernel.read().await;
                        if let Some(cm) = k.chain_manager() {
                            let n = crate::chain_bridge::forward_pending_to_chain(cm);
                            if n > 0 {
                                debug!(count = n, "chain event bridge forwarded pending events");
                            }
                        }
                    }
                    _ = bridge_shutdown_rx.changed() => {
                        if *bridge_shutdown_rx.borrow() {
                            // Final drain before shutdown
                            let k = bridge_kernel.read().await;
                            if let Some(cm) = k.chain_manager() {
                                let n = crate::chain_bridge::forward_pending_to_chain(cm);
                                if n > 0 {
                                    debug!(count = n, "chain event bridge final drain");
                                }
                            }
                            debug!("chain event bridge shutting down");
                            break;
                        }
                    }
                }
            }
        });
    }

    // Health monitor loop — periodic aggregate() with chain logging
    //
    // We access the kernel inside the loop but drop the RwLock guard before
    // any await on service health checks to avoid Send issues.
    let health_kernel = Arc::clone(&kernel);
    let mut health_shutdown_rx = shutdown_rx.clone();
    tokio::spawn(async move {
        let interval_secs = {
            let k = health_kernel.read().await;
            k.kernel_config().health_check_interval_secs
        };
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    // Snapshot services as concrete Vec (releases DashMap refs)
                    let (services_snapshot, event_log) = {
                        let k = health_kernel.read().await;
                        let snapshot = k.services().snapshot();
                        let el = k.event_log().clone();
                        (snapshot, el)
                    };
                    #[cfg(feature = "exochain")]
                    let chain = {
                        let k = health_kernel.read().await;
                        k.chain_manager().cloned()
                    };

                    // Run health checks outside the lock (no DashMap borrow)
                    let mut results = Vec::new();
                    let mut unhealthy_names = Vec::new();
                    let mut all_unhealthy = true;

                    for (name, svc) in &services_snapshot {
                        let status = svc.health_check().await;
                        match &status {
                            clawft_kernel::HealthStatus::Healthy => {
                                all_unhealthy = false;
                            }
                            clawft_kernel::HealthStatus::Degraded(msg) => {
                                event_log.warn("health", format!("{name}: degraded - {msg}"));
                                unhealthy_names.push(name.clone());
                                all_unhealthy = false;
                            }
                            clawft_kernel::HealthStatus::Unhealthy(msg) => {
                                event_log.error("health", format!("{name}: unhealthy - {msg}"));
                                unhealthy_names.push(name.clone());
                            }
                            clawft_kernel::HealthStatus::Unknown => {
                                unhealthy_names.push(name.clone());
                            }
                            _ => {
                                event_log.warn("health", format!("{name}: unrecognized health status"));
                                unhealthy_names.push(name.clone());
                            }
                        }
                        results.push((name.clone(), status));
                    }

                    let overall = if services_snapshot.is_empty() {
                        clawft_kernel::OverallHealth::Down
                    } else if unhealthy_names.is_empty() {
                        clawft_kernel::OverallHealth::Healthy
                    } else if all_unhealthy {
                        clawft_kernel::OverallHealth::Down
                    } else {
                        clawft_kernel::OverallHealth::Degraded {
                            unhealthy_services: unhealthy_names,
                        }
                    };

                    // Chain event (exochain)
                    #[cfg(feature = "exochain")]
                    if let Some(ref cm) = chain {
                        cm.append("health", "health.check", Some(serde_json::json!({
                            "overall": overall.to_string(),
                            "services": results.len(),
                        })));
                    }
                    let _ = overall; // suppress unused warning when exochain is off
                }
                _ = health_shutdown_rx.changed() => {
                    if *health_shutdown_rx.borrow() {
                        debug!("health monitor loop shutting down");
                        break;
                    }
                }
            }
        }
    });

    // Watchdog loop — sweep finished JoinHandles every 5 seconds
    let watchdog_kernel = Arc::clone(&kernel);
    let mut watchdog_shutdown_rx = shutdown_rx.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    let k = watchdog_kernel.read().await;
                    let reaped = k.supervisor().watchdog_sweep().await;
                    for (pid, code) in &reaped {
                        k.event_log().warn("watchdog", format!("reaped PID {pid} (exit code {code})"));
                    }
                }
                _ = watchdog_shutdown_rx.changed() => {
                    if *watchdog_shutdown_rx.borrow() {
                        debug!("watchdog loop shutting down");
                        break;
                    }
                }
            }
        }
    });

    // M2 D1/D6: drive the daemon-hosted TalkModeLoop and its idle reaper —
    // only when the loop was actually hosted at boot (talk_loop on + prereqs
    // met, so `DAEMON_TALK_LOOP` is set). Placed before the accept loop moves
    // `shutdown_rx`.
    // The tier is retrieved from the agent service (the boot-time
    // `session_tier` local was moved into `with_session_tier` above).
    if let (Some(talk_loop), Some(session_tier)) = (
        DAEMON_TALK_LOOP.get().cloned(),
        daemon_agent().and_then(|a| a.session_tier().cloned()),
    ) {
        // (a) run_talk_loop on the ECC self-calibrating cadence, cancelled on
        // daemon shutdown. It wants a tokio-util CancellationToken, so bridge
        // the daemon's watch-based shutdown into one.
        let cancel = tokio_util::sync::CancellationToken::new();
        {
            let cancel = cancel.clone();
            let mut rx = shutdown_rx.clone();
            tokio::spawn(async move {
                let _ = rx.changed().await;
                cancel.cancel();
            });
        }
        tokio::spawn(clawft_kernel::talk_loop::run_talk_loop(
            talk_loop.clone(),
            cancel,
        ));

        // (b) idle reaper — coarse 60s cadence (NOT the ~50ms tick). For each
        // conversation idle past `talk_loop_idle_secs`, run the same
        // postmortem/promote path `agent.chat.end` uses, then evict the loop's
        // per-conversation state (D6), so `lineage`/views don't grow unbounded.
        let idle_secs = {
            let k = kernel.read().await;
            k.kernel_config()
                .agent
                .as_ref()
                .and_then(|a| a.talk_loop_idle_secs)
                .unwrap_or(1800)
        };
        let idle = std::time::Duration::from_secs(idle_secs.max(1));
        let activity = DAEMON_CONV_ACTIVITY
            .get()
            .cloned()
            .unwrap_or_else(|| Arc::new(dashmap::DashMap::new()));
        let reaper_tier = session_tier.clone();
        let reaper_loop = talk_loop.clone();
        let mut reaper_shutdown_rx = shutdown_rx.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        let now = std::time::Instant::now();
                        for conv_id in reaper_tier.active_conversations() {
                            let idle_for = activity
                                .get(&conv_id)
                                .map(|t| now.duration_since(*t.value()));
                            if !matches!(idle_for, Some(d) if d >= idle) {
                                continue;
                            }
                            // Same end path as agent.chat.end: digest → durable
                            // fact → promote_and_drop / drop_view.
                            let durable = match daemon_llm() {
                                Some(llm) => {
                                    match reaper_tier.conversation_digest(&conv_id, 16_384) {
                                        Some(digest) => {
                                            let guard = llm.read().await;
                                            crate::conv_postmortem::summarize_durable_facts(
                                                &guard, &digest,
                                            )
                                            .await
                                        }
                                        None => None,
                                    }
                                }
                                None => None,
                            };
                            if let Some(agent) = daemon_agent() {
                                let _ = agent.end_conversation(&conv_id, durable.as_deref());
                            }
                            // M4 D5: an idle-reaped parent fully ends its spawned
                            // children too (cancel + drop view + evict loop
                            // state) before its own loop state is evicted.
                            cascade_end_children(&conv_id).await;
                            reaper_loop.end_conversation(&conv_id);
                            activity.remove(&conv_id);
                            info!(conv_id = %conv_id, "M2 reaper: ended idle talk conversation");
                        }
                        // WEFT-654 D10 / WEFT-655 Gap B: fail-closed timeout
                        // guard on a parked review-mode proposal. A hold past
                        // its `[kernel.agent.proposal].timeout_secs` deadline
                        // is DISCARDED (prune + witness), never silently
                        // committed — same fail-closed posture as the rest
                        // of governance. Runs every tick regardless of
                        // conversation idleness: the loop's cow lineage is
                        // global, so a held proposal blocks ALL new
                        // dispatches (D9) — a conv "under review" already
                        // can't go idle-active in the ordinary sense, it's
                        // parked until accept/discard/timeout resolves it.
                        // Body factored into `run_proposal_timeout_sweep_tick`
                        // — shared with the standalone sweep task hosted
                        // below when this reaper isn't (Gap B).
                        run_proposal_timeout_sweep_tick();
                        // WEFT-652 AutoPause: with NO conversations left
                        // active, park the cow lineage — freeze working (no
                        // fresh derive) so the daemon idles without an open
                        // writer chain. The next turn's bracket auto-resumes
                        // (O(1) derive). Best-effort: pause failure only logs.
                        if activity.is_empty()
                            && let Some(cow) = DAEMON_COW_MEMORY.get()
                        {
                            let mut guard = cow.lock().unwrap_or_else(|p| p.into_inner());
                            if !guard.is_paused() {
                                match guard.pause("autopause:idle") {
                                    Ok(()) => {
                                        if let Some(ledger) = DAEMON_TURN_LEDGER.get() {
                                            ledger.witness_autopause(idle.as_secs());
                                        }
                                        info!("cow memory: autopaused (daemon idle)");
                                    }
                                    Err(e) => {
                                        warn!(error = %e, "cow memory: autopause failed; lineage stays live");
                                    }
                                }
                            }
                        }
                    }
                    _ = reaper_shutdown_rx.changed() => {
                        if *reaper_shutdown_rx.borrow() {
                            debug!("talk reaper shutting down");
                            break;
                        }
                    }
                }
            }
        });
    } else if DAEMON_PROPOSAL_REVIEW_MODE.get().copied().unwrap_or(false) {
        // WEFT-655 Gap B: the M2 reaper above — which normally carries the
        // WEFT-654 D10 fail-closed proposal timeout guard
        // (`run_proposal_timeout_sweep_tick`) — only exists when `talk_loop`
        // was actually hosted (prereqs: `talk_loop` + chain + causal all on,
        // and the tier ended up owning the forest). Review mode has no such
        // prerequisite: an operator can turn on
        // `[kernel.agent.proposal] mode = "review"` around cow-memory
        // without ever enabling `talk_loop`, and a held proposal would then
        // never time out. Host a minimal sweep-only task instead — same 60s
        // tick, same shutdown_rx pattern as the reaper, but ONLY the timeout
        // discard (no idle-conversation reaping, no autopause; those need
        // the hosted loop this branch doesn't have).
        let mut sweep_shutdown_rx = shutdown_rx.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
            loop {
                tokio::select! {
                    _ = interval.tick() => run_proposal_timeout_sweep_tick(),
                    _ = sweep_shutdown_rx.changed() => {
                        if *sweep_shutdown_rx.borrow() {
                            debug!("standalone proposal timeout sweep shutting down");
                            break;
                        }
                    }
                }
            }
        });
        info!(
            "WEFT-655: standalone proposal timeout sweep hosted (review mode, \
             no talk_loop reaper to carry it)"
        );
    }

    // Phase B (classification-design §D4): drain the async enrichment queue.
    // One task — hosted only when `mode = full` wired the queue + classifier at
    // boot — pulls each committed-turn job, runs the cheap-model four-axis
    // enrichment off the turn path, and patches the node's `classification` blob
    // (tier=llm) via `CausalGraph::merge_node_metadata` (B1). Cancelled on daemon
    // shutdown like the loops above. Serialises the cheap-model calls (one job in
    // flight at a time), which is the point of a single drain.
    if let (Some(queue), Some(enricher)) = (
        DAEMON_ENRICH_QUEUE.get().cloned(),
        DAEMON_ENRICH_CLASSIFIER.get().cloned(),
    ) {
        let causal = {
            let k = kernel.read().await;
            k.ecc_causal().cloned()
        };
        match causal {
            Some(causal) => {
                let mut drain_shutdown_rx = shutdown_rx.clone();
                tokio::spawn(async move {
                    run_enrich_drain(queue, enricher, causal, &mut drain_shutdown_rx).await;
                });
                info!("Phase B: classification enrichment drain task hosted (mode=full)");
            }
            None => {
                warn!(
                    "classification.mode=full but the kernel has no causal graph — \
                     enrichment drain inert"
                );
            }
        }
    }

    // Accept loop — clone shutdown_tx so the outer scope can still use it for Ctrl+C
    let accept_kernel = Arc::clone(&kernel);
    let rpc_shutdown_tx = shutdown_tx.clone();
    #[cfg(unix)]
    let mut accept_handle = {
        let mut shutdown_rx = shutdown_rx.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    result = listener.accept() => {
                        match result {
                            Ok((stream, _addr)) => {
                                let k = Arc::clone(&accept_kernel);
                                let tx = rpc_shutdown_tx.clone();
                                // ADR-070 owner model, now verified: literal
                                // scopes only work for a peer with our uid.
                                // Unknown credentials fail closed.
                                // A supervised project kernel (same uid) is
                                // classified by its process group (review S9).
                                let peer = crate::child_peer::classify_peer(stream.peer_cred());
                                tokio::spawn(handle_connection_classed(stream, k, tx, peer));
                            }
                            Err(e) => {
                                error!("accept error: {e}");
                            }
                        }
                    }
                    _ = shutdown_rx.changed() => {
                        if *shutdown_rx.borrow() {
                            info!("shutdown signal received, stopping accept loop");
                            break;
                        }
                    }
                }
            }
        })
    };
    #[cfg(unix)]
    let child_socket_cleanup = child_listener.as_ref().map(|(path, _)| path.clone());
    #[cfg(unix)]
    let mut child_accept_handle = child_listener.map(|(_, listener)| {
        let kernel = Arc::clone(&kernel);
        let tx = shutdown_tx.clone();
        let mut rx = shutdown_rx.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    result = listener.accept() => match result {
                        Ok((stream, _)) => {
                            tokio::spawn(handle_connection_child(stream, Arc::clone(&kernel), tx.clone()));
                        }
                        Err(e) => error!("child endpoint accept error: {e}"),
                    },
                    _ = rx.changed() => { if *rx.borrow() { break } },
                }
            }
        })
    });
    // Windows named-pipe accept loop (WEFT-559). Peers are not classified
    // here: `handle_connection` defaults them to Owner (ADR-103 A14). After each client
    // connects, re-create the next pipe instance so concurrent clients
    // can dial while the previous connection is still being served.
    #[cfg(windows)]
    let mut accept_handle = {
        let accept_socket_path = socket_path.clone();
        let mut shutdown_rx = shutdown_rx.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    result = pipe_server.connect() => {
                        match result {
                            Ok(()) => {
                                let connected = pipe_server;
                                match clawft_rpc::named_pipe::create_listener_next(
                                    &accept_socket_path,
                                ) {
                                    Ok(next) => {
                                        pipe_server = next;
                                    }
                                    Err(e) => {
                                        error!("named-pipe re-listen error: {e}");
                                        // Still serve the connected client; next
                                        // accept will fail until a later restart.
                                        let k = Arc::clone(&accept_kernel);
                                        let tx = rpc_shutdown_tx.clone();
                                        tokio::spawn(handle_connection(connected, k, tx));
                                        break;
                                    }
                                }
                                let k = Arc::clone(&accept_kernel);
                                let tx = rpc_shutdown_tx.clone();
                                tokio::spawn(handle_connection(connected, k, tx));
                            }
                            Err(e) => {
                                error!("named-pipe accept error: {e}");
                            }
                        }
                    }
                    _ = shutdown_rx.changed() => {
                        if *shutdown_rx.borrow() {
                            info!("shutdown signal received, stopping accept loop");
                            break;
                        }
                    }
                }
            }
        })
    };

    // Liveness self-check and the systemd watchdog (card 1a919ae6): needs
    // the accept loop above to answer its `ping`.
    crate::liveness::start(
        crate::liveness::Targets {
            kernel: Some(Arc::clone(&kernel)),
            socket: Some(protocol::socket_path()),
            ..Default::default()
        },
        shutdown_rx.clone(),
    );

    // Optional TCP relay (Unix only). When `[kernel.ipc_tcp]` is enabled,
    // every accepted TCP connection is relayed to a fresh connection on
    // the unix socket, with self-asserted literal scope strings stripped
    // from requests (`relay_auth`). All auth / JSON dispatch stays in the
    // unix path — the TCP side is a conduit so cross-boundary
    // callers (Windows side of WSL, remote bridges) can reach the RPC
    // without speaking `AF_UNIX`. On Windows the daemon *is* the named
    // pipe endpoint, so the relay is unnecessary.
    #[cfg(unix)]
    let ipc_tcp_cfg = {
        let k = kernel.read().await;
        k.kernel_config().ipc_tcp.clone()
    };
    #[cfg(unix)]
    if let Some(cfg) = ipc_tcp_cfg.filter(|c| c.enabled) {
        // WEFT-481: refuse to bind a non-loopback address without a
        // bearer token. Anonymous broadcast on a routable interface
        // would expose every RPC verb (kernel.shutdown,
        // agent.spawn, ...) to the network. The operator must opt
        // in to the broader interface AND provide auth.
        if cfg.is_non_loopback() && cfg.bearer.is_none() {
            warn!(
                addr = %cfg.listen_addr,
                "ipc tcp relay refusing to bind non-loopback address without bearer token (WEFT-481); set [kernel.ipc_tcp].bearer or restrict listen_addr to 127.0.0.1"
            );
        } else {
            match TcpListener::bind(&cfg.listen_addr).await {
                Ok(tcp_listener) => {
                    info!(
                        addr = %cfg.listen_addr,
                        bearer = cfg.bearer.is_some(),
                        "ipc tcp relay listening"
                    );
                    println!(
                        "IPC TCP relay listening on {} (bearer={})",
                        cfg.listen_addr,
                        if cfg.bearer.is_some() {
                            "required"
                        } else {
                            "none"
                        }
                    );
                    let relay_socket_path = socket_path.clone();
                    let mut relay_shutdown_rx = shutdown_tx.subscribe();
                    let bearer = cfg.bearer.clone();
                    tokio::spawn(async move {
                        loop {
                            tokio::select! {
                                result = tcp_listener.accept() => {
                                    match result {
                                        Ok((tcp_stream, peer)) => {
                                            // WEFT-481: defence in depth. Even
                                            // when bound to 127.0.0.1, drop
                                            // non-loopback peers (some kernels
                                            // route 0.0.0.0 traffic to a
                                            // 127.0.0.1 listener).
                                            if !peer.ip().is_loopback() && bearer.is_none() {
                                                warn!(peer = %peer, "ipc tcp relay: rejected non-loopback peer (no bearer set)");
                                                continue;
                                            }
                                            info!(peer = %peer, "ipc tcp relay: peer connected");
                                            let sock = relay_socket_path.clone();
                                            let bearer_for_conn = bearer.clone();
                                            tokio::spawn(async move {
                                                let mut tcp_stream = tcp_stream;
                                                // WEFT-481: bearer handshake.
                                                // The client must send
                                                // `Bearer: <token>\n` as the
                                                // first wire line. Mismatch
                                                // closes the connection
                                                // before any byte is
                                                // forwarded to the unix
                                                // socket, so the daemon never
                                                // sees an unauthenticated
                                                // RPC verb.
                                                if let Some(expected) = bearer_for_conn.as_deref() {
                                                    use tokio::io::AsyncBufReadExt;
                                                    use tokio::io::BufReader;
                                                    let mut reader = BufReader::new(&mut tcp_stream);
                                                    let mut line = String::new();
                                                    if reader.read_line(&mut line).await.is_err() {
                                                        warn!(peer = %peer, "ipc tcp relay: bearer line read error");
                                                        return;
                                                    }
                                                    let trimmed = line.trim_end_matches(['\r', '\n']);
                                                    let presented = trimmed
                                                        .strip_prefix("Bearer: ")
                                                        .or_else(|| trimmed.strip_prefix("Bearer "))
                                                        .unwrap_or("");
                                                    if presented != expected {
                                                        warn!(peer = %peer, "ipc tcp relay: bearer mismatch, closing");
                                                        return;
                                                    }
                                                }

                                                match UnixStream::connect(&sock).await {
                                                    Ok(unix_stream) => {
                                                        // Sanitised relay: self-asserted literal
                                                        // scopes from TCP are dropped (R3).
                                                        let (a, b) = match crate::relay_auth::relay(
                                                            tcp_stream,
                                                            unix_stream,
                                                        )
                                                        .await
                                                        {
                                                            Ok(pair) => pair,
                                                            Err(e) => {
                                                                debug!(peer = %peer, "ipc tcp relay: copy ended: {e}");
                                                                (0, 0)
                                                            }
                                                        };
                                                        info!(peer = %peer, tx_bytes = a, rx_bytes = b, "ipc tcp relay: peer disconnected");
                                                    }
                                                    Err(e) => {
                                                        warn!(peer = %peer, "ipc tcp relay: unix connect failed: {e}");
                                                    }
                                                }
                                            });
                                        }
                                        Err(e) => {
                                            error!("ipc tcp accept error: {e}");
                                        }
                                    }
                                }
                                _ = relay_shutdown_rx.changed() => {
                                    if *relay_shutdown_rx.borrow() {
                                        debug!("ipc tcp relay shutting down");
                                        break;
                                    }
                                }
                            }
                        }
                    });
                }
                Err(e) => {
                    warn!(addr = %cfg.listen_addr, "ipc tcp relay bind failed: {e}");
                }
            }
        }
    }

    // Wait for shutdown signal or RPC shutdown.
    // Unix: SIGINT / SIGTERM / SIGHUP (restart). Windows: Ctrl+C only.
    #[cfg(unix)]
    let restart_requested = {
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        let mut sighup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("SIGINT received, shutting down daemon");
                let _ = shutdown_tx.send(true);
                false
            }
            _ = sigterm.recv() => {
                info!("SIGTERM received, shutting down daemon");
                let _ = shutdown_tx.send(true);
                false
            }
            _ = sighup.recv() => {
                info!("SIGHUP received — will restart after shutdown");
                let _ = shutdown_tx.send(true);
                true
            }
            _ = &mut accept_handle => {
                info!("accept loop finished (RPC shutdown)");
                false
            }
        }
    };
    #[cfg(windows)]
    let restart_requested = {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("Ctrl+C received, shutting down daemon");
                let _ = shutdown_tx.send(true);
                false
            }
            _ = &mut accept_handle => {
                info!("accept loop finished (RPC shutdown)");
                false
            }
        }
    };

    // Every step below is bounded and logged, and a guard thread ends the
    // process if the whole shutdown overruns (card 1a919ae6): a wedged task
    // must not leave a daemon that took TERM still holding the run dir.
    #[cfg(unix)]
    let nested_instance = crate::nested_boot::active().is_some();
    #[cfg(not(unix))]
    let nested_instance = false;
    let shutdown = crate::shutdown_guard::Shutdown::begin(
        if nested_instance {
            crate::shutdown_guard::NESTED_SHUTDOWN_BUDGET
        } else {
            crate::shutdown_guard::SHUTDOWN_BUDGET
        },
        crate::shutdown_guard::HardExit {
            pid_file: protocol::pid_path(),
            code: if restart_requested { crate::shutdown_guard::HARD_EXIT_RESTART } else { 0 },
        },
    );
    crate::shutdown_guard::test_wedge();
    let secs = std::time::Duration::from_secs;

    // If a signal triggered shutdown, wait for the accept loop to finish.
    if (restart_requested || !accept_handle.is_finished())
        && shutdown.step("rpc accept loop", secs(2), &mut accept_handle).await.is_none()
    {
        accept_handle.abort();
    }
    #[cfg(unix)]
    if let Some(mut handle) = child_accept_handle.take() {
        let _ = shutdown_tx.send(true);
        if shutdown.step("child accept loop", secs(1), &mut handle).await.is_none() {
            handle.abort();
        }
    }

    // Stop stream-window anchors so they flush their final windows
    // before the chain manager shuts down.
    #[cfg(feature = "exochain")]
    {
        for anchor in stream_anchors {
            anchor.shutdown();
        }
    }

    // agent-core-v1 Phase C2 / D3: drain the agent service before the
    // supervisor shutdown sweeps process-table entries. Cancels every
    // in-flight `agent.chat` dispatch and waits up to 5s for the
    // counter to hit zero. After D3 every dispatch flows through this
    // service, so the drain is the single chokepoint for graceful
    // chat shutdown.
    if let Some(agent) = DAEMON_AGENT.get()
        && let Some(drained) = shutdown
            .step("agent service", secs(5), agent.shutdown(secs(5)))
            .await
    {
        info!(drained, "agent service shutdown");
    }

    // Project kernel: final anchor and unregister, before the chain is saved.
    #[cfg(all(unix, feature = "exochain"))]
    if let Some(r) = shutdown.step("nested quiesce", secs(3), crate::nested_rpc::quiesce()).await {
        r?;
    }
    // A nested user instance (ADR-103 Phase 4, D10) owns its projects: they stop with it,
    // whether shutdown came by RPC or by the owner closing the liveness pipe (deny_all). A
    // top-level user daemon keeps its children running for re-adoption on the next boot;
    // `weaver kernel stop` cascades explicitly there. Bounded inside the owner's 15 s
    // graceful window.
    #[cfg(unix)]
    if nested_instance
        && let Some(sup) = crate::project_supervisor::global()
        && let Some(stopped) = shutdown.step("nested project kernels", secs(10), sup.stop_all()).await
    {
        info!(count = stopped.len(), "nested instance: stopped its project kernels");
    }
    shutdown
        .step("project hooks", secs(2), crate::project_hooks::pre_shutdown())
        .await;

    // Gracefully shut down running agents before kernel shutdown
    shutdown
        .step("agent supervisor", secs(5), async {
            let k = kernel.read().await;
            let results = k
                .supervisor()
                .shutdown_all(std::time::Duration::from_secs(5))
                .await;
            for (pid, code) in &results {
                k.event_log().info(
                    "shutdown",
                    format!("agent PID {pid} exited with code {code}"),
                );
            }
        })
        .await;

    // Shut down kernel (services, then the chain checkpoint).
    shutdown
        .final_step("kernel", async {
            let mut k = kernel.write().await;
            if let Err(e) = k.shutdown().await {
                warn!("kernel shutdown error: {e}");
            }
        })
        .await;

    // Clean up transport endpoint artefacts. `kernel.pid` stays until the
    // process is about to exit (the caller removes it; see
    // `commands::kernel_cmd`), so `kernel stop` can always find a live
    // daemon by it.
    #[cfg(unix)]
    if socket_path.exists() {
        let _ = std::fs::remove_file(&socket_path);
    }
    #[cfg(unix)]
    if let Some(path) = child_socket_cleanup {
        let _ = std::fs::remove_file(path);
    }

    println!("Daemon stopped.");

    // If SIGHUP requested restart, re-exec the binary (keeps same PID for systemd).
    // Windows has no SIGHUP path; restart is stop+start from the CLI.
    #[cfg(unix)]
    if restart_requested {
        info!("re-exec for restart");
        use std::os::unix::process::CommandExt;
        // Original args (keeps `--profile user`), captured before the
        // chdir with --config absolute, minus one-shot chain flags; an exe
        // path that survives an update replacing the binary; an absolute
        // runtime dir in the child's environment (Command::env).
        let plan =
            crate::boot_refusal::reexec_plan(&std::env::current_exe()?, crate::boot_refusal::replay());
        let mut cmd = std::process::Command::new(&plan.exe);
        cmd.args(&plan.args);
        cmd.env(crate::boot_refusal::REEXEC_ENV, "1");
        // Release the lock now rather than rely on close-on-exec alone.
        drop(instance_lock);
        if let Some(rt) = &plan.runtime_dir {
            cmd.env(clawft_types::runtime_paths::RUNTIME_DIR_ENV, rt);
        }
        let err = cmd.exec(); // replaces process; only reached on error
        eprintln!("re-exec failed: {err}");
        let _ = std::fs::remove_file(protocol::pid_path());
        std::process::exit(1);
    }
    #[cfg(windows)]
    let _ = (restart_requested, &instance_lock);

    Ok(())
}

/// Handle a single client connection — accepts both JSON line mode and
/// (when the `rvf-rpc` feature is enabled) the RVF-framed protocol.
///
/// Detects the connection mode by reading the first 4 bytes:
///   - `RVFS` → RVF-framed protocol (content-hash verified segments)
///   - anything else → legacy line-delimited JSON (bytes prepended to first line)
///
/// Transport-agnostic (WEFT-559): works over Unix domain sockets and
/// Windows named pipes (`NamedPipeServer` after `connect()`).
///
/// Exposed `pub` so integration tests can drive a preassembled kernel
/// directly without the signal-handler plumbing in [`run`].
pub async fn handle_connection<S>(
    stream: S,
    kernel: Arc<tokio::sync::RwLock<Kernel<NativePlatform>>>,
    shutdown_tx: watch::Sender<bool>,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    // Not classified: this wrapper (and the Windows named-pipe path) default to Owner.
    handle_connection_peer(stream, kernel, shutdown_tx, false).await;
}

/// [`handle_connection`] with the peer-credential verdict: `true` when the
/// peer is known not to be the daemon's uid (see `CallerCtx::peer_untrusted`).
pub async fn handle_connection_peer<S>(
    stream: S,
    kernel: Arc<tokio::sync::RwLock<Kernel<NativePlatform>>>,
    shutdown_tx: watch::Sender<bool>,
    peer_untrusted: bool,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    use crate::child_peer::PeerClass;
    let peer = if peer_untrusted { PeerClass::OtherUid } else { PeerClass::Owner };
    handle_connection_classed(stream, kernel, shutdown_tx, peer).await;
}

/// [`handle_connection_peer`] with the full peer classification
/// ([`crate::child_peer::PeerClass`]): the accept loop uses it so a
/// supervised child's literal scopes are ignored (ADR-103 A14).
pub async fn handle_connection_classed<S>(
    stream: S,
    kernel: Arc<tokio::sync::RwLock<Kernel<NativePlatform>>>,
    shutdown_tx: watch::Sender<bool>,
    peer: crate::child_peer::PeerClass,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    handle_connection_at(stream, kernel, shutdown_tx, peer, crate::rpc_ext::RpcEndpoint::Owner).await;
}

/// Accept a project-kernel connection on the separate child-only socket.
/// Its endpoint identity is supplied by the listener, never by the peer.
pub async fn handle_connection_child<S>(
    stream: S,
    kernel: Arc<tokio::sync::RwLock<Kernel<NativePlatform>>>,
    shutdown_tx: watch::Sender<bool>,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    handle_connection_at(
        stream, kernel, shutdown_tx, crate::child_peer::PeerClass::Child,
        crate::rpc_ext::RpcEndpoint::Child,
    ).await;
}

async fn handle_connection_at<S>(
    mut stream: S,
    kernel: Arc<tokio::sync::RwLock<Kernel<NativePlatform>>>,
    shutdown_tx: watch::Sender<bool>,
    peer: crate::child_peer::PeerClass,
    endpoint: crate::rpc_ext::RpcEndpoint,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    // Read 4-byte header to detect protocol mode.
    let mut header = [0u8; 4];
    if stream.read_exact(&mut header).await.is_err() {
        return; // connection closed immediately
    }

    #[cfg(feature = "rvf-rpc")]
    if &header == b"RVFS" {
        if endpoint == crate::rpc_ext::RpcEndpoint::Child { return }
        return handle_rvf_connection(stream, kernel, shutdown_tx, peer).await;
    }

    // JSON mode: the 4 header bytes are the start of the first JSON line.
    handle_json_connection(header, stream, kernel, shutdown_tx, peer, endpoint).await;
}

/// Outcome of dispatching a single JSON-line request.
///
/// Most requests produce a single response and the connection loop
/// continues reading the next line. `ipc.subscribe_stream` and the
/// other `*.subscribe` stream RPCs are different: after the initial
/// ack is written, the write-half is owned by a streaming forwarder
/// task that pushes every matching publish as one JSON line.
enum DispatchOutcome {
    /// Keep reading subsequent requests from the same connection.
    Continue,
    /// Transport error — close the connection.
    Stop,
    /// The request subscribed to a kernel topic; the writer must be
    /// transferred into a streaming forwarder. The `rx` channel holds
    /// the raw JSON lines to flush to the client; on client disconnect
    /// (write error) the caller must call `on_disconnect` to clean up.
    StreamSubscribe {
        /// Topic that was subscribed to (for logging).
        topic: String,
        /// Receiver for serialized topic messages (JSON + trailing `\n`).
        rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
        /// Unsubscribe action to run when the client disconnects.
        on_disconnect: Box<dyn FnOnce() + Send>,
    },
}

/// Dispatch a single JSON line and write the response.
///
/// Returns the next [`DispatchOutcome`] for the connection loop.
/// Resolve the effective [`crate::capability::CallerCapabilities`]
/// for an RPC caller from the optional `auth` field on the request.
///
/// WEFT-479, amended by ADR-102 D3 / Phase 0 review R3:
///
/// - **Absent / empty**: anonymous (`{Read, Chat}`).
/// - **Token secret** (`wft_...`, issued by `auth.token.issue`): validated
///   against the kernel's [`clawft_kernel::token_authority::TokenAuthority`];
///   a live token grants the owner scope (ADR-102 D4: full surface). An
///   unknown, expired or revoked token is denied, never downgraded.
/// - **Literal scope strings** (`"admin"`, `"write,chat"`, ...): the
///   ADR-070 local-owner shortcut the CLI relies on. Honoured only when
///   the unix-socket peer has the daemon's uid (`peer_untrusted == false`).
///   The TCP relay strips them before they reach the socket
///   (`relay_auth`), so they grant nothing from a network transport.
/// - **Anything else**: denied.
async fn resolve_caller_capabilities(
    caller: &crate::rpc_ext::CallerCtx,
    kernel: &Arc<tokio::sync::RwLock<Kernel<NativePlatform>>>,
) -> crate::capability::CallerCapabilities {
    use crate::capability::{CallerCapabilities, is_literal_scope};
    let token = match caller.auth.as_deref().map(str::trim) {
        None | Some("") => return CallerCapabilities::anonymous(),
        Some(t) => t,
    };

    if token.starts_with(clawft_kernel::token_authority::SECRET_PREFIX) {
        if let Some(authority) = crate::token_rpc::authority_for(kernel).await
            && let Some(info) = authority.validate(token)
        {
            // Owner tokens carry `admin`; a project token (Phase 2 G) carries
            // write only, never admin.
            return CallerCapabilities::from_scopes(info.scope.capability_scopes().iter().copied());
        }
        tracing::warn!("rpc auth: token rejected (unknown, expired or revoked)");
        return CallerCapabilities::denied();
    }

    if is_literal_scope(token) {
        // A supervised project kernel's literal scope (ADR-070 same-uid
        // shortcut) is ignored, not refused: it is anonymous, and only its
        // project token counts (review S9). The client library stamps every
        // call with "admin", and the child's read-level parent calls
        // (`kernel.handshake`, `mesh.*`, `project.anchor.submit`) must work.
        if caller.peer_child {
            tracing::warn!("rpc auth: literal scope from a supervised project kernel's process group; treating as anonymous");
            return CallerCapabilities::anonymous();
        }
        if caller.peer_untrusted {
            tracing::warn!("rpc auth: literal scope from a peer that is not the daemon uid; denying");
            return CallerCapabilities::denied();
        }
        return CallerCapabilities::from_scopes(token.split(',').map(str::trim));
    }

    tracing::warn!("rpc auth: presented token did not match any recognised shape; denying");
    CallerCapabilities::denied()
}

/// True when the caller's credential is a live project-scoped token.
async fn project_token_scope(
    caller: &crate::rpc_ext::CallerCtx,
    kernel: &Arc<tokio::sync::RwLock<Kernel<NativePlatform>>>,
) -> bool {
    let Some(t) = caller.auth.as_deref().map(str::trim) else { return false };
    if !t.starts_with(clawft_kernel::token_authority::SECRET_PREFIX) {
        return false;
    }
    let Some(authority) = crate::token_rpc::authority_for(kernel).await else { return false };
    authority
        .validate(t)
        .is_some_and(|i| i.scope == clawft_kernel::token_authority::TokenScope::Project)
}

/// Resolve the caller's capabilities, then run the capability check and
/// extension gates (ADR-103 D0). `Err` is the refusal response.
async fn authorize_caller(
    caller: &mut crate::rpc_ext::CallerCtx,
    method: &str,
    params: &serde_json::Value,
    kernel: &Arc<tokio::sync::RwLock<Kernel<NativePlatform>>>,
) -> Result<crate::capability::CallerCapabilities, Response> {
    if caller.endpoint == crate::rpc_ext::RpcEndpoint::Child {
        if !crate::project_token_scope::child_endpoint_allows(method) {
            return Err(Response::error_with_kind(
                "child_endpoint_method_denied", format!("the child endpoint may not call {method}"),
            ));
        }
        if crate::project_token_scope::child_endpoint_requires_project_token(method)
            && !project_token_scope(caller, kernel).await
        {
            return Err(Response::error_with_kind(
                "project_token_required", format!("{method} needs a project token on the child endpoint"),
            ));
        }
    }
    // Phase 3 consideration (D3 peer credentials): a daemon under another
    // uid than the CLI user denies the CLI's implicit "admin". Say so
    // instead of a bare permission error.
    if caller.peer_untrusted
        && !caller.peer_child
        && caller
            .auth
            .as_deref()
            .is_some_and(crate::capability::is_literal_scope)
    {
        return Err(Response::error_with_kind(
            "peer_uid_mismatch",
            format!(
                "permission denied: this daemon runs as uid {}; use a token \
                 (`weft token issue` as that user) or connect as that user",
                daemon_uid_label()
            ),
        ));
    }
    // ADR-103 A6 (Phase 2 package I): the caller's verified project comes
    // from a token scope, a verified forward header or this kernel's own
    // binding, never from `Request.project`; a disagreeing claim is refused.
    caller.verified_project = crate::caller_principal::establish(caller, method, params, kernel).await?;
    // A project token may call only what its parent link calls.
    if project_token_scope(caller, kernel).await && !crate::project_token_scope::allows(method) {
        return Err(Response::error_with_kind(
            crate::project_token_scope::DENIED_KIND,
            format!("a project token may not call {method}"),
        ));
    }
    let caps = resolve_caller_capabilities(caller, kernel).await;
    crate::rpc_ext::authorize(caller, &caps, method, params, kernel).await?;
    // Only a call that passed every check above counts for the idle clock: a
    // denied or token-rejected call must not keep a project kernel awake.
    #[cfg(unix)]
    crate::project_boot_run::note_activity(method);
    Ok(caps)
}

/// This daemon's effective uid, for permission messages.
fn daemon_uid_label() -> String {
    #[cfg(unix)]
    let uid = nix::unistd::geteuid().as_raw().to_string();
    #[cfg(not(unix))]
    let uid = "(another account)".to_owned();
    uid
}

/// Dispatch a request that already passed [`authorize_caller`]: extension
/// routes first, then the legacy `match` in [`dispatch`].
async fn dispatch_after_auth(
    caller: &crate::rpc_ext::CallerCtx,
    caps: &crate::capability::CallerCapabilities,
    method: String,
    params: serde_json::Value,
    kernel: Arc<tokio::sync::RwLock<Kernel<NativePlatform>>>,
    shutdown_tx: watch::Sender<bool>,
) -> Response {
    if let Some(r) = crate::rpc_ext::dispatch_ext(caller, caps, &method, &params, &kernel).await {
        return r;
    }
    dispatch(method, params, kernel, shutdown_tx).await
}

/// Authorize then dispatch. Used by entry paths without streaming
/// intercepts (RVF frames, the in-process voice consumer).
async fn dispatch_authorized(
    caller: &crate::rpc_ext::CallerCtx,
    method: String,
    params: serde_json::Value,
    kernel: Arc<tokio::sync::RwLock<Kernel<NativePlatform>>>,
    shutdown_tx: watch::Sender<bool>,
) -> Response {
    let mut caller = caller.clone();
    let caps = match authorize_caller(&mut caller, &method, &params, &kernel).await {
        Ok(caps) => caps,
        Err(denied) => return denied,
    };
    dispatch_after_auth(&caller, &caps, method, params, kernel, shutdown_tx).await
}

async fn dispatch_json_line<W>(
    line: &str,
    kernel: &Arc<tokio::sync::RwLock<Kernel<NativePlatform>>>,
    shutdown_tx: &watch::Sender<bool>,
    writer: &mut W,
    peer: crate::child_peer::PeerClass,
    endpoint: crate::rpc_ext::RpcEndpoint,
) -> DispatchOutcome
where
    W: AsyncWriteExt + Unpin,
{
    let line = line.trim();
    if line.is_empty() {
        return DispatchOutcome::Continue;
    }

    let (response, stream_hookup) = match serde_json::from_str::<Request>(line) {
        Ok(req) => {
            let id = req.id.clone();

            // WEFT-479: per-method capability gating. Build the
            // caller's effective capability set from the optional
            // `auth` token, then refuse to dispatch if the method
            // requires a capability the caller doesn't hold.
            //
            // Default posture: anonymous = {Read, Chat}. A presented
            // token is validated against the kernel's AuthService;
            // unknown tokens map to the empty (denied) set so a
            // typo'd token can't silently fall back to anonymous.
            // ADR-103 D0: one authorization point (capability check, then
            // extension gates) for every entry path, before any streaming
            // intercept or dispatch.
            let mut caller =
                crate::rpc_ext::CallerCtx::from_request(&req).with_peer(peer).with_endpoint(endpoint);
            // ADR-103 D14: refuse an unsupported `proto` / malformed
            // `project` before anything else looks at the request.
            let (caps, denial) = if let Some(refusal) =
                crate::handshake_rpc::envelope_refusal(&req.method, req.proto, req.project.as_deref())
            {
                (crate::capability::CallerCapabilities::denied(), Some(refusal))
            } else {
                match authorize_caller(&mut caller, &req.method, &req.params, kernel).await {
                    Ok(caps) => (caps, None),
                    Err(denied) => (crate::capability::CallerCapabilities::denied(), Some(denied)),
                }
            };
            if let Some(denied) = denial {
                (denied.with_id(id), None)
            } else
            // `*.subscribe_stream` methods take over the connection: the
            // daemon registers an external sink with the router and
            // returns a receiver the caller pipes into the socket.
            if req.method == "ipc.subscribe_stream" {
                match handle_ipc_subscribe_stream(req.params, Arc::clone(kernel)).await {
                    Ok((ack, topic, rx, on_disconnect)) => {
                        (ack.with_id(id), Some((topic, rx, on_disconnect)))
                    }
                    Err(msg) => (Response::error(msg).with_id(id), None),
                }
            } else if req.method == "substrate.subscribe" {
                match handle_substrate_subscribe(req.params, Arc::clone(kernel)).await {
                    Ok((ack, path, rx, on_disconnect)) => {
                        (ack.with_id(id), Some((path, rx, on_disconnect)))
                    }
                    Err(msg) => (Response::error(msg).with_id(id), None),
                }
            } else if req.method == "chain.subscribe" {
                // ADR-103 P2 D: streaming chain tail (body in chain_subscribe_rpc).
                match crate::chain_subscribe_rpc::handle_chain_subscribe(req.params, &caller, &caps, Arc::clone(kernel)).await {
                    Ok((ack, topic, rx, on_disconnect)) => {
                        (ack.with_id(id), Some((topic, rx, on_disconnect)))
                    }
                    Err(refusal) => (refusal.with_id(id), None),
                }
            } else if req.method == "kernel.logs_stream" {
                // WEFT-434: live kernel log tail; write-half owned by
                // the stream forwarder after the ack.
                match handle_kernel_logs_stream(req.params, Arc::clone(kernel)).await {
                    Ok((ack, topic, rx, on_disconnect)) => {
                        (ack.with_id(id), Some((topic, rx, on_disconnect)))
                    }
                    Err(msg) => (Response::error(msg).with_id(id), None),
                }
            } else {
                (
                    dispatch_after_auth(
                        &caller,
                        &caps,
                        req.method,
                        req.params,
                        Arc::clone(kernel),
                        shutdown_tx.clone(),
                    )
                    .await
                    .with_id(id),
                    None,
                )
            }
        }
        Err(e) => (Response::error(format!("invalid request: {e}")), None),
    };

    let mut json = serde_json::to_string(&response).unwrap_or_else(|e| {
        serde_json::to_string(&Response::error(format!("serialize error: {e}"))).unwrap()
    });
    json.push('\n');

    if let Err(e) = writer.write_all(json.as_bytes()).await {
        debug!("write error (client disconnected?): {e}");
        if let Some((_, _, on_disconnect)) = stream_hookup {
            on_disconnect();
        }
        return DispatchOutcome::Stop;
    }

    if let Some((topic, rx, on_disconnect)) = stream_hookup {
        return DispatchOutcome::StreamSubscribe {
            topic,
            rx,
            on_disconnect,
        };
    }

    DispatchOutcome::Continue
}

/// Handle a legacy line-delimited JSON connection.
///
/// The `prefix` bytes were consumed during protocol detection and form
/// the beginning of the first JSON line on the wire.
async fn handle_json_connection<S>(
    prefix: [u8; 4],
    stream: S,
    kernel: Arc<tokio::sync::RwLock<Kernel<NativePlatform>>>,
    shutdown_tx: watch::Sender<bool>,
    peer: crate::child_peer::PeerClass,
    endpoint: crate::rpc_ext::RpcEndpoint,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (reader, mut writer) = tokio::io::split(stream);
    let mut buf = BufReader::new(reader);

    // Reconstruct the first line: the 4-byte prefix + the rest until '\n'.
    let mut rest_of_first = String::new();
    if buf.read_line(&mut rest_of_first).await.is_err() {
        return;
    }
    let first_line = format!("{}{}", String::from_utf8_lossy(&prefix), rest_of_first);
    match dispatch_json_line(&first_line, &kernel, &shutdown_tx, &mut writer, peer, endpoint).await {
        DispatchOutcome::Continue => {}
        DispatchOutcome::Stop => return,
        DispatchOutcome::StreamSubscribe {
            topic,
            rx,
            on_disconnect,
        } => {
            run_stream_subscribe(writer, topic, rx, on_disconnect).await;
            return;
        }
    }

    // Process remaining lines normally.
    loop {
        let mut line = String::new();
        match buf.read_line(&mut line).await {
            Ok(0) => break, // EOF
            Ok(_) => {}
            Err(_) => break,
        }
        match dispatch_json_line(&line, &kernel, &shutdown_tx, &mut writer, peer, endpoint).await {
            DispatchOutcome::Continue => {}
            DispatchOutcome::Stop => break,
            DispatchOutcome::StreamSubscribe {
                topic,
                rx,
                on_disconnect,
            } => {
                run_stream_subscribe(writer, topic, rx, on_disconnect).await;
                return;
            }
        }
    }
}

/// Pump serialized topic messages from `rx` into the client writer.
///
/// Runs until the receiver closes (router-side shutdown) or the
/// writer returns an I/O error (client disconnected). On exit, runs
/// the provided `on_disconnect` closure so the daemon can remove the
/// external subscription from the router.
async fn run_stream_subscribe<W>(
    mut writer: W,
    topic: String,
    mut rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    on_disconnect: Box<dyn FnOnce() + Send>,
) where
    W: AsyncWriteExt + Unpin,
{
    debug!(topic, "ipc.subscribe_stream: forwarder started");
    let _open = crate::open_streams::guard();
    while let Some(bytes) = rx.recv().await {
        if let Err(e) = writer.write_all(&bytes).await {
            debug!(topic, error = %e, "ipc.subscribe_stream: write error, closing");
            break;
        }
    }
    debug!(topic, "ipc.subscribe_stream: forwarder exiting");
    on_disconnect();
}

/// Decode a 32-byte or 64-byte key/signature from either hex or
/// standard base64. Permissive on purpose — Python bridges emit hex,
/// JS bridges often emit base64, and both are safe to accept.
fn decode_bytes(s: &str) -> Result<Vec<u8>, String> {
    let trimmed = s.trim();
    // Try hex first (even-length, all hex digits)
    if !trimmed.is_empty()
        && trimmed.len().is_multiple_of(2)
        && trimmed.chars().all(|c| c.is_ascii_hexdigit())
    {
        let mut out = Vec::with_capacity(trimmed.len() / 2);
        for i in (0..trimmed.len()).step_by(2) {
            let byte = u8::from_str_radix(&trimmed[i..i + 2], 16)
                .map_err(|e| format!("hex decode: {e}"))?;
            out.push(byte);
        }
        return Ok(out);
    }
    // Fall back to base64 (permissive: accept both URL-safe and standard).
    base64_decode_permissive(trimmed)
}

fn base64_decode_permissive(s: &str) -> Result<Vec<u8>, String> {
    // Minimal base64 decoder — sufficient for 32/64 byte keys. Accepts
    // both `+/` and `-_` alphabets. Padding `=` optional.
    let mut buf: u32 = 0;
    let mut bits: u8 = 0;
    let mut out = Vec::with_capacity((s.len() * 3) / 4 + 2);
    for c in s.chars() {
        if c == '=' {
            break;
        }
        let v: u32 = match c {
            'A'..='Z' => (c as u32) - ('A' as u32),
            'a'..='z' => (c as u32) - ('a' as u32) + 26,
            '0'..='9' => (c as u32) - ('0' as u32) + 52,
            '+' | '-' => 62,
            '/' | '_' => 63,
            '\n' | '\r' | ' ' | '\t' => continue,
            _ => return Err(format!("invalid base64 char: {c:?}")),
        };
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xFF) as u8);
        }
    }
    Ok(out)
}

/// Verify an Ed25519 signature for a given node_id against the
/// canonical signed payload. Returns Ok on valid, Err with a message
/// otherwise. Always rejects if the node_id is not registered.
///
/// Mirrors [`verify_agent_signature`] but consults the
/// [`clawft_kernel::NodeRegistry`] instead of the agent registry.
fn verify_node_signature(
    kernel: &Kernel<NativePlatform>,
    node_id: &str,
    signature_str: &str,
    payload: &[u8],
) -> Result<(), String> {
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};

    let node = kernel
        .node_registry()
        .get(node_id)
        .ok_or_else(|| format!("unknown node_id: {node_id}"))?;
    let sig_bytes = decode_bytes(signature_str).map_err(|e| format!("signature: {e}"))?;
    if sig_bytes.len() != 64 {
        return Err(format!(
            "signature must be 64 bytes, got {}",
            sig_bytes.len()
        ));
    }
    let mut sig_arr = [0u8; 64];
    sig_arr.copy_from_slice(&sig_bytes);
    let sig = Signature::from_bytes(&sig_arr);
    let vk = VerifyingKey::from_bytes(&node.pubkey)
        .map_err(|e| format!("stored pubkey invalid: {e}"))?;
    vk.verify(payload, &sig)
        .map_err(|e| format!("signature verify failed: {e}"))?;
    Ok(())
}

/// Verify an Ed25519 signature for a given agent_id against the
/// canonical signed payload. Returns Ok on valid, Err with a message
/// otherwise. Always rejects if the agent_id is not registered.
fn verify_agent_signature(
    kernel: &Kernel<NativePlatform>,
    actor_id: &str,
    signature_str: &str,
    payload: &[u8],
) -> Result<(), String> {
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};

    let agent = kernel
        .agent_registry()
        .get(actor_id)
        .ok_or_else(|| format!("unknown actor_id: {actor_id}"))?;
    let sig_bytes = decode_bytes(signature_str).map_err(|e| format!("signature: {e}"))?;
    if sig_bytes.len() != 64 {
        return Err(format!(
            "signature must be 64 bytes, got {}",
            sig_bytes.len()
        ));
    }
    let mut sig_arr = [0u8; 64];
    sig_arr.copy_from_slice(&sig_bytes);
    let sig = Signature::from_bytes(&sig_arr);
    let vk = VerifyingKey::from_bytes(&agent.pubkey)
        .map_err(|e| format!("stored pubkey invalid: {e}"))?;
    vk.verify(payload, &sig)
        .map_err(|e| format!("signature verify failed: {e}"))?;
    Ok(())
}

/// Handle `agent.register`: verify proof-of-possession, insert the
/// agent into the registry, and chain-append an `agent.registered`
/// event.
async fn handle_agent_register(
    params: serde_json::Value,
    kernel: Arc<tokio::sync::RwLock<Kernel<NativePlatform>>>,
) -> Response {
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};

    let p: crate::protocol::AgentRegisterParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => return Response::error(format!("invalid params: {e}")),
    };

    // Decode pubkey + proof.
    let pubkey_bytes = match decode_bytes(&p.pubkey) {
        Ok(b) => b,
        Err(e) => return Response::error(format!("pubkey decode: {e}")),
    };
    if pubkey_bytes.len() != 32 {
        return Response::error(format!(
            "pubkey must be 32 bytes, got {}",
            pubkey_bytes.len()
        ));
    }
    let mut pubkey_arr = [0u8; 32];
    pubkey_arr.copy_from_slice(&pubkey_bytes);

    let proof_bytes = match decode_bytes(&p.proof) {
        Ok(b) => b,
        Err(e) => return Response::error(format!("proof decode: {e}")),
    };
    if proof_bytes.len() != 64 {
        return Response::error(format!("proof must be 64 bytes, got {}", proof_bytes.len()));
    }
    let mut proof_arr = [0u8; 64];
    proof_arr.copy_from_slice(&proof_bytes);

    // Verify the proof-of-possession.
    let payload = clawft_kernel::register_payload(&p.name, &pubkey_arr, p.ts);
    let vk = match VerifyingKey::from_bytes(&pubkey_arr) {
        Ok(vk) => vk,
        Err(e) => return Response::error(format!("invalid pubkey: {e}")),
    };
    let sig = Signature::from_bytes(&proof_arr);
    if let Err(e) = vk.verify(&payload, &sig) {
        return Response::error(format!("proof-of-possession verify failed: {e}"));
    }

    let k = kernel.read().await;
    let entry = k.agent_registry().register(p.name.clone(), pubkey_arr);

    #[cfg(feature = "exochain")]
    if let Some(cm) = k.chain_manager() {
        cm.append(
            "agent",
            "agent.registered",
            Some(serde_json::json!({
                "agent_id": entry.agent_id,
                "name": entry.name,
                "pubkey_hex": hex_encode(&entry.pubkey),
                "registered_at": entry.registered_at.to_rfc3339(),
            })),
        );
    }

    let pubkey_prefix = hex_encode(&entry.pubkey)[..16.min(entry.pubkey.len() * 2)].to_string();
    k.event_log().info(
        "agent",
        format!(
            "registered agent '{}' as {} (pubkey prefix {})",
            entry.name, entry.agent_id, pubkey_prefix
        ),
    );
    info!(
        agent_id = %entry.agent_id,
        name = %entry.name,
        pubkey_prefix = %pubkey_prefix,
        "agent.register: authorized"
    );

    Response::success(
        serde_json::to_value(crate::protocol::AgentRegisterResult {
            agent_id: entry.agent_id,
            name: entry.name,
        })
        .unwrap(),
    )
}

/// Handle `node.register`: verify proof-of-possession, insert the
/// node into the registry, and chain-append a `node.registered`
/// event. Returns the deterministic node-id derived from the pubkey.
///
/// Distinct from `agent.register`: a node is a *physical thing in
/// the mesh* (ESP32, daemon, Pi), not an agent/program/user. Nodes
/// sign substrate emissions; agents sign Actions. Both share the
/// proof-of-possession shape but live in disjoint registries.
async fn handle_node_register(
    params: serde_json::Value,
    kernel: Arc<tokio::sync::RwLock<Kernel<NativePlatform>>>,
) -> Response {
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};

    let p: crate::protocol::NodeRegisterParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => return Response::error(format!("invalid params: {e}")),
    };

    // Decode pubkey + proof.
    let pubkey_bytes = match decode_bytes(&p.pubkey) {
        Ok(b) => b,
        Err(e) => return Response::error(format!("pubkey decode: {e}")),
    };
    if pubkey_bytes.len() != 32 {
        return Response::error(format!(
            "pubkey must be 32 bytes, got {}",
            pubkey_bytes.len()
        ));
    }
    let mut pubkey_arr = [0u8; 32];
    pubkey_arr.copy_from_slice(&pubkey_bytes);

    let proof_bytes = match decode_bytes(&p.proof) {
        Ok(b) => b,
        Err(e) => return Response::error(format!("proof decode: {e}")),
    };
    if proof_bytes.len() != 64 {
        return Response::error(format!("proof must be 64 bytes, got {}", proof_bytes.len()));
    }
    let mut proof_arr = [0u8; 64];
    proof_arr.copy_from_slice(&proof_bytes);

    // Verify proof-of-possession over the canonical payload.
    let payload = clawft_kernel::node_registry::node_register_payload(&pubkey_arr, p.ts, &p.label);
    let vk = match VerifyingKey::from_bytes(&pubkey_arr) {
        Ok(vk) => vk,
        Err(e) => return Response::error(format!("invalid pubkey: {e}")),
    };
    let sig = Signature::from_bytes(&proof_arr);
    if let Err(e) = vk.verify(&payload, &sig) {
        return Response::error(format!("proof-of-possession verify failed: {e}"));
    }

    let k = kernel.read().await;
    let label = if p.label.is_empty() {
        None
    } else {
        Some(p.label.clone())
    };
    let entry = k.node_registry().register(pubkey_arr, label);

    #[cfg(feature = "exochain")]
    if let Some(cm) = k.chain_manager() {
        cm.append(
            "node",
            "node.registered",
            Some(serde_json::json!({
                "node_id": entry.node_id,
                "label": entry.label,
                "pubkey_hex": hex_encode(&entry.pubkey),
                "registered_at": entry.registered_at.to_rfc3339(),
            })),
        );
    }

    k.event_log().info(
        "node",
        format!(
            "registered node {} (label={:?})",
            entry.node_id, entry.label
        ),
    );
    info!(
        node_id = %entry.node_id,
        label = ?entry.label,
        "node.register: authorized"
    );

    Response::success(
        serde_json::to_value(crate::protocol::NodeRegisterResult {
            node_id: entry.node_id,
            label: entry.label.unwrap_or_default(),
        })
        .unwrap(),
    )
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Handle `kernel.logs_stream` (WEFT-434): subscribe to the kernel
/// event log and stream new entries as line-delimited JSON after an
/// initial ack. Optionally replays a recent tail first (`count`).
///
/// Stream frames are raw [`LogEntry`] JSON objects (not `Response`
/// envelopes), matching `ipc.subscribe_stream` / `substrate.subscribe`.
async fn handle_kernel_logs_stream(
    params: serde_json::Value,
    kernel: Arc<tokio::sync::RwLock<Kernel<NativePlatform>>>,
) -> Result<
    (
        Response,
        String,
        tokio::sync::mpsc::Receiver<Vec<u8>>,
        Box<dyn FnOnce() + Send>,
    ),
    String,
> {
    let log_params: LogsParams = serde_json::from_value(params).unwrap_or_default();

    let level_filter = log_params.level.as_deref().map(|level_str| {
        match level_str {
            "debug" => clawft_kernel::LogLevel::Debug,
            "warn" | "warning" => clawft_kernel::LogLevel::Warn,
            "error" => clawft_kernel::LogLevel::Error,
            _ => clawft_kernel::LogLevel::Info,
        }
    });

    let level_rank = |l: &clawft_kernel::LogLevel| -> u8 {
        match l {
            clawft_kernel::LogLevel::Debug => 0,
            clawft_kernel::LogLevel::Info => 1,
            clawft_kernel::LogLevel::Warn => 2,
            clawft_kernel::LogLevel::Error => 3,
            // LogLevel is #[non_exhaustive]; treat unknown as Info-rank.
            _ => 1,
        }
    };
    let min_rank = level_filter.as_ref().map(level_rank);

    let k = kernel.read().await;
    let event_log = Arc::clone(k.event_log());

    // Subscribe *before* reading the tail so we never miss live
    // entries that arrive during the initial dump. The bridge task
    // de-dupes by seq so overlapping tail + live events are safe.
    //
    // KernelEventLog uses std::sync::mpsc (wasm-safe); we bridge to
    // an async channel for the socket forwarder.
    let (sub_id, entry_rx) = event_log.subscribe();

    let initial: Vec<clawft_kernel::BootEvent> = if let Some(ref min) = level_filter {
        event_log.filter_level(min, log_params.count)
    } else if log_params.count == 0 {
        Vec::new()
    } else {
        event_log.tail(log_params.count)
    };
    let initial_count = initial.len();

    let (bytes_tx, bytes_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(256);

    tokio::spawn(async move {
        let mut max_sent: u64 = 0;
        for event in initial {
            if event.seq > max_sent {
                max_sent = event.seq;
            }
            let entry = LogEntry::from_boot_event(&event);
            let Ok(mut line) = serde_json::to_string(&entry) else {
                continue;
            };
            line.push('\n');
            if bytes_tx.send(line.into_bytes()).await.is_err() {
                return;
            }
        }
        // Poll the std mpsc receiver so we stay on the async runtime
        // without blocking a worker thread on a quiet log stream.
        loop {
            match entry_rx.try_recv() {
                Ok(event) => {
                    if event.seq != 0 && event.seq <= max_sent {
                        continue;
                    }
                    if let Some(min) = min_rank
                        && level_rank(&event.level) < min
                    {
                        continue;
                    }
                    if event.seq > max_sent {
                        max_sent = event.seq;
                    }
                    let entry = LogEntry::from_boot_event(&event);
                    let Ok(mut line) = serde_json::to_string(&entry) else {
                        continue;
                    };
                    line.push('\n');
                    if bytes_tx.send(line.into_bytes()).await.is_err() {
                        return;
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
            }
        }
    });

    let log_for_cleanup = Arc::clone(&event_log);
    let on_disconnect: Box<dyn FnOnce() + Send> = Box::new(move || {
        log_for_cleanup.unsubscribe(sub_id);
    });

    // Use tracing rather than event_log so the subscribe ack itself
    // does not inject a frame into the stream the client just opened.
    debug!(
        sub_id,
        initial_count, "kernel.logs_stream: subscriber attached"
    );

    let ack = Response::success(serde_json::json!({
        "streaming": true,
        "subscriber_id": sub_id,
        "initial_count": initial_count,
    }));
    Ok((ack, "kernel.logs".into(), bytes_rx, on_disconnect))
}

/// Handle `ipc.subscribe_stream`: register an external sink with the
/// topic router and return the ack + receiver for the streaming loop.
async fn handle_ipc_subscribe_stream(
    params: serde_json::Value,
    kernel: Arc<tokio::sync::RwLock<Kernel<NativePlatform>>>,
) -> Result<
    (
        Response,
        String,
        tokio::sync::mpsc::Receiver<Vec<u8>>,
        Box<dyn FnOnce() + Send>,
    ),
    String,
> {
    let p: crate::protocol::IpcSubscribeStreamParams =
        serde_json::from_value(params).map_err(|e| format!("invalid params: {e}"))?;

    let k = kernel.read().await;

    // If the caller provided an identity, verify the signature.
    // Missing actor_id is accepted at bring-up but logged as a warn;
    // missing signature for a declared actor is unauthorized.
    if let Some(actor_id) = p.actor_id.as_ref() {
        let sig = p
            .signature
            .as_ref()
            .ok_or_else(|| "actor_id provided but signature missing".to_string())?;
        let ts = p.ts.unwrap_or(0);
        let payload = clawft_kernel::subscribe_payload(&p.topic, ts, actor_id);
        verify_agent_signature(&k, actor_id, sig, &payload)
            .map_err(|e| format!("unauthorized: {e}"))?;
    } else {
        tracing::warn!(
            topic = %p.topic,
            "ipc.subscribe_stream with no actor_id — anonymous subscribe accepted (bring-up only)"
        );
    }

    // Bounded channel; a slow external client will see drops rather
    // than backpressuring the kernel's in-process fanout path.
    let (tx, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(256);

    let router = k.a2a_router().topic_router().clone();
    let id = router.subscribe_sink(&p.topic, clawft_kernel::SubscriberSink::ExternalStream(tx));
    k.event_log().info(
        "ipc",
        format!(
            "external client subscribed to '{}' (sub_id={}, actor_id={:?})",
            p.topic, id.0, p.actor_id
        ),
    );

    let topic = p.topic.clone();
    let unsubscribe_topic = p.topic.clone();
    let router_for_cleanup = router.clone();
    let on_disconnect: Box<dyn FnOnce() + Send> = Box::new(move || {
        router_for_cleanup.unsubscribe_id(&unsubscribe_topic, id);
    });

    let ack = Response::success(serde_json::json!({
        "subscribed": p.topic,
        "subscriber_id": id.0,
        "streaming": true,
    }));
    Ok((ack, topic, rx, on_disconnect))
}

/// Handle an RVF-framed connection.
///
/// Each request/response is an RVF Meta segment with content-hash
/// integrity. Responses carry the SEALED flag; requests do not.
/// Uses the same `dispatch()` function as JSON mode.
#[cfg(feature = "rvf-rpc")]
async fn handle_rvf_connection<S>(
    stream: S,
    kernel: Arc<tokio::sync::RwLock<Kernel<NativePlatform>>>,
    shutdown_tx: watch::Sender<bool>,
    peer: crate::child_peer::PeerClass,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    use crate::rvf_codec::{RvfFrameReader, RvfFrameWriter};
    use crate::rvf_rpc;

    let (reader, writer) = tokio::io::split(stream);
    let mut frame_reader = RvfFrameReader::new(reader);
    let mut frame_writer = RvfFrameWriter::new(writer);
    let mut next_id: u64 = 1;

    loop {
        let frame = match frame_reader.read_frame().await {
            Ok(Some(f)) => f,
            Ok(None) => break, // clean EOF
            Err(e) => {
                debug!("RVF read error: {e}");
                break;
            }
        };

        let response = match rvf_rpc::decode_request(&frame) {
            Ok(req) => {
                let id = req.id.clone();
                let caller = crate::rpc_ext::CallerCtx::from_request(&req)
                    .with_peer(peer);
                if let Some(refusal) =
                    crate::handshake_rpc::envelope_refusal(&req.method, req.proto, req.project.as_deref())
                {
                    refusal.with_id(id)
                } else {
                    dispatch_authorized(
                        &caller,
                        req.method,
                        req.params,
                        Arc::clone(&kernel),
                        shutdown_tx.clone(),
                    )
                    .await
                    .with_id(id)
                }
            }
            Err(e) => Response::error(format!("invalid RVF request: {e}")),
        };

        let (seg_type, payload, flags, segment_id) = rvf_rpc::encode_response(&response, next_id);
        next_id += 1;

        if let Err(e) = frame_writer
            .write_frame(seg_type, &payload, flags, segment_id)
            .await
        {
            debug!("RVF write error: {e}");
            break;
        }
    }
}

/// Handle `substrate.subscribe`: streaming subscription over the
/// substrate service. Mirrors [`handle_ipc_subscribe_stream`].
async fn handle_substrate_subscribe(
    params: serde_json::Value,
    kernel: Arc<tokio::sync::RwLock<Kernel<NativePlatform>>>,
) -> Result<
    (
        Response,
        String,
        tokio::sync::mpsc::Receiver<Vec<u8>>,
        Box<dyn FnOnce() + Send>,
    ),
    String,
> {
    let p: crate::protocol::SubstrateSubscribeParams =
        serde_json::from_value(params).map_err(|e| format!("invalid params: {e}"))?;

    let k = kernel.read().await;

    if let Some(actor_id) = p.actor_id.as_ref() {
        let sig = p
            .signature
            .as_ref()
            .ok_or_else(|| "actor_id provided but signature missing".to_string())?;
        let ts = p.ts.unwrap_or(0);
        let payload = clawft_kernel::subscribe_payload(&p.path, ts, actor_id);
        verify_agent_signature(&k, actor_id, sig, &payload)
            .map_err(|e| format!("unauthorized: {e}"))?;
    }

    let substrate = k.substrate_service().clone();
    let (id, rx) = match substrate.subscribe(p.actor_id.as_deref(), &p.path) {
        Ok(pair) => pair,
        Err(e) => {
            flush_acl_denials_to_chain(&k);
            return Err(e.wire_message());
        }
    };
    k.event_log().info(
        "substrate",
        format!(
            "external client subscribed to '{}' (sub_id={}, actor_id={:?})",
            p.path, id.0, p.actor_id
        ),
    );

    let path_for_ack = p.path.clone();
    let path_for_cleanup = p.path.clone();
    let substrate_for_cleanup = substrate.clone();
    let on_disconnect: Box<dyn FnOnce() + Send> = Box::new(move || {
        substrate_for_cleanup.unsubscribe(&path_for_cleanup, id);
    });

    let ack = Response::success(serde_json::json!({
        "subscribed": p.path,
        "subscriber_id": id.0,
        "streaming": true,
    }));
    Ok((ack, path_for_ack, rx, on_disconnect))
}

/// Handle `substrate.read`: synchronous value + tick + sensitivity.
async fn handle_substrate_read(
    params: serde_json::Value,
    kernel: Arc<tokio::sync::RwLock<Kernel<NativePlatform>>>,
) -> Response {
    let p: crate::protocol::SubstrateReadParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => return Response::error(format!("invalid params: {e}")),
    };
    let k = kernel.read().await;
    match k.substrate_service().read(p.actor_id.as_deref(), &p.path) {
        Ok(snap) => Response::success(
            serde_json::to_value(crate::protocol::SubstrateReadResult {
                value: snap.value,
                tick: snap.tick,
                sensitivity: snap.sensitivity.as_str().to_string(),
            })
            .unwrap(),
        ),
        Err(e) => {
            // Flush authenticated ACL denials onto the ExoChain
            // (ADR-057 / ADR-022: substrate.read.denied).
            flush_acl_denials_to_chain(&k);
            Response::error(e.wire_message())
        }
    }
}

/// Handle `substrate.list`: enumerate children of a prefix up to `depth`.
///
/// Wire shape (Phase 1 §3.1):
///
/// ```json
/// // request
/// { "prefix": "substrate/sensor", "depth": 1 }
///
/// // response
/// {
///   "children": [
///     { "path": "substrate/sensor/mic", "has_value": true,  "child_count": 0 },
///     { "path": "substrate/sensor/tof", "has_value": true,  "child_count": 0 }
///   ],
///   "tick": 42
/// }
/// ```
///
/// Same egress gating as `substrate.read` — the prefix itself is
/// checked once, and capture-tier descendants are hidden from
/// anonymous callers so path names don't leak.
async fn handle_substrate_list(
    params: serde_json::Value,
    kernel: Arc<tokio::sync::RwLock<Kernel<NativePlatform>>>,
) -> Response {
    let p: crate::protocol::SubstrateListParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => return Response::error(format!("invalid params: {e}")),
    };
    let k = kernel.read().await;
    match k
        .substrate_service()
        .list(p.actor_id.as_deref(), &p.prefix, p.depth)
    {
        Ok(snap) => {
            let result = crate::protocol::SubstrateListResult {
                children: snap
                    .children
                    .into_iter()
                    .map(|c| crate::protocol::SubstrateListChild {
                        path: c.path,
                        has_value: c.has_value,
                        child_count: c.child_count,
                    })
                    .collect(),
                tick: snap.tick,
            };
            Response::success(serde_json::to_value(result).unwrap())
        }
        Err(e) => {
            flush_acl_denials_to_chain(&k);
            Response::error(e.wire_message())
        }
    }
}

/// Append any buffered ADR-057 ACL denial events to the local ExoChain.
fn flush_acl_denials_to_chain(k: &Kernel<NativePlatform>) {
    let events = k.substrate_service().take_acl_denials();
    if events.is_empty() {
        return;
    }
    #[cfg(feature = "exochain")]
    if let Some(cm) = k.chain_manager() {
        for ev in events {
            cm.append("substrate-acl", &ev.kind, Some(ev.payload));
        }
        return;
    }
    // Without exochain, denials were already recorded in the service
    // buffer and drained here as a no-op (best-effort).
    let _ = events;
}

/// Handle `substrate.publish`: verify node signature, enforce the
/// `substrate/<node-id>/...` write prefix, Replace the path's value
/// and fan out.
///
/// Every publish must be node-attributed and signed. Unsigned
/// publishes are rejected — there is no anonymous-publish bypass.
/// The actor-side fields (`actor_id` / `signature` / `ts`) are
/// reserved for the Actions pipeline and ignored here.
async fn handle_substrate_publish(
    params: serde_json::Value,
    kernel: Arc<tokio::sync::RwLock<Kernel<NativePlatform>>>,
) -> Response {
    let p: crate::protocol::SubstratePublishParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => return Response::error(format!("invalid params: {e}")),
    };

    // Hard requirement: every write is node-attributed.
    let node_id = match p.node_id.as_ref() {
        Some(n) => n.clone(),
        None => {
            return Response::error(
                "substrate.publish: node_id required (every write must be node-attributed)"
                    .to_string(),
            );
        }
    };
    let signature = match p.node_signature.as_ref() {
        Some(s) => s.clone(),
        None => {
            return Response::error(
                "substrate.publish: node_signature required when node_id is set".to_string(),
            );
        }
    };
    let node_ts = p.node_ts.unwrap_or(0);

    let k = kernel.read().await;

    // Verify the node signature over the canonical payload.
    let value_bytes = serde_json::to_vec(&p.value).unwrap_or_default();
    let value_str = String::from_utf8_lossy(&value_bytes);
    let payload = clawft_kernel::node_publish_payload(&p.path, &value_str, node_ts, &node_id);
    if let Err(e) = verify_node_signature(&k, &node_id, &signature, &payload) {
        return Response::error(format!("unauthorized: {e}"));
    }

    // Run the publish through the node-identity gate. Rejects writes
    // outside `substrate/<node-id>/...` (node-private tier rule).
    // Mesh-canonical writes (`substrate/_derived/...`) require a
    // separate capability path that isn't wired yet — they will fall
    // through this branch and get rejected, which is correct for
    // this phase.
    let tick = match k
        .substrate_service()
        .publish_gated(Some(&node_id), &p.path, p.value)
    {
        Ok(t) => t,
        Err(e) => return Response::error(format!("gate denied: {e}")),
    };
    k.event_log().info(
        "substrate",
        format!("publish {} tick={} node={}", p.path, tick, node_id),
    );
    if tick == 1 {
        info!(
            path = %p.path,
            node = %node_id,
            "substrate.publish: stream started (first window on path)"
        );
    } else {
        debug!(path = %p.path, node = %node_id, tick, "substrate.publish");
    }
    Response::success(serde_json::json!({
        "path": p.path,
        "tick": tick,
    }))
}

/// Handle `node.identity`: report this daemon's own node-id +
/// label + registration timestamp.
///
/// Lets a remote node (the ESP32 firmware) discover the daemon's
/// node-id at runtime instead of hardcoding it. Used to build
/// control-path prefixes:
/// `substrate/<node_identity.node_id>/control/sensors/...`.
async fn handle_node_identity(
    _params: serde_json::Value,
    kernel: Arc<tokio::sync::RwLock<Kernel<NativePlatform>>>,
) -> Response {
    let state = match daemon_control() {
        Some(s) => s,
        None => {
            return Response::error("daemon control state not initialized".to_string());
        }
    };
    // Look up the daemon's NodeRegistry entry to surface label +
    // registered_at — these are the fields the firmware Claude
    // requested in the dialog.
    let k = kernel.read().await;
    let entry = match k.node_registry().get(&state.daemon_node_id) {
        Some(e) => e,
        None => {
            return Response::error(format!(
                "daemon node {} not in registry (boot inconsistency)",
                state.daemon_node_id
            ));
        }
    };
    Response::success(
        serde_json::to_value(crate::protocol::NodeIdentityResult {
            node_id: entry.node_id,
            label: entry.label.unwrap_or_default(),
            registered_at: entry.registered_at.to_rfc3339(),
        })
        .unwrap(),
    )
}

/// WEFT-331: substrate publisher for interactive defer prompts.
///
/// Writes an `awaiting_defer` stream frame (so the progressive chat
/// panel lights up) and the dedicated `chat_defer_path` value the
/// panel can poll independently of stream phase.
struct DaemonDeferPublisher {
    kernel: Arc<tokio::sync::RwLock<Kernel<NativePlatform>>>,
}

#[async_trait::async_trait]
impl clawft_service_agent::DeferPromptPublisher for DaemonDeferPublisher {
    async fn publish_prompt(&self, prompt: &clawft_types::agent_chat::DeferPromptEvent) {
        // Stream frame — panel already polls this path during chat_stream.
        let frame = crate::protocol::AgentChatStreamFrame::awaiting_defer(
            // High seq so we don't rewind an earlier generating frame;
            // panel treats equal seq as idempotent refresh.
            1_000_000,
            prompt.clone(),
        );
        publish_chat_stream_frame(&self.kernel, &prompt.conv_id, frame).await;

        // Dedicated defer path for non-stream consumers / late joiners.
        let path = crate::protocol::chat_defer_path(&prompt.conv_id);
        let value = match serde_json::to_value(prompt) {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "agent.chat defer: prompt serialize failed");
                return;
            }
        };
        let Some(ctrl) = daemon_control() else {
            warn!("agent.chat defer: daemon control not wired; skip prompt publish");
            return;
        };
        let k = self.kernel.read().await;
        if let Err(e) = k.substrate_service().publish_gated_with_grants(
            Some(&ctrl.daemon_node_id),
            &path,
            value,
            k.node_registry(),
        ) {
            warn!(
                error = %e,
                path = %path,
                conv_id = %prompt.conv_id,
                "agent.chat defer: prompt publish failed"
            );
        }
    }

    async fn clear_prompt(&self, conv_id: &str, defer_id: &str) {
        let path = crate::protocol::chat_defer_path(conv_id);
        let value = serde_json::json!({
            "cleared": true,
            "defer_id": defer_id,
        });
        let Some(ctrl) = daemon_control() else {
            return;
        };
        let k = self.kernel.read().await;
        if let Err(e) = k.substrate_service().publish_gated_with_grants(
            Some(&ctrl.daemon_node_id),
            &path,
            value,
            k.node_registry(),
        ) {
            warn!(
                error = %e,
                path = %path,
                conv_id,
                "agent.chat defer: clear publish failed"
            );
        }
    }
}

/// Publish an [`AgentChatStreamFrame`] to
/// `substrate/_derived/chat/<conv>/stream` under the daemon's chat
/// DerivedWriteGrant (WEFT-253). Best-effort: publish failures are
/// logged and swallowed so a substrate hiccup never fails the chat
/// turn itself.
async fn publish_chat_stream_frame(
    kernel: &Arc<tokio::sync::RwLock<Kernel<NativePlatform>>>,
    conv_id: &str,
    mut frame: crate::protocol::AgentChatStreamFrame,
) {
    // Stamp wall-clock if the caller left it unset.
    if frame.ts_ms.is_none() {
        frame.ts_ms = Some(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
        );
    }
    let path = crate::protocol::chat_stream_path(conv_id);
    let value = match serde_json::to_value(&frame) {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, conv_id, "agent.chat_stream: frame serialize failed");
            return;
        }
    };
    let Some(ctrl) = daemon_control() else {
        warn!(conv_id, "agent.chat_stream: daemon control not wired; skip frame");
        return;
    };
    let k = kernel.read().await;
    if let Err(e) = k.substrate_service().publish_gated_with_grants(
        Some(&ctrl.daemon_node_id),
        &path,
        value,
        k.node_registry(),
    ) {
        warn!(
            error = %e,
            conv_id,
            path = %path,
            "agent.chat_stream: stream-frame publish failed"
        );
    }
}

/// Build a typed `agent.chat` / `agent.chat_stream` error response
/// (WEFT-334).
///
/// Populates both the legacy string `error` field
/// (`"{method}: {message}"`) and the structured `error_kind`
/// discriminator so panels can branch without substring-matching.
fn agent_chat_err_response(
    method: &str,
    err: crate::protocol::AgentChatError,
) -> Response {
    Response::error_with_kind(err.error_kind(), err.legacy_message(method))
}

/// Handle `agent.chat_stream` (WEFT-253).
///
/// Same request / final-response shape as `agent.chat`, plus progressive
/// frames on `substrate/_derived/chat/<conv>/stream` so the chat panel
/// can render tokens as they arrive (and show a phase label while the
/// model is still thinking). The progressive channel is substrate — not
/// a connection-takeover RPC — so native and WASM panel transports both
/// consume it via the existing substrate poll / `substrate.read` path.
///
/// Until the agent loop exposes mid-generation token callbacks, the
/// daemon:
/// 1. Publishes a `thinking` frame at turn start.
/// 2. Runs the ordinary `AgentService::dispatch`.
/// 3. Cascades the final assistant text as successive `generating`
///    frames (word-ish chunks) so the panel paints a typewriter effect
///    while this RPC is still open — concurrent `substrate.read`
///    polls see the growth.
/// 4. Publishes a terminal `done` frame, then returns the full
///    [`AgentChatResult`] (identical to `agent.chat`).
async fn handle_agent_chat_stream(
    params: serde_json::Value,
    kernel: Arc<tokio::sync::RwLock<Kernel<NativePlatform>>>,
) -> Response {
    let Some(agent) = daemon_agent() else {
        return agent_chat_err_response(
            "agent.chat_stream",
            crate::protocol::AgentChatError::not_wired(
                "agent service not wired \
                 (LLM client init failed at boot — \
                 check daemon log for 'llm client init failed')",
            ),
        );
    };
    let params: AgentChatParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => {
            return agent_chat_err_response(
                "agent.chat_stream",
                crate::protocol::AgentChatError::invalid_params(format!(
                    "invalid params: {e}"
                )),
            );
        }
    };
    let conv_id = params.conv_id.clone();
    let stream_path = crate::protocol::chat_stream_path(&conv_id);

    // Frame 0: thinking — panel heartbeat / stream bubble can light up
    // before the first model token (or the first cascade chunk).
    publish_chat_stream_frame(
        &kernel,
        &conv_id,
        crate::protocol::AgentChatStreamFrame::thinking(0),
    )
    .await;

    match agent.dispatch(params).await {
        Ok(result) => {
            if let Some(activity) = DAEMON_CONV_ACTIVITY.get() {
                activity.insert(conv_id.clone(), std::time::Instant::now());
            }

            // Cascade progressive frames with delta + speakable units
            // (WEFT-350) so voice TTS can start sentence-by-sentence
            // while the panel paints tokens. Word-ish chunks keep
            // frame count bounded; SpeakableTracker closes on .!?
            let full = result.assistant_text.clone();
            let frames = clawft_types::agent_chat::cascade_stream_frames(&full, 8);
            for frame in frames {
                publish_chat_stream_frame(&kernel, &conv_id, frame).await;
                // Yield so concurrent substrate.read polls (panel / TTS
                // consumer) can observe intermediate frames. 4ms × N/8
                // chunks keeps a 400-char reply under ~200ms of cascade.
                tokio::time::sleep(std::time::Duration::from_millis(4)).await;
            }

            match serde_json::to_value(&result) {
                Ok(mut v) => {
                    // Echo stream_path so panels don't have to re-derive
                    // it (and so older panels can detect stream support
                    // from the response shape alone).
                    if let Some(obj) = v.as_object_mut() {
                        obj.insert(
                            "stream_path".into(),
                            serde_json::Value::String(stream_path),
                        );
                        obj.insert("streamed".into(), serde_json::Value::Bool(true));
                    }
                    Response::success(v)
                }
                Err(e) => agent_chat_err_response(
                    "agent.chat_stream",
                    crate::protocol::AgentChatError::internal(format!("{e}")),
                ),
            }
        }
        Err(e) => {
            let chat_err = e.to_chat_error();
            let msg = chat_err.legacy_message("agent.chat_stream");
            publish_chat_stream_frame(
                &kernel,
                &conv_id,
                crate::protocol::AgentChatStreamFrame::done_err(1, msg.clone()),
            )
            .await;
            agent_chat_err_response("agent.chat_stream", chat_err)
        }
    }
}

/// Handle `llm.prompt`: synchronous chat completion against the local
/// LLM service.
///
/// This is the V1 shape — one RPC, full completion in the response.
/// Streaming is a deferred follow-up that lands as `llm.prompt_stream`
/// using the same connection-takeover pattern as
/// `substrate.subscribe`, not as a breaking change to this method.
///
/// Behaviour:
/// 1. If the `llm` control flag is `false`, fast-fail with a clear
///    error so the GUI's disable toggle has the same source-cuts
///    semantic as for sensors.
/// 2. If the daemon's LLM client wasn't wired at boot (init error or
///    feature absent), return a clean "service unavailable" instead
///    of panicking.
/// 3. Build a [`ChatMessage`] vector from the params (`messages`
///    wins over `prompt`; optional `system` is prepended only when
///    `messages` doesn't already carry one).
/// 4. Forward to [`LlmClient::complete`]; map errors back to RPC
///    errors with the same string formatting `LlmError::Display`
///    uses, so the wire surface stays diagnosable.
async fn handle_llm_prompt(
    params: serde_json::Value,
    _kernel: Arc<tokio::sync::RwLock<Kernel<NativePlatform>>>,
) -> Response {
    let p: crate::protocol::LlmPromptParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => return Response::error(format!("invalid params: {e}")),
    };

    // Honor the `llm` control flag. Source-cuts: if disabled, we
    // never even reach the HTTP client.
    if let Some(state) = daemon_control()
        && let Some(flag) = state.flags.get(ControlKind::Service, "llm")
        && !flag.load(std::sync::atomic::Ordering::SeqCst)
    {
        return Response::error("llm service is disabled (toggle on via control.set_enabled)");
    }

    let client = match daemon_llm() {
        Some(c) => c,
        None => {
            return Response::error(
                "llm service not initialized (check daemon boot log for 'llm client init failed')",
            );
        }
    };

    let mut messages: Vec<clawft_service_llm::ChatMessage> = match (p.messages, p.prompt.clone()) {
        (Some(msgs), _) => msgs
            .into_iter()
            .map(|m| clawft_service_llm::ChatMessage {
                role: m.role,
                // RPC schema is still plain string content; wrap into
                // MessageContent::Text. Vision/block content can land
                // via a future RPC schema bump.
                content: clawft_service_llm::MessageContent::Text(m.content),
                // The daemon's `llm.prompt` RPC predates tool-call
                // support and accepts only role+content from clients;
                // tool fields stay None until a future RPC schema bump
                // exposes them.
                tool_calls: None,
                tool_call_id: None,
            })
            .collect(),
        (None, Some(prompt)) => vec![clawft_service_llm::ChatMessage::user(prompt)],
        (None, None) => {
            return Response::error("invalid params: must supply `prompt` or `messages`");
        }
    };
    if messages.is_empty() {
        return Response::error("invalid params: messages was empty");
    }
    // Prepend a system prompt only when the caller didn't already
    // provide one. Avoids stomping a deliberately-set conversation
    // shape.
    if let Some(sys) = p.system
        && !sys.is_empty()
        && !messages
            .first()
            .map(|m| m.role == "system")
            .unwrap_or(false)
    {
        messages.insert(0, clawft_service_llm::ChatMessage::system(sys));
    }

    // WEFT-343: read lock across the HTTP call so a concurrent swap
    // waits for in-flight work and never observes a half-swapped client.
    let guard = client.read().await;
    match guard.complete(messages, p.temperature, p.max_tokens).await {
        Ok(resp) => {
            let first = &resp.choices[0]; // complete() rejects empty choices upstream
            let result = crate::protocol::LlmPromptResult {
                // Flatten block content → string for the RPC result
                // shape (completion is still a plain String).
                completion: first.message.content.as_text().into_owned(),
                finish_reason: first.finish_reason.clone(),
                prompt_tokens: resp.usage.prompt_tokens,
                completion_tokens: resp.usage.completion_tokens,
                model: resp.model.clone(),
            };
            Response::success(serde_json::to_value(result).unwrap())
        }
        Err(e) => Response::error(format!("llm.prompt: {e}")),
    }
}

/// Infer a short provider label from the LLM base URL (WEFT-256).
fn llm_provider_label(base_url: &str) -> &'static str {
    let lower = base_url.to_ascii_lowercase();
    if lower.contains("openrouter") {
        "openrouter"
    } else if lower.contains("openai.com") {
        "openai"
    } else if lower.contains("anthropic") {
        "anthropic"
    } else if lower.contains("127.0.0.1")
        || lower.contains("localhost")
        || lower.contains("0.0.0.0")
    {
        "local"
    } else {
        "custom"
    }
}

/// `llm.models` — enumerate models available to the chat panel chip
/// strip (WEFT-256). Always returns at least the configured default
/// when the client is wired; live `/v1/models` results are merged in
/// when the probe succeeds.
async fn handle_llm_models(
    _params: serde_json::Value,
    _kernel: Arc<tokio::sync::RwLock<Kernel<NativePlatform>>>,
) -> Response {
    let client = match daemon_llm() {
        Some(c) => c,
        None => {
            return Response::error(
                "llm service not initialized (check daemon boot log for 'llm client init failed')",
            );
        }
    };

    // WEFT-343: snapshot config under the read lock, then probe models
    // with the same guard so a mid-call swap cannot split the view.
    let guard = client.read().await;
    let default_model = guard.config().model.clone();
    let base_url = guard.config().base_url.clone();
    let default_provider = llm_provider_label(&base_url).to_string();

    let (live_ids, probe_error) = match guard.list_models().await {
        Ok(ids) => (ids, None),
        Err(e) => (Vec::new(), Some(e.to_string())),
    };

    // Build a de-duplicated ordered list: default first, then live ids.
    let mut seen = std::collections::HashSet::new();
    let mut models = Vec::new();

    let push = |models: &mut Vec<crate::protocol::LlmModelEntry>,
                seen: &mut std::collections::HashSet<String>,
                id: String,
                provider: &str,
                is_default: bool,
                live: bool| {
        if id.is_empty() || !seen.insert(id.clone()) {
            return;
        }
        models.push(crate::protocol::LlmModelEntry {
            label: id.clone(),
            id,
            provider: provider.to_string(),
            is_default,
            live,
        });
    };

    push(
        &mut models,
        &mut seen,
        default_model.clone(),
        &default_provider,
        true,
        false,
    );
    for id in live_ids {
        if id == default_model {
            // Already present as the default entry — just mark it live.
            if let Some(entry) = models.iter_mut().find(|m| m.id == default_model) {
                entry.live = true;
            }
            continue;
        }
        push(
            &mut models,
            &mut seen,
            id,
            &default_provider,
            false,
            true,
        );
    }

    let result = crate::protocol::LlmModelsResult {
        default_model,
        default_provider,
        base_url,
        models,
        probe_error,
    };
    Response::success(serde_json::to_value(result).unwrap())
}

// ── Terminal (PTY) RPC handlers ─────────────────────────────────────
//
// All four handlers share two preconditions:
// 1. `DAEMON_TERMINAL` is set (boot wiring did its job).
// 2. The supplied `session_id` resolves to a live session. Spawn is the
//    exception — it allocates the id rather than receiving one.
//
// Output flows the other way: a tokio task started by `terminal.spawn`
// drains the session's `mpsc::UnboundedReceiver<TerminalEvent>` and
// publishes each chunk to
// `substrate/<daemon-node>/derived/terminal/<session_id>` via
// `publish_gated`. Surfaces poll that path through the existing
// `substrate.read` cascade — no separate streaming RPC.

/// Handle `terminal.spawn`: allocate a PTY, spawn a shell, start the
/// substrate publish pump, and return the session id + resolved shell
/// metadata.
async fn handle_terminal_spawn(
    params: serde_json::Value,
    kernel: Arc<tokio::sync::RwLock<Kernel<NativePlatform>>>,
) -> Response {
    let p: crate::protocol::TerminalSpawnParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => return Response::error(format!("invalid params: {e}")),
    };
    let mgr = match daemon_terminal() {
        Some(m) => m,
        None => return Response::error("terminal service not initialized".to_string()),
    };
    let id = match mgr.spawn(p.rows, p.cols, p.shell.clone(), p.cwd.clone()) {
        Ok(id) => id,
        Err(e) => return Response::error(format!("terminal.spawn: {e}")),
    };
    let session = match mgr.session(&id) {
        Some(s) => s,
        None => {
            return Response::error(
                "terminal.spawn: session disappeared between spawn and lookup".to_string(),
            );
        }
    };

    // Drain the session's output channel onto substrate. The reader
    // pump runs on its own OS thread inside the service crate; this
    // tokio task just forwards events.
    //
    // Authority: the daemon's own node-id, so `publish_gated` accepts
    // the write under its node-private prefix.
    let daemon_node_id = match daemon_control() {
        Some(s) => s.daemon_node_id.clone(),
        None => return Response::error("daemon control state not initialized".to_string()),
    };
    let output_path = format!("substrate/{}/derived/terminal/{}", daemon_node_id, id);
    let resolved_shell = session.shell().to_string();
    let resolved_cwd = session.cwd().to_string();

    if let Some(mut events) = session.take_events() {
        let substrate = {
            let k = kernel.read().await;
            k.substrate_service().clone()
        };
        let publish_path = output_path.clone();
        let publish_node = daemon_node_id.clone();
        let session_id_for_log = id.clone();
        tokio::spawn(async move {
            use base64::Engine;
            let b64 = base64::engine::general_purpose::STANDARD;
            while let Some(ev) = events.recv().await {
                let chunk = match ev {
                    clawft_service_terminal::TerminalEvent::Output(bytes) => {
                        crate::protocol::TerminalChunk {
                            data: b64.encode(&bytes),
                            ts_ms: crate::control::now_ms(),
                            exit: false,
                        }
                    }
                    clawft_service_terminal::TerminalEvent::Exit => {
                        crate::protocol::TerminalChunk {
                            data: String::new(),
                            ts_ms: crate::control::now_ms(),
                            exit: true,
                        }
                    }
                };
                let value = match serde_json::to_value(&chunk) {
                    Ok(v) => v,
                    Err(e) => {
                        warn!(error = %e, "terminal: chunk serialize failed");
                        continue;
                    }
                };
                if let Err(e) = substrate.publish_gated(Some(&publish_node), &publish_path, value) {
                    warn!(error = %e, path = %publish_path, "terminal: publish_gated failed");
                }
                if chunk.exit {
                    debug!(session_id = %session_id_for_log, "terminal: drain task exiting on Exit chunk");
                    break;
                }
            }
            debug!(session_id = %session_id_for_log, "terminal: drain task ended");
        });
    } else {
        warn!(session_id = %id, "terminal: take_events returned None — output won't be published");
    }

    let result = crate::protocol::TerminalSpawnResult {
        session_id: id.to_string(),
        rows: if p.rows == 0 {
            clawft_service_terminal::DEFAULT_ROWS
        } else {
            p.rows
        },
        cols: if p.cols == 0 {
            clawft_service_terminal::DEFAULT_COLS
        } else {
            p.cols
        },
        shell: resolved_shell,
        cwd: resolved_cwd,
        output_path,
    };
    Response::success(serde_json::to_value(result).unwrap())
}

/// Handle `terminal.write`: forward base64-decoded bytes into the PTY.
async fn handle_terminal_write(
    params: serde_json::Value,
    _kernel: Arc<tokio::sync::RwLock<Kernel<NativePlatform>>>,
) -> Response {
    let p: crate::protocol::TerminalWriteParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => return Response::error(format!("invalid params: {e}")),
    };
    let mgr = match daemon_terminal() {
        Some(m) => m,
        None => return Response::error("terminal service not initialized".to_string()),
    };
    use base64::Engine;
    let bytes = match base64::engine::general_purpose::STANDARD.decode(p.data.as_bytes()) {
        Ok(b) => b,
        Err(e) => return Response::error(format!("terminal.write: bad base64: {e}")),
    };
    match mgr.write(
        &clawft_service_terminal::SessionId::from(p.session_id.as_str()),
        &bytes,
    ) {
        Ok(()) => Response::success(
            serde_json::to_value(crate::protocol::TerminalAck { ok: true }).unwrap(),
        ),
        Err(e) => Response::error(format!("terminal.write: {e}")),
    }
}

/// Handle `terminal.resize`: reflow the PTY for an in-shell app.
async fn handle_terminal_resize(
    params: serde_json::Value,
    _kernel: Arc<tokio::sync::RwLock<Kernel<NativePlatform>>>,
) -> Response {
    let p: crate::protocol::TerminalResizeParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => return Response::error(format!("invalid params: {e}")),
    };
    let mgr = match daemon_terminal() {
        Some(m) => m,
        None => return Response::error("terminal service not initialized".to_string()),
    };
    match mgr.resize(
        &clawft_service_terminal::SessionId::from(p.session_id.as_str()),
        p.rows,
        p.cols,
    ) {
        Ok(()) => Response::success(
            serde_json::to_value(crate::protocol::TerminalAck { ok: true }).unwrap(),
        ),
        Err(e) => Response::error(format!("terminal.resize: {e}")),
    }
}

/// Handle `terminal.close`: kill the child shell, drop the PTY, forget
/// the session. Idempotent — closing an unknown session is `ok`.
async fn handle_terminal_close(
    params: serde_json::Value,
    _kernel: Arc<tokio::sync::RwLock<Kernel<NativePlatform>>>,
) -> Response {
    let p: crate::protocol::TerminalCloseParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => return Response::error(format!("invalid params: {e}")),
    };
    let mgr = match daemon_terminal() {
        Some(m) => m,
        None => return Response::error("terminal service not initialized".to_string()),
    };
    match mgr.close(&clawft_service_terminal::SessionId::from(
        p.session_id.as_str(),
    )) {
        Ok(()) => Response::success(
            serde_json::to_value(crate::protocol::TerminalAck { ok: true }).unwrap(),
        ),
        Err(e) => Response::error(format!("terminal.close: {e}")),
    }
}

/// Handle `control.set_enabled`: flip a daemon-side enable flag and
/// republish the substrate control intent under the daemon's own
/// prefix.
///
/// Body: `{ kind: "service" | "sensor", target: <string>, enabled:
/// <bool>, label?: <string> }`. The handler:
///
/// 1. Updates the in-memory `Arc<AtomicBool>` registered for the
///    target. If no flag was registered (typo / unknown target)
///    the call is rejected with `400`-flavored error.
/// 2. Publishes a fresh `ControlIntent` value at
///    `substrate/<daemon-node>/control/<kind>s/<target>` so
///    subscribers (the GUI, future firmware) see the change.
async fn handle_control_set_enabled(
    params: serde_json::Value,
    kernel: Arc<tokio::sync::RwLock<Kernel<NativePlatform>>>,
) -> Response {
    let p: crate::protocol::ControlSetEnabledParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => return Response::error(format!("invalid params: {e}")),
    };
    let kind = match ControlKind::parse(&p.kind) {
        Some(k) => k,
        None => {
            return Response::error(format!(
                "unknown kind {:?} (want \"service\" or \"sensor\")",
                p.kind
            ));
        }
    };
    let state = match daemon_control() {
        Some(s) => s,
        None => return Response::error("daemon control state not initialized".to_string()),
    };
    if state.flags.set(kind, &p.target, p.enabled).is_none() {
        return Response::error(format!(
            "no flag registered for kind={:?} target={:?}",
            p.kind, p.target
        ));
    }

    // WEFT-343: any control.set_enabled cycle on the llm service
    // re-reads env + [kernel.llm] and swaps the shared LlmClient so a
    // rotated OPENROUTER_API_KEY / LLM_SERVICE_URL is picked up without
    // a daemon restart. Best-effort: flag flip already succeeded; a
    // failed rebuild is logged and reported but does not roll back.
    if kind == ControlKind::Service && p.target == "llm" {
        match refresh_daemon_llm_from_env(&kernel).await {
            Ok(()) => {}
            Err(e) => {
                warn!(
                    error = %e,
                    "control.set_enabled(llm): env refresh / client swap failed \
                     (flag was still flipped; prior client remains active)"
                );
            }
        }
    }

    // Publish the new intent under the daemon's own prefix. Failure
    // here doesn't roll back the in-memory flip — the in-memory
    // flag IS the source of truth for daemon-side enforcement, and
    // the substrate value is the eventual-consistency mirror.
    let intent = ControlIntent {
        enabled: p.enabled,
        kind,
        target: p.target.clone(),
        label: p.label.clone().unwrap_or_default(),
        updated_at_ms: crate::control::now_ms(),
    };
    let path = crate::control::intent_path(&state.daemon_node_id, kind, &p.target);
    let publish_result = {
        let k = kernel.read().await;
        k.substrate_service()
            .publish_gated(Some(&state.daemon_node_id), &path, intent.to_value())
    };
    if let Err(e) = publish_result {
        warn!(
            error = %e,
            path = %path,
            "control: substrate mirror publish failed (in-memory flag was still flipped)"
        );
    }
    info!(
        kind = ?kind,
        target = %p.target,
        enabled = p.enabled,
        "control: flag set"
    );

    Response::success(
        serde_json::to_value(crate::protocol::ControlSetEnabledResult {
            path,
            enabled: p.enabled,
        })
        .unwrap(),
    )
}

/// Handle `control.list`: snapshot every registered control flag.
/// Used by the Explorer to populate a "Controls" overview without
/// walking substrate.
async fn handle_control_list(
    _params: serde_json::Value,
    _kernel: Arc<tokio::sync::RwLock<Kernel<NativePlatform>>>,
) -> Response {
    let state = match daemon_control() {
        Some(s) => s,
        None => return Response::error("daemon control state not initialized".to_string()),
    };
    let entries: Vec<_> = state
        .flags
        .list()
        .into_iter()
        .map(
            |(kind, target, enabled)| crate::protocol::ControlListEntry {
                kind: match kind {
                    ControlKind::Service => "service".to_string(),
                    ControlKind::Sensor => "sensor".to_string(),
                },
                target,
                enabled,
            },
        )
        .collect();
    Response::success(serde_json::to_value(crate::protocol::ControlListResult { entries }).unwrap())
}

/// Handle `substrate.canonical_publish_payload`: diagnostic — return
/// the exact bytes the daemon would feed to the signature verifier
/// for a hypothetical publish. No signature check, no actual write.
///
/// Lets remote nodes (the ESP32 firmware especially) sanity-check
/// their canonical-payload buffer against what the daemon expects,
/// so byte-drift bugs become a one-shot diff instead of a guessing
/// game.
async fn handle_substrate_canonical_publish_payload(
    params: serde_json::Value,
    _kernel: Arc<tokio::sync::RwLock<Kernel<NativePlatform>>>,
) -> Response {
    let p: crate::protocol::SubstrateCanonicalPublishPayloadParams =
        match serde_json::from_value(params) {
            Ok(p) => p,
            Err(e) => return Response::error(format!("invalid params: {e}")),
        };

    // Same canonicalization the publish handler does: re-serialize
    // the value through serde_json::Value, which alphabetizes object
    // keys. This is the load-bearing step — the surface the firmware
    // most often gets wrong.
    let value_bytes = match serde_json::to_vec(&p.value) {
        Ok(b) => b,
        Err(e) => return Response::error(format!("value re-serialize: {e}")),
    };
    let canonical_value_json = String::from_utf8_lossy(&value_bytes).into_owned();

    let payload =
        clawft_kernel::node_publish_payload(&p.path, &canonical_value_json, p.node_ts, &p.node_id);

    let payload_hex = hex_encode(&payload);
    let payload_len = payload.len();

    Response::success(
        serde_json::to_value(crate::protocol::SubstrateCanonicalPublishPayloadResult {
            payload_hex,
            payload_len,
            canonical_value_json,
        })
        .unwrap(),
    )
}

/// Handle `substrate.notify`: signal-only pulse, no payload change.
async fn handle_substrate_notify(
    params: serde_json::Value,
    kernel: Arc<tokio::sync::RwLock<Kernel<NativePlatform>>>,
) -> Response {
    let p: crate::protocol::SubstrateNotifyParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => return Response::error(format!("invalid params: {e}")),
    };
    let k = kernel.read().await;
    let tick = k.substrate_service().notify(p.actor_id.as_deref(), &p.path);
    Response::success(serde_json::json!({
        "path": p.path,
        "tick": tick,
    }))
}

/// Dispatch a request to the appropriate handler.
///
/// Takes owned values to ensure the future is `Send + 'static`
/// for use inside `tokio::spawn`.
async fn dispatch(
    method: String,
    params: serde_json::Value,
    kernel: Arc<tokio::sync::RwLock<Kernel<NativePlatform>>>,
    shutdown_tx: watch::Sender<bool>,
) -> Response {
    match method.as_str() {
        "kernel.status" => {
            let k = kernel.read().await;
            let state_str = match k.state() {
                KernelState::Booting => "booting",
                KernelState::Running => "running",
                KernelState::ShuttingDown => "shutting_down",
                KernelState::Halted => "halted",
                _ => "unknown",
            };
            let result = KernelStatusResult {
                state: state_str.to_owned(),
                uptime_secs: k.uptime().as_secs_f64(),
                process_count: k.process_table().len(),
                service_count: k.services().len(),
                max_processes: k.kernel_config().max_processes,
                health_check_interval_secs: k.kernel_config().health_check_interval_secs,
                // Build provenance baked into this `weaver` binary at compile
                // time (crates/clawft-weave/build.rs). The CLI compares `sha`
                // against its own stamp to catch stale binary/daemon pairings.
                build: crate::protocol::BuildStamp {
                    sha: env!("BUILD_GIT_HASH").to_owned(),
                    timestamp: env!("BUILD_TIMESTAMP").to_owned(),
                    version: env!("CARGO_PKG_VERSION").to_owned(),
                },
                handshake: Some(crate::handshake_rpc::current_handshake(&k)),
                shared_services: crate::project_profile::shared_services_health(),
                #[cfg(feature = "mesh")]
                leaf_listeners: k.a2a_router().mesh_runtime().and_then(|rt| rt.leaf_bound()).map(|b| {
                    crate::protocol::LeafListeners { tcp: b.tcp.to_string(), discovery: b.discovery.to_string() }
                }),
                #[cfg(feature = "mesh")]
                leaf_listener_list: k.a2a_router().mesh_runtime().map(|rt| rt.leaf_bounds()).unwrap_or_default().into_iter().map(|b| {
                    crate::protocol::LeafListeners { tcp: b.tcp.to_string(), discovery: b.discovery.to_string() }
                }).collect(),
                #[cfg(not(feature = "mesh"))]
                leaf_listeners: None,
                #[cfg(not(feature = "mesh"))]
                leaf_listener_list: Vec::new(),
            };
            Response::success(serde_json::to_value(result).unwrap())
        }
        "kernel.ps" => {
            let k = kernel.read().await;
            // Sample the daemon's own process memory once per request.
            // The in-kernel process_table doesn't periodically poll RSS
            // (zero-cost-by-default); reading it on demand here gives
            // the Processes panel something real to show without
            // adding a periodic sampler. Linux: /proc/self/status
            // VmRSS is the resident set size in KB.
            let daemon_rss_bytes = read_self_rss_bytes();
            let mut entries: Vec<ProcessInfo> = k
                .process_table()
                .list()
                .iter()
                .map(|e| ProcessInfo {
                    // WEFT-416: stable row key for substrate per-id deltas.
                    row_id: crate::protocol::process_row_id_for_pid(e.pid),
                    pid: e.pid,
                    agent_id: e.agent_id.clone(),
                    state: e.state.to_string(),
                    // process_table's resource_usage is normally zero
                    // because no sampler updates it. Substitute the
                    // daemon RSS for the root kernel entry (lowest
                    // pid) so the user sees a real number on at
                    // least one row; child rows fall back to the
                    // tracked value (zero today).
                    memory_bytes: e.resource_usage.memory_bytes,
                    cpu_time_ms: e.resource_usage.cpu_time_ms,
                    parent_pid: e.parent_pid,
                })
                .collect();

            // Attribute the daemon's RSS to the lowest-pid (root)
            // entry. This is honest: services run inside the same
            // address space as the kernel, so the daemon RSS IS the
            // kernel's real memory footprint.
            if let Some(min_pid) = entries.iter().map(|e| e.pid).min()
                && let Some(row) = entries.iter_mut().find(|e| e.pid == min_pid)
                && row.memory_bytes == 0
            {
                row.memory_bytes = daemon_rss_bytes;
            }

            // Merge in registered kernel services as virtual processes.
            // They run in-process under the daemon and aren't tracked by
            // the OS process_table, but the user expects to see them in
            // Processes alongside actual kernel-level entries. Synthesise
            // a ProcessInfo per service with state derived from
            // health_check; pid is the daemon's own pid (they live
            // there); parent_pid points at the root kernel process so
            // the table renders as a tree under it.
            //
            // Memory: services share the daemon's address space, so a
            // truly accurate per-service RSS is unobtainable from the
            // OS. We surface the daemon's measured RSS apportioned
            // evenly across the in-daemon entries (kernel root + each
            // service) so the user sees real, non-zero numbers on
            // every row instead of a misleading 0. The apportionment
            // is documented in the column header on the GUI side; the
            // alternative ("0 bytes" everywhere) is what the user
            // surfaced as a defect.
            let services = k.services().list();
            let daemon_pid = std::process::id() as u64;
            let root_pid: Option<u64> = entries.iter().map(|e| e.pid).min();
            for (name, _stype) in services {
                let state = match k.services().get(&name) {
                    Some(svc) => match svc.health_check().await {
                        clawft_kernel::HealthStatus::Healthy => "running",
                        clawft_kernel::HealthStatus::Degraded(_) => "degraded",
                        clawft_kernel::HealthStatus::Unhealthy(_) => "failed",
                        clawft_kernel::HealthStatus::Unknown => "unknown",
                        _ => "unknown",
                    },
                    None => "unknown",
                };
                entries.push(ProcessInfo {
                    // WEFT-416: svc: prefix — these share daemon_pid.
                    row_id: crate::protocol::process_row_id_for_service(&name),
                    pid: daemon_pid,
                    agent_id: name,
                    state: state.to_string(),
                    memory_bytes: 0,
                    cpu_time_ms: 0,
                    parent_pid: root_pid,
                });
            }

            // Apportion the daemon RSS evenly across every row that is
            // hosted in the daemon's address space (kernel root +
            // services). Rows in entries.iter_mut() are still in
            // pre-sort order; we just count rows with memory_bytes==0
            // (after the kernel-root assignment above) plus the root
            // itself, then divide. This keeps the column sum bounded
            // by the actual daemon RSS instead of multiplying it by
            // the row count.
            let in_daemon_rows = entries
                .iter()
                .filter(|e| e.pid == daemon_pid || Some(e.pid) == root_pid)
                .count() as u64;
            if daemon_rss_bytes > 0 && in_daemon_rows > 0 {
                let share = daemon_rss_bytes / in_daemon_rows;
                let remainder = daemon_rss_bytes % in_daemon_rows;
                let mut first = true;
                for row in entries.iter_mut() {
                    if row.pid == daemon_pid || Some(row.pid) == root_pid {
                        // Round the remainder into the first row so the
                        // apportioned values still sum exactly to the
                        // observed RSS. Only override rows that are
                        // currently 0 OR carry the un-apportioned full
                        // RSS from the kernel-root assignment above.
                        let new_val = if first { share + remainder } else { share };
                        first = false;
                        if row.memory_bytes == 0 || row.memory_bytes == daemon_rss_bytes {
                            row.memory_bytes = new_val;
                        }
                    }
                }
            }

            entries.sort_by_key(|e| e.pid);
            Response::success(serde_json::to_value(entries).unwrap())
        }
        "kernel.services" => {
            let k = kernel.read().await;
            let services = k.services().list();
            // For each registered service, run its health probe and
            // map the result to a user-facing lifecycle state. Match
            // on the enum variant (NOT a substring of Debug output —
            // "unhealthy" contains "healthy"). Healthy → running,
            // Degraded → degraded, Unhealthy → failed, Unknown →
            // unknown. The Services UI and any non-UI client read
            // off `state`.
            let daemon_pid = std::process::id() as u64;
            // Approximate uptime: time since the kernel itself booted.
            // The in-memory ServiceRegistry doesn't track per-service
            // started_at today (registration happens at boot, before
            // start_all). For services that boot with the kernel —
            // cron, assessment, containers, ecc.cognitive_tick — the
            // kernel-uptime approximation is correct to within the boot
            // window. A future patch can add per-service started_at to
            // the registry for services started post-boot.
            let kernel_uptime_ms = k.uptime().as_millis() as u64;
            let restart_counts = service_restart_counts();
            let restart_snapshot: std::collections::HashMap<String, u64> =
                restart_counts.lock().map(|m| m.clone()).unwrap_or_default();
            let mut infos: Vec<ServiceInfo> = Vec::with_capacity(services.len());
            for (name, stype) in services {
                let (state, health_label, detail) = match k.services().get(&name) {
                    Some(svc) => {
                        let h = svc.health_check().await;
                        // WEFT-333: optional metrics/detail (e.g. agent.chat
                        // last_completion / in_flight / lock_contentions).
                        let detail = svc.health_detail();
                        let (state, label) = match &h {
                            clawft_kernel::HealthStatus::Healthy => {
                                ("running", "healthy".to_string())
                            }
                            clawft_kernel::HealthStatus::Degraded(msg) => {
                                ("degraded", format!("degraded: {msg}"))
                            }
                            clawft_kernel::HealthStatus::Unhealthy(msg) => {
                                ("failed", format!("unhealthy: {msg}"))
                            }
                            clawft_kernel::HealthStatus::Unknown => {
                                ("unknown", "unknown".to_string())
                            }
                            _ => ("unknown", "unknown".to_string()),
                        };
                        (state.to_string(), label, detail)
                    }
                    None => ("unknown".to_string(), "registered".to_string(), None),
                };
                let restarts = restart_snapshot.get(&name).copied().unwrap_or(0);
                let uptime_ms = if state == "running" {
                    kernel_uptime_ms
                } else {
                    0
                };
                infos.push(ServiceInfo {
                    // WEFT-416: row_id == name (unique in registry).
                    row_id: name.clone(),
                    name,
                    service_type: stype.to_string(),
                    state,
                    health: health_label,
                    pid: Some(daemon_pid),
                    restarts,
                    uptime_ms,
                    detail,
                });
            }
            Response::success(serde_json::to_value(infos).unwrap())
        }
        // Service lifecycle RPCs — what the desktop Services panel and
        // the `weft service` CLI submit when the user clicks
        // [start] / [stop] / [restart]. Each looks the service up by
        // name in the in-memory ServiceRegistry and calls the matching
        // SystemService trait method. A missing service or a failing
        // start/stop/restart returns Response::error so the next
        // health-check tick reflects reality.
        verb @ ("service.start" | "service.stop" | "service.restart") => {
            let name = params
                .get("name")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let Some(name) = name else {
                return Response::error(format!("{verb} requires {{name: <string>}}"));
            };
            let k = kernel.read().await;
            let Some(svc) = k.services().get(&name) else {
                return Response::error(format!("service not registered: {name}"));
            };
            let result = match verb {
                "service.start" => svc.start().await,
                "service.stop" => svc.stop().await,
                "service.restart" => {
                    // Bump restart counter regardless of stop outcome —
                    // the user requested a restart; that's the intent.
                    if let Ok(mut m) = service_restart_counts().lock() {
                        *m.entry(name.clone()).or_insert(0) += 1;
                    }
                    match svc.stop().await {
                        // Restart = stop, then start. A failure to stop
                        // is not fatal — many services have idempotent
                        // stop() and proceeding to start anyway is the
                        // "reload" semantics most users expect.
                        Ok(()) => svc.start().await,
                        Err(e) => {
                            tracing::warn!(
                                service = %name, error = %e,
                                "stop failed during restart; trying start anyway"
                            );
                            svc.start().await
                        }
                    }
                }
                _ => unreachable!(),
            };
            match result {
                Ok(()) => Response::success(serde_json::json!({
                    "name": name,
                    "verb": verb,
                    "ok": true,
                })),
                Err(e) => Response::error(format!("{verb}: {e}")),
            }
        }
        "kernel.logs" => {
            let log_params: LogsParams = serde_json::from_value(params).unwrap_or(LogsParams {
                count: 50,
                level: None,
            });

            let k = kernel.read().await;
            let event_log = k.event_log();

            let events = if let Some(ref level_str) = log_params.level {
                let level = match level_str.as_str() {
                    "debug" => clawft_kernel::LogLevel::Debug,
                    "warn" | "warning" => clawft_kernel::LogLevel::Warn,
                    "error" => clawft_kernel::LogLevel::Error,
                    _ => clawft_kernel::LogLevel::Info,
                };
                event_log.filter_level(&level, log_params.count)
            } else {
                event_log.tail(log_params.count)
            };

            let entries: Vec<LogEntry> = events.iter().map(LogEntry::from_boot_event).collect();

            Response::success(serde_json::to_value(entries).unwrap())
        }
        "kernel.shutdown" => {
            // Log the shutdown event before signaling
            {
                let k = kernel.read().await;
                k.event_log().info("daemon", "shutdown requested via RPC");
            }
            info!("shutdown requested via RPC");
            let _ = shutdown_tx.send(true);
            Response::success(serde_json::json!("shutting down"))
        }
        // ── M1.5.1a: admin-app write verbs (ADR-015 influences) ────
        //
        // These are the two verbs the WeftOS Admin surface declares
        // influence over in `crates/clawft-app/fixtures/weftos-admin.toml`
        // and binds as affordances on the process table + service
        // gauges in `crates/clawft-surface/fixtures/weftos-admin-desktop.toml`.
        //
        // Governance intersection is stubbed at the compositor level
        // (ADR-006 §2 — honest governance lands with M2's active-radar
        // loop). The daemon's own capability check still applies via
        // `k.supervisor()` + `k.services()`.
        "kernel.kill-process" => {
            let pid = match params.get("pid").and_then(|v| v.as_u64()) {
                Some(p) => p,
                None => {
                    return Response::error(
                        "kernel.kill-process: missing or non-integer `pid`".to_string(),
                    );
                }
            };
            let graceful = params
                .get("graceful")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let k = kernel.read().await;
            match k.supervisor().stop(pid, graceful) {
                Ok(()) => {
                    k.event_log().info(
                        "kernel",
                        format!(
                            "kill-process PID {pid} ({})",
                            if graceful { "graceful" } else { "force" }
                        ),
                    );
                    Response::success(serde_json::json!({ "pid": pid, "graceful": graceful }))
                }
                Err(e) => Response::error(format!("kill-process failed: {e}")),
            }
        }
        "kernel.restart-service" => {
            let name = match params.get("name").and_then(|v| v.as_str()) {
                Some(n) if !n.is_empty() => n.to_string(),
                _ => {
                    return Response::error(
                        "kernel.restart-service: missing or empty `name`".to_string(),
                    );
                }
            };
            let k = kernel.read().await;
            // Preferred path: service has an owner agent, restart via
            // supervisor (re-uses the full agent-restart lineage).
            if let Some(pid) = k.services().resolve_target(&name) {
                match k.supervisor().restart(pid) {
                    Ok(result) => {
                        let _ = k
                            .process_table()
                            .update_state(result.pid, clawft_kernel::ProcessState::Running);
                        k.event_log().info(
                            "kernel",
                            format!("restart-service {name} (PID {pid} -> {})", result.pid),
                        );
                        return Response::success(serde_json::json!({
                            "name": name,
                            "old_pid": pid,
                            "new_pid": result.pid,
                        }));
                    }
                    Err(e) => {
                        return Response::error(format!("restart-service failed: {e}"));
                    }
                }
            }
            // Fallback path: SystemService without an owner agent (e.g.
            // external / infrastructure services). Stop then start.
            if let Some(svc) = k.services().get(&name) {
                if let Err(e) = svc.stop().await {
                    return Response::error(format!("restart-service stop failed: {e}"));
                }
                if let Err(e) = svc.start().await {
                    return Response::error(format!("restart-service start failed: {e}"));
                }
                k.event_log()
                    .info("kernel", format!("restart-service {name} (no owner pid)"));
                return Response::success(serde_json::json!({ "name": name }));
            }
            Response::error(format!("restart-service: unknown service `{name}`"))
        }
        "cluster.status" => {
            let k = kernel.read().await;
            let membership = k.cluster_membership();
            let active = membership.count_by_state(&clawft_kernel::NodeState::Active);
            let total = membership.len();

            let result = ClusterStatusResult {
                total_nodes: total,
                healthy_nodes: active,
                total_shards: 0,
                active_shards: 0,
                consensus_enabled: false,
            };
            Response::success(serde_json::to_value(result).unwrap())
        }
        "cluster.nodes" => {
            let k = kernel.read().await;
            let membership = k.cluster_membership();
            let peers = membership.list_peers();
            let nodes: Vec<ClusterNodeInfo> = peers
                .iter()
                .map(|(id, state, platform)| {
                    ClusterNodeInfo::from_peer(id, state, platform, membership.get_peer(id))
                })
                .collect();
            Response::success(serde_json::to_value(nodes).unwrap())
        }
        // mesh-placement-03: signed node facts (local + verified peers).
        #[cfg(any(feature = "mesh", feature = "exochain"))]
        "cluster.facts" => crate::node_facts_rpc::handle(params, kernel).await,
        // ADR-120: the exposure registry (list, switch, log).
        "expose.list" | "expose.set" | "expose.off_all" | "expose.log" => {
            crate::expose_rpc::handle(method.as_str(), params).await
        }
        // ADR-122: the cog data plane broker's topics.
        #[cfg(all(feature = "placement", unix))]
        "cogbus.list" => crate::cogbus_serve::handle(method.as_str(), params).await,
        // mesh-placement-19: inference placement (off unless inference.json exists).
        #[cfg(all(feature = "placement", unix))]
        "infer.status" | "infer.expose" | "infer.allow" | "infer.start" | "infer.stop" => {
            crate::infer_rpc::handle(method.as_str(), params).await
        }
        "cluster.join" => {
            let join_params: ClusterJoinParams = match serde_json::from_value(params) {
                Ok(p) => p,
                Err(e) => return Response::error(format!("invalid params: {e}")),
            };

            let k = kernel.read().await;
            let membership = k.cluster_membership();
            let node_id = uuid::Uuid::new_v4().to_string();
            let platform = match join_params.platform.as_str() {
                "browser" => clawft_kernel::NodePlatform::Browser,
                "edge" => clawft_kernel::NodePlatform::Edge,
                "wasi" => clawft_kernel::NodePlatform::Wasi,
                _ => clawft_kernel::NodePlatform::CloudNative,
            };
            let peer = clawft_kernel::PeerNode {
                id: node_id.clone(),
                name: join_params.name.unwrap_or_else(|| node_id.clone()),
                platform,
                state: clawft_kernel::NodeState::Active,
                address: join_params.address,
                first_seen: chrono::Utc::now(),
                last_heartbeat: chrono::Utc::now(),
                capabilities: Vec::new(),
                labels: std::collections::HashMap::new(),
            };
            match membership.add_peer(peer) {
                Ok(()) => Response::success(serde_json::json!({ "node_id": node_id })),
                Err(e) => Response::error(format!("join failed: {e}")),
            }
        }
        "cluster.leave" => {
            let leave_params: ClusterLeaveParams = match serde_json::from_value(params) {
                Ok(p) => p,
                Err(e) => return Response::error(format!("invalid params: {e}")),
            };

            let k = kernel.read().await;
            let membership = k.cluster_membership();
            match membership.remove_peer(&leave_params.node_id) {
                Ok(_) => Response::success(serde_json::json!("ok")),
                Err(e) => Response::error(format!("leave failed: {e}")),
            }
        }
        "cluster.health" => {
            let k = kernel.read().await;
            let membership = k.cluster_membership();
            let peers = membership.list_peers();
            let health: Vec<serde_json::Value> = peers
                .iter()
                .map(|(id, state, _)| {
                    serde_json::json!({
                        "node_id": id,
                        "healthy": matches!(state, clawft_kernel::NodeState::Active),
                        "state": state.to_string(),
                    })
                })
                .collect();
            Response::success(serde_json::to_value(health).unwrap())
        }
        "cluster.shards" => {
            // Shards are only available with the cluster feature
            Response::success(serde_json::json!([]))
        }
        "chain.status" => {
            #[cfg(feature = "exochain")]
            {
                let k = kernel.read().await;
                if let Some(cm) = k.chain_manager() {
                    let status = cm.status();
                    let hash_hex: String = status
                        .last_hash
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect();
                    let result = ChainStatusResult {
                        chain_id: status.chain_id,
                        sequence: status.sequence,
                        event_count: status.event_count,
                        checkpoint_count: status.checkpoint_count,
                        events_since_checkpoint: status.events_since_checkpoint,
                        last_hash: hash_hex,
                    };
                    Response::success(serde_json::to_value(result).unwrap())
                } else {
                    Response::error("chain not enabled")
                }
            }
            #[cfg(not(feature = "exochain"))]
            Response::error("exochain feature not enabled")
        }
        // `chain.tail` is the panel/extension-side name; `chain.local` is
        // the older console-side name. Same handler — the alias keeps the
        // panel's witness-chain view (and any other UI consumers) wired
        // without forcing a frontend rebuild. Without this match arm the
        // panel hits `unknown method: chain.tail` and renders blank even
        // when the chain is happily ticking new events.
        "chain.local" | "chain.tail" => {
            #[cfg(feature = "exochain")]
            {
                let local_params: ChainLocalParams =
                    serde_json::from_value(params).unwrap_or(ChainLocalParams { count: 20 });
                let k = kernel.read().await;
                if let Some(cm) = k.chain_manager() {
                    let events = cm.tail(local_params.count);
                    let infos: Vec<ChainEventInfo> = events
                        .iter()
                        .map(|e| {
                            let hash_hex: String =
                                e.hash.iter().map(|b| format!("{b:02x}")).collect();
                            // Build condensed detail from payload
                            let detail = match &e.payload {
                                Some(p) => {
                                    let mut parts = Vec::new();
                                    if let Some(v) = p.get("agent_id").and_then(|v| v.as_str()) {
                                        parts.push(format!("agent={v}"));
                                    }
                                    if let Some(v) = p.get("pid").and_then(|v| v.as_u64()) {
                                        parts.push(format!("pid={v}"));
                                    }
                                    if let Some(v) = p.get("from").and_then(|v| v.as_u64()) {
                                        parts.push(format!("from={v}"));
                                    }
                                    if let Some(v) = p.get("target").and_then(|v| v.as_str()) {
                                        parts.push(format!("to={v}"));
                                    }
                                    if let Some(v) = p.get("exit_code").and_then(|v| v.as_i64()) {
                                        parts.push(format!("exit={v}"));
                                    }
                                    if let Some(v) = p.get("job_name").and_then(|v| v.as_str()) {
                                        parts.push(format!("job={v}"));
                                    }
                                    if let Some(v) = p.get("payload_type").and_then(|v| v.as_str())
                                    {
                                        parts.push(format!("type={v}"));
                                    }
                                    if let Some(v) = p.get("state").and_then(|v| v.as_str()) {
                                        parts.push(format!("state={v}"));
                                    }
                                    if parts.is_empty() {
                                        // Fallback: show first 60 chars of payload
                                        crate::fleet_labels::truncate_chars(&p.to_string(), 60)
                                    } else {
                                        parts.join(" ")
                                    }
                                }
                                None => String::new(),
                            };
                            ChainEventInfo {
                                sequence: e.sequence,
                                chain_id: e.chain_id,
                                timestamp: e.timestamp.to_rfc3339(),
                                source: e.source.clone(),
                                kind: e.kind.clone(),
                                hash: hash_hex,
                                detail,
                            }
                        })
                        .collect();
                    Response::success(serde_json::to_value(infos).unwrap())
                } else {
                    Response::error("chain not enabled")
                }
            }
            #[cfg(not(feature = "exochain"))]
            Response::error("exochain feature not enabled")
        }
        "chain.checkpoint" => {
            #[cfg(feature = "exochain")]
            {
                let k = kernel.read().await;
                if let Some(cm) = k.chain_manager() {
                    let cp = cm.checkpoint();
                    Response::success(serde_json::to_value(cp).unwrap())
                } else {
                    Response::error("chain not enabled")
                }
            }
            #[cfg(not(feature = "exochain"))]
            Response::error("exochain feature not enabled")
        }
        "chain.verify" => {
            #[cfg(feature = "exochain")]
            {
                let k = kernel.read().await;
                if let Some(cm) = k.chain_manager() {
                    let result = cm.verify_integrity();

                    // Also verify RVF signature if chain has a signing key.
                    let signature_verified = if let Some(pubkey) = cm.verifying_key() {
                        let cfg = k.kernel_config().chain.clone().unwrap_or_default();
                        if let Some(ref ckpt_path) = cfg.effective_checkpoint_path() {
                            let rvf_path =
                                std::path::PathBuf::from(ckpt_path).with_extension("rvf");
                            if rvf_path.exists() {
                                match clawft_kernel::ChainManager::verify_rvf_signature(
                                    &rvf_path, &pubkey,
                                ) {
                                    Ok(valid) => Some(valid),
                                    Err(_) => Some(false),
                                }
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    } else {
                        None
                    };

                    let proto_result = ChainVerifyResult {
                        valid: result.valid,
                        event_count: result.event_count,
                        errors: result.errors,
                        signature_verified,
                    };
                    Response::success(serde_json::to_value(proto_result).unwrap())
                } else {
                    Response::error("chain not enabled")
                }
            }
            #[cfg(not(feature = "exochain"))]
            Response::error("exochain feature not enabled")
        }
        "chain.export" => {
            #[cfg(feature = "exochain")]
            {
                let export_params: ChainExportParams =
                    serde_json::from_value(params).unwrap_or(ChainExportParams {
                        format: "json".into(),
                        output: None,
                    });
                let k = kernel.read().await;
                if let Some(cm) = k.chain_manager() {
                    match export_params.format.as_str() {
                        "rvf" => {
                            let default_path =
                                protocol::runtime_paths().chain_dir().join("export.rvf");
                            let output_path = export_params
                                .output
                                .map(std::path::PathBuf::from)
                                .unwrap_or(default_path);
                            match cm.save_to_rvf(&output_path) {
                                Ok(()) => Response::success(serde_json::json!({
                                    "format": "rvf",
                                    "path": output_path.display().to_string(),
                                })),
                                Err(e) => Response::error(format!("RVF export failed: {e}")),
                            }
                        }
                        _ => {
                            let events = cm.tail(0);
                            let infos: Vec<ChainEventInfo> = events
                                .iter()
                                .map(|e| {
                                    let hash_hex: String =
                                        e.hash.iter().map(|b| format!("{b:02x}")).collect();
                                    ChainEventInfo {
                                        sequence: e.sequence,
                                        chain_id: e.chain_id,
                                        timestamp: e.timestamp.to_rfc3339(),
                                        source: e.source.clone(),
                                        kind: e.kind.clone(),
                                        hash: hash_hex,
                                        detail: String::new(),
                                    }
                                })
                                .collect();
                            Response::success(serde_json::to_value(infos).unwrap())
                        }
                    }
                } else {
                    Response::error("chain not enabled")
                }
            }
            #[cfg(not(feature = "exochain"))]
            Response::error("exochain feature not enabled")
        }
        // WEFT-324: public chain.append for WitnessRecord (soul promote +
        // future agent journal). Gated Write capability at the dispatch
        // envelope; handler proxies to ChainManager::append.
        "chain.append" => {
            #[cfg(feature = "exochain")]
            {
                let append_params: ChainAppendParams = match serde_json::from_value(params) {
                    Ok(p) => p,
                    Err(e) => {
                        return Response::error(format!("invalid chain.append params: {e}"));
                    }
                };
                if append_params.record.kind.trim().is_empty() {
                    return Response::error("chain.append: record.kind must be non-empty");
                }
                if clawft_kernel::project_identity::is_caller_reserved_source(&append_params.source) {
                    return Response::error_with_kind(
                        "reserved_source",
                        "chain.append: this source is reserved for the daemon's own events \
                         (identity, anchors, governance, project, supervisor)",
                    );
                }
                let k = kernel.read().await;
                if let Some(cm) = k.chain_manager() {
                    let payload = match serde_json::to_value(&append_params.record) {
                        Ok(v) => Some(v),
                        Err(e) => {
                            return Response::error(format!(
                                "chain.append: failed to serialize record: {e}"
                            ));
                        }
                    };
                    let event = cm.append(
                        &append_params.source,
                        &append_params.record.kind,
                        payload,
                    );
                    let hash_hex: String =
                        event.hash.iter().map(|b| format!("{b:02x}")).collect();
                    let result = ChainAppendResult {
                        sequence: event.sequence,
                        chain_id: event.chain_id,
                        hash: hash_hex,
                        source: event.source,
                        kind: event.kind,
                    };
                    Response::success(serde_json::to_value(result).unwrap())
                } else {
                    Response::error("chain not enabled")
                }
            }
            #[cfg(not(feature = "exochain"))]
            Response::error("exochain feature not enabled")
        }
        "resource.tree" => {
            #[cfg(feature = "exochain")]
            {
                let k = kernel.read().await;
                if let Some(tm) = k.tree_manager() {
                    let tree = tm.tree().lock().unwrap();
                    let mut nodes: Vec<ResourceNodeInfo> = tree
                        .iter()
                        .map(|(id, node)| {
                            let hash_hex: String = node
                                .merkle_hash
                                .iter()
                                .map(|b| format!("{b:02x}"))
                                .collect();
                            ResourceNodeInfo {
                                id: id.to_string(),
                                kind: format!("{:?}", node.kind),
                                parent: node.parent.as_ref().map(|p| p.to_string()),
                                children: node.children.iter().map(|c| c.to_string()).collect(),
                                metadata: serde_json::to_value(&node.metadata).unwrap_or_default(),
                                merkle_hash: hash_hex,
                                scoring: None, // tree listing omits scoring for brevity
                            }
                        })
                        .collect();
                    nodes.sort_by(|a, b| a.id.cmp(&b.id));
                    Response::success(serde_json::to_value(nodes).unwrap())
                } else {
                    Response::error("resource tree not enabled")
                }
            }
            #[cfg(not(feature = "exochain"))]
            Response::error("exochain feature not enabled")
        }
        "resource.inspect" => {
            #[cfg(feature = "exochain")]
            {
                let inspect_params: ResourceInspectParams = match serde_json::from_value(params) {
                    Ok(p) => p,
                    Err(e) => return Response::error(format!("invalid params: {e}")),
                };
                let k = kernel.read().await;
                if let Some(tm) = k.tree_manager() {
                    let rid = exo_resource_tree::ResourceId::new(&inspect_params.path);
                    // Extract node data under lock, then drop lock before get_scoring
                    let node_data = {
                        let tree = tm.tree().lock().unwrap();
                        tree.get(&rid).map(|node| {
                            let hash_hex: String = node
                                .merkle_hash
                                .iter()
                                .map(|b| format!("{b:02x}"))
                                .collect();
                            (
                                node.id.to_string(),
                                format!("{:?}", node.kind),
                                node.parent.as_ref().map(|p| p.to_string()),
                                node.children
                                    .iter()
                                    .map(|c| c.to_string())
                                    .collect::<Vec<_>>(),
                                serde_json::to_value(&node.metadata).unwrap_or_default(),
                                hash_hex,
                            )
                        })
                    }; // tree lock dropped here
                    if let Some((id, kind, parent, children, metadata, hash_hex)) = node_data {
                        // Now safe to call get_scoring (it acquires its own lock)
                        let scoring = tm.get_scoring(&rid).map(|s| {
                            let composite = s.trust * 0.25
                                + s.performance * 0.20
                                + s.reliability * 0.20
                                + s.velocity * 0.15
                                + s.reward * 0.10
                                + (1.0 - s.difficulty) * 0.10;
                            ResourceScoreResult {
                                path: inspect_params.path.clone(),
                                trust: s.trust,
                                performance: s.performance,
                                difficulty: s.difficulty,
                                reward: s.reward,
                                reliability: s.reliability,
                                velocity: s.velocity,
                                composite,
                            }
                        });
                        let info = ResourceNodeInfo {
                            id,
                            kind,
                            parent,
                            children,
                            metadata,
                            merkle_hash: hash_hex,
                            scoring,
                        };
                        Response::success(serde_json::to_value(info).unwrap())
                    } else {
                        Response::error(format!("resource not found: {}", inspect_params.path))
                    }
                } else {
                    Response::error("resource tree not enabled")
                }
            }
            #[cfg(not(feature = "exochain"))]
            Response::error("exochain feature not enabled")
        }
        "resource.stats" => {
            #[cfg(feature = "exochain")]
            {
                let k = kernel.read().await;
                if let Some(tm) = k.tree_manager() {
                    let tree = tm.tree().lock().unwrap();
                    let hash_hex: String = tree
                        .root_hash()
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect();
                    let mut namespaces = 0usize;
                    let mut services = 0usize;
                    let mut agents = 0usize;
                    let mut devices = 0usize;
                    for (_, node) in tree.iter() {
                        match node.kind {
                            exo_resource_tree::ResourceKind::Namespace => namespaces += 1,
                            exo_resource_tree::ResourceKind::Service => services += 1,
                            exo_resource_tree::ResourceKind::Agent => agents += 1,
                            exo_resource_tree::ResourceKind::Device => devices += 1,
                            _ => {}
                        }
                    }
                    let result = ResourceStatsResult {
                        total_nodes: tree.len(),
                        root_hash: hash_hex,
                        namespaces,
                        services,
                        agents,
                        devices,
                    };
                    Response::success(serde_json::to_value(result).unwrap())
                } else {
                    Response::error("resource tree not enabled")
                }
            }
            #[cfg(not(feature = "exochain"))]
            Response::error("exochain feature not enabled")
        }
        "agent.register" => handle_agent_register(params, kernel).await,
        "node.register" => handle_node_register(params, kernel).await,
        "node.identity" => handle_node_identity(params, kernel).await,
        "substrate.read" => handle_substrate_read(params, kernel).await,
        "substrate.list" => handle_substrate_list(params, kernel).await,
        "substrate.publish" => handle_substrate_publish(params, kernel).await,
        "substrate.canonical_publish_payload" => {
            handle_substrate_canonical_publish_payload(params, kernel).await
        }
        "substrate.notify" => handle_substrate_notify(params, kernel).await,
        "control.set_enabled" => handle_control_set_enabled(params, kernel).await,
        "control.list" => handle_control_list(params, kernel).await,
        "llm.prompt" => handle_llm_prompt(params, kernel).await,
        // WEFT-256: model/provider enumeration for the chat chip strip.
        "llm.models" => handle_llm_models(params, kernel).await,
        "agent.chat" => {
            // agent-core-v1 Phase D3 (the cutover): every request goes
            // through `clawft-service-agent::AgentService::dispatch`.
            // The C2 spike path is gone; if the service didn't wire
            // (LLM init failed at boot) we surface a typed error.
            // WEFT-497 removed the empty `agent-core-chat` feature flag
            // after soak — no `#[cfg]` branches remained.
            //
            // WEFT-334: failures carry `error_kind` + legacy string
            // `error` so panels can branch without substring matching.
            let Some(agent) = daemon_agent() else {
                return agent_chat_err_response(
                    "agent.chat",
                    crate::protocol::AgentChatError::not_wired(
                        "agent service not wired \
                         (LLM client init failed at boot — \
                         check daemon log for 'llm client init failed')",
                    ),
                );
            };
            // Wire and service types are now both re-exports of
            // `clawft_types::agent_chat::*` (WEFT-498), so dispatch is
            // a direct hand-off — no `.into()` translator needed.
            let params: AgentChatParams = match serde_json::from_value(params) {
                Ok(p) => p,
                Err(e) => {
                    return agent_chat_err_response(
                        "agent.chat",
                        crate::protocol::AgentChatError::invalid_params(format!(
                            "invalid params: {e}"
                        )),
                    );
                }
            };
            // M2 D6: capture conv_id before `dispatch` consumes params so the
            // idle reaper can stamp last-activity on success.
            let conv_id = params.conv_id.clone();
            match agent.dispatch(params).await {
                Ok(result) => {
                    if let Some(activity) = DAEMON_CONV_ACTIVITY.get() {
                        activity.insert(conv_id, std::time::Instant::now());
                    }
                    match serde_json::to_value(result) {
                        Ok(v) => Response::success(v),
                        Err(e) => agent_chat_err_response(
                            "agent.chat",
                            crate::protocol::AgentChatError::internal(format!("{e}")),
                        ),
                    }
                }
                Err(e) => agent_chat_err_response("agent.chat", e.to_chat_error()),
            }
        }
        // WEFT-253: progressive companion to agent.chat. Same request /
        // response shape as agent.chat, but while the turn runs the
        // daemon publishes [`AgentChatStreamFrame`]s to
        // `substrate/_derived/chat/<conv>/stream` so panels can render
        // tokens as they arrive (and a phase label for the heartbeat).
        // Native + WASM both consume the stream path via substrate
        // polling; the RPC itself stays request/response.
        "agent.chat_stream" => handle_agent_chat_stream(params, kernel).await,
        "agent.chat.cancel" => {
            // agent-core-v1 Phase C2: trip the per-conv cancel token
            // in `AgentService`. Available unconditionally — when the
            // service isn't wired (or the spike is on) calling cancel
            // is a clean error rather than a build break.
            let conv_id = match params.get("conv_id").and_then(|v| v.as_str()) {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => return Response::error("agent.chat.cancel requires conv_id"),
            };
            if let Some(agent) = daemon_agent() {
                agent.cancel(&conv_id);
                // M4 D5 rule: a turn-level cancel tears down only the work
                // blocking that turn. Inline (await:true) children are on the
                // cancelled turn's call stack and unwind with it; detached
                // (await:false) children are independent work the user fired off
                // deliberately and MUST survive — so we do NOT cascade to them
                // here. The full spawn-tree teardown happens only on
                // conversation-end (agent.chat.end / idle reap → cascade_end_children).
                Response::success(serde_json::json!({"cancelled": conv_id}))
            } else {
                Response::error("agent service not wired")
            }
        }
        // WEFT-331: human decision for an interactive gate Defer. The
        // tool loop is suspended on a oneshot; this resolves it so
        // allow resumes the tool (sandbox still applies) and deny /
        // cancel returns a structured envelope without dispatch.
        "agent.chat.defer_decide" => {
            let parsed: crate::protocol::AgentChatDeferDecideParams =
                match serde_json::from_value(params) {
                    Ok(p) => p,
                    Err(e) => {
                        return Response::error(format!(
                            "agent.chat.defer_decide: invalid params: {e}"
                        ));
                    }
                };
            if parsed.conv_id.is_empty() || parsed.defer_id.is_empty() {
                return Response::error(
                    "agent.chat.defer_decide requires conv_id and defer_id",
                );
            }
            let Some(decision) =
                crate::protocol::DeferUserDecision::parse(&parsed.decision)
            else {
                return Response::error(
                    "agent.chat.defer_decide: decision must be allow|deny|cancel",
                );
            };
            let Some(agent) = daemon_agent() else {
                return Response::error("agent service not wired");
            };
            let result = agent.defer_decide(&parsed.conv_id, &parsed.defer_id, decision);
            match serde_json::to_value(&result) {
                Ok(v) => Response::success(v),
                Err(e) => Response::error(format!("agent.chat.defer_decide: serialize: {e}")),
            }
        }
        "agent.chat.end" => {
            // ADR-058 Phase 5 (deferred step 4): the explicit
            // conversation-end signal. Unlike `agent.chat.cancel` (which
            // re-arms the conv for the next turn), `end` rolls the L2 view
            // up: run the LLM postmortem over the conversation digest, then
            // promote the durable fact to the trunk and drop the view. The
            // chain stays the source of truth, so this is safe to call more
            // than once (a second call finds no view and is a no-op).
            let conv_id = match params.get("conv_id").and_then(|v| v.as_str()) {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => return Response::error("agent.chat.end requires conv_id"),
            };
            let Some(agent) = daemon_agent() else {
                return Response::error("agent service not wired");
            };
            // Build the postmortem digest from the live view, then distill the
            // durable fact via the same LLM the daemon already uses. Best-
            // effort: any postmortem failure drops the view without promoting
            // rather than failing the close.
            let durable_fact = match (agent.session_tier(), daemon_llm()) {
                (Some(tier), Some(llm)) => match tier.conversation_digest(&conv_id, 16_384) {
                    Some(digest) => {
                        let guard = llm.read().await;
                        crate::conv_postmortem::summarize_durable_facts(&guard, &digest).await
                    }
                    None => None,
                },
                _ => None,
            };
            let promoted = agent.end_conversation(&conv_id, durable_fact.as_deref());
            // M4 D5: an explicitly ended parent fully ends its still-running
            // spawned children (cancel + drop view + evict loop state) so none
            // are orphaned — the same teardown the parent itself gets below.
            cascade_end_children(&conv_id).await;
            // M2 D6: explicit end also evicts the hosted loop's per-conversation
            // state (lineage / in-flight slot) and drops the activity entry, so
            // the reaper never revisits it.
            if let Some(talk_loop) = DAEMON_TALK_LOOP.get() {
                talk_loop.end_conversation(&conv_id);
            }
            if let Some(activity) = DAEMON_CONV_ACTIVITY.get() {
                activity.remove(&conv_id);
            }
            Response::success(serde_json::json!({
                "ended": conv_id,
                "promoted_seq": promoted,
            }))
        }
        "agent.turn.record" => {
            // Record externally-produced turns (voice Talk-Mode per
            // ADR-062 §1.1) through the same ConversationSink +
            // KernelTurnAnchor path as agent.chat turns: substrate
            // JSONL first (durable), then best-effort chain / HNSW /
            // causal / session-tier mirroring per [kernel.agent].
            // The daemon does NOT run the LLM loop here — the caller
            // already produced the exchange.
            let p: AgentTurnRecordParams = match serde_json::from_value(params) {
                Ok(p) => p,
                Err(e) => {
                    return Response::error(format!("agent.turn.record: invalid params: {e}"));
                }
            };
            if p.conv_id.is_empty() {
                return Response::error("agent.turn.record: conv_id must be non-empty");
            }
            if p.turns.is_empty() {
                return Response::error("agent.turn.record: turns must be non-empty");
            }
            if let Some(bad) = p
                .turns
                .iter()
                .find(|t| !matches!(t.role.as_str(), "user" | "assistant" | "system"))
            {
                return Response::error(format!(
                    "agent.turn.record: unsupported role `{}` \
                     (expected user | assistant | system)",
                    bad.role
                ));
            }
            let Some(sink) = daemon_turn_sink() else {
                return Response::error(
                    "agent.turn.record: turn sink not wired \
                     (agent service did not boot — check daemon log)",
                );
            };
            // Voice Wave 2 §W2.1: capture the busy state BEFORE recording —
            // anchoring registers every turn with the talk loop, so a read
            // after `append_turn` would see the just-recorded user turn, not
            // the in-flight reply the router decides against.
            // Busy read from the voice loop's OWN attempt tracker —
            // `talk_loop::current_turn` is overwritten by every anchored
            // turn's registration and cleared by its commit-tick, so it goes
            // idle mid-generation the moment a backchannel or queued ask is
            // recorded (found live, conv w26-probe).
            let in_flight_before = DAEMON_VOICE_LOOP.get().and_then(|vl| vl.in_flight(&p.conv_id));
            let mut recorded = 0usize;
            for t in &p.turns {
                let turn = clawft_core::agent::sink::Turn {
                    turn_id: String::new(),
                    role: t.role.clone(),
                    content: t.content.clone(),
                    tool_calls: None,
                    tool_call_id: None,
                    ts_ms: t.ts_ms.unwrap_or_else(crate::control::now_ms),
                    // Wave 1 §W1.2: carry the optional per-utterance voice
                    // decomposition verbatim onto the turn so the anchor →
                    // index_turn seam stores it as a sibling metadata key and
                    // overrides the classification emotion axis (tier:"voice").
                    voice_analysis: t.voice_analysis.clone(),
                    // WEFT-350: agent.turn.record path does not attach
                    // substrate AudioRef here (PCM already on voice capture
                    // topics); chat_stream multimodal turns set `audio`.
                    audio: None,
                };
                match sink.append_turn(&p.conv_id, turn).await {
                    Ok(()) => recorded += 1,
                    Err(e) => {
                        return Response::error(format!(
                            "agent.turn.record: append failed after {recorded} \
                             recorded turn(s): {e}"
                        ));
                    }
                }
            }
            debug!(
                conv_id = %p.conv_id,
                channel = %p.channel,
                recorded,
                "agent.turn.record: turns published + anchored"
            );
            // M2 D6: voice turns recorded here also mint EOU impulses via the
            // shared anchor→index_turn seam, so stamp last-activity for the
            // idle reaper just like agent.chat.
            if let Some(activity) = DAEMON_CONV_ACTIVITY.get() {
                activity.insert(p.conv_id.clone(), std::time::Instant::now());
            }
            // Voice Wave 2 §W2.1: route each recorded USER utterance through
            // the interrupt brain (idle → off-path text reply; busy → STOP/
            // Refine/Backchannel/Queue against the in-flight turn). Presence
            // of DAEMON_VOICE_LOOP is the `[kernel.agent].voice_loop` gate.
            // Never fails the record — the turns above are already durable.
            if let Some(voice_loop) = DAEMON_VOICE_LOOP.get() {
                for t in p.turns.iter().filter(|t| t.role == "user") {
                    voice_loop
                        .route_user_turn(
                            &p.conv_id,
                            &t.content,
                            t.voice_analysis.as_ref(),
                            in_flight_before,
                        )
                        .await;
                }
            }
            match serde_json::to_value(crate::protocol::AgentTurnRecordResult { recorded }) {
                Ok(v) => Response::success(v),
                Err(e) => Response::error(format!("agent.turn.record: {e}")),
            }
        }
        "agent.proposal.list" => {
            // WEFT-654 D9-D11: list the held review-mode proposal (0 or 1
            // today; the wire shape is a list for the per-conv Phase 2
            // preview). Reads `DAEMON_AGENT_LOOP` directly — `AgentService`'s
            // `AgentLoopHandle` trait doesn't expose the review-gate surface.
            let Some(loop_handle) = daemon_agent_loop() else {
                return Response::error("agent service not wired");
            };
            let result = proposal_list_result(loop_handle.pending_hold_info());
            match serde_json::to_value(result) {
                Ok(v) => Response::success(v),
                Err(e) => Response::error(format!("agent.proposal.list: {e}")),
            }
        }
        "agent.proposal.accept" => {
            // Accept = promote + witness (D9). Fails cleanly when there's no
            // hold to accept (`ClawftError::CowMemory`, "no pending
            // proposal") rather than panicking.
            let Some(loop_handle) = daemon_agent_loop() else {
                return Response::error("agent service not wired");
            };
            match loop_handle.accept_pending() {
                Ok(label) => {
                    info!(label = %label, "agent.proposal.accept: proposal accepted");
                    // WEFT-655 Gap A: resolve a voice reply parked behind
                    // this hold (commits its forest attempt) — no-op when
                    // nothing was parked under this label.
                    resolve_parked_attempt_on_accept(&label);
                    match serde_json::to_value(AgentProposalDecisionResult { label }) {
                        Ok(v) => Response::success(v),
                        Err(e) => Response::error(format!("agent.proposal.accept: {e}")),
                    }
                }
                Err(e) => Response::error(format!("agent.proposal.accept: {e}")),
            }
        }
        "agent.proposal.discard" => {
            // Discard = rollback + witnessed prune (D9); `Discarded` is not
            // a new state, it's the Wave-2 prune reached deliberately. The
            // partial trace stays on the graph/chain (discard ≠ delete).
            let Some(loop_handle) = daemon_agent_loop() else {
                return Response::error("agent service not wired");
            };
            let discard_params: AgentProposalDiscardParams = match serde_json::from_value(params) {
                Ok(p) => p,
                Err(e) => {
                    return Response::error(format!("agent.proposal.discard: invalid params: {e}"));
                }
            };
            let reason = proposal_discard_reason(&discard_params).to_string();
            match loop_handle.discard_pending(&reason) {
                Ok(label) => {
                    warn!(label = %label, reason = %reason, "agent.proposal.discard: proposal discarded");
                    // WEFT-655 Gap A: clean up a voice reply parked behind
                    // this hold (prune tombstone + witnessed cancel) — same
                    // closure the Stop interrupt uses. No-op when nothing
                    // was parked under this label.
                    cleanup_parked_attempt_on_discard(&label);
                    match serde_json::to_value(AgentProposalDecisionResult { label }) {
                        Ok(v) => Response::success(v),
                        Err(e) => Response::error(format!("agent.proposal.discard: {e}")),
                    }
                }
                Err(e) => Response::error(format!("agent.proposal.discard: {e}")),
            }
        }
        "terminal.spawn" => handle_terminal_spawn(params, kernel).await,
        "terminal.write" => handle_terminal_write(params, kernel).await,
        "terminal.resize" => handle_terminal_resize(params, kernel).await,
        "terminal.close" => handle_terminal_close(params, kernel).await,
        "agent.spawn" => {
            let spawn_params: AgentSpawnParams = match serde_json::from_value(params) {
                Ok(p) => p,
                Err(e) => return Response::error(format!("invalid params: {e}")),
            };
            let k = kernel.read().await;
            let request = clawft_kernel::SpawnRequest {
                agent_id: spawn_params.agent_id,
                capabilities: None,
                parent_pid: spawn_params.parent_pid,
                env: std::collections::HashMap::new(),
                backend: None,
            };

            // Create inbox via A2ARouter before spawning
            let a2a = k.a2a_router().clone();
            let cron = k.cron_service().clone();
            #[cfg(feature = "exochain")]
            let chain = k.chain_manager().cloned();

            // K3: Build ToolRegistry with reference implementations
            let tool_registry: std::sync::Arc<clawft_kernel::ToolRegistry> = {
                let mut registry = clawft_kernel::ToolRegistry::new();
                registry.register(std::sync::Arc::new(clawft_kernel::FsReadFileTool::new()));
                registry.register(std::sync::Arc::new(clawft_kernel::AgentSpawnTool::new(
                    k.process_table().clone(),
                )));
                std::sync::Arc::new(registry)
            };

            // Use spawn_and_run to actually execute the agent work loop
            let process_table = k.process_table().clone();
            match k.supervisor().spawn_and_run(request, {
                let a2a_clone = a2a.clone();
                let cron_clone = cron.clone();
                let pt_clone = process_table;
                let tool_reg_clone = tool_registry.clone();
                #[cfg(feature = "exochain")]
                let chain_clone = chain.clone();
                #[cfg(feature = "exochain")]
                let gate: Option<std::sync::Arc<dyn clawft_kernel::GateBackend>> = {
                    use clawft_kernel::{
                        GovernanceBranch, GovernanceGate, GovernanceRule, RuleSeverity,
                    };
                    let mut g = GovernanceGate::new(0.8, false);
                    if let Some(ref cm) = chain {
                        g = g.with_chain(cm.clone());
                    }
                    g = g
                        .add_rule(GovernanceRule {
                            id: "exec-guard".into(),
                            description: "Block high-risk exec actions".into(),
                            branch: GovernanceBranch::Judicial,
                            severity: RuleSeverity::Blocking,
                            active: true,
                            reference_url: None,
                            sop_category: None,
                            rule_type: Default::default(),
                            action_selector: None,
                            tool_selector: None,
                            force_on_match: false,
                        })
                        .add_rule(GovernanceRule {
                            id: "cron-warn".into(),
                            description: "Warn on cron modifications".into(),
                            branch: GovernanceBranch::Executive,
                            severity: RuleSeverity::Warning,
                            active: true,
                            reference_url: None,
                            sop_category: None,
                            rule_type: Default::default(),
                            action_selector: None,
                            tool_selector: None,
                            force_on_match: false,
                        });
                    // ADR-103 D8: a project kernel's spawned agents are gated by
                    // its effective governance, not this fixed rule set.
                    k.governance_overlay()
                        .and(k.governance_gate().cloned())
                        .or_else(|| {
                            Some(std::sync::Arc::new(g) as std::sync::Arc<dyn clawft_kernel::GateBackend>)
                        })
                };
                move |pid, cancel| {
                    let inbox = a2a_clone.create_inbox(pid);
                    async move {
                        clawft_kernel::agent_loop::kernel_agent_loop(
                            pid,
                            cancel,
                            inbox,
                            a2a_clone,
                            cron_clone,
                            pt_clone,
                            Some(tool_reg_clone),
                            #[cfg(feature = "exochain")]
                            chain_clone,
                            #[cfg(feature = "exochain")]
                            gate,
                        )
                        .await
                    }
                }
            }) {
                Ok(result) => {
                    k.event_log().info(
                        "agent",
                        format!("spawned {} (PID {})", result.agent_id, result.pid),
                    );
                    let spawn_result = AgentSpawnResult {
                        pid: result.pid,
                        agent_id: result.agent_id,
                    };
                    Response::success(serde_json::to_value(spawn_result).unwrap())
                }
                Err(e) => Response::error(format!("spawn failed: {e}")),
            }
        }
        "agent.stop" => {
            let stop_params: AgentStopParams = match serde_json::from_value(params) {
                Ok(p) => p,
                Err(e) => return Response::error(format!("invalid params: {e}")),
            };
            let k = kernel.read().await;
            match k.supervisor().stop(stop_params.pid, stop_params.graceful) {
                Ok(()) => {
                    k.event_log()
                        .info("agent", format!("stopped PID {}", stop_params.pid));
                    Response::success(serde_json::json!("ok"))
                }
                Err(e) => Response::error(format!("stop failed: {e}")),
            }
        }
        "agent.restart" => {
            let restart_params: AgentRestartParams = match serde_json::from_value(params) {
                Ok(p) => p,
                Err(e) => return Response::error(format!("invalid params: {e}")),
            };
            let k = kernel.read().await;
            match k.supervisor().restart(restart_params.pid) {
                Ok(result) => {
                    let _ = k
                        .process_table()
                        .update_state(result.pid, clawft_kernel::ProcessState::Running);
                    k.event_log().info(
                        "agent",
                        format!(
                            "restarted {} (PID {} -> {})",
                            result.agent_id, restart_params.pid, result.pid
                        ),
                    );
                    let spawn_result = AgentSpawnResult {
                        pid: result.pid,
                        agent_id: result.agent_id,
                    };
                    Response::success(serde_json::to_value(spawn_result).unwrap())
                }
                Err(e) => Response::error(format!("restart failed: {e}")),
            }
        }
        "agent.inspect" => {
            let pid = params.get("pid").and_then(|v| v.as_u64()).unwrap_or(0);
            let k = kernel.read().await;
            match k.supervisor().inspect(pid) {
                Ok(entry) => {
                    let topics = k.a2a_router().topic_router().topics_for_pid(pid);
                    let result = AgentInspectResult {
                        pid: entry.pid,
                        agent_id: entry.agent_id,
                        state: entry.state.to_string(),
                        memory_bytes: entry.resource_usage.memory_bytes,
                        cpu_time_ms: entry.resource_usage.cpu_time_ms,
                        messages_sent: entry.resource_usage.messages_sent,
                        tool_calls: entry.resource_usage.tool_calls,
                        topics,
                        parent_pid: entry.parent_pid,
                        can_spawn: entry.capabilities.can_spawn,
                        can_ipc: entry.capabilities.can_ipc,
                        can_exec_tools: entry.capabilities.can_exec_tools,
                        can_network: entry.capabilities.can_network,
                    };
                    Response::success(serde_json::to_value(result).unwrap())
                }
                Err(e) => Response::error(format!("inspect failed: {e}")),
            }
        }
        "agent.list" => {
            let k = kernel.read().await;
            let agents = k.supervisor().list_agents();
            let mut infos: Vec<ProcessInfo> = agents
                .iter()
                .map(|e| ProcessInfo {
                    row_id: crate::protocol::process_row_id_for_pid(e.pid),
                    pid: e.pid,
                    agent_id: e.agent_id.clone(),
                    state: e.state.to_string(),
                    memory_bytes: e.resource_usage.memory_bytes,
                    cpu_time_ms: e.resource_usage.cpu_time_ms,
                    parent_pid: e.parent_pid,
                })
                .collect();
            infos.sort_by_key(|e| e.pid);
            Response::success(serde_json::to_value(infos).unwrap())
        }
        "agent.send" => {
            let send_params: AgentSendParams = match serde_json::from_value(params) {
                Ok(p) => p,
                Err(e) => return Response::error(format!("invalid params: {e}")),
            };
            let k = kernel.read().await;

            // Try to parse message as JSON; fall back to text payload
            let payload = match serde_json::from_str::<serde_json::Value>(&send_params.message) {
                Ok(v) => clawft_kernel::MessagePayload::Json(v),
                Err(_) => clawft_kernel::MessagePayload::Text(send_params.message.clone()),
            };
            let msg = clawft_kernel::KernelMessage::new(
                0, // from kernel (PID 0)
                clawft_kernel::MessageTarget::Process(send_params.pid),
                payload,
            );

            // Route through A2ARouter with chain-logged delivery
            let a2a = k.a2a_router().clone();

            #[cfg(feature = "exochain")]
            let send_result = {
                let chain = k.chain_manager();
                a2a.send_checked(msg, chain.map(|c| c.as_ref())).await
            };
            #[cfg(not(feature = "exochain"))]
            let send_result = a2a.send(msg).await;

            match send_result {
                Ok(()) => {
                    k.event_log()
                        .info("ipc", format!("message sent to PID {}", send_params.pid));

                    // Wait briefly for a response from the agent
                    // Create a temporary inbox for kernel PID 0 if not already present
                    // (it may already exist from a previous call — A2ARouter replaces it)
                    let mut reply_rx = a2a.create_inbox(0);
                    match tokio::time::timeout(std::time::Duration::from_secs(2), reply_rx.recv())
                        .await
                    {
                        Ok(Some(reply)) => {
                            let reply_value = match &reply.payload {
                                clawft_kernel::MessagePayload::Json(v) => v.clone(),
                                clawft_kernel::MessagePayload::Text(t) => {
                                    serde_json::json!({"text": t})
                                }
                                _ => serde_json::json!({"payload": "non-json"}),
                            };
                            Response::success(reply_value)
                        }
                        Ok(None) | Err(_) => {
                            // No reply within timeout — still report send success
                            Response::success(serde_json::json!("sent"))
                        }
                    }
                }
                Err(e) => Response::error(format!("send failed: {e}")),
            }
        }
        "cron.add" => {
            let cron_params: CronAddParams = match serde_json::from_value(params) {
                Ok(p) => p,
                Err(e) => return Response::error(format!("invalid params: {e}")),
            };
            let k = kernel.read().await;
            let job = match k.cron_service().add_job(
                cron_params.name,
                cron_params.interval_secs,
                cron_params.command,
                cron_params.target_pid,
            ) {
                Ok(job) => job,
                Err(e) => return Response::error(format!("cron.add failed: {e}")),
            };

            k.event_log().info(
                "cron",
                format!("job added: {} ({}s)", job.name, job.interval_secs),
            );
            let info = CronJobInfo {
                id: job.id,
                name: job.name,
                interval_secs: job.interval_secs,
                command: job.command,
                target_pid: job.target_pid,
                enabled: job.enabled,
                fire_count: job.fire_count,
                last_fired: job.last_fired.map(|t| t.to_rfc3339()),
                created_at: job.created_at.to_rfc3339(),
            };
            Response::success(serde_json::to_value(info).unwrap())
        }
        "cron.list" => {
            let k = kernel.read().await;
            let jobs = k.cron_service().list_jobs();
            let infos: Vec<CronJobInfo> = jobs
                .iter()
                .map(|j| CronJobInfo {
                    id: j.id.clone(),
                    name: j.name.clone(),
                    interval_secs: j.interval_secs,
                    command: j.command.clone(),
                    target_pid: j.target_pid,
                    enabled: j.enabled,
                    fire_count: j.fire_count,
                    last_fired: j.last_fired.map(|t| t.to_rfc3339()),
                    created_at: j.created_at.to_rfc3339(),
                })
                .collect();
            Response::success(serde_json::to_value(infos).unwrap())
        }
        "cron.remove" => {
            let remove_params: CronRemoveParams = match serde_json::from_value(params) {
                Ok(p) => p,
                Err(e) => return Response::error(format!("invalid params: {e}")),
            };
            let k = kernel.read().await;
            match k.cron_service().remove_job(&remove_params.id) {
                Ok(Some(job)) => {
                    #[cfg(feature = "exochain")]
                    if let Some(cm) = k.chain_manager() {
                        cm.append(
                            "cron",
                            "cron.remove",
                            Some(serde_json::json!({
                                "job_id": job.id,
                                "name": job.name,
                            })),
                        );
                    }
                    k.event_log()
                        .info("cron", format!("job removed: {}", job.name));
                    Response::success(serde_json::json!({"removed": true, "job_id": job.id}))
                }
                Ok(None) => Response::error(format!("cron job not found: {}", remove_params.id)),
                Err(e) => Response::error(format!("cron remove denied: {e}")),
            }
        }
        "cron.enable" | "cron.disable" => {
            let enabled = method == "cron.enable";
            let p: CronRemoveParams = match serde_json::from_value(params) {
                Ok(p) => p,
                Err(e) => return Response::error(format!("invalid params: {e}")),
            };
            let k = kernel.read().await;
            match k.cron_service().set_enabled(&p.id, enabled) {
                Some(job) => {
                    #[cfg(feature = "exochain")]
                    if let Some(cm) = k.chain_manager() {
                        cm.append(
                            "cron",
                            &method,
                            Some(serde_json::json!({"job_id": job.id, "name": job.name})),
                        );
                    }
                    k.event_log().info(
                        "cron",
                        format!(
                            "job {}: {}",
                            if enabled { "enabled" } else { "disabled" },
                            job.name
                        ),
                    );
                    Response::success(serde_json::json!({"job_id": job.id, "enabled": job.enabled}))
                }
                None => Response::error(format!("cron job not found: {}", p.id)),
            }
        }
        // WEFT-494 / ADR-070: live MCP registry (shared McpServerManager).
        // Durable config remains CLI-owned (weft mcp … / WEFT-188).
        "mcp.add" => crate::mcp_rpc::handle_mcp_add(params).await,
        "mcp.list" | "tools.mcp" => crate::mcp_rpc::handle_mcp_list(params).await,
        "mcp.remove" => crate::mcp_rpc::handle_mcp_remove(params).await,
        "mcp.reload" => crate::mcp_rpc::handle_mcp_reload(params).await,
        "ipc.topics" => {
            let k = kernel.read().await;
            let a2a = k.a2a_router();
            let topics = a2a.topic_router().list_topics();
            let infos: Vec<IpcTopicInfo> = topics
                .iter()
                .map(|(topic, _count)| {
                    let subs = a2a.topic_router().subscribers(topic);
                    IpcTopicInfo {
                        topic: topic.clone(),
                        subscriber_count: subs.len(),
                        subscribers: subs,
                    }
                })
                .collect();
            Response::success(serde_json::to_value(infos).unwrap())
        }
        "ipc.subscribe" => {
            let sub_params: IpcSubscribeParams = match serde_json::from_value(params) {
                Ok(p) => p,
                Err(e) => return Response::error(format!("invalid params: {e}")),
            };
            let k = kernel.read().await;

            // Validate that the PID exists and is running
            if k.supervisor().inspect(sub_params.pid).is_err() {
                return Response::error(format!(
                    "PID {} not found — spawn an agent first",
                    sub_params.pid,
                ));
            }

            let a2a = k.a2a_router();
            a2a.topic_router()
                .subscribe(sub_params.pid, &sub_params.topic);
            k.event_log().info(
                "ipc",
                format!(
                    "PID {} subscribed to '{}'",
                    sub_params.pid, sub_params.topic
                ),
            );

            #[cfg(feature = "exochain")]
            if let Some(cm) = k.chain_manager() {
                cm.append(
                    "ipc",
                    "ipc.subscribe",
                    Some(serde_json::json!({
                        "pid": sub_params.pid,
                        "topic": sub_params.topic,
                    })),
                );
            }

            Response::success(serde_json::json!("subscribed"))
        }
        "ipc.publish" => {
            let pub_params: IpcPublishParams = match serde_json::from_value(params) {
                Ok(p) => p,
                Err(e) => return Response::error(format!("invalid params: {e}")),
            };
            let k = kernel.read().await;

            // Authenticate the publish if the caller supplied an
            // identity. Missing actor_id is accepted for bring-up
            // parity with existing clients; missing signature given
            // an actor_id is unauthorized.
            if let Some(actor_id) = pub_params.actor_id.as_ref() {
                let sig = match pub_params.signature.as_ref() {
                    Some(s) => s,
                    None => {
                        return Response::error(
                            "actor_id provided but signature missing".to_string(),
                        );
                    }
                };
                let ts = pub_params.ts.unwrap_or(0);
                let payload = clawft_kernel::publish_payload(
                    &pub_params.topic,
                    &pub_params.message,
                    ts,
                    actor_id,
                );
                if let Err(e) = verify_agent_signature(&k, actor_id, sig, &payload) {
                    return Response::error(format!("unauthorized: {e}"));
                }
            } else {
                tracing::warn!(
                    topic = %pub_params.topic,
                    "ipc.publish with no actor_id — anonymous publish accepted (bring-up only)"
                );
            }

            let a2a = k.a2a_router().clone();

            // Count all registered subscribers for reporting
            let subscriber_count = a2a.topic_router().subscribers(&pub_params.topic).len();

            // Build message payload
            let payload = match serde_json::from_str::<serde_json::Value>(&pub_params.message) {
                Ok(v) => clawft_kernel::MessagePayload::Json(v),
                Err(_) => clawft_kernel::MessagePayload::Text(pub_params.message.clone()),
            };
            let msg = clawft_kernel::KernelMessage::new(
                0, // from kernel (PID 0)
                clawft_kernel::MessageTarget::Topic(pub_params.topic.clone()),
                payload,
            );

            #[cfg(feature = "exochain")]
            let send_result = {
                let chain = k.chain_manager();
                a2a.send_checked(msg, chain.map(|c| c.as_ref())).await
            };
            #[cfg(not(feature = "exochain"))]
            let send_result = a2a.send(msg).await;

            match send_result {
                Ok(()) => {
                    // WEFT-150: explicit ipc.publish chain event (in addition
                    // to the generic ipc.send recorded by send_checked).
                    // Leaf-push topics are tagged so audits can filter
                    // `weaver leaf push` / scene publishes without parsing
                    // the Debug target string on ipc.send.
                    #[cfg(feature = "exochain")]
                    if let Some(cm) = k.chain_manager() {
                        let is_leaf_push = weftos_leaf_types::is_push_topic(&pub_params.topic);
                        let wire_type = serde_json::from_str::<serde_json::Value>(&pub_params.message)
                            .ok()
                            .and_then(|v| {
                                v.get("type")
                                    .and_then(|t| t.as_str())
                                    .map(|s| s.to_string())
                            });
                        cm.append(
                            "ipc",
                            "ipc.publish",
                            Some(serde_json::json!({
                                "topic": pub_params.topic,
                                "subscribers": subscriber_count,
                                "actor_id": pub_params.actor_id,
                                "leaf_push": is_leaf_push,
                                "wire_type": wire_type,
                                "message_len": pub_params.message.len(),
                            })),
                        );
                    }

                    k.event_log().info(
                        "ipc",
                        format!(
                            "published to topic '{}' ({} subscriber{})",
                            pub_params.topic,
                            subscriber_count,
                            if subscriber_count == 1 { "" } else { "s" },
                        ),
                    );
                    Response::success(serde_json::json!({
                        "topic": pub_params.topic,
                        "subscribers": subscriber_count,
                    }))
                }
                Err(e) => Response::error(format!("publish failed: {e}")),
            }
        }
        "resource.score" => {
            #[cfg(feature = "exochain")]
            {
                let score_params: ResourceScoreParams = match serde_json::from_value(params) {
                    Ok(p) => p,
                    Err(e) => return Response::error(format!("invalid params: {e}")),
                };
                let k = kernel.read().await;
                if let Some(tm) = k.tree_manager() {
                    let rid = exo_resource_tree::ResourceId::new(&score_params.path);
                    if let Some(scoring) = tm.get_scoring(&rid) {
                        let composite = scoring.trust * 0.25
                            + scoring.performance * 0.20
                            + scoring.reliability * 0.20
                            + scoring.velocity * 0.15
                            + scoring.reward * 0.10
                            + (1.0 - scoring.difficulty) * 0.10;
                        let result = ResourceScoreResult {
                            path: score_params.path,
                            trust: scoring.trust,
                            performance: scoring.performance,
                            difficulty: scoring.difficulty,
                            reward: scoring.reward,
                            reliability: scoring.reliability,
                            velocity: scoring.velocity,
                            composite,
                        };
                        Response::success(serde_json::to_value(result).unwrap())
                    } else {
                        Response::error(format!("no scoring for: {}", score_params.path))
                    }
                } else {
                    Response::error("resource tree not enabled")
                }
            }
            #[cfg(not(feature = "exochain"))]
            Response::error("exochain feature not enabled")
        }
        "resource.rank" => {
            #[cfg(feature = "exochain")]
            {
                let rank_params: ResourceRankParams =
                    serde_json::from_value(params).unwrap_or(ResourceRankParams { count: 10 });
                let k = kernel.read().await;
                if let Some(tm) = k.tree_manager() {
                    // Equal weight across all 6 dimensions
                    let weights = [1.0, 1.0, 0.5, 0.5, 1.0, 0.5];
                    let ranked = tm.rank_by_score(&weights, rank_params.count);
                    let entries: Vec<ResourceRankEntry> = ranked
                        .iter()
                        .map(|(rid, score)| ResourceRankEntry {
                            path: rid.to_string(),
                            score: *score,
                        })
                        .collect();
                    Response::success(serde_json::to_value(entries).unwrap())
                } else {
                    Response::error("resource tree not enabled")
                }
            }
            #[cfg(not(feature = "exochain"))]
            Response::error("exochain feature not enabled")
        }
        // ── Assessment endpoints ──────────────────────────────
        "assess.run" => {
            let run_params: crate::protocol::AssessRunParams = match serde_json::from_value(params)
            {
                Ok(p) => p,
                Err(e) => return Response::error(format!("invalid params: {e}")),
            };
            let dir = run_params.dir.as_deref().unwrap_or(".");
            let k = kernel.read().await;

            #[cfg(feature = "exochain")]
            if let Some(cm) = k.chain_manager() {
                cm.append(
                    "assessment",
                    "assess.run",
                    Some(serde_json::json!({
                        "scope": run_params.scope,
                        "format": run_params.format,
                        "dir": dir,
                    })),
                );
            }

            k.event_log().info(
                "assessment",
                format!(
                    "assess.run scope={} format={} dir={}",
                    run_params.scope, run_params.format, dir
                ),
            );

            Response::success(serde_json::json!({
                "status": "accepted",
                "scope": run_params.scope,
                "format": run_params.format,
                "dir": dir,
                "message": "Assessment queued. Results will be available via assess.status."
            }))
        }
        "assess.status" => {
            let k = kernel.read().await;

            #[cfg(feature = "exochain")]
            if let Some(cm) = k.chain_manager() {
                cm.append("assessment", "assess.status", None);
            }

            k.event_log()
                .info("assessment", "assess.status queried".to_string());

            // Return stub until AssessmentService is fully wired
            Response::success(serde_json::json!({
                "status": "idle",
                "last_run": null,
                "findings": [],
                "message": "No assessment report available yet."
            }))
        }
        "assess.link" => {
            let link_params: crate::protocol::AssessLinkParams =
                match serde_json::from_value(params) {
                    Ok(p) => p,
                    Err(e) => return Response::error(format!("invalid params: {e}")),
                };
            let k = kernel.read().await;

            #[cfg(feature = "exochain")]
            if let Some(cm) = k.chain_manager() {
                cm.append(
                    "assessment",
                    "assess.link",
                    Some(serde_json::json!({
                        "name": link_params.name,
                        "location": link_params.location,
                    })),
                );
            }

            k.event_log().info(
                "assessment",
                format!(
                    "peer linked: {} -> {}",
                    link_params.name, link_params.location
                ),
            );

            Response::success(serde_json::json!({
                "linked": true,
                "name": link_params.name,
                "location": link_params.location
            }))
        }
        "assess.peers" => {
            let k = kernel.read().await;

            #[cfg(feature = "exochain")]
            if let Some(cm) = k.chain_manager() {
                cm.append("assessment", "assess.peers", None);
            }

            k.event_log()
                .info("assessment", "assess.peers queried".to_string());

            // Return empty list until AssessmentService manages peers
            Response::success(serde_json::json!({
                "peers": []
            }))
        }
        "assess.compare" => {
            let cmp_params: crate::protocol::AssessCompareParams =
                match serde_json::from_value(params) {
                    Ok(p) => p,
                    Err(e) => return Response::error(format!("invalid params: {e}")),
                };
            let k = kernel.read().await;

            #[cfg(feature = "exochain")]
            if let Some(cm) = k.chain_manager() {
                cm.append(
                    "assessment",
                    "assess.compare",
                    Some(serde_json::json!({
                        "peer": cmp_params.peer,
                    })),
                );
            }

            k.event_log().info(
                "assessment",
                format!("assess.compare peer={}", cmp_params.peer),
            );

            Response::success(serde_json::json!({
                "status": "accepted",
                "peer": cmp_params.peer,
                "message": "Comparison queued. Results will be available via assess.status."
            }))
        }
        // ── Assessment mesh endpoints ────────────────────────
        "assess.mesh.status" => {
            let k = kernel.read().await;

            #[cfg(feature = "exochain")]
            if let Some(cm) = k.chain_manager() {
                cm.append("assessment", "assess.mesh.status", None);
            }

            k.event_log()
                .info("assessment", "assess.mesh.status queried".to_string());

            {
                let svc = k.assessment_service();
                if let Some(mc) = svc.mesh_coordinator() {
                    let peers: Vec<crate::protocol::AssessMeshPeerInfo> = mc
                        .peer_states()
                        .into_iter()
                        .map(|p| crate::protocol::AssessMeshPeerInfo {
                            node_id: p.node_id,
                            project_name: p.project_name,
                            last_assessment: p.last_assessment,
                            finding_count: p.finding_count,
                            analyzer_count: p.analyzer_count,
                            last_gossip: p.last_gossip,
                        })
                        .collect();
                    let peer_count = peers.len();
                    Response::success(
                        serde_json::to_value(crate::protocol::AssessMeshStatusResult {
                            mesh_enabled: true,
                            node_id: Some(mc.node_id().to_string()),
                            project_name: Some(mc.project_name().to_string()),
                            peer_count,
                            peers,
                        })
                        .unwrap(),
                    )
                } else {
                    Response::success(
                        serde_json::to_value(crate::protocol::AssessMeshStatusResult {
                            mesh_enabled: false,
                            node_id: None,
                            project_name: None,
                            peer_count: 0,
                            peers: vec![],
                        })
                        .unwrap(),
                    )
                }
            }
        }
        "assess.mesh.gossip" => {
            let k = kernel.read().await;

            #[cfg(feature = "exochain")]
            if let Some(cm) = k.chain_manager() {
                cm.append("assessment", "assess.mesh.gossip", None);
            }

            k.event_log()
                .info("assessment", "assess.mesh.gossip triggered".to_string());

            {
                let svc = k.assessment_service();
                if let Some(mc) = svc.mesh_coordinator() {
                    if let Some(report) = svc.get_latest() {
                        let gossip = mc.build_gossip(&report);
                        mc.set_pending_broadcast(gossip);
                        Response::success(
                            serde_json::to_value(crate::protocol::AssessMeshGossipResult {
                                sent: true,
                                message: "Gossip broadcast queued.".into(),
                            })
                            .unwrap(),
                        )
                    } else {
                        Response::success(
                            serde_json::to_value(crate::protocol::AssessMeshGossipResult {
                                sent: false,
                                message: "No assessment report available to gossip.".into(),
                            })
                            .unwrap(),
                        )
                    }
                } else {
                    Response::error("mesh coordination not enabled")
                }
            }
        }
        // ── ECC methods ────────────────────────────────────────
        #[cfg(feature = "ecc")]
        "ecc.status" => {
            let k = kernel.read().await;
            // Emit the flat keys the `weaver ecc status` CLI printer
            // (`ecc_cmd.rs`) reads: `calibration`, `tick`, `hnsw_count`,
            // `causal_nodes`, `causal_edges`, `crossref_count`,
            // `impulse_pending`. A prior nested shape (`causal_graph.nodes`,
            // `hnsw_entries`, `cognitive_tick`) drifted from the CLI's reads, so
            // every field but `crossref_count` displayed 0 regardless of the
            // real graph state — masking a working ECC pipeline as empty.
            let hnsw_count = k.ecc_hnsw().map(|h| h.len()).unwrap_or(0);
            let (causal_nodes, causal_edges) = k
                .ecc_causal()
                .map(|g| (g.node_count(), g.edge_count()))
                .unwrap_or((0, 0));
            let crossref_count = k.ecc_crossrefs().map(|c| c.count()).unwrap_or(0);
            let impulse_pending = k.ecc_impulses().map(|q| q.pending_count()).unwrap_or(0);
            let calibration = k.ecc_calibration().map(|c| {
                serde_json::json!({
                    "compute_p50_us": c.compute_p50_us,
                    "compute_p95_us": c.compute_p95_us,
                    "tick_interval_ms": c.tick_interval_ms,
                    "spectral_capable": c.spectral_capable,
                })
            });
            let tick = k.ecc_tick().map(|t| {
                serde_json::json!({
                    "tick_count": t.tick_count(),
                    "current_interval_ms": t.current_interval_ms(),
                    "drift_count": t.drift_count(),
                    "running": t.is_running(),
                })
            });
            // Process-local `ecc.spatial.*` scaffold (WEFT-720) is restored
            // via kernel SpatialService when `[kernel.spatial].enabled`.
            Response::success(serde_json::json!({
                "enabled": k.ecc_hnsw().is_some(),
                "calibration": calibration,
                "tick": tick,
                "hnsw_count": hnsw_count,
                "causal_nodes": causal_nodes,
                "causal_edges": causal_edges,
                "crossref_count": crossref_count,
                "impulse_pending": impulse_pending,
            }))
        }
        #[cfg(feature = "ecc")]
        m if m.starts_with("ecc.spatial.") => crate::spatial_rpc::handle(m, &params, &kernel).await,
        #[cfg(feature = "ecc")]
        "ecc.calibrate" => {
            let k = kernel.read().await;
            if let Some(cal) = k.ecc_calibration() {
                Response::success(serde_json::to_value(cal).unwrap_or_default())
            } else {
                Response::error("ECC not initialized or calibration not complete")
            }
        }
        #[cfg(feature = "ecc")]
        "ecc.search" => {
            let k = kernel.read().await;
            if let Some(hnsw) = k.ecc_hnsw() {
                Response::success(serde_json::json!({
                    "available": true,
                    "entries": hnsw.len(),
                    "search_count": hnsw.search_count(),
                    "hint": "use agent tools (ecc/search) for semantic queries — vector embedding required"
                }))
            } else {
                Response::error("HNSW not available")
            }
        }
        #[cfg(feature = "ecc")]
        "ecc.causal" => {
            let node_id = params.get("node_id").and_then(|v| v.as_u64());
            let depth = params.get("depth").and_then(|v| v.as_u64()).unwrap_or(3) as usize;
            let k = kernel.read().await;
            if let Some(graph) = k.ecc_causal() {
                if let Some(id) = node_id {
                    let neighbors = graph.traverse_forward(id, depth);
                    Response::success(serde_json::json!({
                        "node_id": id,
                        "depth": depth,
                        "reachable": neighbors.len(),
                        "nodes": neighbors,
                    }))
                } else {
                    Response::success(serde_json::json!({
                        "nodes": graph.node_count(),
                        "edges": graph.edge_count(),
                        "components": graph.connected_components().len(),
                    }))
                }
            } else {
                Response::error("causal graph not available")
            }
        }
        #[cfg(feature = "ecc")]
        "ecc.tick" => {
            let k = kernel.read().await;
            let cal = k.ecc_calibration();
            if k.ecc_tick().is_some() {
                Response::success(serde_json::json!({
                    "running": true,
                    "interval_ms": cal.map(|c| c.tick_interval_ms).unwrap_or(50),
                    "spectral_capable": cal.map(|c| c.spectral_capable).unwrap_or(false),
                }))
            } else {
                Response::success(serde_json::json!({"running": false}))
            }
        }
        #[cfg(feature = "ecc")]
        "ecc.crossrefs" => {
            let k = kernel.read().await;
            if let Some(crossrefs) = k.ecc_crossrefs() {
                Response::success(serde_json::json!({
                    "count": crossrefs.count(),
                }))
            } else {
                Response::error("crossref store not available")
            }
        }
        // WEFT-364 (was WEFT-125): operator introspection for the active
        // vector backend. Returns live backend name + configured parameters
        // so operators do not have to dig through TOML or trust the build alone.
        #[cfg(feature = "ecc")]
        "ecc.vector-config" => {
            let k = kernel.read().await;
            let cfg = k.kernel_config().vector.clone();
            let (backend_name, vector_count, epoch, max_vectors) =
                match k.ecc_vector_backend() {
                    Some(vb) => (
                        Some(vb.backend_name().to_string()),
                        vb.len(),
                        vb.current_epoch(),
                        vb.max_vectors(),
                    ),
                    None => (None, 0, 0, None),
                };
            let configured_backend = cfg
                .as_ref()
                .map(|c| c.backend)
                .unwrap_or_default();
            let strict = cfg.as_ref().map(|c| c.strict).unwrap_or(false);
            Response::success(serde_json::json!({
                "active": backend_name.is_some(),
                "backend": backend_name,
                "configured_backend": configured_backend,
                "strict": strict,
                "vector_count": vector_count,
                "epoch": epoch,
                "max_vectors": max_vectors,
                "parameters": {
                    "hnsw": cfg.as_ref().and_then(|c| c.hnsw.clone()),
                    "diskann": cfg.as_ref().and_then(|c| c.diskann.clone()),
                    "hybrid": cfg.as_ref().and_then(|c| c.hybrid.clone()),
                    "log_quantized": cfg.as_ref().and_then(|c| c.log_quantized.clone()),
                    "simd_distance": cfg.as_ref().and_then(|c| c.simd_distance.clone()),
                },
            }))
        }
        // ── ADR-067 P0: conversation graph export ──────────────────────
        //
        // `conversation.graph { conv_id, since?: u64, window?: {from_ts, to_ts} }`
        // projects the kernel-global causal forest for a SINGLE conversation
        // into the `{nodes, edges}` shape the egui `GraphViewer`
        // (`explorer/viewers/graph.rs`) already parses. Node `kind` = the
        // NodeState string and edge `kind` = the CausalEdgeType / CrossRefType
        // string, so that viewer's `kind_color` hashing lights up for free.
        //
        // Scoping by `conv_id` is MANDATORY (recon): the global `CausalGraph`
        // accumulates every turn of every conversation for the daemon's whole
        // lifetime and is never pruned — shipping it whole is untenable. Both
        // the causal nodes/edges and the UID-keyed cross-refs are filtered to
        // this conversation before serialization.
        //
        // Node identity: turn nodes are keyed by their stable universal id
        // (hex), stored on the causal node's `metadata.uid` by
        // `dual_write_turn`. This is what lets UID-based cross-refs (Speaker,
        // EmotionCause, GoalMotivation, and the M4 spawn TriggeredBy /
        // EvidenceFor) join back to a turn node — the numeric causal id alone
        // cannot, since a cross-ref endpoint is an opaque BLAKE3 hash of
        // `conv_id + chain_seq + text` the daemon does not hold. Causal
        // `Follows` edges (which reference numeric ids internally) are mapped to
        // the same UID keys via `numeric → uid`. Cross-ref endpoints that are
        // NOT turn nodes — speaker/emotion/goal identity nodes, and far-side
        // turns of a cross-conversation spawn edge — are emitted as minimal
        // stub nodes flagged `"stub": true` (with `"far_side": true` when the
        // endpoint is neither a turn of this conversation nor one of its own
        // identity/classification anchors), so a cross-conv edge is never
        // dangled against a node the projection did not emit.
        #[cfg(feature = "ecc")]
        // Voice-loop decision trace (weft voice watch's process view): read
        // incrementally by index. Companion to `conversation.graph` — the
        // graph is WHAT was said, the trace is HOW the agent decided.
        "voice.trace" => {
            let conv_id = match params.get("conv_id").and_then(|v| v.as_str()) {
                Some(c) if !c.is_empty() => c.to_string(),
                _ => return Response::error("voice.trace requires a non-empty conv_id"),
            };
            let since = params.get("since").and_then(|v| v.as_u64());
            let events = crate::voice_trace::voice_trace().read(&conv_id, since);
            let next = events.last().map(|e| e.idx);
            Response::success(serde_json::json!({
                "conv_id": conv_id,
                "events": events,
                "next": next,
            }))
        }
        // Client-posted process events (the talk client's half of the trace:
        // cue tones, TTS render timing, playback interrupts). `kind` is
        // namespaced with a `client_` prefix so daemon-side kinds can never
        // be spoofed off-process.
        "voice.trace.append" => {
            let conv_id = match params.get("conv_id").and_then(|v| v.as_str()) {
                Some(c) if !c.is_empty() => c.to_string(),
                _ => return Response::error("voice.trace.append requires a non-empty conv_id"),
            };
            let kind = match params.get("kind").and_then(|v| v.as_str()) {
                Some(k) if !k.is_empty() => format!("client_{k}"),
                _ => return Response::error("voice.trace.append requires a non-empty kind"),
            };
            let detail = params
                .get("detail")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            crate::voice_trace::voice_trace().record(&conv_id, &kind, detail);
            Response::success(serde_json::json!({ "recorded": true }))
        }
        // ADR-069 / WEFT-641: reverse-resolve an atom by chain_seq or uid.
        // Read-only over projections (P3); returns null locator when unknown.
        "atom.locate" => {
            let Some(registry) = daemon_atom_registry() else {
                return Response::error(
                    "atom.locate unavailable — atom registry not wired \
                     (enable [kernel.agent] anchor_chain)",
                );
            };
            let key = if let Some(seq) = params.get("chain_seq").and_then(|v| v.as_u64()) {
                clawft_service_agent::AtomKey::ByChainSeq(seq)
            } else if let Some(uid) = params.get("uid").and_then(|v| v.as_str()) {
                if uid.is_empty() {
                    return Response::error("atom.locate uid must be non-empty");
                }
                clawft_service_agent::AtomKey::ByUid(uid.to_string())
            } else {
                return Response::error(
                    "atom.locate requires chain_seq (u64) or uid (hex string)",
                );
            };
            match registry.locate(key) {
                Some(loc) => Response::success(serde_json::to_value(loc).unwrap_or_else(|_| {
                    serde_json::json!({ "error": "locator serialize failed" })
                })),
                None => Response::success(serde_json::json!({ "locator": null })),
            }
        }
        // ADR-069 audit — internal map agreement + optional projection samples
        // (e.g. democritus chain_seq=0 rows). Read-only; never mutates lenses.
        "atom.audit" => {
            let Some(registry) = daemon_atom_registry() else {
                return Response::error(
                    "atom.audit unavailable — atom registry not wired \
                     (enable [kernel.agent] anchor_chain)",
                );
            };
            let rows: Vec<clawft_service_agent::ProjectionAuditRow> = params
                .get("projections")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            let report = registry.audit_projections(&rows);
            Response::success(serde_json::to_value(report).unwrap_or_else(|_| {
                serde_json::json!({ "ok": false, "error": "report serialize failed" })
            }))
        }
        "conversation.graph" => {
            let conv_id = match params.get("conv_id").and_then(|v| v.as_str()) {
                Some(c) if !c.is_empty() => c.to_string(),
                _ => return Response::error("conversation.graph requires a non-empty conv_id"),
            };
            let since = params.get("since").and_then(|v| v.as_u64());
            let (window_from, window_to) = match params.get("window") {
                Some(w) => (
                    w.get("from_ts").and_then(|v| v.as_u64()),
                    w.get("to_ts").and_then(|v| v.as_u64()),
                ),
                None => (None, None),
            };

            let k = kernel.read().await;
            let Some(causal) = k.ecc_causal() else {
                // Honest empty-graph shape rather than an error, so the GUI can
                // render the "ECC conversation loop not enabled" empty-state
                // over a well-formed payload (ADR-067 D7).
                return Response::success(serde_json::json!({
                    "conv_id": conv_id,
                    "nodes": [],
                    "edges": [],
                }));
            };

            // --- Nodes: turn nodes of this conversation, filtered by since/window.
            // `numeric → uid` maps causal edge endpoints onto the UID keys the
            // cross-refs use; `conv_uids` is the membership set that scopes
            // cross-refs and drives stub emission. `emitted` is every node id we
            // actually ship, so no edge is ever dangled.
            let mut numeric_to_uid: std::collections::HashMap<u64, String> =
                std::collections::HashMap::new();
            let mut conv_uids: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            let mut emitted: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            let mut nodes_json: Vec<serde_json::Value> = Vec::new();

            for node in causal.nodes_for_conv(&conv_id) {
                let meta = &node.metadata;
                let chain_seq = meta.get("chain_seq").and_then(|v| v.as_u64());
                let ts_ms = node.created_at;
                if let Some(s) = since
                    && chain_seq.map(|c| c < s).unwrap_or(false)
                {
                    continue;
                }
                if let Some(from) = window_from
                    && ts_ms < from
                {
                    continue;
                }
                if let Some(to) = window_to
                    && ts_ms > to
                {
                    continue;
                }
                // Canonical id: the stable UID (hex) written by dual_write_turn;
                // fall back to the numeric id (stringified) for any legacy node
                // that predates the uid metadata.
                let uid = meta
                    .get("uid")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned)
                    .unwrap_or_else(|| node.id.to_string());
                numeric_to_uid.insert(node.id, uid.clone());
                conv_uids.insert(uid.clone());
                emitted.insert(uid.clone());

                let role = meta.get("role").and_then(|v| v.as_str()).unwrap_or("");
                let state = meta.get("state").and_then(|v| v.as_str()).unwrap_or("unknown");
                let text = meta.get("text").and_then(|v| v.as_str()).unwrap_or("");
                let classification = meta.get("classification").cloned().unwrap_or(serde_json::Value::Null);
                // Wave 1 §W1.2: serve the sibling voice decomposition verbatim
                // (flat keys — the 422c40ea parity lesson). Null for text turns
                // and any node that predates the record, so the surface / GUI
                // node-detail can distinguish an analysed voice turn from a
                // text one without a schema change.
                let voice_analysis = meta.get("voice_analysis").cloned().unwrap_or(serde_json::Value::Null);
                // Wave 2 §W2.6: an in-flight reply is an ATTEMPT node —
                // `register_reply_frontier` (session_tier.rs) stashes
                // `metadata.kind = "reply-attempt"` and `metadata.goal` on the
                // node itself, and neither is in `PROTECTED_METADATA_KEYS`, so
                // both survive the node's full frontier→committed/pruned
                // lifecycle untouched. The top-level `"kind"` key above already
                // mirrors `state` (viewer compatibility, unchanged); project
                // the attempt marker + goal as ADDITIVE flat keys (the
                // 422c40ea parity lesson) so the watch surface can render
                // attempt lifecycle without a schema break.
                let is_attempt = meta.get("kind").and_then(|v| v.as_str()) == Some("reply-attempt");
                let goal = meta.get("goal").and_then(|v| v.as_str()).unwrap_or("");
                nodes_json.push(serde_json::json!({
                    "id": uid,
                    "label": node.label,
                    "kind": state,
                    "chain_seq": chain_seq,
                    "conv_id": conv_id,
                    "role": role,
                    "text": text,
                    "state": state,
                    "ts_ms": ts_ms,
                    "classification": classification,
                    "voice_analysis": voice_analysis,
                    "attempt": is_attempt,
                    "goal": goal,
                }));
            }

            // --- Causal Follows-lineage edges (intra-conversation). Both
            // endpoints are guaranteed in-conversation by `edges_for_conv`;
            // require both to have survived since/window filtering too.
            let mut edges_json: Vec<serde_json::Value> = Vec::new();
            for edge in causal.edges_for_conv(&conv_id) {
                let (Some(src), Some(tgt)) = (
                    numeric_to_uid.get(&edge.source),
                    numeric_to_uid.get(&edge.target),
                ) else {
                    continue;
                };
                if !emitted.contains(src) || !emitted.contains(tgt) {
                    continue;
                }
                edges_json.push(serde_json::json!({
                    "source": src,
                    "target": tgt,
                    "kind": edge.edge_type.to_string(),
                    "weight": edge.weight,
                    "chain_seq": edge.chain_seq,
                }));
            }

            // --- Cross-ref edges (Speaker / EmotionCause / GoalMotivation /
            // spawn TriggeredBy / EvidenceFor / Continuer …). A cross-ref is in
            // scope iff at least one endpoint is a turn node of this
            // conversation. Endpoints that are not this conversation's turn
            // nodes get a stub node so the edge never dangles.
            let mut stubs: std::collections::HashMap<String, bool> =
                std::collections::HashMap::new();
            if let Some(crossrefs) = k.ecc_crossrefs() {
                for cr in crossrefs.all() {
                    if let Some(s) = since
                        && cr.chain_seq < s
                    {
                        continue;
                    }
                    let src = cr.source.to_string();
                    let tgt = cr.target.to_string();
                    let src_in = conv_uids.contains(&src);
                    let tgt_in = conv_uids.contains(&tgt);
                    if !src_in && !tgt_in {
                        continue;
                    }
                    // A local turn endpoint dropped by since/window filtering
                    // means the edge has no anchor to draw against — skip it.
                    if (src_in && !emitted.contains(&src)) || (tgt_in && !emitted.contains(&tgt)) {
                        continue;
                    }
                    // `far_side` = the endpoint is neither a turn of this
                    // conversation nor one of its own identity/classification
                    // anchors — i.e. the far turn of a cross-conversation spawn
                    // edge, which lives in another conversation's graph.
                    if !src_in {
                        stubs.entry(src.clone()).or_insert(true);
                    }
                    if !tgt_in {
                        stubs.entry(tgt.clone()).or_insert(true);
                    }
                    edges_json.push(serde_json::json!({
                        "source": src,
                        "target": tgt,
                        "kind": cr.ref_type.to_string(),
                        "weight": 1.0,
                        "chain_seq": cr.chain_seq,
                    }));
                }
            }

            // Materialize stub nodes for cross-ref endpoints outside this
            // conversation's turn set. Speaker/emotion/goal identity anchors and
            // far-side cross-conv turns share this shape; `far_side` is always
            // true here (a non-turn endpoint is, by definition, not one of this
            // conversation's own emitted nodes).
            for (uid, far_side) in stubs {
                if emitted.insert(uid.clone()) {
                    nodes_json.push(serde_json::json!({
                        "id": uid,
                        "label": "",
                        "kind": "external",
                        "state": "external",
                        "stub": true,
                        "far_side": far_side,
                    }));
                }
            }

            Response::success(serde_json::json!({
                "conv_id": conv_id,
                "nodes": nodes_json,
                "edges": edges_json,
            }))
        }
        // ── Custody attestation ───────────────────────────────────────
        "custody.attest" => {
            #[cfg(feature = "exochain")]
            {
                let k = kernel.read().await;
                if let Some(cm) = k.chain_manager() {
                    #[cfg(feature = "ecc")]
                    let (vector_count, epoch, content_hash) = {
                        if let Some(vb) = k.ecc_vector_backend() {
                            let count = vb.len() as u64;
                            let ep = vb.current_epoch();
                            let hash_input = format!("vector-ids:count={count}:epoch={ep}");
                            let hash = blake3::hash(hash_input.as_bytes());
                            (count, ep, hash.to_hex().to_string())
                        } else if let Some(hnsw) = k.ecc_hnsw() {
                            let count = hnsw.len() as u64;
                            let hash_input = format!("hnsw-ids:count={count}");
                            let hash = blake3::hash(hash_input.as_bytes());
                            (count, 0u64, hash.to_hex().to_string())
                        } else {
                            (0u64, 0u64, "none".to_string())
                        }
                    };
                    #[cfg(not(feature = "ecc"))]
                    let (vector_count, epoch, content_hash) = (0u64, 0u64, "none".to_string());

                    match cm.generate_attestation(vector_count, epoch, &content_hash) {
                        Some(att) => {
                            let sig_hex: String =
                                att.signature.iter().map(|b| format!("{b:02x}")).collect();
                            let result = crate::protocol::CustodyAttestResult {
                                device_id: att.device_id,
                                epoch: att.epoch,
                                chain_head: att.chain_head,
                                chain_depth: att.chain_depth,
                                vector_count: att.vector_count,
                                content_hash: att.content_hash,
                                timestamp: att.timestamp,
                                signature: sig_hex,
                            };
                            Response::success(serde_json::to_value(result).unwrap())
                        }
                        None => Response::error(
                            "no signing key configured — attestation requires Ed25519 key",
                        ),
                    }
                } else {
                    Response::error("chain not enabled")
                }
            }
            #[cfg(not(feature = "exochain"))]
            Response::error("exochain feature not enabled")
        }

        // ── Host revocation ──────────────────────────────────────────
        "mesh.revoke" => {
            let revoke_params: crate::protocol::MeshRevokeParams =
                match serde_json::from_value(params) {
                    Ok(p) => p,
                    Err(e) => return Response::error(format!("invalid params: {e}")),
                };
            let k = kernel.read().await;
            let list = k.revocation_list();
            let added = list.revoke_host(&revoke_params.host_id, &revoke_params.reason);

            #[cfg(feature = "exochain")]
            if let Some(cm) = k.chain_manager() {
                cm.append(
                    "mesh",
                    "host.revoked",
                    Some(serde_json::json!({
                        "host_id": revoke_params.host_id,
                        "reason": revoke_params.reason,
                    })),
                );
            }

            Response::success(serde_json::json!({
                "host_id": revoke_params.host_id,
                "added": added,
            }))
        }
        "mesh.unrevoke" => {
            let unrevoke_params: crate::protocol::MeshUnrevokeParams =
                match serde_json::from_value(params) {
                    Ok(p) => p,
                    Err(e) => return Response::error(format!("invalid params: {e}")),
                };
            let k = kernel.read().await;
            let list = k.revocation_list();
            let removed = list.unrevoke_host(&unrevoke_params.host_id);

            #[cfg(feature = "exochain")]
            if let Some(cm) = k.chain_manager() {
                cm.append(
                    "mesh",
                    "host.unrevoked",
                    Some(serde_json::json!({
                        "host_id": unrevoke_params.host_id,
                    })),
                );
            }

            Response::success(serde_json::json!({
                "host_id": unrevoke_params.host_id,
                "removed": removed,
            }))
        }
        "mesh.revoked" => {
            let k = kernel.read().await;
            let list = k.revocation_list();
            let entries: Vec<crate::protocol::RevokedHostInfo> = list
                .list_revoked()
                .into_iter()
                .map(|h| crate::protocol::RevokedHostInfo {
                    host_id: h.host_id,
                    revoked_at: h.revoked_at,
                    reason: h.reason,
                })
                .collect();
            Response::success(serde_json::to_value(entries).unwrap())
        }

        "ping" => Response::success(serde_json::json!("pong")),
        "kernel.liveness" => crate::liveness::rpc(),

        // ── Workspace methods ─────────────────────────────────────────
        "workspace.create" => {
            use clawft_core::workspace::WorkspaceManager;

            let name = match params["name"].as_str() {
                Some(n) => n,
                None => return Response::error("missing required param: name"),
            };
            let dir = params["dir"]
                .as_str()
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| ".".into()));

            match WorkspaceManager::new() {
                Ok(mut mgr) => match mgr.create(name, &dir) {
                    Ok(path) => Response::success(serde_json::json!({
                        "name": name,
                        "path": path.display().to_string(),
                    })),
                    Err(e) => Response::error(format!("workspace create failed: {e}")),
                },
                Err(e) => Response::error(format!("workspace manager init failed: {e}")),
            }
        }
        "workspace.list" => {
            use clawft_core::workspace::WorkspaceManager;

            match WorkspaceManager::new() {
                Ok(mgr) => {
                    let entries: Vec<serde_json::Value> = mgr
                        .list()
                        .iter()
                        .map(|e| {
                            let exists = e.path.join(".clawft").is_dir();
                            serde_json::json!({
                                "name": e.name,
                                "path": e.path.display().to_string(),
                                "status": if exists { "ok" } else { "MISSING" },
                                "last_accessed": e.last_accessed
                                    .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
                                    .unwrap_or_else(|| "-".into()),
                            })
                        })
                        .collect();
                    Response::success(serde_json::to_value(entries).unwrap())
                }
                Err(e) => Response::error(format!("workspace manager init failed: {e}")),
            }
        }
        "workspace.load" => {
            use clawft_core::workspace::WorkspaceManager;

            let name_or_path = params["name_or_path"]
                .as_str()
                .or_else(|| params["name"].as_str());
            let name_or_path = match name_or_path {
                Some(n) => n,
                None => return Response::error("missing required param: name_or_path"),
            };

            match WorkspaceManager::new() {
                Ok(mut mgr) => match mgr.load(name_or_path) {
                    Ok(path) => Response::success(serde_json::json!({
                        "path": path.display().to_string(),
                    })),
                    Err(e) => Response::error(format!("workspace load failed: {e}")),
                },
                Err(e) => Response::error(format!("workspace manager init failed: {e}")),
            }
        }
        "workspace.status" => {
            use clawft_core::workspace::{WorkspaceManager, discover_workspace};

            let ws_path = match discover_workspace() {
                Some(p) => p,
                None => return Response::error("no workspace found"),
            };

            match WorkspaceManager::new() {
                Ok(mgr) => match mgr.status(&ws_path) {
                    Ok(status) => Response::success(serde_json::json!({
                        "name": status.name,
                        "path": status.path.display().to_string(),
                        "session_count": status.session_count,
                        "has_config": status.has_config,
                        "has_clawft_md": status.has_clawft_md,
                    })),
                    Err(e) => Response::error(format!("workspace status failed: {e}")),
                },
                Err(e) => Response::error(format!("workspace manager init failed: {e}")),
            }
        }
        "workspace.delete" => {
            use clawft_core::workspace::WorkspaceManager;

            let name = match params["name"].as_str() {
                Some(n) => n,
                None => return Response::error("missing required param: name"),
            };
            // FR-W06 / WEFT-86: default removes .clawft/ + CLAWFT.md;
            // keep_data=true is registry-only (pre-0.8 behavior).
            let keep_data = params["keep_data"].as_bool().unwrap_or(false);

            match WorkspaceManager::new() {
                Ok(mut mgr) => match mgr.delete(name, keep_data) {
                    Ok(()) => Response::success(serde_json::json!({
                        "deleted": name,
                        "keep_data": keep_data,
                    })),
                    Err(e) => Response::error(format!("workspace delete failed: {e}")),
                },
                Err(e) => Response::error(format!("workspace manager init failed: {e}")),
            }
        }
        "workspace.config.set" => {
            use clawft_core::workspace::discover_workspace;

            let key = match params["key"].as_str() {
                Some(k) => k,
                None => return Response::error("missing required param: key"),
            };
            let value = match params["value"].as_str() {
                Some(v) => v,
                None => return Response::error("missing required param: value"),
            };

            let ws_path = match discover_workspace() {
                Some(p) => p,
                None => return Response::error("no workspace found"),
            };

            let config_path = ws_path.join(".clawft").join("config.json");
            let mut config: serde_json::Value = if config_path.exists() {
                match std::fs::read_to_string(&config_path) {
                    Ok(content) => serde_json::from_str(&content).unwrap_or(serde_json::json!({})),
                    Err(_) => serde_json::json!({}),
                }
            } else {
                serde_json::json!({})
            };

            // Navigate/create nested keys using dot notation
            let parts: Vec<&str> = key.split('.').collect();
            let mut current = &mut config;
            for part in &parts[..parts.len() - 1] {
                if !current.is_object() {
                    *current = serde_json::json!({});
                }
                current = current
                    .as_object_mut()
                    .unwrap()
                    .entry(*part)
                    .or_insert_with(|| serde_json::json!({}));
            }
            let last = parts[parts.len() - 1];
            // Parse value as JSON primitive
            let json_value = if value == "true" {
                serde_json::Value::Bool(true)
            } else if value == "false" {
                serde_json::Value::Bool(false)
            } else if let Ok(n) = value.parse::<i64>() {
                serde_json::Value::Number(n.into())
            } else {
                serde_json::Value::String(value.into())
            };
            if !current.is_object() {
                *current = serde_json::json!({});
            }
            current
                .as_object_mut()
                .unwrap()
                .insert(last.into(), json_value);

            match std::fs::write(&config_path, serde_json::to_string_pretty(&config).unwrap()) {
                Ok(()) => Response::success(serde_json::json!({ "key": key, "value": value })),
                Err(e) => Response::error(format!("failed to write config: {e}")),
            }
        }
        "workspace.config.get" => {
            use clawft_core::workspace::discover_workspace;

            let key = match params["key"].as_str() {
                Some(k) => k,
                None => return Response::error("missing required param: key"),
            };

            let ws_path = match discover_workspace() {
                Some(p) => p,
                None => return Response::error("no workspace found"),
            };

            let config_path = ws_path.join(".clawft").join("config.json");
            if !config_path.exists() {
                return Response::success(serde_json::Value::Null);
            }

            match std::fs::read_to_string(&config_path) {
                Ok(content) => {
                    let config: serde_json::Value =
                        serde_json::from_str(&content).unwrap_or(serde_json::json!({}));
                    let mut current = &config;
                    for part in key.split('.') {
                        match current.get(part) {
                            Some(v) => current = v,
                            None => return Response::success(serde_json::Value::Null),
                        }
                    }
                    Response::success(current.clone())
                }
                Err(e) => Response::error(format!("failed to read config: {e}")),
            }
        }
        "workspace.config.reset" => {
            use clawft_core::workspace::discover_workspace;

            let ws_path = match discover_workspace() {
                Some(p) => p,
                None => return Response::error("no workspace found"),
            };

            let config_path = ws_path.join(".clawft").join("config.json");
            match std::fs::write(&config_path, "{}\n") {
                Ok(()) => Response::success(serde_json::json!({ "reset": true })),
                Err(e) => Response::error(format!("failed to reset config: {e}")),
            }
        }

        // mesh-placement-06 / ADR-099: `weaver app` and `workload.*` families.
        m if m.starts_with("app.") => crate::app_rpc::dispatch(m, params, kernel).await,
        #[cfg(feature = "exochain")]
        m if m.starts_with("workload.") => crate::workload_rpc::dispatch(m, params, kernel).await,
        #[cfg(not(feature = "exochain"))]
        m if m.starts_with("workload.") => {
            Response::error(format!("{m} requires the exochain governance feature"))
        }

        other => Response::error(format!("unknown method: {other}")),
    }
}

// ── Voice consumer wiring (WEFT-555 / M5-W) ──────────────────────────
//
// Production handlers that bridge the substrate-side voice transcript
// stream into the daemon's existing surfaces. The consumer trait
// surface lives in `crate::voice_router`; these impls are the only
// place that touches daemon-wide state (`DAEMON_AGENT`, `dispatch`).

/// Routes a voice transcript into the daemon's wired agent service.
/// Synthesizes a single-turn `agent.chat` request and feeds it through
/// `clawft_service_agent::AgentService::dispatch`. Source attribution
/// (`source: voice`, transcript_topic, confidence) lands as a
/// `system`-role message prepended to the conversation so the loop's
/// system prompt sees it without changing the wire shape.
struct DaemonAgentChatHandler;

#[async_trait::async_trait]
impl crate::voice_router::ChatHandler for DaemonAgentChatHandler {
    async fn dispatch_chat(&self, turn: crate::voice_router::VoiceChatTurn) -> Result<(), String> {
        let Some(agent) = daemon_agent() else {
            return Err("agent service not wired (DAEMON_AGENT unset)".into());
        };
        let _ = turn.target_agent; // single-tenant concierge today (Phase D2 wiring)
        let confidence_str = turn
            .metadata
            .confidence
            .map(|c| format!("{c:.3}"))
            .unwrap_or_else(|| "n/a".into());
        let system_prefix = format!(
            "[voice transcript — source={} topic={} confidence={}]",
            turn.metadata.source, turn.metadata.transcript_topic, confidence_str,
        );
        let params = clawft_service_agent::AgentChatParams {
            messages: vec![
                clawft_service_agent::AgentChatMessage {
                    role: "system".into(),
                    content: system_prefix,
                            audio: None,
        },
                clawft_service_agent::AgentChatMessage {
                    role: "user".into(),
                    content: turn.text,
                            audio: None,
        },
            ],
            temperature: None,
            max_tokens: None,
            conv_id: turn.conv_id,
            metadata: None,
            caller_id: None,
        };
        agent
            .dispatch(params)
            .await
            .map(|_| ())
            .map_err(|e| format!("agent.chat dispatch: {e}"))
    }
}

/// Routes a parsed `weft <verb> <args>` transcript into the daemon's
/// JSON-RPC `dispatch` function. Uses the same dispatch entry point
/// the Unix-socket handler uses, so every method that works for the
/// panel works for voice — subject to the placeholder permission gate
/// in [`crate::voice_router`] (replaced by WEFT-208).
struct DaemonCommandHandler {
    kernel: Arc<tokio::sync::RwLock<Kernel<NativePlatform>>>,
    /// Throwaway shutdown channel — the consumer will not initiate a
    /// daemon shutdown; verbs that try (`kernel.shutdown`) flip this
    /// local channel rather than the real one.
    shutdown_tx: watch::Sender<bool>,
    /// Holds the daemon's control flag handle alive for the lifetime
    /// of the handler. Read-only today; future per-verb intent
    /// publication (WEFT-210 audit hook) reads it.
    _control: ControlFlags,
}

#[async_trait::async_trait]
impl crate::voice_router::CommandHandler for DaemonCommandHandler {
    async fn dispatch_command(
        &self,
        method: String,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        // Internal principal with capped capabilities (no Admin); see
        // `CallerCtx::internal_voice`.
        let resp = dispatch_authorized(
            &crate::rpc_ext::CallerCtx::internal_voice(),
            method,
            params,
            Arc::clone(&self.kernel),
            self.shutdown_tx.clone(),
        )
        .await;
        if resp.ok {
            Ok(resp.result.unwrap_or(serde_json::Value::Null))
        } else {
            Err(resp.error.unwrap_or_else(|| "unknown error".into()))
        }
    }
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use super::*;

    /// Kernel config whose chain (RVF, key, tree checkpoint) lives in a
    /// fresh temp dir, so unit tests never load or touch `~/.clawft`.
    #[allow(dead_code)]
    fn isolated_kcfg() -> KernelConfig {
        KernelConfig {
            chain: Some(clawft_types::config::ChainConfig::isolated_in(
                &tempfile::tempdir().unwrap().keep(),
            )),
            ..KernelConfig::default()
        }
    }

    /// Review d302d81c item 4: the idle clock moves only for a call that
    /// passed authorization. A denied one (here: a read-scope caller asking
    /// for an Admin verb) must not keep a project kernel awake.
    #[tokio::test]
    async fn only_an_authorized_call_moves_the_idle_clock() {
        use crate::project_boot_run::test_hooks;
        let _serial = test_hooks::SERIAL.lock().await;
        let kernel = Kernel::boot(
            clawft_types::config::Config::default(),
            isolated_kcfg(),
            Arc::new(NativePlatform::new()),
        )
        .await
        .expect("kernel boots");
        let kernel = Arc::new(tokio::sync::RwLock::new(kernel));
        test_hooks::track(true);
        // Restore the process-wide switch even when an assertion fails.
        struct Untrack;
        impl Drop for Untrack {
            fn drop(&mut self) {
                crate::project_boot_run::test_hooks::track(false);
            }
        }
        let _untrack = Untrack;
        let params = serde_json::json!({});
        let mut denied = crate::rpc_ext::CallerCtx::from_auth(Some("read".into()));
        let r = authorize_caller(&mut denied, "project.stop_all", &params, &kernel).await;
        assert!(r.is_err(), "a read-scope caller may not run project.stop_all");
        assert_eq!(test_hooks::last_activity(), 0, "a denied call is not activity");
        let mut allowed = crate::rpc_ext::CallerCtx::from_auth(Some("admin".into()));
        let r = authorize_caller(&mut allowed, "project.stop_all", &params, &kernel).await;
        assert!(r.is_ok(), "the owner may");
        assert!(test_hooks::last_activity() > 0, "an authorized call is activity");
    }

    /// The voice consumer must dispatch as a capped internal principal:
    /// Admin verbs are refused, and a raw `dispatch` regression would let
    /// `kernel.shutdown` through.
    #[tokio::test]
    async fn voice_command_handler_denies_admin_verbs() {
        use crate::voice_router::CommandHandler;
        let kernel = Kernel::boot(
            clawft_types::config::Config::default(),
            isolated_kcfg(),
            Arc::new(NativePlatform::new()),
        )
        .await
        .expect("kernel boots");
        let (tx, rx) = watch::channel(false);
        let handler = DaemonCommandHandler {
            kernel: Arc::new(tokio::sync::RwLock::new(kernel)),
            shutdown_tx: tx,
            _control: ControlFlags::new(),
        };
        let err = handler
            .dispatch_command("kernel.shutdown".into(), serde_json::Value::Null)
            .await
            .unwrap_err();
        assert!(err.contains("permission denied"), "{err}");
        assert!(!*rx.borrow(), "shutdown must not have been signalled");
        // Non-admin verbs still work for the voice principal.
        assert!(
            handler
                .dispatch_command("kernel.status".into(), serde_json::Value::Null)
                .await
                .is_ok()
        );
    }

    /// The daemon boots the kernel with its `node.key`: the kernel's cluster
    /// node id is the identity's node id and is stable across reboots.
    #[cfg(feature = "mesh")]
    #[tokio::test]
    async fn daemon_boot_passes_node_key_into_kernel() {
        let dir = tempfile::tempdir().unwrap();
        let identity = crate::node_identity::load_or_generate(dir.path()).unwrap();
        for _ in 0..2 {
            let mut kernel = boot_kernel_with_identity(
                Config::default(),
                isolated_kcfg(),
                Arc::new(NativePlatform::new()),
                &identity,
            )
            .await
            .expect("boot");
            assert_eq!(kernel.cluster_membership().local_node_id(), identity.node_id);
            kernel.shutdown().await.unwrap();
        }
    }

    #[test]
    fn socket_path_resolves() {
        let path = crate::protocol::socket_path();
        assert!(path.to_string_lossy().ends_with("kernel.sock"));
    }

    /// ADR-067 P0: `conversation.graph` projects one conversation's causal
    /// forest into `{nodes, edges}`, scoped strictly to its `conv_id`.
    ///
    /// Builds a two-turn conversation (`conv-A`) with a `Follows` lineage edge
    /// and a `Speaker` cross-ref, plus a cross-conversation spawn `TriggeredBy`
    /// cross-ref pointing from a far-side turn in `conv-B` into `conv-A`'s first
    /// turn, and a decoy turn in `conv-B`. Asserts the JSON shape, that the
    /// decoy never leaks, that lineage + cross-ref edges are present, and that
    /// non-turn / far-side endpoints are emitted as flagged stub nodes.
    #[cfg(feature = "ecc")]
    #[tokio::test]
    async fn conversation_graph_scopes_and_shapes() {
        use clawft_kernel::causal::CausalEdgeType;
        use clawft_kernel::crossref::{CrossRef, CrossRefType, StructureTag, UniversalNodeId};
        use clawft_kernel::Kernel;
        use clawft_platform::NativePlatform;
        use clawft_types::config::Config;
        use std::sync::Arc;

        // Mirror `dual_write_turn`'s node metadata so the handler reads the same
        // `uid` key it relies on to join cross-refs back to turn nodes.
        fn turn_uid(conv: &str, seq: u64, text: &str) -> UniversalNodeId {
            UniversalNodeId::new(
                &StructureTag::CausalGraph,
                conv.as_bytes(),
                seq,
                text.as_bytes(),
                b"turn",
            )
        }
        fn speaker_uid(conv: &str, role: &str) -> UniversalNodeId {
            UniversalNodeId::new(
                &StructureTag::CausalGraph,
                conv.as_bytes(),
                0,
                role.as_bytes(),
                b"speaker",
            )
        }

        let platform = Arc::new(NativePlatform::new());
        let kernel = Kernel::boot(Config::default(), isolated_kcfg(), platform)
            .await
            .expect("kernel boots");
        let causal = kernel.ecc_causal().expect("causal graph present").clone();
        let crossrefs = kernel.ecc_crossrefs().expect("crossref store present").clone();

        // conv-A: turn 1 (user) → turn 2 (assistant) via Follows.
        let a_uid1 = turn_uid("conv-A", 1, "hello");
        let a_uid2 = turn_uid("conv-A", 2, "hi there");
        // Mirror what `dual_write_turn` writes when the classifier is on
        // (classification-design §D2 blob + §6 `text` piggyback) so the
        // projection is exercised the way the keyword tier populates it.
        let a_class1 = serde_json::json!({
            "intent": "social",
            "topic": "hello",
            "emotion": { "valence": 0.2, "arousal": 0.5, "dominance": 0.0, "label": "neutral" },
            "goal": serde_json::Value::Null,
            "tier": "keyword",
            "v": 1,
        });
        let n1 = causal.add_node(
            "turn:conv-A:1".into(),
            serde_json::json!({
                "conv_id": "conv-A", "chain_seq": 1, "role": "user",
                "state": "committed", "uid": a_uid1.to_string(),
                "text": "hello", "classification": a_class1,
            }),
        );
        let n2 = causal.add_node(
            "turn:conv-A:2".into(),
            serde_json::json!({
                "conv_id": "conv-A", "chain_seq": 2, "role": "assistant",
                "state": "committed", "uid": a_uid2.to_string(),
                "text": "hi there",
                "classification": {
                    "intent": "statement",
                    "topic": "hello",
                    "emotion": { "valence": 0.3, "arousal": 0.5, "dominance": 0.0, "label": "neutral" },
                    "goal": serde_json::Value::Null,
                    "tier": "keyword",
                    "v": 1,
                },
            }),
        );
        assert!(causal.link(n1, n2, CausalEdgeType::Follows, 0.9, 0, 2));

        // Speaker cross-ref: turn-2 → assistant identity node.
        let a_speaker = speaker_uid("conv-A", "assistant");
        crossrefs.insert(CrossRef {
            source: a_uid2.clone(),
            source_structure: StructureTag::CausalGraph,
            target: a_speaker.clone(),
            target_structure: StructureTag::CausalGraph,
            ref_type: CrossRefType::Speaker,
            created_at: 0,
            chain_seq: 2,
        });

        // Cross-conversation spawn: far-side turn in conv-B TriggeredBy conv-A turn-1.
        let b_child_goal = turn_uid("conv-B", 5, "child goal");
        crossrefs.insert(CrossRef {
            source: b_child_goal.clone(),
            source_structure: StructureTag::CausalGraph,
            target: a_uid1.clone(),
            target_structure: StructureTag::CausalGraph,
            ref_type: CrossRefType::TriggeredBy,
            created_at: 0,
            chain_seq: 5,
        });

        // Decoy turn in conv-B — must never appear in conv-A's projection.
        let b_uid1 = turn_uid("conv-B", 5, "child goal");
        causal.add_node(
            "turn:conv-B:5".into(),
            serde_json::json!({
                "conv_id": "conv-B", "chain_seq": 5, "role": "user",
                "state": "committed", "uid": b_uid1.to_string(),
            }),
        );

        let kernel = Arc::new(tokio::sync::RwLock::new(kernel));
        let (shutdown_tx, _rx) = watch::channel(false);
        let resp = dispatch(
            "conversation.graph".into(),
            serde_json::json!({ "conv_id": "conv-A" }),
            kernel,
            shutdown_tx,
        )
        .await;
        assert!(resp.ok, "handler ok: {:?}", resp.error);
        let result = resp.result.expect("result present");
        assert_eq!(result["conv_id"], "conv-A");

        let nodes = result["nodes"].as_array().expect("nodes array");
        let edges = result["edges"].as_array().expect("edges array");

        // Both conv-A turns present, keyed by their UID hex; every emitted turn
        // node is scoped to conv-A (nothing from conv-B leaks).
        let node_ids: std::collections::HashSet<&str> =
            nodes.iter().filter_map(|n| n["id"].as_str()).collect();
        assert!(node_ids.contains(a_uid1.to_string().as_str()));
        assert!(node_ids.contains(a_uid2.to_string().as_str()));
        for n in nodes {
            if n["stub"].as_bool().unwrap_or(false) {
                continue;
            }
            assert_eq!(n["conv_id"], "conv-A", "no foreign conv turn leaked: {n}");
        }

        // Every (non-stub) turn node surfaces its `text` and a populated 4-axis
        // `classification` blob verbatim (classification-design §D2, §6). This
        // is the RPC-surfacing exit criterion for Phase A — the blob is read
        // straight from node metadata with no wire change.
        let a_turn1 = nodes
            .iter()
            .find(|n| n["id"].as_str() == Some(a_uid1.to_string().as_str()))
            .expect("conv-A turn-1 present");
        assert_eq!(a_turn1["text"], "hello", "turn text surfaced: {a_turn1}");
        let class1 = &a_turn1["classification"];
        assert!(class1.is_object(), "classification blob present: {a_turn1}");
        assert_eq!(class1["intent"], "social");
        assert_eq!(class1["topic"], "hello");
        assert_eq!(class1["tier"], "keyword");
        assert_eq!(class1["v"], 1);
        assert!(
            class1["emotion"]["arousal"].is_number(),
            "emotion arousal present for the floor (design §5): {class1}"
        );
        assert!(class1.get("goal").is_some(), "goal axis present (may be null)");

        let a_turn2 = nodes
            .iter()
            .find(|n| n["id"].as_str() == Some(a_uid2.to_string().as_str()))
            .expect("conv-A turn-2 present");
        assert_eq!(a_turn2["text"], "hi there");
        assert!(
            a_turn2["classification"].is_object(),
            "turn-2 classification blob present: {a_turn2}"
        );
        // The decoy conv-B turn's UID must not appear as a *turn* node. It shares
        // its UID with the spawn source (same conv/seq/text), which is correctly
        // emitted only as a far-side stub — assert that entry is a stub.
        let decoy = nodes
            .iter()
            .find(|n| n["id"].as_str() == Some(b_child_goal.to_string().as_str()))
            .expect("far-side spawn endpoint emitted");
        assert_eq!(decoy["stub"], serde_json::json!(true));
        assert_eq!(decoy["far_side"], serde_json::json!(true));

        // Speaker identity endpoint is a stub too.
        let speaker_node = nodes
            .iter()
            .find(|n| n["id"].as_str() == Some(a_speaker.to_string().as_str()))
            .expect("speaker stub emitted");
        assert_eq!(speaker_node["stub"], serde_json::json!(true));

        // Edges: Follows lineage, Speaker cross-ref, TriggeredBy spawn.
        let has_edge = |kind: &str, src: &str, tgt: &str| {
            edges.iter().any(|e| {
                e["kind"] == kind && e["source"].as_str() == Some(src) && e["target"].as_str() == Some(tgt)
            })
        };
        assert!(
            has_edge("Follows", &a_uid1.to_string(), &a_uid2.to_string()),
            "lineage edge present: {edges:?}"
        );
        assert!(
            has_edge("Speaker", &a_uid2.to_string(), &a_speaker.to_string()),
            "speaker cross-ref edge present"
        );
        assert!(
            has_edge("TriggeredBy", &b_child_goal.to_string(), &a_uid1.to_string()),
            "cross-conv spawn edge present"
        );
    }

    /// WEFT-364: `ecc.vector-config` returns the live backend name plus the
    /// configured parameters from `[kernel.vector]`.
    #[cfg(feature = "ecc")]
    #[tokio::test]
    async fn ecc_vector_config_returns_active_backend_and_parameters() {
        use clawft_kernel::Kernel;
        use clawft_platform::NativePlatform;
        use clawft_types::config::{Config, VectorBackendKind, VectorConfig, VectorHnswConfig};
        use std::sync::Arc;

        let platform = Arc::new(NativePlatform::new());
        let mut kconfig = isolated_kcfg();
        kconfig.vector = Some(VectorConfig {
            backend: VectorBackendKind::Hnsw,
            strict: false,
            hnsw: Some(VectorHnswConfig {
                ef_construction: 128,
                m: 12,
                max_elements: 5_000,
            }),
            diskann: None,
            hybrid: None,
            log_quantized: None,
            simd_distance: None,
        });
        let kernel = Kernel::boot(Config::default(), kconfig, platform)
            .await
            .expect("kernel boots with HNSW vector config");
        let kernel = Arc::new(tokio::sync::RwLock::new(kernel));
        let (shutdown_tx, _rx) = watch::channel(false);

        let resp = dispatch(
            "ecc.vector-config".into(),
            serde_json::json!({}),
            kernel,
            shutdown_tx,
        )
        .await;
        assert!(resp.ok, "ecc.vector-config should succeed: {resp:?}");
        let result = resp.result.expect("result present");

        assert_eq!(result["active"], true, "vector backend should be active");
        assert_eq!(
            result["backend"].as_str(),
            Some("hnsw"),
            "live backend_name should be hnsw: {result}"
        );
        assert_eq!(
            result["configured_backend"].as_str(),
            Some("hnsw"),
            "configured_backend from VectorConfig: {result}"
        );
        assert_eq!(result["strict"], false);
        assert!(result["vector_count"].as_u64().is_some());
        assert!(result["epoch"].as_u64().is_some());
        assert_eq!(result["parameters"]["hnsw"]["ef_construction"], 128);
        assert_eq!(result["parameters"]["hnsw"]["m"], 12);
        assert_eq!(result["parameters"]["hnsw"]["max_elements"], 5_000);
        assert!(result["parameters"]["diskann"].is_null());
        assert!(result["parameters"]["hybrid"].is_null());
    }

    /// WEFT-364: default kernel still reports an active HNSW backend
    /// (boot always constructs a vector backend when ecc is enabled).
    #[cfg(feature = "ecc")]
    #[tokio::test]
    async fn ecc_vector_config_default_kernel_reports_hnsw() {
        use clawft_kernel::Kernel;
        use clawft_platform::NativePlatform;
        use clawft_types::config::Config;
        use std::sync::Arc;

        let platform = Arc::new(NativePlatform::new());
        let kernel = Kernel::boot(Config::default(), isolated_kcfg(), platform)
            .await
            .expect("kernel boots");
        let kernel = Arc::new(tokio::sync::RwLock::new(kernel));
        let (shutdown_tx, _rx) = watch::channel(false);

        let resp = dispatch(
            "ecc.vector-config".into(),
            serde_json::json!({}),
            kernel,
            shutdown_tx,
        )
        .await;
        assert!(resp.ok);
        let result = resp.result.expect("result");
        assert_eq!(result["active"], true);
        assert_eq!(result["backend"].as_str(), Some("hnsw"));
        // No explicit [kernel.vector] → configured_backend is the default.
        assert_eq!(result["configured_backend"].as_str(), Some("hnsw"));
    }

    /// A conversation with no nodes returns a well-formed empty graph, not an
    /// error — so the GUI can render its empty-state over a valid payload.
    #[cfg(feature = "ecc")]
    #[tokio::test]
    async fn conversation_graph_unknown_conv_is_empty() {
        use clawft_kernel::Kernel;
        use clawft_platform::NativePlatform;
        use clawft_types::config::Config;
        use std::sync::Arc;

        let platform = Arc::new(NativePlatform::new());
        let kernel = Kernel::boot(Config::default(), isolated_kcfg(), platform)
            .await
            .expect("kernel boots");
        let kernel = Arc::new(tokio::sync::RwLock::new(kernel));
        let (shutdown_tx, _rx) = watch::channel(false);
        let resp = dispatch(
            "conversation.graph".into(),
            serde_json::json!({ "conv_id": "nope" }),
            kernel,
            shutdown_tx,
        )
        .await;
        assert!(resp.ok);
        let result = resp.result.expect("result");
        assert_eq!(result["nodes"].as_array().unwrap().len(), 0);
        assert_eq!(result["edges"].as_array().unwrap().len(), 0);
    }

    /// A missing / empty `conv_id` is rejected — the projection must never run
    /// unscoped against the unbounded global graph.
    #[cfg(feature = "ecc")]
    #[tokio::test]
    async fn conversation_graph_requires_conv_id() {
        use clawft_kernel::Kernel;
        use clawft_platform::NativePlatform;
        use clawft_types::config::Config;
        use std::sync::Arc;

        let platform = Arc::new(NativePlatform::new());
        let kernel = Kernel::boot(Config::default(), isolated_kcfg(), platform)
            .await
            .expect("kernel boots");
        let kernel = Arc::new(tokio::sync::RwLock::new(kernel));
        let (shutdown_tx, _rx) = watch::channel(false);
        let resp = dispatch(
            "conversation.graph".into(),
            serde_json::json!({}),
            kernel,
            shutdown_tx,
        )
        .await;
        assert!(!resp.ok);
    }

    /// Wave 1 §W1.5 — `conversation.graph` serves the sibling `voice_analysis`
    /// record **verbatim**, alongside the emotion-flipped classification blob.
    ///
    /// Coder-B's `voice_analysis_wire.rs` pins that `index_turn` *stores* the
    /// record + flips the classification emotion axis; this pins the read side —
    /// a committed node whose metadata carries a `voice_analysis` sibling key and
    /// a `tier:"voice"` classification is projected by the RPC with the record
    /// intact byte-for-byte (the one added `meta.get("voice_analysis")` read at
    /// the node build), so the surface (`weft voice watch`) and the ADR-067 GUI
    /// graph both light up from the served payload. Device-free: a node with the
    /// frozen §W1.2 shape written straight into the causal graph, then read back
    /// through `dispatch`.
    #[cfg(feature = "ecc")]
    #[tokio::test]
    async fn conversation_graph_serves_voice_analysis_verbatim() {
        use clawft_kernel::crossref::{StructureTag, UniversalNodeId};
        use clawft_kernel::Kernel;
        use clawft_platform::NativePlatform;
        use clawft_types::config::Config;
        use std::sync::Arc;

        let platform = Arc::new(NativePlatform::new());
        let kernel = Kernel::boot(Config::default(), isolated_kcfg(), platform)
            .await
            .expect("kernel boots");
        let causal = kernel.ecc_causal().expect("causal graph present").clone();

        // The frozen §W1.2 record (acted / high-arousal), exactly as the sibling
        // metadata key holds it after `index_turn`.
        let va = serde_json::json!({
            "v": 1, "tier": "voice",
            "stt": {
                "model": "parakeet-tdt-0.6b", "path": "native", "latency_ms": 48,
                "tokens": [{ "text": "stop", "t_ms": 120, "dur_ms": 240, "conf": 0.97 }],
                "token_conf_mean": 0.97, "token_conf_min": 0.97
            },
            "endpoint": { "completion_prob": 0.9, "source": "smart-turn-v3",
                          "silence_tail_ms": 300, "latency_ms": 900 },
            "speaker": { "id": "spk_1", "name": "Mathew", "score": 0.71, "threshold": 0.45,
                         "action": "identified", "embedding_dim": 192 },
            "audio": { "duration_ms": 900, "voiced_ms": 700, "silence_ms": 200,
                       "rms_dbfs_mean": -18.0, "rms_dbfs_peak": -6.0,
                       "noise_floor_dbfs": -52.0, "snr_db": 34.0, "clip_pct": 0.0, "dc_offset": 2 },
            "prosody": { "f0_mean_hz": 210.0, "f0_min_hz": 140.0, "f0_max_hz": 330.0,
                         "f0_range_semitones": 14.8, "f0_slope": 1.2,
                         "rate_tokens_per_s": 5.1, "pause_count": 0, "pause_mean_ms": 0,
                         "energy_dynamics_db": 22.0, "confidence": "medium" },
            "emotion": { "valence": -0.45, "arousal": 0.92, "dominance": 0.30, "label": "agitated",
                         "arousal_conf": "medium", "valence_conf": "low", "source": "prosody-dsp" },
            "paralinguistics": { "non_lexical": false, "class": "speech", "confidence": "low" }
        });
        // The compact cross-modality blob after the voice emotion override.
        let classification = serde_json::json!({
            "intent": "command", "topic": "stop",
            "emotion": { "valence": -0.45, "arousal": 0.92, "dominance": 0.30, "label": "agitated" },
            "goal": serde_json::Value::Null, "tier": "voice", "v": 1,
        });
        let uid = UniversalNodeId::new(
            &StructureTag::CausalGraph,
            b"voice-graph",
            1,
            b"stop",
            b"turn",
        );
        causal.add_node(
            "turn:voice-graph:1".into(),
            serde_json::json!({
                "conv_id": "voice-graph", "chain_seq": 1, "role": "user",
                "state": "committed", "uid": uid.to_string(),
                "text": "stop", "classification": classification, "voice_analysis": va,
            }),
        );

        let kernel = Arc::new(tokio::sync::RwLock::new(kernel));
        let (shutdown_tx, _rx) = watch::channel(false);
        let resp = dispatch(
            "conversation.graph".into(),
            serde_json::json!({ "conv_id": "voice-graph" }),
            kernel,
            shutdown_tx,
        )
        .await;
        assert!(resp.ok, "handler ok: {:?}", resp.error);
        let result = resp.result.expect("result present");
        let node = result["nodes"]
            .as_array()
            .expect("nodes array")
            .iter()
            .find(|n| n["id"].as_str() == Some(uid.to_string().as_str()))
            .expect("voice turn node projected");

        // The sibling record is served verbatim — byte-for-byte, unreshaped.
        assert_eq!(node["voice_analysis"], va, "voice_analysis served verbatim");
        // Spot-check the fields the exit test keys on survive the projection.
        assert_eq!(node["voice_analysis"]["stt"]["tokens"][0]["conf"], serde_json::json!(0.97));
        assert_eq!(node["voice_analysis"]["audio"]["snr_db"], serde_json::json!(34.0));
        assert_eq!(node["voice_analysis"]["prosody"]["f0_mean_hz"], serde_json::json!(210.0));
        assert_eq!(node["voice_analysis"]["emotion"]["arousal"], serde_json::json!(0.92));
        assert_eq!(node["voice_analysis"]["tier"], serde_json::json!("voice"));
        // And the compact classification carries the voice-flipped emotion axis.
        assert_eq!(node["classification"]["tier"], serde_json::json!("voice"));
        assert_eq!(node["classification"]["emotion"]["arousal"], serde_json::json!(0.92));
        assert_eq!(node["text"], serde_json::json!("stop"), "turn text surfaced");
    }

    /// Wave 2 §W2.6 — `conversation.graph` projects the reply-attempt marker
    /// + goal as additive flat keys.
    ///
    /// `register_reply_frontier` (session_tier.rs) stashes
    /// `metadata.kind = "reply-attempt"` and `metadata.goal` on the attempt
    /// node; neither is in `PROTECTED_METADATA_KEYS`, so both survive a
    /// `set_node_state` transition untouched. This pins the read side: a node
    /// with that metadata shape projects `"attempt": true` and
    /// `"goal": <text>` regardless of its `state`, while the pre-existing
    /// `"kind"` key keeps mirroring `state` (viewer compatibility). A plain
    /// committed turn (no `metadata.kind`) projects `"attempt": false` and an
    /// empty `"goal"`.
    #[cfg(feature = "ecc")]
    #[tokio::test]
    async fn conversation_graph_projects_reply_attempt_marker_and_goal() {
        use clawft_kernel::crossref::{StructureTag, UniversalNodeId};
        use clawft_kernel::Kernel;
        use clawft_platform::NativePlatform;
        use clawft_types::config::Config;
        use std::sync::Arc;

        let platform = Arc::new(NativePlatform::new());
        let kernel = Kernel::boot(Config::default(), isolated_kcfg(), platform)
            .await
            .expect("kernel boots");
        let causal = kernel.ecc_causal().expect("causal graph present").clone();

        let attempt_uid = UniversalNodeId::new(
            &StructureTag::CausalGraph,
            b"attempt-graph",
            1,
            b"goal text",
            b"turn",
        );
        causal.add_node(
            "turn:attempt-graph:1".into(),
            serde_json::json!({
                "conv_id": "attempt-graph", "chain_seq": 1, "role": "assistant",
                "state": "frontier", "uid": attempt_uid.to_string(),
                "text": "what's the weather", "kind": "reply-attempt",
                "goal": "what's the weather",
            }),
        );
        let plain_uid = UniversalNodeId::new(
            &StructureTag::CausalGraph,
            b"attempt-graph",
            2,
            b"hey",
            b"turn",
        );
        causal.add_node(
            "turn:attempt-graph:2".into(),
            serde_json::json!({
                "conv_id": "attempt-graph", "chain_seq": 2, "role": "user",
                "state": "committed", "uid": plain_uid.to_string(),
                "text": "hey",
            }),
        );

        let kernel = Arc::new(tokio::sync::RwLock::new(kernel));
        let (shutdown_tx, _rx) = watch::channel(false);
        let resp = dispatch(
            "conversation.graph".into(),
            serde_json::json!({ "conv_id": "attempt-graph" }),
            kernel,
            shutdown_tx,
        )
        .await;
        assert!(resp.ok, "handler ok: {:?}", resp.error);
        let result = resp.result.expect("result present");
        let nodes = result["nodes"].as_array().expect("nodes array");

        let attempt = nodes
            .iter()
            .find(|n| n["id"].as_str() == Some(attempt_uid.to_string().as_str()))
            .expect("attempt node projected");
        assert_eq!(attempt["attempt"], serde_json::json!(true), "attempt marker projected");
        assert_eq!(attempt["goal"], serde_json::json!("what's the weather"));
        assert_eq!(attempt["state"], serde_json::json!("frontier"));
        assert_eq!(attempt["kind"], serde_json::json!("frontier"), "kind still mirrors state");

        let plain = nodes
            .iter()
            .find(|n| n["id"].as_str() == Some(plain_uid.to_string().as_str()))
            .expect("plain turn node projected");
        assert_eq!(plain["attempt"], serde_json::json!(false), "no attempt marker on a plain turn");
        assert_eq!(plain["goal"], serde_json::json!(""), "no goal on a plain turn");
    }

    // ── WEFT-654: agent.proposal RPC wire shape ─────────────────────────
    //
    // Standing up a real `AgentLoop` with a review-mode cow attachment is
    // impractical here (it needs a live `Platform`/`ToolRegistry`/LLM
    // transport, and the review-gate mocks in
    // `clawft-core::agent::loop_core::tests` are private to that crate).
    // What's covered instead: the pure wire-shape helpers
    // (`proposal_list_result`/`proposal_discard_reason`), the capability
    // classification, and — via a real UDS round trip, mirroring
    // `tests/agent_chat_dispatch.rs` — the "not wired" error path for all
    // three arms. The hold/accept/discard state machine itself
    // (checkpoint → hold → promote | rollback + witness) is covered by
    // `clawft-core`'s `turn_checkpoint` tests.

    #[test]
    fn proposal_list_result_empty_when_no_hold() {
        let result = proposal_list_result(None);
        assert!(result.pending.is_empty());
    }

    #[test]
    fn proposal_list_result_shapes_the_held_proposal() {
        let result = proposal_list_result(Some((
            "turn:agent.chat:conv-1".to_string(),
            std::time::Duration::from_secs(42),
        )));
        assert_eq!(result.pending.len(), 1);
        assert_eq!(result.pending[0].label, "turn:agent.chat:conv-1");
        assert_eq!(result.pending[0].age_secs, 42);
    }

    #[test]
    fn proposal_discard_reason_defaults_when_omitted() {
        let params = AgentProposalDiscardParams { reason: None };
        assert_eq!(proposal_discard_reason(&params), "operator discard");
    }

    #[test]
    fn proposal_discard_reason_honors_caller_value() {
        let params = AgentProposalDiscardParams {
            reason: Some("bad output".to_string()),
        };
        assert_eq!(proposal_discard_reason(&params), "bad output");
    }

    #[test]
    fn proposal_discard_params_deserialize_without_reason() {
        let params: AgentProposalDiscardParams = serde_json::from_value(serde_json::json!({}))
            .expect("empty object deserializes (reason optional)");
        assert_eq!(params.reason, None);
    }

    #[test]
    fn proposal_capabilities_classified() {
        assert_eq!(
            crate::capability::required_capability("agent.proposal.list"),
            crate::capability::Capability::Read
        );
        assert_eq!(
            crate::capability::required_capability("agent.proposal.accept"),
            crate::capability::Capability::Write
        );
        assert_eq!(
            crate::capability::required_capability("agent.proposal.discard"),
            crate::capability::Capability::Write
        );
    }

    // ── WEFT-655: voice/review parity + standalone timeout sweep ────────

    #[test]
    fn turn_hold_label_matches_core_pin() {
        // Pinned literal — `AgentLoop::handle_turn`'s
        // `format!("turn:{}:{}", msg.channel, msg.chat_id)` with
        // `channel = "agent.chat"` (AGENT_CHAT_CHANNEL). A rename on either
        // side must break this test, not silently miss the Gap A park.
        assert_eq!(turn_hold_label("conv-1"), "turn:agent.chat:conv-1");
        assert_eq!(turn_hold_label(""), "turn:agent.chat:");
    }

    #[test]
    fn park_and_take_voice_attempt_round_trips() {
        let label = "turn:agent.chat:conv-park";
        assert!(
            take_parked_voice_attempt(label).is_none(),
            "nothing parked yet"
        );
        park_voice_attempt(label, "conv-park", 7);
        assert_eq!(
            take_parked_voice_attempt(label),
            Some(("conv-park".to_string(), 7))
        );
        // Take removes — a second take finds nothing.
        assert!(take_parked_voice_attempt(label).is_none());
    }

    #[test]
    fn park_voice_attempt_overwrites_stale_entry_for_same_label() {
        let label = "turn:agent.chat:conv-overwrite";
        park_voice_attempt(label, "conv-overwrite", 1);
        park_voice_attempt(label, "conv-overwrite", 2);
        assert_eq!(
            take_parked_voice_attempt(label),
            Some(("conv-overwrite".to_string(), 2)),
            "last park wins"
        );
    }

    #[test]
    fn resolve_parked_attempt_on_accept_clears_the_park() {
        // No `daemon_agent()`/`DAEMON_VOICE_LOOP` wired in this test binary
        // (no daemon boot happened) — the tier/voice-loop side-effects
        // (forest commit, clear_parked, WEFT-673 drain) degrade to no-ops,
        // but the park itself must still clear.
        let label = "turn:agent.chat:conv-accept";
        park_voice_attempt(label, "conv-accept", 3);
        resolve_parked_attempt_on_accept(label);
        assert!(
            take_parked_voice_attempt(label).is_none(),
            "accept resolves (removes) the park"
        );
    }

    #[test]
    fn cleanup_parked_attempt_on_discard_clears_the_park() {
        let label = "turn:agent.chat:conv-discard";
        park_voice_attempt(label, "conv-discard", 4);
        cleanup_parked_attempt_on_discard(label);
        assert!(
            take_parked_voice_attempt(label).is_none(),
            "discard resolves (removes) the park"
        );
    }

    #[test]
    fn resolve_and_cleanup_are_noop_when_nothing_parked() {
        // Must not panic when there's nothing under the label — including
        // the WEFT-673 drain call (no voice loop wired ⇒ no drain).
        resolve_parked_attempt_on_accept("turn:agent.chat:never-parked");
        cleanup_parked_attempt_on_discard("turn:agent.chat:never-parked");
    }

    /// WEFT-673: restate the two residual edges WEFT-655 self-documented,
    /// in code terms. Pins the call sites a future reader must keep
    /// aligned; the hold-drain is closed (drain_after_hold_resolve), the
    /// forest-path shape remains deliberately voice-specific (see
    /// `voice_loop` module doc).
    #[test]
    fn weft_673_residual_gaps_restated_in_code_terms() {
        // Gap 1 — voice forest-commit path (deliberate asymmetry):
        //   park:     voice_loop::dispatch_reply Ok arm → park_voice_attempt
        //   accept:   resolve_parked_attempt_on_accept → commit_reply_frontier
        //   discard:  cleanup_parked_attempt_on_discard → emit_cancel_prune
        //             + witness_cancel (same pair as Stop interrupt)
        //   text chat has no reply Frontier; voice needs one for barge-in.
        // Gap 2 — hold-drain seam (CLOSED by WEFT-673):
        //   accept/discard → clear_parked_in_flight THEN
        //                    VoiceLoop::drain_after_hold_resolve
        //   (was: queue waited for next ordinary turn; RPC arm had no seam)
        let accept_label = turn_hold_label("weft-673-accept");
        let discard_label = turn_hold_label("weft-673-discard");
        assert_eq!(accept_label, "turn:agent.chat:weft-673-accept");
        park_voice_attempt(&accept_label, "weft-673-accept", 1);
        park_voice_attempt(&discard_label, "weft-673-discard", 2);
        resolve_parked_attempt_on_accept(&accept_label);
        cleanup_parked_attempt_on_discard(&discard_label);
        assert!(take_parked_voice_attempt(&accept_label).is_none());
        assert!(take_parked_voice_attempt(&discard_label).is_none());
    }

    #[test]
    fn proposal_past_timeout_boundary() {
        let timeout = std::time::Duration::from_secs(600);
        assert!(!proposal_past_timeout(
            std::time::Duration::from_secs(599),
            timeout
        ));
        // Exactly at the deadline does NOT time out (`>`, not `>=`) —
        // matches the original inline M2 reaper check.
        assert!(!proposal_past_timeout(
            std::time::Duration::from_secs(600),
            timeout
        ));
        assert!(proposal_past_timeout(
            std::time::Duration::from_secs(601),
            timeout
        ));
    }

    #[test]
    fn run_proposal_timeout_sweep_tick_noop_without_loop_or_timeout() {
        // No boot happened in this test binary path for this test (module
        // statics are process-global, but this test doesn't depend on any
        // prior test having booted one) — the important property is just
        // that a missing loop/timeout never panics.
        run_proposal_timeout_sweep_tick();
    }

    #[tokio::test]
    async fn proposal_rpcs_error_cleanly_when_agent_loop_not_wired() {
        // Mirrors `tests/agent_chat_dispatch.rs`'s "not wired" check: no
        // boot happened in this test, so `DAEMON_AGENT_LOOP` is unset —
        // every `agent.proposal.*` arm must surface a typed error rather
        // than panic.
        let platform = Arc::new(NativePlatform::new());
        let kernel = Kernel::boot(Config::default(), isolated_kcfg(), platform)
            .await
            .expect("kernel boots");
        let kernel = Arc::new(tokio::sync::RwLock::new(kernel));
        let (shutdown_tx, _rx) = watch::channel(false);

        for (method, params) in [
            ("agent.proposal.list", serde_json::json!({})),
            ("agent.proposal.accept", serde_json::json!({})),
            ("agent.proposal.discard", serde_json::json!({"reason": "x"})),
        ] {
            let resp = dispatch(method.into(), params, kernel.clone(), shutdown_tx.clone()).await;
            assert!(
                !resp.ok,
                "{method} should error when agent loop isn't wired"
            );
            let err = resp.error.expect("error message present");
            assert!(
                err.contains("not wired"),
                "{method}: unexpected error message: {err}"
            );
        }
    }

    /// WEFT-324: public `chain.append` accepts a WitnessRecord, lands it
    /// on the live ChainManager, and returns sequence + hash.
    #[cfg(feature = "exochain")]
    #[tokio::test]
    async fn chain_append_rpc_witnesses_soul_promote() {
        use clawft_service_agent::protocol::{ChainAppendParams, ChainAppendResult, WitnessRecord};
        use clawft_types::config::ChainConfig;

        let platform = Arc::new(NativePlatform::new());
        // Force chain on (default ChainConfig is enabled, but pin it
        // so the test doesn't depend on KernelConfig::default drift).
        let kcfg = KernelConfig {
            chain: Some(ChainConfig {
                enabled: true,
                checkpoint_interval: 1000,
                chain_id: 0,
                checkpoint_path: ChainConfig::isolated_in(&tempfile::tempdir().unwrap().keep())
                    .checkpoint_path,
                external_anchor: None,
            }),
            ..KernelConfig::default()
        };
        let kernel = Kernel::boot(Config::default(), kcfg, platform)
            .await
            .expect("kernel boots");
        let kernel = Arc::new(tokio::sync::RwLock::new(kernel));
        let (shutdown_tx, _rx) = watch::channel(false);

        let before = {
            let k = kernel.read().await;
            k.chain_manager()
                .expect("chain_manager present")
                .len()
        };

        let params = ChainAppendParams {
            source: "soul".into(),
            record: WitnessRecord {
                kind: "soul.promote".into(),
                entries: vec!["01HZXTEST".into()],
                hash_before: "aa".repeat(32),
                hash_after: "bb".repeat(32),
                ts: "2026-07-30T12:00:00Z".into(),
            },
        };
        let resp = dispatch(
            "chain.append".into(),
            serde_json::to_value(&params).unwrap(),
            kernel.clone(),
            shutdown_tx.clone(),
        )
        .await;
        assert!(resp.ok, "chain.append should succeed: {:?}", resp.error);
        let result: ChainAppendResult =
            serde_json::from_value(resp.result.expect("result present")).expect("typed result");
        assert_eq!(result.source, "soul");
        assert_eq!(result.kind, "soul.promote");
        assert_eq!(result.hash.len(), 64, "hash is 32-byte hex");
        assert!(result.sequence >= 1, "non-genesis sequence");

        let after = {
            let k = kernel.read().await;
            let cm = k.chain_manager().expect("chain_manager present");
            let events = cm.tail(0);
            let hit = events
                .iter()
                .find(|e| e.kind == "soul.promote" && e.source == "soul")
                .expect("soul.promote event on chain");
            assert_eq!(
                hit.payload
                    .as_ref()
                    .and_then(|p| p.get("entries"))
                    .and_then(|v| v.as_array())
                    .map(|a| a.len()),
                Some(1),
                "payload carries WitnessRecord.entries"
            );
            cm.len()
        };
        assert_eq!(after, before + 1, "exactly one event appended");
    }

    /// WEFT-150: `ipc.publish` to a leaf-push topic lands on ExoChain
    /// as both `ipc.send` (send_checked) and structured `ipc.publish`
    /// with `leaf_push: true`.
    #[cfg(feature = "exochain")]
    #[tokio::test]
    async fn ipc_publish_leaf_push_lands_on_chain() {
        use clawft_types::config::ChainConfig;

        let platform = Arc::new(NativePlatform::new());
        let kcfg = KernelConfig {
            chain: Some(ChainConfig {
                enabled: true,
                checkpoint_interval: 1000,
                chain_id: 0,
                checkpoint_path: ChainConfig::isolated_in(&tempfile::tempdir().unwrap().keep())
                    .checkpoint_path,
                external_anchor: None,
            }),
            ..KernelConfig::default()
        };
        let kernel = Kernel::boot(Config::default(), kcfg, platform)
            .await
            .expect("kernel boots");
        let kernel = Arc::new(tokio::sync::RwLock::new(kernel));
        let (shutdown_tx, _rx) = watch::channel(false);

        let before = {
            let k = kernel.read().await;
            k.chain_manager()
                .expect("chain_manager present")
                .len()
        };

        let topic = weftos_leaf_types::push_topic("deadbeefcafe");
        let wire = serde_json::json!({
            "type": "leaf_push",
            "cbor_b64": "oQ==",
            "target_pubkey": "deadbeefcafe",
        })
        .to_string();

        let resp = dispatch(
            "ipc.publish".into(),
            serde_json::json!({
                "topic": topic,
                "message": wire,
            }),
            kernel.clone(),
            shutdown_tx.clone(),
        )
        .await;
        assert!(
            resp.ok,
            "leaf ipc.publish should succeed: {:?}",
            resp.error
        );
        let result = resp.result.expect("result present");
        assert_eq!(result["topic"], topic);

        let k = kernel.read().await;
        let cm = k.chain_manager().expect("chain_manager present");
        let events = cm.tail(0);

        let send_hit = events
            .iter()
            .find(|e| e.kind == "ipc.send" && e.source == "ipc")
            .expect("ipc.send event from send_checked");
        let send_payload = send_hit.payload.as_ref().expect("ipc.send payload");
        let target = send_payload["target"].as_str().unwrap_or("");
        assert!(
            target.contains("mesh.leaf.deadbeefcafe.push") || target.contains("Topic"),
            "ipc.send target should reference leaf topic, got {target}"
        );

        let pub_hit = events
            .iter()
            // Match on our own topic: a kernel booted from the default config can
            // share the chain with other publishers (e.g. host services), so the
            // first ipc.publish event is not necessarily this test's.
            .find(|e| {
                e.kind == "ipc.publish"
                    && e.source == "ipc"
                    && e.payload.as_ref().is_some_and(|p| p["topic"] == topic)
            })
            .expect("ipc.publish chain event");
        let pub_payload = pub_hit.payload.as_ref().expect("ipc.publish payload");
        assert_eq!(pub_payload["topic"], topic);
        assert_eq!(pub_payload["leaf_push"], true);
        assert_eq!(pub_payload["wire_type"], "leaf_push");
        assert_eq!(pub_payload["subscribers"], 0);

        assert!(
            cm.len() >= before + 2,
            "expected at least ipc.send + ipc.publish (+ any boot noise); before={before} after={}",
            cm.len()
        );
    }

    /// WEFT-150: non-leaf topic publish still witnesses ipc.publish with
    /// `leaf_push: false`.
    #[cfg(feature = "exochain")]
    #[tokio::test]
    async fn ipc_publish_non_leaf_not_tagged_leaf_push() {
        use clawft_types::config::ChainConfig;

        let platform = Arc::new(NativePlatform::new());
        let kcfg = KernelConfig {
            chain: Some(ChainConfig {
                enabled: true,
                checkpoint_interval: 1000,
                chain_id: 0,
                checkpoint_path: ChainConfig::isolated_in(&tempfile::tempdir().unwrap().keep())
                    .checkpoint_path,
                external_anchor: None,
            }),
            ..KernelConfig::default()
        };
        let kernel = Kernel::boot(Config::default(), kcfg, platform)
            .await
            .expect("kernel boots");
        let kernel = Arc::new(tokio::sync::RwLock::new(kernel));
        let (shutdown_tx, _rx) = watch::channel(false);

        let resp = dispatch(
            "ipc.publish".into(),
            serde_json::json!({
                "topic": "hello",
                "message": "world",
            }),
            kernel.clone(),
            shutdown_tx,
        )
        .await;
        assert!(resp.ok, "publish should succeed: {:?}", resp.error);

        let k = kernel.read().await;
        let cm = k.chain_manager().expect("chain_manager present");
        let hit = cm
            .tail(0)
            .into_iter()
            // Match on our own topic; see ipc_publish_leaf_push_lands_on_chain.
            .find(|e| {
                e.kind == "ipc.publish"
                    && e.payload.as_ref().is_some_and(|p| p["topic"] == "hello")
            })
            .expect("ipc.publish event");
        let p = hit.payload.as_ref().unwrap();
        assert_eq!(p["topic"], "hello");
        assert_eq!(p["leaf_push"], false);
    }

    #[cfg(feature = "exochain")]
    #[tokio::test]
    async fn chain_append_rpc_rejects_empty_kind() {
        use clawft_types::config::ChainConfig;

        let platform = Arc::new(NativePlatform::new());
        let kcfg = KernelConfig {
            chain: Some(ChainConfig {
                enabled: true,
                checkpoint_interval: 1000,
                chain_id: 0,
                checkpoint_path: ChainConfig::isolated_in(&tempfile::tempdir().unwrap().keep())
                    .checkpoint_path,
                external_anchor: None,
            }),
            ..KernelConfig::default()
        };
        let kernel = Kernel::boot(Config::default(), kcfg, platform)
            .await
            .expect("kernel boots");
        let kernel = Arc::new(tokio::sync::RwLock::new(kernel));
        let (shutdown_tx, _rx) = watch::channel(false);

        let resp = dispatch(
            "chain.append".into(),
            serde_json::json!({
                "source": "soul",
                "record": {
                    "kind": "  ",
                    "entries": [],
                    "hash_before": "0".repeat(64),
                    "hash_after": "1".repeat(64),
                    "ts": "t"
                }
            }),
            kernel,
            shutdown_tx,
        )
        .await;
        assert!(!resp.ok, "empty kind must be rejected");
        let err = resp.error.unwrap_or_default();
        assert!(
            err.contains("kind"),
            "error should mention kind, got: {err}"
        );
    }
}

#[cfg(test)]
#[path = "daemon_peer_tests.rs"]
mod peer_tests;
