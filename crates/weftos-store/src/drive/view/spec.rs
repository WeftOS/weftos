//! What a view may show and change (the [`ViewSpec`], its validation, the control-state rule), and
//! what the launcher is handed (the [`ViewManifest`], `view.json`).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::{DEFAULT_LEASE_TTL_MS, MAX_LEASE_TTL_MS, Result, ViewError, WORK_MOUNT};
use crate::drive::cas::hash_of;
use crate::drive::{DRIVE_DIR, TMP_PREFIX, TOMBSTONE_DIR, check_rel};
use crate::share::Limits;

/// Size ceilings, per include. Defaults are the S2 ones (16 MiB a file, 20,000 files, 512 MiB).
/// `max_total` is also checked across all includes together at open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewLimits {
    pub max_file: u64,
    pub max_files: usize,
    pub max_total: u64,
}

impl Default for ViewLimits {
    fn default() -> Self {
        let l = Limits::default();
        Self { max_file: l.max_file, max_files: l.max_files, max_total: l.max_total }
    }
}

impl ViewLimits {
    pub(crate) fn limits(&self) -> Limits {
        Limits { max_file: self.max_file, max_files: self.max_files, max_total: self.max_total, ..Limits::default() }
    }
}

/// One read-only prefix of the bucket, placed at `at` (relative to `base/`, empty for `base/`
/// itself). `at` values must not nest: an include at `""` must be the only one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Include {
    pub prefix: String,
    #[serde(default)]
    pub at: String,
}

/// How the work tree starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkStart {
    /// A clone of the work prefix, bound read-write at `/work`. Works on every host.
    #[default]
    Clone,
    /// Empty: the overlayfs upper over `lower/`. Needs overlayfs in the sandbox's user namespace.
    Empty,
}

/// The one writable prefix. Holding it needs the single-writer lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkSpec {
    pub prefix: String,
    #[serde(default)]
    pub start: WorkStart,
}

/// What a session may see and change. Derived by the daemon from the governance decision, never
/// by the session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewSpec {
    pub bucket: String,
    #[serde(default)]
    pub base: Vec<Include>,
    /// `None`: a read-only view (no work tree, no lease, publish refused).
    #[serde(default)]
    pub work: Option<WorkSpec>,
    /// Extra refusals on top of the drive's own (secret names, VCS files, reserved names). A
    /// pattern ending in `/` names a directory anywhere in the path; anything else is matched
    /// against each path segment, case-insensitively, with at most one `*`.
    #[serde(default)]
    pub deny: Vec<String>,
    #[serde(default)]
    pub limits: ViewLimits,
    #[serde(default = "default_ttl")]
    pub lease_ttl_ms: u64,
    /// Empty directories created at the top of `base/` for the launcher to mount over
    /// (for example `dev`, `proc`, `tmp` when the base is a root image).
    #[serde(default)]
    pub mount_points: Vec<String>,
    /// Lease holder and drive device id. Default: a fresh `view-<suffix>`.
    #[serde(default)]
    pub device: Option<String>,
}

fn default_ttl() -> u64 {
    DEFAULT_LEASE_TTL_MS
}

/// Store control state no view shows or publishes, whatever the include: drive state, delete
/// markers and temp names at any depth (the drive itself only refuses them at the prefix root),
/// the daemon's `.wt/` area, and a project bucket's `worktrees/` registry. Lease objects live
/// under `.drive/` and are covered.
pub fn is_control_key(bucket: &str, key: &str) -> bool {
    let first = key.split('/').next().unwrap_or("");
    first == ".wt"
        || (bucket.starts_with("p-") && first == "worktrees")
        || key.split('/').any(|s| s == DRIVE_DIR || s == TOMBSTONE_DIR || s.starts_with(TMP_PREFIX))
}

fn glob_seg(pat: &str, s: &str) -> bool {
    match pat.split_once('*') {
        Some((a, b)) => s.len() >= a.len() + b.len() && s.starts_with(a) && s.ends_with(b),
        None => pat == s,
    }
}

fn nested(a: &str, b: &str) -> bool {
    a.is_empty() || b.is_empty() || a == b || b.starts_with(&format!("{a}/")) || a.starts_with(&format!("{b}/"))
}

fn bad(s: impl Into<String>) -> ViewError {
    ViewError::Spec(s.into())
}

fn check_prefix(p: &str) -> Result<()> {
    let t = p.trim_matches('/');
    if !t.is_empty() && crate::overlay::validate_key(t).is_err() {
        return Err(bad(format!("prefix {p:?} is not a valid key prefix")));
    }
    Ok(())
}

impl ViewSpec {
    /// A stable hash of the spec, recorded in `view.json` and checked on load.
    pub fn hash(&self) -> String {
        hash_of(&serde_json::to_vec(self).unwrap_or_default())
    }

