//! Read side of worktree sharing: find a shared worktree and its docs from any node, materialize
//! the tree locally, and fetch the git bundle so a different agent can merge. See `share.rs`.

use crate::lock_ext::LockExt;
use crate::overlay::{ObjectBackend, Result, S2Error, validate_key};
use crate::share::*;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Registry prefix in the *project* bucket. Written by the daemon only (it holds the project
/// grant); RustFS IAM refuses it to worktree principals ("writes stay in the worktree bucket", §4).
pub const REGISTRY_PREFIX: &str = "worktrees/";

/// One object holding `slug -> etag of its registry entry`, rewritten on every registry change, so
/// a follower that has seen those etags skips those entries (ADR-126 section 4a poll cost). A hint
/// from a fresh listing each time (a lost update heals on the next refresh); a follower falls back
/// to the listing when it is missing or unreadable.
pub const DIGEST_KEY: &str = "worktrees/_digest.json";

/// How many entry fetches a follower runs at once.
pub const FETCH_JOBS: usize = 8;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Digest {
    pub schema: u32,
    pub entries: std::collections::BTreeMap<String, String>,
}

/// The slug of a registry object key, `None` for the digest and for anything else.
pub fn registry_slug(key: &str) -> Option<&str> {
    let s = key.strip_prefix(REGISTRY_PREFIX)?.strip_suffix(".json")?;
    (key != DIGEST_KEY && !s.is_empty()).then_some(s)
}

/// `slug -> etag` for every registry object, from one listing.
pub fn registry_listing<B: ObjectBackend>(b: &B, project_bucket: &str) -> Result<std::collections::BTreeMap<String, String>> {
    Ok(b.list(project_bucket, REGISTRY_PREFIX)?
        .into_iter()
        .filter_map(|o| registry_slug(&o.key).map(|s| (s.to_owned(), o.etag)))
        .collect())
}

/// Read the digest; a missing or unreadable one is `None` (callers fall back to listing).
pub fn read_digest<B: ObjectBackend>(b: &B, project_bucket: &str) -> Result<Option<Digest>> {
    Ok(b.get(project_bucket, DIGEST_KEY)?.and_then(|d| serde_json::from_slice::<Digest>(&d).ok()).filter(|d| d.schema == 1))
}

/// Daemon-side: rewrite the digest from a fresh listing. Best effort for callers: it is a hint.
pub fn write_digest<B: ObjectBackend>(b: &B, project_bucket: &str) -> Result<()> {
    let d = Digest { schema: 1, entries: registry_listing(b, project_bucket)? };
    b.put(project_bucket, DIGEST_KEY, &serde_json::to_vec(&d)?)?;
    Ok(())
}

/// Parse and cross-check one registry object (see [`list_shared_lenient`]).
pub(crate) fn check_entry(project_bucket: &str, slug: &str, data: Option<&[u8]>) -> Option<RegistryEntry> {
    let e: RegistryEntry = serde_json::from_slice(data?).ok()?;
    (e.slug == slug && expected_worktree_bucket(project_bucket, &e.slug).is_none_or(|x| x == e.bucket)).then_some(e)
}

/// Fetch and check the entries for `slugs`, up to `jobs` at a time. Returns the readable entries
/// and the slugs that were corrupt, from a newer schema, or pointed at the wrong bucket. A backend
/// error is an error.
pub fn fetch_entries<B: ObjectBackend + Sync>(
    b: &B,
    project_bucket: &str,
    slugs: &[String],
    jobs: usize,
) -> Result<(Vec<RegistryEntry>, Vec<String>)> {
    let (good, mut bad, missing) = fetch_entries_split(b, project_bucket, slugs, jobs)?;
    bad.extend(missing);
    bad.sort();
    Ok((good, bad))
}

/// [`fetch_entries`] with the slugs whose object does not exist reported apart from the corrupt
/// ones: an absent object is a removal (the registry listing would not show it), not damage. A
/// caller working from the digest uses this so a stale digest never turns a removed worktree into
/// an `Unreadable` slug. Returns `(readable, corrupt, missing)`.
pub fn fetch_entries_split<B: ObjectBackend + Sync>(
    b: &B,
    project_bucket: &str,
    slugs: &[String],
    jobs: usize,
) -> Result<(Vec<RegistryEntry>, Vec<String>, Vec<String>)> {
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results = std::sync::Mutex::new(Vec::with_capacity(slugs.len()));
    let first_err = std::sync::Mutex::new(None);
    std::thread::scope(|sc| {
        for _ in 0..jobs.clamp(1, slugs.len().max(1)) {
            sc.spawn(|| {
                loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(slug) = slugs.get(i) else { return };
                    match b.get(project_bucket, &format!("{REGISTRY_PREFIX}{slug}.json")) {
                        Ok(d) => results.lock_or_die("share fetch results").push((slug.clone(), check_entry(project_bucket, slug, d.as_deref()), d.is_none())),
                        Err(e) => {
                            first_err.lock_or_die("share fetch first error").get_or_insert(e);
                            return;
                        }
                    }
                }
            });
        }
    });
    if let Some(e) = first_err.into_inner().unwrap_or_else(|e| e.into_inner()) {
        return Err(e);
    }
    let (mut good, mut bad, mut missing) = (Vec::new(), Vec::new(), Vec::new());
    for (slug, e, absent) in results.into_inner().unwrap_or_else(|e| e.into_inner()) {
        match e {
            Some(e) => good.push(e),
            None if absent => missing.push(slug),
            None => bad.push(slug),
        }
    }
    good.sort_by(|a, c| c.updated_ms.cmp(&a.updated_ms).then_with(|| a.slug.cmp(&c.slug)));
    bad.sort();
    missing.sort();
    Ok((good, bad, missing))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RegistryEntry {
    pub slug: String,
    pub bucket: String,
    pub branch: String,
    pub head_commit: String,
    pub state: State,
    pub writer: Actor,
    pub merger: Option<Actor>,
    pub doc_count: usize,
    pub file_count: usize,
    pub updated_ms: u64,
    /// Present only in state `orphan` (see `orphan.rs`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orphan: Option<crate::orphan::OrphanInfo>,
    /// The older-writer check of the daemon that wrote this entry (WO-069 F1); absent from an older daemon's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub writer_check: Option<crate::share::WriterCheck>,
}

/// Build the registry entry for one worktree bucket from its own manifest and handoff.
pub fn summarize<B: ObjectBackend>(
    b: &B,
    slug: &str,
    bucket: &str,
) -> Result<Option<RegistryEntry>> {
    let (Some(m), Some(h)) = (read_manifest(b, bucket)?, read_handoff(b, bucket)?) else {
        return Ok(None);
    };
    Ok(Some(summarize_from(slug, bucket, &m, &h)))
}

/// [`summarize`] from a manifest and handoff the caller already holds (no store calls).
pub fn summarize_from(slug: &str, bucket: &str, m: &TreeManifest, h: &Handoff) -> RegistryEntry {
    RegistryEntry {
        slug: slug.into(),
        bucket: bucket.into(),
        branch: m.branch.clone(),
        head_commit: m.head_commit.clone(),
        state: h.state,
        writer: m.writer.clone(),
        merger: h.merger.clone(),
        doc_count: m.docs.len(),
        file_count: m.files.len(),
        updated_ms: h.updated_ms.max(m.published_ms),
        orphan: None,
        writer_check: Some(writer_check_for(Some(m), Some(h))),
    }
}

/// Daemon-side: refresh the project-bucket registry entry for a worktree.
pub fn write_registry_entry<B: ObjectBackend>(
    b: &B,
    project_bucket: &str,
    e: &RegistryEntry,
) -> Result<()> {
    validate_key(&e.slug)?;
    b.put(
        project_bucket,
        &format!("{REGISTRY_PREFIX}{}.json", e.slug),
        &serde_json::to_vec_pretty(e)?,
    )?;
    let _ = write_digest(b, project_bucket); // a hint; the entry is already written
    Ok(())
}

/// The bucket worktree `slug` of the project whose bucket is `project_bucket` must have. `None`
/// when `project_bucket` is not a real project bucket (`p-<ULID>`), as in fixtures that use
/// made-up names; the cross-check is then skipped.
pub fn expected_worktree_bucket(project_bucket: &str, slug: &str) -> Option<String> {
    let ulid = project_bucket.strip_prefix("p-").filter(|u| u.len() == 26)?;
    crate::StoreName::worktree("x", ulid, slug).ok().map(|n| n.bucket())
}

/// One registry object as read: the slug in its key, the exact bytes, and what they parse to.
pub(crate) struct RawEntry {
    pub key_slug: String,
    pub bytes: Vec<u8>,
    pub entry: RegistryEntry,
}

/// Read the registry once: each object is fetched a single time, so whatever is later checked
/// (a signature, say) is checked against the very bytes that were parsed. An entry whose body names
/// a different slug than its key, or a bucket other than this project's worktree bucket for that
/// slug (review F3), is refused: its key's slug goes to the second list with the unparseable ones.
pub(crate) fn read_registry<B: ObjectBackend>(
    b: &B,
    project_bucket: &str,
) -> Result<(Vec<RawEntry>, Vec<String>)> {
    let (mut v, mut bad) = (Vec::new(), Vec::new());
    for o in b.list(project_bucket, REGISTRY_PREFIX)?.into_iter().filter(|o| o.key != DIGEST_KEY) {
        let key_slug = o
            .key
            .trim_start_matches(REGISTRY_PREFIX)
            .trim_end_matches(".json")
            .to_string();
        let Some(bytes) = b.get(project_bucket, &o.key)? else { continue };
        match serde_json::from_slice::<RegistryEntry>(&bytes) {
            Ok(entry)
                if entry.slug == key_slug
                    && expected_worktree_bucket(project_bucket, &entry.slug).is_none_or(|x| x == entry.bucket) =>
            {
                v.push(RawEntry { key_slug, bytes, entry })
            }
            _ => bad.push(key_slug),
        }
    }
    v.sort_by(|a, c| {
        c.entry
            .updated_ms
            .cmp(&a.entry.updated_ms)
            .then_with(|| a.key_slug.cmp(&c.key_slug))
    });
    bad.sort();
    Ok((v, bad))
}

/// Every shared worktree of a project, newest first. Readable from any node with project read.
/// An unreadable or mislabelled entry is an error here; use [`list_shared_lenient`] to survive one.
pub fn list_shared<B: ObjectBackend>(b: &B, project_bucket: &str) -> Result<Vec<RegistryEntry>> {
    let (v, bad) = read_registry(b, project_bucket)?;
    if let Some(slug) = bad.first() {
        return Err(S2Error::Invalid(format!(
            "registry entry {slug} is unreadable or names a different worktree than its key"
        )));
    }
    Ok(v.into_iter().map(|r| r.entry).collect())
}

/// Like [`list_shared`] but a corrupt, unknown-schema or mislabelled entry is reported, not fatal:
/// returns the readable entries and the key slugs of the rest.
pub fn list_shared_lenient<B: ObjectBackend>(
    b: &B,
    project_bucket: &str,
) -> Result<(Vec<RegistryEntry>, Vec<String>)> {
    let (v, bad) = read_registry(b, project_bucket)?;
    Ok((v.into_iter().map(|r| r.entry).collect(), bad))
}

/// Docs and planning in a published tree whose path or title contains `query` (case-insensitive).
/// An empty query returns all of them.
pub fn find_docs<'m>(m: &'m TreeManifest, query: &str) -> Vec<&'m DocEntry> {
    let q = query.to_ascii_lowercase();
    m.docs
        .iter()
        .filter(|d| {
            q.is_empty()
                || d.path.to_ascii_lowercase().contains(&q)
                || d.title
                    .as_deref()
                    .is_some_and(|t| t.to_ascii_lowercase().contains(&q))
        })
        .collect()
}

