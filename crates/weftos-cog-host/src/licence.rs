//! The ADR-106 start-time licence check in weft-cog-host (phase 3).
//!
//! Before every spawn of a Cognitum-origin cog the host asks the local daemon
//! `cog.check_run` (`weftos.cog.v1`) on its Unix socket. A cog is Cognitum-origin
//! when its record says `source: cognitum` or when its bytes are known Cognitum
//! bytes (listed by a held grant, or a revoked artifact hash:
//! [`CognitumRunGate::claims`]), so relabelling a checked-out binary as `local`
//! does not escape the gate. The hashes are computed from the binary file that
//! is about to run, never taken from the record or the install request.
//! Production code does not call `check_run`. A test listener does, so the
//! grant fixtures still have a daemon double.
//!
//! The licence state lives in its own directory ([`default_licence_dir`]):
//!
//! - `config.json`: `{"mesh_id": "<64 hex>"}`, the mesh the Seed is bound to;
//! - `trust.json`: an operator trust file (the `weaver` schema), whose
//!   operator keys verify the binding, approvals and revocation notices;
//! - the kernel's stores: `checkout_grants.json`, `checkout_approvals.json`,
//!   `licence-bound.marker`, and the subject revocation list.
//!
//! Records reach it only as signed records ([`HostLicence::import`]: the
//! binding, grants, approvals and revocation notices), each verified by the
//! kernel code that verifies them on a mesh node.
//!
//! - **No directory**: the gate does not apply (`NotSeedBound`). No socket call.
//! - **Directory present but unusable**: fail closed, `binding_inactive`. No socket call.
//! - **Configured**: the daemon's `cog.check_run` decides. A configured directory
//!   whose answer is `not_seed_bound` is still `binding_inactive` until a binding
//!   is imported. An unreadable revocation list after a permit is `binding_inactive`.
//! - **Daemon failures** use their own start codes: `daemon_unavailable`,
//!   `malformed_reply`, `timeout`, and `version_mismatch`. Grant refusals keep
//!   the kernel spellings (`binding_inactive`, `no_grant`, `grant_lapsed`,
//!   `not_in_grant`, `hash_revoked`, `no_approval`, `not_holder`).
//!
//! A lapse refuses the next start and leaves a running cog alone (ADR-106
//! section 8, soft stop). A revoked artifact hash also stops a running cog
//! ([`CognitumRunGate::revoked`], checked by the supervisor every tick).

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

use clawft_kernel::licence::{
    APPROVALS_FILE, AdmissionPosture, ApprovalStore, BOUND_MARKER, CheckoutGrantStore, Clock, CognitumRunGate,
    GRANTS_FILE, LocalMeshId, MeshId, NoExtraChecks, RunPermit, RunRefusal, RunRequest, RunVerdict, SignedApproval,
    SignedBinding, SignedGrant, system_clock,
};
use weftos_cog_protocol::DEFAULT_TIMEOUT;
use clawft_kernel::mesh_swarm_revoke::{SignedRevocation, verify_revocation};
use clawft_kernel::revocation::RevocationList;
use clawft_kernel::workload_pkg::TrustAnchors;
use serde::{Deserialize, Serialize};

use crate::{CogRecord, Source};

/// Overrides the licence directory.
pub const LICENCE_DIR_ENV: &str = "WEFT_COG_HOST_LICENCE_DIR";
/// `{"mesh_id": "<64 hex>"}`.
pub const CONFIG_FILE: &str = "config.json";
/// Operator trust file.
pub const TRUST_FILE: &str = "trust.json";
/// The kernel revocation list's anchor file; the subject list sits beside it.
const HOSTS_FILE: &str = "revoked_hosts.json";
const SUBJECTS_FILE: &str = weftos_cog_repo::SUBJECTS_FILE_NAME;
const MAX_CONFIG_BYTES: u64 = 256 * 1024;
/// Most records of one kind accepted in one import.
pub const MAX_IMPORT_RECORDS: usize = 512;
/// Largest import body (file or `POST /licence/records`).
pub const MAX_IMPORT_BYTES: usize = 2 * 1024 * 1024;

