use std::sync::atomic::{AtomicBool, Ordering};

use super::*;
use crate::audit::MemSink;
use crate::mem::MemBackend;

const P: &str = "01JBXK2M3N4P5Q6R7S8T9V0W1X";

#[derive(Default)]
struct Chain(Mutex<Vec<(String, Value)>>);
impl ChainSink for Chain {
    fn record(&self, k: &str, v: Value) {
        self.0.lock().unwrap().push((k.to_owned(), v));
    }
}

struct Gate(AtomicBool);
impl ExposureGate for Gate {
    fn register(&self, _: &str, _: &str) {}
    fn check(&self, _: &str) -> Result<(), String> {
        if self.0.load(Ordering::SeqCst) { Ok(()) } else { Err("switched off".into()) }
    }
}

struct Rig {
    svc: StoreService,
    be: Arc<MemBackend>,
    sink: Arc<MemSink>,
    chain: Arc<Chain>,
    gate: Arc<Gate>,
}

fn rig_at(state: Option<PathBuf>, be: Arc<MemBackend>) -> Rig {
    let sink = Arc::new(MemSink::default());
    let chain = Arc::new(Chain::default());
    let gate = Arc::new(Gate(AtomicBool::new(true)));
    let svc = StoreService::open(
        Config { node_id: "pg-1".into(), state_path: state, clock: || 1_000 },
        be.clone(),
        be.clone(),
        gate.clone(),
        sink.clone(),
        chain.clone(),
    )
    .unwrap();
    Rig { svc, be, sink, chain, gate }
}

fn rig() -> Rig {
    rig_at(None, Arc::new(MemBackend::new()))
}

fn node() -> StoreName {
    StoreName::node("weft", "pg-1").unwrap()
}
fn project() -> StoreName {
    StoreName::project("weft", P).unwrap()
}
fn cog(id: &str) -> Principal {
    Principal::parse(&format!("cog:{id}")).unwrap()
}
fn local(id: &str) -> Caller {
    Caller::local(cog(id))
}

#[test]
fn the_daemon_creates_a_node_store_and_a_project_store() {
    let r = rig();
    let n = r.svc.ensure_store(&node(), "daemon", Parties::defaults_for(&node())).unwrap();
    assert_eq!(n.bucket, "n-pg-1");
    assert!(r.be.bucket_exists("n-pg-1").unwrap());
    let tags = r.be.buckets().unwrap().into_iter().find(|(b, _)| b == "n-pg-1").unwrap().1;
    assert!(tags.contains(&("name".into(), "weftos://weft/nodes/pg-1/store".into())), "{tags:?}");
    assert!(n.grants.iter().any(|g| g.principal.to_string() == "node:pg-1" && g.access == Access::Write), "the node writes its own store");

    let p = r.svc.create_store(&project(), "daemon", Parties { readers: vec![Principal::parse("member:ann").unwrap()], admins: vec![Principal::parse("member:owner").unwrap()], ..Default::default() }).unwrap();
    assert_eq!(p.kind, "project");
    assert_eq!(p.grants.len(), 2);
    assert_eq!(r.svc.list_stores().len(), 2);
    assert!(matches!(r.svc.create_store(&node(), "daemon", Parties::default()), Err(StoreError::Exists(_))));
    assert!(r.svc.ensure_store(&node(), "daemon", Parties::default()).is_ok(), "ensure is idempotent");
    assert!(r.chain.0.lock().unwrap().iter().filter(|(k, _)| k == "store.create").count() == 2);
}

#[test]
fn a_cog_write_shows_in_the_activity_log() {
    let r = rig();
    r.svc.create_store(&node(), "daemon", Parties::default()).unwrap();
    r.svc.grant(&node(), cog("c1"), Access::Write, "op").unwrap();
    let m = r.svc.put(&local("c1"), &node(), "agents/c1/out.txt", b"hello", false).unwrap();
    assert_eq!(m.size, 5);
    assert_eq!(r.svc.get(&local("c1"), &node(), "agents/c1/out.txt").unwrap(), b"hello");
    let ev: Vec<_> = r.sink.events().into_iter().filter(|e| e.principal == "cog:c1").collect();
    let put = ev.iter().find(|e| e.op == "PutObject").expect("the write is logged");
    assert_eq!((put.store.as_str(), put.key.as_deref(), put.bytes, &put.outcome), ("weftos://weft/nodes/pg-1/store", Some("agents/c1/out.txt"), 5, &Outcome::Ok));
    assert!(ev.iter().any(|e| e.op == "GetObject" && e.bytes == 5), "the read is logged with its size");
}

