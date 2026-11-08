use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::*;
use crate::audit::MemSink;
use crate::service::{Caller, ChainSink, Config, ExposureGate, Parties, StoreService};
use crate::{Access, Principal, StoreName};

#[test]
fn xml_helpers_unescape_and_walk_blocks() {
    let x = "<R><Contents><Key>a&amp;b &lt;1&gt;</Key><Size>5</Size></Contents><Contents><Key>c</Key></Contents></R>";
    let b = xml_blocks(x, "Contents");
    assert_eq!(b.len(), 2);
    assert_eq!(xml_tag(b[0], "Key").unwrap(), "a&b <1>");
    assert_eq!(xml_tag(b[0], "Size").unwrap(), "5");
    assert!(xml_tag(b[1], "Size").is_none());
}

#[test]
fn config_reads_an_env_file_and_defaults_the_region() {
    let d = tempfile::tempdir().unwrap();
    let f = d.path().join("c.env");
    std::fs::write(&f, "# c\nMESH_S3_ENDPOINT=http://h:9000\nMESH_S3_ACCESS_KEY=\"ak\"\nMESH_S3_SECRET_KEY=sk\n").unwrap();
    let c = RustfsConfig::from_env_file(&f, d.path().join("s")).unwrap();
    assert_eq!((c.endpoint.as_str(), c.root_access_key.as_str(), c.region.as_str()), ("http://h:9000", "ak", "us-east-1"));
    std::fs::write(&f, "MESH_S3_ENDPOINT=http://h\n").unwrap();
    assert!(RustfsConfig::from_env_file(&f, d.path().join("s")).is_err());
}

#[test]
fn key_ids_are_stable_per_principal() {
    let (a, b) = (RustfsBackend::key_id_for("cog:c1"), RustfsBackend::key_id_for("cog:c2"));
    assert_eq!(a, RustfsBackend::key_id_for("cog:c1"));
    assert_ne!(a, b);
    assert!(a.starts_with("wk-") && a.len() == 19);
}

// ---- against a real RustFS ------------------------------------------------------------------
// Set WEFTOS_STORE_TEST_RUSTFS_ENV to an env file (MESH_S3_ENDPOINT / _ACCESS_KEY / _SECRET_KEY)
// of a THROWAWAY RustFS and run with `--ignored`. Never point it at PG.

fn live() -> Option<(RustfsConfig, tempfile::TempDir)> {
    let f = std::env::var("WEFTOS_STORE_TEST_RUSTFS_ENV").ok()?;
    let d = tempfile::tempdir().unwrap();
    Some((RustfsConfig::from_env_file(Path::new(&f), d.path().join("secrets")).unwrap(), d))
}

fn as_key(cfg: &RustfsConfig, be: &RustfsBackend, key_id: &str) -> RustfsBackend {
    let secret = std::fs::read_to_string(be.secret_path(key_id)).unwrap();
    RustfsBackend::new(RustfsConfig { root_access_key: key_id.into(), root_secret_key: secret, ..cfg.clone() }).unwrap()
}

fn uniq(p: &str) -> String {
    format!("{p}-{}", &ulid::Ulid::new().to_string().to_lowercase()[10..])
}

#[test]
#[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
fn live_object_and_bucket_operations() {
    let (cfg, _d) = live().expect("WEFTOS_STORE_TEST_RUSTFS_ENV");
    let be = RustfsBackend::new(cfg).unwrap();
    let b = uniq("t-obj");
    be.create_bucket(&b, &[("store".into(), "node & co".into()), ("node".into(), "n1".into())]).unwrap();
    assert!(matches!(be.create_bucket(&b, &[]), Err(BackendError::BucketExists(_))));
    assert!(be.bucket_exists(&b).unwrap());
    let tags = be.buckets().unwrap().into_iter().find(|(n, _)| *n == b).unwrap().1;
    assert!(tags.contains(&("store".into(), "node & co".into())));

    let m = be.put(&b, "a/b c&d.txt", b"hello", true).unwrap();
    assert_eq!(m.size, 5);
    assert!(matches!(be.put(&b, "a/b c&d.txt", b"again", true), Err(BackendError::Exists(_))));
    be.put(&b, "a/b c&d.txt", b"replaced", false).unwrap();
    assert_eq!(be.get(&b, "a/b c&d.txt").unwrap(), b"replaced");
    assert_eq!(be.head(&b, "a/b c&d.txt").unwrap().size, 8);
    assert!(matches!(be.get(&b, "nope"), Err(BackendError::NoObject(_))));
    be.put(&b, "z", b"", false).unwrap();
    be.copy(&b, "a/b c&d.txt", &b, "copy/ü.txt").unwrap();
    let l = be.list(&b, "").unwrap();
    assert_eq!(l.iter().map(|m| m.key.as_str()).collect::<Vec<_>>(), ["a/b c&d.txt", "copy/ü.txt", "z"]);
    assert_eq!(be.list(&b, "copy/").unwrap().len(), 1);
    assert!(matches!(be.put("no-such-bucket-x", "k", b"v", false), Err(BackendError::NoBucket(_))));
    assert!(be.delete_bucket(&b).is_err(), "not empty");
    for m in l {
        be.delete(&b, &m.key).unwrap();
    }
    be.delete_bucket(&b).unwrap();
    assert!(!be.bucket_exists(&b).unwrap());
}

