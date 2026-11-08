//! S2 tests: overlay reads, promotion on land, worktree sharing and cross-agent merge handoff.
use weftos_store::overlay::*;
use weftos_store::promote::*;
use weftos_store::share::*;
use weftos_store::share_fetch::*;
use weftos_store::testing::MemBackend;

const WT: &str = "w-01abc-aaaaaaaaaaaa";
const PR: &str = "p-01abc";
const AR: &str = "p-01abc"; // archive lives in the project bucket under a prefix

fn me(a: &str, h: &str) -> Actor {
    Actor {
        agent: a.into(),
        host: h.into(),
    }
}

fn setup() -> MemBackend {
    let b = MemBackend::new();
    b.set_clock(1_000);
    b.put(PR, "assets/logo.png", b"logo-v1").unwrap();
    b.put(PR, "assets/old.txt", b"old").unwrap();
    b.set_clock(2_000);
    write_meta(
        &b,
        WT,
        &WorktreeMeta {
            slug: "s2".into(),
            project: "01abc".into(),
            branch: "feat/x".into(),
            base_commit: "aaa".into(),
            created_ms: 2_000,
        },
    )
    .unwrap();
    b
}

#[test]
fn reads_fall_through_and_writes_stay_local() {
    let b = setup();
    let ov = Overlay::new(&b, WT, PR);
    assert_eq!(ov.get("assets/logo.png").unwrap().unwrap(), b"logo-v1");
    ov.put("assets/logo.png", b"logo-wt").unwrap();
    assert_eq!(ov.get("assets/logo.png").unwrap().unwrap(), b"logo-wt");
    assert_eq!(
        b.get(PR, "assets/logo.png").unwrap().unwrap(),
        b"logo-v1",
        "project untouched"
    );
    assert!(ov.get("nope").unwrap().is_none());
}

#[test]
fn list_merges_shadows_and_hides_reserved() {
    let b = setup();
    let ov = Overlay::new(&b, WT, PR);
    ov.put("assets/new.txt", b"n").unwrap();
    ov.put("assets/logo.png", b"wt").unwrap();
    let keys: Vec<_> = ov
        .list("assets/")
        .unwrap()
        .into_iter()
        .map(|m| m.key)
        .collect();
    assert_eq!(
        keys,
        ["assets/logo.png", "assets/new.txt", "assets/old.txt"]
    );
    assert!(
        ov.list("")
            .unwrap()
            .iter()
            .all(|m| !m.key.starts_with(".wt/")),
        "meta must not leak"
    );
}

#[test]
fn delete_tombstones_project_object_and_put_revives() {
    let b = setup();
    let ov = Overlay::new(&b, WT, PR);
    ov.delete("assets/old.txt").unwrap();
    assert!(ov.get("assets/old.txt").unwrap().is_none());
    assert!(ov.head("assets/old.txt").unwrap().is_none());
    assert!(
        ov.list("assets/")
            .unwrap()
            .iter()
            .all(|m| m.key != "assets/old.txt")
    );
    assert!(
        b.get(PR, "assets/old.txt").unwrap().is_some(),
        "project untouched"
    );
    ov.put("assets/old.txt", b"back").unwrap();
    assert_eq!(ov.get("assets/old.txt").unwrap().unwrap(), b"back");
    assert!(ov.tombstones().unwrap().is_empty());
    // deleting a worktree-only key leaves no tombstone
    ov.put("assets/tmp", b"x").unwrap();
    ov.delete("assets/tmp").unwrap();
    assert!(ov.tombstones().unwrap().is_empty());
}

#[test]
fn bad_and_reserved_keys_are_refused() {
    let b = setup();
    let ov = Overlay::new(&b, WT, PR);
    for k in [
        "",
        "/abs",
        "a/../b",
        "a//b",
        "a\\b",
        "a/./b",
        ".wt/meta.json",
        "x\0y",
    ] {
        assert!(ov.put(k, b"x").is_err(), "{k:?}");
        assert!(ov.get(k).is_err(), "{k:?}");
    }
}

