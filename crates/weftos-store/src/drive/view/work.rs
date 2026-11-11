//! The work tree: clone it, find its changes, publish them through the work drive's `sync_up`
//! (create-only puts, `If-Match` modifies and deletes, tombstones), refresh it, resolve conflicts.
//!
//! The work drive (`.state/drives/work`) is a normal drive whose tree only the daemon writes. A
//! publish copies each work change into that tree and sets the drive's index entry to the base the
//! change was made against, then runs `sync_up`. So a remote change since that base is refused by
//! the store (412) and becomes a conflict record, never an overwrite, even when a refresh has
//! already moved the drive forward.

use std::collections::BTreeSet;
use std::os::unix::fs::{FileTypeExt, MetadataExt};

use super::tree::{self, clone_into, write_into};
use super::{LOWER_DIR, Layout, Result, ScopedView, ViewError, ViewSpec, WorkStart, is_control_key, lease};
use crate::backend::ObjectBackend;
use crate::drive::cas::{hash_of, prune_empty_parents};
use crate::drive::index::{Entry, Index, Local, is_exec, local_state, mtime_ns};
use crate::drive::{
    ChangeKind, DownReport, Keep, SkipWhy, Skipped, UpReport, check_rel, now_ms, resolve, scan, sync_down_with, sync_up_with,
};
use crate::share::{Skip, read_regular};

/// One change in the work tree, relative to the base it was made against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkChange {
    pub path: String,
    pub kind: ChangeKind,
    /// The index entry of the version the change was made against (`None` for a create).
    pub base: Option<Entry>,
}

#[derive(Debug, Default, Clone)]
pub struct PublishReport {
    /// What `sync_up` of the work drive did (puts, deletes, conflicts, conditional writes).
    pub up: UpReport,
    /// Work changes copied into the work drive for this publish.
    pub applied: Vec<(String, ChangeKind)>,
    /// Work changes left alone because their path is in open conflict.
    pub held: Vec<String>,
    /// Open conflicts on the work prefix after this publish.
    pub open_conflicts: usize,
    pub skipped: Vec<Skipped>,
}

#[derive(Debug, Default, Clone)]
pub struct RefreshReport {
    /// One per base include, in spec order.
    pub base: Vec<DownReport>,
    /// Base drive paths put back from the CAS before the refresh (changed under the view).
    pub repaired: Vec<String>,
    pub work: Option<DownReport>,
    /// Unedited work files brought up to date.
    pub work_updated: Vec<String>,
    pub work_removed: Vec<String>,
    /// Remote changed a path the session has edited: left alone; the next publish turns it into
    /// a conflict record.
    pub work_conflicts: Vec<String>,
    /// `WorkStart::Empty`: the overlay lower is live under a running session and is not changed.
    pub work_not_refreshed: bool,
    pub skipped: Vec<Skipped>,
}

fn load_wi(lay: &Layout) -> Result<Index> {
    Ok(serde_json::from_slice(&std::fs::read(lay.work_index())?)?)
}

fn save_wi(lay: &Layout, wi: &Index) -> Result<()> {
    tree::write_json(&lay.work_index(), wi)
}

fn load_staged(lay: &Layout) -> Result<BTreeSet<String>> {
    match std::fs::read(lay.staged()) {
        Ok(d) => Ok(serde_json::from_slice(&d)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeSet::new()),
        Err(e) => Err(e.into()),
    }
}

fn save_staged(lay: &Layout, s: &BTreeSet<String>) -> Result<()> {
    tree::write_json(&lay.staged(), s)
}

fn work_drive_index(lay: &Layout) -> Result<Index> {
    Index::load(&lay.work_drive())?.ok_or_else(|| ViewError::NotAView(lay.work_drive().display().to_string()))
}

