//! Retention: which releases may go, and the sweep that removes them.
//!
//! Rules, in order. A release is kept if any applies:
//! 1. it is one of the newest `keep_last` verified releases (so a channel is never emptied);
//! 2. it is pinned;
//! 3. it is younger than `min_age_secs`.
//!
//! Safety properties the sweep holds:
//! - **Only signed releases count.** A manifest that fails verification is never expired and
//!   never takes a `keep_last` slot, so someone with write access cannot push real releases out
//!   with a forged future-dated manifest. It is reported.
//! - **Ids are retired, not freed.** Before a manifest is deleted a tombstone is written
//!   (write-once), and publish refuses a tombstoned build id.
//! - **Blobs go later.** A blob is deleted only if a tombstone listed it, the tombstone is at
//!   least `blob_grace_secs` old, and no remaining manifest references it. The grace covers a
//!   publish that has uploaded blobs but not yet written its manifest.
//! - **Unreadable means stop.** If any manifest cannot be parsed, no blob is deleted.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::backend::{DeletableBackend, PutOutcome};
use crate::error::{invalid, ReleaseError, Result};
use crate::manifest::{hash_bytes, Manifest, SignedManifest, TrustedKeys};
use crate::name::{
    blob_key, build_id_from_manifest_key, manifest_key, tombstone_key, ReleaseStoreName, BLOBS,
    RELEASES, TOMBSTONES,
};

#[derive(Clone, Debug)]
pub struct RetentionPolicy {
    keep_last: usize,
    pub min_age_secs: u64,
    pub pinned: BTreeSet<String>,
}

impl RetentionPolicy {
    /// `keep_last` must be at least 1.
    pub fn new(keep_last: usize, min_age_secs: u64) -> Result<Self> {
        if keep_last == 0 {
            return Err(invalid("retention", "keep_last must be at least 1"));
        }
        Ok(Self { keep_last, min_age_secs, pinned: BTreeSet::new() })
    }

    pub fn pin(mut self, build_id: &str) -> Self {
        self.pinned.insert(build_id.to_string());
        self
    }

