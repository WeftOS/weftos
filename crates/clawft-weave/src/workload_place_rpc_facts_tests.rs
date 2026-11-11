//! Card b71cd973 (WO-143): the daemon wires its node-facts refresher onto the
//! workload host, so a stop or unload publishes fresh facts before it answers.
//! The kernel tests cover the hook with a stand-in; this drives the daemon's
//! own wiring and `node_facts_rpc::refresh_live_now` against the cache that
//! `cluster.facts` reads. Needs a process of its own (`node_facts_rpc::init`
//! sets process-wide state), which nextest gives each test.

use std::sync::Arc;
use std::time::{Duration, Instant};

use clawft_kernel::cluster::{ClusterConfig, ClusterMembership};
use clawft_kernel::node_facts_advert::sign_node_facts;
use clawft_kernel::workload_ctl::{MeshConnector, PlacementControlPlane};
use clawft_kernel::workload_governance::NodeTrustTier;
use clawft_kernel::workload_runtime::native::host_arch;
use clawft_kernel::workload_runtime::{NativeConfig, NativeRuntime, WorkloadHost};
use clawft_types::placement::{AttrValue, NodeFacts, TrustTier};
use ed25519_dalek::SigningKey;
use serde_json::json;

use super::tests::{anchors, cap, exchange, gate, package};
use super::*;
use crate::node_facts_rpc::{self, FactsEntry, LIVE_PROBE_SECS};

/// What `cluster.facts` returns for this daemon's own node.
fn local_entry(m: &ClusterMembership) -> Option<FactsEntry> {
    node_facts_rpc::facts_entries(m, None).into_iter().find(|e| e.local)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[tokio::test]
async fn an_unload_publishes_the_nodes_facts_before_it_answers_not_a_probe_interval_later() {
    let tmp = tempfile::tempdir().unwrap();
    let run = tmp.path().join("run");
    std::fs::create_dir_all(&run).unwrap();
    // No container engine probe: the test is about the refresh, not the engine.
    std::fs::write(run.join(node_facts_rpc::CONFIG_FILE), r#"{"docker_probe_image": null}"#).unwrap();

    let ctl = SigningKey::from_bytes(&[3; 32]);
    let node_key = SigningKey::from_bytes(&[4; 32]);
    let node_id = clawft_kernel::node_id_from_pubkey(&node_key.verifying_key().to_bytes());
    let membership = Arc::new(ClusterMembership::new(ClusterConfig::default()));

    // The daemon's facts: the first probe is cached at once, the next tick is
    // a probe interval away, so nothing else publishes during this test.
    node_facts_rpc::init(node_key.clone(), run.clone(), membership.clone(), None, None);
    let started = Instant::now();
    let first = loop {
        if let Some(e) = local_entry(&membership) {
            break e;
        }
        assert!(started.elapsed() < Duration::from_secs(30), "the first probe never landed");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    // A workload host for that node, wired the way the daemon wires it.
    let chain = Arc::new(ChainManager::new(0, 1000));
    let g = gate(&chain);
    let rt = NativeRuntime::new(NativeConfig { root: tmp.path().join("inst"), run_as: None, allow_interpreted: true });
    let host = WorkloadHost::new(Arc::new(rt), g.clone(), node_id.clone(), NodeTrustTier::Paired)
        .with_chain(chain.clone());
    let arch = host_arch().unwrap();
    let mut facts = NodeFacts::new(node_id.clone(), now_secs(), 600, 1);
    facts.capabilities = vec![
        cap(&format!("cpu.arch.{arch}")),
        cap("os.linux"),
        cap("runtime.native").with_attr("arches_native", AttrValue::List(vec![AttrValue::from(arch)])),
        cap("mem.system").with_attr("free", 1i64 << 32),
    ];
    let svc = Arc::new(
        WorkloadHostService::new(node_key.clone(), exchange(&chain), anchors(&ctl), g.clone())
            .with_route("native", Arc::new(host))
            .with_controllers(vec![ctl.verifying_key().to_bytes()])
            .with_chain(chain.clone()),
    );
    svc.set_facts(sign_node_facts(&facts, &node_key).unwrap());
    wire_facts_refresher(&svc, membership.clone());

    let conn = Arc::new(MeshConnector::new(false));
    let addr = conn.register_local("board", svc);
    let plane = PlacementControlPlane::new(ctl.clone(), g, chain.clone(), exchange(&chain), anchors(&ctl), conn);
    plane.add_target(&addr, TrustTier::Paired).await.unwrap();

    let pkg = package(&tmp.path().join("pkgsrc"), &ctl);
    let placed = route(&plane, "workload.place", json!({ "package_dir": pkg, "mode": "listener", "csi_port": 15043 })).await;
    let iid = placed.result.expect("placed")["placed"]["instance_id"].as_str().unwrap().to_string();
    let after_place = local_entry(&membership).unwrap();
    assert_eq!(after_place.facts.seq, first.facts.seq, "placing publishes nothing; only a teardown does");

    assert!(route(&plane, "workload.stop", json!({ "instance_id": iid })).await.ok);
    let after_stop = local_entry(&membership).unwrap();
    assert!(after_stop.facts.seq > first.facts.seq, "the stop published fresh facts before it answered");

    // The unload: the cache the node-facts RPC reads has newer facts the
    // moment it returns, stamped no earlier than the unload began.
    let before_unload = now_secs();
    let t = Instant::now();
    assert!(route(&plane, "workload.unload", json!({ "instance_id": iid })).await.ok);
    let freed = local_entry(&membership).unwrap();
    assert!(freed.facts.seq > after_stop.facts.seq, "the unload published fresh facts before it answered");
    assert!(freed.received_at >= before_unload, "published after the unload began");
    assert!(freed.facts.issued_at >= before_unload, "probed after the unload began");
    assert!(
        freed.facts.capabilities.iter().any(|c| c.id.as_str() == "mem.system"),
        "the live memory reading is in the facts"
    );
    assert!(t.elapsed() < Duration::from_secs(LIVE_PROBE_SECS), "no wait for the re-probe tick");
}