/// Fill `work/` with clones of the work drive's files and record their stat (the work index).
pub(crate) fn clone_work(spec: &ViewSpec, lay: &Layout, skipped: &mut Vec<Skipped>) -> Result<usize> {
    let ix = work_drive_index(lay)?;
    let mut wi = Index { entries: Default::default(), conflicts: Default::default(), ..ix.clone() };
    for (rel, e) in &ix.entries {
        if is_control_key(&spec.bucket, &ix.key(rel)) {
            skipped.push(Skipped { path: rel.clone(), why: SkipWhy::Reserved, detail: "store control state".into() });
            continue;
        }
        if spec.denied(rel) {
            skipped.push(Skipped::new(rel, SkipWhy::Denied));
            continue;
        }
        match clone_into(&lay.work_drive().join(rel), &lay.work(), rel) {
            Ok(md) => {
                wi.entries.insert(rel.clone(), Entry { size: md.len(), mtime_ns: mtime_ns(&md), ..e.clone() });
            }
            Err(err) => skipped.push(Skipped::io(rel, err)),
        }
    }
    save_wi(lay, &wi)?;
    Ok(wi.entries.len())
}

fn skip_of(s: Option<Skip>) -> SkipWhy {
    match s {
        Some(Skip::TooLarge) => SkipWhy::TooLarge,
        Some(Skip::Symlink) => SkipWhy::Symlink,
        Some(Skip::NotAFile) => SkipWhy::NotAFile,
        Some(_) => SkipWhy::Changed,
        None => SkipWhy::Io,
    }
}

/// `WorkStart::Clone`: the work tree against the work index.
fn clone_changes(v: &ScopedView, skipped: &mut Vec<Skipped>) -> Result<Vec<WorkChange>> {
    let lay = &v.lay;
    let mut wi = load_wi(lay)?;
    let s = scan(&lay.work(), &wi, &v.spec.limits.limits())?;
    if !s.touched.is_empty() {
        for (p, size, mt) in &s.touched {
            wi.touch(p, *size, *mt);
        }
        save_wi(lay, &wi)?;
    }
    skipped.extend(s.skipped);
    Ok(s.changes.into_iter().map(|c| WorkChange { base: wi.entries.get(&c.path).cloned(), path: c.path, kind: c.kind }).collect())
}

