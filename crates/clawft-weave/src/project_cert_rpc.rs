//! Project key certificates: `project.cert.show`, `project.cert.challenge`,
//! `project.rekey`, `project.revoke`, and [`issue_for_register`] for `mesh.register`
//! (ADR-103 A7, Phase 2 package C).
//!
//! Every route is `Admin` and user-daemon only. A `wft_` token is refused
//! (as in `token_rpc`): the TCP relay strips literal scopes, so a relayed
//! caller can only present a token, and a token must not certify keys.
//! A daemon that is not the `--profile user` daemon refuses too, so a
//! project kernel can never mint certificates with its own key.
//!
//! The user chain (source `user.projects`, reserved: the `chain.append`
//! RPC refuses it) holds `project.register`, `project.rekey` and
//! `project.revoke` events: certificates (public keys and signatures) and
//! metadata, never private bytes. The chain is saved only on clean
//! shutdown, so every operation first appends to the fsynced identity
//! journal (`<manifests>/identity.journal.jsonl`) under an exclusive
//! `flock`, and [`current_view`] merges journal + `<id>.cert.json` files +
//! chain into the one [`RevocationView`] used for every decision. A journal
//! that exists but does not parse stops issuing and verifying (fail closed).
//!
//! Nonces are issued by the daemon ([`issue_challenge`]), single use and
//! short lived; [`claim_nonce`] turns one into a [`DaemonNonce`], the only
//! thing [`issue_for_register`] and `project.rekey` accept, so a replayed or
//! invented nonce cannot reach the PoP check. Package H's `mesh.challenge`
//! must call [`issue_challenge`] and its `mesh.register` [`claim_nonce`]; H
//! also verifies the spawn nonce. TOFU limits are stated in
//! `clawft_kernel::project_identity`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Utc};
use clawft_kernel::chain::ChainManager;
use clawft_kernel::project_identity::{
    self as ident, IdentityError, IdentityJournal, JournalRecord, KIND_REGISTER, KIND_REKEY,
    KIND_REVOKE, Registration, SOURCE,
};
use clawft_kernel::token_authority::SECRET_PREFIX;
use clawft_rpc::Response;
use clawft_types::project::cert::{PopOp, ProjectCert, key_id};
use clawft_types::project::{CertRequest, ProjectError, find_by_id, validate_id};
use ed25519_dalek::SigningKey;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::rpc_ext::{ExtCall, ExtCtx, ExtFuture};

#[path = "project_cert_store.rs"]
mod store;
use store::current_view_locked;
#[cfg(test)]
use store::read_cert_files as store_read_certs;
pub use store::{current_view, repair};

static PROJECT_CLAIMS_CHANGED: OnceLock<tokio::sync::Notify> = OnceLock::new();
type ClaimSyncReply = tokio::sync::oneshot::Sender<Result<(), String>>;
type ClaimSyncSender = tokio::sync::mpsc::UnboundedSender<ClaimSyncReply>;
static CLAIM_SYNC: OnceLock<std::sync::Mutex<ClaimSyncState>> = OnceLock::new();
static CLAIM_SYNC_GENERATION: AtomicU64 = AtomicU64::new(1);

#[derive(Default)]
struct ClaimSyncState {
    /// Set as soon as the daemon selects service mode. A missing sender may
    /// mean the first session is not polling yet, or that an old machine
    /// registration still has project claims pending cleanup.
    service_required: bool,
    active: Option<(u64, ClaimSyncSender)>,
}

pub(crate) fn project_claims_changed() -> &'static tokio::sync::Notify {
    PROJECT_CLAIMS_CHANGED.get_or_init(tokio::sync::Notify::new)
}

fn claim_sync_slot() -> &'static std::sync::Mutex<ClaimSyncState> {
    CLAIM_SYNC.get_or_init(|| std::sync::Mutex::new(ClaimSyncState::default()))
}

fn claim_sync_sender(state: &ClaimSyncState) -> Result<Option<ClaimSyncSender>, String> {
    match &state.active {
        Some((_, sender)) => Ok(Some(sender.clone())),
        None if state.service_required => Err("project claim service session is unavailable; machine cleanup is not confirmed".into()),
        None => Ok(None), // collapsed or never connected: no service claims exist
    }
}

/// Installed only while the mesh-local session can apply and acknowledge
/// project address changes. A stale session cannot clear its successor.
pub(crate) struct ClaimSyncLease(u64);

impl Drop for ClaimSyncLease {
    fn drop(&mut self) {
        let mut slot = claim_sync_slot().lock().expect("claim sync lock");
        if slot.active.as_ref().is_some_and(|(generation, _)| *generation == self.0) { slot.active = None; }
    }
}

pub(crate) fn install_claim_sync(sender: ClaimSyncSender) -> ClaimSyncLease {
    let generation = CLAIM_SYNC_GENERATION.fetch_add(1, Ordering::Relaxed);
    let mut slot = claim_sync_slot().lock().expect("claim sync lock");
    slot.service_required = true;
    slot.active = Some((generation, sender));
    ClaimSyncLease(generation)
}