pub enum Select<'a> {
    All,
    DocsOnly,
    Paths(&'a [String]),
}

#[derive(Debug, Default)]
pub struct FetchReport {
    pub written: Vec<String>,
    pub unchanged: Vec<String>,
    /// Local files that differ from the shared version and were left alone (no `force`).
    pub conflicts: Vec<String>,
    pub missing: Vec<String>,
    /// Refused as unsafe to write (VCS control files, secret-looking names, case collisions).
    pub refused: Vec<String>,
}

/// Materialize the shared tree (or part of it) under `dest`. Every object is checked against the
/// manifest's sha256; local files that differ are never overwritten without `force`.
pub fn fetch_tree<B: ObjectBackend>(
    b: &B,
    bucket: &str,
    dest: &Path,
    sel: Select<'_>,
    force: bool,
) -> Result<FetchReport> {
    fetch_tree_with(b, bucket, dest, sel, force, &Limits::default())
}

/// [`fetch_tree`] with explicit ceilings (review F11). The manifest is untrusted: a listed file
/// over `max_file`, an object whose stored size is over it, or a selection over `max_total` is an
/// error, checked before the bytes are read.
pub fn fetch_tree_with<B: ObjectBackend>(
    b: &B,
    bucket: &str,
    dest: &Path,
    sel: Select<'_>,
    force: bool,
    lim: &Limits,
) -> Result<FetchReport> {
    let m = read_manifest_with(b, bucket, lim)?
        .ok_or_else(|| S2Error::Invalid("no shared tree in this worktree store".into()))?;
    fetch_tree_from(b, bucket, &m, dest, sel, force, lim)
}

