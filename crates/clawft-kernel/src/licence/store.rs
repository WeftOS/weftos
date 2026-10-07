//! [`CheckoutGrantStore`]: the accepted binding, the held grants and the
//! clock floor (ADR-106 sections 3 and 4). Persisted atomically, 0600,
//! size-capped, and fail-closed: a file that cannot be read or re-verified
//! poisons the store, which then serves nothing and never overwrites it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use super::floor::FloorState;
use super::persist::write_atomic;
use super::store_load::{SlotFile, StoreFile, load};
use super::{
    BindState, BindingRecord, CheckoutGrant, Clock, GRANT_SKEW_SECS, GrantArtifact, LicenceError,
    LicenceEvent, LicenceEventSink, LocalMeshId, NoopSink, Outcome, SignedEnvelope,
};
use crate::revocation::{RevocationKind, RevocationList};
use crate::workload_pkg::TrustAnchors;

/// File name inside the store directory.
pub const GRANTS_FILE: &str = "checkout_grants.json";
/// Persist the clock high-water mark when it moves this far past the saved one.
const HW_PERSIST_STEP: u64 = 60;

#[derive(Clone)]
pub(super) struct Held<T> {
    pub(super) signed: SignedEnvelope,
    pub(super) body: T,
}

/// The grants held for one (cog, version).
#[derive(Default, Clone)]
pub(super) struct Slot {
    /// The highest-`seq` accepted grant.
    pub(super) current: Option<Held<CheckoutGrant>>,
    /// The newest accepted grant below `current` (restored on a conflict).
    pub(super) previous: Option<Held<CheckoutGrant>>,
    /// Seqs at which two different payloads were seen; refused for good.
    pub(super) conflicted: Vec<u64>,
}

#[derive(Default, Clone)]
pub(super) struct Inner {
    pub(super) binding: Option<Held<BindingRecord>>,
    pub(super) slots: BTreeMap<(String, String), Slot>,
    pub(super) floors: BTreeMap<String, FloorState>,
    pub(super) persisted_hw: u64,
    pub(super) orphan_reported: bool,
    /// Memory is ahead of the file (a restrictive record could not be saved).
    pub(super) dirty: bool,
    pub(super) poisoned: Option<String>,
}

/// A grant that passed every check and is valid right now. Only the store
/// makes one; [`crate::mesh_artifact::ArtifactExchange::grant_checkout`]
/// takes it.
#[derive(Debug, Clone)]
pub struct VerifiedCheckoutGrant {
    grant: CheckoutGrant,
    grant_pubkey_hex: String,
}

impl VerifiedCheckoutGrant {
    /// A verified grant for the test grant key, without going through a store.
    #[cfg(test)]
    pub(super) fn for_test(grant: CheckoutGrant) -> Self {
        let pk = super::tests_common::pk_hex(&super::tests_common::grant_key());
        Self { grant, grant_pubkey_hex: pk }
    }

    /// The grant.
    pub fn grant(&self) -> &CheckoutGrant {
        &self.grant
    }

    /// The bound grant key, lower-case hex (the signer a key revocation names).
    pub fn grant_pubkey_hex(&self) -> &str {
        &self.grant_pubkey_hex
    }
}

/// Persisted binding and grant state of one node.
pub struct CheckoutGrantStore {
    pub(super) path: PathBuf,
    pub(super) anchors: Arc<TrustAnchors>,
    pub(super) local: LocalMeshId,
    pub(super) clock: Clock,
    pub(super) sink: RwLock<Arc<dyn LicenceEventSink>>,
    pub(super) revocations: RwLock<Option<Arc<RevocationList>>>,
    pub(super) inner: Mutex<Inner>,
}

impl std::fmt::Debug for CheckoutGrantStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CheckoutGrantStore").finish_non_exhaustive()
    }
}

pub(super) fn grant_valid(g: &CheckoutGrant, eff_now: u64) -> bool {
    !g.is_withdrawal()
        && eff_now < g.expires_at
        && eff_now < g.licence.expires
        && g.issued_at <= eff_now.saturating_add(GRANT_SKEW_SECS)
}

impl CheckoutGrantStore {
    /// Open the store in `dir`. A missing file is an empty store; a file that
    /// cannot be read, parsed or re-verified is an error (nothing is written).
    pub fn open(
        dir: &Path,
        anchors: Arc<TrustAnchors>,
        local: LocalMeshId,
        clock: Clock,
    ) -> Result<Self, LicenceError> {
        let path = dir.join(GRANTS_FILE);
        let inner = load(&path, &anchors)?;
        Ok(Self::build(path, anchors, local, clock, inner))
    }

