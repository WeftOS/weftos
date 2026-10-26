//! A host daemon restart (card 5fb0932b): placed native cogs come back, a
//! cog that was stopped stays stopped, an unloaded one stays gone, and a
//! stage directory left on disk no longer blocks placing the cog again.
//! The "restart" builds a second host over the same instance root, artifact
//! store and journal file, as a restarted daemon has.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use clawft_types::placement::{NodeFacts, TrustTier};
use ed25519_dalek::SigningKey;

use super::host_journal::{Journal, JournalEntry};
use super::host_service::WorkloadHostService;
use super::msg::method;
use super::plane::PlacementControlPlane;
use super::test_support::*;
use super::transport::MeshConnector;
use crate::chain::ChainManager;
use crate::mesh_artifact::ArtifactExchange;
use crate::node_facts_advert::sign_node_facts;
use crate::node_registry::node_id_from_pubkey;
use crate::workload_runtime::{NativeConfig, NativeRuntime, WorkloadHost};

const NODE_SEED: u8 = 31;

fn ctl_key() -> SigningKey {
    SigningKey::from_bytes(&[30; 32])
}

/// The node `NODE_SEED` over `root` (instance dir), `ex` (artifact store) and
/// `journal`: what the daemon builds at every start.
fn host(root: &Path, ex: &Arc<ArtifactExchange>, chain: &Arc<ChainManager>, journal: &Path) -> Arc<WorkloadHostService> {
    let key = SigningKey::from_bytes(&[NODE_SEED; 32]);
    let id = node_id_from_pubkey(&key.verifying_key().to_bytes());
    let gate = gate(chain);
    let native = NativeRuntime::new(NativeConfig {
        root: root.to_path_buf(),
        run_as: None,
        allow_interpreted: true,
    });
    let wh = Arc::new(
        WorkloadHost::new(
            Arc::new(native),
            gate.clone(),
            id.clone(),
            crate::workload_governance::NodeTrustTier::Paired,
        )
        .with_chain(chain.clone()),
    );
    let now = chrono::Utc::now().timestamp() as u64;
    let mut facts = NodeFacts::new(id.clone(), now, 600, 1);
    facts.capabilities = board_caps("pi5");
    let signed = sign_node_facts(&facts, &key).unwrap();
    let svc = WorkloadHostService::new(key, ex.clone(), anchors(), gate)
        .with_route("native", wh)
        .with_controllers(vec![ctl_key().verifying_key().to_bytes()])
        .with_chain(chain.clone())
        .with_journal(journal);
    svc.set_facts(signed);
    Arc::new(svc)
}

struct Rig {
    tmp: tempfile::TempDir,
    pkg: PathBuf,
    runs: PathBuf,
    chain: Arc<ChainManager>,
    ex: Arc<ArtifactExchange>,
}

impl Rig {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let runs = tmp.path().join("runs");
        // Each launch appends a line, so a restart shows as a second one.
        // The cog also keeps state in its data directory: each launch appends to it.
        let script = format!(
            "#!/bin/sh\necho started >> '{}'\necho launch >> \"$COGNITUM_COG_DATA_DIR/state\"\nexec sleep 60\n",
            runs.display()
        );
        let pkg = package(tmp.path(), "restart-cog", &script, &[arch()]);
        let chain = Arc::new(ChainManager::new(0, 1000));
        let id = node_id_from_pubkey(&SigningKey::from_bytes(&[NODE_SEED; 32]).verifying_key().to_bytes());
        let ex = exchange(&id, &chain);
        Self { tmp, pkg, runs, chain, ex }
    }

    fn root(&self) -> PathBuf {
        self.tmp.path().join("instances")
    }

    fn journal(&self) -> PathBuf {
        self.tmp.path().join("journal.json")
    }

    fn host(&self) -> Arc<WorkloadHostService> {
        host(&self.root(), &self.ex, &self.chain, &self.journal())
    }

    fn launches(&self) -> usize {
        std::fs::read_to_string(&self.runs).map(|t| t.lines().count()).unwrap_or(0)
    }

    /// Change the journal file's JSON as a local attacker with write access would.
    fn edit_journal(&self, f: impl FnOnce(&mut serde_json::Value)) {
        let mut v: serde_json::Value = serde_json::from_slice(&std::fs::read(self.journal()).unwrap()).unwrap();
        f(&mut v);
        std::fs::write(self.journal(), serde_json::to_vec_pretty(&v).unwrap()).unwrap();
    }

    async fn wait_launches(&self, n: usize) {
        for _ in 0..1_500 {
            if self.launches() >= n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("expected {n} launches, saw {}", self.launches());
    }
}

