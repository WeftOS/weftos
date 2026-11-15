//! WO-069 lane Y (item 4): `FollowState::save` keeps its merge locked where the state directory
//! cannot be locked (Windows cannot open a directory as a file; some filesystems refuse a directory
//! lock), by locking a sibling `.<name>.lock` file instead. The directory lock is switched off here
//! through `save_locked(.., false)`, the seam `save` itself uses with `true`.
use super::*;

fn with_gen(slug: &str, g: u64) -> FollowState {
    let mut s = FollowState::default();
    s.generations.insert(slug.into(), g);
    s
}

fn dir_files(d: &Path) -> Vec<String> {
    let mut v: Vec<String> =
        std::fs::read_dir(d).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    v.sort();
    v
}

#[test]
fn without_a_directory_lock_many_threads_saving_at_once_never_lose_the_highest_generation() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("state").join("follow.json");
    std::thread::scope(|s| {
        for t in 0..8u64 {
            let path = &path;
            s.spawn(move || {
                for i in 0..40u64 {
                    let mut st = with_gen(&format!("t{t}"), i);
                    st.generations.insert("shared".into(), t * 1000 + i);
                    st.save_locked(path, false).unwrap();
                }
            });
        }
    });
    let got = FollowState::load(&path).unwrap().generations;
    for t in 0..8u64 {
        assert_eq!(got[&format!("t{t}")], 39, "thread {t}'s last generation survived every other saver");
    }
    assert_eq!(got["shared"], 7039);
    // the lock file stays (an empty file; the OS lock on it is released when its holder exits)
    assert_eq!(dir_files(path.parent().unwrap()), [".follow.json.lock", "follow.json"]);
}

#[test]
fn without_a_directory_lock_three_savers_in_every_order_leave_the_maximum_per_slug() {
    let views = [
        { let mut s = with_gen("a", 300); s.generations.insert("b".into(), 1); s },
        { let mut s = with_gen("a", 100); s.generations.insert("b".into(), 50); s },
        { let mut s = with_gen("a", 200); s.generations.insert("c".into(), 7); s },
    ];
    for order in [[0, 1, 2], [0, 2, 1], [1, 0, 2], [1, 2, 0], [2, 0, 1], [2, 1, 0]] {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("follow.json");
        for i in order {
            views[i].save_locked(&path, false).unwrap();
        }
        let got = FollowState::load(&path).unwrap().generations;
        assert_eq!((got["a"], got["b"], got["c"]), (300, 50, 7), "order {order:?}");
    }
}

#[cfg(unix)]
#[test]
fn with_a_directory_lock_no_lock_file_is_made() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("follow.json");
    with_gen("a", 1).save(&path).unwrap();
    assert_eq!(dir_files(d.path()), ["follow.json"], "the directory lock is used where it works (unix)");
}
