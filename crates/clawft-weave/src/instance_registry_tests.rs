//! Registry, chain, digest-enforcement and source tests (local chain, temp dirs).

use async_trait::async_trait;
use clawft_core::agent::spawn::{TaskOutcome, TaskStatus};

use super::*;
use crate::instance_digest::DigestError;
use crate::instance_sources::{InstanceSource, SUBAGENT_RUNNER, SourceInstance, SubagentSource, subagent_state};

fn setup() -> (Arc<ChainManager>, Arc<InstanceRegistry>) {
    let chain = Arc::new(ChainManager::new(0, 1000));
    (chain.clone(), InstanceRegistry::new(Some(chain)))
}

fn events(chain: &ChainManager) -> Vec<(String, Value)> {
    chain
        .tail(0)
        .into_iter()
        .filter(|e| e.source == CHAIN_SOURCE)
        .map(|e| (e.kind, e.payload.unwrap_or(Value::Null)))
        .collect()
}

fn pkg_dir() -> (tempfile::TempDir, String) {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("agent.md"), "# agent").unwrap();
    let digest = Sha256SortedFiles.digest(d.path()).unwrap();
    (d, digest)
}

fn installed(dir: &std::path::Path, digest: &str) -> NewInstance {
    NewInstance {
        runner: "harness-job".into(),
        package: Some(PackageRef { id: "agents/coder".into(), digest: digest.into() }),
        package_dir: Some(dir.to_path_buf()),
        project: Some("01PROJECT".into()),
        slot: Some("coder".into()),
        model_role: Some("worker".into()),
        ..Default::default()
    }
}

#[test]
fn lifecycle_is_recorded_and_chained() {
    let (chain, reg) = setup();
    reg.set_node("node-1");
    let (dir, digest) = pkg_dir();
    let id = reg.start(installed(dir.path(), &digest)).unwrap();
    let r = reg.get(&id).unwrap();
    assert_eq!((r.state, r.node.as_deref(), r.slot.as_deref()), (InstanceState::Starting, Some("node-1"), Some("coder")));

    assert!(reg.set_state(&id, InstanceState::Idle).unwrap());
    assert!(reg.set_state(&id, InstanceState::Working).unwrap());
    reg.set_card(&id, Some("0e56dc6f".into())).unwrap();
    reg.set_config(&id, "temperature=0").unwrap();
    reg.set_config(&id, "temperature=0").unwrap();
    reg.set_version(&id, Some("1.1.0".into()), None, Some("model-b".into())).unwrap();
    reg.stop(&id, "done").unwrap();
    reg.stop(&id, "done").unwrap();

    let r = reg.get(&id).unwrap();
    assert_eq!((r.state, r.card.as_deref()), (InstanceState::Exited, Some("0e56dc6f")));
    assert!(r.ended_at.is_some());
    assert!(!reg.set_state(&id, InstanceState::Working).unwrap(), "a finished instance does not move");

    let ev = events(&chain);
    let kinds: Vec<&str> = ev.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(kinds, [CHAIN_START, CHAIN_CONFIG, CHAIN_VERSION, CHAIN_STOP], "an unchanged config and a second stop chain nothing");
    assert_eq!(ev[0].1["digest_verified"], true);
    assert_eq!(ev[3].1["outcome"], "done");
    assert!(ev[1].1.to_string().contains("sha256:") && !ev[1].1.to_string().contains("temperature"), "only a config hash is chained");
}

#[test]
fn failure_is_chained_with_a_bounded_reason() {
    let (chain, reg) = setup();
    let id = reg.start(NewInstance { runner: "x".into(), ..Default::default() }).unwrap();
    reg.fail(&id, &"e".repeat(1000)).unwrap();
    assert_eq!(reg.get(&id).unwrap().state, InstanceState::Failed);
    let ev = events(&chain);
    assert_eq!(ev[1].0, CHAIN_FAIL);
    assert_eq!(ev[1].1["reason"].as_str().unwrap().len(), 200);
    assert_eq!(reg.set_card("nope", None), Err(UnknownInstance("nope".into())));
}