/// The licence directory: `$WEFT_COG_HOST_LICENCE_DIR`, else `<root>/.licence`
/// (a cog id can never start with a dot, so it cannot collide with a cog).
pub fn default_licence_dir(root: &Path) -> PathBuf {
    match std::env::var(LICENCE_DIR_ENV) {
        Ok(d) if !d.trim().is_empty() => PathBuf::from(d),
        _ => root.join(".licence"),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HostConfig {
    mesh_id: String,
}

struct Stores {
    anchors: Arc<TrustAnchors>,
    grants: CheckoutGrantStore,
    approvals: ApprovalStore,
    revocations: Arc<RevocationList>,
}

enum State {
    /// No licence directory: the gate does not apply.
    Unconfigured,
    /// The directory exists but cannot be used: fail closed.
    Broken(String),
    Ready(Box<Stores>),
}

/// Size and mtime of every file the state is read from, to notice changes
/// made by another process (`weft-cog-host licence import`).
type Fingerprint = Vec<Option<(u64, SystemTime)>>;

/// Prefix on a [`RunRefusal::BindingInactive`] message that is really a
/// transport failure. [`check_start_hashed`] turns it into its own code.
/// The kernel has no variant for these, and this host does not add one.
const TRANSPORT_MARK: &str = "weftos.cog.v1:";

/// The host's licence state and run gate.
pub struct HostLicence {
    dir: PathBuf,
    clock: Clock,
    state: RwLock<(Fingerprint, Arc<State>)>,
    /// Per-instance daemon socket. `None` uses `$WEFTOS_RUNTIME_DIR/kernel.sock`.
    daemon_socket: Option<PathBuf>,
    daemon_timeout: Duration,
}

fn read_capped(path: &Path) -> Result<Vec<u8>, String> {
    let len = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?.len();
    if len > MAX_CONFIG_BYTES {
        return Err(format!("{} is too large", path.display()));
    }
    std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))
}

/// Whether the licence directory is there: `Ok(false)` only for a definite not-found. Any other
/// stat error (permissions, I/O, a dangling symlink loop) is not "absent": the gate must not fail open.
fn dir_present(dir: &Path) -> Result<bool, String> {
    match std::fs::metadata(dir) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // A dangling symlink is NotFound from metadata but is still something put there.
            match std::fs::symlink_metadata(dir) {
                Ok(_) => Err(format!("{}: dangling link", dir.display())),
                Err(e2) if e2.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(e2) => Err(format!("{}: {e2}", dir.display())),
            }
        }
        Err(e) => Err(format!("{}: {e}", dir.display())),
    }
}

fn load(dir: &Path, clock: &Clock) -> State {
    match dir_present(dir) {
        Ok(false) => return State::Unconfigured,
        Err(e) => return State::Broken(e),
        Ok(true) => {}
    }
    let cfg = read_capped(&dir.join(CONFIG_FILE)).and_then(|b| {
        serde_json::from_slice::<HostConfig>(&b).map_err(|e| format!("{CONFIG_FILE}: {e}"))
    });
    let mesh = match cfg.map(|c| MeshId::from_hex(&c.mesh_id)) {
        Ok(Some(m)) => m,
        Ok(None) => return State::Broken(format!("{CONFIG_FILE}: mesh_id must be 64 hex characters")),
        Err(e) => return State::Broken(e),
    };
    let anchors = match read_capped(&dir.join(TRUST_FILE)).and_then(|b| TrustAnchors::from_trust_json(&b)) {
        Ok(a) => Arc::new(a),
        Err(e) => return State::Broken(e),
    };
    let local = LocalMeshId::new(mesh);
    let grants = CheckoutGrantStore::open_or_poisoned(dir, Arc::clone(&anchors), local.clone(), Arc::clone(clock));
    let approvals = ApprovalStore::open_or_poisoned(dir, Arc::clone(&anchors), local);
    let revocations = Arc::new(RevocationList::load(dir.join(HOSTS_FILE)));
    grants.attach_revocations(Arc::clone(&revocations));
    State::Ready(Box::new(Stores { anchors, grants, approvals, revocations }))
}

