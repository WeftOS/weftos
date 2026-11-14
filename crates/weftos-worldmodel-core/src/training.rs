//! Training surfaces and RVF-hosted small-model contracts (WEFT-531 / WEFT-532).
//!
//! ADR diagram / research stream contracts:
//!
//! - **Two training surfaces** (T30 / D33):
//!   1. **Offline edge intelligence** — per-sensor-class, RVF-delivered,
//!      hot-swapped at tick alignment.
//!   2. **Online streaming-merge** — in-service trainer with per-class
//!      importance-weighted replay, gated by the four-condition AND rollback
//!      gate ([`crate::rollback_gate`]).
//! - **Three small models per sensor class** (T31 / D37):
//!   transmit-gate / aggregate / encode, RVF-hosted, tick-aligned hot-swap,
//!   auto-rollback on gate veto.
//!
//! Full neural training (ViT-tiny / AdaLN) remains residual; these traits
//! define the host surface so impls can ship stub trainers + segment I/O
//! and later swap in real optimisers.

use crate::error::WorldModelResult;
use crate::latent::Latent;
use crate::rollback_gate::GateVerdict;
use crate::types::Action;

// ── Sensor class + small-model kinds ───────────────────────────────────────

/// Sensor classes that host edge-intelligence small models.
///
/// Classes 0-5 match the sensor plane in the LeWM diagram (RGB-D, IMU,
/// proprio, LiDAR, audio, plus a generic / unknown bucket for future
/// classes). Classes 6-10 follow the ADR-111 / ADR-384 HAL mapping (plan
/// `docs/plans/world-model-data-and-training.md` section 1a): mmWave radar
/// carries `radar_range` and `radar_track_point`, UWB carries `uwb_echo` and
/// `radio_delay`, ToF carries `tof_depth`, CSI carries `rf_gaussian` and
/// `rf_link_observation`, and Reading carries SenML readings (ADR-111 s2).
/// Discriminants are wire-stable and only ever appended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
#[repr(u8)]
pub enum SensorClass {
    /// Unclassified / multi-class residual.
    #[default]
    Generic = 0,
    /// RGB-D camera.
    RgbD = 1,
    /// Inertial measurement unit.
    Imu = 2,
    /// Proprioception / joint state.
    Proprio = 3,
    /// LiDAR / depth clouds.
    Lidar = 4,
    /// Audio / microphone.
    Audio = 5,
    /// mmWave radar (LD2450 / LD6002): `radar_range`, `radar_track_point`.
    MmWave = 6,
    /// Ultra-wideband ranging: `uwb_echo`, `radio_delay`.
    Uwb = 7,
    /// Time-of-flight depth: `tof_depth`.
    Tof = 8,
    /// Wi-Fi channel state information: `rf_gaussian`, `rf_link_observation`.
    Csi = 9,
    /// SenML readings per ADR-111 section 2.
    Reading = 10,
}

impl SensorClass {
    /// Stable wire / log name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Generic => "generic",
            Self::RgbD => "rgb_d",
            Self::Imu => "imu",
            Self::Proprio => "proprio",
            Self::Lidar => "lidar",
            Self::Audio => "audio",
            Self::MmWave => "mmwave",
            Self::Uwb => "uwb",
            Self::Tof => "tof",
            Self::Csi => "csi",
            Self::Reading => "reading",
        }
    }

    /// ADR-111 record type names this class carries (empty when the class
    /// has no mapped evidence type).
    pub const fn evidence_types(self) -> &'static [&'static str] {
        match self {
            Self::Imu => &["imu_event"],
            Self::Audio => &["acoustic_range"],
            Self::MmWave => &["radar_range", "radar_track_point"],
            Self::Uwb => &["uwb_echo", "radio_delay"],
            Self::Tof => &["tof_depth"],
            Self::Csi => &["rf_gaussian", "rf_link_observation"],
            Self::Generic | Self::RgbD | Self::Proprio | Self::Lidar | Self::Reading => &[],
        }
    }

    /// All named classes (tests / docs).
    pub const ALL: [SensorClass; 11] = [
        Self::Generic,
        Self::RgbD,
        Self::Imu,
        Self::Proprio,
        Self::Lidar,
        Self::Audio,
        Self::MmWave,
        Self::Uwb,
        Self::Tof,
        Self::Csi,
        Self::Reading,
    ];
}

impl TryFrom<u8> for SensorClass {
    type Error = u8;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Generic),
            1 => Ok(Self::RgbD),
            2 => Ok(Self::Imu),
            3 => Ok(Self::Proprio),
            4 => Ok(Self::Lidar),
            5 => Ok(Self::Audio),
            6 => Ok(Self::MmWave),
            7 => Ok(Self::Uwb),
            8 => Ok(Self::Tof),
            9 => Ok(Self::Csi),
            10 => Ok(Self::Reading),
            other => Err(other),
        }
    }
}

