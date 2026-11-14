//! `weftos-worldmodel` — single user-facing entrypoint for the LeWM latent
//! world model (WEFT-522).
//!
//! Re-exports:
//! - **traits + latent contract** from [`weftos_worldmodel_core`] (WEFT-520)
//! - **default stubs + wiring** from [`weftos_worldmodel_impls`] (WEFT-521)
//!
//! Downstream crates (sensor pipeline, service binary, LatticeApi hosts)
//! should depend on this crate rather than pin core/impls separately.
//!
//! # Latent contract (WEFT-543)
//!
//! SIGReg manifold is isotropic Gaussian `N(0, I)` in **192** dimensions for
//! `mesh.sensor.v1`. Changing width requires a wire major-version bump.
//!
//! # Training surfaces (WEFT-531 / WEFT-532)
//!
//! Default builds are **weights-free** (`no_std` + `alloc`) but expose:
//! - offline edge + online streaming-merge trainers (stub residual optimisers)
//! - per-sensor-class RVF segment I/O + tick-aligned hot-swap registry
//! - AND-gate promotion / rollback hooks
//!
//! Both trainers fit the action-conditioned linear `pred_φ` (`ẑ = z + A·a + b`).
//! The optional `candle` feature adds the ViT-tiny encoder and the AdaLN
//! `pred_φ` (safetensors checkpoints, AdamW training);
//! [`DefaultWorldModel::with_adaln`] swaps AdaLN in for prediction.
//!
//! # Quick start (stubs)
//!
//! ```rust
//! use weftos_worldmodel::{
//!     default_stub_lattice, Action, Encoder, LatticeApi, NullActionEncoder,
//!     NullEncoder, ObservationFrame, Predictor, ActionEncoder, LATENT_DIM_U16,
//! };
//!
//! let mut lattice = default_stub_lattice();
//! let z = lattice
//!     .observe(ObservationFrame {
//!         bytes: b"frame",
//!         latent_dim: LATENT_DIM_U16,
//!         timestamp_ms: 0,
//!     })
//!     .expect("observe");
//! assert_eq!(z.len(), 192);
//!
//! let enc = NullEncoder;
//! let a = NullActionEncoder.encode_bytes(b"noop").expect("action");
//! let _ = enc.encode(b"sensor").expect("encode");
//! let _ = lattice.predict(&z, &a).expect("predict");
//! let _ = lattice.plan(&z, 4).expect("plan");
//! ```
//!
//! # ExoChain attestation (WEFT-533)
//!
//! With `feature = "std"`, [`attestation`] serializes
//! [`ObservationTuple`] `(a_t, z_t, z_{t+1}, surprise)` with a
//! version-tagged SIGReg manifold reference for append onto a
//! [`ChainSink`] (in-memory or kernel ExoChain bridge).
//!
//! # Feature flags
//!
//! - **default** — stub path, `no_std` + `alloc`
//! - **std** — host marker + JSON attestation encode/decode
//! - **candle** — ViT-tiny encoder + AdaLN `pred_φ` on candle 0.11 CPU (implies `std`)

#![cfg_attr(not(feature = "std"), no_std)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![deny(missing_docs)]

extern crate alloc;

// ── Underlying crates (advanced / selective imports) ───────────────────────
/// Core traits and latent contract (WEFT-520). Prefer top-level re-exports.
pub use weftos_worldmodel_core as core;
/// Concrete stubs and optional candle skeleton (WEFT-521). Prefer top-level re-exports.
pub use weftos_worldmodel_impls as impls;

