//! ADR-126 cross-lane integration tests over a loopback RustFS: S1 (`weftos-store`) x S4
//! (`weftos-release`), and S1 x S3 (`weftos-edge-creds`).
//!
//! Every live test is `#[ignore]` and is run by `scripts/store/integ-live-test.sh`, which starts a
//! THROWAWAY RustFS on a random loopback port and names its env file in
//! `WEFTOS_STORE_TEST_RUSTFS_ENV`. Never point them at PG or any shared RustFS.
//!
//! There is no S4-on-S1 adapter in either crate (S4's docs leave the S3 client to S1, S1 does not
//! depend on S4), so [`SvcBackend`] below is test glue: a `ReleaseBackend` that goes through
//! `StoreService` as one principal, which is exactly the path the daemon would take.

use std::sync::{Arc, Mutex};

use serde_json::Value;
use weftos_edge_creds::grants::{Applied, GrantEvent, GrantLedger, GrantOp, node_store_name};
use weftos_edge_creds::policy as s3pol;
use weftos_pq_sign::MlDsa65SigningKey;
use weftos_release as rel;
use weftos_release::{BackendError as RBackendError, DeletableBackend, PutOutcome, ReleaseBackend};
use weftos_store::audit::MemSink;
use weftos_store::service::{Caller, ChainSink, Config, OpenGate, Parties, StoreService};
use weftos_store::{Access, AdminBackend, BackendError, ObjectBackend, Principal, RustfsBackend, RustfsConfig, StoreError, StoreName};

/// Minimal single-future executor: the adapter's futures never return `Pending`.
fn block_on<F: std::future::Future>(f: F) -> F::Output {
    use std::task::{Context, Poll, Wake, Waker};
    struct Noop;
    impl Wake for Noop {
        fn wake(self: Arc<Self>) {}
    }
    let waker = Waker::from(Arc::new(Noop));
    let mut cx = Context::from_waker(&waker);
    let mut f = std::pin::pin!(f);
    loop {
        if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
    }
}

// ---- harness ----------------------------------------------------------------------------------

