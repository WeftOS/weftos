//! ADR-126 §4 worktree overlay: reads fall through worktree bucket -> project bucket, writes stay
//! in the worktree bucket, deletes of project objects are recorded as tombstones.
//!
//! This module is backend-agnostic. S1's OpenDAL client implements [`ObjectBackend`]; tests use
//! [`crate::testing::MemBackend`]. Nothing here ever writes to the project bucket: promotion
//! (`promote.rs`) is the only path that does.
//!
//! Reserved keys: everything under [`RESERVED`] (`.wt/`) in the worktree bucket is overlay and
//! sharing state, never visible through [`Overlay::get`] / [`Overlay::list`] and never promoted.

use std::collections::BTreeMap;
use std::fmt;

/// Reserved prefix in a worktree bucket for overlay, promotion and sharing state.
pub const RESERVED: &str = ".wt/";
const TOMBSTONES: &str = ".wt/tombstones/";
const BASES: &str = ".wt/base/";
/// Recorded base when the project had no such key at the first write.
pub(crate) const BASE_ABSENT: &str = "-";

/// The project etag this worktree first wrote or deleted `key` against, if recorded. Promotion
/// compares it with the project's current etag: a difference means the project moved on. No
/// clocks are involved, so client or server time skew cannot hide or invent a conflict.
pub(crate) fn read_base<B: ObjectBackend + ?Sized>(
    b: &B,
    worktree: &str,
    key: &str,
) -> Result<Option<String>> {
    Ok(b.get(worktree, &format!("{BASES}{key}"))?
        .map(|d| String::from_utf8_lossy(&d).into_owned()))
}

/// Errors from the S2 worktree-store paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum S2Error {
    Backend(String),
    BadKey(String),
    Reserved(String),
    /// Promotion refused: project objects changed after the worktree was created.
    Conflict(Vec<String>),
    Io(String),
    Json(String),
    /// Another merger holds a live claim.
    Claimed(String),
    Invalid(String),
}

impl fmt::Display for S2Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            S2Error::Backend(m) => write!(f, "backend: {m}"),
            S2Error::BadKey(k) => write!(f, "bad object key: {k:?}"),
            S2Error::Reserved(k) => write!(f, "key is in the reserved {RESERVED} area: {k:?}"),
            S2Error::Conflict(k) => write!(
                f,
                "promotion conflict on {} object(s): {}",
                k.len(),
                k.join(", ")
            ),
            S2Error::Io(m) => write!(f, "io: {m}"),
            S2Error::Json(m) => write!(f, "json: {m}"),
            S2Error::Claimed(by) => write!(f, "worktree is claimed by {by}"),
            S2Error::Invalid(m) => write!(f, "invalid: {m}"),
        }
    }
}
impl std::error::Error for S2Error {}
impl From<std::io::Error> for S2Error {
    fn from(e: std::io::Error) -> Self {
        S2Error::Io(e.to_string())
    }
}
impl From<serde_json::Error> for S2Error {
    fn from(e: serde_json::Error) -> Self {
        S2Error::Json(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, S2Error>;

/// Metadata for one object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectMeta {
    pub key: String,
    pub size: u64,
    pub etag: String,
    pub modified_ms: u64,
}

/// The minimum S3-shaped surface S2 needs. Synchronous on purpose: the daemon adapts its async
/// client with `block_on`/a worker, and this crate section adds no runtime dependency.
pub trait ObjectBackend {
    fn head(&self, bucket: &str, key: &str) -> Result<Option<ObjectMeta>>;
    fn get(&self, bucket: &str, key: &str) -> Result<Option<Vec<u8>>>;
    fn put(&self, bucket: &str, key: &str, data: &[u8]) -> Result<ObjectMeta>;
    fn delete(&self, bucket: &str, key: &str) -> Result<()>;
    /// Every object whose key starts with `prefix`, any order.
    fn list(&self, bucket: &str, prefix: &str) -> Result<Vec<ObjectMeta>>;
    /// Server-side copy where the backend has one; the default streams through memory.
    fn copy(
        &self,
        src_bucket: &str,
        src_key: &str,
        dst_bucket: &str,
        dst_key: &str,
    ) -> Result<ObjectMeta> {
        let data = self.get(src_bucket, src_key)?.ok_or_else(|| {
            S2Error::Backend(format!("copy source missing: {src_bucket}/{src_key}"))
        })?;
        self.put(dst_bucket, dst_key, &data)
    }
}

/// Reject keys that could escape a prefix or a local directory.
pub fn validate_key(key: &str) -> Result<()> {
    let bad = key.is_empty()
        || key.starts_with('/')
        || key.contains('\0')
        || key.contains('\\')
        || key
            .split('/')
            .any(|s| s == ".." || s == "." || s.is_empty());
    if bad {
        return Err(S2Error::BadKey(key.to_string()));
    }
    Ok(())
}

/// `true` for keys the overlay must treat as internal.
pub fn is_reserved(key: &str) -> bool {
    key.starts_with(RESERVED)
}

/// The two-layer view of one worktree: its own bucket over the (read-only) project bucket.
pub struct Overlay<'a, B: ObjectBackend> {
    backend: &'a B,
    worktree: String,
    project: String,
}

impl<'a, B: ObjectBackend> Overlay<'a, B> {
    pub fn new(
        backend: &'a B,
        worktree_bucket: impl Into<String>,
        project_bucket: impl Into<String>,
    ) -> Self {
        Self {
            backend,
            worktree: worktree_bucket.into(),
            project: project_bucket.into(),
        }
    }

