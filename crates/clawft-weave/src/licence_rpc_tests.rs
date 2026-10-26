//! `workload.node.bind | unbind | binding` and the licence boot, against a
//! stub Seed. Every test pins its own runtime dir (a tempdir) and never
//! contacts a real Seed.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use clawft_kernel::chain::ChainManager;
use clawft_kernel::licence::{
    BindState, BindingRecord, MeshId, SignedBinding, key_id, sign_binding,
};
use clawft_kernel::revocation::RevocationList;
use clawft_kernel::workload_pkg::codec::hex_encode;
use clawft_kernel::workload_pkg::{KeyOrigin, TrustAnchors};
use clawft_kernel::workload_runtime::seed_http::{Method, SeedCredentials, SeedTransport};
use clawft_kernel::workload_runtime::{
    LinkSecurity, RuntimeError, SeedApiRuntime, SeedConfig, SeedPin,
};
use clawft_types::config::{MeshAdmissionMode, MeshConfig};
use clawft_types::secret::SecretString;
use ed25519_dalek::SigningKey;
use serde_json::{Value, json};

use super::*;
use crate::capability::{CallerCapabilities, Capability, required_capability};
use crate::licence_boot::{InitArgs, LicenceRuntime, build};

const NOW: u64 = 1_790_000_000;
const DEVICE: &str = "seed-dev-1";
const PIN: &str = "aa00000000000000000000000000000000000000000000000000000000000001";
const NONCE_A: &str = "bb00000000000000000000000000000000000000000000000000000000000002";
const NONCE_B: &str = "cc00000000000000000000000000000000000000000000000000000000000003";

fn sk(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}
fn pk(k: &SigningKey) -> String {
    hex_encode(&k.verifying_key().to_bytes())
}

/// A Seed that answers `GET /api/v1/identity` and nothing else, over a link
/// that counts as pinned.
struct StubSeed;

#[async_trait]
impl SeedTransport for StubSeed {
    async fn request(
        &self,
        _m: Method,
        path: &str,
        _b: Option<&Value>,
        _t: &SecretString,
        _to: Duration,
    ) -> Result<(u16, Value), RuntimeError> {
        assert_eq!(path, "/api/v1/identity", "a bind reads only the identity");
        Ok((200, json!({"device_id": DEVICE, "public_key": pk(&sk(20))})))
    }
    fn link_security(&self) -> LinkSecurity {
        LinkSecurity::Pinned
    }
}

struct Token;
impl SeedCredentials for Token {
    fn get(&self, _: &str) -> Result<SecretString, RuntimeError> {
        Ok(SecretString::new("stub-token-0123456789abcdef".to_owned()))
    }
    fn put(&self, _: &str, _: SecretString) -> Result<(), RuntimeError> {
        Ok(())
    }
}

fn stub_runtime(node: &str) -> SeedApiRuntime {
    SeedApiRuntime::new(
        SeedConfig {
            node_id: node.into(),
            pins: vec![SeedPin::new("fall-detect", "1.0.0")],
            concurrency_cap: 2,
        },
        Arc::new(StubSeed),
        Arc::new(Token),
    )
    .unwrap()
}

fn mesh_cfg(nonce: Option<&str>) -> MeshConfig {
    MeshConfig {
        enabled: true,
        admission: MeshAdmissionMode::Enforce,
        genesis_hash: Some(PIN.into()),
        mesh_nonce: nonce.map(str::to_owned),
        ..MeshConfig::default()
    }
}

fn boot(dir: &Path, chain: &Arc<ChainManager>, mesh: &MeshConfig) -> LicenceRuntime {
    let mut anchors = TrustAnchors::default();
    anchors.push_signer("op", &pk(&sk(1)), KeyOrigin::Operator).unwrap();
    build(InitArgs {
        dir,
        anchors,
        revocations: Arc::new(RevocationList::new(dir.join("revoked.json"))),
        chain: chain.clone(),
        mesh: Some(mesh),
        steward_node_id: "node-steward".into(),
        steward_pubkey: pk(&sk(21)),
    })
}

