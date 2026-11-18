//! `project.anchor.submit`: the user daemon accepts a project's signed
//! chain-head statement (ADR-103 A7, Phase 2 package D).
//!
//! Authenticated by the project key through the statement signature, not by
//! a capability token: a project kernel holds no token, and the claim the
//! daemon records is exactly "this certified key signed this statement".
//! User daemon only. Checks, in order: the certificate in force for the id
//! (from [`current_view`], never called under a journal lock) is unrevoked,
//! names the signing key, matches `cert_serial` and verifies; the signature;
//! then under one lock `seq` is last + 1, `prev_anchor` is the hash of the
//! last accepted statement, `at` is at most five minutes ahead, and
//! `head_seq` does not go backwards. An identical resubmission of the last
//! accepted statement is answered with its original acknowledgement, so a
//! project that lost the answer can replay safely. `at` has no lower bound:
//! a statement replayed after a long outage is old by design.
//!
//! Key history: statements are verified under the project's CURRENT key.
//! A rejection that carries `last` also carries `key_history` (public keys of
//! the certificate history: current and rekeyed-out keys, never a key revoked
//! for compromise), so a project restarted under a new key can verify and
//! adopt the parent's last statement and continue at N + 1. The daemon's own
//! records (the file, its chain events) were verified when accepted and are
//! re-verified against ALL certificates ever issued for the project,
//! compromised keys included: that check is tamper detection, not
//! authorisation. After a compromise revoke the old statements stay the
//! accepted history up to the revoke; nothing signed by that key is ever
//! accepted as new (the view refuses it).
//!
//! Recovery: at user-daemon startup ([`reconcile_startup`], and lazily on the
//! first use of a project) a `<id>.anchor.json` that is ahead of the chain
//! gets its `project.anchor` event re-appended (`recovered: true`), so the
//! chain is the record again. A chain event at the file's `user_seq` with a
//! different hash is a post-crash sequence collision, not corruption: the
//! event is re-appended under a new seq and the file rewritten (logged).
//!
//! The user chain can have per-project seq gaps after a crash: a recovered
//! event carries `original_user_seq` / `original_user_event_hash`, and an
//! identical resend after recovery returns the rewritten (new) ack. The
//! record is signed by the user key (`rec_sig`, see `anchor_record`); one that
//! fails is ignored, and a recovered statement whose key was revoked for
//! compromise is never re-appended.
//!
//! Parent lost its state (no record, the project's `seq` is ahead): the
//! refusal says so and nothing is adopted. The owner runs
//! `project.anchor.restore {statement, user_seq, user_event_hash}` with the
//! `statement`, `user_seq` and `user_event_hash` of the project's last
//! `project.anchored` event ([`restore`]); the project's next anchor then
//! continues at N + 1. Never a rewind, never a compromise-revoked key.
//!
//! Cost: structural checks (id, hex lengths, canonical `at`) run before the
//! identity view, and the signature is checked, all outside the accept lock;
//! the lock is taken only after the signature passes and revocation is
//! re-read under it (`project.revoke` / `project.rekey` hold the same lock). The last
//! accepted statement per project is kept in memory (no chain scan per
//! call). After three consecutive AUTHENTICATED refusals (seq, prev, head)
//! a project is answered `anchor_backoff` for 1 s doubling to 60 s; refusals
//! of unsigned garbage never count, so nobody can lock a project out.
//!
//! Acceptance appends a `project.anchor` event (source `project.anchor`,
//! reserved) to the user chain and writes `<manifests>/<id>.anchor.json`
//! (the chain is saved only on clean shutdown, so the file is the durable
//! record; [`last_accepted`] takes the further of the two).
//!
//! Honest limit: the user daemon attests "this key claimed head X at time
//! T"; it cannot verify X without subscribing to the project chain.

use std::collections::HashMap;
use std::sync::Mutex;

use chrono::{DateTime, Duration, Utc};
use clawft_kernel::chain_anchor::{ANCHOR_SOURCE, AnchorAck};
use clawft_kernel::project_identity::{self as ident, RevocationView};
use clawft_rpc::Response;
use clawft_types::project::canon::hex_decode;
use clawft_types::project::cert::{CertError, ProjectAnchorStmt, ts};
use clawft_types::project::validate_id;
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::project_cert_rpc::{CertEnv, current_view};
use crate::rpc_ext::{ExtCall, ExtFuture, KernelRef};

