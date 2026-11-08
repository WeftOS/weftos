//! The release store against a THROWAWAY RustFS (never PG). Set `WEFTOS_RELEASE_TEST_RUSTFS_ENV` to an
//! env file (MESH_S3_ENDPOINT / _ACCESS_KEY / _SECRET_KEY) and run with `--ignored`.
#![cfg(feature = "store")]

use std::path::Path;
use std::sync::Arc;

use weftos_pq_sign::MlDsa65SigningKey;
use weftos_release::s3::StoreBackend;
use weftos_release::*;
use weftos_store::{AdminBackend, RustfsBackend, RustfsConfig};

const ULID: &str = "01JBXZ3N5C8QW4M2R7T9V6YH0K";

fn rq(build: &str, at: u64, files: &[(&str, &[u8])]) -> PublishRequest {
    PublishRequest {
        build_id: build.into(),
        source_rev: "abc".into(),
        created_at: at,
        builder: "ci".into(),
        artifacts: files.iter().map(|(p, b)| Artifact { path: (*p).into(), bytes: b.to_vec() }).collect(),
    }
}

#[tokio::test]
#[ignore = "needs a throwaway RustFS: WEFTOS_RELEASE_TEST_RUSTFS_ENV"]
async fn live_release_store_round_trip() {
    let Ok(f) = std::env::var("WEFTOS_RELEASE_TEST_RUSTFS_ENV") else { panic!("WEFTOS_RELEASE_TEST_RUSTFS_ENV not set") };
    let dir = tempfile::tempdir().unwrap();
    let cfg = RustfsConfig::from_env_file(Path::new(&f), dir.path().join("secrets")).unwrap();
    // the blocking client must be built off the async threads
    let objects = Arc::new(tokio::task::spawn_blocking(move || RustfsBackend::new(cfg)).await.unwrap().unwrap());

    // a release store of its own, unique per run
    let channel = format!("t{}", &ulid::Ulid::new().to_string().to_lowercase()[16..]);
    let st = ReleaseStoreName::new("pg-mesh", ULID, &channel).unwrap();
    let be = StoreBackend::for_release(objects.clone(), &st).unwrap();
    let (o, b) = (objects.clone(), be.bucket().to_string());
    tokio::task::spawn_blocking(move || o.create_bucket(&b, &[])).await.unwrap().unwrap();

    let sg = ReleaseSigner::new(ed25519_dalek::SigningKey::from_bytes(&[3; 32]), MlDsa65SigningKey::from_seed(&[3; 32]));
    let tk = TrustedKeys(vec![sg.trusted()]);
    let p = ReleasePublisher::new(&be, &st, &sg);

    // publish, retry, rebuild
    let big = vec![0xABu8; 3 * 1024 * 1024];
    let r = p.publish(rq("b1", 100, &[("bin/weaver", &big), ("shared", b"s")]), 1_000).await.unwrap();
    assert_eq!(r.outcome, PublishOutcome::Published);
    assert_eq!(p.publish(rq("b1", 100, &[("bin/weaver", &big), ("shared", b"s")]), 1_000).await.unwrap().outcome, PublishOutcome::AlreadyPresent);
    let e = p.publish(rq("b1", 100, &[("bin/weaver", b"EVIL")]), 1_000).await.unwrap_err();
    assert!(matches!(e, ReleaseError::AlreadyPublished { .. }), "{e}");

    // conditional put on the real server
    assert_eq!(be.put_if_absent("probe/x", b"1").await.unwrap(), PutOutcome::Created);
    assert_eq!(be.put_if_absent("probe/x", b"2").await.unwrap(), PutOutcome::Exists);
    assert_eq!(be.get("probe/x").await.unwrap().unwrap(), b"1");
    assert_eq!(be.get("probe/none").await.unwrap(), None);

    // read back, verified
    let rd = ReleaseReader::new(&be, &st, &tk);
    let m = rd.manifest("b1").await.unwrap();
    assert_eq!(rd.read_artifact(&m, "bin/weaver").await.unwrap(), big);
    assert!(rd.audit(&m).await.unwrap().is_empty());

    // retention with server-side copy
    p.publish(rq("b2", 200, &[("bin/weaver", b"v2"), ("shared", b"s")]), 1_000).await.unwrap();
    let sw = Sweeper::new(&be, &st, &tk);
    let pol = RetentionPolicy::new(1, 0).unwrap();
    let r = sw.sweep(&pol, 1_000, 500, "keep-last=1").await.unwrap();
    assert_eq!(r.expired, vec!["b1"]);
    let r = sw.sweep(&pol, 1_600, 500, "keep-last=1").await.unwrap();
    assert_eq!(r.blobs_deleted.len(), 1, "{r:?}");
    assert!(be.list("quarantine/").await.unwrap().is_empty(), "nothing parked");
    let m2 = rd.manifest("b2").await.unwrap();
    assert!(rd.audit(&m2).await.unwrap().is_empty());
    let e = p.publish(rq("b1", 100, &[("bin/weaver", &big), ("shared", b"s")]), 1_000).await.unwrap_err();
    assert!(matches!(e, ReleaseError::Retired { .. }), "{e}");
}