fn policy() -> PromotionPolicy {
    PromotionPolicy::default()
}
const ARCH: ArchiveTarget<'static> = ArchiveTarget {
    bucket: AR,
    prefix: "archive/worktrees/s2/",
};

#[test]
fn promote_copies_promotable_archives_rest_and_is_idempotent() {
    let b = setup();
    let ov = Overlay::new(&b, WT, PR);
    ov.put("assets/new.png", b"new").unwrap();
    ov.put("agents/i1/out.log", b"log").unwrap();
    ov.delete("assets/old.txt").unwrap();
    ov.delete("scratch/gone").unwrap(); // nothing in project: no tombstone
    b.set_clock(5_000);
    let r = promote(&b, WT, PR, &ARCH, &policy(), 5_000).unwrap();
    assert_eq!(r.promoted, ["assets/new.png"]);
    assert_eq!(r.archived, ["agents/i1/out.log"]);
    assert_eq!(r.deleted, ["assets/old.txt"]);
    assert_eq!(b.get(PR, "assets/new.png").unwrap().unwrap(), b"new");
    assert!(b.get(PR, "assets/old.txt").unwrap().is_none());
    assert!(
        b.get(PR, "agents/i1/out.log").unwrap().is_none(),
        "scratch never promoted"
    );
    assert_eq!(
        b.get(AR, "archive/worktrees/s2/agents/i1/out.log")
            .unwrap()
            .unwrap(),
        b"log"
    );
    assert!(is_promoted(&b, WT).unwrap().is_some());
    let again = promote(&b, WT, PR, &ARCH, &policy(), 9_999).unwrap();
    assert_eq!(again, r, "second run returns the stored report");
}

#[test]
fn promote_conflict_fails_before_any_write() {
    let b = setup();
    let ov = Overlay::new(&b, WT, PR);
    ov.put("assets/logo.png", b"wt-version").unwrap();
    ov.put("assets/fine.txt", b"ok").unwrap();
    b.set_clock(3_000); // another agent promotes a different logo after we branched
    b.put(PR, "assets/logo.png", b"someone-else").unwrap();
    let before = b.len(PR);
    let e = promote(&b, WT, PR, &ARCH, &policy(), 5_000).unwrap_err();
    assert_eq!(e, S2Error::Conflict(vec!["assets/logo.png".into()]));
    assert_eq!(b.len(PR), before, "nothing written");
    assert!(is_promoted(&b, WT).unwrap().is_none());
}

#[test]
fn promote_conflict_policies() {
    for (pol, expect_project) in [
        (OnConflict::KeepProject, &b"someone-else"[..]),
        (OnConflict::Overwrite, &b"wt-version"[..]),
    ] {
        let b = setup();
        Overlay::new(&b, WT, PR)
            .put("assets/logo.png", b"wt-version")
            .unwrap();
        b.set_clock(3_000);
        b.put(PR, "assets/logo.png", b"someone-else").unwrap();
        let p = PromotionPolicy {
            on_conflict: pol,
            ..policy()
        };
        let r = promote(&b, WT, PR, &ARCH, &p, 5_000).unwrap();
        assert_eq!(
            b.get(PR, "assets/logo.png").unwrap().unwrap(),
            expect_project
        );
        if pol == OnConflict::KeepProject {
            assert_eq!(r.kept_project, ["assets/logo.png"]);
            assert!(
                b.get(AR, "archive/worktrees/s2/assets/logo.png")
                    .unwrap()
                    .is_some(),
                "losing copy archived"
            );
        }
    }
}

#[test]
fn identical_content_is_not_a_conflict_and_old_project_edits_dont_conflict() {
    let b = setup();
    Overlay::new(&b, WT, PR)
        .put("assets/logo.png", b"logo-v1")
        .unwrap(); // same bytes as project
    promote(&b, WT, PR, &ARCH, &policy(), 5_000).unwrap();
}