/// Called before publishing a service-mode boot result, so a project RPC
/// cannot succeed in the gap before the link task installs its ACK channel.
pub(crate) fn require_claim_sync() {
    claim_sync_slot().lock().expect("claim sync lock").service_required = true;
}

/// A journal mutation may return success only after the connected machine
/// service has acknowledged the new current address set. A session that was
/// connected but is now unavailable may still have claims pending cleanup on
/// the machine, so its absence must report an incomplete mutation.
pub(crate) async fn sync_claims_barrier() -> Result<(), String> {
    let sender = claim_sync_sender(&claim_sync_slot().lock().expect("claim sync lock"))?;
    let Some(sender) = sender else { return Ok(()); };
    await_claim_sync(sender).await
}

async fn await_claim_sync(sender: ClaimSyncSender) -> Result<(), String> {
    let (reply, received) = tokio::sync::oneshot::channel();
    sender.send(reply).map_err(|_| "project claim session ended before sync".to_string())?;
    tokio::time::timeout(std::time::Duration::from_secs(5), received)
        .await.map_err(|_| "project claim sync did not acknowledge within five seconds".to_string())?
        .map_err(|_| "project claim session ended before acknowledgement".to_string())?
}

#[cfg(test)]
mod claim_sync_tests {
    use super::*;

    #[tokio::test]
    async fn mutation_barrier_waits_for_machine_ack_and_propagates_failure() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let first = tokio::spawn(await_claim_sync(tx.clone()));
        let ack = rx.recv().await.expect("sync request");
        assert!(!first.is_finished(), "journal mutation must wait for service ACK");
        ack.send(Ok(())).unwrap();
        assert!(first.await.unwrap().is_ok());

        let second = tokio::spawn(await_claim_sync(tx));
        rx.recv().await.unwrap().send(Err("machine refused address.remove".into())).unwrap();
        assert!(second.await.unwrap().unwrap_err().contains("machine refused"));
    }

    #[test]
    fn disconnect_gap_is_incomplete_until_successor_installs() {
        let mut state = ClaimSyncState::default();
        assert!(claim_sync_sender(&state).unwrap().is_none(), "collapsed or never-service has no claims");
        let (old, _) = tokio::sync::mpsc::unbounded_channel();
        state.service_required = true; // selected service mode, before first session poll
        assert!(claim_sync_sender(&state).expect_err("first-session gap must fail closed").contains("cleanup is not confirmed"));
        state.active = Some((1, old));
        assert!(claim_sync_sender(&state).unwrap().is_some());
        state.active = None; // lease dropped before remote connection cleanup
        assert!(claim_sync_sender(&state).expect_err("disconnect must fail closed").contains("cleanup is not confirmed"));
        let (new, _) = tokio::sync::mpsc::unbounded_channel();
        state.active = Some((2, new));
        assert!(claim_sync_sender(&state).unwrap().is_some());
    }
}

#[path = "project_cert_nonce.rs"]
mod nonce;
pub use nonce::{CHALLENGE_TTL, DaemonNonce, claim_nonce, issue_challenge};

const MAX_REASON: usize = 256;

/// What the user daemon recorded about the child it spawned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnInfo {
    /// Child pid AS CLAIMED by the child in `mesh.register` (the spawn
    /// ledger checks it against the supervisor's pid when one is recorded,
    /// but the daemon does not otherwise verify it). Recorded on the chain
    /// as `claimed_pid`.
    pub pid: u32,
    /// SHA-256 of the child executable, hex.
    pub exe_sha: String,
}

/// A request to certify a project key (from `mesh.register`).
#[derive(Debug)]
pub struct RegisterRequest {
    /// Project ULID.
    pub project_id: String,
    /// The project's public key.
    pub project_pubkey: [u8; 32],
    /// [`root_sha256`] of the project root the child claims.
    pub root_sha256: String,
    /// Spawn facts recorded on the chain.
    pub spawn: SpawnInfo,
    /// The daemon-issued challenge nonce, claimed with [`claim_nonce`].
    pub nonce: DaemonNonce,
    /// Proof of possession: [`ident::pop_sign`] with [`PopOp::Register`]
    /// and the user key id over this nonce and project id.
    pub pop_sig: [u8; 64],
}

/// A certificate and whether this call created it.
#[derive(Debug, Clone)]
pub struct Issued {
    /// The certificate in force.
    pub cert: ProjectCert,
    /// False when the key was already certified (idempotent re-register).
    pub new: bool,
}

