//! A follower daemon (a second machine holding only a read-only key) asked to write a store it never
//! registered: the refusal says why (WO-069 lane J3). The "other daemon" is whoever created the
//! buckets; the follower sees them through a wrapper whose every write is `Forbidden` and counted.

use std::sync::atomic::{AtomicU64, Ordering};

use std::sync::Arc;

use serde_json::Value;

use crate::access::{Access, Principal};
use crate::audit::{MemSink, Outcome};
use crate::backend::{AccessKey, AdminBackend, BackendError, BucketTags, ObjectBackend, ObjectMeta};
use crate::service::{Caller, Config, Parties, StoreService};
use crate::{StoreError, StoreName};
use crate::fs::FsBackend;
use crate::mem::MemBackend;
use crate::{ChainSink, ExposureGate};

const P: &str = "01JBXK2M3N4P5Q6R7S8T9V0W1X";

struct Open;
impl ExposureGate for Open {
    fn register(&self, _: &str, _: &str) {}
    fn check(&self, _: &str) -> Result<(), String> {
        Ok(())
    }
}
struct NoChain;
impl ChainSink for NoChain {
    fn record(&self, _: &str, _: Value) {}
}

/// What the backend sees of a read-only key: reads and bucket probes pass through, every write
/// is counted and refused (HTTP 403 on S3). `probe_error` makes the bucket probe itself fail.
struct ReadOnly<B> {
    inner: Arc<B>,
    writes: AtomicU64,
    probes: AtomicU64,
    probe_error: Option<fn() -> BackendError>,
}
impl<B> ReadOnly<B> {
    fn new(inner: Arc<B>) -> Self {
        Self { inner, writes: AtomicU64::new(0), probes: AtomicU64::new(0), probe_error: None }
    }
    fn deny<T>(&self) -> Result<T, BackendError> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        Err(BackendError::Forbidden("read-only key".into()))
    }
}
impl<B: ObjectBackend + AdminBackend> ObjectBackend for ReadOnly<B> {
    fn put(&self, _: &str, _: &str, _: &[u8], _: bool) -> Result<ObjectMeta, BackendError> {
        self.deny()
    }
    fn put_if_match(&self, _: &str, _: &str, _: &[u8], _: &str) -> Result<ObjectMeta, BackendError> {
        self.deny()
    }
    fn put_locked(&self, _: &str, _: &str, _: &[u8], _: &crate::backend::PutOpts) -> Result<crate::backend::LockedPut, BackendError> {
        self.deny()
    }
    fn get(&self, b: &str, k: &str) -> Result<Vec<u8>, BackendError> {
        self.inner.get(b, k)
    }
    fn head(&self, b: &str, k: &str) -> Result<ObjectMeta, BackendError> {
        self.inner.head(b, k)
    }
    fn delete(&self, _: &str, _: &str) -> Result<(), BackendError> {
        self.deny()
    }
    fn copy(&self, _: &str, _: &str, _: &str, _: &str) -> Result<ObjectMeta, BackendError> {
        self.deny()
    }
    fn list(&self, b: &str, p: &str) -> Result<Vec<ObjectMeta>, BackendError> {
        self.inner.list(b, p)
    }
}
impl<B: ObjectBackend + AdminBackend> AdminBackend for ReadOnly<B> {
    fn create_bucket(&self, _: &str, _: &[(String, String)]) -> Result<(), BackendError> {
        self.deny()
    }
    fn delete_bucket(&self, _: &str) -> Result<(), BackendError> {
        self.deny()
    }
    fn bucket_exists(&self, b: &str) -> Result<bool, BackendError> {
        self.probes.fetch_add(1, Ordering::SeqCst);
        match self.probe_error {
            Some(f) => Err(f()),
            None => self.inner.bucket_exists(b),
        }
    }
    fn buckets(&self) -> Result<Vec<BucketTags>, BackendError> {
        self.inner.buckets()
    }
    fn create_access_key(&self, _: &str) -> Result<AccessKey, BackendError> {
        self.deny()
    }
    fn set_key_enabled(&self, _: &str, _: bool) -> Result<(), BackendError> {
        self.deny()
    }
    fn put_policy(&self, _: &str, _: &Value) -> Result<(), BackendError> {
        self.deny()
    }
    fn delete_access_key(&self, _: &str) -> Result<(), BackendError> {
        self.deny()
    }
}

struct Follower<B> {
    svc: StoreService,
    ro: Arc<ReadOnly<B>>,
    sink: Arc<MemSink>,
}

