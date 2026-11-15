//! WO-069 lane Y (card 1eb8c7e8): the claim fence holds across every interleaving of a fenced
//! finalisation with a claim or takeover, and on what was claimed, not only on the head. Design:
//! `reports/wo069-fence-atomicity-design-20261115.md`. Interleavings are driven deterministically by a
//! backend hook that fires just before the first conditional write of the handoff record, that is
//! after the caller has read and decided and before its write lands. Single-threaded.
use std::cell::RefCell;
use weftos_store::overlay::{ObjectBackend, ObjectMeta, Result as R, S2Error};
use weftos_store::share::*;
use weftos_store::testing::MemBackend;

const WT: &str = "w-01abc-bbbbbbbbbbbb";
const NOW: u64 = 1_000_000_000;

fn who(n: &str) -> Actor {
    Actor { agent: n.into(), host: "h".into() }
}

/// Runs `hook` once, just before the first conditional write to the handoff record.
struct Hooked<'a> {
    inner: &'a MemBackend,
    hook: RefCell<Option<Box<dyn FnOnce() + 'a>>>,
}
impl<'a> Hooked<'a> {
    fn once(inner: &'a MemBackend, f: impl FnOnce() + 'a) -> Self {
        Self { inner, hook: RefCell::new(Some(Box::new(f))) }
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
        if k == HANDOFF_KEY
            && let Some(f) = self.hook.borrow_mut().take()
        {
            f();
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

/// Publish `files` at `head` (`uncommitted` names the files not in `head`).
fn publish(b: &MemBackend, head: &str, files: &[(&str, &str)], uncommitted: &[&str], now: u64) -> R<PublishReport> {
    let d = tempfile::tempdir().unwrap();
    for (p, c) in files {
        std::fs::write(d.path().join(p), c).unwrap();
    }
    let ps: Vec<String> = files.iter().map(|(p, _)| p.to_string()).collect();
    let unc: Vec<String> = uncommitted.iter().map(|p| p.to_string()).collect();
    let src = TreeSource {
        root: d.path(),
        paths: &ps,
        uncommitted: &unc,
        changes: &[],
        branch: "feat/y",
        base_commit: "base",
        head_commit: head,
        bundle: None,
        writer: who("writer"),
    };
    publish_tree(b, WT, &src, &Limits::default(), now)
}

/// Published at h1 (with an uncommitted `notes.md`) and handed off.
fn ready() -> MemBackend {
    let b = MemBackend::new();
    publish(&b, "h1", &[("a.md", "a"), ("notes.md", "v1")], &["notes.md"], NOW).unwrap();
    mark_handoff(&b, WT, &who("writer"), "ready", NOW).unwrap();
    b
}

/// Drop `field` from the stored handoff record, as a binary from before that field wrote it.
fn strip(b: &MemBackend, field: &str) {
    let mut v: serde_json::Value = serde_json::from_slice(&ObjectBackend::get(b, WT, HANDOFF_KEY).unwrap().unwrap()).unwrap();
    v.as_object_mut().unwrap().remove(field);
    ObjectBackend::put(b, WT, HANDOFF_KEY, &serde_json::to_vec(&v).unwrap()).unwrap();
}

fn state(b: &MemBackend) -> Handoff {
    read_handoff(b, WT).unwrap().unwrap()
}

// ---------------------------------------------------------------------------------------------------------------------
// A2 (item 3): a claim recorded before `claimed_head` existed cannot be merged; the claimer claims again
// ---------------------------------------------------------------------------------------------------------------------

#[test]
fn a_claim_without_a_claimed_head_is_refused_by_every_fenced_path_and_a_reclaim_merges() {
    let b = ready();
    let c = claim_merge(&b, WT, &who("bob"), NOW).unwrap();
    strip(&b, "claimed_head");
    assert_eq!(state(&b).claimed_head, None);
    for e in [
        mark_merged_at(&b, WT, &who("bob"), Some(c.epoch), "m", NOW).unwrap_err(),
        mark_merged(&b, WT, &who("bob"), "m", NOW).unwrap_err(),
        fence_claim(&b, WT, c.epoch, NOW + 2).unwrap_err(),
    ] {
        assert!(matches!(e, S2Error::Claimed(_)), "{e}");
        assert!(e.to_string().contains("claim again"), "the error names the next step: {e}");
    }
    assert_eq!(state(&b).state, State::Claimed, "nothing was written");
    // the holder re-claims: a new epoch, the head recorded, and that claim merges
    let again = claim_merge(&b, WT, &who("bob"), NOW + 1).unwrap();
    assert_eq!((again.epoch, again.claimed_head.as_deref()), (c.epoch + 1, Some("h1")));
    mark_merged_at(&b, WT, &who("bob"), Some(again.epoch), "m", NOW + 2).unwrap();
}

#[test]
fn an_unfenced_land_is_not_stopped_by_a_claim_without_a_claimed_head() {
    let b = ready();
    claim_merge(&b, WT, &who("bob"), NOW).unwrap();
    strip(&b, "claimed_head");
    // promote without --epoch: git is the truth, as before (design note B6)
    assert!(mark_landed(&b, WT, &who("bob"), "landed", NOW).unwrap());
    assert_eq!(state(&b).state, State::Merged);
}

// ---------------------------------------------------------------------------------------------------------------------
// A3: the daemon's `promote --epoch` fences inside the land's conditional write (`mark_landed_at`), so a takeover
// between the check and the write cannot get through (design note I1-c, I1-d)
// ---------------------------------------------------------------------------------------------------------------------

#[test]
fn a_takeover_between_the_fence_and_the_land_write_refuses_the_fenced_land() {
    let b = ready();
    let bob = claim_merge(&b, WT, &who("bob"), NOW).unwrap();
    // bob's checks pass (his claim, his epoch); then his claim lapses and carol takes over before his write lands
    let racy = Hooked::once(&b, || {
        claim_merge(&b, WT, &who("carol"), NOW + CLAIM_TTL_MS + 1).unwrap();
    });
    let e = mark_landed_at(&racy, WT, &who("bob"), Some(bob.epoch), "landed", NOW + 10).unwrap_err();
    assert!(e.to_string().contains("stale"), "{e}");
    let h = state(&b);
    assert_eq!((h.state, h.epoch, h.merger), (State::Claimed, bob.epoch + 1, Some(who("carol"))), "carol's claim stands");
}

#[test]
fn a_fenced_land_still_refuses_a_moved_head_and_a_stale_epoch_and_lands_its_own_claim() {
    let b = ready();
    let c = claim_merge(&b, WT, &who("bob"), NOW + CLAIM_SKEW_MS + 10).unwrap(); // stamped ahead: not live
    assert!(mark_landed_at(&b, WT, &who("bob"), Some(c.epoch + 1), "x", NOW).is_err(), "stale epoch");
    publish(&b, "h2", &[("a.md", "a2")], &[], NOW + 1).unwrap(); // allowed over a claim that is not live
    let e = mark_landed_at(&b, WT, &who("bob"), Some(c.epoch), "x", NOW + 2).unwrap_err();
    assert!(e.to_string().contains("from h1 to h2"), "{e}");
    assert_eq!(state(&b).state, State::Claimed, "nothing landed");
    let again = claim_merge(&b, WT, &who("bob"), NOW + 3).unwrap();
    assert!(mark_landed_at(&b, WT, &who("bob"), Some(again.epoch), "landed", NOW + 4).unwrap());
    assert_eq!(state(&b).state, State::Merged);
}

#[test]
fn without_an_epoch_the_land_is_unfenced_as_before_even_over_a_takeover() {
    let b = ready();
    claim_merge(&b, WT, &who("bob"), NOW).unwrap();
    let racy = Hooked::once(&b, || {
        claim_merge(&b, WT, &who("carol"), NOW + CLAIM_TTL_MS + 1).unwrap();
    });
    // git is the truth when no epoch is presented (design note B6): the lander is recorded, carol is named
    assert!(mark_landed_at(&racy, WT, &who("bob"), None, "landed", NOW + 10).unwrap());
    let h = state(&b);
    assert_eq!((h.state, h.merger), (State::Merged, Some(who("bob"))));
    assert!(h.note.contains("carol"), "{}", h.note);
}

#[test]
fn an_epoch_on_a_record_that_is_not_claimed_passes_as_before() {
    // Today's behaviour, kept: design note B2 asks the owner whether this should refuse.
    let b = ready();
    assert!(mark_landed_at(&b, WT, &who("bob"), Some(0), "landed", NOW).unwrap());
    let b = MemBackend::new();
    assert!(!mark_landed_at(&b, WT, &who("bob"), Some(0), "landed", NOW).unwrap(), "no record: nothing written");
}

#[test]
fn a_publish_between_the_fence_read_and_the_land_write_refuses_the_fenced_land() {
    let b = ready();
    let c = claim_merge(&b, WT, &who("bob"), NOW + CLAIM_SKEW_MS + 10).unwrap(); // not live: the republish is allowed
    let racy = Hooked::once(&b, || {
        publish(&b, "h2", &[("a.md", "a2")], &[], NOW + 1).unwrap();
    });
    assert!(mark_landed_at(&racy, WT, &who("bob"), Some(c.epoch), "landed", NOW + 2).is_err());
}

// ---------------------------------------------------------------------------------------------------------------------
// A1 (item 2): the claim is on the tree it saw (`claimed_tree`), not only on its head (design note I2-a)
// ---------------------------------------------------------------------------------------------------------------------

#[test]
fn a_republish_of_the_claimed_head_with_other_uncommitted_content_voids_the_claim_for_every_fenced_path() {
    let b = ready();
    let c = claim_merge(&b, WT, &who("bob"), NOW + CLAIM_SKEW_MS + 10).unwrap(); // stamped ahead: not live
    // same head h1, but the uncommitted notes.md changed: allowed over a claim that is not live
    publish(&b, "h1", &[("a.md", "a"), ("notes.md", "v2")], &["notes.md"], NOW + 1).unwrap();
    for e in [
        mark_merged_at(&b, WT, &who("bob"), Some(c.epoch), "m", NOW + 2).unwrap_err(),
        mark_merged(&b, WT, &who("bob"), "m", NOW + 2).unwrap_err(),
        fence_claim(&b, WT, c.epoch, NOW + 2).unwrap_err(),
        mark_landed_at(&b, WT, &who("bob"), Some(c.epoch), "landed", NOW + 2).unwrap_err(),
    ] {
        assert!(matches!(e, S2Error::Claimed(_)), "{e}");
        assert!(e.to_string().contains("claim again"), "{e}");
    }
    assert_eq!(state(&b).state, State::Claimed, "nothing was written");
    // a re-claim takes the new tree and merges it
    let again = claim_merge(&b, WT, &who("bob"), NOW + 3).unwrap();
    mark_merged_at(&b, WT, &who("bob"), Some(again.epoch), "m", NOW + 4).unwrap();
}

#[test]
fn a_republish_of_the_same_tree_does_not_void_the_claim() {
    let b = ready();
    let c = claim_merge(&b, WT, &who("bob"), NOW + CLAIM_SKEW_MS + 10).unwrap();
    // byte-for-byte the same tree at a later time: the manifest bytes change (published_ms), the claim holds
    publish(&b, "h1", &[("a.md", "a"), ("notes.md", "v1")], &["notes.md"], NOW + 5).unwrap();
    mark_merged_at(&b, WT, &who("bob"), Some(c.epoch), "m", NOW + 6).unwrap();
}

#[test]
fn a_file_leaving_the_uncommitted_list_voids_the_claim() {
    let b = ready();
    let c = claim_merge(&b, WT, &who("bob"), NOW + CLAIM_SKEW_MS + 10).unwrap();
    // same head, same bytes, but notes.md is now reported as committed: the merger's view of h1 differs
    publish(&b, "h1", &[("a.md", "a"), ("notes.md", "v1")], &[], NOW + 1).unwrap();
    assert!(mark_merged_at(&b, WT, &who("bob"), Some(c.epoch), "m", NOW + 2).is_err());
}

#[test]
fn a_claim_records_the_tree_digest_of_the_manifest_it_read() {
    let b = ready();
    let c = claim_merge(&b, WT, &who("bob"), NOW).unwrap();
    let m = read_manifest(&b, WT).unwrap().unwrap();
    assert_eq!(c.claimed_tree, Some(tree_digest(&m)));
    assert_eq!(state(&b).claimed_tree, c.claimed_tree, "stored, not only returned");
    assert!(c.claimed_tree.unwrap().starts_with("t1:"));
}

#[test]
fn a_claim_recorded_before_claimed_tree_is_fenced_on_head_only() {
    let b = ready();
    let c = claim_merge(&b, WT, &who("bob"), NOW + CLAIM_SKEW_MS + 10).unwrap();
    strip(&b, "claimed_tree");
    publish(&b, "h1", &[("a.md", "a"), ("notes.md", "v2")], &["notes.md"], NOW + 1).unwrap();
    mark_merged_at(&b, WT, &who("bob"), Some(c.epoch), "m", NOW + 2).unwrap();
}

#[test]
fn the_tree_digest_follows_what_a_merger_fetches_and_ignores_the_rest() {
    let base = TreeManifest {
        schema: SCHEMA,
        generation: 0,
        branch: "feat/y".into(),
        base_commit: "base".into(),
        head_commit: "h1".into(),
        files: vec![
            FileEntry { path: "a.md".into(), size: 1, sha256: "aa".into(), generation: None },
            FileEntry { path: "b.md".into(), size: 2, sha256: "bb".into(), generation: None },
        ],
        uncommitted: vec!["b.md".into()],
        changes: vec![],
        docs: vec![],
        has_bundle: false,
        bundle_sha256: None,
        changes_recorded: false,
        bundle_gen: None,
        retiring: vec![],
        v1_retired_ms: None,
        writer: who("writer"),
        published_ms: 1,
    };
    let d = tree_digest(&base);
    let same = |m: TreeManifest| assert_eq!(tree_digest(&m), d);
    let differs = |m: TreeManifest, what: &str| assert_ne!(tree_digest(&m), d, "{what}");
    same(TreeManifest { published_ms: 99, writer: who("other"), branch: "x".into(), ..base.clone() });
    same(TreeManifest { docs: vec![DocEntry { path: "a.md".into(), title: Some("A".into()) }], ..base.clone() });
    same(TreeManifest { changes: vec![PathChange { path: "a.md".into(), kind: ChangeKind::Added }], ..base.clone() });
    same(TreeManifest { files: base.files.iter().rev().cloned().collect(), ..base.clone() });
    differs(TreeManifest { head_commit: "h2".into(), ..base.clone() }, "head");
    differs(TreeManifest { base_commit: "b2".into(), ..base.clone() }, "base");
    let mut f = base.files.clone();
    f[1].sha256 = "cc".into();
    differs(TreeManifest { files: f, ..base.clone() }, "file content");
    differs(TreeManifest { files: base.files[..1].to_vec(), ..base.clone() }, "file set");
    differs(TreeManifest { uncommitted: vec![], ..base.clone() }, "uncommitted list");
    differs(TreeManifest { has_bundle: true, bundle_sha256: Some("bs".into()), ..base.clone() }, "bundle");
    // a path/size boundary cannot be shifted to collide: fields are length-prefixed
    let mut g = base.files.clone();
    g[0].path = "a.m".into();
    g[0].sha256 = "daa".into();
    differs(TreeManifest { files: g, ..base.clone() }, "length-prefixed");
}
