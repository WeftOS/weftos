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
fn version_blocks_come_out_in_document_order_with_markers_between() {
    let x = "<R><Version><Key>a</Key><VersionId>2</VersionId></Version><DeleteMarker><Key>a</Key><VersionId>3</VersionId></DeleteMarker><Version><Key>b</Key></Version></R>";
    let b: Vec<_> = xml_blocks_in_order(x, &["Version", "DeleteMarker"]).into_iter().map(|(t, c)| (t, xml_tag(c, "Key").unwrap())).collect();
    assert_eq!(b, vec![("Version", "a".to_owned()), ("DeleteMarker", "a".to_owned()), ("Version", "b".to_owned())]);
}

fn xport(never_sent: bool, timed_out: bool) -> Xport {
    Xport { msg: "x".into(), never_sent, timed_out }
}

#[test]
fn only_what_is_safe_to_repeat_is_retried() {
    let no_q: &[(String, String)] = &[];
    let ver = vec![("versionId".to_owned(), "v".to_owned())];
    // The server says it did not act: any request.
    for m in ["GET", "PUT", "DELETE", "POST"] {
        for st in [503, 429, 408] {
            assert!(should_retry(m, no_q, &Ok(st)), "{m} {st}");
        }
    }
    // Other 5xx: reads and plain deletes only; a write may have landed.
    for st in [500, 502, 504] {
        assert!(should_retry("GET", no_q, &Ok(st)) && should_retry("HEAD", no_q, &Ok(st)) && should_retry("DELETE", no_q, &Ok(st)));
        assert!(!should_retry("PUT", no_q, &Ok(st)) && !should_retry("POST", no_q, &Ok(st)), "a write is not repeated on {st}");
        assert!(!should_retry("DELETE", &ver, &Ok(st)), "deleting a named version is the caller's to reissue");
    }
    // Answers that are final.
    for st in [200, 204, 400, 403, 404, 409, 412] {
        assert!(!should_retry("GET", no_q, &Ok(st)), "{st} is an answer");
    }
    // Transport failures: a connection that never opened is always safe; otherwise only a repeatable
    // request, and never after a timeout (the work may still be running).
    assert!(should_retry("PUT", no_q, &Err(&xport(true, false))));
    assert!(!should_retry("PUT", no_q, &Err(&xport(false, false))), "a dropped write may have landed");
    assert!(should_retry("GET", no_q, &Err(&xport(false, false))));
    assert!(!should_retry("GET", no_q, &Err(&xport(false, true))), "a timeout is not retried");
}

#[test]
fn waits_grow_fourfold_and_stop_at_five_seconds() {
    let r = Retry { attempts: 9, base: Duration::from_millis(100) };
    let w: Vec<u128> = (1..=5).map(|n| r.wait_after(n).as_millis()).collect();
    assert_eq!(w, vec![100, 400, 1600, 5000, 5000]);
    assert_eq!(Retry::default().attempts, 3);
}

#[test]
fn an_unreachable_backend_is_retried_then_reported_as_unavailable() {
    let cfg = |d: &Path| RustfsConfig { endpoint: "http://127.0.0.1:1".into(), region: "us-east-1".into(), root_access_key: "a".into(), root_secret_key: "b".into(), secrets_dir: d.join("s"), timeout: Duration::from_secs(2) };
    let d = tempfile::tempdir().unwrap();
    let t = std::time::Instant::now();
    let slow = RustfsBackend::new(cfg(d.path())).unwrap().with_retry(Retry { attempts: 3, base: Duration::from_millis(40) });
    let e = slow.get("bkt", "k").unwrap_err();
    assert!(matches!(e, BackendError::Unavailable(_)) && e.is_transient(), "{e:?}");
    assert!(t.elapsed() >= Duration::from_millis(40 + 160), "two waits happened: {:?}", t.elapsed());
    // A write to a port nothing listens on never opened a connection, so it is retried too.
    let t = std::time::Instant::now();
    assert!(slow.put("bkt", "k", b"v", false).unwrap_err().is_transient());
    assert!(t.elapsed() >= Duration::from_millis(200));
    // Off: one try, no waiting.
    let once = RustfsBackend::new(cfg(d.path())).unwrap().with_retry(Retry { attempts: 1, base: Duration::from_secs(9) });
    let t = std::time::Instant::now();
    assert!(once.get("bkt", "k").unwrap_err().is_transient());
    assert!(t.elapsed() < Duration::from_secs(3));
}

#[test]
fn http_statuses_map_to_the_error_that_says_what_to_do() {
    let be = RustfsBackend::new(RustfsConfig { endpoint: "http://127.0.0.1:1".into(), region: "r".into(), root_access_key: "a".into(), root_secret_key: "b".into(), secrets_dir: tempfile::tempdir().unwrap().keep(), timeout: Duration::from_secs(1) }).unwrap();
    let resp = |status, code: &str| Resp { status, etag: None, len: None, version: None, sha256: None, exec: false, body: format!("<Error><Code>{code}</Code><Message>m</Message></Error>").into_bytes() };
    let ob = Ctx::Object("b", "k");
    assert!(matches!(be.check(resp(401, "InvalidAccessKeyId"), ob), Err(BackendError::Forbidden(_))));
    assert!(matches!(be.check(resp(403, "AccessDenied"), ob), Err(BackendError::Forbidden(_))));
    for st in [408, 429, 500, 502, 503, 504] {
        assert!(be.check(resp(st, "SlowDown"), ob).unwrap_err().is_transient(), "{st}");
    }
    assert!(matches!(be.check(resp(404, "NoSuchKey"), ob), Err(BackendError::NoObject(_))));
    assert!(matches!(be.check(resp(404, "NoSuchBucket"), ob), Err(BackendError::NoBucket(_))));
    assert!(matches!(be.check(resp(412, "PreconditionFailed"), ob), Err(BackendError::Exists(_))));
    assert!(matches!(be.check(resp(400, "InvalidArgument"), ob), Err(BackendError::Io(_))), "a bad request is not transient");
    assert!(!be.check(resp(400, "InvalidArgument"), ob).unwrap_err().is_transient());
}

#[test]
fn an_object_lock_configuration_document_is_read_back() {
    let on = "<ObjectLockConfiguration><ObjectLockEnabled>Enabled</ObjectLockEnabled></ObjectLockConfiguration>";
    assert_eq!(parse_lock_config(on).unwrap(), LockConfig { enabled: true, default: None });
    let d = "<ObjectLockConfiguration><ObjectLockEnabled>Enabled</ObjectLockEnabled><Rule><DefaultRetention><Mode>COMPLIANCE</Mode><Years>2</Years></DefaultRetention></Rule></ObjectLockConfiguration>";
    assert_eq!(parse_lock_config(d).unwrap().default, Some(DefaultRetention { mode: LockMode::Compliance, days: None, years: Some(2) }));
    let g = "<ObjectLockConfiguration><ObjectLockEnabled>Enabled</ObjectLockEnabled><Rule><DefaultRetention><Mode>GOVERNANCE</Mode><Days>30</Days></DefaultRetention></Rule></ObjectLockConfiguration>";
    assert_eq!(parse_lock_config(g).unwrap().default, Some(DefaultRetention { mode: LockMode::Governance, days: Some(30), years: None }));
    assert!(parse_lock_config("<ObjectLockConfiguration><Rule><DefaultRetention><Mode>NOPE</Mode></DefaultRetention></Rule></ObjectLockConfiguration>").is_err());
    assert!(!parse_lock_config("<ObjectLockConfiguration></ObjectLockConfiguration>").unwrap().enabled);
}

fn rule(id: &str, prefix: &str) -> LifecycleRule {
    LifecycleRule { id: id.into(), prefix: prefix.into(), enabled: true, expire_days: Some(30), noncurrent_days: None, abort_multipart_days: None }
}

#[test]
fn lifecycle_rules_are_checked_written_and_read_back_as_documents() {
    let all = LifecycleRule { id: "a&b".into(), prefix: "tmp/<x>/".into(), enabled: false, expire_days: Some(7), noncurrent_days: Some(3), abort_multipart_days: Some(1) };
    let x = lifecycle_xml(&[all.clone(), rule("two", "")]);
    assert!(x.contains("<ID>a&amp;b</ID>") && x.contains("<Prefix>tmp/&lt;x&gt;/</Prefix>") && x.contains("<Status>Disabled</Status>"));
    assert_eq!(parse_lifecycle(&x), vec![all, rule("two", "")], "what is written is what is read");
    // A bare <Prefix> (older documents) still reads.
    assert_eq!(parse_lifecycle("<LifecycleConfiguration><Rule><ID>o</ID><Prefix>old/</Prefix><Status>Enabled</Status><Expiration><Days>2</Days></Expiration></Rule></LifecycleConfiguration>")[0].prefix, "old/");
    // Validation.
    assert!(rule("", "").validate().is_err());
    assert!(rule(&"x".repeat(256), "").validate().is_err());
    assert!(LifecycleRule { expire_days: None, ..rule("n", "") }.validate().is_err(), "no action");
    assert!(LifecycleRule { expire_days: Some(0), ..rule("z", "") }.validate().is_err(), "zero days");
    assert!(rule("ok", "p/").validate().is_ok());
}

fn breaker(threshold: u32) -> BreakerState {
    BreakerState { cfg: Breaker { threshold, cooldown: Duration::from_secs(10) }, ..Default::default() }
}