#[test]
fn promote_refuses_without_meta_and_with_bad_policy() {
    let b = MemBackend::new();
    assert!(promote(&b, WT, PR, &ARCH, &policy(), 1).is_err());
    let b = setup();
    let bad = PromotionPolicy {
        promote_prefixes: vec!["assets".into()],
        ..policy()
    };
    assert!(promote(&b, WT, PR, &ARCH, &bad, 1).is_err());
    assert!(promote(&b, WT, WT, &ARCH, &policy(), 1).is_err());
}

fn tree_dir() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    let w = |p: &str, c: &str| {
        let f = d.path().join(p);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, c).unwrap();
    };
    w(
        "docs/grok-build-fork/rooms.md",
        "# Rooms and doors contract\nbody",
    );
    w(".planning/plan.md", "# Plan\n");
    w("src/lib.rs", "fn main(){}");
    w(".env", "TOKEN=1");
    w("keys/server.pem", "pem");
    d
}

fn paths(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

#[test]
fn publish_shares_docs_skips_secrets_symlinks_and_bad_paths() {
    let d = tree_dir();
    #[cfg(unix)]
    std::os::unix::fs::symlink("/etc/passwd", d.path().join("link")).unwrap();
    let b = setup();
    let ps = paths(&[
        "docs/grok-build-fork/rooms.md",
        ".planning/plan.md",
        "src/lib.rs",
        ".env",
        "keys/server.pem",
        "link",
        "../escape",
        "missing.txt",
        ".wt/x",
    ]);
    let src = TreeSource {
        root: d.path(),
        paths: &ps,
        uncommitted: &paths(&["src/lib.rs", "nonexistent"]),
        changes: &[],
        branch: "feat/x",
        base_commit: "aaa",
        head_commit: "bbb",
        bundle: None,
        writer: me("s2", "xpc"),
    };
    let r = publish_tree(&b, WT, &src, &Limits::default(), 6_000).unwrap();
    assert_eq!(r.uploaded, 3);
    let skipped: std::collections::HashMap<_, _> = r.skipped.iter().cloned().collect();
    assert_eq!(skipped[".env"], Skip::Secret);
    assert_eq!(skipped["keys/server.pem"], Skip::Secret);
    assert_eq!(skipped["../escape"], Skip::BadPath);
    assert_eq!(skipped[".wt/x"], Skip::BadPath);
    assert_eq!(skipped["missing.txt"], Skip::Missing);
    #[cfg(unix)]
    assert_eq!(skipped["link"], Skip::Symlink);
    let m = read_manifest(&b, WT).unwrap().unwrap();
    assert_eq!(m.uncommitted, ["src/lib.rs"]);
    // findable by title or path, from the manifest alone
    let hits = find_docs(&m, "rooms and doors");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].path, "docs/grok-build-fork/rooms.md");
    assert_eq!(find_docs(&m, "").len(), 2);
    // never visible through the asset overlay, never promoted as an asset
    assert!(
        Overlay::new(&b, WT, PR)
            .list("")
            .unwrap()
            .iter()
            .all(|m| !m.key.contains("grok-build-fork") && !m.key.starts_with(".wt/"))
    );
    assert!(b.get(WT, ".wt/tree/.env").unwrap().is_none());
}

#[test]
fn republish_is_incremental_and_removes_deleted_files() {
    let d = tree_dir();
    let b = setup();
    let ps = paths(&["docs/grok-build-fork/rooms.md", "src/lib.rs"]);
    fn mk<'a>(d: &'a std::path::Path, ps: &'a [String], head: &'a str) -> TreeSource<'a> {
        TreeSource {
            root: d,
            paths: ps,
            uncommitted: &[],
            changes: &[],
            branch: "feat/x",
            base_commit: "aaa",
            head_commit: head,
            bundle: None,
            writer: me("s2", "xpc"),
        }
    }
    publish_tree(&b, WT, &mk(d.path(), &ps, "b1"), &Limits::default(), 6_000).unwrap();
    std::fs::write(d.path().join("src/lib.rs"), "fn changed(){}").unwrap();
    let r = publish_tree(&b, WT, &mk(d.path(), &ps, "b2"), &Limits::default(), 7_000).unwrap();
    assert_eq!((r.uploaded, r.unchanged), (1, 1));
    let ps2 = paths(&["docs/grok-build-fork/rooms.md"]);
    let r = publish_tree(&b, WT, &mk(d.path(), &ps2, "b3"), &Limits::default(), 8_000).unwrap();
    assert_eq!(r.removed, 1);
    assert!(b.get(WT, ".wt/tree/src/lib.rs").unwrap().is_none());
    assert_eq!(read_manifest(&b, WT).unwrap().unwrap().files.len(), 1);
}