#[derive(Default)]
struct Chain(Mutex<Vec<(String, Value)>>);
impl ChainSink for Chain {
    fn record(&self, k: &str, v: Value) {
        self.0.lock().unwrap().push((k.to_owned(), v));
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

/// The principal's own S3 key, straight at RustFS (bypassing the service).
fn as_key(l: &Live, principal: &str) -> RustfsBackend {
    let id = RustfsBackend::key_id_for(principal);
    let secret = std::fs::read_to_string(l.be.secret_path(&id)).unwrap();
    RustfsBackend::new(RustfsConfig { root_access_key: id, root_secret_key: secret, ..l.cfg.clone() }).unwrap()
}

fn is_forbidden<T: std::fmt::Debug>(r: Result<T, BackendError>) -> bool {
    matches!(r, Err(BackendError::Forbidden(_)))
}

/// S4's backend trait over S1's service, as one principal. The store calls are blocking HTTP, so
/// the futures complete without ever pending, so [`block_on`] drives them with no runtime (a blocking
/// HTTP client must not be created or dropped inside a tokio runtime).
struct SvcBackend {
    svc: Arc<StoreService>,
    who: Caller,
    name: StoreName,
}

impl SvcBackend {
    fn new(svc: &Arc<StoreService>, principal: &str, name: &StoreName) -> Self {
        Self { svc: svc.clone(), who: Caller::local(Principal::parse(principal).unwrap()), name: name.clone() }
    }
}

fn rerr(e: StoreError) -> RBackendError {
    RBackendError(e.to_string())
}

impl ReleaseBackend for SvcBackend {
    async fn put_if_absent(&self, key: &str, bytes: &[u8]) -> Result<PutOutcome, RBackendError> {
        match self.svc.put(&self.who, &self.name, key, bytes, true) {
            Ok(_) => Ok(PutOutcome::Created),
            Err(StoreError::Backend(BackendError::Exists(_))) => Ok(PutOutcome::Exists),
            Err(e) => Err(rerr(e)),
        }
    }
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, RBackendError> {
        match self.svc.get(&self.who, &self.name, key) {
            Ok(b) => Ok(Some(b)),
            Err(StoreError::Backend(BackendError::NoObject(_))) => Ok(None),
            Err(e) => Err(rerr(e)),
        }
    }
    async fn list(&self, prefix: &str) -> Result<Vec<String>, RBackendError> {
        self.svc.list(&self.who, &self.name, prefix).map(|v| v.into_iter().map(|m| m.key).collect()).map_err(rerr)
    }
}

impl DeletableBackend for SvcBackend {
    async fn delete(&self, key: &str) -> Result<(), RBackendError> {
        match self.svc.delete(&self.who, &self.name, key) {
            Ok(()) | Err(StoreError::Backend(BackendError::NoObject(_))) => Ok(()),
            Err(e) => Err(rerr(e)),
        }
    }
}

fn signer(seed: u8) -> rel::ReleaseSigner {
    rel::ReleaseSigner::new(ed25519_dalek::SigningKey::from_bytes(&[seed; 32]), MlDsa65SigningKey::from_seed(&[seed; 32]))
}

fn req(build: &str, created_at: u64, files: &[(&str, &[u8])]) -> rel::PublishRequest {
    rel::PublishRequest {
        build_id: build.into(),
        source_rev: "527ea4db".into(),
        created_at,
        builder: "ci-runner-1".into(),
        artifacts: files.iter().map(|(p, b)| rel::Artifact { path: (*p).into(), bytes: b.to_vec() }).collect(),
    }
}

fn release_store(l: &Live, ci: &str, sweeper: &str) -> (StoreName, rel::ReleaseStoreName) {
    let project = ulid::Ulid::new().to_string();
    let name = StoreName::release("weft", &project, "stable").unwrap();
    let parties = Parties { appenders: vec![Principal::parse(ci).unwrap()], sweepers: vec![Principal::parse(sweeper).unwrap()], ..Default::default() };
    l.svc.create_store(&name, "daemon", parties).unwrap();
    // The two lanes must name the same store: S1's name string is S4's uri.
    let rn = rel::ReleaseStoreName::new("weft", &project, "stable").unwrap();
    assert_eq!(rn.uri(), name.to_string(), "S4's store uri and S1's store name must be one string");
    (name, rn)
}

fn principals() -> (String, String) {
    let sfx = &ulid::Ulid::new().to_string().to_lowercase()[16..];
    (format!("agent:ci-{sfx}"), format!("agent:store-sweeper-{sfx}"))
}

// ---- S1 x S4: the CI publish path -------------------------------------------------------------

/// Joins S1 (`StoreService`, `Access::Append`, `RustfsBackend`) and S4 (`ReleasePublisher`,
/// `ReleaseReader`). CI publishes a signed release into a release store through the service as an
/// `Append` principal. Proves: the release reads back and audits clean; a retry is idempotent; a
/// rebuild with other bytes is `AlreadyPublished`; a second `put_if_absent` of an existing key is
/// `Exists` and leaves the bytes; and the CI principal's own S3 key at RustFS can neither overwrite,
/// create, copy nor delete.
#[test]
#[ignore = "needs a throwaway RustFS: run scripts/store/integ-live-test.sh (WEFTOS_STORE_TEST_RUSTFS_ENV)"]
fn live_s1_s4_ci_publishes_write_once_through_the_service() {
    block_on(live_s1_s4_ci_publishes_write_once_through_the_service_body());
}

async fn live_s1_s4_ci_publishes_write_once_through_the_service_body() {
    let l = live();
    let (ci, sw) = principals();
    let (name, rn) = release_store(&l, &ci, &sw);
    let sg = signer(7);
    let tk = rel::TrustedKeys(vec![sg.trusted()]);
    let ci_be = SvcBackend::new(&l.svc, &ci, &name);

    let p = rel::ReleasePublisher::new(&ci_be, &rn, &sg);
    let r = p.publish(req("b1", 1_000, &[("bin/weaver", b"ELF-weaver"), ("SHA256SUMS", b"sums")]), 1_000).await.unwrap();
    assert_eq!(r.outcome, rel::PublishOutcome::Published);
    let again = p.publish(req("b1", 1_000, &[("bin/weaver", b"ELF-weaver"), ("SHA256SUMS", b"sums")]), 1_000).await.unwrap();
    assert_eq!(again.outcome, rel::PublishOutcome::AlreadyPresent, "a retried job is idempotent");
    let clash = p.publish(req("b1", 1_000, &[("bin/weaver", b"ELF-EVIL")]), 1_000).await;
    assert!(matches!(clash, Err(rel::ReleaseError::AlreadyPublished { .. })), "{clash:?}");

    let rd = rel::ReleaseReader::new(&ci_be, &rn, &tk);
    assert_eq!(rd.build_ids().await.unwrap(), vec!["b1"]);
    let m = rd.manifest("b1").await.unwrap();
    assert_eq!(rd.read_artifact(&m, "bin/weaver").await.unwrap(), b"ELF-weaver");
    assert!(rd.audit(&m).await.unwrap().is_empty(), "every referenced blob is present");

    // Write-once at the trait: an existing key is Exists and unchanged.
    let hash = blake3::hash(b"ELF-weaver").to_hex().to_string();
    let blob = format!("blobs/{hash}");
    assert_eq!(ci_be.put_if_absent(&blob, b"other").await.unwrap(), PutOutcome::Exists);
    // Write-once at the service: an unconditional put by an Append principal is refused.
    assert!(matches!(l.svc.put(&ci_be.who, &name, &blob, b"other", false), Err(StoreError::Refused(_))));
    assert_eq!(l.be.get(&name.bucket(), &blob).unwrap(), b"ELF-weaver", "bytes at RustFS are the published ones");

    // The CI principal's own key, straight at RustFS: read yes, everything that writes no.
    let key = as_key(&l, &ci);
    let b = name.bucket();
    assert_eq!(key.get(&b, &blob).unwrap(), b"ELF-weaver");
    assert!(is_forbidden(key.put(&b, &blob, b"evil", false)), "no overwrite at RustFS");
    assert!(is_forbidden(key.put(&b, "blobs/new", b"x", true)), "no create at RustFS either (the daemon puts for CI)");
    assert!(is_forbidden(key.copy(&b, &blob, &b, "blobs/copy")), "no copy");
    assert!(is_forbidden(key.delete(&b, &blob)), "no delete");
    assert_eq!(l.be.get(&b, &blob).unwrap(), b"ELF-weaver");

    for m in l.be.list(&b, "").unwrap() {
        l.be.delete(&b, &m.key).unwrap();
    }
    let _ = l.be.delete_bucket(&b);
}

/// S4's backend trait over the sweeper principal's OWN RustFS key, straight at RustFS (no service
/// in the way). This is the path S1's owner switch `WEFTOS_STORE_SWEEPER_PUT` widens.
struct DirectBackend {
    be: RustfsBackend,
    bucket: String,
}

impl ReleaseBackend for DirectBackend {
    async fn put_if_absent(&self, key: &str, bytes: &[u8]) -> Result<PutOutcome, RBackendError> {
        match self.be.put(&self.bucket, key, bytes, true) {
            Ok(_) => Ok(PutOutcome::Created),
            Err(BackendError::Exists(_)) => Ok(PutOutcome::Exists),
            Err(e) => Err(RBackendError(e.to_string())),
        }
    }
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, RBackendError> {
        match self.be.get(&self.bucket, key) {
            Ok(b) => Ok(Some(b)),
            Err(BackendError::NoObject(_)) => Ok(None),
            Err(e) => Err(RBackendError(e.to_string())),
        }
    }
    async fn list(&self, prefix: &str) -> Result<Vec<String>, RBackendError> {
        self.be.list(&self.bucket, prefix).map(|v| v.into_iter().map(|m| m.key).collect()).map_err(|e| RBackendError(e.to_string()))
    }
}

impl DeletableBackend for DirectBackend {
    async fn delete(&self, key: &str) -> Result<(), RBackendError> {
        match self.be.delete(&self.bucket, key) {
            Ok(()) | Err(BackendError::NoObject(_)) => Ok(()),
            Err(e) => Err(RBackendError(e.to_string())),
        }
    }
}

fn clean_up(l: &Live, name: &StoreName) {
    let b = name.bucket();
    for m in l.be.list(&b, "").unwrap() {
        l.be.delete(&b, &m.key).unwrap();
    }
    let _ = l.be.delete_bucket(&b);
}

/// Joins S1 (`Access::Sweep`, the service's role table) and S4 (`Sweeper`). S4's retention sweep
/// puts a tombstone first and restores a blob after a racing publish, so it needs PutObject on
/// `tombstones/*` and `blobs/*`; S1's `Sweep` role is "read plus delete, no put". Through the
/// service the sweeper is refused, and S1's owner switch (`WEFTOS_STORE_SWEEPER_PUT`) does NOT
/// change that: it widens only the sweeper's own S3 key. The owner question stays open; this test
/// records the current answer both with the switch off and on.
#[test]
#[ignore = "needs a throwaway RustFS: run scripts/store/integ-live-test.sh (WEFTOS_STORE_TEST_RUSTFS_ENV)"]
fn live_s1_s4_sweeper_through_the_service_is_refused_whatever_the_owner_switch_says() {
    block_on(async {
        for switch_on in [false, true] {
            let l = live();
            let (ci, sw) = principals();
            let (name, rn) = release_store(&l, &ci, &sw);
            if switch_on {
                l.svc.set_policy_opts(weftos_store::policy::PolicyOpts { sweeper_put: true, ..Default::default() });
                assert!(l.svc.reconcile().is_empty());
            }
            let sg = signer(9);
            let tk = rel::TrustedKeys(vec![sg.trusted()]);
            let ci_be = SvcBackend::new(&l.svc, &ci, &name);
            let p = rel::ReleasePublisher::new(&ci_be, &rn, &sg);
            p.publish(req("b1", 1_000, &[("a", b"one")]), 1_000).await.unwrap();
            p.publish(req("b2", 2_000, &[("a", b"two")]), 2_000).await.unwrap();
            let sw_be = SvcBackend::new(&l.svc, &sw, &name);
            let r = rel::Sweeper::new(&sw_be, &rn, &tk).sweep(&rel::RetentionPolicy::new(1, 0).unwrap(), 3_000, 0, "integ").await;
            let e = r.expect_err("the service refuses a sweeper put, switch or no switch").to_string();
            assert!(e.contains("needs append"), "switch_on={switch_on}: {e}");
            clean_up(&l, &name);
        }
    });
}

/// The same sweep with the sweeper holding and using its own RustFS key. RustFS refuses the
/// tombstone put while the owner switch is off; flipping `sweeper_put` (the env var
/// `WEFTOS_STORE_SWEEPER_PUT=1` in a daemon) at run time lets the sweep complete. This is the
/// one-line flip, and the cost is S1's: that key can then overwrite or copy over any artifact in
/// the store (only a locked store's retention protects the original).
#[test]
#[ignore = "needs a throwaway RustFS: run scripts/store/integ-live-test.sh (WEFTOS_STORE_TEST_RUSTFS_ENV)"]
fn live_s1_s4_sweeper_with_its_own_key_is_refused_until_the_owner_switch_is_on() {
    block_on(async {
        let l = live();
        let (ci, sw) = principals();
        let (name, rn) = release_store(&l, &ci, &sw);
        let sg = signer(9);
        let tk = rel::TrustedKeys(vec![sg.trusted()]);
        let ci_be = SvcBackend::new(&l.svc, &ci, &name);
        let p = rel::ReleasePublisher::new(&ci_be, &rn, &sg);
        p.publish(req("b1", 1_000, &[("a", b"one")]), 1_000).await.unwrap();
        p.publish(req("b2", 2_000, &[("a", b"two")]), 2_000).await.unwrap();
        let direct = DirectBackend { be: as_key(&l, &sw), bucket: name.bucket() };
        let policy = rel::RetentionPolicy::new(1, 0).unwrap();
        let rd = rel::ReleaseReader::new(&ci_be, &rn, &tk);

        // Switch off (the default): RustFS refuses the sweeper's put, and nothing is expired.
        let off = rel::Sweeper::new(&direct, &rn, &tk).sweep(&policy, 3_000, 0, "integ").await;
        let e = off.expect_err("RustFS refuses the sweeper's put while the switch is off").to_string();
        assert!(e.to_lowercase().contains("forbidden"), "{e}");
        assert_eq!(rd.build_ids().await.unwrap(), vec!["b1", "b2"], "a refused sweep expires nothing");

        // Switch on: the policy is pushed to the existing key and the same sweep completes.
        l.svc.set_policy_opts(weftos_store::policy::PolicyOpts { sweeper_put: true, ..Default::default() });
        assert!(l.svc.reconcile().is_empty());
        let on = rel::Sweeper::new(&direct, &rn, &tk).sweep(&policy, 3_000, 0, "integ").await.expect("the sweep completes with the switch on");
        assert_eq!(on.expired, vec!["b1"]);
        assert_eq!(rd.build_ids().await.unwrap(), vec!["b2"]);
        clean_up(&l, &name);
    });
}

// ---- S1 x S3 ----------------------------------------------------------------------------------

/// What a daemon has to do today to feed S1's chain events to S3's `GrantLedger`: nothing in
/// either crate does it. S1 now stamps a gapless `seq` on its grant-affecting events (checked
/// here for every one, whoever it concerns) and the ledger applies by `seq`. But `store.create`
/// of a node store, the only record of the node's default write grant (S3's pairing event), is
/// not stamped, and S1's stamps start at the first change after it. So the glue gives the
/// create `seq` 1 and shifts every stamp up by one; S1 stamping `store.create` (or recording the
/// default grant as a stamped `store.grant`) would remove the shift.
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
                    // S3 knows read and write only: S1's write and admin both mean "may write".
                    "write" | "admin" => s3pol::Access::Write,
                    other => panic!("S1 access {other:?} has no S3 equivalent"),
                };
                GrantEvent { seq, principal, store: store.into(), access, op: GrantOp::Grant }
            }
            // S1's revoke removes the whole grant, whatever its level: S3's RevokeAll.
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