fn mesh_of(nonce: &str) -> MeshId {
    MeshId::derive(&[0xaa, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
        &clawft_kernel::workload_pkg::codec::hex_decode_exact::<32>(nonce).unwrap())
}

fn posture() -> AdmissionPosture {
    licence_boot::posture(Some(&mesh_cfg(Some(NONCE_A))), true)
}

fn lookup(id: &str) -> Result<SeedApiRuntime, String> {
    if id == "seed-kitchen" { Ok(stub_runtime(id)) } else { Err(format!("no Seed {id:?}")) }
}

async fn call(rt: &LicenceRuntime, p: AdmissionPosture, m: &str, params: Value) -> Response {
    super::route(&Ctx { rt, posture: p, now: NOW, seed: &lookup, exchange: None }, m, params).await
}

fn record(mesh: &MeshId, seq: u64, state: BindState) -> BindingRecord {
    BindingRecord {
        v: 2,
        device_id: DEVICE.into(),
        device_pubkey: pk(&sk(20)),
        mesh_id: mesh.to_hex(),
        grant_pubkey: pk(&sk(2)),
        steward_node_id: "node-steward".into(),
        steward_pubkey: pk(&sk(21)),
        state,
        seq,
        bound_at: NOW + seq, // strictly increasing: the replay memory refuses an equal one
    }
}

fn signed(r: &BindingRecord) -> SignedBinding {
    sign_binding(r, &sk(1)).unwrap()
}

fn fp() -> String {
    key_id(&sk(2).verifying_key().to_bytes())
}

fn bind_params(r: &BindingRecord) -> Value {
    json!({"seed_node_id": "seed-kitchen", "signed": signed(r), "grant_fingerprint": fp()})
}

fn chain_kinds(chain: &ChainManager) -> Vec<String> {
    chain.tail(chain.len()).into_iter().map(|e| e.kind).collect()
}

fn err_of(r: &Response) -> String {
    assert!(!r.ok, "expected an error, got {:?}", r.result);
    r.error.clone().unwrap_or_default()
}

#[test]
fn the_nonce_derives_the_mesh_id_at_boot_and_a_missing_one_leaves_it_unset() {
    let dir = tempfile::tempdir().unwrap();
    let chain = Arc::new(ChainManager::new(0, 1000));
    let rt = boot(dir.path(), &chain, &mesh_cfg(Some(NONCE_A)));
    assert_eq!(rt.local.get(), Some(mesh_of(NONCE_A)));
    assert!(rt.genesis_pinned && rt.nonce_set && rt.config_error.is_none());
    assert_eq!(licence_boot::status(&rt)["mesh_id"], mesh_of(NONCE_A).to_hex().as_str());

    let none = boot(dir.path(), &chain, &mesh_cfg(None));
    assert_eq!(none.local.get(), None, "no nonce: no mesh id, the path is inert");
    assert!(!none.nonce_set);
    assert!(licence_boot::status(&none)["mesh_id"].is_null());

    let bad = boot(dir.path(), &chain, &mesh_cfg(Some("not-hex")));
    assert_eq!(bad.local.get(), None);
    assert!(bad.config_error.as_deref().unwrap().contains("mesh_nonce"));
}

#[tokio::test]
async fn prepare_reads_the_seed_and_names_what_the_operator_signs() {
    let dir = tempfile::tempdir().unwrap();
    let chain = Arc::new(ChainManager::new(0, 1000));
    let rt = boot(dir.path(), &chain, &mesh_cfg(Some(NONCE_A)));
    let r = call(&rt, posture(), "workload.node.bind", json!({"seed_node_id": "seed-kitchen", "prepare": true})).await;
    assert!(r.ok, "{:?}", r.error);
    let v = r.result.unwrap();
    assert_eq!(v["device_id"], DEVICE);
    assert_eq!(v["mesh_id"], mesh_of(NONCE_A).to_hex().as_str());
    assert_eq!(v["steward_node_id"], "node-steward");
    assert_eq!(v["next_seq"], 1);
    // An unknown Seed is an error, not a panic.
    let e = call(&rt, posture(), "workload.node.bind", json!({"seed_node_id": "nope", "prepare": true})).await;
    assert!(err_of(&e).contains("no Seed"));
}

#[tokio::test]
async fn bind_persists_replay_state_and_a_restart_refuses_a_replay() {
    let dir = tempfile::tempdir().unwrap();
    let chain = Arc::new(ChainManager::new(0, 1000));
    let rt = boot(dir.path(), &chain, &mesh_cfg(Some(NONCE_A)));
    let rec = record(&mesh_of(NONCE_A), 1, BindState::Bound);
    let r = call(&rt, posture(), "workload.node.bind", bind_params(&rec)).await;
    assert!(r.ok, "{:?}", r.error);
    assert!(dir.path().join(clawft_kernel::workload_runtime::seed_bind::BIND_STATE_FILE).is_file());
    assert!(chain_kinds(&chain).contains(&"workload.node.bind".to_owned()));
    assert_eq!(rt.policy.store().active_binding(), Some(rec.clone()));
    let st = call(&rt, posture(), "workload.node.binding", json!({})).await.result.unwrap();
    assert_eq!(st["binding"]["state"], "bound");
    assert_eq!(st["next_seq"], 2);

    // The daemon restarts: the stored binding and the replay memory remain.
    let again = boot(dir.path(), &chain, &mesh_cfg(Some(NONCE_A)));
    assert_eq!(again.policy.store().active_binding(), Some(rec.clone()), "binding survives a restart");
    let e = call(&again, posture(), "workload.node.bind", bind_params(&rec)).await;
    assert!(err_of(&e).contains("seq_not_greater") || err_of(&e).contains("replayed"), "{e:?}");
}

#[tokio::test]
async fn a_second_mesh_is_refused_with_seed_bound_elsewhere() {
    let dir = tempfile::tempdir().unwrap();
    let chain = Arc::new(ChainManager::new(0, 1000));
    let rt = boot(dir.path(), &chain, &mesh_cfg(Some(NONCE_A)));
    let a = record(&mesh_of(NONCE_A), 1, BindState::Bound);
    assert!(call(&rt, posture(), "workload.node.bind", bind_params(&a)).await.ok);
    // The same Seed offered to another mesh id (the node now computes it).
    rt.local.set(Some(mesh_of(NONCE_B)));
    let b = record(&mesh_of(NONCE_B), 2, BindState::Bound);
    let e = call(&rt, posture(), "workload.node.bind", bind_params(&b)).await;
    assert!(err_of(&e).contains("seed_bound_elsewhere"), "{e:?}");
    // Unbind, then the bind goes through.
    let u = record(&mesh_of(NONCE_B), 2, BindState::Unbound);
    let r = call(&rt, posture(), "workload.node.unbind", json!({"signed": signed(&u)})).await;
    assert!(r.ok, "{:?}", r.error);
    assert!(chain_kinds(&chain).contains(&"workload.node.unbind".to_owned()));
    let b3 = record(&mesh_of(NONCE_B), 3, BindState::Bound);
    assert!(call(&rt, posture(), "workload.node.bind", bind_params(&b3)).await.ok);
}

#[tokio::test]
async fn a_changed_nonce_orphans_the_binding_chains_it_and_the_doctor_reports_it() {
    let dir = tempfile::tempdir().unwrap();
    let chain = Arc::new(ChainManager::new(0, 1000));
    let rt = boot(dir.path(), &chain, &mesh_cfg(Some(NONCE_A)));
    assert!(call(&rt, posture(), "workload.node.bind", bind_params(&record(&mesh_of(NONCE_A), 1, BindState::Bound))).await.ok);
    assert!(!chain_kinds(&chain).contains(&"licence.binding_orphaned".to_owned()));

    // Restart with another nonce: orphaned at boot, no first use needed.
    let rt2 = boot(dir.path(), &chain, &mesh_cfg(Some(NONCE_B)));
    let kinds = chain_kinds(&chain);
    assert_eq!(kinds.iter().filter(|k| *k == "licence.binding_orphaned").count(), 1, "{kinds:?}");
    assert!(rt2.policy.store().active_binding().is_none(), "checkout is off");
    let st = licence_boot::status(&rt2);
    assert_eq!(st["binding"]["orphaned"], true);
    let found = crate::licence_doctor::findings(&st);
    assert_eq!(found[0].id, "licence.binding_orphaned");

    // No nonce at all, with a binding held: the other doctor case.
    let rt3 = boot(dir.path(), &chain, &mesh_cfg(None));
    let found = crate::licence_doctor::findings(&licence_boot::status(&rt3));
    assert_eq!(found[0].id, "licence.mesh_id_unset");
}

#[tokio::test]
async fn bind_without_a_mesh_id_or_under_open_membership_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let chain = Arc::new(ChainManager::new(0, 1000));
    let none = boot(dir.path(), &chain, &mesh_cfg(None));
    let rec = record(&mesh_of(NONCE_A), 1, BindState::Bound);
    assert!(err_of(&call(&none, posture(), "workload.node.bind", bind_params(&rec)).await).contains("no local mesh id"));

    let rt = boot(dir.path(), &chain, &mesh_cfg(Some(NONCE_A)));
    let open = AdmissionPosture { open_membership: true, ..posture() };
    let e = call(&rt, open, "workload.node.bind", bind_params(&rec)).await;
    assert!(err_of(&e).contains("open_membership"), "{e:?}");
    assert!(rt.policy.store().held_binding().is_none());
}

