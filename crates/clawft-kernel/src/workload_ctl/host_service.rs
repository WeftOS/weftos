//! The target side: a node's `workload-host` service (ADR-099 section 7).
//!
//! It verifies each signed request, then for `place` / `load`:
//!
//! 1. picks the runtime adapter for the chosen route (`native`,
//!    `container`, ...); no adapter is an admission refusal;
//! 2. fetches the package before loading, over the same connection, with
//!    the artifact piece protocol (card 11) and verifies its signatures
//!    against this node's own trust anchors;
//! 3. rebuilds the workload from the fetched bytes and hands it to the
//!    adapter through [`WorkloadHost`], whose admission self-check (binary
//!    arch, OS, runtime) runs before governance and load (card 09).
//!
//! Every refusal is chained as `workload.refuse` (source
//! [`HOST_CHAIN_SOURCE`]); a placement is chained as `workload.place`.
//! Unknown `workload.*` methods go to the governance gate, which denies
//! and chains them (default deny, ADR-099 section 4).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use ed25519_dalek::SigningKey;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::chain::{self, ChainManager};
use crate::cog_ingest::{IngestError, IngestHooks, IngestLease, InstanceBinding};
use crate::gate::{GateBackend, GateDecision};
use crate::ipc::GlobalPid;
use crate::mesh_artifact::ArtifactExchange;
use crate::mesh_artifact_pkg::PackageExchangeError;
use crate::mesh_artifact_transfer::PeerSet;
use crate::mesh_service_adv::ServiceAdvertisement;
use crate::node_facts_advert::SignedNodeFacts;
use crate::node_registry::node_id_from_pubkey;
use crate::workload_pkg::TrustAnchors;
use crate::workload_pkg::manifest::valid_token;
use crate::workload_pkg::store::StoreSource;
use crate::workload_runtime::{
    HostContract, InstanceHandle, RunEvidence, RunMode, RuntimeError, VerifiedWorkload,
    WorkloadConfig, WorkloadHost,
};

use super::cog_kind::route_of;
use super::host_instances::InFlight;
use super::msg::{
    CTL_VERSION, ControllerPolicy, CtlOutcome, CtlRequest, CtlResponse, NonceGuard, Refusal,
    RefusalCode, SignedCtl, WORKLOAD_HOST_SERVICE, method, verify_request,
};
use super::refusal_budget::RefusalBudget;

/// Chain source for target-side placement records.
pub const HOST_CHAIN_SOURCE: &str = "workload.host";

/// Place-time configuration sent with `place` / `load`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CtlConfig {
    /// Run mode (`once`, `interval`, `listener`).
    pub mode: RunMode,
    /// Extra cog arguments (validated by the adapter).
    #[serde(default)]
    pub args: Vec<String>,
    /// UDP port for the sensor feed (0.0.0.0:<port>).
    #[serde(default = "default_csi_port")]
    pub csi_port: u16,
}

fn default_csi_port() -> u16 {
    crate::workload_runtime::host_contract::DEFAULT_CSI_PORT
}

/// Body of `place` / `load`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlaceBody {
    /// Workload name (cog id).
    pub name: String,
    /// BLAKE3 of the signed manifest (fetch key).
    pub manifest_hash: String,
    /// Chosen variant (`aarch64-native`).
    pub variant: String,
    /// Instance configuration.
    pub config: CtlConfig,
    /// Start after loading (place only).
    #[serde(default)]
    pub start: bool,
    /// Project that placed the workload (ingest bridge destination).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
}

/// Body of the instance verbs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstanceBody {
    /// Instance id (`status` without one lists every instance).
    #[serde(default)]
    pub instance_id: Option<String>,
    /// Stop grace period.
    #[serde(default)]
    pub grace_ms: Option<u64>,
    /// With a listing: also report instances taken down on purpose
    /// (`departed_instance` rows), which a controller needs to tell a
    /// teardown from an instance lost with a restart.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub include_departed: bool,
}