fn fingerprint(dir: &Path) -> Fingerprint {
    [CONFIG_FILE, TRUST_FILE, GRANTS_FILE, APPROVALS_FILE, BOUND_MARKER, SUBJECTS_FILE]
        .iter()
        .map(|f| dir.join(f))
        .chain([dir.to_path_buf()])
        .map(|p| std::fs::metadata(p).ok().map(|m| (m.len(), m.modified().unwrap_or(SystemTime::UNIX_EPOCH))))
        .collect()
}

/// Signed records for [`HostLicence::import`]. Every one is verified; the
/// sender proves nothing by sending them.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Records {
    /// The operator-signed Seed binding (v2).
    #[serde(default)]
    pub binding: Option<SignedBinding>,
    /// Grants signed by the bound grant key, withdrawals included.
    #[serde(default)]
    pub grants: Vec<SignedGrant>,
    /// Operator hash approvals.
    #[serde(default)]
    pub approvals: Vec<SignedApproval>,
    /// Operator revocation notices (an `artifact_hash` one stops a running cog).
    #[serde(default)]
    pub revocations: Vec<SignedRevocation>,
}

/// What [`HostLicence::import`] did with each record, in order.
#[derive(Debug, Clone, Serialize)]
pub struct ImportLine {
    /// `binding`, `grant`, `approval` or `revocation`.
    pub kind: &'static str,
    /// `applied`, `duplicate`, `ignored`, `applied_unsaved` or `refused`.
    pub outcome: String,
    /// Why it was refused.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

fn line(kind: &'static str, r: Result<String, String>) -> ImportLine {
    match r {
        Ok(outcome) => ImportLine { kind, outcome, error: None },
        Err(e) => ImportLine { kind, outcome: "refused".into(), error: Some(e) },
    }
}

fn outcome<E: std::fmt::Display>(r: Result<clawft_kernel::licence::Outcome, E>) -> Result<String, String> {
    use clawft_kernel::licence::Outcome as O;
    r.map(|o| match o {
        O::Applied => "applied",
        O::AppliedUnsaved => "applied_unsaved",
        O::Duplicate => "duplicate",
        O::Ignored => "ignored",
    }
    .to_string())
    .map_err(|e| e.to_string())
}

impl HostLicence {
    /// Open the licence state in `dir` with the system clock.
    pub fn open(dir: PathBuf) -> Self {
        Self::with_clock(dir, system_clock())
    }

    /// [`Self::open`] with an explicit clock (tests).
    pub fn with_clock(dir: PathBuf, clock: Clock) -> Self {
        let fp = fingerprint(&dir);
        let state = Arc::new(load(&dir, &clock));
        Self {
            dir,
            clock,
            state: RwLock::new((fp, state)),
            daemon_socket: None,
            daemon_timeout: DEFAULT_TIMEOUT,
        }
    }

    /// Ask this socket instead of `$WEFTOS_RUNTIME_DIR/kernel.sock`.
    /// Tests pass a private path so they do not share a process-global socket.
    pub fn with_daemon_socket(mut self, socket: impl Into<PathBuf>) -> Self {
        self.daemon_socket = Some(socket.into());
        self
    }

    /// How long one `cog.check_run` waits.
    pub fn with_daemon_timeout(mut self, timeout: Duration) -> Self {
        self.daemon_timeout = timeout;
        self
    }

    /// The licence directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn state(&self) -> Arc<State> {
        Arc::clone(&self.state.read().unwrap_or_else(|p| p.into_inner()).1)
    }

    /// Re-read the state when another process changed its files. Cheap (a
    /// few `stat` calls); the supervisor calls it every tick.
    pub fn refresh(&self) {
        let fp = fingerprint(&self.dir);
        let mut g = self.state.write().unwrap_or_else(|p| p.into_inner());
        if g.0 != fp {
            *g = (fp, Arc::new(load(&self.dir, &self.clock)));
        }
    }

