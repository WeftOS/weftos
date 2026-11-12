//! Per-turn COW memory checkpoint/promote/rollback bracket (WEFT-616 Phase 2).
//!
//! [`with_turn_checkpoint`] wraps a single turn's execution with a
//! `clawft-cow-memory::BranchableMemory` checkpoint: freeze `working` into a
//! checkpoint before the turn runs, `promote()` it into `base` on success,
//! `rollback()` to the pre-turn checkpoint on failure. See
//! `.planning/ruv/integration/agenticow-integration-plan.md` §2/§4 and
//! `.planning/fork-adoption-build-plan-2026-07.md` §1 Phase 2.
//!
//! # What this brackets today
//!
//! [`AgentLoop::handle_turn`](crate::agent::loop_core::AgentLoop::handle_turn)
//! does not currently write vector memory mid-turn -- `memory_bootstrap` is a
//! one-time indexing step run outside the turn path (index `MEMORY.md` once
//! at startup), and [`MemoryStore`](crate::agent::memory::MemoryStore) is a
//! markdown-file store unrelated to `RvfStore`/`BranchableMemory`. So today
//! this bracket's `checkpoint`/`promote`/`rollback` calls replay empty
//! edit-logs -- the lifecycle is exercised and correct, but there is nothing
//! yet for a turn to actually ingest into the checkpointed memory. Routing a
//! turn's writes into `working` instead of directly into the brain store is
//! Phase 3/tools' job -- see `clawft-cow-memory`'s crate-level "Not yet in
//! scope" list.
//!
//! # Failure semantics
//!
//! - If `checkpoint()` itself fails, the turn is not attempted at all (fail
//!   closed -- isolation cannot be promised without it) and the checkpoint
//!   error is returned as the turn's result.
//! - If the turn errors, `rollback()` is attempted. A rollback failure is
//!   logged loudly but does **not** replace the turn's own error -- that is
//!   what the caller actually needs to see.
//! - If the turn succeeds, `promote()` is attempted. A promote failure is
//!   logged loudly but does **not** discard the turn's successful result --
//!   the user-visible reply is not memory-infrastructure's to withhold. The
//!   unpromoted checkpoint chain is left in place; the next turn's own
//!   `checkpoint()` call builds on top of it rather than losing it.

use std::future::Future;
use std::sync::{Arc, Mutex};

use clawft_cow_memory::BranchableMemory;
use clawft_types::error::ClawftError;
use tracing::error;

use super::turn_ledger::TurnLedger;

/// A review-mode turn whose memory promote (and any forest commit the
/// caller owns) is HELD pending `accept`/`discard` (WEFT-654, design D9/D10).
/// Opaque outside this module except for the label and age — the rollback
/// target stays private so callers can't half-apply a decision.
#[derive(Debug)]
pub struct HeldTurn {
    pub label: String,
    pre_turn_head: Option<clawft_cow_memory::CheckpointId>,
    pub created_at: std::time::Instant,
}

/// RAII alignment guard (WEFT-655): `AgentService::cancel` aborts a
/// dispatch by DROPPING the `handle_turn` future at its `select!` — the
/// bracket's Ok/Err arms never run. Without this guard the cancelled turn's
/// checkpoint chain (and any mid-turn writes: tool-cadence checkpoints,
/// tool-side ingests) would dangle, and the NEXT turn's promote would
/// replay the cancelled partials into base — a silent leak. The guard rolls
/// back to the pre-turn head and witnesses the compensating revert on drop,
/// unless the bracket resolved normally and defused it — making the
/// interrupt path and the proposal-discard path converge on the SAME
/// closure: rollback + witnessed revert.
struct BracketGuard {
    mem: Arc<Mutex<BranchableMemory>>,
    ledger: Option<Arc<dyn TurnLedger>>,
    label: String,
    pre_turn_head: Option<clawft_cow_memory::CheckpointId>,
    armed: bool,
}

impl Drop for BracketGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let rolled_back = {
            let mut guard = self.mem.lock().unwrap_or_else(|p| p.into_inner());
            match guard.rollback(self.pre_turn_head) {
                Ok(()) => true,
                Err(e) => {
                    error!(
                        error = %e,
                        label = %self.label,
                        "cancelled-turn rollback failed; checkpoint chain may dangle"
                    );
                    false
                }
            }
        };
        if let Some(ledger) = &self.ledger {
            ledger.on_revert(
                &self.label,
                "turn cancelled (dispatch future dropped)",
                rolled_back,
            );
        }
    }
}