/// Why an operation was refused; `kind()` is the RPC `error_kind`.
#[derive(Debug, thiserror::Error)]
pub enum IssueError {
    /// Not the user daemon, or it has no chain/key.
    #[error("{0}")]
    Unavailable(String),
    /// Malformed parameter.
    #[error("{0}")]
    Invalid(String),
    /// The id is not in the manifest store.
    #[error("project {0} is not registered")]
    NotRegistered(String),
    /// The claimed root is not the registered root. The child must send the
    /// canonical root path (what `project.register` stored).
    #[error("root does not match the registered project root")]
    RootMismatch,
    /// Proof of possession failed or its nonce was used before.
    #[error("{0}")]
    Pop(String),
    /// Identity rule (TOFU conflict, revoked, not bound, bad cert).
    #[error(transparent)]
    Identity(#[from] IdentityError),
    /// The user-key rotation log is unreadable, broken or does not end at the
    /// key in use (ADR-103 A13); nothing is verified until it is repaired.
    #[error(transparent)]
    Rotation(#[from] ident::RotationError),
    /// Manifest store failure.
    #[error("{0}")]
    Store(String),
    /// The identity change is journalled and on the chain (it is in force),
    /// but a later step failed (the cert file). The after-hooks (marker,
    /// child stop) still run; the caller is told it is incomplete.
    #[error("{0}")]
    Incomplete(String),
}

impl IssueError {
    /// Stable snake_case discriminator for clients.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Unavailable(_) => "cert_unavailable",
            Self::Invalid(_) => "invalid_params",
            Self::NotRegistered(_) => "project_not_found",
            Self::RootMismatch => "root_mismatch",
            Self::Pop(_) => "pop_failed",
            Self::Identity(IdentityError::KeyConflict { .. }) => "key_conflict",
            Self::Identity(IdentityError::KeyRevoked { .. }) => "key_revoked",
            Self::Identity(IdentityError::KeyReuse { .. }) => "key_reuse",
            Self::Identity(IdentityError::JournalCorrupt { .. }) => "journal_corrupt",
            Self::Identity(IdentityError::NotBound(_)) => "not_certified",
            Self::Identity(IdentityError::ProjectRevoked(_)) => "project_revoked",
            Self::Identity(_) => "cert_error",
            Self::Rotation(_) => "user_key_history_invalid",
            Self::Store(_) => "project_store_error",
            Self::Incomplete(_) => "identity_change_incomplete",
        }
    }

    fn response(&self) -> Response {
        Response::error_with_kind(self.kind(), self.to_string())
    }
}

/// Everything the operations need, injectable for tests.
pub struct CertEnv {
    /// The user chain.
    pub chain: Arc<ChainManager>,
    /// The user key (the user chain's signing key).
    pub user_key: SigningKey,
    /// `~/.weftos/projects`.
    pub manifests_dir: PathBuf,
}

/// Hex SHA-256 of the root path's raw bytes (`OsStr` bytes on unix, so two
/// distinct non-UTF-8 paths never collide): how a project root is named in
/// `project.register` and in [`RegisterRequest::root_sha256`]. The child
/// must hash the canonical root, exactly as stored in the manifest.
pub fn root_sha256(root: &Path) -> String {
    #[cfg(unix)]
    let bytes = {
        use std::os::unix::ffi::OsStrExt;
        root.as_os_str().as_bytes().to_vec()
    };
    #[cfg(not(unix))]
    let bytes = root.to_string_lossy().into_owned().into_bytes();
    ident::hex(&Sha256::digest(bytes))
}

/// Run root pinned by [`init_run_root`] (tests that host the user daemon's
/// RPCs in-process; a real daemon never sets it).
static RUN_ROOT: std::sync::RwLock<Option<PathBuf>> = std::sync::RwLock::new(None);

/// Pin the user daemon's run root for this process, as
/// `project_rpc::init_manifests_dir` pins the manifest store.
pub fn init_run_root(dir: PathBuf) {
    *RUN_ROOT.write().expect("run root lock poisoned") = Some(dir);
}

/// The user daemon's run root, the parent of every child's run dir, never
/// derived from the manifest store (which can live elsewhere): the running
/// supervisor's (which `post_boot` takes from here), else the pinned root,
/// else, in the user daemon, `RuntimePaths::resolve().root()`
/// (`$WEFTOS_RUNTIME_DIR`, else `~/.weftos/run`). `None` outside the user
/// daemon (no children, nothing to mark). Tests that host the user daemon
/// in process pin the root ([`init_run_root`],
/// `user_daemon::enter_at`) so nothing resolves the real `~/.weftos/run`.
pub fn user_run_root() -> Option<PathBuf> {
    #[cfg(all(unix, feature = "exochain", feature = "placement"))]
    if let Some(sup) = crate::project_supervisor::global() {
        return Some(sup.config().run_root.clone());
    }
    if let Some(dir) = RUN_ROOT.read().expect("run root lock poisoned").clone() {
        return Some(dir);
    }
    if !clawft_types::runtime_paths::user_profile_active() {
        return None;
    }
    // Unit tests never resolve the real `~/.weftos/run`; they pin the root.
    if cfg!(test) && clawft_types::runtime_paths::runtime_dir_env().is_none() {
        return None;
    }
    Some(clawft_types::runtime_paths::RuntimePaths::resolve().root().to_path_buf())
}

/// The marker a revoked project's child refuses to run under,
/// `<run_root>/<id>/revoked` via the one derivation
/// (`clawft_types::runtime_paths::revoked_marker`) the supervisor and the
/// child (`RuntimePaths::revoked_marker`) use too.
pub fn revoked_marker_path(id: &str) -> Result<PathBuf, MarkerError> {
    marker_path_under(user_run_root().as_deref(), id)
}

