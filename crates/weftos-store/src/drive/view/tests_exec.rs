//! The execute bit through a scoped view: stored in the object's attributes on put and publish,
//! applied when the view materialises (base 0555/0444, work 0755/0644). Objects with no attribute
//! (all data from before it existed) stay 0444 / 0644.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use super::tests::{B, files, probe, rd, rw, spec, tmp, wr};
use super::*;
use crate::backend::{ObjectAttrs, ObjectBackend};
use crate::drive::ChangeKind;

const EXEC: ObjectAttrs = ObjectAttrs { executable: true };

fn mode(p: &Path) -> u32 {
    std::fs::symlink_metadata(p).unwrap().permissions().mode() & 0o777
}

fn chmod(p: &Path, m: u32) {
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap();
}

fn exec_of(s: &super::tests::Probe, key: &str) -> bool {
    s.inner.attrs(B, key).unwrap().executable
}

/// Open a fresh read-write view on `root`.
fn reopen(s: &super::tests::Probe, root: &Path) -> ScopedView {
    ScopedView::open(s, &rw(WorkStart::Clone), root).unwrap().0
}

#[test]
fn a_put_executable_lands_in_the_base_as_0555_and_a_plain_object_as_0444() {
    let s = probe();
    s.inner.put_attrs(B, "docs/bin/run.sh", b"#!/bin/sh\n", false, EXEC).unwrap();
    s.inner.put_attrs(B, "docs/bin/tool", b"elf", false, EXEC).unwrap();
    let (_d, t) = tmp();
    let root = t.join("v");
    ScopedView::open(&s, &rw(WorkStart::Clone), &root).unwrap();
    let base = root.join("base/docs");
    assert_eq!(mode(&base.join("bin/run.sh")), 0o555);
    assert_eq!(mode(&base.join("bin/tool")), 0o555);
    assert_eq!(mode(&base.join("bin")), 0o555);
    assert_eq!(mode(&base.join("readme.md")), 0o444, "no attribute is not executable");
    assert!(std::fs::write(base.join("bin/run.sh"), "x").is_err(), "no write bit on an executable base file");
}

#[test]
fn a_legacy_object_with_no_attribute_is_0444_in_base_and_0644_in_work() {
    // `probe()` seeds with the plain `put`, which carries no attribute: the shape of every object
    // that was stored before the attribute existed.
    let s = probe();
    assert!(!exec_of(&s, "docs/readme.md") && !exec_of(&s, "proj/a.txt"));
    let (_d, t) = tmp();
    let root = t.join("v");
    ScopedView::open(&s, &rw(WorkStart::Clone), &root).unwrap();
    assert_eq!(mode(&root.join("base/docs/readme.md")), 0o444);
    assert_eq!(mode(&root.join("base/docs/guide/intro.md")), 0o444);
    assert_eq!(mode(&root.join("work/a.txt")), 0o644);
    assert_eq!(mode(&root.join("work/dir/b.txt")), 0o644);
}

