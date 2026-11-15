//! Publishing half of `share`: the manifest reader and `publish_tree`, with the file-read helpers
//! they use. A child of `share` so it shares that module's private helpers; `share` re-exports
//! every item here, so paths are unchanged.

use super::*;
use crate::overlay::ObjectMeta;
use crate::share_fetch::RegistryEntry;

/// Read a regular file for publishing from one open handle, never following a symlink and never
/// reading more than `max` bytes (review F14). The checks made on the path before this are only
/// for choosing a skip reason; this is the one that holds. The portable std has no `O_NOFOLLOW`,
/// and the store may not take a `libc` dependency, so a symlink is caught the other way round:
/// open, then require that the path still names a regular file that is the very file the handle
/// is on (same device and inode on unix). A swap before the open shows as a symlink or another
/// inode, a swap after it cannot change what the handle reads. `Err(None)` is a real I/O error.
pub(crate) fn read_regular(path: &Path, max: u64) -> std::result::Result<Vec<u8>, Option<Skip>> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(Some(Skip::Missing)),
        Err(_) => return Err(None),
    };
    read_open_regular(file, path, max)
}

pub(super) fn read_open_regular(file: std::fs::File, path: &Path, max: u64) -> std::result::Result<Vec<u8>, Option<Skip>> {
    use std::io::Read;
    let on_handle = file.metadata().map_err(|_| None)?;
    let on_path = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return Err(Some(Skip::Changed)),
    };
    if on_path.file_type().is_symlink() {
        return Err(Some(Skip::Symlink));
    }
    if !on_handle.is_file() || !on_path.is_file() {
        return Err(Some(Skip::NotAFile));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if (on_handle.dev(), on_handle.ino()) != (on_path.dev(), on_path.ino()) {
            return Err(Some(Skip::Changed));
        }
    }
    if on_handle.len() > max {
        return Err(Some(Skip::TooLarge));
    }
    // One byte past the cap tells a file that grew after the stat from one that fits.
    let mut data = Vec::with_capacity(on_handle.len().min(max) as usize);
    file.take(max + 1).read_to_end(&mut data).map_err(|_| None)?;
    if data.len() as u64 > max {
        return Err(Some(Skip::TooLarge));
    }
    Ok(data)
}

/// PEM/OpenSSH private key material, whatever the file is called.
pub(crate) fn looks_like_private_key(data: &[u8]) -> bool {
    const NEEDLE: &[u8] = b"PRIVATE KEY-----";
    data.len() >= NEEDLE.len() && data.windows(NEEDLE.len()).any(|w| w == NEEDLE)
}

pub(crate) fn is_doc_path(path: &str) -> bool {
    path.starts_with("docs/")
        || path.starts_with(".planning/")
        || path.starts_with("planning/")
        || path.to_ascii_lowercase().ends_with(".md")
}

fn md_title(path: &str, data: &[u8]) -> Option<String> {
    if !path.to_ascii_lowercase().ends_with(".md") {
        return None;
    }
    let head = &data[..data.len().min(8192)];
    String::from_utf8_lossy(head).lines().find_map(|l| {
        l.strip_prefix("# ")
            .map(|t| t.trim().chars().take(160).collect())
    })
}

/// Largest manifest object a reader will load (review F11): a writer with direct S3 access could
/// otherwise make every follower read an object of any size into memory.
pub const MAX_MANIFEST_BYTES: u64 = 32 << 20;

pub fn read_manifest<B: ObjectBackend>(b: &B, bucket: &str) -> Result<Option<TreeManifest>> {
    let Some(h) = b.head(bucket, MANIFEST_KEY)? else { return Ok(None) };
    if h.size > MAX_MANIFEST_BYTES {
        return Err(S2Error::Invalid(format!("manifest is {} bytes, over the {MAX_MANIFEST_BYTES} limit", h.size)));
    }
    match b.get(bucket, MANIFEST_KEY)? {
        Some(d) => Ok(Some(parse_manifest(&d)?)),
        None => Ok(None),
    }
}

/// Refuse a manifest written by a newer schema than this build understands: its fields may parse
/// yet mean something else. The current and older schemas are accepted; a manifest with no readable
/// `schema` number is left to the typed parse, which rejects it.
fn check_manifest_schema(d: &[u8]) -> Result<()> {
    #[derive(serde::Deserialize)]
    struct Peek {
        schema: u32,
    }
    if let Ok(Peek { schema }) = serde_json::from_slice::<Peek>(d)
        && schema > SCHEMA
    {
        return Err(S2Error::Invalid(format!(
            "manifest is schema {schema}, newer than the schema {SCHEMA} this build reads; upgrade to read it"
        )));
    }
    Ok(())
}