pub(super) struct Placed {
    pub(super) handle: InstanceHandle,
    pub(super) route: String,
    pub(super) name: String,
    pub(super) variant: String,
    pub(super) decision_id: Option<String>,
    pub(super) last: Option<RunEvidence>,
    /// The instance's ingest registration (cogs, when ingest is wired).
    pub(super) ingest: Option<IngestLease>,
    /// The instance's cogbus registration (cogs, when the broker is up and
    /// the route reaches it; see `host_cogbus`).
    pub(super) cogbus: Option<super::host_cogbus::CogbusRegistration>,
    /// `enabled`, `disabled` (bridge down, no token issued) or `none`.
    pub(super) ingest_state: &'static str,
    /// State machine, health misses and restart history (see `host_supervise`).
    pub(super) life: super::lifecycle::InstanceLife,
    /// It runs until stopped (listener or interval mode), so an exit is a
    /// failure, not the end of a one-shot run.
    pub(super) continuous: bool,
    /// The operator wants it running: set by `start`, cleared by `stop`.
    /// A supervisor restarts only what is meant to run.
    pub(super) desired_running: bool,
    /// Stopped because the controller lease expired (see `host_supervise`);
    /// the controller restarts it if it comes back still owning it.
    pub(super) lease_stopped: bool,
    /// Who placed it (a controller's node id). The lease applies only to
    /// instances placed by another node: what this node controls itself
    /// is exempt.
    pub(super) requester: String,
    /// A Cognitum-origin cog in a Seed-bound mesh: the run gate is asked
    /// again on every start (ADR-106, phase 3).
    pub(super) licence: Option<super::host_licence::LicensedRun>,
}

/// Fresh signed facts on demand (a daemon re-probes before the TTL ends).
pub type FactsSource = Arc<dyn Fn() -> Option<SignedNodeFacts> + Send + Sync>;