/// A selection of up to this many objects to download is size-checked with one HEAD each; a larger
/// one with a single listing of the object prefix (schema 1 `.wt/tree/`, schema 2 the pool). Either
/// way no object is read before its stored size is known and under the caps.
pub const HEAD_LIST_THRESHOLD: usize = 8;

/// [`fetch_tree_with`] from a manifest the caller already read, so a verb that needs the manifest
/// for more than one step (`sync-docs`, `fetch --bundle-out`) reads it once and uses one snapshot.
pub fn fetch_tree_from<B: ObjectBackend>(
    b: &B,
    bucket: &str,
    m: &TreeManifest,
    dest: &Path,
    sel: Select<'_>,
    force: bool,
    lim: &Limits,
) -> Result<FetchReport> {
    let doc_set: std::collections::HashSet<&str> = m.docs.iter().map(|d| d.path.as_str()).collect();
    let mut rep = FetchReport::default();
    let mut lower = std::collections::HashSet::new();
    let mut todo = Vec::new();
    // A set, not a scan of the selection per file: 100 000 files by 10 000 selected paths is quadratic.
    let picked: std::collections::HashSet<&str> = match &sel {
        Select::Paths(ps) => ps.iter().map(|p| p.as_str()).collect(),
        _ => Default::default(),
    };
    for f in &m.files {
        if is_dangerous_path(&f.path)
            || is_secret_path(&f.path)
            || !lower.insert(f.path.to_ascii_lowercase())
        {
            rep.refused.push(f.path.clone());
            continue;
        }
        let wanted = match &sel {
            Select::All => true,
            Select::DocsOnly => doc_set.contains(f.path.as_str()),
            Select::Paths(_) => picked.contains(f.path.as_str()),
        };
        if !wanted {
            continue;
        }
        let target = safe_join(dest, &f.path)?;
        if let Ok(local) = std::fs::read(&target) {
            if sha256_hex(&local) == f.sha256 {
                rep.unchanged.push(f.path.clone());
                continue;
            }
            if !force {
                rep.conflicts.push(f.path.clone());
                continue;
            }
        }
        todo.push((f, target));
    }
    // Presence and size of what is still to download: one listing for a big selection, one HEAD
    // per object for a small one. An object absent here, or by the time it is read, is `missing`.
    let prefix = if m.is_pooled() { OBJ_PREFIX } else { TREE_PREFIX };
    let listed: Option<std::collections::HashMap<String, u64>> =
        (todo.len() > HEAD_LIST_THRESHOLD).then(|| b.list(bucket, prefix)).transpose()?.map(|l| l.into_iter().map(|o| (o.key, o.size)).collect());
    let mut total = 0u64;
    for (f, target) in todo {
        let key = m.object_key(f);
        let stored = match &listed {
            Some(l) => l.get(&key).copied(),
            None => b.head(bucket, &key)?.map(|h| h.size),
        };
        let Some(stored) = stored else {
            rep.missing.push(f.path.clone());
            continue;
        };
        if f.size > lim.max_file || stored > lim.max_file {
            return Err(S2Error::Invalid(format!("{} is over the {} byte per-file limit", f.path, lim.max_file)));
        }
        total += stored;
        if total > lim.max_total {
            return Err(S2Error::Invalid(format!("selection is over the {} byte total limit", lim.max_total)));
        }
        let Some(data) = b.get(bucket, &key)? else {
            rep.missing.push(f.path.clone());
            continue;
        };
        // The object may have been replaced since it was sized: the body is held to the cap too.
        if data.len() as u64 > lim.max_file {
            return Err(S2Error::Invalid(format!("{} is over the {} byte per-file limit", f.path, lim.max_file)));
        }
        if sha256_hex(&data) != f.sha256 {
            return Err(S2Error::Backend(format!(
                "checksum mismatch for {}",
                f.path
            )));
        }
        write_new(dest, &target, &data)?;
        rep.written.push(f.path.clone());
    }
    Ok(rep)
}

