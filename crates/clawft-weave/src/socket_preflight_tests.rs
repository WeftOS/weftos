use super::*;
use clawft_types::socket_path::{LIMIT_LINUX, LIMIT_MACOS};

/// A runtime root of exactly `n` bytes.
fn root_of(n: usize) -> RuntimePaths {
    RuntimePaths::at(format!("/{}", "r".repeat(n - 1)))
}

#[test]
fn lists_every_socket_the_daemon_binds() {
    let p = RuntimePaths::at("/run/x");
    let plain = daemon_socket_paths(&p, false);
    assert_eq!(
        plain,
        vec![PathBuf::from("/run/x/kernel.sock"), PathBuf::from("/run/x/cogbus.sock")]
    );
    let user = daemon_socket_paths(&p, true);
    assert_eq!(user.len(), 3);
    assert_eq!(user[2], PathBuf::from("/run/x/child-ipc/child.sock"));
}

#[test]
fn max_runtime_dir_is_set_by_the_longest_suffix() {
    let p = RuntimePaths::at("/x");
    // user profile: "child-ipc/child.sock" (20) + "/" -> limit - 1 - 21
    assert_eq!(max_runtime_dir_bytes(&p, true, LIMIT_MACOS), 103 - 21);
    assert_eq!(max_runtime_dir_bytes(&p, true, LIMIT_LINUX), 107 - 21);
    // plain: "kernel.sock" / "cogbus.sock" (11) + "/" -> limit - 1 - 12
    assert_eq!(max_runtime_dir_bytes(&p, false, LIMIT_MACOS), 103 - 12);
    assert_eq!(max_runtime_dir_bytes(&p, false, LIMIT_LINUX), 107 - 12);
}

#[test]
fn the_computed_maximum_passes_and_one_more_byte_fails_on_both_platforms() {
    for limit in [LIMIT_MACOS, LIMIT_LINUX] {
        for user in [false, true] {
            let max = max_runtime_dir_bytes(&root_of(5), user, limit);
            assert!(preflight_with(&root_of(max), user, limit).is_ok(), "limit {limit} user {user}");
            let e = preflight_with(&root_of(max + 1), user, limit).unwrap_err().to_string();
            assert!(e.contains("WEFTOS_RUNTIME_DIR"), "{e}");
        }
    }
}

#[test]
fn the_daemon_profile_decides_which_suffix_is_checked() {
    // fits for the plain daemon, overflows only the user daemon's child socket
    let limit = LIMIT_MACOS;
    let n = max_runtime_dir_bytes(&root_of(5), false, limit);
    assert!(preflight_with(&root_of(n), false, limit).is_ok());
    let e = preflight_with(&root_of(n), true, limit).unwrap_err().to_string();
    assert!(e.contains("child-ipc/child.sock"), "{e}");
}

#[test]
fn the_message_names_path_length_limit_and_the_fix() {
    let p = root_of(95);
    let e = preflight_with(&p, true, LIMIT_MACOS).unwrap_err().to_string();
    assert!(e.contains(&p.root().join("child-ipc/child.sock").display().to_string()), "{e}");
    assert!(e.contains("116 bytes"), "path length: {e}");
    assert!(e.contains("103 bytes"), "limit: {e}");
    assert!(e.contains("104 bytes including the terminator"), "{e}");
    assert!(e.contains("use a shorter runtime directory"), "{e}");
    assert!(e.contains("at most 82 bytes"), "suggested maximum: {e}");
    assert!(e.contains("is 95 bytes"), "current root: {e}");
}

#[test]
fn it_reports_the_longest_offender() {
    // kernel.sock overflows too, but child.sock is longer and names the worst case
    let e = preflight_with(&root_of(100), true, LIMIT_MACOS).unwrap_err().to_string();
    assert!(e.contains("child-ipc/child.sock"), "{e}");
}

#[test]
fn a_failing_check_creates_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let mut deep = tmp.path().to_path_buf();
    while deep.as_os_str().len() < 120 {
        deep.push("d".repeat(20));
    }
    let p = RuntimePaths::at(&deep);
    assert!(preflight_with(&p, true, LIMIT_MACOS).is_err());
    assert!(!deep.exists(), "the check must not create the runtime dir");
    assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0, "nothing under the temp dir");
}