/// A node's `workload-host`.
pub struct WorkloadHostService {
    pub(super) node_id: String,
    key: SigningKey,
    anchors: TrustAnchors,
    controllers: Box<dyn ControllerPolicy>,
    nonces: NonceGuard,
    pub(super) routes: BTreeMap<String, Arc<WorkloadHost>>,
    exchange: Arc<ArtifactExchange>,
    gate: Arc<dyn GateBackend>,
    chain: Option<Arc<ChainManager>>,
    facts: Mutex<Option<SignedNodeFacts>>,
    facts_source: Option<FactsSource>,
    address: Option<String>,
    pub(super) instances: tokio::sync::Mutex<HashMap<String, Placed>>,
    /// Decision ids of `place` / `load` requests being handled. A
    /// controller that lost a response reconciles against this and the
    /// instance list (see `plane_reconcile`).
    pub(super) in_flight: Mutex<HashSet<String>>,
    /// Bounds chain writes for requests that failed verification.
    verify_budget: RefusalBudget,
    /// Ingest bridge wiring: a token per placed cog, revoked on stop.
    pub(super) ingest: Option<IngestHooks>,
    /// The node's subject revocation list, for the placement race check and
    /// the forced unload (the gate holds its own handle to the same list).
    pub(super) revocations: std::sync::OnceLock<Arc<crate::revocation::RevocationList>>,
    /// Kinds this node accepts for a fetched package and their health
    /// definitions (see `host_supervise`); builtin by default.
    pub(super) kinds: crate::workload_kind::KindRegistry,
    /// Bounds on supervisor restarts.
    pub(super) restart: super::lifecycle::RestartPolicy,
    /// Opt-in node-side lease: continuous instances stop once no other
    /// controller has been heard from for this long (default off).
    pub(super) lease: Option<std::time::Duration>,
    /// Instances taken down on purpose (revoked, unloaded), newest last, so
    /// a controller can tell them from instances lost with a restart (this
    /// list is in memory: a restart empties it, which is the signal).
    pub(super) departed: Mutex<std::collections::VecDeque<(String, &'static str)>>,
    /// Last request from a controller other than this node itself (ms).
    pub(super) last_contact_ms: std::sync::atomic::AtomicU64,
    /// The ADR-106 run gate for Cognitum-origin cogs (unset: not consulted).
    pub(super) licence_gate: std::sync::OnceLock<Arc<dyn crate::licence::CognitumRunGate>>,
    /// Answers the node-admin methods (`dashboard.*`); unset: they are refused.
    pub(super) node_admin: std::sync::OnceLock<Arc<dyn super::host_node_admin::NodeAdmin>>,
    /// The node's cogbus (ADR-122); unset: cogs get no broker socket.
    pub(super) cogbus: std::sync::OnceLock<Arc<dyn super::host_cogbus::CogbusRegistrar>>,
    /// Placed instances persisted across a daemon restart (unset: a restart
    /// empties the host, as before).
    pub(super) journal: Option<super::host_journal::Journal>,
}

pub(super) fn now_ms() -> u64 {
    chrono::Utc::now().timestamp_millis().max(0) as u64
}

pub(super) fn refuse(code: RefusalCode, reason: impl Into<String>) -> Refusal {
    Refusal::new(code, reason)
}

pub(super) fn runtime_refusal(e: &RuntimeError) -> Refusal {
    let code = match e {
        RuntimeError::AdmissionRefused(_) => RefusalCode::Admission,
        RuntimeError::Governance(_) => RefusalCode::Governance,
        RuntimeError::UnknownInstance(_) => RefusalCode::UnknownInstance,
        RuntimeError::InvalidConfig(_) => RefusalCode::InvalidRequest,
        _ => RefusalCode::Runtime,
    };
    Refusal::new(code, e.to_string())
}

impl WorkloadHostService {
    /// Host for the node owning `key`. Serves nobody until controllers are
    /// added, and loads nothing until a route adapter is added.
    pub fn new(
        key: SigningKey,
        exchange: Arc<ArtifactExchange>,
        anchors: TrustAnchors,
        gate: Arc<dyn GateBackend>,
    ) -> Self {
        Self {
            node_id: node_id_from_pubkey(&key.verifying_key().to_bytes()),
            key,
            anchors,
            controllers: Box::new(Vec::<[u8; 32]>::new()),
            nonces: NonceGuard::new(),
            routes: BTreeMap::new(),
            exchange,
            gate,
            chain: None,
            facts: Mutex::new(None),
            facts_source: None,
            address: None,
            instances: tokio::sync::Mutex::new(HashMap::new()),
            in_flight: Mutex::new(HashSet::new()),
            verify_budget: RefusalBudget::default(),
            ingest: None,
            revocations: std::sync::OnceLock::new(),
            kinds: crate::workload_kind::KindRegistry::builtin(),
            restart: super::lifecycle::RestartPolicy::default(),
            lease: None,
            departed: Mutex::new(std::collections::VecDeque::new()),
            last_contact_ms: std::sync::atomic::AtomicU64::new(now_ms()),
            licence_gate: std::sync::OnceLock::new(),
            node_admin: std::sync::OnceLock::new(),
            cogbus: std::sync::OnceLock::new(),
            journal: None,
        }
    }

    /// Accept these workload kinds for a package fetched from a peer. The
    /// default is [`KindRegistry::builtin`]; a node that places a registered
    /// non-cog kind across nodes must pass the same registry as its control
    /// plane, or the fetch refuses the kind.
    ///
    /// [`KindRegistry::builtin`]: crate::workload_kind::KindRegistry::builtin
    pub fn with_kinds(mut self, kinds: crate::workload_kind::KindRegistry) -> Self {
        self.kinds = kinds;
        self
    }

    /// The revocation list this node enforces (first call wins). With it a
    /// `place` that races a revocation is caught and torn down, and
    /// [`Self::enforce_revocations`] callers share one source of truth.
    pub fn set_revocations(&self, list: Arc<crate::revocation::RevocationList>) -> bool {
        self.revocations.set(list).is_ok()
    }

