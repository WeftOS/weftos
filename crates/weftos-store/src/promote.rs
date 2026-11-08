//! ADR-126 §4 promotion: when a worktree lands, objects under promotable prefixes are copied
//! into the project bucket and everything else is archived. Worktree bucket deletion stays with
//! the daemon (§3); this module only reports when it is safe via [`is_promoted`].
//!
//! Planning happens before any write, so a conflict under [`OnConflict::Fail`] leaves both
//! buckets untouched. The run is idempotent: the report is stored at `.wt/promoted.json`.

use crate::overlay::{
    BASE_ABSENT, ObjectBackend, Overlay, Result, S2Error, is_reserved, read_base, validate_key,
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
    /// Refuse a land that would copy more than this many bytes into the project (`None`: no cap).
    pub max_promote_bytes: Option<u64>,
}

impl Default for PromotionPolicy {
    /// `agents/<instance>/` is per-agent scratch and is archived, never promoted (ADR-126 §4).
    fn default() -> Self {
        Self {
            promote_prefixes: vec!["assets/".into(), "shared/".into()],
            on_conflict: OnConflict::Fail,
            max_promote_bytes: Some(DEFAULT_MAX_PROMOTE_BYTES),
        }
    }
}

/// Default cap on one land: 1 GiB copied into the project.
pub const DEFAULT_MAX_PROMOTE_BYTES: u64 = 1 << 30;

impl PromotionPolicy {
    fn promotable(&self, key: &str) -> bool {
        self.promote_prefixes
            .iter()
            .any(|p| key.starts_with(p.as_str()))
    }
    fn check_size(&self, bytes: u64) -> Result<()> {
        match self.max_promote_bytes {
            Some(max) if bytes > max => Err(S2Error::Invalid(format!(
                "landing would copy {bytes} bytes into the project, over the {max} byte cap; \
                 promote a prefix at a time or raise the cap"
            ))),
            _ => Ok(()),
        }
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
    /// `created_ms` of the worktree metadata this report belongs to. A report from an earlier
    /// life of a reused slug does not match the new metadata and is ignored (review F6).
    #[serde(default)]
    pub generation_ms: u64,
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
        Some(d) => {
            let r: PromotionReport = serde_json::from_slice(&d)?;
            match read_meta(b, worktree_bucket)? {
                Some(m) if m.created_ms != r.generation_ms && r.generation_ms != 0 => Ok(None),
                _ => Ok(Some(r)),
            }
        }
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

/// Keep the project version that a promotion is about to overwrite or delete, under the archive's
/// reserved `.wt/displaced/` prefix (worktree keys can never start with `.wt/`).
fn displace<B: ObjectBackend>(
    b: &B,
    project: &str,
    key: &str,
    archive: &ArchiveTarget<'_>,
) -> Result<()> {
    if b.head(project, key)?.is_some() {
        b.copy(project, key, archive.bucket, &format!("{}.wt/displaced/{}", archive.prefix, key))?;
    }
    Ok(())
}

/// Re-check, right before a delete, that the project object is still the one the plan was made
/// against (backends have no conditional delete, so this narrows the window to one head call
/// rather than closing it). Copies use [`ObjectBackend::put_cas`] instead. Earlier writes of the
/// same run stay valid: a retry sees them as already promoted.
fn still_unchanged<B: ObjectBackend>(
    b: &B,
    project: &str,
    key: &str,
    planned: Option<&str>,
) -> Result<()> {
    let now = b.head(project, key)?.map(|m| m.etag);
    if now.as_deref() != planned {
        return Err(S2Error::Conflict(vec![key.to_string()]));
    }
    Ok(())
}

enum Act {
    Copy(String, u64, Option<String>),
    Keep(String),
    Archive(String),
}

/// Why a key cannot be landed under `OnConflict::Fail`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictKind {
    /// The worktree edited it and the project changed it too since the worktree first saw it.
    BothEdited,
    /// The worktree edited it and the project deleted it.
    ProjectDeleted,
    /// The worktree deleted it and the project changed it since.
    DeletedButProjectEdited,
}

struct Plan {
    acts: Vec<Act>,
    deletes: Vec<(String, String)>,
    dropped: Vec<String>,
    conflicts: Vec<(String, ConflictKind)>,
    /// Bytes the land would copy into the project.
    total_bytes: u64,
}

/// Decide what landing would do. Reads only; nothing is written.
fn plan<B: ObjectBackend>(
    b: &B,
    ov: &Overlay<'_, B>,
    worktree_bucket: &str,
    project_bucket: &str,
    policy: &PromotionPolicy,
) -> Result<Plan> {
    let locals = ov.local_objects()?;
    let mut total_bytes = 0u64;
    let local_keys: std::collections::HashSet<&str> =
        locals.iter().map(|o| o.key.as_str()).collect();
    let mut acts = Vec::new();
    let mut conflicts: Vec<(String, ConflictKind)> = Vec::new();
    for o in &locals {
        if !policy.promotable(&o.key) {
            acts.push(Act::Archive(o.key.clone()));
            continue;
        }
        let planned = b.head(project_bucket, &o.key)?.map(|m| m.etag);
        match b.head(project_bucket, &o.key)? {
            Some(p) if p.etag != o.etag && project_moved(b, worktree_bucket, &o.key, &p.etag)? => {
                match policy.on_conflict {
                    OnConflict::Fail => conflicts.push((o.key.clone(), ConflictKind::BothEdited)),
                    OnConflict::KeepProject => acts.push(Act::Keep(o.key.clone())),
                    OnConflict::Overwrite => {
                        total_bytes += o.size;
                        acts.push(Act::Copy(o.key.clone(), o.size, planned.clone()))
                    }
                }
            }
            // The project deleted what this worktree edited: resurrecting it silently is a conflict.
            None if matches!(read_base(b, worktree_bucket, &o.key)?, Some(base) if base != BASE_ABSENT) => {
                match policy.on_conflict {
                    OnConflict::Fail => conflicts.push((o.key.clone(), ConflictKind::ProjectDeleted)),
                    OnConflict::KeepProject => acts.push(Act::Keep(o.key.clone())),
                    OnConflict::Overwrite => {
                        total_bytes += o.size;
                        acts.push(Act::Copy(o.key.clone(), o.size, planned.clone()))
                    }
                }
            }
            _ => {
                total_bytes += o.size;
                acts.push(Act::Copy(o.key.clone(), o.size, planned.clone()))
            }
        }
    }
    let mut deletes: Vec<(String, String)> = Vec::new();
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
                    conflicts.push((k.clone(), ConflictKind::DeletedButProjectEdited));
                }
                dropped.push(k);
            }
            Some(p) => deletes.push((k, p.etag)),
            None => dropped.push(k),
        }
    }
    Ok(Plan { acts, deletes, dropped, conflicts, total_bytes })
}

