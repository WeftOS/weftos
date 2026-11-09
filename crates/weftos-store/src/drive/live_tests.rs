//! The whole drive cycle against a REAL RustFS. Ignored by default.
//!
//! Point `WEFTOS_STORE_TEST_RUSTFS_ENV` at the env file of a THROWAWAY loopback RustFS
//! (`MESH_S3_ENDPOINT`, `MESH_S3_ACCESS_KEY`, `MESH_S3_SECRET_KEY`) and run with `--ignored`.
//! Never point it at PG. Timings are printed with `eprintln!`; run with `--no-capture` to see them.

use std::path::Path;
use std::time::{Duration, Instant};

use super::*;
use crate::backend::{AdminBackend, ObjectBackend};
use crate::rustfs::{RustfsBackend, RustfsConfig};

const FILES: usize = 500;

fn body(i: usize) -> Vec<u8> {
    // 1 to 4 KiB of text, different for every file.
    let line = format!("file {i} line of drive test content\n");
    line.repeat(30 + (i % 90)).into_bytes()
}

fn ms(t: Instant) -> u128 {
    t.elapsed().as_millis()
}

#[test]
#[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
fn live_drive_cycle() {
    let envf = std::env::var("WEFTOS_STORE_TEST_RUSTFS_ENV").expect("WEFTOS_STORE_TEST_RUSTFS_ENV");
    let scratch = tempfile::tempdir().unwrap();
    let be = RustfsBackend::new(RustfsConfig::from_env_file(Path::new(&envf), scratch.path().join("secrets")).unwrap()).unwrap();
    let bucket = format!("t-drive-{}", &ulid::Ulid::new().to_string().to_lowercase()[10..]);
    be.create_bucket(&bucket, &[]).unwrap();

    let t = Instant::now();
    for i in 0..FILES {
        be.put(&bucket, &format!("proj/d{:02}/f{i:03}.txt", i % 25), &body(i), false).unwrap();
    }
    let seed_ms = ms(t);

    // Device A: first fill.
    let (a, b) = (scratch.path().join("a"), scratch.path().join("b"));
    let t = Instant::now();
    let r = materialise(&be, &bucket, "proj", &a).unwrap();
    let mat_ms = ms(t);
    assert_eq!((r.created.len(), r.fetched), (FILES, FILES), "{:?}", r.skipped);
    assert_eq!(std::fs::read(a.join("d07/f007.txt")).unwrap(), body(7));

    // Refresh with nothing changed: one listing, no GETs.
    let t = Instant::now();
    let r = materialise(&be, &bucket, "proj", &a).unwrap();
    let noop_ms = ms(t);
    assert_eq!((r.fetched, r.unchanged, r.created.len(), r.updated.len()), (0, FILES, 0, 0));

    // Device B fills too (the second device's cold join).
    let t = Instant::now();
    materialise(&be, &bucket, "proj", &b).unwrap();
    let mat_b_ms = ms(t);

    // One watch-and-sync cycle on A: edit, create, delete; default 2 s quiet.
    std::fs::write(a.join("d01/f001.txt"), b"edited on A\n").unwrap();
    std::fs::write(a.join("d01/new.txt"), b"created on A\n").unwrap();
    std::fs::remove_file(a.join("d02/f002.txt")).unwrap();
    let cycle = Instant::now();
    let changes = watch_once(&a, &WatchConfig::default(), Duration::from_secs(30)).unwrap().expect("a quiet batch");
    let watch_ms = ms(cycle);
    assert_eq!(changes.len(), 3, "{changes:?}");
    let t = Instant::now();
    let up = sync_up(&be, &a).unwrap();
    let up_ms = ms(t);
    assert_eq!((up.put.len(), up.deleted.len(), up.conflicts.len()), (2, 1, 0), "{up:?}");
    assert!(be.head(&bucket, "proj/d02/f002.txt").is_err());
    assert!(be.head(&bucket, "proj/.drive-tombstones/d02/f002.txt").is_ok());

    // B edits f001 too, then pulls A's changes: the edit is a conflict, the rest applies.
    std::fs::write(b.join("d01/f001.txt"), b"edited on B\n").unwrap();
    let t = Instant::now();
    let down = sync_down(&be, &b).unwrap();
    let down_ms = ms(t);
    assert_eq!((down.created.len(), down.removed.len(), down.conflicts.len(), down.fetched), (1, 1, 1, 2), "{down:?}");
    assert_eq!(std::fs::read(b.join("d01/f001.txt")).unwrap(), b"edited on B\n");
    assert!(!b.join("d02/f002.txt").exists() && b.join("d01/new.txt").exists());

    eprintln!(
        "DRIVE LIVE: seed {FILES} objects {seed_ms} ms | materialise A {mat_ms} ms | refresh no change {noop_ms} ms | \
         materialise B {mat_b_ms} ms | watch (2 s quiet) {watch_ms} ms | sync_up (2 put, 1 delete) {up_ms} ms | \
         cycle total {} ms | B sync_down (2 fetched, 1 removed, 1 conflict) {down_ms} ms",
        watch_ms + up_ms
    );

    for m in be.list(&bucket, "").unwrap() {
        be.delete(&bucket, &m.key).unwrap();
    }
    be.delete_bucket(&bucket).unwrap();
}

