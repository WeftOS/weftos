//! S2 tests: promotion archives, retry after partial promotion, reader ceilings, and secret-name sharing rules.
use weftos_store::overlay::*;
use weftos_store::promote::*;
use weftos_store::share::*;
use weftos_store::share_fetch::*;
use weftos_store::testing::MemBackend;

#[path = "s2_worktree_common/mod.rs"]
mod common;
use common::*;

#[test]
fn promotion_archives_the_shared_tree_and_never_promotes_it() {
    let d = tree_dir();
    let b = setup();
    let ps = paths(&["docs/grok-build-fork/rooms.md"]);
    let src = TreeSource {
        root: d.path(),
        paths: &ps,
        uncommitted: &[],
        changes: &[],
        changes_recorded: false,
        branch: "f",
        base_commit: "a",
        head_commit: "b",
        bundle: None,
        writer: me("w", "h"),
    };
    publish_tree(&b, WT, &src, &Limits::default(), 1).unwrap();
    promote(&b, WT, PR, &ARCH, &policy(), 5_000).unwrap();
    assert!(
        b.list(PR, ".wt/").unwrap().is_empty(),
        "no reserved state in the project bucket"
    );
    assert!(
        b.get(
            AR,
            &format!("archive/worktrees/s2/{}", obj(&b, "docs/grok-build-fork/rooms.md"))
        )
        .unwrap()
        .is_some()
    );
}

/// Backend that fails the Nth project write, to prove a retry after partial promotion works.
struct Flaky<'a> {
    inner: &'a MemBackend,
    fail_after: std::cell::Cell<usize>,
}
impl ObjectBackend for Flaky<'_> {
    fn head(&self, b: &str, k: &str) -> Result<Option<ObjectMeta>> {
        self.inner.head(b, k)
    }
    fn get(&self, b: &str, k: &str) -> Result<Option<Vec<u8>>> {
        self.inner.get(b, k)
    }
    fn put(&self, b: &str, k: &str, d: &[u8]) -> Result<ObjectMeta> {
        if b == PR && !k.starts_with("archive/") {
            let n = self.fail_after.get();
            if n == 0 {
                return Err(S2Error::Backend("injected".into()));
            }
            self.fail_after.set(n - 1);
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
fn retry_after_partial_promotion_is_not_a_false_conflict() {
    let b = setup();
    let ov = Overlay::new(&b, WT, PR);
    ov.put("assets/a.bin", b"aaa").unwrap();
    ov.put("assets/b.bin", b"bbb").unwrap();
    b.set_clock(5_000);
    let flaky = Flaky {
        inner: &b,
        fail_after: std::cell::Cell::new(1),
    };
    assert!(promote(&flaky, WT, PR, &ARCH, &policy(), 5_000).is_err());
    assert!(is_promoted(&b, WT).unwrap().is_none());
    b.set_clock(6_000);
    let r = promote(&b, WT, PR, &ARCH, &policy(), 6_000).unwrap();
    assert_eq!(r.promoted.len(), 2);
    assert_eq!(b.get(PR, "assets/b.bin").unwrap().unwrap(), b"bbb");
}

#[test]
fn a_same_size_project_edit_between_attempts_is_a_conflict_not_our_copy() {
    let b = setup();
    let ov = Overlay::new(&b, WT, PR);
    ov.put("assets/a.bin", b"aaa").unwrap();
    b.set_clock(5_000);
    let flaky = Flaky { inner: &b, fail_after: std::cell::Cell::new(0) };
    assert!(promote(&flaky, WT, PR, &ARCH, &policy(), 5_000).is_err(), "attempt 1 records its intent, then fails before copying");
    // someone puts different content of the same size into the project
    b.put(PR, "assets/a.bin", b"zzz").unwrap();
    let r = promote(&b, WT, PR, &ARCH, &policy(), 6_000);
    assert!(matches!(r, Err(S2Error::Conflict(ref k)) if k == &["assets/a.bin".to_string()]), "{r:?}");
    assert_eq!(b.get(PR, "assets/a.bin").unwrap().unwrap(), b"zzz", "the project edit survived");
}

#[test]
fn a_promotion_record_from_an_earlier_life_of_a_slug_is_ignored() {
    let b = setup();
    let meta = |ms| WorktreeMeta { slug: "f".into(), project: "01abc".into(), branch: "f".into(), base_commit: "a".into(), created_ms: ms };
    write_meta(&b, WT, &meta(100)).unwrap();
    Overlay::new(&b, WT, PR).put("assets/a.bin", b"aaa").unwrap();
    promote(&b, WT, PR, &ARCH, &policy(), 200).unwrap();
    assert!(is_promoted(&b, WT).unwrap().is_some());
    // the slug is reused: new metadata, same bucket, old promoted.json still there
    write_meta(&b, WT, &meta(300)).unwrap();
    assert!(is_promoted(&b, WT).unwrap().is_none(), "the old report must not stand in for the new worktree");
    Overlay::new(&b, WT, PR).put("assets/new.bin", b"nnn").unwrap();
    let r = promote(&b, WT, PR, &ARCH, &policy(), 400).unwrap();
    assert!(r.promoted.contains(&"assets/new.bin".to_string()), "{r:?}");
}

#[test]
fn readers_enforce_size_and_count_ceilings_and_check_the_bundle_hash() {
    let b = setup();
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("a.txt"), "hello").unwrap();
    let bundle = d.path().join("in.bundle");
    std::fs::write(&bundle, b"GOOD-BUNDLE").unwrap();
    let ps = paths(&["a.txt"]);
    let src = TreeSource { root: d.path(), paths: &ps, uncommitted: &[], changes: &[], changes_recorded: false, branch: "f", base_commit: "a", head_commit: "h", bundle: Some(&bundle), writer: me("w", "h") };
    publish_tree(&b, WT, &src, &Limits::default(), 1).unwrap();
    assert!(read_manifest(&b, WT).unwrap().unwrap().bundle_sha256.is_some());

    // a writer with direct S3 access swaps the bundle
    b.put(WT, &bundle_obj(&b), b"EVIL-BUNDLE").unwrap();
    let out = tempfile::tempdir().unwrap();
    let e = fetch_bundle(&b, WT, &out.path().join("x.bundle")).unwrap_err().to_string();
    assert!(e.contains("checksum"), "{e}");
    assert!(!out.path().join("x.bundle").exists());

    // an object grown past the per-file ceiling after publishing is refused before it is read
    let big = vec![b'x'; 2048];
    b.put(WT, &obj(&b, "a.txt"), &big).unwrap();
    let lim = Limits { max_file: 1024, ..Limits::default() };
    let dest = tempfile::tempdir().unwrap();
    let e = fetch_tree_with(&b, WT, dest.path(), Select::All, true, &lim).unwrap_err().to_string();
    assert!(e.contains("per-file limit"), "{e}");
    let lim = Limits { max_total: 3, ..Limits::default() };
    let e = fetch_tree_with(&b, WT, dest.path(), Select::All, true, &lim).unwrap_err().to_string();
    assert!(e.contains("total limit"), "{e}");

    // an enormous manifest is not loaded
    b.put(WT, MANIFEST_KEY, &vec![b' '; (MAX_MANIFEST_BYTES + 1) as usize]).unwrap();
    assert!(read_manifest(&b, WT).unwrap_err().to_string().contains("over the"));
}

#[test]
fn a_claim_stamped_far_in_the_future_does_not_hold_the_merge() {
    let b = setup();
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("a.txt"), "x").unwrap();
    let ps = paths(&["a.txt"]);
    let src = TreeSource { root: d.path(), paths: &ps, uncommitted: &[], changes: &[], changes_recorded: false, branch: "f", base_commit: "a", head_commit: "h", bundle: None, writer: me("w", "h") };
    publish_tree(&b, WT, &src, &Limits::default(), 1_000).unwrap();
    let (bob, alice) = (me("bob", "m2"), me("alice", "m1"));
    // bob's clock is a day ahead: his claim carries a stamp far past this daemon's clock
    claim_merge(&b, WT, &bob, 1_000 + 24 * 3600 * 1000).unwrap();
    // a modest skew still counts as live
    let e = claim_merge(&b, WT, &alice, 1_000 + 24 * 3600 * 1000 - 5 * 60 * 1000).unwrap_err();
    assert!(matches!(e, S2Error::Claimed(_)), "{e:?}");
    // but at the daemon's own clock the stamp is more than ten minutes ahead: not trusted as live
    let h = claim_merge(&b, WT, &alice, 2_000).unwrap();
    assert_eq!(h.merger, Some(alice));
}

