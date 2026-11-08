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
    d: tempfile::TempDir,
    svc: StoreService,
    sink: Arc<MemSink>,
    chain: Arc<Chain>,
    name: String,
    bucket: String,
}

fn rig() -> Rig {
    let d = tempfile::tempdir().unwrap();
    let be = Arc::new(FsBackend::open(&d.path().join("store")).unwrap());
    let sink = Arc::new(MemSink::default());
    let chain = Arc::new(Chain::default());
    let svc = StoreService::open(Config { node_id: "pg-1".into(), state_path: None, clock: || 5 }, be.clone(), be, Arc::new(OpenGate), sink.clone(), chain.clone()).unwrap();
    let n = StoreName::project("local", PROJ).unwrap();
    svc.ensure_store(&n, "cli", Parties::defaults_for(&n)).unwrap();
    let r = Rig { d, svc, sink, chain, name: n.to_string(), bucket: n.bucket() };
    r.seed("docs/readme.md", "read me");
    r.seed("proj/a.txt", "alpha");
    r.seed("proj/dir/b.txt", "beta");
    r
}

fn spec() -> Value {
    json!({"base": [{"prefix": "docs", "at": "docs"}], "work": {"prefix": "proj/"}})
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
        run_with(&self.svc, &format!("store.view.{verb}"), p, &Guard::none())
    }
    fn root(&self, n: &str) -> String {
        self.d.path().join(n).to_string_lossy().into_owned()
    }
    fn open(&self, root: &str) -> Value {
        self.call("open", json!({"name": self.name, "root": root, "spec": spec()})).unwrap()
    }
    fn kinds(&self) -> Vec<String> {
        self.chain.0.lock().unwrap().iter().map(|(k, _)| k.clone()).filter(|k| k.starts_with("store.view.")).collect()
    }
}

fn file(root: &str, rel: &str) -> std::path::PathBuf {
    Path::new(root).join(rel)
}

#[test]
fn open_status_close_round_trip() {
    let r = rig();
    let root = r.root("view");
    let o = r.open(&root);
    assert_eq!((o["files"].as_u64(), o["read_only"].as_bool()), (Some(3), Some(false)), "{o}");
    assert!(o["lease"]["id"].is_string() && o["spec_hash"].is_string(), "{o}");
    assert_eq!(std::fs::read_to_string(file(&root, "base/docs/readme.md")).unwrap(), "read me");
    assert_eq!(std::fs::read_to_string(file(&root, "work/a.txt")).unwrap(), "alpha");
    assert!(file(&root, "view.json").is_file());

    let s = r.call("status", json!({"root": root})).unwrap();
    assert_eq!(s["lease"]["id"], o["lease"]["id"]);
    assert!(s["pending"].as_array().unwrap().is_empty() && s["tampered"].as_array().unwrap().is_empty(), "{s}");
    assert_eq!((s["manifest"]["schema"].as_u64(), s["manifest"]["bucket"].as_str()), (Some(1), Some(r.bucket.as_str())), "status prints the view.json summary");
    assert_eq!(s["work"]["files"], 2);

    let c = r.call("close", json!({"name": r.name, "root": root, "discard": true})).unwrap();
    assert_eq!((c["mode"].as_str(), c["lease_released"].as_bool(), c["root_removed"].as_bool()), (Some("discard"), Some(true), Some(true)), "{c}");
    assert!(!Path::new(&root).exists());
    assert!(r.call("status", json!({"root": root})).unwrap_err().contains("not an open view"));
}

