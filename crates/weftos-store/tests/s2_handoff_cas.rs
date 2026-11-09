//! The handoff record is changed with compare-and-swap (S1 `put_if_match` through `put_cas`): a
//! competing write between a read and our write makes ours fail and re-decide, instead of silently
//! overwriting it (S5 gap: S2 read-modify-write without the conditional write).
use std::cell::RefCell;
use weftos_store::overlay::{ObjectBackend, ObjectMeta, Result as R, S2Error};
use weftos_store::share::*;
use weftos_store::share_fetch::*;
use weftos_store::testing::MemBackend;

const WT: &str = "w-01abc-aaaaaaaaaaaa";

fn who(n: &str) -> Actor {
    Actor { agent: n.into(), host: "h".into() }
}

/// Runs `hook` once, just before the first conditional write to the handoff record, i.e. after the
/// caller has read the record and decided, and before its write lands.
struct Hooked<'a> {
    inner: &'a MemBackend,
    hook: RefCell<Option<Box<dyn FnOnce() + 'a>>>,
    every_time: Option<Box<dyn Fn() + 'a>>,
}
impl<'a> Hooked<'a> {
    fn once(inner: &'a MemBackend, f: impl FnOnce() + 'a) -> Self {
        Self { inner, hook: RefCell::new(Some(Box::new(f))), every_time: None }
    }
    fn always(inner: &'a MemBackend, f: impl Fn() + 'a) -> Self {
        Self { inner, hook: RefCell::new(None), every_time: Some(Box::new(f)) }
    }
}
impl ObjectBackend for Hooked<'_> {
    fn head(&self, b: &str, k: &str) -> R<Option<ObjectMeta>> {
        self.inner.head(b, k)
    }
    fn get(&self, b: &str, k: &str) -> R<Option<Vec<u8>>> {
        self.inner.get(b, k)
    }
    fn put(&self, b: &str, k: &str, d: &[u8]) -> R<ObjectMeta> {
        self.inner.put(b, k, d)
    }
    fn put_cas(&self, b: &str, k: &str, d: &[u8], expect: Option<&str>) -> R<bool> {
        if k.ends_with("handoff.json") {
            if let Some(f) = self.hook.borrow_mut().take() {
                f();
            }
            if let Some(f) = &self.every_time {
                f();
            }
        }
        self.inner.put_cas(b, k, d, expect)
    }
    fn delete(&self, b: &str, k: &str) -> R<()> {
        self.inner.delete(b, k)
    }
    fn list(&self, b: &str, p: &str) -> R<Vec<ObjectMeta>> {
        self.inner.list(b, p)
    }
}

/// A published tree whose handoff is ready to claim.
fn ready() -> MemBackend {
    let b = MemBackend::new();
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("a.md"), "x").unwrap();
    let ps = vec!["a.md".to_string()];
    let src = TreeSource {
        root: d.path(),
        paths: &ps,
        uncommitted: &[],
        changes: &[],
        branch: "feat/x",
        base_commit: "b",
        head_commit: "h",
        bundle: None,
        writer: who("writer"),
    };
    publish_tree(&b, WT, &src, &Limits::default(), 1_000).unwrap();
    mark_handoff(&b, WT, &who("writer"), "ready", 1_001).unwrap();
    b
}

#[test]
fn two_claimers_who_read_the_same_record_cannot_both_win() {
    let b = ready();
    // A has read "Handoff" and is about to write Claimed; B's claim lands first.
    let racy = Hooked::once(&b, || {
        claim_merge(&b, WT, &who("bob"), 2_000).unwrap();
    });
    let e = claim_merge(&racy, WT, &who("alice"), 2_001).unwrap_err();
    assert_eq!(e, S2Error::Claimed("bob@h".into()), "alice lost and was told who won");
    let h = read_handoff(&b, WT).unwrap().unwrap();
    assert_eq!((h.state, h.merger.unwrap().agent.as_str(), h.epoch), (State::Claimed, "bob", 1), "bob's claim is intact");
}