/// Write `data` to `target` through a fresh, exclusively-created temp name in the same directory
/// (never an existing file), then rename it into place.
fn write_new(dest: &Path, target: &Path, data: &[u8]) -> Result<()> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let dir = target.parent().unwrap_or(dest);
    let mut n = 0u32;
    let (tmp, mut file) = loop {
        let t = dir.join(format!(".wt-tmp-{}-{n}", std::process::id()));
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&t) {
            Ok(h) => break (t, h),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && n < 100 => n += 1,
            Err(e) => return Err(e.into()),
        }
    };
    std::io::Write::write_all(&mut file, data)?;
    drop(file);
    std::fs::rename(&tmp, target)?;
    Ok(())
}

/// Download the unmerged-commits bundle to `out`; returns the head commit it carries.
pub fn fetch_bundle<B: ObjectBackend>(b: &B, bucket: &str, out: &Path) -> Result<String> {
    fetch_bundle_with(b, bucket, out, &Limits::default())
}

/// [`fetch_bundle`] under explicit `lim` (the manifest file-count cap and the bundle cap).
pub fn fetch_bundle_with<B: ObjectBackend>(b: &B, bucket: &str, out: &Path, lim: &Limits) -> Result<String> {
    let m = read_manifest_with(b, bucket, lim)?
        .ok_or_else(|| S2Error::Invalid("no shared tree in this worktree store".into()))?;
    fetch_bundle_from_with(b, bucket, &m, out, lim)
}

