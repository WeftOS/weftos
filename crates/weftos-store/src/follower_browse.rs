//! Read-only browse of a shared worktree's tree, for a dashboard that has no checkout and no
//! local weaver (WO-069 lane B, prototype). The daemon's `store.browse.*` RPC (WO-067) calls these
//! functions; the CLI verb `store worktree browse` calls the same ones.
//!
//! Safety shape, all of it checked before an object is read:
//! - the slug is resolved through the project registry, then [`verify_entry`] cross-checks the
//!   bucket's `.wt/meta.json` (a registry entry is a pointer, not authority);
//! - the only objects ever read are `.wt/tree/<path>` for a path the manifest lists, and the
//!   manifest itself. Never `.wt/git/`, `.wt/handoff.json`, `.wt/base/`, `.wt/meta.json` contents,
//!   or the overlay layer. Listings are built from the manifest, not by listing the bucket;
//! - a path is refused exactly as [`crate::share_fetch::fetch_tree_with`] refuses one: traversal,
//!   absolute, VCS control names, `.wt`, secret-looking names, Windows-hostile names, and any file
//!   the manifest does not list (or lists under a name a fetch would refuse);
//! - `get` never returns more than [`MAX_GET_BYTES`]: over the cap it returns `withheld` with the
//!   size and checksum instead of bytes, and the bytes it does return match the manifest's SHA-256.
//!
//! Field names avoid `key`, `token`, `secret` and `password`, which the dashboard channel filters.

use crate::follower::verify_entry;
use crate::overlay::{ObjectBackend, Result, S2Error, validate_key};
use crate::share::{
    FileEntry, State, TREE_PREFIX, TreeManifest, is_dangerous_path, is_secret_path, read_manifest,
    sha256_hex,
};
use crate::share_fetch::{REGISTRY_PREFIX, RegistryEntry, check_entry, list_shared_lenient};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};

/// Hard upper bound on the bytes one [`browse_get`] returns, whatever `max_bytes` asks for.
pub const MAX_GET_BYTES: u64 = 4 << 20;
/// Most entries one [`browse_ls`] returns; more sets `truncated`.
pub const MAX_LS_ENTRIES: usize = 2000;
/// Longest path or prefix accepted.
pub const MAX_PATH_BYTES: usize = 1024;

/// One worktree in the project registry, as a dashboard list row.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RegistrySummary {
    pub slug: String,
    pub branch: String,
    pub head: String,
    pub state: State,
    pub writer: String,
    pub merger: Option<String>,
    pub doc_count: usize,
    pub file_count: usize,
    /// Latest of the last publish and the last handoff change.
    pub updated_ms: u64,
}

/// The registry list, newest first, with the entries that could not be read.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct RegistryBrowse {
    pub worktrees: Vec<RegistrySummary>,
    /// Slugs whose registry entry is corrupt, from a newer schema, or names another worktree.
    pub unreadable: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    Dir,
    File,
}

/// One row of a folder listing.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BrowseEntry {
    pub name: String,
    pub kind: EntryKind,
    /// Path from the tree root, the value to pass back as `prefix` or `path`.
    pub path: String,
    /// File size, or for a folder the sum of the files under it.
    pub size: u64,
    /// For a folder, how many files are under it.
    pub files: usize,
    pub sha256: Option<String>,
    /// Title from the manifest's docs index, when the file is a doc.
    pub title: Option<String>,
    /// The file's working-tree content is not in the published head commit.
    pub uncommitted: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BrowseListing {
    pub slug: String,
    pub branch: String,
    pub head: String,
    pub state: State,
    pub writer: String,
    pub doc_count: usize,
    pub file_count: usize,
    pub updated_ms: u64,
    pub published_ms: u64,
    /// The folder listed, `""` for the root.
    pub path: String,
    pub entries: Vec<BrowseEntry>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BrowseFile {
    pub slug: String,
    pub path: String,
    pub size: u64,
    pub sha256: String,
    pub title: Option<String>,
    pub uncommitted: bool,
    /// The bytes were not returned (over the cap); `size` and `sha256` still are.
    pub withheld: bool,
    pub withheld_reason: Option<String>,
    /// The cap that applied: the smaller of the caller's `max_bytes` and [`MAX_GET_BYTES`].
    pub cap: u64,
    pub data: Option<Vec<u8>>,
}

fn actor(a: &crate::share::Actor) -> String {
    format!("{}@{}", a.agent, a.host)
}

pub(crate) fn summary(e: &RegistryEntry) -> RegistrySummary {
    RegistrySummary {
        slug: e.slug.clone(),
        branch: e.branch.clone(),
        head: e.head_commit.clone(),
        state: e.state,
        writer: actor(&e.writer),
        merger: e.merger.as_ref().map(actor),
        doc_count: e.doc_count,
        file_count: e.file_count,
        updated_ms: e.updated_ms,
    }
}

/// The project's worktrees for a dashboard list. One corrupt registry entry is reported in
/// `unreadable`, not fatal (as [`list_shared_lenient`]).
pub fn browse_registry<B: ObjectBackend>(b: &B, project_bucket: &str) -> Result<RegistryBrowse> {
    let (entries, bad) = list_shared_lenient(b, project_bucket)?;
    Ok(RegistryBrowse { worktrees: entries.iter().map(summary).collect(), unreadable: bad })
}

pub(crate) fn slug_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 48
        && s.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        && !s.starts_with('-')
}