#[test]
fn limits_are_enforced() {
    let d = tree_dir();
    let b = setup();
    let ps = paths(&[
        "docs/grok-build-fork/rooms.md",
        "src/lib.rs",
        ".planning/plan.md",
    ]);
    let src = TreeSource {
        root: d.path(),
        paths: &ps,
        uncommitted: &[],
        changes: &[],
        branch: "f",
        base_commit: "a",
        head_commit: "b",
        bundle: None,
        writer: me("s2", "xpc"),
    };
    let lim = Limits {
        max_file: 12,
        max_files: 1,
        ..Limits::default()
    };
    let r = publish_tree(&b, WT, &src, &lim, 1).unwrap();
    assert_eq!(r.uploaded, 1);
    assert!(r.skipped.iter().any(|(_, s)| *s == Skip::TooLarge));
    assert!(r.skipped.iter().any(|(_, s)| *s == Skip::OverBudget));
}

#[test]
fn second_agent_on_second_machine_fetches_tree_and_bundle_then_merges() {
    let d = tree_dir();
    let b = setup();
    let bundle = d.path().join("branch.bundle");
    std::fs::write(&bundle, b"BUNDLE-BYTES").unwrap();
    let ps = paths(&["docs/grok-build-fork/rooms.md", "src/lib.rs"]);
    let writer = me("writer", "maxbook");
    let src = TreeSource {
        root: d.path(),
        paths: &ps,
        uncommitted: &[],
        changes: &[],
        branch: "feat/x",
        base_commit: "aaa",
        head_commit: "bbb",
        bundle: Some(&bundle),
        writer: writer.clone(),
    };
    let rep = publish_tree(&b, WT, &src, &Limits::default(), 6_000).unwrap();
    assert!(rep.bundle_uploaded);
    mark_handoff(&b, WT, &writer, "docs ready, please merge", 6_100).unwrap();
    let e = summarize(&b, "s2", WT).unwrap().unwrap();
    write_registry_entry(&b, PR, &e).unwrap();

    // a different node sees it without the original disk path
    let listed = list_shared(&b, PR).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].state, State::Handoff);
    assert_eq!(listed[0].doc_count, 1);

    let other = tempfile::tempdir().unwrap();
    let fr = fetch_tree(&b, WT, other.path(), Select::DocsOnly, false).unwrap();
    assert_eq!(fr.written, ["docs/grok-build-fork/rooms.md"]);
    assert!(!other.path().join("src/lib.rs").exists());
    let fr = fetch_tree(&b, WT, other.path(), Select::All, false).unwrap();
    assert_eq!((fr.written.len(), fr.unchanged.len()), (1, 1));
    let out = other.path().join("in/branch.bundle");
    assert_eq!(fetch_bundle(&b, WT, &out).unwrap(), "bbb");
    assert_eq!(std::fs::read(out).unwrap(), b"BUNDLE-BYTES");

    // merger claims; a rival is refused; stale claims can be taken over
    let m1 = me("merger", "xpc");
    let m2 = me("rival", "pg");
    claim_merge(&b, WT, &m1, 7_000).unwrap();
    assert_eq!(
        claim_merge(&b, WT, &m2, 7_001).unwrap_err(),
        S2Error::Claimed("merger@xpc".into())
    );
    claim_merge(&b, WT, &m1, 7_002).unwrap(); // idempotent for the holder
    claim_merge(&b, WT, &m2, 7_002 + CLAIM_TTL_MS + 1).unwrap();
    assert!(
        mark_merged(&b, WT, &m1, "x", 99).is_err(),
        "no longer the holder"
    );
    mark_merged(&b, WT, &m2, "landed as abc123", 8_000_000).unwrap();
    assert!(claim_merge(&b, WT, &m1, 9_000_000).is_err());
    assert!(mark_handoff(&b, WT, &writer, "again", 9_000_001).is_err());
}

