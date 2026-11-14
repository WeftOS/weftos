//! Peer-to-peer fusion is observation only (ADR-090 R3 / R5).

use clawft_worldmodel_service::{
    DeploymentTopology, PeerError, PeerLatents, ServiceConfig, ServiceError, WorldModelService,
};
use weftos_worldmodel::{Action, NodeId, LATENT_DIM};

fn p2p() -> WorldModelService {
    let mut svc =
        WorldModelService::new(ServiceConfig::for_topology(DeploymentTopology::PeerToPeer))
            .unwrap();
    svc.boot().unwrap();
    svc
}

fn flat(v: f32) -> Vec<f32> {
    vec![v; LATENT_DIM]
}

fn run(svc: &mut WorldModelService, peers: bool) -> Vec<clawft_worldmodel_service::FrameResult> {
    let mut out = Vec::new();
    for i in 0..20u64 {
        if peers && i % 3 == 0 {
            svc.ingest_peer_latent(NodeId(1 + (i / 3) % 3), &flat(i as f32), 1000 + i)
                .unwrap();
        }
        let bytes = format!("local-{i}");
        out.push(
            svc.process_frame(bytes.as_bytes(), Action::null(), 5000 + i * 100)
                .unwrap(),
        );
    }
    out
}

#[test]
fn local_frames_identical_with_and_without_peers() {
    let mut solo = p2p();
    let mut fused = p2p();
    let a = run(&mut solo, false);
    let b = run(&mut fused, true);
    assert_eq!(a, b, "peers must not change local FrameResults");
    assert_eq!(solo.chain.entries, fused.chain.entries);
    assert_eq!(solo.last_tuple, fused.last_tuple);
    assert_eq!(solo.peers_seen, 0);
    assert_eq!(fused.peers_seen, 3);
    assert!(fused.last_consensus.is_some());
}

#[test]
fn median_ignores_one_outlier_and_mean_does_not() {
    let mut p = PeerLatents::new();
    p.ingest(NodeId(1), &flat(1.0), 1).unwrap();
    p.ingest(NodeId(2), &flat(1.0), 2).unwrap();
    p.ingest(NodeId(3), &flat(1000.0), 3).unwrap();
    assert_eq!(p.consensus_median().unwrap(), [1.0f32; LATENT_DIM]);
    assert!(p.consensus_mean().unwrap()[0] > 300.0);
}

#[test]
fn last_write_wins_and_even_count_median() {
    let mut p = PeerLatents::new();
    assert!(p.consensus_median().is_none());
    p.ingest(NodeId(1), &flat(5.0), 1).unwrap();
    p.ingest(NodeId(1), &flat(2.0), 2).unwrap();
    p.ingest(NodeId(2), &flat(4.0), 3).unwrap();
    assert_eq!(p.len(), 2);
    assert_eq!(p.get(NodeId(1)).unwrap().timestamp_ms, 2);
    assert_eq!(p.consensus_median().unwrap()[0], 3.0);
}

#[test]
fn wrong_length_and_non_finite_latents_rejected() {
    let mut svc = p2p();
    let err = svc
        .ingest_peer_latent(NodeId(1), &[0.0; 10], 1)
        .unwrap_err();
    assert!(matches!(
        err,
        ServiceError::Peer(PeerError::Dim { expected: 192, got: 10 })
    ));
    let mut bad = flat(0.0);
    bad[7] = f32::NAN;
    assert!(matches!(
        svc.ingest_peer_latent(NodeId(1), &bad, 1),
        Err(ServiceError::Peer(PeerError::NonFinite))
    ));
    assert_eq!(svc.peers_seen, 0);
}

#[test]
fn ingest_rejected_outside_peer_to_peer() {
    for topo in [DeploymentTopology::Single, DeploymentTopology::HotStandby] {
        let mut svc = WorldModelService::new(ServiceConfig::for_topology(topo)).unwrap();
        svc.boot().unwrap();
        assert!(matches!(
            svc.ingest_peer_latent(NodeId(1), &flat(0.0), 1),
            Err(ServiceError::Topology(_))
        ));
    }
}