/// Kind of the user-chain event.
pub use clawft_kernel::chain_anchor::KIND_ANCHOR;
/// How far ahead of the daemon clock `at` may be, seconds.
pub const MAX_AHEAD_SECS: i64 = 300;
/// Authenticated refusals in a row before `anchor_backoff` starts.
const BACKOFF_AFTER: u32 = 3;
const BACKOFF_MAX_SECS: i64 = 60;

type Key = (usize, String);

#[derive(Default)]
pub(crate) struct Inner {
    /// Last accepted statement per (chain, project). `None` = loaded, none yet.
    index: HashMap<Key, Option<Accepted>>,
    /// Anchor epoch per (chain, project): the number of owner resets.
    epochs: HashMap<Key, u64>,
    /// Consecutive authenticated refusals and the end of the quiet period.
    backoff: HashMap<Key, (u32, Option<DateTime<Utc>>)>,
}

/// Serialises verify-then-append (and owns the in-memory index).
static ACCEPT: Mutex<Option<Inner>> = Mutex::new(None);

/// Held by `project.revoke` and `project.rekey` for their whole operation, so
/// a statement is verified either wholly before or wholly after one.
/// Lock order: this guard first, then the identity journal lock (a submit
/// holds this guard and takes only the shared journal lock inside it).
pub(crate) fn identity_change_guard() -> std::sync::MutexGuard<'static, Option<Inner>> {
    ACCEPT.lock().expect("ACCEPT lock poisoned")
}

#[path = "anchor_record.rs"]
mod record;
use record::{append_event, read_file, write_file};
#[path = "anchor_reset.rs"]
mod reset_record;
pub use reset_record::{KIND_RESET, ResetRecord};
#[path = "anchor_error.rs"]
mod error;
pub use error::{Accepted, AnchorError, Resync};

/// Forget the in-memory baseline of `env`'s chain, as a restart that
/// brought the saved chain back would (tests).
#[cfg(test)]
pub(super) fn drop_cache(env: &CertEnv) {
    let ptr = std::sync::Arc::as_ptr(&env.chain) as *const () as usize;
    let mut g = ACCEPT.lock().expect("anchor accept lock poisoned");
    if let Some(i) = g.as_mut() {
        i.index.retain(|k, _| k.0 != ptr);
        i.epochs.retain(|k, _| k.0 != ptr);
    }
}

fn key_of(env: &CertEnv, id: &str) -> Key {
    (std::sync::Arc::as_ptr(&env.chain) as *const () as usize, id.to_owned())
}