#[test]
fn fetch_never_clobbers_local_edits_and_detects_corruption() {
    let d = tree_dir();
    let b = setup();
    let ps = paths(&["src/lib.rs"]);
    let src = TreeSource {
        root: d.path(),
        paths: &ps,
        uncommitted: &[],
        changes: &[],
        branch: "f",
        base_commit: "a",
        head_commit: "b",
        bundle: None,
        writer: me("w", "h"),
    };
    publish_tree(&b, WT, &src, &Limits::default(), 1).unwrap();
    let dest = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dest.path().join("src")).unwrap();
    std::fs::write(dest.path().join("src/lib.rs"), "local edit").unwrap();
    let r = fetch_tree(&b, WT, dest.path(), Select::All, false).unwrap();
    assert_eq!(r.conflicts, ["src/lib.rs"]);
    assert_eq!(
        std::fs::read_to_string(dest.path().join("src/lib.rs")).unwrap(),
        "local edit"
    );
    fetch_tree(&b, WT, dest.path(), Select::All, true).unwrap();
    assert_eq!(
        std::fs::read_to_string(dest.path().join("src/lib.rs")).unwrap(),
        "fn main(){}"
    );
    // tamper with the stored object
    b.put(WT, ".wt/tree/src/lib.rs", b"tampered").unwrap();
    let fresh = tempfile::tempdir().unwrap();
    assert!(fetch_tree(&b, WT, fresh.path(), Select::All, false).is_err());
}

#[test]
fn tampered_manifest_cannot_escape_dest() {
    let b = setup();
    let m = TreeManifest {
        schema: 1,
        branch: "f".into(),
        base_commit: "a".into(),
        head_commit: "b".into(),
        files: vec![FileEntry {
            path: "../evil".into(),
            size: 1,
            sha256: sha256_hex(b"x"),
        }],
        uncommitted: vec![],
        changes: vec![],
        docs: vec![],
        has_bundle: false,
        bundle_sha256: None,
        writer: me("w", "h"),
        published_ms: 1,
    };
    b.put(WT, MANIFEST_KEY, &serde_json::to_vec(&m).unwrap())
        .unwrap();
    b.put(WT, ".wt/tree/../evil", b"x").unwrap();
    let dest = tempfile::tempdir().unwrap();
    match fetch_tree(&b, WT, dest.path(), Select::All, true) {
        Err(_) => {}
        Ok(r) => {
            assert_eq!(r.refused, ["../evil"]);
            assert!(r.written.is_empty());
        }
    }
    assert!(!dest.path().parent().unwrap().join("evil").exists());
}

