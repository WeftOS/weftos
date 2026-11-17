//! Card 5e5b5d23, review of 08eee8a0a: what the `store.worktree.*` handlers do for a verified
//! principal (`actor`): `gc` records under it and cannot be turned into a purge (M1), and `create`
//! re-checks, under a lock, that the slug is not somebody else's live worktree (L-a, the race).
use std::sync::{Arc, Mutex};

use clawft_kernel::exposure::ExposureRegistry;
use serde_json::{Value, json};
use weftos_store::{Access, FsBackend, Principal, StoreName, StoreService};

use super::*;
use crate::store_rt::{ActivitySink, ChainAudit, RegistryGate};

const PROJ: &str = "01JBXK2M3N4P5Q6R7S8T9V0W1X";
const H: u64 = 3_600_000;
const DAY: u64 = 24 * H;

type Chain = Arc<Mutex<Vec<(String, Value)>>>;

fn rig() -> (tempfile::TempDir, StoreService, Chain, Arc<ExposureRegistry>) {
    let d = tempfile::tempdir().unwrap();
    let reg = Arc::new(ExposureRegistry::new("pg-1"));
    let be = Arc::new(FsBackend::open(d.path()).unwrap());
    let seen: Chain = Arc::new(Mutex::new(Vec::new()));
    let s2 = seen.clone();
    let f: clawft_kernel::exposure::AuditFn = Arc::new(move |k, v| s2.lock().unwrap().push((k.to_owned(), v)));
    let svc = StoreService::open(
        weftos_store::service::Config { node_id: "pg-1".into(), state_path: None, clock: clawft_kernel::exposure::registry::now_ms },
        be.clone(),
        be,
        Arc::new(RegistryGate(reg.clone())),
        Arc::new(ActivitySink(reg.clone())),
        Arc::new(ChainAudit(Some(f))),
    )
    .unwrap();
    (d, svc, seen, reg)
}

fn call(svc: &StoreService, verb: &str, p: Value, now: u64, actor: Option<&str>) -> Result<Value, String> {
    run_full_as(svc, "local", &format!("store.worktree.{verb}"), p, now, false, &SignCfg::off(), &Guard::none(), actor)
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
    call(svc, "create", json!({"project": PROJ, "slug": slug, "branch": slug, "base": "abc"}), 10, None).unwrap();
    call(svc, "publish", json!({"project": PROJ, "slug": slug, "root": dir.to_string_lossy(), "branch": slug, "base": "abc", "head": "h1", "paths_z": z, "uncommitted_z": z, "changes_z": c}), 20, None)
        .unwrap();
}

/// One landed worktree and one due orphan, at `late`.
fn landed_and_orphan(svc: &StoreService) -> u64 {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    publish(svc, "feat-landed", da.path());
    call(svc, "promote", json!({"project": PROJ, "slug": "feat-landed"}), 30, None).unwrap();
    publish(svc, "feat-gone", db.path());
    let since = 5 * DAY;
    let t0 = since + 25 * H;
    call(svc, "orphan", json!({"project": PROJ, "slug": "feat-gone", "since": since}), t0, None).unwrap();
    t0 + 14 * DAY
}

/// Every actor the activity log and the chain name, from the records gc left.
fn actors(seen: &Chain, reg: &ExposureRegistry) -> Vec<String> {
    let mut v: Vec<String> = seen.lock().unwrap().iter().filter_map(|(_, p)| p["actor"].as_str().map(str::to_owned)).collect();
    v.extend(reg.activity().query(None, 500).into_iter().map(|r| r.caller_node));
    v
}

fn gc_actors(actor: Option<&str>) -> (Vec<String>, Value) {
    let (_d, svc, seen, reg) = rig();
    let late = landed_and_orphan(&svc);
    let before = actors(&seen, &reg).len();
    let out = call(&svc, "gc", json!({"project": PROJ, "confirmed": true, "keep_days": 14}), late, actor).unwrap();
    (actors(&seen, &reg).split_off(before), out)
}

/// M1: every record gc writes names the principal who ran it, not `gc` or `gc@daemon`.
#[test]
fn a_principals_gc_records_name_the_principal() {
    let (new, out) = gc_actors(Some("agent:admin"));
    assert_eq!(out["retired"], json!(["feat-landed"]), "{out}");
    assert_eq!(out["orphans_retired"], json!(["feat-gone"]), "{out}");
    assert!(!new.is_empty(), "gc left records");
    assert!(new.iter().any(|a| a == "agent:admin"), "{new:?}");
    assert!(new.iter().all(|a| a != "gc" && a != "gc@daemon"), "a record names gc, not the principal: {new:?}");
    // The orphan retire event on the chain carries the actor too.
    let (_d, svc, seen, _reg) = rig();
    let late = landed_and_orphan(&svc);
    call(&svc, "gc", json!({"project": PROJ, "confirmed": true}), late, Some("agent:admin")).unwrap();
    let ev: Vec<Value> = seen.lock().unwrap().iter().filter(|(k, _)| k == "store.worktree.orphan_retire").map(|(_, v)| v.clone()).collect();
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0]["actor"], "agent:admin", "{ev:?}");
}