    /// Verify and apply signed records: revocations, then the binding, then
    /// grants, then approvals. Each record is judged on its own.
    pub fn import(&self, recs: &Records) -> Result<Vec<ImportLine>, String> {
        if recs.grants.len() > MAX_IMPORT_RECORDS
            || recs.approvals.len() > MAX_IMPORT_RECORDS
            || recs.revocations.len() > MAX_IMPORT_RECORDS
        {
            return Err(format!("at most {MAX_IMPORT_RECORDS} records of each kind per import"));
        }
        if !dir_present(&self.dir)? {
            return Err(format!(
                "no licence directory at {}: create it with {CONFIG_FILE} and {TRUST_FILE} first",
                self.dir.display()
            ));
        }
        self.refresh();
        let state = self.state();
        let s = match &*state {
            State::Ready(s) => s,
            State::Broken(e) => return Err(format!("licence state unusable: {e}")),
            State::Unconfigured => return Err("licence state is not configured".into()),
        };
        let mut out = Vec::new();
        for r in &recs.revocations {
            let res = verify_revocation(r, &s.anchors).map_err(|e| e.to_string()).and_then(|n| {
                s.revocations
                    .revoke_subject_by(n.kind, &n.id, &n.reason, "operator-notice")
                    .map(|added| if added { "applied" } else { "duplicate" }.to_string())
                    .map_err(|e| e.to_string())
            });
            out.push(line("revocation", res));
        }
        if let Some(b) = &recs.binding {
            // The host consumes the binding to verify grants; it admits no
            // mesh peers, so the admission posture is not its to check.
            let posture = AdmissionPosture { enforce: true, verdict_source_bound: true, open_membership: false };
            out.push(line("binding", outcome(s.grants.accept_binding(b, posture, &NoExtraChecks))));
        }
        for g in &recs.grants {
            out.push(line("grant", outcome(s.grants.accept_grant(g))));
        }
        for a in &recs.approvals {
            out.push(line("approval", outcome(s.approvals.accept(a))));
        }
        drop(state);
        self.refresh();
        Ok(out)
    }

    /// The state for `GET /licence` and `weft-cog-host licence status`.
    pub fn status(&self) -> serde_json::Value {
        match &*self.state() {
            State::Unconfigured => serde_json::json!({"state": "unconfigured", "dir": self.dir.display().to_string()}),
            State::Broken(e) => serde_json::json!({"state": "broken", "dir": self.dir.display().to_string(), "error": e}),
            State::Ready(s) => serde_json::json!({
                "state": "ready",
                "dir": self.dir.display().to_string(),
                "mesh_id": s.grants.local_mesh_id().get().map(|m| m.to_hex()),
                "binding": s.grants.held_binding(),
                "binding_in_effect": s.grants.binding_status().is_ok(),
                "store_error": s.grants.poisoned().or_else(|| s.approvals.poisoned()),
                "revocations_error": s.revocations.subjects_error(),
                "grants": s.grants.grant_rows(),
                "approvals": s.approvals.rows(),
                "revoked_artifacts": s.revocations
                    .list_subjects(Some(clawft_kernel::revocation::RevocationKind::ArtifactHash))
                    .len(),
            }),
        }
    }
}

/// A transport code stashed in [`RunRefusal::BindingInactive`], if this is one.
fn transport_parts(e: &RunRefusal) -> Option<(&'static str, String)> {
    let RunRefusal::BindingInactive(msg) = e else { return None };
    let rest = msg.strip_prefix(TRANSPORT_MARK)?;
    let (code, detail) = rest.split_once(':')?;
    let code = match code {
        "daemon_unavailable" => "daemon_unavailable",
        "malformed_reply" => "malformed_reply",
        "timeout" => "timeout",
        "version_mismatch" => "version_mismatch",
        _ => return None,
    };
    Some((code, detail.to_string()))
}

