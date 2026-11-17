//! Received `infer.<role>` adverts go stale: a controllable clock drives the
//! TTL (no sleeps), a newer advert replaces the older, and a role withdrawn at
//! its source stops being advertised.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::support::*;
use super::svc::SvcRoute;
use super::table::PlacementTable;
use super::types::*;

const TTL: Duration = Duration::from_secs(30);

fn clock() -> (Arc<dyn Fn() -> Instant + Send + Sync>, Arc<AtomicU64>) {
    let base = Instant::now();
    let secs = Arc::new(AtomicU64::new(0));
    let s = secs.clone();
    (Arc::new(move || base + Duration::from_secs(s.load(Ordering::SeqCst))), secs)
}

async fn serving_node(mesh: &Arc<FakeMesh>, id: &str) -> Arc<PlacementTable> {
    let t = Arc::new(PlacementTable::new(id, None, None));
    let up = fake(Reply::ok(id)).await;
    t.register_local("hermes", &up.base(), None, "openai-v1", "LlamaCpp").unwrap();
    t.expose_to_mesh("hermes", true);
    mesh.add_node(id, t.clone(), true);
    t
}

fn consumer(mesh: &Arc<FakeMesh>) -> (PlacementTable, Arc<AtomicU64>) {
    let (c, secs) = clock();
    let a = PlacementTable::new("node-a", Some(mesh.clone() as Arc<dyn MeshDialer>), None)
        .with_advert_ttl(TTL)
        .with_clock(c);
    (a, secs)
}

#[tokio::test]
async fn a_stale_advert_stops_routing_after_its_ttl_and_is_pruned() {
    let mesh = FakeMesh::new("node-a");
    let b = serving_node(&mesh, "node-b").await;
    let (a, now) = consumer(&mesh);
    a.allow_remote_node("hermes", "node-b", true);
    let ad = b.advertisement("hermes", 1).unwrap();
    assert!(a.ingest_advertisement("node-b", &ad));
    now.store(30, Ordering::SeqCst);
    assert_eq!(a.resolve("hermes"), Some(Target::Remote { node_id: "node-b".into() }), "at the TTL");
    now.store(31, Ordering::SeqCst);
    assert_eq!(a.resolve("hermes"), None, "past the TTL without a refresh");
    assert_eq!(a.prune_expired(), 1, "the dead entry is removed, not kept");
    assert_eq!(a.prune_expired(), 0);
    assert!(a.ingest_advertisement("node-b", &ad), "a live node that adverts again is back");
    assert!(a.resolve("hermes").is_some());
}

#[tokio::test]
async fn a_newer_advert_replaces_the_older_for_the_same_node_and_role() {
    let mesh = FakeMesh::new("node-a");
    let b = serving_node(&mesh, "node-b").await;
    let (a, now) = consumer(&mesh);
    a.allow_remote_node("hermes", "node-b", true);
    assert!(a.ingest_advertisement("node-b", &b.advertisement("hermes", 1).unwrap()));
    now.store(20, Ordering::SeqCst);
    assert!(a.ingest_advertisement("node-b", &b.advertisement("hermes", 20).unwrap()));
    // 45 s after the first, 25 s after the second: one entry, kept by the newer.
    now.store(45, Ordering::SeqCst);
    assert_eq!(a.prune_expired(), 0);
    assert!(a.resolve("hermes").is_some());
    now.store(51, Ordering::SeqCst);
    assert_eq!(a.prune_expired(), 1, "a single entry per (node, role) was held");
}

#[tokio::test]
async fn two_live_providers_do_not_trade_places_on_each_announce() {
    let mesh = FakeMesh::new("node-a");
    let b = serving_node(&mesh, "node-b").await;
    let c = serving_node(&mesh, "node-c").await;
    let (a, now) = consumer(&mesh);
    for n in ["node-b", "node-c"] {
        a.allow_remote_node("hermes", n, true);
    }
    // node-c is heard first, then node-b; both repeat every 20 s.
    assert!(a.ingest_advertisement("node-c", &c.advertisement("hermes", 1).unwrap()));
    now.store(5, Ordering::SeqCst);
    assert!(a.ingest_advertisement("node-b", &b.advertisement("hermes", 5).unwrap()));
    let c_wins = Some(Target::Remote { node_id: "node-c".into() });
    assert_eq!(a.resolve("hermes"), c_wins);
    for round in 1..=6u64 {
        let t = 5 + round * 20;
        now.store(t - 10, Ordering::SeqCst);
        assert!(a.ingest_advertisement("node-c", &c.advertisement("hermes", t).unwrap()));
        assert_eq!(a.resolve("hermes"), c_wins, "round {round}: after node-c");
        now.store(t, Ordering::SeqCst);
        assert!(a.ingest_advertisement("node-b", &b.advertisement("hermes", t).unwrap()));
        assert_eq!(a.resolve("hermes"), c_wins, "round {round}: node-b, heard last, does not take over");
    }
}