/// Parse manifest bytes and apply the file-count limit (shared with the follower manifest cache).
pub(crate) fn parse_manifest(d: &[u8]) -> Result<TreeManifest> {
    check_manifest_schema(d)?;
    let m: TreeManifest = serde_json::from_slice(d)?;
    let lim = Limits::default();
    if m.files.len() > lim.max_files || m.docs.len() > lim.max_files {
        return Err(S2Error::Invalid(format!("manifest lists more than {} files", lim.max_files)));
    }
    Ok(m)
}

/// What a publish produced, for a caller that goes on to derive the registry entry (WO-069 X3).
#[derive(Debug)]
pub struct Published {
    pub report: PublishReport,
    /// The manifest this publish just wrote.
    pub manifest: TreeManifest,
    /// The handoff record, only when THIS publish created it (the create-only put won), so it is
    /// exactly what is stored. `None` when a record already existed (it may have changed since it
    /// was read) or the create lost a race: the caller reads it fresh.
    pub handoff_created: Option<Handoff>,
}

/// The registry entry for a worktree from a manifest and handoff already in hand. Same mapping as
/// `summarize` (`share_fetch.rs`), which reads the two objects first; a test pins them equal.
pub fn summarize_with(slug: &str, bucket: &str, m: TreeManifest, h: Handoff) -> RegistryEntry {
    RegistryEntry {
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
        orphan: None,
    }
}

/// Publish (or refresh) the worktree tree into its bucket. Re-runs upload only changed files and
/// remove objects that left the tree. The manifest is written last, so a reader that sees it sees
/// a complete tree.
pub fn publish_tree<B: ObjectBackend>(
    b: &B,
    bucket: &str,
    src: &TreeSource<'_>,
    limits: &Limits,
    now_ms: u64,
) -> Result<PublishReport> {
    Ok(publish_tree_listed(b, bucket, src, limits, now_ms, None)?.report)
}