#[tokio::test]
async fn a_wrong_fingerprint_or_bad_params_bind_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let chain = Arc::new(ChainManager::new(0, 1000));
    let rt = boot(dir.path(), &chain, &mesh_cfg(Some(NONCE_A)));
    let rec = record(&mesh_of(NONCE_A), 1, BindState::Bound);
    let mut p = bind_params(&rec);
    p["grant_fingerprint"] = json!("ed25519:0000000000000000");
    assert!(err_of(&call(&rt, posture(), "workload.node.bind", p).await).contains("fingerprint_mismatch"));
    let e = call(&rt, posture(), "workload.node.bind", json!({"seed_node_id": "seed-kitchen"})).await;
    assert!(err_of(&e).contains("needs 'signed'"));
    let e = call(&rt, posture(), "workload.node.bind", json!({"seed_node_id": "x", "bogus": 1})).await;
    assert!(err_of(&e).contains("invalid"));
    assert!(rt.policy.store().held_binding().is_none());
}

#[tokio::test]
async fn reset_floor_previews_without_confirm_and_chains_with_it() {
    let dir = tempfile::tempdir().unwrap();
    let chain = Arc::new(ChainManager::new(0, 1000));
    let rt = boot(dir.path(), &chain, &mesh_cfg(Some(NONCE_A)));
    assert!(!call(&rt, posture(), "workload.node.reset-floor", json!({})).await.ok, "no binding yet");
    assert!(call(&rt, posture(), "workload.node.bind", bind_params(&record(&mesh_of(NONCE_A), 1, BindState::Bound))).await.ok);
    let r = call(&rt, posture(), "workload.node.reset-floor", json!({})).await;
    assert!(r.ok, "{:?}", r.error);
    let v = r.result.unwrap();
    assert_eq!(v["applied"], false);
    assert!(v["preview"]["revived"].is_array());
    assert!(!chain_kinds(&chain).contains(&"licence.floor_reset".to_owned()), "a preview changes nothing");
    let e = call(&rt, posture(), "workload.node.reset-floor", json!({"confirm": true})).await;
    assert!(err_of(&e).contains("'floor'"), "a confirm must name the floor");
    let floor = v["preview"]["floor"].as_u64().unwrap();
    let r = call(&rt, posture(), "workload.node.reset-floor", json!({"confirm": true, "floor": floor})).await;
    assert_eq!(r.result.unwrap()["applied"], true);
    let kinds = chain_kinds(&chain);
    assert!(kinds.contains(&"licence.floor_reset_requested".to_owned()) && kinds.contains(&"licence.floor_reset".to_owned()), "{kinds:?}");
}

