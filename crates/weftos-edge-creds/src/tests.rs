use std::sync::Arc;

use crate::broker::{BrokerConfig, EdgeBroker};
use crate::clock::ManualClock;
use crate::creds::{CredentialRefusal, TempCredentials};
use crate::edge::{EdgeError, EdgeNodeStore, PutOutcome};
use crate::mock::{MockRustfs, StaticGrants, VecAudit};
use crate::policy::{Access, bucket_session_policy, validate_key, validate_node_id};
use crate::sigv4::{self, Canonical, HeaderSigner};
use crate::wire::*;

const T0: u64 = 1_800_000_000;

type Broker = EdgeBroker<MockRustfs, StaticGrants, VecAudit, ManualClock>;

struct Rig {
    clock: ManualClock,
    store: MockRustfs,
    grants: StaticGrants,
    audit: VecAudit,
    broker: Arc<Broker>,
}

fn rig() -> Rig {
    let clock = ManualClock::new(T0);
    let store = MockRustfs::new(clock.clone());
    let grants = StaticGrants::default();
    let audit = VecAudit::default();
    let cfg = BrokerConfig::new(Endpoint { scheme: "http".into(), host: "pg.tailnet:9000".into() });
    let broker = Arc::new(EdgeBroker::new(cfg, store.clone(), grants.clone(), audit.clone(), clock.clone()));
    Rig { clock, store, grants, audit, broker }
}

/// The parent link as the edge sees it: the link has already authenticated `node`.
struct Link {
    node: String,
    broker: Arc<Broker>,
}

impl ParentLink for Link {
    async fn request_lease(&self, req: LeaseRequest) -> Result<LeaseResponse, LeaseRefused> {
        // Round-trip through JSON: this is what crosses the wire.
        let req: LeaseRequest = serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
        let resp = self.broker.lease(&self.node, req).await?;
        Ok(serde_json::from_str(&serde_json::to_string(&resp).unwrap()).unwrap())
    }
}

fn edge(r: &Rig, node: &str) -> EdgeNodeStore<Link, MockRustfs, ManualClock> {
    EdgeNodeStore::new(Link { node: node.into(), broker: r.broker.clone() }, r.store.clone(), r.clock.clone())
}

fn run<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(f)
}

// ---- SigV4 known answers (AWS S3 documentation examples) -------------------

const AWS_AK: &str = "AKIAIOSFODNN7EXAMPLE";
const AWS_SK: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";

#[test]
fn sigv4_header_known_answer() {
    let headers = vec![
        ("host".to_string(), "examplebucket.s3.amazonaws.com".to_string()),
        ("range".to_string(), "bytes=0-9".to_string()),
        ("x-amz-content-sha256".to_string(), sigv4::sha256_hex(b"")),
        ("x-amz-date".to_string(), "20130524T000000Z".to_string()),
    ];
    let (canon, signed) = sigv4::canonical_request(&Canonical {
        method: "GET",
        path: "/test.txt",
        query: &[],
        headers: &headers,
        payload_hash: &sigv4::sha256_hex(b""),
    });
    assert_eq!(signed, "host;range;x-amz-content-sha256;x-amz-date");
    assert_eq!(
        sigv4::sign(AWS_SK, "20130524T000000Z", "us-east-1", &canon),
        "f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
    );
}

#[test]
fn sigv4_presign_known_answer() {
    let signer = HeaderSigner { access_key: AWS_AK, secret_key: AWS_SK, session_token: None, region: "us-east-1", now: sigv4::parse_stamp("20130524T000000Z").unwrap() };
    let p = sigv4::presign(&signer, "examplebucket.s3.amazonaws.com", "GET", "/test.txt", 86_400);
    let sig = &p.query.iter().find(|(k, _)| k == "X-Amz-Signature").unwrap().1;
    assert_eq!(sig, "aeeed9bbccd4d02ee5c0109b86d86835f995330da4c265957d157751f604d404");
}

#[test]
fn stamps_round_trip() {
    for t in [0, 86_399, 951_782_400, T0, 4_102_444_799] {
        assert_eq!(sigv4::parse_stamp(&sigv4::amz_dates(t).1), Some(t));
    }
    assert_eq!(sigv4::amz_dates(T0).0.len(), 8);
}

// ---- the done condition: a Wasm node writes with no long-lived key ----------

