//! Append-only version index for first-class objects (ADR-121 O1).
//!
//! Every first-class object with a definition (agent packages, teams, cogs)
//! has versions: a content digest, a semantic label, its parent version and
//! its dependencies. This crate records them in one append-only JSON-lines
//! file per kind, `<dir>/<kind>.jsonl`, which is the record of truth, and
//! mirrors each record into a `clawft-cow-memory` lineage per object
//! (`mirror` module): one `promote` per recorded version. Derivations are
//! tracked by the `parent` field in the file, not by lineage `branch` (see the
//! `mirror` module for why).
//!
//! Queries ([`query`]) read the file only: the history of an object, its
//! version tree (children, including other objects derived from it), what it
//! depends on and what depends on it.
//!
//! Malformed input is refused with a [`VersionsError`], never turned into an
//! empty answer: a corrupt line, an unknown parent, an unsafe name, a digest
//! that is not `sha256:<64 lowercase hex>`, and a query for an object that
//! was never recorded are all errors.

mod import;
mod mirror;
mod query;

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub use import::ImportReport;
pub use query::{DependencyView, TreeNode};

/// A content digest: `sha256:` followed by 64 lowercase hex.
pub fn digest_ok(s: &str) -> bool {
    s.strip_prefix("sha256:")
        .is_some_and(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
}

/// Why an operation was refused.
#[derive(Debug, thiserror::Error)]
pub enum VersionsError {
    #[error("invalid {what}: {value:?}")]
    Invalid { what: &'static str, value: String },
    #[error("{kind}/{object}: digest {digest} is already recorded as version {existing}, not {version}")]
    Conflict { kind: String, object: String, digest: String, existing: String, version: String },
    #[error("{kind}/{object}: parent {parent} is not a recorded {kind} version")]
    UnknownParent { kind: String, object: String, parent: String },
    #[error("no versions recorded for {kind}/{object}")]
    NotFound { kind: String, object: String },
    #[error("{path}:{line}: corrupt version record: {reason}")]
    Corrupt { path: String, line: usize, reason: String },
    #[error("record appended but lineage mirror failed (rebuild with the same record): {0}")]
    Mirror(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// A dependency of a version. Packages are pinned by `digest`; skills, cogs
/// and model roles are named only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DepRef {
    /// `agent`, `skill`, `cog` or `model_role`.
    pub kind: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
}

/// One recorded version of one object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionRecord {
    pub object: String,
    pub version: String,
    pub digest: String,
    /// Digest of the version this one derives from, or `None` for a root.
    pub parent: Option<String>,
    #[serde(default)]
    pub dependencies: Vec<DepRef>,
    /// Unix seconds, supplied by the caller.
    pub recorded_at: u64,
}

/// What [`VersionsIndex::record`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recorded {
    /// A new line was appended and mirrored.
    Appended,
    /// The same object, version and digest was already recorded; nothing changed.
    AlreadyRecorded,
}

/// The version index under one directory (`<runtime>/versions`).
#[derive(Debug, Clone)]
pub struct VersionsIndex {
    dir: PathBuf,
}

fn kind_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !s.starts_with('-')
}

fn name_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && !s.starts_with('.')
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'~' | b'-'))
}

fn invalid<T>(what: &'static str, value: &str) -> Result<T, VersionsError> {
    Err(VersionsError::Invalid { what, value: value.to_owned() })
}

pub(crate) fn check_kind(kind: &str) -> Result<(), VersionsError> {
    if kind_ok(kind) { Ok(()) } else { invalid("kind", kind) }
}

pub(crate) fn check_object(object: &str) -> Result<(), VersionsError> {
    if name_ok(object) { Ok(()) } else { invalid("object name", object) }
}