/// `publish_tree` with the bucket listing supplied by the caller. `listing` is the WHOLE bucket
/// (`list(bucket, "")`), taken before any PUT; `None` lists it here, one call, as before (the
/// tree-prefix listing it replaces was one call too). The tree objects and whether a bundle object
/// exists are read from it, so no second listing and no blind bundle DELETE.
pub fn publish_tree_listed<B: ObjectBackend>(
    b: &B,
    bucket: &str,
    src: &TreeSource<'_>,
    limits: &Limits,
    now_ms: u64,
    listing: Option<Vec<ObjectMeta>>,
) -> Result<Published> {
    let existing_handoff = read_handoff(b, bucket)?;
    let handoff_exists = existing_handoff.is_some();
    if let Some(h) = existing_handoff {
        if matches!(h.state, State::Merged | State::Archived) {
            return Err(S2Error::Invalid(format!(
                "worktree is already {:?}; refusing to republish",
                h.state
            )));
        }
        guard_live_claim(&h, &src.writer, now_ms)?;
    }
    if let Some(bp) = src.bundle {
        let len = std::fs::metadata(bp)?.len();
        if len > limits.max_bundle {
            return Err(S2Error::Invalid(format!(
                "bundle is {len} bytes, over the {} limit",
                limits.max_bundle
            )));
        }
    }
    let prev = read_manifest(b, bucket)?;
    let prev_sha: std::collections::HashMap<&str, &str> = prev
        .as_ref()
        .map(|m| {
            m.files
                .iter()
                .map(|f| (f.path.as_str(), f.sha256.as_str()))
                .collect()
        })
        .unwrap_or_default();
    // One listing of the tree prefix per publish, instead of one HEAD per file: a file is unchanged
    // when its sha matches the previous manifest AND its object is listed with the same size. The
    // same listing feeds the stale sweep at the end (objects put by this run are in `kept`).
    let whole = match listing {
        Some(l) => l,
        None => b.list(bucket, "")?,
    };
    let bundle_listed = whole.iter().any(|o| o.key == BUNDLE_KEY);
    let listed: std::collections::HashMap<String, u64> =
        whole.into_iter().filter(|o| o.key.starts_with(TREE_PREFIX)).map(|o| (o.key, o.size)).collect();
    let mut rep = PublishReport::default();
    let (mut files, mut docs, mut total) = (Vec::new(), Vec::new(), 0u64);
    let mut seen = std::collections::BTreeSet::new();

    for p in src.paths {
        if validate_key(p).is_err() || p.starts_with(".wt/") || !seen.insert(p.clone()) {
            rep.skipped.push((p.clone(), Skip::BadPath));
            continue;
        }
        if is_secret_path(p) {
            rep.skipped.push((p.clone(), Skip::Secret));
            continue;
        }
        if is_dangerous_path(p) {
            rep.skipped.push((p.clone(), Skip::BadPath));
            continue;
        }
        let full = match safe_join(src.root, p) {
            Ok(f) => f,
            Err(_) => {
                rep.skipped.push((p.clone(), Skip::Symlink));
                continue;
            }
        };
        let md = match std::fs::symlink_metadata(&full) {
            Ok(m) => m,
            Err(_) => {
                rep.skipped.push((p.clone(), Skip::Missing));
                continue;
            }
        };
        if md.file_type().is_symlink() {
            rep.skipped.push((p.clone(), Skip::Symlink));
            continue;
        }
        if !md.is_file() {
            rep.skipped.push((p.clone(), Skip::NotAFile));
            continue;
        }
        if md.len() > limits.max_file {
            rep.skipped.push((p.clone(), Skip::TooLarge));
            continue;
        }
        if files.len() >= limits.max_files || total + md.len() > limits.max_total {
            rep.skipped.push((p.clone(), Skip::OverBudget));
            continue;
        }
        let data = match read_regular(&full, limits.max_file) {
            Ok(d) => d,
            Err(Some(skip)) => {
                rep.skipped.push((p.clone(), skip));
                continue;
            }
            Err(None) => return Err(S2Error::Backend(format!("cannot read {p}"))),
        };
        if total + data.len() as u64 > limits.max_total {
            rep.skipped.push((p.clone(), Skip::OverBudget));
            continue;
        }
        if looks_like_private_key(&data) {
            rep.skipped.push((p.clone(), Skip::Secret));
            continue;
        }
        let sha = sha256_hex(&data);
        let key = format!("{TREE_PREFIX}{p}");
        if prev_sha.get(p.as_str()) == Some(&sha.as_str()) && listed.get(&key) == Some(&(data.len() as u64)) {
            rep.unchanged += 1;
        } else {
            b.put(bucket, &key, &data)?;
            rep.uploaded += 1;
        }
        total += data.len() as u64;
        if is_doc_path(p) {
            docs.push(DocEntry {
                path: p.clone(),
                title: md_title(p, &data),
            });
        }
        files.push(FileEntry {
            path: p.clone(),
            size: data.len() as u64,
            sha256: sha,
        });
    }

    let mut has_bundle = prev
        .as_ref()
        .map(|m| m.has_bundle && m.head_commit == src.head_commit)
        .unwrap_or(false);
    let mut bundle_sha256 = if has_bundle { prev.as_ref().and_then(|m| m.bundle_sha256.clone()) } else { None };
    if let Some(bp) = src.bundle {
        if !has_bundle {
            let md = std::fs::metadata(bp)?;
            if md.len() > limits.max_bundle {
                return Err(S2Error::Invalid(format!(
                    "bundle is {} bytes, over the {} limit",
                    md.len(),
                    limits.max_bundle
                )));
            }
            let bytes = std::fs::read(bp)?;
            bundle_sha256 = Some(sha256_hex(&bytes));
            b.put(bucket, BUNDLE_KEY, &bytes)?;
            rep.bundle_uploaded = true;
            has_bundle = true;
        }
    } else if !has_bundle && bundle_listed {
        // Only when the listing shows a bundle object: no blind DELETE, but a leftover that no
        // manifest names (a crash between the bundle PUT and the manifest PUT) is still removed.
        b.delete(bucket, BUNDLE_KEY)?;
    }

    let uncommitted = src
        .uncommitted
        .iter()
        .filter(|u| files.iter().any(|f| &f.path == *u))
        .cloned()
        .collect();
    let manifest = TreeManifest {
        schema: SCHEMA,
        branch: src.branch.into(),
        base_commit: src.base_commit.into(),
        head_commit: src.head_commit.into(),
        files,
        uncommitted,
        changes: src.changes.to_vec(),
        docs,
        has_bundle,
        bundle_sha256,
        writer: src.writer.clone(),
        published_ms: now_ms,
    };
    b.put(bucket, MANIFEST_KEY, &serde_json::to_vec_pretty(&manifest)?)?;
    // Stale objects go after the manifest no longer lists them, so a reader never sees a listed file
    // missing. Found by listing, not from the previous manifest: a publish killed after its manifest
    // write but before this sweep leaves objects that no manifest names, and only a listing finds them.
    let kept: std::collections::HashSet<&str> =
        manifest.files.iter().map(|f| f.path.as_str()).collect();
    for key in listed.keys() {
        if !kept.contains(&key[TREE_PREFIX.len()..]) {
            b.delete(bucket, key)?;
            rep.removed += 1;
        }
    }

    // Create the record only if none exists. Conditional, so a handoff or claim written between a
    // check and this write is never overwritten with a fresh `Open`; losing the race is fine.
    // Not attempted when the record was just read: that create-only put could only lose.
    let mut handoff_created = None;
    if !handoff_exists {
        let fresh = Handoff {
            state: State::Open,
            note: String::new(),
            writer: src.writer.clone(),
            merger: None,
            updated_ms: now_ms,
            epoch: 0,
            claimed_head: None,
            claimed_tree: None,
        };
        if write_handoff_if(b, bucket, &fresh, None)? {
            handoff_created = Some(fresh);
        }
    }
    Ok(Published { report: rep, manifest, handoff_created })
}
