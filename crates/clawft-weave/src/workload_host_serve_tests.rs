//! The daemon's `workload-host`, served over real mesh TCP with Noise XX to
//! another node's control plane (card 12: a mesh of WeftOS daemons places
//! onto each other, not onto an example binary).

use std::sync::Arc;

use clawft_kernel::chain::{ChainManager, EVENT_KIND_ARTIFACT_FETCH, EVENT_KIND_WORKLOAD_REFUSE};
use clawft_kernel::node_facts_advert::sign_node_facts;
use clawft_kernel::workload_ctl::{
    CtlConfig, FactsSource, MeshConnector, PlaceOrder, PlacementControlPlane,
};
use clawft_kernel::workload_pkg::codec::hex_encode;
use clawft_kernel::workload_runtime::RunMode;
use clawft_kernel::workload_runtime::native::host_arch;
use clawft_types::placement::{AttrValue, NodeFacts, TrustTier};
use ed25519_dalek::SigningKey;
use serde_json::json;

use super::*;
use crate::workload_place_rpc::route;
use crate::workload_place_rpc::tests::{anchors, cap, exchange, gate, package};

fn cfg(controllers: Vec<String>) -> HostConfig {
    HostConfig {
        listen: "127.0.0.1:0".into(),
        noise: true,
        controllers,
        advertise: Some("board.test:9471".into()),
        lease_secs: None,
    }
}

#[test]
fn serving_config_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    assert!(load_host_config(dir.path()).unwrap().is_none(), "off by default");
    let ok = hex_encode(&[7u8; 32]);
    for bad in [
        json!({ "listen": "0.0.0.0:9471", "controllers": [] }),
        json!({ "listen": "0.0.0.0:9471", "controllers": ["abc"] }),
        json!({ "listen": "not an addr", "controllers": [ok] }),
        json!({ "listen": "0.0.0.0:9471", "controllers": [ok], "advertise": "a b" }),
        json!({ "listen": "0.0.0.0:9471", "controllers": [ok], "extra": 1 }),
    ] {
        std::fs::write(dir.path().join(HOST_FILE), bad.to_string()).unwrap();
        assert!(load_host_config(dir.path()).is_err(), "{bad}");
    }
    let good = json!({ "listen": "0.0.0.0:9471", "controllers": [ok] });
    std::fs::write(dir.path().join(HOST_FILE), good.to_string()).unwrap();
    let c = load_host_config(dir.path()).unwrap().unwrap();
    assert!(c.noise, "noise defaults on");
    assert_eq!(c.advertised(), "0.0.0.0:9471");
}

struct Daemon {
    id: String,
    chain: Arc<ChainManager>,
    addr: String,
    _tmp: tempfile::TempDir,
}

/// A daemon-built host (as `workload_place_rpc::build` makes it) served
/// on a loopback port, answering `controller`.
async fn served_daemon(controller: &SigningKey) -> Daemon {
    let tmp = tempfile::tempdir().unwrap();
    let key = SigningKey::from_bytes(&[5; 32]);
    let id = clawft_kernel::node_id_from_pubkey(&key.verifying_key().to_bytes());
    let chain = Arc::new(ChainManager::new(0, 1000));
    let arch = host_arch().unwrap();
    let mut facts = NodeFacts::new(id.clone(), chrono::Utc::now().timestamp() as u64, 600, 1);
    facts.capabilities = vec![
        cap(&format!("cpu.arch.{arch}")),
        cap("os.linux"),
        cap("runtime.native").with_attr(
            "arches_native",
            AttrValue::List(vec![AttrValue::from(arch)]),
        ),
        cap("mem.system").with_attr("free", 1i64 << 32),
    ];
    let signed = sign_node_facts(&facts, &key).unwrap();
    let source: FactsSource = Arc::new(move || Some(signed.clone()));
    let serving = cfg(vec![hex_encode(&controller.verifying_key().to_bytes())]);
    let list = Arc::new(clawft_kernel::revocation::RevocationList::new(std::path::PathBuf::from(
        "unused/revoked_hosts.json",
    )));
    let svc = local_host(HostParts {
        key: &key,
        dir: tmp.path(),
        chain: &chain,
        gate: gate(&chain),
        exchange: exchange(&chain),
        anchors: anchors(controller),
        facts: source,
        serving: Some(&serving),
        container: None,
        revocations: list.clone(),
        ingest: None,
        computer: None,
    })
    .unwrap();
    // The daemon's host constructor wires the list into the service: without
    // it the place-race check and the revoked-start rollback never run.
    assert!(
        svc.revocations().is_some_and(|l| Arc::ptr_eq(l, &list)),
        "local_host must give the service the node's revocation list"
    );
    let bound = serve(&serving, Arc::new(svc)).await.unwrap();
    Daemon {
        id,
        chain,
        addr: bound.to_string(),
        _tmp: tmp,
    }
}