fn lower_hex(s: &str, n: usize) -> bool {
    s.len() == n && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Cheap checks that need no identity view: run first.
fn structural(stmt: &ProjectAnchorStmt, now: DateTime<Utc>) -> Result<(), AnchorError> {
    let bad = |m: &str| AnchorError::BadStatement(m.to_owned());
    validate_id(&stmt.project_id).map_err(|_| bad("project_id is not a canonical ULID"))?;
    for (name, v, n) in [
        ("project_key_id", stmt.project_key_id.as_str(), 32),
        ("head_hash", &stmt.head_hash, 64),
        ("rule_hash", &stmt.rule_hash, 64),
        ("sig", &stmt.sig, 128),
    ] {
        if !lower_hex(v, n) {
            return Err(AnchorError::BadStatement(format!("`{name}` is not {n} lowercase hex characters")));
        }
    }
    if stmt.prev_anchor.as_deref().is_some_and(|p| !lower_hex(p, 64)) {
        return Err(bad("`prev_anchor` is not 64 lowercase hex characters"));
    }
    let at = DateTime::parse_from_rfc3339(&stmt.at)
        .map(|t| t.with_timezone(&Utc))
        .ok()
        .filter(|t| ts(*t) == stmt.at)
        .ok_or_else(|| bad("`at` is not canonical RFC 3339 (YYYY-MM-DDTHH:MM:SSZ)"))?;
    if at > now + Duration::seconds(MAX_AHEAD_SECS) {
        return Err(AnchorError::Future);
    }
    Ok(())
}

fn history_keys(view: &RevocationView, id: &str) -> Vec<String> {
    view.key_history(id).iter().map(|c| c.project_pubkey.clone()).collect()
}

/// Load the last accepted statement from the record and the chain, and
/// re-append the chain event when the record is ahead (or its event is gone).
fn load_last(env: &CertEnv, id: &str, view: &RevocationView, epoch: u64) -> Result<Option<Accepted>, AnchorError> {
    // Statements of an earlier epoch are history (the owner reset the anchors).
    let file = read_file(env, id, view).filter(|a| a.epoch == epoch);
    let events = env.chain.tail(0);
    let chain_best = events
        .iter()
        .filter(|e| e.source == ANCHOR_SOURCE && e.kind == KIND_ANCHOR)
        .filter(|e| e.payload.as_ref().and_then(|p| p.get("epoch")).and_then(Value::as_u64).unwrap_or(0) == epoch)
        .filter_map(|e| {
            let statement: ProjectAnchorStmt =
                serde_json::from_value(e.payload.as_ref()?.get("statement")?.clone()).ok()?;
            (statement.project_id == id)
                .then(|| record::seal(env, statement, e.sequence, ident::hex(&e.hash), epoch))
        })
        .max_by_key(|a| a.statement.seq);
    let Some(f) = file else { return Ok(chain_best) };
    if chain_best.as_ref().is_some_and(|c| c.statement.seq >= f.statement.seq) {
        return Ok(chain_best);
    }
    if view.is_compromised(id, &f.statement.project_key_id) {
        warn!(project = id, "anchor record is signed by a compromise-revoked key; not re-appended");
        return Ok(chain_best);
    }
    // The record is ahead of the chain (the chain was not saved before a crash).
    let colliding = events.iter().any(|e| e.sequence == f.user_seq && ident::hex(&e.hash) != f.user_event_hash);
    if colliding {
        warn!(project = id, user_seq = f.user_seq, "user-chain event at the recorded seq differs; re-appending");
    }
    let fixed = append_event(env, &f.statement, Some(&f), epoch);
    write_file(env, &fixed)?;
    info!(project = id, seq = f.statement.seq, user_seq = fixed.user_seq, "recovered project.anchor event from the anchor record");
    Ok(Some(fixed))
}

/// The last statement accepted for `project_id` (index, else record + chain).
pub fn last_accepted(env: &CertEnv, project_id: &str) -> Result<Option<Accepted>, AnchorError> {
    let view = current_view(env)?;
    let mut g = ACCEPT.lock().expect("anchor accept lock poisoned");
    cached_last(&mut g, env, project_id, &view)
}

fn cached_last(
    g: &mut Option<Inner>,
    env: &CertEnv,
    id: &str,
    view: &RevocationView,
) -> Result<Option<Accepted>, AnchorError> {
    let inner = g.get_or_insert_with(Inner::default);
    let key = key_of(env, id);
    if let Some(v) = inner.index.get(&key) {
        return Ok(v.clone());
    }
    let epoch = reset_record::current_epoch(env, id);
    let last = load_last(env, id, view, epoch)?;
    inner.epochs.insert(key.clone(), epoch);
    inner.index.insert(key, last.clone());
    Ok(last)
}

fn cert_error(e: CertError) -> AnchorError {
    match e {
        CertError::BadSignature => AnchorError::BadSignature,
        other => AnchorError::BadStatement(other.to_string()),
    }
}

/// The certificate in force names the statement's key and serial; returns
/// that key.
fn check_cert(
    env: &CertEnv,
    view: &RevocationView,
    stmt: &ProjectAnchorStmt,
    now: DateTime<Utc>,
) -> Result<[u8; 32], AnchorError> {
    let revoked = || AnchorError::KeyRevoked {
        project_id: stmt.project_id.clone(),
        key_id: stmt.project_key_id.clone(),
    };
    if view.is_revoked(&stmt.project_id, &stmt.project_key_id) {
        return Err(revoked());
    }
    let cert = view
        .current_cert(&stmt.project_id)
        .ok_or_else(|| AnchorError::NotCertified(stmt.project_id.clone()))?;
    if cert.project_key_id != stmt.project_key_id {
        return Err(revoked());
    }
    // The certificate may have been sealed by a user key that was rotated
    // out after it was issued (ADR-103 A13); it verifies under that key up
    // to the rotation point. Expiry is judged as before.
    let history = crate::project_cert_rpc::user_history(env).map_err(|e| AnchorError::CertInvalid(e.to_string()))?;
    ident::verify_cert_historic(cert, &history).map_err(|e| AnchorError::CertInvalid(e.to_string()))?;
    if cert.expires_at.as_deref().and_then(|t| DateTime::parse_from_rfc3339(t).ok()).is_some_and(|t| t.with_timezone(&Utc) <= now) {
        return Err(AnchorError::CertInvalid(CertError::Expired.to_string()));
    }
    if stmt.cert_serial != cert.serial {
        return Err(AnchorError::CertInvalid(format!(
            "cert_serial {} is not the certificate in force ({})",
            stmt.cert_serial, cert.serial
        )));
    }
    hex_decode(&cert.project_pubkey)
        .ok_or_else(|| AnchorError::CertInvalid("certificate public key is malformed".into()))
}

/// Verify `stmt` and, when it is the next statement, record it. Pure of the
/// daemon: tests and an in-process transport call this directly.
///
/// The structural checks, the identity view and the signature run outside
/// the accept lock; the lock is taken only once the signature passed, and
/// revocation is re-read under it.
pub fn submit(
    env: &CertEnv,
    stmt: &ProjectAnchorStmt,
    now: DateTime<Utc>,
) -> Result<Accepted, AnchorError> {
    structural(stmt, now)?;
    let pk = check_cert(env, &current_view(env)?, stmt, now)?;
    stmt.verify(&pk).map_err(cert_error)?;

    let key = key_of(env, &stmt.project_id);
    let mut guard = ACCEPT.lock().expect("anchor accept lock poisoned");
    // A revoke or rekey takes the same lock, so this view is current.
    let view = current_view(env)?;
    check_cert(env, &view, stmt, now)?;
    let last = cached_last(&mut guard, env, &stmt.project_id, &view)?;
    // An identical resend (a lost answer) is not a refusal and never backs off.
    if let Some(l) = &last
        && l.statement == *stmt
    {
        // Best effort: the record may have been unwritable the first time.
        if let Err(e) = write_file(env, l) {
            warn!(project = %stmt.project_id, error = %e, "anchor record still not writable");
        }
        return Ok(l.clone());
    }
    // Authenticated from here on: refusals count towards the backoff.
    let inner = guard.as_mut().expect("inner exists");
    if let Some((_, Some(until))) = inner.backoff.get(&key)
        && now < *until
    {
        return Err(AnchorError::Backoff((*until - now).num_seconds().max(1)));
    }
    let outcome = accept(env, &view, stmt, last, inner, &key);
    match &outcome {
        Ok(_) => {
            inner.backoff.remove(&key);
        }
        Err(AnchorError::Store(_)) => {}
        Err(_) => {
            let e = inner.backoff.entry(key).or_insert((0, None));
            e.0 += 1;
            if e.0 >= BACKOFF_AFTER {
                let secs = (1i64 << (e.0 - BACKOFF_AFTER).min(6)).min(BACKOFF_MAX_SECS);
                e.1 = Some(now + Duration::seconds(secs));
            }
        }
    }
    outcome
}

fn accept(
    env: &CertEnv,
    view: &RevocationView,
    stmt: &ProjectAnchorStmt,
    last: Option<Accepted>,
    inner: &mut Inner,
    key: &Key,
) -> Result<Accepted, AnchorError> {
    let resync = |last: Option<Accepted>| {
        last.map(|l| Box::new(Resync { last: l, key_history: history_keys(view, &stmt.project_id) }))
    };
    let want = last.as_ref().map_or(1, |l| l.statement.seq + 1);
    if stmt.seq != want {
        return Err(AnchorError::Seq { got: stmt.seq, want, last: resync(last) });
    }
    if stmt.prev_anchor != last.as_ref().map(|l| l.statement.hash()) {
        return Err(AnchorError::Prev { last: resync(last) });
    }
    if let Some(l) = &last
        && stmt.head_seq < l.statement.head_seq
    {
        return Err(AnchorError::HeadRegress { got: stmt.head_seq, last: l.statement.head_seq });
    }
    if let Some(l) = &last
        && stmt.chain_id != l.statement.chain_id
    {
        return Err(AnchorError::ChainId { got: stmt.chain_id, want: l.statement.chain_id });
    }
    let epoch = inner.epochs.get(key).copied().unwrap_or(0);
    let accepted = append_event(env, stmt, None, epoch);
    // The event is on the chain: publish before the file write, so a failed
    // write (reported as Store; the project retries) cannot make the retry
    // look like a new statement and append a second event.
    inner.index.insert(key.clone(), Some(accepted.clone()));
    write_file(env, &accepted)?;
    Ok(accepted)
}

/// User-daemon startup: bring every `<id>.anchor.json` and the user chain
/// back into agreement (see the module docs). Returns the projects whose
/// chain event was re-appended.
pub fn reconcile(env: &CertEnv) -> Result<Vec<String>, AnchorError> {
    let view = current_view(env)?;
    let mut fixed = Vec::new();
    let Ok(rd) = std::fs::read_dir(&env.manifests_dir) else { return Ok(fixed) };
    let mut g = ACCEPT.lock().expect("anchor accept lock poisoned");
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(id) = name.strip_suffix(".anchor.json") else { continue };
        if validate_id(id).is_err() {
            continue;
        }
        let before = env.chain.sequence();
        cached_last(&mut g, env, id, &view)?;
        if env.chain.sequence() != before {
            fixed.push(id.to_owned());
        }
    }
    Ok(fixed)
}

