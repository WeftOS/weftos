//! Conflicts are records, never silent drops (ADR-131 section 3.6, ADR-128 section 7).
//!
//! The local file is never touched. The remote version, when it has bytes, is written beside it
//! as `<name>.remote-<8 hex of its hash>`. The conflict is appended to `conflicts.jsonl` and held
//! open in the index: sync neither uploads nor overwrites that path until [`resolve`] is called.

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::cas::{hash_of, prune_empty_parents, write_atomic};
use super::index::{Entry, Index, OpenConflict, mtime_ns};
use super::{DriveError, Result, append_jsonl, log_event, now_ms};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictKind {
    /// Both sides changed the file since the last sync.
    BothModified,
    /// Both sides created the path, with different content.
    BothCreated,
    /// The remote changed it; locally it was deleted.
    RemoteModifiedLocalDeleted,
    /// The remote deleted it; locally it was modified.
    RemoteDeletedLocalModified,
}

/// Which side [`resolve`] keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keep {
    /// Keep the local state; the next `sync_up` makes the remote match it.
    Local,
    /// Take the remote state: the remote copy replaces the local file, or the file is removed if
    /// the remote was deleted.
    Remote,
}

/// Make a placed file executable (0755) or leave it as written. Only the execute attribute is
/// carried; the other bits are the writer's default.
fn set_exec(target: &Path, exec: bool) -> Result<()> {
    if exec {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(target, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

/// Record a conflict on `path`: write the remote copy (if any), log it, hold it open. The caller
/// has checked that `path` has no open conflict yet and that `path` passed `check_rel`.
pub(crate) fn record(
    dest: &Path,
    index: &mut Index,
    path: &str,
    kind: ConflictKind,
    remote: Option<(&str, &[u8], bool)>,
    local_hash: Option<&str>,
    detail: &str,
) -> Result<()> {
    let mut copy_rel = None;
    let mut remote_hash = None;
    if let Some((_, bytes, exec)) = remote {
        let h = hash_of(bytes);
        let rel = format!("{path}.remote-{}", &h[..8]);
        let target = crate::share::safe_join(dest, &rel).map_err(|e| DriveError::Invalid(e.to_string()))?;
        write_atomic(&target, bytes)?;
        set_exec(&target, exec)?;
        remote_hash = Some(h);
        copy_rel = Some(rel);
    }
    let at_ms = now_ms();
    append_jsonl(
        dest,
        "conflicts.jsonl",
        &serde_json::json!({
            "ts_ms": at_ms, "device": index.device, "path": path, "kind": kind,
            "local_hash": local_hash, "remote_etag": remote.map(|r| r.0), "remote_hash": remote_hash, "remote_exec": remote.is_some_and(|r| r.2),
            "remote_copy": copy_rel, "detail": detail,
        }),
    )?;
    log_event(dest, "conflict", path);
    index.conflicts.insert(
        path.to_owned(),
        OpenConflict { kind, remote_copy: copy_rel, remote_etag: remote.map(|r| r.0.to_owned()), at_ms, remote_exec: remote.is_some_and(|r| r.2) },
    );
    Ok(())
}

/// Close an open conflict by choosing a side. Needs no backend: the remote copy already holds the
/// remote bytes. After `Keep::Local` run `sync_up`; if the remote moved again meanwhile that is a
/// new conflict, not an overwrite.
pub fn resolve(dest: &Path, path: &str, keep: Keep) -> Result<()> {
    let mut index = Index::load(dest)?.ok_or_else(|| DriveError::Invalid(format!("{} is not a drive", dest.display())))?;
    let c = index.conflicts.get(path).cloned().ok_or_else(|| DriveError::Invalid(format!("no open conflict on {path:?}")))?;
    let target = crate::share::safe_join(dest, path).map_err(|e| DriveError::Invalid(e.to_string()))?;
    match (keep, c.remote_etag.as_deref()) {
        (Keep::Local, Some(etag)) => {
            // Pretend the local side last synced against the remote version it is about to replace.
            index
                .entries
                .entry(path.to_owned())
                .and_modify(|e| e.etag = etag.to_owned())
                .or_insert_with(|| Entry { size: 0, mtime_ns: 0, hash: String::new(), etag: etag.to_owned(), exec: false, stamp: String::new() });
        }
        (Keep::Local, None) => {
            // The remote object is gone: the local file is a new file again.
            index.entries.remove(path);
        }
        (Keep::Remote, Some(etag)) => {
            let copy = c.remote_copy.as_deref().ok_or_else(|| DriveError::Invalid("conflict has no remote copy".into()))?;
            let bytes = std::fs::read(dest.join(copy))?;
            write_atomic(&target, &bytes)?;
            set_exec(&target, c.remote_exec)?;
            let md = std::fs::metadata(&target)?;
            index.entries.insert(
                path.to_owned(),
                Entry { size: md.len(), mtime_ns: mtime_ns(&md), hash: hash_of(&bytes), etag: etag.to_owned(), exec: c.remote_exec, stamp: String::new() },
            );
        }
        (Keep::Remote, None) => {
            match std::fs::remove_file(&target) {
                Ok(()) => prune_empty_parents(dest, path),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            index.entries.remove(path);
        }
    }
    if let Some(copy) = &c.remote_copy {
        let _ = std::fs::remove_file(dest.join(copy));
        prune_empty_parents(dest, copy);
    }
    index.conflicts.remove(path);
    index.save(dest)?;
    log_event(dest, if keep == Keep::Local { "resolve-local" } else { "resolve-remote" }, path);
    Ok(())
}