#[test]
fn wasm_node_writes_to_its_bucket_with_only_a_short_lived_session() {
    let r = rig();
    r.grants.grant("wasm-7", &[Access::Write]);
    let node = edge(&r, "wasm-7");
    run(async {
        node.put("readings/2026-11-07.json", b"{\"t\":21.5}").await.unwrap();
        assert_eq!(node.get("readings/2026-11-07.json").await.unwrap().unwrap(), b"{\"t\":21.5}");
    });
    assert_eq!(r.store.object("n-wasm-7", "readings/2026-11-07.json").unwrap(), b"{\"t\":21.5}");
    // The store only ever saw STS access keys (ASIA...), never a static AKIA key.
    let log = r.store.log();
    assert!(!log.is_empty() && log.iter().all(|(_, ak, _, _)| ak.starts_with("ASIA")));
    assert!(log.iter().all(|(s, ..)| *s < 300));
    // One lease served both requests.
    assert_eq!(r.store.session_keys().len(), 1);
    // The activity log saw the lease, without secrets.
    let ev = r.audit.events();
    assert_eq!(ev.len(), 1);
    assert!(ev[0].refused.is_none() && !ev[0].detail.contains("secret"));
}

#[test]
fn conditional_put_and_delete_work() {
    let r = rig();
    r.grants.grant("wasm-7", &[Access::Write]);
    let node = edge(&r, "wasm-7");
    run(async {
        assert_eq!(node.put_if_absent("k", b"1").await.unwrap(), PutOutcome::Written);
        assert_eq!(node.put_if_absent("k", b"2").await.unwrap(), PutOutcome::AlreadyExists);
        assert_eq!(node.get("k").await.unwrap().unwrap(), b"1");
        node.delete("k").await.unwrap();
        assert!(node.get("k").await.unwrap().is_none());
    });
}

#[test]
fn presigned_put_needs_no_credentials_on_the_edge() {
    let r = rig();
    r.grants.grant("wasm-7", &[Access::Write]);
    let node = edge(&r, "wasm-7");
    run(async { node.put_presigned("blob/a.bin", b"payload", 120).await.unwrap() });
    assert_eq!(r.store.object("n-wasm-7", "blob/a.bin").unwrap(), b"payload");
}

// ---- scope ------------------------------------------------------------------

#[test]
fn a_node_cannot_reach_another_nodes_bucket() {
    let r = rig();
    r.grants.grant("node-a", &[Access::Write]);
    r.grants.grant("node-b", &[Access::Write]);
    run(async {
        let a = edge(&r, "node-a");
        a.put("mine", b"a").await.unwrap();
        // Take A's lease and aim it at B's bucket by hand.
        let lease = Link { node: "node-a".into(), broker: r.broker.clone() }.request_lease(LeaseRequest::Sts).await.unwrap();
        let LeaseResponse::Sts { credentials, region, .. } = lease else { panic!() };
        let signer = HeaderSigner { access_key: &credentials.access_key, secret_key: &credentials.secret_key, session_token: Some(&credentials.session_token), region: &region, now: T0 };
        let base = vec![("host".to_string(), "pg.tailnet:9000".to_string())];
        let headers = signer.sign("PUT", "/n-node-b/stolen", &[], &base, b"x");
        let resp = r.store.send(HttpRequest { method: "PUT".into(), scheme: "http".into(), host: "pg.tailnet:9000".into(), path: "/n-node-b/stolen".into(), query: vec![], headers, body: b"x".to_vec() }).await.unwrap();
        assert_eq!(resp.status, 403);
    });
    assert!(r.store.object("n-node-b", "stolen").is_none());
}

#[test]
fn bucket_comes_from_the_authenticated_node_not_the_request() {
    let r = rig();
    r.grants.grant("node-a", &[Access::Write]);
    let resp = run(r.broker.lease("node-a", LeaseRequest::Sts)).unwrap();
    let LeaseResponse::Sts { bucket, .. } = resp else { panic!() };
    assert_eq!(bucket, "n-node-a");
    // The request type has no bucket field at all.
    assert!(!serde_json::to_string(&LeaseRequest::Sts).unwrap().contains("bucket"));
}

#[test]
fn session_policy_names_only_the_node_bucket() {
    let p = bucket_session_policy("n-x", Access::Write);
    assert!(p.contains("arn:aws:s3:::n-x/*") && p.contains("\"arn:aws:s3:::n-x\""));
    assert_eq!(p.matches("arn:aws:s3:::").count(), 2);
    assert!(!p.contains("s3:*") && !p.contains("\"*\""));
}

#[test]
fn presigned_url_is_bound_to_its_key() {
    let r = rig();
    r.grants.grant("wasm-7", &[Access::Write]);
    run(async {
        let LeaseResponse::Presigned(p) = r.broker.lease("wasm-7", LeaseRequest::PresignPut { key: "a".into(), ttl_secs: 120 }).await.unwrap() else { panic!() };
        let send = |path: &str, host: &str| {
            let req = HttpRequest { method: "PUT".into(), scheme: "http".into(), host: host.into(), path: path.into(), query: p.query.clone(), headers: vec![], body: b"x".to_vec() };
            let s = r.store.clone();
            async move { s.send(req).await.unwrap().status }
        };
        assert_eq!(send("/n-wasm-7/other", "pg.tailnet:9000").await, 403, "other key");
        assert_eq!(send("/n-node-b/a", "pg.tailnet:9000").await, 403, "other bucket");
        assert_eq!(send("/n-wasm-7/a", "evil:9000").await, 403, "other host");
        assert_eq!(send("/n-wasm-7/a", "pg.tailnet:9000").await, 200);
    });
}

