//! In-process nodes for control-plane tests: real signed packages, real
//! signed facts, real native adapters under `WorkloadGate`, and the real
//! wire (envelopes + artifact frames over in-memory streams). Every node
//! gets its own in-memory chain (never the operator's chain.rvf).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use clawft_types::placement::{AttrValue, Capability, CapabilityId, NodeFacts, Provenance};
use ed25519_dalek::SigningKey;
use serde_json::Value;

use crate::artifact_store::ArtifactStore;
use crate::chain::ChainManager;
use crate::mesh_artifact::{ArtifactExchange, ExchangeConfig};
use crate::node_facts_advert::sign_node_facts;
use crate::node_registry::node_id_from_pubkey;
use crate::workload_governance::{NetworkPolicy, WorkloadGate, WorkloadPermitRule};
use crate::workload_pkg::codec::hex_encode;
use crate::workload_pkg::{
    CogPackInput, KeyOrigin, PackageSource, TrustAnchors, key_id_for, pack_cog, sign_envelope,
    write_manifest,
};
use crate::workload_runtime::native::host_arch;
use crate::workload_runtime::container_cmd::CmdOutput;
use crate::workload_runtime::{
    CommandRunner, ContainerRuntime, ContainerRuntimeConfig, Engine, NativeConfig, NativeRuntime,
    RuntimeError, WorkloadHost,
};

use super::host_service::WorkloadHostService;
use super::plane::PlacementControlPlane;
use super::transport::CtlConnector;

/// The package signer (operator key) every node pins.
pub fn signer() -> SigningKey {
    SigningKey::from_bytes(&[42; 32])
}

pub fn anchors() -> TrustAnchors {
    let pk = signer().verifying_key().to_bytes();
    let mut a = TrustAnchors::default();
    a.push_signer(&key_id_for(&pk), &hex_encode(&pk), KeyOrigin::Operator)
        .unwrap();
    a
}

/// This host's arch (the arch the test package ships and nodes advertise).
pub fn arch() -> &'static str {
    host_arch().expect("tests run on a supported arch")
}

/// A signed cog package whose binary (for this host's arch) is `script`.
pub fn package(root: &Path, id: &str, script: &str, arches: &[&str]) -> PathBuf {
    package_from(root, id, script, arches, None)
}

/// [`package`] with a release URL (one naming `cognitum` gives the package
/// Cognitum origin, ADR-106).
pub fn package_from(root: &Path, id: &str, script: &str, arches: &[&str], release_url: Option<&str>) -> PathBuf {
    let per: Vec<(&str, &str)> = arches.iter().map(|a| (*a, script)).collect();
    package_bins(root, id, &per, release_url)
}

/// [`package_from`] with its own bytes per arch.
pub fn package_bins(root: &Path, id: &str, bins_in: &[(&str, &str)], release_url: Option<&str>) -> PathBuf {
    package_toml(root, id, bins_in, release_url, "")
}

/// [`package_bins`] with `extra` appended to `cog.toml` (`[[outputs]]`, ...).
pub fn package_toml(root: &Path, id: &str, bins_in: &[(&str, &str)], release_url: Option<&str>, extra: &str) -> PathBuf {
    let same = bins_in.windows(2).all(|w| w[0].1 == w[1].1);
    let src = root.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("cog.toml"),
        format!("[cog]\nid = \"{id}\"\nname = \"T\"\nversion = \"0.1.0\"\n{extra}"),
    )
    .unwrap();
    let mut bins = Vec::new();
    for (a, script) in bins_in {
        let p = root.join(format!("bin-{a}"));
        std::fs::write(&p, script).unwrap();
        bins.push((a.to_string(), p));
    }
    // With the same bytes on every arch one provenance covers them all;
    // different bytes are packed without one (and not redistributable).
    if let Some((_, first)) = bins.first().filter(|_| same) {
        crate::workload_pkg::write_source_build_provenance(&src, first).unwrap();
    }
    let input = CogPackInput {
        cog_dir: src,
        binaries: bins,
        source: PackageSource {
            repo: Some("cogs-fork".into()),
            commit: Some("0000000".into()),
            release_url: release_url.map(str::to_owned),
        },
        cognitum_record: None,
        redistributable: same,
        provenance: None,
        allow_no_provenance: !same,
    };
    let pkg = root.join("pkg");
    let mut env = pack_cog(&input, &pkg).unwrap();
    let k = signer();
    sign_envelope(&mut env, &k, &key_id_for(&k.verifying_key().to_bytes())).unwrap();
    write_manifest(&pkg, &env).unwrap();
    pkg
}

