//! The user daemon's record of its children for `mesh-local/1`
//! (ADR-103 A6, Phase 2 package H): the spawn ledger the supervisor
//! fills ([`expect_spawn`]) and the [`ProjectRegistry`] of live sessions.
//!
//! Rules (all enforced here, under one lock, so a race cannot get around
//! them):
//!
//! * a child registers only against an outstanding [`SpawnExpectation`]
//!   (one per project id, replaced by a newer spawn, single use, expiring)
//!   or as a re-register of a session this daemon already knows by pid;
//! * one live session per project id: a second `register` while the first
//!   still beats is refused (`kernel.lock` is the child's own guard, this is
//!   the parent's);
//! * a session is live for [`MISSED_BEATS`] heartbeat intervals after its
//!   last beat, then `expired` (kept as a tombstone so the same child can
//!   re-register, a different pid cannot without a new spawn);
//! * [`ProjectRegistry::route_for`] is the Phase 3 upstream hook: where the
//!   child answers, nothing more. Mesh delivery is not wired in Phase 2.
//!
//! Time is injected (`*_at` methods) so expiry is testable without sleeping.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use clawft_rpc::mesh_local::Activity;

/// Heartbeat interval the user daemon asks children for.
pub const HEARTBEAT_SECS: u64 = 15;
/// A session expires after this many missed beats.
pub const MISSED_BEATS: u32 = 3;
/// Most unexpired spawn expectations the ledger holds.
pub const MAX_EXPECTATIONS: usize = 256;

/// What the supervisor knows about a child it just spawned. Filed by
/// [`expect_spawn`] right when it writes `spawn.json`.
#[derive(Clone, PartialEq, Eq)]
pub struct SpawnExpectation {
    /// Project ULID.
    pub project_id: String,
    /// The spawn nonce written to `spawn.json` (32 hex).
    pub nonce: String,
    /// Native host PID once spawned. Containers use engine identity instead.
    pub pid: Option<u32>,
    pub container: Option<clawft_rpc::mesh_local::ContainerRegistration>,
    /// SHA-256 of the child executable, hex (recorded on the user chain).
    pub exe_sha: String,
    /// The canonical project root the supervisor spawned it in.
    pub root: PathBuf,
    /// Unix seconds after which the nonce is void.
    pub expires_unix: u64,
}

impl std::fmt::Debug for SpawnExpectation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpawnExpectation")
            .field("project_id", &self.project_id)
            .field("nonce", &"<redacted>")
            .field("pid", &self.pid)
            .field("root", &self.root)
            .field("expires_unix", &self.expires_unix)
            .finish()
    }
}

/// Why the ledger or registry refused; `kind()` is the RPC `error_kind`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegistryError {
    /// No spawn was expected for this project (or none is outstanding).
    #[error("no spawn is expected for project {0}; project kernels are started by the user daemon")]
    NotExpected(String),
    /// The spawn nonce is missing, wrong or used up.
    #[error("the spawn nonce is not accepted")]
    BadSpawnNonce,
    /// The spawn nonce is past its expiry.
    #[error("the spawn nonce expired")]
    SpawnExpired,
    /// The registering pid is not the spawned child's.
    #[error("pid {got} is not the spawned child (pid {want})")]
    PidMismatch {
        /// Pid in the registration.
        got: u32,
        /// Pid the supervisor recorded.
        want: u32,
    },
    /// The project root the child claims is not the one it was spawned in.
    #[error("root does not match the spawned project root")]
    WrongRoot,
    /// Guest identity or host endpoint differs from engine inspection.
    #[error("container identity or host endpoint does not match the supervised launch")]
    ContainerMismatch,
    /// A live session already exists for the project.
    #[error("project {0} already has a live session")]
    SecondSession(String),
    /// No such session (never registered, unregistered, or replaced).
    #[error("unknown session")]
    UnknownSession,
    /// The spawn ledger is full of unexpired expectations.
    #[error("too many outstanding spawns")]
    LedgerFull,
    /// A session-bound request failed its proof (pid, freshness, signature).
    #[error("session proof refused: {0}")]
    BadProof(&'static str),
    /// The session missed three heartbeats; register again.
    #[error("session expired after {0} missed heartbeats; register again")]
    SessionExpired(u32),
}

