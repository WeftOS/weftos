//! The ADR-106 run gate in the `workload-host` (phase 3): a Cognitum-origin
//! cog is installed and started only when the gate permits, the refusal
//! reason reaches `--explain` and the chain, and other packages never reach
//! the gate.

use std::sync::{Arc, Mutex};

use clawft_types::placement::TrustTier;
use ed25519_dalek::SigningKey;

use super::host_licence::EVENT_KIND_RUN_PERMIT;
use super::host_service::CtlConfig;
use super::msg::method;
use super::plane::PlacementControlPlane;
use super::plane_place::PlaceOrder;
use super::test_support::*;
use super::transport::MeshConnector;
use crate::artifact_store::ArtifactStore;
use crate::chain::{ChainManager, EVENT_KIND_WORKLOAD_REFUSE};
use crate::licence::{CognitumRunGate, RunPermit, RunRefusal, RunRequest, RunVerdict, sha256_hex};
use crate::mesh_artifact::{ArtifactExchange, ExchangeConfig};
use crate::mesh_swarm_state::{Audience, GrantInfo, RedistributionPolicy};
use crate::node_registry::node_id_from_pubkey;
use crate::workload_pkg::codec::hex_encode;
use crate::workload_runtime::RunMode;

const SCRIPT: &str = "#!/bin/sh\nexec sleep 30\n";
const COGNITUM_URL: &str = "https://cognitum.one/releases/fall-detect";

/// The swarm policy is not under test here: the controller hands its bytes over.
#[derive(Debug)]
struct ServeAll;
impl RedistributionPolicy for ServeAll {
    fn allows(&self, _: &[u8; 32], _: &[GrantInfo], _: &Audience<'_>) -> bool {
        true
    }
}

/// A gate whose answer the test sets, recording what it was asked.
struct TestGate {
    answer: Mutex<Result<RunVerdict, RunRefusal>>,
    asked: Mutex<Vec<(String, String, String, String)>>,
    /// Bytes it claims as Cognitum whatever the package says.
    claimed: Mutex<Option<String>>,
    /// Only this sha256 is permitted, when set.
    only_sha: Mutex<Option<String>>,
}

impl TestGate {
    fn new(answer: Result<RunVerdict, RunRefusal>) -> Arc<Self> {
        Arc::new(Self { answer: Mutex::new(answer), asked: Mutex::default(), claimed: Mutex::default(), only_sha: Mutex::default() })
    }
    fn set(&self, a: Result<RunVerdict, RunRefusal>) {
        *self.answer.lock().unwrap() = a;
    }
}

impl CognitumRunGate for TestGate {
    fn check(&self, r: &RunRequest<'_>) -> Result<RunVerdict, RunRefusal> {
        self.asked.lock().unwrap().push((r.cog_id.into(), r.version.into(), r.sha256.into(), r.blake3.into()));
        if self.only_sha.lock().unwrap().as_deref().is_some_and(|s| s != r.sha256) {
            return Err(RunRefusal::NotInGrant);
        }
        self.answer.lock().unwrap().clone()
    }
    fn claims(&self, sha256: &str, _: &str) -> bool {
        self.claimed.lock().unwrap().as_deref() == Some(sha256)
    }
}

fn permit() -> Result<RunVerdict, RunRefusal> {
    Ok(RunVerdict::Permit(RunPermit { grant_id: "g".repeat(64), approval_id: "a".repeat(64), blake3: "b".repeat(64) }))
}

fn order(pkg: &std::path::Path) -> PlaceOrder {
    PlaceOrder {
        package_dir: pkg.to_path_buf(),
        config: CtlConfig { mode: RunMode::Listener, args: vec![], csi_port: 15016 },
        pin: None,
        prefer: vec![],
        avoid: vec![],
        allow_emulated: false,
        start: true,
        dry_run: false,
        project_id: None,
    }
}

struct Rig {
    plane: PlacementControlPlane,
    host: HostNode,
    gate: Arc<TestGate>,
    _tmp: tempfile::TempDir,
    pkg: std::path::PathBuf,
}