#[test]
fn disabling_a_grant_refuses_the_next_request_here_and_at_the_backend() {
    let r = rig();
    r.svc.create_store(&node(), "daemon", Parties::default()).unwrap();
    r.svc.grant(&node(), cog("c1"), Access::Write, "op").unwrap();
    r.svc.put(&local("c1"), &node(), "k", b"1", false).unwrap();
    let key = r.svc.lock().keys.get("cog:c1").cloned().unwrap();
    r.be.authorize(&key, "n-pg-1", "s3:PutObject").expect("the backend allows it while the grant is on");

    r.svc.set_grant_enabled(&node(), &cog("c1"), false, "op").unwrap();
    let err = r.svc.put(&local("c1"), &node(), "k2", b"2", false).unwrap_err();
    assert!(matches!(err, StoreError::Refused(ref w) if w.contains("no enabled grant")), "{err}");
    assert!(r.svc.get(&local("c1"), &node(), "k").is_err(), "reads are refused too");
    assert!(r.be.authorize(&key, "n-pg-1", "s3:PutObject").is_err(), "a direct S3 request with the key is refused");
    assert_eq!(r.be.key_enabled(&key), Some(false));
    assert!(r.be.get_missing_check("n-pg-1", "k2"), "nothing was written");
    let refused = r.sink.events().into_iter().filter(|e| matches!(e.outcome, Outcome::Refused(_))).count();
    assert_eq!(refused, 2, "both refusals are in the log");

    r.svc.set_grant_enabled(&node(), &cog("c1"), true, "op").unwrap();
    r.svc.put(&local("c1"), &node(), "k2", b"2", false).expect("re-enabling takes effect at the next request");
    let kinds: Vec<String> = r.chain.0.lock().unwrap().iter().map(|(k, _)| k.clone()).collect();
    assert!(kinds.contains(&"store.grant".to_string()) && kinds.contains(&"store.grant.disable".to_string()) && kinds.contains(&"store.grant.enable".to_string()), "{kinds:?}");
}

#[test]
fn revoke_read_only_unknown_and_suspended_principals_are_refused_and_logged() {
    let r = rig();
    r.svc.create_store(&project(), "daemon", Parties::default()).unwrap();
    r.svc.grant(&project(), cog("reader"), Access::Read, "op").unwrap();
    r.svc.grant(&project(), cog("writer"), Access::Write, "op").unwrap();
    r.svc.put(&local("writer"), &project(), "assets/a", b"x", false).unwrap();
    assert_eq!(r.svc.get(&local("reader"), &project(), "assets/a").unwrap(), b"x");
    let e = r.svc.put(&local("reader"), &project(), "assets/b", b"y", false).unwrap_err().to_string();
    assert!(e.contains("holds read") && e.contains("needs append"), "{e}");
    assert!(r.svc.delete(&local("reader"), &project(), "assets/a").is_err());
    assert!(r.svc.get(&local("stranger"), &project(), "assets/a").is_err());

    r.svc.revoke(&project(), &cog("writer"), "op").unwrap();
    assert!(r.svc.put(&local("writer"), &project(), "assets/c", b"z", false).is_err());
    assert!(r.svc.revoke(&project(), &cog("writer"), "op").is_err(), "revoking twice says so");

    r.svc.suspend_principal(&cog("reader"), "rule hit").unwrap();
    assert!(r.svc.get(&local("reader"), &project(), "assets/a").unwrap_err().to_string().contains("suspended"));
    let key = r.svc.lock().keys.get("cog:reader").cloned().unwrap();
    assert_eq!(r.be.key_enabled(&key), Some(false));
    r.svc.resume_principal(&cog("reader")).unwrap();
    assert!(r.svc.get(&local("reader"), &project(), "assets/a").is_ok());
    assert_eq!(r.be.key_enabled(&key), Some(true));
}

#[test]
fn revoking_the_last_grant_disables_the_key_and_leaves_no_policy_statements() {
    let r = rig();
    r.svc.create_store(&node(), "daemon", Parties::default()).unwrap();
    r.svc.grant(&node(), cog("c1"), Access::Write, "op").unwrap();
    let key = r.svc.lock().keys.get("cog:c1").cloned().unwrap();
    r.svc.revoke(&node(), &cog("c1"), "op").unwrap();
    assert_eq!(r.be.key_enabled(&key), Some(false));
    assert!(r.be.policy_of(&key).unwrap()["Statement"].as_array().unwrap().is_empty());
}