#[test]
fn the_breaker_opens_after_a_run_of_failures_probes_once_and_closes_on_any_answer() {
    let t0 = Instant::now();
    let s = Duration::from_secs;
    let mut b = breaker(3);
    // Two failures then a success: not open, and the count starts over.
    b.record(true, t0);
    b.record(true, t0);
    b.record(false, t0);
    b.record(true, t0);
    b.record(true, t0);
    assert!(b.admit(t0).is_ok() && !b.is_open(t0), "a success reset the run");
    // Third in a row opens it for the cooldown.
    b.record(true, t0);
    assert!(b.is_open(t0));
    assert_eq!(b.admit(t0 + s(4)), Err(s(6)), "fails fast, says how long");
    // After the cooldown exactly one probe gets through.
    assert!(b.admit(t0 + s(10)).is_ok());
    assert!(b.admit(t0 + s(10)).is_err(), "others wait for the probe");
    assert!(b.is_open(t0 + s(10)));
    // The probe fails: open again for a full cooldown from then.
    b.record(true, t0 + s(11));
    assert_eq!(b.admit(t0 + s(12)), Err(s(9)));
    // The next probe gets an answer: closed, everything flows.
    assert!(b.admit(t0 + s(21)).is_ok());
    b.record(false, t0 + s(21));
    assert!(!b.is_open(t0 + s(21)) && b.admit(t0 + s(21)).is_ok() && b.admit(t0 + s(21)).is_ok());
    // Off: never opens.
    let mut off = breaker(0);
    for _ in 0..50 {
        off.record(true, t0);
    }
    assert!(off.admit(t0).is_ok() && !off.is_open(t0));
}

#[test]
fn a_short_views_timeout_is_not_news_about_rustfs_and_a_lost_probe_is_given_up_on() {
    // An impatient caller's timeout feeds nothing; a connect failure or a default-deadline timeout is down.
    assert_eq!(breaker_feed(&Err(&xport(false, true)), true), Feed::Neutral);
    assert_eq!(breaker_feed(&Err(&xport(false, true)), false), Feed::Down);
    assert_eq!(breaker_feed(&Err(&xport(true, false)), true), Feed::Down, "no connection is down whichever view saw it");
    assert_eq!(breaker_feed(&Ok(503), true), Feed::Down);
    assert_eq!(breaker_feed(&Ok(404), true), Feed::Up);
    // Five neutral results never open it.
    let t0 = Instant::now();
    let mut b = breaker(3);
    for _ in 0..5 {
        b.release_probe();
    }
    assert!(!b.is_open(t0) && b.admit(t0).is_ok());
    // A probe whose thread died never reports back: after a cooldown another is let through.
    b.record(true, t0);
    b.record(true, t0);
    b.record(true, t0);
    let after = t0 + Duration::from_secs(10);
    assert!(b.admit(after).is_ok(), "the probe");
    assert!(b.admit(after + Duration::from_secs(5)).is_err(), "still waiting for it");
    assert!(b.admit(after + Duration::from_secs(10)).is_ok(), "given up on, a new probe");
    // A probe that ended with a local fault frees the slot without closing or counting.
    b.release_probe();
    let later = after + Duration::from_secs(10);
    assert!(!b.is_open(later), "half-open: the fault neither closed it nor counted");
    assert!(b.admit(later).is_ok(), "slot free again");
    assert!(b.admit(later).is_err(), "and only one probe at a time");
}

#[test]
fn a_retain_until_must_be_in_the_future_and_inside_year_9999() {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    assert!(check_until(now + 60).is_ok() && check_until(sigv4::MAX_ISO_SECS).is_ok());
    assert!(check_until(now).is_err() && check_until(0).is_err());
    let e = check_until(sigv4::MAX_ISO_SECS + 1).unwrap_err().to_string();
    assert!(e.contains("9999"), "{e}");
    assert!(check_until(u64::MAX).is_err());
}

#[test]
fn only_an_unreachable_rustfs_counts_toward_the_breaker() {
    assert!(counts_as_down(&Err(&xport(true, false))) && counts_as_down(&Err(&xport(false, true))));
    for st in [502, 503, 504] {
        assert!(counts_as_down(&Ok(st)), "{st}");
    }
    for st in [200, 400, 403, 404, 408, 412, 429, 500] {
        assert!(!counts_as_down(&Ok(st)), "{st} is an answer: RustFS is up");
    }
}