/// Accept a held proposal: promote the checkpoint chain into base and
/// witness the lineage — the same closure an auto-mode turn gets at
/// finalize, just human-gated (accept = promote).
pub fn accept_held(
    mem: &Arc<Mutex<BranchableMemory>>,
    ledger: Option<&Arc<dyn TurnLedger>>,
    held: HeldTurn,
) -> clawft_types::Result<()> {
    let mut guard = mem.lock().unwrap_or_else(|p| p.into_inner());
    let report = guard.promote().map_err(cow_err)?;
    if let Some(ledger) = ledger {
        ledger.on_promote(
            &held.label,
            true,
            Some(super::turn_ledger::PromotedLineage {
                child_id: report.child_id,
                parent_id: report.parent_id,
                parent_hash: report.parent_hash,
                mutation_count: (report.ingested + report.deleted) as u32,
            }),
        );
    }
    Ok(())
}

/// Discard a held proposal: roll the lineage back to the pre-turn head and
/// witness the compensating revert (discard = rollback; the fact of the
/// discard is on chain forever — discard ≠ delete).
pub fn discard_held(
    mem: &Arc<Mutex<BranchableMemory>>,
    ledger: Option<&Arc<dyn TurnLedger>>,
    held: HeldTurn,
    reason: &str,
) -> clawft_types::Result<()> {
    let rolled_back = {
        let mut guard = mem.lock().unwrap_or_else(|p| p.into_inner());
        match guard.rollback(held.pre_turn_head) {
            Ok(()) => true,
            Err(e) => {
                error!(error = %e, label = %held.label, "discard rollback failed");
                false
            }
        }
    };
    if let Some(ledger) = ledger {
        ledger.on_revert(&held.label, reason, rolled_back);
    }
    if rolled_back {
        Ok(())
    } else {
        Err(ClawftError::CowMemory {
            reason: "discard rollback failed; lineage may hold the undecided chain".into(),
        })
    }
}