impl CognitumRunGate for HostLicence {
    fn check(&self, req: &RunRequest<'_>) -> Result<RunVerdict, RunRefusal> {
        match &*self.state() {
            State::Unconfigured => Ok(RunVerdict::NotSeedBound),
            State::Broken(e) => Err(RunRefusal::BindingInactive(format!("licence state unusable: {e}"))),
            State::Ready(s) => self.check_ready(s, req),
        }
    }

    fn claims(&self, sha256: &str, blake3: &str) -> bool {
        match &*self.state() {
            State::Ready(s) => s.grants.claims_artifact(sha256, blake3) || s.grants.is_hash_revoked(blake3),
            _ => false,
        }
    }

    fn revoked(&self, blake3: &str) -> bool {
        match &*self.state() {
            State::Ready(s) => s.grants.is_hash_revoked(blake3),
            _ => false,
        }
    }
}

/// The (sha256, BLAKE3) of a binary, lower-case hex, from its bytes.
pub fn hashes(bytes: &[u8]) -> (String, String) {
    (weftos_cog_repo::sha256_hex(bytes), blake3::hash(bytes).to_hex().to_string())
}

/// A start the gate refused. `code` is a kernel refusal or a transport code
/// (`daemon_unavailable`, `malformed_reply`, `timeout`, `version_mismatch`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartRefused {
    /// Stable code. Transport codes are not kernel [`RunRefusal`] variants.
    pub code: &'static str,
    /// The full reason, with the remedy.
    pub reason: String,
}

impl std::fmt::Display for StartRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason)
    }
}

/// What a permitted Cognitum start ran, for the revocation sweep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LicensedStart {
    /// BLAKE3 of the bytes that were started.
    pub blake3: String,
    /// The grant and approval that covered them.
    pub permit: RunPermit,
}

/// Decide whether `rec` may start with these `bytes` (read from the file that
/// will run). `Ok(None)`: not a Cognitum-origin cog, or the gate does not
/// apply. Only `gate` decides; nothing in the cog's bytes or arguments does.
pub fn check_start(gate: &dyn CognitumRunGate, rec: &CogRecord, bytes: &[u8]) -> Result<Option<LicensedStart>, StartRefused> {
    let (sha256, blake3) = hashes(bytes);
    check_start_hashed(gate, rec, &sha256, &blake3)
}

/// [`check_start`] for hashes already computed from the bytes that will run.
pub fn check_start_hashed(gate: &dyn CognitumRunGate, rec: &CogRecord, sha256: &str, blake3: &str) -> Result<Option<LicensedStart>, StartRefused> {
    if rec.source != Source::Cognitum && !gate.claims(sha256, blake3) {
        return Ok(None);
    }
    let req = RunRequest { cog_id: &rec.id, version: &rec.version, sha256, blake3 };
    match gate.check(&req) {
        Ok(RunVerdict::NotSeedBound) => Ok(None),
        Ok(RunVerdict::Permit(permit)) => Ok(Some(LicensedStart { blake3: blake3.to_string(), permit })),
        Err(e) => Err(start_refused(&e, rec, sha256)),
    }
}

fn start_refused(e: &RunRefusal, rec: &CogRecord, sha256: &str) -> StartRefused {
    let (code, shown, remedy) = match transport_parts(e) {
        Some((code, detail)) => (
            code,
            detail,
            "start the local WeftOS daemon and retry the licence check".to_string(),
        ),
        None => (e.code(), e.to_string(), e.remedy(&rec.id, &rec.version)),
    };
    StartRefused {
        code,
        reason: format!(
            "licence run gate: [{code}] {shown} ({} {}, sha256 {}); remedy: {remedy}",
            rec.id,
            rec.version,
            &sha256[..sha256.len().min(16)]
        ),
    }
}

#[path = "licence_check.rs"]
mod licence_check;

#[cfg(all(test, unix))]
#[path = "licence_test_gate.rs"]
mod test_gate;

#[cfg(all(test, unix))]
#[path = "licence_daemon_tests.rs"]
mod daemon_tests;

#[cfg(test)]
#[path = "licence_tests.rs"]
mod tests;