impl RegistryError {
    /// Stable snake_case discriminator for clients.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::NotExpected(_) => "spawn_not_expected",
            Self::BadSpawnNonce => "bad_spawn_nonce",
            Self::SpawnExpired => "spawn_expired",
            Self::PidMismatch { .. } => "pid_mismatch",
            Self::WrongRoot => "root_mismatch",
            Self::ContainerMismatch => "container_mismatch",
            Self::SecondSession(_) => "second_session",
            Self::UnknownSession => "unknown_session",
            Self::LedgerFull => "spawn_ledger_full",
            Self::BadProof(_) => "bad_session_proof",
            Self::SessionExpired(_) => "session_expired",
        }
    }
}

/// A new session's facts (from a verified `mesh.register`).
#[derive(Debug, Clone)]
pub struct NewSession {
    /// Project ULID.
    pub project_id: String,
    /// The child's own socket.
    pub socket: PathBuf,
    /// Child pid.
    pub pid: u32,
    pub container: Option<clawft_rpc::mesh_local::ContainerRegistration>,
    /// Mesh addresses the child answers to.
    pub addresses: Vec<String>,
    /// Chain topic prefixes it serves.
    pub topic_prefixes: Vec<String>,
    /// Kernel version.
    pub version: String,
    /// `key_id` of the certified project key.
    pub project_key_id: String,
    /// The certified project public key (verifies session-bound requests).
    pub project_pubkey: [u8; 32],
}

/// Session liveness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// Beating.
    Live,
    /// Missed [`MISSED_BEATS`] heartbeats.
    Expired,
}

/// One registered child.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    /// Session id (ULID).
    pub session: String,
    /// The facts it registered with.
    pub facts: NewSession,
    /// Last beat (or registration).
    pub last_heartbeat: Instant,
    /// Activity the last beat carried.
    pub activity: Activity,
    /// A tombstone filed by adoption ([`ProjectRegistry::adopt_expired`]):
    /// the child has not registered since. Never a "lost heartbeat" (an
    /// adopted child of an older build may not re-register at all).
    pub adopted: bool,
}

/// Where a project's child answers (the Phase 3 upstream hook).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    /// The child's socket.
    pub socket: PathBuf,
    /// Child pid.
    pub pid: u32,
    /// Session id.
    pub session: String,
    /// Addresses it registered.
    pub addresses: Vec<String>,
}

/// Registered children, by project id.
pub struct ProjectRegistry {
    beat: Duration,
    inner: Mutex<HashMap<String, SessionInfo>>,
}


/// The fields of a heartbeat/unregister proof as received over RPC.
#[derive(Debug, Clone, Copy)]
pub struct SessionProof<'a> {
    /// `"heartbeat"` or `"unregister"`.
    pub op: &'a str,
    /// Session id the proof names.
    pub session: &'a str,
    /// Pid the caller claims.
    pub pid: u32,
    /// Caller timestamp (unix seconds) the signature covers.
    pub at_unix: u64,
    /// Op-specific extra signed payload (activity digest, or empty).
    pub extra: &'a str,
    /// Hex signature by the certified key.
    pub sig_hex: &'a str,
}

