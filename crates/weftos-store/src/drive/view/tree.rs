//! Local tree work for a view: the root check, mode changes, mirroring a drive into a clean tree,
//! exact-path removal, crash recovery and the manifest.

use std::collections::BTreeSet;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::Serialize;

use super::{Bind, Layout, Lease, RefreshEndpoint, Result, VIEW_SCHEMA, ViewError, ViewManifest, ViewSpec, WorkStart};
use super::{REFRESH_RPC, WORK_MOUNT, is_control_key, lease};
use crate::backend::ObjectBackend;
use crate::drive::cas::Cas;
use crate::drive::index::{Index, mtime_ns};
use crate::drive::{DriveError, SkipWhy, Skipped, TMP_PREFIX, norm_prefix, now_ms, scan};
use crate::share::{Limits, safe_join};

const RO_DIR: u32 = 0o555;
const RO_FILE: u32 = 0o444;
const RW_DIR: u32 = 0o755;
const RW_FILE: u32 = 0o644;

fn root_err(root: &Path, why: &str) -> ViewError {
    ViewError::Root(root.display().to_string(), why.to_owned())
}

/// The root must be absolute, already resolved (no `.`, `..` or symlinked component: the parent
/// canonicalises to itself), and not itself a symlink or a file. The daemon's `Guard` does the
/// policy half (outside its own state); this is the library's floor.
pub(crate) fn check_root(root: &Path) -> Result<()> {
    use std::path::Component;
    if !root.is_absolute() {
        return Err(root_err(root, "not absolute"));
    }
    if root.components().any(|c| matches!(c, Component::CurDir | Component::ParentDir)) {
        return Err(root_err(root, "has . or .."));
    }
    let parent = root.parent().ok_or_else(|| root_err(root, "has no parent"))?;
    let canon = std::fs::canonicalize(parent).map_err(|e| root_err(root, &format!("parent: {e}")))?;
    if canon != parent {
        return Err(root_err(root, &format!("parent resolves to {}: pass the resolved path", canon.display())));
    }
    match std::fs::symlink_metadata(root) {
        Ok(m) if m.file_type().is_symlink() => Err(root_err(root, "is a symlink")),
        Ok(m) if !m.is_dir() => Err(root_err(root, "is not a directory")),
        _ => Ok(()),
    }
}

pub(crate) fn create_private_dir(p: &Path) -> Result<()> {
    std::fs::create_dir_all(p)?;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

/// Write JSON through a temp file and rename.
pub(crate) fn write_json(path: &Path, v: &impl Serialize) -> Result<()> {
    crate::drive::cas::write_atomic(path, &serde_json::to_vec_pretty(v)?)?;
    Ok(())
}

fn set_mode(p: &Path, mode: u32) -> std::io::Result<()> {
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode))
}

/// Walk `dir` without following symlinks and set every directory and regular file to the
/// read-only (0555/0444) or the writable (0755/0644) modes. Directories are made writable before
/// they are entered, and read-only after.
pub(crate) fn set_tree_mode(dir: &Path, read_only: bool) -> Result<()> {
    let (dm, fm) = if read_only { (RO_DIR, RO_FILE) } else { (RW_DIR, RW_FILE) };
    let md = match std::fs::symlink_metadata(dir) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    if !md.is_dir() {
        return Ok(());
    }
    if !read_only {
        set_mode(dir, dm)?;
    }
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let p = e.path();
        let m = std::fs::symlink_metadata(&p)?;
        if m.is_dir() {
            set_tree_mode(&p, read_only)?;
        } else if m.is_file() {
            set_mode(&p, fm)?;
        }
    }
    if read_only {
        set_mode(dir, dm)?;
    }
    Ok(())
}

/// Remove one path the view owns, exactly: a directory after making it writable (never following
/// a symlink), anything else as a single entry. `Ok(false)` when it was not there.
pub(crate) fn remove_exact(p: &Path) -> Result<bool> {
    let md = match std::fs::symlink_metadata(p) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    if md.is_dir() {
        set_tree_mode(p, false)?;
        std::fs::remove_dir_all(p)?;
    } else {
        std::fs::remove_file(p)?;
    }
    Ok(true)
}

/// Remove the view's own paths: the manifest first (so a crash part way leaves a recoverable
/// marker-without-manifest), the state directory (which holds the marker) last.
pub(crate) fn remove_view(lay: &Layout) -> Result<Vec<PathBuf>> {
    let mut removed = Vec::new();
    for p in [lay.manifest(), lay.base(), lay.lower(), lay.work(), lay.state()] {
        if remove_exact(&p)? {
            removed.push(p);
        }
    }
    Ok(removed)
}

