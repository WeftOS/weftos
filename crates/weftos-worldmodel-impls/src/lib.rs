//! `weftos-worldmodel-impls` — concrete LeWM trait implementations (WEFT-521).
//!
//! Default path is **weights-free**: null/stub types plus runtime
//! [`LinearPredPhi`], [`CemPlanner`], and [`WelfordSigRegMonitor`] that satisfy
//! [`weftos_worldmodel_core`] traits without candle or model files. Optional
//! `candle` feature exposes a ViT-tiny / AdaLN **skeleton** (layout +
//! `Unavailable` fallbacks); trained weights remain residual work — see crate
//! `README.md`.
//!
//! # Modules
//!
//! - [`action_encoder`] — control intent → fixed-width [`Action`] code
//! - [`encoder`] — sensor → latent (null / hash stubs; candle ViT skeleton)
//! - [`predictor`] — `pred_φ` ([`LinearPredPhi`]) + stubs + AdaLN skeleton
//! - [`planner`] — CEM default / MPPI-warm / gradient planners (WEFT-529)
//! - [`lattice`] — composed [`StubLattice`] implementing [`LatticeApi`]
//! - [`sigreg`] — Welford SIGReg monitor + auto-rollback (WEFT-528)
//! - [`rollback_gate`] — four-condition AND promotion gate (WEFT-530)
//! - [`training`] — offline edge + streaming-merge surfaces; RVF hot-swap (WEFT-531 / WEFT-532)
//! - [`candle`] — optional ML skeleton (`feature = "candle"`)
//!
//! Designed for `no_std` + `alloc` on the default feature set. The `candle`
//! feature implies `std`.

#![cfg_attr(not(feature = "std"), no_std)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![deny(missing_docs)]

extern crate alloc;

pub mod action_encoder;
pub mod encoder;
pub mod eval_harness;
pub mod lattice;
mod math_no_std;
pub mod planner;
pub mod predictor;
pub mod rollback_gate;
pub mod sigreg;
pub mod training;

#[cfg(feature = "candle")]
#[cfg_attr(docsrs, doc(cfg(feature = "candle")))]
pub mod candle;

// ── Re-exports from core (facade-friendly surface) ─────────────────────────
pub use weftos_worldmodel_core::{
    zero_latent, Action, ActionPlan, Encoder, GateCondition, GateVerdict, HotSwapOutcome, Latent,
    LatentPlanner, LatticeApi, ModelCheckpoint, ModelHotSwap, NodeId, ObservationFrame,
    OfflineEdgeTrainer, PlanStep, PlannerKind, Predictor, RecallHit, RollbackGate,
    RollbackGateMetrics, SensorClass, SigRegHealth, SigRegMonitor, SmallModelKind,
    StreamingMergeTrainer, StreamingTrainResult, SubscriptionId, TrainingSample,
    TrainingSurfaceKind, WorldModelError, WorldModelResult, EVENT_KIND_MODEL_HOT_SWAP,
    EVENT_KIND_ROLLBACK_GATE, EVENT_KIND_TRAINING_CYCLE, GATE_HELD_OUT_PROBE_MIN,
    GATE_SIGREG_HEALTH_MIN, GATE_TEMPORAL_STRAIGHTEN_MIN, GATE_VOE_DIFF_MIN,
    LEWM_MODEL_SEGMENT_DOMAIN_END, LEWM_MODEL_SEGMENT_DOMAIN_START, LATENT_DIM, LATENT_DIM_U16,
    SIGREG_HEALTH_ROLLBACK_THRESHOLD, SIGREG_HEALTH_WINDOW_SECS,
};

// ── Crate-local stubs + production runtime monitors/planners ───────────────
pub use action_encoder::{ActionEncoder, HashActionEncoder, NullActionEncoder, ACTION_CODE_DIM};
pub use encoder::{HashEncoder, NullEncoder};
pub use lattice::StubLattice;
pub use planner::{
    planner_for_kind, CemPlanner, GradientPlanner, MppiWarmPlanner, NullPlanner,
};
pub use predictor::{IdentityPredictor, LinearPredPhi, NullPredictor, PredPhi};
pub use rollback_gate::{
    FourConditionRollbackGate, ProbePair, RollbackGateLog, DEFAULT_MIN_SAMPLES,
    DEFAULT_PROBE_CAPACITY, DEFAULT_TRAJECTORY_CAPACITY,
};
pub use sigreg::{
    NullSigRegMonitor, SigRegHealthEvent, SigRegHealthLog, WelfordSigRegMonitor,
    EVENT_KIND_SIGREG_HEALTH,
};
pub use training::{
    decode_model_segment, encode_model_segment, model_segment_from_wire, model_segment_to_wire,
    require_promoted, ImportanceReplayBuffer, LewmModelSegment, OfflineEdgePipeline,
    SensorModelRegistry, SlotState, StreamingMergePipeline, DEFAULT_BOOT_VERSION,
    DEFAULT_REPLAY_CAPACITY, SEGMENT_HEADER_LEN,
};