impl ProjectRegistry {
    /// A registry whose sessions expire after [`MISSED_BEATS`] x `beat`.
    pub fn new(beat: Duration) -> Self {
        Self {
            beat,
            inner: Mutex::new(HashMap::new()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, SessionInfo>> {
        self.inner.lock().expect("inner lock poisoned")
    }

    fn state_of(&self, s: &SessionInfo, now: Instant) -> SessionState {
        if now.saturating_duration_since(s.last_heartbeat) >= self.beat * MISSED_BEATS {
            SessionState::Expired
        } else {
            SessionState::Live
        }
    }

    /// The tombstone for `id`, if any, with its state.
    pub fn state_at(&self, id: &str, now: Instant) -> Option<(SessionState, u32)> {
        self.lock()
            .get(id)
            .map(|s| (self.state_of(s, now), s.facts.pid))
    }

    /// Facts for readiness and re-registration identity checks.
    pub fn facts_at(&self, id: &str) -> Option<NewSession> {
        self.lock().get(id).map(|s| s.facts.clone())
    }

    pub fn container_for_session(&self, session: &str) -> Option<(String, clawft_rpc::mesh_local::ContainerRegistration)> {
        self.lock().values().find(|s| s.session == session)
            .and_then(|s| s.facts.container.clone().map(|c| (s.facts.project_id.clone(), c)))
    }

    /// Open a session. Refused while a live one exists. A tombstone (expired
    /// session) is replaced.
    pub fn register_at(
        &self,
        facts: NewSession,
        now: Instant,
    ) -> Result<SessionInfo, RegistryError> {
        let mut map = self.lock();
        if let Some(old) = map.get(&facts.project_id)
            && self.state_of(old, now) == SessionState::Live
        {
            return Err(RegistryError::SecondSession(facts.project_id));
        }
        let info = SessionInfo {
            session: clawft_types::project::new_id(),
            facts,
            last_heartbeat: now,
            activity: Activity::default(),
            adopted: false,
        };
        map.insert(info.facts.project_id.clone(), info.clone());
        Ok(info)
    }

    /// Record a beat. An expired or replaced session is refused.
    pub fn heartbeat_at(
        &self,
        session: &str,
        activity: Activity,
        now: Instant,
    ) -> Result<(), RegistryError> {
        let mut map = self.lock();
        let Some(s) = map.values_mut().find(|s| s.session == session) else {
            return Err(RegistryError::UnknownSession);
        };
        if now.saturating_duration_since(s.last_heartbeat) >= self.beat * MISSED_BEATS {
            return Err(RegistryError::SessionExpired(MISSED_BEATS));
        }
        s.last_heartbeat = now;
        s.activity = activity;
        Ok(())
    }

    /// Check a heartbeat/unregister proof (see [`SessionProof`]): the session exists and beats,
    /// `pid` is the registered one, `at_unix` is within the window of
    /// `now_unix`, and `sig` is the certified key's signature over
    /// [`clawft_rpc::mesh_local::session_signed_bytes`].
    pub fn verify_proof_at(
        &self,
        proof: &SessionProof<'_>,
        now_unix: u64,
        now: Instant,
    ) -> Result<(), RegistryError> {
        let SessionProof {
            op,
            session,
            pid,
            at_unix,
            extra,
            sig_hex,
        } = *proof;
        use clawft_rpc::mesh_local::{SESSION_PROOF_WINDOW_SECS, session_signed_bytes};
        let (pubkey, want_pid, state) = {
            let map = self.lock();
            let s = map
                .values()
                .find(|s| s.session == session)
                .ok_or(RegistryError::UnknownSession)?;
            (s.facts.project_pubkey, s.facts.pid, self.state_of(s, now))
        };
        if pid != want_pid {
            return Err(RegistryError::BadProof("pid is not the registered one"));
        }
        if at_unix.abs_diff(now_unix) > SESSION_PROOF_WINDOW_SECS {
            return Err(RegistryError::BadProof("stale or future timestamp"));
        }
        let sig = clawft_types::project::canon::hex_decode::<64>(sig_hex)
            .ok_or(RegistryError::BadProof("signature is missing or not hex"))?;
        let vk = ed25519_dalek::VerifyingKey::from_bytes(&pubkey)
            .map_err(|_| RegistryError::BadProof("registered key is invalid"))?;
        vk.verify_strict(
            &session_signed_bytes(op, session, pid, at_unix, extra),
            &ed25519_dalek::Signature::from_bytes(&sig),
        )
        .map_err(|_| {
            RegistryError::BadProof("signature does not verify under the certified key")
        })?;
        if state == SessionState::Expired && op == "heartbeat" {
            return Err(RegistryError::SessionExpired(MISSED_BEATS));
        }
        Ok(())
    }

    /// End a session (the tombstone is dropped: a clean unregister is not a
    /// crash, and the next start comes with a new spawn nonce).
    pub fn unregister(&self, session: &str) -> Result<String, RegistryError> {
        let mut map = self.lock();
        let id = map
            .iter()
            .find(|(_, s)| s.session == session)
            .map(|(id, _)| id.clone())
            .ok_or(RegistryError::UnknownSession)?;
        map.remove(&id);
        Ok(id)
    }

    /// Where `project_id`'s child answers, if its session is live. The
    /// Phase 3 upstream hook.
    pub fn route_for_at(&self, project_id: &str, now: Instant) -> Option<Route> {
        let map = self.lock();
        let s = map.get(project_id)?;
        (self.state_of(s, now) == SessionState::Live).then(|| Route {
            socket: s.facts.socket.clone(),
            pid: s.facts.pid,
            session: s.session.clone(),
            addresses: s.facts.addresses.clone(),
        })
    }

    /// [`Self::route_for_at`] now.
    pub fn route_for(&self, project_id: &str) -> Option<Route> {
        self.route_for_at(project_id, Instant::now())
    }

    /// Every session with its state (for `weaver kernel status`, G's idle
    /// poll and tests).
    pub fn sessions_at(&self, now: Instant) -> Vec<(SessionInfo, SessionState)> {
        let mut v: Vec<_> = self
            .lock()
            .values()
            .map(|s| (s.clone(), self.state_of(s, now)))
            .collect();
        v.sort_by(|a, b| a.0.facts.project_id.cmp(&b.0.facts.project_id));
        v
    }

    /// [`Self::sessions_at`] now.
    pub fn sessions(&self) -> Vec<(SessionInfo, SessionState)> {
        self.sessions_at(Instant::now())
    }

    /// Ids whose session has expired and not been replaced: the supervisor's
    /// "lost heartbeat" input.
    pub fn expired_ids_at(&self, now: Instant) -> Vec<String> {
        let mut v: Vec<String> = self
            .lock()
            .iter()
            .filter(|(_, s)| self.state_of(s, now) == SessionState::Expired)
            .map(|(id, _)| id.clone())
            .collect();
        v.sort();
        v
    }

    /// [`Self::expired_ids_at`] now.
    pub fn expired_ids(&self) -> Vec<String> {
        self.expired_ids_at(Instant::now())
    }

    /// Drop `project_id`'s session, live or not. The supervisor calls this
    /// before it spawns a replacement for a child it knows is dead, so the
    /// new child is not refused as a second live session.
    pub fn evict(&self, project_id: &str) -> bool {
        self.lock().remove(project_id).is_some()
    }

    /// File an already-expired session for a child the supervisor adopted
    /// after a user-daemon restart: it may re-register (same pid, certified
    /// key, proof of possession) without a spawn nonce. Does nothing when a
    /// session exists.
    pub fn adopt_expired(&self, facts: NewSession) {
        let mut map = self.lock();
        if map.contains_key(&facts.project_id) {
            return;
        }
        let dead = Instant::now()
            .checked_sub(self.beat * (MISSED_BEATS + 1))
            .unwrap_or_else(Instant::now);
        map.insert(
            facts.project_id.clone(),
            SessionInfo {
                session: clawft_types::project::new_id(),
                facts,
                last_heartbeat: dead,
                activity: Activity::default(),
                adopted: true,
            },
        );
    }

    /// Record the child's pid once the supervisor learns it: on a live
    /// session, and on the outstanding spawn expectation (filed without a PID
    /// before the process existed), so the first registration's claimed
    /// pid is checked against the one the supervisor started (review S8c).
    pub fn note_pid(&self, project_id: &str, pid: u32) {
        if let Some(s) = self.lock().get_mut(project_id)
            && s.facts.container.is_none()
        {
            s.facts.pid = pid;
        }
        note_spawn_pid(project_id, pid);
    }
}

/// The process's registry.
pub fn registry() -> &'static ProjectRegistry {
    static R: OnceLock<ProjectRegistry> = OnceLock::new();
    R.get_or_init(|| ProjectRegistry::new(Duration::from_secs(HEARTBEAT_SECS)))
}

fn ledger() -> &'static Mutex<HashMap<String, SpawnExpectation>> {
    static L: OnceLock<Mutex<HashMap<String, SpawnExpectation>>> = OnceLock::new();
    L.get_or_init(Default::default)
}

fn ct_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

/// File the expectation for a child about to start (the supervisor calls
/// this when it writes `spawn.json`). One per project id: a newer spawn
/// replaces an older one.
///
/// Refuses ([`RegistryError::LedgerFull`]) when [`MAX_EXPECTATIONS`]
/// unexpired expectations for other projects are outstanding (expired ones
/// are purged first).
pub fn expect_spawn(e: SpawnExpectation) -> Result<(), RegistryError> {
    expect_spawn_capped(e, MAX_EXPECTATIONS)
}

fn expect_spawn_capped(e: SpawnExpectation, cap: usize) -> Result<(), RegistryError> {
    let mut map = ledger().lock().expect("ledger lock poisoned");
    insert_capped(&mut map, e, cap, now_unix())
}

fn insert_capped(
    map: &mut HashMap<String, SpawnExpectation>,
    e: SpawnExpectation,
    cap: usize,
    now: u64,
) -> Result<(), RegistryError> {
    if map.len() >= cap && !map.contains_key(&e.project_id) {
        map.retain(|_, v| v.expires_unix > now);
        if map.len() >= cap {
            return Err(RegistryError::LedgerFull);
        }
    }
    map.insert(e.project_id.clone(), e);
    Ok(())
}

/// Set the pid of the outstanding expectation for `project_id` when it was
/// filed without one.
pub fn note_spawn_pid(project_id: &str, pid: u32) {
    if let Some(e) = ledger().lock().expect("ledger lock poisoned").get_mut(project_id)
        && e.pid.is_none()
    {
        e.pid = Some(pid);
    }
}

/// Forget the outstanding expectation for `project_id` (spawn failed).
pub fn cancel_spawn(project_id: &str) {
    ledger()
        .lock()
        .expect("ledger lock poisoned")
        .remove(project_id);
}

/// True when an unexpired expectation is outstanding for `project_id`.
pub fn spawn_expected(project_id: &str, now_unix: u64) -> bool {
    ledger()
        .lock()
        .expect("ledger lock poisoned")
        .get(project_id)
        .is_some_and(|e| e.expires_unix > now_unix)
}

/// Check, without consuming it, that `nonce`/`pid` match the outstanding
/// expectation for `project_id`.
pub fn peek_spawn(
    project_id: &str,
    nonce: Option<&str>,
    pid: u32,
    now_unix: u64,
) -> Result<SpawnExpectation, RegistryError> {
    let map = ledger().lock().expect("ledger lock poisoned");
    let e = map
        .get(project_id)
        .ok_or_else(|| RegistryError::NotExpected(project_id.to_owned()))?;
    if e.expires_unix <= now_unix {
        return Err(RegistryError::SpawnExpired);
    }
    // Always compare, even when the caller sent nothing, so timing does not
    // distinguish "no nonce" from "wrong nonce".
    let given = nonce.unwrap_or("");
    if !ct_eq(given, &e.nonce) || nonce.is_none() {
        return Err(RegistryError::BadSpawnNonce);
    }
    if e.container.is_none() && let Some(want) = e.pid && want != pid {
        return Err(RegistryError::PidMismatch {
            got: pid,
            want,
        });
    }
    Ok(e.clone())
}

/// Consume the expectation `peek_spawn` approved. `false` when it was
/// already consumed or replaced (a lost race).
pub fn consume_spawn(e: &SpawnExpectation) -> bool {
    let mut map = ledger().lock().expect("ledger lock poisoned");
    match map.get(&e.project_id) {
        Some(cur) if cur.nonce == e.nonce => {
            map.remove(&e.project_id);
            true
        }
        _ => false,
    }
}

/// Wall clock in unix seconds.
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(id: &str, pid: u32) -> NewSession {
        NewSession {
            project_id: id.into(),
            socket: format!("/run/{id}/kernel.sock").into(),
            pid,
            container: None,
            addresses: vec![id.into()],
            topic_prefixes: vec![format!("chain/{id}/")],
            version: "0.8.2".into(),
            project_key_id: "k".into(),
            project_pubkey: [0u8; 32],
        }
    }

