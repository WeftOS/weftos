//! ADR-126 §4 promotion: when a worktree lands, objects under promotable prefixes are copied
//! into the project bucket and everything else is archived. Worktree bucket deletion stays with
//! the daemon (§3); this module only reports when it is safe via [`is_promoted`].
//!
//! Planning happens before any write, so a conflict under [`OnConflict::Fail`] leaves both
//! buckets untouched. The run is idempotent: the report is stored at `.wt/promoted.json`.

use crate::overlay::{
    ObjectBackend, Overlay, Result, S2Error, is_reserved, read_base, validate_key,
};
use serde::{Deserialize, Serialize};

pub const PROMOTED_KEY: &str = ".wt/promoted.json";
pub const META_KEY: &str = ".wt/meta.json";
pub const PROMOTING_KEY: &str = ".wt/promoting.json";

/// Written before the first project write so a retry can tell its own earlier copies from
/// someone else's changes.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Intent {
    copy: Vec<(String, u64)>,
}

/// What to do when the project object changed after this worktree was created.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnConflict {
    /// Abort before writing anything (default).
    Fail,
    /// Keep the project version and archive the worktree's.
    KeepProject,
    /// Replace the project version with the worktree's.
    Overwrite,
}

#[derive(Debug, Clone)]
pub struct PromotionPolicy {
    /// Key prefixes (ending in `/`) whose objects are promoted. Everything else is archived.
    pub promote_prefixes: Vec<String>,
    pub on_conflict: OnConflict,
}

impl Default for PromotionPolicy {
    /// `agents/<instance>/` is per-agent scratch and is archived, never promoted (ADR-126 §4).
    fn default() -> Self {
        Self {
            promote_prefixes: vec!["assets/".into(), "shared/".into()],
            on_conflict: OnConflict::Fail,
        }
    }
}

impl PromotionPolicy {
    fn promotable(&self, key: &str) -> bool {
        self.promote_prefixes
            .iter()
            .any(|p| key.starts_with(p.as_str()))
    }
    fn validate(&self) -> Result<()> {
        for p in &self.promote_prefixes {
            validate_key(p.trim_end_matches('/'))?;
            if !p.ends_with('/') {
                return Err(S2Error::Invalid(format!(
                    "promote prefix must end in '/': {p:?}"
                )));
            }
        }
        Ok(())
    }
}

/// Stored at [`META_KEY`] when the worktree store is created.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorktreeMeta {
    pub slug: String,
    pub project: String,
    pub branch: String,
    pub base_commit: String,
    pub created_ms: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PromotionReport {
    pub promoted: Vec<String>,
    pub deleted: Vec<String>,
    pub kept_project: Vec<String>,
    pub archived: Vec<String>,
    pub dropped_tombstones: Vec<String>,
    pub promoted_ms: u64,
}

pub fn write_meta<B: ObjectBackend>(
    b: &B,
    worktree_bucket: &str,
    meta: &WorktreeMeta,
) -> Result<()> {
    b.put(worktree_bucket, META_KEY, &serde_json::to_vec_pretty(meta)?)?;
    Ok(())
}

pub fn read_meta<B: ObjectBackend>(b: &B, worktree_bucket: &str) -> Result<Option<WorktreeMeta>> {
    match b.get(worktree_bucket, META_KEY)? {
        Some(d) => Ok(Some(serde_json::from_slice(&d)?)),
        None => Ok(None),
    }
}

/// `Some(report)` once promotion has completed for this worktree.
pub fn is_promoted<B: ObjectBackend>(
    b: &B,
    worktree_bucket: &str,
) -> Result<Option<PromotionReport>> {
    match b.get(worktree_bucket, PROMOTED_KEY)? {
        Some(d) => Ok(Some(serde_json::from_slice(&d)?)),
        None => Ok(None),
    }
}

/// Where non-promoted objects go: `<bucket>/<prefix><key>`.
pub struct ArchiveTarget<'a> {
    pub bucket: &'a str,
    pub prefix: &'a str,
}

/// Did the project change this key since the worktree first touched it? With no recorded base
/// (an object written before base tracking, or written around the overlay) assume yes: failing
/// safe means a `Fail` policy asks a human rather than overwriting someone's edit.
fn project_moved<B: ObjectBackend>(
    b: &B,
    worktree: &str,
    key: &str,
    project_etag: &str,
) -> Result<bool> {
    Ok(match read_base(b, worktree, key)? {
        Some(base) => base != project_etag,
        None => true,
    })
}

/// Do the worktree object and the project object hold the same bytes? Equal etags say so cheaply;
/// a server-side copy of a multipart object changes the etag, so equal sizes fall back to a
/// byte comparison (only on the rare retry path).
fn same_content<B: ObjectBackend>(
    b: &B,
    worktree: &str,
    project: &str,
    w: &crate::overlay::ObjectMeta,
    p: &crate::overlay::ObjectMeta,
) -> Result<bool> {
    if w.etag == p.etag {
        return Ok(true);
    }
    if w.size != p.size {
        return Ok(false);
    }
    Ok(match (b.get(worktree, &w.key)?, b.get(project, &w.key)?) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    })
}

