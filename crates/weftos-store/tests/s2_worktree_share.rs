//! S2 tests: publishing a worktree tree, fetching it on a second machine, and the size limits.
use weftos_store::overlay::*;
use weftos_store::share::*;
use weftos_store::share_fetch::*;

#[path = "s2_worktree_common/mod.rs"]
mod common;
use common::*;

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
        changes_recorded: false,
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
            changes_recorded: false,
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
    let lib = obj(&b, "src/lib.rs");
    let ps2 = paths(&["docs/grok-build-fork/rooms.md"]);
    let r = publish_tree(&b, WT, &mk(d.path(), &ps2, "b3"), &Limits::default(), 8_000).unwrap();
    // lane D3: the dropped file is retired (kept for the grace), then swept by a later publish
    assert_eq!((r.removed, r.retired), (0, 1));
    assert!(b.get(WT, &lib).unwrap().is_some());
    assert_eq!(read_manifest(&b, WT).unwrap().unwrap().files.len(), 1);
    let r = publish_tree(&b, WT, &mk(d.path(), &ps2, "b3"), &Limits::default(), 8_000 + DEFAULT_GRACE_MS).unwrap();
    assert!(r.removed >= 1, "{r:?}");
    assert!(b.get(WT, &lib).unwrap().is_none());
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
        changes_recorded: false,
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
        changes_recorded: false,
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
        changes_recorded: false,
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
    b.put(WT, &obj(&b, "src/lib.rs"), b"tampered").unwrap();
    let fresh = tempfile::tempdir().unwrap();
    assert!(fetch_tree(&b, WT, fresh.path(), Select::All, false).is_err());
}

#[test]
fn tampered_manifest_cannot_escape_dest() {
    let b = setup();
    let m = TreeManifest {
        schema: 1,
        generation: 0,
        branch: "f".into(),
        base_commit: "a".into(),
        head_commit: "b".into(),
        files: vec![FileEntry {
            path: "../evil".into(),
            size: 1,
            sha256: sha256_hex(b"x"),
            generation: None,
        }],
        uncommitted: vec![],
        changes: vec![],
        docs: vec![],
        has_bundle: false,
        bundle_sha256: None,
        changes_recorded: false,
        bundle_gen: None,
        retiring: vec![],
        v1_retired_ms: None,
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
        changes_recorded: false,
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
