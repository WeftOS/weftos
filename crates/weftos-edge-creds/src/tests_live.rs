//! The live path end to end: EdgeNodeStore -> link -> EdgeBroker ->
//! RustfsMinter (signed AssumeRole / admin calls) -> a store that verifies
//! signatures, session tokens and policy intersection. Grants come from
//! chain events through GrantLedger.

use std::collections::HashMap;
use std::sync::Arc;

use crate::broker::{BrokerConfig, EdgeBroker};
use crate::clock::ManualClock;
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
