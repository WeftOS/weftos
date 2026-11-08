//! A scoped view of the store for one sandboxed session (Tapestry R9, the filesystem half of the
//! Linux bwrap/Landlock backend). Spec: `docs/architecture/store-scoped-view.md`.
//!
//! The view is **materialised, not mounted**: every file is a real local file placed by the drive
//! (ADR-131 section 1), and the sandbox launcher binds plain local directories. Layout under the
//! view root, an absolute, already resolved, empty or absent directory:
//!
//! - `base/`: the read-only includes, mirrored out of their drives with no drive state in them,
//!   plus empty mount points (`work/` when there is a work tree, and [`ViewSpec::mount_points`]).
//!   Directories 0555, files 0444. Bound read-only, as `/` or under a chroot path. On the Mac (no
//!   bwrap) the modes are advisory: the owner can change them back, and [`ScopedView::status`]
//!   reports a changed base file as tampered.
//! - `work/`: the session's writable tree, bound at `/work`. [`WorkStart::Clone`]: a clone of the
//!   work prefix, bound read-write. [`WorkStart::Empty`]: an empty overlayfs upper over `lower/`;
//!   the upper is the change set.
//! - `lower/` ([`WorkStart::Empty`] only): read-only mirror of the work prefix, the overlay lower.
//! - `.state/`: the daemon's alone, never bound: one drive per include, the work drive, the lease,
//!   the work index, the overlay workdir.
//! - `view.json`: the [`ViewManifest`] the launcher reads. Written last, so a root with the state
//!   marker and no manifest is a crashed open, which the next open cleans up.
//!
//! The session never talks to the store. The daemon derives the [`ViewSpec`] from the governance
//! decision, opens the view, hands `view.json` to the launcher, and calls refresh, publish and
//! close on the session's behalf.

use std::path::{Path, PathBuf};

use super::index::Entry;
use super::{ChangeKind, DriveError, Index, Options, Skipped, materialise_with, norm_prefix, now_ms, scan};
use crate::backend::{BackendError, ObjectBackend};

mod lease;
mod spec;
mod tree;
mod work;

#[cfg(all(test, feature = "rustfs"))]
mod live_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_fail;

pub use lease::{LEASE_PREFIX, LEASE_SKEW_MS, Lease, LeaseBody, lease_key};
pub use spec::{Bind, Include, LeaseInfo, RefreshEndpoint, ViewLimits, ViewManifest, ViewSpec, WorkSpec, WorkStart, is_control_key};
pub use work::{PublishReport, RefreshReport, WorkChange};

pub const VIEW_SCHEMA: u32 = 1;
pub const MANIFEST_FILE: &str = "view.json";
pub const BASE_DIR: &str = "base";
pub const WORK_DIR: &str = "work";
pub const LOWER_DIR: &str = "lower";
pub const STATE_DIR: &str = ".state";
/// The mount point created in `base/` for the work tree.
pub const WORK_MOUNT: &str = "work";
/// The daemon RPC the launcher (or the session's supervisor) asks for a refresh.
pub const REFRESH_RPC: &str = "store.view.refresh";
pub const DEFAULT_LEASE_TTL_MS: u64 = 15 * 60 * 1000;
pub(crate) const MAX_LEASE_TTL_MS: u64 = 24 * 60 * 60 * 1000;

