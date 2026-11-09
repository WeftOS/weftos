//! Scoped-view failure modes (spec section 7) on the in-memory backend.

use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::Ordering;

use super::tests::{B, files, probe, rd, rw, spec, tmp, wr, docs};
use super::*;
use crate::backend::ObjectBackend;
use crate::drive::{DriveError, SkipWhy};

#[test]
fn a_hostile_listing_cannot_escape_the_root() {
    let mut s = probe();
    s.ghosts = vec![
        "proj/../escape.txt".into(),
        "proj/x/../../y.txt".into(),
        "proj/a\\b.txt".into(),
        "proj//empty.txt".into(),
        "docs/./dot.md".into(),
    ];
    let (_d, t) = tmp();
    let root = t.join("v");
    let (_v, rep) = ScopedView::open(&s, &rw(WorkStart::Clone), &root).unwrap();
    assert_eq!(files(&t), files(&t).into_iter().filter(|p| p.starts_with("v")).collect::<Vec<_>>(), "nothing outside the root");
    let bad = rep.skipped.iter().filter(|(_, k)| k.why == SkipWhy::BadPath).count();
    assert_eq!(bad, 5, "{:?}", rep.skipped);
    for p in files(&root.join("base")).iter().chain(&files(&root.join("work"))) {
        assert!(!p.contains("..") && !p.contains('\\'), "{p}");
    }
}