/// The owner daemon made the project store and a worktree store (and put an object in the first);
/// the follower service has registered nothing.
fn follower<B: ObjectBackend + AdminBackend + 'static>(be: Arc<B>, probe_error: Option<fn() -> BackendError>) -> Follower<B> {
    for n in [project(), worktree()] {
        be.create_bucket(&n.bucket(), &[]).unwrap();
    }
    be.put(&project().bucket(), "README.md", b"owned elsewhere", false).unwrap();
    let mut ro = ReadOnly::new(be);
    ro.probe_error = probe_error;
    let ro = Arc::new(ro);
    let sink = Arc::new(MemSink::default());
    let svc = StoreService::open(Config { node_id: "xpc-1".into(), state_path: None, clock: || 1_000 }, ro.clone(), ro.clone(), Arc::new(Open), sink.clone(), Arc::new(NoChain)).unwrap();
    Follower { svc, ro, sink }
}

fn project() -> StoreName {
    StoreName::project("local", P).unwrap()
}
fn worktree() -> StoreName {
    StoreName::worktree("local", P, "feat--gate").unwrap()
}
fn who() -> Caller {
    Caller::local(Principal::parse("member:ann").unwrap())
}
fn refusal(e: StoreError) -> String {
    match e {
        StoreError::Refused(s) => s,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn writes_are_refused_as_not_a_writer<B: ObjectBackend + AdminBackend + 'static>(be: Arc<B>) {
    let f = follower(be, None);
    for n in [project(), worktree()] {
        let put = refusal(f.svc.put(&who(), &n, "k", b"x", false).unwrap_err());
        assert!(put.contains("not a writer"), "{put}");
        assert!(put.contains(&n.to_string()), "names the store: {put}");
        assert!(put.contains("grant") && put.contains("write key"), "says what is needed: {put}");
        assert!(!put.contains("unknown store"), "{put}");
        let del = refusal(f.svc.delete(&who(), &n, "k").unwrap_err());
        assert!(del.contains("not a writer"), "{del}");
        let ifm = refusal(f.svc.put_if_match(&who(), &n, "k", b"x", "e").unwrap_err());
        assert!(ifm.contains("not a writer"), "{ifm}");
        let locked = refusal(f.svc.put_locked(&who(), &n, "k", b"x", &Default::default()).unwrap_err());
        assert!(locked.contains("not a writer"), "{locked}");
        let stream = refusal(f.svc.put_stream(&who(), &n, "k", &mut &b"x"[..], &Default::default()).unwrap_err());
        assert!(stream.contains("not a writer"), "{stream}");
    }
    assert_eq!(f.ro.writes.load(Ordering::SeqCst), 0, "no write call reached the backend");
    assert!(f.svc.store_names().is_empty() && !f.svc.has_store(&project()), "nothing was registered or created");
    // The audit line has the same shape as before (a refusal on PutObject/DeleteObject) with the accurate reason.
    let ev = f.sink.events();
    assert_eq!(ev.len(), 10);
    for e in &ev {
        assert!(matches!(&e.outcome, Outcome::Refused(w) if w.contains("not a writer")), "{e:?}");
        assert_eq!(e.principal, "member:ann");
    }
    assert_eq!((ev[0].op.as_str(), ev[1].op.as_str()), ("PutObject", "DeleteObject"));
}

#[test]
fn a_follower_put_or_delete_is_refused_as_not_a_writer_and_nothing_is_written_memory() {
    writes_are_refused_as_not_a_writer(Arc::new(MemBackend::new()));
}

#[test]
fn a_follower_put_or_delete_is_refused_as_not_a_writer_and_nothing_is_written_directory() {
    let d = tempfile::tempdir().unwrap();
    writes_are_refused_as_not_a_writer(Arc::new(FsBackend::open(d.path()).unwrap()));
}

#[test]
fn a_store_with_no_bucket_is_still_an_unknown_store() {
    let f = follower(Arc::new(MemBackend::new()), None);
    let nobody = StoreName::project("local", "01JBXK2M3N4P5Q6R7S8T9V0W2Y").unwrap();
    for e in [f.svc.put(&who(), &nobody, "k", b"x", false).unwrap_err(), f.svc.delete(&who(), &nobody, "k").unwrap_err()] {
        assert_eq!(refusal(e), format!("unknown store {nobody}"));
    }
    assert_eq!(f.ro.writes.load(Ordering::SeqCst), 0);
    let ev = f.sink.events();
    assert!(ev.iter().all(|e| matches!(&e.outcome, Outcome::Refused(w) if *w == format!("unknown store {nobody}"))), "{ev:?}");
}

#[test]
fn a_bucket_the_key_cannot_see_is_reported_as_unknown_or_no_access_not_guessed() {
    let f = follower(Arc::new(MemBackend::new()), Some(|| BackendError::Forbidden("no".into())));
    let why = refusal(f.svc.put(&who(), &project(), "k", b"x", false).unwrap_err());
    assert_eq!(why, format!("unknown store {} or no access", project()));
    assert_eq!(f.ro.writes.load(Ordering::SeqCst), 0);
    // A backend that cannot be reached does not turn into a verdict either.
    let f = follower(Arc::new(MemBackend::new()), Some(|| BackendError::Unavailable("down".into())));
    let why = refusal(f.svc.put(&who(), &project(), "k", b"x", false).unwrap_err());
    assert!(why.starts_with(&format!("unknown store {}", project())) && why.contains("could not check"), "{why}");
}

#[test]
fn a_mesh_caller_learns_nothing_about_buckets_and_the_probe_is_not_made() {
    let f = follower(Arc::new(MemBackend::new()), None);
    let remote = Caller::mesh(Principal::parse("member:ann").unwrap());
    let why = refusal(f.svc.put(&remote, &project(), "k", b"x", false).unwrap_err());
    assert_eq!(why, format!("unknown store {}", project()));
    assert_eq!(f.ro.probes.load(Ordering::SeqCst), 0);
}

#[test]
fn governed_reads_stay_refused_with_an_accurate_message_and_no_access_is_widened() {
    let f = follower(Arc::new(MemBackend::new()), None);
    let get = refusal(f.svc.get(&who(), &project(), "README.md").unwrap_err());
    assert!(get.contains("not registered on this daemon") && !get.contains("not a writer") && !get.contains("unknown store"), "{get}");
    assert!(f.svc.list(&who(), &project(), "").is_err() && f.svc.head(&who(), &project(), "README.md").is_err());
    let nobody = StoreName::project("local", "01JBXK2M3N4P5Q6R7S8T9V0W2Y").unwrap();
    assert_eq!(refusal(f.svc.get(&who(), &nobody, "k").unwrap_err()), format!("unknown store {nobody}"));
}

/// An admin backend that only counts bucket probes; everything is delegated.
struct Spy(Arc<MemBackend>, AtomicU64);
impl AdminBackend for Spy {
    fn create_bucket(&self, b: &str, t: &[(String, String)]) -> Result<(), BackendError> {
        self.0.create_bucket(b, t)
    }
    fn delete_bucket(&self, b: &str) -> Result<(), BackendError> {
        self.0.delete_bucket(b)
    }
    fn bucket_exists(&self, b: &str) -> Result<bool, BackendError> {
        self.1.fetch_add(1, Ordering::SeqCst);
        self.0.bucket_exists(b)
    }
    fn buckets(&self) -> Result<Vec<BucketTags>, BackendError> {
        self.0.buckets()
    }
    fn create_access_key(&self, p: &str) -> Result<AccessKey, BackendError> {
        self.0.create_access_key(p)
    }
    fn set_key_enabled(&self, k: &str, e: bool) -> Result<(), BackendError> {
        self.0.set_key_enabled(k, e)
    }
    fn put_policy(&self, k: &str, p: &Value) -> Result<(), BackendError> {
        self.0.put_policy(k, p)
    }
    fn delete_access_key(&self, k: &str) -> Result<(), BackendError> {
        self.0.delete_access_key(k)
    }
}

#[test]
fn a_registered_store_is_unchanged_and_no_request_that_passes_the_gate_probes() {
    let be = Arc::new(MemBackend::new());
    let spy = Arc::new(Spy(be.clone(), AtomicU64::new(0)));
    let svc = StoreService::open(Config { node_id: "pg-1".into(), state_path: None, clock: || 1_000 }, spy.clone(), be, Arc::new(Open), Arc::new(MemSink::default()), Arc::new(NoChain)).unwrap();
    let n = project();
    svc.create_store(&n, "op", Parties::default()).unwrap();
    let before = spy.1.load(Ordering::SeqCst);
    let w = Principal::parse("cog:w").unwrap();
    svc.grant(&n, w.clone(), Access::Write, "op").unwrap();
    let wc = Caller::local(w);
    svc.put(&wc, &n, "k", b"1", false).unwrap();
    assert_eq!(svc.get(&wc, &n, "k").unwrap(), b"1");
    svc.delete(&wc, &n, "k").unwrap();
    // Registered, no grant: the old message, from the grant check.
    let e = refusal(svc.put(&who(), &n, "k", b"2", false).unwrap_err());
    assert!(e.contains("holds no enabled grant"), "{e}");
    assert_eq!(spy.1.load(Ordering::SeqCst), before, "a registered store is never probed, passing or refused");
}