fn marker_path_under(root: Option<&Path>, id: &str) -> Result<PathBuf, MarkerError> {
    let root = root.ok_or(MarkerError::NoRunRoot)?;
    clawft_types::runtime_paths::revoked_marker(root, id).ok_or_else(|| MarkerError::UnsafeId(id.to_owned()))
}

/// Why the terminal `revoked` marker was not written. The revocation itself
/// (journal, chain, session, child stop) has happened; the marker is what a
/// child that cannot reach the parent sees, so its absence is reported to
/// the caller (`revoke_marker_unwritten`), never only logged.
#[derive(Debug, thiserror::Error)]
pub enum MarkerError {
    /// Not the user daemon: no run root to put the marker in.
    #[error("no user-daemon run root, so the revoked marker was not written")]
    NoRunRoot,
    /// The id is not a single safe path component.
    #[error("project id {0:?} is not a safe path component, so the revoked marker was not written")]
    UnsafeId(String),
    /// Creating the run dir or writing the file failed.
    #[error("could not write the revoked marker {path}: {reason}")]
    Io {
        /// The marker path.
        path: PathBuf,
        /// The error.
        reason: String,
    },
}

impl MarkerError {
    /// RPC error kind of every marker failure.
    pub const KIND: &'static str = "revoke_marker_unwritten";

    /// The refusal a revoke returns when its marker could not be written.
    pub fn response(&self, id: &str) -> Response {
        Response::error_with_kind(
            Self::KIND,
            format!(
                "project {id} is revoked (journal, chain, session and child stop are done) but {self}; \
                 fix the cause and write the marker by hand, or the child can still boot while the user daemon is unreachable"
            ),
        )
    }
}

/// `project.revoke` is terminal for the id: drop the session (a running
/// child's next heartbeat fails and its re-register is refused `key_revoked`)
/// and, from [`on_identity_change`], write the marker (atomic, 0600; the run dir is created 0700 when the
/// child is stopped, so a stopped child is marked too). Nothing here clears
/// it: delete `<run_root>/<id>/revoked` (`~/.weftos/run/<id>/revoked` by default) by hand after re-enrolling the
/// project. A marker that cannot be written fails the RPC
/// (`revoke_marker_unwritten`) after the revocation itself is done: the
/// journal is the authority, the marker is what lets a child that cannot
/// reach the parent still see the revocation.
fn mark_revoked(_env: &CertEnv, id: &str, _why: &str) {
    // The marker itself is written by [`on_identity_change`] (package G, the
    // single writer) once the journal write has succeeded.
    #[cfg(unix)]
    crate::mesh_local_registry::registry().evict(id);
    #[cfg(not(unix))]
    let _ = id;
}

/// Write the terminal `revoked` marker (atomic, 0600; the run dir is created
/// 0700 when the child is stopped, so a stopped child is marked too) and
/// return its path. Fails closed: every reason it was not written is an
/// error the revoke RPC returns. Called only from [`on_identity_change`].
fn write_revoked_marker(id: &str, why: &str) -> Result<PathBuf, MarkerError> {
    write_marker_under(user_run_root().as_deref(), id, why)
}

fn write_marker_under(root: Option<&Path>, id: &str, why: &str) -> Result<PathBuf, MarkerError> {
    let path = marker_path_under(root, id)?;
    let io = |reason: String| MarkerError::Io { path: path.clone(), reason };
    if let Some(dir) = path.parent() {
        let mut b = std::fs::DirBuilder::new();
        b.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            b.mode(0o700);
        }
        b.create(dir).map_err(|e| io(format!("run dir {}: {e}", dir.display())))?;
    }
    ident::write_private_atomic(&path, format!("{why}\n").as_bytes(), false).map_err(|e| io(e.to_string()))?;
    Ok(path)
}

/// `project.rekey` replaces the key: the old key's session is dropped (its
/// re-register is refused `key_revoked`) but NO marker is written, because
/// the rekeyed child must be able to boot under its new certificate.
fn drop_session(id: &str) {
    #[cfg(unix)]
    crate::mesh_local_registry::registry().evict(id);
    #[cfg(not(unix))]
    let _ = id;
}

fn cert_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.cert.json"))
}

fn write_cert_file(dir: &Path, cert: &ProjectCert) -> Result<(), IssueError> {
    let path = cert_path(dir, &cert.project_id);
    let text = serde_json::to_vec_pretty(cert).map_err(|e| IssueError::Store(e.to_string()))?;
    if std::fs::read(&path).is_ok_and(|b| b == text) {
        return Ok(());
    }
    ident::write_private_atomic(&path, &text, false)?;
    Ok(())
}

fn clean_reason(v: Option<&Value>) -> String {
    v.and_then(Value::as_str)
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_REASON)
        .collect()
}

fn registered_manifest(
    env: &CertEnv,
    id: &str,
) -> Result<clawft_types::project::ProjectManifest, IssueError> {
    validate_id(id).map_err(|_| IssueError::Invalid("project id is not a canonical ULID".into()))?;
    find_by_id(&env.manifests_dir, id)
        .map_err(|e: ProjectError| IssueError::Store(e.to_string()))?
        .ok_or_else(|| IssueError::NotRegistered(id.to_owned()))
}