#[test]
fn an_edit_is_refreshed_published_and_reported_as_a_conditional_write() {
    let r = rig();
    let root = r.root("view");
    r.open(&root);
    std::fs::write(file(&root, "work/a.txt"), "alpha v2").unwrap();
    std::fs::write(file(&root, "work/new.txt"), "new").unwrap();
    let s = r.call("status", json!({"root": root})).unwrap();
    assert_eq!(s["pending"].as_array().unwrap().len(), 2, "{s}");

    r.seed("proj/dir/b.txt", "beta v2");
    r.seed("docs/readme.md", "read me v2");
    let f = r.call("refresh", json!({"root": root, "name": r.name})).unwrap();
    assert_eq!(f["work_updated"], json!(["dir/b.txt"]), "{f}");
    assert_eq!(std::fs::read_to_string(file(&root, "base/docs/readme.md")).unwrap(), "read me v2");
    assert_eq!(r.text("proj/a.txt").as_deref(), Some("alpha"), "refresh never writes the store");

    let p = r.call("publish", json!({"name": r.name, "root": root})).unwrap();
    assert_eq!(p["put"].as_array().unwrap().len(), 2, "{p}");
    assert_eq!((p["conditional"].as_u64(), p["head_then_write"].as_u64()), (Some(1), Some(0)), "{p}");
    assert_eq!((r.text("proj/a.txt").as_deref(), r.text("proj/new.txt").as_deref()), (Some("alpha v2"), Some("new")));
    assert!(r.call("status", json!({"root": root})).unwrap()["pending"].as_array().unwrap().is_empty());
    let c = r.call("close", json!({"name": r.name, "root": root, "publish": true})).unwrap();
    assert_eq!((c["mode"].as_str(), c["root_removed"].as_bool()), (Some("publish"), Some(true)), "{c}");
}

#[test]
fn publish_is_refused_on_a_read_only_view_and_no_lease_is_taken() {
    let r = rig();
    let root = r.root("ro");
    // read_only drops the work tree from a spec that has one
    let o = r.call("open", json!({"name": r.name, "root": root, "spec": spec(), "read_only": true})).unwrap();
    assert_eq!((o["read_only"].as_bool(), o["lease"].clone()), (Some(true), Value::Null), "{o}");
    assert!(!file(&root, "work/a.txt").exists());
    let e = r.call("publish", json!({"name": r.name, "root": root})).unwrap_err();
    assert!(e.contains("no work tree"), "{e}");
    // a spec with no work prefix is read-only on its own, and another view can still lease the prefix
    let r2 = r.root("ro2");
    assert_eq!(r.call("open", json!({"name": r.name, "root": r2, "spec": {"base": [{"prefix": "docs"}]}})).unwrap()["read_only"], true);
    r.open(&r.root("rw"));
}

#[test]
fn a_second_open_on_a_live_lease_is_refused_with_the_typed_error_and_leaves_nothing() {
    let r = rig();
    let first = r.root("one");
    r.open(&first);
    let second = r.root("two");
    let e = r.call("open", json!({"name": r.name, "root": second, "spec": spec()})).unwrap_err();
    assert!(e.contains("is leased by") && e.contains("until"), "{e}");
    assert!(!Path::new(&second).exists(), "a refused open creates nothing");
    // and the same root cannot be opened twice
    assert!(r.call("open", json!({"name": r.name, "root": first, "spec": spec()})).is_err());
}

#[test]
fn close_with_open_conflicts_refuses_unless_discard_and_resolve_clears_them() {
    let r = rig();
    let root = r.root("view");
    r.open(&root);
    std::fs::write(file(&root, "work/a.txt"), "mine").unwrap();
    r.seed("proj/a.txt", "theirs");
    let p = r.call("publish", json!({"name": r.name, "root": root})).unwrap();
    assert_eq!(p["conflicts"], json!(["a.txt"]), "{p}");
    assert_eq!(r.text("proj/a.txt").as_deref(), Some("theirs"), "the remote is left alone");

    let e = r.call("close", json!({"name": r.name, "root": root, "publish": true})).unwrap_err();
    assert!(e.contains("open conflict") && e.contains("nothing removed"), "{e}");
    assert!(file(&root, "work/a.txt").exists() && file(&root, "view.json").exists());

    assert!(r.call("resolve", json!({"name": r.name, "root": root, "path": "a.txt", "keep": "sideways"})).is_err());
    r.call("resolve", json!({"name": r.name, "root": root, "path": "a.txt", "keep": "local"})).unwrap();
    let p = r.call("publish", json!({"name": r.name, "root": root})).unwrap();
    assert_eq!(p["put"], json!(["a.txt"]));
    assert_eq!(r.text("proj/a.txt").as_deref(), Some("mine"));

    // discard drops unpublished work without touching the store
    std::fs::write(file(&root, "work/a.txt"), "dropped").unwrap();
    let c = r.call("close", json!({"name": r.name, "root": root, "discard": true})).unwrap();
    assert_eq!(c["published"], Value::Null);
    assert_eq!(r.text("proj/a.txt").as_deref(), Some("mine"));
}

