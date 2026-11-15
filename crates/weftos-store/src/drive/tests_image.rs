//! The image profile over a synthetic minimal rootfs built in a tempdir.

use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

use super::image::*;

fn rootfs() -> tempfile::TempDir {
    let t = tempfile::tempdir().unwrap();
    let r = t.path();
    std::fs::create_dir_all(r.join("usr/bin")).unwrap();
    std::fs::create_dir_all(r.join("etc")).unwrap();
    std::fs::write(r.join("usr/bin/sh"), b"#!sh\n").unwrap();
    std::fs::set_permissions(r.join("usr/bin/sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(r.join("usr/bin/su"), b"su").unwrap();
    std::fs::set_permissions(r.join("usr/bin/su"), std::fs::Permissions::from_mode(0o4755)).unwrap();
    std::fs::hard_link(r.join("usr/bin/sh"), r.join("usr/bin/bash")).unwrap();
    symlink("/usr/bin", r.join("bin")).unwrap();
    symlink("../../../../etc/shadow", r.join("etc/escape")).unwrap();
    symlink("../usr/bin/sh", r.join("etc/inside")).unwrap();
    t
}

fn find<'a>(v: &'a [ImageEntry], p: &str) -> &'a ImageEntry {
    v.iter().find(|e| e.path == p).unwrap_or_else(|| panic!("no entry {p}"))
}

#[test]
fn capture_classifies_each_kind() {
    let t = rootfs();
    let v = capture(t.path(), &ImageProfile::default()).unwrap();
    assert_eq!(find(&v, "bin").kind, ImageKind::Symlink { target: "/usr/bin".into() });
    assert_eq!(find(&v, "usr/bin/su").mode, 0o755, "setuid stripped by default");
    assert_eq!(find(&v, "usr/bin/sh").mode, 0o755);
    // Entries are sorted, so `bash` is the first name and `sh` links to it.
    assert_eq!(find(&v, "usr/bin/sh").kind, ImageKind::HardLink { to: "usr/bin/bash".into() });
    assert_eq!(find(&v, "usr/bin/bash").kind, ImageKind::File { size: 5 });
}

#[test]
fn setid_kept_only_by_explicit_rule() {
    let t = rootfs();
    let v = capture(t.path(), &ImageProfile { setid: SetidRule::Keep }).unwrap();
    assert_eq!(find(&v, "usr/bin/su").mode, 0o4755);
    assert_eq!(image_mode(0o6755, SetidRule::Strip), 0o755);
    assert_eq!(image_mode(0o1777, SetidRule::Strip), 0o1777, "sticky survives");
}

#[test]
fn special_file_is_skipped_not_carried() {
    // A unix socket inode is a non-regular, non-dir, non-symlink entry creatable without root;
    // it takes the same branch as a device node or FIFO.
    let t = rootfs();
    let _sock = std::os::unix::net::UnixListener::bind(t.path().join("etc/sock")).unwrap();
    let v = capture(t.path(), &ImageProfile::default()).unwrap();
    assert!(matches!(find(&v, "etc/sock").kind, ImageKind::Skipped { .. }));
    let dst = tempfile::tempdir().unwrap();
    let rep = materialise(t.path(), &v, dst.path(), &ImageProfile::default()).unwrap();
    assert_eq!(rep.skipped.len(), 1);
    assert!(!dst.path().join("etc/sock").exists());
}

#[test]
fn round_trip_keeps_attributes() {
    let src = rootfs();
    let p = ImageProfile::default();
    let v = capture(src.path(), &p).unwrap();
    let dst = tempfile::tempdir().unwrap();
    let rep = materialise(src.path(), &v, dst.path(), &p).unwrap();
    assert!(rep.placed >= 6);
    let d = dst.path();
    assert_eq!(std::fs::read_link(d.join("bin")).unwrap().to_str().unwrap(), "/usr/bin");
    let su = std::fs::metadata(d.join("usr/bin/su")).unwrap();
    assert_eq!(su.mode() & 0o7777, 0o755);
    let (sh, bash) = (std::fs::metadata(d.join("usr/bin/sh")).unwrap(), std::fs::metadata(d.join("usr/bin/bash")).unwrap());
    assert_eq!(sh.ino(), bash.ino(), "hard link preserved");
    assert_eq!(sh.nlink(), 2);
    assert_eq!(std::fs::read(d.join("usr/bin/sh")).unwrap(), b"#!sh\n");
}

#[test]
fn escaping_symlinks_resolve_inside_root() {
    assert_eq!(confine_link("etc/escape", "../../../../etc/shadow"), "etc/shadow");
    assert_eq!(confine_link("etc/x", "/../../root"), "root");
    assert_eq!(confine_link("etc/inside", "../usr/bin/sh"), "usr/bin/sh");
    let src = rootfs();
    let p = ImageProfile::default();
    let v = capture(src.path(), &p).unwrap();
    let dst = tempfile::tempdir().unwrap();
    materialise(src.path(), &v, dst.path(), &p).unwrap();
    let esc = std::fs::read_link(dst.path().join("etc/escape")).unwrap();
    assert_eq!(esc.to_str().unwrap(), "/etc/shadow", "rewritten to the clamped in-image form");
    let ins = std::fs::read_link(dst.path().join("etc/inside")).unwrap();
    assert_eq!(ins.to_str().unwrap(), "../usr/bin/sh", "non-escaping relative link untouched");
}

#[test]
fn hostile_manifest_cannot_write_through_a_symlink() {
    let src = tempfile::tempdir().unwrap();
    std::fs::write(src.path().join("evil"), b"x").unwrap();
    let outside = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let link = ImageEntry { path: "lnk".into(), mode: 0o777, kind: ImageKind::Symlink { target: outside.path().to_string_lossy().into() } };
    let file = ImageEntry { path: "lnk/evil".into(), mode: 0o644, kind: ImageKind::File { size: 1 } };
    let r = materialise(src.path(), &[link, file], dst.path(), &ImageProfile::default());
    assert!(r.is_err());
    assert!(!outside.path().join("evil").exists());
    for bad in ["../x", "/abs", "a/../b", ""] {
        let e = ImageEntry { path: bad.into(), mode: 0o644, kind: ImageKind::Dir };
        assert!(materialise(src.path(), &[e], dst.path(), &ImageProfile::default()).is_err(), "{bad}");
    }
}