    #[test]
    fn second_live_session_is_refused_expired_one_is_replaced() {
        let r = ProjectRegistry::new(Duration::from_secs(15));
        let t0 = Instant::now();
        let s1 = r.register_at(facts("P", 1), t0).unwrap();
        assert_eq!(
            r.register_at(facts("P", 2), t0 + Duration::from_secs(44))
                .unwrap_err()
                .kind(),
            "second_session"
        );
        let t = t0 + Duration::from_secs(45);
        assert_eq!(r.expired_ids_at(t), vec!["P".to_string()]);
        assert!(r.route_for_at("P", t).is_none());
        let s2 = r.register_at(facts("P", 2), t).unwrap();
        assert_ne!(s1.session, s2.session);
        assert_eq!(
            r.heartbeat_at(&s1.session, Activity::default(), t)
                .unwrap_err()
                .kind(),
            "unknown_session"
        );
    }

    #[test]
    fn three_missed_beats_expire_and_a_beat_in_time_keeps_it_live() {
        let r = ProjectRegistry::new(Duration::from_secs(15));
        let t0 = Instant::now();
        let s = r.register_at(facts("P", 1), t0).unwrap();
        r.heartbeat_at(
            &s.session,
            Activity::default(),
            t0 + Duration::from_secs(40),
        )
        .unwrap();
        let live = t0 + Duration::from_secs(40 + 44);
        assert!(r.route_for_at("P", live).is_some());
        let dead = t0 + Duration::from_secs(40 + 45);
        assert!(r.route_for_at("P", dead).is_none());
        assert_eq!(
            r.heartbeat_at(&s.session, Activity::default(), dead)
                .unwrap_err()
                .kind(),
            "session_expired"
        );
    }