/// The operator's gc is unchanged: it is still `gc`.
#[test]
fn the_operators_gc_records_are_unchanged() {
    let (new, out) = gc_actors(None);
    assert_eq!(out["retired"], json!(["feat-landed"]), "{out}");
    assert!(new.iter().any(|a| a == "gc" || a == "gc@daemon"), "{new:?}");
    assert!(new.iter().all(|a| !a.starts_with("agent:")), "{new:?}");
}

/// M1, defence in depth: the handler refuses what the gate refuses, so a call that reaches it by
/// another road cannot purge. The operator may still do both.
#[test]
fn a_principals_gc_cannot_purge_in_the_handler_either() {
    let (_d, svc, _seen, _reg) = rig();
    let late = landed_and_orphan(&svc);
    for bad in [json!({"keep_days": 0}), json!({"keep_days": 13}), json!({"archive_keep_days": 0}), json!({"archive_keep_days": 30})] {
        let mut p = json!({"project": PROJ, "confirmed": true});
        p.as_object_mut().unwrap().extend(bad.as_object().unwrap().clone());
        let e = call(&svc, "gc", p, late, Some("agent:admin")).expect_err(&format!("{bad}"));
        assert!(e.contains("keep_days") && e.contains("operator"), "{bad}: {e}");
    }
    let n = names("local", PROJ, "feat-landed").unwrap();
    assert!(!S1::list(&*svc.operator_objects(), &n.wt, "").unwrap().is_empty(), "nothing was retired");
    // The operator can retire at keep_days 0 and then expire the archives.
    let c = call(&svc, "gc", json!({"project": PROJ, "confirmed": true, "keep_days": 0}), late, None).unwrap();
    assert_eq!(c["retired"], json!(["feat-landed"]), "{c}");
    let x = call(&svc, "gc", json!({"project": PROJ, "confirmed": true, "archive_keep_days": 0}), late + DAY, None).unwrap();
    assert!(x["expired_archived"].as_u64().unwrap() > 0, "{x}");
}

/// L-a, the race: the gate checked that the slug was nobody's, then another principal made it. The
/// handler checks again, so the second `create` cannot rewrite the first one's meta.
#[test]
fn create_rechecks_that_the_slug_is_not_somebody_elses_live_worktree() {
    let (_d, svc, _seen, _reg) = rig();
    let pr = StoreName::project("local", PROJ).unwrap();
    svc.create_store(&pr, "t", Default::default()).unwrap();
    let new = |branch: &str| json!({"project": PROJ, "slug": "feat-x", "branch": branch, "base": "1"});
    call(&svc, "create", new("first"), 10, Some("agent:pw1")).expect("the first create makes it");
    let n = names("local", PROJ, "feat-x").unwrap();
    let wt = StoreName::worktree("local", PROJ, "feat-x").unwrap();
    // pw2 holds nothing on it, and its call got past the gate when the slug was fresh.
    let e = call(&svc, "create", new("evil"), 20, Some("agent:pw2")).expect_err("a live worktree of another principal");
    assert!(e.contains("does not hold"), "{e}");
    assert_eq!(read_meta(&Op(svc.operator_objects()), &n.wt).unwrap().unwrap().branch, "first", "the meta was not rewritten");
    // Write on the worktree makes it the caller's own; the operator is never refused.
    svc.grant(&wt, Principal::parse("agent:pw2").unwrap(), Access::Write, "t").unwrap();
    call(&svc, "create", new("second"), 30, Some("agent:pw2")).unwrap();
    call(&svc, "create", new("third"), 40, None).unwrap();
}

/// L-a: a store that exists with someone's grants but no meta yet counts as live in the handler too.
#[test]
fn create_treats_an_existing_store_without_a_meta_as_live() {
    let (_d, svc, _seen, _reg) = rig();
    let pr = StoreName::project("local", PROJ).unwrap();
    svc.create_store(&pr, "t", Default::default()).unwrap();
    let wt = StoreName::worktree("local", PROJ, "feat-b").unwrap();
    svc.create_store(&wt, "t", Default::default()).unwrap();
    svc.grant(&wt, Principal::parse("agent:b").unwrap(), Access::Admin, "t").unwrap();
    let p = json!({"project": PROJ, "slug": "feat-b", "branch": "evil", "base": "0"});
    assert!(call(&svc, "create", p.clone(), 10, Some("agent:pw")).is_err());
    let n = names("local", PROJ, "feat-b").unwrap();
    assert!(read_meta(&Op(svc.operator_objects()), &n.wt).unwrap().is_none(), "nothing was written");
    call(&svc, "create", p, 10, Some("agent:b")).expect("its owner");
}
