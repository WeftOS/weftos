//! Daemon boot for the Seed licence path (ADR-106 phase 1d): the mesh id from
//! `kernel.mesh.genesis_hash` and `kernel.mesh.mesh_nonce`, the checkout
//! policy (and the store it reads), the chain sink for licence events, and
//! the steward's [`SeedBinder`] over `dir/workload-seed-binds.json`.
//!
//! A node with no `mesh_nonce` has no local mesh id: the policy then equals
//! `ManifestPolicy` and no binding is accepted (the path is inert). A changed
//! nonce or pin makes the stored binding orphaned: the store chains
//! `licence.binding_orphaned` (once, at boot below), turns checkout off, and
//! `weaver doctor` reports it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use clawft_kernel::chain::ChainManager;
use clawft_kernel::licence::{
    AdmissionPosture, ChainLicenceSink, CheckoutGrantStore, LICENCE_EVENT_PREFIX, LocalMeshId,
    MeshCheckoutPolicy, mesh_id_from_config,
};
use clawft_kernel::revocation::RevocationList;
use clawft_kernel::workload_pkg::TrustAnchors;
use clawft_kernel::workload_runtime::seed_bind::{BIND_STATE_FILE, SeedBinder};
use clawft_types::config::{MeshAdmissionMode, MeshConfig};
use serde_json::{Value, json};

/// Chain source of licence events.
pub const LICENCE_CHAIN_SOURCE: &str = "licence";

/// The chain kind of a licence event this module or the licence RPCs append
/// directly: `licence.<name>`, the same form the store's
/// [`ChainLicenceSink`] uses for its own events.
pub fn licence_kind(name: &str) -> String {
    format!("{LICENCE_EVENT_PREFIX}{name}")
}

/// How often the boot-owned store is ticked (clock high-water mark persisted,
/// an unsaved unbind retried).
pub const TICK_PERIOD: std::time::Duration = std::time::Duration::from_secs(60);

/// What the licence RPCs and the policy share, built once at daemon boot.
///
/// This is the owner of the node's [`CheckoutGrantStore`] and
/// [`MeshCheckoutPolicy`]: anything that needs them (the artifact exchange,
/// the swarm floods, the tick) takes them from here via [`store`] / [`policy`]
/// (or [`LicenceRuntime::store`] / [`LicenceRuntime::policy`]) rather than
/// opening a second store over the same files.
pub struct LicenceRuntime {
    /// The live local mesh id (unset without a nonce).
    pub local: LocalMeshId,
    /// The policy the artifact exchange uses; owns the grant store.
    pub policy: Arc<MeshCheckoutPolicy>,
    /// The steward's binder, or why it could not be built (an unreadable
    /// replay file: binding then refuses rather than overwrite it).
    pub binder: Result<SeedBinder, String>,
    /// The runtime dir (`workload-seeds.json`, the bind state file).
    pub dir: PathBuf,
    /// The kernel chain (licence events and admin actions are chained here).
    pub chain: Arc<ChainManager>,
    /// `kernel.mesh.genesis_hash` was set.
    pub genesis_pinned: bool,
    /// `kernel.mesh.mesh_nonce` was set.
    pub nonce_set: bool,
    /// The configured pin or nonce is malformed (checkout stays off).
    pub config_error: Option<String>,
    /// Where the live mesh id came from (`None`: no id). Without a licence id
    /// the node's mesh record (`weaver mesh create`, or learned by pairing) is used.
    pub mesh_source: Option<crate::mesh_genesis::IdSource>,
    /// The mesh record is unusable, or names a mesh other than the licence's.
    pub record_issue: Option<String>,
    /// This node's id, the steward a binding names.
    pub steward_node_id: String,
    /// This node's request-signing key, 64 hex chars.
    pub steward_pubkey: String,
}

static RUNTIME: OnceLock<Arc<LicenceRuntime>> = OnceLock::new();

impl LicenceRuntime {
    /// The boot-owned grant store (holds the live local mesh id).
    pub fn store(&self) -> &Arc<CheckoutGrantStore> {
        self.policy.store()
    }

    /// The boot-owned redistribution policy, for the artifact exchange.
    pub fn policy(&self) -> &Arc<MeshCheckoutPolicy> {
        &self.policy
    }
}

/// The daemon's grant store (`None` before boot or without placement).
pub fn store() -> Option<Arc<CheckoutGrantStore>> {
    RUNTIME.get().map(|r| r.store().clone())
}