    #[test]
    fn unregister_drops_the_session() {
        let r = ProjectRegistry::new(Duration::from_secs(15));
        let s = r.register_at(facts("P", 1), Instant::now()).unwrap();
        assert_eq!(r.unregister(&s.session).unwrap(), "P");
        assert!(r.route_for("P").is_none());
        assert!(r.unregister(&s.session).is_err());
    }

    #[test]
    fn spawn_nonce_is_single_use_pid_checked_and_expiring() {
        let id = "01TESTLEDGER0000000000000A";
        let n = "ab".repeat(16);
        expect_spawn(SpawnExpectation {
            project_id: id.into(),
            nonce: n.clone(),
            pid: None,
            container: None,
            exe_sha: String::new(),
            root: "/r".into(),
            expires_unix: 1_000,
        })
        .unwrap();
        assert_eq!(
            peek_spawn(id, None, 7, 10).unwrap_err().kind(),
            "bad_spawn_nonce"
        );
        assert_eq!(
            peek_spawn(id, Some(&"cd".repeat(16)), 7, 10)
                .unwrap_err()
                .kind(),
            "bad_spawn_nonce"
        );
        assert_eq!(
            peek_spawn(id, Some(&n), 7, 1_000).unwrap_err().kind(),
            "spawn_expired"
        );
        let e = peek_spawn(id, Some(&n), 7, 10).unwrap();
        assert!(consume_spawn(&e));
        assert!(!consume_spawn(&e), "single use");
        assert_eq!(
            peek_spawn(id, Some(&n), 7, 10).unwrap_err().kind(),
            "spawn_not_expected"
        );
        // Recorded pid must match.
        let id2 = "01TESTLEDGER0000000000000B";
        expect_spawn(SpawnExpectation {
            project_id: id2.into(),
            nonce: n.clone(),
            pid: Some(9),
            container: None,
            exe_sha: String::new(),
            root: "/r".into(),
            expires_unix: 1_000,
        })
        .unwrap();
        assert_eq!(
            peek_spawn(id2, Some(&n), 8, 10).unwrap_err().kind(),
            "pid_mismatch"
        );
        assert!(peek_spawn(id2, Some(&n), 9, 10).is_ok());
        cancel_spawn(id2);
        // Filed with pid 0, the supervisor's spawned pid is noted afterwards
        // and then checked (review S8c).
        let id3 = "01TESTLEDGER0000000000000C";
        expect_spawn(SpawnExpectation {
            project_id: id3.into(),
            nonce: n.clone(),
            pid: None,
            container: None,
            exe_sha: String::new(),
            root: "/r".into(),
            expires_unix: 1_000,
        })
        .unwrap();
        registry().note_pid(id3, 4242);
        assert_eq!(peek_spawn(id3, Some(&n), 7, 10).unwrap_err().kind(), "pid_mismatch");
        assert!(peek_spawn(id3, Some(&n), 4242, 10).is_ok());
        note_spawn_pid(id3, 1); // never overwrites a known pid
        assert!(peek_spawn(id3, Some(&n), 4242, 10).is_ok());
        cancel_spawn(id3);
    }