/// Place the rig's cog on `svc` through a controller; returns the instance
/// id and the controller (drop it to drop the connection to `svc`).
async fn place(rig: &Rig, svc: &Arc<WorkloadHostService>) -> (String, PlacementControlPlane) {
    let conn = Arc::new(MeshConnector::new(false));
    let addr = conn.register_local("restart-node", svc.clone());
    let (plane, _) = controller(&ctl_key(), conn);
    plane.add_target(&addr, TrustTier::Pinned).await.unwrap();
    let r = plane
        .place(&mode_order(&rig.pkg, crate::workload_runtime::RunMode::Listener, None))
        .await
        .unwrap();
    let iid = r.placed.expect(&r.explain).instance_id;
    (iid, plane)
}

#[tokio::test]
async fn a_placed_cog_that_was_running_runs_again_after_a_restart() {
    let rig = Rig::new();
    let iid = {
        let a = rig.host();
        let (iid, _plane) = place(&rig, &a).await;
        rig.wait_launches(1).await;
        assert_eq!(a.held().await, [(iid.clone(), true)]);
        iid
    }; // the first daemon exits: its children are killed, its memory is gone
    assert!(rig.root().join(&iid).exists(), "the stage directory outlives the daemon");

    let b = rig.host();
    assert!(b.held().await.is_empty(), "a fresh host holds nothing until it restores");
    let report = b.restore_instances().await;
    assert_eq!(report.restored, std::slice::from_ref(&iid), "{report:?}");
    assert!(report.failed.is_empty());
    assert_eq!(b.held().await, [(iid.clone(), true)]);
    rig.wait_launches(2).await;
}

#[tokio::test]
async fn a_cog_the_operator_stopped_comes_back_loaded_but_not_running() {
    let rig = Rig::new();
    let iid = {
        let a = rig.host();
        let (iid, plane) = place(&rig, &a).await;
        rig.wait_launches(1).await;
        plane.instance(method::STOP, &iid).await.unwrap();
        iid
    };
    let b = rig.host();
    let report = b.restore_instances().await;
    assert_eq!(report.restored, std::slice::from_ref(&iid), "{report:?}");
    assert_eq!(b.held().await, [(iid, false)]);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(rig.launches(), 1, "a stopped cog is not started by a restart");
}

#[tokio::test]
async fn an_unloaded_cog_is_not_restored() {
    let rig = Rig::new();
    {
        let a = rig.host();
        let (iid, plane) = place(&rig, &a).await;
        plane.instance(method::UNLOAD, &iid).await.unwrap();
    }
    let b = rig.host();
    let report = b.restore_instances().await;
    assert!(report.restored.is_empty() && report.failed.is_empty(), "{report:?}");
    assert!(b.held().await.is_empty());
}

#[tokio::test]
async fn an_instance_whose_package_is_gone_is_reported_and_dropped_not_retried_forever() {
    let rig = Rig::new();
    let iid = {
        let a = rig.host();
        place(&rig, &a).await.0
    };
    // The artifact store did not survive: an empty one, and no peers to fetch from.
    let id = node_id_from_pubkey(&SigningKey::from_bytes(&[NODE_SEED; 32]).verifying_key().to_bytes());
    let empty = exchange(&id, &rig.chain);
    let b = host(&rig.root(), &empty, &rig.chain, &rig.journal());
    let report = b.restore_instances().await;
    assert!(report.restored.is_empty());
    assert_eq!(report.failed.len(), 1, "{report:?}");
    assert_eq!(report.failed[0].0, iid);
    assert!(Journal::open(&rig.journal(), &b.node_id, &b.key).entries().is_empty(), "dropped, so the next start is clean");
}

#[tokio::test]
async fn placing_over_a_stale_stage_directory_works_without_a_journal() {
    // The state the board was in: the directory is there, the host holds nothing.
    let rig = Rig::new();
    let iid = {
        let a = rig.host();
        let iid = place(&rig, &a).await.0;
        rig.wait_launches(1).await;
        iid
    };
    std::fs::remove_file(rig.journal()).unwrap();
    assert!(rig.root().join(&iid).join("data").exists());
    let b = rig.host();
    let (again, _plane) = place(&rig, &b).await;
    assert_eq!(again, iid, "same cog, same config, same instance id");
    rig.wait_launches(2).await;
}

#[test]
fn a_damaged_journal_is_set_aside_and_the_host_starts_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("journal.json");
    std::fs::write(&p, b"{ not json").unwrap();
    let k = SigningKey::from_bytes(&[5; 32]);
    let j = Journal::open(&p, "node", &k);
    assert!(j.entries().is_empty());
    assert!(!p.exists() && p.with_extension("json.bad").exists());
}

