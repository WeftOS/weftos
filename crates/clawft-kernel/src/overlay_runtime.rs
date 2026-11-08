//! A project kernel's live governance: boot, reload and parent updates
//! (ADR-103 A6, D8, Phase 2 package E).
//!
//! * [`prepare`] runs early in boot, before any subsystem is built: it reads
//!   the project certificate, the signed parent policy and the overlay, checks
//!   them and merges. Any failure is a boot refusal (fail closed, decision 2);
//!   a project never starts with fewer rules than its parent.
//! * [`Prepared::apply_limits`] enforces the merged `max_processes` (the
//!   process table's cap) and `spawn_budget` (the concurrent sub-agent spawn
//!   cap, `kernel.agent.subagents.max_per_conv`) by lowering the kernel config
//!   before it is used. Both are boot-time caps: a pushed policy that lowers
//!   them reports `restart_required` instead of pretending to apply live.
//! * [`OverlayRuntime`] owns the running state. [`OverlayRuntime::reload`] is
//!   the only thing that re-reads the files; an overlay edited on disk does
//!   nothing until it is called. [`OverlayRuntime::apply_parent_update`]
//!   accepts a pushed policy, which must carry a valid user signature and a
//!   version no older than the newest accepted. Every accepted change appends
//!   `governance.overlay.applied` and publishes the new effective hash to the
//!   chain's `rule_hash` provider; every refused one appends
//!   `governance.overlay.rejected` and leaves the running rules alone.
//!
//! The chain stamps `rule_hash` through a lock-free [`RuleHashCell`]; the gate
//! swap and the cell write happen under the gate's write lock, so a decision
//! is never chained with the other generation's hash. Lock order: the gate
//! lock is taken before the chain lock (a check appends under it); nothing
//! takes the gate lock while holding the chain lock.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use clawft_types::config::KernelConfig;
use clawft_types::project::canon::hex_encode;
use clawft_types::runtime_paths::RuntimePaths;
use serde_json::json;

use crate::chain::ChainManager;
use crate::chain_rule_hash::RuleHashCell;
use crate::gate::{GateBackend, GateDecision, GovernanceGate, GovernanceSnapshot};
use crate::governance_overlay::{Effective, Overlay, OverlayError, load_overlay, merge};
pub use crate::overlay_trust::{REVOKED_FILE, USER_PIN_FILE, VERSION_PIN_FILE, write_user_pin};
use crate::overlay_trust::{chain_history, check_version, load_user_pubkey, read_pin, write_pin};
use crate::parent_policy::{
    ParentPolicy, ParentPolicyError, load_parent_policy, verify_parent_policy,
};

/// Fallback engine threshold when neither parent nor overlay set one.
pub const DEFAULT_RISK_THRESHOLD: f64 = 0.7;
#[cfg(test)]
thread_local! {
    /// Tests boot with `RuntimePaths::at`; this lets one name a child root.
    pub(crate) static TEST_CHILD_PATHS: std::cell::RefCell<Option<RuntimePaths>> =
        const { std::cell::RefCell::new(None) };
}

/// The child's paths: `paths` itself when it is a child root. A project
/// profile on any other root is refused (package H makes
/// `RuntimePaths::resolve()` return the child root for a spawned child).
pub fn child_paths(paths: &RuntimePaths) -> Result<RuntimePaths, OverlayError> {
    #[cfg(test)]
    if let Some(p) = TEST_CHILD_PATHS.with(|c| c.borrow().clone()) {
        return Ok(p);
    }
    #[cfg(feature = "test-support")]
    if let Some(p) = test_support::child_paths() {
        return Ok(p);
    }
    paths.child_id().ok_or(OverlayError::NotAChild)?;
    Ok(paths.clone())
}

/// Process-wide child-root override for other crates' integration tests,
/// which boot a project-profile kernel before package H's supervisor exists.
/// Only compiled with the `test-support` feature, which a release build
/// refuses (see the `compile_error!` in `lib.rs`).
#[cfg(feature = "test-support")]
pub mod test_support {
    use super::RuntimePaths;
    use std::sync::Mutex;

    static CHILD_PATHS: Mutex<Option<RuntimePaths>> = Mutex::new(None);

