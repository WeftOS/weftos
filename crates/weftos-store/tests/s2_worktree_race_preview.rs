//! S2 tests: races between plan and apply, promotion preview, the land size cap and retention.
use weftos_store::overlay::*;
use weftos_store::promote::*;
use weftos_store::testing::MemBackend;

#[path = "s2_worktree_common/mod.rs"]
mod common;
use common::*;

/// A backend that lets "someone else" write the project between a promotion's plan and its apply
/// (the moment the promotion intent is recorded).
struct Racy<'a> {
    inner: &'a MemBackend,
    race: Option<(&'static str, Option<&'static [u8]>)>, // key, Some(content) = write, None = delete
}
impl ObjectBackend for Racy<'_> {
    fn head(&self, b: &str, k: &str) -> Result<Option<ObjectMeta>> {
        self.inner.head(b, k)
    }
    fn get(&self, b: &str, k: &str) -> Result<Option<Vec<u8>>> {
        self.inner.get(b, k)
    }
    fn put(&self, b: &str, k: &str, d: &[u8]) -> Result<ObjectMeta> {
        if k.ends_with("promoting.json")
            && let Some((rk, c)) = self.race
        {
            match c {
                Some(c) => drop(self.inner.put(PR, rk, c)?),
                None => self.inner.delete(PR, rk)?,
            }
        }
        self.inner.put(b, k, d)
    }
    fn delete(&self, b: &str, k: &str) -> Result<()> {
        self.inner.delete(b, k)
    }
    fn list(&self, b: &str, p: &str) -> Result<Vec<ObjectMeta>> {
        self.inner.list(b, p)
    }
}

#[test]
fn a_project_write_between_plan_and_apply_aborts_the_copy() {
    let b = setup();
    Overlay::new(&b, WT, PR).put("assets/logo.png", b"mine").unwrap();
    let racy = Racy { inner: &b, race: Some(("assets/logo.png", Some(b"racer"))) };
    let e = promote(&racy, WT, PR, &ARCH, &policy(), 5_000).unwrap_err();
    assert_eq!(e, S2Error::Conflict(vec!["assets/logo.png".into()]));
    assert_eq!(b.get(PR, "assets/logo.png").unwrap().unwrap(), b"racer", "the racer's write survives");
    assert!(is_promoted(&b, WT).unwrap().is_none());
}

#[test]
fn a_project_change_between_plan_and_apply_aborts_the_delete() {
    let b = setup();
    Overlay::new(&b, WT, PR).delete("assets/old.txt").unwrap();
    let racy = Racy { inner: &b, race: Some(("assets/old.txt", Some(b"edited"))) };
    let e = promote(&racy, WT, PR, &ARCH, &policy(), 5_000).unwrap_err();
    assert_eq!(e, S2Error::Conflict(vec!["assets/old.txt".into()]));
    assert_eq!(b.get(PR, "assets/old.txt").unwrap().unwrap(), b"edited");
}

#[test]
fn a_run_aborted_by_a_race_retries_cleanly_once_the_conflict_is_settled() {
    let b = setup();
    let ov = Overlay::new(&b, WT, PR);
    ov.put("assets/a.txt", b"A").unwrap();
    ov.put("assets/logo.png", b"mine").unwrap();
    let racy = Racy { inner: &b, race: Some(("assets/logo.png", Some(b"racer"))) };
    assert!(promote(&racy, WT, PR, &ARCH, &policy(), 5_000).is_err());
    // the owner resolves it by choosing to overwrite; the retry completes and a.txt is not redone wrongly
    let p = PromotionPolicy { on_conflict: OnConflict::Overwrite, ..policy() };
    let r = promote(&b, WT, PR, &ARCH, &p, 6_000).unwrap();
    assert!(r.promoted.contains(&"assets/logo.png".to_string()));
    assert_eq!(b.get(AR, "archive/worktrees/s2/.wt/displaced/assets/logo.png").unwrap().unwrap(), b"racer");
}

// ---- land preview and size cap ------------------------------------------------------------------

#[test]
fn preview_reports_every_kind_of_conflict_and_writes_nothing() {
    let b = setup();
    let ov = Overlay::new(&b, WT, PR);
    ov.put("assets/logo.png", b"wt-version").unwrap(); // project moves it below: both edited
    ov.put("assets/fine.txt", b"ok").unwrap();
    ov.put("agents/i1/out.log", b"log").unwrap();
    b.put(PR, "assets/a.png", b"v1").unwrap();
    ov.put("assets/a.png", b"mine").unwrap(); // project deletes it below
    ov.delete("assets/old.txt").unwrap(); // project changes it below: deleted but project edited
    b.set_clock(3_000);
    b.put(PR, "assets/logo.png", b"someone-else").unwrap();
    b.delete(PR, "assets/a.png").unwrap();
    b.put(PR, "assets/old.txt", b"changed").unwrap();
    let (before_pr, before_wt) = (b.len(PR), b.len(WT));

    let pv = preview(&b, WT, PR, &policy()).unwrap();
    assert_eq!(
        pv.conflicts,
        [
            ("assets/a.png".to_string(), ConflictKind::ProjectDeleted),
            ("assets/logo.png".to_string(), ConflictKind::BothEdited),
            ("assets/old.txt".to_string(), ConflictKind::DeletedButProjectEdited),
        ]
    );
    assert_eq!(pv.would_promote, ["assets/fine.txt"]);
    assert_eq!(pv.would_archive, ["agents/i1/out.log"]);
    assert_eq!(pv.promote_bytes, 2);
    assert!(!pv.over_size_cap && !pv.already_promoted);
    assert_eq!((b.len(PR), b.len(WT)), (before_pr, before_wt), "a preview writes nothing, not even the intent record");
    // the real land agrees with the preview
    let e = promote(&b, WT, PR, &ARCH, &policy(), 5_000).unwrap_err();
    assert_eq!(e, S2Error::Conflict(vec!["assets/a.png".into(), "assets/logo.png".into(), "assets/old.txt".into()]));

    // under keep_project the same land has no conflicts and says what it keeps
    let keep = PromotionPolicy { on_conflict: OnConflict::KeepProject, ..policy() };
    let pv = preview(&b, WT, PR, &keep).unwrap();
    assert!(pv.conflicts.is_empty());
    assert!(pv.would_keep_project.contains(&"assets/logo.png".to_string()));
}

