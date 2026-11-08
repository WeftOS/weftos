//! The release store against a THROWAWAY RustFS (never PG). Set `WEFTOS_RELEASE_TEST_RUSTFS_ENV` to an
//! env file (MESH_S3_ENDPOINT / _ACCESS_KEY / _SECRET_KEY) and run with `--ignored`.
#![cfg(feature = "store")]

use std::path::Path;
use std::sync::Arc;

use weftos_pq_sign::MlDsa65SigningKey;
use weftos_release::s3::StoreBackend;
use weftos_release::*;
use weftos_release::{LockMode, LockPolicy};
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

/// The same flow on a versioned, Object-Lock bucket made through S1's `create_locked_bucket`, with
/// a locked publisher and sweeper: a delete is a delete marker, so listings, conditional puts and
/// the locked restore must all still hold, and the server must really hold the retention.
#[tokio::test]
#[ignore = "needs a throwaway RustFS: WEFTOS_RELEASE_TEST_RUSTFS_ENV"]
async fn live_release_store_on_a_locked_versioned_bucket() {
    let f = std::env::var("WEFTOS_RELEASE_TEST_RUSTFS_ENV").expect("WEFTOS_RELEASE_TEST_RUSTFS_ENV");
    let env = Path::new(&f);
    let dir = tempfile::tempdir().unwrap();
    let cfg = RustfsConfig::from_env_file(env, dir.path().join("secrets")).unwrap();
    let objects = Arc::new(tokio::task::spawn_blocking(move || RustfsBackend::new(cfg)).await.unwrap().unwrap());
    let channel = format!("l{}", &ulid::Ulid::new().to_string().to_lowercase()[16..]);
    let st = ReleaseStoreName::new("pg-mesh", ULID, &channel).unwrap();
    let o = objects.clone();
    let st2 = st.clone();
    let bucket = tokio::task::spawn_blocking(move || weftos_release::s3::create_locked_release_bucket(&*o, &st2))
        .await
        .unwrap()
        .unwrap();
    let raw_objects = objects.clone();
    let be = StoreBackend::for_release(objects, &st).unwrap();
    assert_eq!(be.bucket(), bucket);

    let sg = ReleaseSigner::new(ed25519_dalek::SigningKey::from_bytes(&[3; 32]), MlDsa65SigningKey::from_seed(&[3; 32]));
    let tk = TrustedKeys(vec![sg.trusted()]);
    // a real clock: the server refuses a retain-until date in the past
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let pol = LockPolicy::new(LockMode::Compliance, 2 * 86_400).unwrap();
    let p = ReleasePublisher::new(&be, &st, &sg).with_lock(pol);
    let mut r1 = rq("b1", now - 500, &[("bin/weaver", b"v1"), ("shared", b"s")]);
    r1.created_at = now - 500;
    p.publish(r1, now).await.unwrap();
    let mut r2 = rq("b2", now - 400, &[("bin/weaver", b"v2"), ("shared", b"s")]);
    r2.created_at = now - 400;
    p.publish(r2, now).await.unwrap();
    let e = p.publish(rq("b1", now - 500, &[("bin/weaver", b"EVIL")]), now).await.unwrap_err();
    assert!(matches!(e, ReleaseError::AlreadyPublished { .. }), "{e}");

    // the server really holds COMPLIANCE retention on the manifest, to roughly now + 2 days,
    // and refuses to delete that version, bypass or not (read back through S1's own calls)
    let (o, bk) = (raw_objects.clone(), bucket.clone());
    let (ret, del) = tokio::task::spawn_blocking(move || {
        let key = "releases/b1/manifest.json";
        use weftos_store::ObjectBackend as _;
        let v = o.list_versions(&bk, "releases/b1/").unwrap().into_iter().find(|v| v.key == key && !v.is_delete_marker).expect("the manifest version");
        let ret = o.get_retention(&bk, key, &v.version_id).unwrap();
        (ret, o.delete_version(&bk, key, &v.version_id, true))
    })
    .await
    .unwrap();
    let ret = ret.expect("a retention on the manifest");
    assert_eq!(ret.mode, weftos_store::LockMode::Compliance, "{ret:?}");
    assert!(ret.until_unix_s >= now + 2 * 86_400 - 60 && ret.until_unix_s <= now + 2 * 86_400 + 60, "{ret:?}");
    assert!(del.is_err(), "a COMPLIANCE version must refuse delete even with the bypass header: {del:?}");

    // retention: b1 retires (its manifest gets a delete marker; the locked version stays)
    let sw = Sweeper::new(&be, &st, &tk).with_lock(pol);
    let keep = RetentionPolicy::new(1, 0).unwrap();
    assert_eq!(sw.sweep(&keep, now, 500, "t").await.unwrap().expired, vec!["b1"]);
    let r = sw.sweep(&keep, now + 600, 500, "t").await.unwrap();
    assert_eq!(r.blobs_deleted.len(), 1, "{r:?}");
    // deleted keys must not show up in listings, or the next sweep would chew on them again
    let r3 = sw.sweep(&keep, now + 700, 500, "t").await.unwrap();
    assert!(r3.blobs_deleted.is_empty() && r3.blobs_restored.is_empty() && r3.expired.is_empty(), "{r3:?}");
    assert!(be.list("quarantine/").await.unwrap().is_empty());
    let rd = ReleaseReader::new(&be, &st, &tk);
    assert_eq!(rd.build_ids().await.unwrap(), vec!["b2"]);
    let m = rd.manifest("b2").await.unwrap();
    assert!(rd.audit(&m).await.unwrap().is_empty());
    // a retired id stays retired even though its manifest key is only hidden by a marker
    let e = p.publish(rq("b1", now - 500, &[("bin/weaver", b"v1"), ("shared", b"s")]), now).await.unwrap_err();
    assert!(matches!(e, ReleaseError::Retired { .. }), "{e}");

    // version history on the real server: after publishes, a retirement, a sweep and a restore-free
    // collection, nothing is flagged...
    let h = rd.history().await.unwrap();
    assert!(h.checked, "a locked bucket keeps versions");
    assert!(h.findings.is_empty(), "retention is not a replacement: {:?}", h.findings);

    // ...then a principal that can write the key replaces b2's manifest with another validly signed
    // one (the direct CI role can; S1's append role cannot). The plain read trusts it, history does not.
    let scratch = MemoryBackend::new();
    let mut evil = rq("b2", now - 400, &[("bin/weaver", b"EVIL")]);
    evil.created_at = now - 400;
    ReleasePublisher::new(&scratch, &st, &sg).publish(evil, now).await.unwrap();
    let forged = scratch.get("releases/b2/manifest.json").await.unwrap().unwrap();
    let (o, bk) = (raw_objects.clone(), bucket.clone());
    tokio::task::spawn_blocking(move || {
        use weftos_store::ObjectBackend;
        o.put(&bk, "releases/b2/manifest.json", &forged, false)
    })
    .await
    .unwrap()
    .unwrap();
    assert!(rd.manifest("b2").await.is_ok(), "signed, so a plain read accepts the replacement");
    let h = rd.history().await.unwrap();
    assert_eq!(h.findings, vec![HistoryFinding::Replaced { key: "releases/b2/manifest.json".into(), contents: 2 }]);
}

