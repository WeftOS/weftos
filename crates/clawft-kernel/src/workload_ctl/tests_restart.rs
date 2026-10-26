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
    host_with(root, ex, Some(chain), journal, &ctl_key(), None)
}

/// [`host`] with its own controller key, and optionally no chain.
fn host_with(
    root: &Path,
    ex: &Arc<ArtifactExchange>,
    chain: Option<&Arc<ChainManager>>,
    journal: &Path,
    controller: &SigningKey,
    gate_override: Option<Arc<crate::workload_governance::WorkloadGate>>,
) -> Arc<WorkloadHostService> {
    let chain_given = chain.is_some();
    let own = Arc::new(ChainManager::new(0, 1000));
    let chain = chain.unwrap_or(&own);
    let key = SigningKey::from_bytes(&[NODE_SEED; 32]);
    let id = node_id_from_pubkey(&key.verifying_key().to_bytes());
    let gate = gate_override.unwrap_or_else(|| gate(chain));
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
    let mut svc = WorkloadHostService::new(key, ex.clone(), anchors(), gate)
        .with_route("native", wh)
        .with_controllers(vec![controller.verifying_key().to_bytes()]);
    // No chain on the service when the test asks for none (its host adapter keeps its own).
    if chain_given {
        svc = svc.with_chain(chain.clone());
    }
    let svc = svc.with_journal(journal);
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
    place_for(rig, svc, None).await
}

