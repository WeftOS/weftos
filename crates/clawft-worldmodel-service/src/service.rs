//! [`WorldModelService`]: frame processing, planner loop and chain logging.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use weftos_worldmodel::{
    attest_frame, Action, ActionEncoder, ActionPlan, AttestationError, ChainSink,
    DefaultWorldModel, Encoder as _, GateVerdict, HashActionEncoder, HashEncoder, Latent,
    LatentPlanner, LatticeApi, MemoryChainSink, NullActionEncoder, ObservationFrame,
    ObservationTuple, SigRegHealth, EVENT_KIND_ROLLBACK_GATE,
    EVENT_KIND_SIGREG_HEALTH, LATENT_DIM_U16, SIGREG_HEALTH_ROLLBACK_THRESHOLD,
    SIGREG_HEALTH_WINDOW_SECS,
};

use crate::peers::PeerLatents;
use crate::peers::PeerError;
use crate::topology::{DeploymentTopology, ServiceConfig, ServiceRole};

/// Background planner cadence for the CEM / MPPI loop (Hz).
pub const PLANNER_HZ: f32 = 10.0;

/// Period between planner ticks in milliseconds.
pub const PLANNER_PERIOD_MS: u64 = 100;

/// Running world-model service instance (in-process).
#[derive(Debug)]
pub struct WorldModelService {
    /// Active configuration.
    pub config: ServiceConfig,
    /// Default runtime world-model stack (facade).
    pub model: DefaultWorldModel,
    /// In-memory ExoChain attestation sink (WEFT-533 / WEFT-528 health).
    pub chain: MemoryChainSink,
    /// Frames processed since boot.
    pub frames: u64,
    /// Whether boot completed successfully.
    pub booted: bool,
    /// Last published action plan from the 10 Hz planner loop.
    pub last_plan: Option<ActionPlan>,
    /// Timestamp (ms) of the last planner tick.
    pub last_plan_ms: Option<u64>,
    /// Planner ticks completed since boot.
    pub plan_ticks: u64,
    /// SIGReg auto-rollback events observed.
    pub sigreg_rollbacks: u64,
    /// Four-condition AND-gate vetoes (promotion blocked).
    pub gate_vetoes: u64,
    /// Four-condition AND-gate promotions allowed.
    pub gate_promotes: u64,
    /// Last AND-gate verdict (if any frame processed).
    pub last_gate: Option<GateVerdict>,
    /// Highest primary chain sequence applied by standby replay.
    pub last_replayed_seq: Option<u64>,
    /// Peer latents received (PeerToPeer only; observation only).
    pub peers: PeerLatents,
    /// Distinct peers seen so far.
    pub peers_seen: u64,
    /// Most recent peer consensus latent fed to the lattice.
    pub last_consensus: Option<Latent>,
    /// Tuple of the most recent frame processed or replayed.
    pub last_tuple: Option<ObservationTuple>,
}

impl WorldModelService {
    /// Construct from config without booting.
    pub fn new(config: ServiceConfig) -> Result<Self, String> {
        config.validate()?;
        Ok(Self {
            config,
            model: DefaultWorldModel::new(),
            chain: MemoryChainSink::new(),
            frames: 0,
            booted: false,
            last_plan: None,
            last_plan_ms: None,
            plan_ticks: 0,
            sigreg_rollbacks: 0,
            gate_vetoes: 0,
            gate_promotes: 0,
            last_gate: None,
            last_replayed_seq: None,
            peers: PeerLatents::new(),
            peers_seen: 0,
            last_consensus: None,
            last_tuple: None,
        })
    }

    /// Boot the service: load facade, announce topology, ready for frames.
    pub fn boot(&mut self) -> Result<BootReport, String> {
        self.config.validate()?;
        // Touch the lattice so single-node mode proves the facade is live.
        self
            .model
            .lattice
            .subscribe_surprise(weftos_worldmodel::SubscriptionId(0))
            .map_err(|e| e.to_string())?;
        self.booted = true;
        Ok(BootReport {
            topology: self.config.topology,
            role: self.config.role,
            node_id: self.config.node_id.clone(),
            latent_dim: LATENT_DIM_U16,
            available: true,
        })
    }

