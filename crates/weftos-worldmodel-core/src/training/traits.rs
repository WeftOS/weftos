//! Training-surface kinds, hot-swap outcome and the trainer / hot-swap traits.

use super::{ModelCheckpoint, SensorClass, SmallModelKind, TrainingSample};
use crate::error::WorldModelResult;
use crate::rollback_gate::GateVerdict;

/// Which of the two LeWM training surfaces produced a checkpoint (WEFT-531).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TrainingSurfaceKind {
    /// Offline per-sensor-class edge intelligence (RVF-delivered, hot-swap).
    OfflineEdge,
    /// Online streaming-merge world-model training (AND-gated promotion).
    OnlineStreamingMerge,
}

impl TrainingSurfaceKind {
    /// Stable wire / log name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OfflineEdge => "offline_edge",
            Self::OnlineStreamingMerge => "online_streaming_merge",
        }
    }
}

/// ExoChain / metrics event kind for training-surface cycles.
pub const EVENT_KIND_TRAINING_CYCLE: &str = "lewm.training_cycle";
/// ExoChain / metrics event kind for model hot-swap / rollback.
pub const EVENT_KIND_MODEL_HOT_SWAP: &str = "lewm.model_hot_swap";

/// Outcome of a tick-aligned hot-swap commit or rollback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum HotSwapOutcome {
    /// No pending checkpoint for this tick.
    Idle,
    /// Pending checkpoint committed to active.
    Committed {
        /// New active version tag.
        version_tag: u64,
    },
    /// Active rolled back to last-good segment.
    RolledBack {
        /// Version tag restored.
        version_tag: u64,
    },
    /// Pending still waiting for a later tick.
    Deferred {
        /// Tick at which commit becomes eligible.
        target_tick: u64,
    },
}

// ── Traits ─────────────────────────────────────────────────────────────────

/// Offline per-sensor-class edge-intelligence trainer (WEFT-531 surface 1).
///
/// Collects replay / synthetic samples, runs a train cycle, and emits an
/// RVF-deliverable [`ModelCheckpoint`] for tick-aligned hot-swap.
pub trait OfflineEdgeTrainer {
    /// Push one sample into the offline buffer.
    fn push_sample(&mut self, sample: &TrainingSample) -> WorldModelResult<()>;

    /// Run one offline train cycle for `class` / `kind`.
    ///
    /// Returns a checkpoint ready for RVF packaging + staging.
    fn train_cycle(
        &mut self,
        class: SensorClass,
        kind: SmallModelKind,
        target_tick: u64,
    ) -> WorldModelResult<ModelCheckpoint>;

    /// Number of buffered samples (all classes).
    fn sample_count(&self) -> usize;
}

/// Online streaming-merge world-model trainer (WEFT-531 surface 2).
///
/// Uses per-class importance-weighted replay and must consult the four-
/// condition AND gate before promoting a checkpoint.
pub trait StreamingMergeTrainer {
    /// Push a live (or ExoChain-replay) sample into the importance buffer.
    fn push_sample(&mut self, sample: &TrainingSample) -> WorldModelResult<()>;

    /// One streaming train step: sample a weighted batch, update weights,
    /// evaluate the AND gate, and return a checkpoint only when promotion
    /// is allowed (otherwise `Err` or a non-promoted status via result).
    fn train_step(
        &mut self,
        class: SensorClass,
        kind: SmallModelKind,
    ) -> WorldModelResult<StreamingTrainResult>;

    /// Last AND-gate verdict observed during training (if any).
    fn last_gate(&self) -> Option<GateVerdict>;

    /// Number of samples currently in the replay buffer.
    fn replay_len(&self) -> usize;
}

/// Result of one online streaming-merge train step.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamingTrainResult {
    /// Candidate checkpoint (always produced; may not be promoted).
    pub checkpoint: ModelCheckpoint,
    /// AND-gate verdict that decided promotion.
    pub gate: GateVerdict,
    /// `true` only when the gate allowed promotion.
    pub promoted: bool,
}

/// Tick-aligned hot-swap registry for RVF-hosted small models (WEFT-532).
///
/// Stages a pending checkpoint, commits at the next eligible tick boundary,
/// and rolls back to the last-good segment when the AND gate (or host) vetoes.
pub trait ModelHotSwap {
    /// Stage a checkpoint for commit at `checkpoint.target_tick` (or next tick).
    fn stage(&mut self, checkpoint: ModelCheckpoint) -> WorldModelResult<()>;

    /// Attempt tick-aligned commits for all pending slots at `tick`.
    ///
    /// Returns one outcome per slot that was pending or rolled back.
    fn commit_at_tick(
        &mut self,
        tick: u64,
    ) -> WorldModelResult<alloc::vec::Vec<(SensorClass, SmallModelKind, HotSwapOutcome)>>;

    /// Force rollback of one slot to last-good (e.g. after AND-gate veto).
    fn rollback(
        &mut self,
        class: SensorClass,
        kind: SmallModelKind,
    ) -> WorldModelResult<HotSwapOutcome>;

    /// Active checkpoint for a slot, if any has been committed.
    fn active(&self, class: SensorClass, kind: SmallModelKind) -> Option<&ModelCheckpoint>;

    /// Last-good checkpoint retained for rollback, if any.
    fn last_good(&self, class: SensorClass, kind: SmallModelKind) -> Option<&ModelCheckpoint>;
}