    /// The list set by [`Self::set_revocations`], if any.
    pub fn revocations(&self) -> Option<&Arc<crate::revocation::RevocationList>> {
        self.revocations.get()
    }

    /// Wire the ingest bridge: placed cogs get a per-instance token,
    /// bound to the placing project and revoked at stop and unload.
    pub fn with_ingest(mut self, hooks: IngestHooks) -> Self {
        self.ingest = Some(hooks);
        self
    }

    /// The ingest wiring, if any.
    pub fn ingest(&self) -> Option<&IngestHooks> {
        self.ingest.as_ref()
    }

    /// Replace the bound on chained refusals of unverified requests.
    pub fn with_refusal_budget(mut self, b: RefusalBudget) -> Self {
        self.verify_budget = b;
        self
    }

    /// Adapter (under governance) for one route kind (`native`, ...).
    pub fn with_route(mut self, route: &str, host: Arc<WorkloadHost>) -> Self {
        self.routes.insert(route.to_string(), host);
        self
    }

    /// Who may send requests.
    pub fn with_controllers(mut self, c: impl ControllerPolicy + 'static) -> Self {
        self.controllers = Box::new(c);
        self
    }

    /// Chain for placement and refusal records.
    pub fn with_chain(mut self, cm: Arc<ChainManager>) -> Self {
        self.chain = Some(cm);
        self
    }

    /// Mesh address advertised for this service.
    pub fn with_address(mut self, addr: impl Into<String>) -> Self {
        self.address = Some(addr.into());
        self
    }

    /// Replace the signed facts `describe` returns.
    pub fn set_facts(&self, facts: SignedNodeFacts) {
        if let Ok(mut f) = self.facts.lock() {
            *f = Some(facts);
        }
    }

    /// Read the signed facts from `source` on every `describe` (falling
    /// back to [`Self::set_facts`] when it has none).
    pub fn with_facts_source(mut self, source: FactsSource) -> Self {
        self.facts_source = Some(source);
        self
    }

    fn current_facts(&self) -> Option<SignedNodeFacts> {
        self.facts_source
            .as_ref()
            .and_then(|f| f())
            .or_else(|| self.facts.lock().ok().and_then(|f| f.clone()))
    }

    /// This node's id.
    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    /// The `workload-host` advertisement (ADR-099 section 7).
    pub fn advertisement(&self) -> ServiceAdvertisement {
        let mut metadata = HashMap::new();
        let routes: Vec<&str> = self.routes.keys().map(String::as_str).collect();
        metadata.insert("routes".to_string(), routes.join(","));
        if let Some(a) = &self.address {
            metadata.insert("addr".to_string(), a.clone());
        }
        metadata.insert(
            "ingest".to_string(),
            self.ingest.as_ref().map_or("none", |h| h.state()).to_string(),
        );
        ServiceAdvertisement {
            name: WORKLOAD_HOST_SERVICE.to_string(),
            methods: method::ALL.iter().map(|m| m.to_string()).collect(),
            node_id: self.node_id.clone(),
            global_pid: GlobalPid::new(self.node_id.clone(), 0),
            version: CTL_VERSION.to_string(),
            contract_hash: None,
            metadata,
            last_updated: chrono::Utc::now().timestamp().max(0) as u64,
        }
    }

    /// Remember that `instance_id` was taken down on purpose (`why`:
    /// `revoked` or `unloaded`); the last 256 are reported by `status`.
    pub(super) fn note_departed(&self, instance_id: &str, why: &'static str) {
        if let Some(j) = &self.journal {
            j.remove(instance_id);
        }
        if let Ok(mut d) = self.departed.lock() {
            if d.len() >= 256 {
                d.pop_front();
            }
            d.push_back((instance_id.to_string(), why));
        }
    }

    pub(super) fn record(&self, kind: &str, payload: Value) {
        if let Some(cm) = &self.chain {
            cm.append(HOST_CHAIN_SOURCE, kind, Some(payload));
        }
    }