/// Run `turn` bracketed by a checkpoint/promote/rollback cycle on `mem`.
///
/// `turn` is called at most once, after the checkpoint succeeds. `label`
/// names the checkpoint (visible via [`BranchableMemory::lineage`] for
/// debugging/audit) -- callers should pass something that identifies the
/// turn (e.g. `"turn:{channel}:{chat_id}"`).
/// `items_on_ok` produces the turn's memory writes from its successful
/// result — called between turn-Ok and `promote()` so the writes land in
/// the checkpointed `working` node and are therefore covered by the same
/// promote/rollback the turn is. It is async because embedding is. An empty
/// vec skips the ingest. A failure to produce the items (no embedder, an
/// embedder whose dimension differs from the lineage's, an embed error) or to
/// ingest them logs loudly but never fails the turn (the reply is not memory
/// infrastructure's to withhold) — the promote proceeds without the items.
/// `review`: hold instead of promote on Ok (WEFT-654). The held turn is
/// written into `held_out` for the caller to register; everything else
/// (ingest, ledger checkpoint witness, Err rollback) behaves identically.
pub(crate) async fn with_turn_checkpoint<F, Fut, T, I, IFut>(
    mem: &Arc<Mutex<BranchableMemory>>,
    ledger: Option<&Arc<dyn TurnLedger>>,
    label: impl Into<String>,
    turn: F,
    items_on_ok: I,
    review: bool,
    held_out: &mut Option<HeldTurn>,
) -> clawft_types::Result<T>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = clawft_types::Result<T>>,
    I: FnOnce(&T) -> IFut,
    IFut: Future<Output = clawft_types::Result<Vec<clawft_cow_memory::VectorItem>>>,
{
    let label: String = label.into();
    // `rollback(Some(id))` discards everything *newer* than `id` but keeps
    // `id` itself (it is a "roll back to this point", not "roll back past
    // it") -- see `ops.rs::rollback`'s doc comment. So targeting the
    // checkpoint we are about to create would keep it, which is exactly
    // the turn's own (to-be-discarded) work. What we actually want on
    // failure is "undo this turn's checkpoint too" -- i.e. roll back to
    // whatever checkpoint was the head *before* this call, which is `None`
    // (collapse to `base`) in the steady state, since `promote`/`rollback`
    // always collapse the chain back to depth 0 after each prior turn.
    let pre_turn_head = {
        let guard = mem.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        guard
            .lineage()
            .into_iter()
            .find(|entry| matches!(entry.role, clawft_cow_memory::NodeRole::Checkpoint))
            .map(|entry| entry.id)
    };

    {
        let mut guard = mem.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        // WEFT-652 AutoResume: a parked lineage (idle-reaper AutoPause, or a
        // reopened manifest that was paused) rehydrates on demand — an O(1)
        // derive of a fresh working child. The turn is the "impulse" the
        // plan describes; the subsequent checkpoint witness records the
        // wake-up on chain implicitly.
        if guard.is_paused() {
            tracing::info!(label, "cow memory: auto-resuming parked lineage for incoming turn");
            guard.resume().map_err(cow_err)?;
        }
        guard.checkpoint(label.clone()).map_err(cow_err)?;
    }
    // Witness AFTER the memory checkpoint succeeded — the chain must never
    // claim a checkpoint that doesn't exist (WEFT-616 Phase 3, §7).
    if let Some(ledger) = ledger {
        ledger.on_checkpoint(&label);
    }

    // Armed until the bracket resolves: a cancel that drops this future
    // mid-turn triggers the guard's rollback + revert witness (WEFT-655).
    let mut cancel_guard = BracketGuard {
        mem: Arc::clone(mem),
        ledger: ledger.cloned(),
        label: label.clone(),
        pre_turn_head,
        armed: true,
    };

    let result = turn().await;
    cancel_guard.armed = false;

    match &result {
        Ok(val) => {
            // Phase 3 write routing: the turn's memory writes land in the
            // checkpointed `working` node BEFORE promote, so promote carries
            // them and a (hypothetical) failure after this point would still
            // discard them with the branch — the bracket protects real data.
            let items = match items_on_ok(val).await {
                Ok(items) => items,
                Err(e) => {
                    error!(
                        error = %e,
                        label,
                        "turn-exchange memory write refused; the turn proceeds without the exchange"
                    );
                    Vec::new()
                }
            };
            if review {
                // WEFT-654 review mode: ingest, then HOLD — no promote, no
                // promote witness. The decision (accept = promote / discard
                // = rollback) belongs to the reviewer; timeout discards.
                let mut guard = mem.lock().unwrap_or_else(|p| p.into_inner());
                if !items.is_empty()
                    && let Err(e) = guard.ingest(&items)
                {
                    error!(error = %e, label, "review-mode ingest failed; holding without the exchange");
                }
                *held_out = Some(HeldTurn {
                    label,
                    pre_turn_head,
                    created_at: std::time::Instant::now(),
                });
                return result;
            }
            let (promoted, lineage) = {
                let mut guard = mem.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                if !items.is_empty()
                    && let Err(e) = guard.ingest(&items)
                {
                    error!(
                        error = %e,
                        label,
                        "clawft-cow-memory: turn-exchange ingest failed; \
                         promoting without the exchange"
                    );
                }
                match guard.promote() {
                    Ok(report) => (
                        true,
                        Some(super::turn_ledger::PromotedLineage {
                            child_id: report.child_id,
                            parent_id: report.parent_id,
                            parent_hash: report.parent_hash,
                            mutation_count: (report.ingested + report.deleted) as u32,
                        }),
                    ),
                    Err(e) => {
                        error!(
                            error = %e,
                            "clawft-cow-memory: promote failed after successful turn; \
                             checkpoint chain left in place for the next turn to build on"
                        );
                        (false, None)
                    }
                }
            };
            if let Some(ledger) = ledger {
                ledger.on_promote(&label, promoted, lineage);
            }
        }
        Err(turn_err) => {
            let rolled_back = {
                let mut guard = mem.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                match guard.rollback(pre_turn_head) {
                    Ok(()) => true,
                    Err(e) => {
                        error!(
                            error = %e,
                            "clawft-cow-memory: rollback failed after turn error; \
                             original turn error still returned to the caller"
                        );
                        false
                    }
                }
            };
            if let Some(ledger) = ledger {
                // Append-only compensation: memory was discarded; the FACT of
                // the revert is witnessed forever (never truncate history).
                ledger.on_revert(&label, &turn_err.to_string(), rolled_back);
            }
        }
    }

    result
}