#[test]
fn a_remote_caller_needs_the_exposure_on_and_a_local_one_does_not() {
    let r = rig();
    r.svc.create_store(&project(), "daemon", Parties::default()).unwrap();
    let peer = Principal::parse("node:other").unwrap();
    r.svc.grant(&project(), peer.clone(), Access::Read, "op").unwrap();
    r.svc.grant(&project(), cog("c1"), Access::Write, "op").unwrap();
    r.svc.put(&local("c1"), &project(), "k", b"v", false).unwrap();
    assert!(r.svc.get(&Caller::mesh(peer.clone()), &project(), "k").is_ok());
    r.gate.0.store(false, Ordering::SeqCst);
    let e = r.svc.get(&Caller::mesh(peer), &project(), "k").unwrap_err().to_string();
    assert!(e.contains("exposure") && e.contains("switched off"), "{e}");
    assert!(r.svc.get(&local("c1"), &project(), "k").is_ok(), "switching the exposure off withdraws it from the mesh, not from this node");
}

#[test]
fn backend_failures_are_logged_as_errors_and_successful_listings_are_not_logged() {
    let r = rig();
    r.svc.create_store(&node(), "daemon", Parties::default()).unwrap();
    r.svc.grant(&node(), cog("c1"), Access::Write, "op").unwrap();
    assert!(matches!(r.svc.get(&local("c1"), &node(), "missing"), Err(StoreError::Backend(BackendError::NoObject(_)))));
    assert!(r.sink.events().iter().any(|e| e.op == "GetObject" && matches!(e.outcome, Outcome::Error(_))));
    let before = r.sink.events().len();
    assert!(r.svc.list(&local("c1"), &node(), "").unwrap().is_empty());
    assert_eq!(r.sink.events().len(), before, "a successful listing is not logged");
    assert!(r.svc.list(&local("nobody"), &node(), "").is_err());
    assert_eq!(r.sink.events().len(), before + 1, "a refused listing is");
    assert!(r.svc.put(&local("c1"), &node(), "../x", b"1", false).is_err());
    let w = r.svc.put(&local("c1"), &node(), "once", b"1", true).unwrap();
    assert_eq!(w.size, 1);
    assert!(matches!(r.svc.put(&local("c1"), &node(), "once", b"2", true), Err(StoreError::Backend(BackendError::Exists(_)))));
}

#[test]
fn state_survives_a_restart_and_a_disabled_grant_stays_disabled() {
    let d = tempfile::tempdir().unwrap();
    let sp = d.path().join("store-state.json");
    let be = Arc::new(MemBackend::new());
    {
        let r = rig_at(Some(sp.clone()), be.clone());
        r.svc.create_store(&node(), "daemon", Parties::default()).unwrap();
        r.svc.grant(&node(), cog("c1"), Access::Write, "op").unwrap();
        r.svc.grant(&node(), cog("c2"), Access::Write, "op").unwrap();
        r.svc.set_grant_enabled(&node(), &cog("c2"), false, "op").unwrap();
        r.svc.put(&local("c1"), &node(), "k", b"1", false).unwrap();
    }
    let r = rig_at(Some(sp.clone()), be);
    assert_eq!(r.svc.list_stores().len(), 1);
    assert!(r.svc.put(&local("c1"), &node(), "k2", b"2", false).is_ok());
    assert!(r.svc.put(&local("c2"), &node(), "k3", b"3", false).is_err());
    std::fs::write(&sp, b"{broken").unwrap();
    assert!(StoreService::open(Config { node_id: "x".into(), state_path: Some(sp), clock: || 0 }, r.be.clone(), r.be.clone(), r.gate.clone(), r.sink.clone(), r.chain.clone()).is_err(), "a broken state file is an error, never an empty table");
}