    /// Process one observation frame: encode path via lattice, predict,
    /// update Welford SIGReg health, form `(a_t, z_t, z_{t+1}, surprise)`,
    /// attest to the chain sink.
    pub fn process_frame(
        &mut self,
        sensor_bytes: &[u8],
        action: Action,
        timestamp_ms: u64,
    ) -> Result<FrameResult, ServiceError> {
        if !self.booted {
            return Err(ServiceError::NotBooted);
        }
        // A standby only replays; a peer or primary owns its local loop.
        if self.config.role == ServiceRole::Standby {
            return Err(ServiceError::NotPrimary);
        }

        let z_t = self
            .model
            .lattice
            .observe(ObservationFrame {
                bytes: sensor_bytes,
                latent_dim: LATENT_DIM_U16,
                timestamp_ms,
            })
            .map_err(ServiceError::WorldModel)?;

        // WEFT-528: online Welford SIGReg health + optional auto-rollback log.
        let health = self
            .model
            .sigreg
            .update_at(&z_t, timestamp_ms)
            .map_err(ServiceError::WorldModel)?;
        self.log_sigreg_health(health)?;

        let z_hat = self
            .model
            .predict(&z_t, &action)
            .map_err(ServiceError::WorldModel)?;

        // Next observation: for the smoke path we re-encode the same bytes
        // (null encoder → zero latent) or use a hash encoder path when the
        // lattice last state is already set. Use hash of next-frame marker
        // for non-zero transitions when bytes change.
        let next_bytes = sensor_bytes;
        let z_tp1 = self
            .model
            .encoder
            .encode(next_bytes)
            .map_err(ServiceError::WorldModel)?;

        // WEFT-530: four-condition AND gate for streaming-merge promotion.
        let gate = self.model.evaluate_transition(
            &z_t,
            &z_hat,
            &z_tp1,
            Some(health.score),
        );
        self.last_gate = Some(gate);
        if gate.promote {
            self.gate_promotes = self.gate_promotes.saturating_add(1);
        } else {
            self.gate_vetoes = self.gate_vetoes.saturating_add(1);
        }
        self.log_rollback_gate(gate)?;

        let frame_seq = self.frames;
        let tuple = ObservationTuple::new(action, z_t, z_hat, z_tp1, timestamp_ms, frame_seq);
        let chain_seq = attest_frame(&mut self.chain, &tuple).map_err(ServiceError::Attestation)?;
        self.last_tuple = Some(tuple);
        self.frames = self.frames.saturating_add(1);

        Ok(FrameResult {
            frame_seq,
            chain_seq,
            surprise: tuple.surprise,
            manifold_major: tuple.manifold.major,
            latent_dim: tuple.latent_dim,
            sigreg_health: health.score,
            sigreg_version: health.version_tag,
            gate_promote: gate.promote,
            gate_sigreg: gate.metrics.sigreg_health,
            gate_probe: gate.metrics.held_out_probe_accuracy,
            gate_voe_diff: gate.metrics.voe_surprise_diff,
            gate_straighten: gate.metrics.temporal_straightening,
        })
    }

    /// Whether a streaming-merge checkpoint may be promoted (AND of four conditions).
    pub fn may_promote_checkpoint(&self) -> bool {
        self.model.may_promote_checkpoint()
    }

    /// Log SIGReg health to the ExoChain sink (metrics + rollback gate).
    pub(crate) fn log_sigreg_health(&mut self, health: SigRegHealth) -> Result<(), ServiceError> {
        let rolled = self.model.sigreg.last_rolled_back();
        if rolled {
            self.sigreg_rollbacks = self.sigreg_rollbacks.saturating_add(1);
        }
        // Log on rollback or every sample once warm — keep payload tiny JSON.
        let should_log = rolled || self.model.sigreg.sample_count().is_multiple_of(8);
        if !should_log {
            return Ok(());
        }
        let payload = serde_json::json!({
            "score": health.score,
            "seconds_below_threshold": health.seconds_below_threshold,
            "version_tag": health.version_tag,
            "rolled_back": rolled,
            "threshold": SIGREG_HEALTH_ROLLBACK_THRESHOLD,
            "window_secs": SIGREG_HEALTH_WINDOW_SECS,
            "sample_count": self.model.sigreg.sample_count(),
        });
        let bytes = serde_json::to_vec(&payload).map_err(|e| {
            ServiceError::Attestation(AttestationError::Serialize(e.to_string()))
        })?;
        self.chain
            .append_event(EVENT_KIND_SIGREG_HEALTH, &bytes)
            .map_err(ServiceError::Attestation)?;
        Ok(())
    }

