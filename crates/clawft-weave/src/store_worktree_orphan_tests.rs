//! The `orphan` and `gc` verbs on the daemon, driven by `now_ms`: grace, refusals, dry run, confirmed
//! retire, and that a second worktree and the project's own objects stay as they were.
use std::sync::Arc;

use clawft_kernel::exposure::ExposureRegistry;
use serde_json::json;
use weftos_store::{FsBackend, StoreService};

use super::*;
use crate::store_rt::{ActivitySink, ChainAudit, RegistryGate};

const PROJ: &str = "01JBXK2M3N4P5Q6R7S8T9V0W1X";
const H: u64 = 3_600_000;
const DAY: u64 = 24 * H;

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

fn publish(svc: &StoreService, slug: &str, dir: &std::path::Path) {
    std::fs::create_dir_all(dir.join(".git")).unwrap();
    std::fs::create_dir_all(dir.join("docs")).unwrap();
    std::fs::write(dir.join("docs/plan.md"), format!("# Plan {slug}\n")).unwrap();
    let z = dir.join("paths.z");
    std::fs::write(&z, b"docs/plan.md\0").unwrap();
    let c = dir.join("chg.z");
    std::fs::write(&c, b"A\0docs/plan.md\0").unwrap();
    let (z, c) = (z.to_string_lossy().into_owned(), c.to_string_lossy().into_owned());
    call(svc, "create", json!({"project": PROJ, "slug": slug, "branch": slug, "base": "abc"}), 10).unwrap();
    call(svc, "publish", json!({"project": PROJ, "slug": slug, "root": dir.to_string_lossy(), "branch": slug, "base": "abc", "head": "h1", "paths_z": z, "uncommitted_z": z, "changes_z": c}), 20)
        .unwrap();
}

fn state_of(svc: &StoreService, slug: &str) -> Option<String> {
    let l = call(svc, "browse", json!({"project": PROJ}), 1).unwrap();
    l["worktrees"].as_array().unwrap().iter().find(|w| w["slug"] == slug).map(|w| w["state"].as_str().unwrap_or("").to_owned())
}

