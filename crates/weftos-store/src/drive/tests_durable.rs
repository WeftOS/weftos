//! Stale `.drive.tmp.*` cleanup at open, and the durable index save with its two crash windows.
//!
//! The deaths are in-process (a panic at a checkpoint inside `Index::save`, on its own thread), the
//! same form as `tests_crash.rs`. They cover a process death, not a power cut: fsync order cannot
//! be observed from a test.

use std::path::Path;
use std::time::{Duration, SystemTime};

use super::index::{DIE_AT, SaveStage};
use super::tests_hardening::*;
use super::tmp::Live;
use super::*;

fn age(p: &Path) {
    let f = std::fs::OpenOptions::new().write(true).open(p).unwrap();
    f.set_modified(SystemTime::now() - Duration::from_secs(3600))
        .unwrap();
}

fn old(d: &Path, rel: &str) {
    wr(d, rel, "debris");
    age(&d.join(rel));
}

fn drive() -> (MemBackend, tempfile::TempDir) {
    (mem(), tempfile::tempdir().unwrap())
}
use crate::mem::MemBackend;

#[test]
fn old_temps_are_removed_at_open_and_fresh_ones_survive() {
    let (be, _t) = drive();
    let r = Rig::new(&be, BK, "d1");
    r.seed("a.txt", "a");
    let a = r.join("a");
    for rel in [
        ".drive.tmp.OLD1",
        "sub/.drive.tmp.OLD2",
        ".drive/cas/ab/.drive.tmp.OLD3",
    ] {
        old(&a, rel);
    }
    for rel in [
        "a.drive.tmp.notatemp",
        ".drive.tmp.FRESH",
        "sub/.drive.tmp.FRESH",
    ] {
        wr(&a, rel, "in flight");
    }
    let rep = sync_down(&be, &a).unwrap();
    assert_eq!(rep.stale_tmp_removed, 3);
    for rel in [
        ".drive.tmp.OLD1",
        "sub/.drive.tmp.OLD2",
        ".drive/cas/ab/.drive.tmp.OLD3",
    ] {
        assert!(!a.join(rel).exists(), "{rel} is stale and should be gone");
    }
    for rel in [
        "a.drive.tmp.notatemp",
        ".drive.tmp.FRESH",
        "sub/.drive.tmp.FRESH",
    ] {
        assert!(a.join(rel).exists(), "{rel} is younger than the gate");
    }
    // Materialise (first fill) is an open too.
    let b = r.join("b0");
    old(&b, ".drive.tmp.OLD");
    let rep = materialise(&be, BK, "d1", &b).unwrap();
    assert_eq!(
        (rep.stale_tmp_removed, b.join(".drive.tmp.OLD").exists()),
        (1, false)
    );
    // And the gate is an option: zero treats even a fresh temp as stale.
    let rep = sync_down_with(
        &be,
        &a,
        &Options {
            stale_tmp_age: Duration::ZERO,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(rep.stale_tmp_removed, 2);
}

#[test]
fn a_temp_this_process_is_writing_is_never_removed() {
    let (be, _t) = drive();
    let r = Rig::new(&be, BK, "d2");
    r.seed("a.txt", "a");
    let a = r.join("a");
    old(&a, "sub/.drive.tmp.LIVE");
    let guard = Live::new(&a.join("sub/.drive.tmp.LIVE"));
    let opts = Options {
        stale_tmp_age: Duration::ZERO,
        ..Default::default()
    };
    assert_eq!(sync_down_with(&be, &a, &opts).unwrap().stale_tmp_removed, 0);
    assert!(a.join("sub/.drive.tmp.LIVE").exists());
    drop(guard);
    assert_eq!(sync_down(&be, &a).unwrap().stale_tmp_removed, 1);
    assert!(!a.join("sub/.drive.tmp.LIVE").exists());
}

#[cfg(unix)]
#[test]
fn cleanup_never_follows_symlinks_or_leaves_the_drive_root() {
    use std::os::unix::fs::symlink;
    let (be, _t) = drive();
    let r = Rig::new(&be, BK, "d3");
    r.seed("a.txt", "a");
    let a = r.join("a");
    let outside = tempfile::tempdir().unwrap();
    old(outside.path(), ".drive.tmp.OUTSIDE");
    old(outside.path(), "victim.txt");
    symlink(outside.path(), a.join("linkdir")).unwrap();
    symlink(
        outside.path().join("victim.txt"),
        a.join(".drive.tmp.symfile"),
    )
    .unwrap();
    symlink(&a, a.join("loop")).unwrap();
    let opts = Options {
        stale_tmp_age: Duration::ZERO,
        ..Default::default()
    };
    assert_eq!(sync_down_with(&be, &a, &opts).unwrap().stale_tmp_removed, 0);
    assert!(
        outside.path().join(".drive.tmp.OUTSIDE").exists()
            && outside.path().join("victim.txt").exists()
    );
    for l in ["linkdir", ".drive.tmp.symfile", "loop"] {
        assert!(
            std::fs::symlink_metadata(a.join(l))
                .unwrap()
                .file_type()
                .is_symlink(),
            "{l} must stay a symlink"
        );
    }
}

/// Seed `g.txt` after `a` joined, then run `sync_down` on a thread that dies at `stage` in the
/// index save. The file was placed; the index save did not complete.
fn die_in_save(be: &MemBackend, r: &Rig<MemBackend>, a: &Path, stage: SaveStage) {
    r.seed("g.txt", "g bytes");
    let died = std::thread::scope(|sc| {
        sc.spawn(|| {
            DIE_AT.with(|d| d.set(Some(stage)));
            sync_down(be, a)
        })
        .join()
    });
    assert!(died.is_err(), "the pull was meant to die in the index save");
    assert_eq!(
        rd(a, "g.txt"),
        "g bytes",
        "the file was placed before the save"
    );
}

#[test]
fn a_death_after_the_temp_is_written_keeps_the_old_index_and_the_temp_is_cleaned_at_next_open() {
    let (be, _t) = drive();
    let r = Rig::new(&be, BK, "d4");
    r.seed("f0.txt", "f0");
    let a = r.join("a");
    die_in_save(&be, &r, &a, SaveStage::AfterTemp);

    let ix = Index::load(&a).unwrap().expect("the old index is intact");
    assert!(ix.entries.contains_key("f0.txt") && !ix.entries.contains_key("g.txt"));
    // The temp holds the complete new index (it was fsynced before the death).
    let tmp = a.join(DRIVE_DIR).join(".drive.tmp.index");
    let new: Index = serde_json::from_slice(&std::fs::read(&tmp).unwrap()).unwrap();
    assert!(new.entries.contains_key("g.txt"));

    // Age it: the next open removes it and recovers g.txt by content, with no conflict.
    age(&tmp);
    let rep = sync_down(&be, &a).unwrap();
    assert_eq!(rep.stale_tmp_removed, 1);
    assert!(
        rep.conflicts.is_empty() && rep.skipped.is_empty(),
        "{rep:?}"
    );
    assert!(!tmp.exists());
    let ix = Index::load(&a).unwrap().unwrap();
    assert!(ix.entries.contains_key("g.txt") && ix.conflicts.is_empty());
}

#[test]
fn a_death_after_the_rename_leaves_the_new_index_readable() {
    let (be, _t) = drive();
    let r = Rig::new(&be, BK, "d5");
    r.seed("f0.txt", "f0");
    let a = r.join("a");
    die_in_save(&be, &r, &a, SaveStage::AfterRename);

    let ix = Index::load(&a).unwrap().expect("the new index is readable");
    assert!(ix.entries.contains_key("g.txt") && ix.entries.contains_key("f0.txt"));
    assert!(
        !a.join(DRIVE_DIR).join(".drive.tmp.index").exists(),
        "the rename consumed the temp"
    );
    let rep = sync_down(&be, &a).unwrap();
    assert_eq!(
        (
            rep.fetched,
            rep.created.len(),
            rep.updated.len(),
            rep.stale_tmp_removed
        ),
        (0, 0, 0, 0)
    );
    assert!(rep.conflicts.is_empty());
}