/// S1's lock read-back, extend-retention, lifecycle and multipart, through the S3 adapter on a real
/// locked bucket: the same behaviour the mock tests pin.
#[tokio::test]
#[ignore = "needs a throwaway RustFS: WEFTOS_RELEASE_TEST_RUSTFS_ENV"]
async fn live_s1_seams_lock_read_back_extend_lifecycle_and_multipart() {
    use weftos_release::s3::StoreLifecycle;
    let f = std::env::var("WEFTOS_RELEASE_TEST_RUSTFS_ENV").expect("WEFTOS_RELEASE_TEST_RUSTFS_ENV");
    let dir = tempfile::tempdir().unwrap();
    let cfg = RustfsConfig::from_env_file(Path::new(&f), dir.path().join("secrets")).unwrap();
    let objects = Arc::new(tokio::task::spawn_blocking(move || RustfsBackend::new(cfg)).await.unwrap().unwrap());
    let uniq = || ulid::Ulid::new().to_string().to_lowercase()[16..].to_string();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let sg = ReleaseSigner::new(ed25519_dalek::SigningKey::from_bytes(&[3; 32]), MlDsa65SigningKey::from_seed(&[3; 32]));
    let tk = TrustedKeys(vec![sg.trusted()]);

    // ---- an unlocked bucket: read-back says so and a locked publish writes nothing
    let plain = ReleaseStoreName::new("pg-mesh", ULID, &format!("p{}", uniq())).unwrap();
    let be_plain = StoreBackend::for_release(objects.clone(), &plain).unwrap();
    {
        let (o, b) = (objects.clone(), be_plain.bucket().to_string());
        tokio::task::spawn_blocking(move || o.create_bucket(&b, &[])).await.unwrap().unwrap();
    }
    assert!(!be_plain.lock_config().await.unwrap().unwrap().enabled);
    let pol = |m, s| LockPolicy::new(m, s).unwrap();
    let e = ReleasePublisher::new(&be_plain, &plain, &sg)
        .with_lock(pol(LockMode::Governance, 3_600))
        .publish(rq("b1", now - 5, &[("a", b"x")]), now)
        .await
        .unwrap_err();
    assert!(e.to_string().contains("Object Lock"), "{e}");
    assert!(be_plain.list("").await.unwrap().is_empty(), "nothing written");

    // ---- a locked bucket
    let st = ReleaseStoreName::new("pg-mesh", ULID, &format!("s{}", uniq())).unwrap();
    {
        let (o, s) = (objects.clone(), st.clone());
        tokio::task::spawn_blocking(move || weftos_release::s3::create_locked_release_bucket(&*o, &s)).await.unwrap().unwrap();
    }
    let be = StoreBackend::for_release(objects.clone(), &st).unwrap();
    assert!(be.lock_config().await.unwrap().unwrap().enabled);

    // extend: b1 governance 1h, b2 compliance 3h over the same shared blob
    let p1 = ReleasePublisher::new(&be, &st, &sg).with_lock(pol(LockMode::Governance, 3_600));
    p1.publish(rq("b1", now - 5, &[("a", b"shared")]), now).await.unwrap();
    let blob = format!("blobs/{}", blake3::hash(b"shared").to_hex());
    let first = be.retention_of(&blob).await.unwrap().unwrap();
    assert_eq!(first.mode, LockMode::Governance);
    let p2 = ReleasePublisher::new(&be, &st, &sg).with_lock(pol(LockMode::Compliance, 3 * 3_600));
    p2.publish(rq("b2", now - 4, &[("a", b"shared")]), now).await.unwrap();
    let after = be.retention_of(&blob).await.unwrap().unwrap();
    assert_eq!(after.mode, LockMode::Governance, "the mode is kept: RustFS refuses a change without a bypass");
    assert!(after.until >= now + 3 * 3_600 - 2, "lengthened: {after:?}");
    // a later, shorter, weaker publish changes nothing
    let p3 = ReleasePublisher::new(&be, &st, &sg).with_lock(pol(LockMode::Governance, 60));
    p3.publish(rq("b3", now - 3, &[("a", b"shared")]), now).await.unwrap();
    assert_eq!(be.retention_of(&blob).await.unwrap().unwrap(), after);

    // multipart: an 11 MiB file (two parts) published locked from disk
    let big: Vec<u8> = (0..11 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    let fpath = dir.path().join("big.bin");
    std::fs::write(&fpath, &big).unwrap();
    let fr = FileRequest {
        build_id: "b4".into(), source_rev: "r".into(), created_at: now - 2, builder: "ci".into(),
        artifacts: vec![FileArtifact { path: "bin/big".into(), file: fpath.clone() }],
    };
    let p4 = ReleasePublisher::new(&be, &st, &sg).with_lock(pol(LockMode::Compliance, 3_600));
    p4.publish_files(fr, now).await.unwrap();
    let rd = ReleaseReader::new(&be, &st, &tk);
    let m = rd.manifest("b4").await.unwrap();
    assert_eq!(rd.read_artifact(&m, "bin/big").await.unwrap(), big);
    let bigblob = format!("blobs/{}", blake3::hash(&big).to_hex());
    assert_eq!(be.retention_of(&bigblob).await.unwrap().unwrap().mode, LockMode::Compliance);
    let fr_retry = FileRequest {
        build_id: "b4".into(), source_rev: "r".into(), created_at: now - 2, builder: "ci".into(),
        artifacts: vec![FileArtifact { path: "bin/big".into(), file: fpath }],
    };
    assert_eq!(p4.publish_files(fr_retry, now).await.unwrap().outcome, PublishOutcome::AlreadyPresent);
    assert!(rd.history().await.unwrap().findings.is_empty());

    // lifecycle: set, read back, and the unsupported marker rule refused
    let lc = StoreLifecycle::for_release(objects.clone(), &st).unwrap();
    let rules = LifecycleRules { noncurrent_days: 30, expire_delete_markers: false, abort_multipart_days: Some(7) };
    provision_lifecycle(&lc, &rules).await.unwrap();
    assert_eq!(lc.get_lifecycle().await.unwrap(), Some(rules));
    assert!(provision_lifecycle(&lc, &LifecycleRules { expire_delete_markers: true, ..rules }).await.is_err());
}
