//! WO-069 lane G3: `FollowState::save` on a filesystem with NO advisory locking. Every test here
//! switches all locking off through the `follower_seam` seam (calls that fail, or succeed without
//! excluding anyone), so the only thing standing between two savers and a lowered generation is
//! the verify-and-repair pass. Interleavings are driven by a hook run at the points where another
//! saver can slip in; nothing here uses a real second process.
use super::*;
use crate::follower_seam::{self as seam, Phase};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Mutex;

fn st(gens: &[(&str, u64)]) -> FollowState {
    let mut s = FollowState::default();
    for (k, g) in gens {
        s.generations.insert((*k).into(), *g);
    }
    s
}

fn gens(path: &Path) -> BTreeMap<String, u64> {
    FollowState::load(path).unwrap().generations
}

/// Locks off, in-process memory as asked, no hook.
fn no_lock_env(memory: bool) -> (tempfile::TempDir, PathBuf) {
    seam::reset();
    seam::set_no_locks(true);
    seam::set_no_memory(!memory);
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("state").join("follow.json");
    (d, p)
}

/// Run `f` once, the first time `phase` fires on this thread's next save.
fn once_at(phase: Phase, f: impl FnOnce() + 'static) {
    let f = Rc::new(Cell::new(Some(f)));
    seam::set_hook(Some(Box::new(move |p| {
        if p == phase && let Some(f) = f.take() {
            f();
        }
    })));
}

/// Replace the file the way a second process on a no-lock filesystem would: a bare rename-less
/// overwrite with `s`, no merge.
fn raw_write(path: &Path, s: &FollowState) {
    std::fs::write(path, serde_json::to_vec_pretty(s).unwrap()).unwrap();
}

#[test]
fn a_saver_with_a_stale_read_does_not_lower_what_another_wrote_after_that_read() {
    // A reads, B reads, B writes 9, A writes its stale 5. Memory off: two processes.
    let (_d, path) = no_lock_env(false);
    let p = path.clone();
    once_at(Phase::AfterRead, move || st(&[("x", 9)]).save(&p).unwrap());
    st(&[("x", 5)]).save(&path).unwrap();
    assert_eq!(gens(&path)["x"], 9);
}

#[test]
fn a_rename_that_lands_after_ours_with_an_older_view_is_repaired() {
    // We write 9; before we read it back someone else's older view replaces the file.
    let (_d, path) = no_lock_env(false);
    let p = path.clone();
    once_at(Phase::AfterRename, move || raw_write(&p, &st(&[("x", 5)])));
    st(&[("x", 9)]).save(&path).unwrap();
    assert_eq!(gens(&path)["x"], 9);
}

#[test]
fn the_gap_between_the_last_check_and_the_rename_is_closed_in_process_only() {
    // B completes inside A's one unguarded gap. Same process: the witness exposes it and A repairs.
    let (_d, path) = no_lock_env(true);
    let p = path.clone();
    once_at(Phase::BeforeRename, move || st(&[("x", 9)]).save(&p).unwrap());
    st(&[("x", 5)]).save(&path).unwrap();
    assert_eq!(gens(&path)["x"], 9, "in one process a completed save is never lowered");

    // Two processes share no memory, so nothing records B's 9 once A's rename erased it. This pins
    // the documented residual (see the invariant comment in follower_save.rs): if a later change
    // closes it, this assertion flips and the comment and ADR text must change with it.
    let (_d2, path2) = no_lock_env(false);
    let p2 = path2.clone();
    once_at(Phase::BeforeRename, move || st(&[("x", 9)]).save(&p2).unwrap());
    st(&[("x", 5)]).save(&path2).unwrap();
    assert_eq!(gens(&path2)["x"], 5, "cross-process residual: one syscall wide, lost until the next save");
}

fn views() -> [FollowState; 3] {
    [
        st(&[("a", 300), ("b", 1)]),
        st(&[("a", 100), ("b", 50)]),
        st(&[("a", 200), ("c", 7)]),
    ]
}

/// Saver `order[0]` starts; at `phase` it lets `order[1]` run a whole save, which in turn lets
/// `order[2]` run one at the same point, so the innermost finishes first.
fn nest(order: &[usize], phase: Phase, path: &Path) {
    let Some((&first, rest)) = order.split_first() else { return };
    if !rest.is_empty() {
        let (rest, p) = (rest.to_vec(), path.to_path_buf());
        once_at(phase, move || nest(&rest, phase, &p));
    }
    views()[first].save(path).unwrap();
}

#[test]
fn three_savers_in_every_order_leave_the_maximum_per_slug_without_any_lock() {
    for (phase, memory) in [(Phase::AfterRead, false), (Phase::AfterRead, true), (Phase::BeforeRename, true)] {
        for order in [[0, 1, 2], [0, 2, 1], [1, 0, 2], [1, 2, 0], [2, 0, 1], [2, 1, 0]] {
            let (_d, path) = no_lock_env(memory);
            nest(&order, phase, &path);
            let got = gens(&path);
            assert_eq!(
                (got["a"], got["b"], got["c"]),
                (300, 50, 7),
                "order {order:?} at {phase:?}, memory {memory}"
            );
        }
    }
}