// ---- expiry and refresh -----------------------------------------------------

#[test]
fn lease_refreshes_before_expiry_without_the_caller_noticing() {
    let r = rig();
    r.grants.grant("wasm-7", &[Access::Write]);
    let node = edge(&r, "wasm-7");
    run(async {
        node.put("a", b"1").await.unwrap();
        r.clock.advance(900 - 30); // inside the refresh margin
        node.put("b", b"2").await.unwrap();
        r.clock.advance(2_000); // long past the first two sessions
        node.put("c", b"3").await.unwrap();
    });
    assert_eq!(r.store.session_keys().len(), 3);
    assert_eq!(r.store.object_count(), 3);
}

#[test]
fn an_expired_session_is_refused_by_the_store() {
    let r = rig();
    r.grants.grant("wasm-7", &[Access::Write]);
    run(async {
        let LeaseResponse::Sts { credentials, bucket, .. } = r.broker.lease("wasm-7", LeaseRequest::Sts).await.unwrap() else { panic!() };
        r.clock.advance(901);
        let signer = HeaderSigner { access_key: &credentials.access_key, secret_key: &credentials.secret_key, session_token: Some(&credentials.session_token), region: "us-east-1", now: r.clock_now() };
        let path = format!("/{bucket}/k");
        let headers = signer.sign("PUT", &path, &[], &[("host".into(), "pg.tailnet:9000".into())], b"x");
        let resp = r.store.send(HttpRequest { method: "PUT".into(), scheme: "http".into(), host: "pg.tailnet:9000".into(), path, query: vec![], headers, body: b"x".to_vec() }).await.unwrap();
        assert_eq!(resp.status, 403);
    });
}

impl Rig {
    fn clock_now(&self) -> u64 {
        use crate::clock::Clock;
        self.clock.now()
    }
}

// ---- revocation -------------------------------------------------------------

#[test]
fn revoking_a_node_refuses_its_very_next_request() {
    let r = rig();
    r.grants.grant("wasm-7", &[Access::Write]);
    let node = edge(&r, "wasm-7");
    run(async {
        node.put("a", b"1").await.unwrap();
        r.broker.revoke("wasm-7").await.unwrap();
        // Live session: the store refuses (user disabled), the retry asks the
        // parent, and the parent refuses too.
        let err = node.put("b", b"2").await.unwrap_err();
        assert!(matches!(err, EdgeError::Parent(LeaseRefused { code: RefusalCode::Revoked, .. })), "{err:?}");
        assert!(r.store.object("n-wasm-7", "b").is_none());
        // Presign is refused too.
        assert!(node.put_presigned("c", b"3", 60).await.is_err());
        // Restore brings it back.
        r.broker.restore("wasm-7").await.unwrap();
        node.put("b", b"2").await.unwrap();
    });
    let ev = r.audit.events();
    assert!(ev.iter().any(|e| e.action == "revoke"));
    assert!(ev.iter().any(|e| e.refused == Some(RefusalCode::Revoked)));
}

#[test]
fn no_grant_no_lease() {
    let r = rig();
    let node = edge(&r, "wasm-9");
    let err = run(node.put("a", b"1")).unwrap_err();
    assert!(matches!(err, EdgeError::Parent(LeaseRefused { code: RefusalCode::NotGranted, .. })));
    assert_eq!(r.store.session_keys().len(), 0);
    // A read-only grant gets no STS write lease.
    r.grants.grant("wasm-9", &[Access::Read]);
    assert!(run(node.put("a", b"1")).is_err());
}

#[test]
fn lease_storms_are_rate_limited() {
    let r = rig();
    r.grants.grant("wasm-7", &[Access::Write]);
    let mut denied = 0;
    for _ in 0..40 {
        if let Err(e) = run(r.broker.lease("wasm-7", LeaseRequest::Sts)) {
            assert_eq!(e.code, RefusalCode::RateLimited);
            denied += 1;
        }
    }
    assert_eq!(denied, 10);
    r.clock.advance(61);
    assert!(run(r.broker.lease("wasm-7", LeaseRequest::Sts)).is_ok());
}

// ---- the edge refuses anything long-lived ------------------------------------