    #[test]
    fn a_full_ledger_purges_expired_then_refuses_a_new_project() {
        let mk = |id: &str, exp: u64| SpawnExpectation {
            project_id: id.into(),
            nonce: "ab".repeat(16),
            pid: None,
            container: None,
            exe_sha: String::new(),
            root: "/r".into(),
            expires_unix: exp,
        };
        let mut m = HashMap::new();
        insert_capped(&mut m, mk("A", 100), 2, 50).unwrap();
        insert_capped(&mut m, mk("B", 100), 2, 50).unwrap();
        // Full of live entries: a third project is refused, a replacement is not.
        assert_eq!(
            insert_capped(&mut m, mk("C", 100), 2, 50)
                .unwrap_err()
                .kind(),
            "spawn_ledger_full"
        );
        insert_capped(&mut m, mk("A", 200), 2, 50).unwrap();
        // Once the old ones have expired they are purged to make room.
        insert_capped(&mut m, mk("C", 300), 2, 150).unwrap();
        assert!(m.contains_key("C") && m.contains_key("A") && !m.contains_key("B"));
    }

    #[test]
    fn a_newer_spawn_replaces_the_older_one() {
        let id = "01TESTLEDGER0000000000000C";
        let mk = |n: &str| SpawnExpectation {
            project_id: id.into(),
            nonce: n.repeat(16),
            pid: None,
            container: None,
            exe_sha: String::new(),
            root: "/r".into(),
            expires_unix: 1_000,
        };
        expect_spawn(mk("aa")).unwrap();
        expect_spawn(mk("bb")).unwrap();
        assert!(peek_spawn(id, Some(&"aa".repeat(16)), 1, 1).is_err());
        assert!(peek_spawn(id, Some(&"bb".repeat(16)), 1, 1).is_ok());
        cancel_spawn(id);
    }
}