#[tokio::test]
async fn confirm_with_a_stale_floor_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let chain = Arc::new(ChainManager::new(0, 1000));
    let rt = boot(dir.path(), &chain, &mesh_cfg(Some(NONCE_A)));
    assert!(call(&rt, posture(), "workload.node.bind", bind_params(&record(&mesh_of(NONCE_A), 1, BindState::Bound))).await.ok);
    let shown = call(&rt, posture(), "workload.node.reset-floor", json!({})).await.result.unwrap();
    let floor = shown["preview"]["floor"].as_u64().unwrap();
    let e = call(&rt, posture(), "workload.node.reset-floor", json!({"confirm": true, "floor": floor + 1})).await;
    assert!(err_of(&e).contains("floor_changed"), "{e:?}");
    assert!(!chain_kinds(&chain).contains(&"licence.floor_reset".to_owned()));
    assert!(call(&rt, posture(), "workload.node.reset-floor", json!({"confirm": true, "floor": floor})).await.ok);
}

#[tokio::test]
async fn unbind_works_under_any_posture_and_a_rogue_signer_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let chain = Arc::new(ChainManager::new(0, 1000));
    let rt = boot(dir.path(), &chain, &mesh_cfg(Some(NONCE_A)));
    assert!(call(&rt, posture(), "workload.node.bind", bind_params(&record(&mesh_of(NONCE_A), 1, BindState::Bound))).await.ok);
    let u = record(&mesh_of(NONCE_A), 2, BindState::Unbound);
    let rogue = sign_binding(&u, &sk(44)).unwrap();
    let e = call(&rt, posture(), "workload.node.unbind", json!({"signed": rogue})).await;
    assert!(err_of(&e).contains("untrusted_operator"), "{e:?}");
    assert!(rt.store().active_binding().is_some());
    // Non-enforce, open membership, no verdict source: turning the licence off still works.
    let lax = AdmissionPosture { enforce: false, verdict_source_bound: false, open_membership: true };
    let r = call(&rt, lax, "workload.node.unbind", json!({"signed": signed(&u)})).await;
    assert!(r.ok, "{:?}", r.error);
    assert_eq!(r.result.unwrap()["save_pending"], false);
    assert!(rt.store().active_binding().is_none());
}