#[test]
fn a_late_handoff_cannot_erase_a_claim_that_landed_after_it_read() {
    let b = ready();
    // the writer re-handoffs; meanwhile bob claims
    let racy = Hooked::once(&b, || {
        claim_merge(&b, WT, &who("bob"), 2_000).unwrap();
    });
    let e = mark_handoff(&racy, WT, &who("writer"), "again", 2_001).unwrap_err();
    assert_eq!(e, S2Error::Claimed("bob@h".into()));
    let h = read_handoff(&b, WT).unwrap().unwrap();
    assert_eq!(h.state, State::Claimed, "the claim was not overwritten by Handoff");
}

#[test]
fn a_merge_cannot_land_over_a_takeover_that_happened_after_its_checks() {
    let b = ready();
    claim_merge(&b, WT, &who("bob"), 2_000).unwrap();
    // bob's checks pass (his claim, epoch 1); then his claim lapses and carol takes over before his write
    let ttl = CLAIM_TTL_MS;
    let racy = Hooked::once(&b, || {
        claim_merge(&b, WT, &who("carol"), 2_000 + ttl + 1).unwrap();
    });
    let e = mark_merged_at(&racy, WT, &who("bob"), Some(1), "done", 2_100).unwrap_err();
    assert_eq!(e, S2Error::Claimed("carol@h".into()));
    let h = read_handoff(&b, WT).unwrap().unwrap();
    assert_eq!((h.state, h.epoch), (State::Claimed, 2), "carol's claim stands; bob's merge did not land");
}

#[test]
fn publishing_never_overwrites_a_record_created_between_its_check_and_its_write() {
    // first publish of a tree: the record does not exist yet; a claim-shaped record appears first
    let b = MemBackend::new();
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("a.md"), "x").unwrap();
    let ps = vec!["a.md".to_string()];
    let src = TreeSource {
        root: d.path(), paths: &ps, uncommitted: &[], changes: &[], branch: "b", base_commit: "b",
        head_commit: "h", bundle: None, writer: who("writer"),
    };
    let planted = Handoff { state: State::Handoff, note: "n".into(), writer: who("w2"), merger: None, updated_ms: 5, epoch: 3 };
    let racy = Hooked::once(&b, || {
        b.put(WT, ".wt/handoff.json", &serde_json::to_vec(&planted).unwrap()).unwrap();
    });
    publish_tree(&racy, WT, &src, &Limits::default(), 1_000).unwrap();
    assert_eq!(read_handoff(&b, WT).unwrap().unwrap(), planted, "the Open record did not replace it");
}

#[test]
fn endless_contention_gives_up_with_an_error_instead_of_looping_or_clobbering() {
    let b = ready();
    // every time we are about to write, someone else changes the record first
    let n = RefCell::new(0u64);
    let racy = Hooked::always(&b, || {
        let mut n = n.borrow_mut();
        *n += 1;
        let mut h = read_handoff(&b, WT).unwrap().unwrap();
        h.note = format!("churn {n}");
        b.put(WT, ".wt/handoff.json", &serde_json::to_vec(&h).unwrap()).unwrap();
    });
    let e = claim_merge(&racy, WT, &who("alice"), 2_000).unwrap_err();
    assert!(matches!(&e, S2Error::Backend(m) if m.contains("being changed")), "{e:?}");
    let h = read_handoff(&b, WT).unwrap().unwrap();
    assert_eq!(h.state, State::Handoff, "alice never overwrote the churning record");
    assert!(*n.borrow() <= 6, "bounded retries");
}

#[test]
fn the_uncontended_paths_still_work_and_a_claimer_can_reclaim_its_own() {
    let b = ready();
    let h = claim_merge(&b, WT, &who("alice"), 2_000).unwrap();
    assert_eq!((h.state, h.epoch), (State::Claimed, 1));
    let again = claim_merge(&b, WT, &who("alice"), 2_001).unwrap();
    assert_eq!(again.epoch, 2, "a re-claim by the same agent is a new epoch");
    assert!(claim_merge(&b, WT, &who("bob"), 2_002).is_err());
    mark_merged_at(&b, WT, &who("alice"), Some(2), "done", 2_003).unwrap();
    assert_eq!(read_handoff(&b, WT).unwrap().unwrap().state, State::Merged);
    let _ = summarize(&b, "x", WT);
}