/// The daemon's checkout policy (`None` before boot or without placement).
pub fn policy() -> Option<Arc<MeshCheckoutPolicy>> {
    RUNTIME.get().map(|r| r.policy().clone())
}

/// Run `f` every `period` on a blocking thread. A panic in `f` is logged and
/// the loop goes on; the first run is one `period` from now.
pub fn spawn_tick(period: std::time::Duration, f: Arc<dyn Fn() + Send + Sync>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut iv = tokio::time::interval(period);
        iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        iv.tick().await; // the first tick is immediate
        loop {
            iv.tick().await;
            let f = f.clone();
            if let Err(e) = tokio::task::spawn_blocking(move || f()).await {
                tracing::warn!(error = %e, "licence tick failed; will retry");
            }
        }
    })
}

/// The store tick [`install`] starts: every `period`, `store.tick()` records
/// the clock, persists the floor's high-water mark and retries an unsaved
/// restrictive record.
pub fn spawn_store_tick(store: Arc<CheckoutGrantStore>, period: std::time::Duration) -> tokio::task::JoinHandle<()> {
    spawn_tick(period, Arc::new(move || store.tick()))
}

static NOT_INSTALLED: OnceLock<String> = OnceLock::new();
/// Whether this daemon holds the mesh service's reserved licence topics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HolderState {
    /// Collapsed mode (no service), or before boot.
    NotApplicable,
    /// The cluster owner's daemon: it runs the licence path.
    Holder,
    /// Another tenant's daemon: no licence path here.
    NotHolder,
    /// The service could not be asked (link down, refresh failed).
    Unknown,
}

static HOLDER: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// Why a service-mode daemon that is not the reserved-topic holder runs no
/// licence path.
pub const NOT_HOLDER: &str = "this daemon does not hold the mesh service's reserved licence topics \
     (it is not the cluster owner's daemon); the Seed licence runtime, exchange and binder run only there";

/// Why the licence path is off while the holder cannot be determined.
pub const HOLDER_UNKNOWN: &str = "holder status unknown: the mesh service query failed; \
     the licence path is off until the service answers";

/// Record the holder state (it changes on reconnect and on a
/// `cluster_owner_uid` change at the service).
pub fn set_holder_state(v: HolderState) {
    let n = match v {
        HolderState::NotApplicable => 0,
        HolderState::Holder => 1,
        HolderState::NotHolder => 2,
        HolderState::Unknown => 3,
    };
    HOLDER.store(n, std::sync::atomic::Ordering::Release);
}

/// See [`set_holder_state`].
pub fn holder_state() -> HolderState {
    match HOLDER.load(std::sync::atomic::Ordering::Acquire) {
        1 => HolderState::Holder,
        2 => HolderState::NotHolder,
        3 => HolderState::Unknown,
        _ => HolderState::NotApplicable,
    }
}

/// `Some(true|false)` when known in service mode, `None` otherwise.
pub fn reserved_holder() -> Option<bool> {
    match holder_state() {
        HolderState::Holder => Some(true),
        HolderState::NotHolder => Some(false),
        _ => None,
    }
}

/// Why the licence RPCs refuse on this daemon right now, if they do: the
/// reason, the holder (uid and user, as the mesh service names it) and how
/// to run the command there.
pub fn holder_refusal() -> Option<String> {
    match holder_state() {
        HolderState::NotHolder => Some(not_holder_message(holder_uid())),
        HolderState::Unknown => Some(format!(
            "{HOLDER_UNKNOWN}; check the link with `weaver mesh status` and retry"
        )),
        _ => None,
    }
}

/// The uid the mesh service names as the licence holder (service mode).
pub fn holder_uid() -> Option<u32> {
    crate::cog_swarm::licence_links().holder_uid()
}

/// The user name of `uid` on this machine, if it has one.
pub fn user_name(uid: u32) -> Option<String> {
    nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid)).ok().flatten().map(|u| u.name)
}

/// `^[a-z_][a-z0-9._-]*$`: safe to paste into `sudo -u <name>`.
pub fn plain_user_name(n: &str) -> bool {
    let mut c = n.chars();
    c.next().is_some_and(|f| f.is_ascii_lowercase() || f == '_')
        && c.all(|x| x.is_ascii_lowercase() || x.is_ascii_digit() || matches!(x, '.' | '_' | '-'))
}

