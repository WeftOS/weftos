//! How long archives live (ADR-126 open question 2), for the stores S1 archives itself: nodes
//! (unpair), projects and releases. Worktree archives are S2's (`store worktree gc`) and are never
//! touched here.
//!
//! The archive bucket holds sets of objects keyed `<bucket>@<archived ms>/<key>` (see
//! [`archive_key`]). A set is one archive of one store. Nothing expires unless the operator names a
//! number of days for that kind of store, and even then a run only reports what it would delete
//! until it is `confirmed`. Whole sets go together, oldest first, never part of one.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::json;

use super::{ARCHIVE_BUCKET, StoreService};
use crate::StoreError;
use crate::backend::BackendError;

const DAY_MS: u64 = 86_400_000;

/// Days to keep an archive, per kind of store. `None` keeps that kind for ever (the default: an
/// archive is recovery data). Worktree archives are not here on purpose.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct ArchiveKeep {
    pub node_days: Option<u64>,
    pub project_days: Option<u64>,
    pub release_days: Option<u64>,
}

impl ArchiveKeep {
    fn days_for(&self, kind: &str) -> Option<u64> {
        match kind {
            "node" => self.node_days,
            "project" => self.project_days,
            "release" => self.release_days,
            _ => None,
        }
    }

    /// Zero days would delete an archive the moment it is made: refused.
    pub fn validate(&self) -> Result<(), StoreError> {
        for (what, d) in [("node", self.node_days), ("project", self.project_days), ("release", self.release_days)] {
            if d == Some(0) {
                return Err(StoreError::Refused(format!("archive keep for {what} stores must be at least 1 day (leave it off to keep for ever)")));
            }
        }
        Ok(())
    }
}

/// One archive of one store, as found in the archive bucket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ArchiveSet {
    /// The store's bucket.
    pub bucket: String,
    /// `node`, `project` or `release`.
    pub kind: String,
    /// When the archive began, ms since the epoch.
    pub archived_ms: u64,
    pub age_days: u64,
    pub objects: u64,
    pub bytes: u64,
}

/// What an archive sweep found and did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ArchiveGcReport {
    /// Nothing was deleted: the run was not confirmed.
    pub dry_run: bool,
    /// Sets past their keep time (deleted when confirmed).
    pub expired: Vec<ArchiveSet>,
    /// Sets past their keep time that were left alone because their archive is still being written.
    pub in_progress: Vec<ArchiveSet>,
    /// Sets kept (inside their time, or their kind has no limit).
    pub kept: u64,
    /// Sets of worktree stores, which belong to `store worktree gc`, and keys that are not archive keys at all.
    pub not_ours: u64,
    pub deleted_objects: u64,
}

/// `<bucket>@<ms>/<key>` -> (bucket, ms).
fn parse_archive_key(k: &str) -> Option<(&str, u64)> {
    let (head, _rest) = k.split_once('/')?;
    let (bucket, ts) = head.rsplit_once('@')?;
    (!bucket.is_empty()).then_some(())?;
    Some((bucket, ts.parse().ok()?))
}

/// The kind of store a bucket name belongs to (`n-`, `p-`, `r-`, `w-`).
fn kind_of(bucket: &str) -> Option<&'static str> {
    match bucket.split_once('-')?.0 {
        "n" => Some("node"),
        "p" => Some("project"),
        "r" => Some("release"),
        "w" => Some("worktree"),
        _ => None,
    }
}

impl StoreService {
    /// Sweep the archive bucket for archives past `keep`. A dry run (`confirmed` false) only
    /// reports. An archive that a store record shows as still being written is never deleted. Safe to
    /// repeat after a failure: it deletes whole sets, and a half-deleted set is finished next time.
    pub fn archive_gc(&self, keep: &ArchiveKeep, confirmed: bool, actor: &str) -> Result<ArchiveGcReport, StoreError> {
        keep.validate()?;
        let now = self.now();
        let objs = match self.objects.list(ARCHIVE_BUCKET, "") {
            Ok(o) => o,
            // No archive has ever been made.
            Err(BackendError::NoBucket(_)) => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        // (bucket, archived ms) -> keys and bytes
        let mut sets: BTreeMap<(String, u64), (Vec<String>, u64)> = BTreeMap::new();
        let mut report = ArchiveGcReport { dry_run: !confirmed, ..Default::default() };
        for m in &objs {
            match parse_archive_key(&m.key) {
                Some((b, ts)) => {
                    let e = sets.entry((b.to_owned(), ts)).or_default();
                    e.0.push(m.key.clone());
                    e.1 += m.size;
                }
                None => report.not_ours += 1,
            }
        }
        let unfinished: Vec<(String, u64)> = self
            .lock()
            .stores
            .values()
            .filter(|r| r.archived && !r.archive_done)
            .filter_map(|r| Some((crate::name::StoreName::parse(&r.name).ok()?.bucket(), r.archive_ts?)))
            .collect();
        let mut doomed: Vec<Vec<String>> = Vec::new();
        for ((bucket, ts), (keys, bytes)) in sets {
            let Some(kind) = kind_of(&bucket) else {
                report.not_ours += 1;
                continue;
            };
            if kind == "worktree" {
                report.not_ours += 1;
                continue;
            }
            let age_ms = now.saturating_sub(ts);
            let set = ArchiveSet { bucket: bucket.clone(), kind: kind.to_owned(), archived_ms: ts, age_days: age_ms / DAY_MS, objects: keys.len() as u64, bytes };
            let Some(days) = keep.days_for(kind) else {
                report.kept += 1;
                continue;
            };
            if age_ms < days.saturating_mul(DAY_MS) {
                report.kept += 1;
            } else if unfinished.contains(&(bucket, ts)) {
                report.in_progress.push(set);
            } else {
                report.expired.push(set);
                doomed.push(keys);
            }
        }
        if confirmed {
            for key in doomed.iter().flatten() {
                match self.objects.delete(ARCHIVE_BUCKET, key) {
                    Ok(()) | Err(BackendError::NoObject(_)) => report.deleted_objects += 1,
                    Err(e) => return Err(e.into()),
                }
            }
            if report.deleted_objects > 0 {
                self.chain.record(
                    "store.archive.gc",
                    json!({"actor": actor, "sets": report.expired.len(), "objects": report.deleted_objects, "keep": keep}),
                );
            }
        }
        Ok(report)
    }
}

#[cfg(test)]
#[path = "archive_gc_tests.rs"]
mod tests;