#[test]
#[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
fn live_listing_pages_past_a_thousand_keys() {
    let (cfg, _d) = live().expect("WEFTOS_STORE_TEST_RUSTFS_ENV");
    let be = RustfsBackend::new(cfg).unwrap();
    let b = uniq("t-page");
    be.create_bucket(&b, &[]).unwrap();
    for i in 0..1005 {
        be.put(&b, &format!("k/{i:05}"), b"x", false).unwrap();
    }
    assert_eq!(be.list(&b, "k/").unwrap().len(), 1005);
    for i in 0..1005 {
        be.delete(&b, &format!("k/{i:05}")).unwrap();
    }
    be.delete_bucket(&b).unwrap();
}

#[test]
#[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
fn live_a_key_is_scoped_then_disabled_then_refused() {
    let (cfg, _d) = live().expect("WEFTOS_STORE_TEST_RUSTFS_ENV");
    let be = RustfsBackend::new(cfg.clone()).unwrap();
    let (mine, other) = (uniq("t-mine"), uniq("t-other"));
    be.create_bucket(&mine, &[]).unwrap();
    be.create_bucket(&other, &[]).unwrap();
    let k = be.create_access_key(&uniq("cog:scoped")).unwrap();
    assert_eq!(be.create_access_key("cog:x").unwrap().id, be.create_access_key("cog:x").unwrap().id);
    let secret = std::fs::read_to_string(be.secret_path(&k.id)).unwrap();
    assert_eq!(be.create_access_key("cog:x").map(|_| ()), Ok(()), "idempotent");

    let user = as_key(&cfg, &be, &k.id);
    // No policy yet: nothing is allowed.
    assert!(matches!(user.put(&mine, "k", b"v", false), Err(BackendError::Forbidden(_))));

    let allow = |b: &str| json!({"Version": "2012-10-17", "Statement": [{"Effect": "Allow",
        "Action": ["s3:GetObject", "s3:PutObject", "s3:ListBucket"], "Resource": [format!("arn:aws:s3:::{b}"), format!("arn:aws:s3:::{b}/*")]}]});
    be.put_policy(&k.id, &allow(&mine)).unwrap();
    user.put(&mine, "k", b"v", false).unwrap();
    assert_eq!(user.get(&mine, "k").unwrap(), b"v");
    assert!(matches!(user.put(&other, "k", b"v", false), Err(BackendError::Forbidden(_))), "other bucket is refused");

    be.set_key_enabled(&k.id, false).unwrap();
    assert!(matches!(user.get(&mine, "k"), Err(BackendError::Forbidden(_))), "disabled key: next request refused");
    // Re-creating the key must not re-enable it, and must keep the secret.
    be.create_access_key("cog:scoped-again").unwrap();
    assert!(matches!(user.get(&mine, "k"), Err(BackendError::Forbidden(_))));
    assert_eq!(std::fs::read_to_string(be.secret_path(&k.id)).unwrap(), secret);
    be.set_key_enabled(&k.id, true).unwrap();
    assert_eq!(user.get(&mine, "k").unwrap(), b"v");

    // No enabled grants: the empty policy becomes deny-all.
    be.put_policy(&k.id, &json!({"Version": "2012-10-17", "Statement": []})).unwrap();
    assert!(matches!(user.get(&mine, "k"), Err(BackendError::Forbidden(_))));

    be.delete_access_key(&k.id).unwrap();
    assert!(matches!(user.get(&mine, "k"), Err(BackendError::Forbidden(_))));
    assert!(!be.secret_path(&k.id).exists());
    be.delete(&mine, "k").unwrap();
    be.delete_bucket(&mine).unwrap();
    be.delete_bucket(&other).unwrap();
}

struct Open;
impl ExposureGate for Open {
    fn register(&self, _: &str, _: &str) {}
    fn check(&self, _: &str) -> Result<(), String> {
        Ok(())
    }
}
#[derive(Default)]
struct Chain(Mutex<Vec<(String, Value)>>);
impl ChainSink for Chain {
    fn record(&self, k: &str, v: Value) {
        self.0.lock().unwrap().push((k.to_owned(), v));
    }
}

