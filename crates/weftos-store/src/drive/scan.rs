//! Compare the tree with the index. Size and mtime first; a file is hashed only when one of them
//! moved, and a changed mtime with the same content is a touch, not a modification.

use std::collections::BTreeSet;
use std::path::Path;

use serde::Serialize;

use super::cas::hash_of;
use super::index::{Index, mtime_ns};
use super::{DRIVE_DIR, DriveError, Result, SkipWhy, Skipped, TMP_PREFIX, check_rel};
use crate::share::{Limits, Skip, read_regular};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Create,
    Modify,
    Delete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub path: String,
    pub kind: ChangeKind,
    pub size: u64,
    pub mtime_ns: u64,
    /// Set for `Modify` (it was hashed to tell it from a touch); `None` for `Create` and `Delete`.
    pub hash: Option<String>,
}

#[derive(Debug, Default)]
pub struct Scan {
    pub changes: Vec<Change>,
    pub skipped: Vec<Skipped>,
    /// `(path, size, mtime_ns)` of files whose stat moved but whose content did not.
    pub touched: Vec<(String, u64, u64)>,
}

/// Walk `dest` (never following a symlink, never entering `.drive/`) and list what differs from
/// `index`. Paths in an open conflict, and the remote copies written for conflicts, are left out.
pub fn scan(dest: &Path, index: &Index, limits: &Limits) -> Result<Scan> {
    let remote_copies: BTreeSet<&str> = index.conflicts.values().filter_map(|c| c.remote_copy.as_deref()).collect();
    let mut out = Scan::default();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut stack = vec![(dest.to_path_buf(), String::new())];
    while let Some((dir, dir_rel)) = stack.pop() {
        let mut names: Vec<_> = std::fs::read_dir(&dir)?.collect::<std::io::Result<Vec<_>>>()?;
        names.sort_by_key(|e| e.file_name());
        for e in names {
            let Some(name) = e.file_name().to_str().map(str::to_owned) else {
                out.skipped.push(Skipped::new(&format!("{dir_rel}/{}", e.file_name().to_string_lossy()), SkipWhy::BadPath));
                continue;
            };
            if (dir_rel.is_empty() && name == DRIVE_DIR) || name.starts_with(TMP_PREFIX) {
                continue;
            }
            let rel = if dir_rel.is_empty() { name } else { format!("{dir_rel}/{name}") };
            let md = match std::fs::symlink_metadata(e.path()) {
                Ok(m) => m,
                Err(_) => continue, // vanished since read_dir
            };
            let ft = md.file_type();
            if ft.is_symlink() {
                seen.insert(rel.clone());
                out.skipped.push(Skipped::new(&rel, SkipWhy::Symlink));
            } else if ft.is_dir() {
                stack.push((e.path(), rel));
            } else if ft.is_file() {
                seen.insert(rel.clone());
                if index.conflicts.contains_key(&rel) || remote_copies.contains(rel.as_str()) {
                    continue;
                }
                if let Err(why) = check_rel(&rel) {
                    out.skipped.push(Skipped::new(&rel, why));
                    continue;
                }
                if md.len() > limits.max_file {
                    out.skipped.push(Skipped::new(&rel, SkipWhy::TooLarge));
                    continue;
                }
                classify(dest, index, limits, &rel, &md, &mut out)?;
            } else {
                seen.insert(rel.clone());
                out.skipped.push(Skipped::new(&rel, SkipWhy::NotAFile));
            }
        }
    }
    for (rel, e) in &index.entries {
        if !seen.contains(rel) && !index.conflicts.contains_key(rel) {
            out.changes.push(Change { path: rel.clone(), kind: ChangeKind::Delete, size: e.size, mtime_ns: e.mtime_ns, hash: None });
        }
    }
    out.changes.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

fn classify(dest: &Path, index: &Index, limits: &Limits, rel: &str, md: &std::fs::Metadata, out: &mut Scan) -> Result<()> {
    let (size, mt) = (md.len(), mtime_ns(md));
    let Some(e) = index.entries.get(rel) else {
        out.changes.push(Change { path: rel.to_owned(), kind: ChangeKind::Create, size, mtime_ns: mt, hash: None });
        return Ok(());
    };
    if e.size == size && e.mtime_ns == mt {
        return Ok(());
    }
    match read_regular(&dest.join(rel), limits.max_file) {
        Ok(d) if hash_of(&d) == e.hash => out.touched.push((rel.to_owned(), d.len() as u64, mt)),
        Ok(d) => out.changes.push(Change {
            path: rel.to_owned(),
            kind: ChangeKind::Modify,
            size: d.len() as u64,
            mtime_ns: mt,
            hash: Some(hash_of(&d)),
        }),
        Err(Some(Skip::Missing)) => out.skipped.push(Skipped::new(rel, SkipWhy::Changed)),
        Err(Some(Skip::TooLarge)) => out.skipped.push(Skipped::new(rel, SkipWhy::TooLarge)),
        Err(Some(Skip::Symlink)) => out.skipped.push(Skipped::new(rel, SkipWhy::Symlink)),
        Err(Some(_)) => out.skipped.push(Skipped::new(rel, SkipWhy::Changed)),
        Err(None) => return Err(DriveError::Io(format!("cannot read {rel}"))),
    }
    Ok(())
}