    /// Make [`super::child_paths`] return `paths` for the rest of the process.
    pub fn set_child_paths(paths: Option<RuntimePaths>) {
        *CHILD_PATHS.lock().unwrap_or_else(|e| e.into_inner()) = paths;
    }

    pub(super) fn child_paths() -> Option<RuntimePaths> {
        CHILD_PATHS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// A project the child-boot tests run as.
    pub const FIXTURE_PROJECT_ID: &str = "01JB8Z3Q0V6X9KQ4M2N7T5R1WD";

    /// Build a REAL child root under `base` and make it this process's child
    /// root. Nothing is bypassed: the overlay's own checks run over it.
    ///
    /// Written exactly as the user daemon and supervisor will write them: a
    /// user key (seed `[7; 32]`) that signs the project certificate
    /// (`ProjectCert::sign`) and an empty-rules parent policy
    /// (`parent_policy::export_rules_to`), the `user.pub` pin
    /// (`write_user_pin`), and a 0600 `project.key` matching the certificate.
    /// The version pin is written by `Prepared::commit`, as on a real boot.
    pub fn install_child_fixture(base: &std::path::Path) -> RuntimePaths {
        use chrono::{Duration, Utc};
        use clawft_types::config::overlay::Limits;
        use clawft_types::project::cert::{CertRequest, ProjectCert};
        use ed25519_dalek::SigningKey;

        let run = base.join("run").join(FIXTURE_PROJECT_ID);
        let root = base.join("project");
        std::fs::create_dir_all(&run).expect("run dir");
        std::fs::create_dir_all(root.join(".weftos")).expect("project .weftos");
        let paths = RuntimePaths::child_at(&run, FIXTURE_PROJECT_ID, &root).expect("child paths");

        let user_key = SigningKey::from_bytes(&[7u8; 32]);
        let project_key = SigningKey::from_bytes(&[3u8; 32]);
        let cert = ProjectCert::sign(
            &user_key,
            &CertRequest {
                project_id: FIXTURE_PROJECT_ID.into(),
                project_pubkey: project_key.verifying_key().to_bytes(),
                serial: 1,
                issued_at: Utc::now() - Duration::minutes(1),
                expires_at: None,
            },
        );
        let cert_path = paths.project_cert().expect("cert path");
        std::fs::write(&cert_path, serde_json::to_vec(&cert).expect("cert json")).expect("cert");
        crate::parent_policy::write_atomic_0600(
            &paths.project_key().expect("key path"),
            &project_key.to_bytes(),
        )
        .expect("project.key");
        crate::parent_policy::export_rules_to(
            &paths.parent_policy(),
            Vec::new(),
            0.8,
            false,
            &Limits::default(),
            &user_key,
        )
        .expect("signed parent policy");
        crate::overlay_trust::write_user_pin(&run, &user_key.verifying_key().to_bytes())
            .expect("user.pub pin");
        set_child_paths(Some(paths.clone()));
        paths
    }
}

/// A gate whose inner [`GovernanceGate`] is replaced atomically on update.
pub struct OverlayGate {
    inner: RwLock<GovernanceGate>,
}

impl OverlayGate {
    fn new(gate: GovernanceGate) -> Self {
        Self {
            inner: RwLock::new(gate),
        }
    }

    /// Replace the rules and publish `hash` in one step (see module docs).
    fn swap(&self, mut gate: GovernanceGate, hash: [u8; 32], cell: &RuleHashCell) {
        let mut w = self.inner.write().unwrap_or_else(|e| e.into_inner());
        // Keep what the kernel configured on the running gate (exemptions,
        // rate limit, scorer); a fresh gate would fall back to defaults.
        gate.inherit_config(&w);
        *w = gate;
        cell.set(Some(hash));
    }
}

impl GateBackend for OverlayGate {
    fn check(&self, agent_id: &str, action: &str, context: &serde_json::Value) -> GateDecision {
        // The read lock spans the whole evaluation: a concurrent swap waits.
        self.inner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .check(agent_id, action, context)
    }

    fn governance_snapshot(&self) -> Option<GovernanceSnapshot> {
        self.inner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .governance_snapshot()
    }