    pub fn read_only(&self) -> bool {
        self.work.is_none()
    }

    /// The mount points created in `base/`, the work mount first.
    pub fn mounts(&self) -> Vec<String> {
        let mut v: Vec<String> = self.work.iter().map(|_| WORK_MOUNT.to_owned()).collect();
        v.extend(self.mount_points.iter().cloned());
        v
    }

    /// Is `rel` (a path relative to its include) refused by a deny pattern?
    pub fn denied(&self, rel: &str) -> bool {
        let l = rel.to_ascii_lowercase();
        self.deny.iter().any(|p| {
            let p = p.to_ascii_lowercase();
            match p.strip_suffix('/') {
                Some(dir) => l == dir || l.starts_with(&format!("{dir}/")) || l.contains(&format!("/{dir}/")),
                None => l.split('/').any(|s| glob_seg(&p, s)),
            }
        })
    }

    pub fn validate(&self) -> Result<()> {
        if self.bucket.is_empty() || self.bucket.contains('/') {
            return Err(bad(format!("bucket {:?}", self.bucket)));
        }
        if self.base.is_empty() && self.work.is_none() {
            return Err(bad("nothing to show: no base include and no work tree"));
        }
        if self.lease_ttl_ms == 0 || self.lease_ttl_ms > MAX_LEASE_TTL_MS {
            return Err(bad(format!("lease_ttl_ms {} is not in 1..=24 h", self.lease_ttl_ms)));
        }
        if self.limits.max_file == 0 || self.limits.max_files == 0 || self.limits.max_total == 0 {
            return Err(bad("a zero size ceiling"));
        }
        let mounts = self.mounts();
        for (i, m) in mounts.iter().enumerate() {
            if m.contains('/') || check_rel(m).is_err() || mounts[..i].contains(m) {
                return Err(bad(format!("mount point {m:?} must be one unique, plain path segment")));
            }
        }
        for (i, inc) in self.base.iter().enumerate() {
            check_prefix(&inc.prefix)?;
            if !inc.at.is_empty() {
                check_rel(&inc.at).map_err(|why| bad(format!("include at {:?}: {why:?}", inc.at)))?;
                let first = inc.at.split('/').next().unwrap_or("");
                if mounts.iter().any(|m| m == first) {
                    return Err(bad(format!("include at {:?} lands on a mount point", inc.at)));
                }
            }
            if let Some(o) = self.base[..i].iter().find(|o| nested(&o.at, &inc.at)) {
                return Err(bad(format!("includes at {:?} and {:?} nest", o.at, inc.at)));
            }
        }
        if let Some(w) = &self.work {
            check_prefix(&w.prefix)?;
        }
        for p in &self.deny {
            let seg = p.strip_suffix('/').unwrap_or(p);
            if seg.is_empty() || seg.matches('*').count() > 1 || (p.ends_with('/') && seg.contains('*')) {
                return Err(bad(format!("deny pattern {p:?}")));
            }
        }
        Ok(())
    }
}

/// One bind the launcher makes. `dest` is the path inside the sandbox, assuming `base/` is `/`;
/// a launcher that binds `base/` under a chroot path prefixes every `dest` with it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Bind {
    /// `bwrap --ro-bind <src> <dest>`.
    Ro { src: PathBuf, dest: String },
    /// `bwrap --bind <src> <dest>`.
    Rw { src: PathBuf, dest: String },
    /// `bwrap --overlay-src <lower> --overlay <upper> <workdir> <dest>`.
    Overlay { lower: PathBuf, upper: PathBuf, workdir: PathBuf, dest: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseInfo {
    pub key: String,
    pub id: String,
    pub holder: String,
    pub expires_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefreshEndpoint {
    pub rpc: String,
    pub root: PathBuf,
}

/// `view.json`: everything the launcher needs, nothing secret (no credential, no store endpoint).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewManifest {
    pub schema: u32,
    pub root: PathBuf,
    pub base: PathBuf,
    pub work: Option<PathBuf>,
    pub work_start: Option<WorkStart>,
    /// In order: the base first, then the work tree.
    pub binds: Vec<Bind>,
    pub lease: Option<LeaseInfo>,
    pub spec_hash: String,
    pub bucket: String,
    pub base_includes: Vec<Include>,
    pub work_prefix: Option<String>,
    pub mount_points: Vec<String>,
    /// The deny patterns the view applied (the spec's list; the drive's built-in secret-name and
    /// dangerous-path denylists are not repeated here). The launcher checks that the globs it asked
    /// for took effect. Absent in manifests written before 2026-11-08: read as empty.
    #[serde(default)]
    pub deny: Vec<String>,
    pub refresh: RefreshEndpoint,
    pub opened_ms: u64,
    pub updated_ms: u64,
}