#[test]
fn promote_copies_promotable_paths_and_archives_or_leaves_the_rest() {
    let r = rig();
    let wt = StoreName::worktree("weft", P, "feat-x").unwrap();
    r.svc.create_store(&project(), "daemon", Parties::default()).unwrap();
    r.svc.create_store(&wt, "daemon", Parties::default()).unwrap();
    r.svc.grant(&wt, cog("a1"), Access::Write, "op").unwrap();
    r.svc.grant(&project(), cog("a1"), Access::Write, "op").unwrap();
    for (k, v) in [("assets/new.bin", "n"), ("assets/same.bin", "s"), ("assets/changed.bin", "w2"), ("scratch/tmp.log", "t")] {
        r.svc.put(&local("a1"), &wt, k, v.as_bytes(), false).unwrap();
    }
    r.svc.put(&local("a1"), &project(), "assets/same.bin", b"s", false).unwrap();
    r.svc.put(&local("a1"), &project(), "assets/changed.bin", b"w1", false).unwrap();

    let rep = r.svc.promote(&wt, &["assets/".into()], true, "land").unwrap();
    assert_eq!((rep.promoted, rep.unchanged, rep.left, rep.archived), (2, 1, 0, 1));
    assert_eq!(rep.replaced, vec!["assets/changed.bin".to_string()]);
    assert_eq!(r.be.get(&project().bucket(), "assets/changed.bin").unwrap(), b"w2");
    assert_eq!(r.be.get(ARCHIVE_BUCKET, &archive_key(&project().bucket(), 1_000, "replaced/assets/changed.bin")).unwrap(), b"w1", "what promote replaced is kept");
    assert!(r.sink.events().iter().any(|e| e.op == "CopyObject" && e.key.as_deref() == Some("assets/new.bin")), "each promoted object is logged");
    assert_eq!(r.be.get(ARCHIVE_BUCKET, &archive_key(&wt.bucket(), 1_000, "scratch/tmp.log")).unwrap(), b"t");
    let left = r.svc.promote(&wt, &["assets/".into()], false, "land").unwrap();
    assert_eq!((left.promoted, left.unchanged, left.left), (0, 3, 1), "promoting twice changes nothing");
    assert!(r.svc.promote(&project(), &[], false, "x").is_err(), "only worktree stores promote");
    let other = StoreName::worktree("weft", "01JBXK2M3N4P5Q6R7S8T9V0W2Y", "z").unwrap();
    r.svc.create_store(&other, "d", Parties::default()).unwrap();
    assert!(r.svc.promote(&other, &[], false, "x").unwrap_err().to_string().contains("project store"), "no project store, no promotion");
}

#[test]
fn archive_keeps_the_content_and_closes_the_store_and_delete_removes_it() {
    let r = rig();
    r.svc.create_store(&project(), "daemon", Parties::default()).unwrap();
    r.svc.grant(&project(), cog("c1"), Access::Write, "op").unwrap();
    r.svc.put(&local("c1"), &project(), "a/b", b"data", false).unwrap();
    assert_eq!(r.svc.archive(&project(), false, "op").unwrap(), 1);
    assert_eq!(r.be.get(ARCHIVE_BUCKET, &archive_key(&project().bucket(), 1_000, "a/b")).unwrap(), b"data");
    assert!(r.svc.put(&local("c1"), &project(), "late", b"x", false).unwrap_err().to_string().contains("archived"));
    assert!(r.svc.grant(&project(), cog("c2"), Access::Read, "op").is_err());
    assert!(r.svc.archive(&project(), false, "op").is_err(), "already archived");
    let key = r.svc.lock().keys.get("cog:c1").cloned().unwrap();
    assert_eq!(r.be.key_enabled(&key), Some(false));

    let n = StoreName::node("weft", "gone").unwrap();
    r.svc.create_store(&n, "d", Parties::default()).unwrap();
    r.svc.grant(&n, cog("c9"), Access::Write, "op").unwrap();
    r.svc.put(&local("c9"), &n, "k", b"v", false).unwrap();
    r.svc.archive(&n, true, "op").unwrap();
    assert!(!r.be.bucket_exists("n-gone").unwrap() && !r.svc.has_store(&n));
    assert_eq!(r.be.get(ARCHIVE_BUCKET, &archive_key("n-gone", 1_000, "k")).unwrap(), b"v", "archived, then deleted");
    r.svc.create_store(&n, "d", Parties::default()).expect("the name can be reused after delete");
}