/// Build the memory writes for one successful exchange (Phase 3 write
/// routing): two entries — the user utterance and the assistant reply —
/// each embedded by the configured real `embedder` (document lane), each
/// carrying its text payload and {channel, chat_id, role} tags. Two entries
/// rather than one concatenated document so a recall hit identifies WHO said
/// the matched text — mirroring how the kernel tier indexes user/assistant
/// turns separately.
///
/// There is no hash fallback. The lineage has ONE vector dimension, fixed
/// when it was created (`lineage_dim`); an embedder of a different dimension
/// is refused with an explicit [`CowMemoryError::DimensionMismatch`](clawft_cow_memory::CowMemoryError)
/// before anything is embedded or written. Vectors are never truncated or
/// padded, and a vector whose length disagrees with the lineage is refused
/// too.
pub(crate) async fn exchange_items(
    embedder: &dyn crate::embeddings::Embedder,
    lineage_dim: u16,
    channel: &str,
    chat_id: &str,
    user_text: &str,
    reply_text: &str,
) -> clawft_types::Result<Vec<clawft_cow_memory::VectorItem>> {
    use clawft_cow_memory::VectorItem;
    let want = lineage_dim as usize;
    if embedder.dimension() != want {
        return Err(cow_err(clawft_cow_memory::CowMemoryError::DimensionMismatch {
            expected: want,
            got: embedder.dimension(),
        }));
    }
    let mut tags = clawft_cow_memory::VectorTags::new();
    tags.insert("channel".into(), channel.to_string());
    tags.insert("chat_id".into(), chat_id.to_string());
    let mut items = Vec::with_capacity(2);
    for (role, text) in [("user", user_text), ("assistant", reply_text)] {
        if text.trim().is_empty() {
            continue;
        }
        let vector = embedder.embed(text).await.map_err(|e| ClawftError::CowMemory {
            reason: format!("embedding the {role} text failed: {e}"),
        })?;
        if vector.len() != want {
            return Err(cow_err(clawft_cow_memory::CowMemoryError::DimensionMismatch {
                expected: want,
                got: vector.len(),
            }));
        }
        let mut t = tags.clone();
        t.insert("role".into(), role.to_string());
        items.push(VectorItem::new(vector).with_text(text).with_tags(t));
    }
    Ok(items)
}

