use std::sync::Arc;

use clawft_kernel::exposure::ExposureRegistry;
use serde_json::json;
use weftos_store::{FsBackend, StoreService};

use super::*;
use crate::store_worktree_paths::Guard;
use std::path::Path;
use crate::store_rt::{ActivitySink, ChainAudit, RegistryGate};

const PROJ: &str = "01JBXK2M3N4P5Q6R7S8T9V0W1X";

fn rig() -> (tempfile::TempDir, StoreService) {
    let d = tempfile::tempdir().unwrap();
    let reg = Arc::new(ExposureRegistry::new("pg-1"));
    let be = Arc::new(FsBackend::open(d.path()).unwrap());
    let svc = StoreService::open(
        weftos_store::service::Config { node_id: "pg-1".into(), state_path: None, clock: clawft_kernel::exposure::registry::now_ms },
        be.clone(),
        be,
        Arc::new(RegistryGate(reg.clone())),
        Arc::new(ActivitySink(reg)),
        Arc::new(ChainAudit(None)),
    )
    .unwrap();
    (d, svc)
}

fn call(svc: &StoreService, verb: &str, p: Value, now: u64) -> Result<Value, String> {
    run(svc, "local", &format!("store.worktree.{verb}"), p, now)
}

fn files(dir: &Path) -> (String, String, String) {
    std::fs::create_dir_all(dir.join(".git")).unwrap();
    std::fs::create_dir_all(dir.join("docs")).unwrap();
    std::fs::write(dir.join("docs/plan.md"), "# Rooms plan\n").unwrap();
    std::fs::write(dir.join("a.rs"), "fn a() {}\n").unwrap();
    let w = |n: &str, c: &[u8]| {
        let f = dir.join(n);
        std::fs::write(&f, c).unwrap();
        f.to_string_lossy().into_owned()
    };
    (
        w("paths.z", b"docs/plan.md\0a.rs\0"),
        w("unc.z", b"docs/plan.md\0"),
        w("chg.z", b"M\0a.rs\0A\0docs/plan.md\0D\0gone.rs\0"),
    )
}

#[test]
fn the_verbs_carry_a_worktree_from_create_to_land_through_the_daemon_store() {
    let (_d, svc) = rig();
    let b = call(&svc, "buckets", json!({"project": PROJ, "slug": "feat-rooms"}), 1).unwrap();
    assert!(b["worktree"].as_str().unwrap().starts_with("w-01jbxk2m3n4p5q6r7s8t9v0w1x-"));
    assert_eq!(b["project"], "p-01jbxk2m3n4p5q6r7s8t9v0w1x");

    call(&svc, "create", json!({"project": PROJ, "slug": "feat-rooms", "branch": "feat/rooms", "base": "abc"}), 10).unwrap();
    // assets the project already has are visible to the worktree with no copy
    let pb = b["project"].as_str().unwrap();
    S1::put(&*svc.operator_objects(), pb, "assets/hero.bin", b"HERO", false).unwrap();

    let src = tempfile::tempdir().unwrap();
    let (paths, unc, chg) = files(src.path());
    let root = src.path().to_string_lossy().into_owned();
    let publish = |agent: &str, now| {
        call(
            &svc,
            "publish",
            json!({"project": PROJ, "slug": "feat-rooms", "root": root, "branch": "feat/rooms", "base": "abc", "head": "def",
                   "paths_z": paths, "uncommitted_z": unc, "changes_z": chg, "handoff": "please merge", "agent": agent, "host": "m1"}),
            now,
        )
    };
    let r = publish("alice", 20).unwrap();
    assert_eq!(r["uploaded"], 2);

    // a second machine follows, searches, fetches, claims
    let st = src.path().join("follow.json");
    let f = call(&svc, "follow", json!({"project": PROJ, "state": st.to_string_lossy()}), 30).unwrap();
    assert_eq!(f["changes"][0]["change"], "new");
    let s = call(&svc, "search", json!({"project": PROJ, "query": "plan"}), 31).unwrap();
    assert_eq!(s["hits"][0]["path"], "docs/plan.md");
    let dest = tempfile::tempdir().unwrap();
    let fe = call(&svc, "fetch", json!({"project": PROJ, "slug": "feat-rooms", "dest": dest.path().to_string_lossy()}), 32).unwrap();
    assert_eq!(fe["written"], 2);
    assert_eq!(std::fs::read_to_string(dest.path().join("docs/plan.md")).unwrap(), "# Rooms plan\n");
    let c = call(&svc, "claim", json!({"project": PROJ, "slug": "feat-rooms", "agent": "bob", "host": "m2"}), 40).unwrap();
    assert_eq!(c["merger"], "bob@m2");
    assert!(publish("alice", 41).is_err(), "the writer cannot overwrite a claimed merge");
    let f2 = call(&svc, "follow", json!({"project": PROJ, "state": st.to_string_lossy()}), 42).unwrap();
    assert_eq!(f2["changes"][0]["change"], "state_changed");

    // landing: the worktree's own asset reaches the project, the claim is closed as merged
    let n = names("local", PROJ, "feat-rooms").unwrap();
    weftos_store::overlay::Overlay::new(&Op(svc.operator_objects()), &n.wt, &n.pr).put("assets/room.bin", b"ROOM").unwrap();
    let p = call(&svc, "promote", json!({"project": PROJ, "slug": "feat-rooms", "agent": "bob", "host": "m2"}), 50).unwrap();
    assert_eq!(p["promoted"], json!(["assets/room.bin"]));
    assert_eq!(S1::get(&*svc.operator_objects(), pb, "assets/room.bin").unwrap(), b"ROOM");
    // followers see the landed worktree as merged, not still claimed
    let f3 = call(&svc, "follow", json!({"project": PROJ, "state": st.to_string_lossy()}), 52).unwrap();
    assert_eq!(f3["changes"][0]["change"], "state_changed");
    assert_eq!(f3["changes"][0]["worktree"]["state"], "merged");
    let again = call(&svc, "promote", json!({"project": PROJ, "slug": "feat-rooms"}), 51).unwrap();
    assert_eq!(again["promoted"], p["promoted"], "landing twice is a no-op");
}

#[test]
fn bad_input_is_refused() {
    let (_d, svc) = rig();
    assert!(call(&svc, "buckets", json!({"project": PROJ, "slug": "Bad Slug"}), 1).is_err());
    assert!(call(&svc, "buckets", json!({"project": "../x", "slug": "ok"}), 1).is_err());
    assert!(call(&svc, "buckets", json!({"project": PROJ, "slug": "ok", "extra": 1}), 1).is_err(), "unknown params refused");
    assert!(call(&svc, "nope", json!({"project": PROJ}), 1).is_err());
    assert!(call(&svc, "publish", json!({"project": PROJ, "slug": "ok", "root": "relative/dir", "branch": "b", "base": "a", "head": "h"}), 1).is_err());
}