fn controller_plane(key: &SigningKey) -> (PlacementControlPlane, Arc<ChainManager>) {
    let chain = Arc::new(ChainManager::new(0, 1000));
    let p = PlacementControlPlane::new(
        key.clone(),
        gate(&chain),
        chain.clone(),
        exchange(&chain),
        anchors(key),
        Arc::new(MeshConnector::new(true)),
    );
    (p, chain)
}

#[tokio::test]
async fn another_nodes_controller_reaches_the_served_daemon_host_over_noise_tcp() {
    let ctl = SigningKey::from_bytes(&[3; 32]);
    let d = served_daemon(&ctl).await;
    let (plane, _chain) = controller_plane(&ctl);
    assert_eq!(plane.add_target(&d.addr, TrustTier::Paired).await.unwrap(), d.id);
    let adv = plane.hosts();
    assert_eq!(adv.len(), 1, "workload-host advertisement merged");
    assert_eq!(adv[0].node_id, d.id);
    assert_eq!(adv[0].metadata["addr"], "board.test:9471");
    assert!(adv[0].methods.iter().any(|m| m == "workload.place"));

    // A placement reaches it: the payload is fetched from the controller,
    // then the daemon's own admission self-check refuses a script (the
    // daemon never runs interpreted payloads), chained on the daemon.
    let tmp = tempfile::tempdir().unwrap();
    let pkg = package(tmp.path(), &ctl);
    let r = plane
        .place(&PlaceOrder {
            package_dir: pkg,
            config: CtlConfig {
                mode: RunMode::Listener,
                args: vec![],
                csi_port: 15008,
            },
            pin: None,
            prefer: vec![],
            avoid: vec![],
            allow_emulated: false,
            start: true,
            dry_run: false,
            project_id: None,
        })
        .await
        .unwrap();
    assert_eq!(r.decision.placement.as_ref().unwrap().node_id, d.id);
    assert_eq!(r.attempts[0].code.as_deref(), Some("admission"), "{}", r.explain);
    let tail = d.chain.tail(d.chain.len());
    assert!(tail.iter().any(|e| e.kind == EVENT_KIND_ARTIFACT_FETCH));
    assert!(tail.iter().any(|e| e.kind == EVENT_KIND_WORKLOAD_REFUSE
        && e.source == "workload.host"
        && e.payload.as_ref().is_some_and(|p| p["code"] == "admission")));
}

#[tokio::test]
async fn an_unlisted_controller_is_refused_by_the_served_host() {
    let ctl = SigningKey::from_bytes(&[3; 32]);
    let d = served_daemon(&ctl).await;
    let stranger = SigningKey::from_bytes(&[9; 32]);
    let (plane, _) = controller_plane(&stranger);
    let err = plane
        .add_target(&d.addr, TrustTier::Paired)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("unauthori"), "{err}");
    assert!(plane.targets().is_empty());
}

#[tokio::test]
async fn a_peer_named_in_a_request_is_only_discovered() {
    let ctl = SigningKey::from_bytes(&[3; 32]);
    let d = served_daemon(&ctl).await;
    let (plane, _) = controller_plane(&ctl);
    let tmp = tempfile::tempdir().unwrap();
    let pkg = package(tmp.path(), &ctl);
    let r = route(
        &plane,
        "workload.explain",
        json!({ "package_dir": pkg, "peers": [d.addr], "mode": "listener" }),
    )
    .await;
    let v = r.result.expect("explained");
    let t = plane.targets();
    assert_eq!((t.len(), t[0].tier), (1, TrustTier::Discovered));
    assert!(
        v["decision"]["placement"].is_null(),
        "governance needs an operator-assigned tier: {}",
        v["explain"]
    );
}