fn cow_err(e: clawft_cow_memory::CowMemoryError) -> ClawftError {
    ClawftError::CowMemory {
        reason: e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Recording ledger: captures the bracket's witness sequence.
    #[derive(Default)]
    struct RecordingLedger {
        log: Mutex<Vec<String>>,
    }
    impl RecordingLedger {
        fn events(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }
    }
    impl TurnLedger for RecordingLedger {
        fn on_checkpoint(&self, label: &str) {
            self.log.lock().unwrap().push(format!("checkpoint:{label}"));
        }
        fn on_revert(&self, label: &str, _err: &str, rolled_back: bool) {
            self.log
                .lock()
                .unwrap()
                .push(format!("revert:{label}:rolled_back={rolled_back}"));
        }
        fn on_promote(&self, label: &str, promoted: bool, lineage: Option<super::super::turn_ledger::PromotedLineage>) {
            self.log.lock().unwrap().push(format!(
                "promote:{label}:promoted={promoted}:lineage={}",
                lineage.is_some()
            ));
        }
    }

    #[tokio::test]
    async fn ledger_witness_sequence_ok_and_err() {
        let dir = TempDir::new().unwrap();
        let mem = Arc::new(Mutex::new(BranchableMemory::create(dir.path(), 4).unwrap()));
        let concrete = Arc::new(RecordingLedger::default());
        let ledger: Arc<dyn TurnLedger> = concrete.clone();

        let ok: clawft_types::Result<u8> =
            with_turn_checkpoint(&mem, Some(&ledger), "t-ok", || async { Ok(1u8) }, no_items, false, &mut None).await;
        assert!(ok.is_ok());
        let err: clawft_types::Result<u8> = with_turn_checkpoint(&mem, Some(&ledger), "t-err", || async {
            Err(ClawftError::CowMemory { reason: "boom".into() })
        }, no_items, false, &mut None)
        .await;
        assert!(err.is_err());

        let events = concrete.events();
        assert_eq!(
            events,
            vec![
                "checkpoint:t-ok".to_string(),
                "promote:t-ok:promoted=true:lineage=true".to_string(),
                "checkpoint:t-err".to_string(),
                "revert:t-err:rolled_back=true".to_string(),
            ],
            "append-only witness order: checkpoint precedes its outcome, \
             revert witnesses the discard"
        );
    }

    #[tokio::test]
    async fn exchange_ingest_promotes_on_ok_and_adds_nothing_on_err() {
        // THE Phase-3 exit test: a successful turn's exchange is queryable
        // after promote; a failed turn adds nothing (its branch is
        // discarded and items_on_ok is never called on Err).
        let dir = TempDir::new().unwrap();
        let mem = test_mem(&dir); // dim 4
        let items = |reply: &'static str| {
            move |_: &String| plumbing_items(4, "hello loom", reply)
        };

        let ok: clawft_types::Result<String> = with_turn_checkpoint(
            &mem,
            None,
            "t-ok",
            || async { Ok("threads woven".to_string()) },
            items("threads woven"),
            false,
            &mut None,
        )
        .await;
        assert!(ok.is_ok());
        {
            let guard = mem.lock().unwrap();
            let probe = crate::embeddings::hash_embedder::HashEmbedder::new(4)
                .compute_embedding("threads woven");
            let hits = guard.query(&probe, 4).expect("query");
            assert!(!hits.is_empty(), "promoted exchange must be queryable");
        }

        let before = {
            let guard = mem.lock().unwrap();
            let s = guard.status();
            s.base.total_vectors + s.working.total_vectors
        };
        let err: clawft_types::Result<String> = with_turn_checkpoint(
            &mem,
            None,
            "t-err",
            || async {
                Err(ClawftError::Timeout {
                    operation: "boom".into(),
                })
            },
            items("never spoken"),
            false,
            &mut None,
        )
        .await;
        assert!(err.is_err());
        let after = {
            let guard = mem.lock().unwrap();
            let s = guard.status();
            s.base.total_vectors + s.working.total_vectors
        };
        assert_eq!(before, after, "a failed turn must add nothing to memory");
    }

    #[tokio::test]
    async fn exchange_items_shape() {
        let items = plumbing_items(8, "ask", "answer").await.expect("items");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].tags.get("role").map(String::as_str), Some("user"));
        assert_eq!(
            items[1].tags.get("role").map(String::as_str),
            Some("assistant")
        );
        assert!(items.iter().all(|i| i.vector.len() == 8));
        assert_eq!(plumbing_items(8, "ask", "  ").await.expect("items").len(), 1);
    }

    #[tokio::test]
    async fn bracket_auto_resumes_a_parked_lineage() {
        // WEFT-652 AutoResume: an idle-autopaused (or reopened-paused)
        // lineage rehydrates on the next turn instead of failing closed.
        let dir = TempDir::new().unwrap();
        let mem = test_mem(&dir);
        {
            let mut guard = mem.lock().unwrap();
            guard.pause("test-park").expect("pause");
            assert!(guard.is_paused());
        }
        let items = |_: &String| plumbing_items(4, "wake up", "awake");
        let out: clawft_types::Result<String> =
            with_turn_checkpoint(&mem, None, "t-wake", || async { Ok("awake".to_string()) }, items, false, &mut None)
                .await;
        assert!(out.is_ok(), "turn on a parked lineage must auto-resume, not fail");
        let guard = mem.lock().unwrap();
        assert!(!guard.is_paused(), "lineage resumed");
        let probe =
            crate::embeddings::hash_embedder::HashEmbedder::new(4).compute_embedding("awake");
        assert!(
            !guard.query(&probe, 4).expect("query").is_empty(),
            "post-resume exchange promoted and queryable"
        );
    }

    #[tokio::test]
    async fn review_mode_holds_then_accept_promotes_witnessed() {
        // WEFT-654: review holds (no promote, no promote witness), accept =
        // promote with lineage witnessed — the human-gated finalize.
        let dir = TempDir::new().unwrap();
        let mem = test_mem(&dir);
        let concrete = Arc::new(RecordingLedger::default());
        let ledger: Arc<dyn TurnLedger> = concrete.clone();
        let items = |_: &String| plumbing_items(4, "hold me", "held reply");

        let mut held = None;
        let out: clawft_types::Result<String> = with_turn_checkpoint(
            &mem, Some(&ledger), "t-hold", || async { Ok("held reply".to_string()) },
            items, true, &mut held,
        )
        .await;
        assert!(out.is_ok());
        let held = held.expect("review mode must yield a HeldTurn");
        assert_eq!(held.label, "t-hold");
        {
            let events = concrete.events();
            assert!(events.iter().any(|e| e.starts_with("checkpoint:t-hold")));
            assert!(
                !events.iter().any(|e| e.starts_with("promote:")),
                "no promote witness while held: {events:?}"
            );
        }
        // Held writes are visible via the chain-walk (working holds them)…
        let probe = crate::embeddings::hash_embedder::HashEmbedder::new(4)
            .compute_embedding("held reply");
        assert!(!mem.lock().unwrap().query(&probe, 4).unwrap().is_empty());

        accept_held(&mem, Some(&ledger), held).expect("accept");
        let events = concrete.events();
        assert!(
            events.iter().any(|e| e.starts_with("promote:t-hold:promoted=true:lineage=true")),
            "accept promotes with lineage witnessed: {events:?}"
        );
        // …and remain queryable after promote.
        assert!(!mem.lock().unwrap().query(&probe, 4).unwrap().is_empty());
    }

    #[tokio::test]
    async fn review_mode_discard_rolls_back_witnessed() {
        let dir = TempDir::new().unwrap();
        let mem = test_mem(&dir);
        let concrete = Arc::new(RecordingLedger::default());
        let ledger: Arc<dyn TurnLedger> = concrete.clone();
        let items = |_: &String| plumbing_items(4, "hold me", "unwanted");

        let mut held = None;
        let _: clawft_types::Result<String> = with_turn_checkpoint(
            &mem, Some(&ledger), "t-no", || async { Ok("unwanted".to_string()) },
            items, true, &mut held,
        )
        .await;
        let held = held.expect("held");

        discard_held(&mem, Some(&ledger), held, "reviewer said no").expect("discard");
        let events = concrete.events();
        assert!(
            events.iter().any(|e| e.starts_with("revert:t-no:rolled_back=true")),
            "discard witnesses the revert: {events:?}"
        );
        let probe = crate::embeddings::hash_embedder::HashEmbedder::new(4)
            .compute_embedding("unwanted");
        assert!(
            mem.lock().unwrap().query(&probe, 4).unwrap().is_empty(),
            "discarded exchange must vanish"
        );
    }

    #[tokio::test]
    async fn cancelled_drop_rolls_back_and_witnesses_like_a_discard() {
        // WEFT-655 THE alignment test: cancel (future drop), turn error, and
        // proposal discard must converge on the same closure — memory rolled
        // back to pre-turn state + a witnessed compensating revert. This
        // reproduces AgentService::cancel's select!-drop shape exactly.
        let dir = TempDir::new().unwrap();
        let mem = test_mem(&dir);
        let concrete = Arc::new(RecordingLedger::default());
        let ledger: Arc<dyn TurnLedger> = concrete.clone();

        // Baseline: one promoted vector so rollback has a real target state.
        {
            let mut g = mem.lock().unwrap();
            g.ingest(&[clawft_cow_memory::VectorItem::new(vec![0.5, 0.5, 0.0, 0.0])])
                .unwrap();
            g.promote().unwrap();
        }
        let count = |mem: &Arc<Mutex<BranchableMemory>>| {
            let g = mem.lock().unwrap();
            let s = g.status();
            s.base.total_vectors + s.working.total_vectors
        };
        let before = count(&mem);

        // A turn that writes mid-flight then parks forever; we cancel by
        // dropping the bracket future at a select!, like the service does.
        let never = tokio::sync::Notify::new();
        let mut held_slot = None;
        let bracket = with_turn_checkpoint(
            &mem,
            Some(&ledger),
            "t-cancelled",
            || async {
                {
                    let mut g = mem.lock().unwrap();
                    g.ingest(&[clawft_cow_memory::VectorItem::new(vec![
                        0.0, 0.0, 1.0, 0.0,
                    ])])
                    .unwrap();
                }
                never.notified().await; // parked "generation"
                Ok(String::from("unreachable"))
            },
            no_items,
            false,
            &mut held_slot,
        );
        tokio::select! {
            biased;
            _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
            _ = bracket => panic!("bracket must not resolve"),
        }
        // The select! dropped the bracket future → guard fired.
        assert_eq!(count(&mem), before, "cancelled turn leaves no memory residue");
        let events = concrete.events();
        assert!(
            events
                .iter()
                .any(|e| e.starts_with("revert:t-cancelled:rolled_back=true")),
            "cancel witnesses the compensating revert like a discard: {events:?}"
        );

        // Parity: an errored turn and a discarded proposal produce the same
        // end-state class — no residue + revert witness with their labels.
        let _: clawft_types::Result<u8> = with_turn_checkpoint(
            &mem, Some(&ledger), "t-err", || async {
                Err(ClawftError::Timeout { operation: "boom".into() })
            },
            no_items, false, &mut None,
        )
        .await;
        let mut held = None;
        let _: clawft_types::Result<String> = with_turn_checkpoint(
            &mem, Some(&ledger), "t-disc", || async { Ok("x".to_string()) },
            |_: &String| plumbing_items(4, "q", "x"), true, &mut held,
        )
        .await;
        discard_held(&mem, Some(&ledger), held.unwrap(), "reviewer").unwrap();

        assert_eq!(count(&mem), before, "all three paths leave identical memory state");
        let events = concrete.events();
        for lbl in ["t-cancelled", "t-err", "t-disc"] {
            assert!(
                events.iter().any(|e| e.starts_with(&format!("revert:{lbl}:rolled_back=true"))),
                "{lbl} must witness its revert: {events:?}"
            );
        }
    }

    /// The no-write items closure for tests that drive their own ingests.
    fn no_items<T>(
        _: &T,
    ) -> std::future::Ready<clawft_types::Result<Vec<clawft_cow_memory::VectorItem>>> {
        std::future::ready(Ok(Vec::new()))
    }

    /// PLUMBING-ONLY test embedder: equal text gives equal vectors, nothing
    /// more. These tests exercise the bracket and the dimension guard, never
    /// recall quality; production has no hash path (see `exchange_items`).
    struct PlumbingEmbedder {
        inner: crate::embeddings::hash_embedder::HashEmbedder,
        dim: usize,
        /// Reported dimension, when it should disagree with the vectors.
        claims: Option<usize>,
    }
    impl PlumbingEmbedder {
        fn new(dim: usize) -> Self {
            Self { inner: crate::embeddings::hash_embedder::HashEmbedder::new(dim), dim, claims: None }
        }
    }
    #[async_trait::async_trait]
    impl crate::embeddings::Embedder for PlumbingEmbedder {
        async fn embed(&self, text: &str) -> Result<Vec<f32>, crate::embeddings::EmbeddingError> {
            Ok(self.inner.compute_embedding(text))
        }
        fn dimension(&self) -> usize {
            self.claims.unwrap_or(self.dim)
        }
        fn name(&self) -> &str {
            "plumbing-test-embedder"
        }
    }

    async fn plumbing_items(
        dim: u16,
        user: &str,
        reply: &str,
    ) -> clawft_types::Result<Vec<clawft_cow_memory::VectorItem>> {
        exchange_items(&PlumbingEmbedder::new(dim as usize), dim, "cli", "c1", user, reply).await
    }

    fn test_mem(dir: &TempDir) -> Arc<Mutex<BranchableMemory>> {
        let mem = BranchableMemory::create(dir.path(), 4).expect("create BranchableMemory");
        Arc::new(Mutex::new(mem))
    }

    #[tokio::test]
    async fn successful_turn_promotes_writes() {
        let dir = TempDir::new().unwrap();
        let mem = test_mem(&dir);

        let result: clawft_types::Result<()> = with_turn_checkpoint(&mem, None, "t1", || async {
            let mut guard = mem.lock().unwrap();
            guard
                .ingest(&[clawft_cow_memory::VectorItem::new(vec![1.0, 0.0, 0.0, 0.0])])
                .map_err(|e| ClawftError::CowMemory {
                    reason: e.to_string(),
                })?;
            Ok(())
        }, no_items, false, &mut None)
        .await;

        assert!(result.is_ok());

        let guard = mem.lock().unwrap();
        // Promoted into base and visible via a fresh query.
        let hits = guard.query(&[1.0, 0.0, 0.0, 0.0], 5).expect("query");
        assert_eq!(hits.len(), 1, "promoted vector should be queryable");
    }

    #[tokio::test]
    async fn failed_turn_rolls_back_writes() {
        let dir = TempDir::new().unwrap();
        let mem = test_mem(&dir);

        let result: clawft_types::Result<()> = with_turn_checkpoint(&mem, None, "t1", || async {
            let mut guard = mem.lock().unwrap();
            guard
                .ingest(&[clawft_cow_memory::VectorItem::new(vec![1.0, 0.0, 0.0, 0.0])])
                .map_err(|e| ClawftError::CowMemory {
                    reason: e.to_string(),
                })?;
            drop(guard);
            Err(ClawftError::Timeout {
                operation: "simulated turn failure".into(),
            })
        }, no_items, false, &mut None)
        .await;

        assert!(result.is_err());

        let guard = mem.lock().unwrap();
        let hits = guard.query(&[1.0, 0.0, 0.0, 0.0], 5).expect("query");
        assert!(
            hits.is_empty(),
            "rolled-back turn's writes must not be queryable"
        );
    }

    #[tokio::test]
    async fn disabled_memory_untouched_by_a_turn_that_never_wraps_it() {
        // Sanity check for the "zero behavior change when disabled" claim:
        // a turn that simply never calls into cow_memory leaves it exactly
        // as `create` left it (empty, dimension unchanged).
        let dir = TempDir::new().unwrap();
        let mem = test_mem(&dir);
        let guard = mem.lock().unwrap();
        let status = guard.status();
        assert_eq!(status.dimension, 4);
        assert_eq!(status.own_checkpoint_depth, 0);
    }

    // ── Per-lineage vector dimension: refuse to mix (owner decision 2026-11-12) ──

    fn total_vectors(mem: &Arc<Mutex<BranchableMemory>>) -> u64 {
        let guard = mem.lock().unwrap();
        let st = guard.status();
        st.base.total_vectors + st.working.total_vectors
    }

    /// An embedder of another width than the lineage is refused with the
    /// explicit dimension-mismatch error. Nothing is embedded, truncated or
    /// padded, and the lineage stays as it was.
    #[tokio::test]
    async fn exchange_items_refuses_an_embedder_of_another_dimension() {
        // Lineage is 4-d, embedder is 8-d (a real model is 768-d).
        let err = exchange_items(&PlumbingEmbedder::new(8), 4, "cli", "c1", "ask", "answer")
            .await
            .expect_err("mixing dimensions must be refused");
        let msg = err.to_string();
        assert!(msg.contains("dimension mismatch: expected 4, got 8"), "{msg}");

        // An embedder that CLAIMS the lineage width but returns another is
        // refused per vector: no truncation, no padding.
        let liar = PlumbingEmbedder { claims: Some(4), ..PlumbingEmbedder::new(8) };
        let err = exchange_items(&liar, 4, "cli", "c1", "ask", "answer")
            .await
            .expect_err("a vector of the wrong length must be refused");
        assert!(err.to_string().contains("dimension mismatch: expected 4, got 8"), "{err}");
    }

    /// Through the bracket: a turn whose exchange cannot be written because
    /// of a dimension mismatch still returns its reply and promotes, but the
    /// lineage gains nothing (no truncated or padded vector slipped in).
    #[tokio::test]
    async fn turn_with_a_mismatched_embedder_writes_nothing() {
        let dir = TempDir::new().unwrap();
        let mem = test_mem(&dir); // dim 4
        let before = total_vectors(&mem);
        let ok: clawft_types::Result<String> = with_turn_checkpoint(
            &mem,
            None,
            "t-mismatch",
            || async { Ok("reply".to_string()) },
            |_: &String| async {
                exchange_items(&PlumbingEmbedder::new(8), 4, "cli", "c1", "ask", "reply").await
            },
            false,
            &mut None,
        )
        .await;
        assert_eq!(ok.unwrap(), "reply", "the reply is not memory's to withhold");
        assert_eq!(total_vectors(&mem), before, "a mismatched exchange must write nothing");
    }

    /// The lineage itself refuses to mix on both write and query.
    #[test]
    fn lineage_refuses_writes_and_queries_at_another_dimension() {
        use clawft_cow_memory::{CowMemoryError, VectorItem};
        let dir = TempDir::new().unwrap();
        let mem = test_mem(&dir); // dim 4
        let mut guard = mem.lock().unwrap();
        let w = guard.ingest(&[VectorItem::new(vec![1.0; 8])]).expect_err("8-d write into a 4-d lineage");
        assert!(matches!(w, CowMemoryError::DimensionMismatch { expected: 4, got: 8 }), "{w:?}");
        let q = guard.query(&[1.0; 8], 3).expect_err("8-d query on a 4-d lineage");
        assert!(matches!(q, CowMemoryError::DimensionMismatch { expected: 4, got: 8 }), "{q:?}");
        // A 2-d vector is not padded up either.
        let short = guard.ingest(&[VectorItem::new(vec![1.0; 2])]).expect_err("short write");
        assert!(matches!(short, CowMemoryError::DimensionMismatch { expected: 4, got: 2 }), "{short:?}");
    }
}
