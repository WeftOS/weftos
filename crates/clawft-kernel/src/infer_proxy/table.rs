//! The placement table: which node serves `infer.<role>` right now.
//!
//! Local instances come from the card-18 adapter ([`InferRuntime`]:
//! `reconcile`, `health`, `endpoint`); remote ones from `infer.<role>`
//! [`ServiceAdvertisement`]s of admitted peers. Resolution prefers the
//! local instance (sticky: warm KV), then the first admitted peer in node-id
//! order. Every change bumps a generation so caches (the `ProviderRouter`
//! TTL cache) can be invalidated.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use tokio::sync::watch;

use super::svc::{SvcBackend, SvcRoute};
use super::types::{MeshDialer, ProxyAudit, ProxyError, Target};
use super::upstream::parse_loopback_base;
use super::wire::MAX_ROLE;
use crate::exposure::ExposureRegistry;
use crate::ipc::GlobalPid;
use crate::mesh_service_adv::ServiceAdvertisement;
use crate::workload_pkg::manifest::valid_token;
use crate::workload_runtime::infer::{Health, InferRuntime, Reconcile};
use crate::workload_runtime::types::InstanceHandle;

/// Prefix of the advertised service name.
pub const SERVICE_PREFIX: &str = "infer.";

#[derive(Debug, Clone)]
struct LocalEntry {
    base: String,
    model: Option<String>,
    api: String,
    runtime: String,
    /// Served by a cogbus service instead of the HTTP server at `base`.
    svc: Option<SvcRoute>,
}

/// How long a received advertisement counts without being refreshed.
pub const DEFAULT_ADVERT_TTL: Duration = Duration::from_secs(60);

struct RemoteEntry {
    ad: ServiceAdvertisement,
    /// When *we* received it. The peer's own timestamp is never trusted
    /// for freshness.
    received: Instant,
}

/// A local instance a peer's request may reach.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeshLocal {
    /// Verified loopback base URL.
    pub base: String,
    /// The model the server knows the instance by; requests are pinned to it.
    pub model: Option<String>,
    /// Server software (`LlamaCpp`, `MlxLm`, `Ollama`, or `svc`).
    pub runtime: String,
    /// Set when the role is served by a cogbus service: `base` is empty and
    /// the request goes to this service instead.
    pub svc: Option<SvcRoute>,
}

#[derive(Default)]
struct Inner {
    local: HashMap<String, LocalEntry>,
    remote: HashMap<String, BTreeMap<String, RemoteEntry>>,
    /// Consumer-side: nodes allowed to serve each role. Default deny.
    allow: HashMap<String, HashSet<String>>,
    /// Serving-side: peers this node serves each role to. Default deny.
    serve_allow: HashMap<String, HashSet<String>>,
    exposed: HashSet<String>,
    proxy_ports: HashMap<String, u16>,
}

/// What [`PlacementTable::sync_local`] found.
#[derive(Debug, Clone, PartialEq)]
pub struct SyncOutcome {
    /// Whether the role is now served locally.
    pub registered: bool,
    /// What reconcile did (restart, give up, ...), for the caller to chain.
    pub reconcile: Option<Reconcile>,
    /// Health seen.
    pub health: Option<Health>,
}

/// Role to serving node.
pub struct PlacementTable {
    node_id: String,
    dialer: Option<Arc<dyn MeshDialer>>,
    audit: Option<Arc<dyn ProxyAudit>>,
    inner: RwLock<Inner>,
    gen_tx: watch::Sender<u64>,
    advert_ttl: Duration,
    /// Receive-time clock for adverts; replaceable so expiry is testable
    /// without sleeping.
    clock: Arc<dyn Fn() -> Instant + Send + Sync>,
    /// ADR-120: the exposure registry the `infer.<role>` roles live in.
    exposures: Arc<ExposureRegistry>,
    /// The caller of cogbus-served roles; absent until the broker is up.
    svc_backend: RwLock<Option<Arc<dyn SvcBackend>>>,
}

mod exposure;

fn valid_role(role: &str) -> bool {
    valid_token(role, MAX_ROLE) && !role.contains('/')
}