fn verify_pop(
    env: &CertEnv,
    op: PopOp,
    pubkey: &[u8; 32],
    nonce: &DaemonNonce,
    project_id: &str,
    sig: &[u8; 64],
) -> Result<(), IssueError> {
    if nonce.project_id() != project_id {
        return Err(IssueError::Pop("nonce was issued for another project".into()));
    }
    let uk = key_id(&user_pubkey(env));
    ident::pop_verify(pubkey, op, &uk, nonce.as_str(), project_id, sig)
        .map_err(|e| IssueError::Pop(e.to_string()))
}

fn user_pubkey(env: &CertEnv) -> [u8; 32] {
    env.user_key.verifying_key().to_bytes()
}

/// The user keys this daemon trusts for material it sealed earlier: the key in
/// use plus every key it replaced, from `<manifests>/user-key-rotations.jsonl`
/// (ADR-103 A13). An empty log is a single key; a log that is unreadable,
/// broken or does not end at the key in use is an error (fail closed).
pub fn user_history(env: &CertEnv) -> Result<ident::UserKeyHistory, IssueError> {
    Ok(ident::RotationLog::new(&env.manifests_dir).history(&user_pubkey(env))?)
}

/// The rotation records the user chain holds, oldest first, each verified
/// (both signatures) and one per `seq`.
fn chain_records(env: &CertEnv) -> Vec<ident::RotationRecord> {
    let mut by_seq = std::collections::BTreeMap::new();
    for e in env.chain.tail_from(0).iter().filter(|e| e.source == SOURCE && e.kind == ident::KIND_ROTATED) {
        let rec = e
            .payload
            .as_ref()
            .and_then(|p| p.get("record"))
            .and_then(|r| serde_json::from_value::<ident::RotationRecord>(r.clone()).ok())
            .filter(|r| r.verify().is_ok());
        if let Some(r) = rec {
            by_seq.entry(r.seq).or_insert(r);
        }
    }
    by_seq.into_values().collect()
}

/// Keep the rotation log and the user chain in agreement (ADR-103 A13).
///
/// * A record in the log the chain does not hold yet is appended as a
///   `user.key.rotated` event (the offline `weaver migrate user-key --rotate`
///   cannot open the chain). Idempotent.
/// * A log that is missing or shorter than the chain's records (deleted, or
///   truncated, which would silently downgrade verification to the key in
///   use) is rebuilt from the chain's verified records, provided they form a
///   history ending at the key in use; otherwise the error names the cause.
///
/// Returns how many events were appended.
pub fn chain_rotations(env: &CertEnv) -> Result<usize, IssueError> {
    let log = ident::RotationLog::new(&env.manifests_dir);
    let mut records = log.read()?;
    let on_chain = chain_records(env);
    if on_chain.len() > records.len() {
        let end = user_pubkey(env);
        ident::UserKeyHistory::from_records(&end, &on_chain)
            .map_err(|e| IssueError::Store(format!("the user-key rotation log is missing or short and the chain's records do not rebuild it: {e}")))?;
        log.replace(&on_chain)?;
        tracing::warn!(restored = on_chain.len(), "rebuilt the user-key rotation log from the user chain");
        records = on_chain;
    }
    let chained: std::collections::HashSet<String> = chain_records(env).iter().map(|r| r.hash()).collect();
    let mut n = 0;
    for r in records.iter().filter(|r| !chained.contains(&r.hash())) {
        env.chain.append(SOURCE, ident::KIND_ROTATED, Some(json!({ "record": r, "record_hash": r.hash() })));
        n += 1;
    }
    Ok(n)
}