/// A crashed open: release the lease it recorded (if still ours) and remove its paths. Best
/// effort; the caller checks the root is empty afterwards.
pub(crate) fn recover<B: ObjectBackend + ?Sized>(b: &B, lay: &Layout) {
    if let Some(l) = std::fs::read(lay.lease()).ok().and_then(|d| serde_json::from_slice::<Lease>(&d).ok()) {
        let _ = lease::release(b, &l);
    }
    let _ = remove_view(lay);
}

fn same_file(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    a.dev() == b.dev() && a.ino() == b.ino()
}

/// Place `dst` as a hard link to `src` (a copy if a link is refused), through a temp name.
fn link_into(src: &Path, dst: &Path) -> Result<()> {
    let sm = std::fs::symlink_metadata(src)?;
    if !sm.is_file() {
        return Err(DriveError::Invalid(format!("{} is not a regular file", src.display())).into());
    }
    match std::fs::symlink_metadata(dst) {
        Ok(dm) if dm.is_file() && same_file(&sm, &dm) => return Ok(()),
        Ok(dm) if dm.is_dir() => return Err(DriveError::Invalid("a directory is in the way".into()).into()),
        _ => {}
    }
    let dir = dst.parent().ok_or_else(|| DriveError::Io(format!("{}: no parent", dst.display())))?;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!("{TMP_PREFIX}{}", ulid::Ulid::new()));
    if std::fs::hard_link(src, &tmp).is_err() {
        std::fs::copy(src, &tmp)?;
    }
    std::fs::rename(&tmp, dst).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })?;
    Ok(())
}

fn ancestors(rel: &str, into: &mut BTreeSet<String>) {
    let mut cur = rel;
    while let Some((parent, _)) = cur.rsplit_once('/') {
        if !into.insert(parent.to_owned()) {
            break;
        }
        cur = parent;
    }
}

/// Remove everything under `dir` that is not a kept file or an ancestor of one. Never follows a
/// symlink (a symlink is removed as an entry). `protect`: top-level names left alone (mount points).
fn prune(dir: &Path, rel: &str, keep: &BTreeSet<String>, dirs: &BTreeSet<String>, protect: &[String]) -> Result<()> {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    for e in rd {
        let e = e?;
        let name = e.file_name().to_string_lossy().into_owned();
        if rel.is_empty() && protect.contains(&name) {
            continue;
        }
        let r = if rel.is_empty() { name } else { format!("{rel}/{name}") };
        let md = std::fs::symlink_metadata(e.path())?;
        if md.is_dir() {
            if dirs.contains(&r) {
                prune(&e.path(), &r, keep, dirs, protect)?;
            } else {
                remove_exact(&e.path())?;
            }
        } else if !(md.is_file() && keep.contains(&r)) {
            std::fs::remove_file(e.path())?;
        }
    }
    Ok(())
}

/// Make `<dest_root>/<at>/` hold exactly the drive's indexed files, as hard links to the drive's
/// files, minus control state, denied paths and paths on a mount point. A path in open conflict keeps whatever
/// is there. Every refusal goes to `skipped`. `dest_root` must be writable. Returns (files, bytes).
pub(crate) fn mirror(
    drive: &Path,
    dest_root: &Path,
    at: &str,
    prefix: &str,
    spec: &ViewSpec,
    mounts: &[String],
    skipped: &mut Vec<Skipped>,
) -> Result<(usize, u64)> {
    let prefix = norm_prefix(prefix);
    let ix = Index::load(drive)?.ok_or_else(|| ViewError::NotAView(drive.display().to_string()))?;
    std::fs::create_dir_all(dest_root)?;
    let target = if at.is_empty() {
        dest_root.to_path_buf()
    } else {
        let t = safe_join(dest_root, at).map_err(|e| ViewError::Spec(e.to_string()))?;
        std::fs::create_dir_all(&t)?;
        t
    };
    let protect: &[String] = if at.is_empty() { mounts } else { &[] };
    let mut place = Vec::new();
    let mut keep = BTreeSet::new();
    for (rel, e) in &ix.entries {
        if is_control_key(&spec.bucket, &format!("{prefix}{rel}")) {
            skipped.push(Skipped { path: rel.clone(), why: SkipWhy::Reserved, detail: "store control state".into() });
        } else if ix.conflicts.contains_key(rel) {
            keep.insert(rel.clone());
        } else if spec.denied(rel) {
            skipped.push(Skipped::new(rel, SkipWhy::Denied));
        } else if protect.iter().any(|m| rel == m || rel.starts_with(&format!("{m}/"))) {
            skipped.push(Skipped { path: rel.clone(), why: SkipWhy::Reserved, detail: "under a mount point".into() });
        } else {
            keep.insert(rel.clone());
            place.push((rel, e.size));
        }
    }
    let mut dirs = BTreeSet::new();
    for k in &keep {
        ancestors(k, &mut dirs);
    }
    // Prune first: a symlink planted where a directory belongs goes before anything is linked.
    prune(&target, "", &keep, &dirs, protect)?;
    let (mut n, mut bytes) = (0, 0);
    for (rel, size) in place {
        let dst = match safe_join(&target, rel) {
            Ok(p) => p,
            Err(_) => {
                skipped.push(Skipped::new(rel, SkipWhy::BadPath));
                continue;
            }
        };
        match link_into(&drive.join(rel), &dst) {
            Ok(()) => {
                n += 1;
                bytes += size;
            }
            Err(e) => skipped.push(Skipped::io(rel, e)),
        }
    }
    Ok((n, bytes))
}