    fn refused(&self, req: Option<&CtlRequest>, phase: &str, r: &Refusal) {
        // Unauthenticated refusals are chained within a budget only.
        let suppressed = match req {
            Some(_) => 0,
            None => match self.verify_budget.take() {
                Some(n) => n,
                None => return,
            },
        };
        self.record(
            chain::EVENT_KIND_WORKLOAD_REFUSE,
            json!({
                "node": self.node_id, "phase": phase, "code": r.code.as_str(),
                "reason": r.reason, "method": req.map(|q| q.method.as_str()),
                "requester": req.map(|q| q.requester.as_str()),
                "decision_id": req.and_then(|q| q.decision_id.as_deref()),
                "suppressed": suppressed,
            }),
        );
    }

    /// Handle one signed request (`fetch` is the connection back to the
    /// controller, used to fetch the payload before loading).
    pub async fn handle(
        &self,
        header_method: &str,
        signed: &SignedCtl,
        fetch: Option<&mut PeerSet>,
    ) -> SignedCtl {
        self.handle_checked(header_method, signed, fetch).await.0
    }

    /// [`Self::handle`], also saying whether the request was authenticated
    /// (a session drops a peer after an unauthenticated request).
    pub async fn handle_checked(
        &self,
        header_method: &str,
        signed: &SignedCtl,
        fetch: Option<&mut PeerSet>,
    ) -> (SignedCtl, bool) {
        let (resp, authenticated, _) = self.handle_checked_raw(header_method, signed, fetch).await;
        (resp, authenticated)
    }

    /// [`Self::handle_checked`], also returning the raw bytes a node-admin
    /// hook attached to its result (announced as `trailing` in the signed
    /// response). The transport sends them as one frame right after the
    /// response; a caller that cannot must not ask for them.
    pub async fn handle_checked_raw(
        &self,
        header_method: &str,
        signed: &SignedCtl,
        fetch: Option<&mut PeerSet>,
    ) -> (SignedCtl, bool, Option<Vec<u8>>) {
        let verified = verify_request(
            signed,
            &self.node_id,
            now_ms(),
            self.controllers.as_ref(),
            &self.nonces,
        )
        .and_then(|r| {
            if r.method == header_method {
                Ok(r)
            } else {
                Err(refuse(
                    RefusalCode::InvalidRequest,
                    "envelope method differs from signed method",
                ))
            }
        });
        let authenticated = verified.is_ok();
        let (nonce, method_name, outcome, trailer) = match verified {
            Err(r) => {
                self.refused(None, "verify", &r);
                // Unauthenticated nonce, only to bind the refusal.
                let claimed: Option<CtlRequest> = serde_json::from_str(&signed.payload).ok();
                let nonce = claimed.map(|c| c.nonce).unwrap_or_default();
                (nonce, header_method.to_string(), Err(r), None)
            }
            Ok(req) => {
                if req.requester != self.node_id {
                    self.last_contact_ms
                        .store(now_ms(), std::sync::atomic::Ordering::Relaxed);
                }
                let (out, trailer) = match self.dispatch_raw(&req, fetch).await {
                    Ok((v, t)) => (Ok(v), t),
                    Err(r) => (Err(r), None),
                };
                if let Err(r) = &out {
                    self.refused(Some(&req), "handle", r);
                }
                (req.nonce, req.method, out, trailer)
            }
        };
        let resp = CtlResponse {
            version: CTL_VERSION,
            method: method_name,
            responder: self.node_id.clone(),
            request_nonce: nonce,
            outcome: match outcome {
                Ok(result) => CtlOutcome::Ok { result },
                Err(refusal) => CtlOutcome::Refused { refusal },
            },
            trailing: trailer.as_ref().map(|t| t.len() as u64),
        }
        .sign(&self.key);
        (resp, authenticated, trailer)
    }