/// Run [`reconcile`] for the user daemon's own chain (call once after boot).
pub async fn reconcile_startup(kernel: &KernelRef) {
    let env = match crate::project_cert_rpc::env_from_kernel(kernel).await {
        Ok(e) => e,
        Err(e) => return warn!(error = %e, "anchor reconcile skipped"),
    };
    match tokio::task::spawn_blocking(move || reconcile(&env)).await {
        Ok(Ok(f)) if !f.is_empty() => info!(projects = ?f, "re-appended project.anchor events from anchor records"),
        Ok(Ok(_)) => {}
        Ok(Err(e)) => warn!(error = %e, "anchor reconcile failed"),
        Err(e) => warn!(error = %e, "anchor reconcile task failed"),
    }
}

/// `project.anchor.restore`: the owner re-seeds the user daemon's anchor
/// record for a project from the project's own last `project.anchored`
/// event (`statement`, `user_seq`, `user_event_hash`), after the daemon lost
/// its state. The statement must verify under the project's certificate
/// history; a key revoked for compromise is refused. Never rewinds: a record
/// that is already further, or a different statement at the same seq, is
/// refused. The record is sealed with the user key and written, and the user
/// chain event is re-appended (`recovered: true`).
pub fn restore(env: &CertEnv, params: &Value, now: DateTime<Utc>) -> Result<Value, AnchorError> {
    let bad = |m: &str| AnchorError::BadStatement(m.to_owned());
    let stmt: ProjectAnchorStmt = serde_json::from_value(params.get("statement").cloned().ok_or_else(|| bad("missing `statement`"))?)
        .map_err(|e| AnchorError::BadStatement(format!("not an anchor statement: {e}")))?;
    let user_seq = params.get("user_seq").and_then(Value::as_u64).ok_or_else(|| bad("missing `user_seq`"))?;
    let hash = params.get("user_event_hash").and_then(Value::as_str).ok_or_else(|| bad("missing `user_event_hash`"))?;
    if !lower_hex(hash, 64) {
        return Err(bad("`user_event_hash` is not 64 lowercase hex characters"));
    }
    // The statement may be old; only its shape and signature matter here.
    structural(&stmt, now + Duration::days(36_500))?;
    let key = key_of(env, &stmt.project_id);
    let mut guard = ACCEPT.lock().expect("anchor accept lock poisoned");
    let view = current_view(env)?;
    if view.is_compromised(&stmt.project_id, &stmt.project_key_id) {
        return Err(AnchorError::KeyRevoked { project_id: stmt.project_id.clone(), key_id: stmt.project_key_id.clone() });
    }
    let pk = view
        .key_history(&stmt.project_id)
        .iter()
        .find(|c| c.project_key_id == stmt.project_key_id)
        .and_then(|c| hex_decode::<32>(&c.project_pubkey))
        .ok_or_else(|| AnchorError::NotCertified(stmt.project_id.clone()))?;
    stmt.verify(&pk).map_err(cert_error)?;
    let last = cached_last(&mut guard, env, &stmt.project_id, &view)?;
    if let Some(l) = &last
        && (l.statement.seq > stmt.seq || (l.statement.seq == stmt.seq && l.statement != stmt)) {
            return Err(bad("the daemon's record is already at or beyond this statement; restore never rewinds"));
        }
    let epoch = guard.as_ref().and_then(|i| i.epochs.get(&key).copied()).unwrap_or(0);
    let sealed = record::seal(env, stmt, user_seq, hash.to_owned(), epoch);
    write_file(env, &sealed)?;
    let inner = guard.as_mut().expect("inner exists");
    inner.index.remove(&key);
    inner.backoff.remove(&key);
    let fixed = cached_last(&mut guard, env, &sealed.statement.project_id, &view)?.unwrap_or(sealed);
    info!(project = %fixed.statement.project_id, seq = fixed.statement.seq, "anchor record restored by the owner");
    Ok(json!({ "project_id": fixed.statement.project_id, "seq": fixed.statement.seq, "user_seq": fixed.user_seq, "user_event_hash": fixed.user_event_hash }))
}