#[test]
fn rustfs_audit_log_and_notifications_are_resolved_into_the_same_events() {
    let r = rig();
    r.svc.create_store(&node(), "daemon", Parties::default()).unwrap();
    r.svc.grant(&node(), cog("c1"), Access::Write, "op").unwrap();
    let key = r.svc.lock().keys.get("cog:c1").cloned().unwrap();
    let base = r.sink.events().len();
    assert!(r.svc.ingest_audit_line(&format!(r#"{{"api":{{"name":"PutObject","bucket":"n-pg-1","object":"k","statusCode":200}},"accessKey":"{key}"}}"#)));
    assert!(r.svc.ingest_log_line(&format!(r#"{{"message":"s3_authorization_denied","account":"{key}","bucket":"n-pg-1","object":"o","action":"s3:DeleteObject","reason":"no"}}"#)));
    assert!(!r.svc.ingest_audit_line("garbage") && !r.svc.ingest_log_line(r#"{"message":"other"}"#));
    let seen = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
    let s2 = seen.clone();
    r.svc.set_invalidator(Arc::new(move |st, k| s2.lock().unwrap().push((st.to_owned(), k.to_owned()))));
    let n = r.svc.ingest_notification(r#"{"Records":[{"eventName":"s3:ObjectCreated:Put","s3":{"bucket":{"name":"n-pg-1"},"object":{"key":"k","size":3}}}]}"#);
    assert_eq!(n, 1);
    let ev = r.sink.events();
    let put = &ev[base];
    assert_eq!((put.principal.as_str(), put.store.as_str(), put.source), ("cog:c1", "weftos://weft/nodes/pg-1/store", Source::RustfsAudit));
    let deny = &ev[base + 1];
    assert_eq!((deny.op.as_str(), deny.source), ("DeleteObject", Source::RustfsLog));
    assert!(matches!(deny.outcome, Outcome::Refused(_)));
    assert_eq!(seen.lock().unwrap().as_slice(), &[("weftos://weft/nodes/pg-1/store".to_string(), "k".to_string())]);
}

#[test]
fn closed_stores_take_no_promotion_and_a_failed_delete_can_be_retried() {
    let r = rig();
    let wt = StoreName::worktree("weft", P, "w").unwrap();
    r.svc.create_store(&project(), "d", Parties::default()).unwrap();
    r.svc.create_store(&wt, "d", Parties::default()).unwrap();
    r.svc.grant(&wt, cog("a"), Access::Write, "op").unwrap();
    r.svc.put(&local("a"), &wt, "assets/x", b"1", false).unwrap();
    r.svc.archive(&project(), false, "op").unwrap();
    assert!(r.svc.promote(&wt, &["assets/".into()], false, "land").unwrap_err().to_string().contains("archived"));
    assert_eq!(r.be.list(&project().bucket(), "").unwrap().len(), 0, "nothing was written into the closed store");
    // Archived but the bucket is still there: delete finishes the job.
    assert!(r.svc.archive(&project(), false, "op").is_err());
    r.svc.archive(&project(), true, "op").unwrap();
    assert!(!r.svc.has_store(&project()));
}

#[test]
fn deleting_a_store_leaves_no_policy_that_names_it_and_regrant_keeps_a_disabled_grant_off() {
    let r = rig();
    r.svc.create_store(&node(), "d", Parties::default()).unwrap();
    r.svc.grant(&node(), cog("c1"), Access::Write, "op").unwrap();
    let key = r.svc.lock().keys.get("cog:c1").cloned().unwrap();
    r.svc.set_grant_enabled(&node(), &cog("c1"), false, "op").unwrap();
    r.svc.grant(&node(), cog("c1"), Access::Admin, "op").unwrap();
    assert!(r.svc.put(&local("c1"), &node(), "k", b"1", false).is_err(), "changing the level does not switch a grant back on");
    r.svc.archive(&node(), true, "op").unwrap();
    assert!(r.be.policy_of(&key).unwrap()["Statement"].as_array().unwrap().is_empty());
    assert_eq!(r.be.key_enabled(&key), Some(false));
}

#[test]
fn suspend_and_resume_are_in_the_activity_log() {
    let r = rig();
    r.svc.create_store(&node(), "d", Parties::default()).unwrap();
    r.svc.grant(&node(), cog("c1"), Access::Read, "op").unwrap();
    r.svc.suspend_principal(&cog("c1"), "rule hit").unwrap();
    r.svc.resume_principal(&cog("c1")).unwrap();
    let ops: Vec<String> = r.sink.events().into_iter().map(|e| e.op).collect();
    assert!(ops.contains(&"Suspend".to_string()) && ops.contains(&"Resume".to_string()), "{ops:?}");
}

#[test]
fn a_release_store_lets_ci_write_once_and_the_sweeper_only_delete() {
    let r = rig();
    let rel = StoreName::release("weft", P, "stable").unwrap();
    r.svc.create_store(&rel, "daemon", Parties::defaults_for(&rel)).unwrap();
    let ci = Caller::local(Principal::parse(CI_PRINCIPAL).unwrap());
    let sw = Caller::local(Principal::parse(SWEEPER_PRINCIPAL).unwrap());

    // CI creates a build artifact once, and cannot replace, copy or delete it.
    r.svc.put(&ci, &rel, "v1/app.tar", b"build", true).unwrap();
    assert!(matches!(r.svc.put(&ci, &rel, "v1/app.tar", b"again", true), Err(StoreError::Backend(BackendError::Exists(_)))));
    assert!(matches!(r.svc.put(&ci, &rel, "v1/app.tar", b"evil", false), Err(StoreError::Refused(_))), "an unconditional put could overwrite");
    let etag = r.svc.head(&ci, &rel, "v1/app.tar").unwrap().etag;
    assert!(matches!(r.svc.put_if_match(&ci, &rel, "v1/app.tar", b"evil", &etag), Err(StoreError::Refused(_))), "a swap overwrites too");
    assert!(matches!(r.svc.delete(&ci, &rel, "v1/app.tar"), Err(StoreError::Refused(_))));
    assert_eq!(r.svc.get(&ci, &rel, "v1/app.tar").unwrap(), b"build", "the artifact is unchanged");

    // The sweeper reads and deletes, and cannot put.
    assert_eq!(r.svc.list(&sw, &rel, "v1/").unwrap().len(), 1);
    assert!(matches!(r.svc.put(&sw, &rel, "v1/x", b"x", true), Err(StoreError::Refused(_))));
    r.svc.delete(&sw, &rel, "v1/app.tar").unwrap();
    assert!(r.svc.list(&sw, &rel, "").unwrap().is_empty());

    // Both entries are real grants: visible, and the backend policy matches.
    let grants = r.svc.lock().grants.for_store(&rel);
    assert!(grants.iter().any(|g| g.principal.to_string() == CI_PRINCIPAL && g.access == Access::Append));
    assert!(grants.iter().any(|g| g.principal.to_string() == SWEEPER_PRINCIPAL && g.access == Access::Sweep));
}

#[test]
fn put_if_match_through_the_service_is_gated_and_logged() {
    let r = rig();
    let n = StoreName::project("weft", P).unwrap();
    r.svc.create_store(&n, "daemon", Parties::default()).unwrap();
    let w = Principal::parse("cog:w").unwrap();
    r.svc.grant(&n, w.clone(), Access::Write, "op").unwrap();
    let who = Caller::local(w);
    let m = r.svc.put(&who, &n, "k", b"1", false).unwrap();
    r.svc.put_if_match(&who, &n, "k", b"2", &m.etag).unwrap();
    assert!(matches!(r.svc.put_if_match(&who, &n, "k", b"3", &m.etag), Err(StoreError::Backend(BackendError::Stale(_)))));
    let nobody = Caller::local(Principal::parse("cog:nobody").unwrap());
    assert!(matches!(r.svc.put_if_match(&nobody, &n, "k", b"x", &m.etag), Err(StoreError::Refused(_))));
}

/// An admin backend that can be told to fail its pushes, to model RustFS being down.
struct Flaky {
    inner: Arc<MemBackend>,
    down: AtomicBool,
}
impl Flaky {
    fn check(&self) -> Result<(), BackendError> {
        if self.down.load(Ordering::SeqCst) { Err(BackendError::Io("backend down".into())) } else { Ok(()) }
    }
}
impl AdminBackend for Flaky {
    fn create_bucket(&self, b: &str, t: &[(String, String)]) -> Result<(), BackendError> {
        self.inner.create_bucket(b, t)
    }
    fn delete_bucket(&self, b: &str) -> Result<(), BackendError> {
        self.inner.delete_bucket(b)
    }
    fn bucket_exists(&self, b: &str) -> Result<bool, BackendError> {
        self.inner.bucket_exists(b)
    }
    fn buckets(&self) -> Result<Vec<crate::backend::BucketTags>, BackendError> {
        self.inner.buckets()
    }
    fn create_access_key(&self, p: &str) -> Result<crate::backend::AccessKey, BackendError> {
        self.inner.create_access_key(p)
    }
    fn set_key_enabled(&self, k: &str, e: bool) -> Result<(), BackendError> {
        self.check()?;
        self.inner.set_key_enabled(k, e)
    }
    fn put_policy(&self, k: &str, p: &Value) -> Result<(), BackendError> {
        self.check()?;
        self.inner.put_policy(k, p)
    }
    fn delete_access_key(&self, k: &str) -> Result<(), BackendError> {
        self.inner.delete_access_key(k)
    }
}

fn flaky_rig() -> (StoreService, Arc<MemBackend>, Arc<Flaky>) {
    let be = Arc::new(MemBackend::new());
    let fl = Arc::new(Flaky { inner: be.clone(), down: AtomicBool::new(false) });
    let svc = StoreService::open(
        Config { node_id: "pg-1".into(), state_path: None, clock: || 1_000 },
        fl.clone(),
        be.clone(),
        Arc::new(Gate(AtomicBool::new(true))),
        Arc::new(MemSink::default()),
        Arc::new(Chain::default()),
    )
    .unwrap();
    (svc, be, fl)
}

#[test]
fn a_push_that_failed_is_made_good_by_reconcile() {
    let (svc, be, fl) = flaky_rig();
    let n = project();
    svc.create_store(&n, "daemon", Parties::default()).unwrap();
    let c = cog("c");
    svc.grant(&n, c.clone(), Access::Write, "op").unwrap();
    let key = be.keys_of(&c.to_string()).pop().expect("key");
    assert_eq!(be.key_enabled(&key), Some(true));
    fl.down.store(true, Ordering::SeqCst);
    assert!(svc.set_grant_enabled(&n, &c, false, "op").is_err(), "the push fails");
    assert!(svc.get(&local("c"), &n, "x").is_err(), "the service already refuses");
    assert_eq!(be.key_enabled(&key), Some(true), "the backend key is still on: it is behind");
    assert!(!svc.reconcile().is_empty(), "still down: reported, not hidden");
    fl.down.store(false, Ordering::SeqCst);
    assert!(svc.reconcile().is_empty());
    assert_eq!(be.key_enabled(&key), Some(false), "now the backend refuses the key too");
}

#[test]
fn a_bucket_made_but_never_recorded_is_adopted_and_a_foreign_one_is_not() {
    let (svc, be, _fl) = flaky_rig();
    let n = project();
    be.create_bucket(&n.bucket(), &n.tags()).unwrap();
    svc.ensure_store(&n, "daemon", Parties::default()).expect("our own bucket, recorded now");
    assert!(svc.has_store(&n));
    let other = StoreName::project("weft", "01JBXK2M3N4P5Q6R7S8T9V0W2Y").unwrap();
    be.create_bucket(&other.bucket(), &[]).unwrap();
    assert!(matches!(svc.create_store(&other, "daemon", Parties::default()), Err(StoreError::Exists(_))), "someone else's bucket");
}

#[test]
fn an_archived_store_cannot_have_a_grant_switched_back_on() {
    let r = rig();
    let n = project();
    r.svc.create_store(&n, "daemon", Parties::default()).unwrap();
    let c = cog("c");
    r.svc.grant(&n, c.clone(), Access::Write, "op").unwrap();
    r.svc.archive(&n, false, "op").unwrap();
    assert!(matches!(r.svc.set_grant_enabled(&n, &c, true, "op"), Err(StoreError::Refused(_))));
}

#[test]
fn object_lock_needs_a_backend_that_has_it_and_a_store_made_for_it() {
    let r = rig();
    let rel = StoreName::release("weft", P, "stable").unwrap();
    // The in-memory backend has no Object Lock: asking for a locked store is an error, and nothing is recorded.
    assert!(matches!(r.svc.create_locked_store(&rel, "daemon", Parties::defaults_for(&rel)), Err(StoreError::Backend(BackendError::Unsupported(_)))));
    assert!(!r.svc.has_store(&rel));
    // An ordinary store takes no locked puts.
    r.svc.create_store(&rel, "daemon", Parties::defaults_for(&rel)).unwrap();
    assert!(!r.svc.is_locked(&rel));
    let ci = Caller::local(Principal::parse(CI_PRINCIPAL).unwrap());
    let opts = crate::backend::PutOpts { if_none_match: true, ..Default::default() };
    assert!(matches!(r.svc.put_locked(&ci, &rel, "a", b"x", &opts), Err(StoreError::Refused(_))));
}

#[test]
fn listing_versions_needs_a_grant_and_a_backend_that_keeps_them() {
    let r = rig();
    r.svc.create_store(&project(), "daemon", Parties::default()).unwrap();
    r.svc.grant(&project(), cog("reader"), Access::Read, "op").unwrap();
    assert!(r.svc.list_versions(&local("stranger"), &project(), "").is_err(), "no grant, no listing");
    // The in-memory backend keeps one copy per key; the answer says so rather than inventing versions.
    assert!(matches!(r.svc.list_versions(&local("reader"), &project(), ""), Err(StoreError::Backend(BackendError::Unsupported(_)))));
}

#[test]
fn the_owner_switches_reach_the_backend_keys_both_ways_after_a_reconcile() {
    use crate::policy::PolicyOpts;
    let r = rig();
    let rel = StoreName::release("weft", P, "stable").unwrap();
    let (ci, sw) = (Principal::parse(CI_PRINCIPAL).unwrap(), Principal::parse(SWEEPER_PRINCIPAL).unwrap());
    r.svc.create_store(&rel, "daemon", Parties { appenders: vec![ci.clone()], sweepers: vec![sw.clone()], ..Default::default() }).unwrap();
    let key = |p: &Principal| crate::backend::AdminBackend::create_access_key(&*r.be, &p.to_string()).unwrap().id;
    let (ci_k, sw_k) = (key(&ci), key(&sw));
    let can_put = |k: &str| r.be.authorize(k, &rel.bucket(), "s3:PutObject").is_ok();
    // Default: both keys are put-less at the backend.
    assert!(!can_put(&ci_k) && !can_put(&sw_k));
    // The owner turns the sweeper switch on: after a reconcile only the sweeper key can put.
    r.svc.set_policy_opts(PolicyOpts { sweeper_put: true, ..Default::default() });
    assert!(r.svc.reconcile().is_empty());
    assert!(can_put(&sw_k) && !can_put(&ci_k));
    // CI switch on, sweeper off.
    r.svc.set_policy_opts(PolicyOpts { ci_direct_put: true, ..Default::default() });
    assert!(r.svc.reconcile().is_empty());
    assert!(can_put(&ci_k) && !can_put(&sw_k));
    // Back to the default: both refused again, and the service's own gate never moved.
    r.svc.set_policy_opts(PolicyOpts::default());
    assert!(r.svc.reconcile().is_empty());
    assert!(!can_put(&ci_k) && !can_put(&sw_k));
    let sweeper = Caller::local(sw);
    assert!(r.svc.put(&sweeper, &rel, "v1/x", b"x", false).is_err(), "the service still refuses a sweeper put");
}

#[test]
fn a_stores_lock_configuration_is_read_through_the_service_or_reported_unsupported() {
    let r = rig();
    let rel = StoreName::release("weft", P, "stable").unwrap();
    assert!(r.svc.lock_config(&rel).is_err(), "unknown store");
    r.svc.create_store(&rel, "daemon", Parties::defaults_for(&rel)).unwrap();
    assert!(matches!(r.svc.lock_config(&rel), Err(StoreError::Backend(BackendError::Unsupported(_)))));
}

#[test]
fn lifecycle_provisioning_is_checked_recorded_and_unsupported_on_local_backends() {
    let r = rig();
    let n = project();
    let rule = crate::backend::LifecycleRule { id: "tmp".into(), prefix: "tmp/".into(), enabled: true, expire_days: Some(7), noncurrent_days: None, abort_multipart_days: None };
    assert!(r.svc.set_lifecycle(&n, std::slice::from_ref(&rule), "op").is_err(), "unknown store");
    r.svc.create_store(&n, "daemon", Parties::default()).unwrap();
    assert!(matches!(r.svc.set_lifecycle(&n, std::slice::from_ref(&rule), "op"), Err(StoreError::Backend(BackendError::Unsupported(_)))));
    assert!(matches!(r.svc.lifecycle(&n), Err(StoreError::Backend(BackendError::Unsupported(_)))));
    assert!(r.chain.0.lock().unwrap().iter().all(|(k, _)| k != "store.lifecycle"), "a refused provisioning leaves no record");
}

#[test]
fn extending_retention_needs_a_grant_and_a_store_made_with_lock() {
    let r = rig();
    let rel = StoreName::release("weft", P, "stable").unwrap();
    r.svc.create_store(&rel, "daemon", Parties::defaults_for(&rel)).unwrap();
    let ci = Caller::local(Principal::parse(CI_PRINCIPAL).unwrap());
    // CI (append) is allowed to call it; this store has no lock, so it is refused for that.
    assert!(matches!(r.svc.extend_retention(&ci, &rel, "a", "v", 4_000_000_000, crate::backend::LockMode::Compliance), Err(StoreError::Refused(_))));
    assert!(r.svc.extend_retention(&local("stranger"), &rel, "a", "v", 4_000_000_000, crate::backend::LockMode::Compliance).is_err(), "no grant");
    r.svc.grant(&rel, cog("reader"), Access::Read, "op").unwrap();
    assert!(r.svc.extend_retention(&local("reader"), &rel, "a", "v", 4_000_000_000, crate::backend::LockMode::Compliance).is_err(), "read cannot hold");
}