#[test]
fn an_exec_bit_set_in_work_is_recorded_on_publish_and_applied_on_reopen() {
    let s = probe();
    let (_d, t) = tmp();
    let root = t.join("v");
    let (mut v, _) = ScopedView::open(&s, &rw(WorkStart::Clone), &root).unwrap();
    // A chmod alone (same bytes) is a change, and a new executable file is a create.
    chmod(&root.join("work/a.txt"), 0o755);
    wr(&root.join("work/new.sh"), "#!/bin/sh\n");
    chmod(&root.join("work/new.sh"), 0o755);
    wr(&root.join("work/plain.txt"), "plain");
    let pend = v.status().unwrap().pending;
    assert!(pend.contains(&("a.txt".to_owned(), ChangeKind::Modify)), "{pend:?}");
    let p = v.publish(&s).unwrap();
    assert!(p.open_conflicts == 0 && p.up.conflicts.is_empty(), "{p:?}");
    assert_eq!(s.text("proj/a.txt").as_deref(), Some("alpha"), "same bytes");
    assert!(exec_of(&s, "proj/a.txt"), "the chmod reached the object's attributes");
    assert!(exec_of(&s, "proj/new.sh"));
    assert!(!exec_of(&s, "proj/plain.txt"));
    assert!(v.status().unwrap().pending.is_empty(), "nothing is left pending after a publish");
    v.close(&s, CloseMode::Discard).unwrap();

    // Reopen on a new root: the stored attribute decides the mode.
    let root2 = t.join("v2");
    let v2 = reopen(&s, &root2);
    assert_eq!(mode(&root2.join("work/a.txt")), 0o755);
    assert_eq!(mode(&root2.join("work/new.sh")), 0o755);
    assert_eq!(mode(&root2.join("work/plain.txt")), 0o644);
    assert_eq!(mode(&root2.join("work/d.txt")), 0o644);
    assert!(v2.status().unwrap().pending.is_empty(), "a freshly opened view has no pending changes");

    // And back: clearing the bit is a change too, and clears the attribute.
    let mut v2 = v2;
    chmod(&root2.join("work/a.txt"), 0o644);
    assert!(v2.status().unwrap().pending.contains(&("a.txt".to_owned(), ChangeKind::Modify)));
    v2.publish(&s).unwrap();
    assert!(!exec_of(&s, "proj/a.txt"));
    v2.close(&s, CloseMode::Discard).unwrap();
    let root3 = t.join("v3");
    reopen(&s, &root3);
    assert_eq!(mode(&root3.join("work/a.txt")), 0o644);
}

#[test]
fn a_published_executable_is_0555_in_another_views_base() {
    let s = probe();
    let (_d, t) = tmp();
    // A work prefix that is also a base include of the next view.
    let root = t.join("v");
    let (v, _) = ScopedView::open(&s, &rw(WorkStart::Clone), &root).unwrap();
    wr(&root.join("work/bin/hello"), "#!/bin/sh\n");
    chmod(&root.join("work/bin/hello"), 0o755);
    v.close(&s, CloseMode::Publish).unwrap();
    let ro = spec(vec![Include { prefix: "proj".into(), at: "".into() }], None);
    let root2 = t.join("ro");
    ScopedView::open(&s, &ro, &root2).unwrap();
    assert_eq!(mode(&root2.join("base/bin/hello")), 0o555);
    assert_eq!(mode(&root2.join("base/a.txt")), 0o444);
}

#[test]
fn an_overlay_work_tree_publishes_an_exec_change_too() {
    let s = probe();
    s.inner.put_attrs(B, "proj/run.sh", b"#!/bin/sh\n", false, EXEC).unwrap();
    let (_d, t) = tmp();
    let root = t.join("v");
    let (mut v, _) = ScopedView::open(&s, &rw(WorkStart::Empty), &root).unwrap();
    assert_eq!(mode(&root.join("lower/run.sh")), 0o755, "the overlay lower is writable and keeps the execute bits");
    assert_eq!(mode(&root.join("lower/a.txt")), 0o644);
    // What overlayfs leaves in the upper: a copy-up of a.txt with the bit set, and a new script.
    wr(&root.join("work/a.txt"), "alpha");
    chmod(&root.join("work/a.txt"), 0o755);
    wr(&root.join("work/new.sh"), "#!/bin/sh\n");
    chmod(&root.join("work/new.sh"), 0o755);
    v.publish(&s).unwrap();
    assert!(exec_of(&s, "proj/a.txt") && exec_of(&s, "proj/new.sh") && exec_of(&s, "proj/run.sh"));
}

#[test]
fn a_chmod_on_a_base_file_is_reported_as_tampering() {
    let s = probe();
    let (_d, t) = tmp();
    let root = t.join("v");
    let (v, _) = ScopedView::open(&s, &rw(WorkStart::Clone), &root).unwrap();
    assert!(v.status().unwrap().tampered.is_empty());
    chmod(&root.join("base/docs/readme.md"), 0o555);
    assert_eq!(v.status().unwrap().tampered, ["docs/readme.md"]);
}