/// `project.anchor.reset {project_id, reason?}`: the owner retires the
/// project's anchor head after its chain was reset on purpose, so the project
/// can anchor again from genesis (review S2). The old statements stay in the
/// user chain as history. Appends a `project.anchor.reset` event, writes a
/// record sealed with the user key (the new epoch) and drops the in-memory
/// baseline; the next accepted statement must be `seq = 1`, `prev_anchor`
/// absent. Refused when there is no accepted statement in the current epoch.
///
/// Honest limit: the daemon cannot tell a deliberate chain reset from a
/// project that merely lost its anchor state; the owner decides, and a project
/// that did NOT reset keeps submitting `N + 1` and is refused (`anchor_seq`)
/// until it restarts its anchors too. Nothing about the project's key changes.
pub fn reset(env: &CertEnv, params: &Value, now: DateTime<Utc>) -> Result<Value, AnchorError> {
    let bad = |m: &str| AnchorError::BadStatement(m.to_owned());
    let id = params.get("project_id").and_then(Value::as_str).ok_or_else(|| bad("missing `project_id`"))?;
    validate_id(id).map_err(|_| bad("project_id is not a canonical ULID"))?;
    if clawft_types::project::find_by_id(&env.manifests_dir, id)
        .map_err(|e| AnchorError::Store(e.to_string()))?
        .is_none()
    {
        return Err(bad("project is not registered"));
    }
    let reason: String = params
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_control())
        .take(256)
        .collect();
    let key = key_of(env, id);
    let mut guard = ACCEPT.lock().expect("anchor accept lock poisoned");
    let view = current_view(env)?;
    let last = cached_last(&mut guard, env, id, &view)?.ok_or_else(|| AnchorError::NothingToReset(id.to_owned()))?;
    let epoch = last.epoch + 1;
    let at = ts(now);
    let retired_hash = last.statement.hash();
    let ev = env.chain.append(
        ANCHOR_SOURCE,
        KIND_RESET,
        Some(json!({
            "project_id": id,
            "epoch": epoch,
            "retired": {
                "statement_hash": retired_hash,
                "seq": last.statement.seq,
                "head_seq": last.statement.head_seq,
                "chain_id": last.statement.chain_id,
                "user_seq": last.user_seq,
                "user_event_hash": last.user_event_hash,
            },
            "reason": reason,
            "at": at,
        })),
    );
    let mut rec = ResetRecord {
        project_id: id.to_owned(),
        epoch,
        retired_statement_hash: retired_hash,
        retired_seq: last.statement.seq,
        user_seq: ev.sequence,
        user_event_hash: ident::hex(&ev.hash),
        at,
        reason,
        rec_sig: String::new(),
    };
    reset_record::sign(env, &mut rec);
    // The chain event already makes the new epoch current; a failed record
    // write is reported, and the event keeps the reset in force meanwhile.
    let written = reset_record::record(env, &rec);
    let inner = guard.get_or_insert_with(Inner::default);
    inner.epochs.insert(key.clone(), epoch);
    inner.index.insert(key.clone(), None);
    inner.backoff.remove(&key);
    written?;
    info!(project = id, epoch, retired_seq = rec.retired_seq, "anchor head retired by the owner; the project may anchor from genesis");
    Ok(json!({
        "project_id": id,
        "epoch": epoch,
        "retired": { "seq": rec.retired_seq, "statement_hash": rec.retired_statement_hash },
        "user_seq": rec.user_seq,
        "user_event_hash": rec.user_event_hash,
    }))
}

