//! Pairing: payload verification, both sides' trust files, the chain, the
//! reporter seam. Temp dirs and a fake dashboard only (never `~/.weftos`).

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;

use clawft_kernel::chain::ChainManager;
use ed25519_dalek::SigningKey;
use serde_json::{Value, json};

use super::*;
use crate::dashboard_report::Beat;
use crate::dashboard_test_support::*;

const ULID_A: &str = "01K00000000000000000000000";
const ULID_B: &str = "01K00000000000000000000001";
const HB: &str = "/api/nodes/heartbeat";

/// A deterministic test key: its public half (hex) and its node id.
fn peer(seed: u8) -> (String, String) {
    let pk = SigningKey::from_bytes(&[seed; 32]).verifying_key().to_bytes();
    (hex::encode(pk), clawft_kernel::node_id_from_pubkey(&pk))
}

fn payload(op: &str, role: &str, seed: u8, projects: &[&str]) -> Value {
    let (key, node) = peer(seed);
    json!({
        "op": op, "request_id": "11111111-2222-4333-8444-555555555555",
        "peer_node": node, "fingerprint": &node[..16], "peer_ed25519": key,
        "advertise": "100.64.0.9:9471", "role": role, "projects": projects,
    })
}

fn deps(dir: &Path, chain: &Arc<ChainManager>) -> PairDeps {
    PairDeps { runtime_dir: dir.to_path_buf(), chain: Some(chain.clone()), self_node: Some(peer(9).1) }
}

fn peers(dir: &Path) -> Vec<Value> {
    match std::fs::read_to_string(dir.join(PEERS_FILE)) {
        Ok(t) => serde_json::from_str(&t).unwrap(),
        Err(_) => vec![],
    }
}

fn mode(p: &Path) -> u32 {
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

#[test]
fn identity_is_derived_from_the_key_and_carries_no_secret() {
    let sk = SigningKey::from_bytes(&[7; 32]);
    let pk = sk.verifying_key().to_bytes();
    let id = MeshIdentity::from_pubkey(&pk, None);
    assert_eq!(id.node, clawft_kernel::node_id_from_pubkey(&pk));
    assert_eq!(id.fingerprint, id.node[..16]);
    assert_eq!(id.fingerprint, fingerprint_of(&pk));
    assert_eq!(id.ed25519, hex::encode(pk));
    let v = serde_json::to_value(&id).unwrap();
    assert!(v.get("advertise").is_none(), "omitted when unknown");
    assert!(!v.to_string().contains(&hex::encode(sk.to_bytes())));
    let v = serde_json::to_value(MeshIdentity::from_pubkey(&pk, Some("100.64.0.1:9471".into()))).unwrap();
    assert_eq!(v["advertise"], "100.64.0.1:9471");
}

#[test]
fn a_fingerprint_or_id_that_is_not_the_keys_is_refused_before_any_write() {
    let d = tempfile::tempdir().unwrap();
    let chain = Arc::new(ChainManager::new(0, 1000));
    let dp = deps(d.path(), &chain);
    let (other_key, other_node) = peer(2);

    let mut p = payload("add", "member", 1, &[ULID_A]);
    p["fingerprint"] = json!(&other_node[..16]);
    assert!(apply(&dp, "a1", &p).unwrap_err().contains("fingerprint"));

    let mut p = payload("add", "member", 1, &[ULID_A]);
    p["peer_ed25519"] = json!(other_key);
    assert!(apply(&dp, "a2", &p).unwrap_err().contains("not the key of peer_node"));

    let mut p = payload("add", "member", 1, &[ULID_A]);
    p["peer_ed25519"] = json!("ff".repeat(32));
    assert!(apply(&dp, "a3", &p).is_err(), "not a curve point");

    assert!(!d.path().join(PEERS_FILE).exists());
    assert!(!d.path().join(crate::project_fetch_grants::FETCH_FILE).exists());
    assert_eq!(chain.tail(0).iter().filter(|e| e.source == CHAIN_SOURCE).count(), 0);
}

#[test]
fn malformed_payloads_are_refused() {
    let d = tempfile::tempdir().unwrap();
    let dp = deps(d.path(), &Arc::new(ChainManager::new(0, 1000)));
    let ok = payload("add", "member", 1, &[ULID_A]);
    let mut bad: Vec<(&str, Value)> = vec![
        ("not an object", json!("add")),
        ("empty", json!({})),
    ];
    let mut with = |name: &'static str, f: &dyn Fn(&mut Value)| {
        let mut p = ok.clone();
        f(&mut p);
        bad.push((name, p));
    };
    with("unknown field", &|p| p["node_key"] = json!("x"));
    with("bad op", &|p| p["op"] = json!("upsert"));
    with("bad role", &|p| p["role"] = json!("controller"));
    with("upper-case node id", &|p| p["peer_node"] = json!(p["peer_node"].as_str().unwrap().to_uppercase()));
    with("short fingerprint", &|p| p["fingerprint"] = json!("abc"));
    with("advertise without port", &|p| p["advertise"] = json!("pi5.local"));
    with("advertise with a scheme", &|p| p["advertise"] = json!("mem://local"));
    with("bad project", &|p| p["projects"] = json!(["not-a-ulid"]));
    with("request id with a slash", &|p| p["request_id"] = json!("../x"));
    with("missing key", &|p| {
        p.as_object_mut().unwrap().remove("peer_ed25519");
    });
    for (name, p) in bad {
        assert!(apply(&dp, "a", &p).is_err(), "{name} must be refused");
    }
    assert!(!d.path().join(PEERS_FILE).exists());
}