impl PlacementTable {
    /// Table for node `node_id`. Without a dialer there is no mesh: only
    /// local instances resolve.
    pub fn new(
        node_id: impl Into<String>,
        dialer: Option<Arc<dyn MeshDialer>>,
        audit: Option<Arc<dyn ProxyAudit>>,
    ) -> Self {
        let node_id = node_id.into();
        Self {
            exposures: Arc::new(ExposureRegistry::new(&node_id)),
            node_id,
            dialer,
            audit,
            inner: RwLock::new(Inner::default()),
            gen_tx: watch::channel(0).0,
            advert_ttl: DEFAULT_ADVERT_TTL,
            clock: Arc::new(Instant::now),
            svc_backend: RwLock::new(None),
        }
    }

    /// Install the caller of cogbus-served roles (the daemon's broker session).
    pub fn set_svc_backend(&self, b: Arc<dyn SvcBackend>) {
        *self.svc_backend.write().unwrap() = Some(b);
    }

    /// The caller of cogbus-served roles, once installed.
    pub fn svc_backend(&self) -> Option<Arc<dyn SvcBackend>> {
        self.svc_backend.read().unwrap().clone()
    }

    /// Lifetime of a received advertisement (refreshed by each repeat).
    pub fn with_advert_ttl(mut self, ttl: Duration) -> Self {
        self.advert_ttl = ttl;
        self
    }

    /// Replace the receive-time clock (tests drive expiry with a fake one).
    pub fn with_clock(mut self, clock: Arc<dyn Fn() -> Instant + Send + Sync>) -> Self {
        self.clock = clock;
        self
    }

    fn age(&self, e: &RemoteEntry) -> Duration {
        (self.clock)().saturating_duration_since(e.received)
    }

    /// Drop received adverts that were not refreshed within the TTL, so a
    /// dead instance cannot linger. Returns how many were removed.
    pub fn prune_expired(&self) -> usize {
        let n = {
            let mut g = self.inner.write().unwrap();
            let mut n = 0;
            for per in g.remote.values_mut() {
                let before = per.len();
                per.retain(|_, e| self.age(e) <= self.advert_ttl);
                n += before - per.len();
            }
            g.remote.retain(|_, per| !per.is_empty());
            n
        };
        if n > 0 {
            self.bump();
        }
        n
    }

    /// Allow or stop allowing `node` to serve `role` to this node. This is
    /// the consumer-side counterpart of [`expose_to_mesh`](Self::expose_to_mesh)
    /// and the governed decision to send this node's prompts to that node:
    /// without it no remote instance resolves, however it advertises.
    pub fn allow_remote_node(&self, role: &str, node: &str, on: bool) {
        let changed = {
            let mut g = self.inner.write().unwrap();
            let set = g.allow.entry(role.to_string()).or_default();
            if on {
                set.insert(node.to_string())
            } else {
                set.remove(node)
            }
        };
        if changed {
            self.audit(
                "infer.remote.allow",
                serde_json::json!({"role": role, "node": node, "allowed": on, "consumer": self.node_id}),
            );
            self.bump();
        }
    }

    /// This node's id.
    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    /// The mesh dialer, when there is one.
    pub fn dialer(&self) -> Option<&Arc<dyn MeshDialer>> {
        self.dialer.as_ref()
    }

    /// Changes so far. A router cache keyed on this is never stale.
    pub fn generation(&self) -> u64 {
        *self.gen_tx.borrow()
    }