/// Handler for `project.anchor.reset` (Admin, user daemon only, no token).
pub fn handle_reset(call: ExtCall) -> ExtFuture {
    Box::pin(async move {
        if call
            .ctx
            .auth
            .as_deref()
            .is_some_and(|a| a.trim().starts_with(clawft_kernel::token_authority::SECRET_PREFIX))
        {
            return Response::error_with_kind(
                crate::token_rpc::TOKEN_CANNOT_MINT_KIND,
                "a token cannot reset anchors; use the local socket as the owner",
            );
        }
        let env = match crate::project_cert_rpc::env_from(&call.ctx).await {
            Ok(e) => e,
            Err(e) => return AnchorError::Unavailable(e.to_string()).response(),
        };
        let params = call.params;
        match tokio::task::spawn_blocking(move || reset(&env, &params, Utc::now())).await {
            Ok(Ok(v)) => Response::success(v),
            Ok(Err(e)) => e.response(),
            Err(e) => Response::error(format!("anchor reset task failed: {e}")),
        }
    })
}

/// Handler for `project.anchor.restore` (Admin, user daemon only, no token).
pub fn handle_restore(call: ExtCall) -> ExtFuture {
    Box::pin(async move {
        if call
            .ctx
            .auth
            .as_deref()
            .is_some_and(|a| a.trim().starts_with(clawft_kernel::token_authority::SECRET_PREFIX))
        {
            return Response::error_with_kind(
                crate::token_rpc::TOKEN_CANNOT_MINT_KIND,
                "a token cannot restore anchor records; use the local socket as the owner",
            );
        }
        let env = match crate::project_cert_rpc::env_from(&call.ctx).await {
            Ok(e) => e,
            Err(e) => return AnchorError::Unavailable(e.to_string()).response(),
        };
        let params = call.params;
        match tokio::task::spawn_blocking(move || restore(&env, &params, Utc::now())).await {
            Ok(Ok(v)) => Response::success(v),
            Ok(Err(e)) => e.response(),
            Err(e) => Response::error(format!("anchor restore task failed: {e}")),
        }
    })
}