/// Joins S1 (`StoreService` grant/revoke, `ChainSink`, the principal's RustFS key) and S3
/// (`GrantLedger`). S1's chain events, adapted, drive the ledger; after the node-store create,
/// after a revoke and after a re-grant, the ledger and the node's own S3 key at RustFS agree on
/// whether the node may write its bucket.
#[test]
#[ignore = "needs a throwaway RustFS: run scripts/store/integ-live-test.sh (WEFTOS_STORE_TEST_RUSTFS_ENV)"]
fn live_s1_s3_grant_ledger_agrees_with_what_rustfs_enforces() {
    let l = live();
    let node_id = uniq("n");
    let node = StoreName::node("weft", &node_id).unwrap();
    let node_p = Principal::parse(&format!("node:{node_id}")).unwrap();
    l.svc.ensure_store(&node, "daemon", Parties::defaults_for(&node)).unwrap();
    let bucket = node.bucket();
    assert_eq!(bucket, s3pol::node_bucket(&node_id).unwrap(), "S3's node bucket rule and S1's StoreName must agree");
    let store_uri = node_store_name("weft", &node_id);
    assert_eq!(store_uri, node.to_string(), "S3's node store name and S1's must be one string");

    let key = as_key(&l, &node_p.to_string());
    let ledger = GrantLedger::new();
    let mut fed = 0usize;
    let mut last_stamp = 0u64;
    let mut feed = |ledger: &GrantLedger| -> Vec<Applied> {
        let chain = l.chain.0.lock().unwrap().clone();
        let evs = to_grant_events(&chain[fed..], &mut last_stamp);
        fed = chain.len();
        evs.iter().map(|e| ledger.apply(e)).collect()
    };
    let writes = |k: &str| key.put(&bucket, k, b"v", false);

    // 1. Created: S1 gave the node write on its own store.
    {
        let chain = l.chain.0.lock().unwrap();
        assert!(!chain.iter().any(|(k, _)| k == "store.grant"), "GAP: S1 records the node's default write grant only inside store.create, not as store.grant");
        assert!(chain.iter().filter(|(k, _)| k == "store.create").all(|(_, v)| v.get("seq").is_none()), "GAP: store.create carries no seq");
    }
    feed(&ledger);
    assert!(ledger.holds(&node_id, &store_uri, s3pol::Access::Write), "ledger: node holds write");
    writes("a").expect("RustFS: node key writes");

    // 2. Revoked: both refuse, and S3 reports the node lost write (the broker's cue to cut sessions).
    l.svc.revoke(&node, &node_p, "owner").unwrap();
    let applied = feed(&ledger);
    assert_eq!(applied, vec![Applied::Changed { lost_write: true }]);
    assert!(!ledger.holds(&node_id, &store_uri, s3pol::Access::Write) && !ledger.holds(&node_id, &store_uri, s3pol::Access::Read));
    assert!(is_forbidden(writes("b")), "RustFS: node key refused after revoke");
    assert!(is_forbidden(key.get(&bucket, "a")), "RustFS: node key cannot read either");

    // 3. Re-granted read only: the ledger says read, not write; RustFS reads and refuses a write.
    l.svc.grant(&node, node_p.clone(), Access::Read, "owner").unwrap();
    feed(&ledger);
    assert!(ledger.holds(&node_id, &store_uri, s3pol::Access::Read) && !ledger.holds(&node_id, &store_uri, s3pol::Access::Write));
    assert_eq!(key.get(&bucket, "a").unwrap(), b"v");
    assert!(is_forbidden(writes("c")), "RustFS: read-only key cannot write");

    // 4. Write again.
    l.svc.grant(&node, node_p.clone(), Access::Write, "owner").unwrap();
    feed(&ledger);
    assert!(ledger.holds(&node_id, &store_uri, s3pol::Access::Write));
    writes("d").expect("RustFS: node key writes again");

    let _ = l.be.delete(&bucket, "a");
    let _ = l.be.delete(&bucket, "d");
    let _ = l.be.delete_bucket(&bucket);
}

/// S1 x S3, no RustFS needed. S3's broker names the RustFS user it disables on revoke with
/// `principal_for`; S1 creates and scopes the node's user with `RustfsBackend::key_id_for`. The
/// bucket rule agrees. The user rule does not, so S3's `revoke` would disable a user S1 never made.
#[test]
fn s1_s3_node_bucket_names_agree_but_rustfs_user_names_do_not() {
    for id in ["n1", "pg-1", "a"] {
        assert_eq!(s3pol::node_bucket(id).unwrap(), StoreName::node("weft", id).unwrap().bucket(), "bucket rule for {id}");
    }
    let s3_user = s3pol::principal_for("n1");
    let s1_user = RustfsBackend::key_id_for("node:n1");
    assert_ne!(s3_user, s1_user, "documented GAP: S3 disables {s3_user:?}, S1's node key is {s1_user:?}");
}