#[test]
fn fetch_writes_the_bundle_when_one_was_published_and_says_so_when_not() {
    let (_d, svc) = rig();
    call(&svc, "create", json!({"project": PROJ, "slug": "feat-b", "branch": "feat/b", "base": "abc"}), 10).unwrap();
    let src = tempfile::tempdir().unwrap();
    let (paths, unc, chg) = files(src.path());
    let root = src.path().to_string_lossy().into_owned();
    let publish = |extra: Value| {
        let mut p = json!({"project": PROJ, "slug": "feat-b", "root": root, "branch": "feat/b", "base": "abc", "head": "def",
                           "paths_z": paths, "uncommitted_z": unc, "changes_z": chg});
        p.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        call(&svc, "publish", p, 20)
    };
    let dest = tempfile::tempdir().unwrap();
    let out = dest.path().join("nested/b.bundle");
    let fetch = || {
        call(&svc, "fetch", json!({"project": PROJ, "slug": "feat-b", "dest": dest.path().join("t").to_string_lossy(),
                                   "bundle_out": out.to_string_lossy()}), 30)
    };

    // no bundle published: a clear error, and no file written
    publish(json!({})).unwrap();
    let err = fetch().unwrap_err();
    assert!(err.contains("without a git bundle"), "{err}");
    assert!(!out.exists());

    // with a bundle: bytes land at the requested path (parent created), head returned
    let bundle = src.path().join("in.bundle");
    std::fs::write(&bundle, b"BUNDLE-BYTES").unwrap();
    publish(json!({"bundle": bundle.to_string_lossy()})).unwrap();
    let r = fetch().unwrap();
    assert_eq!(r["bundle_head"], "def");
    assert_eq!(std::fs::read(&out).unwrap(), b"BUNDLE-BYTES");
}

#[test]
fn archiving_and_deleting_a_worktree_store_removes_its_registry_entry() {
    let (_d, svc) = rig();
    let ens = |slug: &str| {
        let n = names("local", PROJ, slug).unwrap();
        svc.ensure_store(&n.pr_name, "t", Parties::defaults_for(&n.pr_name)).unwrap();
        svc.ensure_store(&n.wt_name, "t", Parties::defaults_for(&n.wt_name)).unwrap();
        n
    };
    let (keep, gone) = (ens("feat-keep"), ens("feat-gone"));
    let src = tempfile::tempdir().unwrap();
    let (paths, unc, chg) = files(src.path());
    let root = src.path().to_string_lossy().into_owned();
    for slug in ["feat-keep", "feat-gone"] {
        call(&svc, "create", json!({"project": PROJ, "slug": slug, "branch": slug, "base": "abc"}), 10).unwrap();
        call(
            &svc,
            "publish",
            json!({"project": PROJ, "slug": slug, "root": root, "branch": slug, "base": "abc", "head": "def",
                   "paths_z": paths, "uncommitted_z": unc, "changes_z": chg}),
            20,
        )
        .unwrap();
    }
    let slugs = || -> Vec<String> {
        weftos_store::share_fetch::list_shared(&Op(svc.operator_objects()), &keep.pr).unwrap().into_iter().map(|e| e.slug).collect()
    };
    assert_eq!(slugs().len(), 2);
    // closing without deleting keeps the entry (the bucket still exists); deleting drops only its own
    svc.archive(&gone.wt_name, true, "t").unwrap();
    forget(&svc, &gone.wt_name).unwrap();
    assert_eq!(slugs(), vec!["feat-keep".to_string()]);
    forget(&svc, &gone.wt_name).unwrap(); // already gone: fine
    // a project store is not a worktree: nothing happens
    forget(&svc, &keep.pr_name).unwrap();
    assert_eq!(slugs(), vec!["feat-keep".to_string()]);
}

