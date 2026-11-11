//! Cards 2bc237be, d630abbf and b71cd973: an operator unload is durable and
//! complete. It survives a lost answer plus a restart of either side, it
//! removes the controller's placement even when the node no longer holds the
//! instance, and the node publishes the memory it freed before the
//! controller judges the next placement.

use std::sync::Arc;

use clawft_types::placement::TrustTier;
use ed25519_dalek::SigningKey;

use super::msg::method;
use super::plane_place::PlaceOrder;
use super::test_support::*;
use super::tests_life_plane::{Rig, actions, cfg, count, past_backoff, rig};
use crate::workload_runtime::RunMode;

fn pinned(r: &Rig) -> PlaceOrder {
    let mut o = mode_order(&r.pkg, RunMode::Listener, Some(&r.a.id));
    o.pin = Some(r.a.id.clone());
    o
}

/// A node restart: what it held, and what it remembered of unloads, is gone.
async fn restart(n: &HostNode) {
    let held: Vec<_> = n.svc.instances.lock().await.drain().collect();
    for (_, p) in held {
        let _ = n.svc.routes[&p.route].unload(p.handle).await;
    }
    n.svc.departed.lock().unwrap().clear();
}

async fn passes(r: &Rig, n: usize) -> Vec<String> {
    let mut seen = Vec::new();
    for _ in 0..n {
        past_backoff().await;
        seen.extend(actions(&r.plane.lifecycle_tick().await).into_iter().map(String::from));
    }
    seen
}

#[tokio::test]
async fn an_unload_whose_answer_is_lost_is_not_undone_by_a_node_restart() {
    let r = rig().await;
    let placed = r.plane.place(&pinned(&r)).await.unwrap().placed.unwrap();
    // The unload reaches the node, which does it, but no answer comes back.
    r.net.kill(&r.addr_a);
    assert!(r.plane.instance(method::UNLOAD, &placed.instance_id).await.is_err());
    assert!(r.plane.has_unload_intent(&placed.instance_id), "the intent is kept");
    // The node restarts before the next lifecycle pass and forgets the unload.
    restart(&r.a).await;
    r.net.revive(&r.addr_a);
    let seen = passes(&r, 3).await;
    assert!(!seen.iter().any(|a| a == "lost" || a == "replaced"), "{seen:?}");
    assert_eq!(count(&r.a).await + count(&r.b).await, 0, "not placed again");
    assert!(r.plane.placements().is_empty(), "the unload was finished");
    assert!(r.plane.lifecycle_of(&placed.instance_id).is_none());
}

#[tokio::test]
async fn an_unload_intent_survives_a_controller_restart() {
    let r = rig().await;
    let state = r._tmp.path().join("placements.json");
    let key = SigningKey::from_bytes(&[10; 32]);
    let (plane, _) = controller(&key, r.net.clone());
    let plane = plane.with_state_file(&state).unwrap().with_config(cfg());
    plane.add_target(&r.addr_a, TrustTier::Paired).await.unwrap();
    plane.add_target(&r.addr_b, TrustTier::Paired).await.unwrap();
    let placed = plane.place(&pinned(&r)).await.unwrap().placed.unwrap();
    r.net.kill(&r.addr_a);
    assert!(plane.instance(method::UNLOAD, &placed.instance_id).await.is_err());
    drop(plane);

    restart(&r.a).await;
    r.net.revive(&r.addr_a);
    let (again, _) = controller(&key, r.net.clone());
    let again = again.with_state_file(&state).unwrap().with_config(cfg());
    again.add_target(&r.addr_a, TrustTier::Paired).await.unwrap();
    again.add_target(&r.addr_b, TrustTier::Paired).await.unwrap();
    assert!(again.has_unload_intent(&placed.instance_id), "the intent is on disk");
    for _ in 0..3 {
        past_backoff().await;
        again.lifecycle_tick().await;
    }
    assert_eq!(count(&r.a).await + count(&r.b).await, 0, "not placed again");
    assert!(again.placements().is_empty());
}

#[tokio::test]
async fn an_unload_of_an_instance_the_node_no_longer_holds_removes_the_placement() {
    let r = rig().await;
    let placed = r.plane.place(&pinned(&r)).await.unwrap().placed.unwrap();
    // The node blipped: it answers, but the instance is gone and unremembered.
    restart(&r.a).await;
    let e = r.plane.instance(method::UNLOAD, &placed.instance_id).await.unwrap_err();
    assert!(e.to_string().contains("no instance"), "{e}");
    assert!(r.plane.placements().is_empty(), "no placement left behind");
    assert!(r.plane.lifecycle_of(&placed.instance_id).is_none(), "no lives record left behind");
    let seen = passes(&r, 3).await;
    assert!(seen.is_empty(), "the lifecycle has nothing to do: {seen:?}");
    assert_eq!(count(&r.a).await + count(&r.b).await, 0);
}

#[tokio::test]
async fn an_unload_frees_memory_the_next_placement_sees_at_once() {
    use crate::node_facts_advert::sign_node_facts;
    use crate::node_registry::node_id_from_pubkey;
    use clawft_types::placement::NodeFacts;

    let tmp = tempfile::tempdir().unwrap();
    let pkg = package_toml(tmp.path(), "mem-cog", &[(arch(), "#!/bin/sh\nexec sleep 60\n")], None, "\n[resources]\nram_mb = 600\n");
    let small = package_toml(tmp.path().join("small").as_path(), "small-cog", &[(arch(), "#!/bin/sh\nexec sleep 60\n")], None, "\n[resources]\nram_mb = 100\n");
    let key = SigningKey::from_bytes(&[10; 32]);
    let node_key = SigningKey::from_bytes(&[31; 32]);
    let mem = |free: i64| {
        let mut caps = board_caps("pi5");
        caps.retain(|c| c.id.as_str() != "mem.system");
        caps.push(cap("mem.system").with_attr("free", free << 20));
        caps
    };
    let a = host_node(31, mem(300), true, &key);
    // The daemon's own re-probe: it publishes facts from the live state. Only
    // the node's teardown may call it here; nothing else refreshes the facts.
    let svc = a.svc.clone();
    let id = node_id_from_pubkey(&node_key.verifying_key().to_bytes());
    let seq = Arc::new(std::sync::atomic::AtomicU64::new(1));
    a.svc.set_facts_refresher(Arc::new(move || {
        let (svc, id, node_key, seq) = (svc.clone(), id.clone(), node_key.clone(), seq.clone());
        Box::pin(async move {
            let now = chrono::Utc::now().timestamp() as u64;
            let n = seq.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            let mut f = NodeFacts::new(id, now, 600, n);
            f.capabilities = mem(1000);
            svc.set_facts(sign_node_facts(&f, &node_key).unwrap());
        })
    }));
    let net = Killable::new();
    let addr = net.serve("a", a.svc.clone());
    let (plane, _chain) = controller(&key, net.clone());
    plane.add_target(&addr, TrustTier::Paired).await.unwrap();

    let order = mode_order(&pkg, RunMode::Listener, Some(&a.id));
    let placed = plane.place(&mode_order(&small, RunMode::Listener, Some(&a.id))).await.unwrap().placed.unwrap();
    assert!(plane.place(&order).await.unwrap().placed.is_none(), "600 MiB does not fit 300 MiB free");

    plane.instance(method::UNLOAD, &placed.instance_id).await.unwrap();
    let again = plane.place(&order).await.unwrap();
    assert!(again.placed.is_some(), "{}", again.explain);
}
