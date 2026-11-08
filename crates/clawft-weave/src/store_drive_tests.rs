use std::path::Path;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use weftos_store::audit::MemSink;
use weftos_store::service::{Config, OpenGate, Parties};
use weftos_store::{FsBackend, StoreName, StoreService};

use super::*;

const PROJ: &str = "01JBXK2M3N4P5Q6R7S8T9V0W1X";

#[derive(Default)]
struct Chain(Mutex<Vec<(String, Value)>>);
impl weftos_store::service::ChainSink for Chain {
    fn record(&self, k: &str, v: Value) {
        self.0.lock().unwrap().push((k.to_owned(), v));
    }
}

struct Rig {
    _d: tempfile::TempDir,
    svc: StoreService,
    sink: Arc<MemSink>,
    chain: Arc<Chain>,
    name: String,
    bucket: String,
}

fn rig() -> Rig {
    let d = tempfile::tempdir().unwrap();
    let be = Arc::new(FsBackend::open(d.path()).unwrap());
    let sink = Arc::new(MemSink::default());
    let chain = Arc::new(Chain::default());
    let svc = StoreService::open(Config { node_id: "pg-1".into(), state_path: None, clock: || 5 }, be.clone(), be, Arc::new(OpenGate), sink.clone(), chain.clone()).unwrap();
    let n = StoreName::project("local", PROJ).unwrap();
    svc.ensure_store(&n, "cli", Parties::defaults_for(&n)).unwrap();
    Rig { _d: d, svc, sink, chain, name: n.to_string(), bucket: n.bucket() }
}

impl Rig {
    fn seed(&self, key: &str, data: &str) {
        self.svc.operator_objects().put(&self.bucket, key, data.as_bytes(), false).unwrap();
    }
    fn text(&self, key: &str) -> Option<String> {
        self.svc.operator_objects().get(&self.bucket, key).ok().map(|d| String::from_utf8(d).unwrap())
    }
    fn call(&self, verb: &str, mut p: Value) -> Result<Value, String> {
        p["agent"] = json!("bob");
        p["host"] = json!("m2");
        run_with(&self.svc, &format!("store.drive.{verb}"), p, &Guard::none())
    }
    fn kinds(&self) -> Vec<String> {
        self.chain.0.lock().unwrap().iter().map(|(k, _)| k.clone()).filter(|k| k.starts_with("store.drive.")).collect()
    }
}