#[cfg(unix)]
#[test]
fn symlinked_parent_in_dest_is_refused() {
    let b = setup();
    let d = tree_dir();
    let ps = paths(&["src/lib.rs"]);
    let src = TreeSource {
        root: d.path(),
        paths: &ps,
        uncommitted: &[],
        changes: &[],
        branch: "f",
        base_commit: "a",
        head_commit: "b",
        bundle: None,
        writer: me("w", "h"),
    };
    publish_tree(&b, WT, &src, &Limits::default(), 1).unwrap();
    let outside = tempfile::tempdir().unwrap();
    let dest = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), dest.path().join("src")).unwrap();
    assert!(fetch_tree(&b, WT, dest.path(), Select::All, true).is_err());
    assert!(!outside.path().join("lib.rs").exists());
}

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
            "archive/worktrees/s2/.wt/tree/docs/grok-build-fork/rooms.md"
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
    let src = TreeSource { root: d.path(), paths: &ps, uncommitted: &[], changes: &[], branch: "f", base_commit: "a", head_commit: "h", bundle: Some(&bundle), writer: me("w", "h") };
    publish_tree(&b, WT, &src, &Limits::default(), 1).unwrap();
    assert!(read_manifest(&b, WT).unwrap().unwrap().bundle_sha256.is_some());

    // a writer with direct S3 access swaps the bundle
    b.put(WT, BUNDLE_KEY, b"EVIL-BUNDLE").unwrap();
    let out = tempfile::tempdir().unwrap();
    let e = fetch_bundle(&b, WT, &out.path().join("x.bundle")).unwrap_err().to_string();
    assert!(e.contains("checksum"), "{e}");
    assert!(!out.path().join("x.bundle").exists());

    // an object grown past the per-file ceiling after publishing is refused before it is read
    let big = vec![b'x'; 2048];
    b.put(WT, ".wt/tree/a.txt", &big).unwrap();
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
    assert!(b.get(WT, ".wt/tree/secrets-policy.md").unwrap().is_some());
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

fn manifest_with(files: &[(&str, &[u8])]) -> TreeManifest {
    TreeManifest {
        schema: 1,
        branch: "f".into(),
        base_commit: "a".into(),
        head_commit: "b".into(),
        files: files
            .iter()
            .map(|(p, d)| FileEntry {
                path: p.to_string(),
                size: d.len() as u64,
                sha256: sha256_hex(d),
            })
            .collect(),
        uncommitted: vec![],
        changes: vec![],
        docs: vec![],
        has_bundle: false,
        bundle_sha256: None,
        writer: me("w", "h"),
        published_ms: 1,
    }
}

#[test]
fn fetch_refuses_git_control_files_secrets_and_case_collisions() {
    let b = setup();
    let files: &[(&str, &[u8])] = &[
        (".git/hooks/post-checkout", b"#!/bin/sh\nevil"),
        (".gitattributes", b"x"),
        ("a/.envrc", b"x"),
        ("k/prod.env", b"x"),
        ("win/a:b", b"x"),
        ("Doc.md", b"one"),
        ("doc.md", b"two"),
        ("ok.txt", b"ok"),
    ];
    for (p, d) in files {
        b.put(WT, &format!("{TREE_PREFIX}{p}"), d).unwrap();
    }
    b.put(
        WT,
        MANIFEST_KEY,
        &serde_json::to_vec(&manifest_with(files)).unwrap(),
    )
    .unwrap();
    let dest = tempfile::tempdir().unwrap();
    let r = fetch_tree(&b, WT, dest.path(), Select::All, true).unwrap();
    assert_eq!(r.written, ["Doc.md", "ok.txt"]);
    assert_eq!(r.refused.len(), 6);
    assert!(!dest.path().join(".git").exists());
}

#[test]
fn fetch_temp_file_never_clobbers_neighbours() {
    let b = setup();
    let files: &[(&str, &[u8])] = &[("foo.rs", b"new")];
    b.put(WT, &format!("{TREE_PREFIX}foo.rs"), b"new").unwrap();
    b.put(
        WT,
        MANIFEST_KEY,
        &serde_json::to_vec(&manifest_with(files)).unwrap(),
    )
    .unwrap();
    let dest = tempfile::tempdir().unwrap();
    std::fs::write(dest.path().join("foo.wt-tmp"), "precious").unwrap();
    fetch_tree(&b, WT, dest.path(), Select::All, false).unwrap();
    assert_eq!(
        std::fs::read_to_string(dest.path().join("foo.wt-tmp")).unwrap(),
        "precious"
    );
    assert_eq!(
        std::fs::read_to_string(dest.path().join("foo.rs")).unwrap(),
        "new"
    );
}