/// Registry entry and manifest for `slug`, after the slug check and the `.wt/meta.json` cross-check.
fn open<B: ObjectBackend>(
    b: &B,
    project_bucket: &str,
    slug: &str,
) -> Result<(RegistryEntry, TreeManifest)> {
    if !slug_ok(slug) {
        return Err(S2Error::Invalid(format!("bad worktree slug {slug:?}")));
    }
    let raw = b.get(project_bucket, &format!("{REGISTRY_PREFIX}{slug}.json"))?;
    let entry = check_entry(project_bucket, slug, raw.as_deref())
        .ok_or_else(|| S2Error::Invalid(format!("no readable registry entry for worktree {slug}")))?;
    verify_entry(b, &entry)?;
    let m = read_manifest(b, &entry.bucket)?
        .ok_or_else(|| S2Error::Invalid(format!("worktree {slug} has no shared tree")))?;
    Ok((entry, m))
}

/// A path under the tree root, validated as a fetch validates one. `""` is the root when
/// `allow_root` (folders), which also accepts a trailing `/`; a file path may not end in one.
pub(crate) fn clean_path(raw: &str, allow_root: bool) -> Result<String> {
    // A folder may be named with a trailing `/`; a file may not.
    let p = if allow_root { raw.strip_suffix('/').unwrap_or(raw) } else { raw };
    if p.is_empty() && raw != "/" {
        return if allow_root { Ok(String::new()) } else { Err(S2Error::BadKey(raw.to_string())) };
    }
    if p.len() > MAX_PATH_BYTES {
        return Err(S2Error::BadKey(raw.chars().take(64).collect()));
    }
    validate_key(p)?;
    // `.wt` is a dangerous segment, so a prefix aimed at `.wt/git/` or at the control area is refused here.
    if is_dangerous_path(p) || is_secret_path(&format!("{p}/x")) || is_secret_path(p) {
        return Err(S2Error::BadKey(p.to_string()));
    }
    Ok(p.to_string())
}

/// The manifest's files a fetch would write: not dangerous, not secret-looking, first of any case
/// collision (the same rule as `fetch_tree_with`).
fn servable(m: &TreeManifest) -> Vec<&FileEntry> {
    let mut lower = HashSet::new();
    m.files
        .iter()
        .filter(|f| {
            validate_key(&f.path).is_ok()
                && !is_dangerous_path(&f.path)
                && !is_secret_path(&f.path)
                && lower.insert(f.path.to_ascii_lowercase())
        })
        .collect()
}

fn titles(m: &TreeManifest) -> HashMap<&str, &str> {
    m.docs.iter().filter_map(|d| Some((d.path.as_str(), d.title.as_deref()?))).collect()
}

/// List one folder of the shared tree, one level, folders first. `prefix` is `""` for the root.
/// Built from the manifest; no object is read except the manifest.
pub fn browse_ls<B: ObjectBackend>(
    b: &B,
    project_bucket: &str,
    slug: &str,
    prefix: &str,
) -> Result<BrowseListing> {
    let dir = clean_path(prefix, true)?;
    let (e, m) = open(b, project_bucket, slug)?;
    ls_from(&e, &m, dir)
}