fn path(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

#[test]
fn the_verbs_carry_a_tree_down_edit_it_back_up_and_report_the_conditional_write() {
    let r = rig();
    r.seed("proj/a.txt", "alpha");
    r.seed("proj/dir/b.txt", "beta");
    let t = tempfile::tempdir().unwrap();
    let root = path(&t.path().join("tree"));

    let m = r.call("materialise", json!({"name": r.name, "prefix": "proj", "dest": root})).unwrap();
    assert_eq!(m["created"].as_array().unwrap().len(), 2);
    assert_eq!(std::fs::read_to_string(t.path().join("tree/a.txt")).unwrap(), "alpha");

    std::fs::write(t.path().join("tree/a.txt"), "alpha v2").unwrap();
    std::fs::write(t.path().join("tree/new.txt"), "new").unwrap();
    // watch is one scan per call and changes nothing
    let w = r.call("watch", json!({"root": root})).unwrap();
    let mut seen: Vec<String> = w["changes"].as_array().unwrap().iter().map(|c| c["path"].as_str().unwrap().to_owned()).collect();
    seen.sort();
    assert_eq!(seen, ["a.txt", "new.txt"]);
    assert_eq!(r.call("watch", json!({"root": root})).unwrap(), w, "a second scan sees the same thing");
    assert_eq!(r.text("proj/a.txt").as_deref(), Some("alpha"));

    let u = r.call("sync-up", json!({"name": r.name, "root": root})).unwrap();
    assert_eq!(u["put"].as_array().unwrap().len(), 2);
    // The daemon's wrapper forwards the conditional write: the modify went out as one.
    assert_eq!((u["conditional"].as_u64(), u["head_then_write"].as_u64()), (Some(1), Some(0)), "{u}");
    assert_eq!((r.text("proj/a.txt").as_deref(), r.text("proj/new.txt").as_deref()), (Some("alpha v2"), Some("new")));
    assert!(r.call("watch", json!({"root": root})).unwrap()["changes"].as_array().unwrap().is_empty());

    // A remote change comes down.
    r.seed("proj/dir/b.txt", "beta v2");
    let d = r.call("sync-down", json!({"root": root})).unwrap();
    assert_eq!(d["updated"], json!(["dir/b.txt"]));
    assert_eq!(std::fs::read_to_string(t.path().join("tree/dir/b.txt")).unwrap(), "beta v2");
}

#[test]
fn a_conflict_is_resolved_with_the_resolve_verb() {
    let r = rig();
    r.seed("proj/a.txt", "alpha");
    let t = tempfile::tempdir().unwrap();
    let root = path(&t.path().join("tree"));
    r.call("materialise", json!({"name": r.name, "prefix": "proj", "dest": root})).unwrap();
    std::fs::write(t.path().join("tree/a.txt"), "mine").unwrap();
    r.seed("proj/a.txt", "theirs");
    let u = r.call("sync-up", json!({"name": r.name, "root": root})).unwrap();
    assert_eq!(u["conflicts"], json!(["a.txt"]));
    assert_eq!(r.text("proj/a.txt").as_deref(), Some("theirs"), "the remote is left alone");
    assert_eq!(r.call("watch", json!({"root": root})).unwrap()["open_conflicts"], 1);

    assert!(r.call("resolve", json!({"name": r.name, "root": root, "path": "a.txt", "keep": "sideways"})).is_err());
    r.call("resolve", json!({"name": r.name, "root": root, "path": "a.txt", "keep": "local"})).unwrap();
    let u = r.call("sync-up", json!({"name": r.name, "root": root})).unwrap();
    assert_eq!(u["put"], json!(["a.txt"]));
    assert_eq!(r.text("proj/a.txt").as_deref(), Some("mine"));
}

#[test]
fn a_relative_dest_or_root_is_refused_by_the_path_guard() {
    let r = rig();
    for (verb, key) in [("materialise", "dest"), ("sync-down", "root"), ("sync-up", "root"), ("watch", "root"), ("resolve", "root")] {
        let e = r.call(verb, json!({"name": r.name, key: "relative/tree", "path": "x", "keep": "local"})).unwrap_err();
        assert!(e.contains("must be an absolute path"), "{verb}: {e}");
    }
    // and the daemon's own state is off limits, as for the S2 verbs
    let t = tempfile::tempdir().unwrap();
    let state = t.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    let g = Guard::new([state.clone()]);
    let e = run_with(&r.svc, "store.drive.materialise", json!({"name": r.name, "dest": path(&state.join("x"))}), &g).unwrap_err();
    assert!(e.contains("daemon's own state"), "{e}");
    assert!(!state.join("x").exists());
}

#[test]
fn a_read_only_call_refuses_sync_up_and_touches_nothing() {
    let r = rig();
    r.seed("proj/a.txt", "alpha");
    let t = tempfile::tempdir().unwrap();
    let root = path(&t.path().join("tree"));
    r.call("materialise", json!({"name": r.name, "prefix": "proj", "dest": root, "read_only": true})).unwrap();
    std::fs::write(t.path().join("tree/a.txt"), "edited").unwrap();
    let e = r.call("sync-up", json!({"name": r.name, "root": root, "read_only": true})).unwrap_err();
    assert!(e.contains("read-only"), "{e}");
    assert_eq!(r.text("proj/a.txt").as_deref(), Some("alpha"));
    // reads still work read-only
    r.call("sync-down", json!({"root": root, "read_only": true})).unwrap();
}

#[test]
fn secrets_are_not_uploaded_and_the_report_says_so() {
    let r = rig();
    r.seed("proj/a.txt", "alpha");
    let t = tempfile::tempdir().unwrap();
    let root = path(&t.path().join("tree"));
    r.call("materialise", json!({"name": r.name, "prefix": "proj", "dest": root})).unwrap();
    std::fs::write(t.path().join("tree/.env"), "TOKEN=1").unwrap();
    let u = r.call("sync-up", json!({"name": r.name, "root": root})).unwrap();
    assert!(u["put"].as_array().unwrap().is_empty());
    assert_eq!(u["skipped"][0]["why"], "secret", "{u}");
    assert!(r.text("proj/.env").is_none());
}

#[test]
fn a_drive_bound_to_another_store_is_refused() {
    let r = rig();
    let t = tempfile::tempdir().unwrap();
    let root = path(&t.path().join("tree"));
    r.call("materialise", json!({"name": r.name, "prefix": "proj", "dest": root})).unwrap();
    let other = StoreName::project("local", "01JBXK2M3N4P5Q6R7S8T9V0W2Y").unwrap().to_string();
    let e = r.call("sync-up", json!({"name": other, "root": root})).unwrap_err();
    assert!(e.contains("is bound to bucket"), "{e}");
    assert!(r.call("sync-up", json!({"root": root})).unwrap_err().contains("missing --name"));
}

#[test]
fn materialise_sync_up_and_resolve_are_in_the_activity_log_and_on_the_chain_and_reads_are_not() {
    let r = rig();
    r.seed("proj/a.txt", "alpha");
    let t = tempfile::tempdir().unwrap();
    let root = path(&t.path().join("tree"));
    r.call("materialise", json!({"name": r.name, "prefix": "proj", "dest": root})).unwrap();
    std::fs::write(t.path().join("tree/a.txt"), "mine").unwrap();
    r.seed("proj/a.txt", "theirs");
    r.call("sync-up", json!({"name": r.name, "root": root})).unwrap();
    r.call("resolve", json!({"name": r.name, "root": root, "path": "a.txt", "keep": "remote"})).unwrap();
    // a refused call is recorded too
    let e = r.call("sync-up", json!({"name": r.name, "root": root, "read_only": true})).unwrap_err();
    // reads record nothing
    r.call("sync-down", json!({"root": root})).unwrap();
    r.call("watch", json!({"root": root})).unwrap();

    assert_eq!(r.kinds(), ["materialise", "sync-up", "resolve", "sync-up"].map(|v| format!("store.drive.{v}")));
    let ev: Vec<(String, String, String)> = r.sink.events().into_iter().filter(|e| e.op.starts_with("Drive")).map(|e| (e.op, e.principal, e.outcome.text())).collect();
    assert_eq!(ev.len(), 4, "{ev:?}");
    assert!(ev.iter().any(|(op, who, out)| op == "DriveMaterialise" && who == "bob@m2" && out == "ok"));
    assert!(ev.iter().any(|(op, _, out)| op == "DriveSyncUp" && *out == format!("error:{e}")));
    assert!(ev.iter().any(|(op, _, _)| op == "DriveResolve"));
    // the chain payload carries counts, not file lists
    let ch = r.chain.0.lock().unwrap();
    let up = ch.iter().find(|(k, _)| k == "store.drive.sync-up").unwrap();
    assert!(up.1["result"]["conflicts"].is_number(), "{}", up.1);
}

#[test]
fn unknown_params_and_verbs_are_rejected() {
    let r = rig();
    assert!(r.call("materialise", json!({"name": r.name, "dest": "/x", "bogus": 1})).unwrap_err().contains("invalid params"));
    assert!(r.call("teleport", json!({})).unwrap_err().contains("unknown drive verb"));
    assert!(r.call("sync-down", json!({"root": "/a", "dest": "/b"})).unwrap_err().contains("differ"));
}