#[cfg(unix)]
#[test]
fn publish_does_not_follow_symlinked_parent_directories() {
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("passwd"), "root:x").unwrap();
    let d = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), d.path().join("docs")).unwrap();
    let b = setup();
    let ps = paths(&["docs/passwd"]);
    let src = TreeSource {
        root: d.path(),
        paths: &ps,
        uncommitted: &[],
        changes: &[],
        branch: "f",
        base_commit: "a",
        head_commit: "b",
        bundle: None,
        writer: me("w", "h"),
    };
    let r = publish_tree(&b, WT, &src, &Limits::default(), 1).unwrap();
    assert_eq!(r.uploaded, 0);
    assert!(b.get(WT, ".wt/tree/docs/passwd").unwrap().is_none());
}

#[test]
fn secret_name_gaps_are_closed_but_templates_pass() {
    for p in [
        "prod.env",
        ".envrc",
        ".git-credentials",
        ".pgpass",
        "id_ecdsa",
        "a.jks",
        "terraform.tfstate",
        ".htpasswd",
        "x/.env.local",
    ] {
        assert!(is_secret_path(p), "{p}");
    }
    for p in [".env.example", "docs/secrets-policy.md", "src/main.rs"] {
        assert!(!is_secret_path(p), "{p}");
    }
}

#[test]
fn live_claim_blocks_republish_and_rehandoff_and_merge_needs_a_claim() {
    let d = tree_dir();
    let b = setup();
    let ps = paths(&["src/lib.rs"]);
    let w = me("w", "h");
    let m = me("m", "x");
    let src = TreeSource {
        root: d.path(),
        paths: &ps,
        uncommitted: &[],
        changes: &[],
        branch: "f",
        base_commit: "a",
        head_commit: "b",
        bundle: None,
        writer: w.clone(),
    };
    publish_tree(&b, WT, &src, &Limits::default(), 1_000).unwrap();
    assert!(mark_merged(&b, WT, &m, "x", 1_001).is_err(), "no claim yet");
    claim_merge(&b, WT, &m, 2_000).unwrap();
    assert_eq!(
        publish_tree(&b, WT, &src, &Limits::default(), 2_001).unwrap_err(),
        S2Error::Claimed("m@x".into())
    );
    assert!(mark_handoff(&b, WT, &w, "again", 2_002).is_err());
    // the claim holder may still republish, and a stale claim does not block
    let mut mine = TreeSource {
        writer: m.clone(),
        ..src
    };
    mine.paths = &ps;
    publish_tree(&b, WT, &mine, &Limits::default(), 2_003).unwrap();
    mark_handoff(&b, WT, &w, "again", 2_000 + CLAIM_TTL_MS + 1).unwrap();
}

#[test]
fn client_clock_skew_cannot_hide_a_project_edit() {
    let b = MemBackend::new();
    b.set_clock(2_000); // server time at worktree creation
    // the client wrote a created_ms 10 minutes in the future
    write_meta(
        &b,
        WT,
        &WorktreeMeta {
            slug: "s2".into(),
            project: "01abc".into(),
            branch: "f".into(),
            base_commit: "a".into(),
            created_ms: 2_000 + 600_000,
        },
    )
    .unwrap();
    Overlay::new(&b, WT, PR).put("assets/x", b"mine").unwrap();
    b.set_clock(5_000); // someone edits the project 3 s later (server time)
    b.put(PR, "assets/x", b"theirs").unwrap();
    assert!(matches!(
        promote(&b, WT, PR, &ARCH, &policy(), 6_000),
        Err(S2Error::Conflict(_))
    ));
    assert_eq!(b.get(PR, "assets/x").unwrap().unwrap(), b"theirs");
}

#[test]
fn object_plus_leftover_tombstone_keeps_the_object() {
    let b = setup();
    b.set_clock(2_500);
    // simulate a put that crashed before clearing the tombstone
    Overlay::new(&b, WT, PR)
        .put("assets/old.txt", b"rewritten")
        .unwrap();
    b.put(WT, ".wt/tombstones/assets/old.txt", b"").unwrap();
    promote(&b, WT, PR, &ARCH, &policy(), 5_000).unwrap();
    assert_eq!(b.get(PR, "assets/old.txt").unwrap().unwrap(), b"rewritten");
}