    /// Delegate to the inner engine, which holds the merged parent + overlay
    /// rules (the same strictest-wins set `check` evaluates). Without this the
    /// trait default answers `None` ("no engine"), and a `governance.check`
    /// caller treats governance as unavailable on a project kernel.
    fn check_detailed(
        &self,
        agent_id: &str,
        action: &str,
        context: &serde_json::Value,
    ) -> Option<crate::gate::GateVerdict> {
        self.inner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .check_detailed(agent_id, action, context)
    }
}

/// The hashes and version of the rules a kernel is running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    /// Hex.
    pub parent_hash: String,
    /// Hex.
    pub overlay_hash: String,
    /// Hex; the chain's `rule_hash`.
    pub effective_hash: String,
    /// Parent policy version.
    pub parent_version: u64,
    /// Rules in the engine.
    pub rule_count: usize,
    /// `max_processes` / `spawn_budget` changed since boot and only a restart
    /// applies them.
    pub restart_required: bool,
}

impl Applied {
    fn of(e: &Effective, boot: &Effective) -> Self {
        let restart_required = e.limits.max_processes != boot.limits.max_processes
            || e.limits.spawn_budget != boot.limits.spawn_budget;
        Self {
            parent_hash: hex_encode(&e.parent_hash),
            overlay_hash: hex_encode(&e.overlay_hash),
            effective_hash: hex_encode(&e.effective_hash),
            parent_version: e.parent_version,
            rule_count: e.rules.len(),
            restart_required,
        }
    }

    /// JSON for RPC replies and chain payloads.
    pub fn to_json(&self) -> serde_json::Value {
        json!({
            "parent_hash": self.parent_hash,
            "overlay_hash": self.overlay_hash,
            "effective_hash": self.effective_hash,
            "parent_version": self.parent_version,
            "rule_count": self.rule_count,
            "restart_required": self.restart_required,
        })
    }
}

/// Everything boot learned about a project's governance, before the chain
/// exists.
pub struct Prepared {
    nested: bool,
    paths: RuntimePaths,
    user_pubkey: [u8; 32],
    user_pin: bool,
    parent: ParentPolicy,
    overlay: Overlay,
    overlay_present: bool,
    pinned: Option<u64>,
    effective: Effective,
    cell: Arc<RuleHashCell>,
}

/// Read, verify and merge the certificate, parent policy and overlay of the
/// child at `paths`. Fails closed.
pub fn prepare(paths: &RuntimePaths) -> Result<Prepared, OverlayError> {
    let paths = child_paths(paths)?;
    let (user_pubkey, user_pin) = load_user_pubkey(&paths)?;
    let parent = load_parent_policy(&paths.parent_policy())?;
    verify_parent_policy(&parent, &user_pubkey)?;
    let pinned = read_pin(&paths)?;
    check_version(&parent, pinned)?;
    let overlay_present = paths.overlay().is_some_and(|p| p.exists());
    let overlay = match paths.overlay() {
        Some(p) => load_overlay(&p)?,
        None => Overlay::empty(),
    };
    let effective = merge(&parent, &overlay)?;
    let cell = Arc::new(RuleHashCell::new());
    cell.set(Some(effective.effective_hash));
    Ok(Prepared {
        nested: false,
        paths,
        user_pubkey,
        user_pin,
        parent,
        overlay,
        overlay_present,
        pinned,
        effective,
        cell,
    })
}