#[test]
fn a_symlink_planted_in_the_base_is_never_written_through() {
    let s = probe();
    let (_d, t) = tmp();
    let root = t.join("v");
    let outside = t.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let (mut v, _) = ScopedView::open(&s, &rw(WorkStart::Clone), &root).unwrap();
    // A host with no bwrap: the owner can undo the modes and swap a directory for a symlink.
    let base = root.join("base");
    for p in [&base, &base.join("docs")] {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    std::fs::set_permissions(base.join("docs/guide"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::remove_dir_all(base.join("docs/guide")).unwrap();
    std::os::unix::fs::symlink(&outside, base.join("docs/guide")).unwrap();
    s.seed("docs/guide/intro.md", "intro v2");
    v.refresh(&s).unwrap();
    assert!(files(&outside).is_empty(), "nothing written through the symlink");
    let guide = base.join("docs/guide");
    assert!(std::fs::symlink_metadata(&guide).unwrap().is_dir(), "the symlink was replaced by the real directory");
    assert_eq!(rd(&guide.join("intro.md")), "intro v2");
}

#[test]
fn a_bad_root_is_refused_and_left_untouched() {
    let s = probe();
    let (_d, t) = tmp();
    let sp = rw(WorkStart::Clone);
    assert!(matches!(ScopedView::open(&s, &sp, Path::new("relative/v")), Err(ViewError::Root(..))));
    assert!(matches!(ScopedView::open(&s, &sp, &t.join("a/../v")), Err(ViewError::Root(..))));
    std::fs::create_dir_all(t.join("real")).unwrap();
    std::os::unix::fs::symlink(t.join("real"), t.join("link")).unwrap();
    assert!(matches!(ScopedView::open(&s, &sp, &t.join("link")), Err(ViewError::Root(..))));
    assert!(matches!(ScopedView::open(&s, &sp, &t.join("link/v")), Err(ViewError::Root(..))), "symlinked parent");
    wr(&t.join("foreign/data.txt"), "someone's");
    assert!(matches!(ScopedView::open(&s, &sp, &t.join("foreign")), Err(ViewError::Root(..))));
    assert_eq!(files(&t.join("foreign")), ["data.txt"]);
    assert!(s.inner.head(B, &lease_key(B, "proj/")).is_err(), "no lease taken for a refused root");
}

#[test]
fn a_crashed_open_is_cleaned_up_by_the_next_open() {
    let s = probe();
    let (_d, t) = tmp();
    let root = t.join("v");
    // What a kill -9 mid-materialise leaves: the marker, a lease record, a torn index, a temp file.
    let (v, _) = ScopedView::open(&s, &rw(WorkStart::Clone), &root).unwrap();
    let lay = Layout(root.clone());
    std::fs::remove_file(lay.manifest()).unwrap();
    std::fs::write(lay.drive(0).join(".drive/index.json"), b"{\"schema\": 1, \"buck").unwrap();
    std::fs::write(lay.drive(0).join(".drive.tmp.01J0000000000000000000000"), b"half").unwrap();
    let old = v.lease().unwrap().clone();
    let (v2, rep) = ScopedView::open(&s, &rw(WorkStart::Clone), &root).unwrap();
    assert!(rep.recovered);
    assert_ne!(v2.lease().unwrap().body.id, old.body.id, "the crashed open's lease was released and a new one taken");
    assert!(!lay.drive(0).join(".drive.tmp.01J0000000000000000000000").exists());
    assert_eq!(rd(&root.join("base/docs/readme.md")), "docs");
}

#[test]
fn a_torn_index_in_an_open_view_is_a_typed_error_and_the_view_survives() {
    let s = probe();
    let (_d, t) = tmp();
    let root = t.join("v");
    let (mut v, _) = ScopedView::open(&s, &rw(WorkStart::Clone), &root).unwrap();
    std::fs::write(Layout(root.clone()).drive(0).join(".drive/index.json"), b"{not json").unwrap();
    assert!(matches!(v.refresh(&s), Err(ViewError::Drive(DriveError::Json(_)))));
    assert_eq!(std::fs::symlink_metadata(root.join("base")).unwrap().permissions().mode() & 0o777, 0o555, "base locked again");
    assert!(ScopedView::load(&root).is_ok());
    // Discard still cleans up a damaged view.
    assert!(v.close(&s, CloseMode::Discard).unwrap().root_removed);
}

#[test]
fn an_unreachable_store_at_open_leaves_nothing_behind() {
    let s = probe();
    let (_d, t) = tmp();
    let root = t.join("v");
    // Reachable for the lease, gone for the listing: flip it from inside a spec-sized window by
    // making the base include's listing fail with a prefix the probe refuses.
    s.down.store(true, Ordering::SeqCst);
    assert!(matches!(ScopedView::open(&s, &rw(WorkStart::Clone), &root), Err(ViewError::Drive(DriveError::Backend(_)))));
    assert!(!root.exists());
    s.down.store(false, Ordering::SeqCst);

    // Down after the lease: the lease is released on the way out.
    struct DownAfterLease<'a>(&'a super::tests::Probe);
    impl ObjectBackend for DownAfterLease<'_> {
        fn put(&self, b: &str, k: &str, d: &[u8], n: bool) -> std::result::Result<crate::backend::ObjectMeta, BackendError> {
            self.0.put(b, k, d, n)
        }
        fn get(&self, b: &str, k: &str) -> std::result::Result<Vec<u8>, BackendError> {
            self.0.get(b, k)
        }
        fn head(&self, b: &str, k: &str) -> std::result::Result<crate::backend::ObjectMeta, BackendError> {
            self.0.head(b, k)
        }
        fn put_if_match(&self, b: &str, k: &str, d: &[u8], e: &str) -> std::result::Result<crate::backend::ObjectMeta, BackendError> {
            self.0.put_if_match(b, k, d, e)
        }
        fn delete_if_match(&self, b: &str, k: &str, e: &str) -> std::result::Result<(), BackendError> {
            self.0.delete_if_match(b, k, e)
        }
        fn delete(&self, b: &str, k: &str) -> std::result::Result<(), BackendError> {
            self.0.delete(b, k)
        }
        fn copy(&self, a: &str, b: &str, c: &str, d: &str) -> std::result::Result<crate::backend::ObjectMeta, BackendError> {
            self.0.copy(a, b, c, d)
        }
        fn list(&self, _: &str, _: &str) -> std::result::Result<Vec<crate::backend::ObjectMeta>, BackendError> {
            Err(BackendError::Io("listing failed (test)".into()))
        }
    }
    assert!(ScopedView::open(&DownAfterLease(&s), &rw(WorkStart::Clone), &root).is_err());
    assert!(!root.exists());
    assert!(s.inner.head(B, &lease_key(B, "proj/")).is_err(), "lease released after a failed open");
}

#[test]
fn an_unreachable_store_at_refresh_or_publish_leaves_the_view_intact() {
    let s = probe();
    let (_d, t) = tmp();
    let root = t.join("v");
    let (mut v, _) = ScopedView::open(&s, &rw(WorkStart::Clone), &root).unwrap();
    wr(&root.join("work/a.txt"), "edit");
    s.down.store(true, Ordering::SeqCst);
    assert!(matches!(v.refresh(&s), Err(ViewError::Drive(DriveError::Backend(_)))));
    assert!(matches!(v.publish(&s), Err(ViewError::Drive(DriveError::Backend(_)))));
    s.down.store(false, Ordering::SeqCst);
    assert_eq!(rd(&root.join("work/a.txt")), "edit");
    assert_eq!(v.status().unwrap().pending.len(), 1);
    v.publish(&s).unwrap();
    assert_eq!(s.text("proj/a.txt").as_deref(), Some("edit"));
}

#[test]
fn a_lost_or_expired_lease_refuses_publish() {
    let s = probe();
    let (_d, t) = tmp();
    let root = t.join("v");
    let (mut v, _) = ScopedView::open(&s, &rw(WorkStart::Clone), &root).unwrap();
    wr(&root.join("work/a.txt"), "edit");
    // Another host took the lease over (for example after our clock ran slow past its expiry).
    let key = lease_key(B, "proj/");
    s.inner.put(B, &key, br#"{"other":"host"}"#, false).unwrap();
    s.clear();
    assert!(matches!(v.publish(&s), Err(ViewError::LeaseLost(_))));
    assert!(matches!(v.renew(&s), Err(ViewError::LeaseLost(_))));
    assert!(s.data_puts().is_empty());
    let c = v.close(&s, CloseMode::Discard).unwrap();
    assert_eq!(c.lease_released, Some(false), "someone else's lease is left alone");
    assert!(s.inner.head(B, &key).is_ok());

    let s = probe();
    let (_d, t) = tmp();
    let root = t.join("v");
    let mut sp = rw(WorkStart::Clone);
    sp.lease_ttl_ms = 1;
    let (mut v, _) = ScopedView::open(&s, &sp, &root).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    wr(&root.join("work/a.txt"), "edit");
    assert!(matches!(v.publish(&s), Err(ViewError::LeaseExpired(_))));
    assert!(v.status().unwrap().lease_expired);
    v.spec.lease_ttl_ms = 60_000;
    v.renew(&s).unwrap();
    v.publish(&s).unwrap();
    assert_eq!(s.text("proj/a.txt").as_deref(), Some("edit"));
}

#[test]
fn writes_outside_the_work_tree_never_reach_the_store() {
    let s = probe();
    let (_d, t) = tmp();
    let root = t.join("v");
    let (mut v, _) = ScopedView::open(&s, &rw(WorkStart::Clone), &root).unwrap();
    // Into the base: refused by the modes (and by the read-only bind under bwrap).
    assert!(std::fs::write(root.join("base/docs/readme.md"), "x").is_err());
    // Control names inside the work tree are never published.
    wr(&root.join("work/.drive/index.json"), "{}");
    wr(&root.join("work/sub/.drive-tombstones/a.txt"), "{}");
    wr(&root.join("work/.git/config"), "[core]");
    std::os::unix::fs::symlink("/etc/hosts", root.join("work/hosts")).unwrap();
    s.clear();
    let p = v.publish(&s).unwrap();
    assert!(s.data_puts().is_empty(), "{:?}", s.data_puts());
    assert!(p.skipped.iter().any(|k| k.path == "hosts" && k.why == SkipWhy::Symlink), "{:?}", p.skipped);
}

#[test]
fn a_changed_base_file_is_reported_and_repaired_and_blocks_publish_on_the_work_drive() {
    let s = probe();
    let (_d, t) = tmp();
    let root = t.join("v");
    let (mut v, _) = ScopedView::open(&s, &rw(WorkStart::Clone), &root).unwrap();
    // No bwrap: the owner makes a base file writable and edits it in place (its hard link).
    let f = root.join("base/docs/readme.md");
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::write(&f, "tampered").unwrap();
    assert_eq!(v.status().unwrap().tampered, ["docs/readme.md"]);
    let r = v.refresh(&s).unwrap();
    assert_eq!(r.repaired, ["readme.md"]);
    assert_eq!(rd(&f), "docs");
    assert!(v.status().unwrap().tampered.is_empty());

    // The work drive changed outside a publish: refuse rather than upload it.
    let wd = Layout(root.clone()).work_drive().join("a.txt");
    std::fs::set_permissions(&wd, std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::write(&wd, "not from the session").unwrap();
    assert!(matches!(v.publish(&s), Err(ViewError::Tampered(p)) if p == ["a.txt"]));
    assert_eq!(s.text("proj/a.txt").as_deref(), Some("alpha"));
}

#[test]
fn an_over_budget_open_is_refused_and_cleaned_up() {
    let s = probe();
    let (_d, t) = tmp();
    let root = t.join("v");
    // Each include fits the 10-byte ceiling on its own (the drive skips what does not); together
    // they do not.
    let mut sp = spec(vec![docs(), Include { prefix: "proj".into(), at: "p".into() }], None);
    sp.limits.max_total = 10;
    let r = ScopedView::open(&s, &sp, &root);
    assert!(matches!(r, Err(ViewError::OverBudget(_))), "{:?}", r.err());
    assert!(!root.exists());
}

use std::path::Path;
use crate::backend::BackendError;