/// [`place`] for `project`.
async fn place_for(rig: &Rig, svc: &Arc<WorkloadHostService>, project: Option<&str>) -> (String, PlacementControlPlane) {
    let conn = Arc::new(MeshConnector::new(false));
    let addr = conn.register_local("restart-node", svc.clone());
    let (plane, _) = controller(&ctl_key(), conn);
    plane.add_target(&addr, TrustTier::Pinned).await.unwrap();
    let mut order = mode_order(&rig.pkg, crate::workload_runtime::RunMode::Listener, None);
    order.project_id = project.map(String::from);
    let r = plane.place(&order).await.unwrap();
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

/// An empty artifact store for the rig's node, as if the real one did not come up.
fn empty_store(rig: &Rig) -> Arc<ArtifactExchange> {
    let id = node_id_from_pubkey(&SigningKey::from_bytes(&[NODE_SEED; 32]).verifying_key().to_bytes());
    exchange(&id, &rig.chain)
}

fn entries_of(rig: &Rig) -> Vec<JournalEntry> {
    let k = SigningKey::from_bytes(&[NODE_SEED; 32]);
    let id = node_id_from_pubkey(&k.verifying_key().to_bytes());
    Journal::open(&rig.journal(), &id, &k).entries()
}

#[tokio::test]
async fn a_restore_that_fails_for_a_transient_reason_is_kept_and_retried_at_the_next_start() {
    let rig = Rig::new();
    let iid = {
        let a = rig.host();
        let (iid, _plane) = place(&rig, &a).await;
        rig.wait_launches(1).await;
        iid
    };
    // First start: the artifact store is not there yet (no peers to fetch from).
    let empty = empty_store(&rig);
    let b = host(&rig.root(), &empty, &rig.chain, &rig.journal());
    let report = b.restore_instances().await;
    assert!(report.restored.is_empty(), "{report:?}");
    assert_eq!(report.failed.len(), 1, "{report:?}");
    assert!(report.failed[0].1.contains("will retry at the next start (1 of 5)"), "{report:?}");
    let kept = entries_of(&rig);
    assert_eq!((kept.len(), kept[0].failures, kept[0].desired_running), (1, 1, true), "kept, counted, intent unchanged");

    // Next start: the store is back, and the same instance comes up and its count resets.
    drop(b);
    let c = rig.host();
    let report = c.restore_instances().await;
    assert_eq!(report.restored, std::slice::from_ref(&iid), "{report:?}");
    assert_eq!(entries_of(&rig)[0].failures, 0);
    rig.wait_launches(2).await;
}

#[tokio::test]
async fn a_restore_that_keeps_failing_is_given_up_after_five_starts() {
    let rig = Rig::new();
    {
        let a = rig.host();
        place(&rig, &a).await;
    }
    let empty = empty_store(&rig);
    for n in 1..=5u32 {
        let b = host(&rig.root(), &empty, &rig.chain, &rig.journal());
        let report = b.restore_instances().await;
        assert_eq!(report.failed.len(), 1, "start {n}: {report:?}");
        assert_eq!(entries_of(&rig).len(), usize::from(n < 5), "start {n}");
    }
}

#[tokio::test]
async fn an_older_copy_of_the_journal_is_refused_even_though_it_is_validly_signed() {
    let rig = Rig::new();
    let (iid, stale) = {
        let a = rig.host();
        let (iid, plane) = place(&rig, &a).await;
        rig.wait_launches(1).await;
        // the journal as it was while the cog was meant to be running
        let stale = std::fs::read(rig.journal()).unwrap();
        plane.instance(method::STOP, &iid).await.unwrap();
        (iid, stale)
    };
    // an attacker puts the old file back
    std::fs::write(rig.journal(), &stale).unwrap();
    let b = rig.host();
    let report = b.restore_instances().await;
    assert!(report.restored.is_empty(), "{report:?}");
    assert_eq!(report.failed[0].0, iid);
    assert!(report.failed[0].1.contains("older copy"), "{report:?}");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(rig.launches(), 1, "the stopped cog was not started");
    assert!(b.held().await.is_empty());
}

#[tokio::test]
async fn the_generation_goes_on_the_chain_and_survives_a_deleted_journal() {
    let rig = Rig::new();
    let gens = |rig: &Rig| -> Vec<u64> {
        rig.chain
            .tail(0)
            .iter()
            .filter(|e| e.kind == crate::chain::EVENT_KIND_WORKLOAD_JOURNAL)
            .filter_map(|e| e.payload.as_ref()?["generation"].as_u64())
            .collect()
    };
    {
        let a = rig.host();
        let (iid, plane) = place(&rig, &a).await;
        plane.instance(method::STOP, &iid).await.unwrap();
    }
    let g = gens(&rig);
    assert!(g.len() >= 2 && g.windows(2).all(|w| w[0] < w[1]), "one rising generation per save: {g:?}");
    // The file is deleted and a new instance journaled: the generation continues above the chain.
    std::fs::remove_file(rig.journal()).unwrap();
    let before = *g.last().unwrap();
    {
        let b = rig.host();
        place(&rig, &b).await;
    }
    assert!(*gens(&rig).last().unwrap() > before);
    // so the next start does not take the fresh file for a replay
    let c = rig.host();
    assert_eq!(c.restore_instances().await.restored.len(), 1);
}

#[tokio::test]
async fn without_a_chain_instances_are_restored_loaded_and_nothing_is_started() {
    let rig = Rig::new();
    let iid = {
        let a = rig.host();
        let (iid, _plane) = place(&rig, &a).await;
        rig.wait_launches(1).await;
        iid
    };
    let b = host_with(&rig.root(), &rig.ex, None, &rig.journal(), &ctl_key(), None);
    let report = b.restore_instances().await;
    assert_eq!(report.restored, std::slice::from_ref(&iid), "{report:?}");
    assert!(report.note.as_deref().is_some_and(|n| n.contains("no chain")), "{report:?}");
    assert_eq!(b.held().await, [(iid.clone(), false)]);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(rig.launches(), 1, "nothing was started");
    assert!(entries_of(&rig)[0].desired_running, "the operator's intent is not rewritten");
}

#[tokio::test]
async fn a_requester_the_policy_no_longer_allows_is_refused_and_dropped() {
    let rig = Rig::new();
    let iid = {
        let a = rig.host();
        place(&rig, &a).await.0
    };
    // The same node, now configured with a different controller.
    let other = SigningKey::from_bytes(&[77; 32]);
    let b = host_with(&rig.root(), &rig.ex, Some(&rig.chain), &rig.journal(), &other, None);
    let report = b.restore_instances().await;
    assert!(report.restored.is_empty(), "{report:?}");
    assert_eq!(report.failed[0].0, iid);
    assert!(report.failed[0].1.contains("no longer a controller"), "{report:?}");
    assert!(entries_of(&rig).is_empty());
    assert!(b.held().await.is_empty());
}

#[test]
fn the_journal_file_is_created_private_even_over_a_wide_leftover_temp_file() {
    use super::host_service::{CtlConfig, PlaceBody};
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("journal.json");
    // a leftover temp file with a wide mode, as a crashed earlier save could leave
    let leftover = p.with_extension("json.tmp");
    std::fs::write(&leftover, b"old").unwrap();
    std::fs::set_permissions(&leftover, std::fs::Permissions::from_mode(0o644)).unwrap();
    let k = SigningKey::from_bytes(&[5; 32]);
    Journal::open(&p, "node", &k).record(JournalEntry {
        instance_id: "i1".into(),
        decision_id: None,
        requester: "ctl".into(),
        body: PlaceBody {
            name: "c".into(),
            manifest_hash: "0".repeat(64),
            variant: "v".into(),
            config: CtlConfig { mode: crate::workload_runtime::RunMode::Listener, args: vec![], csi_port: 1 },
            start: true,
            project_id: None,
        },
        desired_running: true,
        failures: 0,
    });
    let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "the saved file is owner-only");
    assert!(!leftover.exists(), "the wide temp file was replaced, not written into");
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
    assert!(!p.exists());
    assert_eq!(bad_files(tmp.path()).len(), 1, "set aside under a timestamped name");
}

