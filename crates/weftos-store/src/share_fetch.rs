//! Read side of worktree sharing: find a shared worktree and its docs from any node, materialize
//! the tree locally, and fetch the git bundle so a different agent can merge. See `share.rs`.

use crate::overlay::{ObjectBackend, Result, S2Error, validate_key};
use crate::share::*;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Registry prefix in the *project* bucket. Written by the daemon only (it holds the project
/// grant); RustFS IAM refuses it to ordinary worktree principals, keeping "writes stay in the
/// worktree bucket" (ADR-126 §4) true for agents.
pub const REGISTRY_PREFIX: &str = "worktrees/";

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
    Ok(Some(RegistryEntry {
        slug: slug.into(),
        bucket: bucket.into(),
        branch: m.branch,
        head_commit: m.head_commit,
        state: h.state,
        writer: m.writer,
        merger: h.merger,
        doc_count: m.docs.len(),
        file_count: m.files.len(),
        updated_ms: h.updated_ms.max(m.published_ms),
    }))
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
    Ok(())
}

/// Every shared worktree of a project, newest first. Readable from any node with project read.
pub fn list_shared<B: ObjectBackend>(b: &B, project_bucket: &str) -> Result<Vec<RegistryEntry>> {
    let mut v = Vec::new();
    for o in b.list(project_bucket, REGISTRY_PREFIX)? {
        if let Some(d) = b.get(project_bucket, &o.key)? {
            v.push(serde_json::from_slice::<RegistryEntry>(&d)?);
        }
    }
    v.sort_by(|a, c| {
        c.updated_ms
            .cmp(&a.updated_ms)
            .then_with(|| a.slug.cmp(&c.slug))
    });
    Ok(v)
}

/// The bucket worktree `slug` of the project whose bucket is `project_bucket` must have. `None`
/// when `project_bucket` is not a real project bucket (`p-<ULID>`), as in fixtures that use
/// made-up names; the cross-check is then skipped.
pub fn expected_worktree_bucket(project_bucket: &str, slug: &str) -> Option<String> {
    let ulid = project_bucket.strip_prefix("p-").filter(|u| u.len() == 26)?;
    crate::StoreName::worktree("x", ulid, slug).ok().map(|n| n.bucket())
}

/// Like [`list_shared`] but a corrupt or unknown-schema entry is reported, not fatal: returns the
/// readable entries and the slugs of the unreadable ones.
pub fn list_shared_lenient<B: ObjectBackend>(
    b: &B,
    project_bucket: &str,
) -> Result<(Vec<RegistryEntry>, Vec<String>)> {
    let (mut v, mut bad) = (Vec::new(), Vec::new());
    for o in b.list(project_bucket, REGISTRY_PREFIX)? {
        let slug = o
            .key
            .trim_start_matches(REGISTRY_PREFIX)
            .trim_end_matches(".json")
            .to_string();
        match b
            .get(project_bucket, &o.key)?
            .map(|d| serde_json::from_slice::<RegistryEntry>(&d))
        {
            // A registry entry is a pointer, not authority (review F3): it must name the bucket
            // this project's worktree of that slug has, and the slug its own key carries.
            Some(Ok(e)) if e.slug == slug && expected_worktree_bucket(project_bucket, &e.slug).is_none_or(|x| x == e.bucket) => {
                v.push(e)
            }
            _ => bad.push(slug),
        }
    }
    v.sort_by(|a, c| {
        c.updated_ms
            .cmp(&a.updated_ms)
            .then_with(|| a.slug.cmp(&c.slug))
    });
    bad.sort();
    Ok((v, bad))
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
    let m = read_manifest(b, bucket)?
        .ok_or_else(|| S2Error::Invalid("no shared tree in this worktree store".into()))?;
    let doc_set: std::collections::HashSet<&str> = m.docs.iter().map(|d| d.path.as_str()).collect();
    let mut rep = FetchReport::default();
    let mut lower = std::collections::HashSet::new();
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
            Select::Paths(ps) => ps.iter().any(|p| p == &f.path),
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
        let Some(data) = b.get(bucket, &format!("{TREE_PREFIX}{}", f.path))? else {
            rep.missing.push(f.path.clone());
            continue;
        };
        if sha256_hex(&data) != f.sha256 {
            return Err(S2Error::Backend(format!(
                "checksum mismatch for {}",
                f.path
            )));
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Fresh, exclusively-created temp name in the same directory: never an existing file.
        let dir = target.parent().unwrap_or(dest);
        let mut n = 0u32;
        let (tmp, mut file) = loop {
            let t = dir.join(format!(".wt-tmp-{}-{n}", std::process::id()));
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&t)
            {
                Ok(h) => break (t, h),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && n < 100 => n += 1,
                Err(e) => return Err(e.into()),
            }
        };
        std::io::Write::write_all(&mut file, &data)?;
        drop(file);
        std::fs::rename(&tmp, &target)?;
        rep.written.push(f.path.clone());
    }
    Ok(rep)
}

/// Download the unmerged-commits bundle to `out`; returns the head commit it carries.
pub fn fetch_bundle<B: ObjectBackend>(b: &B, bucket: &str, out: &Path) -> Result<String> {
    let m = read_manifest(b, bucket)?
        .ok_or_else(|| S2Error::Invalid("no shared tree in this worktree store".into()))?;
    if !m.has_bundle {
        return Err(S2Error::Invalid(
            "this tree was published without a git bundle".into(),
        ));
    }
    let data = b
        .get(bucket, BUNDLE_KEY)?
        .ok_or_else(|| S2Error::Backend("manifest says bundle, object missing".into()))?;
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
    Ok(m.head_commit)
}
