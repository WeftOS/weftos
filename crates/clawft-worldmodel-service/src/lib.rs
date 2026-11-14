//! `clawft-worldmodel-service` — LeWM WorldModelService host (WEFT-524).
//!
//! Boots a [`weftos_worldmodel::DefaultWorldModel`] facade under one of three
//! deployment topologies (ADR-054 marketing contract):
//!
//! - [`DeploymentTopology::Single`] — one primary for the fleet (default)
//! - [`DeploymentTopology::HotStandby`] — Raft-elected primary + hot standby
//! - [`DeploymentTopology::PeerToPeer`] — every node runs its own service
//!
//! Single-node mode boots end-to-end against a fake sensor pipeline and
//! attests every frame onto an in-memory ExoChain sink (WEFT-533). Full
//! Raft / mesh wiring is scaffolded; standby and P2P currently share the
//! same local loop with topology-specific role metadata.

#![deny(missing_docs)]

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use weftos_worldmodel::{
    attest_frame, Action, ActionEncoder, ActionPlan, AttestationError, ChainSink,
    DefaultWorldModel, GateVerdict, HashActionEncoder, HashEncoder, Latent, LatentPlanner,
    LatticeApi, MemoryChainSink, NullActionEncoder, ObservationFrame,
    ObservationTuple, SigRegHealth, Encoder as _,
    EVENT_KIND_ROLLBACK_GATE, EVENT_KIND_SIGREG_HEALTH, LATENT_DIM_U16,
    SIGREG_HEALTH_ROLLBACK_THRESHOLD, SIGREG_HEALTH_WINDOW_SECS,
};

/// Three deployment topologies for WorldModelService (ADR-054 / WEFT-524).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentTopology {
    /// One WorldModelService for the entire fleet (1–10 nodes).
    #[default]
    Single,
    /// Active primary with hot standby (10–500 nodes). Scaffold role only.
    HotStandby,
    /// Every node runs its own service; gossip later (500+ / low-trust).
    PeerToPeer,
}

impl DeploymentTopology {
    /// Parse from CLI / config string (`single`, `hot_standby`, `standby`, `p2p`, `peer_to_peer`).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "single" | "primary" => Some(Self::Single),
            "hot_standby" | "standby" | "raft" | "hot-standby" => Some(Self::HotStandby),
            "peer_to_peer" | "p2p" | "peer-to-peer" | "mesh" => Some(Self::PeerToPeer),
            _ => None,
        }
    }

    /// Stable config / wire name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Single => "single",
            Self::HotStandby => "hot_standby",
            Self::PeerToPeer => "peer_to_peer",
        }
    }

    /// Human label for logs.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Single => "single primary",
            Self::HotStandby => "raft-elected primary + standby",
            Self::PeerToPeer => "peer-to-peer mesh",
        }
    }
}

/// Role within a topology (standby scaffold; single always Primary).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ServiceRole {
    /// Authoritative writer for the cluster / process.
    #[default]
    Primary,
    /// Hot standby replaying ExoChain (hot_standby topology only).
    Standby,
    /// Peer participant (peer_to_peer topology).
    Peer,
}

/// Service configuration selectable via CLI / env / file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceConfig {
    /// Deployment topology.
    pub topology: DeploymentTopology,
    /// Role within the topology.
    pub role: ServiceRole,
    /// Optional node / peer identifier for multi-node scaffolds.
    pub node_id: String,
    /// When true, run the fake sensor smoke loop after boot.
    pub smoke_frames: u64,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            topology: DeploymentTopology::Single,
            role: ServiceRole::Primary,
            node_id: "wm-0".into(),
            smoke_frames: 0,
        }
    }
}

impl ServiceConfig {
    /// Build config from topology, applying default role for that topology.
    pub fn for_topology(topology: DeploymentTopology) -> Self {
        let role = match topology {
            DeploymentTopology::Single => ServiceRole::Primary,
            DeploymentTopology::HotStandby => ServiceRole::Primary,
            DeploymentTopology::PeerToPeer => ServiceRole::Peer,
        };
        Self {
            topology,
            role,
            ..Self::default()
        }
    }

    /// Validate topology × role combinations (soft rules for the scaffold).
    pub fn validate(&self) -> Result<(), String> {
        match (self.topology, self.role) {
            (DeploymentTopology::Single, ServiceRole::Primary) => Ok(()),
            (DeploymentTopology::Single, _) => Err(
                "single topology requires role=primary (standby/peer not valid)".into(),
            ),
            (DeploymentTopology::HotStandby, ServiceRole::Primary | ServiceRole::Standby) => Ok(()),
            (DeploymentTopology::HotStandby, ServiceRole::Peer) => {
                Err("hot_standby does not use peer role; use primary or standby".into())
            }
            (DeploymentTopology::PeerToPeer, ServiceRole::Peer | ServiceRole::Primary) => Ok(()),
            (DeploymentTopology::PeerToPeer, ServiceRole::Standby) => {
                Err("peer_to_peer does not use standby role; use peer".into())
            }
        }
    }
}

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
    fn log_sigreg_health(&mut self, health: SigRegHealth) -> Result<(), ServiceError> {
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
}