/// `WorkStart::Empty`: the overlay upper against the work drive's index. Files are creates or
/// modifies; overlayfs whiteouts (character devices 0/0) are deletes, of a file or of a whole
/// directory. Opaque-directory markers are an xattr this crate cannot read without a new
/// dependency: a directory removed and made again in the session keeps its lower files (spec §7).
fn upper_changes(v: &ScopedView, ix: &Index, skipped: &mut Vec<Skipped>) -> Result<Vec<WorkChange>> {
    let max = v.spec.limits.max_file;
    let mut out = Vec::new();
    let mut stack = vec![(v.lay.work(), String::new())];
    while let Some((dir, rel)) = stack.pop() {
        let mut names: Vec<_> = std::fs::read_dir(&dir)?.collect::<std::io::Result<Vec<_>>>()?;
        names.sort_by_key(|e| e.file_name());
        for e in names {
            let name = e.file_name().to_string_lossy().into_owned();
            let r = if rel.is_empty() { name } else { format!("{rel}/{name}") };
            let md = std::fs::symlink_metadata(e.path())?;
            let ft = md.file_type();
            if ft.is_dir() {
                stack.push((e.path(), r));
            } else if ft.is_char_device() && md.rdev() == 0 {
                let under = format!("{r}/");
                for (p, ent) in ix.entries.iter().filter(|(p, _)| **p == r || p.starts_with(&under)) {
                    out.push(WorkChange { path: p.clone(), kind: ChangeKind::Delete, base: Some(ent.clone()) });
                }
            } else if ft.is_symlink() {
                skipped.push(Skipped::new(&r, SkipWhy::Symlink));
            } else if !ft.is_file() {
                skipped.push(Skipped::new(&r, SkipWhy::NotAFile));
            } else if let Err(why) = check_rel(&r) {
                skipped.push(Skipped::new(&r, why));
            } else {
                let data = match read_regular(&e.path(), max) {
                    Ok(d) => d,
                    Err(s) => {
                        skipped.push(Skipped::new(&r, skip_of(s)));
                        continue;
                    }
                };
                match ix.entries.get(&r) {
                    Some(ent) if ent.hash == hash_of(&data) && ent.exec == is_exec(&md) => {}
                    Some(ent) => out.push(WorkChange { path: r, kind: ChangeKind::Modify, base: Some(ent.clone()) }),
                    None => out.push(WorkChange { path: r, kind: ChangeKind::Create, base: None }),
                }
            }
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out.dedup_by(|a, b| a.path == b.path);
    Ok(out)
}

/// Unpublished work changes (no store access).
pub(crate) fn pending(v: &ScopedView) -> Result<Vec<WorkChange>> {
    let mut sk = Vec::new();
    match v.spec.work.as_ref().map(|w| w.start) {
        Some(WorkStart::Clone) => clone_changes(v, &mut sk),
        Some(WorkStart::Empty) => upper_changes(v, &work_drive_index(&v.lay)?, &mut sk),
        None => Ok(Vec::new()),
    }
}

impl ScopedView {
    /// Push the work tree's changes. Refused for a read-only view, on a lost or expired lease,
    /// and when the work drive's tree no longer matches its index.
    pub fn publish<B: ObjectBackend + ?Sized>(&mut self, b: &B) -> Result<PublishReport> {
        let w = self.spec.work.clone().ok_or(ViewError::ReadOnly)?;
        let l = self.lease.as_ref().ok_or_else(|| ViewError::LeaseLost("no lease recorded".into()))?;
        lease::check(b, l, now_ms())?;
        let lay = self.lay.clone();
        let (sd, limits) = (lay.work_drive(), self.spec.limits.limits());
        let mut ix = work_drive_index(&lay)?;
        let mut staged = load_staged(&lay)?;
        let foreign: Vec<String> = scan(&sd, &ix, &limits)?.changes.into_iter().map(|c| c.path).filter(|p| !staged.contains(p)).collect();
        let mut foreign = foreign;
        if w.start == WorkStart::Empty {
            foreign.extend(tree::lower_drift(&lay.lower_index(), &lay.lower(), &self.spec, &w.prefix, &limits)?.into_iter().map(|p| format!("{LOWER_DIR}/{p}")));
        }
        if !foreign.is_empty() {
            return Err(ViewError::Tampered(foreign));
        }
        let mut rep = PublishReport::default();
        let mut changes = match w.start {
            WorkStart::Clone => clone_changes(self, &mut rep.skipped)?,
            WorkStart::Empty => upper_changes(self, &ix, &mut rep.skipped)?,
        };
        // Deletes first, so a file replacing a directory (or the reverse) has room.
        changes.sort_by_key(|c| (c.kind != ChangeKind::Delete, c.path.clone()));
        let src_root = lay.work();
        for c in changes {
            if ix.conflicts.contains_key(&c.path) {
                rep.held.push(c.path);
                continue;
            }
            if is_control_key(&self.spec.bucket, &ix.key(&c.path)) {
                rep.skipped.push(Skipped { path: c.path, why: SkipWhy::Reserved, detail: "store control state".into() });
                continue;
            }
            if self.spec.denied(&c.path) {
                rep.skipped.push(Skipped::new(&c.path, SkipWhy::Denied));
                continue;
            }
            match c.kind {
                ChangeKind::Delete => {
                    let Some(base) = c.base else { continue };
                    if let Ok(p) = crate::share::safe_join(&sd, &c.path)
                        && std::fs::symlink_metadata(&p).is_ok_and(|m| m.is_file())
                    {
                        std::fs::remove_file(&p)?;
                    }
                    ix.entries.insert(c.path.clone(), Entry { mtime_ns: 0, ..base });
                }
                ChangeKind::Create | ChangeKind::Modify => {
                    let data = match read_regular(&src_root.join(&c.path), limits.max_file) {
                        Ok(d) => d,
                        Err(s) => {
                            rep.skipped.push(Skipped::new(&c.path, skip_of(s)));
                            continue;
                        }
                    };
                    let exec = std::fs::symlink_metadata(src_root.join(&c.path)).is_ok_and(|m| is_exec(&m));
                    if let Err(e) = write_into(&sd, &c.path, &data, exec) {
                        rep.skipped.push(Skipped::io(&c.path, e));
                        continue;
                    }
                    match c.base {
                        Some(base) => ix.entries.insert(c.path.clone(), Entry { mtime_ns: 0, ..base }),
                        None => ix.entries.remove(&c.path),
                    };
                }
            }
            rep.applied.push((c.path, c.kind));
        }
        ix.save(&sd)?;
        staged.extend(rep.applied.iter().map(|(p, _)| p.clone()));
        save_staged(&lay, &staged)?;
        let up = sync_up_with(b, &sd, &self.opts(false));
        let after = work_drive_index(&lay)?;
        let left: BTreeSet<String> = scan(&sd, &after, &limits)?.changes.into_iter().map(|c| c.path).collect();
        staged.retain(|p| left.contains(p) || after.conflicts.contains_key(p));
        save_staged(&lay, &staged)?;
        if w.start == WorkStart::Clone {
            let mut paths: BTreeSet<String> = rep.applied.iter().map(|(p, _)| p.clone()).collect();
            if let Ok(u) = &up {
                paths.extend(u.put.iter().chain(&u.deleted).cloned());
            }
            reconcile(&lay, &after, &paths, limits.max_file)?;
        }
        rep.up = up?;
        rep.open_conflicts = after.conflicts.len();
        self.save_manifest()?;
        Ok(rep)
    }

    /// Pull remote changes: base includes always (repaired first), the work tree only where the
    /// session has not edited (`WorkStart::Clone`). Never writes to the store.
    pub fn refresh<B: ObjectBackend + ?Sized>(&mut self, b: &B) -> Result<RefreshReport> {
        let lay = self.lay.clone();
        let limits = self.spec.limits.limits();
        let mounts = self.spec.mounts();
        let mut rep = RefreshReport::default();
        tree::set_tree_mode(&lay.base(), false)?;
        let r = (|| -> Result<()> {
            for (i, inc) in self.spec.base.iter().enumerate() {
                rep.repaired.extend(tree::repair(&lay.drive(i), &limits)?);
                let d = sync_down_with(b, &lay.drive(i), &self.opts(true))?;
                tree::mirror(&lay.drive(i), &lay.base(), &inc.at, &inc.prefix, &self.spec, &mounts, &mut rep.skipped)?;
                rep.base.push(d);
            }
            Ok(())
        })();
        let locked = tree::set_tree_mode(&lay.base(), true);
        r?;
        locked?;
        match self.spec.work.as_ref().map(|w| w.start) {
            Some(WorkStart::Clone) => {
                let d = sync_down_with(b, &lay.work_drive(), &self.opts(false))?;
                self.apply_to_work(&d, &mut rep)?;
                rep.work = Some(d);
            }
            Some(WorkStart::Empty) => {
                // The lower is not brought forward under a running session, but it is put back to
                // the listing it was stamped with at open: extras removed, edited or missing files
                // restored from the CAS.
                let prefix = self.spec.work.as_ref().map(|w| w.prefix.clone()).unwrap_or_default();
                if load_staged(&lay)?.is_empty() {
                    rep.repaired.extend(tree::repair(&lay.work_drive(), &limits)?);
                }
                rep.repaired.extend(tree::repair_lower(&lay, &self.spec, &prefix, &limits)?);
                rep.work_not_refreshed = true;
            }
            None => {}
        }
        self.save_manifest()?;
        Ok(rep)
    }

    fn apply_to_work(&self, d: &DownReport, rep: &mut RefreshReport) -> Result<()> {
        let lay = &self.lay;
        let after = work_drive_index(lay)?;
        let mut wi = load_wi(lay)?;
        let max = self.spec.limits.max_file;
        for p in d.created.iter().chain(&d.updated) {
            let Some(e) = after.entries.get(p) else { continue };
            if self.spec.denied(p) {
                continue;
            }
            let prev = wi.entries.get(p).cloned();
            // The session changed the file's mode (bytes the same): that is its edit, not to be
            // overwritten by a refresh. A store mode equal to it is adopted; a content change from
            // the store over it is a conflict.
            let work_exec = std::fs::symlink_metadata(lay.work().join(p)).ok().filter(|m| m.is_file()).map(|m| is_exec(&m));
            let mode_edited = matches!((&prev, work_exec), (Some(pe), Some(w)) if w != pe.exec);
            match (prev.is_some(), local_state(&lay.work(), p, prev.as_ref(), max)) {
                (true, Ok(Local::Unchanged)) if mode_edited => {
                    if work_exec == Some(e.exec) {
                        let md = std::fs::symlink_metadata(lay.work().join(p))?;
                        wi.entries.insert(p.clone(), Entry { size: md.len(), mtime_ns: mtime_ns(&md), ..e.clone() });
                    } else {
                        rep.work_conflicts.push(p.clone());
                    }
                }
                (true, Ok(Local::Unchanged)) | (false, Ok(Local::Missing)) => {
                    let md = clone_into(&lay.work_drive().join(p), &lay.work(), p)?;
                    wi.entries.insert(p.clone(), Entry { size: md.len(), mtime_ns: mtime_ns(&md), ..e.clone() });
                    rep.work_updated.push(p.clone());
                }
                _ => rep.work_conflicts.push(p.clone()),
            }
        }
        for p in &d.removed {
            let prev = wi.entries.get(p).cloned();
            match local_state(&lay.work(), p, prev.as_ref(), max) {
                Ok(Local::Unchanged) if prev.is_some() => {
                    std::fs::remove_file(lay.work().join(p))?;
                    prune_empty_parents(&lay.work(), p);
                    wi.entries.remove(p);
                    rep.work_removed.push(p.clone());
                }
                Ok(Local::Missing) => {
                    wi.entries.remove(p);
                }
                _ => rep.work_conflicts.push(p.clone()),
            }
        }
        save_wi(lay, &wi)
    }

    /// Close one open conflict on the work prefix. `Keep::Local`: the next publish writes the work
    /// file over the remote version recorded in the conflict. `Keep::Remote`: the work file becomes
    /// the remote version (`WorkStart::Clone` only: an overlay upper cannot be changed under a
    /// running session).
    pub fn resolve(&mut self, path: &str, keep: Keep) -> Result<()> {
        let w = self.spec.work.clone().ok_or(ViewError::ReadOnly)?;
        if w.start == WorkStart::Empty && keep == Keep::Remote {
            return Err(ViewError::Unsupported("keep remote under an overlay upper: close the session, then reopen".into()));
        }
        let lay = self.lay.clone();
        resolve(&lay.work_drive(), path, keep)?;
        if w.start == WorkStart::Empty {
            return Ok(());
        }
        let after = work_drive_index(&lay)?;
        let mut wi = load_wi(&lay)?;
        match (keep, after.entries.get(path)) {
            // Force the next publish to see a modify against the remote version.
            (Keep::Local, Some(e)) => {
                wi.entries.insert(path.to_owned(), Entry { size: 0, mtime_ns: 0, hash: String::new(), etag: e.etag.clone(), exec: e.exec, stamp: String::new() });
            }
            (Keep::Local, None) => {
                wi.entries.remove(path);
            }
            (Keep::Remote, Some(e)) => {
                let md = clone_into(&lay.work_drive().join(path), &lay.work(), path)?;
                wi.entries.insert(path.to_owned(), Entry { size: md.len(), mtime_ns: mtime_ns(&md), ..e.clone() });
            }
            (Keep::Remote, None) => {
                if let Ok(p) = crate::share::safe_join(&lay.work(), path)
                    && std::fs::symlink_metadata(&p).is_ok_and(|m| m.is_file())
                {
                    std::fs::remove_file(&p)?;
                    prune_empty_parents(&lay.work(), path);
                }
                wi.entries.remove(path);
            }
        }
        save_wi(&lay, &wi)
    }
}

/// After a publish: a work path whose bytes now equal the work drive's entry is synced (record the
/// new etag); one deleted on both is forgotten; one in conflict stays pending.
fn reconcile(lay: &Layout, after: &Index, paths: &BTreeSet<String>, max: u64) -> Result<()> {
    let mut wi = load_wi(lay)?;
    for p in paths {
        if after.conflicts.contains_key(p) {
            continue;
        }
        let wp = lay.work().join(p);
        match std::fs::symlink_metadata(&wp) {
            Ok(md) if md.is_file() => {
                if let (Ok(data), Some(e)) = (read_regular(&wp, max), after.entries.get(p))
                    && hash_of(&data) == e.hash
                {
                    wi.entries.insert(p.clone(), Entry { size: md.len(), mtime_ns: mtime_ns(&md), ..e.clone() });
                }
            }
            Err(_) if !after.entries.contains_key(p) => {
                wi.entries.remove(p);
            }
            _ => {}
        }
    }
    save_wi(lay, &wi)
}
