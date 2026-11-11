//! macOS drive hardening (WO-076). Fixed: Finder junk uploaded (finder_junk, remote_junk); a case-only rename that sorts upper-case first aborted sync_up on the file-system backend (case_only_rename_upper, dir_case_flip). The rest pin behaviour found sound on APFS: normalisation twins become a conflict, a case twin never overwrites a local file, a symlinked root works.
use std::path::Path;
use super::tests_hardening::rd;
use super::tests_hardening::{Rig, quiet_batch, wr};
use super::*;
use crate::backend::{AdminBackend, ObjectBackend};
use crate::mem::MemBackend;

const BK: &str = "b-mac";
fn be() -> MemBackend {
    let b = MemBackend::new();
    b.create_bucket(BK, &[]).unwrap();
    b
}

#[test]
fn finder_junk_is_not_uploaded() {
    let b = be();
    let rig = Rig::new(&b, BK, "p");
    rig.seed("a.txt", "a");
    let d = rig.join("d");
    for n in [".DS_Store", "sub/.DS_Store", "._a.txt", "sub/._b", ".Spotlight-V100/x", ".fseventsd/y", ".Trashes/z", ".TemporaryItems/t"] {
        wr(&d, n, "junk");
    }
    wr(&d, "real.txt", "r");
    let ch: Vec<String> = quiet_batch(&d).into_iter().map(|c| c.path).collect();
    assert_eq!(ch, vec!["real.txt".to_string()], "junk leaked: {ch:?}");
}

#[test]
fn normalisation_twins_do_not_lose_data() {
    let b = be();
    let rig = Rig::new(&b, BK, "p");
    let nfc = "caf\u{e9}.txt";
    let nfd = "cafe\u{301}.txt";
    rig.seed(nfc, "NFC");
    rig.seed(nfd, "NFD");
    let d = rig.join("d");
    let _ = sync_up(&b, &d);
    let still: Vec<bool> = [nfc, nfd].iter().map(|k| rig.text(k).is_some()).collect();
    eprintln!("remote after join+sync_up: {still:?}; local: {:?}", std::fs::read_dir(&d).unwrap().map(|e| e.unwrap().file_name()).collect::<Vec<_>>());
    assert_eq!(still, vec![true, true], "a remote object was deleted");
}

#[test]
fn case_only_rename() {
    let b = be();
    let rig = Rig::new(&b, BK, "p");
    rig.seed("Readme.md", "hello");
    let d = rig.join("d");
    std::fs::rename(d.join("Readme.md"), d.join("readme.md")).unwrap();
    let ch = quiet_batch(&d);
    eprintln!("changes: {ch:?}");
    let r = sync_up(&b, &d).unwrap();
    eprintln!("up: {r:?}");
    assert_eq!(rig.text("readme.md").as_deref(), Some("hello"));
    let e = rig.join("e");
    assert_eq!(std::fs::read_to_string(e.join("readme.md")).unwrap(), "hello");
}

fn live_keys(b: &impl ObjectBackend) -> Vec<String> {
    b.list(BK, "p/").unwrap().into_iter().map(|m| m.key).filter(|k| !k.contains(TOMBSTONE_DIR)).collect()
}

fn fs_be(root: &Path) -> crate::fs::FsBackend {
    let b = crate::fs::FsBackend::open(&root.join("store")).unwrap();
    b.create_bucket(BK, &[]).unwrap();
    b
}

#[test]
fn case_only_rename_upper_after_lower_on_fs_backend() {
    let root = tempfile::tempdir().unwrap();
    let b = fs_be(root.path());
    b.put(BK, "p/readme.md", b"hello", false).unwrap();
    let d = root.path().join("d");
    materialise(&b, BK, "p", &d).unwrap();
    std::fs::rename(d.join("readme.md"), d.join("Readme.md")).unwrap();
    let r = sync_up(&b, &d);
    eprintln!("up: {r:?}");
    r.unwrap();
    let keys = live_keys(&b);
    assert_eq!(keys, vec!["p/Readme.md".to_string()]);
}

#[test]
fn dir_case_flip_on_fs_backend() {
    let root = tempfile::tempdir().unwrap();
    let b = fs_be(root.path());
    b.put(BK, "p/docs/a.md", b"a", false).unwrap();
    b.put(BK, "p/docs/b.md", b"b", false).unwrap();
    let d = root.path().join("d");
    materialise(&b, BK, "p", &d).unwrap();
    std::fs::rename(d.join("docs"), d.join("tmp-x")).unwrap();
    std::fs::rename(d.join("tmp-x"), d.join("Docs")).unwrap();
    let r = sync_up(&b, &d);
    eprintln!("up: {r:?}");
    r.unwrap();
    let keys = live_keys(&b);
    assert_eq!(keys, vec!["p/Docs/a.md".to_string(), "p/Docs/b.md".to_string()]);
}

#[test]
fn symlinked_drive_root() {
    let root = tempfile::tempdir().unwrap();
    let b = fs_be(root.path());
    b.put(BK, "p/a.txt", b"a", false).unwrap();
    let real = root.path().join("real");
    std::fs::create_dir(&real).unwrap();
    let link = root.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let r = materialise(&b, BK, "p", &link).unwrap();
    eprintln!("mat: {r:?}");
    assert!(real.join("a.txt").is_file());
    wr(&link, "n.txt", "n");
    assert_eq!(quiet_batch(&link).len(), 1);
    sync_up(&b, &link).unwrap();
    assert!(b.get(BK, "p/n.txt").is_ok());
}

#[test]
fn remote_case_twin_of_an_untracked_local_file_is_not_overwritten() {
    let root = tempfile::tempdir().unwrap();
    let b = fs_be(root.path());
    b.put(BK, "p/A.txt", b"remote", false).unwrap();
    let d = root.path().join("d");
    std::fs::create_dir(&d).unwrap();
    std::fs::write(d.join("a.txt"), "local").unwrap();
    let r = materialise(&b, BK, "p", &d).unwrap();
    eprintln!("mat: {r:?}");
    let names: Vec<_> = std::fs::read_dir(&d).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).filter(|n| n != ".drive").collect();
    eprintln!("names {names:?}");
    let all: String = names.iter().map(|n| std::fs::read_to_string(d.join(n)).unwrap()).collect::<Vec<_>>().join("|");
    assert!(all.contains("local"), "local bytes lost: {all}");
}

#[test]
fn remote_junk_key_is_not_materialised() {
    let b = be();
    let rig = Rig::new(&b, BK, "p");
    rig.seed("a.txt", "a");
    rig.seed(".DS_Store", "junk");
    rig.seed("sub/._b", "junk");
    let d = rig.join("d");
    assert!(d.join("a.txt").is_file());
    assert!(!d.join(".DS_Store").exists() && !d.join("sub/._b").exists());
}