async fn rig(release_url: Option<&str>, answer: Result<RunVerdict, RunRefusal>) -> Rig {
    let tmp = tempfile::tempdir().unwrap();
    let pkg = package_from(tmp.path(), "fall-detect", SCRIPT, &[arch()], release_url);
    let key = SigningKey::from_bytes(&[30; 32]);
    let host = host_node(31, board_caps("pi5"), true, &key);
    let gate = TestGate::new(answer);
    assert!(host.svc.set_licence_gate(gate.clone()));
    let conn = Arc::new(MeshConnector::new(false));
    let addr = conn.register_local("pi", host.svc.clone());
    let chain = Arc::new(ChainManager::new(0, 1000));
    let id = node_id_from_pubkey(&key.verifying_key().to_bytes());
    let cfg = ExchangeConfig { redistribution: Arc::new(ServeAll), ..ExchangeConfig::default() };
    let mut ex = ArtifactExchange::new(&id, Arc::new(ArtifactStore::new_memory()), cfg).unwrap();
    ex.set_chain_manager(chain.clone());
    let plane = PlacementControlPlane::new(key, gate_for(&chain), chain, Arc::new(ex), anchors(), conn);
    plane.add_target(&addr, TrustTier::Paired).await.unwrap();
    Rig { plane, host, gate, _tmp: tmp, pkg }
}

fn gate_for(chain: &Arc<ChainManager>) -> Arc<crate::workload_governance::WorkloadGate> {
    gate(chain)
}

#[tokio::test]
async fn a_cognitum_cog_without_an_approval_is_refused_with_the_reason_in_explain_and_on_the_chain() {
    let r = rig(Some(COGNITUM_URL), Err(RunRefusal::NoApproval)).await;
    let rep = r.plane.place(&order(&r.pkg)).await.unwrap();
    assert!(rep.placed.is_none(), "{}", rep.explain);
    let a = &rep.attempts[0];
    assert_eq!(a.code.as_deref(), Some("governance"));
    let reason = a.reason.clone().unwrap();
    assert!(reason.contains("licence run gate: [no_approval]"), "{reason}");
    assert!(reason.contains("weaver cog checkout approve fall-detect@0.1.0"), "{reason}");
    assert!(rep.explain.contains("[no_approval]"), "{}", rep.explain);
    let refused = events(&r.host.chain, EVENT_KIND_WORKLOAD_REFUSE);
    assert!(refused.iter().any(|(_, p)| p["reason"].as_str().is_some_and(|s| s.contains("[no_approval]"))));
    // The gate was asked with the hashes of the bytes that would run.
    let asked = r.gate.asked.lock().unwrap().clone();
    let want_b3 = hex_encode(blake3::hash(SCRIPT.as_bytes()).as_bytes());
    assert_eq!(asked, vec![("fall-detect".into(), "0.1.0".into(), sha256_hex(SCRIPT.as_bytes()), want_b3)]);
}

#[tokio::test]
async fn a_permitted_cog_runs_and_a_lapse_refuses_its_restart() {
    let r = rig(Some(COGNITUM_URL), permit()).await;
    let rep = r.plane.place(&order(&r.pkg)).await.unwrap();
    let placed = rep.placed.clone().unwrap_or_else(|| panic!("{}", rep.explain));
    let permits = events(&r.host.chain, EVENT_KIND_RUN_PERMIT);
    assert_eq!(permits.len(), 1);
    assert_eq!(permits[0].1["approval_id"], "a".repeat(64));
    assert_eq!(permits[0].1["phase"], "place");

    r.plane.instance(method::STOP, &placed.instance_id).await.unwrap();
    r.gate.set(Err(RunRefusal::GrantLapsed));
    let e = r.plane.instance(method::START, &placed.instance_id).await.unwrap_err();
    assert!(e.to_string().contains("[grant_lapsed]"), "{e}");
    r.gate.set(permit());
    r.plane.instance(method::START, &placed.instance_id).await.unwrap();
    assert_eq!(events(&r.host.chain, EVENT_KIND_RUN_PERMIT).len(), 2);
    r.plane.instance(method::STOP, &placed.instance_id).await.unwrap();
}