#[test]
fn a_refresh_repairs_a_tampered_base_file_and_keeps_the_execute_bit_of_the_others() {
    let s = probe();
    s.inner.put_attrs(B, "docs/bin/run.sh", b"#!/bin/sh\n", false, EXEC).unwrap();
    let (_d, t) = tmp();
    let root = t.join("v");
    let (mut v, _) = ScopedView::open(&s, &rw(WorkStart::Clone), &root).unwrap();
    chmod(&root.join("base/docs/readme.md"), 0o755);
    s.seed("docs/new.md", "new");
    s.inner.put_attrs(B, "docs/bin/run2.sh", b"#!/bin/sh\n", false, EXEC).unwrap();
    v.refresh(&s).unwrap();
    let base = root.join("base/docs");
    assert_eq!(mode(&base.join("readme.md")), 0o444, "the flipped bit was put back from the entry");
    assert_eq!(mode(&base.join("bin/run.sh")), 0o555);
    assert_eq!(mode(&base.join("bin/run2.sh")), 0o555, "an executable that arrives on refresh");
    assert_eq!(mode(&base.join("new.md")), 0o444);
    assert!(v.status().unwrap().tampered.is_empty());
    assert!(files(&root.join("base")).contains(&"docs/bin/run.sh".to_owned()));
    assert_eq!(rd(&base.join("bin/run.sh")), "#!/bin/sh\n");
}

/// What overlayfs copy-up does: a new file in the upper with the lower's mode and bytes.
fn copy_up(root: &Path, rel: &str) {
    let dst = root.join("work").join(rel);
    std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
    std::fs::copy(root.join("lower").join(rel), &dst).unwrap(); // std::fs::copy keeps the mode
}

#[test]
fn an_overlay_lower_file_is_writable_and_an_in_place_edit_publishes() {
    let s = probe();
    let (_d, t) = tmp();
    let root = t.join("v");
    let (mut v, _) = ScopedView::open(&s, &rw(WorkStart::Empty), &root).unwrap();
    assert_eq!(mode(&root.join("lower/a.txt")), 0o644);
    assert_eq!(mode(&root.join("lower/dir")), 0o755, "a lower directory is writable too: copy-up of it must allow new files");
    copy_up(&root, "a.txt");
    // The in-place edit, with no chmod: it fails on a 0444 copy-up.
    std::fs::OpenOptions::new().write(true).truncate(true).open(root.join("work/a.txt")).unwrap();
    std::fs::write(root.join("work/a.txt"), "edited in place").unwrap();
    s.clear();
    v.publish(&s).unwrap();
    assert_eq!(s.data_puts(), ["proj/a.txt"]);
    assert_eq!(rd(&root.join("lower/a.txt")), "alpha", "the lower is a separate drive file until the next open");
    assert!(v.status().unwrap().tampered.is_empty(), "a writable lower file is not tampering");
}

#[test]
fn an_overlay_lower_executable_is_0755_and_stays_executable_through_copy_up_and_publish() {
    let s = probe();
    s.inner.put_attrs(B, "proj/run.sh", b"#!/bin/sh\n", false, EXEC).unwrap();
    let (_d, t) = tmp();
    let root = t.join("v");
    let (mut v, _) = ScopedView::open(&s, &rw(WorkStart::Empty), &root).unwrap();
    assert_eq!(mode(&root.join("lower/run.sh")), 0o755);
    copy_up(&root, "run.sh");
    assert_eq!(mode(&root.join("work/run.sh")), 0o755, "copy-up keeps the execute bit and the write bit");
    std::fs::write(root.join("work/run.sh"), "#!/bin/sh\necho edited\n").unwrap();
    v.publish(&s).unwrap();
    assert!(exec_of(&s, "proj/run.sh"), "the attribute survives an edit of the executable");
    assert_eq!(mode(&root.join("work/run.sh")), 0o755);
}