#[test]
fn handoff_and_bundle_records_are_archived_on_promotion() {
    let d = tree_dir();
    let b = setup();
    let bundle = d.path().join("b.bundle");
    std::fs::write(&bundle, b"BUNDLE").unwrap();
    let ps = paths(&["src/lib.rs"]);
    let src = TreeSource {
        root: d.path(),
        paths: &ps,
        uncommitted: &[],
        changes: &[],
        branch: "f",
        base_commit: "a",
        head_commit: "b",
        bundle: Some(&bundle),
        writer: me("w", "h"),
    };
    publish_tree(&b, WT, &src, &Limits::default(), 1).unwrap();
    promote(&b, WT, PR, &ARCH, &policy(), 5_000).unwrap();
    for k in [
        ".wt/git/branch.bundle",
        ".wt/tree.manifest.json",
        ".wt/handoff.json",
    ] {
        assert!(
            b.get(AR, &format!("archive/worktrees/s2/{k}"))
                .unwrap()
                .is_some(),
            "{k}"
        );
    }
}

#[test]
fn windows_hostile_names_are_refused_on_every_platform() {
    for p in [
        "docs/CON",
        "docs/con.txt",
        "NUL.md",
        "a/COM1",
        "a/lpt9.log",
        "a/trailing.",
        "a/trailing ",
        "a/b?c",
        "a/b*c",
        "a/b|c",
        "a/b\"c",
        "a/GIT~1/config",
        "a/gitatt~1",
        "x/CONIN$",
    ] {
        assert!(is_dangerous_path(p), "{p}");
    }
    for p in [
        "docs/console.md",
        "docs/communication.md",
        "a/com0",
        "docs/auxiliary.md",
        "src/lib.rs",
        "docs/a.b.md",
    ] {
        assert!(!is_dangerous_path(p), "{p}");
    }
}

#[test]
fn conflict_detection_needs_no_clock_at_all() {
    // Server clock goes backwards and the client clock is garbage: only etags decide.
    let b = setup();
    Overlay::new(&b, WT, PR).put("assets/x", b"mine").unwrap();
    b.set_clock(1); // project edit "in the past"
    b.put(PR, "assets/x", b"theirs").unwrap();
    assert!(matches!(
        promote(&b, WT, PR, &ARCH, &policy(), 0),
        Err(S2Error::Conflict(_))
    ));
}

#[test]
fn project_edit_before_the_first_write_is_not_a_conflict() {
    let b = setup();
    b.put(PR, "assets/logo.png", b"v2-before-we-wrote").unwrap();
    Overlay::new(&b, WT, PR)
        .put("assets/logo.png", b"mine")
        .unwrap();
    promote(&b, WT, PR, &ARCH, &policy(), 5_000).unwrap();
    assert_eq!(b.get(PR, "assets/logo.png").unwrap().unwrap(), b"mine");
}

#[test]
fn key_created_in_the_project_after_our_new_key_conflicts() {
    let b = setup();
    Overlay::new(&b, WT, PR)
        .put("assets/new.bin", b"mine")
        .unwrap();
    b.put(PR, "assets/new.bin", b"theirs").unwrap();
    assert!(matches!(
        promote(&b, WT, PR, &ARCH, &policy(), 5_000),
        Err(S2Error::Conflict(_))
    ));
}

#[test]
fn delete_of_a_project_file_the_project_since_changed_conflicts() {
    let b = setup();
    Overlay::new(&b, WT, PR).delete("assets/old.txt").unwrap();
    b.put(PR, "assets/old.txt", b"changed").unwrap();
    assert!(matches!(
        promote(&b, WT, PR, &ARCH, &policy(), 5_000),
        Err(S2Error::Conflict(_))
    ));
}

#[test]
fn object_written_around_the_overlay_has_no_base_and_fails_safe() {
    let b = setup();
    b.put(WT, "assets/logo.png", b"raw").unwrap();
    assert!(matches!(
        promote(&b, WT, PR, &ARCH, &policy(), 5_000),
        Err(S2Error::Conflict(_))
    ));
}