#[test]
fn eight_threads_with_no_locking_never_lower_a_completed_generation() {
    for run in 0..20 {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("follow.json");
        let completed: Mutex<BTreeMap<String, u64>> = Mutex::new(BTreeMap::new());
        let done = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|s| {
            let (path, completed, done) = (&path, &completed, &done);
            let checker = s.spawn(move || {
                let mut looks = 0u32;
                while !done.load(std::sync::atomic::Ordering::Acquire) {
                    // Snapshot what has completed, then look at the file: it may not be lower.
                    let floor = completed.lock().unwrap().clone();
                    if !floor.is_empty() {
                        let got = FollowState::load(path).map(|s| s.generations).unwrap_or_default();
                        for (slug, g) in &floor {
                            assert!(
                                got.get(slug).is_some_and(|x| x >= g),
                                "run {run}: {slug} on disk {:?} < completed {g}",
                                got.get(slug)
                            );
                        }
                        looks += 1;
                    }
                    std::thread::yield_now();
                }
                looks
            });
            let workers: Vec<_> = (0..8u64)
                .map(|t| {
                    s.spawn(move || {
                        seam::reset();
                        seam::set_no_locks(true);
                        for i in 0..40u64 {
                            let mut v = st(&[(&format!("t{t}"), i)]);
                            v.generations.insert(format!("s{}", i % 3), t * 1000 + i);
                            v.save(path).unwrap();
                            let mut c = completed.lock().unwrap();
                            for (k, g) in &v.generations {
                                let e = c.entry(k.clone()).or_insert(0);
                                *e = (*e).max(*g);
                            }
                        }
                    })
                })
                .collect();
            for w in workers {
                w.join().unwrap();
            }
            done.store(true, std::sync::atomic::Ordering::Release);
            assert!(checker.join().unwrap() > 0, "the checker looked at least once");
        });
        let got = gens(&path);
        for (slug, g) in completed.lock().unwrap().iter() {
            assert_eq!(got[slug], *g, "run {run}: final {slug}");
        }
    }
}

#[test]
fn a_file_corrupted_or_deleted_between_our_write_and_the_read_back_is_rewritten() {
    for damage in ["truncated", "empty", "garbage", "deleted"] {
        let (_d, path) = no_lock_env(false);
        let p = path.clone();
        once_at(Phase::AfterRename, move || match damage {
            "truncated" => std::fs::write(&p, b"{\"seen\":{},\"gener").unwrap(),
            "empty" => std::fs::write(&p, b"").unwrap(),
            "garbage" => std::fs::write(&p, [0xff, 0xfe, 0x00]).unwrap(),
            _ => std::fs::remove_file(&p).unwrap(),
        });
        st(&[("x", 9), ("y", 2)]).save(&path).unwrap();
        let got = gens(&path);
        assert_eq!((got["x"], got["y"]), (9, 2), "{damage}");
    }
}

#[test]
fn when_the_file_keeps_being_lowered_save_says_so_and_a_later_save_repairs_it() {
    let (_d, path) = no_lock_env(false);
    let p = path.clone();
    seam::set_hook(Some(Box::new(move |ph| {
        if ph == Phase::AfterRename {
            raw_write(&p, &st(&[("x", 1)]));
        }
    })));
    let err = st(&[("x", 9)]).save(&path).expect_err("never stable");
    let msg = err.to_string();
    assert!(msg.contains("could not be made stable"), "{msg}");
    assert!(msg.contains("save again"), "{msg}");
    assert_eq!(gens(&path)["x"], 1, "the hostile writer won the last round");
    seam::set_hook(None);
    st(&[("x", 9)]).save(&path).unwrap();
    assert_eq!(gens(&path)["x"], 9);
}

#[test]
fn an_uncontended_save_costs_one_merge_read_one_write_and_one_read_back() {
    seam::reset();
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("follow.json");
    st(&[("x", 1)]).save(&path).unwrap();
    seam::reset_counts();
    st(&[("x", 2)]).save(&path).unwrap();
    assert_eq!(seam::counts(), (2, 1), "(reads, writes) with the real locks");
    assert_eq!(gens(&path)["x"], 2);
}

#[test]
fn real_locking_is_unchanged_a_stale_saver_cannot_lower() {
    seam::reset();
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("follow.json");
    st(&[("x", 9)]).save(&path).unwrap();
    st(&[("x", 5), ("y", 1)]).save(&path).unwrap();
    let got = gens(&path);
    assert_eq!((got["x"], got["y"]), (9, 1));
}

#[test]
fn u64_edges_and_slugs_we_do_not_hold() {
    let (_d, path) = no_lock_env(false);
    st(&[("max", u64::MAX), ("zero", 0), ("only_disk", 4)]).save(&path).unwrap();
    st(&[("max", 3), ("zero", 0), ("new", u64::MAX - 1)]).save(&path).unwrap();
    let got = gens(&path);
    assert_eq!(got["max"], u64::MAX);
    assert_eq!(got["zero"], 0);
    assert_eq!(got["only_disk"], 4, "a slug on disk we do not hold is kept");
    assert_eq!(got["new"], u64::MAX - 1);
}

#[test]
fn seen_stays_last_writer_wins_while_generations_merge() {
    let (_d, path) = no_lock_env(true);
    let seen = |b: &str| Seen { etag: None, bucket: b.into(), head_commit: "h".into(), state: State::Open };
    let mut first = st(&[("x", 9)]);
    first.seen.insert("one".into(), seen("b1"));
    first.save(&path).unwrap();
    let mut second = st(&[("x", 2)]);
    second.seen.insert("two".into(), seen("b2"));
    second.save(&path).unwrap();
    let got = FollowState::load(&path).unwrap();
    assert_eq!(got.generations["x"], 9);
    assert_eq!(got.seen.keys().collect::<Vec<_>>(), ["two"], "seen is not merged");
}