/// Errors of the view layer. Every one leaves the view in a stated condition (see the spec).
#[derive(Debug, thiserror::Error)]
pub enum ViewError {
    #[error(transparent)]
    Drive(#[from] DriveError),
    #[error("bad view spec: {0}")]
    Spec(String),
    #[error("bad view root {0}: {1}")]
    Root(String, String),
    #[error("{0} is already an open view")]
    AlreadyOpen(String),
    #[error("{0} is not an open view")]
    NotAView(String),
    #[error("the work prefix is leased by {holder} until {expires_ms} (ms since epoch)")]
    LeaseHeld { holder: String, expires_ms: u64 },
    #[error("lease lost: {0}")]
    LeaseLost(String),
    #[error("lease expired at {0} (ms since epoch); renew it before publishing")]
    LeaseExpired(u64),
    #[error("the backend has no conditional write: a stale lease cannot be taken over safely")]
    LeaseTakeoverUnsupported,
    #[error("the view has no work tree: publish refused")]
    ReadOnly,
    #[error("view over budget: {0}")]
    OverBudget(String),
    #[error("a drive tree changed outside the view's own writes (tampered or damaged): {0:?}")]
    Tampered(Vec<String>),
    #[error("{0} open conflict(s) on the work prefix: close refused, nothing removed")]
    Unpublished(usize),
    #[error("not supported: {0}")]
    Unsupported(String),
}

impl From<std::io::Error> for ViewError {
    fn from(e: std::io::Error) -> Self {
        ViewError::Drive(e.into())
    }
}
impl From<serde_json::Error> for ViewError {
    fn from(e: serde_json::Error) -> Self {
        ViewError::Drive(e.into())
    }
}
impl From<BackendError> for ViewError {
    fn from(e: BackendError) -> Self {
        ViewError::Drive(e.into())
    }
}

pub type Result<T> = std::result::Result<T, ViewError>;

/// Paths under a view root.
#[derive(Debug, Clone)]
pub(crate) struct Layout(PathBuf);

impl Layout {
    pub(crate) fn root(&self) -> &Path {
        &self.0
    }
    pub(crate) fn manifest(&self) -> PathBuf {
        self.0.join(MANIFEST_FILE)
    }
    pub(crate) fn base(&self) -> PathBuf {
        self.0.join(BASE_DIR)
    }
    pub(crate) fn work(&self) -> PathBuf {
        self.0.join(WORK_DIR)
    }
    pub(crate) fn lower(&self) -> PathBuf {
        self.0.join(LOWER_DIR)
    }
    pub(crate) fn state(&self) -> PathBuf {
        self.0.join(STATE_DIR)
    }
    pub(crate) fn marker(&self) -> PathBuf {
        self.state().join("VIEW")
    }
    pub(crate) fn spec(&self) -> PathBuf {
        self.state().join("spec.json")
    }
    pub(crate) fn lease(&self) -> PathBuf {
        self.state().join("lease.json")
    }
    pub(crate) fn work_index(&self) -> PathBuf {
        self.state().join("work-index.json")
    }
    /// Work changes copied into the work drive whose upload is not finished (refused, in
    /// conflict, or interrupted). The tamper check allows exactly these.
    pub(crate) fn staged(&self) -> PathBuf {
        self.state().join("staged.json")
    }
    pub(crate) fn ovl_workdir(&self) -> PathBuf {
        self.state().join("ovl-work")
    }
    pub(crate) fn drive(&self, i: usize) -> PathBuf {
        self.state().join("drives").join(i.to_string())
    }
    pub(crate) fn work_drive(&self) -> PathBuf {
        self.state().join("drives").join("work")
    }
}

/// What [`ScopedView::open`] did.
#[derive(Debug, Default, Clone)]
pub struct OpenReport {
    /// A crashed earlier open (state marker, no manifest) was cleaned up first.
    pub recovered: bool,
    /// The holder of a stale (expired, or stamped from the future) lease that was taken over.
    pub took_over: Option<String>,
    pub files: usize,
    pub bytes: u64,
    pub fetched: usize,
    /// `(prefix, refusal)`; never silent.
    pub skipped: Vec<(String, Skipped)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncludeStatus {
    pub prefix: String,
    /// `None` for the work prefix.
    pub at: Option<String>,
    pub files: usize,
    pub bytes: u64,
    pub open_conflicts: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ViewStatus {
    pub root: PathBuf,
    pub spec_hash: String,
    pub read_only: bool,
    pub lease: Option<LeaseInfo>,
    /// By this host's clock.
    pub lease_expired: bool,
    pub includes: Vec<IncludeStatus>,
    pub work: Option<IncludeStatus>,
    /// Unpublished work changes.
    pub pending: Vec<(String, ChangeKind)>,
    /// Drive tree paths that no longer match their index (a base file changed under the view).
    pub tampered: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseMode {
    /// Publish the work tree first; refuse to close (removing nothing) if conflicts stay open.
    Publish,
    /// Drop the work tree unpublished.
    Discard,
}

#[derive(Debug, Default, Clone)]
pub struct CloseReport {
    pub published: Option<PublishReport>,
    /// `None`: there was no lease. `Some(false)`: the lease object was no longer ours (taken
    /// over, or gone), so it was left alone.
    pub lease_released: Option<bool>,
    pub lease_error: Option<String>,
    pub removed: Vec<PathBuf>,
    pub root_removed: bool,
    /// Names left in the root that the view did not create (the root is then kept).
    pub left: Vec<String>,
}

/// An open view. Cheap to drop: everything lives on disk, and [`ScopedView::load`] reopens it.
#[derive(Debug, Clone)]
pub struct ScopedView {
    lay: Layout,
    spec: ViewSpec,
    manifest: ViewManifest,
    lease: Option<Lease>,
}

impl ScopedView {
    pub fn root(&self) -> &Path {
        self.lay.root()
    }
    pub fn spec(&self) -> &ViewSpec {
        &self.spec
    }
    pub fn manifest(&self) -> &ViewManifest {
        &self.manifest
    }
    pub fn lease(&self) -> Option<&Lease> {
        self.lease.as_ref()
    }

    fn opts(&self, read_only: bool) -> Options {
        Options { limits: self.spec.limits.limits(), device: Some(self.device()), read_only, ..Options::default() }
    }

    fn device(&self) -> String {
        self.lease.as_ref().map(|l| l.body.holder.clone()).or_else(|| self.spec.device.clone()).unwrap_or_else(|| "view".into())
    }

    /// Lease (when there is a work tree), materialise, mirror, lock the base, write `view.json`.
    /// Any failure releases the lease and removes what was created, by exact path.
    pub fn open<B: ObjectBackend + ?Sized>(b: &B, spec: &ViewSpec, root: &Path) -> Result<(Self, OpenReport)> {
        spec.validate()?;
        tree::check_root(root)?;
        let lay = Layout(root.to_path_buf());
        let mut rep = OpenReport::default();
        let existed = std::fs::symlink_metadata(root).is_ok();
        if existed {
            if lay.manifest().exists() {
                return Err(ViewError::AlreadyOpen(root.display().to_string()));
            }
            if lay.marker().is_file() {
                tree::recover(b, &lay);
                rep.recovered = true;
            }
            if std::fs::read_dir(root)?.next().is_some() {
                return Err(ViewError::Root(root.display().to_string(), "not empty, and not a view".into()));
            }
        }
        tree::create_private_dir(root)?;
        let holder = spec.device.clone().unwrap_or_else(|| format!("view-{}", &ulid::Ulid::new().to_string().to_lowercase()[16..]));
        let mut view = Self {
            lay: lay.clone(),
            spec: spec.clone(),
            manifest: tree::manifest(spec, &lay, None),
            lease: None,
        };
        let res = view.open_inner(b, &holder, &mut rep);
        if let Err(e) = res {
            if let Some(l) = &view.lease {
                let _ = lease::release(b, l);
            }
            let _ = tree::remove_view(&lay);
            if !existed {
                let _ = std::fs::remove_dir(root);
            }
            return Err(e);
        }
        Ok((view, rep))
    }

    fn open_inner<B: ObjectBackend + ?Sized>(&mut self, b: &B, holder: &str, rep: &mut OpenReport) -> Result<()> {
        let lay = self.lay.clone();
        let spec = self.spec.clone();
        std::fs::create_dir_all(lay.state())?;
        tree::write_json(&lay.spec(), &spec)?;
        tree::write_json(&lay.marker(), &serde_json::json!({"spec_hash": spec.hash()}))?;
        if let Some(w) = &spec.work {
            let (l, prev) = lease::acquire(b, &spec.bucket, &norm_prefix(&w.prefix), holder, spec.lease_ttl_ms, now_ms())?;
            tree::write_json(&lay.lease(), &l)?;
            rep.took_over = prev.map(|p| p.holder);
            self.lease = Some(l);
        }
        let opts = Options { limits: spec.limits.limits(), device: Some(holder.to_owned()), read_only: true, ..Options::default() };
        let mut total = 0u64;
        let mut fill = |dest: &Path, prefix: &str, read_only: bool, rep: &mut OpenReport| -> Result<()> {
            let d = materialise_with(b, &spec.bucket, prefix, dest, &Options { read_only, ..opts.clone() })?;
            rep.fetched += d.fetched;
            rep.skipped.extend(d.skipped.into_iter().map(|s| (prefix.to_owned(), s)));
            let ix = Index::load(dest)?.ok_or_else(|| ViewError::NotAView(dest.display().to_string()))?;
            total += ix.entries.values().map(|e| e.size).sum::<u64>();
            if total > spec.limits.max_total {
                return Err(ViewError::OverBudget(format!("{total} bytes across includes, ceiling {}", spec.limits.max_total)));
            }
            Ok(())
        };
        for (i, inc) in spec.base.iter().enumerate() {
            fill(&lay.drive(i), &inc.prefix, true, rep)?;
        }
        if let Some(w) = &spec.work {
            fill(&lay.work_drive(), &w.prefix, false, rep)?;
        }
        std::fs::create_dir_all(lay.base())?;
        let mounts = spec.mounts();
        for (i, inc) in spec.base.iter().enumerate() {
            let mut sk = Vec::new();
            let (n, bytes) = tree::mirror(&lay.drive(i), &lay.base(), &inc.at, &inc.prefix, &spec, &mounts, &mut sk)?;
            rep.files += n;
            rep.bytes += bytes;
            rep.skipped.extend(sk.into_iter().map(|s| (inc.prefix.clone(), s)));
        }
        for m in &mounts {
            std::fs::create_dir_all(lay.base().join(m))?;
        }
        if let Some(w) = &spec.work {
            let mut sk = Vec::new();
            std::fs::create_dir_all(lay.work())?;
            match w.start {
                WorkStart::Clone => rep.files += work::clone_work(&spec, &lay, &mut sk)?,
                WorkStart::Empty => {
                    let (n, _) = tree::mirror(&lay.work_drive(), &lay.lower(), "", &w.prefix, &spec, &[], &mut sk)?;
                    rep.files += n;
                    std::fs::create_dir_all(lay.ovl_workdir())?;
                    tree::set_tree_mode(&lay.lower(), true)?;
                }
            }
            rep.skipped.extend(sk.into_iter().map(|s| (w.prefix.clone(), s)));
        }
        tree::set_tree_mode(&lay.base(), true)?;
        self.manifest = tree::manifest(&spec, &lay, self.lease.as_ref());
        self.save_manifest()
    }

    pub(crate) fn save_manifest(&mut self) -> Result<()> {
        self.manifest.updated_ms = now_ms();
        self.manifest.lease = self.lease.as_ref().map(lease::info);
        tree::write_json(&self.lay.manifest(), &self.manifest)
    }

    /// Reopen a view from disk (another process, or after a daemon restart).
    pub fn load(root: &Path) -> Result<Self> {
        let lay = Layout(root.to_path_buf());
        let read = |p: &Path| -> Result<Vec<u8>> {
            std::fs::read(p).map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => ViewError::NotAView(root.display().to_string()),
                _ => e.into(),
            })
        };
        let manifest: ViewManifest = serde_json::from_slice(&read(&lay.manifest())?)?;
        let spec: ViewSpec = serde_json::from_slice(&read(&lay.spec())?)?;
        if manifest.schema != VIEW_SCHEMA || manifest.spec_hash != spec.hash() || manifest.root != root {
            return Err(ViewError::NotAView(format!("{}: manifest does not match its spec or root", root.display())));
        }
        let lease = match spec.work {
            Some(_) => Some(serde_json::from_slice(&read(&lay.lease())?)?),
            None => None,
        };
        Ok(Self { lay, spec, manifest, lease })
    }

    /// Extend the lease. Fails with [`ViewError::LeaseLost`] if another holder took it.
    pub fn renew<B: ObjectBackend + ?Sized>(&mut self, b: &B) -> Result<()> {
        let l = self.lease.as_ref().ok_or(ViewError::ReadOnly)?;
        let l = lease::renew(b, l, self.spec.lease_ttl_ms, now_ms())?;
        tree::write_json(&self.lay.lease(), &l)?;
        self.lease = Some(l);
        self.save_manifest()
    }

    pub fn status(&self) -> Result<ViewStatus> {
        let limits = self.spec.limits.limits();
        let mut tampered = Vec::new();
        let mut inc_status = |dest: &Path, prefix: &str, at: Option<String>| -> Result<IncludeStatus> {
            let ix = Index::load(dest)?.ok_or_else(|| ViewError::NotAView(dest.display().to_string()))?;
            tampered.extend(scan(dest, &ix, &limits)?.changes.into_iter().map(|c| format!("{prefix}{}", c.path)));
            Ok(IncludeStatus {
                prefix: prefix.to_owned(),
                at,
                files: ix.entries.len(),
                bytes: ix.entries.values().map(|e: &Entry| e.size).sum(),
                open_conflicts: ix.conflicts.keys().cloned().collect(),
            })
        };
        let mut includes = Vec::new();
        for (i, inc) in self.spec.base.iter().enumerate() {
            includes.push(inc_status(&self.lay.drive(i), &norm_prefix(&inc.prefix), Some(inc.at.clone()))?);
        }
        let work = match &self.spec.work {
            Some(w) => Some(inc_status(&self.lay.work_drive(), &norm_prefix(&w.prefix), None)?),
            None => None,
        };
        let pending = match &self.spec.work {
            Some(_) => work::pending(self)?.into_iter().map(|c| (c.path, c.kind)).collect(),
            None => Vec::new(),
        };
        let lease = self.lease.as_ref().map(lease::info);
        Ok(ViewStatus {
            root: self.lay.root().to_path_buf(),
            spec_hash: self.manifest.spec_hash.clone(),
            read_only: self.spec.read_only(),
            lease_expired: lease.as_ref().is_some_and(|l| now_ms() >= l.expires_ms),
            lease,
            includes,
            work,
            pending,
            tampered,
        })
    }

    /// Publish or discard the work, release the lease, remove the view's own paths. A root that
    /// holds anything else is kept and its extra names are reported.
    pub fn close<B: ObjectBackend + ?Sized>(mut self, b: &B, mode: CloseMode) -> Result<CloseReport> {
        let mut rep = CloseReport::default();
        if mode == CloseMode::Publish && self.spec.work.is_some() {
            let p = self.publish(b)?;
            if p.open_conflicts > 0 {
                return Err(ViewError::Unpublished(p.open_conflicts));
            }
            rep.published = Some(p);
        }
        if let Some(l) = &self.lease {
            match lease::release(b, l) {
                Ok(released) => rep.lease_released = Some(released),
                Err(e) => rep.lease_error = Some(e.to_string()),
            }
        }
        rep.removed = tree::remove_view(&self.lay)?;
        let root = self.lay.root();
        match std::fs::remove_dir(root) {
            Ok(()) => rep.root_removed = true,
            Err(_) => {
                rep.left = std::fs::read_dir(root)
                    .map(|rd| rd.filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned())).collect())
                    .unwrap_or_default();
                rep.left.sort();
            }
        }
        Ok(rep)
    }
}