/// S1 done-when, against a real RustFS: the service creates a node store and a project store, a
/// cog's write shows in the activity log, and disabling the grant refuses the next request, both
/// at the service and at RustFS itself for the cog's own key.
#[test]
#[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
fn live_service_done_when() {
    let (cfg, _d) = live().expect("WEFTOS_STORE_TEST_RUSTFS_ENV");
    let be = Arc::new(RustfsBackend::new(cfg.clone()).unwrap());
    let sink = Arc::new(MemSink::default());
    let node_id = uniq("n").replace('_', "-");
    let svc = StoreService::open(
        Config { node_id: node_id.clone(), state_path: None, clock: || 1_000 },
        be.clone(),
        be.clone(),
        Arc::new(Open),
        sink.clone(),
        Arc::new(Chain::default()),
    )
    .unwrap();
    let node = StoreName::node("weft", &node_id).unwrap();
    let project = StoreName::project("weft", &ulid::Ulid::new().to_string()).unwrap();
    svc.ensure_store(&node, "daemon", Parties::defaults_for(&node)).unwrap();
    svc.create_store(&project, "daemon", Parties::default()).unwrap();
    assert!(be.bucket_exists(&node.bucket()).unwrap() && be.bucket_exists(&project.bucket()).unwrap());

    let cog = Principal::parse("cog:c1").unwrap();
    svc.grant(&project, cog.clone(), Access::Write, "daemon").unwrap();
    let who = Caller::local(cog.clone());
    svc.put(&who, &project, "notes/a.txt", b"hello", false).unwrap();
    assert_eq!(svc.get(&who, &project, "notes/a.txt").unwrap(), b"hello");
    assert!(sink.events().iter().any(|e| e.op == "PutObject" && e.principal == "cog:c1"), "the cog's write is in the activity log");

    // The cog's own key works at RustFS while the grant is on.
    let key = RustfsBackend::key_id_for(&cog.to_string());
    let direct = as_key(&cfg, &be, &key);
    assert_eq!(direct.get(&project.bucket(), "notes/a.txt").unwrap(), b"hello");

    svc.set_grant_enabled(&project, &cog, false, "owner").unwrap();
    assert!(svc.get(&who, &project, "notes/a.txt").is_err(), "service refuses the next request");
    assert!(matches!(direct.get(&project.bucket(), "notes/a.txt"), Err(BackendError::Forbidden(_))), "RustFS refuses the key too");

    // Clean up.
    be.delete(&project.bucket(), "notes/a.txt").unwrap();
    let _ = be.delete_access_key(&key);
    for s in [&node, &project] {
        let _ = be.delete_bucket(&s.bucket());
    }
}

#[test]
fn the_notification_body_names_the_target_and_a_bad_target_is_refused() {
    let x = crate::rustfs::notification_xml("primary");
    assert!(x.contains("<Queue>arn:rustfs:sqs::primary:webhook</Queue>") && x.contains("s3:ObjectCreated:*") && x.contains("s3:ObjectRemoved:*"));
    let be = RustfsBackend::new(RustfsConfig { endpoint: "http://127.0.0.1:1".into(), region: "us-east-1".into(), root_access_key: "a".into(), root_secret_key: "b".into(), secrets_dir: std::env::temp_dir(), timeout: std::time::Duration::from_secs(1) }).unwrap();
    assert!(matches!(be.put_bucket_notification("b", "x/y"), Err(BackendError::BadKey(_))));
    assert!(matches!(be.put_bucket_notification("b", ""), Err(BackendError::BadKey(_))));
}

#[test]
#[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
fn live_put_if_match_is_a_compare_and_swap() {
    let (cfg, _d) = live().expect("WEFTOS_STORE_TEST_RUSTFS_ENV");
    let be = RustfsBackend::new(cfg).unwrap();
    let b = uniq("t-cas");
    be.create_bucket(&b, &[]).unwrap();
    assert!(matches!(be.put_if_match(&b, "k", b"x", "abc"), Err(BackendError::NoObject(_))), "missing key");
    let v1 = be.put(&b, "k", b"one", false).unwrap();
    let v2 = be.put_if_match(&b, "k", b"two", &v1.etag).unwrap();
    assert!(matches!(be.put_if_match(&b, "k", b"three", &v1.etag), Err(BackendError::Stale(_))), "stale etag");
    assert_eq!(be.get(&b, "k").unwrap(), b"two");
    be.put_if_match(&b, "k", b"three", &v2.etag).unwrap();
    assert_eq!(be.get(&b, "k").unwrap(), b"three");
    be.delete(&b, "k").unwrap();
    be.delete_bucket(&b).unwrap();
}