#[test]
fn a_dead_rustfs_fails_fast_after_the_breaker_opens_and_a_short_timeout_view_shares_it() {
    let d = tempfile::tempdir().unwrap();
    let cfg = RustfsConfig { endpoint: "http://127.0.0.1:1".into(), region: "us-east-1".into(), root_access_key: "a".into(), root_secret_key: "b".into(), secrets_dir: d.path().join("s"), timeout: Duration::from_secs(5) };
    let be = RustfsBackend::new(cfg).unwrap().with_retry(Retry { attempts: 1, base: Duration::ZERO }).with_breaker(Breaker { threshold: 3, cooldown: Duration::from_secs(30) });
    let quick = be.with_timeout(Duration::from_millis(200));
    for i in 0..3 {
        assert!(!be.breaker_open(), "closed before failure {i}");
        assert!(be.get("bkt", "k").unwrap_err().is_transient());
    }
    assert!(be.breaker_open() && quick.breaker_open(), "all views see it");
    let t = Instant::now();
    let e = quick.get("bkt", "k").unwrap_err();
    assert!(e.is_transient() && e.to_string().contains("circuit open"), "{e}");
    assert!(t.elapsed() < Duration::from_millis(50), "no connection attempt: {:?}", t.elapsed());
    assert!(be.put("bkt", "k", b"v", false).unwrap_err().to_string().contains("circuit open"));
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
    let d = tempfile::tempdir().unwrap();
    let be = RustfsBackend::new(RustfsConfig { endpoint: "http://127.0.0.1:1".into(), region: "us-east-1".into(), root_access_key: "a".into(), root_secret_key: "b".into(), secrets_dir: d.path().join("s"), timeout: std::time::Duration::from_secs(1) }).unwrap();
    assert!(matches!(be.put_bucket_notification("bkt", "x/y"), Err(BackendError::BadKey(_))));
    assert!(matches!(be.put_bucket_notification("bkt", ""), Err(BackendError::BadKey(_))));
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

/// The release-store roles, live. CI (`append`) writes once through the service; its own key at
/// RustFS can only read (no put, copy or delete). The sweeper (`sweep`) key may read and delete
/// but not put.
#[test]
#[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
fn live_release_roles_ci_appends_and_the_sweeper_only_deletes() {
    use crate::service::{CI_PRINCIPAL, SWEEPER_PRINCIPAL};
    let (cfg, _d) = live().expect("WEFTOS_STORE_TEST_RUSTFS_ENV");
    let be = Arc::new(RustfsBackend::new(cfg.clone()).unwrap());
    let svc = StoreService::open(
        Config { node_id: uniq("n").replace('_', "-"), state_path: None, clock: || 1_000 },
        be.clone(),
        be.clone(),
        Arc::new(Open),
        Arc::new(MemSink::default()),
        Arc::new(Chain::default()),
    )
    .unwrap();
    let rel = StoreName::release("weft", &ulid::Ulid::new().to_string(), "stable").unwrap();
    // Well-known roles with a unique suffix, so tests sharing one RustFS do not share keys.
    let (ci_p, sw_p) = (format!("{CI_PRINCIPAL}-{}", &ulid::Ulid::new().to_string().to_lowercase()[16..]), format!("{SWEEPER_PRINCIPAL}-{}", &ulid::Ulid::new().to_string().to_lowercase()[16..]));
    let parties = Parties { appenders: vec![Principal::parse(&ci_p).unwrap()], sweepers: vec![Principal::parse(&sw_p).unwrap()], ..Default::default() };
    svc.create_store(&rel, "daemon", parties).unwrap();
    let b = rel.bucket();
    let ci = as_key(&cfg, &be, &RustfsBackend::key_id_for(&ci_p));
    let sw = as_key(&cfg, &be, &RustfsBackend::key_id_for(&sw_p));

    // CI creates an artifact through the service (write-once), and reads it with its own key.
    let ci_caller = Caller::local(Principal::parse(&ci_p).unwrap());
    svc.put(&ci_caller, &rel, "v1/app.tar", b"build", true).unwrap();
    assert!(matches!(svc.put(&ci_caller, &rel, "v1/app.tar", b"evil", false), Err(crate::StoreError::Refused(_))), "service: overwrite refused");
    assert!(matches!(svc.delete(&ci_caller, &rel, "v1/app.tar"), Err(crate::StoreError::Refused(_))), "service: delete refused");
    assert_eq!(ci.get(&b, "v1/app.tar").unwrap(), b"build");
    // With its own key straight at RustFS, CI is refused put (so no overwrite), copy and delete.
    assert!(matches!(ci.put(&b, "v1/app.tar", b"evil", false), Err(BackendError::Forbidden(_))), "append key: overwrite refused at RustFS");
    assert!(matches!(ci.put(&b, "v1/other.tar", b"x", true), Err(BackendError::Forbidden(_))), "append key: even a create is the daemon's job");
    assert!(matches!(ci.copy(&b, "v1/app.tar", &b, "v1/copy.tar"), Err(BackendError::Forbidden(_))), "append key: copy refused at RustFS");
    assert!(matches!(ci.delete(&b, "v1/app.tar"), Err(BackendError::Forbidden(_))), "append key: delete refused at RustFS");
    assert_eq!(ci.get(&b, "v1/app.tar").unwrap(), b"build", "the artifact is unchanged");

    // The sweeper reads and lists, cannot put, and can delete.
    assert!(sw.list(&b, "v1/").unwrap().iter().any(|m| m.key == "v1/app.tar"));
    assert_eq!(sw.get(&b, "v1/app.tar").unwrap(), b"build");
    assert!(matches!(sw.put(&b, "v1/new", b"x", false), Err(BackendError::Forbidden(_))), "sweep: put refused");
    assert!(matches!(sw.put(&b, "v1/new", b"x", true), Err(BackendError::Forbidden(_))), "sweep: conditional put refused");
    sw.delete(&b, "v1/app.tar").unwrap();
    assert!(be.list(&b, "").unwrap().iter().all(|m| m.key != "v1/app.tar"), "the sweeper's delete worked");

    for m in be.list(&b, "").unwrap() {
        be.delete(&b, &m.key).unwrap();
    }
    let _ = be.delete_bucket(&b);
}

/// Object Lock, live: a locked bucket, retention and legal-hold headers on put, and what RustFS
/// does when a locked version is deleted.
#[test]
#[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
fn live_object_lock_bucket_retention_and_legal_hold() {
    use crate::backend::{LockMode, PutOpts, Retention};
    let (cfg, _d) = live().expect("WEFTOS_STORE_TEST_RUSTFS_ENV");
    let be = RustfsBackend::new(cfg).unwrap();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let plain = uniq("t-nolock");
    be.create_bucket(&plain, &[]).unwrap();
    assert!(be.put_locked(&plain, "k", b"x", &PutOpts { retention: Some(Retention { mode: LockMode::Compliance, until_unix_s: now + 3600 }), ..Default::default() }).is_err(), "a bucket made without lock refuses lock headers");
    be.delete_bucket(&plain).unwrap();

    let b = uniq("t-lock");
    be.create_locked_bucket(&b, &[("store".into(), "release".into())]).unwrap();
    assert!(matches!(be.create_locked_bucket(&b, &[]), Err(BackendError::BucketExists(_))));
    let until = now + 3600;
    let comp = be.put_locked(&b, "c.bin", b"compliance", &PutOpts { if_none_match: true, retention: Some(Retention { mode: LockMode::Compliance, until_unix_s: until }), legal_hold: false, ..Default::default() }).unwrap();
    let gov = be.put_locked(&b, "g.bin", b"governance", &PutOpts { retention: Some(Retention { mode: LockMode::Governance, until_unix_s: until }), ..Default::default() }).unwrap();
    let hold = be.put_locked(&b, "h.bin", b"hold", &PutOpts { legal_hold: true, ..Default::default() }).unwrap();
    for p in [&comp, &gov, &hold] {
        assert!(p.version_id.is_some(), "a versioned bucket returns a version id: {p:?}");
    }
    assert!(matches!(be.put_locked(&b, "c.bin", b"again", &PutOpts { if_none_match: true, ..Default::default() }), Err(BackendError::Exists(_))));
    assert!(be.put_locked(&b, "past", b"x", &PutOpts { retention: Some(Retention { mode: LockMode::Compliance, until_unix_s: now - 5 }), ..Default::default() }).is_err(), "a past date is refused before sending");

    let (cv, gv, hv) = (comp.version_id.unwrap(), gov.version_id.unwrap(), hold.version_id.unwrap());
    let c = be.delete_version(&b, "c.bin", &cv, false);
    let c_bypass = be.delete_version(&b, "c.bin", &cv, true);
    let g = be.delete_version(&b, "g.bin", &gv, false);
    let h = be.delete_version(&b, "h.bin", &hv, false);
    eprintln!("LOCK compliance delete: {c:?}; compliance+bypass: {c_bypass:?}; governance delete: {g:?}; legal-hold delete: {h:?}");
    assert!(matches!(c, Err(BackendError::Forbidden(_))), "compliance: version delete refused");
    assert!(matches!(c_bypass, Err(BackendError::Forbidden(_))), "compliance: bypass does not help");
    assert!(matches!(g, Err(BackendError::Forbidden(_))), "governance: refused without bypass");
    assert!(matches!(h, Err(BackendError::Forbidden(_))), "legal hold: refused");
    assert_eq!(be.get(&b, "c.bin").unwrap(), b"compliance", "still there");
    // Lift the hold, then the version can go; governance can go with bypass.
    be.put_legal_hold(&b, "h.bin", &hv, false).unwrap();
    be.delete_version(&b, "h.bin", &hv, false).unwrap();
    be.delete_version(&b, "g.bin", &gv, true).unwrap();
}

/// S4's path, live: a release store made with Object Lock, CI writing a retained artifact once
/// through the service, the sweeper unable to remove it, and nobody able to overwrite it.
#[test]
#[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
fn live_locked_release_store_through_the_service() {
    use crate::backend::{LockMode, PutOpts, Retention};
    use crate::service::{CI_PRINCIPAL, SWEEPER_PRINCIPAL};
    let (cfg, _d) = live().expect("WEFTOS_STORE_TEST_RUSTFS_ENV");
    let be = Arc::new(RustfsBackend::new(cfg.clone()).unwrap());
    let svc = StoreService::open(
        Config { node_id: uniq("n").replace('_', "-"), state_path: None, clock: || 1_000 },
        be.clone(),
        be.clone(),
        Arc::new(Open),
        Arc::new(MemSink::default()),
        Arc::new(Chain::default()),
    )
    .unwrap();
    let rel = StoreName::release("weft", &ulid::Ulid::new().to_string(), "stable").unwrap();
    let sfx = ulid::Ulid::new().to_string().to_lowercase()[16..].to_owned();
    let (ci_p, sw_p) = (Principal::parse(&format!("{CI_PRINCIPAL}-{sfx}")).unwrap(), Principal::parse(&format!("{SWEEPER_PRINCIPAL}-{sfx}")).unwrap());
    svc.create_locked_store(&rel, "daemon", Parties { appenders: vec![ci_p.clone()], sweepers: vec![sw_p.clone()], ..Default::default() }).unwrap();
    assert!(svc.is_locked(&rel));
    let ci = Caller::local(ci_p);
    let sw = Caller::local(sw_p);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let opts = PutOpts { if_none_match: true, retention: Some(Retention { mode: LockMode::Compliance, until_unix_s: now + 3600 }), legal_hold: false, ..Default::default() };
    let put = svc.put_locked(&ci, &rel, "v1/app.tar", b"build", &opts).unwrap();
    assert!(put.version_id.is_some());
    assert!(matches!(svc.put_locked(&ci, &rel, "v1/app.tar", b"evil", &opts), Err(crate::StoreError::Backend(BackendError::Exists(_)))), "no second version under the same key");
    assert!(matches!(svc.put_locked(&ci, &rel, "v1/app.tar", b"evil", &PutOpts::default()), Err(crate::StoreError::Refused(_))), "append must be conditional");
    // The sweeper's plain delete is allowed by the service; what it does to a locked key is RustFS's call.
    let swept = svc.delete(&sw, &rel, "v1/app.tar");
    eprintln!("LOCK sweeper plain delete: {swept:?}");
    // The version itself cannot be removed.
    assert!(matches!(be.delete_version(&rel.bucket(), "v1/app.tar", put.version_id.as_deref().unwrap(), true), Err(BackendError::Forbidden(_))));
}

/// S4's replaced-manifest check, live: every version of a key is listed, a replaced key shows two,
/// a plain delete shows as a marker, paging works past the page size, and a Read key can list.
#[test]
#[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
fn live_list_versions_shows_a_replaced_manifest_and_a_delete_marker() {
    use crate::backend::{LockMode, PutOpts, Retention};
    let (cfg, _d) = live().expect("WEFTOS_STORE_TEST_RUSTFS_ENV");
    let be = RustfsBackend::new(cfg.clone()).unwrap();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let plain = uniq("t-nover");
    be.create_bucket(&plain, &[]).unwrap();
    let unversioned = be.list_versions(&plain, "");
    eprintln!("VERSIONS unversioned bucket: {unversioned:?}");
    be.delete_bucket(&plain).unwrap();

    let b = uniq("t-ver");
    be.create_locked_bucket(&b, &[]).unwrap();
    let keep = PutOpts { if_none_match: true, retention: Some(Retention { mode: LockMode::Governance, until_unix_s: now + 3600 }), legal_hold: false, ..Default::default() };
    let first = be.put_locked(&b, "release/manifest.json", b"{\"sha\":\"good\"}", &keep).unwrap();
    be.put_locked(&b, "release/other.bin", b"o", &keep).unwrap();
    let v = be.list_versions(&b, "release/").unwrap();
    assert_eq!(v.len(), 2);
    assert!(v.iter().all(|x| x.is_latest && !x.is_delete_marker && !x.version_id.is_empty()), "{v:?}");
    assert_eq!(v[0].key, "release/manifest.json");
    assert_eq!(v[0].version_id, first.version_id.clone().unwrap());
    assert_eq!(v[0].etag, first.meta.etag, "a listed version carries the etag the put returned");
    assert_eq!(v[0].size, 14);
    assert!(be.list_versions(&b, "nothing/").unwrap().is_empty());

    // Someone replaces the manifest with a plain put (no If-None-Match): a second version appears.
    let evil = be.put_locked(&b, "release/manifest.json", b"{\"sha\":\"evil\"}", &PutOpts::default()).unwrap();
    let v = be.list_versions(&b, "release/manifest").unwrap();
    assert_eq!(v.len(), 2, "{v:?}");
    assert!(v[0].is_latest && v[0].version_id == evil.version_id.clone().unwrap(), "newest first: {v:?}");
    assert!(!v[1].is_latest && v[1].version_id == first.version_id.clone().unwrap());
    assert_ne!(v[0].etag, v[1].etag, "the replacement is told apart by etag");
    assert_eq!(v.iter().filter(|x| !x.is_delete_marker).count(), 2);

    // A plain delete adds a marker on top; both real versions stay listed beneath it.
    be.delete(&b, "release/other.bin").unwrap();
    let o = be.list_versions(&b, "release/other").unwrap();
    eprintln!("VERSIONS after plain delete: {o:?}");
    assert!(o[0].is_delete_marker && o[0].is_latest, "{o:?}");
    assert!(o.iter().any(|x| !x.is_delete_marker), "the content version is still there");
    eprintln!("VERSIONS get after plain delete: {:?}", be.get(&b, "release/other.bin"));

    // Paging: force page boundaries with a small page size instead of making 1000 keys.
    for i in 0..30 {
        be.put_locked(&b, &format!("p/{i:03}"), b"x", &PutOpts::default()).unwrap();
    }
    assert_eq!(be.list_versions(&b, "p/").unwrap().len(), 30);
    let paged = be.list_versions_paged(&b, "p/", 7).unwrap();
    assert_eq!(paged.len(), 30, "five pages of seven stitch back together");
    assert_eq!(paged.iter().map(|v| &v.key).collect::<std::collections::BTreeSet<_>>().len(), 30, "no key twice, none lost");

    // A Read key can list versions (the policy carries s3:ListBucketVersions).
    let k = be.create_access_key(&uniq("cog:vreader")).unwrap();
    be.put_policy(&k.id, &json!({"Version": "2012-10-17", "Statement": [{"Effect": "Allow",
        "Action": ["s3:GetObject", "s3:ListBucket", "s3:ListBucketVersions", "s3:GetBucketLocation"],
        "Resource": [format!("arn:aws:s3:::{b}"), format!("arn:aws:s3:::{b}/*")]}]})).unwrap();
    let user = as_key(&cfg, &be, &k.id);
    let seen = user.list_versions(&b, "release/manifest");
    eprintln!("VERSIONS read key: {:?}", seen.as_ref().map(|v| v.len()));
    assert_eq!(seen.unwrap().len(), 2);
    assert!(matches!(user.delete_version(&b, "release/manifest.json", &evil.version_id.unwrap(), true), Err(BackendError::Forbidden(_))));
    be.set_key_enabled(&k.id, false).unwrap();
    assert!(matches!(user.list_versions(&b, ""), Err(BackendError::Forbidden(_))), "a disabled key lists nothing");
}

/// The owner's two switches, live, both ways: off, the CI and sweeper keys cannot put (the
/// `live_release_roles_*` test); on, each can, and what that costs is visible. In a locked store
/// an overwrite by the widened key adds a version and leaves the retained one in place.
#[test]
#[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
fn live_owner_switches_let_the_ci_and_sweeper_keys_put_and_the_lock_keeps_the_original() {
    use crate::backend::{LockMode, PutOpts, Retention};
    use crate::policy::PolicyOpts;
    use crate::service::{CI_PRINCIPAL, SWEEPER_PRINCIPAL};
    let (cfg, _d) = live().expect("WEFTOS_STORE_TEST_RUSTFS_ENV");
    let be = Arc::new(RustfsBackend::new(cfg.clone()).unwrap());
    let svc = StoreService::open(
        Config { node_id: uniq("n").replace('_', "-"), state_path: None, clock: || 1_000 },
        be.clone(),
        be.clone(),
        Arc::new(Open),
        Arc::new(MemSink::default()),
        Arc::new(Chain::default()),
    )
    .unwrap();
    let rel = StoreName::release("weft", &ulid::Ulid::new().to_string(), "stable").unwrap();
    let sfx = ulid::Ulid::new().to_string().to_lowercase()[16..].to_owned();
    let (ci_p, sw_p) = (Principal::parse(&format!("{CI_PRINCIPAL}-{sfx}")).unwrap(), Principal::parse(&format!("{SWEEPER_PRINCIPAL}-{sfx}")).unwrap());
    svc.create_locked_store(&rel, "daemon", Parties { appenders: vec![ci_p.clone()], sweepers: vec![sw_p.clone()], ..Default::default() }).unwrap();
    let b = rel.bucket();
    let ci = as_key(&cfg, &be, &RustfsBackend::key_id_for(&ci_p.to_string()));
    let sw = as_key(&cfg, &be, &RustfsBackend::key_id_for(&sw_p.to_string()));
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let keep = PutOpts { if_none_match: true, retention: Some(Retention { mode: LockMode::Compliance, until_unix_s: now + 3600 }), legal_hold: false, ..Default::default() };
    let orig = svc.put_locked(&Caller::local(ci_p.clone()), &rel, "v1/app.tar", b"build", &keep).unwrap();

    // Off: both keys refused.
    assert!(matches!(ci.put(&b, "v1/app.tar", b"evil", false), Err(BackendError::Forbidden(_))));
    assert!(matches!(sw.put(&b, "v1/app.tar", b"evil", false), Err(BackendError::Forbidden(_))));

    // Sweeper switch on: the sweeper key can now put (an overwrite), the CI key still cannot.
    svc.set_policy_opts(PolicyOpts { sweeper_put: true, ..Default::default() });
    assert!(svc.reconcile().is_empty());
    sw.put(&b, "v1/app.tar", b"swept-in", false).expect("sweeper put allowed with the switch on");
    assert!(matches!(ci.put(&b, "v1/app.tar", b"evil", false), Err(BackendError::Forbidden(_))));
    // The retained original is still there as its own version; the newest is the overwrite.
    let v = be.list_versions(&b, "v1/app.tar").unwrap();
    eprintln!("SWITCH sweeper put in a locked store: {v:?}");
    assert_eq!(v.len(), 2);
    assert!(v.iter().any(|x| Some(&x.version_id) == orig.version_id.as_ref() && !x.is_latest), "the original is kept, no longer newest");
    assert!(matches!(be.delete_version(&b, "v1/app.tar", orig.version_id.as_deref().unwrap(), true), Err(BackendError::Forbidden(_))));

    // CI switch on, sweeper off: CI can put directly (even a copy over a key), the sweeper cannot.
    svc.set_policy_opts(PolicyOpts { ci_direct_put: true, ..Default::default() });
    assert!(svc.reconcile().is_empty());
    ci.put(&b, "v1/direct.tar", b"direct", true).expect("CI direct put allowed with the switch on");
    assert!(matches!(sw.put(&b, "v1/app.tar", b"again", false), Err(BackendError::Forbidden(_))));
    assert!(matches!(ci.delete(&b, "v1/direct.tar"), Err(BackendError::Forbidden(_))), "the switch gives CI put, never delete");

    // Back to the default: refused again.
    svc.set_policy_opts(PolicyOpts::default());
    assert!(svc.reconcile().is_empty());
    assert!(matches!(ci.put(&b, "v1/direct2.tar", b"x", true), Err(BackendError::Forbidden(_))));
}

/// Object Lock configuration read-back, live: plain bucket, locked bucket, locked bucket with a
/// default retention (and a put with no headers inheriting it).
#[test]
#[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
fn live_lock_configuration_reads_back() {
    use crate::backend::{DefaultRetention, LockConfig, LockMode, PutOpts};
    let (cfg, _d) = live().expect("WEFTOS_STORE_TEST_RUSTFS_ENV");
    let be = RustfsBackend::new(cfg).unwrap();
    let plain = uniq("t-cfgp");
    be.create_bucket(&plain, &[]).unwrap();
    assert_eq!(be.get_lock_config(&plain).unwrap(), LockConfig { enabled: false, default: None });
    assert!(be.put_default_retention(&plain, &DefaultRetention { mode: LockMode::Governance, days: Some(1), years: None }).is_err(), "a plain bucket takes no default retention");
    be.delete_bucket(&plain).unwrap();

    let b = uniq("t-cfgl");
    be.create_locked_bucket(&b, &[]).unwrap();
    assert_eq!(be.get_lock_config(&b).unwrap(), LockConfig { enabled: true, default: None });
    let d = DefaultRetention { mode: LockMode::Governance, days: Some(2), years: None };
    be.put_default_retention(&b, &d).unwrap();
    assert_eq!(be.get_lock_config(&b).unwrap(), LockConfig { enabled: true, default: Some(d) });
    assert!(be.put_default_retention(&b, &DefaultRetention { mode: LockMode::Governance, days: Some(1), years: Some(1) }).is_err(), "days and years together are refused before sending");
    // A put with no lock headers now carries the default: its delete is refused.
    let v = be.put_locked(&b, "k", b"x", &PutOpts::default()).unwrap().version_id.unwrap();
    assert!(matches!(be.delete_version(&b, "k", &v, false), Err(BackendError::Forbidden(_))), "the default retention applied");
    be.delete_version(&b, "k", &v, true).unwrap();
    assert!(matches!(be.get_lock_config("no-such-bucket-zz"), Err(BackendError::NoBucket(_)) | Err(BackendError::Io(_))));
}

/// Lifecycle rules, live: written, read back, replaced, refused when malformed, removed.
#[test]
#[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
fn live_lifecycle_rules_round_trip() {
    let (cfg, _d) = live().expect("WEFTOS_STORE_TEST_RUSTFS_ENV");
    let be = RustfsBackend::new(cfg).unwrap();
    let b = uniq("t-life");
    be.create_bucket(&b, &[]).unwrap();
    assert_eq!(be.get_lifecycle(&b).unwrap(), vec![], "a new bucket has none");
    be.put_lifecycle(&b, &[]).unwrap();
    let rules = vec![rule("tmp-30d", "tmp/"), LifecycleRule { id: "mp".into(), prefix: String::new(), enabled: true, expire_days: None, noncurrent_days: None, abort_multipart_days: Some(2) }];
    be.put_lifecycle(&b, &rules).unwrap();
    let got = be.get_lifecycle(&b).unwrap();
    eprintln!("LIFECYCLE read back: {got:?}");
    assert_eq!(got, rules);
    // A second put replaces the set.
    be.put_lifecycle(&b, &[rule("only", "x/")]).unwrap();
    assert_eq!(be.get_lifecycle(&b).unwrap(), vec![rule("only", "x/")]);
    assert!(be.put_lifecycle(&b, &[rule("d", ""), rule("d", "p/")]).is_err(), "duplicate ids refused before sending");
    assert!(be.put_lifecycle("no-such-bucket-zz", &[rule("a", "")]).is_err());
    be.put_lifecycle(&b, &[]).unwrap();
    assert_eq!(be.get_lifecycle(&b).unwrap(), vec![]);
    be.delete_bucket(&b).unwrap();
}

/// Stamping retention after the upload, live: an object put with no retention is held afterwards,
/// the hold only ever lengthens, and RustFS itself refuses to shorten a compliance hold.
#[test]
#[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
fn live_retention_is_stamped_after_commit_and_only_lengthens() {
    use crate::backend::{LockMode, PutOpts, Retention};
    let (cfg, _d) = live().expect("WEFTOS_STORE_TEST_RUSTFS_ENV");
    let be = RustfsBackend::new(cfg).unwrap();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let b = uniq("t-ret");
    be.create_locked_bucket(&b, &[]).unwrap();
    let v = be.put_locked(&b, "a.tar", b"build", &PutOpts { if_none_match: true, ..Default::default() }).unwrap().version_id.unwrap();
    assert_eq!(be.get_retention(&b, "a.tar", &v).unwrap(), None, "committed with no hold");

    // Stamp it.
    let held = be.extend_retention(&b, "a.tar", &v, now + 3600, LockMode::Compliance).unwrap();
    assert_eq!(held, Retention { mode: LockMode::Compliance, until_unix_s: now + 3600 });
    assert_eq!(be.get_retention(&b, "a.tar", &v).unwrap(), Some(held), "read back as stamped");
    assert!(matches!(be.delete_version(&b, "a.tar", &v, true), Err(BackendError::Forbidden(_))), "now held");
    // Lengthen it; the mode stays, whatever mode the caller names.
    let longer = be.extend_retention(&b, "a.tar", &v, now + 7200, LockMode::Governance).unwrap();
    assert_eq!(longer, Retention { mode: LockMode::Compliance, until_unix_s: now + 7200 });
    // Not later: refused before sending. Shortening straight at the API: refused by RustFS.
    assert!(matches!(be.extend_retention(&b, "a.tar", &v, now + 7200, LockMode::Compliance), Err(BackendError::BadKey(_))), "equal is not an extension");
    assert!(matches!(be.extend_retention(&b, "a.tar", &v, now + 60, LockMode::Compliance), Err(BackendError::BadKey(_))));
    let shorter = be.put_retention(&b, "a.tar", &v, &Retention { mode: LockMode::Compliance, until_unix_s: now + 120 }, true);
    eprintln!("RETENTION shorten compliance with bypass: {shorter:?}");
    assert!(shorter.is_err(), "RustFS refuses to shorten a compliance hold");
    assert_eq!(be.get_retention(&b, "a.tar", &v).unwrap().unwrap().until_unix_s, now + 7200, "unchanged");
    // A past date is refused before sending; an unknown version is an error, not a stamp.
    assert!(be.put_retention(&b, "a.tar", &v, &Retention { mode: LockMode::Compliance, until_unix_s: now - 1 }, false).is_err());
    assert!(be.extend_retention(&b, "a.tar", "no-such-version", now + 9000, LockMode::Governance).is_err());
}

/// Multipart upload with lock headers, live: an 11 MiB object streamed in three parts into a
/// locked bucket with retention, read back byte for byte, held, and an aborted upload leaves nothing.
#[test]
#[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
fn live_multipart_upload_carries_lock_headers_and_aborts_clean() {
    use crate::backend::{LockMode, MIN_PART, Part, PutOpts, Retention};
    let (cfg, _d) = live().expect("WEFTOS_STORE_TEST_RUSTFS_ENV");
    let be = RustfsBackend::new(cfg).unwrap();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let b = uniq("t-mp");
    be.create_locked_bucket(&b, &[]).unwrap();
    // A reader with a pattern that is not constant, so a misordered part shows.
    let total = 11 * 1024 * 1024usize;
    let pattern = |i: usize| (i.wrapping_mul(31) % 251) as u8;
    let want: Vec<u8> = (0..total).map(pattern).collect();
    let opts = PutOpts { if_none_match: true, retention: Some(Retention { mode: LockMode::Compliance, until_unix_s: now + 3600 }), legal_hold: false, ..Default::default() };
    let put = be.put_stream(&b, "big.bin", &mut std::io::Cursor::new(want.clone()), MIN_PART, &opts).unwrap();
    eprintln!("MULTIPART put: {put:?}");
    assert_eq!(put.meta.size, total as u64);
    let vid = put.version_id.clone().expect("a locked bucket gives a version id");
    assert_eq!(be.get(&b, "big.bin").unwrap(), want, "read back byte for byte");
    assert!(put.meta.etag.contains('-'), "a multipart etag ends in -<parts>: {}", put.meta.etag);
    assert_eq!(be.head(&b, "big.bin").unwrap().size, total as u64);
    // The retention given at create applies to the finished object.
    let held = be.get_retention(&b, "big.bin", &vid).unwrap().expect("retention applied");
    assert_eq!(held.mode, LockMode::Compliance);
    assert!(matches!(be.delete_version(&b, "big.bin", &vid, true), Err(BackendError::Forbidden(_))), "held");
    // if_none_match on a second upload to the same key: RustFS's answer is recorded.
    let again = be.put_stream(&b, "big.bin", &mut std::io::Cursor::new(vec![1u8; 6 * 1024 * 1024]), MIN_PART, &opts);
    eprintln!("MULTIPART second upload with if_none_match: {:?}", again.as_ref().map(|p| &p.meta.etag).map_err(|e| e.to_string()));
    assert!(again.is_err(), "an existing key is refused at complete");
    assert_eq!(be.get(&b, "big.bin").unwrap(), want, "the first object is untouched");
    // Small objects take the single-request path with the same headers.
    let small = be.put_stream(&b, "small.bin", &mut std::io::Cursor::new(b"tiny".to_vec()), MIN_PART, &opts).unwrap();
    assert!(small.version_id.is_some() && small.meta.size == 4);
    // An aborted upload leaves nothing behind.
    let id = be.create_multipart(&b, "gone.bin", &PutOpts::default()).unwrap();
    let e1 = be.upload_part(&b, "gone.bin", &id, 1, &vec![3u8; MIN_PART]).unwrap();
    assert!(!e1.is_empty());
    be.abort_multipart(&b, "gone.bin", &id).unwrap();
    assert!(matches!(be.head(&b, "gone.bin"), Err(BackendError::NoObject(_))));
    assert!(be.upload_part(&b, "gone.bin", &id, 2, b"x").is_err(), "no part after abort");
    be.abort_multipart(&b, "gone.bin", &id).unwrap();
    // Order and numbering are checked before sending; a part below the minimum that is not last is RustFS's call.
    assert!(be.complete_multipart(&b, "x", "id", &[], &PutOpts::default()).is_err());
    assert!(be.complete_multipart(&b, "x", "id", &[Part { number: 2, etag: "a".into() }, Part { number: 1, etag: "b".into() }], &PutOpts::default()).is_err());
    assert!(be.upload_part(&b, "x", "id", 0, b"x").is_err() && be.upload_part(&b, "x", "id", 10_001, b"x").is_err());
}

/// An unpaired node's store, live: archived into the archive bucket with its data, grants off,
/// the id not handed out again.
#[test]
#[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
fn live_retiring_a_node_archives_its_store_and_keeps_the_id_out() {
    let (cfg, _d) = live().expect("WEFTOS_STORE_TEST_RUSTFS_ENV");
    let be = Arc::new(RustfsBackend::new(cfg).unwrap());
    let node_id = uniq("n").replace('_', "-");
    let svc = StoreService::open(Config { node_id: node_id.clone(), state_path: None, clock: || 1_000 }, be.clone(), be.clone(), Arc::new(Open), Arc::new(MemSink::default()), Arc::new(Chain::default())).unwrap();
    let n = StoreName::node("weft", &node_id).unwrap();
    svc.ensure_store(&n, "daemon", Parties::defaults_for(&n)).unwrap();
    let cog = Principal::parse("cog:old-node-cog").unwrap();
    svc.grant(&n, cog.clone(), Access::Write, "op").unwrap();
    svc.put(&Caller::local(cog.clone()), &n, "d/a.txt", b"before unpair", false).unwrap();
    assert_eq!(svc.retire_node_store(&n, "op").unwrap(), 1);
    let archived = be.list(crate::service::ARCHIVE_BUCKET, "").unwrap();
    assert!(archived.iter().any(|m| m.key.contains(&n.bucket()) && m.key.ends_with("d/a.txt")), "{archived:?}");
    assert!(svc.get(&Caller::local(cog), &n, "d/a.txt").is_err(), "old grants are off");
    assert!(svc.ensure_store(&n, "daemon", Parties::defaults_for(&n)).is_err(), "the id is retired");
}

// ---- F15: secrets are not printed, the secrets directory is private, plain http is loopback only

fn cfg_in(d: &Path, endpoint: &str) -> RustfsConfig {
    RustfsConfig { endpoint: endpoint.into(), region: "us-east-1".into(), root_access_key: "ROOTACCESS-Q9".into(), root_secret_key: "ROOTSECRET-Z7".into(), secrets_dir: d.join("s"), timeout: Duration::from_secs(1) }
}

#[test]
fn debug_of_the_config_and_its_error_never_shows_the_root_keys() {
    let d = tempfile::tempdir().unwrap();
    let c = cfg_in(d.path(), "http://127.0.0.1:1");
    for shown in [format!("{c:?}"), format!("{c:#?}")] {
        assert!(!shown.contains("ROOTACCESS-Q9") && !shown.contains("ROOTSECRET-Z7"), "{shown}");
        assert!(shown.contains("redacted") && shown.contains("127.0.0.1:1"), "the rest is still shown: {shown}");
    }
    // A refused endpoint's error does not carry them either.
    let e = RustfsBackend::new(cfg_in(d.path(), "http://rustfs.example.com:9000")).err().unwrap().to_string();
    assert!(!e.contains("ROOTSECRET-Z7") && !e.contains("ROOTACCESS-Q9"), "{e}");
}

#[test]
fn plain_http_is_for_loopback_only() {
    let d = tempfile::tempdir().unwrap();
    for ok in ["http://127.0.0.1:9000", "http://127.9.9.9", "http://localhost:9000", "http://LOCALHOST", "http://[::1]:9000", "https://rustfs.example.com:9000", "https://10.0.0.5"] {
        assert!(RustfsBackend::new(cfg_in(d.path(), ok)).is_ok(), "{ok} is allowed");
    }
    for bad in ["http://10.0.0.5:9000", "http://rustfs.example.com", "http://192.168.1.2:9000", "http://127.0.0.1.evil.example:9000", "http://localhost.evil.example", "http://user@evil.example/", "http://[::2]:9000", "ftp://127.0.0.1", "127.0.0.1:9000"] {
        let e = RustfsBackend::new(cfg_in(d.path(), bad)).err().unwrap_or_else(|| panic!("{bad} must be refused")).to_string();
        assert!(e.contains("http") || e.contains("endpoint"), "{bad}: {e}");
    }
    // The host after an `@` is the host that is connected to.
    assert!(RustfsBackend::new(cfg_in(d.path(), "http://evil.example@127.0.0.1:9000")).is_ok());
    assert!(RustfsBackend::new(cfg_in(d.path(), "http://127.0.0.1@evil.example:9000")).is_err());
}

#[test]
fn the_secrets_directory_is_0700_a_loose_one_of_ours_is_tightened_and_a_secret_file_is_0600() {
    use std::os::unix::fs::PermissionsExt;
    let d = tempfile::tempdir().unwrap();
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    let c = cfg_in(d.path(), "http://127.0.0.1:1");
    let _ = RustfsBackend::new(c.clone()).unwrap();
    assert_eq!(mode(&c.secrets_dir), 0o700, "made private, nested parents included");
    std::fs::set_permissions(&c.secrets_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    let be = RustfsBackend::new(c.clone()).unwrap();
    assert_eq!(mode(&c.secrets_dir), 0o700, "an existing loose directory of ours is tightened");
    let nested = RustfsConfig { secrets_dir: d.path().join("a/b/c"), ..c };
    let _ = RustfsBackend::new(nested.clone()).unwrap();
    assert_eq!(mode(&nested.secrets_dir), 0o700);
    RustfsBackend::write_secret(&be.secret_path("k1"), "s3cr3t").unwrap();
    assert_eq!(mode(&be.secret_path("k1")), 0o600);
}

#[test]
fn a_secret_is_written_with_create_new_a_leftover_is_replaced_and_a_planted_link_is_not_followed() {
    use std::os::unix::fs::PermissionsExt;
    let d = tempfile::tempdir().unwrap();
    let be = RustfsBackend::new(cfg_in(d.path(), "http://127.0.0.1:1")).unwrap();
    let path = be.secret_path("k2");
    let tmp = path.with_extension("tmp");
    // A leftover tmp file with loose permissions from a crash: the secret still ends up 0600,
    // because the old file is removed and a new one made, not reopened with its old mode.
    std::fs::write(&tmp, b"stale").unwrap();
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o666)).unwrap();
    RustfsBackend::write_secret(&path, "fresh").unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "fresh");
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    // A symlink planted at the tmp path is replaced by a file; what it pointed at is untouched.
    let victim = d.path().join("victim");
    std::fs::write(&victim, b"keep").unwrap();
    std::os::unix::fs::symlink(&victim, &tmp).unwrap();
    RustfsBackend::write_secret(&path, "again").unwrap();
    assert_eq!(std::fs::read(&victim).unwrap(), b"keep");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "again");
}


