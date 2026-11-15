//! Host daemon restart seen from the controller (card 5fb0932b): a native cog
//! placed on a node, then the node's daemon restarts over the same run root.
//! The host journal puts the instance back with the same id (`tests_restart`
//! covers the host alone); these tests show the controller side: it keeps the
//! same record, mirrors the instance as running, and neither places it again
//! nor reports it lost, whether the node was down for a blip or for longer
//! than `dead_after`.

use std::sync::Arc;
use std::time::Duration;

use clawft_types::placement::TrustTier;

use super::lifecycle::LifecycleState;
use super::plane::PlaneConfig;
use super::plane_lifecycle::LifecycleEvent;
use super::test_support::*;
use super::tests_restart::{Rig, ctl_key};

const DEAD_AFTER: Duration = Duration::from_millis(300);

fn actions(ev: &[LifecycleEvent]) -> Vec<&str> {
    ev.iter().map(|e| e.action.as_str()).collect()
}

/// Place the rig's cog on a first daemon and return the controller, the
/// switchable network, the node address and the instance id.
async fn placed(rig: &Rig) -> (super::plane::PlacementControlPlane, Arc<Killable>, String, String) {
    let net = Killable::new();
    let addr = net.serve("restart-node", rig.host());
    let (plane, _chain) = controller(&ctl_key(), net.clone());
    let plane = plane.with_config(PlaneConfig { dead_after: DEAD_AFTER, ..PlaneConfig::default() });
    plane.add_target(&addr, TrustTier::Pinned).await.unwrap();
    let order = mode_order(&rig.pkg, crate::workload_runtime::RunMode::Listener, None);
    let r = plane.place(&order).await.unwrap();
    let iid = r.placed.expect(&r.explain).instance_id;
    rig.wait_launches(1).await;
    plane.lifecycle_tick().await;
    assert_eq!(plane.lifecycle_of(&iid), Some(LifecycleState::Running));
    (plane, net, addr, iid)
}

/// The daemon restarts: the old host is gone, a new one restores the journal
/// and answers at the same address.
async fn restart(rig: &Rig, net: &Arc<Killable>, addr: &str, iid: &str) {
    net.kill(addr);
    let b = rig.host();
    let report = b.restore_instances().await;
    assert_eq!(report.restored, [iid.to_string()], "{report:?}");
    net.serve("restart-node", b);
}

#[tokio::test]
async fn the_controller_shows_a_restarted_cog_running_with_the_same_id_after_a_short_outage() {
    let rig = Rig::new();
    let (plane, net, addr, iid) = placed(&rig).await;
    restart(&rig, &net, &addr, &iid).await;
    // The node is unreachable for a blip, shorter than dead_after: nothing moves.
    let ev = plane.lifecycle_tick().await;
    assert!(!actions(&ev).contains(&"lost"), "{ev:?}");
    net.revive(&addr);
    let ev = plane.lifecycle_tick().await;
    assert!(actions(&ev).iter().all(|a| *a != "lost" && *a != "rescheduled"), "{ev:?}");
    assert_eq!(plane.lifecycle_of(&iid), Some(LifecycleState::Running));
    let recs = plane.placements();
    assert_eq!(recs.len(), 1, "no second placement: {recs:?}");
    assert_eq!(recs[0].instance_id, iid);
    rig.wait_launches(2).await;
}

#[tokio::test]
async fn a_cog_whose_node_was_down_past_dead_after_is_adopted_back_when_it_restarts() {
    let rig = Rig::new();
    let (plane, net, addr, iid) = placed(&rig).await;
    net.kill(addr.as_str());
    plane.lifecycle_tick().await;
    tokio::time::sleep(DEAD_AFTER + Duration::from_millis(80)).await;
    // Only one node: the instance is Lost and has nowhere else to go.
    let ev = plane.lifecycle_tick().await;
    assert!(actions(&ev).contains(&"lost"), "{ev:?}");
    assert_eq!(plane.lifecycle_of(&iid), Some(LifecycleState::Lost));
    // The daemon comes back over the same run root and restores the journal.
    let b = rig.host();
    let report = b.restore_instances().await;
    assert_eq!(report.restored, std::slice::from_ref(&iid), "{report:?}");
    net.serve("restart-node", b);
    net.revive(&addr);
    let ev = plane.lifecycle_tick().await;
    assert!(actions(&ev).contains(&"recovered"), "{ev:?}");
    assert_eq!(plane.lifecycle_of(&iid), Some(LifecycleState::Running));
    let recs = plane.placements();
    assert_eq!(recs.len(), 1, "{recs:?}");
    assert_eq!(recs[0].instance_id, iid);
}