fn bad_files(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().unwrap().to_string_lossy().starts_with("journal.json.bad."))
        .collect();
    v.sort();
    v
}

#[test]
fn unreadable_journals_are_set_aside_without_overwriting_and_only_the_newest_few_are_kept() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("journal.json");
    let k = SigningKey::from_bytes(&[5; 32]);
    for n in 0..5 {
        std::fs::write(&p, format!("{{ damaged {n}").as_bytes()).unwrap();
        Journal::open(&p, "node", &k);
        std::thread::sleep(Duration::from_millis(5));
    }
    let kept = bad_files(tmp.path());
    assert_eq!(kept.len(), 3, "{kept:?}");
    let texts: Vec<String> = kept.iter().map(|f| std::fs::read_to_string(f).unwrap()).collect();
    assert_eq!(texts, ["{ damaged 2", "{ damaged 3", "{ damaged 4"], "the newest three, none overwritten");
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
    rig.edit_journal(|v| v["entries"][0]["requester"] = "someone-else".into());
    let b = rig.host();
    let report = b.restore_instances().await;
    assert!(report.restored.is_empty(), "{report:?}");
    assert_eq!(report.failed.len(), 1, "{report:?}");
    assert_eq!(report.failed[0].0, iid);
    assert!(report.failed[0].1.contains("signature"), "{report:?}");
    // labelled for what it is, not as a replay of an older file
    assert_eq!(b.journal_status().unwrap().state, "refused (signature)");
    assert!(report.note.as_deref().is_some_and(|n| n.contains("refused (signature)") && !n.contains("older copy")), "{report:?}");
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
        assert_eq!(v["entries"][0]["desired_running"], false);
        v["entries"][0]["desired_running"] = true.into();
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
    rig.edit_journal(|v| v["sig"] = "".into());
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
        failures: 0,
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

const PROJECT_A: &str = "01J9ZXW0PRJCTAAAAAAAAAAAAA";
const PROJECT_B: &str = "01J9ZXW0PRJCTBBBBBBBBBBBBB";

#[tokio::test]
async fn the_same_package_placed_for_another_project_does_not_see_the_first_projects_data() {
    let rig = Rig::new();
    let a_iid = {
        let a = rig.host();
        let (iid, _plane) = place_for(&rig, &a, Some(PROJECT_A)).await;
        rig.wait_launches(1).await;
        std::fs::write(rig.root().join(&iid).join("data/secret"), b"project A only").unwrap();
        iid
    };
    // The daemon restarted, and the same package and config are now placed for project B
    // (no unload in between: A's directory is still on disk).
    let b = rig.host();
    let (b_iid, _plane) = place_for(&rig, &b, Some(PROJECT_B)).await;
    assert_ne!(a_iid, b_iid, "the project is part of the instance id");
    assert!(
        !rig.root().join(&b_iid).join("data/secret").exists(),
        "B's data/ is its own, without A's files"
    );
    assert_eq!(
        std::fs::read(rig.root().join(&a_iid).join("data/secret")).unwrap(),
        b"project A only",
        "and A's directory was not touched"
    );
    // The same project placed again lands in its own directory and keeps its data.
    drop((b, _plane));
    let c = rig.host();
    let (again, _plane) = place_for(&rig, &c, Some(PROJECT_A)).await;
    assert_eq!(again, a_iid);
    assert_eq!(std::fs::read(rig.root().join(&a_iid).join("data/secret")).unwrap(), b"project A only");
}