// ---- F16: bucket names, a copy that fails inside a 200, and the integrity of what is read ------

/// A canned HTTP/1.1 server on loopback: each request is answered by `f(request line)` as
/// `(status, extra headers, body)`; every request line it saw is returned when `stop` is called.
struct Canned {
    port: u16,
    seen: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl Canned {
    fn start(f: impl Fn(&str) -> (u16, Vec<(&'static str, String)>, Vec<u8>) + Send + 'static) -> Self {
        use std::io::{Read, Write};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let s2 = seen.clone();
        std::thread::spawn(move || {
            for c in l.incoming() {
                let Ok(mut c) = c else { break };
                let mut buf = Vec::new();
                let mut b = [0u8; 4096];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match c.read(&mut b) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => buf.extend_from_slice(&b[..n]),
                    }
                }
                let line = String::from_utf8_lossy(&buf).lines().next().unwrap_or("").to_owned();
                s2.lock().unwrap().push(line.clone());
                let (st, hs, body) = f(&line);
                let mut head = format!("HTTP/1.1 {st} X\r\ncontent-length: {}\r\nconnection: close\r\n", body.len());
                for (k, v) in hs {
                    head.push_str(&format!("{k}: {v}\r\n"));
                }
                let _ = c.write_all(head.as_bytes());
                let _ = c.write_all(b"\r\n");
                let _ = c.write_all(&body);
            }
        });
        Self { port, seen }
    }
    fn backend(&self, d: &Path) -> RustfsBackend {
        RustfsBackend::new(cfg_in(d, &format!("http://127.0.0.1:{}", self.port))).unwrap().with_retry(Retry { attempts: 1, base: Duration::from_millis(1) })
    }
    fn requests(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }
}

#[test]
fn a_bucket_string_that_is_not_a_bucket_name_never_reaches_the_network_on_any_call() {
    let srv = Canned::start(|_| (200, vec![], b"<CopyObjectResult><ETag>\"x\"</ETag></CopyObjectResult>".to_vec()));
    let d = tempfile::tempdir().unwrap();
    let be = srv.backend(d.path());
    for bad in ["w-x/../p-y", "../x", "a/b", "ab", "UPPER", "-lead", "a b c", "x%2Fy", "p-y?acl", &"a".repeat(64), "", "a.b.c"] {
        let k = "k";
        let bad_key = |e: BackendError| matches!(e, BackendError::BadKey(_));
        assert!(bad_key(be.put(bad, k, b"v", false).unwrap_err()), "put {bad:?}");
        assert!(bad_key(be.get(bad, k).unwrap_err()), "get {bad:?}");
        assert!(bad_key(be.head(bad, k).unwrap_err()), "head {bad:?}");
        assert!(bad_key(be.delete(bad, k).unwrap_err()), "delete {bad:?}");
        assert!(bad_key(be.list(bad, "").unwrap_err()), "list {bad:?}");
        assert!(bad_key(be.copy(bad, k, "good-bucket", "k2").unwrap_err()), "copy src {bad:?}");
        assert!(bad_key(be.copy("good-bucket", k, bad, "k2").unwrap_err()), "copy dst {bad:?}");
        assert!(bad_key(be.create_bucket(bad, &[]).unwrap_err()), "create {bad:?}");
        assert!(bad_key(be.delete_bucket(bad).unwrap_err()), "delete_bucket {bad:?}");
        assert!(bad_key(be.bucket_exists(bad).unwrap_err()), "exists {bad:?}");
        assert!(bad_key(be.get_lifecycle(bad).unwrap_err()), "lifecycle {bad:?}");
        assert!(bad_key(be.put_if_match(bad, k, b"v", "e").unwrap_err()), "put_if_match {bad:?}");
        assert!(bad_key(be.list_versions(bad, "").unwrap_err()), "list_versions {bad:?}");
        assert!(bad_key(be.get_lock_config(bad).unwrap_err()), "lock config {bad:?}");
        assert!(bad_key(be.put_bucket_notification(bad, "t").unwrap_err()), "notification {bad:?}");
    }
    assert!(srv.requests().is_empty(), "not one request was sent: {:?}", srv.requests());
    // The names the daemon really makes pass.
    for ok in ["weftos-archive", "p-01jbxk2m3n4p5q6r7s8t9v0w1x", "w-abc-def", "n-pg-1"] {
        assert!(crate::backend::check_bucket(ok).is_ok(), "{ok}");
    }
}

#[test]
fn a_copy_that_answers_200_with_an_error_body_is_an_error_and_a_real_result_is_not() {
    let d = tempfile::tempdir().unwrap();
    let err = |code: &'static str| Canned::start(move |_| (200, vec![], format!("<?xml version=\"1.0\"?><Error><Code>{code}</Code><Message>it broke</Message></Error>").into_bytes()));
    for (code, want) in [("InternalError", "unavailable"), ("NoSuchKey", "noobject"), ("NoSuchBucket", "nobucket"), ("AccessDenied", "forbidden"), ("EntityTooLarge", "io")] {
        let srv = err(code);
        let e = srv.backend(d.path()).copy("src-bkt", "a.txt", "dst-bkt", "b.txt").unwrap_err();
        let got = match &e {
            BackendError::Unavailable(_) => "unavailable",
            BackendError::NoObject(_) => "noobject",
            BackendError::NoBucket(_) => "nobucket",
            BackendError::Forbidden(_) => "forbidden",
            BackendError::Io(_) => "io",
            o => panic!("{o:?}"),
        };
        assert_eq!(got, want, "{code}");
        assert_eq!(srv.requests().len(), 1, "{code}: failed on the copy's own answer; no HEAD of a destination was trusted");
    }
    // 200 with no body at all, or with something else: not a copy result.
    for body in ["", "<html>ok</html>"] {
        let srv = Canned::start(move |_| (200, vec![], body.as_bytes().to_vec()));
        assert!(matches!(srv.backend(d.path()).copy("src-bkt", "a", "dst-bkt", "b"), Err(BackendError::Io(_))), "{body:?}");
    }
    // A real result passes, and the destination's own ETag has to agree with it.
    let ok = Canned::start(|line| {
        if line.starts_with("PUT") {
            (200, vec![], b"<CopyObjectResult><ETag>&quot;5d41402abc4b2a76b9719d911017c592&quot;</ETag></CopyObjectResult>".to_vec())
        } else {
            (200, vec![("etag", "\"5d41402abc4b2a76b9719d911017c592\"".into())], vec![])
        }
    });
    let m = ok.backend(d.path()).copy("src-bkt", "a", "dst-bkt", "b").unwrap();
    assert_eq!(m.etag, "5d41402abc4b2a76b9719d911017c592");
    let liar = Canned::start(|line| {
        if line.starts_with("PUT") { (200, vec![], b"<CopyObjectResult><ETag>\"aaaa\"</ETag></CopyObjectResult>".to_vec()) } else { (200, vec![("etag", "\"bbbb\"".into())], vec![]) }
    });
    assert!(matches!(liar.backend(d.path()).copy("src-bkt", "a", "dst-bkt", "b"), Err(BackendError::Io(_))), "a destination that is not what the copy reported");
}

#[test]
fn a_read_whose_body_does_not_match_its_checksum_or_etag_is_an_error_not_data() {
    let d = tempfile::tempdir().unwrap();
    let good = crate::integrity::checksum_sha256(b"hello");
    let md5 = "5d41402abc4b2a76b9719d911017c592";
    // The store's SHA-256 matches / does not.
    let s = Canned::start({
        let good = good.clone();
        move |_| (200, vec![("x-amz-checksum-sha256", good.clone()), ("etag", "\"whatever\"".into())], b"hello".to_vec())
    });
    assert_eq!(s.backend(d.path()).get_checked("bkt", "k").unwrap(), (b"hello".to_vec(), crate::integrity::Check::Sha256));
    let s = Canned::start(move |_| (200, vec![("x-amz-checksum-sha256", good.clone())], b"hellp".to_vec()));
    assert!(s.backend(d.path()).get("bkt", "k").unwrap_err().to_string().contains("integrity"));
    // No checksum: the single-part ETag.
    let s = Canned::start(move |_| (200, vec![("etag", format!("\"{md5}\""))], b"hello".to_vec()));
    assert_eq!(s.backend(d.path()).get_checked("bkt", "k").unwrap().1, crate::integrity::Check::Md5Etag);
    let s = Canned::start(move |_| (200, vec![("etag", format!("\"{md5}\""))], b"HELLO".to_vec()));
    assert!(s.backend(d.path()).get("bkt", "k").unwrap_err().to_string().contains("MD5"));
    // A multipart ETag proves nothing: reported as unverified, not as verified.
    let s = Canned::start(move |_| (200, vec![("etag", format!("\"{md5}-2\""))], b"anything".to_vec()));
    assert_eq!(s.backend(d.path()).get_checked("bkt", "k").unwrap().1, crate::integrity::Check::Unverified);
    // The request asked for the checksum, and a put sends one.
    let seen = Canned::start(|_| (200, vec![], vec![]));
    let be = seen.backend(d.path());
    let _ = be.put("bkt", "k", b"hello", false);
    let _ = be.get("bkt", "k");
    assert_eq!(seen.requests().len(), 2);
}

#[test]
fn put_and_get_requests_carry_the_checksum_headers() {
    use std::io::{Read, Write};
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let h = std::thread::spawn(move || {
        let mut heads = Vec::new();
        for _ in 0..2 {
            let (mut c, _) = l.accept().unwrap();
            let mut buf = Vec::new();
            let mut b = [0u8; 4096];
            while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = c.read(&mut b).unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&b[..n]);
            }
            heads.push(String::from_utf8_lossy(&buf).to_lowercase());
            let _ = c.write_all(b"HTTP/1.1 200 X\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
        }
        heads
    });
    let d = tempfile::tempdir().unwrap();
    let be = RustfsBackend::new(cfg_in(d.path(), &format!("http://127.0.0.1:{port}"))).unwrap().with_retry(Retry { attempts: 1, base: Duration::from_millis(1) });
    be.put("bkt", "k", b"hello", false).unwrap();
    let _ = be.get("bkt", "k");
    let heads = h.join().unwrap();
    assert!(heads[0].starts_with("put ") && heads[0].contains(&format!("x-amz-checksum-sha256: {}", crate::integrity::checksum_sha256(b"hello").to_lowercase())), "{}", heads[0]);
    assert!(heads[0].contains("signedheaders=") && heads[0].contains("x-amz-checksum-sha256"), "the checksum is a signed header: {}", heads[0]);
    assert!(heads[1].starts_with("get ") && heads[1].contains("x-amz-checksum-mode: enabled"), "{}", heads[1]);
}