#[test]
fn close_needs_exactly_one_of_publish_or_discard_and_removes_nothing_otherwise() {
    let r = rig();
    let root = r.root("view");
    r.open(&root);
    for p in [json!({}), json!({"publish": true, "discard": true})] {
        let mut p = p;
        p["name"] = json!(r.name);
        p["root"] = json!(root);
        let e = r.call("close", p).unwrap_err();
        assert!(e.contains("exactly one of --publish, --discard"), "{e}");
    }
    assert!(file(&root, "view.json").exists());
}

#[test]
fn renew_extends_the_lease_and_a_missing_flag_or_value_is_named() {
    let r = rig();
    let root = r.root("view");
    let o = r.open(&root);
    let before = o["lease"]["expires_ms"].as_u64().unwrap();
    let n = r.call("renew", json!({"root": root})).unwrap();
    assert!(n["lease"]["expires_ms"].as_u64().unwrap() >= before, "{n}");
    assert_eq!(n["lease"]["id"], o["lease"]["id"]);
    assert!(r.call("open", json!({"name": r.name, "root": r.root("x")})).unwrap_err().contains("missing --spec"));
    assert!(r.call("open", json!({"root": r.root("x"), "spec": spec()})).unwrap_err().contains("missing --name"));
    assert!(r.call("status", json!({})).unwrap_err().contains("missing --root"));
    assert!(r.call("resolve", json!({"name": r.name, "root": root})).unwrap_err().contains("missing --path"));
}

#[test]
fn a_relative_root_is_refused_by_the_path_guard_for_every_verb() {
    let r = rig();
    for verb in ["open", "status", "refresh", "publish", "resolve", "renew", "close"] {
        let e = r.call(verb, json!({"name": r.name, "root": "relative/view", "spec": spec(), "path": "x", "keep": "local", "discard": true})).unwrap_err();
        assert!(e.contains("must be an absolute path"), "{verb}: {e}");
    }
}

#[test]
fn the_daemons_own_state_is_off_limits_and_nothing_is_created_there() {
    let r = rig();
    let state = r.d.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    let g = Guard::new([state.clone()]);
    let root = state.join("view").to_string_lossy().into_owned();
    let e = run_with(&r.svc, "store.view.open", json!({"name": r.name, "root": root, "spec": spec()}), &g).unwrap_err();
    assert!(e.contains("daemon's own state"), "{e}");
    assert!(!state.join("view").exists());
    // a root that is a symlink into protected state resolves there and is refused too
    let link = r.d.path().join("link");
    std::os::unix::fs::symlink(&state, &link).unwrap();
    let e = run_with(&r.svc, "store.view.status", json!({"root": link.join("view").to_string_lossy()}), &g).unwrap_err();
    assert!(e.contains("daemon's own state"), "{e}");
}

#[test]
fn a_view_bound_to_another_store_is_refused_and_so_is_a_spec_naming_another_bucket() {
    let r = rig();
    let root = r.root("view");
    r.open(&root);
    let other = StoreName::project("local", "01JBXK2M3N4P5Q6R7S8T9V0W2Y").unwrap().to_string();
    for verb in ["status", "refresh", "publish", "close", "renew"] {
        let e = r.call(verb, json!({"name": other, "root": root, "discard": true})).unwrap_err();
        assert!(e.contains("is bound to bucket"), "{verb}: {e}");
    }
    let mut s = spec();
    s["bucket"] = json!("someone-elses-bucket");
    let e = r.call("open", json!({"name": r.name, "root": r.root("y"), "spec": s})).unwrap_err();
    assert!(e.contains("is not the bucket of"), "{e}");
    assert!(!Path::new(&r.root("y")).exists());
}

