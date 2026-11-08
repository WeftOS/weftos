//! The release store on S1's object backend: the same guarantees, through the real bucket mapping.
#![cfg(feature = "store")]

use std::sync::Arc;

use weftos_pq_sign::MlDsa65SigningKey;
use weftos_release::s3::StoreBackend;
use weftos_release::*;
use weftos_store::{AdminBackend, MemBackend, ObjectBackend};

const ULID: &str = "01JBXZ3N5C8QW4M2R7T9V6YH0K";

fn signer() -> ReleaseSigner {
    ReleaseSigner::new(ed25519_dalek::SigningKey::from_bytes(&[7; 32]), MlDsa65SigningKey::from_seed(&[7; 32]))
}

fn rq(build: &str, at: u64, files: &[(&str, &[u8])]) -> PublishRequest {
    PublishRequest {
        build_id: build.into(),
        source_rev: "abc".into(),
        created_at: at,
        builder: "ci".into(),
        artifacts: files.iter().map(|(p, b)| Artifact { path: (*p).into(), bytes: b.to_vec() }).collect(),
    }
}

fn setup() -> (Arc<MemBackend>, ReleaseStoreName, StoreBackend<MemBackend>) {
    let mem = Arc::new(MemBackend::new());
    let st = ReleaseStoreName::new("pg-mesh", ULID, "stable").unwrap();
    let be = StoreBackend::for_release(mem.clone(), &st).unwrap();
    mem.create_bucket(be.bucket(), &[]).unwrap();
    (mem, st, be)
}

#[tokio::test]
async fn bucket_is_the_one_s1_assigns() {
    let (_m, st, be) = setup();
    let want = weftos_store::StoreName::release("pg-mesh", ULID, "stable").unwrap().bucket();
    assert_eq!(be.bucket(), want);
    assert!(be.bucket().starts_with("r-"), "{st:?}");
}

#[tokio::test]
async fn publish_read_rebuild_and_retention_over_the_store_backend() {
    let (mem, st, be) = setup();
    let sg = signer();
    let tk = TrustedKeys(vec![sg.trusted()]);
    let p = ReleasePublisher::new(&be, &st, &sg);

    let r = p.publish(rq("b1", 100, &[("bin/weaver", b"v1"), ("shared", b"s")]), 1_000).await.unwrap();
    assert_eq!(r.outcome, PublishOutcome::Published);
    // the objects are really in S1's bucket, by hash
    let h = blake3::hash(b"v1").to_hex().to_string();
    assert_eq!(mem.get(be.bucket(), &format!("blobs/{h}")).unwrap(), b"v1");

    // a rebuild cannot overwrite; a retry is idempotent
    let e = p.publish(rq("b1", 100, &[("bin/weaver", b"EVIL")]), 1_000).await.unwrap_err();
    assert!(matches!(e, ReleaseError::AlreadyPublished { .. }), "{e}");
    let again = p.publish(rq("b1", 100, &[("bin/weaver", b"v1"), ("shared", b"s")]), 1_000).await.unwrap();
    assert_eq!(again.outcome, PublishOutcome::AlreadyPresent);

    let rd = ReleaseReader::new(&be, &st, &tk);
    let m = rd.manifest("b1").await.unwrap();
    assert_eq!(rd.read_artifact(&m, "bin/weaver").await.unwrap(), b"v1");

    // retention: b2 supersedes b1, b1 retires, its id stays dead, its private blob goes after grace
    p.publish(rq("b2", 200, &[("bin/weaver", b"v2"), ("shared", b"s")]), 1_000).await.unwrap();
    let sw = Sweeper::new(&be, &st, &tk);
    let pol = RetentionPolicy::new(1, 0).unwrap();
    let r = sw.sweep(&pol, 1_000, 500, "keep-last=1").await.unwrap();
    assert_eq!(r.expired, vec!["b1"]);
    let e = p.publish(rq("b1", 100, &[("bin/weaver", b"v1"), ("shared", b"s")]), 1_000).await.unwrap_err();
    assert!(matches!(e, ReleaseError::Retired { .. }), "{e}");
    let r = sw.sweep(&pol, 1_600, 500, "keep-last=1").await.unwrap();
    assert_eq!(r.blobs_deleted, vec![h.clone()]);
    assert!(mem.get(be.bucket(), &format!("blobs/{h}")).is_err());
    let m2 = rd.manifest("b2").await.unwrap();
    assert!(rd.audit(&m2).await.unwrap().is_empty());
}

#[tokio::test]
async fn conditional_put_maps_exists_and_get_maps_absent() {
    let (_m, _st, be) = setup();
    assert_eq!(be.put_if_absent("k/1", b"a").await.unwrap(), PutOutcome::Created);
    assert_eq!(be.put_if_absent("k/1", b"b").await.unwrap(), PutOutcome::Exists);
    assert_eq!(be.get("k/1").await.unwrap().unwrap(), b"a");
    assert_eq!(be.get("k/none").await.unwrap(), None);
    be.delete("k/none").await.unwrap(); // deleting nothing is fine
    assert_eq!(be.list("k/").await.unwrap(), vec!["k/1"]);
}

#[tokio::test]
async fn a_missing_bucket_is_a_backend_error_not_an_absent_key() {
    let mem = Arc::new(MemBackend::new());
    let be = StoreBackend::new(mem, "r-nope");
    assert!(be.get("x").await.is_err());
    assert!(be.put_if_absent("x", b"1").await.is_err());
}

#[tokio::test]
async fn server_side_copy_moves_no_bytes_through_the_caller() {
    let (mem, _st, be) = setup();
    be.put_if_absent("blobs/a", b"payload").await.unwrap();
    be.copy("blobs/a", "quarantine/a").await.unwrap();
    assert_eq!(mem.get(be.bucket(), "quarantine/a").unwrap(), b"payload");
    assert!(be.copy("blobs/none", "quarantine/none").await.is_err(), "a missing source is an error");
}
