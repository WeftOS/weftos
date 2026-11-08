//! The scoped-view cycle against a REAL RustFS. Ignored by default.
//!
//! Point `WEFTOS_STORE_TEST_RUSTFS_ENV` at the env file of a THROWAWAY loopback RustFS
//! (`MESH_S3_ENDPOINT`, `MESH_S3_ACCESS_KEY`, `MESH_S3_SECRET_KEY`, `MESH_S3_REGION`) and run:
//!   scripts/build.sh test weftos-store --features rustfs --ignored --filter live_view
//! Never point it at PG. Step times are printed with `eprintln!` (set
//! `NEXTEST_SUCCESS_OUTPUT=final` to see them); they are one sample on a loaded machine.

use std::path::Path;
use std::time::Instant;

use super::*;
use crate::backend::{AdminBackend, ObjectBackend};
use crate::drive::Keep;
use crate::rustfs::{RustfsBackend, RustfsConfig};

const WORK_FILES: usize = 200;
const BASE_FILES: usize = 100;

fn body(i: usize) -> Vec<u8> {
    format!("view test file {i}\n").repeat(40 + i % 60).into_bytes()
}

fn ms(t: Instant) -> u128 {
    t.elapsed().as_millis()
}

#[test]
#[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
fn live_view_open_refresh_publish_close() {
    let envf = std::env::var("WEFTOS_STORE_TEST_RUSTFS_ENV").expect("WEFTOS_STORE_TEST_RUSTFS_ENV");
    let scratch = tempfile::tempdir().unwrap();
    let dir = std::fs::canonicalize(scratch.path()).unwrap();
    let be = RustfsBackend::new(RustfsConfig::from_env_file(Path::new(&envf), dir.join("secrets")).unwrap()).unwrap();
    let bucket = format!("t-view-{}", &ulid::Ulid::new().to_string().to_lowercase()[10..]);
    be.create_bucket(&bucket, &[]).unwrap();
    for i in 0..WORK_FILES {
        be.put(&bucket, &format!("proj/d{:02}/f{i:03}.txt", i % 20), &body(i), false).unwrap();
    }
    for i in 0..BASE_FILES {
        be.put(&bucket, &format!("docs/s{:02}/p{i:03}.md", i % 10), &body(1000 + i), false).unwrap();
    }

    let spec = ViewSpec {
        bucket: bucket.clone(),
        base: vec![Include { prefix: "docs".into(), at: "docs".into() }],
        work: Some(WorkSpec { prefix: "proj".into(), start: WorkStart::Clone }),
        deny: vec!["*.log".into()],
        limits: ViewLimits::default(),
        lease_ttl_ms: DEFAULT_LEASE_TTL_MS,
        mount_points: vec!["tmp".into()],
        device: Some("live-test".into()),
    };
    let root = dir.join("view");

    // The same two prefixes through the bare drive, for an in-run comparison: open minus this is
    // the view's own local cost (lease, mirror, clone, modes, manifest) plus noise.
    let t = Instant::now();
    crate::drive::materialise(&be, &bucket, "docs", &dir.join("bare-docs")).unwrap();
    crate::drive::materialise(&be, &bucket, "proj", &dir.join("bare-proj")).unwrap();
    let bare_ms = ms(t);

    let t = Instant::now();
    let (mut v, rep) = ScopedView::open(&be, &spec, &root).unwrap();
    let open_ms = ms(t);
    assert_eq!((rep.files, rep.fetched), (WORK_FILES + BASE_FILES, WORK_FILES + BASE_FILES), "{:?}", rep.skipped);
    // A second opener elsewhere is refused while the lease is live.
    assert!(matches!(ScopedView::open(&be, &spec, &dir.join("other")), Err(ViewError::LeaseHeld { .. })));

    // The session edits; another writer changes the store meanwhile.
    let w = root.join("work");
    std::fs::write(w.join("d01/f001.txt"), b"edited in the session\n").unwrap();
    std::fs::write(w.join("d02/f002.txt"), b"edited in the session, raced remotely\n").unwrap();
    std::fs::write(w.join("d03/new.txt"), b"created in the session\n").unwrap();
    std::fs::remove_file(w.join("d04/f004.txt")).unwrap();
    be.put(&bucket, "docs/s01/p001.md", b"docs changed remotely\n", false).unwrap();
    be.put(&bucket, "proj/d02/f002.txt", b"changed remotely\n", false).unwrap();
    be.put(&bucket, "proj/d05/f005.txt", b"changed remotely, unedited here\n", false).unwrap();

    let t = Instant::now();
    let r = v.refresh(&be).unwrap();
    let refresh_ms = ms(t);
    assert_eq!(std::fs::read(root.join("base/docs/s01/p001.md")).unwrap(), b"docs changed remotely\n");
    assert_eq!(std::fs::read(w.join("d05/f005.txt")).unwrap(), b"changed remotely, unedited here\n");
    assert_eq!(r.work_conflicts, ["d02/f002.txt"]);

    let t = Instant::now();
    let p = v.publish(&be).unwrap();
    let publish_ms = ms(t);
    assert_eq!((p.up.put.len(), p.up.deleted.len(), p.up.conflicts.len()), (2, 1, 1), "{p:?}");
    assert_eq!(p.up.head_then_write, 0, "RustFS honours If-Match");
    assert_eq!(be.get(&bucket, "proj/d02/f002.txt").unwrap(), b"changed remotely\n", "the race is a conflict, not an overwrite");
    assert!(be.head(&bucket, "proj/.drive-tombstones/d04/f004.txt").is_ok());

    let t = Instant::now();
    v.resolve("d02/f002.txt", Keep::Local).unwrap();
    let p = v.publish(&be).unwrap();
    let resolve_ms = ms(t);
    assert_eq!((p.up.put.len(), p.open_conflicts), (1, 0), "{p:?}");

    let t = Instant::now();
    let c = v.close(&be, CloseMode::Publish).unwrap();
    let close_ms = ms(t);
    assert!(c.root_removed && c.lease_released == Some(true), "{c:?}");
    assert!(be.head(&bucket, &lease_key(&bucket, "proj/")).is_err());
    assert_eq!(be.get(&bucket, "proj/d02/f002.txt").unwrap(), b"edited in the session, raced remotely\n");

    eprintln!(
        "VIEW LIVE (noisy, load 6-7): bare drive materialise of the same prefixes {bare_ms} ms | open {} files ({WORK_FILES} work + {BASE_FILES} base) {open_ms} ms | \
         refresh (2 base + 2 work objects changed) {refresh_ms} ms | publish (2 put, 1 delete, 1 conflict) {publish_ms} ms | \
         resolve + republish {resolve_ms} ms | close (publish no-op, lease release, remove) {close_ms} ms",
        WORK_FILES + BASE_FILES
    );

    for m in be.list(&bucket, "").unwrap() {
        be.delete(&bucket, &m.key).unwrap();
    }
    be.delete_bucket(&bucket).unwrap();
}