    /// [`Self::open`], but a bad file gives a poisoned store (it serves
    /// nothing, refuses writes and leaves the file alone) so the daemon can
    /// still install its policy, which then behaves like `ManifestPolicy`.
    pub fn open_or_poisoned(
        dir: &Path,
        anchors: Arc<TrustAnchors>,
        local: LocalMeshId,
        clock: Clock,
    ) -> Self {
        let path = dir.join(GRANTS_FILE);
        let inner = load(&path, &anchors).unwrap_or_else(|e| Inner {
            poisoned: Some(e.to_string()),
            ..Inner::default()
        });
        Self::build(path, anchors, local, clock, inner)
    }

    fn build(
        path: PathBuf,
        anchors: Arc<TrustAnchors>,
        local: LocalMeshId,
        clock: Clock,
        inner: Inner,
    ) -> Self {
        Self {
            path,
            anchors,
            local,
            clock,
            sink: RwLock::new(Arc::new(NoopSink)),
            revocations: RwLock::new(None),
            inner: Mutex::new(inner),
        }
    }

    /// Report [`LicenceEvent`]s to `sink`.
    pub fn set_sink(&self, sink: Arc<dyn LicenceEventSink>) {
        *self.sink.write().unwrap_or_else(|p| p.into_inner()) = sink;
    }

    /// Use `list` for `SignerKey` and `ArtifactHash` revocations.
    pub fn attach_revocations(&self, list: Arc<RevocationList>) {
        *self.revocations.write().unwrap_or_else(|p| p.into_inner()) = Some(list);
    }

    /// The live local mesh id handle.
    pub fn local_mesh_id(&self) -> &LocalMeshId {
        &self.local
    }

    /// Why the store is poisoned, if it is.
    pub fn poisoned(&self) -> Option<String> {
        self.lock().poisoned.clone()
    }

    pub(super) fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn emit(&self, ev: LicenceEvent) {
        self.sink.read().unwrap_or_else(|p| p.into_inner()).emit(ev);
    }

    /// Run `f` under the state lock, then emit what it queued. Events leave
    /// only after the lock is dropped, so a sink never runs under it (and
    /// must not call back into the store regardless).
    pub(super) fn run<T>(&self, f: impl FnOnce(&mut Inner, &mut Vec<LicenceEvent>) -> T) -> T {
        let mut events = Vec::new();
        let out = {
            let mut g = self.lock();
            f(&mut g, &mut events)
        };
        for e in events {
            self.emit(e);
        }
        out
    }

    pub(super) fn key_revoked(&self, pk_hex: &str) -> bool {
        let g = self.revocations.read().unwrap_or_else(|p| p.into_inner());
        g.as_ref()
            .is_some_and(|l| l.is_subject_revoked(RevocationKind::SignerKey, pk_hex))
    }

    /// True when `blake3_hex` is revoked as an `ArtifactHash`.
    pub fn is_hash_revoked(&self, blake3_hex: &str) -> bool {
        let g = self.revocations.read().unwrap_or_else(|p| p.into_inner());
        g.as_ref()
            .is_some_and(|l| l.is_subject_revoked(RevocationKind::ArtifactHash, blake3_hex))
    }

    /// Why the attached revocation list cannot be read.
    pub fn revocation_list_error(&self) -> Option<String> {
        let guard = self.revocations.read().unwrap_or_else(|poison| poison.into_inner());
        guard.as_ref().and_then(|list| list.subjects_error())
    }

    /// How many times the attached list has changed in this process. `0` when
    /// no list is attached.
    pub fn revocation_generation(&self) -> u64 {
        let guard = self.revocations.read().unwrap_or_else(|poison| poison.into_inner());
        guard.as_ref().map(|list| list.generation()).unwrap_or(0)
    }

    /// Artifact-hash subjects on the attached list. `0` when no list is attached.
    pub fn revoked_artifact_count(&self) -> usize {
        let guard = self.revocations.read().unwrap_or_else(|poison| poison.into_inner());
        guard
            .as_ref()
            .map(|list| list.list_subjects(Some(RevocationKind::ArtifactHash)).len())
            .unwrap_or(0)
    }

