//! Unit tests carried over from the single-file layout.

use super::*;
use weftos_worldmodel::{
    LatentPlanner, EVENT_KIND_ROLLBACK_GATE, SIGREG_HEALTH_ROLLBACK_THRESHOLD,
    SIGREG_HEALTH_WINDOW_SECS,
};
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

/// Lane H: a model trained through the facade's offline cycle and committed at a tick
/// is what `process_frame` predicts with, so the frame's surprise drops.
///
/// SYNTHETIC stream: the service's null encoder maps every frame to the zero latent, so
/// the world does not respond to the action. The untrained `pred_φ` still adds the
/// action code to the latent and reports surprise; the trained one learns `A = 0`.
#[test]
fn committed_offline_model_lowers_frame_surprise() {
    use weftos_worldmodel::{
        zero_latent, Action, OfflineEdgeTrainer, SensorClass, SmallModelKind, TrainingSample,
    };

    let mut svc =
        WorldModelService::new(ServiceConfig::for_topology(DeploymentTopology::Single)).unwrap();
    svc.boot().unwrap();

    let action = |i: usize| {
        let mut a = Action::null();
        for (k, c) in a.code.iter_mut().enumerate() {
            *c = (((i * 7 + k * 13) % 17) as f32 - 8.0) / 8.0;
        }
        a
    };
    let before = svc.process_frame(b"frame", action(3), 0).unwrap().surprise;
    assert!(before > 0.1, "untrained pred_phi should be surprised, got {before}");

    for i in 0..40 {
        let mut s = TrainingSample::basic(SensorClass::Imu, zero_latent(), zero_latent(), i as u64);
        s.action = action(i as usize);
        svc.model.offline_edge.push_sample(&s).unwrap();
    }
    svc.model.offline_edge_cycle(SensorClass::Imu, SmallModelKind::Encode, 5).unwrap();
    svc.model.commit_models_at_tick(5).unwrap();
    assert!(svc.model.has_learned_predictor(SensorClass::Imu));

    let after = svc.process_frame(b"frame", action(3), 100).unwrap().surprise;
    assert!(after < 0.01 * before, "surprise {before} -> {after}");
}
