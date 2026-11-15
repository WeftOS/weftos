//! WO-069 lane Y, A4/A5: a publish announces the manifest it is about to write in the handoff record
//! (conditional write) before the manifest PUT, and a fenced merge or land refuses while one is
//! pending; a failed manifest PUT withdraws the announcement. Design note
//! `reports/wo069-fence-atomicity-design-20261115.md` §2. Deterministic, single-threaded: the hook
//! backend runs an action just before a chosen call, or fails one.
use std::cell::{Cell, RefCell};
use weftos_store::overlay::{ObjectBackend, ObjectMeta, Result as R, S2Error};
use weftos_store::share::*;
use weftos_store::testing::MemBackend;

const WT: &str = "w-01abc-cccccccccccc";
const NOW: u64 = 1_000_000_000;
/// A claim stamped this far ahead is not live, so the writer may republish under it (ADR-126 §4).
const AHEAD: u64 = CLAIM_SKEW_MS + 10;

type Act<'a> = RefCell<Option<Box<dyn FnOnce() + 'a>>>;

#[derive(Default)]
struct Hook<'a> {
    before_manifest_put: Act<'a>,
    before_tree_put: Act<'a>,
    before_handoff_cas: Act<'a>,
    fail_manifest_put: Cell<bool>,
    fail_delete: Cell<bool>,
    /// Fail the handoff `put_cas` with this 0-based index (counting every one this hook sees).
    fail_handoff_cas_at: Option<usize>,
    handoff_cas: Cell<usize>,
}
struct Hooked<'a> {
    inner: &'a MemBackend,
    h: Hook<'a>,
}
fn fire(a: &Act<'_>) {
    let f = a.borrow_mut().take();
    if let Some(f) = f {
        f();
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
        if k.starts_with(TREE_PREFIX) || k.starts_with(OBJ_PREFIX) {
            fire(&self.h.before_tree_put);
        }
        self.inner.put(b, k, d)
    }
    fn put_cas(&self, b: &str, k: &str, d: &[u8], e: Option<&str>) -> R<bool> {
        // The manifest write is conditional since lane D3 (a `put_cas` on the version read).
        if k == MANIFEST_KEY {
            fire(&self.h.before_manifest_put);
            if self.h.fail_manifest_put.get() {
                return Err(S2Error::Backend("injected: manifest PUT failed".into()));
            }
        }
        if k == HANDOFF_KEY {
            fire(&self.h.before_handoff_cas);
            let n = self.h.handoff_cas.get();
            self.h.handoff_cas.set(n + 1);
            if self.h.fail_handoff_cas_at == Some(n) {
                return Err(S2Error::Backend("injected: handoff write failed".into()));
            }
        }
        self.inner.put_cas(b, k, d, e)
    }
    fn delete(&self, b: &str, k: &str) -> R<()> {
        if self.h.fail_delete.get() {
            return Err(S2Error::Backend("injected: the publish died in its sweep".into()));
        }
        self.inner.delete(b, k)
    }
    fn list(&self, b: &str, p: &str) -> R<Vec<ObjectMeta>> {
        self.inner.list(b, p)
    }
}
fn hooked<'a>(inner: &'a MemBackend, h: Hook<'a>) -> Hooked<'a> {
    Hooked { inner, h }
}

fn who(n: &str) -> Actor {
    Actor { agent: n.into(), host: "h".into() }
}

fn publish_on<B: ObjectBackend>(b: &B, head: &str, files: &[(&str, &str)], now: u64) -> R<PublishReport> {
    let d = tempfile::tempdir().unwrap();
    for (p, c) in files {
        std::fs::write(d.path().join(p), c).unwrap();
    }
    let ps: Vec<String> = files.iter().map(|(p, _)| p.to_string()).collect();
    let src = TreeSource {
        root: d.path(),
        paths: &ps,
        uncommitted: &[],
        changes: &[],
        branch: "feat/y",
        base_commit: "base",
        head_commit: head,
        bundle: None,
        writer: who("writer"),
    };
    publish_tree(b, WT, &src, &Limits::default(), now)
}
fn publish<B: ObjectBackend>(b: &B, head: &str, now: u64) -> R<PublishReport> {
    publish_on(b, head, &[("a.md", head)], now)
}