    /// Verify an operator notice and apply it. The store's read guard is
    /// dropped before the list lock, so the two locks are not nested.
    pub fn apply_operator_revocation(
        &self,
        signed: &crate::mesh_swarm_revoke::SignedRevocation,
    ) -> Result<bool, String> {
        let notice = crate::mesh_swarm_revoke::verify_revocation(signed, &self.anchors)
            .map_err(|err| err.to_string())?;
        let list = {
            let guard = self.revocations.read().unwrap_or_else(|poison| poison.into_inner());
            guard.clone()
        };
        let Some(list) = list else {
            return Err("the revocation list is not attached on this node".into());
        };
        list.revoke_subject_by(notice.kind, &notice.id, &notice.reason, "operator-notice")
            .map_err(|err| err.to_string())
    }

    fn save(&self, inner: &mut Inner) -> Result<(), LicenceError> {
        let file = StoreFile {
            v: 1,
            binding: inner.binding.as_ref().map(|b| b.signed.clone()),
            grants: inner
                .slots
                .iter()
                .map(|((cog_id, version), s)| SlotFile {
                    cog_id: cog_id.clone(),
                    version: version.clone(),
                    current: s.current.as_ref().map(|h| h.signed.clone()),
                    previous: s.previous.as_ref().map(|h| h.signed.clone()),
                    conflicted: s.conflicted.clone(),
                })
                .collect(),
            floors: inner.floors.clone(),
        };
        let bytes =
            serde_json::to_vec(&file).map_err(|e| LicenceError::Persist(e.to_string()))?;
        write_atomic(&self.path, &bytes)?;
        inner.persisted_hw = inner.floors.values().map(|f| f.hw).max().unwrap_or(0);
        inner.dirty = false;
        Ok(())
    }

    /// Persist `next`, and only then make it the state. On a save error the
    /// state is unchanged.
    pub(super) fn commit(&self, inner: &mut Inner, mut next: Inner) -> Result<(), LicenceError> {
        self.save(&mut next)?;
        *inner = next;
        Ok(())
    }

    /// Make `next` the state even if it cannot be saved (the result is then
    /// [`Outcome::AppliedUnsaved`], and `tick` retries the save). Only for records that
    /// *restrict*: an unbind, a withdrawal, a conflict. A disk fault must
    /// not leave a lapsed grant in force.
    pub(super) fn commit_restrictive(&self, inner: &mut Inner, mut next: Inner) -> Outcome {
        let saved = self.save(&mut next);
        next.dirty = saved.is_err();
        *inner = next;
        match saved {
            Ok(()) => Outcome::Applied,
            Err(e) => {
                tracing::warn!(error = %e, "restrictive licence record applied but not saved; tick retries");
                Outcome::AppliedUnsaved
            }
        }
    }

    /// The accepted binding, or why there is none in effect: not bound, bound
    /// to a mesh id this node no longer computes (orphaned: chained once),
    /// no local mesh id, or poisoned.
    pub(super) fn binding_in_effect(
        &self,
        inner: &mut Inner,
        ev: &mut Vec<LicenceEvent>,
    ) -> Result<BindingRecord, LicenceError> {
        if let Some(p) = &inner.poisoned {
            return Err(LicenceError::Poisoned(p.clone()));
        }
        let local = self.local.get().ok_or(LicenceError::NoLocalMesh)?;
        let b = inner.binding.as_ref().ok_or(LicenceError::NoBinding)?.body.clone();
        if b.mesh_id != local.to_hex() {
            if !inner.orphan_reported {
                inner.orphan_reported = true;
                ev.push(LicenceEvent::BindingOrphaned {
                    stored: b.mesh_id.clone(),
                    local: local.to_hex(),
                });
            }
            return Err(LicenceError::Orphaned);
        }
        inner.orphan_reported = false;
        if b.state != BindState::Bound {
            return Err(LicenceError::Unbound);
        }
        Ok(b)
    }

    /// The time to judge `key`'s grants by: `max(now, floor)`, after
    /// recording the clock in memory. Never touches the disk (see `tick`).
    pub(super) fn eff_now(&self, inner: &mut Inner, key: &str) -> u64 {
        let now = (self.clock)();
        let fs = inner.floors.entry(key.to_owned()).or_default();
        fs.observe(now);
        fs.effective_now(now)
    }

    /// The binding in effect, if any.
    pub fn active_binding(&self) -> Option<BindingRecord> {
        self.binding_status().ok()
    }

    /// The binding in effect, or the reason there is none.
    pub fn binding_status(&self) -> Result<BindingRecord, LicenceError> {
        self.run(|inner, ev| self.binding_in_effect(inner, ev))
    }

    /// The stored binding whatever its state (bound, unbound or orphaned), for
    /// the steward's `seq` rule and for status. Not for gating anything.
    pub fn held_binding(&self) -> Option<BindingRecord> {
        self.lock().binding.as_ref().map(|h| h.body.clone())
    }