// ── Core: traits, types, latent contract, attestation ──────────────────────
pub use weftos_worldmodel_core::{
    latent_dim_matches_v1, surprise_voe, zero_latent, Action, ActionPlan, Encoder, GateCondition,
    GateVerdict, HotSwapOutcome, Latent, LatentPlanner, LatentVersion, LatticeApi, LatticeMethod,
    ModelCheckpoint, ModelHotSwap, NodeId, ObservationFrame, ObservationTuple, OfflineEdgeTrainer,
    PlanStep, PlannerKind, Predictor, RecallHit, RollbackGate, RollbackGateMetrics, SensorClass,
    SigRegHealth, SigRegMonitor, SmallModelKind, StreamingMergeTrainer, StreamingTrainResult,
    SubscriptionId, TrainingSample, TrainingSurfaceKind, WorldModelError, WorldModelResult,
    ATTESTATION_SOURCE, EVENT_KIND_LEWM_FRAME_ATTESTATION, EVENT_KIND_MODEL_HOT_SWAP,
    EVENT_KIND_ROLLBACK_GATE, EVENT_KIND_TRAINING_CYCLE, GATE_HELD_OUT_PROBE_MIN,
    GATE_SIGREG_HEALTH_MIN, GATE_TEMPORAL_STRAIGHTEN_MIN, GATE_VOE_DIFF_MIN,
    LATENT_DIM, LATENT_DIM_U16, LATENT_SCHEMA_MAJOR_V1, LATTICE_METHOD_COUNT, LATTICE_METHODS,
    LEWM_MODEL_SEGMENT_DOMAIN_END, LEWM_MODEL_SEGMENT_DOMAIN_START,
    SIGREG_HEALTH_ROLLBACK_THRESHOLD, SIGREG_HEALTH_WINDOW_SECS,
};

// ── Host attestation path (JSON + ChainSink; requires `std`) ───────────────
#[cfg(feature = "std")]
#[cfg_attr(docsrs, doc(cfg(feature = "std")))]
pub mod attestation;

#[cfg(feature = "std")]
#[cfg_attr(docsrs, doc(cfg(feature = "std")))]
pub use attestation::{
    attest_frame, AttestationError, AttestationPayload, ChainEntry, ChainSink, MemoryChainSink,
    ATTESTATION_SCHEMA_MAJOR, ATTESTATION_SCHEMA_MINOR,
};

// ── Impls: runtime monitors/planners + stubs + action encoder ─────────────
pub use weftos_worldmodel_impls::{
    decode_model_segment, encode_model_segment, model_segment_from_wire, model_segment_to_wire,
    planner_for_kind, require_promoted, ActionEncoder, CemPlanner, FourConditionRollbackGate,
    GateProbeKind,
    GradientPlanner, HashActionEncoder, HashEncoder, IdentityPredictor, ImportanceReplayBuffer,
    LewmModelSegment, LinearPredPhi, ActionLinearModel, MppiWarmPlanner, NullActionEncoder, NullEncoder, NullPlanner,
    NullPredictor, NullSigRegMonitor, OfflineEdgePipeline, PredPhi, ProbePair, RollbackGateLog,
    SensorModelRegistry, SigRegHealthEvent, SigRegHealthLog, SlotState, StreamingMergePipeline,
    StubLattice, WelfordSigRegMonitor, ACTION_CODE_DIM, DEFAULT_BOOT_VERSION, DEFAULT_MIN_SAMPLES,
    DEFAULT_PROBE_CAPACITY, DEFAULT_REPLAY_CAPACITY, DEFAULT_TRAJECTORY_CAPACITY,
    EVENT_KIND_SIGREG_HEALTH, SEGMENT_HEADER_LEN, ACTION_LINEAR_BLOB_LEN,
    LEWM_MODEL_SEGMENT_CODEC_RAW, LEWM_MODEL_SEGMENT_CODEC_SAFETENSORS, codec_for_weights,
};

// ── Optional candle ViT-tiny + AdaLN (plan W5) ─────────────────────────────
#[cfg(feature = "candle")]
#[cfg_attr(docsrs, doc(cfg(feature = "candle")))]
pub use weftos_worldmodel_impls::{
    active_candle_model, candle_cpu_device, train_adaln, train_adaln_with, AdaLnConfig,
    AdaLnPredictor, AdaLnTrainConfig, AdaLnTrainReport, CandleModel, CandleModelError,
    CandleVitEncoder, VitTinyConfig,
};

