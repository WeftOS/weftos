use std::sync::Arc;

use clawft_kernel::exposure::ExposureRegistry;
use serde_json::json;
use weftos_store::{FsBackend, StoreService};

use super::*;
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