/// What a land would do right now, without doing it.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct Preview {
    pub would_promote: Vec<String>,
    pub would_keep_project: Vec<String>,
    pub would_archive: Vec<String>,
    pub would_delete: Vec<String>,
    pub would_drop_tombstones: Vec<String>,
    /// Keys that make a land fail under `OnConflict::Fail`, with why. Empty under the other policies.
    pub conflicts: Vec<(String, ConflictKind)>,
    pub promote_bytes: u64,
    /// The land would be refused for size (`PromotionPolicy::max_promote_bytes`).
    pub over_size_cap: bool,
    /// Already landed; the other fields are then empty.
    pub already_promoted: bool,
}

/// [`promote`] as a dry run: the same plan, reported, with no writes (not even the intent record).
pub fn preview<B: ObjectBackend>(
    b: &B,
    worktree_bucket: &str,
    project_bucket: &str,
    policy: &PromotionPolicy,
) -> Result<Preview> {
    policy.validate()?;
    if worktree_bucket == project_bucket {
        return Err(S2Error::Invalid("worktree and project bucket are the same".into()));
    }
    if is_promoted(b, worktree_bucket)?.is_some() {
        return Ok(Preview { already_promoted: true, ..Default::default() });
    }
    read_meta(b, worktree_bucket)?.ok_or_else(|| {
        S2Error::Invalid("worktree has no .wt/meta.json; refusing to promote".into())
    })?;
    let ov = Overlay::new(b, worktree_bucket, project_bucket);
    let Plan { acts, deletes, dropped, mut conflicts, total_bytes } = plan(b, &ov, worktree_bucket, project_bucket, policy)?;
    conflicts.sort_by(|x, y| x.0.cmp(&y.0));
    let mut pv = Preview {
        would_delete: deletes.into_iter().map(|d| d.0).collect(),
        would_drop_tombstones: dropped,
        conflicts,
        promote_bytes: total_bytes,
        over_size_cap: policy.check_size(total_bytes).is_err(),
        ..Default::default()
    };
    for a in acts {
        match a {
            Act::Copy(k, ..) => pv.would_promote.push(k),
            Act::Keep(k) => pv.would_keep_project.push(k),
            Act::Archive(k) => pv.would_archive.push(k),
        }
    }
    Ok(pv)
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
    // Presence is the check; `created_ms` identifies this life of the slug.
    let ov = Overlay::new(b, worktree_bucket, project_bucket);
    let Plan { acts, deletes, dropped, mut conflicts, total_bytes } = plan(b, &ov, worktree_bucket, project_bucket, policy)?;
    if !conflicts.is_empty() {
        conflicts.sort_by(|x, y| x.0.cmp(&y.0));
        return Err(S2Error::Conflict(conflicts.into_iter().map(|c| c.0).collect()));
    }
    policy.check_size(total_bytes)?;

    let intent = Intent {
        copy: acts
            .iter()
            .filter_map(|a| {
                if let Act::Copy(k, n, _) = a {
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
        generation_ms: meta.created_ms,
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
            Act::Copy(k, _, planned) => {
                displace(b, project_bucket, &k, archive)?;
                if policy.on_conflict == OnConflict::Overwrite {
                    b.copy(worktree_bucket, &k, project_bucket, &k)?;
                } else {
                    // Compare-and-swap against the version the plan was made on.
                    let data = b.get(worktree_bucket, &k)?.ok_or_else(|| {
                        S2Error::Backend(format!("worktree object vanished: {k}"))
                    })?;
                    if !b.put_cas(project_bucket, &k, &data, planned.as_deref())? {
                        return Err(S2Error::Conflict(vec![k]));
                    }
                }
                report.promoted.push(k);
            }
            Act::Keep(k) => report.kept_project.push(k),
            Act::Archive(k) => report.archived.push(k),
        }
    }
    for (k, planned) in deletes {
        if policy.on_conflict != OnConflict::Overwrite {
            still_unchanged(b, project_bucket, &k, Some(&planned))?;
        }
        displace(b, project_bucket, &k, archive)?;
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