    /// The MEMBER profile against this store's anchors and local mesh id,
    /// without storing anything (the steward verifies before it spends any
    /// replay state).
    pub fn verify_member(&self, signed: &super::SignedBinding) -> Result<BindingRecord, LicenceError> {
        let local = self.local.get().ok_or(LicenceError::NoLocalMesh)?;
        super::verify_binding_member(signed, &self.anchors, &local)
    }

    /// Run `f` over every currently valid grant (binding in effect, grant key
    /// not revoked, grant unexpired by `max(now, floor)`).
    fn with_valid<T>(&self, mut f: impl FnMut(&CheckoutGrant, &str) -> Option<T>) -> Option<T> {
        self.run(|inner, ev| {
            let b = self.binding_in_effect(inner, ev).ok()?;
            if self.key_revoked(&b.grant_pubkey) {
                return None;
            }
            let eff = self.eff_now(inner, &b.grant_pubkey);
            inner
                .slots
                .values()
                .filter_map(|s| s.current.as_ref())
                .filter(|h| h.body.mesh_id == b.mesh_id && grant_valid(&h.body, eff))
                .find_map(|h| f(&h.body, &b.grant_pubkey))
        })
    }

    /// A valid grant that lists BLAKE3 `blake3_hex` for `cog_id` `version`.
    pub fn valid_grant_covering(
        &self,
        blake3_hex: &str,
        cog_id: &str,
        version: &str,
    ) -> Option<CheckoutGrant> {
        self.with_valid(|g, _| {
            (g.cog_id == cog_id && g.version == version && g.artifact_by_blake3(blake3_hex).is_some())
                .then(|| g.clone())
        })
    }

    /// A valid grant for `cog_id` `version` that lists a binary whose
    /// (sha256, blake3) pair is exactly `sha256` and `blake3`.
    pub fn valid_grant_for_artifact(
        &self,
        cog_id: &str,
        version: &str,
        sha256: &str,
        blake3: &str,
    ) -> Option<(CheckoutGrant, GrantArtifact)> {
        self.with_valid(|g, _| {
            if g.cog_id != cog_id || g.version != version {
                return None;
            }
            g.artifacts
                .iter()
                .find(|a| a.sha256 == sha256 && a.blake3 == blake3)
                .map(|a| (g.clone(), a.clone()))
        })
    }

    /// Every currently valid grant, ready for `grant_checkout`.
    pub fn verified_grants(&self) -> Vec<VerifiedCheckoutGrant> {
        let mut out = Vec::new();
        self.with_valid::<()>(|g, pk| {
            out.push(VerifiedCheckoutGrant { grant: g.clone(), grant_pubkey_hex: pk.to_owned() });
            None
        });
        out
    }

    /// The floor for the bound key, in unix seconds.
    pub fn floor(&self) -> Option<u64> {
        self.run(|inner, ev| {
            let b = self.binding_in_effect(inner, ev).ok()?;
            self.eff_now(inner, &b.grant_pubkey);
            inner.floors.get(&b.grant_pubkey).map(FloorState::floor)
        })
    }

    /// Admin hook (`weaver cog checkout reset-floor`): forget the clock
    /// high-water mark and restart it from the current clock. This is the
    /// only way to undo a forward clock jump. The caller gates it (Admin).
    pub fn reset_floor(&self) -> Result<(), LicenceError> {
        self.run(|inner, ev| {
            let b = self.binding_in_effect(inner, ev)?;
            let now = (self.clock)();
            let mut next = inner.clone();
            next.floors.entry(b.grant_pubkey).or_default().reset(now);
            ev.push(LicenceEvent::FloorReset(now));
            self.commit(inner, next)
        })
    }

    /// Record the clock and persist the high-water mark (the daemon calls
    /// this on its tick). The only place the read side writes to disk.
    pub fn tick(&self) {
        self.run(|inner, ev| {
            // Retry an unsaved restrictive record first, even when no binding
            // is in effect (an unbind is exactly that case).
            if inner.dirty && inner.poisoned.is_none() {
                let _ = self.save(inner);
            }
            if let Ok(b) = self.binding_in_effect(inner, ev) {
                self.eff_now(inner, &b.grant_pubkey);
                let hw = inner.floors.get(&b.grant_pubkey).map_or(0, |f| f.hw);
                if hw >= inner.persisted_hw.saturating_add(HW_PERSIST_STEP) {
                    let _ = self.save(inner);
                }
            }
        })
    }
}
