//! Card 82a59d97: an operator unload is final. The lifecycle never places an
//! unloaded instance again, not from its own record, not from a stale `Lost`
//! record whose order now yields the same instance, and not after a
//! controller or node restart; an upgrade (place the new version, unload the
//! old) ends with exactly one instance.

use clawft_types::placement::TrustTier;
use ed25519_dalek::SigningKey;

use super::lifecycle::LifecycleState;
use super::msg::method;
use super::plane::PlaneConfig;
use super::plane_place::PlaceOrder;
use super::test_support::*;
use super::tests_life_plane::{Rig, actions, cfg, chained, count, past_backoff, rig};
use crate::chain::EVENT_KIND_WORKLOAD_LIFECYCLE;
use crate::workload_runtime::RunMode;

const V1: &str = "#!/bin/sh\nexec sleep 60\n";
const V2: &str = "#!/bin/sh\n# v2\nexec sleep 60\n";

fn pinned(r: &Rig, pkg: &std::path::Path) -> PlaceOrder {
    let mut o = mode_order(pkg, RunMode::Listener, Some(&r.a.id));
    o.pin = Some(r.a.id.clone());
    o
}

/// A reboot: everything the node held, in the host and in its adapter, is gone.
async fn reboot(n: &HostNode) {
    let held: Vec<_> = n.svc.instances.lock().await.drain().collect();
    for (_, p) in held {
        let _ = n.svc.routes[&p.route].unload(p.handle).await;
    }
}

/// Rebuild the rig's package where it was (new bytes, so a new package id).
fn rebuild(r: &Rig) -> std::path::PathBuf {
    std::fs::remove_dir_all(&r.pkg).unwrap();
    package(r._tmp.path(), "life-cog", V2, &[arch()])
}

/// Several lifecycle passes, each past the retry backoff.
async fn settle(r: &Rig) -> Vec<String> {
    let mut seen = Vec::new();
    for _ in 0..4 {
        past_backoff().await;
        seen.extend(actions(&r.plane.lifecycle_tick().await).into_iter().map(String::from));
    }
    seen
}

#[tokio::test]
async fn an_unloaded_instance_stays_unloaded_through_the_lifecycle() {
    let r = rig().await;
    let placed = r.plane.place(&pinned(&r, &r.pkg)).await.unwrap().placed.unwrap();
    r.plane.instance(method::UNLOAD, &placed.instance_id).await.unwrap();
    let seen = settle(&r).await;
    assert!(seen.is_empty(), "{seen:?}");
    assert_eq!(count(&r.a).await + count(&r.b).await, 0);
    assert!(r.plane.placements().is_empty());
    assert!(r.plane.lifecycle_of(&placed.instance_id).is_none());
}

/// The Ventuno rehearsal: a `Lost` record left from an earlier node restart
/// kept retrying its order, which (the package having been rebuilt in place)
/// now yields the running instance's id, so every retry failed with "already
/// loaded". The operator's unload of that instance let the next retry
/// succeed, and the unloaded instance came back beside its upgrade.
#[tokio::test]
async fn a_stale_lost_record_does_not_place_an_unloaded_instance_again() {
    let r = rig().await;
    let old_order = pinned(&r, &r.pkg);
    let first = r.plane.place(&old_order).await.unwrap().placed.unwrap();
    // The node restarts empty while it may not take the instance back: Lost, retrying.
    reboot(&r.a).await;
    assert!(r.plane.set_tier(&r.a.id, TrustTier::Discovered));
    let ev = r.plane.lifecycle_tick().await;
    assert_eq!(actions(&ev), ["lost", "reschedule_failed"], "{ev:#?}");
    assert_eq!(r.plane.lifecycle_of(&first.instance_id), Some(LifecycleState::Lost));
    // The package is rebuilt where it was, and the operator places it again.
    assert_eq!(rebuild(&r), r.pkg);
    assert!(r.plane.set_tier(&r.a.id, TrustTier::Paired));
    let running = r.plane.place(&old_order).await.unwrap().placed.unwrap();
    assert_ne!(running.instance_id, first.instance_id, "a rebuilt package is another instance");

    // The upgrade: the new version goes on, the old one comes off.
    let tmp = tempfile::tempdir().unwrap();
    let v2 = package(tmp.path(), "life-cog", "#!/bin/sh\n# v0.1.2\nexec sleep 60\n", &[arch()]);
    let upgrade = r.plane.place(&pinned(&r, &v2)).await.unwrap().placed.unwrap();
    r.plane.instance(method::UNLOAD, &running.instance_id).await.unwrap();

    settle(&r).await;
    let held: Vec<String> = r.a.svc.instances.lock().await.keys().cloned().collect();
    assert_eq!(held, vec![upgrade.instance_id.clone()], "exactly the upgrade runs");
    let recs: Vec<String> = r.plane.placements().into_iter().map(|p| p.instance_id).collect();
    assert_eq!(recs, vec![upgrade.instance_id.clone()]);
    assert!(r.plane.lifecycle_of(&first.instance_id).is_none(), "the stale record is retired");
    assert!(r.plane.lifecycle_of(&running.instance_id).is_none());
    let life = chained(&r.chain, EVENT_KIND_WORKLOAD_LIFECYCLE);
    assert!(
        life.iter().any(|p| p["instance_id"] == first.instance_id.as_str() && p["phase"] == "superseded"),
        "the retirement is chained: {life:#?}"
    );
}

