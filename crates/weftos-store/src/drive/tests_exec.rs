//! The execute bit through the drive: `sync_up` stores it in the object's attributes, `materialise`
//! applies it, and an index written before the attribute existed still loads.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use super::*;
use crate::backend::{AdminBackend, ObjectAttrs, ObjectBackend};
use crate::mem::MemBackend;

const B: &str = "b-exec";

fn mem() -> MemBackend {
    let m = MemBackend::new();
    m.create_bucket(B, &[]).unwrap();
    m
}

fn mode(p: &Path) -> u32 {
    std::fs::symlink_metadata(p).unwrap().permissions().mode() & 0o777
}

fn chmod(p: &Path, m: u32) {
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap();
}

#[test]
fn an_executable_round_trips_through_sync_up_and_materialise() {
    let b = mem();
    let d1 = tempfile::tempdir().unwrap();
    materialise(&b, B, "p", d1.path()).unwrap();
    std::fs::write(d1.path().join("run.sh"), "#!/bin/sh\n").unwrap();
    chmod(&d1.path().join("run.sh"), 0o755);
    std::fs::write(d1.path().join("data.txt"), "data").unwrap();
    let up = sync_up(&b, d1.path()).unwrap();
    assert_eq!(up.put.len(), 2, "{up:?}");
    assert!(b.attrs(B, "p/run.sh").unwrap().executable);
    assert!(!b.attrs(B, "p/data.txt").unwrap().executable);

    // Another device materialises: the execute bit is there, no write beyond the owner's.
    let d2 = tempfile::tempdir().unwrap();
    materialise(&b, B, "p", d2.path()).unwrap();
    assert_eq!(mode(&d2.path().join("run.sh")), 0o755);
    assert_eq!(mode(&d2.path().join("data.txt")) & 0o111, 0, "the plain file is not executable");
    // Nothing to push back: materialising an executable is not a local change.
    assert!(sync_up(&b, d2.path()).unwrap().put.is_empty());
}

#[test]
fn a_chmod_with_the_same_bytes_is_pushed_as_a_modify_and_idempotent() {
    let b = mem();
    b.put(B, "p/x.sh", b"#!/bin/sh\n", false).unwrap();
    let d = tempfile::tempdir().unwrap();
    materialise(&b, B, "p", d.path()).unwrap();
    assert_eq!(mode(&d.path().join("x.sh")) & 0o111, 0, "a legacy object has no execute bit");
    assert!(sync_up(&b, d.path()).unwrap().put.is_empty());
    chmod(&d.path().join("x.sh"), 0o755);
    let up = sync_up(&b, d.path()).unwrap();
    assert_eq!(up.put, ["x.sh"], "{up:?}");
    assert!(b.attrs(B, "p/x.sh").unwrap().executable);
    assert!(sync_up(&b, d.path()).unwrap().put.is_empty(), "second push has nothing to do");
    chmod(&d.path().join("x.sh"), 0o644);
    assert_eq!(sync_up(&b, d.path()).unwrap().put, ["x.sh"]);
    assert!(!b.attrs(B, "p/x.sh").unwrap().executable);
}

#[test]
fn a_legacy_object_materialises_without_an_execute_bit() {
    let b = mem();
    b.put(B, "p/old.sh", b"#!/bin/sh\n", false).unwrap();
    assert_eq!(b.attrs(B, "p/old.sh").unwrap(), ObjectAttrs::default());
    let d = tempfile::tempdir().unwrap();
    materialise(&b, B, "p", d.path()).unwrap();
    assert_eq!(mode(&d.path().join("old.sh")) & 0o111, 0);
}

#[test]
fn an_index_from_before_the_attribute_loads_and_does_not_write_it_for_plain_files() {
    let b = mem();
    b.put(B, "p/a.txt", b"a", false).unwrap();
    let d = tempfile::tempdir().unwrap();
    materialise(&b, B, "p", d.path()).unwrap();
    let ix_path = d.path().join(".drive/index.json");
    let text = std::fs::read_to_string(&ix_path).unwrap();
    assert!(!text.contains("\"exec\""), "a non-executable entry is written exactly as before: {text}");
    // An old index has no `exec` field at all: it loads as not executable.
    let ix = Index::load(d.path()).unwrap().unwrap();
    assert!(!ix.entries["a.txt"].exec);
    assert_eq!(Index::load(d.path()).unwrap().unwrap().entries, ix.entries);
}