impl Prepared {
    /// Boot-time history checks against the project chain, then persist the
    /// rollback pin. Call once the chain is restored and before anything is
    /// appended to it.
    ///
    /// * the pin is missing but the chain records an applied overlay: someone
    ///   removed it to roll the parent policy back, so boot is refused;
    /// * `overlay.toml` is missing but the last applied overlay was not empty:
    ///   deleting the file must not clear the overlay (an empty file does).
    pub fn commit(&self, chain: &ChainManager) -> Result<(), OverlayError> {
        let h = chain_history(chain);
        if self.pinned.is_none() && h.any_applied {
            return Err(OverlayError::PinMissing);
        }
        // The pin file can be lowered or zeroed; the chain remembers the
        // highest version ever applied, so neither the file's version nor the
        // pin may be below it.
        if let Some(max) = h.max_parent_version {
            for have in [Some(self.parent.version), self.pinned]
                .into_iter()
                .flatten()
            {
                if have < max {
                    return Err(ParentPolicyError::Rollback { have, pinned: max }.into());
                }
            }
        }
        // A pin that was in use must not silently disappear.
        if h.user_pin_used && !self.user_pin {
            return Err(OverlayError::Cert(
                "a user.pub pin was in use on an earlier boot but is now absent".into(),
            ));
        }
        let empty = hex_encode(&Overlay::empty().hash);
        if let Some(last) = &h.last_overlay_hash
            && !self.overlay_present
            && *last != empty
        {
            return Err(OverlayError::OverlayMissing);
        }
        if self.pinned.is_none_or(|p| self.parent.version > p) {
            write_pin(&self.paths, self.parent.version)?;
        }
        Ok(())
    }

    /// Lower `kc` to the merged limits. A limit can only go down.
    pub fn apply_limits(&self, kc: &mut KernelConfig) {
        let l = &self.effective.limits;
        if let Some(n) = l.max_processes {
            kc.max_processes = kc.max_processes.min(u32::try_from(n).unwrap_or(u32::MAX));
        }
        if let Some(b) = l.spawn_budget {
            let sub = &mut kc.agent.get_or_insert_with(Default::default).subagents;
            sub.max_per_conv = sub.max_per_conv.min(u32::try_from(b).unwrap_or(u32::MAX));
            if b == 0 {
                sub.enabled = false;
            }
        }
    }

    /// The merged rules (they replace the default set).
    pub fn rules(&self) -> &[crate::governance::GovernanceRule] {
        &self.effective.rules
    }

    /// Engine threshold and human-approval flag for the gate.
    pub fn engine_params(&self) -> (f64, bool) {
        (
            self.effective.risk_threshold(DEFAULT_RISK_THRESHOLD),
            self.effective.human_approval(false),
        )
    }

    /// Hashes of what boot merged.
    pub fn applied(&self) -> Applied {
        Applied::of(&self.effective, &self.effective)
    }

    /// Start stamping `rule_hash` on every chain append. Call before the
    /// first governance event so genesis carries it.
    pub fn install_provider(&self, chain: &ChainManager) {
        chain.set_rule_hash_provider(self.cell.provider());
    }

    /// Wrap the boot gate (already built from [`Self::rules`] and
    /// [`Self::engine_params`]) so it can be replaced, and record the
    /// applied rules on the chain.
    pub fn into_runtime(
        self,
        gate: GovernanceGate,
        chain: Arc<ChainManager>,
    ) -> (Arc<dyn GateBackend>, Arc<OverlayRuntime>) {
        let gate = Arc::new(OverlayGate::new(gate));
        let applied = self.applied();
        chain.append(
            "governance",
            "governance.overlay.applied",
            Some(applied_payload(
                &applied,
                "boot",
                self.user_pin,
                &self.user_pubkey,
            )),
        );
        let rt = Arc::new(OverlayRuntime {
            gate: Arc::clone(&gate),
            cell: self.cell,
            chain,
            nested: self.nested,
            paths: self.paths,
            user_pubkey: self.user_pubkey,
            user_pin: self.user_pin,
            boot: self.effective.clone(),
            state: Mutex::new(State {
                parent: self.parent,
                overlay: self.overlay,
                effective: self.effective,
            }),
        });
        (gate as Arc<dyn GateBackend>, rt)
    }
}

/// Chain payload of an applied change: the hashes plus the trust root in use
/// (`user_pin` says whether a `user.pub` pin backed it).
fn applied_payload(
    a: &Applied,
    source: &str,
    user_pin: bool,
    user_pubkey: &[u8; 32],
) -> serde_json::Value {
    let mut v = a.to_json();
    v["source"] = json!(source);
    v["user_pin"] = json!(user_pin);
    v["user_key_id"] = json!(clawft_types::project::cert::key_id(user_pubkey));
    v
}

struct State {
    parent: ParentPolicy,
    overlay: Overlay,
    effective: Effective,
}

