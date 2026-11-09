//! ADR-126 S1 x S3: S3's chain-authenticated grant feed (`authchain::ChainFeed`) driven by REAL
//! S1 grant events (`StoreService` grant/revoke, `seq` stamped by the service) over a loopback
//! RustFS, with a forged, a replayed and a reordered event.
//!
//! The chain owner's signer does not exist yet. Here the "chain" is a stand-in that signs each
//! event with a real ed25519 key (verified with `verify_strict` through S3's `SnapshotVerifier`
//! interface). Every live test is `#[ignore]`: run it with `scripts/store/integ-live-test.sh`,
//! which starts a THROWAWAY RustFS on a random loopback port. Never point it at PG.

use std::sync::{Arc, Mutex};

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde_json::Value;
use weftos_edge_creds::authchain::{ChainFeed, ChainGrantEvent, FeedError};
use weftos_edge_creds::compaction::SnapshotVerifier;
use weftos_edge_creds::grants::{node_store_name, Applied, GrantEvent, GrantLedger, GrantOp};
use weftos_edge_creds::policy as s3pol;
use weftos_store::audit::MemSink;
use weftos_store::service::{ChainSink, Config, OpenGate, Parties, StoreService};
use weftos_store::{Access, AdminBackend, BackendError, ObjectBackend, Principal, RustfsBackend, RustfsConfig, StoreName};

const MESH: &str = "weft";
const SCOPE: &str = "grants/weft";
const SIGNER: &str = "chain-1";

#[derive(Default)]
struct Chain(Mutex<Vec<(String, Value)>>);
impl ChainSink for Chain {
    fn record(&self, k: &str, v: Value) {
        self.0.lock().unwrap().push((k.to_owned(), v));
    }
}

struct Ed(VerifyingKey);
impl SnapshotVerifier for Ed {
    fn verify(&self, signer: &str, message: &[u8], signature: &[u8]) -> bool {
        let Ok(sig) = Signature::from_slice(signature) else { return false };
        signer == SIGNER && !self.0.is_weak() && self.0.verify_strict(message, &sig).is_ok()
    }
}

struct Live {
    cfg: RustfsConfig,
    be: Arc<RustfsBackend>,
    svc: Arc<StoreService>,
    chain: Arc<Chain>,
    _secrets: tempfile::TempDir,
}

fn uniq(p: &str) -> String {
    format!("{p}-{}", &ulid::Ulid::new().to_string().to_lowercase()[10..])
}

fn live() -> Live {
    let f = std::env::var("WEFTOS_STORE_TEST_RUSTFS_ENV").expect("WEFTOS_STORE_TEST_RUSTFS_ENV (run scripts/store/integ-live-test.sh)");
    let d = tempfile::tempdir().unwrap();
    let cfg = RustfsConfig::from_env_file(std::path::Path::new(&f), d.path().join("secrets")).unwrap();
    let be = Arc::new(RustfsBackend::new(cfg.clone()).unwrap());
    let chain = Arc::new(Chain::default());
    let svc = Arc::new(
        StoreService::open(
            Config { node_id: uniq("n"), state_path: None, clock: || 1_000 },
            be.clone(),
            be.clone(),
            Arc::new(OpenGate),
            Arc::new(MemSink::default()),
            chain.clone(),
        )
        .unwrap(),
    );
    Live { cfg, be, svc, chain, _secrets: d }
}

fn as_key(l: &Live, principal: &str) -> RustfsBackend {
    let id = RustfsBackend::key_id_for(principal);
    let secret = std::fs::read_to_string(l.be.secret_path(&id)).unwrap();
    RustfsBackend::new(RustfsConfig { root_access_key: id, root_secret_key: secret, ..l.cfg.clone() }).unwrap()
}

fn is_forbidden<T: std::fmt::Debug>(r: Result<T, BackendError>) -> bool {
    matches!(r, Err(BackendError::Forbidden(_)))
}

/// S1 chain records to S3 grant events. `store.create` of a node store is unstamped (a recorded
/// S1 gap), so the glue gives it seq 1 and shifts S1's stamps up by one, as the S1 x S3 ledger
/// test does. Every stamped record must be gapless.
fn to_grant_events(chain: &[(String, Value)], last_stamp: &mut u64) -> Vec<GrantEvent> {
    let mut out = Vec::new();
    for (kind, v) in chain {
        if let Some(stamp) = v["seq"].as_u64() {
            assert_eq!(stamp, *last_stamp + 1, "S1's grant-event seq must be gapless ({kind})");
            *last_stamp = stamp;
        }
        let seq = v["seq"].as_u64().map_or(1, |stamp| stamp + 1);
        let store = v["store"].as_str().unwrap_or_default();
        let node_of = |p: &str| p.strip_prefix("node:").map(str::to_owned);
        let ev = match kind.as_str() {
            "store.create" if store.contains("/nodes/") => {
                let node = store.split('/').nth(4).unwrap().to_owned();
                GrantEvent { seq, principal: node, store: store.into(), access: s3pol::Access::Write, op: GrantOp::Grant }
            }
            "store.grant" => {
                let Some(principal) = node_of(v["principal"].as_str().unwrap()) else { continue };
                let access = match v["access"].as_str().unwrap() {
                    "read" => s3pol::Access::Read,
                    "write" | "admin" => s3pol::Access::Write,
                    other => panic!("S1 access {other:?} has no S3 equivalent"),
                };
                GrantEvent { seq, principal, store: store.into(), access, op: GrantOp::Grant }
            }
            "store.revoke" => {
                let Some(principal) = node_of(v["principal"].as_str().unwrap()) else { continue };
                GrantEvent { seq, principal, store: store.into(), access: s3pol::Access::Write, op: GrantOp::RevokeAll }
            }
            _ => continue,
        };
        out.push(ev);
    }
    out
}