#[test]
fn a_corrupt_registry_entry_does_not_fail_search() {
    let (_d, svc) = rig();
    call(&svc, "create", json!({"project": PROJ, "slug": "feat-s", "branch": "feat/s", "base": "abc"}), 10).unwrap();
    let src = tempfile::tempdir().unwrap();
    let (paths, unc, chg) = files(src.path());
    let root = src.path().to_string_lossy().into_owned();
    call(
        &svc,
        "publish",
        json!({"project": PROJ, "slug": "feat-s", "root": root, "branch": "feat/s", "base": "abc", "head": "def",
               "paths_z": paths, "uncommitted_z": unc, "changes_z": chg}),
        20,
    )
    .unwrap();
    let n = names("local", PROJ, "feat-s").unwrap();
    S1::put(&*svc.operator_objects(), &n.pr, "worktrees/corrupt.json", b"{ not json", false).unwrap();
    S1::put(&*svc.operator_objects(), &n.pr, "worktrees/future.json", br#"{"schema":99,"slug":"future"}"#, false).unwrap();
    let r = call(&svc, "search", json!({"project": PROJ, "query": "plan"}), 30).unwrap();
    assert_eq!(r["hits"][0]["slug"], "feat-s");
    assert_eq!(r["unreadable_entries"], json!(["corrupt", "future"]));
}

#[test]
fn create_on_a_second_daemon_sharing_the_backend_succeeds() {
    // Two daemons, one backend (one RustFS): the second has no record of the stores the first made.
    let d = tempfile::tempdir().unwrap();
    let mk = |node: &str| {
        let reg = Arc::new(ExposureRegistry::new(node));
        let be = Arc::new(FsBackend::open(d.path()).unwrap());
        StoreService::open(
            weftos_store::service::Config { node_id: node.into(), state_path: None, clock: clawft_kernel::exposure::registry::now_ms },
            be.clone(),
            be,
            Arc::new(RegistryGate(reg.clone())),
            Arc::new(ActivitySink(reg)),
            Arc::new(ChainAudit(None)),
        )
        .unwrap()
    };
    let (a, b) = (mk("node-a"), mk("node-b"));
    call(&a, "create", json!({"project": PROJ, "slug": "feat-one", "branch": "feat/one", "base": "abc"}), 10).unwrap();
    // the project store exists already (node-a made it); node-b must still create its own worktree
    let r = call(&b, "create", json!({"project": PROJ, "slug": "feat-two", "branch": "feat/two", "base": "abc"}), 11).unwrap();
    let wt = r["worktree"].as_str().unwrap();
    assert!(S1::list(&*b.operator_objects(), wt, ".wt/").unwrap().iter().any(|m| m.key == ".wt/meta.json"), "the second worktree has its metadata");
    // and the same slug from the other daemon is idempotent
    call(&b, "create", json!({"project": PROJ, "slug": "feat-one", "branch": "feat/one", "base": "abc"}), 12).unwrap();
}

// ---- F1: path params are checked whoever sends them ----------------------------------------------

fn call_g(svc: &StoreService, verb: &str, p: Value, g: &Guard) -> Result<Value, String> {
    run_with(svc, "local", &format!("store.worktree.{verb}"), p, 10, g)
}

fn published(svc: &StoreService, slug: &str, src: &Path) {
    call(svc, "create", json!({"project": PROJ, "slug": slug, "branch": slug, "base": "abc"}), 10).unwrap();
    let (paths, unc, chg) = files(src);
    call(
        svc,
        "publish",
        json!({"project": PROJ, "slug": slug, "root": src.to_string_lossy(), "branch": slug, "base": "abc", "head": "def",
               "paths_z": paths, "uncommitted_z": unc, "changes_z": chg}),
        20,
    )
    .unwrap();
}

#[test]
fn every_path_param_must_be_absolute() {
    let (_d, svc) = rig();
    let g = Guard::none();
    let src = tempfile::tempdir().unwrap();
    published(&svc, "feat-p", src.path());
    let abs = src.path().to_string_lossy().into_owned();
    let (paths, _, _) = files(src.path());
    let pub_with = |k: &str, v: &str| {
        let mut p = json!({"project": PROJ, "slug": "feat-p", "root": abs, "branch": "b", "base": "a", "head": "h", "paths_z": paths});
        p[k] = json!(v);
        call_g(&svc, "publish", p, &g)
    };
    for k in ["root", "paths_z", "uncommitted_z", "changes_z", "bundle"] {
        let e = pub_with(k, "relative/x").unwrap_err();
        assert!(e.contains("absolute"), "{k}: {e}");
    }
    for (verb, key) in [("fetch", "dest"), ("sync-docs", "cache"), ("follow", "state")] {
        let e = call_g(&svc, verb, json!({"project": PROJ, "slug": "feat-p", key: "rel/dir"}), &g).unwrap_err();
        assert!(e.contains("absolute"), "{verb}: {e}");
    }
    let e = call_g(&svc, "fetch", json!({"project": PROJ, "slug": "feat-p", "dest": abs, "bundle_out": "rel.bundle"}), &g).unwrap_err();
    assert!(e.contains("absolute"), "{e}");
    let e = pub_with("root", &format!("{abs}/../x")).unwrap_err();
    assert!(e.contains("'..'"), "{e}");
}

#[test]
fn publish_refuses_the_daemons_own_state_and_non_worktree_roots() {
    let (_d, svc) = rig();
    let base = tempfile::tempdir().unwrap();
    let root = base.path().join("repo");
    let run = root.join("run"); // a runtime dir that happens to sit under the root
    std::fs::create_dir_all(run.join("store-secrets")).unwrap();
    std::fs::write(run.join("store-secrets/wk-1"), "deadbeefdeadbeef").unwrap();
    let (paths, _, _) = files(&root);
    let g = Guard::new([run.clone()]);
    call(&svc, "create", json!({"project": PROJ, "slug": "feat-s", "branch": "b", "base": "abc"}), 10).unwrap();
    let list = base.path().join("l.z");
    std::fs::write(&list, "docs/plan.md\0run/store-secrets/wk-1\0").unwrap();
    let pubp = |root: &Path, list: &str| {
        call_g(&svc, "publish", json!({"project": PROJ, "slug": "feat-s", "root": root.to_string_lossy(), "branch": "b", "base": "a", "head": "h", "paths_z": list}), &g)
    };
    let e = pubp(&root, &list.to_string_lossy()).unwrap_err();
    assert!(e.contains("daemon's own state"), "{e}");
    // the secret never reached a bucket
    let n = names("local", PROJ, "feat-s").unwrap();
    assert!(S1::list(&*svc.operator_objects(), &n.wt, ".wt/tree/").unwrap().is_empty());
    // a root inside the protected dir, and a root that is not a git worktree
    let e = pubp(&run, &paths).unwrap_err();
    assert!(e.contains("daemon's own state"), "{e}");
    let plain = base.path().join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    let e = pubp(&plain, &paths).unwrap_err();
    assert!(e.contains("git worktree"), "{e}");
    // a list file inside the protected dir is not read either
    std::fs::write(run.join("l.z"), "docs/plan.md\0").unwrap();
    let e = pubp(&root, &run.join("l.z").to_string_lossy()).unwrap_err();
    assert!(e.contains("daemon's own state"), "{e}");
}

#[test]
fn a_bundle_that_is_a_symlink_to_something_else_is_not_uploaded() {
    let (_d, svc) = rig();
    let src = tempfile::tempdir().unwrap();
    let (paths, _, _) = files(src.path());
    std::fs::write(src.path().join("secret.txt"), "hunter2").unwrap();
    std::os::unix::fs::symlink(src.path().join("secret.txt"), src.path().join("b.bundle")).unwrap();
    call(&svc, "create", json!({"project": PROJ, "slug": "feat-l", "branch": "b", "base": "abc"}), 10).unwrap();
    let e = call_g(
        &svc,
        "publish",
        json!({"project": PROJ, "slug": "feat-l", "root": src.path().to_string_lossy(), "branch": "b", "base": "a", "head": "h",
               "paths_z": paths, "bundle": src.path().join("b.bundle").to_string_lossy()}),
        &Guard::none(),
    )
    .unwrap_err();
    assert!(e.contains("symlink"), "{e}");
}

#[test]
fn fetch_never_writes_into_or_over_the_daemons_state_or_through_a_symlink() {
    let (_d, svc) = rig();
    let src = tempfile::tempdir().unwrap();
    published(&svc, "feat-f", src.path());
    let base = tempfile::tempdir().unwrap();
    let run = base.path().join("run");
    std::fs::create_dir_all(run.join("store-secrets")).unwrap();
    let g = Guard::new([run.clone()]);
    let fetch = |dest: &Path, extra: Value| {
        let mut p = json!({"project": PROJ, "slug": "feat-f", "dest": dest.to_string_lossy()});
        p.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        call_g(&svc, "fetch", p, &g)
    };
    // inside the runtime dir, and an ancestor that would contain it
    let e = fetch(&run.join("x"), json!({})).unwrap_err();
    assert!(e.contains("daemon's own state"), "{e}");
    let e = fetch(base.path(), json!({"force": true})).unwrap_err();
    assert!(e.contains("contain"), "{e}");
    assert!(!base.path().join("docs").exists());
    // bundle-out: a symlink is refused and its target is untouched
    let victim = base.path().join("victim");
    std::fs::write(&victim, "keep").unwrap();
    let link = base.path().join("link.bundle");
    std::os::unix::fs::symlink(&victim, &link).unwrap();
    let ok_dest = tempfile::tempdir().unwrap();
    let e = fetch(ok_dest.path(), json!({"bundle_out": link.to_string_lossy()})).unwrap_err();
    assert!(e.contains("symlink"), "{e}");
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep");
    // follow state: same rule
    let e = call_g(&svc, "follow", json!({"project": PROJ, "state": link.to_string_lossy()}), &g).unwrap_err();
    assert!(e.contains("symlink"), "{e}");
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep");
    // and the honest case still works
    assert_eq!(fetch(ok_dest.path(), json!({})).unwrap()["written"], 2);
}

// ---- F6, F8: reused slugs and closed or foreign buckets -------------------------------------------

#[test]
fn a_landed_slug_cannot_be_reused_until_its_store_is_deleted() {
    let (_d, svc) = rig();
    let src = tempfile::tempdir().unwrap();
    published(&svc, "feat-r", src.path());
    call(&svc, "claim", json!({"project": PROJ, "slug": "feat-r", "agent": "bob", "host": "m"}), 30).unwrap();
    call(&svc, "promote", json!({"project": PROJ, "slug": "feat-r", "agent": "bob", "host": "m"}), 40).unwrap();
    let e = call(&svc, "create", json!({"project": PROJ, "slug": "feat-r", "branch": "b", "base": "abc"}), 50).unwrap_err();
    assert!(e.contains("already landed"), "{e}");
    // an unlanded worktree can be created again, and keeps its first creation time
    published(&svc, "feat-live", src.path());
    let n = names("local", PROJ, "feat-live").unwrap();
    let before = read_meta(&Op(svc.operator_objects()), &n.wt).unwrap().unwrap().created_ms;
    call(&svc, "create", json!({"project": PROJ, "slug": "feat-live", "branch": "b", "base": "abc"}), 999).unwrap();
    assert_eq!(read_meta(&Op(svc.operator_objects()), &n.wt).unwrap().unwrap().created_ms, before);
}

#[test]
fn closed_unknown_and_foreign_buckets_take_no_publish_claim_or_promote() {
    let (_d, svc) = rig();
    let src = tempfile::tempdir().unwrap();
    let (paths, unc, chg) = files(src.path());
    let pubp = |slug: &str| {
        call(&svc, "publish", json!({"project": PROJ, "slug": slug, "root": src.path().to_string_lossy(), "branch": "b", "base": "a", "head": "h",
                                      "paths_z": paths, "uncommitted_z": unc, "changes_z": chg}), 60)
    };
    // never created
    let e = pubp("feat-none").unwrap_err();
    assert!(e.contains("never created"), "{e}");
    // archived (closed, not deleted)
    published(&svc, "feat-closed", src.path());
    let n = names("local", PROJ, "feat-closed").unwrap();
    svc.archive(&n.wt_name, false, "t").unwrap();
    for verb in ["publish", "claim", "promote"] {
        let mut p = json!({"project": PROJ, "slug": "feat-closed", "root": src.path().to_string_lossy(), "branch": "b", "base": "a", "head": "h", "paths_z": paths});
        p["agent"] = json!("x");
        let e = call(&svc, verb, p, 61).unwrap_err();
        assert!(e.contains("archived"), "{verb}: {e}");
    }
    // a bucket whose metadata names another slug (a hash collision, or a forged redirect)
    published(&svc, "feat-a", src.path());
    let a = names("local", PROJ, "feat-a").unwrap();
    let c = names("local", PROJ, "feat-c").unwrap();
    svc.ensure_store(&c.wt_name, "t", Parties::defaults_for(&c.wt_name)).unwrap();
    let meta = read_meta(&Op(svc.operator_objects()), &a.wt).unwrap().unwrap();
    write_meta(&Op(svc.operator_objects()), &c.wt, &meta).unwrap(); // feat-c's bucket claims to be feat-a's
    let e = pubp("feat-c").unwrap_err();
    assert!(e.contains("belongs to worktree feat-a"), "{e}");
}

// ---- F10: state-changing verbs are recorded -------------------------------------------------------

#[derive(Default)]
struct Chain(std::sync::Mutex<Vec<(String, Value)>>);
impl weftos_store::service::ChainSink for Chain {
    fn record(&self, k: &str, v: Value) {
        self.0.lock().unwrap().push((k.to_owned(), v));
    }
}

#[test]
fn create_publish_claim_and_promote_are_in_the_activity_log_and_on_the_chain() {
    let d = tempfile::tempdir().unwrap();
    let be = Arc::new(FsBackend::open(d.path()).unwrap());
    let sink = Arc::new(weftos_store::audit::MemSink::default());
    let chain = Arc::new(Chain::default());
    let svc = StoreService::open(
        weftos_store::service::Config { node_id: "pg-1".into(), state_path: None, clock: || 5 },
        be.clone(),
        be,
        Arc::new(weftos_store::service::OpenGate),
        sink.clone(),
        chain.clone(),
    )
    .unwrap();
    let src = tempfile::tempdir().unwrap();
    published(&svc, "feat-audit", src.path());
    call(&svc, "claim", json!({"project": PROJ, "slug": "feat-audit", "agent": "bob", "host": "m2"}), 30).unwrap();
    // a refused claim is recorded too
    let e = call(&svc, "claim", json!({"project": PROJ, "slug": "feat-audit", "agent": "eve", "host": "m3"}), 31).unwrap_err();
    call(&svc, "promote", json!({"project": PROJ, "slug": "feat-audit", "agent": "bob", "host": "m2"}), 40).unwrap();
    // read verbs record nothing
    call(&svc, "search", json!({"project": PROJ, "query": "plan"}), 41).unwrap();

    let kinds: Vec<String> = chain.0.lock().unwrap().iter().map(|(k, _)| k.clone()).filter(|k| k.starts_with("store.worktree.")).collect();
    assert_eq!(kinds, ["create", "publish", "claim", "claim", "promote"].map(|v| format!("store.worktree.{v}")));
    let ev: Vec<(String, String, String)> = sink.events().into_iter().filter(|e| e.op.starts_with("Worktree")).map(|e| (e.op, e.principal, e.outcome.text())).collect();
    assert_eq!(ev.len(), 5, "{ev:?}");
    assert!(ev.iter().any(|(op, who, out)| op == "WorktreeClaim" && who == "bob@m2" && out == "ok"));
    assert!(ev.iter().any(|(op, who, out)| op == "WorktreeClaim" && who == "eve@m3" && *out == format!("error:{e}")));
    assert!(ev.iter().any(|(op, _, _)| op == "WorktreePromote"));
}

#[test]
fn overlaps_names_both_bases_so_callers_can_discount_it() {
    let (_d, svc) = rig();
    let src = tempfile::tempdir().unwrap();
    published(&svc, "feat-a", src.path());
    published(&svc, "feat-b", src.path());
    let r = call(&svc, "overlaps", json!({"project": PROJ, "a": "feat-a", "b": "feat-b"}), 50).unwrap();
    assert_eq!((r["base_a"].as_str(), r["base_b"].as_str(), r["reliable"].as_bool()), (Some("abc"), Some("abc"), Some(true)));
}

#[test]
fn promote_runs_when_someone_else_holds_the_claim_or_it_expired_and_records_the_lander() {
    let (_d, svc) = rig();
    let n = |slug: &str| names("local", PROJ, slug).unwrap();
    let handoff = |slug: &str| weftos_store::share::read_handoff(&Op(svc.operator_objects()), &n(slug).wt).unwrap().unwrap();
    for (slug, claim_at, promote_at) in [("feat-other", 30u64, 40u64), ("feat-expired", 30, 30 + weftos_store::share::CLAIM_TTL_MS + 5)] {
        let src = tempfile::tempdir().unwrap();
        published(&svc, slug, src.path());
        call(&svc, "claim", json!({"project": PROJ, "slug": slug, "agent": "bob", "host": "m2"}), claim_at).unwrap();
        weftos_store::overlay::Overlay::new(&Op(svc.operator_objects()), &n(slug).wt, &n(slug).pr).put("assets/room.bin", b"ROOM").unwrap();
        // carol landed the branch through git; bob's claim is live (or stale) and is not hers
        let p = call(&svc, "promote", json!({"project": PROJ, "slug": slug, "agent": "carol", "host": "m3"}), promote_at).unwrap();
        assert_eq!(p["promoted"], json!(["assets/room.bin"]), "{slug}");
        let h = handoff(slug);
        assert_eq!(h.state, weftos_store::share::State::Merged, "{slug}");
        assert_eq!(h.merger.as_ref().map(|m| format!("{}@{}", m.agent, m.host)).as_deref(), Some("carol@m3"), "{slug}");
        assert!(h.note.contains("bob@m2"), "the displaced claimer is named: {}", h.note);
    }
    // never claimed: the land is still recorded as the merge
    let src = tempfile::tempdir().unwrap();
    published(&svc, "feat-open", src.path());
    call(&svc, "promote", json!({"project": PROJ, "slug": "feat-open", "agent": "carol", "host": "m3"}), 60).unwrap();
    let h = handoff("feat-open");
    assert_eq!((h.state, h.merger.map(|m| m.agent)), (weftos_store::share::State::Merged, Some("carol".into())));
    // never published: there is no handoff to close, and none is invented
    call(&svc, "create", json!({"project": PROJ, "slug": "feat-quiet", "branch": "q", "base": "abc"}), 70).unwrap();
    call(&svc, "promote", json!({"project": PROJ, "slug": "feat-quiet", "agent": "carol", "host": "m3"}), 71).unwrap();
    assert!(weftos_store::share::read_handoff(&Op(svc.operator_objects()), &n("feat-quiet").wt).unwrap().is_none());
}

#[test]
fn the_daemons_wrapping_backend_forwards_the_conditional_writes_to_the_drive() {
    use weftos_store::drive::{materialise, sync_up};
    let (_d, svc) = rig();
    let n = StoreName::project("local", PROJ).unwrap();
    svc.ensure_store(&n, "cli", Parties::defaults_for(&n)).unwrap();
    let b = Op(svc.operator_objects());
    b.put(&n.bucket(), "proj/a.txt", b"alpha", false).unwrap();
    let t = tempfile::tempdir().unwrap();
    let dest = t.path().join("tree");
    materialise(&b, &n.bucket(), "proj", &dest).unwrap();
    std::fs::write(dest.join("a.txt"), "alpha v2").unwrap();
    let r = sync_up(&b, &dest).unwrap();
    // Through `Op` the store checked the etag. Without the forwarding this is (0, 1): `Unsupported`.
    assert_eq!((r.conditional, r.head_then_write), (1, 0));
    assert_eq!(b.get(&n.bucket(), "proj/a.txt").unwrap(), b"alpha v2");
}

// ---- S2 tests carried over from feat/adr126-s2 (registry signing, switch, retry, preview, quota, gc) ----

/// S3 review S2 #2: plain `verify` accepts a forged signature under a small-order key.
#[test]
fn a_small_order_trusted_key_is_refused_at_load_and_at_verify_and_strict_checks_are_used() {
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};
    // the identity point: with it, R = identity, S = 0 "verifies" any message under plain verify
    let mut identity = [0u8; 32];
    identity[0] = 1;
    let mut forged = [0u8; 64];
    forged[0] = 1;
    let vk = VerifyingKey::from_bytes(&identity).unwrap();
    assert!(vk.is_weak());
    assert!(vk.verify(b"any entry bytes", &Signature::from_bytes(&forged)).is_ok(), "the hole: plain verify accepts it");
    // ... so it must be refused wherever we decide who to trust
    let t = Trusted(&[identity]);
    assert!(!t.verify(&hex::encode(identity), "ed25519", b"any entry bytes", &forged), "verifier refuses");
    let d = tempfile::tempdir().unwrap();
    let f = d.path().join("trusted");
    std::fs::write(&f, format!("{}\n", hex::encode(identity))).unwrap();
    let path = f.to_string_lossy().into_owned();
    let cfg = SignCfg::from_lookup(|v| (v == "WEFTOS_WT_REGISTRY_TRUSTED").then(|| path.clone()));
    assert!(cfg.err().unwrap().contains("small-order"), "the trusted file is refused at load");
    // a real key still works, and one bit flipped in a good signature does not
    let sk = ed25519_dalek::SigningKey::from_bytes(&[11u8; 32]);
    let pubk = sk.verifying_key().to_bytes();
    let sig = Ed(sk).sign(b"entry");
    let good = Trusted(&[pubk]);
    assert!(good.verify(&hex::encode(pubk), "ed25519", b"entry", &sig));
    let mut bad = sig.clone();
    bad[10] ^= 1;
    assert!(!good.verify(&hex::encode(pubk), "ed25519", b"entry", &bad));
    assert!(!good.verify(&hex::encode(pubk), "ed25519", b"other", &sig));
    // a seed typo does not echo any of what was typed
    let e = hex32("zz-secret-ish").unwrap_err();
    assert!(!e.contains("zz") && !e.contains("secret"), "{e}");
}

