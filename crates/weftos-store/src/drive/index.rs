//! `<dest>/.drive/index.json`: what the tree looked like at the last sync.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

use super::cas::{hash_of, valid_hash};
use super::conflict::ConflictKind;
use super::tmp::Live;
use super::{DRIVE_DIR, DriveError, Result, TMP_PREFIX};
use crate::share::{Skip, read_regular};

const SCHEMA: u32 = 1;

/// Where `Index::save` can be made to die in a test.
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SaveStage {
    AfterTemp,
    AfterRename,
}
#[cfg(test)]
thread_local! {
    pub(crate) static DIE_AT: std::cell::Cell<Option<SaveStage>> = const { std::cell::Cell::new(None) };
}
#[cfg(test)]
fn checkpoint(s: SaveStage) {
    if DIE_AT.with(|d| d.get()) == Some(s) {
        panic!("simulated death in Index::save {s:?}");
    }
}
#[cfg(not(test))]
#[derive(Clone, Copy)]
enum SaveStage {
    AfterTemp,
    AfterRename,
}
#[cfg(not(test))]
fn checkpoint(_: SaveStage) {}

/// One synced path. `etag` is the store's etag of the version this file was synced against;
/// `hash` is the BLAKE3 of the local bytes (the CAS key).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub size: u64,
    pub mtime_ns: u64,
    pub hash: String,
    pub etag: String,
    /// The file is executable (any execute bit), mirroring the object's
    /// [`ObjectAttrs::executable`](crate::backend::ObjectAttrs). Absent in an index written
    /// before the attribute existed, which reads as `false`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub exec: bool,
}

/// Whether a file's mode has any execute bit.
pub(crate) fn is_exec(md: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    md.permissions().mode() & 0o111 != 0
}

/// A path whose local and remote versions disagree and nobody has chosen yet. While it is open the
/// path is neither uploaded nor overwritten.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenConflict {
    pub kind: ConflictKind,
    /// Relative path of the remote copy written beside the local file, if the remote had bytes.
    pub remote_copy: Option<String>,
    /// The remote etag at conflict time; `None` means the remote object was deleted.
    pub remote_etag: Option<String>,
    pub at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Index {
    pub schema: u32,
    pub bucket: String,
    /// Normalised: empty, or ending in `/`.
    pub prefix: String,
    pub device: String,
    pub entries: BTreeMap<String, Entry>,
    pub conflicts: BTreeMap<String, OpenConflict>,
}

impl Index {
    pub(crate) fn new(bucket: &str, prefix: &str, device: Option<&str>) -> Self {
        Self {
            schema: SCHEMA,
            bucket: bucket.to_owned(),
            prefix: prefix.to_owned(),
            device: device.map_or_else(|| format!("drive-{}", &ulid::Ulid::new().to_string().to_lowercase()[16..]), str::to_owned),
            entries: BTreeMap::new(),
            conflicts: BTreeMap::new(),
        }
    }

    fn file(dest: &Path) -> std::path::PathBuf {
        dest.join(DRIVE_DIR).join("index.json")
    }

    /// `Ok(None)` when `dest` has no index (not a drive yet).
    pub fn load(dest: &Path) -> Result<Option<Self>> {
        let b = match std::fs::read(Self::file(dest)) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let ix: Self = serde_json::from_slice(&b).map_err(|e| DriveError::Json(format!("{}: {e}", Self::file(dest).display())))?;
        if ix.schema != SCHEMA {
            return Err(DriveError::Invalid(format!("index schema {} is not {SCHEMA}", ix.schema)));
        }
        if let Some(bad) = ix.entries.values().find(|e| !e.hash.is_empty() && !valid_hash(&e.hash)) {
            return Err(DriveError::Invalid(format!("index holds a bad hash {:?}", bad.hash)));
        }
        Ok(Some(ix))
    }

    pub fn save(&self, dest: &Path) -> Result<()> {
        let f = Self::file(dest);
        let dir = f.parent().ok_or_else(|| DriveError::Io("index path has no parent".into()))?;
        std::fs::create_dir_all(dir)?;
        let tmp = dir.join(format!("{TMP_PREFIX}index"));
        let _live = Live::new(&tmp);
        // Durable order: temp bytes on disk, rename, then the directory entry on disk.
        let mut t = std::fs::File::create(&tmp)?;
        t.write_all(&serde_json::to_vec_pretty(self)?)?;
        t.sync_all()?;
        drop(t);
        checkpoint(SaveStage::AfterTemp);
        std::fs::rename(&tmp, &f)?;
        checkpoint(SaveStage::AfterRename);
        std::fs::File::open(dir)?.sync_all()?;
        Ok(())
    }

    pub(crate) fn key(&self, rel: &str) -> String {
        format!("{}{rel}", self.prefix)
    }

    /// Record a file's new size/mtime without changing its content identity (a touch).
    pub(crate) fn touch(&mut self, rel: &str, size: u64, mtime_ns: u64) {
        if let Some(e) = self.entries.get_mut(rel) {
            e.size = size;
            e.mtime_ns = mtime_ns;
        }
    }
}

pub(crate) fn mtime_ns(md: &std::fs::Metadata) -> u64 {
    md.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos() as u64)
}

/// What is on disk at a path, relative to what the index says.
#[derive(Debug)]
pub(crate) enum Local {
    Missing,
    /// A directory, symlink or anything that is not a regular file, or one over the size ceiling.
    NotFile,
    /// Same content as the index (the stat may have moved without the bytes).
    Unchanged,
    Modified { hash: String },
    /// A file with no index entry.
    Untracked { hash: String },
}

pub(crate) fn local_state(dest: &Path, rel: &str, entry: Option<&Entry>, max: u64) -> Result<Local> {
    let path = dest.join(rel);
    let md = match std::fs::symlink_metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Local::Missing),
        // `a/b` where `a` is a file: nothing lives at `a/b`.
        Err(e) if e.raw_os_error() == Some(20) => return Ok(Local::Missing),
        Err(e) => return Err(e.into()),
    };
    if !md.is_file() {
        return Ok(Local::NotFile);
    }
    let (size, mt) = (md.len(), mtime_ns(&md));
    if let Some(e) = entry
        && e.size == size
        && e.mtime_ns == mt
    {
        return Ok(Local::Unchanged);
    }
    let data = match read_regular(&path, max) {
        Ok(d) => d,
        Err(Some(Skip::Missing)) => return Ok(Local::Missing),
        Err(Some(_)) => return Ok(Local::NotFile),
        Err(None) => return Err(DriveError::Io(format!("cannot read {}", path.display()))),
    };
    let hash = hash_of(&data);
    Ok(match entry {
        Some(e) if e.hash == hash => Local::Unchanged,
        Some(_) => Local::Modified { hash },
        None => Local::Untracked { hash },
    })
}