/// Certify the key in `req` for `project.register`. Refuses an id that is
/// not in the manifest store, a root that is not that project's registered
/// root, a key the caller cannot prove (PoP over a daemon-issued nonce), a
/// second key for a certified id, a key used by another project, and a
/// revoked key. Idempotent for the certified key.
pub fn register(env: &CertEnv, req: RegisterRequest, now: DateTime<Utc>) -> Result<Issued, IssueError> {
    let manifest = registered_manifest(env, &req.project_id)?;
    if req.root_sha256 != root_sha256(&manifest.root) {
        return Err(IssueError::RootMismatch);
    }
    verify_pop(env, PopOp::Register, &req.project_pubkey, &req.nonce, &req.project_id, &req.pop_sig)?;
    let journal = IdentityJournal::new(&env.manifests_dir);
    let lock = journal.lock()?;
    let view = current_view_locked(env, &lock)?;
    match view.plan_registration(&req.project_id, &req.project_pubkey)? {
        Registration::Existing(cert) => {
            let history = user_history(env)?;
            let current = key_id(&user_pubkey(env));
            if cert.user_key_id == current {
                ident::verify_cert_at(&cert, &user_pubkey(env), now).map_err(IdentityError::from)?;
                write_cert_file(&env.manifests_dir, &cert)?;
                return Ok(Issued { cert: *cert, new: false });
            }
            // Sealed by a user key that has since been rotated out: it still
            // verifies (dated before the rotation point), but the child pins
            // the key in use, so certify the same project key again under the
            // current user key with the next serial (ADR-103 A13).
            ident::verify_cert_historic(&cert, &history)?;
            let renewed = ident::sign_cert(
                &env.user_key,
                &CertRequest {
                    project_id: req.project_id.clone(),
                    project_pubkey: req.project_pubkey,
                    serial: view.last_serial(&req.project_id) + 1,
                    issued_at: now,
                    expires_at: None,
                },
            )?;
            journal.append(&lock, &JournalRecord::Register { cert: renewed.clone() })?;
            env.chain.append(
                SOURCE,
                KIND_REGISTER,
                Some(json!({
                    "cert": renewed,
                    "renewal": true,
                    "replaces_serial": cert.serial,
                    "replaces_user_key_id": cert.user_key_id,
                })),
            );
            write_cert_file(&env.manifests_dir, &renewed)?;
            Ok(Issued { cert: renewed, new: true })
        }
        Registration::New { serial } => {
            let cert = ident::sign_cert(
                &env.user_key,
                &CertRequest {
                    project_id: req.project_id.clone(),
                    project_pubkey: req.project_pubkey,
                    serial,
                    issued_at: now,
                    expires_at: None,
                },
            )?;
            journal.append(&lock, &JournalRecord::Register { cert: cert.clone() })?;
            env.chain.append(
                SOURCE,
                KIND_REGISTER,
                Some(json!({
                    "cert": cert,
                    "name": manifest.name,
                    "root_sha256": req.root_sha256,
                    "manifest_schema": manifest.schema_version,
                    "spawn": { "claimed_pid": req.spawn.pid, "exe_sha": req.spawn.exe_sha },
                })),
            );
            write_cert_file(&env.manifests_dir, &cert)?;
            Ok(Issued { cert, new: true })
        }
    }
}

/// `project.rekey`: replace the certified key with `new_pubkey` (proved by
/// PoP over a nonce from `project.cert.challenge`). The old key is revoked
/// for good.
pub fn rekey(env: &CertEnv, params: &Value, now: DateTime<Utc>) -> Result<Issued, IssueError> {
    // Before the journal lock (see `anchor_rpc::identity_change_guard`).
    let _anchor_guard = crate::anchor_rpc::identity_change_guard();
    let id = str_param(params, "id")?;
    registered_manifest(env, id)?;
    let new_pk = pubkey_param(params, "new_pubkey")?;
    let nonce = claim_nonce(str_param(params, "nonce")?, id)?;
    let sig = sig_param(params, "pop_sig")?;
    verify_pop(env, PopOp::Rekey, &new_pk, &nonce, id, &sig)?;
    let journal = IdentityJournal::new(&env.manifests_dir);
    let lock = journal.lock()?;
    let (old_key_id, serial) = current_view_locked(env, &lock)?.plan_rekey(id, &new_pk)?;
    let cert = ident::sign_cert(
        &env.user_key,
        &CertRequest {
            project_id: id.to_owned(),
            project_pubkey: new_pk,
            serial,
            issued_at: now,
            expires_at: None,
        },
    )?;
    journal.append(&lock, &JournalRecord::Rekey { old_key_id: old_key_id.clone(), cert: cert.clone() })?;
    env.chain.append(
        SOURCE,
        KIND_REKEY,
        Some(json!({
            "project_id": id,
            "old_key_id": old_key_id,
            "new_cert": cert,
            "reason": clean_reason(params.get("reason")),
        })),
    );
    // Journalled: the rekey is in force. Drop the old key's session first,
    // so a store failure below cannot leave it live (review S1).
    drop_session(id);
    write_cert_file(&env.manifests_dir, &cert).map_err(|e| {
        IssueError::Incomplete(format!(
            "project {id} is rekeyed (serial {}), but writing {id}.cert.json failed: {e}",
            cert.serial
        ))
    })?;
    Ok(Issued { cert, new: true })
}

/// `project.revoke`: revoke the certified key. The project has no key
/// until a later register (the revoked key can never return).
pub fn revoke(env: &CertEnv, params: &Value) -> Result<Value, IssueError> {
    // Before the journal lock (see `anchor_rpc::identity_change_guard`).
    let _anchor_guard = crate::anchor_rpc::identity_change_guard();
    let id = str_param(params, "id")?;
    validate_id(id).map_err(|_| IssueError::Invalid("project id is not a canonical ULID".into()))?;
    let journal = IdentityJournal::new(&env.manifests_dir);
    let lock = journal.lock()?;
    let old = current_view_locked(env, &lock)?
        .bound_key_id(id)
        .ok_or_else(|| IdentityError::NotBound(id.to_owned()))?
        .to_owned();
    journal.append(&lock, &JournalRecord::Revoke { project_id: id.to_owned(), key_id: old.clone() })?;
    env.chain.append(
        SOURCE,
        KIND_REVOKE,
        Some(json!({
            "project_id": id,
            "old_key_id": old,
            "reason": clean_reason(params.get("reason")),
        })),
    );
    mark_revoked(env, id, "revoked");
    match std::fs::remove_file(cert_path(&env.manifests_dir, id)) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(IssueError::Incomplete(format!(
                "key {old} is revoked, but removing {id}.cert.json failed: {e}"
            )));
        }
    }
    Ok(json!({ "project_id": id, "revoked_key_id": old }))
}