#[tokio::test]
async fn a_corrupt_replay_file_refuses_bind_and_is_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join(clawft_kernel::workload_runtime::seed_bind::BIND_STATE_FILE);
    std::fs::write(&f, b"{ not json").unwrap();
    let chain = Arc::new(ChainManager::new(0, 1000));
    let rt = boot(dir.path(), &chain, &mesh_cfg(Some(NONCE_A)));
    let e = call(&rt, posture(), "workload.node.bind", bind_params(&record(&mesh_of(NONCE_A), 1, BindState::Bound))).await;
    assert!(err_of(&e).contains("bind state unreadable"), "{e:?}");
    assert_eq!(std::fs::read(&f).unwrap(), b"{ not json", "never overwritten");
    assert!(rt.store().held_binding().is_none());
}

#[tokio::test]
async fn a_poisoned_store_is_a_flag_not_an_error_string_and_a_bad_nonce_with_a_binding_is_chained() {
    let dir = tempfile::tempdir().unwrap();
    let chain = Arc::new(ChainManager::new(0, 1000));
    let rt = boot(dir.path(), &chain, &mesh_cfg(Some(NONCE_A)));
    assert!(call(&rt, posture(), "workload.node.bind", bind_params(&record(&mesh_of(NONCE_A), 1, BindState::Bound))).await.ok);
    let bad = boot(dir.path(), &chain, &mesh_cfg(Some("zz")));
    assert!(chain_kinds(&chain).contains(&"licence.mesh_config_error".to_owned()));
    assert!(
        licence_boot::status(&bad)["config_error"]
            .as_str()
            .is_some()
    );

    let dir2 = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir2.path().join("licence")).unwrap();
    std::fs::write(dir2.path().join("licence/checkout_grants.json"), b"garbage").unwrap();
    let p = boot(dir2.path(), &chain, &mesh_cfg(Some(NONCE_A)));
    let st = licence_boot::status(&p);
    assert_eq!(st["poisoned"], true);
    assert!(!st.to_string().contains(dir2.path().to_str().unwrap()), "no path in the status");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_tick_runs_repeatedly_and_survives_a_panic() {
    let n = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let n2 = n.clone();
    let h = licence_boot::spawn_tick(
        Duration::from_millis(10),
        Arc::new(move || {
            if n2.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                panic!("first tick panics");
            }
        }),
    );
    // Wait on the count, not on a fixed sleep: under load the 10 ms ticks and
    // their blocking calls can be late by any amount, but they do arrive.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while n.load(std::sync::atomic::Ordering::SeqCst) < 3 && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    h.abort();
    assert!(n.load(std::sync::atomic::Ordering::SeqCst) >= 3, "kept ticking after the panic");
}

