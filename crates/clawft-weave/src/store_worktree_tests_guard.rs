use std::sync::Arc;

use serde_json::json;
use weftos_store::{FsBackend, StoreService};

use super::*;
use crate::store_worktree_paths::Guard;
use std::path::Path;

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
    assert!(S1::list(&*svc.operator_objects(), &n.wt, weftos_store::share::OBJ_PREFIX).unwrap().is_empty(), "nor the pool");
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