    /// A decision id is the chain hash of the controller's decision.
    fn valid_decision(req: &CtlRequest) -> bool {
        req.decision_id
            .as_deref()
            .is_some_and(|d| crate::workload_pkg::codec::is_lower_hex(d, 64))
    }

    /// [`Self::dispatch`] keeping a node-admin hook's raw bytes (every other
    /// method answers JSON only).
    async fn dispatch_raw(
        &self,
        req: &CtlRequest,
        fetch: Option<&mut PeerSet>,
    ) -> Result<(Value, Option<Vec<u8>>), Refusal> {
        if method::is_node_admin(&req.method) && (!method::mutates(&req.method) || Self::valid_decision(req)) {
            return self.node_admin_op(req).await;
        }
        self.dispatch(req, fetch).await.map(|v| (v, None))
    }

    async fn dispatch(
        &self,
        req: &CtlRequest,
        fetch: Option<&mut PeerSet>,
    ) -> Result<Value, Refusal> {
        if method::mutates(&req.method) && !Self::valid_decision(req) {
            return Err(refuse(
                RefusalCode::InvalidRequest,
                "mutations carry a decision id (64 hex chain hash)",
            ));
        }
        match req.method.as_str() {
            method::DESCRIBE => {
                Ok(json!({ "facts": self.current_facts(), "advertisement": self.advertisement() }))
            }
            m if method::is_node_admin(m) => self.node_admin_op(req).await.map(|(v, _)| v),
            method::PLACE | method::LOAD => self.place(req, fetch).await,
            method::START | method::STOP | method::UNLOAD | method::STATUS | method::LOGS => {
                self.instance_op(req).await
            }
            m if m.starts_with(crate::workload_governance::policy::WORKLOAD_ACTION_PREFIX) => {
                // Not in the message set: ask the gate so the refusal is on
                // its audit trail, and refuse whatever it says.
                let ctx = json!({ "requester": req.requester, "method": m });
                let why = match self.gate.check(&req.requester, m, &ctx) {
                    GateDecision::Deny { reason, .. } => reason,
                    _ => "not a workload.ctl method".to_string(),
                };
                Err(refuse(RefusalCode::UnknownMethod, format!("{m}: {why}")))
            }
            m => Err(refuse(
                RefusalCode::InvalidRequest,
                format!("{m} is not a workload.ctl method"),
            )),
        }
    }