#[tokio::test]
async fn unloading_for_one_project_then_placing_for_another_starts_with_no_data() {
    let rig = Rig::new();
    let a = rig.host();
    let (a_iid, plane) = place_for(&rig, &a, Some(PROJECT_A)).await;
    rig.wait_launches(1).await;
    std::fs::write(rig.root().join(&a_iid).join("data/secret"), b"project A only").unwrap();
    plane.instance(method::UNLOAD, &a_iid).await.unwrap();
    let (b_iid, _plane) = place_for(&rig, &a, Some(PROJECT_B)).await;
    let data = rig.root().join(&b_iid).join("data");
    assert!(data.is_dir() && !data.join("secret").exists());
    assert!(std::fs::read_dir(&data).unwrap().all(|e| e.unwrap().file_name() != "secret"));
}

#[tokio::test]
async fn a_restart_restores_each_project_into_its_own_instance() {
    let rig = Rig::new();
    let (ia, ib) = {
        let h = rig.host();
        let (ia, _pa) = place_for(&rig, &h, Some(PROJECT_A)).await;
        let (ib, _pb) = place_for(&rig, &h, Some(PROJECT_B)).await;
        rig.wait_launches(2).await;
        (ia, ib)
    };
    assert_ne!(ia, ib);
    let h = rig.host();
    let mut restored = h.restore_instances().await.restored;
    restored.sort();
    let mut want = vec![ia.clone(), ib.clone()];
    want.sort();
    assert_eq!(restored, want);
}

fn jsub(tmp: &Path) -> PathBuf {
    tmp.join("journal.json")
}

#[tokio::test]
async fn a_journal_the_chain_has_never_heard_of_is_restored_loaded_and_not_started() {
    let rig = Rig::new();
    let iid = {
        let a = rig.host();
        let (iid, _plane) = place(&rig, &a).await;
        rig.wait_launches(1).await;
        iid
    };
    // The chain is present but empty of journal events: a rolled-back or replaced chain.
    let fresh = Arc::new(ChainManager::new(0, 1000));
    let b = host_with(&rig.root(), &rig.ex, Some(&fresh), &rig.journal(), &ctl_key(), None);
    let report = b.restore_instances().await;
    assert_eq!(report.restored, std::slice::from_ref(&iid), "{report:?}");
    assert!(report.note.as_deref().is_some_and(|n| n.contains("no record of this journal")), "{report:?}");
    assert_eq!(b.held().await, [(iid, false)]);
    assert_eq!(b.journal_status().unwrap().state, "unverified");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(rig.launches(), 1, "nothing was auto-started");
}

#[tokio::test]
async fn a_peer_cannot_put_a_journal_generation_on_the_chain_and_foreign_events_do_not_count() {
    assert!(crate::chain::is_caller_reserved_kind(crate::chain::EVENT_KIND_WORKLOAD_JOURNAL));
    // Replication refuses the reserved kind.
    let other = ChainManager::new(0, 1000);
    let ev = other.append("workload.host", crate::chain::EVENT_KIND_WORKLOAD_JOURNAL, Some(serde_json::json!({"node": "x", "generation": 999})));
    let mine = ChainManager::new(0, 1000);
    assert!(matches!(mine.append_signed(ev), Err(crate::chain::AppendSignedError::ReservedSource { .. })));

    // An event for another node id on this node's chain is not this node's generation.
    let rig = Rig::new();
    {
        let a = rig.host();
        let (_iid, _plane) = place(&rig, &a).await;
    }
    rig.chain.append(
        "workload.host",
        crate::chain::EVENT_KIND_WORKLOAD_JOURNAL,
        Some(serde_json::json!({"node": "some-other-node", "generation": 9999})),
    );
    let b = rig.host();
    let report = b.restore_instances().await;
    assert_eq!(report.restored.len(), 1, "{report:?}");
    assert!(report.note.is_none(), "{report:?}");
}