#[test]
#[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
fn live_reads_are_checked_against_the_stored_checksum_and_a_copy_is_a_copy_result() {
    let (cfg, _d) = live().expect("WEFTOS_STORE_TEST_RUSTFS_ENV");
    let be = RustfsBackend::new(cfg).unwrap();
    let b = uniq("t-int");
    be.create_bucket(&b, &[]).unwrap();
    be.put(&b, "a.txt", b"hello", false).unwrap();
    let (body, how) = be.get_checked(&b, "a.txt").unwrap();
    assert_eq!(body, b"hello");
    eprintln!("RustFS 1.0.4 read of an object put with a checksum was verified by {how:?}");
    assert_ne!(how, crate::integrity::Check::Unverified, "a put by this backend can always be checked on read");
    // An object put without our checksum header (a raw signed PUT) is still checkable by its ETag.
    let r = be.send("PUT", &format!("/{b}/raw.txt"), &[], b"hello", &[]).unwrap();
    assert_eq!(r.status, 200);
    assert_ne!(be.get_checked(&b, "raw.txt").unwrap().1, crate::integrity::Check::Unverified);
    // A wrong checksum on a put is refused by the store.
    let wrong = be.send("PUT", &format!("/{b}/bad.txt"), &[], b"hello", &[("x-amz-checksum-sha256", crate::integrity::checksum_sha256(b"other"))]).unwrap();
    assert_ne!(wrong.status, 200, "RustFS verifies the checksum it is sent");
    // Copy: the result's ETag is the destination's.
    let m = be.copy(&b, "a.txt", &b, "b.txt").unwrap();
    assert_eq!(m.etag, "5d41402abc4b2a76b9719d911017c592");
    assert!(matches!(be.copy(&b, "missing.txt", &b, "c.txt"), Err(BackendError::NoObject(_))));
}