pub fn cap(id: &str) -> Capability {
    Capability::new(CapabilityId::new(id).unwrap(), Provenance::Probed)
}

pub fn list(items: &[&str]) -> AttrValue {
    AttrValue::List(items.iter().map(|s| AttrValue::from(*s)).collect())
}

/// Facts of a Linux ARM-class board (a Pi 5).
pub fn board_caps(class: &str) -> Vec<Capability> {
    vec![
        cap(&format!("cpu.arch.{}", arch())),
        cap("os.linux"),
        cap(&format!("node.class.{class}")),
        cap("runtime.native").with_attr("arches_native", list(&[arch()])),
        cap("mem.system").with_attr("free", 4i64 << 30),
    ]
}

/// Facts of the dev Mac (as the macOS probe reports it with OrbStack).
pub fn mac_caps() -> Vec<Capability> {
    vec![
        cap(&format!("cpu.arch.{}", arch())),
        cap("os.macos"),
        cap("node.class.dev-mac"),
        cap("runtime.native").with_attr("arches_native", list(&[arch()])),
        cap("runtime.container.docker")
            .with_attr("arches_native", list(&[arch()]))
            .with_attr("arches_emulated", list(&["armv7", "x86_64"])),
        cap("mem.unified").with_attr("free", 64i64 << 30),
    ]
}

pub fn gate(chain: &Arc<ChainManager>) -> Arc<WorkloadGate> {
    let mut permit = WorkloadPermitRule::new("test-cog", ["workload.*"], ["cog"]);
    permit.max_network = NetworkPolicy::Egress;
    Arc::new(
        WorkloadGate::exempt(0.95, false, "test")
            .with_permit(permit)
            .unwrap()
            .with_chain(chain.clone()),
    )
}

/// A gate that permits cogs but whose subject revocation list in `dir` is
/// unreadable, so every request fails closed (a fault of the node's files).
pub fn gate_unreadable_list(chain: &Arc<ChainManager>, dir: &Path) -> Arc<WorkloadGate> {
    std::fs::write(dir.join(crate::revocation::SUBJECTS_FILE_NAME), "[{").unwrap();
    let list = Arc::new(crate::revocation::RevocationList::load(dir.join("revoked_hosts.json")));
    let mut permit = WorkloadPermitRule::new("test-cog", ["workload.*"], ["cog"]);
    permit.max_network = NetworkPolicy::Egress;
    Arc::new(
        WorkloadGate::exempt(0.95, false, "test")
            .with_revocations(list)
            .with_permit(permit)
            .unwrap()
            .with_chain(chain.clone()),
    )
}

/// A gate with no permit for cogs: every placement is denied (a verdict).
pub fn gate_no_permit(chain: &Arc<ChainManager>) -> Arc<WorkloadGate> {
    Arc::new(WorkloadGate::exempt(0.95, false, "test").with_chain(chain.clone()))
}

pub fn exchange(id: &str, chain: &Arc<ChainManager>) -> Arc<ArtifactExchange> {
    let mut ex = ArtifactExchange::new(
        id,
        Arc::new(ArtifactStore::new_memory()),
        ExchangeConfig::default(),
    )
    .unwrap();
    ex.set_chain_manager(chain.clone());
    Arc::new(ex)
}