/// Three trainable RVF-hosted small models per sensor class (WEFT-532).
///
/// Discriminants occupy the LeWM RVF segment domain `0x60`–`0x6F`
/// (after ECC `0x50`–`0x5F` and context-router `0x40`–`0x42`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
#[repr(u8)]
pub enum SmallModelKind {
    /// Collect-stage transmit gate (novelty / quality / salience / summary).
    TransmitGate = 0x60,
    /// Multi-sensor fusion / aggregate stage.
    Aggregate = 0x61,
    /// SIGReg-manifold encoder stage (ViT-tiny path when weights exist).
    Encode = 0x62,
}

impl SmallModelKind {
    /// Stable wire / log name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TransmitGate => "transmit_gate",
            Self::Aggregate => "aggregate",
            Self::Encode => "encode",
        }
    }

    /// RVF segment type byte.
    pub const fn segment_type(self) -> u8 {
        self as u8
    }

    /// All three model variants.
    pub const ALL: [SmallModelKind; 3] =
        [Self::TransmitGate, Self::Aggregate, Self::Encode];
}

impl TryFrom<u8> for SmallModelKind {
    type Error = u8;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x60 => Ok(Self::TransmitGate),
            0x61 => Ok(Self::Aggregate),
            0x62 => Ok(Self::Encode),
            other => Err(other),
        }
    }
}

/// Start of the LeWM small-model RVF segment domain.
pub const LEWM_MODEL_SEGMENT_DOMAIN_START: u8 = 0x60;
/// Inclusive end of the LeWM small-model RVF segment domain.
pub const LEWM_MODEL_SEGMENT_DOMAIN_END: u8 = 0x6F;

/// JSON / wire codec byte for LeWM model segments (mirrors ECC JSON = `0x01`).
pub const LEWM_MODEL_SEGMENT_CODEC_JSON: u8 = 0x01;
/// Raw little-endian weight blob codec.
pub const LEWM_MODEL_SEGMENT_CODEC_RAW: u8 = 0x02;

// ── Training surface kinds ─────────────────────────────────────────────────

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

// ── Sample + checkpoint ────────────────────────────────────────────────────

/// One transition sample used by either training surface.
///
/// ML trainers may ignore some fields; the stub path uses residual
/// statistics (`z_tp1 - z_t`) weighted by [`TrainingSample::importance`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrainingSample {
    /// Sensor class that produced the observation.
    pub sensor_class: SensorClass,
    /// Latent at step `t`.
    pub z_t: Latent,
    /// Observed latent at step `t+1`.
    pub z_tp1: Latent,
    /// Control / intervention applied between `t` and `t+1`.
    pub action: Action,
    /// VoE surprise for this transition (higher → more rare / informative).
    pub surprise: f32,
    /// Per-class importance weight for replay (must be `> 0` to be drawn).
    pub importance: f32,
    /// Observation timestamp (milliseconds).
    pub timestamp_ms: u64,
}

impl TrainingSample {
    /// Construct with default importance `1.0` and zero action.
    pub fn basic(
        sensor_class: SensorClass,
        z_t: Latent,
        z_tp1: Latent,
        timestamp_ms: u64,
    ) -> Self {
        Self {
            sensor_class,
            z_t,
            z_tp1,
            action: Action::null(),
            surprise: 0.0,
            importance: 1.0,
            timestamp_ms,
        }
    }

    /// Residual `z_tp1 - z_t` (used by stub trainers).
    pub fn residual(&self) -> Latent {
        let mut r = self.z_tp1;
        for (ri, zt) in r.iter_mut().zip(self.z_t.iter()) {
            *ri -= *zt;
        }
        r
    }
}

/// Versioned model checkpoint delivered via RVF or promoted in-service.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelCheckpoint {
    /// Sensor class this checkpoint targets.
    pub sensor_class: SensorClass,
    /// Which of the three small-model slots.
    pub kind: SmallModelKind,
    /// Monotonic version tag (ExoChain attestation).
    pub version_tag: u64,
    /// Training surface that produced the checkpoint.
    pub surface: TrainingSurfaceKind,
    /// Opaque weight blob (stub: packed f32 residual; later: real tensors).
    pub weights: alloc::vec::Vec<u8>,
    /// FNV-1a style digest of `weights` for integrity / equality.
    pub weight_digest: u64,
    /// Tick at which a hot-swap may commit (`None` = immediate / online).
    pub target_tick: Option<u64>,
}

impl ModelCheckpoint {
    /// Recompute [`Self::weight_digest`] from current weights.
    pub fn recompute_digest(&mut self) {
        self.weight_digest = fnv1a64(&self.weights);
    }

    /// Construct and digest in one step.
    pub fn new(
        sensor_class: SensorClass,
        kind: SmallModelKind,
        version_tag: u64,
        surface: TrainingSurfaceKind,
        weights: alloc::vec::Vec<u8>,
        target_tick: Option<u64>,
    ) -> Self {
        let weight_digest = fnv1a64(&weights);
        Self {
            sensor_class,
            kind,
            version_tag,
            surface,
            weights,
            weight_digest,
            target_tick,
        }
    }
}

/// FNV-1a 64-bit over bytes (no_std, no deps).
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x100_0000_01b3;
    let mut h = OFFSET;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(PRIME);
    }
    h
}