#[test]
fn the_member_side_pins_the_primary_and_remove_undoes_it() {
    let d = tempfile::tempdir().unwrap();
    let chain = Arc::new(ChainManager::new(0, 1000));
    let dp = deps(d.path(), &chain);
    let (key, node) = peer(1);

    let r = apply(&dp, "act-1", &payload("add", "primary", 1, &[ULID_A])).unwrap();
    assert_eq!(r, json!({ "op": "add", "peer_node": node, "fingerprint": &node[..16], "tier": "pinned", "projects": [ULID_A] }));
    let ps = peers(d.path());
    assert_eq!(ps, vec![json!({ "addr": "100.64.0.9:9471", "tier": "pinned", "key": key })]);
    assert_eq!(mode(&d.path().join(PEERS_FILE)), 0o600);
    assert!(!d.path().join(crate::project_fetch_grants::FETCH_FILE).exists(), "a member grants nothing");
    assert!(!d.path().join("workload-host.json").exists(), "never a controller");
    assert_eq!(crate::mesh_pairings::primary_for(d.path(), ULID_A).unwrap().as_deref(), Some(node.as_str()), "the member records which primary serves the project");

    let r = apply(&dp, "act-2", &payload("remove", "primary", 1, &[ULID_A])).unwrap();
    assert_eq!(r["op"], "remove");
    assert!(peers(d.path()).is_empty());
    assert_eq!(crate::mesh_pairings::primary_for(d.path(), ULID_A).unwrap(), None);

    let ev: Vec<_> = chain.tail(0).into_iter().filter(|e| e.source == CHAIN_SOURCE).collect();
    assert_eq!(ev.len(), 2);
    assert_eq!(ev[0].kind, CHAIN_ADD);
    assert_eq!(ev[1].kind, CHAIN_REMOVE);
    let p = ev[0].payload.as_ref().unwrap();
    assert_eq!(p["action_id"], "act-1");
    assert_eq!(p["request_id"], "11111111-2222-4333-8444-555555555555");
    assert_eq!(p["peer_node"], node);
    assert_eq!(p["tier"], "pinned");
    assert!(!p.to_string().contains(&key), "no key material on the chain");
}