/// A container engine that is not there (tests never run a real engine):
/// every CLI call fails, so a dispatched container variant is refused by
/// the adapter itself.
pub struct NoEngine;

#[async_trait::async_trait]
impl CommandRunner for NoEngine {
    async fn run(
        &self,
        program: &str,
        _: &[String],
        _: std::time::Duration,
        _: usize,
    ) -> Result<CmdOutput, RuntimeError> {
        Err(RuntimeError::Backend(format!("{program}: no engine in tests")))
    }
}

/// One node's `workload-host`.
pub struct HostNode {
    pub svc: Arc<WorkloadHostService>,
    pub chain: Arc<ChainManager>,
    pub id: String,
    pub _tmp: tempfile::TempDir,
}

/// A node with `caps`, a native adapter (`scripts` allows interpreted
/// payloads), accepting requests from `controller`.
pub fn host_node(
    seed: u8,
    caps: Vec<Capability>,
    scripts: bool,
    controller: &SigningKey,
) -> HostNode {
    host_node_with(seed, caps, scripts, controller, gate)
}

/// [`host_node`] with its own governance (`make_gate` gets the node's chain).
pub fn host_node_with(
    seed: u8,
    caps: Vec<Capability>,
    scripts: bool,
    controller: &SigningKey,
    make_gate: impl FnOnce(&Arc<ChainManager>) -> Arc<WorkloadGate>,
) -> HostNode {
    let container = caps
        .iter()
        .any(|c| c.id.as_str().starts_with("runtime.container"));
    host_node_routes(seed, caps, scripts, controller, make_gate, container, None, None)
}

/// [`host_node`] serving only its native adapter, whatever its facts say.
pub fn host_node_native_only(
    seed: u8,
    caps: Vec<Capability>,
    scripts: bool,
    controller: &SigningKey,
) -> HostNode {
    host_node_routes(seed, caps, scripts, controller, gate, false, None, None)
}

/// [`host_node_native_only`] with the ingest bridge wired in.
pub fn host_node_ingest(
    seed: u8,
    caps: Vec<Capability>,
    controller: &SigningKey,
    ingest: crate::cog_ingest::IngestHooks,
) -> HostNode {
    host_node_routes(seed, caps, true, controller, gate, false, Some(ingest), None)
}

/// [`host_node`] with the opt-in controller lease.
pub fn host_node_leased(
    seed: u8,
    caps: Vec<Capability>,
    controller: &SigningKey,
    lease: std::time::Duration,
) -> HostNode {
    host_node_routes(seed, caps, true, controller, gate, false, None, Some(lease))
}

#[allow(clippy::too_many_arguments)]
fn host_node_routes(
    seed: u8,
    caps: Vec<Capability>,
    scripts: bool,
    controller: &SigningKey,
    make_gate: impl FnOnce(&Arc<ChainManager>) -> Arc<WorkloadGate>,
    container: bool,
    ingest: Option<crate::cog_ingest::IngestHooks>,
    lease: Option<std::time::Duration>,
) -> HostNode {
    let key = SigningKey::from_bytes(&[seed; 32]);
    let id = node_id_from_pubkey(&key.verifying_key().to_bytes());
    let chain = Arc::new(ChainManager::new(0, 1000));
    let tmp = tempfile::tempdir().unwrap();
    let gate = make_gate(&chain);
    let native = NativeRuntime::new(NativeConfig {
        root: tmp.path().join("instances"),
        run_as: None,
        allow_interpreted: scripts,
    });
    let host = Arc::new(
        WorkloadHost::new(
            Arc::new(native),
            gate.clone(),
            id.clone(),
            crate::workload_governance::NodeTrustTier::Paired,
        )
        .with_chain(chain.clone()),
    );
    let now = chrono::Utc::now().timestamp() as u64;
    let mut facts = NodeFacts::new(id.clone(), now, 600, 1);
    facts.capabilities = caps.clone();
    let signed = sign_node_facts(&facts, &key).unwrap();
    let mut svc = WorkloadHostService::new(key, exchange(&id, &chain), anchors(), gate.clone())
        .with_route("native", host);
    if container {
        let cfg = ContainerRuntimeConfig::new(
            Engine::Docker,
            format!("debian@sha256:{}", "0".repeat(64)),
            tmp.path().join("containers"),
        );
        let rt = ContainerRuntime::new(cfg, Arc::new(NoEngine));
        let h = WorkloadHost::new(
            Arc::new(rt),
            gate,
            id.clone(),
            crate::workload_governance::NodeTrustTier::Paired,
        )
        .with_chain(chain.clone());
        svc = svc.with_route("container", Arc::new(h));
    }
    if let Some(h) = ingest {
        svc = svc.with_ingest(h);
    }
    if let Some(l) = lease {
        svc = svc.with_lease(l);
    }
    let svc = svc
        .with_controllers(vec![controller.verifying_key().to_bytes()])
        .with_chain(chain.clone());
    svc.set_facts(signed);
    HostNode {
        svc: Arc::new(svc),
        chain,
        id,
        _tmp: tmp,
    }
}