#[test]
fn orphan_marking_and_gc_run_on_the_clock_and_touch_only_the_orphan() {
    let (_d, svc) = rig();
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    publish(&svc, "feat-gone", da.path());
    publish(&svc, "feat-here", db.path());
    let n = names("local", PROJ, "feat-gone").unwrap();
    let other = names("local", PROJ, "feat-here").unwrap();
    S1::put(&*svc.operator_objects(), &n.pr, "assets/shared.bin", b"SHARED", false).unwrap();

    // no --since: refused. Missing for under 24 h: not marked, not an error.
    assert!(call(&svc, "orphan", json!({"project": PROJ, "slug": "feat-gone"}), 0).is_err());
    let since = 5 * DAY;
    let soon = call(&svc, "orphan", json!({"project": PROJ, "slug": "feat-gone", "since": since}), since + 23 * H).unwrap();
    assert_eq!((soon["marked"].as_bool(), soon["reason"].as_str()), (Some(false), Some("grace")));
    assert_eq!(state_of(&svc, "feat-gone").as_deref(), Some("open"));
    // an unpublished or unknown slug is refused
    assert!(call(&svc, "orphan", json!({"project": PROJ, "slug": "ghost", "since": since}), since + DAY).is_err());

    let t0 = since + 25 * H;
    let m = call(&svc, "orphan", json!({"project": PROJ, "slug": "feat-gone", "since": since}), t0).unwrap();
    assert_eq!((m["marked"].as_bool(), m["new"].as_bool(), m["last_head"].as_str()), (Some(true), Some(true), Some("h1")));
    assert_eq!(state_of(&svc, "feat-gone").as_deref(), Some("orphan"));
    let again = call(&svc, "orphan", json!({"project": PROJ, "slug": "feat-gone", "since": since}), t0 + H).unwrap();
    assert_eq!(again["new"].as_bool(), Some(false), "the sweep may call every pass");
    assert_eq!(state_of(&svc, "feat-here").as_deref(), Some("open"));

    // gc: the dry run lists the orphan with its age and what is due, and changes nothing
    let g = call(&svc, "gc", json!({"project": PROJ}), t0 + 13 * DAY).unwrap();
    assert_eq!(g["dry_run"], true);
    assert_eq!((g["orphans"][0]["slug"].as_str(), g["orphans"][0]["due"].as_bool()), (Some("feat-gone"), Some(false)));
    assert_eq!(g["orphans_retired"], json!([]));
    let c = call(&svc, "gc", json!({"project": PROJ, "confirmed": true}), t0 + 13 * DAY).unwrap();
    assert_eq!(c["orphans_retired"], json!([]), "not due yet: confirmed still keeps it");
    assert_eq!(state_of(&svc, "feat-gone").as_deref(), Some("orphan"));

    let late = t0 + 14 * DAY;
    let dry = call(&svc, "gc", json!({"project": PROJ}), late).unwrap();
    assert_eq!(dry["orphans"][0]["due"], true);
    assert_eq!(dry["orphans_retired"], json!([]), "a dry run retires nothing");
    assert!(!S1::list(&*svc.operator_objects(), &n.wt, "").unwrap().is_empty());

    let other_before = S1::list(&*svc.operator_objects(), &other.wt, "").unwrap().iter().map(|m| (m.key.clone(), m.etag.clone())).collect::<Vec<_>>();
    let done = call(&svc, "gc", json!({"project": PROJ, "confirmed": true}), late).unwrap();
    assert_eq!((done["orphans_retired"].clone(), done["orphans_failed"].clone()), (json!(["feat-gone"]), json!([])));
    assert_eq!(state_of(&svc, "feat-gone"), None, "registry entry removed");
    assert!(svc.list_stores().iter().all(|i| !i.name.contains("feat-gone")), "the store is forgotten");
    let arch = S1::list(&*svc.operator_objects(), ARCHIVE_BUCKET, &format!("{}@{late}/", n.wt)).unwrap();
    assert!(arch.iter().any(|m| m.key.ends_with(".wt/tree.manifest.json")) && arch.iter().any(|m| m.key.ends_with(".wt/git/../handoff.json") || m.key.ends_with(".wt/handoff.json")), "{arch:?}");
    // the other worktree and the project's own object are untouched
    let other_after = S1::list(&*svc.operator_objects(), &other.wt, "").unwrap().iter().map(|m| (m.key.clone(), m.etag.clone())).collect::<Vec<_>>();
    assert_eq!(other_before, other_after);
    assert_eq!(S1::get(&*svc.operator_objects(), &n.pr, "assets/shared.bin").unwrap(), b"SHARED");
    assert_eq!(state_of(&svc, "feat-here").as_deref(), Some("open"));
}

#[test]
fn a_handed_off_worktree_is_never_marked() {
    let (_d, svc) = rig();
    let d = tempfile::tempdir().unwrap();
    publish(&svc, "feat-offer", d.path());
    call(&svc, "claim", json!({"project": PROJ, "slug": "feat-offer", "agent": "bob", "host": "m2"}), 30).unwrap();
    let e = call(&svc, "orphan", json!({"project": PROJ, "slug": "feat-offer", "since": 1}), 10 * DAY).unwrap_err();
    assert!(e.contains("only an open one"), "{e}");
}

#[test]
fn gc_of_a_landed_worktree_drops_its_registry_entry() {
    let (_d, svc) = rig();
    let d = tempfile::tempdir().unwrap();
    publish(&svc, "feat-landed", d.path());
    call(&svc, "promote", json!({"project": PROJ, "slug": "feat-landed"}), 30).unwrap();
    assert_eq!(state_of(&svc, "feat-landed").as_deref(), Some("merged"));
    let c = call(&svc, "gc", json!({"project": PROJ, "keep_days": 0, "confirmed": true}), 10 * DAY).unwrap();
    assert_eq!((c["retired"].as_array().map(|a| a.len()), c["failed"].clone()), (Some(1), json!([])), "{c}");
    assert_eq!(state_of(&svc, "feat-landed"), None, "the registry entry goes with the bucket");
}