#[test]
fn edge_refuses_long_lived_or_tokenless_credentials() {
    let now = T0;
    let mk = |token: &str, ttl: u64| TempCredentials { access_key: "AKIA".into(), secret_key: "s".into(), session_token: token.into(), expires_at: now + ttl };
    assert_eq!(mk("", 600).admit(now), Err(CredentialRefusal::NoSessionToken));
    assert!(matches!(mk("t", 3_600).admit(now), Err(CredentialRefusal::TooLong(_))));
    assert_eq!(mk("t", 0).admit(now), Err(CredentialRefusal::Expired));
    assert!(mk("t", 900).admit(now).is_ok());
}

#[test]
fn a_parent_that_leaks_a_long_lived_key_is_rejected_by_the_edge() {
    struct BadLink;
    impl ParentLink for BadLink {
        async fn request_lease(&self, _: LeaseRequest) -> Result<LeaseResponse, LeaseRefused> {
            Ok(LeaseResponse::Sts {
                credentials: TempCredentials { access_key: "AKIAROOT".into(), secret_key: "root".into(), session_token: String::new(), expires_at: u64::MAX / 2 },
                bucket: "n-x".into(),
                region: "us-east-1".into(),
                endpoint: Endpoint { scheme: "http".into(), host: "h".into() },
            })
        }
    }
    let r = rig();
    let node = EdgeNodeStore::new(BadLink, r.store.clone(), r.clock.clone());
    let err = run(node.put("k", b"v")).unwrap_err();
    assert!(matches!(err, EdgeError::Credential(CredentialRefusal::NoSessionToken)), "{err:?}");
    assert_eq!(r.store.object_count(), 0);
}

#[test]
fn secrets_are_redacted_in_debug_output() {
    let c = TempCredentials { access_key: "ASIA1".into(), secret_key: "TOPSECRET".into(), session_token: "TOKEN123".into(), expires_at: 1 };
    let d = format!("{c:?}");
    assert!(!d.contains("TOPSECRET") && !d.contains("TOKEN123") && d.contains("ASIA1"));
}

// ---- input validation --------------------------------------------------------

#[test]
fn keys_and_node_ids_are_validated() {
    for bad in ["", "/abs", "a/../b", "..", "a//b", "a/./b", "a\\b", "a\0b", "a\nb"] {
        assert!(validate_key(bad).is_err(), "{bad:?}");
    }
    assert!(validate_key(&"k".repeat(1025)).is_err());
    // RustFS refuses a path segment over 255 bytes however short the whole key is.
    assert!(validate_key(&"k".repeat(255)).is_ok());
    assert!(validate_key(&"k".repeat(256)).is_err());
    assert!(validate_key(&format!("a/{}", "k".repeat(256))).is_err());
    assert!(validate_key(&format!("{}/{}", "k".repeat(255), "k".repeat(255))).is_ok());
    // Bytes, not characters: 128 two-byte characters are 256 bytes.
    assert!(validate_key(&"\u{e9}".repeat(127)).is_ok());
    assert!(validate_key(&"\u{e9}".repeat(128)).is_err());
    for ok in ["a", "a/b/c.json", "agents/01HX/out.bin", "with space/ünï"] {
        assert!(validate_key(ok).is_ok(), "{ok:?}");
    }
    for bad in ["", "-a", "a-", "A", "a_b", "a/b", "../x", &"a".repeat(49)] {
        assert!(validate_node_id(bad).is_err(), "{bad:?}");
    }
    let r = rig();
    r.grants.grant("wasm-7", &[Access::Write]);
    let e = run(r.broker.lease("wasm-7", LeaseRequest::PresignPut { key: "../n-other/x".into(), ttl_secs: 60 })).unwrap_err();
    assert_eq!(e.code, RefusalCode::BadRequest);
    let e = run(r.broker.lease("../root", LeaseRequest::Sts)).unwrap_err();
    assert_eq!(e.code, RefusalCode::BadRequest);
    // The edge also refuses a bad key before any network use.
    let node = edge(&r, "wasm-7");
    assert!(matches!(run(node.put("a/../b", b"x")), Err(EdgeError::Name(_))));
}

#[test]
fn presign_ttl_is_capped_at_fifteen_minutes() {
    let r = rig();
    r.grants.grant("wasm-7", &[Access::Write]);
    let LeaseResponse::Presigned(p) = run(r.broker.lease("wasm-7", LeaseRequest::PresignPut { key: "a".into(), ttl_secs: 86_400 })).unwrap() else { panic!() };
    assert_eq!(p.expires_at, T0 + 900);
    assert!(p.query.iter().any(|(k, v)| k == "X-Amz-Expires" && v == "900"));
}

#[test]
fn wire_types_round_trip() {
    let req = LeaseRequest::PresignGet { key: "a/b".into(), ttl_secs: 5 };
    let j = serde_json::to_string(&req).unwrap();
    assert!(j.contains("\"op\":\"presign_get\""));
    assert_eq!(serde_json::from_str::<LeaseRequest>(&j).unwrap(), req);
}