// ── ServiceApi-shaped LatticeApi facade (WEFT-527) ─────────────────────────
/// LatticeApi via ServiceApi-shaped call surface (requires `service-api`).
#[cfg(feature = "service-api")]
#[cfg_attr(docsrs, doc(cfg(feature = "service-api")))]
pub mod service_api;

#[cfg(feature = "service-api")]
#[cfg_attr(docsrs, doc(cfg(feature = "service-api")))]
pub use service_api::{
    default_lattice_service, dispatch_typed, observe_json, parse_lattice_method, FacadeHealth,
    FacadeServiceInfo, LatticeServiceError, LatticeServiceFacade, LatticeServiceResult,
    LATTICE_SERVICE_NAME, LATTICE_SERVICE_TYPE,
};

/// Facade crate version string.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Build the default weight-free [`StubLattice`] for service scaffolding / tests.
///
/// Composes null encoder, linear `pred_φ`, and CEM planner. No ML weights and
/// no candle dependency on the default feature set.
#[inline]
pub fn default_stub_lattice() -> StubLattice {
    StubLattice::default()
}

/// Default runtime stack: encoder, action encoder, `pred_φ`, CEM planner,
/// lattice, Welford SIGReg monitor, four-condition AND rollback gate, and
/// both training surfaces + sensor-model hot-swap registry
/// (WEFT-528 / WEFT-529 / WEFT-530 / WEFT-531 / WEFT-532).
///
/// Weights-free but not a pure no-op: linear residual prediction, CEM planning,
/// online SIGReg health with auto-rollback, streaming-merge promotion gating,
/// offline edge + online streaming trainers (stub residual optimisers), and
/// tick-aligned RVF model hot-swap. Trained AdaLN / ViT weights remain residual
/// (see impls README).
#[derive(Debug, Clone)]
pub struct DefaultWorldModel {
    /// Sensor → latent encoder (null by default).
    pub encoder: NullEncoder,
    /// Control bytes → action code (null by default).
    pub action_encoder: NullActionEncoder,
    /// Action-conditioned `pred_φ` (linear residual).
    pub predictor: LinearPredPhi,
    /// AdaLN `pred_φ`; when set, [`DefaultWorldModel::predict`] uses it instead of
    /// [`Self::predictor`].
    #[cfg(feature = "candle")]
    pub adaln: Option<AdaLnPredictor>,
    /// Per-class predictors auto-loaded from committed `Encode` checkpoints.
    class_models: autoload::ClassModels,
    /// CEM planner (default algorithm).
    pub planner: CemPlanner,
    /// Composed lattice API (observe / predict / plan / recall / subscribe).
    pub lattice: StubLattice,
    /// Welford SIGReg health monitor (auto-rollback at 0.85 / 30s).
    pub sigreg: WelfordSigRegMonitor,
    /// Four-condition AND rollback / promotion gate (WEFT-530).
    pub rollback_gate: FourConditionRollbackGate,
    /// Offline per-sensor-class edge-intelligence trainer (WEFT-531).
    pub offline_edge: OfflineEdgePipeline,
    /// Online streaming-merge trainer with importance-weighted replay (WEFT-531).
    pub streaming_merge: StreamingMergePipeline,
    /// Per-sensor-class RVF-hosted small models + tick hot-swap (WEFT-532).
    pub sensor_models: SensorModelRegistry,
}

impl Default for DefaultWorldModel {
    fn default() -> Self {
        Self::new()
    }
}

impl DefaultWorldModel {
    /// Construct the default runtime stack.
    #[inline]
    pub fn new() -> Self {
        Self {
            encoder: NullEncoder,
            action_encoder: NullActionEncoder,
            predictor: LinearPredPhi::default(),
            #[cfg(feature = "candle")]
            adaln: None,
            class_models: autoload::ClassModels::default(),
            planner: CemPlanner::default(),
            lattice: StubLattice::default(),
            sigreg: WelfordSigRegMonitor::new(1),
            rollback_gate: FourConditionRollbackGate::new(),
            offline_edge: OfflineEdgePipeline::new(),
            streaming_merge: StreamingMergePipeline::new(),
            sensor_models: SensorModelRegistry::new(),
        }
    }