    /// Log four-condition AND gate evaluation to the ExoChain sink (WEFT-530).
    fn log_rollback_gate(&mut self, gate: GateVerdict) -> Result<(), ServiceError> {
        // Always log vetoes; sample promotes to keep the chain compact.
        let should_log = !gate.promote || self.frames.is_multiple_of(8);
        if !should_log {
            return Ok(());
        }
        let first_veto = gate.first_veto().map(|c| c.as_str());
        let payload = serde_json::json!({
            "promote": gate.promote,
            "sigreg_health": gate.metrics.sigreg_health,
            "held_out_probe_accuracy": gate.metrics.held_out_probe_accuracy,
            "voe_surprise_diff": gate.metrics.voe_surprise_diff,
            "temporal_straightening": gate.metrics.temporal_straightening,
            "first_veto": first_veto,
            "sigreg_ok": gate.sigreg_ok,
            "probe_ok": gate.probe_ok,
            "voe_ok": gate.voe_ok,
            "straighten_ok": gate.straighten_ok,
        });
        let bytes = serde_json::to_vec(&payload).map_err(|e| {
            ServiceError::Attestation(AttestationError::Serialize(e.to_string()))
        })?;
        self.chain
            .append_event(EVENT_KIND_ROLLBACK_GATE, &bytes)
            .map_err(ServiceError::Attestation)?;
        Ok(())
    }

    /// Run one 10 Hz planner tick if `timestamp_ms` is due.
    ///
    /// Returns `Some(plan)` when a new plan was computed; `None` if the
    /// period has not elapsed since the last tick.
    pub fn maybe_plan_tick(
        &mut self,
        z_t: &Latent,
        timestamp_ms: u64,
        horizon: usize,
    ) -> Result<Option<ActionPlan>, ServiceError> {
        if !self.booted {
            return Err(ServiceError::NotBooted);
        }
        if let Some(last) = self.last_plan_ms
            && timestamp_ms.saturating_sub(last) < PLANNER_PERIOD_MS {
                return Ok(None);
            }
        let plan = self
            .model
            .planner
            .plan(z_t, horizon)
            .map_err(ServiceError::WorldModel)?;
        self.last_plan = Some(plan.clone());
        self.last_plan_ms = Some(timestamp_ms);
        self.plan_ticks = self.plan_ticks.saturating_add(1);
        Ok(Some(plan))
    }

    /// Force a planner tick regardless of cadence (tests / CLI).
    pub fn plan_now(&mut self, z_t: &Latent, horizon: usize) -> Result<ActionPlan, ServiceError> {
        if !self.booted {
            return Err(ServiceError::NotBooted);
        }
        let plan = self
            .model
            .planner
            .plan(z_t, horizon)
            .map_err(ServiceError::WorldModel)?;
        self.last_plan = Some(plan.clone());
        self.last_plan_ms = Some(now_ms());
        self.plan_ticks = self.plan_ticks.saturating_add(1);
        Ok(plan)
    }

    /// Simulate `n` background planner ticks at 10 Hz starting at `start_ms`.
    pub fn run_planner_loop(
        &mut self,
        z_t: &Latent,
        n: u64,
        horizon: usize,
        start_ms: u64,
    ) -> Result<Vec<ActionPlan>, ServiceError> {
        let mut out = Vec::with_capacity(n as usize);
        for i in 0..n {
            let ts = start_ms.saturating_add(i.saturating_mul(PLANNER_PERIOD_MS));
            if let Some(plan) = self.maybe_plan_tick(z_t, ts, horizon)? {
                out.push(plan);
            }
        }
        Ok(out)
    }