    pub(super) async fn place(&self, req: &CtlRequest, fetch: Option<&mut PeerSet>) -> Result<Value, Refusal> {
        let _in_flight = InFlight::enter(&self.in_flight, req.decision_id.clone());
        // A revocation that lands after the gate looked at the list but
        // before the instance is listed would be swept past: remember where
        // the list stood, and look again once the instance is listed.
        let list_at_start = self.revocations.get().map(|l| l.generation());
        let b: PlaceBody = serde_json::from_value(req.body.clone())
            .map_err(|e| refuse(RefusalCode::InvalidRequest, format!("place body: {e}")))?;
        if !valid_token(&b.name, 64)
            || !crate::workload_pkg::codec::is_lower_hex(&b.manifest_hash, 64)
        {
            return Err(refuse(
                RefusalCode::InvalidRequest,
                "bad name or manifest hash",
            ));
        }
        let route = route_of(&b.variant).to_string();
        let host = self.routes.get(&route).cloned().ok_or_else(|| {
            refuse(
                RefusalCode::Admission,
                format!("no {route} adapter on this node for {}", b.variant),
            )
        })?;
        let peers = fetch.ok_or_else(|| refuse(RefusalCode::Fetch, "no payload source"))?;
        let pkg = self
            .exchange
            .fetch_package_in(peers, &b.manifest_hash, &self.anchors, &self.kinds)
            .await
            .map_err(|e| match e {
                PackageExchangeError::Verify(v) => refuse(RefusalCode::Verify, v.to_string()),
                other => refuse(RefusalCode::Fetch, other.to_string()),
            })?;
        let w =
            VerifiedWorkload::from_package(&pkg.verified, &StoreSource::new(self.exchange.store()))
                .map_err(|e| refuse(RefusalCode::Verify, e.to_string()))?;
        if w.id != b.name {
            return Err(refuse(
                RefusalCode::Verify,
                "package id differs from the placed name",
            ));
        }
        // ADR-106 run gate: before anything of a Cognitum-origin cog is installed.
        let licence = self.licence_check_place(&pkg.verified, &w, &b.variant, &host, req).await?;
        if let Some(p) = &b.project_id
            && !crate::cog_ingest::valid_project_id(p)
        {
            return Err(refuse(
                RefusalCode::InvalidRequest,
                "project_id must be a 26-character project id",
            ));
        }
        let mut contract = HostContract::new(SocketAddr::from(([0, 0, 0, 0], b.config.csi_port)));
        let mut listener = None;
        let hooks = self.ingest.as_ref().filter(|_| w.kind == "cog");
        // `none`: no ingest wiring on this node. `disabled`: wired, but the
        // bridge could not start; the cog runs with no token and no URL.
        let ingest_state: &'static str = hooks.map_or("none", |h| h.state());
        // The project check runs whether or not the bridge is up, so a
        // degraded placement never records an unverified project id.
        if let Some(hk) = hooks {
            // Instance id unknown yet: the binding only carries the project
            // and the controller for the check.
            let probe = InstanceBinding::new("", b.project_id.clone(), &req.requester);
            hk.authorize(&probe).map_err(|e| match e {
                IngestError::Forbidden => refuse(
                    RefusalCode::Unauthorized,
                    "this controller may not place cogs for that project",
                ),
                IngestError::NotRouted(m) => refuse(RefusalCode::Admission, m),
                other => refuse(RefusalCode::Runtime, format!("ingest bridge: {other}")),
            })?;
        }
        let hooks = hooks.filter(|h| h.is_enabled());
        if ingest_state == "disabled" {
            contract = contract.without_ingest();
        }
        if let Some(hk) = hooks {
            (contract, listener) = hk
                .prepare(&route, contract)
                .await
                .map_err(|e| refuse(RefusalCode::Runtime, format!("ingest bridge: {e}")))?;
        }
        let (contract, bus) = self.bus_prepare(&route, &w, contract);
        let cfg = WorkloadConfig {
            mode: b.config.mode.clone(),
            args: b.config.args.clone(),
            host: contract.clone(),
            node_id: self.node_id.clone(),
        };
        let h = host.load(&w, &cfg).await.map_err(|e| runtime_refusal(&e))?;
        let iid = h.instance_id.clone();
        let bus = match self.bus_lease(bus, &iid, &w.id, (b.project_id.clone(), &req.requester), &contract) {
            Ok(r) => r,
            Err(mut r) => {
                if let Err(u) = host.unload(h.clone()).await {
                    r.reason = format!("{}; unload of {iid} failed too ({u})", r.reason);
                }
                return Err(r);
            }
        };
        let lease = match hooks {
            Some(hk) => {
                let binding = InstanceBinding::new(&iid, b.project_id.clone(), &req.requester);
                match hk.lease(binding, contract, listener) {
                    Ok(l) => Some(l),
                    Err(e) => {
                        self.bus_off(bus.as_ref());
                        let mut r = refuse(RefusalCode::Runtime, format!("ingest bridge: {e}"));
                        if let Err(u) = host.unload(h.clone()).await {
                            r.reason = format!("{}; unload of {iid} failed too ({u})", r.reason);
                        }
                        return Err(r);
                    }
                }
            }
            None => None,
        };
        // A node derives instance ids from the workload: a fresh placement
        // under an id this node reported as departed is no longer departed.
        if let Ok(mut d) = self.departed.lock() {
            d.retain(|(i, _)| i != &iid);
        }
        let continuous = !matches!(b.config.mode, crate::workload_runtime::RunMode::Once);
        let will_start = req.method == method::PLACE && b.start;
        let mut life = super::lifecycle::InstanceLife::new(
            super::lifecycle::LifecycleState::Loaded,
            now_ms(),
        );
        if will_start {
            let _ = life.transition(super::lifecycle::LifecycleState::Running, now_ms());
        }
        self.instances.lock().await.insert(
            iid.clone(),
            Placed {
                handle: h.clone(),
                route: route.clone(),
                name: b.name.clone(),
                variant: b.variant.clone(),
                decision_id: req.decision_id.clone(),
                last: None,
                ingest: lease,
                cogbus: bus,
                ingest_state,
                life,
                continuous,
                desired_running: will_start,
                lease_stopped: false,
                requester: req.requester.clone(),
                licence,
            },
        );
        if let (Some(list), Some(g0)) = (self.revocations.get(), list_at_start)
            && list.generation() != g0
        {
            self.enforce_revocations(list).await;
            if !self.instances.lock().await.contains_key(&iid) {
                return Err(refuse(
                    RefusalCode::Governance,
                    format!("revoked while it was being placed: instance {iid} torn down"),
                ));
            }
        }
        let started = req.method == method::PLACE && b.start;
        if started && let Err(e) = host.start(&h).await {
            // A start the gate refused because the package was revoked is
            // taken down by the revocation itself: the gated unload would
            // need an unload permit the operator may not have written.
            let revoked = match (self.revocations.get(), host.workload_for(&h).await) {
                (Some(l), Some(w)) => {
                    let (p, k, a) = w.revocation_refs();
                    l.first_revoked(Some(&p), &k, &a)
                }
                _ => None,
            };
            return Err(self.roll_back(&host, &h, &e, revoked).await);
        }
        if let Some(j) = &self.journal {
            j.record(super::host_journal::JournalEntry {
                instance_id: iid.clone(),
                decision_id: req.decision_id.clone(),
                requester: req.requester.clone(),
                body: b.clone(),
                desired_running: started,
            });
        }
        let status = host.status(&h).await;
        self.record(
            chain::EVENT_KIND_WORKLOAD_PLACE,
            json!({
                "phase": "placed", "node": self.node_id, "requester": req.requester,
                "decision_id": req.decision_id, "workload": b.name, "variant": b.variant,
                "runtime": h.runtime, "instance_id": iid, "package_id": pkg.package_id,
                "started": started,
            }),
        );
        Ok(json!({
            "instance_id": iid, "runtime": h.runtime, "variant": b.variant,
            "package_id": pkg.package_id, "status": status, "node": self.node_id,
            "ingest": ingest_state,
        }))
    }