#[test]
fn a_tampered_package_is_refused_chained_and_reported() {
    let (chain, reg) = setup();
    let (dir, digest) = pkg_dir();
    std::fs::write(dir.path().join("agent.md"), "# agent, edited").unwrap();

    let err = reg.start(installed(dir.path(), &digest)).unwrap_err();
    let StartRefusal::Drift(DigestError::Mismatch { expected, actual }) = err else { panic!("{err:?}") };
    assert_eq!(expected, digest);
    assert_ne!(actual, digest);
    assert!(reg.list().is_empty(), "no instance exists for a refused start");

    let ev = events(&chain);
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0].0, CHAIN_DRIFT);
    assert_eq!(ev[0].1["drift"]["package"], "agents/coder");
    assert_eq!(ev[0].1["drift"]["reason"], "digest mismatch");
    assert_eq!(ev[0].1["refused"], true);
    let s = reg.summary();
    assert_eq!(s["drift"][0]["expected"], digest);
    assert_eq!(s["total"], 0);

    // Restoring the files clears the drift on the next successful start.
    std::fs::write(dir.path().join("agent.md"), "# agent").unwrap();
    reg.start(installed(dir.path(), &digest)).unwrap();
    assert!(reg.drift().is_empty());
}

#[test]
fn unreadable_missing_pin_and_malformed_pin_refuse() {
    let (_chain, reg) = setup();
    let (dir, digest) = pkg_dir();
    let gone = dir.path().join("gone");
    assert!(matches!(reg.start(installed(&gone, &digest)), Err(StartRefusal::Drift(DigestError::Unreadable(_)))));
    assert!(matches!(reg.start(installed(dir.path(), "not-a-digest")), Err(StartRefusal::Drift(DigestError::BadPin(_)))));
    let mut n = installed(dir.path(), &digest);
    n.package = None;
    assert_eq!(reg.start(n), Err(StartRefusal::NoPin));
    assert!(reg.list().is_empty());
    assert_eq!(reg.drift().len(), 1, "the unreadable and malformed attempts share one package entry");
}

#[test]
fn summary_has_counts_recent_live_first_and_no_transcripts() {
    let (_chain, reg) = setup();
    let done = reg.start(NewInstance { runner: "x".into(), ..Default::default() }).unwrap();
    reg.stop(&done, "done").unwrap();
    let live = reg.start(NewInstance { runner: "x".into(), restart_of: Some(done.clone()), ..Default::default() }).unwrap();
    reg.set_state(&live, InstanceState::Working).unwrap();
    let s = reg.summary();
    assert_eq!(s["total"], 2);
    assert_eq!(s["counts"]["working"], 1);
    assert_eq!(s["counts"]["exited"], 1);
    assert_eq!(s["recent"][0]["instance_id"], live.as_str());
    assert_eq!(s["recent"][0]["restart_of"], done.as_str());
    assert!(s["recent"][0].get("transcript").is_none());
}

#[test]
fn the_registry_is_bounded_and_evicts_finished_first() {
    let (_chain, reg) = setup();
    let keep = reg.start(NewInstance { runner: "x".into(), ..Default::default() }).unwrap();
    let first_done = reg.start(NewInstance { runner: "x".into(), ..Default::default() }).unwrap();
    reg.stop(&first_done, "done").unwrap();
    // keep + first_done + (MAX_RECORDS - 2) fill the registry; the last start evicts first_done.
    for _ in 0..MAX_RECORDS - 1 {
        reg.start(NewInstance { runner: "x".into(), ..Default::default() }).unwrap();
    }
    assert_eq!(reg.list().len(), MAX_RECORDS);
    assert!(reg.get(&keep).is_some());
    assert!(reg.get(&first_done).is_none());
}

struct Fake(Mutex<Vec<SourceInstance>>);