#[test]
fn the_primary_side_pairs_the_member_grants_its_projects_and_keeps_strangers() {
    let d = tempfile::tempdir().unwrap();
    let chain = Arc::new(ChainManager::new(0, 1000));
    let dp = deps(d.path(), &chain);
    let (key, node) = peer(1);
    let (stranger_key, _) = peer(5);
    let hand = json!([{ "addr": "pi5.local:9471", "tier": "pinned", "key": stranger_key }]);
    crate::dashboard_token::write_atomic(&d.path().join(PEERS_FILE), &hand.to_string()).unwrap();
    let host = json!({ "listen": "0.0.0.0:9471", "controllers": [stranger_key] }).to_string();
    std::fs::write(d.path().join("workload-host.json"), &host).unwrap();

    apply(&dp, "act-1", &payload("add", "member", 1, &[ULID_A])).unwrap();
    // Re-add with a wider scope: still one entry, one grant.
    let r = apply(&dp, "act-2", &payload("add", "member", 1, &[ULID_A, ULID_B])).unwrap();
    assert_eq!(r["tier"], "paired");
    let ps = peers(d.path());
    assert_eq!(ps.len(), 2, "{ps:?}");
    assert_eq!(ps[0]["key"], stranger_key, "operator entry kept first and intact");
    assert_eq!(ps[1], json!({ "addr": "100.64.0.9:9471", "tier": "paired", "key": key }));
    let g = crate::project_fetch_grants::Grants::load(d.path()).unwrap();
    assert_eq!(g.for_peer(&node).len(), 1);
    assert!(g.is_granted(&node, ULID_A));
    assert!(g.is_granted(&node, ULID_B));
    let grant = &g.for_peer(&node)[0];
    assert_eq!(grant.source.as_deref(), Some("dashboard-pair:act-2"));
    assert_eq!(grant.peer_ed25519.as_deref(), Some(key.as_str()));
    assert_eq!(mode(&d.path().join(crate::project_fetch_grants::FETCH_FILE)), 0o600);
    assert_eq!(std::fs::read_to_string(d.path().join("workload-host.json")).unwrap(), host, "controllers untouched");

    apply(&dp, "act-3", &payload("remove", "member", 1, &[])).unwrap();
    let ps = peers(d.path());
    assert_eq!(ps.len(), 1);
    assert_eq!(ps[0]["key"], stranger_key);
    assert!(!crate::project_fetch_grants::Grants::load(d.path()).unwrap().is_granted(&node, ULID_A));
    assert!(apply(&dp, "act-4", &payload("remove", "member", 1, &[])).is_ok(), "remove is idempotent");
}

#[test]
fn pairing_with_itself_is_refused_and_pending_requests_settle_on_add() {
    let d = tempfile::tempdir().unwrap();
    let dp = deps(d.path(), &Arc::new(ChainManager::new(0, 1000)));
    assert!(apply(&dp, "a", &payload("add", "member", 9, &[])).unwrap_err().contains("this node"));

    let (_, node) = peer(1);
    let req = mesh_pair_requests::record(d.path(), &node, &[ULID_A.to_owned()]).unwrap();
    let other = mesh_pair_requests::record(d.path(), &peer(2).1, &[]).unwrap();
    let mut p = payload("add", "primary", 1, &[ULID_A]);
    p["request_id"] = json!(req.request_id);
    apply(&dp, "a", &p).unwrap();
    assert_eq!(mesh_pair_requests::list(d.path()).unwrap(), vec![other]);
}

#[cfg(all(feature = "placement", unix))]
#[test]
fn what_is_written_is_what_the_placement_policy_loads() {
    use clawft_types::placement::TrustTier;
    let d = tempfile::tempdir().unwrap();
    let dp = deps(d.path(), &Arc::new(ChainManager::new(0, 1000)));
    assert_eq!(PEERS_FILE, crate::workload_place_policy::PEERS_FILE);
    apply(&dp, "a", &payload("add", "primary", 1, &[])).unwrap();
    let loaded = crate::workload_place_policy::load_peers(d.path()).unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].addr, "100.64.0.9:9471");
    assert_eq!(loaded[0].tier, TrustTier::Pinned);
    assert_eq!(loaded[0].key.map(hex::encode), Some(peer(1).0));
}