/// S3 review S2 #4: a trusted-key file filters, and the verbs say how much each entry is trusted.
#[test]
fn a_trusted_key_file_filters_unless_audit_mode_and_every_verb_shows_trust() {
    let (_d, svc) = rig();
    let key = [7u8; 32];
    let pubkey = ed25519_dalek::SigningKey::from_bytes(&key).verifying_key().to_bytes();
    let signed = SignCfg::new(Some(key), Some(vec![pubkey]), true);
    let on = |cfg: &SignCfg, verb: &str, p: Value, now: u64| run_full(&svc, "local", &format!("store.worktree.{verb}"), p, now, false, cfg, &Guard::from_runtime());
    on(&signed, "create", json!({"project": PROJ, "slug": "feat-t", "branch": "feat/t", "base": "abc"}), 1).unwrap();
    let src = tempfile::tempdir().unwrap();
    let (paths, unc, chg) = files(src.path());
    // published by a daemon with no signing key: the entry is there but unsigned
    on(&SignCfg::off(), "publish", json!({"project": PROJ, "slug": "feat-t", "root": src.path().to_string_lossy(), "branch": "feat/t",
        "base": "abc", "head": "def", "paths_z": paths, "uncommitted_z": unc, "changes_z": chg}), 2).unwrap();

    // from_lookup would have made this filter; `require` here is what a trusted file alone gives
    let filtering = SignCfg::new(None, Some(vec![pubkey]), true);
    let audit = SignCfg::new(None, Some(vec![pubkey]), false);
    let s = on(&filtering, "search", json!({"project": PROJ, "query": "plan"}), 3).unwrap();
    assert_eq!(s["hits"], json!([]), "unsigned entry hidden");
    assert_eq!(s["trust"]["feat-t"], "unsigned", "the trust level is shown even for what is filtered out");
    let s = on(&audit, "search", json!({"project": PROJ, "query": "plan"}), 3).unwrap();
    assert_eq!(s["hits"].as_array().unwrap().len(), 1, "audit mode shows it");
    assert_eq!(s["trust"]["feat-t"], "unsigned", "and says why it is not to be trusted");
    let l = on(&audit, "leftovers", json!({"project": PROJ}), 3).unwrap();
    assert_eq!(l["trust"]["feat-t"], "unsigned");
    let st = src.path().join("f.json");
    let f = on(&audit, "follow", json!({"project": PROJ, "state": st.to_string_lossy()}), 4).unwrap();
    assert_eq!(f["trust"]["feat-t"], "unsigned");
    // no trusted file: no trust field content, behaviour as before
    assert_eq!(on(&SignCfg::off(), "search", json!({"project": PROJ, "query": "plan"}), 3).unwrap()["trust"], json!({}));

    // a signed republish is verified, and the follower's state file remembers the generation
    on(&signed, "publish", json!({"project": PROJ, "slug": "feat-t", "root": src.path().to_string_lossy(), "branch": "feat/t",
        "base": "abc", "head": "def2", "paths_z": paths, "uncommitted_z": unc, "changes_z": chg}), 5).unwrap();
    let st2 = src.path().join("f2.json");
    let f = on(&filtering, "follow", json!({"project": PROJ, "state": st2.to_string_lossy()}), 6).unwrap();
    assert_eq!(f["trust"]["feat-t"], "verified");
    assert!(std::fs::read_to_string(&st2).unwrap().contains("\"generations\""));
}