#[tokio::test]
async fn other_packages_never_reach_the_gate_and_an_unbound_node_is_not_gated() {
    let r = rig(None, Err(RunRefusal::NoApproval)).await;
    let rep = r.plane.place(&order(&r.pkg)).await.unwrap();
    let placed = rep.placed.clone().unwrap_or_else(|| panic!("{}", rep.explain));
    assert!(r.gate.asked.lock().unwrap().is_empty(), "a signed non-Cognitum package skips the gate");
    r.plane.instance(method::STOP, &placed.instance_id).await.unwrap();

    let r = rig(Some(COGNITUM_URL), Ok(RunVerdict::NotSeedBound)).await;
    let rep = r.plane.place(&order(&r.pkg)).await.unwrap();
    let placed = rep.placed.clone().unwrap_or_else(|| panic!("{}", rep.explain));
    assert_eq!(r.gate.asked.lock().unwrap().len(), 1);
    assert!(events(&r.host.chain, EVENT_KIND_RUN_PERMIT).is_empty());
    r.plane.instance(method::STOP, &placed.instance_id).await.unwrap();
}

/// The real stores behind the gate: a member holding the binding and grant
/// starts the cog only once the operator approval arrives, and not after the
/// artifact hash is revoked.
#[tokio::test]
async fn with_the_real_stores_a_member_runs_the_cog_only_with_grant_and_approval() {
    use crate::licence::{
        AdmissionPosture, Approval, ApprovalStore, BindState, BindingRecord, CheckoutGrant, CheckoutGrantStore,
        GrantArtifact, LicenceRef, LocalMeshId, MeshId, NoExtraChecks, StoreRunGate, sign_approval, sign_binding,
        sign_grant, system_clock,
    };
    use crate::revocation::{RevocationKind, RevocationList};
    use crate::workload_pkg::{KeyOrigin, TrustAnchors};

    let k = |n: u8| SigningKey::from_bytes(&[n; 32]);
    let hex = |s: &SigningKey| hex_encode(&s.verifying_key().to_bytes());
    let now = chrono::Utc::now().timestamp() as u64;
    let dir = tempfile::tempdir().unwrap();
    let mesh = MeshId::derive(&[9; 32], &[7; 32]);
    let mut ops = TrustAnchors::default();
    ops.push_signer("op", &hex(&k(1)), KeyOrigin::Operator).unwrap();
    let ops = Arc::new(ops);
    let local = LocalMeshId::new(mesh);
    let store = Arc::new(CheckoutGrantStore::open(dir.path(), ops.clone(), local.clone(), system_clock()).unwrap());
    let approvals = Arc::new(ApprovalStore::open(dir.path(), ops, local).unwrap());
    let revoked = Arc::new(RevocationList::new(dir.path().join("revoked.json")));
    store.attach_revocations(revoked.clone());
    let rec = BindingRecord {
        v: 2,
        device_id: "seed-1".into(),
        device_pubkey: hex(&k(20)),
        mesh_id: mesh.to_hex(),
        grant_pubkey: hex(&k(2)),
        steward_node_id: "node-steward".into(),
        steward_pubkey: hex(&k(21)),
        state: BindState::Bound,
        seq: 1,
        bound_at: now,
    };
    let posture = AdmissionPosture { enforce: true, verdict_source_bound: true, open_membership: false };
    store.accept_binding(&sign_binding(&rec, &k(1)).unwrap(), posture, &NoExtraChecks).unwrap();
    let (sha, b3) = (sha256_hex(SCRIPT.as_bytes()), hex_encode(blake3::hash(SCRIPT.as_bytes()).as_bytes()));
    let g = CheckoutGrant {
        v: 1,
        grant_id: String::new(),
        mesh_id: mesh.to_hex(),
        seed_device_id: "seed-1".into(),
        grant_key_id: String::new(),
        source: "cognitum".into(),
        registry: "registry.example".into(),
        cog_id: "fall-detect".into(),
        version: "0.1.0".into(),
        artifacts: vec![GrantArtifact { arch: arch().into(), size: SCRIPT.len() as u64, sha256: sha.clone(), blake3: b3.clone() }],
        manifest_sha256: sha256_hex(b"manifest"),
        licence: LicenceRef { ref_sha256: sha256_hex(b"licence"), expires: now + 30 * 86_400 },
        seq: 1,
        issued_at: now,
        expires_at: now + 72 * 3600,
    };
    store.accept_grant(&sign_grant(&g, &k(2)).unwrap()).unwrap();

    let real = StoreRunGate { grants: store.clone(), approvals: Some(approvals.clone()) };
    let tmp = tempfile::tempdir().unwrap();
    let key = SigningKey::from_bytes(&[30; 32]);
    let host = host_node(32, board_caps("pi5"), true, &key);
    assert!(host.svc.set_licence_gate(Arc::new(real)));
    let conn = Arc::new(MeshConnector::new(false));
    let addr = conn.register_local("member", host.svc.clone());
    let chain = Arc::new(ChainManager::new(0, 1000));
    let cfg = ExchangeConfig { redistribution: Arc::new(ServeAll), ..ExchangeConfig::default() };
    let id = node_id_from_pubkey(&key.verifying_key().to_bytes());
    let ex = ArtifactExchange::new(&id, Arc::new(ArtifactStore::new_memory()), cfg).unwrap();
    let plane = PlacementControlPlane::new(key, gate_for(&chain), chain, Arc::new(ex), anchors(), conn);
    plane.add_target(&addr, TrustTier::Paired).await.unwrap();
    let pkg = package_from(tmp.path(), "fall-detect", SCRIPT, &[arch()], Some(COGNITUM_URL));

    // Grant only: refused.
    let rep = plane.place(&order(&pkg)).await.unwrap();
    assert!(rep.placed.is_none());
    assert!(rep.attempts[0].reason.as_deref().unwrap_or("").contains("[no_approval]"), "{}", rep.explain);

    // The operator approval arrives: the member runs it.
    let a = Approval { v: 1, mesh_id: mesh.to_hex(), cog_id: "fall-detect".into(), version: "0.1.0".into(), sha256: vec![sha], approved_at: now };
    approvals.accept(&sign_approval(&a, &k(1)).unwrap()).unwrap();
    let rep = plane.place(&order(&pkg)).await.unwrap();
    let placed = rep.placed.clone().unwrap_or_else(|| panic!("{}", rep.explain));
    let permit = &events(&host.chain, EVENT_KIND_RUN_PERMIT)[0].1;
    assert_eq!(permit["approval_id"], a.content_key().as_str());

    // Revoking the artifact hash refuses the restart.
    plane.instance(method::STOP, &placed.instance_id).await.unwrap();
    revoked.revoke_subject(RevocationKind::ArtifactHash, &b3, "withdrawn").unwrap();
    let e = plane.instance(method::START, &placed.instance_id).await.unwrap_err();
    assert!(e.to_string().contains("[hash_revoked]"), "{e}");
}


