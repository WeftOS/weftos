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
