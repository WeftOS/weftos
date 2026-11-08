//! The live path end to end: EdgeNodeStore -> link -> EdgeBroker ->
//! RustfsMinter (signed AssumeRole / admin calls) -> a store that verifies
//! signatures, session tokens and policy intersection. Grants come from
//! chain events through GrantLedger.

use std::collections::HashMap;
use std::sync::Arc;

use crate::broker::{BrokerConfig, EdgeBroker};
use crate::clock::{Clock as _, ManualClock};
use crate::edge::{EdgeError, EdgeNodeStore};
use crate::grants::{Applied, GrantEvent, GrantLedger, GrantOp, apply_and_enforce, node_store_name};
use crate::mock::{MockRustfs, VecAudit};
use crate::policy::{Access, bucket_session_policy};
use crate::rustfs::{NodeKeys, RustfsConfig, RustfsMinter, StaticKey};
use crate::wire::*;

const T0: u64 = 1_800_000_000;
const MESH: &str = "m1";
const HOST: &str = "pg.tailnet:9000";

#[derive(Clone, Default)]
struct Keys(Arc<HashMap<String, (String, String)>>);

impl NodeKeys for Keys {
    fn key_for(&self, principal: &str) -> Option<StaticKey> {
        self.0.get(principal).map(|(a, s)| StaticKey { access_key: a.clone(), secret_key: s.clone() })
    }
}

type Minter = RustfsMinter<MockRustfs, Keys, ManualClock>;
type Broker = EdgeBroker<Minter, Arc<GrantLedger>, VecAudit, ManualClock>;

struct Rig {
    clock: ManualClock,
    store: MockRustfs,
    ledger: Arc<GrantLedger>,
    audit: VecAudit,
    broker: Arc<Broker>,
}

/// Nodes whose key the parent will hold, but with nothing created in RustFS yet.
fn rig_unprovisioned(nodes: &[&str]) -> Rig {
    let clock = ManualClock::new(T0);
    let store = MockRustfs::new(clock.clone());
    store.add_admin("rootadmin", "adminsecret");
    let keys: HashMap<_, _> = nodes.iter().map(|n| (format!("node-{n}"), (format!("node-{n}"), format!("generated-secret-{n}")))).collect();
    let ep = Endpoint { scheme: "http".into(), host: HOST.into() };
    let minter = RustfsMinter::new(
        RustfsConfig::new(ep.clone()),
        store.clone(),
        Keys(Arc::new(keys)),
        StaticKey { access_key: "rootadmin".into(), secret_key: "adminsecret".into() },
        clock.clone(),
    );
    let ledger = Arc::new(GrantLedger::new());
    let audit = VecAudit::default();
    let broker = Arc::new(EdgeBroker::new(BrokerConfig::new(ep), minter, ledger.clone(), audit.clone(), clock.clone()));
    Rig { clock, store, ledger, audit, broker }
}

/// `users`: node ids provisioned in RustFS with their own-bucket policy.
/// `wrong_base`: node ids whose attached policy names someone else's bucket.
fn rig(users: &[&str], wrong_base: &[&str]) -> Rig {
    let clock = ManualClock::new(T0);
    let store = MockRustfs::new(clock.clone());
    store.add_admin("rootadmin", "adminsecret");
    let mut keys = HashMap::new();
    for n in users.iter().chain(wrong_base) {
        let bucket = if wrong_base.contains(n) { "n-someone-else".to_string() } else { format!("n-{n}") };
        let secret = format!("long-lived-secret-of-{n}");
        store.add_user(&format!("node-{n}"), &secret, &bucket_session_policy(&bucket, Access::Write));
        keys.insert(format!("node-{n}"), (format!("node-{n}"), secret));
    }
    let ep = Endpoint { scheme: "http".into(), host: HOST.into() };
    let minter = RustfsMinter::new(
        RustfsConfig::new(ep.clone()),
        store.clone(),
        Keys(Arc::new(keys)),
        StaticKey { access_key: "rootadmin".into(), secret_key: "adminsecret".into() },
        clock.clone(),
    );
    let ledger = Arc::new(GrantLedger::new());
    let audit = VecAudit::default();
    let broker = Arc::new(EdgeBroker::new(BrokerConfig::new(ep), minter, ledger.clone(), audit.clone(), clock.clone()));
    Rig { clock, store, ledger, audit, broker }
}

struct Link {
    node: String,
    broker: Arc<Broker>,
}