/// The refusal a non-holder daemon gives for a licence verb: where the verbs
/// run and how to run them there. A token on this daemon is no authority on
/// the holder's, so nothing is forwarded (ADR-106, phase 3 notes).
pub fn not_holder_message(uid: Option<u32>) -> String {
    let (who, as_user) = match uid {
        // Only a plain user name goes into the suggested command; anything
        // else (shell metacharacters, spaces, unusual names) uses the uid.
        Some(u) => match user_name(u).filter(|n| plain_user_name(n)) {
            Some(n) => (format!("uid {u}, user {n}"), n),
            None => (format!("uid {u}"), format!("'#{u}'")),
        },
        None => ("not named by the mesh service; `weaver mesh status` as an admin shows cluster_owner_uid".into(),
                 "<cluster-owner>".into()),
    };
    format!(
        "{NOT_HOLDER}. The licence holder is the cluster owner's daemon ({who}). \
         Run the command as that user, against their daemon: `sudo -u {as_user} weaver ...`"
    )
}

/// The state name for status output.
pub fn holder_state_name() -> &'static str {
    match holder_state() {
        HolderState::NotApplicable => "not_applicable",
        HolderState::Holder => "holder",
        HolderState::NotHolder => "not_holder",
        HolderState::Unknown => "unknown",
    }
}

/// Why the licence runtime was not installed although `kernel.mesh.mesh_nonce`
/// is configured (`None`: it was installed, or no nonce is configured).
pub fn not_installed() -> Option<String> {
    NOT_INSTALLED.get().cloned()
}

/// Called at boot when the runtime cannot be built (no chain manager, no
/// signing key). With a configured nonce this is a warning and a doctor
/// finding, not a silent fall back to the inert path.
pub fn skipped(mesh: Option<&MeshConfig>, why: &str) {
    if mesh.is_some_and(|m| m.mesh_nonce.is_some()) {
        tracing::warn!(%why, "kernel.mesh.mesh_nonce is set but the Seed licence runtime is NOT installed; checkout stays off");
        let _ = NOT_INSTALLED.set(why.to_owned());
    }
}

/// The daemon's licence runtime (`None` before boot or without placement).
pub fn runtime() -> Option<Arc<LicenceRuntime>> {
    RUNTIME.get().cloned()
}

/// Everything [`init`] reads.
pub struct InitArgs<'a> {
    /// The runtime dir.
    pub dir: &'a Path,
    /// Pinned operator keys (a binding is accepted only from one of them).
    pub anchors: TrustAnchors,
    /// The node's revocation list.
    pub revocations: Arc<RevocationList>,
    /// The kernel chain.
    pub chain: Arc<ChainManager>,
    /// `kernel.mesh`, when configured.
    pub mesh: Option<&'a MeshConfig>,
    /// This node's id.
    pub steward_node_id: String,
    /// This node's request-signing key, 64 hex chars.
    pub steward_pubkey: String,
}

/// Build the licence runtime: derive the mesh id, set it on the shared
/// handle, open the store (poisoned, not fatal, on a bad file), route its
/// events to the chain, and look at the binding once so an orphaned one is
/// chained at boot, not at first use.
pub fn build(a: InitArgs<'_>) -> LicenceRuntime {
    let genesis = a.mesh.and_then(|m| m.genesis_hash.as_deref());
    let nonce = a.mesh.and_then(|m| m.mesh_nonce.as_deref());
    let local = LocalMeshId::unset();
    let mut config_error = None;
    match mesh_id_from_config(genesis, nonce) {
        Ok(id) => local.set(id),
        Err(e) => {
            tracing::warn!(error = %e, "mesh id not derived; Seed checkout stays off");
            config_error = Some(e.to_string());
        }
    }
    let (mesh_source, record_issue) = apply_mesh_record(a.dir, &local, config_error.is_none());
    let policy = MeshCheckoutPolicy::open(a.dir, a.anchors, a.revocations, local.clone());
    policy.store().set_sink(Arc::new(ChainLicenceSink::new(a.chain.clone())));
    let _ = policy.store().binding_status(); // chains binding_orphaned now
    if let Some(why) = &config_error
        && policy.store().held_binding().is_some()
    {
        // A binding is held but the mesh id cannot be derived: say so on the chain.
        a.chain.append(LICENCE_CHAIN_SOURCE, &licence_kind("mesh_config_error"), Some(json!({ "reason": why })));
    }
    let binder = SeedBinder::new(Vec::new(), a.chain.clone())
        .with_state_file(a.dir.join(BIND_STATE_FILE))
        .map_err(|e| e.to_string());
    if let Err(why) = &binder {
        tracing::warn!(%why, "Seed bind state unreadable; workload.node.bind refuses");
    }
    LicenceRuntime {
        local,
        policy,
        binder,
        dir: a.dir.to_owned(),
        chain: a.chain,
        genesis_pinned: genesis.is_some(),
        nonce_set: nonce.is_some(),
        config_error,
        mesh_source,
        record_issue,
        steward_node_id: a.steward_node_id,
        steward_pubkey: a.steward_pubkey,
    }
}