#[test]
fn a_worktree_store_over_its_quota_refuses_another_publish() {
    let (_d, svc) = rig();
    call(&svc, "create", json!({"project": PROJ, "slug": "feat-q", "branch": "feat/q", "base": "abc"}), 1).unwrap();
    let src = tempfile::tempdir().unwrap();
    let (paths, unc, chg) = files(src.path());
    let publish = |extra: Value, now| {
        let mut p = json!({"project": PROJ, "slug": "feat-q", "root": src.path().to_string_lossy(), "branch": "feat/q", "base": "abc",
            "head": "def", "paths_z": paths, "uncommitted_z": unc, "changes_z": chg});
        p.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        call(&svc, "publish", p, now)
    };
    publish(json!({}), 2).unwrap();
    let e = publish(json!({"quota_bytes": 10}), 3).unwrap_err();
    assert!(e.contains("over its 10 byte quota"), "{e}");
    publish(json!({"quota_bytes": 0}), 4).unwrap();
}

#[test]
fn follow_retry_is_off_by_default_and_validated() {
    let with = |v: Option<&'static str>| SignCfg::from_lookup(move |k| (k == "WEFTOS_WT_FOLLOW_RETRY_MS").then(|| v.map(String::from)).flatten());
    assert_eq!(with(None).unwrap().follow_retry_ms, 0, "off unless the owner picks option B");
    assert_eq!(with(Some("")).unwrap().follow_retry_ms, 0);
    assert_eq!(with(Some("750")).unwrap().follow_retry_ms, 750);
    for bad in ["-1", "fast", "10001", "1.5"] {
        assert!(with(Some(bad)).err().unwrap().contains("WEFTOS_WT_FOLLOW_RETRY_MS"), "{bad}");
    }
}