/// Handler for `project.anchor.submit`. Params are the statement itself.
pub fn handle(call: ExtCall) -> ExtFuture {
    Box::pin(async move {
        let env = match crate::project_cert_rpc::env_from(&call.ctx).await {
            Ok(e) => e,
            Err(e) => return AnchorError::Unavailable(e.to_string()).response(),
        };
        let stmt: ProjectAnchorStmt = match serde_json::from_value(call.params) {
            Ok(s) => s,
            Err(e) => return AnchorError::BadStatement(format!("not an anchor statement: {e}")).response(),
        };
        match tokio::task::spawn_blocking(move || submit(&env, &stmt, Utc::now())).await {
            Ok(Ok(a)) => Response::success(json!({
                "user_seq": a.user_seq,
                "user_event_hash": a.user_event_hash,
            })),
            Ok(Err(e)) => e.response(),
            Err(e) => Response::error(format!("anchor task failed: {e}")),
        }
    })
}

/// Parse the success result of `project.anchor.submit` (a transport helper).
pub fn parse_ack(result: &Value) -> Option<AnchorAck> {
    Some(AnchorAck {
        user_seq: result.get("user_seq")?.as_u64()?,
        user_event_hash: result.get("user_event_hash")?.as_str()?.to_owned(),
    })
}

#[cfg(test)]
#[path = "anchor_rpc_tests.rs"]
mod tests;
#[cfg(test)]
#[path = "anchor_rpc_record_tests.rs"]
mod record_tests;
#[cfg(test)]
#[path = "anchor_rpc_reset_tests.rs"]
mod reset_tests;
#[cfg(test)]
#[path = "anchor_rpc_rotation_tests.rs"]
mod rotation_tests;