#[tokio::test]
async fn a_stale_lost_record_yields_to_the_live_instance_it_would_duplicate() {
    let r = rig().await;
    let order = pinned(&r, &r.pkg);
    let first = r.plane.place(&order).await.unwrap().placed.unwrap();
    reboot(&r.a).await;
    assert!(r.plane.set_tier(&r.a.id, TrustTier::Discovered));
    r.plane.lifecycle_tick().await;
    rebuild(&r);
    assert!(r.plane.set_tier(&r.a.id, TrustTier::Paired));
    let running = r.plane.place(&order).await.unwrap().placed.unwrap();
    // No retry fails against the running copy; the old record is simply gone.
    let seen = settle(&r).await;
    assert!(!seen.iter().any(|a| a == "reschedule_failed"), "{seen:?}");
    assert!(r.plane.lifecycle_of(&first.instance_id).is_none());
    assert_eq!(r.plane.lifecycle_of(&running.instance_id), Some(LifecycleState::Running));
    assert_eq!(count(&r.a).await, 1);
}

#[tokio::test]
async fn an_upgrade_ends_with_exactly_one_instance() {
    let r = rig().await;
    let old = r.plane.place(&pinned(&r, &r.pkg)).await.unwrap().placed.unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let v2 = package(tmp.path(), "life-cog", V2, &[arch()]);
    let new = r.plane.place(&pinned(&r, &v2)).await.unwrap().placed.unwrap();
    assert_ne!(old.instance_id, new.instance_id);
    r.plane.instance(method::UNLOAD, &old.instance_id).await.unwrap();
    let seen = settle(&r).await;
    assert!(seen.is_empty(), "{seen:?}");
    let held: Vec<String> = r.a.svc.instances.lock().await.keys().cloned().collect();
    assert_eq!(held, vec![new.instance_id.clone()]);
    assert_eq!(count(&r.b).await, 0);
}

#[tokio::test]
async fn an_unload_survives_a_controller_restart() {
    let r = rig().await;
    let state = r._tmp.path().join("placements.json");
    let key = SigningKey::from_bytes(&[10; 32]);
    let (plane, _) = controller(&key, r.net.clone());
    let plane = plane.with_state_file(&state).unwrap().with_config(cfg());
    plane.add_target(&r.addr_a, TrustTier::Paired).await.unwrap();
    plane.add_target(&r.addr_b, TrustTier::Paired).await.unwrap();
    let kept = plane.place(&pinned(&r, &r.pkg)).await.unwrap().placed.unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let other = package(tmp.path(), "other-cog", V1, &[arch()]);
    let gone = plane.place(&pinned(&r, &other)).await.unwrap().placed.unwrap();
    plane.instance(method::UNLOAD, &gone.instance_id).await.unwrap();
    drop(plane);

    let (again, _) = controller(&key, r.net.clone());
    let again = again.with_state_file(&state).unwrap().with_config(PlaneConfig { ..cfg() });
    assert!(again.lifecycle_of(&gone.instance_id).is_none());
    assert_eq!(again.lifecycle_of(&kept.instance_id), Some(LifecycleState::Running));
    for _ in 0..3 {
        past_backoff().await;
        let ev = again.lifecycle_tick().await;
        assert!(ev.is_empty(), "{ev:#?}");
    }
    let held: Vec<String> = r.a.svc.instances.lock().await.keys().cloned().collect();
    assert_eq!(held, vec![kept.instance_id.clone()], "the placed one still runs, the unloaded one stays gone");
}

/// A `Lost` record of `running`'s order under another id, as a controller
/// from before card 82a59d97 left it in its state file.
fn plant_stale(r: &Rig, running: &super::plane::PlacementRecord) -> String {
    let id = format!("life-cog-{}-{}", "0".repeat(32), running.node_id);
    let mut life = r.plane.life_of(&running.instance_id).unwrap();
    life.life.state = LifecycleState::Lost;
    let rec = super::plane::PlacementRecord { instance_id: id.clone(), ..running.clone() };
    r.plane.placements.lock().unwrap().insert(id.clone(), rec);
    r.plane.lives.lock().unwrap().insert(id.clone(), life);
    id
}

#[tokio::test]
async fn an_unload_retires_a_lost_record_of_the_same_order() {
    let r = rig().await;
    let running = r.plane.place(&pinned(&r, &r.pkg)).await.unwrap().placed.unwrap();
    let stale = plant_stale(&r, &running);
    r.plane.instance(method::UNLOAD, &running.instance_id).await.unwrap();
    assert!(r.plane.lifecycle_of(&stale).is_none(), "retired with the unload");
    let seen = settle(&r).await;
    assert!(seen.is_empty(), "{seen:?}");
    assert_eq!(count(&r.a).await + count(&r.b).await, 0, "nothing placed again");
    assert!(r.plane.placements().is_empty());
}

#[tokio::test]
async fn the_lifecycle_supersedes_a_lost_record_whose_order_runs_on_its_node() {
    let r = rig().await;
    let running = r.plane.place(&pinned(&r, &r.pkg)).await.unwrap().placed.unwrap();
    let stale = plant_stale(&r, &running);
    let ev = r.plane.lifecycle_tick().await;
    assert_eq!(actions(&ev), ["superseded"], "{ev:#?}");
    assert!(r.plane.lifecycle_of(&stale).is_none());
    assert_eq!(r.plane.lifecycle_of(&running.instance_id), Some(LifecycleState::Running));
    // Its unload is then final.
    r.plane.instance(method::UNLOAD, &running.instance_id).await.unwrap();
    assert!(settle(&r).await.is_empty());
    assert_eq!(count(&r.a).await, 0);
}