#[test]
fn the_base_is_still_read_only_when_the_lower_is_writable() {
    let s = probe();
    let (_d, t) = tmp();
    let root = t.join("v");
    let (v, _) = ScopedView::open(&s, &rw(WorkStart::Empty), &root).unwrap();
    assert_eq!(mode(&root.join("base")), 0o555);
    assert_eq!(mode(&root.join("base/docs/readme.md")), 0o444);
    assert!(v.status().unwrap().tampered.is_empty());
    chmod(&root.join("base/docs/readme.md"), 0o644);
    assert_eq!(v.status().unwrap().tampered, [] as [&str; 0], "a write bit alone is not content or exec tampering");
}

/// Put the same bytes again with another execute attribute: the etag and size do not move.
fn set_attr(s: &super::tests::Probe, key: &str, bytes: &str, a: ObjectAttrs) {
    let e = s.inner.head(B, key).unwrap().etag;
    s.inner.put_if_match_attrs(B, key, bytes.as_bytes(), &e, a).unwrap();
}

#[test]
fn a_mode_only_change_on_the_store_reaches_the_base_and_the_work_tree_on_refresh() {
    let s = probe();
    let (_d, t) = tmp();
    let root = t.join("v");
    let (mut v, _) = ScopedView::open(&s, &rw(WorkStart::Clone), &root).unwrap();
    assert_eq!((mode(&root.join("base/docs/readme.md")), mode(&root.join("work/a.txt"))), (0o444, 0o644));
    set_attr(&s, "docs/readme.md", "docs", EXEC);
    set_attr(&s, "proj/a.txt", "alpha", EXEC);
    let r = v.refresh(&s).unwrap();
    assert_eq!(mode(&root.join("base/docs/readme.md")), 0o555, "the base follows the store, read-only");
    assert_eq!(mode(&root.join("work/a.txt")), 0o755);
    assert!(r.work_updated.contains(&"a.txt".to_owned()) && r.work_conflicts.is_empty(), "{r:?}");
    assert_eq!(r.base[0].mode_updated, ["readme.md"], "{r:?}");
    let st = v.status().unwrap();
    assert!(st.tampered.is_empty() && st.pending.is_empty(), "{st:?}");
    // The other way.
    set_attr(&s, "proj/a.txt", "alpha", ObjectAttrs::default());
    v.refresh(&s).unwrap();
    assert_eq!(mode(&root.join("work/a.txt")), 0o644);
}

#[test]
fn a_mode_the_session_set_is_adopted_when_equal_and_never_overwritten_by_a_content_refresh() {
    let s = probe();
    let (_d, t) = tmp();
    let root = t.join("v");
    let (mut v, _) = ScopedView::open(&s, &rw(WorkStart::Clone), &root).unwrap();
    chmod(&root.join("work/a.txt"), 0o755);
    chmod(&root.join("work/d.txt"), 0o755);
    // The store flips d.txt to executable too: the session already has it, so it is adopted.
    set_attr(&s, "proj/d.txt", "delta", EXEC);
    let r = v.refresh(&s).unwrap();
    assert!(r.work_conflicts.is_empty(), "{r:?}");
    assert_eq!((mode(&root.join("work/a.txt")), mode(&root.join("work/d.txt"))), (0o755, 0o755));
    assert_eq!(v.status().unwrap().pending, [("a.txt".to_owned(), ChangeKind::Modify)], "only the session's own chmod is pending");
    // New bytes for a.txt from the store while the session holds its own mode: not overwritten.
    let e = s.inner.head(B, "proj/a.txt").unwrap().etag;
    s.inner.put_if_match(B, "proj/a.txt", b"alpha 2", &e).unwrap();
    let r = v.refresh(&s).unwrap();
    assert_eq!(r.work_conflicts, ["a.txt"], "{r:?}");
    assert_eq!((mode(&root.join("work/a.txt")), std::fs::read_to_string(root.join("work/a.txt")).unwrap()), (0o755, "alpha".to_owned()));
}