#[test]
fn private_key_content_and_secret_names_are_not_shared_but_secret_docs_are() {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(
        d.path().join("notes.txt"),
        "-----BEGIN OPENSSH PRIVATE KEY-----\nxx",
    )
    .unwrap();
    std::fs::write(d.path().join("secrets-policy.md"), "# Secrets policy\n").unwrap();
    std::fs::write(d.path().join("my_secret.json"), "{}").unwrap();
    let b = setup();
    let ps = paths(&["notes.txt", "secrets-policy.md", "my_secret.json"]);
    let src = TreeSource {
        root: d.path(),
        paths: &ps,
        uncommitted: &[],
        changes: &[],
        changes_recorded: false,
        branch: "f",
        base_commit: "a",
        head_commit: "b",
        bundle: None,
        writer: me("w", "h"),
    };
    let r = publish_tree(&b, WT, &src, &Limits::default(), 1).unwrap();
    assert_eq!(r.uploaded, 1);
    assert_eq!(
        r.skipped.iter().filter(|(_, s)| *s == Skip::Secret).count(),
        2
    );
    assert!(b.get(WT, &obj(&b, "secrets-policy.md")).unwrap().is_some());
}

#[test]
fn merged_worktree_cannot_be_republished() {
    let d = tree_dir();
    let b = setup();
    let ps = paths(&["src/lib.rs"]);
    let w = me("w", "h");
    let src = TreeSource {
        root: d.path(),
        paths: &ps,
        uncommitted: &[],
        changes: &[],
        changes_recorded: false,
        branch: "f",
        base_commit: "a",
        head_commit: "b",
        bundle: None,
        writer: w.clone(),
    };
    publish_tree(&b, WT, &src, &Limits::default(), 1).unwrap();
    claim_merge(&b, WT, &w, 2).unwrap();
    mark_merged(&b, WT, &w, "done", 3).unwrap();
    assert!(publish_tree(&b, WT, &src, &Limits::default(), 4).is_err());
}