fn check_record(rec: &VersionRecord) -> Result<(), VersionsError> {
    check_object(&rec.object)?;
    if rec.version.is_empty() || rec.version.len() > 64 || rec.version.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return invalid("version", &rec.version);
    }
    if !digest_ok(&rec.digest) {
        return invalid("digest", &rec.digest);
    }
    if let Some(p) = rec.parent.as_deref().filter(|p| !digest_ok(p)) {
        return invalid("parent digest", p);
    }
    for d in &rec.dependencies {
        if !kind_ok(&d.kind.replace('_', "-")) {
            return invalid("dependency kind", &d.kind);
        }
        check_object(&d.name)?;
        if let Some(dg) = d.digest.as_deref().filter(|dg| !digest_ok(dg)) {
            return invalid("dependency digest", dg);
        }
    }
    Ok(())
}

impl VersionsIndex {
    /// An index rooted at `dir`, which holds one `<kind>.jsonl` file per kind.
    /// The directory is not created or read until a record is listed or appended.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The directory holding the per-kind `.jsonl` files.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub(crate) fn file(&self, kind: &str) -> PathBuf {
        self.dir.join(format!("{kind}.jsonl"))
    }

    /// Every record of `kind`, in append order. A kind with no file has no
    /// records (`Ok(vec![])` means exactly that); a corrupt line is an error.
    pub fn list(&self, kind: &str) -> Result<Vec<VersionRecord>, VersionsError> {
        check_kind(kind)?;
        let path = self.file(kind);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        for (i, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let corrupt = |reason: String| VersionsError::Corrupt { path: path.display().to_string(), line: i + 1, reason };
            let rec: VersionRecord = serde_json::from_str(line).map_err(|e| corrupt(e.to_string()))?;
            check_record(&rec).map_err(|e| corrupt(e.to_string()))?;
            out.push(rec);
        }
        Ok(out)
    }

    /// The kinds that have a record file, sorted.
    pub fn kinds(&self) -> Result<Vec<String>, VersionsError> {
        let mut kinds = Vec::new();
        let rd = match std::fs::read_dir(&self.dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(kinds),
            Err(e) => return Err(e.into()),
        };
        for entry in rd {
            let name = entry?.file_name().to_string_lossy().into_owned();
            if let Some(k) = name.strip_suffix(".jsonl").filter(|k| kind_ok(k)) {
                kinds.push(k.to_owned());
            }
        }
        kinds.sort();
        Ok(kinds)
    }

    /// Record a version. Idempotent for an identical record; refuses a digest
    /// already recorded under a different version, and a parent that is not
    /// already recorded for this kind. Appends first (the file is the record
    /// of truth), then mirrors into the object's lineage.
    pub fn record(&self, kind: &str, rec: &VersionRecord) -> Result<Recorded, VersionsError> {
        check_kind(kind)?;
        check_record(rec)?;
        let existing = self.list(kind)?;
        if let Some(same) = existing.iter().find(|r| r.object == rec.object && r.digest == rec.digest) {
            if same.version == rec.version {
                return Ok(Recorded::AlreadyRecorded);
            }
            return Err(VersionsError::Conflict {
                kind: kind.to_owned(),
                object: rec.object.clone(),
                digest: rec.digest.clone(),
                existing: same.version.clone(),
                version: rec.version.clone(),
            });
        }
        if let Some(p) = rec.parent.as_ref().filter(|p| !existing.iter().any(|r| &r.digest == *p)) {
            return Err(VersionsError::UnknownParent { kind: kind.to_owned(), object: rec.object.clone(), parent: p.clone() });
        }
        std::fs::create_dir_all(&self.dir)?;
        let mut line = serde_json::to_string(rec).map_err(|e| VersionsError::Invalid { what: "record", value: e.to_string() })?;
        line.push('\n');
        let mut f = OpenOptions::new().create(true).append(true).open(self.file(kind))?;
        f.write_all(line.as_bytes())?;
        f.sync_data()?;
        self.mirror(kind, rec).map_err(|e| VersionsError::Mirror(e.to_string()))?;
        Ok(Recorded::Appended)
    }
}

#[cfg(test)]
mod tests;