#[test]
fn the_three_methods_are_served_here_and_bind_and_unbind_are_admin_only() {
    for m in METHODS {
        assert!(handles(m), "{m}");
    }
    let (anon, write, admin) = (
        CallerCapabilities::anonymous(),
        CallerCapabilities::from_scopes(["write"]),
        CallerCapabilities::from_scopes(["admin"]),
    );
    for m in ["workload.node.bind", "workload.node.unbind", "workload.node.reset-floor"] {
        assert_eq!(required_capability(m), Capability::Admin, "{m}");
        assert!(!anon.allows_method(m) && !write.allows_method(m), "{m} must need admin");
        assert!(admin.allows_method(m), "{m}");
    }
    assert_eq!(required_capability("workload.node.binding"), Capability::Read);
    assert!(anon.allows_method("workload.node.binding"));
}

// ── bind and unbind are flooded at once ─────────────────────────

/// A licence exchange over `rt`'s store, on its own in-process mesh runtime.
/// Catch-up sync is off (no sync on connect, the 30 min period never comes),
/// so anything a peer learns came by flood.
fn exchange_over(
    rt: &LicenceRuntime,
    id: &str,
) -> (Arc<clawft_kernel::mesh_runtime::MeshRuntime>, Arc<clawft_kernel::licence::LicenceExchange>) {
    use clawft_kernel::licence as l;
    let mut anchors = TrustAnchors::default();
    anchors.push_signer("op", &pk(&sk(1)), KeyOrigin::Operator).unwrap();
    let anchors = Arc::new(anchors);
    let store = rt.policy.store().clone();
    let approvals = Arc::new(l::ApprovalStore::open_or_poisoned(
        &rt.dir.join("licence"),
        anchors.clone(),
        store.local_mesh_id().clone(),
    ));
    let mesh = Arc::new(clawft_kernel::mesh_runtime::MeshRuntime::new(id.into()));
    let p = posture();
    let ex = l::LicenceExchange::start(l::LicenceExchangeParts {
        store,
        approvals,
        anchors,
        runtime: mesh.clone(),
        posture: Arc::new(move || p),
        admission: Arc::new(l::CtxAdmission),
        sink: Arc::new(l::ChainLicenceSink::new(rt.chain.clone())),
        config: l::LicenceExchangeConfig { sync_on_connect: false, ..Default::default() },
    });
    (mesh, ex)
}

/// Link two runtimes as admitted full nodes.
fn link_nodes(
    a: (&str, &Arc<clawft_kernel::mesh_runtime::MeshRuntime>),
    b: (&str, &Arc<clawft_kernel::mesh_runtime::MeshRuntime>),
) {
    use clawft_kernel::mesh_admit::PeerClass;
    use clawft_kernel::mesh_delivery::PeerCtx;
    let (tx_ab, rx_ab) = tokio::sync::mpsc::channel::<Vec<u8>>(256);
    let (tx_ba, rx_ba) = tokio::sync::mpsc::channel::<Vec<u8>>(256);
    for (to, from, mut rx, back) in [
        (b.1.clone(), a.0.to_owned(), rx_ab, tx_ba.clone()),
        (a.1.clone(), b.0.to_owned(), rx_ba, tx_ab.clone()),
    ] {
        tokio::spawn(async move {
            let ctx = PeerCtx {
                peer_id: from,
                node_verified: true,
                class: PeerClass::Node,
                remote_static: None,
                src_scope: None,
            };
            while let Some(bytes) = rx.recv().await {
                let _ = to.handle_incoming_peer(&bytes, back.clone(), Some(&ctx)).await;
            }
        });
    }
    let tally = clawft_kernel::mesh_runtime::RouteTally::default();
    assert!(a.1.register_authenticated_as(b.0.into(), tx_ab, true, PeerClass::Node, &tally));
    assert!(b.1.register_authenticated_as(a.0.into(), tx_ba, true, PeerClass::Node, &tally));
}

