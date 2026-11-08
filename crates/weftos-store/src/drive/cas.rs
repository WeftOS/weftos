//! The local content-addressed cache: `<dest>/.drive/cas/<hh>/<blake3 hex>`.
//!
//! A blob enters once, keyed by the BLAKE3 of its bytes, and is never rewritten. A working-tree
//! file is a clone of it (APFS `clonefile` through `std::fs::copy` where the standard library
//! uses it, a plain copy elsewhere), so editing the file never touches the cache.

use std::path::{Path, PathBuf};

use super::tmp::Live;
use super::{DRIVE_DIR, DriveError, Result, TMP_PREFIX};

pub(crate) struct Cas {
    root: PathBuf,
}

/// A BLAKE3 hex digest and nothing else, so a hash read from `index.json` can never be a path.
pub(crate) fn valid_hash(h: &str) -> bool {
    h.len() == 64 && h.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub(crate) fn hash_of(data: &[u8]) -> String {
    blake3::hash(data).to_hex().to_string()
}

impl Cas {
    pub(crate) fn new(dest: &Path) -> Self {
        Self { root: dest.join(DRIVE_DIR).join("cas") }
    }

    fn path(&self, hash: &str) -> Result<PathBuf> {
        if !valid_hash(hash) {
            return Err(DriveError::Invalid(format!("not a blake3 hash: {hash:?}")));
        }
        Ok(self.root.join(&hash[..2]).join(hash))
    }

    pub(crate) fn has(&self, hash: &str) -> bool {
        self.path(hash).is_ok_and(|p| p.is_file())
    }

    pub(crate) fn put(&self, hash: &str, data: &[u8]) -> Result<()> {
        let p = self.path(hash)?;
        if p.is_file() {
            return Ok(());
        }
        let dir = p.parent().ok_or_else(|| DriveError::Io("cas path has no parent".into()))?;
        std::fs::create_dir_all(dir)?;
        let tmp = dir.join(format!("{TMP_PREFIX}{}", ulid::Ulid::new()));
        let _live = Live::new(&tmp);
        std::fs::write(&tmp, data)?;
        std::fs::rename(&tmp, &p).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })?;
        Ok(())
    }

    pub(crate) fn read(&self, hash: &str) -> Result<Vec<u8>> {
        Ok(std::fs::read(self.path(hash)?)?)
    }

    /// Create `target` as a copy-on-write clone (or copy) of the blob. `target` must not exist.
    pub(crate) fn clone_to(&self, hash: &str, target: &Path) -> Result<()> {
        std::fs::copy(self.path(hash)?, target)?;
        Ok(())
    }
}

/// Write `data` to `path` through a temp file in the same directory, creating parents.
pub(crate) fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let dir = path.parent().ok_or_else(|| DriveError::Io(format!("{} has no parent", path.display())))?;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!("{TMP_PREFIX}{}", ulid::Ulid::new()));
    let _live = Live::new(&tmp);
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })?;
    Ok(())
}

/// Remove the empty directories above `rel` up to (not including) `dest`.
pub(crate) fn prune_empty_parents(dest: &Path, rel: &str) {
    let mut cur = dest.join(rel);
    while cur.pop() && cur != dest && cur.starts_with(dest) {
        if std::fs::remove_dir(&cur).is_err() {
            break;
        }
    }
}