fn other_arch() -> &'static str {
    if arch() == "aarch64" { "x86_64" } else { "aarch64" }
}

fn ctl_req() -> super::msg::CtlRequest {
    super::msg::CtlRequest {
        version: super::msg::CTL_VERSION,
        method: method::PLACE.into(),
        requester: "controller".into(),
        target: "node".into(),
        nonce: "0".repeat(32),
        issued_at_ms: 0,
        expires_at_ms: 0,
        decision_id: Some("d".repeat(64)),
        body: serde_json::Value::Null,
    }
}

fn verified(pkg: &std::path::Path) -> (crate::workload_pkg::VerifiedPackage, crate::workload_runtime::VerifiedWorkload) {
    let vp = crate::workload_pkg::verify_dir(pkg, &anchors(), &crate::workload_pkg::VerifyPolicy::default()).unwrap();
    let w = crate::workload_runtime::VerifiedWorkload::from_package(&vp, &crate::workload_pkg::DirSource::new(pkg)).unwrap();
    (vp, w)
}

/// A minimal little-endian ELF64 header for `arch` (enough for admission).
fn elf(arch: &str) -> Vec<u8> {
    let m = crate::workload_runtime::native::elf_machine(arch).unwrap().to_le_bytes();
    let mut b = vec![0x7f, b'E', b'L', b'F', 2, 1, 1];
    b.resize(18, 0);
    b.extend_from_slice(&m);
    b.resize(64, 0);
    // Raw bytes: e_machine for aarch64 is 0xB7, which a text round trip would
    // write as the UTF-8 pair C2 B7.
    b
}