// ── Hot-swap result ────────────────────────────────────────────────────────

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::latent::zero_latent;

    #[test]
    fn small_model_segment_bytes_stable() {
        assert_eq!(SmallModelKind::TransmitGate as u8, 0x60);
        assert_eq!(SmallModelKind::Aggregate as u8, 0x61);
        assert_eq!(SmallModelKind::Encode as u8, 0x62);
        for k in SmallModelKind::ALL {
            assert_eq!(SmallModelKind::try_from(k as u8), Ok(k));
            assert!(k.segment_type() >= LEWM_MODEL_SEGMENT_DOMAIN_START);
            assert!(k.segment_type() <= LEWM_MODEL_SEGMENT_DOMAIN_END);
        }
    }

    #[test]
    fn sensor_class_roundtrip() {
        for c in SensorClass::ALL {
            assert_eq!(SensorClass::try_from(c as u8), Ok(c));
            assert!(!c.as_str().is_empty());
        }
        assert_eq!(SensorClass::try_from(99), Err(99));
        assert_eq!(SensorClass::try_from(11), Err(11));
    }

    #[test]
    fn sensor_class_all_is_eleven_and_dense() {
        assert_eq!(SensorClass::ALL.len(), 11);
        for (i, c) in SensorClass::ALL.iter().enumerate() {
            assert_eq!(*c as u8 as usize, i);
        }
    }

    #[test]
    fn sensor_class_names_unique() {
        let mut names: alloc::vec::Vec<&str> =
            SensorClass::ALL.iter().map(|c| c.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), SensorClass::ALL.len());
    }

    #[test]
    fn sensor_class_original_six_pinned() {
        let pinned = [
            (SensorClass::Generic, 0u8, "generic"),
            (SensorClass::RgbD, 1, "rgb_d"),
            (SensorClass::Imu, 2, "imu"),
            (SensorClass::Proprio, 3, "proprio"),
            (SensorClass::Lidar, 4, "lidar"),
            (SensorClass::Audio, 5, "audio"),
        ];
        for (c, d, n) in pinned {
            assert_eq!(c as u8, d);
            assert_eq!(c.as_str(), n);
        }
        let hal = [
            (SensorClass::MmWave, 6u8, "mmwave"),
            (SensorClass::Uwb, 7, "uwb"),
            (SensorClass::Tof, 8, "tof"),
            (SensorClass::Csi, 9, "csi"),
            (SensorClass::Reading, 10, "reading"),
        ];
        for (c, d, n) in hal {
            assert_eq!(c as u8, d);
            assert_eq!(c.as_str(), n);
        }
    }

    #[test]
    fn sensor_class_evidence_types() {
        assert!(SensorClass::Generic.evidence_types().is_empty());
        assert!(SensorClass::RgbD.evidence_types().is_empty());
        assert!(SensorClass::Proprio.evidence_types().is_empty());
        assert!(SensorClass::Lidar.evidence_types().is_empty());
        assert!(SensorClass::Reading.evidence_types().is_empty());
        assert_eq!(SensorClass::Imu.evidence_types(), &["imu_event"]);
        assert_eq!(SensorClass::Audio.evidence_types(), &["acoustic_range"]);
        assert_eq!(
            SensorClass::MmWave.evidence_types(),
            &["radar_range", "radar_track_point"]
        );
        assert_eq!(SensorClass::Uwb.evidence_types(), &["uwb_echo", "radio_delay"]);
        assert_eq!(SensorClass::Tof.evidence_types(), &["tof_depth"]);
        assert_eq!(
            SensorClass::Csi.evidence_types(),
            &["rf_gaussian", "rf_link_observation"]
        );
    }

    #[test]
    fn sample_residual_and_digest() {
        let mut z0 = zero_latent();
        let mut z1 = zero_latent();
        z0[0] = 1.0;
        z1[0] = 3.0;
        let s = TrainingSample::basic(SensorClass::Imu, z0, z1, 10);
        let r = s.residual();
        assert!((r[0] - 2.0).abs() < 1e-6);

        let mut cp = ModelCheckpoint::new(
            SensorClass::Imu,
            SmallModelKind::Encode,
            1,
            TrainingSurfaceKind::OfflineEdge,
            alloc::vec![1, 2, 3],
            Some(5),
        );
        let d = cp.weight_digest;
        cp.recompute_digest();
        assert_eq!(cp.weight_digest, d);
        assert_eq!(fnv1a64(&[1, 2, 3]), d);
    }

    #[test]
    fn surface_names() {
        assert_eq!(
            TrainingSurfaceKind::OfflineEdge.as_str(),
            "offline_edge"
        );
        assert_eq!(
            TrainingSurfaceKind::OnlineStreamingMerge.as_str(),
            "online_streaming_merge"
        );
        assert_eq!(EVENT_KIND_TRAINING_CYCLE, "lewm.training_cycle");
        assert_eq!(EVENT_KIND_MODEL_HOT_SWAP, "lewm.model_hot_swap");
    }
}