/// `project.cert.show`: the certificate in force, if any.
pub fn show(env: &CertEnv, params: &Value) -> Result<Value, IssueError> {
    let id = str_param(params, "id")?;
    validate_id(id).map_err(|_| IssueError::Invalid("project id is not a canonical ULID".into()))?;
    let view = current_view(env)?;
    Ok(json!({
        "project_id": id,
        "certified": view.bound_key_id(id).is_some(),
        "cert": view.current_cert(id),
        "last_serial": view.last_serial(id),
    }))
}

/// `project.cert.challenge`: a nonce for a `project.rekey` proof.
pub fn challenge(env: &CertEnv, params: &Value) -> Result<Value, IssueError> {
    let id = str_param(params, "id")?;
    registered_manifest(env, id)?;
    Ok(json!({
        "nonce": issue_challenge(id)?,
        "ttl_secs": CHALLENGE_TTL.as_secs(),
        "user_key_id": key_id(&user_pubkey(env)),
        "op": PopOp::Rekey.as_str(),
    }))
}

fn str_param<'a>(p: &'a Value, k: &str) -> Result<&'a str, IssueError> {
    p.get(k)
        .and_then(Value::as_str)
        .ok_or_else(|| IssueError::Invalid(format!("missing string param `{k}`")))
}

fn pubkey_param(p: &Value, k: &str) -> Result<[u8; 32], IssueError> {
    ident::parse_pubkey(str_param(p, k)?)
        .ok_or_else(|| IssueError::Invalid(format!("`{k}` must be 64 lowercase hex characters")))
}

fn sig_param(p: &Value, k: &str) -> Result<[u8; 64], IssueError> {
    clawft_types::project::canon::hex_decode::<64>(str_param(p, k)?)
        .ok_or_else(|| IssueError::Invalid(format!("`{k}` must be 128 lowercase hex characters")))
}

/// Build the environment from the daemon: only the user daemon, which
/// signs with the user key and keeps the manifests.
pub(crate) async fn env_from(ctx: &ExtCtx) -> Result<CertEnv, IssueError> {
    env_from_kernel(&ctx.kernel).await
}

/// [`env_from`] for a caller that has the kernel but no request context.
pub(crate) async fn env_from_kernel(kernel: &crate::rpc_ext::KernelRef) -> Result<CertEnv, IssueError> {
    if !crate::user_daemon::is_active() {
        return Err(IssueError::Unavailable(
            "project certificates are issued by the user daemon only".into(),
        ));
    }
    let k = kernel.read().await;
    let chain = k.chain_manager().cloned();
    let user_key = chain.as_ref().and_then(|c| c.signing_key_clone());
    let (Some(chain), Some(user_key)) = (chain, user_key) else {
        return Err(IssueError::Unavailable("the user chain has no signing key".into()));
    };
    let manifests_dir = crate::project_rpc::configured_dir()
        .ok_or_else(|| IssueError::Unavailable("no manifest store (no home directory)".into()))?;
    let env = CertEnv { chain, user_key, manifests_dir };
    // Repair the log from the chain first, then require that it ends at the
    // key in use before anything is signed or verified (fail closed).
    chain_rotations(&env)?;
    user_history(&env)?;
    Ok(env)
}

/// Certify a registering child's key. Shared by package H's
/// `mesh.register`; H verifies the spawn nonce and issued the PoP nonce.
pub async fn issue_for_register(ctx: &ExtCtx, req: RegisterRequest) -> Result<Issued, IssueError> {
    let env = env_from(ctx).await?;
    let issued = tokio::task::spawn_blocking(move || register(&env, req, Utc::now()))
        .await
        .map_err(|e| IssueError::Store(format!("task failed: {e}")))?;
    if issued.is_ok() {
        project_claims_changed().notify_waiters();
        sync_claims_barrier().await.map_err(|e| IssueError::Incomplete(format!("project certificate was journalled but {e}")))?;
    }
    issued
}

/// A project's key was revoked (writes the terminal marker; the child is
/// stopped and its credentials killed) or replaced (child stopped, no
/// marker). The child is stopped whether or not the marker could be written;
/// a marker failure is returned. Without a supervisor (not the user daemon)
/// there is no child to stop.
#[cfg(all(unix, feature = "exochain", feature = "placement"))]
pub async fn on_identity_change(id: &str, method: &str) -> Result<(), MarkerError> {
    project_claims_changed().notify_waiters();
    let marker = (method == "project.revoke").then(|| write_revoked_marker(id, "revoked"));
    if let Some(sup) = crate::project_supervisor::global() {
        if method == "project.revoke" {
            sup.revoked(id, method).await;
        } else {
            sup.rekeyed(id).await;
        }
    }
    marker.transpose().map(|_| ())
}