/// Published at h1, handed off, and claimed by bob with a stamp that is not live.
fn claimed() -> (MemBackend, Handoff) {
    let b = MemBackend::new();
    publish(&b, "h1", NOW).unwrap();
    mark_handoff(&b, WT, &who("writer"), "ready", NOW).unwrap();
    let c = claim_merge(&b, WT, &who("bob"), NOW + AHEAD).unwrap();
    (b, c)
}
fn record(b: &MemBackend) -> Handoff {
    read_handoff(b, WT).unwrap().unwrap()
}
fn head(b: &MemBackend) -> String {
    read_manifest(b, WT).unwrap().unwrap().head_commit
}
fn in_flight(e: &S2Error) -> bool {
    e.to_string().contains("in flight")
}

// ---- the window between a fence's manifest read and its write -----------------------------------------------------

#[test]
fn a_publish_inside_the_merge_window_makes_the_merge_refuse() {
    let (b, c) = claimed();
    let racy = hooked(&b, Hook { before_handoff_cas: RefCell::new(Some(Box::new(|| { publish(&b, "h2", NOW + 1).unwrap(); }))), ..Hook::default() });
    let e = mark_merged_at(&racy, WT, &who("bob"), Some(c.epoch), "m", NOW + 2).unwrap_err();
    assert!(e.to_string().contains("from h1 to h2"), "the CAS lost to the announcement, the re-read saw h2: {e}");
    assert_eq!((record(&b).state, head(&b)), (State::Claimed, "h2".into()));
}

#[test]
fn every_fenced_path_refuses_while_an_announced_manifest_is_not_yet_written() {
    for path in ["mark_merged_at", "mark_merged", "mark_landed_at", "fence_claim"] {
        let (b, c) = claimed();
        let seen = RefCell::new(None);
        let p = hooked(&b, Hook {
            before_manifest_put: RefCell::new(Some(Box::new(|| {
                let r = match path {
                    "mark_merged_at" => mark_merged_at(&b, WT, &who("bob"), Some(c.epoch), "m", NOW + 2),
                    "mark_merged" => mark_merged(&b, WT, &who("bob"), "m", NOW + 2),
                    "mark_landed_at" => mark_landed_at(&b, WT, &who("bob"), Some(c.epoch), "l", NOW + 2).map(|_| ()),
                    _ => fence_claim(&b, WT, c.epoch, NOW + 2),
                };
                *seen.borrow_mut() = Some(r);
            }))),
            ..Hook::default()
        });
        // the same tree republished: only the announcement can stop the merge (head and tree still match)
        publish_on(&p, "h1", &[("a.md", "h1")], NOW + 1).unwrap();
        drop(p);
        let e = seen.into_inner().unwrap().unwrap_err();
        assert!(in_flight(&e), "{path}: {e}");
        assert_eq!(record(&b).state, State::Claimed, "{path}");
        // once the manifest is the announced one, the same merge goes through
        mark_merged_at(&b, WT, &who("bob"), Some(c.epoch), "m", NOW + 3).unwrap();
    }
}

// ---- publish racing publish -----------------------------------------------------------------------------------------

#[test]
fn a_publish_that_loses_its_manifest_race_writes_nothing_and_the_winner_merges_at_once() {
    // Lane D3 changed this: the manifest write is conditional on the version the publish read, so a
    // publish whose announcement was replaced can no longer land late. Before, P's late PUT landed
    // and the hold kept merges waiting for the TTL.
    let (b, _) = claimed();
    // P announces h2; before P's manifest write, Q publishes h3 entirely and bob claims h3
    let bob = RefCell::new(None);
    let p = hooked(&b, Hook {
        before_manifest_put: RefCell::new(Some(Box::new(|| {
            publish(&b, "h3", NOW + 2).unwrap();
            let q = record(&b).publishing.unwrap();
            assert!(q.done && q.held_ms.is_none(), "Q's manifest landed, so Q settled and dropped P's hold: {q:?}");
            *bob.borrow_mut() = Some(claim_merge(&b, WT, &who("bob"), NOW + 3).unwrap());
        }))),
        ..Hook::default()
    });
    let e = publish(&p, "h2", NOW + 1).unwrap_err();
    assert!(e.to_string().contains("manifest changed while this publish ran"), "{e}");
    drop(p);
    assert_eq!(head(&b), "h3", "P wrote nothing");
    let c = bob.into_inner().unwrap();
    assert_eq!(c.claimed_head.as_deref(), Some("h3"));
    mark_merged_at(&b, WT, &who("bob"), Some(c.epoch), "m", NOW + 5).unwrap();
}