impl ParentLink for Link {
    async fn request_lease(&self, req: LeaseRequest) -> Result<LeaseResponse, LeaseRefused> {
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

fn pair(r: &Rig, seq: u64, node: &str) {
    r.ledger.apply(&GrantEvent::pairing(seq, MESH, node));
}

// ---- the acceptance path ------------------------------------------------------

#[test]
fn paired_wasm_node_writes_through_the_live_minter_without_a_long_lived_key() {
    let r = rig(&["wasm-1"], &[]);
    pair(&r, 1, "wasm-1");
    let node = edge(&r, "wasm-1");
    run(async {
        node.put("readings/a.json", b"{}").await.unwrap();
        assert_eq!(node.get("readings/a.json").await.unwrap().unwrap(), b"{}");
        node.put_presigned("blob/b.bin", b"bb", 60).await.unwrap();
    });
    assert_eq!(r.store.object("n-wasm-1", "readings/a.json").unwrap(), b"{}");
    assert_eq!(r.store.object("n-wasm-1", "blob/b.bin").unwrap(), b"bb");
    // Objects were only ever touched with STS (ASIA) keys; the long-lived
    // user key signed STS calls and nothing else.
    for (status, ak, _, path) in r.store.log() {
        assert!(status < 300, "{path}");
        assert!(ak.starts_with("ASIA") || (ak == "node-wasm-1" && path == "sts"), "{ak} {path}");
    }
}

#[test]
fn the_long_lived_secret_never_appears_in_anything_sent_to_the_edge() {
    let r = rig(&["wasm-1"], &[]);
    pair(&r, 1, "wasm-1");
    for req in [LeaseRequest::Sts, LeaseRequest::PresignPut { key: "k".into(), ttl_secs: 60 }] {
        let wire = serde_json::to_string(&run(r.broker.lease("wasm-1", req)).unwrap()).unwrap();
        assert!(!wire.contains("long-lived-secret") && !wire.contains("adminsecret"));
    }
    assert!(r.audit.events().iter().all(|e| !e.detail.contains("secret")));
}

// ---- scope: session policy intersects the user's attached policy --------------

#[test]
fn a_session_cannot_exceed_the_users_attached_policy() {
    // node-x's RustFS user is only allowed on someone else's bucket, so even a
    // correctly scoped session policy for n-x yields nothing.
    let r = rig(&["wasm-1"], &["x"]);
    pair(&r, 1, "x");
    let err = run(edge(&r, "x").put("k", b"v")).unwrap_err();
    assert!(matches!(err, EdgeError::Forbidden(403)), "{err:?}");
    assert_eq!(r.store.object_count(), 0);
}

#[test]
fn one_nodes_session_is_refused_on_another_nodes_bucket() {
    let r = rig(&["a", "b"], &[]);
    pair(&r, 1, "a");
    pair(&r, 2, "b");
    run(async {
        edge(&r, "a").put("mine", b"1").await.unwrap();
        edge(&r, "b").put("mine", b"2").await.unwrap();
    });
    assert_eq!(r.store.object("n-a", "mine").unwrap(), b"1");
    assert_eq!(r.store.object("n-b", "mine").unwrap(), b"2");
    // A's lease, signed by hand against B's bucket.
    let LeaseResponse::Sts { credentials: c, .. } = run(r.broker.lease("a", LeaseRequest::Sts)).unwrap() else { panic!() };
    let signer = crate::sigv4::HeaderSigner { access_key: &c.access_key, secret_key: &c.secret_key, session_token: Some(&c.session_token), region: "us-east-1", now: T0 };
    let headers = signer.sign("PUT", "/n-b/x", &[], &[("host".into(), HOST.into())], b"x");
    let resp = run(crate::wire::ObjectTransport::send(&r.store, HttpRequest { method: "PUT".into(), scheme: "http".into(), host: HOST.into(), path: "/n-b/x".into(), query: vec![], headers, body: b"x".to_vec() })).unwrap();
    assert_eq!(resp.status, 403);
}

// ---- revocation from the chain -------------------------------------------------

#[test]
fn an_unpair_event_cuts_the_node_off_at_its_next_request() {
    let r = rig(&["wasm-1"], &[]);
    pair(&r, 1, "wasm-1");
    let node = edge(&r, "wasm-1");
    run(async {
        node.put("a", b"1").await.unwrap();
        let ev = GrantEvent::unpair(2, MESH, "wasm-1");
        assert_eq!(apply_and_enforce(&r.ledger, &r.broker, &ev).await.unwrap(), Applied::Changed { lost_write: true });
        // Same instant, still-valid session: refused anyway.
        let err = node.put("b", b"2").await.unwrap_err();
        assert!(matches!(err, EdgeError::Parent(LeaseRefused { code: RefusalCode::Revoked, .. })), "{err:?}");
        assert!(node.put_presigned("c", b"3", 60).await.is_err());
    });
    assert!(r.store.object("n-wasm-1", "b").is_none());
    // The admin call went out signed and was accepted.
    assert!(r.store.log().iter().any(|(s, ak, m, p)| *s == 200 && ak == "rootadmin" && m == "PUT" && p.ends_with("/set-user-status")));
}

#[test]
fn a_live_session_is_refused_by_the_store_itself_once_the_user_is_disabled() {
    let r = rig(&["wasm-1"], &[]);
    pair(&r, 1, "wasm-1");
    let LeaseResponse::Sts { credentials: c, .. } = run(r.broker.lease("wasm-1", LeaseRequest::Sts)).unwrap() else { panic!() };
    let put = |r: &Rig| {
        let signer = crate::sigv4::HeaderSigner { access_key: &c.access_key, secret_key: &c.secret_key, session_token: Some(&c.session_token), region: "us-east-1", now: T0 };
        let headers = signer.sign("PUT", "/n-wasm-1/k", &[], &[("host".into(), HOST.into())], b"x");
        run(crate::wire::ObjectTransport::send(&r.store, HttpRequest { method: "PUT".into(), scheme: "http".into(), host: HOST.into(), path: "/n-wasm-1/k".into(), query: vec![], headers, body: b"x".to_vec() })).unwrap().status
    };
    assert_eq!(put(&r), 200);
    run(r.broker.revoke("wasm-1")).unwrap();
    assert_eq!(put(&r), 403, "the session the edge still holds is dead");
    run(r.broker.restore("wasm-1")).unwrap();
    assert_eq!(put(&r), 200);
}

#[test]
fn a_failed_admin_call_still_blocks_new_leases() {
    let r = rig(&["wasm-1"], &[]);
    pair(&r, 1, "wasm-1");
    // A principal RustFS has no key for: disable cannot be issued...
    let r2 = rig(&[], &[]);
    pair(&r2, 1, "ghost");
    assert!(run(r2.broker.revoke("ghost")).is_err());
    // ...but the parent has already stopped issuing.
    let e = run(r2.broker.lease("ghost", LeaseRequest::Sts)).unwrap_err();
    assert_eq!(e.code, RefusalCode::Revoked);
    drop(r);
}

// ---- the minter's wire behaviour ------------------------------------------------

#[test]
fn presigned_leases_still_ask_sts_for_at_least_900_seconds() {
    let r = rig(&["wasm-1"], &[]);
    pair(&r, 1, "wasm-1");
    // ttl 60 would be refused by STS (400); the minter raises the session to 900
    // while the URL itself still expires in 60.
    let LeaseResponse::Presigned(p) = run(r.broker.lease("wasm-1", LeaseRequest::PresignPut { key: "k".into(), ttl_secs: 60 })).unwrap() else { panic!() };
    assert_eq!(p.expires_at, T0 + 60);
}

#[test]
fn an_oversize_session_policy_is_refused_before_anything_is_sent() {
    let r = rig(&["wasm-1"], &[]);
    pair(&r, 1, "wasm-1");
    // Prove the size guard on the minter directly.
    let big = format!("{{\"Statement\":[],\"pad\":\"{}\"}}", "x".repeat(2100));
    let minter = RustfsMinter::new(
        RustfsConfig::new(Endpoint { scheme: "http".into(), host: HOST.into() }),
        r.store.clone(),
        Keys(Arc::new(HashMap::from([("node-wasm-1".to_string(), ("node-wasm-1".to_string(), "s".to_string()))]))),
        StaticKey { access_key: "rootadmin".into(), secret_key: "adminsecret".into() },
        r.clock.clone(),
    );
    let before = r.store.log().len();
    let e = run(crate::broker::StsMinter::assume_role(&minter, "node-wasm-1", &big, 900)).unwrap_err();
    assert!(e.0.contains("limit 2048"));
    assert_eq!(r.store.log().len(), before, "no request left the parent");
}

#[test]
fn a_wrong_user_secret_or_wrong_admin_secret_is_refused_by_the_store() {
    let r = rig(&["wasm-1"], &[]);
    let ep = Endpoint { scheme: "http".into(), host: HOST.into() };
    let mk = |user_secret: &str, admin_secret: &str| {
        RustfsMinter::new(
            RustfsConfig::new(ep.clone()),
            r.store.clone(),
            Keys(Arc::new(HashMap::from([("node-wasm-1".to_string(), ("node-wasm-1".to_string(), user_secret.to_string()))]))),
            StaticKey { access_key: "rootadmin".into(), secret_key: admin_secret.into() },
            r.clock.clone(),
        )
    };
    use crate::broker::StsMinter;
    let policy = bucket_session_policy("n-wasm-1", Access::Write);
    assert!(run(mk("long-lived-secret-of-wasm-1", "adminsecret").assume_role("node-wasm-1", &policy, 900)).is_ok());
    assert!(run(mk("wrong", "adminsecret").assume_role("node-wasm-1", &policy, 900)).is_err());
    assert!(run(mk("long-lived-secret-of-wasm-1", "wrong").disable_principal("node-wasm-1")).is_err());
    assert!(run(mk("long-lived-secret-of-wasm-1", "adminsecret").disable_principal("node-wasm-1")).is_ok());
    // A node user is not an admin.
    let sneaky = RustfsMinter::new(
        RustfsConfig::new(ep.clone()),
        r.store.clone(),
        Keys(Arc::new(HashMap::from([("node-wasm-1".to_string(), ("node-wasm-1".to_string(), "s".to_string()))]))),
        StaticKey { access_key: "node-wasm-1".into(), secret_key: "long-lived-secret-of-wasm-1".into() },
        r.clock.clone(),
    );
    assert!(run(sneaky.enable_principal("node-wasm-1")).is_err(), "a node user cannot re-enable itself");
}

#[test]
fn rfc3339_expiry_parses() {
    assert_eq!(crate::sigv4::parse_rfc3339("2026-11-07T11:00:00Z"), crate::sigv4::parse_stamp("20261107T110000Z"));
    assert_eq!(crate::sigv4::parse_rfc3339("2026-11-07T11:00:00.123Z"), crate::sigv4::parse_stamp("20261107T110000Z"));
    assert!(crate::sigv4::parse_rfc3339("2026-11-07 11:00:00").is_none());
}

#[test]
fn static_key_debug_is_redacted() {
    let k = StaticKey { access_key: "node-a".into(), secret_key: "SUPERSECRET".into() };
    assert!(!format!("{k:?}").contains("SUPERSECRET"));
}

// ---- the grant ledger ------------------------------------------------------------

#[test]
fn ledger_folds_chain_events_and_replay_rebuilds_it() {
    let events = vec![
        GrantEvent::pairing(1, MESH, "a"),
        GrantEvent::pairing(2, MESH, "b"),
        GrantEvent::unpair(3, MESH, "a"),
        GrantEvent { seq: 4, principal: "b".into(), store: node_store_name(MESH, "b"), access: Access::Write, op: GrantOp::Revoke },
        GrantEvent { seq: 5, principal: "b".into(), store: node_store_name(MESH, "b"), access: Access::Read, op: GrantOp::Grant },
    ];
    let l = GrantLedger::replay(events.clone());
    use crate::broker::GrantCheck;
    assert!(!l.allows("a", Access::Read) && !l.allows("a", Access::Write));
    assert!(l.allows("b", Access::Read) && !l.allows("b", Access::Write));
    // Events survive the chain's JSON.
    let again = GrantLedger::replay(events.iter().map(|e| serde_json::from_str(&serde_json::to_string(e).unwrap()).unwrap()));
    assert!(again.allows("b", Access::Read) && !again.allows("a", Access::Write));
}

#[test]
fn ledger_ignores_replayed_and_out_of_order_events() {
    let l = GrantLedger::new();
    assert!(matches!(l.apply(&GrantEvent::pairing(5, MESH, "a")), Applied::Changed { .. }));
    assert_eq!(l.apply(&GrantEvent::unpair(5, MESH, "a")), Applied::Stale);
    assert_eq!(l.apply(&GrantEvent::unpair(3, MESH, "a")), Applied::Stale);
    assert!(l.holds("a", &node_store_name(MESH, "a"), Access::Write));
}

#[test]
fn a_grant_on_someone_elses_store_does_not_let_a_node_use_its_own() {
    use crate::broker::GrantCheck;
    let l = GrantLedger::new();
    // a reads b's store; that is not a grant on a's own store.
    l.apply(&GrantEvent { seq: 1, principal: "a".into(), store: node_store_name(MESH, "b"), access: Access::Write, op: GrantOp::Grant });
    assert!(!l.allows("a", Access::Write) && !l.allows("a", Access::Read));
    assert!(!l.allows("b", Access::Read));
    // Malformed store names grant nothing.
    l.apply(&GrantEvent { seq: 2, principal: "c".into(), store: "weftos://m1/projects/x/store".into(), access: Access::Write, op: GrantOp::Grant });
    l.apply(&GrantEvent { seq: 3, principal: "c".into(), store: "weftos://m1/nodes/c/store/extra".into(), access: Access::Write, op: GrantOp::Grant });
    assert!(!l.allows("c", Access::Write));
}

#[test]
fn losing_write_but_keeping_read_still_cuts_off_the_write_lease() {
    let r = rig(&["wasm-1"], &[]);
    pair(&r, 1, "wasm-1");
    run(async {
        let down = GrantEvent { seq: 2, principal: "wasm-1".into(), store: node_store_name(MESH, "wasm-1"), access: Access::Write, op: GrantOp::Revoke };
        assert_eq!(apply_and_enforce(&r.ledger, &r.broker, &down).await.unwrap(), Applied::Changed { lost_write: true });
        let e = r.broker.lease("wasm-1", LeaseRequest::Sts).await.unwrap_err();
        assert_eq!(e.code, RefusalCode::Revoked);
    });
}

// ---- pairing: provision and deprovision ----------------------------------------

#[test]
fn pairing_provisions_the_user_policy_and_bucket_and_unpairing_removes_them() {
    let r = rig_unprovisioned(&["wasm-9"]);
    pair(&r, 1, "wasm-9");
    let node = edge(&r, "wasm-9");
    // Nothing exists in RustFS yet: the parent cannot mint.
    assert!(matches!(run(node.put("k", b"v")), Err(EdgeError::Parent(LeaseRefused { code: RefusalCode::Unavailable, .. }))));
    run(r.broker.minter().provision_node("wasm-9", "generated-secret-wasm-9")).unwrap();
    run(node.put("k", b"v")).unwrap();
    assert_eq!(r.store.object("n-wasm-9", "k").unwrap(), b"v");
    // Provisioning twice is harmless.
    run(r.broker.minter().provision_node("wasm-9", "generated-secret-wasm-9")).unwrap();
    run(r.broker.minter().deprovision_node("wasm-9")).unwrap();
    // The session the edge still holds died with its user; a new lease cannot be minted.
    assert!(run(node.put("k2", b"v")).is_err());
    assert!(r.store.object("n-wasm-9", "k2").is_none());
}

#[test]
fn provisioned_user_is_limited_to_its_own_bucket() {
    let r = rig_unprovisioned(&["wasm-9", "wasm-8"]);
    pair(&r, 1, "wasm-9");
    run(r.broker.minter().provision_node("wasm-9", "generated-secret-wasm-9")).unwrap();
    run(r.broker.minter().provision_node("wasm-8", "generated-secret-wasm-8")).unwrap();
    // wasm-9's own user policy is what bounds the session: ask for a session
    // policy on wasm-8's bucket as wasm-9's user and write there.
    use crate::broker::StsMinter;
    let c = run(r.broker.minter().assume_role("node-wasm-9", &bucket_session_policy("n-wasm-8", Access::Write), 900)).unwrap();
    let signer = crate::sigv4::HeaderSigner { access_key: &c.access_key, secret_key: &c.secret_key, session_token: Some(&c.session_token), region: "us-east-1", now: T0 };
    let h = signer.sign("PUT", "/n-wasm-8/x", &[], &[("host".into(), HOST.into())], b"x");
    let resp = run(crate::wire::ObjectTransport::send(&r.store, HttpRequest { method: "PUT".into(), scheme: "http".into(), host: HOST.into(), path: "/n-wasm-8/x".into(), query: vec![], headers: h, body: b"x".to_vec() })).unwrap();
    assert_eq!(resp.status, 403, "session policy cannot widen the user's policy");
}

#[test]
fn provisioning_rejects_bad_node_ids_before_any_request() {
    let r = rig_unprovisioned(&[]);
    let before = r.store.log().len();
    assert!(run(r.broker.minter().provision_node("../x", "s")).is_err());
    assert!(run(r.broker.minter().deprovision_node("A B")).is_err());
    assert_eq!(r.store.log().len(), before);
}

#[test]
fn session_floor_is_configurable_for_stores_that_allow_short_sessions() {
    use crate::broker::StsMinter;
    // The mock refuses < 900 like MinIO; a floor of 60 reaches it as 60 and is refused,
    // the default floor of 900 is accepted.
    let r = rig(&["wasm-1"], &[]);
    let ep = Endpoint { scheme: "http".into(), host: HOST.into() };
    let mk = |floor: u64| {
        let mut cfg = RustfsConfig::new(ep.clone());
        cfg.min_session_secs = floor;
        RustfsMinter::new(
            cfg,
            r.store.clone(),
            Keys(Arc::new(HashMap::from([("node-wasm-1".to_string(), ("node-wasm-1".to_string(), "long-lived-secret-of-wasm-1".to_string()))]))),
            StaticKey { access_key: "rootadmin".into(), secret_key: "adminsecret".into() },
            r.clock.clone(),
        )
    };
    let p = bucket_session_policy("n-wasm-1", Access::Write);
    assert!(run(mk(900).assume_role("node-wasm-1", &p, 60)).is_ok());
    assert!(run(mk(60).assume_role("node-wasm-1", &p, 60)).is_err());
}

#[test]
fn keygen_secret_is_hex_of_entropy_and_refuses_constant_input() {
    use crate::keygen::*;
    let mut e = [0u8; SECRET_ENTROPY_BYTES];
    assert_eq!(secret_from_entropy(&e), Err(KeygenError::Degenerate));
    e = [7u8; SECRET_ENTROPY_BYTES];
    assert_eq!(secret_from_entropy(&e), Err(KeygenError::Degenerate));
    for (i, b) in e.iter_mut().enumerate() {
        *b = i as u8 + 1;
    }
    let s = secret_from_entropy(&e).unwrap();
    assert_eq!(s.len(), SECRET_LEN);
    assert!(s.starts_with("0102030405"));
}

#[test]
fn keygen_node_key_uses_principal_and_validates_id() {
    use crate::keygen::*;
    let mut e = [0u8; SECRET_ENTROPY_BYTES];
    for (i, b) in e.iter_mut().enumerate() {
        *b = (i * 13 + 1) as u8;
    }
    let k = node_key_from_entropy("edge-7", &e).unwrap();
    assert_eq!(k.access_key, "node-edge-7");
    assert!(node_key_from_entropy("Bad/Id", &e).is_err());
    assert!(node_key_from_entropy("", &e).is_err());
}

#[cfg(feature = "keygen")]
#[test]
fn keygen_os_keys_differ_and_are_provisionable() {
    use crate::keygen::*;
    let a = generate_node_key("edge-7").unwrap();
    let b = generate_node_key("edge-7").unwrap();
    assert_ne!(a.secret_key, b.secret_key);
    assert_eq!(a.secret_key.len(), SECRET_LEN);
}

// ---- adversarial review fixes -----------------------------------------------------

/// A minter that misbehaves in the ways a real store could.
struct Rogue {
    lifetime: u64,
    clock: ManualClock,
}

impl crate::broker::StsMinter for Rogue {
    async fn assume_role(&self, _p: &str, _pol: &str, _ttl: u64) -> Result<crate::creds::TempCredentials, crate::broker::StsError> {
        if self.lifetime == 0 {
            return Err(crate::broker::StsError("AssumeRole: HTTP 403 for node-secret-internal".into()));
        }
        Ok(crate::creds::TempCredentials {
            access_key: "A".into(),
            secret_key: "S".into(),
            session_token: "T".into(),
            expires_at: self.clock.now() + self.lifetime,
        })
    }
    async fn disable_principal(&self, _p: &str) -> Result<(), crate::broker::StsError> { Ok(()) }
    async fn enable_principal(&self, _p: &str) -> Result<(), crate::broker::StsError> { Ok(()) }
}

fn rogue_broker(lifetime: u64) -> (EdgeBroker<Rogue, crate::mock::StaticGrants, VecAudit, ManualClock>, VecAudit) {
    let clock = ManualClock::new(T0);
    let g = crate::mock::StaticGrants::default();
    g.grant("n1", &[Access::Write]);
    let audit = VecAudit::default();
    let ep = Endpoint { scheme: "http".into(), host: HOST.into() };
    (EdgeBroker::new(BrokerConfig::new(ep), Rogue { lifetime, clock: clock.clone() }, g, audit.clone(), clock), audit)
}

#[test]
fn broker_refuses_a_session_that_outlives_the_cap_even_if_the_store_granted_it() {
    let (b, _) = rogue_broker(3600);
    let e = run(b.lease("n1", LeaseRequest::Sts)).unwrap_err();
    assert_eq!(e.code, RefusalCode::Unavailable);
    let e = run(b.lease("n1", LeaseRequest::PresignPut { key: "k".into(), ttl_secs: 60 })).unwrap_err();
    assert_eq!(e.code, RefusalCode::Unavailable);
    // A compliant store is still served.
    let (ok, _) = rogue_broker(900);
    assert!(run(ok.lease("n1", LeaseRequest::Sts)).is_ok());
}

#[test]
fn minter_detail_goes_to_the_audit_log_not_to_the_edge() {
    let (b, audit) = rogue_broker(0);
    let e = run(b.lease("n1", LeaseRequest::Sts)).unwrap_err();
    assert!(!e.message.contains("secret-internal") && !e.message.contains("403"), "{}", e.message);
    assert!(audit.events().iter().any(|a| a.detail.contains("secret-internal")));
}

#[test]
fn revoke_disables_the_user_even_when_the_key_store_has_no_entry() {
    // Provisioned in RustFS, but absent from the parent's key map.
    let r = rig(&[], &[]);
    r.store.add_user("node-ghost", "s", &bucket_session_policy("n-ghost", Access::Write));
    pair(&r, 1, "ghost");
    run(r.broker.revoke("ghost")).unwrap();
    assert!(r.store.log().iter().any(|(s, _, _, p)| *s == 200 && p.contains("set-user-status")), "{:?}", r.store.log());
}

#[test]
fn debug_output_of_transport_types_carries_no_secrets() {
    let req = HttpRequest {
        method: "PUT".into(), scheme: "http".into(), host: HOST.into(), path: "/p".into(), query: vec![],
        headers: vec![("x-amz-security-token".into(), "TOKEN-SECRET".into())],
        body: b"secretKey=LONG-LIVED".to_vec(),
    };
    let resp = HttpResponse { status: 200, headers: vec![], body: b"<SecretAccessKey>SAK</SecretAccessKey>".to_vec() };
    let pre = Presigned { method: "GET".into(), endpoint: Endpoint { scheme: "http".into(), host: HOST.into() }, path: "/b/k".into(), query: vec![("X-Amz-Signature".into(), "SIG".into())], url: "http://h/b/k?X-Amz-Security-Token=TOK".into(), expires_at: 1 };
    let all = format!("{req:?} {resp:?} {pre:?}");
    for bad in ["TOKEN-SECRET", "LONG-LIVED", "SAK", "SIG", "TOK"] {
        assert!(!all.contains(bad), "{bad} leaked: {all}");
    }
}

#[test]
fn presign_refuses_keys_that_would_be_policy_wildcards() {
    let r = rig(&["wasm-1"], &[]);
    pair(&r, 1, "wasm-1");
    for k in ["a*", "dir/?x", "${aws:username}"] {
        let e = run(r.broker.lease("wasm-1", LeaseRequest::PresignPut { key: k.into(), ttl_secs: 60 })).unwrap_err();
        assert_eq!(e.code, RefusalCode::BadRequest, "{k}");
    }
    assert!(run(r.broker.lease("wasm-1", LeaseRequest::PresignPut { key: "plain/k.bin".into(), ttl_secs: 60 })).is_ok());
}

// ---- known gaps handed to other lanes (ignored until fixed) ---------------------------
// Each test states the behaviour the ADR wants. Run with `--ignored` to see it fail today.

/// Owner: the lane that feeds chain events into `GrantLedger::apply` (S1 weftos-store /
/// chain integration). One event claiming `seq = u64::MAX` makes every later event
/// `Stale`, so a later revoke is silently dropped. Only chain-authenticated events may
/// reach `apply`, or `apply` must bound a jump in `seq`.
#[test]
#[ignore = "known gap: seq = u64::MAX freezes the ledger; owner S1/chain feed (ADR-126 S3 results)"]
fn a_far_future_seq_cannot_freeze_later_revocations() {
    let ledger = GrantLedger::new();
    ledger.apply(&GrantEvent::pairing(1, MESH, "wasm-1"));
    let _ = ledger.apply(&GrantEvent { seq: u64::MAX, principal: "wasm-1".into(), store: node_store_name(MESH, "wasm-1"), access: Access::Read, op: GrantOp::Grant });
    let later = GrantEvent { seq: 5, principal: "wasm-1".into(), store: node_store_name(MESH, "wasm-1"), access: Access::Write, op: GrantOp::RevokeAll };
    assert!(matches!(ledger.apply(&later), Applied::Changed { .. }), "a legitimate revoke must still apply");
}

/// Owner: S1 (weftos-store) archives the bucket on unpair (ADR-126 section 3). Today
/// `deprovision_node` leaves `n-<id>`, so a later node that reuses the id inherits the data.
#[test]
#[ignore = "known gap: node id reuse after unpair inherits the old bucket; owner S1 archive-on-unpair (ADR-126 S3 results)"]
fn a_reused_node_id_does_not_inherit_the_previous_nodes_objects() {
    let r = rig_unprovisioned(&["wasm-9"]);
    pair(&r, 1, "wasm-9");
    run(r.broker.minter().provision_node("wasm-9", "generated-secret-wasm-9")).unwrap();
    run(edge(&r, "wasm-9").put("secret.txt", b"old node data")).unwrap();
    run(r.broker.minter().deprovision_node("wasm-9")).unwrap();
    run(r.broker.minter().provision_node("wasm-9", "generated-secret-wasm-9")).unwrap();
    let fresh = edge(&r, "wasm-9");
    assert_eq!(run(fresh.get("secret.txt")).unwrap(), None, "new pairing must start from an empty bucket");
}

// ---- compaction spike ---------------------------------------------------------------

mod compaction_spike {
    use super::*;
    use crate::compaction::*;
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    fn mac(key: &[u8], msg: &[u8]) -> Vec<u8> {
        let mut m = Hmac::<Sha256>::new_from_slice(key).unwrap();
        m.update(msg);
        m.finalize().into_bytes().to_vec()
    }

    /// Stand-in for the chain signer: HMAC under a per-signer key.
    struct Keyring(Vec<(&'static str, &'static [u8])>);
    impl SnapshotVerifier for Keyring {
        fn verify(&self, signer: &str, message: &[u8], signature: &[u8]) -> bool {
            self.0.iter().any(|(id, k)| *id == signer && mac(k, message) == signature)
        }
    }
    const KEY: &[u8] = b"chain-signer-key";
    fn ring() -> Keyring { Keyring(vec![("chain-1", KEY)]) }

    fn history() -> Vec<GrantEvent> {
        let st = |n: &str| node_store_name(MESH, n);
        vec![
            GrantEvent::pairing(3, MESH, "a"),
            GrantEvent::pairing(5, MESH, "b"),
            GrantEvent { seq: 9, principal: "a".into(), store: st("b"), access: Access::Read, op: GrantOp::Grant },
            GrantEvent::unpair(12, MESH, "b"),
            GrantEvent::pairing(14, MESH, "c"),
        ]
    }

    fn snap_of(h: &[GrantEvent]) -> SignedSnapshot {
        GrantLedger::replay(h.iter().cloned()).signed_snapshot("grants/m1", "chain-1", |m| mac(KEY, m))
    }

    #[test]
    fn snapshot_plus_tail_equals_full_replay() {
        let h = history();
        let signed = snap_of(&h[..3]);
        let restored = GrantLedger::restore(&signed, "grants/m1", &ring(), &h[3..]).unwrap();
        let full = GrantLedger::replay(h.iter().cloned());
        assert_eq!(restored.snapshot("grants/m1"), full.snapshot("grants/m1"));
        // The revoke in the tail took effect; the node paired after it is held.
        use crate::broker::GrantCheck;
        assert!(!restored.allows("b", Access::Write));
        assert!(restored.allows("c", Access::Write) && restored.allows("a", Access::Write));
    }

    #[test]
    fn a_pre_snapshot_event_is_not_replayed_over_the_snapshot() {
        let h = history();
        let signed = snap_of(&h[..4]);
        // Handing the whole log in as the "tail" mixes two histories: refused.
        assert!(matches!(GrantLedger::restore(&signed, "grants/m1", &ring(), &h), Err(SnapshotError::TailBeforeSnapshot { .. })));
    }

    #[test]
    fn follower_refuses_a_tampered_unsigned_wrong_scope_or_edited_snapshot() {
        let h = history();
        let good = snap_of(&h[..3]);
        let ok = |s: &SignedSnapshot, scope: &str| GrantLedger::restore(s, scope, &ring(), &[]).err();
        assert_eq!(ok(&good, "grants/m1"), None);
        assert_eq!(ok(&good, "grants/other"), Some(SnapshotError::Scope { want: "grants/other".into(), got: "grants/m1".into() }));
        // Grant an extra right inside the state.
        let mut edited = good.clone();
        edited.snapshot.state[0].access.push(Access::Write);
        assert_eq!(ok(&edited, "grants/m1"), Some(SnapshotError::StateHash));
        // Edit the state and fix its hash: the signature no longer matches.
        let mut rehashed = edited.clone();
        let mut forged = rehashed.snapshot.clone();
        forged.state_hash = GrantLedger::replay(std::iter::empty()).snapshot("x").state_hash;
        forged.state = vec![];
        rehashed.snapshot = forged;
        assert_eq!(ok(&rehashed, "grants/m1"), Some(SnapshotError::BadSignature));
        // Signed by a key the follower does not trust.
        let mut stranger = good.clone();
        stranger.signer = "chain-9".into();
        assert_eq!(ok(&stranger, "grants/m1"), Some(SnapshotError::BadSignature));
        // Moving through_seq back (to replay old events over it) breaks the signature too.
        let mut rewound = good.clone();
        rewound.snapshot.through_seq = 1;
        assert_eq!(ok(&rewound, "grants/m1"), Some(SnapshotError::BadSignature));
    }

    #[test]
    fn a_follower_with_history_catches_a_validly_signed_but_false_snapshot() {
        let h = history();
        // A compromised signer signs a snapshot that omits a revoke.
        let lie = GrantLedger::replay(h[..2].iter().cloned()); // never saw the unpair at 12
        let mut forged = lie.snapshot("grants/m1");
        forged.through_seq = 12;
        let signed = SignedSnapshot { signature: mac(KEY, &signing_bytes(&forged)), snapshot: forged, signer: "chain-1".into() };
        // Signature and hash are fine, so the trusting follower accepts it...
        assert!(GrantLedger::restore(&signed, "grants/m1", &ring(), &[]).is_ok());
        // ...but one that kept the events does not.
        assert!(matches!(verify_against_history(&signed.snapshot, &h), Err(SnapshotError::HistoryMismatch(_))));
        // And the honest snapshot passes.
        assert_eq!(verify_against_history(&snap_of(&h[..4]).snapshot, &h), Ok(()));
    }

    #[test]
    fn ledger_head_is_order_and_content_sensitive() {
        let h = history();
        let a = GrantLedger::replay(h.iter().cloned()).snapshot("s").events_head;
        let mut h2 = h.clone();
        h2[2].access = Access::Write;
        let b = GrantLedger::replay(h2).snapshot("s").events_head;
        assert_ne!(a, b);
    }

    #[test]
    fn compaction_shrinks_the_replay_input() {
        let mut ev = Vec::new();
        for i in 0..500u64 {
            let n = format!("n{}", i % 10);
            ev.push(GrantEvent::pairing(2 * i + 1, MESH, &n));
            ev.push(GrantEvent::unpair(2 * i + 2, MESH, &n));
        }
        let snap = GrantLedger::replay(ev.iter().cloned()).snapshot("s");
        assert!(snap.state.is_empty(), "everything was revoked, so the snapshot holds nothing");
        assert_eq!(snap.through_seq, 1000);
    }

    #[test]
    fn same_state_reached_by_a_different_history_is_caught_by_the_head() {
        let st = node_store_name(MESH, "a");
        let ev = |seq, access| GrantEvent { seq, principal: "a".into(), store: st.clone(), access, op: GrantOp::Grant };
        let real = vec![ev(1, Access::Read), ev(2, Access::Write)];
        let other = vec![ev(1, Access::Write), ev(2, Access::Read)];
        let snap = GrantLedger::replay(other).snapshot("s");
        assert_eq!(GrantLedger::replay(real.clone()).snapshot("s").state, snap.state, "same final state");
        assert_eq!(verify_against_history(&snap, &real), Err(SnapshotError::HistoryMismatch("events_head")));
    }
}

// ---- S1's archive-on-unpair and seq guard: S3 consume side (mock backend) ----
mod lifecycle_consume {
    use super::*;
    use crate::grants::{MaxSeqJump, SeqGuard};
    use crate::lifecycle::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;

    /// Stands in for S1's archive-on-unpair until its real calls land.
    #[derive(Clone, Default)]
    struct MockArchiver {
        /// bucket -> object count
        buckets: Arc<Mutex<HashMap<String, u64>>>,
        down: Arc<AtomicBool>,
        log: Arc<Mutex<Vec<String>>>,
    }
    impl MockArchiver {
        fn fill(&self, node: &str, n: u64) {
            self.buckets.lock().unwrap().insert(format!("n-{node}"), n);
        }
    }
    impl NodeArchiver for MockArchiver {
        async fn archive_node(&self, node_id: &str) -> Result<ArchiveReceipt, ArchiveError> {
            if self.down.load(Ordering::SeqCst) {
                return Err(ArchiveError::Unavailable("archive store down".into()));
            }
            let n = self.buckets.lock().unwrap().insert(format!("n-{node_id}"), 0).unwrap_or(0);
            self.log.lock().unwrap().push(format!("archive:{node_id}"));
            Ok(ArchiveReceipt { node_id: node_id.into(), location: format!("archive/n-{node_id}"), objects: n })
        }
        async fn bucket_is_empty(&self, node_id: &str) -> Result<bool, ArchiveError> {
            if self.down.load(Ordering::SeqCst) {
                return Err(ArchiveError::Unavailable("down".into()));
            }
            Ok(self.buckets.lock().unwrap().get(&format!("n-{node_id}")).copied().unwrap_or(0) == 0)
        }
    }

    #[derive(Clone)]
    struct Prov {
        broker: Arc<Broker>,
        log: Arc<Mutex<Vec<String>>>,
    }
    impl NodeProvisioner for Prov {
        async fn deprovision(&self, node_id: &str) -> Result<(), crate::broker::StsError> {
            self.log.lock().unwrap().push(format!("deprovision:{node_id}"));
            self.broker.minter().deprovision_node(node_id).await
        }
    }

    fn setup(node: &str) -> (Rig, MockArchiver, NodeLifecycle<MockArchiver, Prov>) {
        let r = rig_unprovisioned(&[node]);
        pair(&r, 1, node);
        run(r.broker.minter().provision_node(node, &format!("generated-secret-{node}"))).unwrap();
        let a = MockArchiver::default();
        a.fill(node, 3);
        let p = Prov { broker: r.broker.clone(), log: a.log.clone() };
        let lc = NodeLifecycle::new(a.clone(), p);
        (r, a, lc)
    }

    fn removed_user(r: &Rig) -> bool {
        r.store.log().iter().any(|(_, _, _, p)| p.contains("remove-user"))
    }

    #[test]
    fn unpair_archives_before_it_deletes_the_user() {
        let (r, a, lc) = setup("wasm-9");
        let rc = run(lc.unpair(&r.ledger, &r.broker, &GrantEvent::unpair(2, MESH, "wasm-9"))).unwrap();
        assert_eq!((rc.objects, rc.node_id.as_str()), (3, "wasm-9"));
        assert_eq!(*a.log.lock().unwrap(), vec!["archive:wasm-9", "deprovision:wasm-9"], "archive first, delete second");
        assert!(removed_user(&r));
        assert!(run(edge(&r, "wasm-9").put("k", b"v")).is_err(), "the node is cut off");
        assert!(lc.is_sealed("wasm-9"));
    }

    #[test]
    fn a_failed_archive_keeps_the_user_and_the_data_and_a_retry_finishes() {
        let (r, a, lc) = setup("wasm-9");
        a.down.store(true, Ordering::SeqCst);
        let ev = GrantEvent::unpair(2, MESH, "wasm-9");
        let e = run(lc.unpair(&r.ledger, &r.broker, &ev)).unwrap_err();
        assert!(matches!(e, LifecycleError::Archive(ArchiveError::Unavailable(_))));
        assert!(!removed_user(&r), "nothing is deleted before the archive exists");
        assert!(lc.is_sealed("wasm-9"));
        assert!(run(edge(&r, "wasm-9").put("k", b"v")).is_err(), "disabled in the meantime");

        a.down.store(false, Ordering::SeqCst);
        // The same event again is Stale in the ledger; the lifecycle still completes.
        run(lc.unpair(&r.ledger, &r.broker, &ev)).unwrap();
        assert!(removed_user(&r));
        assert_eq!(*a.log.lock().unwrap(), vec!["archive:wasm-9", "deprovision:wasm-9"]);
    }

    #[test]
    fn a_reused_id_is_refused_until_the_bucket_is_empty() {
        let (r, a, lc) = setup("wasm-9");
        // Never-unpaired ids pair freely, even with data in their bucket.
        assert!(run(lc.check_pairable("wasm-9")).is_ok());
        a.down.store(true, Ordering::SeqCst);
        let _ = run(lc.unpair(&r.ledger, &r.broker, &GrantEvent::unpair(2, MESH, "wasm-9")));
        a.down.store(false, Ordering::SeqCst);
        // Archive never happened: the bucket still holds the old node's objects.
        assert_eq!(run(lc.check_pairable("wasm-9")), Err(LifecycleError::IdRetired("wasm-9".into())));
        // A broken probe fails closed.
        a.down.store(true, Ordering::SeqCst);
        assert_eq!(run(lc.check_pairable("wasm-9")), Err(LifecycleError::IdRetired("wasm-9".into())));
        a.down.store(false, Ordering::SeqCst);

        run(lc.unpair(&r.ledger, &r.broker, &GrantEvent::unpair(2, MESH, "wasm-9"))).unwrap();
        assert!(run(lc.check_pairable("wasm-9")).is_ok(), "archived and emptied: the id may be used again");
        assert!(!lc.is_sealed("wasm-9"));
        assert!(run(lc.check_pairable("wasm-9")).is_ok());
    }

    #[test]
    fn after_a_restart_the_chain_history_reseals_unpaired_ids() {
        let (r, a, _lc) = setup("wasm-9");
        let history = [GrantEvent::pairing(1, MESH, "wasm-9"), GrantEvent::unpair(2, MESH, "wasm-9"), GrantEvent::pairing(3, MESH, "wasm-2")];
        let fresh = NodeLifecycle::new(a.clone(), Prov { broker: r.broker.clone(), log: a.log.clone() });
        assert!(run(fresh.check_pairable("wasm-9")).is_ok(), "nothing known yet");
        fresh.seal_from_history(history.iter());
        assert!(fresh.is_sealed("wasm-9") && !fresh.is_sealed("wasm-2"));
        assert!(run(fresh.check_pairable("wasm-9")).is_err(), "bucket still has 3 objects");
    }

    #[test]
    fn only_an_unpair_event_starts_the_unpair_flow() {
        let (r, a, lc) = setup("wasm-9");
        let revoke_one = GrantEvent { seq: 2, principal: "wasm-9".into(), store: node_store_name(MESH, "wasm-9"), access: Access::Write, op: GrantOp::Revoke };
        assert_eq!(run(lc.unpair(&r.ledger, &r.broker, &revoke_one)).unwrap_err(), LifecycleError::NotUnpair);
        // A RevokeAll on someone else's store is not this node's unpair either.
        let other = GrantEvent { seq: 3, principal: "wasm-9".into(), store: node_store_name(MESH, "other"), access: Access::Write, op: GrantOp::RevokeAll };
        assert_eq!(run(lc.unpair(&r.ledger, &r.broker, &other)).unwrap_err(), LifecycleError::NotUnpair);
        assert!(!lc.is_sealed("wasm-9") && a.log.lock().unwrap().is_empty());
    }

    #[test]
    fn a_seq_guard_stops_the_u64_max_freeze_and_a_real_revoke_still_applies() {
        let ledger = GrantLedger::new();
        ledger.set_seq_guard(MaxSeqJump(1_000_000));
        ledger.apply(&GrantEvent::pairing(1, MESH, "wasm-1"));
        let before = ledger.snapshot("s");
        let bad = GrantEvent { seq: u64::MAX, principal: "wasm-1".into(), store: node_store_name(MESH, "wasm-1"), access: Access::Read, op: GrantOp::Grant };
        assert_eq!(ledger.apply(&bad), Applied::OutOfRange);
        assert_eq!(ledger.snapshot("s"), before, "a refused event changes nothing, not even the head");
        let later = GrantEvent { seq: 5, principal: "wasm-1".into(), store: node_store_name(MESH, "wasm-1"), access: Access::Write, op: GrantOp::RevokeAll };
        assert!(matches!(ledger.apply(&later), Applied::Changed { lost_write: true }));
    }

    #[test]
    fn the_seq_guard_is_pluggable_and_must_be_reinstalled_after_restore() {
        struct Never;
        impl SeqGuard for Never {
            fn admit(&self, _: u64, _: u64) -> bool {
                false
            }
        }
        let l = GrantLedger::new();
        l.set_seq_guard(Never);
        assert_eq!(l.apply(&GrantEvent::pairing(1, MESH, "a")), Applied::OutOfRange);
        // A guard is part of the running ledger, not of a snapshot: a restored
        // ledger has none until the host installs it again.
        let live = GrantLedger::replay([GrantEvent::pairing(1, MESH, "a")]);
        let signed = live.signed_snapshot("s", "k", |_| vec![1]);
        struct Ok1;
        impl crate::compaction::SnapshotVerifier for Ok1 {
            fn verify(&self, _: &str, _: &[u8], _: &[u8]) -> bool {
                true
            }
        }
        let restored = GrantLedger::restore(&signed, "s", &Ok1, &[]).unwrap();
        let far = GrantEvent { seq: u64::MAX, ..GrantEvent::pairing(0, MESH, "b") };
        assert!(matches!(restored.apply(&far), Applied::Changed { .. }), "no guard until installed");
        let again = GrantLedger::restore(&signed, "s", &Ok1, &[]).unwrap();
        again.set_seq_guard(MaxSeqJump(10));
        assert_eq!(again.apply(&far), Applied::OutOfRange);
    }

    #[test]
    fn an_unpair_refused_by_the_seq_guard_does_not_archive() {
        let (r, a, lc) = setup("wasm-9");
        r.ledger.set_seq_guard(MaxSeqJump(5));
        let e = run(lc.unpair(&r.ledger, &r.broker, &GrantEvent::unpair(500, MESH, "wasm-9"))).unwrap_err();
        assert!(matches!(e, LifecycleError::Revoke(_)));
        assert!(a.log.lock().unwrap().is_empty() && !removed_user(&r));
    }
}

mod chain_auth_enforce {
    use super::*;
    use crate::authchain::*;
    use crate::compaction::SnapshotVerifier;
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    const SC: &str = "grants/m1";
    fn mac(msg: &[u8]) -> Vec<u8> {
        let mut m = Hmac::<Sha256>::new_from_slice(b"k").unwrap();
        m.update(msg);
        m.finalize().into_bytes().to_vec()
    }
    struct V;
    impl SnapshotVerifier for V {
        fn verify(&self, s: &str, m: &[u8], sig: &[u8]) -> bool {
            s == "c" && mac(m) == sig
        }
    }

    #[test]
    fn an_authenticated_unpair_cuts_the_node_off_and_a_forged_one_does_not() {
        let r = rig(&["wasm-1"], &[]);
        let feed = ChainFeed::new(r.ledger.clone(), SC, V);
        let head = feed.head();
        let pairing = ChainGrantEvent::sign(SC, GrantEvent::pairing(1, MESH, "wasm-1"), &head, "c", mac);
        run(ingest_and_enforce(&feed, &r.broker, &pairing)).unwrap();
        let node = edge(&r, "wasm-1");
        run(node.put("k", b"v")).unwrap();

        let mut forged = ChainGrantEvent::sign(SC, GrantEvent::unpair(2, MESH, "wasm-1"), &feed.head(), "c", mac);
        forged.signature[0] ^= 1;
        assert_eq!(run(ingest_and_enforce(&feed, &r.broker, &forged)), Err(IngestError::Feed(FeedError::BadSignature)));
        run(node.put("k2", b"v")).unwrap();

        let real = ChainGrantEvent::sign(SC, GrantEvent::unpair(2, MESH, "wasm-1"), &feed.head(), "c", mac);
        assert_eq!(run(ingest_and_enforce(&feed, &r.broker, &real)).unwrap(), Applied::Changed { lost_write: true });
        assert!(run(node.put("k3", b"v")).is_err(), "revoked at once");
    }
}
