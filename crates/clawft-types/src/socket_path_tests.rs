use super::*;
use std::path::Path;

/// A path of exactly `n` bytes.
fn of_len(n: usize) -> PathBuf {
    PathBuf::from(format!("/{}", "a".repeat(n - 1)))
}

#[test]
fn exactly_at_the_limit_passes_both_platforms() {
    for limit in [LIMIT_MACOS, LIMIT_LINUX] {
        let p = of_len(limit - 1);
        assert_eq!(path_bytes(&p), limit - 1);
        assert!(check_with(&p, limit).is_ok(), "limit {limit}");
    }
}

#[test]
fn one_byte_over_fails_and_names_path_length_and_limit() {
    for limit in [LIMIT_MACOS, LIMIT_LINUX] {
        let p = of_len(limit);
        let e = check_with(&p, limit).unwrap_err();
        assert_eq!(e.bytes, limit);
        assert_eq!(e.limit, limit);
        assert_eq!(e.max_bytes(), limit - 1);
        let text = e.to_string();
        assert!(text.contains(&p.display().to_string()), "{text}");
        assert!(text.contains(&format!("{limit} bytes")), "{text}");
        assert!(text.contains(&format!("{} bytes", limit - 1)), "{text}");
    }
}

#[test]
fn the_two_platform_limits_differ() {
    let p = of_len(105);
    assert!(check_with(&p, LIMIT_MACOS).is_err());
    assert!(check_with(&p, LIMIT_LINUX).is_ok());
}

#[test]
fn multibyte_components_count_bytes_not_chars() {
    // 50 two-byte chars is 100 bytes plus "/" = 101; 52 chars would be 105.
    let ok = PathBuf::from(format!("/{}", "é".repeat(51))); // 1 + 102 = 103
    assert_eq!(ok.to_string_lossy().chars().count(), 52);
    assert_eq!(path_bytes(&ok), 103);
    assert!(check_with(&ok, LIMIT_MACOS).is_ok());
    let over = PathBuf::from(format!("/{}", "é".repeat(52))); // 105 bytes, 53 chars
    assert!(over.to_string_lossy().chars().count() < LIMIT_MACOS);
    let e = check_with(&over, LIMIT_MACOS).unwrap_err();
    assert_eq!(e.bytes, 105);
}

#[test]
fn relative_paths_are_measured_as_written() {
    let p = Path::new("rt/kernel.sock");
    assert_eq!(path_bytes(p), 14);
    assert!(check_with(p, LIMIT_MACOS).is_ok());
}

#[test]
fn max_root_bytes_is_right_for_the_longest_suffix() {
    let suffixes = ["kernel.sock", "child-ipc/child.sock"];
    for limit in [LIMIT_MACOS, LIMIT_LINUX] {
        let n = max_root_bytes(suffixes, limit);
        // "/" + the longest suffix (20 bytes) after the root.
        assert_eq!(n, limit - 1 - 21);
        let root = of_len(n);
        for s in suffixes {
            assert!(check_with(&root.join(s), limit).is_ok(), "{s} at {limit}");
        }
        let root = of_len(n + 1);
        assert!(check_with(&root.join("child-ipc/child.sock"), limit).is_err());
        // the shorter suffix would still fit one byte further: only the
        // longest suffix decides the maximum.
        assert!(check_with(&root.join("kernel.sock"), limit).is_ok());
    }
}

#[test]
fn check_all_reports_the_longest_offender() {
    let short = of_len(120);
    let long = of_len(130);
    let fine = of_len(10);
    let e = check_all_with([fine.as_path(), short.as_path(), long.as_path()], LIMIT_MACOS)
        .unwrap_err();
    assert_eq!(e.bytes, 130);
    assert!(check_all_with([fine.as_path()], LIMIT_MACOS).is_ok());
    assert!(check_all_with([], LIMIT_MACOS).is_ok());
}

#[test]
fn platform_limit_matches_the_os() {
    #[cfg(target_os = "macos")]
    assert_eq!(PLATFORM_LIMIT, 104);
    #[cfg(target_os = "linux")]
    assert_eq!(PLATFORM_LIMIT, 108);
}