/// The stand-in chain: signs events in order, tracking the ledger head the way the real chain's
/// own fold would (a shadow ledger that applies every event it signs).
struct StandInChain {
    key: SigningKey,
    shadow: GrantLedger,
}
impl StandInChain {
    fn new(key: SigningKey) -> Self {
        Self { key, shadow: GrantLedger::new() }
    }
    fn next(&self, ev: &GrantEvent) -> ChainGrantEvent {
        let head = self.shadow.snapshot(SCOPE).events_head;
        let signed = ChainGrantEvent::sign(SCOPE, ev.clone(), &head, SIGNER, |m| self.key.sign(m).to_bytes().to_vec());
        self.shadow.apply(ev);
        signed
    }
}

#[test]
#[ignore = "needs a throwaway RustFS: run scripts/store/integ-live-test.sh (WEFTOS_STORE_TEST_RUSTFS_ENV)"]
fn live_s1_events_through_the_chain_authenticated_feed_agree_with_rustfs() {
    let l = live();
    let node_id = uniq("n");
    let node = StoreName::node(MESH, &node_id).unwrap();
    let node_p = Principal::parse(&format!("node:{node_id}")).unwrap();
    l.svc.ensure_store(&node, "daemon", Parties::defaults_for(&node)).unwrap();
    let bucket = node.bucket();
    let store_uri = node_store_name(MESH, &node_id);
    assert_eq!(store_uri, node.to_string());
    let key = as_key(&l, &node_p.to_string());

    let sk = SigningKey::from_bytes(&[7u8; 32]);
    let chain_signer = StandInChain::new(sk.clone());
    let feed = ChainFeed::new(Arc::new(GrantLedger::new()), SCOPE, Ed(sk.verifying_key()));

    let mut fed = 0usize;
    let mut last_stamp = 0u64;
    let mut next_signed = || -> Vec<ChainGrantEvent> {
        let chain = l.chain.0.lock().unwrap().clone();
        let evs = to_grant_events(&chain[fed..], &mut last_stamp);
        fed = chain.len();
        evs.iter().map(|e| chain_signer.next(e)).collect()
    };
    let writes = |k: &str| key.put(&bucket, k, b"v", false);
    let holds = |a: s3pol::Access| feed.ledger().holds(&node_id, &store_uri, a);

    // 1. Created: S1's create reaches the ledger through the feed; the node writes at RustFS.
    let created = next_signed();
    assert_eq!(created.len(), 1);
    assert!(feed.ingest(&created[0]).is_ok());
    assert!(holds(s3pol::Access::Write));
    writes("a").expect("RustFS: node key writes");

    // 2. Revoke by S1. FORGED copies are refused and the ledger still says the node may write.
    l.svc.revoke(&node, &node_p, "owner").unwrap();
    let revoke = next_signed().remove(0);
    let mut forged = revoke.clone();
    forged.event.op = GrantOp::Grant; // relabel the revoke as a grant
    assert_eq!(feed.ingest(&forged), Err(FeedError::BadSignature));
    let mut other_key = revoke.clone();
    other_key.signature = SigningKey::from_bytes(&[9u8; 32]).sign(b"x").to_bytes().to_vec();
    assert_eq!(feed.ingest(&other_key), Err(FeedError::BadSignature), "a signature from another key");
    assert!(holds(s3pol::Access::Write), "refused events change nothing");

    // The genuine revoke applies; the ledger and RustFS agree the node is out.
    assert_eq!(feed.ingest(&revoke), Ok(Applied::Changed { lost_write: true }));
    assert!(!holds(s3pol::Access::Write));
    assert!(is_forbidden(writes("b")), "RustFS: node key refused after revoke");

    // 3. REPLAYED: the same signed revoke again, and the original create, are dropped as already
    //    applied; a replayed create cannot bring the grant back.
    assert_eq!(feed.ingest(&revoke), Err(FeedError::Replay(revoke.event.seq)));
    assert_eq!(feed.ingest(&created[0]), Err(FeedError::Replay(created[0].event.seq)));
    assert!(!holds(s3pol::Access::Write), "a replay does not resurrect the grant");

    // 4. REORDERED: S1 grants read then write. Delivered write-first, the link breaks and nothing
    //    applies; delivered in order, both apply.
    l.svc.grant(&node, node_p.clone(), Access::Read, "owner").unwrap();
    l.svc.grant(&node, node_p.clone(), Access::Write, "owner").unwrap();
    let pair = next_signed();
    assert_eq!(pair.len(), 2);
    assert_eq!(feed.ingest(&pair[1]), Err(FeedError::BrokenLink(pair[1].event.seq)));
    assert!(!holds(s3pol::Access::Read), "the out-of-order event applied nothing");
    assert!(feed.ingest(&pair[0]).is_ok());
    assert!(holds(s3pol::Access::Read) && !holds(s3pol::Access::Write));
    assert!(feed.ingest(&pair[1]).is_ok());
    assert!(holds(s3pol::Access::Write));
    writes("d").expect("RustFS: node key writes again");

    // 5. Another mesh's validly signed event is refused by scope.
    let other_mesh = ChainGrantEvent::sign("grants/other", GrantEvent { seq: 99, ..pair[1].event.clone() }, &feed.head(), SIGNER, |m| sk.sign(m).to_bytes().to_vec());
    assert!(matches!(feed.ingest(&other_mesh), Err(FeedError::Scope { .. })));

    for k in ["a", "d"] {
        let _ = l.be.delete(&bucket, k);
    }
    let _ = l.be.delete_bucket(&bucket);
}