/// The listing half of [`browse_ls`], on an already-opened (or cached) entry and manifest.
pub(crate) fn ls_from(e: &RegistryEntry, m: &TreeManifest, dir: String) -> Result<BrowseListing> {
    let files = servable(m);
    if files.iter().any(|f| f.path == dir) {
        return Err(S2Error::Invalid(format!("{dir} is a file; use get")));
    }
    let base = if dir.is_empty() { String::new() } else { format!("{dir}/") };
    let (titles, unc): (_, HashSet<&str>) = (titles(m), m.uncommitted.iter().map(String::as_str).collect());
    let mut dirs: BTreeMap<&str, (u64, usize)> = BTreeMap::new();
    let mut leaves: BTreeMap<&str, &FileEntry> = BTreeMap::new();
    for f in files {
        let Some(rest) = f.path.strip_prefix(&base) else { continue };
        match rest.split_once('/') {
            Some((d, _)) => {
                let s = dirs.entry(d).or_default();
                s.0 += f.size;
                s.1 += 1;
            }
            None => {
                leaves.insert(rest, f);
            }
        }
    }
    if !dir.is_empty() && dirs.is_empty() && leaves.is_empty() {
        return Err(S2Error::Invalid(format!("no such folder {dir}")));
    }
    let mut entries: Vec<BrowseEntry> = dirs
        .into_iter()
        .map(|(n, (size, files))| BrowseEntry {
            name: n.into(),
            kind: EntryKind::Dir,
            path: format!("{base}{n}"),
            size,
            files,
            sha256: None,
            title: None,
            uncommitted: false,
        })
        .chain(leaves.into_iter().map(|(n, f)| BrowseEntry {
            name: n.into(),
            kind: EntryKind::File,
            path: f.path.clone(),
            size: f.size,
            files: 1,
            sha256: Some(f.sha256.clone()),
            title: titles.get(f.path.as_str()).map(|t| (*t).to_string()),
            uncommitted: unc.contains(f.path.as_str()),
        }))
        .collect();
    let truncated = entries.len() > MAX_LS_ENTRIES;
    entries.truncate(MAX_LS_ENTRIES);
    Ok(BrowseListing {
        slug: e.slug.clone(),
        branch: e.branch.clone(),
        head: e.head_commit.clone(),
        state: e.state,
        writer: actor(&e.writer),
        doc_count: e.doc_count,
        file_count: e.file_count,
        updated_ms: e.updated_ms,
        published_ms: m.published_ms,
        path: dir,
        entries,
        truncated,
    })
}

/// Read one file of the shared tree, at most `min(max_bytes, MAX_GET_BYTES)` bytes. Over the cap
/// the result has `withheld: true`, the size and checksum, and no bytes. The path must be listed
/// in the manifest; returned bytes are checked against its SHA-256.
pub fn browse_get<B: ObjectBackend>(
    b: &B,
    project_bucket: &str,
    slug: &str,
    path: &str,
    max_bytes: u64,
) -> Result<BrowseFile> {
    let path = clean_path(path, false)?;
    let (e, m) = open(b, project_bucket, slug)?;
    get_from(b, &e, &m, slug, &path, max_bytes)
}

/// The read half of [`browse_get`], on an already-opened (or cached) entry and manifest. `path`
/// is already cleaned.
pub(crate) fn get_from<B: ObjectBackend>(
    b: &B,
    e: &RegistryEntry,
    m: &TreeManifest,
    slug: &str,
    path: &str,
    max_bytes: u64,
) -> Result<BrowseFile> {
    let f = servable(m)
        .into_iter()
        .find(|f| f.path == path)
        .ok_or_else(|| S2Error::Invalid(format!("{path} is not in the shared tree of {slug}")))?;
    let cap = max_bytes.min(MAX_GET_BYTES);
    let bucket = e.bucket.as_str();
    let object = format!("{TREE_PREFIX}{}", f.path);
    debug_assert!(object.starts_with(TREE_PREFIX));
    let mut out = BrowseFile {
        slug: slug.to_string(),
        path: f.path.clone(),
        size: f.size,
        sha256: f.sha256.clone(),
        title: titles(m).get(f.path.as_str()).map(|t| (*t).to_string()),
        uncommitted: m.uncommitted.iter().any(|u| u == &f.path),
        withheld: false,
        withheld_reason: None,
        cap,
        data: None,
    };
    let over = |out: &mut BrowseFile| {
        out.withheld = true;
        out.withheld_reason = Some("over_cap".into());
    };
    if f.size > cap {
        over(&mut out);
        return Ok(out);
    }
    // The stored size decides too: the manifest is the writer's claim, the object is the fact.
    let Some(h) = b.head(bucket, &object)? else {
        return Err(S2Error::Invalid(format!("{path} is listed but its object is missing")));
    };
    if h.size > cap {
        out.size = h.size;
        over(&mut out);
        return Ok(out);
    }
    let data = b
        .get(bucket, &object)?
        .ok_or_else(|| S2Error::Invalid(format!("{path} is listed but its object is missing")))?;
    if data.len() as u64 > cap {
        out.size = data.len() as u64;
        over(&mut out);
        return Ok(out);
    }
    if sha256_hex(&data) != f.sha256 {
        return Err(S2Error::Backend(format!("checksum mismatch for {path}")));
    }
    out.data = Some(data);
    Ok(out)
}