    /// Default stack predicting with a loaded AdaLN `pred_φ`.
    #[cfg(feature = "candle")]
    pub fn with_adaln(predictor: AdaLnPredictor) -> Self {
        Self { adaln: Some(predictor), ..Self::new() }
    }

    /// Observe a frame through the composed lattice and update SIGReg health.
    pub fn observe(
        &mut self,
        frame: ObservationFrame<'_>,
    ) -> WorldModelResult<(Latent, SigRegHealth)> {
        let z = self.lattice.observe(frame)?;
        let health = self.sigreg.update_at(&z, frame.timestamp_ms)?;
        self.rollback_gate.set_sigreg_health(health.score);
        self.streaming_merge.gate.set_sigreg_health(health.score);
        Ok((z, health))
    }

    /// Ingest a full transition into the four-condition gate and return the
    /// AND verdict (blocks streaming-merge promotion when any condition fails).
    pub fn evaluate_transition(
        &mut self,
        z_t: &Latent,
        z_hat: &Latent,
        z_tp1: &Latent,
        sigreg_health: Option<f32>,
    ) -> GateVerdict {
        let v = self
            .rollback_gate
            .observe_transition(z_t, z_hat, z_tp1, sigreg_health);
        // Keep streaming-merge gate in sync for promotion decisions.
        self.streaming_merge
            .observe_transition_for_gate(z_t, z_hat, z_tp1, sigreg_health);
        v
    }

    /// Whether a streaming-merge checkpoint may be promoted right now.
    pub fn may_promote_checkpoint(&self) -> bool {
        self.rollback_gate.may_promote()
    }

    /// One offline edge train cycle → stage for tick-aligned hot-swap.
    pub fn offline_edge_cycle(
        &mut self,
        class: SensorClass,
        kind: SmallModelKind,
        target_tick: u64,
    ) -> WorldModelResult<ModelCheckpoint> {
        let cp = OfflineEdgeTrainer::train_cycle(&mut self.offline_edge, class, kind, target_tick)?;
        ModelHotSwap::stage(&mut self.sensor_models, cp.clone())?;
        Ok(cp)
    }

    /// One online streaming-merge train step; stages only when AND gate promotes.
    pub fn streaming_merge_step(
        &mut self,
        class: SensorClass,
        kind: SmallModelKind,
    ) -> WorldModelResult<StreamingTrainResult> {
        // Share host SIGReg health into the streaming gate.
        let health = SigRegMonitor::health(&self.sigreg);
        self.streaming_merge
            .gate
            .set_sigreg_health(health.score);
        let result =
            StreamingMergeTrainer::train_step(&mut self.streaming_merge, class, kind)?;
        if result.promoted {
            let mut cp = result.checkpoint.clone();
            cp.target_tick = Some(self.sensor_models.current_tick.saturating_add(1));
            ModelHotSwap::stage(&mut self.sensor_models, cp)?;
        } else {
            // Veto: roll back active slot for this class/kind if a prior good exists.
            if let Ok(outcome) = self.sensor_models.apply_gate_verdict(class, kind, result.gate) {
                self.note_encode_outcome(class, kind, outcome);
            }
        }
        Ok(result)
    }

    /// Commit pending hot-swaps at DEMOCRITUS / mesh tick boundary. A committed `Encode`
    /// checkpoint also rebuilds that class's `pred_φ` (see [`mod@autoload`] docs).
    pub fn commit_models_at_tick(
        &mut self,
        tick: u64,
    ) -> WorldModelResult<alloc::vec::Vec<(SensorClass, SmallModelKind, HotSwapOutcome)>> {
        let outcomes = ModelHotSwap::commit_at_tick(&mut self.sensor_models, tick)?;
        for &(class, kind, outcome) in &outcomes {
            self.note_encode_outcome(class, kind, outcome);
        }
        Ok(outcomes)
    }
}

mod autoload;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod autoload_tests;