    /// Wakes on every change (`invalidate_all_placement` on it).
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.gen_tx.subscribe()
    }

    fn bump(&self) {
        self.gen_tx.send_modify(|g| *g += 1);
    }

    fn audit(&self, kind: &str, payload: serde_json::Value) {
        if let Some(a) = &self.audit {
            a.record(kind, payload);
        }
    }

    /// Serve `role` from the loopback server at `base`. Refused when the
    /// endpoint is not `http://<loopback ip>:<port>`.
    pub fn register_local(
        &self,
        role: &str,
        base: &str,
        model: Option<String>,
        api: &str,
        runtime: &str,
    ) -> Result<(), ProxyError> {
        if !valid_role(role) {
            return Err(ProxyError::BadRequest(format!("bad role '{role}'")));
        }
        parse_loopback_base(base)?;
        let entry = LocalEntry {
            base: base.trim_end_matches('/').to_string(),
            model,
            api: api.to_string(),
            runtime: runtime.to_string(),
            svc: None,
        };
        let changed = {
            let mut g = self.inner.write().unwrap();
            let prev = g.local.insert(role.to_string(), entry.clone());
            prev.is_none_or(|p| p.base != entry.base)
        };
        if changed {
            self.bump();
        }
        self.sync_exposure(role);
        Ok(())
    }

    /// Serve `role` from a cogbus service on this node. The role advertises
    /// and exposes exactly like an HTTP-served one.
    pub fn register_local_svc(&self, role: &str, route: SvcRoute) -> Result<(), ProxyError> {
        if !valid_role(role) {
            return Err(ProxyError::BadRequest(format!("bad role '{role}'")));
        }
        let entry = LocalEntry {
            base: String::new(),
            model: Some(route.model.clone()),
            api: "openai".to_string(),
            runtime: "svc".to_string(),
            svc: Some(route),
        };
        let changed = {
            let mut g = self.inner.write().unwrap();
            let prev = g.local.insert(role.to_string(), entry.clone());
            prev.is_none_or(|p| p.svc != entry.svc)
        };
        if changed {
            self.bump();
        }
        self.sync_exposure(role);
        Ok(())
    }

    /// Stop serving `role` locally.
    pub fn deregister_local(&self, role: &str) {
        let (had, exposed) = {
            let mut g = self.inner.write().unwrap();
            (g.local.remove(role).is_some(), g.exposed.contains(role))
        };
        if had {
            self.bump();
        }
        self.sync_exposure(role);
        // Peers hold the advert of an instance that just stopped serving:
        // tell them now instead of letting them route to it until the TTL.
        if had && exposed {
            self.exposures.queue_withdrawal(&Self::exposure_id(role));
        }
    }

    /// Re-evaluate `role` against its adapter instance: reconcile (a dead
    /// managed server is restarted within its policy), probe, and serve it
    /// only while it answers.
    pub async fn sync_local(
        &self,
        role: &str,
        rt: &InferRuntime,
        h: &InstanceHandle,
    ) -> SyncOutcome {
        let reconcile = rt.reconcile(h).await.ok();
        let health = rt.health(h).await.ok().map(|r| r.health);
        let endpoint = rt.endpoint(h).await;
        let spec = rt.spec_of(h).await;
        let served = rt.served_model(h).await;
        let up = matches!(health, Some(Health::Up));
        let mut registered = false;
        if let (true, Some(base), Some(spec)) = (up, endpoint, spec) {
            registered = self
                .register_local(
                    role,
                    &base,
                    served,
                    &spec.api,
                    &format!("{:?}", spec.runtime),
                )
                .is_ok();
        }
        if !registered {
            self.deregister_local(role);
        }
        SyncOutcome {
            registered,
            reconcile,
            health,
        }
    }

    /// Allow or stop serving `role` to mesh peers. Beyond loopback this is
    /// a governed decision (ADR-101 section 7): the caller decides, this
    /// records it.
    pub fn expose_to_mesh(&self, role: &str, on: bool) {
        let changed = {
            let mut g = self.inner.write().unwrap();
            if on {
                g.exposed.insert(role.to_string())
            } else {
                g.exposed.remove(role)
            }
        };
        self.sync_exposure(role);
        if changed && !on {
            self.exposures.queue_withdrawal(&Self::exposure_id(role));
        }
        if changed {
            self.audit(
                "infer.mesh.expose",
                serde_json::json!({"role": role, "node_id": self.node_id, "exposed": on}),
            );
            self.bump();
        }
    }

    /// The loopback proxy port registered for `role` (tests, status).
    pub fn proxy_port(&self, role: &str) -> Option<u16> {
        self.inner.read().unwrap().proxy_ports.get(role).copied()
    }

    /// The loopback proxy port that fronts `role` on this node.
    pub fn set_proxy_port(&self, role: &str, port: u16) {
        self.inner
            .write()
            .unwrap()
            .proxy_ports
            .insert(role.to_string(), port);
    }

    /// Allow or stop allowing `peer` to use this node's instance of `role`
    /// (the serving-side counterpart of
    /// [`allow_remote_node`](Self::allow_remote_node)). Exposure
    /// ([`expose_to_mesh`](Self::expose_to_mesh)) says the role may leave
    /// loopback at all; this says to whom. Default deny, audited.
    pub fn allow_mesh_peer(&self, role: &str, peer: &str, on: bool) {
        let changed = {
            let mut g = self.inner.write().unwrap();
            let set = g.serve_allow.entry(role.to_string()).or_default();
            if on {
                set.insert(peer.to_string())
            } else {
                set.remove(peer)
            }
        };
        if changed {
            self.audit(
                "infer.mesh.allow",
                serde_json::json!({"role": role, "peer": peer, "allowed": on, "server": self.node_id}),
            );
        }
    }

    /// Take an `infer.<role>` advertisement received from `sender`, the
    /// node id the connection was verified as. Heard only when the
    /// advertisement is the sender's own (`ad.node_id == sender`), the sender
    /// is an enforced-admission full node with a trusted scope, and this
    /// node allowlisted `sender` for the role. A repeat refreshes the TTL.
    pub fn ingest_advertisement(&self, sender: &str, ad: &ServiceAdvertisement) -> bool {
        let Some(role) = ad.name.strip_prefix(SERVICE_PREFIX) else {
            return false;
        };
        if !valid_role(role) || sender == self.node_id {
            return false;
        }
        let refuse = |why: &str| {
            self.audit(
                "infer.advert.refused",
                serde_json::json!({"role": role, "sender": sender, "advertised_node": ad.node_id, "why": why}),
            );
            false
        };
        if ad.node_id != sender {
            return refuse("advertisement is for another node");
        }
        if !self.dialer.as_ref().is_some_and(|d| d.is_admitted(sender)) {
            return refuse("peer not admitted as a trusted node");
        }
        let mut g = self.inner.write().unwrap();
        if !g.allow.get(role).is_some_and(|s| s.contains(sender)) {
            drop(g);
            return refuse("node is not allowed to serve this role");
        }
        let per = g.remote.entry(role.to_string()).or_default();
        // Expired neighbours of this role go now rather than lingering.
        let before = per.len();
        per.retain(|_, e| (self.clock)().saturating_duration_since(e.received) <= self.advert_ttl);
        let pruned = per.len() != before;
        let changed = per.get(sender).is_none_or(|e| {
            e.ad.metadata != ad.metadata || e.ad.methods != ad.methods || self.age(e) > self.advert_ttl
        });
        // One entry per (role, node): a newer advert replaces the older one,
        // so a restarted or moved instance supersedes what it advertised before.
        per.insert(
            sender.to_string(),
            RemoteEntry {
                ad: ad.clone(),
                received: (self.clock)(),
            },
        );
        drop(g);
        if changed || pruned {
            self.bump();
        }
        true
    }

    /// A peer left or was revoked: forget what it advertised.
    pub fn remove_node(&self, node_id: &str) {
        let changed = {
            let mut g = self.inner.write().unwrap();
            let mut any = false;
            for per in g.remote.values_mut() {
                any |= per.remove(node_id).is_some();
            }
            g.remote.retain(|_, per| !per.is_empty());
            any
        };
        if changed {
            self.bump();
        }
    }

    /// Where `role` is served: this node first, then an admitted peer.
    /// Admission is re-checked here, so a revoked peer stops resolving at
    /// once even before its entry is removed.
    pub fn resolve(&self, role: &str) -> Option<Target> {
        let g = self.inner.read().unwrap();
        if let Some(l) = g.local.get(role) {
            if let Some(s) = &l.svc {
                return Some(Target::Svc {
                    topic: s.topic.clone(),
                    model: s.model.clone(),
                });
            }
            return Some(Target::Local {
                base: l.base.clone(),
            });
        }
        let dialer = self.dialer.as_ref()?;
        let allowed = g.allow.get(role)?;
        g.remote
            .get(role)?
            .iter()
            .filter(|(n, e)| {
                allowed.contains(*n) && self.age(e) <= self.advert_ttl && dialer.is_admitted(n)
            })
            // The most recently heard node wins, so a placement that moved
            // to another node supersedes the one that went quiet.
            .max_by_key(|(_, e)| e.received)
            .map(|(n, _)| Target::Remote { node_id: n.clone() })
    }

    /// The local instance of `role`, only when it is exposed to the mesh
    /// and `peer` is on the role's serve allowlist: what that peer's
    /// forwarded request may reach. Never a remote target, so a request
    /// cannot be bounced from node to node.
    pub fn local_for_peer(&self, role: &str, peer: &str) -> Option<MeshLocal> {
        let g = self.inner.read().unwrap();
        (g.exposed.contains(role)
            && g.serve_allow.get(role).is_some_and(|s| s.contains(peer))
            && self.serve_refusal(role).is_none())
            .then(|| {
                g.local.get(role).map(|l| MeshLocal {
                    base: l.base.clone(),
                    model: l.model.clone(),
                    runtime: l.runtime.clone(),
                    svc: l.svc.clone(),
                })
            })
            .flatten()
    }

    /// Whether `peer` is on `role`'s serve allowlist.
    pub fn mesh_peer_allowed(&self, role: &str, peer: &str) -> bool {
        self.inner
            .read()
            .unwrap()
            .serve_allow
            .get(role)
            .is_some_and(|s| s.contains(peer))
    }

    /// Roles currently exposed to the mesh.
    pub fn exposed_roles(&self) -> Vec<String> {
        let mut v: Vec<String> = self.inner.read().unwrap().exposed.iter().cloned().collect();
        v.sort();
        v
    }

    /// Whether `peer` is on any role's serve allowlist.
    pub fn peer_listed_any(&self, peer: &str) -> bool {
        self.inner
            .read()
            .unwrap()
            .serve_allow
            .values()
            .any(|s| s.contains(peer))
    }

    /// Base URL for in-process consumers (`PlacementResolver`): the
    /// instance itself when it is local, this node's loopback proxy when it
    /// is remote (the proxy speaks the mesh). `None` falls back.
    pub fn base_url_for_role(&self, role: &str) -> Option<String> {
        match self.resolve(role)? {
            Target::Local { base } => Some(format!("{base}/v1")),
            // The proxy speaks the mesh and the service; in-process
            // consumers only speak HTTP.
            Target::Remote { .. } | Target::Svc { .. } => {
                let port = *self.inner.read().unwrap().proxy_ports.get(role)?;
                Some(format!("http://127.0.0.1:{port}/v1"))
            }
        }
    }

    /// Base URL of a server for `role` on this node only (`.../v1`), for
    /// consumers that cannot go through the mesh.
    pub fn local_base_for_role(&self, role: &str) -> Option<String> {
        match self.resolve(role)? {
            Target::Local { base } => Some(format!("{base}/v1")),
            Target::Remote { .. } | Target::Svc { .. } => None,
        }
    }

    /// `infer.<role>` advertisement for a locally served role that is
    /// exposed to the mesh.
    pub fn advertisement(&self, role: &str, now_secs: u64) -> Option<ServiceAdvertisement> {
        let g = self.inner.read().unwrap();
        let l = g
            .local
            .get(role)
            .filter(|_| g.exposed.contains(role) && self.serve_refusal(role).is_none())?;
        let mut metadata = HashMap::new();
        metadata.insert("role".to_string(), role.to_string());
        metadata.insert("node_id".to_string(), self.node_id.clone());
        metadata.insert("api".to_string(), l.api.clone());
        metadata.insert("runtime".to_string(), l.runtime.clone());
        metadata.insert("load".to_string(), "0".to_string());
        if let Some(m) = &l.model {
            metadata.insert("model".to_string(), m.clone());
        }
        Some(ServiceAdvertisement {
            name: format!("{SERVICE_PREFIX}{role}"),
            methods: if l.svc.is_some() {
                vec!["chat.completions".to_string(), "models".to_string()]
            } else {
                ["chat.completions", "completions", "embeddings", "models"]
                    .map(String::from)
                    .to_vec()
            },
            node_id: self.node_id.clone(),
            global_pid: GlobalPid::local(0, &self.node_id),
            version: "1".to_string(),
            contract_hash: None,
            metadata,
            last_updated: now_secs,
        })
    }

    /// Advertisements of every exposed local role.
    pub fn advertisements(&self, now_secs: u64) -> Vec<ServiceAdvertisement> {
        let roles: Vec<String> = self.inner.read().unwrap().local.keys().cloned().collect();
        roles
            .iter()
            .filter_map(|r| self.advertisement(r, now_secs))
            .collect()
    }
}
