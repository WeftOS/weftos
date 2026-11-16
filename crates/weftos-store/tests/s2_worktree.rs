//! S2 tests: overlay reads and promotion on land.
use weftos_store::overlay::*;
use weftos_store::promote::*;
use weftos_store::testing::MemBackend;

#[path = "s2_worktree_common/mod.rs"]
mod common;
use common::*;

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