#[test]
fn open_publish_resolve_and_close_are_recorded_with_counts_and_reads_are_not() {
    let r = rig();
    let root = r.root("view");
    r.open(&root);
    std::fs::write(file(&root, "work/a.txt"), "mine").unwrap();
    r.seed("proj/a.txt", "theirs");
    r.call("publish", json!({"name": r.name, "root": root})).unwrap();
    r.call("resolve", json!({"name": r.name, "root": root, "path": "a.txt", "keep": "remote"})).unwrap();
    // a refused call is recorded too
    let e = r.call("open", json!({"name": r.name, "root": r.root("again"), "spec": spec()})).unwrap_err();
    // reads and the lease renewal record nothing
    r.call("status", json!({"root": root})).unwrap();
    r.call("refresh", json!({"root": root})).unwrap();
    r.call("renew", json!({"root": root})).unwrap();
    r.call("close", json!({"name": r.name, "root": root, "discard": true})).unwrap();

    assert_eq!(r.kinds(), ["open", "publish", "resolve", "open", "close"].map(|v| format!("store.view.{v}")));
    let ev: Vec<(String, String, String)> = r.sink.events().into_iter().filter(|e| e.op.starts_with("View")).map(|e| (e.op, e.principal, e.outcome.text())).collect();
    assert_eq!(ev.len(), 5, "{ev:?}");
    assert!(ev.iter().any(|(op, who, out)| op == "ViewOpen" && who == "bob@m2" && out == "ok"));
    assert!(ev.iter().any(|(op, _, out)| op == "ViewOpen" && *out == format!("error:{e}")));
    for op in ["ViewPublish", "ViewResolve", "ViewClose"] {
        assert!(ev.iter().any(|(o, _, _)| o == op), "{op}");
    }
    // the chain payloads carry counts, never a list of files
    fn no_arrays(v: &Value) -> bool {
        match v {
            Value::Array(_) => false,
            Value::Object(m) => m.values().all(no_arrays),
            _ => true,
        }
    }
    let ch = r.chain.0.lock().unwrap();
    for (k, v) in ch.iter().filter(|(k, _)| k.starts_with("store.view.")) {
        assert!(no_arrays(v), "{k}: {v}");
    }
    let open = ch.iter().find(|(k, _)| k == "store.view.open").unwrap();
    assert!(open.1["result"]["spec_hash"].is_string() && open.1["result"]["lease"]["id"].is_string() && open.1["result"]["files"].is_number(), "{}", open.1);
    let publish = ch.iter().find(|(k, _)| k == "store.view.publish").unwrap();
    assert_eq!(publish.1["result"]["conflicts"], 1, "{}", publish.1);
}

#[test]
fn unknown_params_verbs_and_spec_shapes_are_rejected() {
    let r = rig();
    assert!(r.call("status", json!({"root": "/x", "bogus": 1})).unwrap_err().contains("invalid params"));
    assert!(r.call("teleport", json!({})).unwrap_err().contains("unknown view verb"));
    assert!(r.call("open", json!({"name": r.name, "root": r.root("z"), "spec": "nope"})).unwrap_err().contains("JSON object"));
    assert!(r.call("open", json!({"name": r.name, "root": r.root("z"), "spec": {"base": 3}})).unwrap_err().contains("bad view spec"));
    // an invalid spec reaches the library's validation: nested includes are refused
    let nested = json!({"base": [{"prefix": "docs", "at": ""}, {"prefix": "lib", "at": "lib"}]});
    assert!(r.call("open", json!({"name": r.name, "root": r.root("z"), "spec": nested})).is_err());
    assert!(!Path::new(&r.root("z")).exists());
}