/// Put a drive tree back to its index from the CAS: a file changed under the view (a base file
/// written through its hard link on a host with no bwrap) is replaced by a fresh copy, an extra
/// file is removed. Returns the repaired paths.
pub(crate) fn repair(drive: &Path, limits: &Limits) -> Result<Vec<String>> {
    let Some(mut ix) = Index::load(drive)? else { return Ok(Vec::new()) };
    let s = scan(drive, &ix, limits)?;
    let cas = Cas::new(drive);
    let mut fixed = Vec::new();
    for (p, size, mt) in &s.touched {
        ix.touch(p, *size, *mt);
    }
    for ch in s.changes {
        let path = safe_join(drive, &ch.path).map_err(|e| ViewError::Spec(e.to_string()))?;
        match ix.entries.get(&ch.path).cloned() {
            Some(e) if cas.has(&e.hash) => {
                let dir = path.parent().ok_or_else(|| DriveError::Io("no parent".into()))?;
                std::fs::create_dir_all(dir)?;
                let tmp = dir.join(format!("{TMP_PREFIX}{}", ulid::Ulid::new()));
                cas.clone_to(&e.hash, &tmp)?;
                std::fs::rename(&tmp, &path)?;
                let md = std::fs::metadata(&path)?;
                ix.touch(&ch.path, md.len(), mtime_ns(&md));
            }
            Some(_) => continue,
            None => {
                let _ = std::fs::remove_file(&path);
            }
        }
        fixed.push(ch.path);
    }
    ix.save(drive)?;
    Ok(fixed)
}

/// Write `data` at `root/rel` through a temp file (mode 0644), refusing traversal and symlinked
/// parents.
pub(crate) fn write_into(root: &Path, rel: &str, data: &[u8]) -> Result<()> {
    let dst = safe_join(root, rel).map_err(|e| ViewError::Spec(e.to_string()))?;
    let dir = dst.parent().ok_or_else(|| DriveError::Io(format!("{rel}: no parent")))?;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!("{TMP_PREFIX}{}", ulid::Ulid::new()));
    let r = std::fs::write(&tmp, data).and_then(|()| set_mode(&tmp, RW_FILE)).and_then(|()| std::fs::rename(&tmp, &dst));
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    Ok(r?)
}

/// Clone a daemon-owned file into the work tree (APFS clone where `std::fs::copy` clones), mode 0644.
pub(crate) fn clone_into(src: &Path, root: &Path, rel: &str) -> Result<std::fs::Metadata> {
    let dst = safe_join(root, rel).map_err(|e| ViewError::Spec(e.to_string()))?;
    let dir = dst.parent().ok_or_else(|| DriveError::Io(format!("{rel}: no parent")))?;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!("{TMP_PREFIX}{}", ulid::Ulid::new()));
    let r = std::fs::copy(src, &tmp).and_then(|_| set_mode(&tmp, RW_FILE)).and_then(|()| std::fs::rename(&tmp, &dst));
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r?;
    Ok(std::fs::symlink_metadata(&dst)?)
}

pub(crate) fn manifest(spec: &ViewSpec, lay: &Layout, l: Option<&Lease>) -> ViewManifest {
    let mut binds = vec![Bind::Ro { src: lay.base(), dest: "/".into() }];
    let dest = format!("/{WORK_MOUNT}");
    match spec.work.as_ref().map(|w| w.start) {
        Some(WorkStart::Clone) => binds.push(Bind::Rw { src: lay.work(), dest }),
        Some(WorkStart::Empty) => {
            binds.push(Bind::Overlay { lower: lay.lower(), upper: lay.work(), workdir: lay.ovl_workdir(), dest })
        }
        None => {}
    }
    let now = now_ms();
    ViewManifest {
        schema: VIEW_SCHEMA,
        root: lay.root().to_path_buf(),
        base: lay.base(),
        work: spec.work.as_ref().map(|_| lay.work()),
        work_start: spec.work.as_ref().map(|w| w.start),
        binds,
        lease: l.map(lease::info),
        spec_hash: spec.hash(),
        bucket: spec.bucket.clone(),
        base_includes: spec.base.clone(),
        work_prefix: spec.work.as_ref().map(|w| norm_prefix(&w.prefix)),
        mount_points: spec.mounts(),
        refresh: RefreshEndpoint { rpc: REFRESH_RPC.into(), root: lay.root().to_path_buf() },
        opened_ms: now,
        updated_ms: now,
    }
}