#[test]
fn preview_of_a_landed_worktree_says_so() {
    let b = setup();
    Overlay::new(&b, WT, PR).put("assets/x.txt", b"x").unwrap();
    promote(&b, WT, PR, &ARCH, &policy(), 5_000).unwrap();
    let pv = preview(&b, WT, PR, &policy()).unwrap();
    assert!(pv.already_promoted && pv.would_promote.is_empty());
    assert!(preview(&b, "w-01abc-bbbbbbbbbbbb", PR, &policy()).is_err(), "no meta, no preview");
}

#[test]
fn a_land_over_the_size_cap_is_refused_before_any_write_and_the_preview_says_so() {
    let b = setup();
    let ov = Overlay::new(&b, WT, PR);
    ov.put("assets/big.bin", &[7u8; 600]).unwrap();
    ov.put("assets/more.bin", &[8u8; 600]).unwrap();
    let small = PromotionPolicy { max_promote_bytes: Some(1_000), ..policy() };
    let before = b.len(PR);
    let pv = preview(&b, WT, PR, &small).unwrap();
    assert!(pv.over_size_cap);
    assert_eq!(pv.promote_bytes, 1_200);
    let e = promote(&b, WT, PR, &ARCH, &small, 5_000).unwrap_err();
    assert!(matches!(&e, S2Error::Invalid(m) if m.contains("over the 1000 byte cap")), "{e:?}");
    assert_eq!(b.len(PR), before, "nothing written");
    assert!(is_promoted(&b, WT).unwrap().is_none());
    // the cap counts only what is copied: scratch that is archived does not count
    ov.delete("assets/more.bin").unwrap();
    ov.put("agents/i1/huge.log", &[1u8; 5_000]).unwrap();
    promote(&b, WT, PR, &ARCH, &small, 6_000).unwrap();
    // no cap at all
    let b2 = setup();
    Overlay::new(&b2, WT, PR).put("assets/big.bin", &[7u8; 5_000]).unwrap();
    let off = PromotionPolicy { max_promote_bytes: None, ..policy() };
    promote(&b2, WT, PR, &ARCH, &off, 5_000).unwrap();
}

// ---- retention -------------------------------------------------------------------------------

#[test]
fn only_landed_worktrees_past_the_cutoff_are_retention_candidates() {
    use weftos_store::retention::*;
    let b = setup();
    assert_eq!(landed_before(&b, WT, u64::MAX).unwrap(), None, "unlanded is never a candidate, however old");
    Overlay::new(&b, WT, PR).put("assets/x.txt", b"x").unwrap();
    promote(&b, WT, PR, &ARCH, &policy(), 5_000).unwrap();
    assert_eq!(landed_before(&b, WT, 4_999).unwrap(), None, "landed too recently");
    assert_eq!(landed_before(&b, WT, 5_000).unwrap(), Some(5_000));
    let (n, bytes) = usage(&b, WT).unwrap();
    assert!(n >= 2 && bytes >= 1, "meta, base and the object are counted: {n} {bytes}");
}

#[test]
fn archive_expiry_reads_the_timestamp_and_touches_only_worktree_buckets() {
    use weftos_store::retention::*;
    let b = MemBackend::new();
    for k in [
        "w-01abc-aaaaaaaaaaaa@1000/assets/a.png",
        "w-01abc-aaaaaaaaaaaa@9000/assets/b.png",
        "w-01abc-bbbbbbbbbbbb@500/x",
        "w-01zzz-cccccccccccc@500/x",         // another project
        "n-node@500/x",                         // a node store's archive: not ours to expire
        "w-01abc-dddddddddddd@notanumber/x",   // malformed
        "no-at-sign/x",
    ] {
        b.put("arch", k, b"d").unwrap();
    }
    let mut got = expired_archive_keys(&b, "arch", "w-01abc-", 2_000).unwrap();
    got.sort();
    assert_eq!(got, ["w-01abc-aaaaaaaaaaaa@1000/assets/a.png", "w-01abc-bbbbbbbbbbbb@500/x"]);
    assert_eq!(archive_key_parts("w-01abc-x@42/a/b"), Some(("w-01abc-x", 42)));
    assert_eq!(archive_key_parts("w-01abc-x/a"), None);
}