#[test]
fn a_generation_at_the_end_of_its_range_is_not_wrapped_and_nothing_is_saved() {
    use super::host_service::{CtlConfig, PlaceBody};
    let tmp = tempfile::tempdir().unwrap();
    let p = jsub(tmp.path());
    let k = SigningKey::from_bytes(&[5; 32]);
    let j = Journal::open(&p, "node", &k);
    j.set_generation(u64::MAX);
    j.record(JournalEntry {
        instance_id: "i1".into(),
        decision_id: None,
        requester: "ctl".into(),
        body: PlaceBody {
            name: "c".into(),
            manifest_hash: "0".repeat(64),
            variant: "v".into(),
            config: CtlConfig { mode: crate::workload_runtime::RunMode::Listener, args: vec![], csi_port: 1 },
            start: true,
            project_id: None,
        },
        desired_running: true,
        failures: 0,
    });
    assert!(!p.exists(), "refused: no file was written at a wrapped generation");
}

#[tokio::test]
async fn a_deleted_journal_is_reported_as_missing_not_as_an_empty_node() {
    let rig = Rig::new();
    {
        let a = rig.host();
        place(&rig, &a).await;
    }
    std::fs::remove_file(rig.journal()).unwrap();
    let b = rig.host();
    let report = b.restore_instances().await;
    assert!(report.restored.is_empty(), "{report:?}");
    assert!(report.note.as_deref().is_some_and(|n| n.starts_with("journal missing")), "{report:?}");
    let st = b.journal_status().unwrap();
    assert_eq!(st.state, "missing");
    assert!(st.chain_generation.unwrap() > 0 && st.file_generation == 0);

    // and the status listing an operator reads says so
    let req = super::msg::CtlRequest {
        version: super::msg::CTL_VERSION,
        method: method::STATUS.to_string(),
        requester: b.node_id.clone(),
        target: b.node_id.clone(),
        nonce: "0".repeat(32),
        issued_at_ms: 0,
        expires_at_ms: u64::MAX,
        decision_id: None,
        body: serde_json::json!({"include_departed": true}),
    };
    let rows = b.instance_op(&req).await.unwrap();
    assert!(rows.as_array().unwrap().iter().any(|r| r["journal"]["state"] == "missing"), "{rows}");
}

#[tokio::test]
async fn an_unreadable_journal_is_reported_too_when_the_chain_says_there_was_one() {
    let rig = Rig::new();
    {
        let a = rig.host();
        place(&rig, &a).await;
    }
    std::fs::write(rig.journal(), b"{ garbage").unwrap();
    let b = rig.host();
    let report = b.restore_instances().await;
    assert!(report.restored.is_empty());
    assert!(report.note.as_deref().is_some_and(|n| n.starts_with("journal unreadable")), "{report:?}");
    assert_eq!(b.journal_status().unwrap().state, "unreadable");
}

#[tokio::test]
async fn an_unreadable_revocation_list_is_transient_but_a_governance_denial_is_permanent() {
    let rig = Rig::new();
    let iid = {
        let a = rig.host();
        place(&rig, &a).await.0
    };
    // The node's revocation file is bad at this start: the gate fails closed.
    let dir = tempfile::tempdir().unwrap();
    let bad = gate_unreadable_list(&rig.chain, dir.path());
    let b = host_with(&rig.root(), &rig.ex, Some(&rig.chain), &rig.journal(), &ctl_key(), Some(bad));
    let report = b.restore_instances().await;
    assert!(report.restored.is_empty(), "{report:?}");
    assert!(report.failed[0].1.contains("will retry at the next start"), "{report:?}");
    assert_eq!(entries_of(&rig).len(), 1, "kept for the next start");
    drop(b);

    // Fixed at the next start: the instance comes back.
    let c = rig.host();
    assert_eq!(c.restore_instances().await.restored, std::slice::from_ref(&iid));
    drop(c);

    // A real denial (no permit) is a verdict: dropped.
    let d = host_with(&rig.root(), &rig.ex, Some(&rig.chain), &rig.journal(), &ctl_key(), Some(gate_no_permit(&rig.chain)));
    let report = d.restore_instances().await;
    assert!(report.restored.is_empty(), "{report:?}");
    assert!(!report.failed[0].1.contains("will retry"), "{report:?}");
    assert!(entries_of(&rig).is_empty(), "dropped");
}

#[test]
fn the_directory_is_flushed_after_the_journal_rename() {
    // A saved journal leaves no temp file and a readable, verified file; the directory
    // sync itself cannot be observed here, but it must not fail on a real directory.
    let tmp = tempfile::tempdir().unwrap();
    super::host_journal::sync_dir_for_test(tmp.path()).unwrap();
}
