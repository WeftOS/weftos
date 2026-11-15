//! S2 tests: claims and handoff, clock-free conflict detection, and archiving of overwritten project versions.
use weftos_store::overlay::*;
use weftos_store::promote::*;
use weftos_store::share::*;
use weftos_store::testing::MemBackend;

#[path = "s2_worktree_common/mod.rs"]
mod common;
use common::*;

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
        changes_recorded: false,
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
        changes_recorded: false,
        branch: "f",
        base_commit: "a",
        head_commit: "b",
        bundle: Some(&bundle),
        writer: me("w", "h"),
    };
    publish_tree(&b, WT, &src, &Limits::default(), 1).unwrap();
    promote(&b, WT, PR, &ARCH, &policy(), 5_000).unwrap();
    let bundle_key = bundle_obj(&b);
    for k in [
        bundle_key.as_str(),
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

#[test]
fn an_edit_based_on_a_read_is_compared_with_what_was_read() {
    let b = setup();
    let ov = Overlay::new(&b, WT, PR);
    b.put(PR, "assets/shared.md", b"v1").unwrap();
    let seen = ov.get("assets/shared.md").unwrap().unwrap(); // the worktree reads v1 ...
    b.put(PR, "assets/shared.md", b"v2-from-teammate").unwrap(); // ... a teammate lands v2 ...
    ov.put("assets/shared.md", &[seen.as_slice(), b"+edit"].concat()).unwrap(); // ... then we edit v1
    let e = promote(&b, WT, PR, &ARCH, &policy(), 5_000).unwrap_err();
    assert_eq!(e, S2Error::Conflict(vec!["assets/shared.md".into()]));
}

#[test]
fn a_project_side_delete_of_an_edited_file_is_a_conflict() {
    let b = setup();
    let ov = Overlay::new(&b, WT, PR);
    b.put(PR, "assets/a.png", b"v1").unwrap();
    ov.put("assets/a.png", b"mine").unwrap();
    b.delete(PR, "assets/a.png").unwrap();
    let e = promote(&b, WT, PR, &ARCH, &policy(), 5_000).unwrap_err();
    assert_eq!(e, S2Error::Conflict(vec!["assets/a.png".into()]));
}

#[test]
fn overwritten_and_deleted_project_versions_are_archived() {
    let b = setup();
    let ov = Overlay::new(&b, WT, PR);
    b.put(PR, "assets/x.bin", b"old-x").unwrap();
    b.put(PR, "assets/y.bin", b"old-y").unwrap();
    ov.put("assets/x.bin", b"new-x").unwrap();
    ov.delete("assets/y.bin").unwrap();
    promote(&b, WT, PR, &ARCH, &policy(), 5_000).unwrap();
    let pre = "archive/worktrees/s2/.wt/displaced/assets";
    assert_eq!(b.get(AR, &format!("{pre}/x.bin")).unwrap().unwrap(), b"old-x");
    assert_eq!(b.get(AR, &format!("{pre}/y.bin")).unwrap().unwrap(), b"old-y");
}