/// The execute attribute against a real RustFS (item 5 of the store-attrs follow-ups): the
/// `x-amz-meta-weft-exec` header round-trips on every put path, CopyObject keeps it, a locked put
/// and a multipart upload carry it, and the listing's `LastModified` moves when only the attribute
/// changes (what a refresh's mode-only check relies on).
#[test]
#[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
fn live_exec_attribute_round_trips_through_every_put_path_and_copy() {
    use crate::backend::{LockMode, MIN_PART, PutOpts, Retention};
    let x = ObjectAttrs { executable: true };
    let (cfg, _d) = live().expect("WEFTOS_STORE_TEST_RUSTFS_ENV");
    let be = RustfsBackend::new(cfg).unwrap();
    let b = uniq("t-exec");
    be.create_bucket(&b, &[]).unwrap();

    // put, put_if_match: set, read back, replaced.
    let v1 = be.put_attrs(&b, "bin/run", b"#!/bin/sh\n", true, x).unwrap();
    assert!(be.attrs(&b, "bin/run").unwrap().executable, "put_attrs");
    be.put(&b, "plain", b"data", false).unwrap();
    assert!(!be.attrs(&b, "plain").unwrap().executable);
    // A mode-only put (same bytes): the etag stays, the attribute and the listing stamp change.
    let stamp = |be: &RustfsBackend, k: &str| be.list_stamped(&b, "").unwrap().into_iter().find(|(m, _)| m.key == k).unwrap().1;
    let (s0, e0) = (stamp(&be, "bin/run"), be.head(&b, "bin/run").unwrap().etag);
    let v2 = be.put_if_match_attrs(&b, "bin/run", b"#!/bin/sh\n", &v1.etag, ObjectAttrs::default()).unwrap();
    let s1 = stamp(&be, "bin/run");
    eprintln!("EXEC LIVE: etag {e0} -> {}; LastModified {s0:?} -> {s1:?} (no sleep between the puts)", v2.etag);
    assert_eq!(v2.etag, e0, "same bytes, same etag: a mode-only change is invisible to the etag");
    assert!(!be.attrs(&b, "bin/run").unwrap().executable, "a replace without the attribute clears it");
    std::thread::sleep(std::time::Duration::from_millis(1100));
    be.put_if_match_attrs(&b, "bin/run", b"#!/bin/sh\n", &v2.etag, x).unwrap();
    let s2 = stamp(&be, "bin/run");
    assert_ne!(s1, s2, "LastModified moved on a mode-only put (a second later)");
    assert!(be.attrs(&b, "bin/run").unwrap().executable);

    // CopyObject: the default metadata directive copies the user metadata.
    be.copy(&b, "bin/run", &b, "copy/run").unwrap();
    assert!(be.attrs(&b, "copy/run").unwrap().executable, "CopyObject keeps x-amz-meta-weft-exec");
    be.copy(&b, "plain", &b, "copy/plain").unwrap();
    assert!(!be.attrs(&b, "copy/plain").unwrap().executable);

    // Multipart (two parts: MIN_PART + 1 byte): the attribute is set when the upload is created.
    let big: Vec<u8> = (0..MIN_PART + 1).map(|i| (i % 251) as u8).collect();
    let m = be.put_stream(&b, "big/run", &mut std::io::Cursor::new(big.clone()), MIN_PART, &PutOpts { attrs: x, ..Default::default() }).unwrap();
    assert_eq!(m.meta.size, big.len() as u64);
    assert!(be.attrs(&b, "big/run").unwrap().executable, "multipart upload");
    be.put_stream(&b, "big/data", &mut std::io::Cursor::new(big.clone()), MIN_PART, &PutOpts::default()).unwrap();
    assert!(!be.attrs(&b, "big/data").unwrap().executable);
    // A single-part stream goes through put_locked.
    be.put_stream(&b, "small/run", &mut std::io::Cursor::new(b"x".to_vec()), MIN_PART, &PutOpts { attrs: x, ..Default::default() }).unwrap();
    assert!(be.attrs(&b, "small/run").unwrap().executable, "single-part stream");

    // Object Lock: a locked put carries it too.
    let lb = uniq("t-exec-lock");
    be.create_locked_bucket(&lb, &[]).unwrap();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let hold = PutOpts { legal_hold: true, attrs: x, ..Default::default() };
    let lp = be.put_locked(&lb, "bin/locked", b"locked", &hold).unwrap();
    assert!(be.attrs(&lb, "bin/locked").unwrap().executable, "put_locked");
    let gov = PutOpts { retention: Some(Retention { mode: LockMode::Governance, until_unix_s: now + 3600 }), attrs: x, ..Default::default() };
    let gp = be.put_locked(&lb, "bin/gov", b"gov", &gov).unwrap();
    assert!(be.attrs(&lb, "bin/gov").unwrap().executable, "put_locked with retention");
    be.put_legal_hold(&lb, "bin/locked", lp.version_id.as_deref().unwrap(), false).unwrap();
    let _ = be.delete_version(&lb, "bin/locked", lp.version_id.as_deref().unwrap(), false);
    let _ = be.delete_version(&lb, "bin/gov", gp.version_id.as_deref().unwrap(), true);

    for m in be.list(&b, "").unwrap() {
        be.delete(&b, &m.key).unwrap();
    }
    be.delete_bucket(&b).unwrap();
}