#[tokio::test]
async fn a_placement_that_moved_to_another_node_takes_over_when_the_quiet_one_expires() {
    let mesh = FakeMesh::new("node-a");
    let b = serving_node(&mesh, "node-b").await;
    let c = serving_node(&mesh, "node-c").await;
    let (a, now) = consumer(&mesh);
    for n in ["node-b", "node-c"] {
        a.allow_remote_node("hermes", n, true);
    }
    assert!(a.ingest_advertisement("node-b", &b.advertisement("hermes", 1).unwrap()));
    now.store(10, Ordering::SeqCst);
    assert!(a.ingest_advertisement("node-c", &c.advertisement("hermes", 10).unwrap()));
    // node-b has been live longest: it keeps the role while both are live.
    assert_eq!(a.resolve("hermes"), Some(Target::Remote { node_id: "node-b".into() }));
    // node-b goes quiet (its last advert was at 1 s), node-c keeps advertising.
    now.store(25, Ordering::SeqCst);
    assert!(a.ingest_advertisement("node-c", &c.advertisement("hermes", 25).unwrap()));
    now.store(31, Ordering::SeqCst);
    assert_eq!(a.resolve("hermes"), Some(Target::Remote { node_id: "node-c".into() }), "node-b expired");
    // node-b returns: it is a newcomer now and does not displace node-c.
    now.store(32, Ordering::SeqCst);
    assert!(a.ingest_advertisement("node-b", &b.advertisement("hermes", 32).unwrap()));
    assert_eq!(a.resolve("hermes"), Some(Target::Remote { node_id: "node-c".into() }));
}

#[tokio::test]
async fn a_svc_role_is_unadvertised_and_withdrawn_when_its_provider_stops_serving() {
    let t = Arc::new(PlacementTable::new("venq", None, None));
    t.register_local_svc("vision", SvcRoute { topic: "svc.infer/venq-npu".into(), model: "m".into() }).unwrap();
    t.expose_to_mesh("vision", true);
    t.allow_mesh_peer("vision", "mac", true);
    assert!(t.advertisement("vision", 1).is_some());
    // The daemon's sync deregisters the role when nothing serves the topic.
    t.deregister_local("vision");
    assert!(t.advertisement("vision", 2).is_none());
    assert_eq!(t.resolve("vision"), None);
    assert_eq!(t.exposures().drain_withdrawals(), vec!["infer.vision".to_string()]);
}

#[tokio::test]
async fn the_default_ttl_survives_one_missed_announcement() {
    // Defaults (2026-11-16): announce every 60 s, adverts count for 120 s.
    assert_eq!(super::table::DEFAULT_ADVERT_TTL, Duration::from_secs(120));
    let mesh = FakeMesh::new("node-a");
    let b = serving_node(&mesh, "node-b").await;
    let (c, now) = clock();
    let a = PlacementTable::new("node-a", Some(mesh.clone() as Arc<dyn MeshDialer>), None).with_clock(c);
    a.allow_remote_node("hermes", "node-b", true);
    assert!(a.ingest_advertisement("node-b", &b.advertisement("hermes", 1).unwrap()));
    // The announce at 60 s is lost; the one at 120 s (the TTL, inclusive) lands.
    now.store(119, Ordering::SeqCst);
    assert!(a.resolve("hermes").is_some(), "one announcement missed, still routed");
    now.store(120, Ordering::SeqCst);
    assert!(a.ingest_advertisement("node-b", &b.advertisement("hermes", 2).unwrap()));
    now.store(121, Ordering::SeqCst);
    assert_eq!(a.resolve("hermes"), Some(Target::Remote { node_id: "node-b".into() }), "the role did not flap");
}

#[tokio::test]
async fn a_provider_that_crashes_without_a_withdrawal_loses_the_role_within_the_default_ttl() {
    let mesh = FakeMesh::new("node-a");
    let b = serving_node(&mesh, "node-b").await;
    let c_node = serving_node(&mesh, "node-c").await;
    let (c, now) = clock();
    let a = PlacementTable::new("node-a", Some(mesh.clone() as Arc<dyn MeshDialer>), None).with_clock(c);
    a.allow_remote_node("hermes", "node-b", true);
    a.allow_remote_node("hermes", "node-c", true);
    assert!(a.ingest_advertisement("node-b", &b.advertisement("hermes", 1).unwrap()));
    now.store(10, Ordering::SeqCst);
    assert!(a.ingest_advertisement("node-c", &c_node.advertisement("hermes", 1).unwrap()));
    // node-b (the longest-live holder) crashes after its first advert; node-c keeps announcing.
    for t in [70u64, 130] {
        now.store(t, Ordering::SeqCst);
        assert!(a.ingest_advertisement("node-c", &c_node.advertisement("hermes", t).unwrap()));
    }
    now.store(121, Ordering::SeqCst);
    assert_eq!(
        a.resolve("hermes"),
        Some(Target::Remote { node_id: "node-c".into() }),
        "the crashed node-b lost the role inside 121 s; with the old 180 s TTL it kept it"
    );
}