    pub fn worktree_bucket(&self) -> &str {
        &self.worktree
    }
    pub fn project_bucket(&self) -> &str {
        &self.project
    }

    fn tombstone_key(key: &str) -> String {
        format!("{TOMBSTONES}{key}")
    }

    fn user_key(key: &str) -> Result<()> {
        validate_key(key)?;
        if is_reserved(key) {
            return Err(S2Error::Reserved(key.to_string()));
        }
        Ok(())
    }

    /// Which layer holds the visible version of `key`, if any.
    pub fn head(&self, key: &str) -> Result<Option<ObjectMeta>> {
        Self::user_key(key)?;
        if let Some(m) = self.backend.head(&self.worktree, key)? {
            return Ok(Some(m));
        }
        if self
            .backend
            .head(&self.worktree, &Self::tombstone_key(key))?
            .is_some()
        {
            return Ok(None);
        }
        self.backend.head(&self.project, key)
    }

    /// Worktree first, then the project; a tombstone hides the project object.
    pub fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        Self::user_key(key)?;
        if let Some(d) = self.backend.get(&self.worktree, key)? {
            return Ok(Some(d));
        }
        if self
            .backend
            .head(&self.worktree, &Self::tombstone_key(key))?
            .is_some()
        {
            return Ok(None);
        }
        // Reading a project object pins the version this worktree saw, so a later edit based on
        // it is compared against the right base. Best effort: a failed note only means promotion
        // falls back to its fail-safe (no base = conflict).
        if self.backend.head(&self.project, key)?.is_some() {
            let _ = self.note_base(key);
        }
        self.backend.get(&self.project, key)
    }

    /// Remember which project version a first write or delete is based on. One tiny object per
    /// key, written once; skipped when a base or a local copy already exists (the base was
    /// recorded then, or the object predates base tracking and promotion treats it conservatively).
    fn note_base(&self, key: &str) -> Result<()> {
        let bk = format!("{BASES}{key}");
        if self.backend.head(&self.worktree, &bk)?.is_some()
            || self.backend.head(&self.worktree, key)?.is_some()
        {
            return Ok(());
        }
        let base = match self.backend.head(&self.project, key)? {
            Some(p) => p.etag,
            None => BASE_ABSENT.to_string(),
        };
        self.backend.put(&self.worktree, &bk, base.as_bytes())?;
        Ok(())
    }

    /// Writes go to the worktree bucket only; a write revives a tombstoned key.
    pub fn put(&self, key: &str, data: &[u8]) -> Result<ObjectMeta> {
        Self::user_key(key)?;
        self.note_base(key)?;
        let meta = self.backend.put(&self.worktree, key, data)?;
        self.backend
            .delete(&self.worktree, &Self::tombstone_key(key))?;
        Ok(meta)
    }

    /// Remove the worktree copy; if the project still has the key, leave a tombstone so reads
    /// stop falling through. The project bucket is never touched.
    pub fn delete(&self, key: &str) -> Result<()> {
        Self::user_key(key)?;
        self.note_base(key)?;
        self.backend.delete(&self.worktree, key)?;
        if self.backend.head(&self.project, key)?.is_some() {
            self.backend
                .put(&self.worktree, &Self::tombstone_key(key), b"")?;
        }
        Ok(())
    }

    /// Merged listing: worktree objects shadow project ones, tombstoned keys vanish.
    pub fn list(&self, prefix: &str) -> Result<Vec<ObjectMeta>> {
        let mut out: BTreeMap<String, ObjectMeta> = BTreeMap::new();
        for m in self.backend.list(&self.project, prefix)? {
            if !is_reserved(&m.key) {
                out.insert(m.key.clone(), m);
            }
        }
        for t in self.backend.list(&self.worktree, TOMBSTONES)? {
            if let Some(k) = t.key.strip_prefix(TOMBSTONES) {
                out.remove(k);
            }
        }
        for m in self.backend.list(&self.worktree, prefix)? {
            if !is_reserved(&m.key) {
                out.insert(m.key.clone(), m);
            }
        }
        Ok(out.into_values().collect())
    }

    /// Keys written in this worktree (the set promotion considers), excluding reserved state.
    pub fn local_objects(&self) -> Result<Vec<ObjectMeta>> {
        let mut v: Vec<_> = self
            .backend
            .list(&self.worktree, "")?
            .into_iter()
            .filter(|m| !is_reserved(&m.key))
            .collect();
        v.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(v)
    }

    /// Keys deleted in this worktree that exist in the project.
    pub fn tombstones(&self) -> Result<Vec<String>> {
        let mut v: Vec<_> = self
            .backend
            .list(&self.worktree, TOMBSTONES)?
            .into_iter()
            .filter_map(|m| m.key.strip_prefix(TOMBSTONES).map(str::to_string))
            .collect();
        v.sort();
        Ok(v)
    }
}
