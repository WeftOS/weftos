//! Follower reports computed from the registry alone or from two manifests: forgotten worktrees
//! ([`leftovers`]) and cross-worktree file conflicts ([`overlaps`]). Split out of `follower`
//! (re-exported there, so paths are unchanged).

use crate::follower::verify_entry;
use crate::overlay::{ObjectBackend, Result, S2Error};
use crate::share::{CLAIM_TTL_MS, ChangeKind, State, read_manifest};
use crate::share_fetch::RegistryEntry;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy)]
pub struct LeftoverPolicy {
    /// An `Open` worktree with no update this long is forgotten work.
    pub open_idle_ms: u64,
    /// A `Handoff` that nobody claimed this long needs a nudge. Also the limit for a `Merged` or
    /// `Archived` entry still in the registry (cleanup did not finish).
    pub unclaimed_ms: u64,
    /// A timestamp this far ahead of the follower's clock is reported as skew, not trusted.
    pub skew_ms: u64,
}
impl Default for LeftoverPolicy {
    fn default() -> Self {
        Self {
            open_idle_ms: 3 * 24 * 3600 * 1000,
            unclaimed_ms: 24 * 3600 * 1000,
            skew_ms: 3600 * 1000,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    ForgottenOpen,
    UnclaimedHandoff,
    AbandonedClaim,
    /// Merged or archived but never removed from the registry.
    StuckCleanup,
    /// Marked orphan by the daemon (its local path went missing); `idle_ms` is the orphan age.
    Orphan,
    /// `updated_ms` is in the future: the writer's clock is ahead, so idle time is unknown.
    FutureTimestamp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Leftover {
    pub entry: RegistryEntry,
    pub idle_ms: u64,
    pub why: Why,
}

/// Unmerged worktrees nobody is finishing, oldest first. `now_ms` and the registry's `updated_ms`
/// come from different machines' clocks, so thresholds should be generous (days, not minutes).
pub fn leftovers(entries: &[RegistryEntry], now_ms: u64, p: &LeftoverPolicy) -> Vec<Leftover> {
    let mut v: Vec<_> = entries
        .iter()
        .filter_map(|e| {
            let idle = now_ms.saturating_sub(e.updated_ms);
            if e.updated_ms > now_ms.saturating_add(p.skew_ms)
                && !matches!(e.state, State::Merged | State::Archived)
            {
                return Some(Leftover {
                    entry: e.clone(),
                    idle_ms: 0,
                    why: Why::FutureTimestamp,
                });
            }
            if e.state == State::Orphan {
                // an orphan stays visible until gc retires it; its age counts from when it was marked
                let since = e.orphan.as_ref().map_or(e.updated_ms, |o| o.orphaned_at_ms);
                return Some(Leftover { entry: e.clone(), idle_ms: now_ms.saturating_sub(since), why: Why::Orphan });
            }
            let why = match e.state {
                State::Merged | State::Archived if idle >= p.unclaimed_ms => Why::StuckCleanup,
                State::Open if idle >= p.open_idle_ms => Why::ForgottenOpen,
                State::Handoff if idle >= p.unclaimed_ms => Why::UnclaimedHandoff,
                State::Claimed if idle >= CLAIM_TTL_MS => Why::AbandonedClaim,
                _ => return None,
            };
            Some(Leftover {
                entry: e.clone(),
                idle_ms: idle,
                why,
            })
        })
        .collect();
    v.sort_by(|a, c| {
        c.idle_ms
            .cmp(&a.idle_ms)
            .then_with(|| a.entry.slug.cmp(&c.entry.slug))
    });
    v
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overlap {
    pub path: String,
    pub in_a: String,
    pub in_b: String,
    pub kind: OverlapKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlapKind {
    /// Both trees hold the file with different content.
    BothChanged,
    /// One worktree deleted the file; the other changed it. `deleted_in` is the deleting slug.
    ModifyDelete { deleted_in: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlapReport {
    /// Both trees branch from the same base commit. If `false`, a path can differ only because
    /// the base moved, so treat `overlaps` as a superset of the real conflicts.
    pub same_base: bool,
    /// The base commits of the two trees (`a` then `b`), so a caller can discount the report
    /// when they differ: on different bases a path can differ only because the base moved.
    pub base_a: String,
    pub base_b: String,
    /// Files in both trees with different content, and files one worktree deleted that the other
    /// changed (needs `changes` in both manifests; a rename counts as delete + add). Absence is not
    /// proof of no conflict: only paths, never hunks, are compared.
    pub overlaps: Vec<Overlap>,
}

/// Files that both unmerged worktrees contain with *different* content, before either is merged.
pub fn overlaps<B: ObjectBackend>(
    b: &B,
    a: &RegistryEntry,
    c: &RegistryEntry,
) -> Result<OverlapReport> {
    if a.bucket == c.bucket {
        return Err(S2Error::Invalid(
            "both entries point at the same bucket".into(),
        ));
    }
    verify_entry(b, a)?;
    verify_entry(b, c)?;
    let ma = read_manifest(b, &a.bucket)?
        .ok_or_else(|| S2Error::Invalid(format!("{} has no published tree", a.slug)))?;
    let mc = read_manifest(b, &c.bucket)?
        .ok_or_else(|| S2Error::Invalid(format!("{} has no published tree", c.slug)))?;
    let other: BTreeMap<&str, &str> = mc
        .files
        .iter()
        .map(|f| (f.path.as_str(), f.sha256.as_str()))
        .collect();
    // Every worktree holds the whole project tree, so a file one side edited and the other did not
    // differs too. When both manifests say what they changed, only a path changed on both sides
    // (and not deleted on either; that case is below) is a conflict candidate. With no change list
    // on either side (a manifest from before the field existed) every differing path is reported,
    // a superset.
    let touched = |m: &crate::share::TreeManifest| -> std::collections::BTreeSet<String> {
        m.changes
            .iter()
            .filter(|x| x.kind != ChangeKind::Deleted)
            .map(|x| x.path.clone())
            .collect()
    };
    let exact = !ma.changes.is_empty() && !mc.changes.is_empty();
    let (ta, tc) = (touched(&ma), touched(&mc));
    let mut v: Vec<_> = ma
        .files
        .iter()
        .filter(|f| other.get(f.path.as_str()).is_some_and(|s| *s != f.sha256))
        .filter(|f| !exact || (ta.contains(&f.path) && tc.contains(&f.path)))
        .map(|f| Overlap {
            path: f.path.clone(),
            in_a: a.slug.clone(),
            in_b: c.slug.clone(),
            kind: OverlapKind::BothChanged,
        })
        .collect();
    for (del, del_slug, chg) in [(&ma, &a.slug, &mc), (&mc, &c.slug, &ma)] {
        for d in del.changes.iter().filter(|x| x.kind == ChangeKind::Deleted) {
            let touched = chg
                .changes
                .iter()
                .any(|x| x.path == d.path && x.kind != ChangeKind::Deleted);
            if touched {
                v.push(Overlap {
                    path: d.path.clone(),
                    in_a: a.slug.clone(),
                    in_b: c.slug.clone(),
                    kind: OverlapKind::ModifyDelete {
                        deleted_in: del_slug.clone(),
                    },
                });
            }
        }
    }
    v.sort_by(|x, y| x.path.cmp(&y.path));
    Ok(OverlapReport {
        same_base: ma.base_commit == mc.base_commit,
        base_a: ma.base_commit.clone(),
        base_b: mc.base_commit.clone(),
        overlaps: v,
    })
}