/// The running project governance.
pub struct OverlayRuntime {
    nested: bool,
    gate: Arc<OverlayGate>,
    cell: Arc<RuleHashCell>,
    chain: Arc<ChainManager>,
    paths: RuntimePaths,
    user_pubkey: [u8; 32],
    user_pin: bool,
    boot: Effective,
    state: Mutex<State>,
}

impl OverlayRuntime {
    /// The rules this kernel is running.
    pub fn applied(&self) -> Applied {
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        Applied::of(&s.effective, &self.boot)
    }

    /// The `rule_hash` the chain is stamping now.
    pub fn rule_hash(&self) -> Option<[u8; 32]> {
        self.cell.get()
    }

    /// Re-check that the trust root is unchanged: certificate, expiry,
    /// revocation and the user-key pin (see [`load_user_pubkey`]).
    fn recheck_trust(&self) -> Result<(), OverlayError> {
        let (pk, pinned) = if self.nested {
            nested_pin(&self.paths).map(|pk| (pk, true))?
        } else {
            load_user_pubkey(&self.paths)?
        };
        if self.user_pin && !pinned {
            return Err(OverlayError::Cert(
                "the user.pub pin that was in use is gone".into(),
            ));
        }
        if pk != self.user_pubkey {
            return Err(OverlayError::Cert(
                "the trusted user key changed since boot; restart the kernel".into(),
            ));
        }
        Ok(())
    }

    /// Re-read the overlay (and a newer parent policy, if the file holds one)
    /// and apply. The only path by which a disk edit takes effect. A deleted
    /// overlay file is refused once a non-empty overlay is in force; an empty
    /// file is the deliberate way to clear it.
    pub fn reload(&self) -> Result<Applied, OverlayError> {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let result = (|| {
            self.recheck_trust()?;
            let overlay = match self.paths.overlay() {
                Some(p) if p.exists() => load_overlay(&p)?,
                _ if s.overlay != Overlay::empty() => return Err(OverlayError::OverlayMissing),
                _ => Overlay::empty(),
            };
            let mut parent = s.parent.clone();
            let disk = load_parent_policy(&self.paths.parent_policy())?;
            verify_parent_policy(&disk, &self.user_pubkey)?;
            if self.nested && disk.sig != s.parent.sig {
                return Err(OverlayError::Cert(
                    "nested master policy changes require a new signed boot contract and restart"
                        .into(),
                ));
            }
            // An older (validly signed) file on disk is ignored, not applied.
            if disk.version >= parent.version {
                parent = disk;
            }
            self.apply_locked(&mut s, parent, overlay)
        })();
        self.record(&result, "reload");
        result
    }

    /// Accept a pushed policy: valid user signature, version no older than
    /// the newest accepted, merged with the overlay as last loaded (the file
    /// on disk is not consulted). The project must still be unrevoked and its
    /// certificate unexpired.
    pub fn apply_parent_update(&self, policy: ParentPolicy) -> Result<Applied, OverlayError> {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let result = (|| {
            self.recheck_trust()?;
            verify_parent_policy(&policy, &self.user_pubkey)?;
            if self.nested && policy.sig != s.parent.sig {
                return Err(OverlayError::Cert(
                    "nested master policy changes require a new signed boot contract and restart"
                        .into(),
                ));
            }
            let overlay = s.overlay.clone();
            self.apply_locked(&mut s, policy, overlay)
        })();
        self.record(&result, "parent.update");
        result
    }

    /// The effective rules and engine parameters now in force.
    pub fn effective_rules(&self) -> (Vec<crate::governance::GovernanceRule>, f64, bool) {
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        (
            s.effective.rules.clone(),
            s.effective.risk_threshold(DEFAULT_RISK_THRESHOLD),
            s.effective.human_approval(false),
        )
    }

    /// [`Self::effective_rules`] and the effective hash (hex) from ONE lock
    /// acquisition, so the pair always describes the same generation.
    pub fn effective_rules_and_hash(
        &self,
    ) -> ((Vec<crate::governance::GovernanceRule>, f64, bool), String) {
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        (
            (
                s.effective.rules.clone(),
                s.effective.risk_threshold(DEFAULT_RISK_THRESHOLD),
                s.effective.human_approval(false),
            ),
            Applied::of(&s.effective, &self.boot).effective_hash,
        )
    }