#[cfg(feature = "candle")]
pub use candle::{candle_cpu_device, AdaLnPredictor, CandleVitEncoder, VitTinyConfig};

#[cfg(test)]
mod tests {
    use super::*;
    use weftos_worldmodel_core::LATTICE_METHOD_COUNT;

    #[test]
    fn null_encoder_predictor_planner_satisfy_traits() {
        let enc = NullEncoder;
        let pred = NullPredictor;
        let planner = NullPlanner::default();
        let action_enc = NullActionEncoder;

        let z = enc.encode(b"sensor").expect("encode");
        assert_eq!(z, zero_latent());
        assert_eq!(enc.latent_dim(), LATENT_DIM);

        let action = action_enc.encode_bytes(b"").expect("action");
        assert_eq!(action, Action::null());

        let z_hat = pred.predict(&z, &action).expect("predict");
        assert_eq!(z_hat, zero_latent());

        let plan = planner.plan(&z, 4).expect("plan");
        assert_eq!(plan.steps.len(), 4);
        assert_eq!(planner.kind(), PlannerKind::Cem);
    }

    #[test]
    fn hash_encoder_is_deterministic_and_non_zero() {
        let enc = HashEncoder;
        let a = enc.encode(b"frame-a").expect("a");
        let b = enc.encode(b"frame-a").expect("a2");
        let c = enc.encode(b"frame-b").expect("b");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, zero_latent());
        assert_eq!(enc.latent_dim(), LATENT_DIM);
    }

    #[test]
    fn hash_action_encoder_fills_code() {
        let enc = HashActionEncoder;
        let a = enc.encode_bytes(b"steer-left").expect("act");
        assert_ne!(a, Action::null());
        assert_eq!(a.code.len(), ACTION_CODE_DIM);
    }

    #[test]
    fn stub_lattice_seven_methods_smoke() {
        assert_eq!(LATTICE_METHOD_COUNT, 7);
        let mut api = StubLattice::default();
        let frame = ObservationFrame {
            bytes: b"frame",
            latent_dim: LATENT_DIM_U16,
            timestamp_ms: 0,
        };
        let z = api.observe(frame).expect("observe");
        assert_eq!(z.len(), LATENT_DIM);
        let z2 = api
            .observe_node(
                NodeId(1),
                ObservationFrame {
                    bytes: b"n",
                    latent_dim: LATENT_DIM_U16,
                    timestamp_ms: 1,
                },
            )
            .expect("observe_node");
        assert_eq!(z2.len(), LATENT_DIM);
        assert!(api.predict(&z, &Action::null()).is_ok());
        assert!(api.plan(&z, 2).is_ok());
        assert!(api.recall(&z, 3).expect("recall").is_empty());
        assert!(api.subscribe_surprise(SubscriptionId(0)).is_ok());
        assert!(api.subscribe_drift(SubscriptionId(1)).is_ok());
    }

    #[test]
    fn welford_sigreg_healthy_on_unit_samples() {
        let mut mon = WelfordSigRegMonitor::new(1);
        mon.min_samples = 4;
        for t in 0..20u64 {
            let mut z = zero_latent();
            for (i, zi) in z.iter_mut().enumerate() {
                *zi = if (t as usize + i).is_multiple_of(2) {
                    1.0
                } else {
                    -1.0
                };
            }
            let h = mon.update_at(&z, t * 1000).expect("update");
            if t >= 4 {
                assert!(h.is_healthy(), "score={}", h.score);
            }
        }
        assert!(!mon.health().should_rollback());
    }

    #[test]
    fn identity_predictor_returns_input() {
        let mut z = zero_latent();
        z[0] = 1.5;
        z[191] = -0.25;
        let out = IdentityPredictor
            .predict(&z, &Action::null())
            .expect("predict");
        assert_eq!(out, z);
    }

    #[test]
    fn linear_pred_phi_and_cem_smoke() {
        let pred = LinearPredPhi::default();
        let mut a = Action::null();
        a.code[0] = 0.25;
        let z = zero_latent();
        let z_hat = pred.predict(&z, &a).expect("predict");
        assert!((z_hat[0] - 0.25).abs() < 1e-5);

        let plan = CemPlanner::with_seed(1).plan(&z, 2).expect("plan");
        assert_eq!(plan.steps.len(), 2);
        assert_eq!(plan.kind, PlannerKind::Cem);
    }

    #[test]
    fn observation_dim_mismatch_rejected() {
        let mut api = StubLattice::default();
        let bad = ObservationFrame {
            bytes: b"",
            latent_dim: 64,
            timestamp_ms: 0,
        };
        assert!(matches!(
            api.observe(bad),
            Err(WorldModelError::LatentDimMismatch { got: 64, .. })
        ));
    }
}
