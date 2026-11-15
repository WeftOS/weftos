//! Follower helpers that read worktree contents: registry-entry verification, docs search and the
//! read-only docs mirror. Split out of `follower` (re-exported there, so paths are unchanged).

use crate::overlay::{ObjectBackend, Result, S2Error, validate_key};
use crate::promote::read_meta;
use crate::share::{State, read_manifest, safe_join};
use crate::share_fetch::{FetchReport, REGISTRY_PREFIX, RegistryEntry, Select, fetch_tree};
use std::path::Path;

/// A registry entry is a pointer, not authority: before reading a bucket on its say-so, check the
/// slug is well-formed and the bucket's own `.wt/meta.json` names the same slug. This stops an
/// entry from redirecting a follower (which holds its own read grants) at someone else's bucket.
pub fn verify_entry<B: ObjectBackend>(b: &B, e: &RegistryEntry) -> Result<()> {
    let ok_slug = !e.slug.is_empty()
        && e.slug.len() <= 48
        && e.slug
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        && !e.slug.starts_with('-');
    if !ok_slug {
        return Err(S2Error::Invalid(format!(
            "registry slug {:?} is not a valid slug",
            e.slug
        )));
    }
    match read_meta(b, &e.bucket)? {
        Some(m) if m.slug == e.slug && bucket_project(&e.bucket).is_none_or(|p| p.eq_ignore_ascii_case(&m.project)) => Ok(()),
        Some(m) if m.slug == e.slug => Err(S2Error::Invalid(format!(
            "registry entry {} points at a bucket whose metadata names project {}",
            e.slug, m.project
        ))),
        Some(m) => Err(S2Error::Invalid(format!(
            "registry entry {} points at bucket of worktree {}",
            e.slug, m.slug
        ))),
        None => Err(S2Error::Invalid(format!(
            "bucket {} has no worktree metadata; refusing to follow {}",
            e.bucket, e.slug
        ))),
    }
}

/// The project ULID in a worktree bucket name `w-<ulid>-<h12>`, if it has that shape.
fn bucket_project(bucket: &str) -> Option<&str> {
    let rest = bucket.strip_prefix("w-")?;
    (rest.len() > 27 && rest.as_bytes()[26] == b'-').then(|| &rest[..26])
}

/// Daemon-side: drop a registry entry once its worktree bucket is promoted and deleted.
pub fn remove_registry_entry<B: ObjectBackend>(
    b: &B,
    project_bucket: &str,
    slug: &str,
) -> Result<()> {
    validate_key(slug)?;
    b.delete(project_bucket, &format!("{REGISTRY_PREFIX}{slug}.json"))?;
    let _ = crate::share_fetch::write_digest(b, project_bucket); // a hint
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocHit {
    pub slug: String,
    pub path: String,
    pub title: Option<String>,
    pub state: State,
}

#[derive(Debug, Default)]
pub struct SearchResult {
    pub hits: Vec<DocHit>,
    /// Worktrees whose manifest could not be read; the search is partial for these.
    pub unreadable: Vec<String>,
}

/// Search docs and planning (path or title, case-insensitive) across unmerged worktrees.
/// Merged and archived ones are skipped. Hits are ordered by slug, then path.
pub fn search_docs<B: ObjectBackend>(
    b: &B,
    entries: &[RegistryEntry],
    query: &str,
) -> SearchResult {
    let mut res = SearchResult::default();
    for e in entries
        .iter()
        .filter(|e| !matches!(e.state, State::Merged | State::Archived))
    {
        match verify_entry(b, e).and_then(|_| read_manifest(b, &e.bucket)) {
            Ok(Some(m)) => {
                for d in crate::share_fetch::find_docs(&m, query) {
                    res.hits.push(DocHit {
                        slug: e.slug.clone(),
                        path: d.path.clone(),
                        title: d.title.clone(),
                        state: e.state,
                    });
                }
            }
            _ => res.unreadable.push(e.slug.clone()),
        }
    }
    res.hits
        .sort_by(|a, c| a.slug.cmp(&c.slug).then_with(|| a.path.cmp(&c.path)));
    res
}

const INDEX: &str = ".wt-index.json";

/// Mirror one worktree's docs under `cache/<slug>/`, read-only, removing docs that left the tree.
/// The cache is the follower's own: local edits in it are overwritten.
pub fn sync_docs<B: ObjectBackend>(b: &B, e: &RegistryEntry, cache: &Path) -> Result<FetchReport> {
    verify_entry(b, e)?;
    // safe_join refuses a symlinked `cache/<slug>` as well as traversal.
    let dest = safe_join(cache, &e.slug)?;
    std::fs::create_dir_all(&dest)?;
    let m = read_manifest(b, &e.bucket)?
        .ok_or_else(|| S2Error::Invalid("worktree has no published tree".into()))?;
    // The manifest is untrusted: a doc that names the follower's own control files, a dangerous
    // name or a path that leaves the cache is neither fetched nor tracked in the index.
    let want: std::collections::BTreeSet<String> = m
        .docs
        .iter()
        .map(|d| d.path.clone())
        .filter(|p| validate_key(p).is_ok() && !crate::share::is_dangerous_path(p) && !crate::share::is_secret_path(p))
        .collect();
    // The index is the follower's own fixed name (`safe_join` refuses `.wt-` names on purpose, so
    // a manifest cannot reach it). A symlink put there is replaced, not written through.
    let idx = dest.join(INDEX);
    if std::fs::symlink_metadata(&idx).is_ok_and(|m| m.file_type().is_symlink()) {
        std::fs::remove_file(&idx)?;
    }
    let old: Vec<String> = std::fs::read(&idx)
        .ok()
        .and_then(|d| serde_json::from_slice(&d).ok())
        .unwrap_or_default();
    // Index first, as old + new, so a failed fetch below still leaves every file it may have
    // written (or that was there) tracked and prunable on the next run.
    let mut tracked: std::collections::BTreeSet<String> = old.iter().cloned().collect();
    tracked.extend(want.iter().cloned());
    std::fs::write(&idx, serde_json::to_vec(&tracked)?)?;
    for p in old.iter().filter(|p| !want.contains(p.as_str())) {
        if let Ok(f) = safe_join(&dest, p) {
            let _ = std::fs::remove_file(f);
        }
    }
    let rep = fetch_tree(b, &e.bucket, &dest, Select::DocsOnly, true)?;
    let keep: Vec<&String> = want.iter().collect();
    std::fs::write(&idx, serde_json::to_vec(&keep)?)?;
    #[cfg(unix)]
    for p in &rep.written {
        let f = safe_join(&dest, p)?;
        let mut perm = std::fs::metadata(&f)?.permissions();
        perm.set_readonly(true);
        std::fs::set_permissions(&f, perm)?;
    }
    Ok(rep)
}