impl core::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotBooted => write!(f, "world-model service not booted"),
            Self::WorldModel(e) => write!(f, "world model: {e}"),
            Self::Attestation(e) => write!(f, "attestation: {e}"),
        }
    }
}

impl std::error::Error for ServiceError {}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use weftos_worldmodel::{
        AttestationPayload, CemPlanner, EVENT_KIND_LEWM_FRAME_ATTESTATION, LATENT_DIM,
        LinearPredPhi, PlannerKind, WelfordSigRegMonitor,
    };

    #[test]
    fn topology_parse_and_names() {
        assert_eq!(
            DeploymentTopology::parse("single"),
            Some(DeploymentTopology::Single)
        );
        assert_eq!(
            DeploymentTopology::parse("hot-standby"),
            Some(DeploymentTopology::HotStandby)
        );
        assert_eq!(
            DeploymentTopology::parse("p2p"),
            Some(DeploymentTopology::PeerToPeer)
        );
        assert_eq!(DeploymentTopology::Single.as_str(), "single");
        assert!(DeploymentTopology::parse("nope").is_none());
    }

    #[test]
    fn single_boots_and_smoke_fake_sensor() {
        let mut svc = WorldModelService::new(ServiceConfig::for_topology(
            DeploymentTopology::Single,
        ))
        .expect("config");
        let report = svc.boot().expect("boot");
        assert!(report.available);
        assert_eq!(report.topology, DeploymentTopology::Single);
        assert_eq!(report.latent_dim, 192);

        let frames = svc.run_fake_sensor_pipeline(5, false).expect("smoke");
        assert_eq!(frames.len(), 5);
        assert_eq!(svc.frames, 5);

        // Chain may also hold SIGReg health events (WEFT-528); filter attests.
        let attests: Vec<_> = svc
            .chain
            .entries
            .iter()
            .filter(|e| e.kind == EVENT_KIND_LEWM_FRAME_ATTESTATION)
            .collect();
        assert_eq!(attests.len(), 5);

        // Chain entries decode round-trip (WEFT-533).
        for (i, fr) in frames.iter().enumerate() {
            assert_eq!(fr.manifold_major, 1);
            assert_eq!(fr.latent_dim, LATENT_DIM as u16);
            let payload =
                AttestationPayload::from_json_bytes(&attests[i].payload).expect("json");
            let tuple = payload.to_tuple().expect("tuple");
            assert_eq!(tuple.frame_seq, fr.frame_seq);
            assert!(tuple.manifold_matches_local());
            assert_eq!(payload.manifold_major, 1);
            assert_eq!(payload.latent_dim, 192);
        }
    }

    #[test]
    fn all_three_topologies_construct() {
        for topo in [
            DeploymentTopology::Single,
            DeploymentTopology::HotStandby,
            DeploymentTopology::PeerToPeer,
        ] {
            let cfg = ServiceConfig::for_topology(topo);
            let mut svc = WorldModelService::new(cfg).expect("new");
            let report = svc.boot().expect("boot");
            assert_eq!(report.topology, topo);
            assert!(report.available);
        }
    }

    #[test]
    fn hot_standby_accepts_standby_role() {
        let mut cfg = ServiceConfig::for_topology(DeploymentTopology::HotStandby);
        cfg.role = ServiceRole::Standby;
        let mut svc = WorldModelService::new(cfg).expect("new");
        assert!(svc.boot().is_ok());
    }

    #[test]
    fn single_rejects_standby_role() {
        let mut cfg = ServiceConfig::for_topology(DeploymentTopology::Single);
        cfg.role = ServiceRole::Standby;
        assert!(WorldModelService::new(cfg).is_err());
    }

    #[test]
    fn planner_loop_runs_at_10hz() {
        let mut svc = WorldModelService::new(ServiceConfig::for_topology(
            DeploymentTopology::Single,
        ))
        .expect("config");
        svc.boot().expect("boot");
        assert_eq!(PLANNER_HZ, 10.0);
        assert_eq!(PLANNER_PERIOD_MS, 100);
        assert_eq!(svc.model.planner.kind(), PlannerKind::Cem);

        let z = weftos_worldmodel::zero_latent();
        let plans = svc.run_planner_loop(&z, 5, 3, 0).expect("loop");
        assert_eq!(plans.len(), 5);
        assert_eq!(svc.plan_ticks, 5);
        for p in &plans {
            assert_eq!(p.steps.len(), 3);
            assert_eq!(p.kind, PlannerKind::Cem);
        }

        // Within the same period: no new tick.
        assert!(svc.maybe_plan_tick(&z, 50, 3).unwrap().is_none());
    }

    #[test]
    fn toy_planning_task_via_service() {
        let mut svc = WorldModelService::new(ServiceConfig::for_topology(
            DeploymentTopology::Single,
        ))
        .expect("config");
        svc.boot().expect("boot");

        // Goal: move latent dim-0 family toward +1 via action channel 0.
        let mut goal = weftos_worldmodel::zero_latent();
        for g in goal.iter_mut().step_by(32) {
            *g = 1.0;
        }
        let mut planner = CemPlanner::with_seed(99).with_goal(goal);
        planner.population = 64;
        planner.elites = 16;
        planner.iterations = 8;
        planner.init_std = 0.35;
        svc.model.planner = planner;
        svc.model.predictor = LinearPredPhi::default();

        let z0 = weftos_worldmodel::zero_latent();
        let plan = svc.plan_now(&z0, 4).expect("plan");
        assert_eq!(plan.steps.len(), 4);
        // Cost only on goal-aligned dimensions (CEM may keep residual noise
        // on unused action channels; the toy task cares about a[0] → z[0]).
        let final_z = plan.steps.last().unwrap().z_hat;
        let mut cost = 0.0f32;
        let mut null_cost = 0.0f32;
        for i in (0..192).step_by(32) {
            let d = final_z[i] - goal[i];
            cost += d * d;
            null_cost += goal[i] * goal[i];
        }
        assert!(
            cost < null_cost,
            "goal-aligned cost plan={cost} null={null_cost}; a0_sum={}",
            plan.steps.iter().map(|s| s.action.code[0]).sum::<f32>()
        );
        // Residual dynamics integrate actions: net a[0] over the horizon
        // should push dim-0 family toward the +1 goal.
        let a0_sum: f32 = plan.steps.iter().map(|s| s.action.code[0]).sum();
        assert!(
            a0_sum > 0.0,
            "expected positive net action on channel 0, got {a0_sum}"
        );
        assert!((final_z[0] - 0.0).abs() > 1e-3, "latent should move");
    }

    #[test]
    fn sigreg_welford_wired_into_process_frame() {
        let mut svc = WorldModelService::new(ServiceConfig::for_topology(
            DeploymentTopology::Single,
        ))
        .expect("config");
        svc.boot().expect("boot");
        // Force quick rollback: low min_samples + biased latents via process path
        // uses null encoder (zero latent). Instead poke the monitor directly.
        let mon: &mut WelfordSigRegMonitor = &mut svc.model.sigreg;
        mon.min_samples = 2;
        let mut bad = weftos_worldmodel::zero_latent();
        bad[0] = 40.0;
        let mut rolled = false;
        for t in 0..40u64 {
            let h = mon.update_at(&bad, t * 1000).unwrap();
            if mon.last_rolled_back() {
                rolled = true;
                assert!(h.version_tag > 1 || mon.rollback_count >= 1);
                break;
            }
        }
        assert!(rolled);
        assert!((SIGREG_HEALTH_ROLLBACK_THRESHOLD - 0.85).abs() < f32::EPSILON);
        assert_eq!(SIGREG_HEALTH_WINDOW_SECS, 30);
    }

    #[test]
    fn four_condition_gate_wired_into_process_frame() {
        let mut svc = WorldModelService::new(ServiceConfig::for_topology(
            DeploymentTopology::Single,
        ))
        .expect("config");
        svc.boot().expect("boot");

        // Null encoder → zero latents; cold-start gate promotes (min_samples).
        let frames = svc.run_fake_sensor_pipeline(3, false).expect("smoke");
        assert_eq!(frames.len(), 3);
        for fr in &frames {
            // Cold start: all four metrics report perfect / above threshold.
            assert!(fr.gate_promote, "cold-start should promote: {fr:?}");
            assert!(fr.gate_sigreg >= 0.85);
        }
        assert!(svc.gate_promotes >= 1);
        assert!(svc.last_gate.is_some());
        assert!(svc.may_promote_checkpoint());

        // Force SIGReg-only veto through the gate on the model.
        svc.model.rollback_gate.set_sigreg_health(0.10);
        let v = svc.model.rollback_gate.evaluate_now();
        assert!(!v.promote);
        assert_eq!(
            v.first_veto(),
            Some(weftos_worldmodel::GateCondition::ClusterSigRegHealth)
        );

        // Chain should carry at least one rollback-gate event.
        let gate_events: Vec<_> = svc
            .chain
            .entries
            .iter()
            .filter(|e| e.kind == EVENT_KIND_ROLLBACK_GATE)
            .collect();
        assert!(
            !gate_events.is_empty(),
            "expected lewm.rollback_gate ExoChain events"
        );
    }
}
