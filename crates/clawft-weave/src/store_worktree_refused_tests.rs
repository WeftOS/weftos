//! WO-069 lane L1 (gate block 4, finding 2): a claim refused because another merger holds it makes
//! the registry entry agree with the handoff record. The holder's daemon died after its conditional
//! PUT of the record and before its own registry refresh; the entry kept saying `handoff`, no merger.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use clawft_kernel::exposure::ExposureRegistry;
use serde_json::json;
use weftos_store::backend::{BackendError, ObjectAttrs, ObjectBackend as S1, ObjectMeta};
use weftos_store::share::{Actor, State, claim_merge, read_handoff};
use weftos_store::{FsBackend, StoreService};


use super::*;
use crate::store_rt::{ActivitySink, ChainAudit, RegistryGate};

const PROJ: &str = "01JBXK2M3N4P5Q6R7S8T9V0W1X";
const SLUG: &str = "feat-rooms";

struct W {
    _d: tempfile::TempDir,
    svc: StoreService,
    _src: tempfile::TempDir,
    wt: String,
    pr: String,
}

fn call(svc: &StoreService, verb: &str, p: Value, now: u64) -> Result<Value, String> {
    run(svc, "local", &format!("store.worktree.{verb}"), p, now)
}

/// A handed-off worktree published by alice@m1.
fn setup() -> W {
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
    let b = call(&svc, "buckets", json!({"project": PROJ, "slug": SLUG}), 1).unwrap();
    let (wt, pr) = (b["worktree"].as_str().unwrap().to_owned(), b["project"].as_str().unwrap().to_owned());
    call(&svc, "create", json!({"project": PROJ, "slug": SLUG, "branch": "feat/rooms", "base": "abc"}), 10).unwrap();
    let src = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(src.path().join(".git")).unwrap();
    std::fs::create_dir_all(src.path().join("docs")).unwrap();
    std::fs::write(src.path().join("docs/plan.md"), "# Rooms plan\n").unwrap();
    std::fs::write(src.path().join("paths.z"), b"docs/plan.md\0").unwrap();
    call(
        &svc,
        "publish",
        json!({"project": PROJ, "slug": SLUG, "root": src.path().to_string_lossy(), "branch": "feat/rooms", "base": "abc",
               "head": "def", "paths_z": src.path().join("paths.z").to_string_lossy(), "agent": "alice", "host": "m1",
               "handoff": "ready"}),
        20,
    )
    .unwrap();
    W { _d: d, svc, _src: src, wt, pr }
}

impl W {
    fn objects(&self) -> Arc<dyn S1> {
        self.svc.operator_objects()
    }
    /// Merger A's claim reaches the record (its conditional PUT) and the daemon dies before the
    /// registry refresh: the library claim, with no `refresh`.
    fn a_claims_then_dies(&self, now: u64) -> u64 {
        let a = Actor { agent: "merger-a".into(), host: "m2".into() };
        claim_merge(&Op(self.objects()), &self.wt, &a, now).unwrap().epoch
    }
    fn b_claims(&self, now: u64) -> Result<Value, String> {
        call(&self.svc, "claim", json!({"project": PROJ, "slug": SLUG, "agent": "merger-b", "host": "m3"}), now)
    }
    fn entry(&self) -> Value {
        serde_json::from_slice(&self.objects().get(&self.pr, &format!("worktrees/{SLUG}.json")).unwrap()).unwrap()
    }
}

#[test]
fn a_refused_claim_refreshes_a_stale_registry_entry_to_the_real_holder() {
    let w = setup();
    assert_eq!(w.entry()["state"], "handoff");
    let epoch = w.a_claims_then_dies(100);
    // the block 4 situation: the record says claimed, the entry still says handoff with no merger
    assert_eq!(w.entry()["state"], "handoff");
    assert!(w.entry()["merger"].is_null());

    let err = w.b_claims(200).unwrap_err();
    assert_eq!(err, "worktree is claimed by merger-a@m2", "the refusal is unchanged");

    let ent = w.entry();
    assert_eq!(ent["state"], "claimed");
    assert_eq!(ent["merger"]["agent"], "merger-a");
    assert_eq!(ent["merger"]["host"], "m2");
    let rec = read_handoff(&Op(w.objects()), &w.wt).unwrap().unwrap();
    assert_eq!((rec.state, rec.epoch), (State::Claimed, epoch), "the record is untouched by the refusal");
    // and what browse shows
    let br = call(&w.svc, "browse", json!({"project": PROJ}), 300).unwrap();
    assert!(br.to_string().contains("merger-a@m2"), "{br}");
}

/// Counts and optionally fails writes to registry entries; everything else passes through.
struct Spy {
    inner: Arc<dyn S1>,
    entry_puts: AtomicUsize,
    fail_entry_puts: AtomicBool,
}

impl Spy {
    fn is_entry(k: &str) -> bool {
        k == format!("worktrees/{SLUG}.json")
    }
    fn gate(&self, k: &str) -> Result<(), BackendError> {
        if Self::is_entry(k) {
            self.entry_puts.fetch_add(1, Ordering::SeqCst);
            if self.fail_entry_puts.load(Ordering::SeqCst) {
                return Err(BackendError::Unavailable("injected registry write failure".into()));
            }
        }
        Ok(())
    }
}