    pub fn keep_last(&self) -> usize {
        self.keep_last
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseMeta {
    pub build_id: String,
    pub created_at: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeepReason {
    Newest,
    Pinned,
    TooYoung,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RetentionPlan {
    pub expire: Vec<String>,
    pub keep: Vec<(String, KeepReason)>,
}

/// Pure decision. `now` comes from the caller's clock (ADR-124); a release dated after `now`
/// counts as age 0.
pub fn plan(releases: &[ReleaseMeta], policy: &RetentionPolicy, now: u64) -> RetentionPlan {
    let mut sorted: Vec<&ReleaseMeta> = releases.iter().collect();
    // Newest first; build id breaks ties so the order is total and deterministic.
    // A date after `now` is a bad clock; rank it as now so it cannot hold the newest slot forever.
    let at = |r: &ReleaseMeta| r.created_at.min(now);
    sorted.sort_by(|a, b| at(b).cmp(&at(a)).then_with(|| b.build_id.cmp(&a.build_id)));
    let mut out = RetentionPlan::default();
    for (i, r) in sorted.iter().enumerate() {
        let reason = if i < policy.keep_last {
            Some(KeepReason::Newest)
        } else if policy.pinned.contains(&r.build_id) {
            Some(KeepReason::Pinned)
        } else if now.saturating_sub(at(r)) < policy.min_age_secs {
            Some(KeepReason::TooYoung)
        } else {
            None
        };
        match reason {
            Some(k) => out.keep.push((r.build_id.clone(), k)),
            None => out.expire.push(r.build_id.clone()),
        }
    }
    out
}

/// Written once, before a release's manifest is deleted. It retires the build id.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tombstone {
    pub build_id: String,
    pub removed_at: u64,
    pub reason: String,
    pub manifest_digest: String,
    /// Blob hashes the removed release referenced.
    pub blobs: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    pub expired: Vec<String>,
    pub kept: Vec<(String, KeepReason)>,
    /// Manifests that failed verification. Left alone.
    pub unverified: Vec<(String, String)>,
    pub blobs_deleted: Vec<String>,
    /// Blobs a publish started referencing while they were being deleted, and that were put back.
    pub blobs_restored: Vec<String>,
    /// Blobs left alone because their stored bytes do not match their name: (hash, why).
    pub blobs_skipped: Vec<(String, String)>,
    /// Set when blob deletion was skipped because a manifest could not be parsed.
    pub blob_gc_skipped: Option<String>,
}

pub struct Sweeper<'a, B: DeletableBackend> {
    backend: &'a B,
    store: &'a ReleaseStoreName,
    trusted: &'a TrustedKeys,
}

impl<'a, B: DeletableBackend> Sweeper<'a, B> {
    pub fn new(backend: &'a B, store: &'a ReleaseStoreName, trusted: &'a TrustedKeys) -> Self {
        Self { backend, store, trusted }
    }

    pub async fn sweep(
        &self,
        policy: &RetentionPolicy,
        now: u64,
        blob_grace_secs: u64,
        reason: &str,
    ) -> Result<SweepReport> {
        let mut report = SweepReport::default();

        // 1. Classify every manifest object.
        let mut verified: Vec<Manifest> = Vec::new();
        let mut referenced: BTreeSet<String> = BTreeSet::new();
        let mut unparseable: Option<String> = None;
        let mut ids: Vec<String> = self
            .backend
            .list(RELEASES)
            .await?
            .iter()
            .filter_map(|k| build_id_from_manifest_key(k).map(str::to_string))
            .collect();
        ids.sort();
        for id in ids {
            let Some(bytes) = self.backend.get(&manifest_key(&id)).await? else { continue };
            match SignedManifest::parse(&bytes) {
                Err(e) => {
                    unparseable.get_or_insert(format!("{id}: {e}"));
                    report.unverified.push((id, e.to_string()));
                }
                Ok(signed) => {
                    let verdict = signed
                        .verify(self.trusted, &self.store.uri())
                        .and_then(|()| {
                            if signed.manifest.build_id == id {
                                Ok(())
                            } else {
                                Err(ReleaseError::Untrusted("build id mismatch".into()))
                            }
                        });
                    match verdict {
                        Ok(()) => verified.push(signed.manifest),
                        Err(e) => {
                            // Not trusted, but whatever it names stays referenced. Verified
                            // releases add theirs below, once we know which survive.
                            referenced.extend(signed.manifest.blob_hashes());
                            report.unverified.push((id, e.to_string()));
                        }
                    }
                }
            }
        }

        // 2. Decide, then retire: tombstone first, manifest second.
        let metas: Vec<ReleaseMeta> = verified
            .iter()
            .map(|m| ReleaseMeta { build_id: m.build_id.clone(), created_at: m.created_at })
            .collect();
        let decision = plan(&metas, policy, now);
        for id in &decision.expire {
            let m = verified.iter().find(|m| &m.build_id == id).expect("planned from verified");
            let tomb = Tombstone {
                build_id: id.clone(),
                removed_at: now,
                reason: reason.to_string(),
                manifest_digest: m.digest(),
                blobs: m.blob_hashes(),
            };
            let bytes = serde_json::to_vec_pretty(&tomb).expect("tombstone serializes");
            // Exists means a previous sweep already retired it and crashed before the delete.
            let _: PutOutcome = self.backend.put_if_absent(&tombstone_key(id), &bytes).await?;
            self.backend.delete(&manifest_key(id)).await?;
        }
        let expired: BTreeSet<&String> = decision.expire.iter().collect();
        for m in verified.iter().filter(|m| !expired.contains(&m.build_id)) {
            referenced.extend(m.blob_hashes());
        }
        report.expired = decision.expire;
        report.kept = decision.keep;

        // 3. Blobs.
        if let Some(why) = unparseable {
            report.blob_gc_skipped = Some(why);
            return Ok(report);
        }
        let mut candidates: BTreeSet<String> = BTreeSet::new();
        for key in self.backend.list(TOMBSTONES).await? {
            let Some(bytes) = self.backend.get(&key).await? else { continue };
            let Ok(t) = serde_json::from_slice::<Tombstone>(&bytes) else { continue };
            if t.removed_at.saturating_add(blob_grace_secs) <= now {
                candidates.extend(t.blobs);
            }
        }
        if candidates.is_empty() {
            return Ok(report);
        }
        let existing: BTreeSet<String> = self
            .backend
            .list(BLOBS)
            .await?
            .into_iter()
            .filter_map(|k| k.strip_prefix(BLOBS).map(str::to_string))
            .collect();
        for hash in candidates.iter().filter(|h| existing.contains(*h) && !referenced.contains(*h)) {
            // A publish may have committed a manifest since step 1: look again just before deleting.
            if self.referenced_now().await?.contains(hash) {
                continue;
            }
            // Keep the bytes so the delete can be undone. Without this a publish that reused the
            // blob, saw it present, and committed between the look above and the delete below
            // would leave a manifest naming a blob that is gone.
            let Some(bytes) = self.backend.get(&blob_key(hash)).await? else { continue };
            if hash_bytes(&bytes) != *hash {
                report.blobs_skipped.push((hash.clone(), "stored bytes do not match the name".into()));
                continue;
            }
            self.backend.delete(&blob_key(hash)).await?;
            // Look once more, now that the blob is gone. Every publish that could have been hurt
            // wrote its manifest before this read or heals after it: a manifest written first is seen
            // here and the blob goes back; one written later is followed by the publisher's own
            // re-put, which finds the blob absent. If the manifests cannot be read, undo the delete
            // rather than guess.
            match self.referenced_now().await {
                Ok(still) if !still.contains(hash) => report.blobs_deleted.push(hash.clone()),
                Ok(_) => {
                    self.backend.put_if_absent(&blob_key(hash), &bytes).await?;
                    report.blobs_restored.push(hash.clone());
                }
                Err(e) => {
                    self.backend.put_if_absent(&blob_key(hash), &bytes).await?;
                    return Err(e);
                }
            }
        }
        Ok(report)
    }
}

impl<B: DeletableBackend> Sweeper<'_, B> {
    /// Hashes named by every manifest object right now, trusted or not. Unreadable means "everything
    /// is referenced" is not representable, so an unreadable manifest is an error and stops the delete.
    async fn referenced_now(&self) -> Result<BTreeSet<String>> {
        let mut out = BTreeSet::new();
        for key in self.backend.list(RELEASES).await? {
            if build_id_from_manifest_key(&key).is_none() {
                continue;
            }
            let Some(bytes) = self.backend.get(&key).await? else { continue };
            out.extend(SignedManifest::parse(&bytes)?.manifest.blob_hashes());
        }
        Ok(out)
    }
}

/// True if `err` means a build id is retired (convenience for callers mapping to exit codes).
pub fn is_retired(err: &ReleaseError) -> bool {
    matches!(err, ReleaseError::Retired { .. })
}