// ---- the manifest PUT fails (A5) ------------------------------------------------------------------------------------

#[test]
fn a_failed_manifest_put_withdraws_the_announcement() {
    let (b, c) = claimed();
    let before = record(&b).publishing;
    let p = hooked(&b, Hook { fail_manifest_put: Cell::new(true), ..Hook::default() });
    let e = publish(&p, "h2", NOW + 1).unwrap_err();
    assert!(e.to_string().contains("manifest PUT failed") && !e.to_string().contains("withdrawn"), "{e}");
    assert_eq!(record(&b).publishing, before, "the previous announcement is back");
    assert_eq!(head(&b), "h1");
    mark_merged_at(&b, WT, &who("bob"), Some(c.epoch), "m", NOW + 2).unwrap();
}

#[test]
fn a_failed_first_publish_leaves_an_open_record_with_no_announcement() {
    let b = MemBackend::new();
    let p = hooked(&b, Hook { fail_manifest_put: Cell::new(true), ..Hook::default() });
    assert!(publish(&p, "h1", NOW).is_err());
    let h = record(&b);
    assert_eq!((h.state, h.publishing), (State::Open, None));
    assert!(read_manifest(&b, WT).unwrap().is_none());
    publish(&b, "h1", NOW + 1).unwrap(); // the rerun is not blocked
}

#[test]
fn when_the_withdrawal_fails_too_merges_wait_until_the_next_publish_lands() {
    let (b, c) = claimed();
    // handoff put_cas #0 is the announcement, #1 the withdrawal
    let p = hooked(&b, Hook { fail_manifest_put: Cell::new(true), fail_handoff_cas_at: Some(1), ..Hook::default() });
    let e = publish(&p, "h2", NOW + 1).unwrap_err();
    assert!(e.to_string().contains("could not be withdrawn"), "the error says merges wait: {e}");
    assert!(in_flight(&mark_merged_at(&b, WT, &who("bob"), Some(c.epoch), "m", NOW + 2).unwrap_err()));
    // The writer's next publish is not blocked. It carries the stuck stamp as a hold while it runs;
    // once its own conditional manifest write lands the stuck publish can never land (its write is
    // on a version that is gone), so the hold is dropped (lane D3; before, merges waited the TTL).
    publish(&b, "h1", NOW + 3).unwrap();
    let i = record(&b).publishing.unwrap();
    assert!(i.done && i.held_ms.is_none(), "{i:?}");
    mark_merged_at(&b, WT, &who("bob"), Some(c.epoch), "m", NOW + 4).unwrap();
}

// ---- other cut points and interleavings -----------------------------------------------------------------------------

#[test]
fn a_crash_after_the_manifest_leaves_nothing_pending() {
    for cut in ["post-check", "sweep"] {
        let b = MemBackend::new();
        publish_on(&b, "h1", &[("a.md", "a"), ("b.md", "b")], NOW).unwrap();
        mark_handoff(&b, WT, &who("writer"), "ready", NOW).unwrap();
        // h2 drops b.md, then dies right after its manifest write: in the post-check's record write
        // (handoff put_cas #1, #0 is the announcement), or in the sweep that deletes b.md's object
        // (a zero grace, so the sweep runs at once)
        let p = hooked(&b, Hook { fail_handoff_cas_at: (cut == "post-check").then_some(1), fail_delete: Cell::new(cut == "sweep"), ..Hook::default() });
        let src_dir = tempfile::tempdir().unwrap();
        std::fs::write(src_dir.path().join("a.md"), "a").unwrap();
        let ps = vec!["a.md".to_string()];
        let src = TreeSource { root: src_dir.path(), paths: &ps, uncommitted: &[], changes: &[], branch: "feat/y", base_commit: "base", head_commit: "h2", bundle: None, writer: who("writer") };
        let opts = PublishOptions { grace_ms: 0, ..PublishOptions::default() };
        assert!(publish_tree_opts(&p, WT, &src, &Limits::default(), NOW + 1, None, &opts).is_err(), "{cut}");
        assert_eq!(head(&b), "h2", "{cut}");
        let c = claim_merge(&b, WT, &who("bob"), NOW + 2).unwrap();
        mark_merged_at(&b, WT, &who("bob"), Some(c.epoch), "m", NOW + 3).unwrap();
    }
}