    /// A start that failed after load: unload, so the target keeps no
    /// instance the controller was told was refused. If the unload fails
    /// too, the instance stays listed (and reconcilable) and the refusal
    /// says so.
    async fn roll_back(
        &self,
        host: &WorkloadHost,
        h: &InstanceHandle,
        e: &RuntimeError,
        revoked: Option<crate::revocation::RevokedSubject>,
    ) -> Refusal {
        let mut r = runtime_refusal(e);
        let iid = h.instance_id.clone();
        if let Some(p) = self.instances.lock().await.get(&iid) {
            if let (Some(hk), Some(l)) = (&self.ingest, &p.ingest) {
                hk.deactivate(l);
            }
            self.bus_off(p.cogbus.as_ref());
        }
        let unloaded = match &revoked {
            Some(subject) => {
                host.revoke_teardown(h, std::time::Duration::from_secs(2), json!(subject))
                    .await
            }
            None => host.unload(h.clone()).await,
        };
        match unloaded {
            Ok(()) => {
                self.instances.lock().await.remove(&iid);
                r.reason = format!("start failed: {}; loaded instance {iid} unloaded", r.reason);
            }
            Err(u) => {
                r.reason = format!(
                    "start failed: {}; unload of {iid} failed too ({u}), it stays listed",
                    r.reason
                );
            }
        }
        r
    }
}
