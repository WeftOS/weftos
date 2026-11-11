//! What a NON-ROOT CI key can do on a locked release bucket, against a THROWAWAY RustFS
//! (`WEFTOS_RELEASE_TEST_RUSTFS_ENV`, run with `--ignored`, never PG). Pins what S1's append-level
//! policy allows and what extra actions a direct-to-S3 CI key would need.
#![cfg(feature = "store")]

use std::path::Path;
use std::sync::Arc;

use serde_json::{json, Value};
use weftos_store::backend::{LockMode, PutOpts, Retention};
use weftos_store::{AdminBackend, BackendError, ObjectBackend, RustfsBackend, RustfsConfig, StoreName};

const ULID: &str = "01JBXZ3N5C8QW4M2R7T9V6YH0K";

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

fn policy(bucket: &str, actions: &[&str]) -> Value {
    json!({"Version": "2012-10-17", "Statement": [{
        "Effect": "Allow", "Action": actions,
        "Resource": [format!("arn:aws:s3:::{bucket}"), format!("arn:aws:s3:::{bucket}/*")]}]})
}

fn as_key(cfg: &RustfsConfig, root: &RustfsBackend, id: &str) -> RustfsBackend {
    let secret = std::fs::read_to_string(root.secret_path(id)).unwrap();
    RustfsBackend::new(RustfsConfig { root_access_key: id.into(), root_secret_key: secret, ..cfg.clone() }).unwrap()
}

fn locked(until_in: u64) -> PutOpts {
    PutOpts { if_none_match: true, retention: Some(Retention { mode: LockMode::Compliance, until_unix_s: now() + until_in }), legal_hold: false, ..Default::default() }
}

fn forbidden<T: std::fmt::Debug>(r: Result<T, BackendError>) -> bool {
    matches!(r, Err(BackendError::Forbidden(_)))
}