    /// Run `n` frames through a deterministic fake sensor pipeline.
    ///
    /// Uses [`HashEncoder`] / [`HashActionEncoder`] for non-trivial latents
    /// when `use_hash` is true; otherwise null stubs (zero latent, zero
    /// surprise).
    pub fn run_fake_sensor_pipeline(
        &mut self,
        n: u64,
        use_hash: bool,
    ) -> Result<Vec<FrameResult>, ServiceError> {
        if !self.booted {
            return Err(ServiceError::NotBooted);
        }
        let hash_enc = HashEncoder;
        let hash_act = HashActionEncoder;
        let null_act = NullActionEncoder;
        let mut out = Vec::with_capacity(n as usize);
        let base_ms = now_ms();

        for i in 0..n {
            let payload = format!("fake-sensor-frame-{i}");
            let bytes = payload.as_bytes();
            let action = if use_hash {
                hash_act
                    .encode_bytes(bytes)
                    .map_err(ServiceError::WorldModel)?
            } else {
                null_act
                    .encode_bytes(b"")
                    .map_err(ServiceError::WorldModel)?
            };

            // When using hash, drive lattice observe with hash-encoded path
            // by feeding distinct bytes; predictor is still the service stub.
            if use_hash {
                // Override model encoder path: observe via lattice (null
                // encoder inside StubLattice) then replace z with hash for
                // attestation richness — process_frame uses lattice observe.
                let _ = hash_enc.encode(bytes).map_err(ServiceError::WorldModel)?;
            }

            let r = self.process_frame(bytes, action, base_ms.saturating_add(i))?;
            out.push(r);
        }
        Ok(out)
    }
}

/// Boot announcement returned by [`WorldModelService::boot`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootReport {
    /// Selected topology.
    pub topology: DeploymentTopology,
    /// Service role.
    pub role: ServiceRole,
    /// Node identifier.
    pub node_id: String,
    /// SIGReg latent width.
    pub latent_dim: u16,
    /// Service is available (false would map to ServiceUnavailable contract).
    pub available: bool,
}

/// Result of processing one frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrameResult {
    /// Local frame sequence.
    pub frame_seq: u64,
    /// Chain sequence from the attestation sink.
    pub chain_seq: u64,
    /// VoE surprise for the frame.
    pub surprise: f32,
    /// Manifold major version attested.
    pub manifold_major: u16,
    /// Latent dim attested.
    pub latent_dim: u16,
    /// SIGReg health score after this frame (WEFT-528).
    pub sigreg_health: f32,
    /// SIGReg version tag after this frame.
    pub sigreg_version: u64,
    /// Four-condition AND gate: promotion allowed (WEFT-530).
    pub gate_promote: bool,
    /// Gate metric: cluster SIGReg health.
    pub gate_sigreg: f32,
    /// Gate metric: held-out probe accuracy.
    pub gate_probe: f32,
    /// Gate metric: VoE surprise differentiation.
    pub gate_voe_diff: f32,
    /// Gate metric: temporal straightening.
    pub gate_straighten: f32,
}

/// Service-level errors.
#[derive(Debug)]
pub enum ServiceError {
    /// [`WorldModelService::boot`] has not been called.
    NotBooted,
    /// World-model facade error.
    WorldModel(weftos_worldmodel::WorldModelError),
    /// Attestation / chain sink error.
    Attestation(AttestationError),
    /// Operation needs the primary (or a peer) role; this node is a standby.
    NotPrimary,
    /// Operation needs the standby role (replay, promotion).
    NotStandby,
    /// Operation is not valid for the configured topology or role change.
    Topology(String),
    /// Peer latent rejected (PeerToPeer observation path).
    Peer(PeerError),
    /// Replayed chain entry could not be decoded.
    Replay(String),
}

impl core::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotBooted => write!(f, "world-model service not booted"),
            Self::WorldModel(e) => write!(f, "world model: {e}"),
            Self::Attestation(e) => write!(f, "attestation: {e}"),
            Self::NotPrimary => write!(f, "node is a standby, not primary; frame refused"),
            Self::NotStandby => write!(f, "operation requires the standby role"),
            Self::Topology(m) => write!(f, "topology: {m}"),
            Self::Peer(e) => write!(f, "peer: {e}"),
            Self::Replay(m) => write!(f, "replay: {m}"),
        }
    }
}

impl std::error::Error for ServiceError {}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