pub fn promote<B: ObjectBackend>(
    b: &B,
    worktree_bucket: &str,
    project_bucket: &str,
    archive: &ArchiveTarget<'_>,
    policy: &PromotionPolicy,
    now_ms: u64,
) -> Result<PromotionReport> {
    policy.validate()?;
    validate_key(archive.prefix.trim_end_matches('/'))?;
    if worktree_bucket == project_bucket {
        return Err(S2Error::Invalid(
            "worktree and project bucket are the same".into(),
        ));
    }
    if let Some(done) = is_promoted(b, worktree_bucket)? {
        return Ok(done);
    }
    let meta = read_meta(b, worktree_bucket)?.ok_or_else(|| {
        S2Error::Invalid("worktree has no .wt/meta.json; refusing to promote".into())
    })?;
    let _ = &meta; // presence is the check; `created_ms` is informational
    let ov = Overlay::new(b, worktree_bucket, project_bucket);

    // Plan: no writes yet.
    enum Act {
        Copy(String, u64),
        Keep(String),
        Archive(String),
    }
    let prior: Intent = match b.get(worktree_bucket, PROMOTING_KEY)? {
        Some(d) => serde_json::from_slice(&d)?,
        None => Intent::default(),
    };
    let locals = ov.local_objects()?;
    let local_keys: std::collections::HashSet<&str> =
        locals.iter().map(|o| o.key.as_str()).collect();
    let mut acts = Vec::new();
    let mut conflicts = Vec::new();
    for o in &locals {
        if !policy.promotable(&o.key) {
            acts.push(Act::Archive(o.key.clone()));
            continue;
        }
        match b.head(project_bucket, &o.key)? {
            // Our own earlier copy only if the bytes are the worktree object's (review F5): a size
            // match alone would let a changed project object be overwritten under `Fail`.
            Some(p)
                if prior.copy.iter().any(|(k, n)| k == &o.key && *n == p.size)
                    && same_content(b, worktree_bucket, project_bucket, o, &p)? =>
            {
                acts.push(Act::Copy(o.key.clone(), o.size))
            }
            Some(p) if p.etag != o.etag && project_moved(b, worktree_bucket, &o.key, &p.etag)? => {
                match policy.on_conflict {
                    OnConflict::Fail => conflicts.push(o.key.clone()),
                    OnConflict::KeepProject => acts.push(Act::Keep(o.key.clone())),
                    OnConflict::Overwrite => acts.push(Act::Copy(o.key.clone(), o.size)),
                }
            }
            _ => acts.push(Act::Copy(o.key.clone(), o.size)),
        }
    }
    let mut deletes = Vec::new();
    let mut dropped = Vec::new();
    for k in ov.tombstones()? {
        // Object and tombstone together (a put that crashed before clearing it): the object wins.
        if local_keys.contains(k.as_str()) {
            dropped.push(k);
            continue;
        }
        if !policy.promotable(&k) {
            dropped.push(k);
            continue;
        }
        match b.head(project_bucket, &k)? {
            Some(p)
                if policy.on_conflict != OnConflict::Overwrite
                    && project_moved(b, worktree_bucket, &k, &p.etag)? =>
            {
                if policy.on_conflict == OnConflict::Fail {
                    conflicts.push(k.clone());
                }
                dropped.push(k);
            }
            Some(_) => deletes.push(k),
            None => dropped.push(k),
        }
    }
    if !conflicts.is_empty() {
        conflicts.sort();
        return Err(S2Error::Conflict(conflicts));
    }

    let intent = Intent {
        copy: acts
            .iter()
            .filter_map(|a| {
                if let Act::Copy(k, n) = a {
                    Some((k.clone(), *n))
                } else {
                    None
                }
            })
            .collect(),
    };
    b.put(
        worktree_bucket,
        PROMOTING_KEY,
        &serde_json::to_vec(&intent)?,
    )?;

    // Apply. Archive first so nothing is lost if a later copy fails.
    let mut report = PromotionReport {
        promoted_ms: now_ms,
        ..Default::default()
    };
    let arch_key = |k: &str| format!("{}{}", archive.prefix, k);
    for a in &acts {
        match a {
            Act::Archive(k) | Act::Keep(k) => {
                b.copy(worktree_bucket, k, archive.bucket, &arch_key(k))?;
            }
            Act::Copy(..) => {}
        }
    }
    for a in acts {
        match a {
            Act::Copy(k, _) => {
                b.copy(worktree_bucket, &k, project_bucket, &k)?;
                report.promoted.push(k);
            }
            Act::Keep(k) => report.kept_project.push(k),
            Act::Archive(k) => report.archived.push(k),
        }
    }
    for k in deletes {
        b.delete(project_bucket, &k)?;
        report.deleted.push(k);
    }
    report.dropped_tombstones = dropped;
    // Reserved state (shared tree, bundle, manifest, handoff) is archived too: it is the record of
    // what was unmerged.
    for o in b.list(worktree_bucket, crate::overlay::RESERVED)? {
        if o.key == PROMOTING_KEY || o.key == PROMOTED_KEY {
            continue;
        }
        debug_assert!(is_reserved(&o.key));
        b.copy(worktree_bucket, &o.key, archive.bucket, &arch_key(&o.key))?;
    }
    b.put(
        worktree_bucket,
        PROMOTED_KEY,
        &serde_json::to_vec_pretty(&report)?,
    )?;
    Ok(report)
}