impl S1 for Spy {
    fn put(&self, b: &str, k: &str, d: &[u8], n: bool) -> Result<ObjectMeta, BackendError> {
        self.gate(k)?;
        self.inner.put(b, k, d, n)
    }
    fn get(&self, b: &str, k: &str) -> Result<Vec<u8>, BackendError> {
        self.inner.get(b, k)
    }
    fn head(&self, b: &str, k: &str) -> Result<ObjectMeta, BackendError> {
        self.inner.head(b, k)
    }
    fn delete_if_match(&self, b: &str, k: &str, e: &str) -> Result<(), BackendError> {
        self.inner.delete_if_match(b, k, e)
    }
    fn delete(&self, b: &str, k: &str) -> Result<(), BackendError> {
        self.inner.delete(b, k)
    }
    fn put_if_match(&self, b: &str, k: &str, d: &[u8], e: &str) -> Result<ObjectMeta, BackendError> {
        self.gate(k)?;
        self.inner.put_if_match(b, k, d, e)
    }
    fn put_attrs(&self, b: &str, k: &str, d: &[u8], n: bool, a: ObjectAttrs) -> Result<ObjectMeta, BackendError> {
        self.gate(k)?;
        self.inner.put_attrs(b, k, d, n, a)
    }
    fn put_if_match_attrs(&self, b: &str, k: &str, d: &[u8], e: &str, a: ObjectAttrs) -> Result<ObjectMeta, BackendError> {
        self.gate(k)?;
        self.inner.put_if_match_attrs(b, k, d, e, a)
    }
    fn attrs(&self, b: &str, k: &str) -> Result<ObjectAttrs, BackendError> {
        self.inner.attrs(b, k)
    }
    fn copy(&self, sb: &str, sk: &str, db: &str, dk: &str) -> Result<ObjectMeta, BackendError> {
        self.inner.copy(sb, sk, db, dk)
    }
    fn list(&self, b: &str, p: &str) -> Result<Vec<ObjectMeta>, BackendError> {
        self.inner.list(b, p)
    }
    fn list_stamped(&self, b: &str, p: &str) -> Result<Vec<(ObjectMeta, String)>, BackendError> {
        self.inner.list_stamped(b, p)
    }
}

fn spy(w: &W) -> (Arc<Spy>, Op) {
    let s = Arc::new(Spy { inner: w.objects(), entry_puts: AtomicUsize::new(0), fail_entry_puts: AtomicBool::new(false) });
    (s.clone(), Op(s))
}

#[test]
fn a_second_refused_claim_with_a_matching_entry_writes_nothing() {
    let w = setup();
    w.a_claims_then_dies(100);
    w.b_claims(200).unwrap_err(); // the first refusal repairs the entry
    let (s, op) = spy(&w);
    let sign = SignCfg::default();
    for _ in 0..3 {
        refresh_refused_claim(&op, &w.pr, SLUG, &w.wt, &sign, "merger-a@m2");
    }
    assert_eq!(s.entry_puts.load(Ordering::SeqCst), 0, "a matching entry is not rewritten");
    // and through the daemon: still refused, still no change
    let before = w.entry();
    assert_eq!(w.b_claims(300).unwrap_err(), "worktree is claimed by merger-a@m2");
    assert_eq!(w.entry(), before);
}

#[test]
fn a_mismatched_entry_is_written_once_through_the_ordinary_refresh() {
    let w = setup();
    w.a_claims_then_dies(100);
    let (s, op) = spy(&w);
    let sign = SignCfg::default();
    refresh_refused_claim(&op, &w.pr, SLUG, &w.wt, &sign, "merger-a@m2");
    assert_eq!(s.entry_puts.load(Ordering::SeqCst), 1);
    refresh_refused_claim(&op, &w.pr, SLUG, &w.wt, &sign, "merger-a@m2");
    assert_eq!(s.entry_puts.load(Ordering::SeqCst), 1, "the second call finds it matching");
}

#[test]
fn a_fence_sentence_is_not_a_holder_and_refreshes_nothing() {
    let w = setup();
    w.a_claims_then_dies(100);
    let (s, op) = spy(&w);
    refresh_refused_claim(&op, &w.pr, SLUG, &w.wt, &SignCfg::default(), "claim refused: stale epoch 1");
    assert_eq!(s.entry_puts.load(Ordering::SeqCst), 0);
    assert_eq!(w.entry()["state"], "handoff");
}

#[test]
fn a_failing_refresh_does_not_change_the_refusal() {
    let w = setup();
    w.a_claims_then_dies(100);
    let (s, op) = spy(&w);
    s.fail_entry_puts.store(true, Ordering::SeqCst);
    // the helper returns () and must neither panic nor write
    refresh_refused_claim(&op, &w.pr, SLUG, &w.wt, &SignCfg::default(), "merger-a@m2");
    assert_eq!(s.entry_puts.load(Ordering::SeqCst), 1, "it tried");
    assert_eq!(w.entry()["state"], "handoff", "and the failed write changed nothing");
    // the caller still gets the one refusal
    assert_eq!(w.b_claims(200).unwrap_err(), "worktree is claimed by merger-a@m2");
}