#[test]
fn landing_can_be_previewed_and_the_conflict_policy_and_size_cap_are_chosen_per_land() {
    let (_d, svc) = rig();
    call(&svc, "create", json!({"project": PROJ, "slug": "feat-c", "branch": "feat/c", "base": "abc"}), 1).unwrap();
    let n = names("local", PROJ, "feat-c").unwrap();
    let b = Op(svc.operator_objects());
    S1::put(&*svc.operator_objects(), &n.pr, "assets/logo.png", b"v1", false).unwrap();
    let ov = weftos_store::overlay::Overlay::new(&b, &n.wt, &n.pr);
    ov.put("assets/logo.png", b"mine").unwrap();
    ov.put("assets/new.bin", &[1u8; 300]).unwrap();
    S1::put(&*svc.operator_objects(), &n.pr, "assets/logo.png", b"teammate", false).unwrap();
    let p = |extra: Value| {
        let mut base = json!({"project": PROJ, "slug": "feat-c", "agent": "bob", "host": "m2"});
        base.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        base
    };

    let pv = call(&svc, "promote", p(json!({"dry_run": true})), 10).unwrap();
    assert_eq!(pv["dry_run"], true);
    assert_eq!(pv["preview"]["conflicts"], json!([["assets/logo.png", "both_edited"]]));
    assert_eq!(pv["preview"]["would_promote"], json!(["assets/new.bin"]));
    assert!(weftos_store::promote::is_promoted(&b, &n.wt).unwrap().is_none(), "a preview lands nothing");

    let e = call(&svc, "promote", p(json!({})), 11).unwrap_err();
    assert!(e.contains("conflict") && e.contains("assets/logo.png"), "{e}");
    assert!(call(&svc, "promote", p(json!({"on_conflict": "bogus"})), 12).is_err());
    let cap = call(&svc, "promote", p(json!({"on_conflict": "keep_project", "max_bytes": 100})), 13).unwrap_err();
    assert!(cap.contains("byte cap"), "{cap}");

    let r = call(&svc, "promote", p(json!({"on_conflict": "keep_project", "max_bytes": 0})), 14).unwrap();
    assert_eq!(r["promoted"], json!(["assets/new.bin"]));
    assert_eq!(S1::get(&*svc.operator_objects(), &n.pr, "assets/logo.png").unwrap(), b"teammate", "the project version was kept");
}