    fn apply_locked(
        &self,
        s: &mut State,
        parent: ParentPolicy,
        overlay: Overlay,
    ) -> Result<Applied, OverlayError> {
        check_version(&parent, Some(s.parent.version))?;
        let effective = merge(&parent, &overlay)?;
        let (threshold, human) = (
            effective.risk_threshold(DEFAULT_RISK_THRESHOLD),
            effective.human_approval(false),
        );
        let mut gate = GovernanceGate::new(threshold, human).with_chain(Arc::clone(&self.chain));
        for r in effective.rules.iter().cloned() {
            gate = gate.add_rule(r);
        }
        if parent.version > s.parent.version {
            write_pin(&self.paths, parent.version)?;
        }
        self.gate.swap(gate, effective.effective_hash, &self.cell);
        let applied = Applied::of(&effective, &self.boot);
        s.parent = parent;
        s.overlay = overlay;
        s.effective = effective;
        Ok(applied)
    }

    fn record(&self, result: &Result<Applied, OverlayError>, source: &str) {
        match result {
            Ok(a) => {
                self.chain.append(
                    "governance",
                    "governance.overlay.applied",
                    Some(applied_payload(a, source, self.user_pin, &self.user_pubkey)),
                );
            }
            Err(e) => {
                self.chain.append(
                    "governance",
                    "governance.overlay.rejected",
                    Some(json!({"source": source, "key": e.key(), "reason": e.to_string()})),
                );
            }
        }
    }

    /// Path of the overlay file this kernel reads on reload.
    pub fn overlay_path(&self) -> Option<PathBuf> {
        self.paths.overlay()
    }
}

// Only the verified nested-user boot path installs this contract. A config
// string cannot select this trust path or turn off certificate checks for projects.
static NESTED_POLICY: std::sync::OnceLock<(RuntimePaths, [u8; 32], ParentPolicy)> =
    std::sync::OnceLock::new();

pub fn install_nested_policy(
    run: PathBuf,
    home: PathBuf,
    id: &str,
    master: [u8; 32],
    policy: ParentPolicy,
) -> Result<(), String> {
    verify_parent_policy(&policy, &master).map_err(|e| e.to_string())?;
    let paths = RuntimePaths::child_at(&run, id, &home)
        .ok_or_else(|| "invalid nested instance id".to_owned())?;
    NESTED_POLICY
        .set((paths, master, policy))
        .map_err(|_| "nested policy already installed".into())
}

fn nested_pin(paths: &RuntimePaths) -> Result<[u8; 32], OverlayError> {
    if paths.revoked_marker().exists() {
        return Err(OverlayError::Cert("nested instance revoked".into()));
    }
    let text = std::fs::read_to_string(paths.root().join(USER_PIN_FILE))
        .map_err(|e| OverlayError::Cert(e.to_string()))?;
    clawft_types::project::canon::hex_decode::<32>(text.trim())
        .ok_or_else(|| OverlayError::Cert("missing or malformed nested master pin".into()))
}

/// The existing overlay runtime handles nested users too: signature verification,
/// version history, limits, gate swap and every event's rule hash are unchanged.
pub fn prepare_nested() -> Result<Option<Prepared>, OverlayError> {
    let Some((paths, master, parent)) = NESTED_POLICY.get() else {
        return Ok(None);
    };
    if nested_pin(paths)? != *master {
        return Err(OverlayError::Cert("nested master pin changed".into()));
    }
    verify_parent_policy(parent, master)?;
    let pinned = read_pin(paths)?;
    check_version(parent, pinned)?;
    let overlay_present = paths.overlay().is_some_and(|p| p.exists());
    let overlay = match paths.overlay() {
        Some(p) => load_overlay(&p)?,
        None => Overlay::empty(),
    };
    let effective = merge(parent, &overlay)?;
    let cell = Arc::new(RuleHashCell::new());
    cell.set(Some(effective.effective_hash));
    Ok(Some(Prepared {
        nested: true,
        paths: paths.clone(),
        user_pubkey: *master,
        user_pin: true,
        parent: parent.clone(),
        overlay,
        overlay_present,
        pinned,
        effective,
        cell,
    }))
}