/// See the supervised variant; without placement there is no supervisor.
#[cfg(not(all(unix, feature = "exochain", feature = "placement")))]
pub async fn on_identity_change(id: &str, method: &str) -> Result<(), MarkerError> {
    project_claims_changed().notify_waiters();
    if method == "project.revoke" {
        write_revoked_marker(id, "revoked")?;
    }
    Ok(())
}

/// Handler for `project.cert.show`, `project.cert.challenge`,
/// `project.rekey`, `project.revoke` and `project.identity.repair`.
pub fn handle(call: ExtCall) -> ExtFuture {
    Box::pin(async move {
        if call
            .ctx
            .auth
            .as_deref()
            .is_some_and(|a| a.trim().starts_with(SECRET_PREFIX))
        {
            return Response::error_with_kind(
                crate::token_rpc::TOKEN_CANNOT_MINT_KIND,
                "a token cannot manage project certificates; use the local socket as the owner",
            );
        }
        let env = match env_from(&call.ctx).await {
            Ok(e) => e,
            Err(e) => return e.response(),
        };
        let (method, params) = (call.method, call.params);
        // After the journal write succeeds, a revoke stops the child for good
        // (`on_identity_change` writes the marker first) and a rekey stops the old
        // child (package G); a repair that changed a bound key does the same.
        let is_repair = method == "project.identity.repair";
        let after = matches!(method.as_str(), "project.revoke" | "project.rekey").then(|| {
            (
                method.clone(),
                params.get("id").and_then(Value::as_str).map(str::to_owned),
            )
        });
        let needs_claim_sync = after.is_some() || is_repair;
        let out = tokio::task::spawn_blocking(move || match method.as_str() {
            "project.cert.show" => show(&env, &params),
            "project.cert.challenge" => challenge(&env, &params),
            "project.identity.repair" => {
                // Which bound keys does the repair change? (Compared by id.)
                let bound = |e: &CertEnv| {
                    current_view(e).ok().map(|v| {
                        v.all_project_ids()
                            .into_iter()
                            .map(|p| (p.clone(), v.bound_key_id(&p).map(str::to_owned)))
                            .collect::<std::collections::BTreeMap<_, _>>()
                    })
                };
                // A corrupt journal has no readable "before": every project
                // the repaired view knows is treated as possibly changed
                // (its child is stopped; it is started again on demand).
                let before = bound(&env);
                repair(&env).map(|mut v| {
                    let after = bound(&env).unwrap_or_default();
                    let changed: Vec<Value> = match &before {
                        Some(before) => before
                            .iter()
                            .filter(|(id, k)| after.get(*id) != Some(*k))
                            .map(|(id, _)| json!({"id": id, "revoked": after.get(id).is_none_or(Option::is_none)}))
                            .collect(),
                        None => after
                            .iter()
                            .map(|(id, k)| json!({"id": id, "revoked": k.is_none()}))
                            .collect(),
                    };
                    v["key_changes"] = Value::Array(changed);
                    v
                })
            }
            "project.rekey" => rekey(&env, &params, Utc::now())
                .map(|i| json!({ "cert": i.cert })),
            "project.revoke" => revoke(&env, &params),
            other => Err(IssueError::Invalid(format!("unknown method: {other}"))),
        })
        .await;
        // Review S1: once the change is journalled (success, or a late store
        // failure) the hooks run, so a revoked project's child is stopped and
        // marked even when the cert file could not be removed.
        let (v, incomplete) = match out {
            Ok(Ok(v)) => (v, None),
            Ok(Err(e @ IssueError::Incomplete(_))) => (Value::Null, Some(e.response())),
            Ok(Err(e)) => return e.response(),
            Err(e) => return Response::error(format!("project cert task failed: {e}")),
        };
        let mut unwritten: Option<Response> = None;
        if let Some((method, Some(id))) = after
            && let Err(e) = crate::project_cert_rpc::on_identity_change(&id, &method).await
        {
            tracing::error!(project = %id, error = %e, "revoked marker not written");
            unwritten.get_or_insert(e.response(&id));
        }
        if is_repair {
            for c in v.get("key_changes").and_then(Value::as_array).into_iter().flatten() {
                let Some(id) = c.get("id").and_then(Value::as_str) else { continue };
                let m = if c["revoked"] == true { "project.revoke" } else { "project.rekey" };
                if let Err(e) = crate::project_cert_rpc::on_identity_change(id, m).await {
                    tracing::error!(project = %id, error = %e, "revoked marker not written");
                    unwritten.get_or_insert(e.response(id));
                }
            }
        }
        if needs_claim_sync
            && let Err(e) = sync_claims_barrier().await
        {
            return Response::error_with_kind(
                "project_claim_sync_incomplete",
                format!("project identity was journalled but {e}; the machine service claim is not confirmed"),
            );
        }
        unwritten.or(incomplete).unwrap_or_else(|| Response::success(v))
    })
}

#[cfg(test)]
#[path = "project_cert_rpc_tests.rs"]
mod tests;