async fn eventually(what: &str, mut ok: impl FnMut() -> bool) {
    for _ in 0..400 {
        if ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test]
async fn a_member_learns_a_bind_and_an_unbind_at_once_by_flood() {
    let (sd, md) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let steward = boot(sd.path(), &Arc::new(ChainManager::new(0, 1000)), &mesh_cfg(Some(NONCE_A)));
    let member = boot(md.path(), &Arc::new(ChainManager::new(0, 1000)), &mesh_cfg(Some(NONCE_A)));
    let (s_mesh, s_ex) = exchange_over(&steward, "node-steward");
    let (m_mesh, _m_ex) = exchange_over(&member, "node-member");
    link_nodes(("node-steward", &s_mesh), ("node-member", &m_mesh));
    let ctx = |rt| Ctx { rt, posture: posture(), now: NOW, seed: &lookup, exchange: Some(s_ex.clone()) };

    let b1 = record(&mesh_of(NONCE_A), 1, BindState::Bound);
    assert!(super::route(&ctx(&steward), "workload.node.bind", bind_params(&b1)).await.ok);
    eventually("the member to hold the bind", || member.policy.store().active_binding().is_some()).await;

    let u = record(&mesh_of(NONCE_A), 2, BindState::Unbound);
    let r = super::route(&ctx(&steward), "workload.node.unbind", json!({"signed": signed(&u)})).await;
    assert!(r.ok, "{:?}", r.error);
    eventually("the member to hold the unbind", || {
        member.policy.store().binding_status() == Err(clawft_kernel::licence::LicenceError::Unbound)
    })
    .await;
    assert!(member.policy.store().active_binding().is_none(), "checkout is off on the member");
}

// ── licence_boot's tick persists the floor ──────────────────────

#[tokio::test]
async fn the_boot_store_tick_persists_the_clock_floor() {
    use clawft_kernel::licence::{CheckoutGrant, GrantArtifact, LicenceRef, sign_grant};
    let dir = tempfile::tempdir().unwrap();
    let chain = Arc::new(ChainManager::new(0, 1000));
    let rt = boot(dir.path(), &chain, &mesh_cfg(Some(NONCE_A)));
    let mesh = mesh_of(NONCE_A);
    assert!(call(&rt, posture(), "workload.node.bind", bind_params(&record(&mesh, 1, BindState::Bound))).await.ok);
    // The floor records the clock only once a grant has been accepted.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let grant = CheckoutGrant {
        v: 1,
        grant_id: String::new(),
        mesh_id: mesh.to_hex(),
        seed_device_id: DEVICE.into(),
        grant_key_id: String::new(),
        source: "cognitum".into(),
        registry: "registry.example".into(),
        cog_id: "probe".into(),
        version: "1.0.0".into(),
        artifacts: vec![GrantArtifact {
            arch: "x86_64".into(),
            size: 1,
            sha256: hex_encode(&[1; 32]),
            blake3: hex_encode(&[2; 32]),
        }],
        manifest_sha256: hex_encode(&[3; 32]),
        licence: LicenceRef { ref_sha256: hex_encode(&[4; 32]), expires: now + 86_400 },
        seq: 1,
        issued_at: now,
        expires_at: now + 3600,
    };
    rt.store().accept_grant(&sign_grant(&grant, &sk(2)).unwrap()).unwrap();
    let file = dir.path().join("licence").join("checkout_grants.json");
    let hw = |p: &Path| -> u64 {
        let v: Value = serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap();
        v["floors"].as_object().and_then(|m| m.values().next()).map_or(0, |f| f["hw"].as_u64().unwrap_or(0))
    };
    assert_eq!(hw(&file), 0, "the grant save carries no clock high-water mark");
    let tick = crate::licence_boot::spawn_store_tick(rt.store().clone(), Duration::from_millis(10));
    for _ in 0..200 {
        if hw(&file) >= now {
            tick.abort();
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the boot store tick never persisted the floor");
}