#[test]
#[ignore = "needs a throwaway RustFS: WEFTOS_RELEASE_TEST_RUSTFS_ENV"]
fn live_a_non_root_ci_key_and_the_lock_headers() {
    let f = std::env::var("WEFTOS_RELEASE_TEST_RUSTFS_ENV").expect("WEFTOS_RELEASE_TEST_RUSTFS_ENV");
    let dir = tempfile::tempdir().unwrap();
    let cfg = RustfsConfig::from_env_file(Path::new(&f), dir.path().join("secrets")).unwrap();
    let root = Arc::new(RustfsBackend::new(cfg.clone()).unwrap());

    let name = StoreName::release("m", ULID, &format!("c{}", &ulid::Ulid::new().to_string().to_lowercase()[16..])).unwrap();
    let b = name.bucket();
    root.create_locked_bucket(&b, &name.tags()).unwrap();

    // S1's append level: read, list, nothing else (weftos_store::policy for Access::Append).
    let read = ["s3:GetObject", "s3:ListBucket", "s3:GetBucketLocation"];
    let try_policy = |label: &str, extra: &[&str], who: &str| -> RustfsBackend {
        let id = root.create_access_key(&format!("cog:ci-{who}")).unwrap().id;
        let acts: Vec<&str> = read.iter().chain(extra.iter()).copied().collect();
        root.put_policy(&id, &policy(&b, &acts)).unwrap();
        eprintln!("policy {label}: {acts:?}");
        as_key(&cfg, &root, &id)
    };

    // 1. the append key as S1 defines it
    let ci = try_policy("append", &[], &format!("a{}", &ulid::Ulid::new().to_string().to_lowercase()[16..]));
    let r = ci.put_locked(&b, "blobs/x1", b"x", &locked(3600));
    eprintln!("append key put_locked: {r:?}");
    assert!(forbidden(r), "S1's append key cannot put, with or without lock headers");

    // 2. PutObject alone
    let ci = try_policy("put", &["s3:PutObject"], &format!("p{}", &ulid::Ulid::new().to_string().to_lowercase()[16..]));
    let r = ci.put_locked(&b, "blobs/x2", b"x", &locked(3600));
    eprintln!("PutObject only, put_locked: {r:?}");
    let put_alone_sends_lock = r.is_ok();

    // 3. PutObject + the retention action
    let ci = try_policy("put+retention", &["s3:PutObject", "s3:PutObjectRetention"], &format!("r{}", &ulid::Ulid::new().to_string().to_lowercase()[16..]));
    let r = ci.put_locked(&b, "blobs/x3", b"x", &locked(3600));
    eprintln!("PutObject + PutObjectRetention, put_locked: {r:?}");
    assert!(r.is_ok(), "with PutObject and PutObjectRetention a non-root key can send the lock headers");
    if !put_alone_sends_lock {
        eprintln!("=> PutObject alone is NOT enough: the lock headers need s3:PutObjectRetention");
    } else {
        eprintln!("=> PutObject alone is enough for the lock headers");
    }

    // 4. what that key can then do to its own locked key
    let over = ci.put(&b, "blobs/x3", b"OVERWRITE", false);
    eprintln!("unconditional overwrite of a locked key: {over:?}");
    let got = root.get(&b, "blobs/x3").unwrap();
    eprintln!("current bytes after the overwrite: {:?}", String::from_utf8_lossy(&got));
    let cond = ci.put(&b, "blobs/x3", b"again", true);
    eprintln!("conditional put on the same key: {cond:?}");
    assert!(matches!(cond, Err(BackendError::Exists(_))), "If-None-Match still holds for a conditional writer");
    let cp = ci.copy(&b, "blobs/x3", &b, "blobs/x3copy");
    eprintln!("copy (needs read on source + put): {cp:?}");
    let del = ci.delete(&b, "blobs/x3");
    eprintln!("delete: {del:?}");
    assert!(forbidden(del), "no DeleteObject in the grant");

    // 5. can IAM be made to refuse the unconditional overwrite? S1 found `s3:if-none-match` rejected.
    for (label, cond_json) in [
        ("Null if-none-match", json!({"Null": {"s3:if-none-match": "true"}})),
        ("StringNotEquals if-none-match", json!({"StringNotEquals": {"s3:if-none-match": "*"}})),
        ("object-lock-mode present", json!({"Null": {"s3:object-lock-mode": "true"}})),
    ] {
        let id = root.create_access_key(&format!("cog:ci-c{}", &ulid::Ulid::new().to_string().to_lowercase()[16..])).unwrap().id;
        let acts: Vec<&str> = read.iter().chain(["s3:PutObject", "s3:PutObjectRetention"].iter()).copied().collect();
        let pol = json!({"Version": "2012-10-17", "Statement": [
            {"Effect": "Allow", "Action": acts, "Resource": [format!("arn:aws:s3:::{b}"), format!("arn:aws:s3:::{b}/*")]},
            {"Effect": "Deny", "Action": ["s3:PutObject"], "Resource": [format!("arn:aws:s3:::{b}/*")], "Condition": cond_json}]});
        let stored = root.put_policy(&id, &pol);
        eprintln!("condition {label}: policy accepted = {}", stored.is_ok());
        if stored.is_ok() {
            let k = as_key(&cfg, &root, &id);
            eprintln!("  conditional+locked put: {:?}", k.put_locked(&b, &format!("blobs/c-{label}").replace(' ', "-"), b"z", &locked(3600)).map(|_| ()));
            eprintln!("  unconditional plain put: {:?}", k.put(&b, &format!("blobs/p-{label}").replace(' ', "-"), b"z", false).map(|_| ()));
        }
    }

    // 6. the direct CI role this crate proposes: read, put, retention, and a Deny on any put that
    //    carries no lock mode. No DeleteObject.
    let id = root.create_access_key(&format!("cog:ci-d{}", &ulid::Ulid::new().to_string().to_lowercase()[16..])).unwrap().id;
    root.put_policy(&id, &weftos_release::s3::direct_ci_policy(&b)).unwrap();
    let d = as_key(&cfg, &root, &id);
    let first = d.put_locked(&b, "d/one", b"v1", &locked(3600));
    eprintln!("direct role: locked conditional put: {:?}", first.as_ref().map(|_| ()));
    assert!(first.is_ok());
    assert!(forbidden(d.put(&b, "d/two", b"x", true)), "a put with no lock headers is refused");
    assert!(forbidden(d.put(&b, "d/two", b"x", false)), "a plain overwrite is refused");
    assert!(matches!(d.put_locked(&b, "d/one", b"v2", &locked(3600)), Err(BackendError::Exists(_))), "conditional locked put on an existing key: Exists");
    let unconditional = PutOpts { if_none_match: false, ..locked(3600) };
    let over = d.put_locked(&b, "d/one", b"EVIL", &unconditional);
    eprintln!("direct role: UNCONDITIONAL locked put over an existing locked key: {:?}", over.as_ref().map(|_| ()));
    // Pinned as a known limit, not a feature: if RustFS ever refuses this, the plan can say so.
    assert!(over.is_ok(), "the direct role can replace a key with a locked unconditional put");
    assert_eq!(root.get(&b, "d/one").unwrap(), b"EVIL");
    let cp = d.copy(&b, "d/one", &b, "d/copy");
    eprintln!("direct role: copy: {cp:?}");
    assert!(forbidden(cp), "a copy carries no lock mode, so the Deny refuses it");
    assert!(forbidden(d.delete(&b, "d/one")), "no delete");
}