/// Create a bucket with Object Lock (which also turns versioning on) through curl's SigV4. S1's
/// `create_bucket` cannot ask for a lock, and a lock cannot be added to an existing bucket.
fn create_locked_bucket(env: &Path, bucket: &str) {
    let text = std::fs::read_to_string(env).unwrap();
    let get = |k: &str| text.lines().find_map(|l| l.strip_prefix(&format!("{k}="))).unwrap().trim().to_string();
    let out = std::process::Command::new("curl")
        .args(["-s", "-o", "/dev/null", "-w", "%{http_code}", "-X", "PUT", "--aws-sigv4", "aws:amz:us-east-1:s3", "--user"])
        .arg(format!("{}:{}", get("MESH_S3_ACCESS_KEY"), get("MESH_S3_SECRET_KEY")))
        .args(["-H", "x-amz-bucket-object-lock-enabled: true"])
        .arg(format!("{}/{bucket}", get("MESH_S3_ENDPOINT")))
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "200", "creating the locked bucket");
}

/// The same flow on a versioned, Object-Lock bucket: a delete is a delete marker, so the sweeper's
/// listings, conditional puts and copy-back must all still hold.
#[tokio::test]
#[ignore = "needs a throwaway RustFS: WEFTOS_RELEASE_TEST_RUSTFS_ENV"]
async fn live_release_store_on_a_locked_versioned_bucket() {
    let f = std::env::var("WEFTOS_RELEASE_TEST_RUSTFS_ENV").expect("WEFTOS_RELEASE_TEST_RUSTFS_ENV");
    let dir = tempfile::tempdir().unwrap();
    let cfg = RustfsConfig::from_env_file(Path::new(&f), dir.path().join("secrets")).unwrap();
    let objects = Arc::new(tokio::task::spawn_blocking(move || RustfsBackend::new(cfg)).await.unwrap().unwrap());
    let channel = format!("l{}", &ulid::Ulid::new().to_string().to_lowercase()[16..]);
    let st = ReleaseStoreName::new("pg-mesh", ULID, &channel).unwrap();
    let be = StoreBackend::for_release(objects, &st).unwrap();
    create_locked_bucket(Path::new(&f), be.bucket());

    let sg = ReleaseSigner::new(ed25519_dalek::SigningKey::from_bytes(&[3; 32]), MlDsa65SigningKey::from_seed(&[3; 32]));
    let tk = TrustedKeys(vec![sg.trusted()]);
    let p = ReleasePublisher::new(&be, &st, &sg);
    p.publish(rq("b1", 100, &[("bin/weaver", b"v1"), ("shared", b"s")]), 1_000).await.unwrap();
    p.publish(rq("b2", 200, &[("bin/weaver", b"v2"), ("shared", b"s")]), 1_000).await.unwrap();
    let e = p.publish(rq("b1", 100, &[("bin/weaver", b"EVIL")]), 1_000).await.unwrap_err();
    assert!(matches!(e, ReleaseError::AlreadyPublished { .. }), "{e}");

    let sw = Sweeper::new(&be, &st, &tk);
    let pol = RetentionPolicy::new(1, 0).unwrap();
    assert_eq!(sw.sweep(&pol, 1_000, 500, "t").await.unwrap().expired, vec!["b1"]);
    let r = sw.sweep(&pol, 1_600, 500, "t").await.unwrap();
    assert_eq!(r.blobs_deleted.len(), 1, "{r:?}");
    // deleted keys must not show up in listings, or the next sweep would chew on them again
    let r2 = sw.sweep(&pol, 1_700, 500, "t").await.unwrap();
    assert!(r2.blobs_deleted.is_empty() && r2.blobs_restored.is_empty() && r2.expired.is_empty(), "{r2:?}");
    assert!(be.list("quarantine/").await.unwrap().is_empty());
    let rd = ReleaseReader::new(&be, &st, &tk);
    assert_eq!(rd.build_ids().await.unwrap(), vec!["b2"]);
    let m = rd.manifest("b2").await.unwrap();
    assert!(rd.audit(&m).await.unwrap().is_empty());
    // a retired id stays retired even though its manifest key is only hidden by a marker
    let e = p.publish(rq("b1", 100, &[("bin/weaver", b"v1"), ("shared", b"s")]), 1_000).await.unwrap_err();
    assert!(matches!(e, ReleaseError::Retired { .. }), "{e}");
}