/// What the backend puts on the wire for the execute attribute, against a loopback fake S3 that
/// records each request's headers (the real round trip is `live_exec_attribute_*` below).
mod attrs_wire {
    use std::io::{Read, Write};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use super::super::*;
    use crate::backend::{ObjectAttrs, PutOpts};

    const EXEC_HDR: &str = "x-amz-meta-weft-exec";

    struct Seen {
        line: String,
        headers: Vec<(String, String)>,
    }

    impl Seen {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
        }
    }

    fn http(headers: &[(&str, &str)], body: &str) -> String {
        let mut r = String::from("HTTP/1.1 200 OK\r\n");
        for (k, v) in headers {
            r.push_str(&format!("{k}: {v}\r\n"));
        }
        r.push_str(&format!("Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()));
        r
    }

    /// A one-thread fake S3: records every request, answers with `respond(request line)`.
    fn fake(respond: impl Fn(&str) -> String + Send + 'static) -> (RustfsBackend, Arc<Mutex<Vec<Seen>>>, tempfile::TempDir) {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            for conn in l.incoming() {
                let Ok(mut c) = conn else { break };
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let (head_end, want) = loop {
                    let n = c.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        break (buf.len(), 0);
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..i]).to_lowercase();
                        let len = head.lines().find_map(|l| l.strip_prefix("content-length:")).and_then(|v| v.trim().parse::<usize>().ok()).unwrap_or(0);
                        break (i + 4, len);
                    }
                };
                while buf.len() < head_end + want {
                    let n = c.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                let text = String::from_utf8_lossy(&buf[..head_end]).into_owned();
                let mut lines = text.split("\r\n");
                let line = lines.next().unwrap_or("").to_owned();
                let headers = lines.filter_map(|h| h.split_once(": ")).map(|(k, v)| (k.to_lowercase(), v.to_owned())).collect();
                let reply = respond(&line);
                log.lock().unwrap().push(Seen { line, headers });
                let _ = c.write_all(reply.as_bytes());
            }
        });
        let d = tempfile::tempdir().unwrap();
        let cfg = RustfsConfig {
            endpoint: format!("http://127.0.0.1:{port}"),
            region: "us-east-1".into(),
            root_access_key: "a".into(),
            root_secret_key: "b".into(),
            secrets_dir: d.path().join("s"),
            timeout: Duration::from_secs(5),
        };
        (RustfsBackend::new(cfg).unwrap().with_retry(Retry { attempts: 1, base: Duration::from_millis(1) }), seen, d)
    }

    fn ok_put() -> String {
        http(&[("ETag", "\"abc\""), ("x-amz-version-id", "v1")], "")
    }

    const EXEC: ObjectAttrs = ObjectAttrs { executable: true };

    #[test]
    fn a_locked_put_carries_the_execute_attribute_only_when_set() {
        let (be, seen, _d) = fake(|_| ok_put());
        let lock = PutOpts { retention: Some(Retention { mode: LockMode::Governance, until_unix_s: 4_000_000_000 }), ..Default::default() };
        be.put_locked("b-x", "bin/run", b"#!/bin/sh\n", &PutOpts { attrs: EXEC, ..lock }).unwrap();
        be.put_locked("b-x", "plain", b"data", &lock).unwrap();
        let s = seen.lock().unwrap();
        assert_eq!(s[0].header(EXEC_HDR), Some("1"), "{}", s[0].line);
        assert!(s[0].header("x-amz-object-lock-mode").is_some(), "the lock headers are still sent");
        assert_eq!(s[1].header(EXEC_HDR), None);
    }

    #[test]
    fn a_streamed_object_that_fits_one_part_carries_the_execute_attribute() {
        let (be, seen, _d) = fake(|_| ok_put());
        be.put_stream("b-x", "bin/run", &mut std::io::Cursor::new(b"#!/bin/sh\n".to_vec()), crate::backend::MIN_PART, &PutOpts { attrs: EXEC, ..Default::default() }).unwrap();
        let s = seen.lock().unwrap();
        assert_eq!(s[0].header(EXEC_HDR), Some("1"), "{}", s[0].line);
    }

    #[test]
    fn a_multipart_upload_sends_the_execute_attribute_when_it_is_created() {
        let (be, seen, _d) = fake(|line| {
            if line.contains("uploads") {
                http(&[], "<InitiateMultipartUploadResult><UploadId>u1</UploadId></InitiateMultipartUploadResult>")
            } else {
                ok_put()
            }
        });
        assert_eq!(be.create_multipart("b-x", "big/run", &PutOpts { attrs: EXEC, ..Default::default() }).unwrap(), "u1");
        assert_eq!(be.create_multipart("b-x", "big/data", &PutOpts::default()).unwrap(), "u1");
        let s = seen.lock().unwrap();
        assert!(s[0].line.starts_with("POST ") && s[0].line.contains("uploads"), "{}", s[0].line);
        assert_eq!(s[0].header(EXEC_HDR), Some("1"));
        assert_eq!(s[1].header(EXEC_HDR), None);
    }

    #[test]
    fn a_listing_carries_each_objects_modification_stamp() {
        let xml = "<ListBucketResult><IsTruncated>false</IsTruncated>\
            <Contents><Key>p/a</Key><LastModified>2026-11-10T12:00:00.123Z</LastModified><ETag>\"e1\"</ETag><Size>3</Size></Contents>\
            <Contents><Key>p/b</Key><LastModified>2026-11-10T12:00:05.000Z</LastModified><ETag>\"e2\"</ETag><Size>4</Size></Contents>\
            </ListBucketResult>";
        let (be, _seen, _d) = fake(move |_| http(&[], xml));
        let l = be.list_stamped("b-x", "p/").unwrap();
        assert_eq!(l.iter().map(|(m, s)| (m.key.as_str(), m.size, s.as_str())).collect::<Vec<_>>(), [("p/a", 3, "2026-11-10T12:00:00.123Z"), ("p/b", 4, "2026-11-10T12:00:05.000Z")]);
        assert_eq!(be.list("b-x", "p/").unwrap().len(), 2, "list is the same listing without the stamps");
    }
}