/// The runtime's admission picks the arch: a placement whose variant names
/// another arch is refused, for native (host arch) and container (its arch
/// order) alike, and every binary of the package is asked about.
#[tokio::test]
async fn the_gate_hashes_the_binary_the_runtime_admits_and_refuses_another_variant_arch() {
    // Native: the host arch runs; a variant for the other arch is refused.
    let tmp = tempfile::tempdir().unwrap();
    let pkg = package_from(tmp.path(), "fall-detect", SCRIPT, &[arch(), other_arch()], Some(COGNITUM_URL));
    let (vp, w) = verified(&pkg);
    let key = SigningKey::from_bytes(&[30; 32]);
    let node = host_node(33, board_caps("pi5"), true, &key);
    let gate = TestGate::new(permit());
    node.svc.set_licence_gate(gate.clone());
    let native = node.svc.routes["native"].clone();
    let e = node.svc
        .licence_check_place(&vp, &w, &format!("{}-native", other_arch()), &native, &ctl_req())
        .await
        .unwrap_err();
    assert!(e.reason.contains("[arch_mismatch]"), "{}", e.reason);
    let run = node.svc
        .licence_check_place(&vp, &w, &format!("{}-native", arch()), &native, &ctl_req())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(run.arch, arch());
    assert_eq!(gate.asked.lock().unwrap().len(), 1, "only the binary that runs needs a grant");

    // Container: it runs the first arch of its own order; another variant arch is refused.
    let tmp = tempfile::tempdir().unwrap();
    let runs = other_arch();
    let pkg = package_from(tmp.path(), "fall-detect", &elf(runs), &[arch(), runs], Some(COGNITUM_URL));
    let (vp, w) = verified(&pkg);
    let chain = Arc::new(ChainManager::new(0, 1000));
    let wg = gate_for(&chain);
    let mut cfg = crate::workload_runtime::ContainerRuntimeConfig::new(
        crate::workload_runtime::Engine::Docker,
        format!("debian@sha256:{}", "0".repeat(64)),
        tmp.path().join("ctx"),
    );
    cfg.arches_native = vec![runs.to_string()];
    let rt = crate::workload_runtime::ContainerRuntime::new(cfg, Arc::new(NoEngine));
    let host = Arc::new(crate::workload_runtime::WorkloadHost::new(
        Arc::new(rt), wg.clone(), "c", crate::workload_governance::NodeTrustTier::Paired,
    ));
    let svc = super::host_service::WorkloadHostService::new(SigningKey::from_bytes(&[34; 32]), exchange("c", &chain), anchors(), wg)
        .with_route("container", host.clone());
    svc.set_licence_gate(TestGate::new(permit()));
    let e = svc.licence_check_place(&vp, &w, &format!("{}-container", arch()), &host, &ctl_req()).await.unwrap_err();
    assert!(e.reason.contains("[arch_mismatch]") && e.reason.contains(runs), "{}", e.reason);
    let run = svc.licence_check_place(&vp, &w, &format!("{runs}-container"), &host, &ctl_req()).await.unwrap().unwrap();
    assert_eq!(run.arch, runs);
}