#[test]
fn primary_accepts_a_member_without_advertise_and_fetch_authorises_it() {
    let d = tempfile::tempdir().unwrap();
    let chain = Arc::new(ChainManager::new(0, 1000));
    let dp = deps(d.path(), &chain);
    let mut p = payload("add", "member", 1, &[ULID_A]);
    p.as_object_mut().unwrap().remove("advertise");
    apply(&dp, "act-1", &p).unwrap();
    let ps = peers(d.path());
    assert_eq!(ps.len(), 1);
    assert_eq!(ps[0]["key"], peer(1).0);
    assert_eq!(ps[0]["tier"], "paired");
    assert!(ps[0].get("addr").is_none(), "no address to dial");
    let (_, node) = peer(1);
    let (tier, key) = crate::project_fetch_policy::listed_peer(d.path(), &node).unwrap().expect("listed by key");
    assert_eq!(tier, clawft_types::placement::TrustTier::Paired);
    assert_eq!(hex::encode(key), peer(1).0);
    assert!(
        crate::project_fetch_policy::authorize(d.path(), &node, ULID_A).is_ok(),
        "project fetch is granted without an address"
    );
    // The addr-less entry is never offered to the dialer.
    assert!(crate::workload_place_policy::load_peers(d.path()).unwrap().is_empty());
    // And it is removable.
    p["op"] = json!("remove");
    apply(&dp, "act-2", &p).unwrap();
    assert!(peers(d.path()).is_empty());
}

#[test]
fn member_refuses_a_primary_without_advertise_and_writes_nothing() {
    let d = tempfile::tempdir().unwrap();
    let dp = deps(d.path(), &Arc::new(ChainManager::new(0, 1000)));
    let mut p = payload("add", "primary", 1, &[]);
    p.as_object_mut().unwrap().remove("advertise");
    let e = apply(&dp, "a", &p).unwrap_err();
    assert!(e.contains("missing field advertise") && e.contains("dial its primary"), "{e}");
    assert!(!d.path().join(PEERS_FILE).exists());
    assert!(!d.path().join(crate::mesh_pairings::PAIRINGS_FILE).exists());
}

#[cfg(all(feature = "placement", unix))]
#[test]
fn addr_less_peer_entries_are_not_dialed_but_keep_their_key() {
    let d = tempfile::tempdir().unwrap();
    peers_add(d.path(), &peer(1).0, clawft_types::placement::TrustTier::Paired, None).unwrap();
    peers_add(d.path(), &peer(2).0, clawft_types::placement::TrustTier::Paired, Some("100.64.0.2:9471")).unwrap();
    let dialable = crate::workload_place_policy::load_peers(d.path()).unwrap();
    assert_eq!(dialable.len(), 1);
    assert_eq!(dialable[0].addr, "100.64.0.2:9471");
    assert_eq!(crate::workload_place_policy::load_peer_keys(d.path()).unwrap().len(), 2);
}

struct Src(Option<MeshIdentity>, Vec<PairRequest>);

#[async_trait]
impl PairSource for Src {
    async fn mesh_identity(&self) -> Option<MeshIdentity> {
        self.0.clone()
    }
    async fn pair_requests(&self) -> Vec<PairRequest> {
        self.1.clone()
    }
}

fn beat_body(fake: &FakeDash) -> Value {
    serde_json::from_str(&fake.requests(HB).last().unwrap().body).unwrap()
}