/// The daemon's real ed25519 signer and trusted-key verifier against a real RustFS (the bytes the
/// store holds are what is signed). Throwaway RustFS only: WEFTOS_STORE_TEST_RUSTFS_ENV.
#[test]
#[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
fn live_ed25519_signed_entries_verify_against_the_bytes_rustfs_holds() {
    use weftos_store::backend::AdminBackend;
    use weftos_store::registry_sign::{Trust, list_trusted_lenient, verify_registry_entry};
    let f = std::env::var("WEFTOS_STORE_TEST_RUSTFS_ENV").expect("WEFTOS_STORE_TEST_RUSTFS_ENV");
    let d = tempfile::tempdir().unwrap();
    let cfg = weftos_store::RustfsConfig::from_env_file(Path::new(&f), d.path().join("secrets")).unwrap();
    let be = weftos_store::RustfsBackend::new(cfg).unwrap();
    let b = format!("p-{}", ulid::Ulid::new().to_string().to_lowercase());
    be.create_bucket(&b, &[]).unwrap();

    let daemon = ed25519_dalek::SigningKey::from_bytes(&[3u8; 32]);
    let intruder = ed25519_dalek::SigningKey::from_bytes(&[4u8; 32]);
    let trusted = [daemon.verifying_key().to_bytes()];
    let entry = |slug: &str| RegistryEntry {
        slug: slug.into(),
        bucket: format!("w-01abc-{slug}"),
        branch: "feat/x".into(),
        head_commit: "abc".into(),
        state: weftos_store::share::State::Open,
        writer: Actor { agent: "a".into(), host: "h".into() },
        merger: None,
        doc_count: 1,
        file_count: 1,
        updated_ms: 5,
    };
    write_registry_entry_signed(&be, &b, &entry("good"), &Ed(daemon.clone())).unwrap();
    write_registry_entry_signed(&be, &b, &entry("forged"), &Ed(intruder)).unwrap();
    write_registry_entry(&be, &b, &entry("plain")).unwrap();

    let v = Trusted(&trusted);
    assert_eq!(verify_registry_entry(&be, &b, "good", &v).unwrap(), Trust::Verified);
    assert_eq!(verify_registry_entry(&be, &b, "forged", &v).unwrap(), Trust::Bad, "signed by a key nobody trusts");
    assert_eq!(verify_registry_entry(&be, &b, "plain", &v).unwrap(), Trust::Unsigned);
    let (ok, refused) = list_trusted_lenient(&be, &b, &v, true).unwrap();
    assert_eq!(ok.len(), 1);
    assert_eq!(refused, ["forged", "plain"]);

    // an edit of the stored entry breaks the signature
    let mut e = entry("good");
    e.head_commit = "edited".into();
    write_registry_entry(&be, &b, &e).unwrap();
    assert_eq!(verify_registry_entry(&be, &b, "good", &v).unwrap(), Trust::Bad);

    for s in ["good", "forged", "plain"] {
        remove_registry_entry_signed(&be, &b, s).unwrap();
    }
    be.delete_bucket(&b).unwrap();
}

#[test]
fn signing_settings_come_from_files_and_a_bad_file_is_an_error_not_unsigned() {
    let d = tempfile::tempdir().unwrap();
    let k = d.path().join("k");
    let t = d.path().join("t");
    std::fs::write(&k, format!("{}\n", "ab".repeat(32))).unwrap();
    // the seed file must be private; make it so (a fresh file is typically 0644)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&k, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    std::fs::write(&t, format!("# trusted\n{}\n", "cd".repeat(32))).unwrap();
    let env = |k: &'static str, t: &'static str, r: Option<&'static str>| {
        let (k, t) = (k.to_owned(), t.to_owned());
        move |v: &str| match v {
            "WEFTOS_WT_REGISTRY_SIGN_KEY" => (!k.is_empty()).then(|| k.clone()),
            "WEFTOS_WT_REGISTRY_TRUSTED" => (!t.is_empty()).then(|| t.clone()),
            "WEFTOS_WT_REGISTRY_REQUIRE_SIGNED" => r.map(str::to_owned),
            _ => None,
        }
    };
    let (ks, ts) = (k.to_string_lossy().into_owned(), t.to_string_lossy().into_owned());
    let leak = |s: String| -> &'static str { Box::leak(s.into_boxed_str()) };
    let (ks, ts) = (leak(ks), leak(ts));
    #[cfg(unix)]
    {
        assert!(SignCfg::from_lookup(env(ks, ts, Some("1"))).err().unwrap().contains("accessible to other users"), "a world-readable seed is refused");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&k, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let c = SignCfg::from_lookup(env(ks, ts, Some("1"))).unwrap();
    assert!(c.key.is_some() && c.trusted.as_ref().unwrap().len() == 1 && c.require);
    std::fs::write(&k, "tooshort").unwrap();
    assert!(SignCfg::from_lookup(env(ks, ts, None)).is_err());
    let missing = leak(d.path().join("missing").to_string_lossy().into_owned());
    assert!(SignCfg::from_lookup(env(missing, ts, None)).err().unwrap().contains("missing"));
    assert!(SignCfg::from_lookup(env("", "", Some("1"))).is_err(), "require without a trusted file is refused");
    assert!(SignCfg::from_lookup(env("", "", None)).unwrap().key.is_none());
    // a trusted-key file filters by itself; `0` is the explicit audit mode
    assert!(SignCfg::from_lookup(env("", ts, None)).unwrap().require, "trusted file implies filtering");
    assert!(!SignCfg::from_lookup(env("", ts, Some("0"))).unwrap().require);
    assert!(SignCfg::from_lookup(env("", "", Some("0"))).unwrap().trusted.is_none());
}