/// [`fetch_bundle`] from a manifest the caller already read.
pub fn fetch_bundle_from<B: ObjectBackend>(b: &B, bucket: &str, m: &TreeManifest, out: &Path) -> Result<String> {
    fetch_bundle_from_with(b, bucket, m, out, &Limits::default())
}

/// [`fetch_bundle_from`] with the bundle cap taken from `lim.max_bundle`.
pub fn fetch_bundle_from_with<B: ObjectBackend>(b: &B, bucket: &str, m: &TreeManifest, out: &Path, lim: &Limits) -> Result<String> {
    if !m.has_bundle {
        return Err(S2Error::Invalid(
            "this tree was published without a git bundle".into(),
        ));
    }
    // Schema 2: the pool object named by sha, left in place for the grace by any later publish.
    let key = m
        .bundle_object()
        .ok_or_else(|| S2Error::Invalid("manifest says bundle but names no bundle object".into()))?;
    let h = b
        .head(bucket, &key)?
        .ok_or_else(|| S2Error::Backend("manifest says bundle, object missing".into()))?;
    if h.size > lim.max_bundle {
        return Err(S2Error::Invalid(format!("bundle is {} bytes, over the {} limit", h.size, lim.max_bundle)));
    }
    let data = b
        .get(bucket, &key)?
        .ok_or_else(|| S2Error::Backend("manifest says bundle, object missing".into()))?;
    if let Some(want) = &m.bundle_sha256
        && sha256_hex(&data) != *want
    {
        return Err(S2Error::Backend("bundle does not match the checksum in the manifest".into()));
    }
    // Never write through a symlink: the final component is refused, and the bytes go to a fresh
    // exclusive temp file that is renamed over `out`, so a link or hard link at `out` is replaced.
    if std::fs::symlink_metadata(out).is_ok_and(|md| md.file_type().is_symlink()) {
        return Err(S2Error::Invalid(format!("{} is a symlink; refusing to write the bundle", out.display())));
    }
    let dir = out.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".wt-bundle-{}.tmp", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp)?;
    std::io::Write::write_all(&mut f, &data)?;
    drop(f);
    std::fs::rename(&tmp, out).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })?;
    Ok(m.head_commit.clone())
}