/// A package that does not say Cognitum but carries bytes a held grant lists
/// is gated all the same.
#[tokio::test]
async fn bytes_a_grant_lists_are_gated_whatever_the_package_says() {
    let r = rig(None, Err(RunRefusal::NoGrant)).await;
    *r.gate.claimed.lock().unwrap() = Some(sha256_hex(SCRIPT.as_bytes()));
    let rep = r.plane.place(&order(&r.pkg)).await.unwrap();
    assert!(rep.placed.is_none());
    assert!(rep.attempts[0].reason.as_deref().unwrap_or("").contains("[no_grant]"), "{}", rep.explain);
    assert_eq!(r.gate.asked.lock().unwrap()[0].0, "fall-detect", "checked under the package's own id");
}

/// At start the staged file is rehashed: bytes changed on disk after the
/// load are refused.
#[tokio::test]
async fn a_start_rehashes_the_staged_file_on_disk() {
    let r = rig(Some(COGNITUM_URL), permit()).await;
    *r.gate.only_sha.lock().unwrap() = Some(sha256_hex(SCRIPT.as_bytes()));
    let rep = r.plane.place(&order(&r.pkg)).await.unwrap();
    let placed = rep.placed.clone().unwrap_or_else(|| panic!("{}", rep.explain));
    r.plane.instance(method::STOP, &placed.instance_id).await.unwrap();
    let (host, handle) = {
        let map = r.host.svc.instances.lock().await;
        let p = &map[&placed.instance_id];
        (r.host.svc.routes[&p.route].clone(), p.handle.clone())
    };
    let path = host.runtime().staged_payload(&handle).await.expect("native stages a file");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(&path, "#!/bin/sh\necho swapped\n").unwrap();
    let e = r.plane.instance(method::START, &placed.instance_id).await.unwrap_err();
    assert!(e.to_string().contains("[not_in_grant]"), "{e}");
}

/// An ArtifactHash revocation of a licensed cog's binary tears the running
/// instance down (the existing revocation sweep covers it).
#[tokio::test]
async fn revoking_a_licensed_binary_tears_its_running_instance_down() {
    let r = rig(Some(COGNITUM_URL), permit()).await;
    let rep = r.plane.place(&order(&r.pkg)).await.unwrap();
    let placed = rep.placed.clone().unwrap_or_else(|| panic!("{}", rep.explain));
    let dir = tempfile::tempdir().unwrap();
    let list = crate::revocation::RevocationList::new(dir.path().join("revoked.json"));
    let b3 = hex_encode(blake3::hash(SCRIPT.as_bytes()).as_bytes());
    list.revoke_subject(crate::revocation::RevocationKind::ArtifactHash, &b3, "withdrawn").unwrap();
    let forced = r.host.svc.enforce_revocations(&list).await;
    assert_eq!(forced.len(), 1, "{forced:?}");
    assert!(!r.host.svc.instances.lock().await.contains_key(&placed.instance_id));
}