#[tokio::test]
async fn a_cogs_own_data_survives_a_restart_and_a_re_place() {
    let rig = Rig::new();
    let iid = {
        let a = rig.host();
        let (iid, _plane) = place(&rig, &a).await;
        rig.wait_launches(1).await;
        // state the cog wrote, and a file an operator put there
        std::fs::write(rig.root().join(&iid).join("data/note"), b"keep me").unwrap();
        iid
    };
    let data = rig.root().join(&iid).join("data");
    assert_eq!(std::fs::read_to_string(data.join("state")).unwrap(), "launch\n");

    let b = rig.host();
    assert_eq!(b.restore_instances().await.restored, std::slice::from_ref(&iid));
    rig.wait_launches(2).await;
    assert_eq!(std::fs::read(data.join("note")).unwrap(), b"keep me", "restore keeps data/");
    for _ in 0..200 {
        if std::fs::read_to_string(data.join("state")).is_ok_and(|t| t.lines().count() == 2) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(std::fs::read_to_string(data.join("state")).unwrap(), "launch\nlaunch\n");

    // and a fresh placement over the same directory, with no journal
    drop(b);
    std::fs::remove_file(rig.journal()).unwrap();
    let c = rig.host();
    let (again, _plane) = place(&rig, &c).await;
    assert_eq!(again, iid);
    assert_eq!(std::fs::read(data.join("note")).unwrap(), b"keep me", "so does a re-place");
}

#[tokio::test]
async fn an_entry_with_a_changed_requester_is_refused() {
    let rig = Rig::new();
    let iid = {
        let a = rig.host();
        place(&rig, &a).await.0
    };
    rig.edit_journal(|v| v["entries"][0]["entry"]["requester"] = "someone-else".into());
    let b = rig.host();
    let report = b.restore_instances().await;
    assert!(report.restored.is_empty(), "{report:?}");
    assert_eq!(report.failed.len(), 1, "{report:?}");
    assert_eq!(report.failed[0].0, iid);
    assert!(report.failed[0].1.contains("signature"), "{report:?}");
    assert!(b.held().await.is_empty());
    assert!(Journal::open(&rig.journal(), &b.node_id, &b.key).entries().is_empty(), "the refused entry is not kept");
}

#[tokio::test]
async fn a_stopped_instance_cannot_be_flipped_to_running_by_editing_the_file() {
    let rig = Rig::new();
    {
        let a = rig.host();
        let (iid, plane) = place(&rig, &a).await;
        rig.wait_launches(1).await;
        plane.instance(method::STOP, &iid).await.unwrap();
    }
    rig.edit_journal(|v| {
        assert_eq!(v["entries"][0]["entry"]["desired_running"], false);
        v["entries"][0]["entry"]["desired_running"] = true.into();
    });
    let b = rig.host();
    let report = b.restore_instances().await;
    assert!(report.restored.is_empty(), "{report:?}");
    assert!(report.failed[0].1.contains("signature"), "{report:?}");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(rig.launches(), 1, "nothing was started");
    assert!(b.held().await.is_empty());
}

#[tokio::test]
async fn unsigned_entries_and_entries_from_another_node_or_key_are_refused() {
    let rig = Rig::new();
    {
        let a = rig.host();
        place(&rig, &a).await;
    }
    // the signature removed
    rig.edit_journal(|v| v["entries"][0]["sig"] = "".into());
    let report = rig.host().restore_instances().await;
    assert!(report.restored.is_empty() && report.failed.len() == 1, "{report:?}");

    // a journal signed for another node id, or by another key, is not this node's
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("j.json");
    let (k1, k2) = (SigningKey::from_bytes(&[1; 32]), SigningKey::from_bytes(&[2; 32]));
    let rig2 = Rig::new();
    let body = {
        let a = rig2.host();
        place(&rig2, &a).await;
        let j = Journal::open(&rig2.journal(), &a.node_id, &a.key);
        j.entries()[0].body.clone()
    };
    let entry = |id: &str| JournalEntry {
        instance_id: id.into(),
        decision_id: None,
        requester: "ctl".into(),
        body: body.clone(),
        desired_running: true,
    };
    Journal::open(&p, "node-a", &k1).record(entry("i1"));
    assert_eq!(Journal::open(&p, "node-a", &k1).entries().len(), 1, "same node, same key: accepted");
    let other_node = Journal::open(&p, "node-b", &k1);
    assert!(other_node.entries().is_empty());
    assert_eq!(other_node.take_rejected().len(), 1);
    Journal::open(&p, "node-a", &k1).record(entry("i1"));
    let other_key = Journal::open(&p, "node-a", &k2);
    assert!(other_key.entries().is_empty());
    assert_eq!(other_key.take_rejected()[0].0, "i1");
}