#[test]
fn usage_shows_each_worktree_and_gc_retires_only_landed_ones_after_the_keep_period_and_only_when_confirmed() {
    let (_d, svc) = rig();
    let day = weftos_store::retention::DAY_MS;
    for s in ["feat-old", "feat-new", "feat-open"] {
        call(&svc, "create", json!({"project": PROJ, "slug": s, "branch": s, "base": "abc"}), 1).unwrap();
        let n = names("local", PROJ, s).unwrap();
        weftos_store::overlay::Overlay::new(&Op(svc.operator_objects()), &n.wt, &n.pr).put(&format!("assets/{s}.bin"), &[9u8; 40]).unwrap();
    }
    call(&svc, "promote", json!({"project": PROJ, "slug": "feat-old"}), 2 * day).unwrap();
    call(&svc, "promote", json!({"project": PROJ, "slug": "feat-new"}), 20 * day).unwrap();
    let now = 21 * day;

    let u = call(&svc, "usage", json!({"project": PROJ, "quota_bytes": 100}), now).unwrap();
    let wts = u["worktrees"].as_array().unwrap();
    assert_eq!(wts.len(), 3);
    assert!(wts.iter().all(|w| w["over_quota"] == true), "each holds meta, base and 40 bytes: {u}");
    assert!(u["project"]["bytes"].as_u64().unwrap() >= 80, "two landed assets");
    let landed: Vec<_> = wts.iter().filter(|w| !w["landed_ms"].is_null()).collect();
    assert_eq!(landed.len(), 2);

    // dry run is the default and changes nothing
    let g = call(&svc, "gc", json!({"project": PROJ}), now).unwrap();
    assert_eq!(g["dry_run"], true);
    let retire = g["retire"].as_array().unwrap();
    assert_eq!(retire.len(), 1, "feat-old landed 19 days ago; feat-new 1 day ago; feat-open never: {g}");
    assert!(retire[0].as_str().unwrap().ends_with("/worktrees/feat-old/store"));
    assert_eq!(call(&svc, "usage", json!({"project": PROJ}), now).unwrap()["worktrees"].as_array().unwrap().len(), 3);

    let g = call(&svc, "gc", json!({"project": PROJ, "confirmed": true}), now).unwrap();
    assert_eq!(g["retired"].as_array().unwrap().len(), 1);
    assert_eq!(g["failed"], json!([]));
    let left = call(&svc, "usage", json!({"project": PROJ}), now).unwrap();
    assert_eq!(left["worktrees"].as_array().unwrap().len(), 2, "the open worktree and the recent landing remain");
    // what landed is still in the project, and the retired store's contents are archived
    let nm = names("local", PROJ, "feat-old").unwrap();
    assert_eq!(S1::get(&*svc.operator_objects(), &nm.pr, "assets/feat-old.bin").unwrap(), vec![9u8; 40]);
    let arch = S1::list(&*svc.operator_objects(), ARCHIVE_BUCKET, &format!("{}@", nm.wt)).unwrap();
    assert!(!arch.is_empty(), "retired store archived before deletion");

    // archive expiry is opt-in; with a short keep period the old archived objects go
    let none = call(&svc, "gc", json!({"project": PROJ}), 400 * day).unwrap();
    assert_eq!(none["expire_archived"], 0, "no archive_keep_days: the archive is untouched");
    let ex = call(&svc, "gc", json!({"project": PROJ, "archive_keep_days": 30, "confirmed": true}), 400 * day).unwrap();
    assert!(ex["expired_archived"].as_u64().unwrap() > 0, "{ex}");
    // keys archived at the promotion time (2 days in) are gone; the retire copies carry the service's
    // real clock (newer than this test's cutoff) and stay
    let rest = S1::list(&*svc.operator_objects(), ARCHIVE_BUCKET, &format!("{}@", nm.wt)).unwrap();
    assert!(rest.iter().all(|m| weftos_store::retention::archive_key_parts(&m.key).is_some_and(|(_, ts)| ts > 400 * day - 30 * day)), "{rest:?}");
}

#[test]
fn with_signing_on_the_daemon_signs_and_a_strict_follower_hides_forged_entries() {
    let (_d, svc) = rig();
    let key = [7u8; 32];
    let pubkey = ed25519_dalek::SigningKey::from_bytes(&key).verifying_key().to_bytes();
    let strict = SignCfg::new(Some(key), Some(vec![pubkey]), true);
    let other = SignCfg::new(None, Some(vec![[9u8; 32]]), true); // trusts a different key
    let on = |cfg: &SignCfg, verb: &str, p: Value, now: u64| run_full(&svc, "local", &format!("store.worktree.{verb}"), p, now, false, cfg, &Guard::from_runtime());
    on(&strict, "create", json!({"project": PROJ, "slug": "feat-s", "branch": "feat/s", "base": "abc"}), 1).unwrap();
    let src = tempfile::tempdir().unwrap();
    let (paths, unc, chg) = files(src.path());
    let pubp = json!({"project": PROJ, "slug": "feat-s", "root": src.path().to_string_lossy(), "branch": "feat/s", "base": "abc",
        "head": "def", "paths_z": paths, "uncommitted_z": unc, "changes_z": chg});
    on(&strict, "publish", pubp, 2).unwrap();

    let n = names("local", PROJ, "feat-s").unwrap();
    let b = Op(svc.operator_objects());
    assert!(S1::get(&*svc.operator_objects(), &n.reg, "worktrees.sig/feat-s.json").is_ok(), "the daemon wrote a signature");

    let hits = |cfg: &SignCfg| on(cfg, "search", json!({"project": PROJ, "query": "plan"}), 3).unwrap()["hits"].as_array().unwrap().len();
    assert_eq!(hits(&strict), 1, "a follower trusting the daemon key sees it");
    assert_eq!(hits(&other), 0, "a follower trusting another key refuses it");
    assert_eq!(hits(&SignCfg::off()), 1, "no trust config: behaves as before");

    // someone with project write redirects the entry; the old signature no longer matches
    let mut e = weftos_store::share_fetch::list_shared(&b, &n.reg).unwrap().remove(0);
    e.head_commit = "forged".into();
    weftos_store::share_fetch::write_registry_entry(&b, &n.reg, &e).unwrap();
    assert_eq!(hits(&strict), 0, "an edited entry is hidden from a strict follower");
    let st = src.path().join("f.json");
    let f = on(&strict, "follow", json!({"project": PROJ, "state": st.to_string_lossy()}), 4).unwrap();
    assert_eq!(f["changes"][0], json!({"change": "unreadable", "slug": "feat-s"}), "refused, and said so");
}

#[test]
fn with_the_registry_switch_on_entries_live_in_a_store_only_the_daemon_writes() {
    let (_d, svc) = rig();
    let run_on = |verb: &str, p: Value, now: u64| run_full(&svc, "local", &format!("store.worktree.{verb}"), p, now, true, &SignCfg::off(), &Guard::from_runtime());
    run_on("create", json!({"project": PROJ, "slug": "feat-r", "branch": "feat/r", "base": "abc"}), 1).unwrap();
    let src = tempfile::tempdir().unwrap();
    let (paths, unc, chg) = files(src.path());
    run_on(
        "publish",
        json!({"project": PROJ, "slug": "feat-r", "root": src.path().to_string_lossy(), "branch": "feat/r", "base": "abc", "head": "def",
               "paths_z": paths, "uncommitted_z": unc, "changes_z": chg}),
        2,
    )
    .unwrap();
    let on = names_with("local", PROJ, "feat-r", true).unwrap();
    let b = Op(svc.operator_objects());
    assert_eq!(weftos_store::share_fetch::list_shared(&b, &on.reg).unwrap().len(), 1, "registry is in its own store");
    assert!(weftos_store::share_fetch::list_shared(&b, &on.pr).unwrap().is_empty(), "nothing in the project store");
    // a follower sees it through the verb
    let st = src.path().join("f.json");
    let f = run_on("follow", json!({"project": PROJ, "state": st.to_string_lossy()}), 3).unwrap();
    assert_eq!(f["changes"][0]["change"], "new");
    // a governed principal cannot write into the registry store (no grants of any kind on it)
    let who = weftos_store::service::Caller::local(weftos_store::Principal::parse("agent:mallory").unwrap());
    let rn = weftos_store::StoreName::release("local", PROJ, REGISTRY_CHANNEL).unwrap();
    assert!(svc.put(&who, &rn, "worktrees/feat-r.json", b"{}", false).is_err());
    // default (switch off) keeps the registry in the project store, as before
    let off = names_with("local", PROJ, "feat-r", false).unwrap();
    assert_eq!(off.reg, off.pr);
}