/// Mesh genesis (ADR-114 §2): with no licence id, take the id from the mesh
/// record; with one, keep it and report a record that disagrees. A record
/// that cannot be used leaves the id unset and says why.
fn apply_mesh_record(
    dir: &Path,
    local: &LocalMeshId,
    config_ok: bool,
) -> (Option<crate::mesh_genesis::IdSource>, Option<String>) {
    use crate::mesh_genesis::{IdSource, resolve};
    if !config_ok {
        return (None, None);
    }
    let licence = local.get();
    match resolve(dir, licence) {
        Ok(Some(r)) => {
            let issue = r.conflicting_record.map(|c| {
                let why = format!(
                    "the mesh record names {} but the Seed licence names {}; the licence id is in use",
                    c.to_hex(),
                    r.id.to_hex()
                );
                tracing::error!("{why}");
                why
            });
            if r.source != IdSource::Licence {
                local.set(Some(r.id));
            }
            (Some(r.source), issue)
        }
        Ok(None) => (None, None),
        Err(e) => {
            tracing::error!(error = %e, "mesh record unusable; this node has no MeshId");
            (None, Some(e))
        }
    }
}

/// Install the daemon's runtime (first call wins).
pub fn install(rt: LicenceRuntime) -> Arc<LicenceRuntime> {
    let rt = Arc::new(rt);
    if RUNTIME.set(rt.clone()).is_ok() && tokio::runtime::Handle::try_current().is_ok() {
        // This module owns the store, so it owns the tick.
        spawn_store_tick(rt.store().clone(), TICK_PERIOD);
    }
    RUNTIME.get().cloned().unwrap_or(rt)
}

/// The node's admission state, as a binding sees it: enforce, a governance
/// verdict source bound, open membership off.
pub fn posture(mesh: Option<&MeshConfig>, gate_bound: bool) -> AdmissionPosture {
    AdmissionPosture {
        enforce: mesh.is_some_and(|m| m.admission == MeshAdmissionMode::Enforce),
        verdict_source_bound: gate_bound,
        open_membership: mesh.is_some_and(|m| m.admission_open_membership),
    }
}

/// `{mesh_id, genesis_pinned, nonce_set, ...}` for `workload.node.binding`.
pub fn status(rt: &LicenceRuntime) -> Value {
    let store = rt.policy.store();
    let _ = store.binding_status(); // chains an orphan once, if not yet
    let local = rt.local.get().map(|m| m.to_hex());
    let held = store.held_binding();
    let binding = held.as_ref().map(|h| {
        let orphaned = local.as_deref().is_some_and(|l| l != h.mesh_id);
        json!({
            "state": h.state,
            "mesh_id": h.mesh_id,
            "seq": h.seq,
            "device_id": h.device_id,
            "grant_pubkey": h.grant_pubkey,
            "grant_fingerprint": clawft_kernel::workload_runtime::grant_fingerprint(&h.grant_pubkey),
            "steward_node_id": h.steward_node_id,
            "orphaned": orphaned,
            "record": h,
        })
    });
    json!({
        "mesh_id": local,
        "genesis_pinned": rt.genesis_pinned,
        "nonce_set": rt.nonce_set,
        "config_error": rt.config_error,
        // A fixed flag, not the error text (it can contain a path).
        "poisoned": store.poisoned().is_some(),
        "binding": binding,
        "next_seq": held.as_ref().map_or(1, |h| h.seq + 1),
        "steward": { "node_id": rt.steward_node_id, "pubkey": rt.steward_pubkey },
        "reserved_holder": reserved_holder(),
        "holder_state": holder_state_name(),
    })
}

#[cfg(test)]
mod name_tests {
    use super::*;

    #[test]
    fn only_plain_user_names_go_into_the_suggested_command() {
        for ok in ["alice", "_svc", "a.b-c_1"] {
            assert!(plain_user_name(ok), "{ok}");
        }
        for bad in ["", "Alice", "1abc", "a b", "a;rm", "$(x)", "a/b", "é"] {
            assert!(!plain_user_name(bad), "{bad:?}");
        }
        let m = not_holder_message(Some(4_000_000_000));
        assert!(m.contains("uid 4000000000") && m.contains("sudo -u '#4000000000'"), "{m}");
    }
}