/// The controller with its own chain and gate.
pub fn controller(
    key: &SigningKey,
    connector: Arc<dyn CtlConnector>,
) -> (PlacementControlPlane, Arc<ChainManager>) {
    let chain = Arc::new(ChainManager::new(0, 1000));
    let id = node_id_from_pubkey(&key.verifying_key().to_bytes());
    let plane = PlacementControlPlane::new(
        key.clone(),
        gate(&chain),
        chain.clone(),
        exchange(&id, &chain),
        anchors(),
        connector,
    );
    (plane, chain)
}

/// Payloads of chain events of `kind`.
pub fn events(chain: &ChainManager, kind: &str) -> Vec<(String, Value)> {
    chain
        .tail(chain.len())
        .into_iter()
        .filter(|e| e.kind == kind)
        .map(|e| (e.source, e.payload.unwrap_or(Value::Null)))
        .collect()
}

/// A connector whose nodes can be killed and revived: a dead address
/// refuses every connection, as a powered-off node does.
pub struct Killable {
    inner: super::transport::MeshConnector,
    dead: std::sync::RwLock<std::collections::HashSet<String>>,
}

impl Killable {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: super::transport::MeshConnector::new(false),
            dead: Default::default(),
        })
    }

    /// Serve `svc` in process at `mem://<name>`.
    pub fn serve(&self, name: &str, svc: Arc<WorkloadHostService>) -> String {
        self.inner.register_local(name, svc)
    }

    pub fn kill(&self, addr: &str) {
        self.dead.write().unwrap().insert(addr.to_string());
    }

    pub fn revive(&self, addr: &str) {
        self.dead.write().unwrap().remove(addr);
    }
}

#[async_trait::async_trait]
impl CtlConnector for Killable {
    async fn connect(
        &self,
        addr: &str,
    ) -> Result<Box<dyn crate::mesh::MeshStream>, crate::mesh::MeshError> {
        if self.dead.read().unwrap().contains(addr) {
            return Err(crate::mesh::MeshError::PeerNotConnected(addr.to_string()));
        }
        self.inner.connect(addr).await
    }
}

/// A placement order for `pkg` in `mode`, preferring `prefer` when given.
pub fn mode_order(
    pkg: &Path,
    mode: crate::workload_runtime::RunMode,
    prefer: Option<&str>,
) -> super::plane_place::PlaceOrder {
    super::plane_place::PlaceOrder {
        package_dir: pkg.to_path_buf(),
        config: super::host_service::CtlConfig {
            mode,
            args: vec![],
            csi_port: 15040,
        },
        pin: None,
        prefer: prefer.map(|p| vec![p.to_string()]).unwrap_or_default(),
        avoid: vec![],
        allow_emulated: false,
        start: true,
        dry_run: false,
        project_id: None,
    }
}