#[test]
fn a_live_claim_taken_during_the_upload_refuses_the_publish_before_its_manifest() {
    let b = MemBackend::new();
    publish(&b, "h1", NOW).unwrap();
    mark_handoff(&b, WT, &who("writer"), "ready", NOW).unwrap();
    let m1 = ObjectBackend::get(&b, WT, MANIFEST_KEY).unwrap();
    let p = hooked(&b, Hook { before_tree_put: RefCell::new(Some(Box::new(|| { claim_merge(&b, WT, &who("bob"), NOW + 1).unwrap(); }))), ..Hook::default() });
    let e = publish(&p, "h2", NOW + 1).unwrap_err();
    assert!(matches!(e, S2Error::Claimed(_)), "{e}");
    assert_eq!(ObjectBackend::get(&b, WT, MANIFEST_KEY).unwrap(), m1, "the manifest was not written");
    let c = record(&b);
    mark_merged_at(&b, WT, &who("bob"), Some(c.epoch), "m", NOW + 2).unwrap();
}

#[test]
fn a_merge_during_the_upload_refuses_the_publish_before_its_manifest() {
    let (b, c) = claimed();
    let m1 = ObjectBackend::get(&b, WT, MANIFEST_KEY).unwrap();
    let p = hooked(&b, Hook { before_tree_put: RefCell::new(Some(Box::new(|| { mark_merged_at(&b, WT, &who("bob"), Some(c.epoch), "m", NOW + 1).unwrap(); }))), ..Hook::default() });
    let e = publish(&p, "h2", NOW + 1).unwrap_err();
    assert!(e.to_string().contains("became Merged"), "{e}");
    assert_eq!(ObjectBackend::get(&b, WT, MANIFEST_KEY).unwrap(), m1);
}

/// A pending announcement stamped at `ms` (the manifest is not its bytes).
fn plant(b: &MemBackend, ms: u64) {
    let mut h = record(b);
    h.publishing = Some(PublishIntent { manifest_sha256: "0".repeat(64), head: "h9".into(), tree: "t1:x".into(), ms, writer: who("writer"), held_ms: None, done: false });
    ObjectBackend::put(b, WT, HANDOFF_KEY, &serde_json::to_vec(&h).unwrap()).unwrap();
}

#[test]
fn a_stale_or_far_future_announcement_does_not_hold_a_merge_and_a_live_one_does() {
    let at = NOW + DAY_MS;
    for (ms, live) in [
        (at - CLAIM_TTL_MS, false),
        (at - CLAIM_TTL_MS + 1, true),
        (at + CLAIM_SKEW_MS, true),
        (at + CLAIM_SKEW_MS + 1, false),
    ] {
        let (b, c) = claimed();
        plant(&b, ms);
        let r = mark_merged_at(&b, WT, &who("bob"), Some(c.epoch), "m", at);
        assert_eq!(r.is_err(), live, "stamp {ms} vs now {at}: {r:?}");
    }
}

const DAY_MS: u64 = 24 * 3600 * 1000;

#[test]
fn the_announcement_is_kept_by_a_handoff_note_and_is_additive_on_the_wire() {
    let (b, _) = claimed();
    plant(&b, NOW);
    mark_handoff(&b, WT, &who("writer"), "again", NOW + AHEAD + CLAIM_TTL_MS).unwrap(); // bob's claim expired
    assert_eq!(record(&b).publishing.unwrap().ms, NOW, "a handoff note does not drop a pending announcement");
    // a record from before the field parses; an old reader parses a new record (unknown fields ignored)
    let mut v: serde_json::Value = serde_json::from_slice(&ObjectBackend::get(&b, WT, HANDOFF_KEY).unwrap().unwrap()).unwrap();
    #[derive(serde::Deserialize)]
    #[allow(dead_code)]
    struct OldHandoff {
        state: State,
        note: String,
        writer: Actor,
        merger: Option<Actor>,
        updated_ms: u64,
        #[serde(default)]
        epoch: u64,
    }
    serde_json::from_value::<OldHandoff>(v.clone()).unwrap();
    v.as_object_mut().unwrap().remove("publishing");
    assert_eq!(serde_json::from_value::<Handoff>(v).unwrap().publishing, None);
}
