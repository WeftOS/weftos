//! F14: publish reads one open handle, refuses a path that is or became a symlink or another file,
//! and cannot be made to read past the size cap by a file that grows after the stat.
use super::*;

fn open(p: &Path) -> std::fs::File {
    std::fs::File::open(p).unwrap()
}

#[test]
fn a_regular_file_is_read_whole() {
    let d = tempfile::tempdir().unwrap();
    let f = d.path().join("a.txt");
    std::fs::write(&f, b"hello").unwrap();
    assert_eq!(read_regular(&f, 5).unwrap(), b"hello");
}

#[cfg(unix)]
#[test]
fn a_symlink_is_never_followed_even_if_it_appears_after_the_checks() {
    let d = tempfile::tempdir().unwrap();
    let (real, secret, link) = (d.path().join("real.txt"), d.path().join("outside.txt"), d.path().join("link.txt"));
    std::fs::write(&real, b"ok").unwrap();
    std::fs::write(&secret, b"TOP SECRET").unwrap();
    std::os::unix::fs::symlink(&secret, &link).unwrap();
    // swapped in for the path before the open
    assert_eq!(read_regular(&link, 100), Err(Some(Skip::Symlink)));
    // swapped in for the path after the open: the handle is on the file the path no longer names
    let h = open(&real);
    std::fs::remove_file(&real).unwrap();
    std::os::unix::fs::symlink(&secret, &real).unwrap();
    assert_eq!(read_open_regular(h, &real, 100), Err(Some(Skip::Symlink)));
    // replaced by a different regular file between the open and the check
    let h = open(&secret);
    std::fs::write(d.path().join("swap.txt"), b"other").unwrap();
    std::fs::rename(d.path().join("swap.txt"), &secret).unwrap();
    assert_eq!(read_open_regular(h, &secret, 100), Err(Some(Skip::Changed)));
}

#[test]
fn a_file_over_the_cap_is_refused_and_a_missing_one_is_named() {
    let d = tempfile::tempdir().unwrap();
    let f = d.path().join("big.bin");
    std::fs::write(&f, vec![7u8; 11]).unwrap();
    assert_eq!(read_regular(&f, 10), Err(Some(Skip::TooLarge)));
    assert_eq!(read_regular(&d.path().join("gone"), 10), Err(Some(Skip::Missing)));
}
