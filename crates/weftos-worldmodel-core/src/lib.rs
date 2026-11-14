//! `weftos-worldmodel-core` — LeWM latent world-model **traits** and constants.
//!
//! First crate of the LeWM 7-crate batch (WEFT-520). Defines the no_std surface
//! that impls (`weftos-worldmodel-impls`, WEFT-521) and the facade
//! (`weftos-worldmodel`, WEFT-522) satisfy.
//!
//! # Latent contract (WEFT-543)
//!
//! SIGReg manifold is isotropic Gaussian `N(0, I)` in **192** dimensions for
//! `mesh.sensor.v1`. Changing width requires a wire major-version bump.
//!
//! # Decoupling (ADR-090 / WEFT-519)
//!
//! This crate does **not** depend on `clawft-kernel`. Kernel injection seams
//! must call `clawft_kernel::lewm_invariant` at the facade boundary. Traits
//! here only model perception / prediction / planning / lattice query APIs.
//!
//! # Modules
//!
//! - [`latent`] — `LATENT_DIM`, [`Latent`], version helpers
//! - [`encoder`] — [`Encoder`] (sensor → latent)
//! - [`predictor`] — [`Predictor`] (`pred_φ`: z_t, a_t → ẑ_{t+1})
//! - [`planner`] — [`LatentPlanner`] (CEM default, 10 Hz)
//! - [`lattice`] — [`LatticeApi`] (7 methods)
//! - [`sigreg`] — health thresholds and monitor trait
//! - [`rollback_gate`] — four-condition AND promotion / rollback gate (WEFT-530)
//! - [`training`] — two training surfaces + RVF small-model contracts (WEFT-531 / WEFT-532)
//! - [`types`] — shared observation / action / plan types
//! - [`error`] — portable error kinds
//!
//! Designed for `no_std` + `alloc`. Enable `std` on host builds if desired.

#![cfg_attr(not(feature = "std"), no_std)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![deny(missing_docs)]

extern crate alloc;

pub mod attestation;
pub mod encoder;
pub mod error;
pub mod lattice;
pub mod latent;
pub mod planner;
pub mod predictor;
pub mod rollback_gate;
pub mod sigreg;
pub mod training;
pub mod types;

pub use attestation::{
    surprise_voe, ObservationTuple, ATTESTATION_SOURCE, EVENT_KIND_LEWM_FRAME_ATTESTATION,
};
pub use encoder::Encoder;
pub use error::{WorldModelError, WorldModelResult};
pub use lattice::{LatticeApi, LatticeMethod, LATTICE_METHOD_COUNT, LATTICE_METHODS};
pub use latent::{
    latent_dim_matches_v1, zero_latent, Latent, LatentVersion, LATENT_DIM, LATENT_DIM_U16,
    LATENT_SCHEMA_MAJOR_V1,
};
pub use planner::{ActionPlan, LatentPlanner, PlanStep, PlannerKind};
pub use predictor::Predictor;
pub use rollback_gate::{
    GateCondition, GateVerdict, RollbackGate, RollbackGateMetrics, EVENT_KIND_ROLLBACK_GATE,
    GATE_HELD_OUT_PROBE_MIN, GATE_SIGREG_HEALTH_MIN, GATE_TEMPORAL_STRAIGHTEN_MIN,
    GATE_VOE_DIFF_MIN,
};
pub use sigreg::{
    SigRegHealth, SigRegMonitor, SIGREG_HEALTH_ROLLBACK_THRESHOLD, SIGREG_HEALTH_WINDOW_SECS,
};
pub use training::{
    fnv1a64, HotSwapOutcome, ModelCheckpoint, ModelHotSwap, OfflineEdgeTrainer, SensorClass,
    SmallModelKind, StreamingMergeTrainer, StreamingTrainResult, TrainingSample,
    TrainingSurfaceKind, EVENT_KIND_MODEL_HOT_SWAP, EVENT_KIND_TRAINING_CYCLE,
    LEWM_MODEL_SEGMENT_CODEC_JSON, LEWM_MODEL_SEGMENT_CODEC_RAW,
    LEWM_MODEL_SEGMENT_CODEC_SAFETENSORS, LEWM_MODEL_SEGMENT_DOMAIN_END,
    LEWM_MODEL_SEGMENT_DOMAIN_START,
};
pub use types::{Action, NodeId, ObservationFrame, RecallHit, SubscriptionId};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoder::NullEncoder;
    use crate::lattice::NullLattice;
    use crate::planner::NullPlanner;
    use crate::predictor::NullPredictor;
    use crate::sigreg::NullSigRegMonitor;

    #[test]
    fn latent_dim_is_192() {
        assert_eq!(LATENT_DIM, 192);
        assert_eq!(LATENT_DIM_U16, 192);
        assert_eq!(zero_latent().len(), 192);
        assert!(latent_dim_matches_v1(192));
        assert!(!latent_dim_matches_v1(128));
    }

    #[test]
    fn null_encoder_predictor_planner_roundtrip() {
        let enc = NullEncoder;
        let pred = NullPredictor;
        let planner = NullPlanner::default();

        let z = enc.encode(b"sensor").expect("encode");
        assert_eq!(z, zero_latent());
        assert_eq!(enc.latent_dim(), LATENT_DIM);

        let action = Action::null();
        let z_hat = pred.predict(&z, &action).expect("predict");
        assert_eq!(z_hat, zero_latent());

        let plan = planner.plan(&z, 4).expect("plan");
        assert_eq!(plan.steps.len(), 4);
        assert_eq!(planner.kind(), PlannerKind::Cem);
    }

    #[test]
    fn lattice_seven_methods_named() {
        assert_eq!(LATTICE_METHOD_COUNT, 7);
        assert_eq!(LATTICE_METHODS.len(), 7);
        assert_eq!(LatticeMethod::Observe.as_str(), "observe");
        assert_eq!(LatticeMethod::SubscribeDrift.as_str(), "subscribe_drift");
    }

    #[test]
    fn null_lattice_smoke() {
        let mut api = NullLattice::default();
        let frame = ObservationFrame {
            bytes: b"frame",
            latent_dim: LATENT_DIM_U16,
            timestamp_ms: 0,
        };
        let z = api.observe(frame).expect("observe");
        assert_eq!(z, zero_latent());
        let z2 = api
            .observe_node(NodeId(1), ObservationFrame {
                bytes: b"n",
                latent_dim: LATENT_DIM_U16,
                timestamp_ms: 1,
            })
            .expect("observe_node");
        assert_eq!(z2, zero_latent());
        assert!(api.predict(&z, &Action::null()).is_ok());
        assert!(api.plan(&z, 2).is_ok());
        assert!(api.recall(&z, 3).expect("recall").is_empty());
        assert!(api.subscribe_surprise(SubscriptionId(0)).is_ok());
        assert!(api.subscribe_drift(SubscriptionId(1)).is_ok());
    }

    #[test]
    fn sigreg_thresholds_match_research_stream() {
        assert!((SIGREG_HEALTH_ROLLBACK_THRESHOLD - 0.85).abs() < f32::EPSILON);
        assert_eq!(SIGREG_HEALTH_WINDOW_SECS, 30);
        let mut mon = NullSigRegMonitor::default();
        let h = mon.update(&zero_latent()).expect("update");
        assert!(h.is_healthy());
        assert!(!h.should_rollback());
    }

    #[test]
    fn world_model_error_display() {
        let e = WorldModelError::LatentDimMismatch {
            expected: 192,
            got: 64,
        };
        let s = alloc::format!("{e}");
        assert!(s.contains("192"));
        assert!(s.contains("64"));
    }
}