#[tokio::test]
async fn the_report_carries_mesh_identity_and_pair_requests_only_when_there_are_some() {
    let fake = FakeDash::start().await;
    fake.answer(HB, &[(200, "{}"), (200, "{}"), (200, "{}")]);
    let (_d, path) = token_dir(&token('a'));

    // No source: neither key.
    let dash = dashboard(config(&fake.url, &path));
    assert_eq!(dash.heartbeat_once().await, Beat::Ok);
    let r = &beat_body(&fake)["report"];
    assert!(r.get("mesh_identity").is_none());
    assert!(r.get("pair_requests").is_none());

    // Mesh off: identity omitted, requests still reported.
    let req = PairRequest { request_id: "r1".into(), with_node: peer(1).1, projects: vec![ULID_A.into()], requested_at: "2026-10-08T00:00:00Z".into() };
    let dash = dashboard(config(&fake.url, &path));
    dash.set_pair_source(Arc::new(Src(None, vec![req.clone()])));
    assert_eq!(dash.heartbeat_once().await, Beat::Ok);
    let r = &beat_body(&fake)["report"];
    assert!(r.get("mesh_identity").is_none());
    assert_eq!(r["pair_requests"][0]["request_id"], "r1");
    assert_eq!(r["pair_requests"][0]["with_node"], peer(1).1);

    // Mesh on.
    let pk = SigningKey::from_bytes(&[3; 32]).verifying_key().to_bytes();
    let dash = dashboard(config(&fake.url, &path));
    dash.set_pair_source(Arc::new(Src(Some(MeshIdentity::from_pubkey(&pk, Some("100.64.0.3:9471".into()))), vec![])));
    assert_eq!(dash.heartbeat_once().await, Beat::Ok);
    let r = &beat_body(&fake)["report"];
    assert_eq!(r["mesh_identity"]["node"], clawft_kernel::node_id_from_pubkey(&pk));
    assert_eq!(r["mesh_identity"]["fingerprint"], fingerprint_of(&pk));
    assert_eq!(r["mesh_identity"]["advertise"], "100.64.0.3:9471");
    assert_eq!(r["pair_requests"], json!([]));
}

#[tokio::test]
async fn a_pair_action_from_the_dashboard_runs_the_handler_and_reports_its_outcome() {
    let fake = FakeDash::start().await;
    let (_d, path) = token_dir(&token('a'));
    let dir = tempfile::tempdir().unwrap();
    let chain = Arc::new(ChainManager::new(0, 1000));
    let good = payload("add", "primary", 1, &[ULID_A]);
    let mut bad = payload("add", "primary", 2, &[ULID_A]);
    bad["fingerprint"] = json!("0000000000000000");
    let answer = json!({ "ok": true, "actions": [
        { "id": "ok-1", "kind": "pair", "payload": good },
        { "id": "bad-1", "kind": "pair", "payload": bad },
    ] });
    fake.answer(HB, &[(200, &answer.to_string())]);
    fake.answer("/api/nodes/actions/ok-1/result", &[(200, "{}"), (200, "{}")]);
    fake.answer("/api/nodes/actions/bad-1/result", &[(200, "{}"), (200, "{}")]);

    let dash = dashboard(config(&fake.url, &path));
    dash.set_action_handler(Arc::new(PairHandler::with_deps(deps(dir.path(), &chain))));
    assert_eq!(dash.heartbeat_once().await, Beat::Ok);

    let results = |id: &str| -> Vec<Value> {
        fake.requests(&format!("/api/nodes/actions/{id}/result")).iter().map(|r| serde_json::from_str(&r.body).unwrap()).collect()
    };
    let ok = results("ok-1");
    assert_eq!(ok[0]["status"], "running");
    assert_eq!(ok[1]["status"], "succeeded");
    assert_eq!(ok[1]["result"]["tier"], "pinned");
    assert_eq!(ok[1]["result"]["peer_node"], peer(1).1);
    let bad = results("bad-1");
    assert_eq!(bad[1]["status"], "failed");
    assert!(bad[1]["result"]["error"].as_str().unwrap().contains("fingerprint"));
    assert_eq!(peers(dir.path()).len(), 1, "only the verified one was written");
}

#[tokio::test]
async fn without_init_the_global_handler_fails_clearly() {
    let h = PairHandler::global();
    let out = h.handle(&Action { id: "x".into(), kind: "pair".into(), payload: json!({}), created_at: None }).await;
    assert_eq!(out.status, "failed");
    assert!(out.result["error"].as_str().unwrap().contains("not initialised"));
}