/// Does RustFS honour `If-Match` on PUT and DELETE? Prints what it did; asserts the right etag
/// always works, and that a wrong etag is refused (412) or, if ignored, that the write landed.
#[test]
#[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
fn live_if_match_is_honoured_or_not() {
    use crate::backend::BackendError;
    let envf = std::env::var("WEFTOS_STORE_TEST_RUSTFS_ENV").expect("WEFTOS_STORE_TEST_RUSTFS_ENV");
    let scratch = tempfile::tempdir().unwrap();
    let be = RustfsBackend::new(RustfsConfig::from_env_file(Path::new(&envf), scratch.path().join("secrets")).unwrap()).unwrap();
    let bucket = format!("t-ifm-{}", &ulid::Ulid::new().to_string().to_lowercase()[10..]);
    be.create_bucket(&bucket, &[]).unwrap();

    let v1 = be.put(&bucket, "k", b"one", false).unwrap();
    let v2 = be.put_if_match(&bucket, "k", b"two", &v1.etag).expect("right etag must succeed");
    assert_ne!(v1.etag, v2.etag);
    assert_eq!(be.get(&bucket, "k").unwrap(), b"two");

    // Wrong (stale) etag on PUT.
    let put_stale = be.put_if_match(&bucket, "k", b"three", &v1.etag);
    let after_put = be.get(&bucket, "k").unwrap();
    eprintln!("PUT with stale If-Match: {put_stale:?}; object now {:?}", String::from_utf8_lossy(&after_put));
    // Missing key.
    let put_missing = be.put_if_match(&bucket, "absent", b"x", &v1.etag);
    eprintln!("PUT If-Match on a missing key: {put_missing:?}; exists afterwards: {}", be.head(&bucket, "absent").is_ok());

    // DELETE with a stale etag, then with the current one.
    let cur = be.head(&bucket, "k").unwrap().etag;
    let del_stale = be.delete_if_match(&bucket, "k", &v1.etag);
    let still = be.head(&bucket, "k").is_ok();
    eprintln!("DELETE with stale If-Match: {del_stale:?}; object still there: {still}");
    if still {
        let del_ok = be.delete_if_match(&bucket, "k", &cur);
        eprintln!("DELETE with current If-Match: {del_ok:?}; gone: {}", be.head(&bucket, "k").is_err());
    }

    match put_stale {
        Err(BackendError::Stale(_)) => assert_eq!(after_put, b"two", "a refused put must leave the object"),
        Ok(_) => assert_eq!(after_put, b"three", "RustFS ignored If-Match on PUT: the stale write landed"),
        Err(e) => panic!("unexpected: {e}"),
    }
    for k in ["k", "absent"] {
        let _ = be.delete(&bucket, k);
    }
    let _ = be.delete_bucket(&bucket);
}