/// Real stores for one member: bound, with a grant listing exactly `listed`
/// (arch, bytes) and an approval for those sha256s.
fn real_member(listed: &[(&str, &str)]) -> (tempfile::TempDir, Arc<crate::licence::StoreRunGate>, Arc<crate::revocation::RevocationList>) {
    use crate::licence::*;
    use crate::workload_pkg::{KeyOrigin, TrustAnchors};
    let k = |n: u8| SigningKey::from_bytes(&[n; 32]);
    let hex = |s: &SigningKey| hex_encode(&s.verifying_key().to_bytes());
    let now = chrono::Utc::now().timestamp() as u64;
    let dir = tempfile::tempdir().unwrap();
    let mesh = MeshId::derive(&[9; 32], &[7; 32]);
    let mut ops = TrustAnchors::default();
    ops.push_signer("op", &hex(&k(1)), KeyOrigin::Operator).unwrap();
    let ops = Arc::new(ops);
    let local = LocalMeshId::new(mesh);
    let store = Arc::new(CheckoutGrantStore::open(dir.path(), ops.clone(), local.clone(), system_clock()).unwrap());
    let approvals = Arc::new(ApprovalStore::open(dir.path(), ops, local).unwrap());
    let revoked = Arc::new(crate::revocation::RevocationList::new(dir.path().join("revoked.json")));
    store.attach_revocations(revoked.clone());
    let rec = BindingRecord {
        v: 2, device_id: "seed-1".into(), device_pubkey: hex(&k(20)), mesh_id: mesh.to_hex(),
        grant_pubkey: hex(&k(2)), steward_node_id: "node-steward".into(), steward_pubkey: hex(&k(21)),
        state: BindState::Bound, seq: 1, bound_at: now,
    };
    let posture = AdmissionPosture { enforce: true, verdict_source_bound: true, open_membership: false };
    store.accept_binding(&sign_binding(&rec, &k(1)).unwrap(), posture, &NoExtraChecks).unwrap();
    let artifacts = listed
        .iter()
        .map(|(a, b)| GrantArtifact {
            arch: (*a).into(), size: b.len() as u64, sha256: sha256_hex(b.as_bytes()),
            blake3: hex_encode(blake3::hash(b.as_bytes()).as_bytes()),
        })
        .collect::<Vec<_>>();
    let mut shas: Vec<String> = artifacts.iter().map(|a| a.sha256.clone()).collect();
    shas.sort();
    let g = CheckoutGrant {
        v: 1, grant_id: String::new(), mesh_id: mesh.to_hex(), seed_device_id: "seed-1".into(),
        grant_key_id: String::new(), source: "cognitum".into(), registry: "registry.example".into(),
        cog_id: "fall-detect".into(), version: "0.1.0".into(), artifacts,
        manifest_sha256: sha256_hex(b"manifest"),
        licence: LicenceRef { ref_sha256: sha256_hex(b"licence"), expires: now + 30 * 86_400 },
        seq: 1, issued_at: now, expires_at: now + 72 * 3600,
    };
    store.accept_grant(&sign_grant(&g, &k(2)).unwrap()).unwrap();
    let a = Approval { v: 1, mesh_id: mesh.to_hex(), cog_id: "fall-detect".into(), version: "0.1.0".into(), sha256: shas, approved_at: now };
    approvals.accept(&sign_approval(&a, &k(1)).unwrap()).unwrap();
    (dir, Arc::new(StoreRunGate { grants: store, approvals: Some(approvals) }), revoked)
}

/// A multi-arch package with only the running arch checked out and approved
/// is admitted; a revoked binary of another arch still refuses it.
#[tokio::test]
async fn a_multi_arch_package_needs_a_grant_only_for_the_binary_that_runs() {
    const OTHER: &str = "#!/bin/sh\necho other arch\n";
    let (_d, real, revoked) = real_member(&[(arch(), SCRIPT)]);
    let tmp = tempfile::tempdir().unwrap();
    let pkg = package_bins(tmp.path(), "fall-detect", &[(arch(), SCRIPT), (other_arch(), OTHER)], Some(COGNITUM_URL));
    let (vp, w) = verified(&pkg);
    let key = SigningKey::from_bytes(&[30; 32]);
    let node = host_node(35, board_caps("pi5"), true, &key);
    node.svc.set_licence_gate(real);
    let native = node.svc.routes["native"].clone();
    let variant = format!("{}-native", arch());
    let run = node.svc.licence_check_place(&vp, &w, &variant, &native, &ctl_req()).await.expect("admitted").unwrap();
    assert_eq!(run.arch, arch());
    revoked
        .revoke_subject(crate::revocation::RevocationKind::ArtifactHash, &hex_encode(blake3::hash(OTHER.as_bytes()).as_bytes()), "bad")
        .unwrap();
    let e = node.svc.licence_check_place(&vp, &w, &variant, &native, &ctl_req()).await.unwrap_err();
    assert!(e.reason.contains("[hash_revoked]") && e.reason.contains(other_arch()), "{}", e.reason);
}