/// The handler path end to end on a REAL RustFS. Needs a THROWAWAY loopback RustFS (never PG):
/// WEFTOS_STORE_TEST_RUSTFS_ENV names an env file with MESH_S3_ENDPOINT / _ACCESS_KEY / _SECRET_KEY /
/// _REGION. Run: `scripts/build.sh test clawft-weave --ignored --filter live_view_verbs`.
#[test]
#[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
fn live_view_verbs_open_edit_refresh_publish_status_close() {
    use std::time::Instant;
    let env = std::path::PathBuf::from(std::env::var("WEFTOS_STORE_TEST_RUSTFS_ENV").expect("WEFTOS_STORE_TEST_RUSTFS_ENV"));
    let d = tempfile::tempdir().unwrap();
    let reg = Arc::new(clawft_kernel::exposure::ExposureRegistry::new("live-1"));
    let node = format!("live-{}", &ulid::Ulid::new().to_string().to_lowercase()[10..]);
    let svc = crate::store_rt::open_service(&node, d.path(), reg, None, Some(env)).unwrap();
    let proj = ulid::Ulid::new().to_string();
    let name = StoreName::project("local", &proj).unwrap();
    svc.ensure_store(&name, "cli", Parties::defaults_for(&name)).unwrap();
    let (bucket, ops) = (name.bucket(), svc.operator_objects());
    for i in 0..30 {
        ops.put(&bucket, &format!("proj/d{}/f{i:02}.txt", i % 3), format!("work {i}\n").repeat(20).as_bytes(), false).unwrap();
        ops.put(&bucket, &format!("docs/p{i:02}.md", ), format!("doc {i}\n").repeat(20).as_bytes(), false).unwrap();
    }
    let root = d.path().join("view");
    let g = Guard::none();
    let go = |verb: &str, mut p: Value| {
        p["agent"] = json!("live");
        p["host"] = json!("m2");
        let t = Instant::now();
        let r = run_with(&svc, &format!("store.view.{verb}"), p, &g);
        (r, t.elapsed().as_millis())
    };
    let rs = root.to_string_lossy().into_owned();

    let (o, open_ms) = go("open", json!({"name": name.to_string(), "root": rs, "spec": spec()}));
    let o = o.unwrap();
    assert_eq!(o["files"], 60, "{o}");
    std::fs::write(root.join("work/d0/f00.txt"), "edited in the session\n").unwrap();
    std::fs::write(root.join("work/new.txt"), "created in the session\n").unwrap();
    ops.put(&bucket, "docs/p01.md", b"docs changed remotely\n", false).unwrap();
    let (f, refresh_ms) = go("refresh", json!({"name": name.to_string(), "root": rs}));
    f.unwrap();
    assert_eq!(std::fs::read(root.join("base/docs/p01.md")).unwrap(), b"docs changed remotely\n");
    let (p, publish_ms) = go("publish", json!({"name": name.to_string(), "root": rs}));
    let p = p.unwrap();
    assert_eq!((p["put"].as_array().unwrap().len(), p["head_then_write"].as_u64()), (2, Some(0)), "{p}");
    let (s, status_ms) = go("status", json!({"root": rs}));
    assert!(s.unwrap()["pending"].as_array().unwrap().is_empty());
    let (c, close_ms) = go("close", json!({"name": name.to_string(), "root": rs, "publish": true}));
    assert_eq!(c.unwrap()["lease_released"], true);
    assert_eq!(ops.get(&bucket, "proj/new.txt").unwrap(), b"created in the session\n");
    eprintln!("VIEW VERBS LIVE (noisy, load 6-7): open 60 files {open_ms} ms | refresh {refresh_ms} ms | publish (2 puts) {publish_ms} ms | status {status_ms} ms | close {close_ms} ms");

    for m in ops.list(&bucket, "").unwrap_or_default() {
        let _ = ops.delete(&bucket, &m.key);
    }
}
