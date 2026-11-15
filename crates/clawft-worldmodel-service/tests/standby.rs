//! Standby replay and lease failover, in-process (no network, no consensus).

use clawft_worldmodel_service::{
    DeploymentTopology, LeaseTracker, ServiceConfig, ServiceError, ServiceRole,
    WorldModelService, EVENT_KIND_ROLE_CHANGE,
};
use weftos_worldmodel::{zero_latent, Action, EVENT_KIND_LEWM_FRAME_ATTESTATION};

fn node(role: ServiceRole, id: &str) -> WorldModelService {
    let mut cfg = ServiceConfig::for_topology(DeploymentTopology::HotStandby);
    cfg.role = role;
    cfg.node_id = id.into();
    let mut svc = WorldModelService::new(cfg).expect("config");
    svc.boot().expect("boot");
    svc
}

fn primary_with(n: u64) -> WorldModelService {
    let mut p = node(ServiceRole::Primary, "wm-a");
    p.run_fake_sensor_pipeline(n, true).expect("frames");
    p
}

#[test]
fn standby_replay_matches_primary_state() {
    let n = 40;
    let primary = primary_with(n);
    let mut standby = node(ServiceRole::Standby, "wm-b");
    let out = standby.replay_from(&primary.chain).expect("replay");

    assert_eq!(out.frames, n);
    assert_eq!(standby.frames, primary.frames);
    assert_eq!(standby.gate_promotes, primary.gate_promotes);
    assert_eq!(standby.gate_vetoes, primary.gate_vetoes);
    assert_eq!(standby.sigreg_rollbacks, primary.sigreg_rollbacks);
    assert_eq!(
        standby.model.sigreg.sample_count(),
        primary.model.sigreg.sample_count()
    );
    assert_eq!(standby.last_tuple, primary.last_tuple);
    assert!(standby.chain.is_empty(), "standby must not attest while replaying");
    // Health/gate entries on the primary chain were counted, not re-applied.
    let frame_entries = primary
        .chain
        .entries
        .iter()
        .filter(|e| e.kind == EVENT_KIND_LEWM_FRAME_ATTESTATION)
        .count() as u64;
    assert_eq!(frame_entries, n);
    assert_eq!(
        out.frames + out.sigreg_events + out.gate_events + out.role_events + out.other_events,
        primary.chain.len() as u64
    );
}

#[test]
fn replay_health_score_within_tolerance() {
    let primary = primary_with(33);
    let mut standby = node(ServiceRole::Standby, "wm-b");
    standby.replay_from(&primary.chain).unwrap();
    // Same next sample on cloned monitors must give the same score.
    let mut pm = primary.model.sigreg.clone();
    let mut sm = standby.model.sigreg.clone();
    let z = zero_latent();
    let ps = pm.update_at(&z, 10_000_000_000_000).unwrap().score;
    let ss = sm.update_at(&z, 10_000_000_000_000).unwrap().score;
    assert!((ps - ss).abs() < 1e-6, "primary {ps} standby {ss}");
    assert_eq!(pm.mean(), sm.mean());
}

#[test]
fn replay_is_idempotent_and_rejects_wrong_role() {
    let primary = primary_with(5);
    let mut standby = node(ServiceRole::Standby, "wm-b");
    standby.replay_from(&primary.chain).unwrap();
    let again = standby.replay_from(&primary.chain).unwrap();
    assert_eq!(again.frames, 0);
    assert_eq!(again.skipped, primary.chain.len() as u64);
    assert_eq!(standby.frames, 5);

    let mut not_standby = node(ServiceRole::Primary, "wm-c");
    assert!(matches!(
        not_standby.replay_entry(&primary.chain.entries[0]),
        Err(ServiceError::NotStandby)
    ));
}

#[test]
fn replay_detects_frame_gap() {
    let primary = primary_with(4);
    let mut standby = node(ServiceRole::Standby, "wm-b");
    let second_frame = primary
        .chain
        .entries
        .iter()
        .filter(|e| e.kind == EVENT_KIND_LEWM_FRAME_ATTESTATION)
        .nth(1)
        .unwrap();
    assert!(matches!(
        standby.replay_entry(second_frame),
        Err(ServiceError::Replay(_))
    ));
}

#[test]
fn no_promotion_while_heartbeats_arrive() {
    let mut standby = node(ServiceRole::Standby, "wm-b");
    let mut lease = LeaseTracker::from_config(&standby.config, 0);
    assert_eq!(lease.lease_ms, 3000);
    for t in (1000..=20_000).step_by(1000) {
        lease.heartbeat(t);
        assert!(!standby.maybe_failover(&lease, t + 500).unwrap());
    }
    assert_eq!(standby.config.role, ServiceRole::Standby);
    assert!(standby.chain.is_empty());
}

#[test]
fn promotion_after_missed_lease_then_serves_frames() {
    let primary = primary_with(6);
    let mut standby = node(ServiceRole::Standby, "wm-b");
    standby.replay_from(&primary.chain).unwrap();
    let mut lease = LeaseTracker::new(3000, 0);
    lease.heartbeat(10_000);
    assert!(!lease.expired(13_000));
    assert!(!standby.maybe_failover(&lease, 13_000).unwrap());
    assert!(standby.maybe_failover(&lease, 13_001).unwrap());
    assert_eq!(standby.config.role, ServiceRole::Primary);

    // Exactly one role-change event, with the documented fields.
    let ev: Vec<_> = standby
        .chain
        .entries
        .iter()
        .filter(|e| e.kind == EVENT_KIND_ROLE_CHANGE)
        .collect();
    assert_eq!(ev.len(), 1);
    let v: serde_json::Value = serde_json::from_slice(&ev[0].payload).unwrap();
    assert_eq!(v["from"], "standby");
    assert_eq!(v["to"], "primary");
    assert_eq!(v["ts"], 13_001);
    assert_eq!(v["node_id"], "wm-b");

    // The promoted node continues the frame sequence and attests.
    let r = standby
        .process_frame(b"after-failover", Action::null(), 14_000)
        .expect("promoted standby serves frames");
    assert_eq!(r.frame_seq, 6);
    assert!(standby
        .chain
        .entries
        .iter()
        .any(|e| e.kind == EVENT_KIND_LEWM_FRAME_ATTESTATION));
    // Already primary: no second promotion.
    assert!(matches!(
        standby.promote_to_primary(15_000),
        Err(ServiceError::NotStandby)
    ));
}

#[test]
fn demoted_primary_refuses_frames_and_attests_change() {
    let mut p = primary_with(3);
    p.demote_to_standby(5_000).expect("demote");
    assert_eq!(p.config.role, ServiceRole::Standby);
    assert!(matches!(
        p.process_frame(b"x", Action::null(), 6_000),
        Err(ServiceError::NotPrimary)
    ));
    let last = p.chain.entries.last().unwrap();
    assert_eq!(last.kind, EVENT_KIND_ROLE_CHANGE);
    let v: serde_json::Value = serde_json::from_slice(&last.payload).unwrap();
    assert_eq!((v["from"].as_str(), v["to"].as_str()), (Some("primary"), Some("standby")));
}

#[test]
fn single_topology_cannot_change_role() {
    let mut svc = WorldModelService::new(ServiceConfig::for_topology(DeploymentTopology::Single))
        .unwrap();
    svc.boot().unwrap();
    assert!(matches!(svc.demote_to_standby(1), Err(ServiceError::Topology(_))));
    assert_eq!(svc.config.role, ServiceRole::Primary);
    assert!(svc.chain.is_empty());
}