#[async_trait]
impl InstanceSource for Fake {
    fn runner(&self) -> &'static str {
        "fake"
    }
    async fn snapshot(&self) -> Vec<SourceInstance> {
        self.0.lock().unwrap().clone()
    }
}

fn item(key: &str, state: InstanceState, detail: Option<&str>) -> SourceInstance {
    SourceInstance { key: key.into(), new: NewInstance::default(), state, detail: detail.map(Into::into) }
}

#[tokio::test]
async fn a_source_registers_follows_and_ends_instances() {
    let (chain, reg) = setup();
    let src = Arc::new(Fake(Mutex::new(vec![item("a", InstanceState::Working, None), item("b", InstanceState::Working, None)])));
    reg.add_source(src.clone());
    reg.sync().await;
    reg.sync().await;
    assert_eq!(reg.list().len(), 2, "a second poll registers nothing new");
    assert!(reg.list().iter().all(|r| r.runner == "fake" && r.state == InstanceState::Working));

    *src.0.lock().unwrap() = vec![item("a", InstanceState::Failed, Some("boom"))];
    reg.sync().await;
    let by_state = |s| reg.list().into_iter().filter(|r| r.state == s).count();
    assert_eq!((by_state(InstanceState::Failed), by_state(InstanceState::Exited)), (1, 1), "a failed, b vanished");
    let kinds: Vec<String> = events(&chain).into_iter().map(|(k, _)| k).collect();
    assert_eq!(kinds, [CHAIN_START, CHAIN_START, CHAIN_FAIL, CHAIN_STOP]);
}

#[tokio::test]
async fn a_source_instance_whose_start_is_refused_is_not_retried() {
    let (chain, reg) = setup();
    let (dir, digest) = pkg_dir();
    std::fs::write(dir.path().join("agent.md"), "tampered").unwrap();
    let mut bad = item("a", InstanceState::Working, None);
    bad.new = installed(dir.path(), &digest);
    reg.add_source(Arc::new(Fake(Mutex::new(vec![bad]))));
    reg.sync().await;
    reg.sync().await;
    assert!(reg.list().is_empty());
    assert_eq!(events(&chain).len(), 1);
}

#[test]
fn subagent_statuses_map_to_instance_states() {
    assert_eq!(subagent_state(TaskStatus::Running, None).0, InstanceState::Working);
    assert_eq!(subagent_state(TaskStatus::Completed, None), (InstanceState::Exited, Some("done".into())));
    assert_eq!(subagent_state(TaskStatus::Cancelled, None), (InstanceState::Exited, Some("stopped".into())));
    assert_eq!(subagent_state(TaskStatus::Failed, Some("timeout")), (InstanceState::Failed, Some("timeout".into())));
    assert_eq!(subagent_state(TaskStatus::Denied, None).0, InstanceState::Failed);
}

#[tokio::test]
async fn the_subagent_source_reads_the_spawn_registry() {
    let spawn = Arc::new(clawft_service_agent::SpawnRegistry::new());
    spawn.insert("t1", "conv", "sub:conv:t1", None);
    let (_chain, reg) = setup();
    reg.add_source(Arc::new(SubagentSource { registry: spawn.clone() }));
    reg.sync().await;
    let l = reg.list();
    assert_eq!((l.len(), l[0].runner.as_str(), l[0].state), (1, SUBAGENT_RUNNER, InstanceState::Working));
    spawn.set_outcome(
        "t1",
        TaskOutcome {
            task_id: "t1".into(),
            child_conv_id: "sub:conv:t1".into(),
            status: TaskStatus::Completed,
            result: Some("the answer, which must not be reported".into()),
            finish_reason: None,
            iterations: 1,
            group_id: None,
            error: None,
        },
    );
    reg.sync().await;
    assert_eq!(reg.list()[0].state, InstanceState::Exited);
    assert!(!reg.summary().to_string().contains("the answer"), "results never reach the report");
}
