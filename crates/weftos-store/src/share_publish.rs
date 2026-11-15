//! Publishing half of `share`: the manifest reader and the file-read helpers `publish_tree`
//! (`share_write.rs`) uses. A child of `share` so it shares that module's private helpers; `share` re-exports
//! every item here, so paths are unchanged.

use super::*;
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

pub(super) fn md_title(path: &str, data: &[u8]) -> Option<String> {
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
/// otherwise make every follower read an object of any size into memory. Sized against
/// `Limits::DEFAULT_MAX_FILES`: a pretty-printed manifest is 157 to 175 bytes a file (see the
/// `manifest_cap_*` test), so 100 000 files is 15.7 to 17.6 MB and the cap (33.5 MB) is hit at about
/// 190 000 to 213 000 files; the publish itself refuses a manifest over this cap rather than write
/// an unreadable tree.
pub const MAX_MANIFEST_BYTES: u64 = 32 << 20;

pub fn read_manifest<B: ObjectBackend>(b: &B, bucket: &str) -> Result<Option<TreeManifest>> {
    read_manifest_with(b, bucket, &Limits::default())
}

/// [`read_manifest`] under explicit `limits`: the file-count cap is `limits.max_files`, the same
/// value the writer of that tree used.
pub fn read_manifest_with<B: ObjectBackend>(b: &B, bucket: &str, limits: &Limits) -> Result<Option<TreeManifest>> {
    Ok(read_manifest_bytes_with(b, bucket, limits)?.map(|(m, _)| m))
}

/// [`read_manifest`] with the bytes it parsed (a publish announcement names them by digest). Same
/// calls: HEAD (size check), GET.
pub(crate) fn read_manifest_bytes<B: ObjectBackend>(b: &B, bucket: &str) -> Result<Option<(TreeManifest, Vec<u8>)>> {
    read_manifest_bytes_with(b, bucket, &Limits::default())
}

/// [`read_manifest_bytes`] under explicit `limits`.
pub(crate) fn read_manifest_bytes_with<B: ObjectBackend>(
    b: &B,
    bucket: &str,
    limits: &Limits,
) -> Result<Option<(TreeManifest, Vec<u8>)>> {
    Ok(read_manifest_versioned(b, bucket, limits)?.map(|(m, d, _)| (m, d)))
}

/// [`read_manifest_bytes_with`] with the etag of the HEAD that sized it, for a conditional write
/// over exactly this version (a publish's manifest `put_cas`). Same calls: HEAD, GET.
pub(crate) fn read_manifest_versioned<B: ObjectBackend>(
    b: &B,
    bucket: &str,
    limits: &Limits,
) -> Result<Option<(TreeManifest, Vec<u8>, String)>> {
    let Some(h) = b.head(bucket, MANIFEST_KEY)? else { return Ok(None) };
    if h.size > MAX_MANIFEST_BYTES {
        return Err(S2Error::Invalid(format!("manifest is {} bytes, over the {MAX_MANIFEST_BYTES} limit", h.size)));
    }
    match b.get(bucket, MANIFEST_KEY)? {
        Some(d) => Ok(Some((parse_manifest_with(&d, limits)?, d, h.etag))),
        None => Ok(None),
    }
}

/// Refuse a manifest written by a newer schema than this build understands: its fields may parse
/// yet mean something else. Schemas 1 and 2 are accepted (3 and later refused); a manifest with no
/// readable `schema` number is left to the typed parse, which rejects it.
fn check_manifest_schema(d: &[u8]) -> Result<()> {
    #[derive(serde::Deserialize)]
    struct Peek {
        schema: u32,
    }
    if let Ok(Peek { schema }) = serde_json::from_slice::<Peek>(d)
        && schema > READ_SCHEMA_MAX
    {
        return Err(S2Error::Invalid(format!(
            "manifest is schema {schema}, newer than the schema {READ_SCHEMA_MAX} this build reads; upgrade to read it"
        )));
    }
    Ok(())
}

/// Parse manifest bytes and apply the file-count limit (shared with the follower manifest cache).
pub(crate) fn parse_manifest(d: &[u8]) -> Result<TreeManifest> {
    parse_manifest_with(d, &Limits::default())
}

/// [`parse_manifest`] with the file-count cap taken from `limits` (the writer's own value).
pub fn parse_manifest_with(d: &[u8], limits: &Limits) -> Result<TreeManifest> {
    check_manifest_schema(d)?;
    let m: TreeManifest = serde_json::from_slice(d)?;
    if m.files.len() > limits.max_files || m.docs.len() > limits.max_files {
        return Err(S2Error::Invalid(format!("manifest lists more than the {} file limit", limits.max_files)));
    }
    if m.retiring.len() > max_retiring(limits) {
        return Err(S2Error::Invalid(format!("manifest retires more than the {} object limit", max_retiring(limits))));
    }
    Ok(m)
}

/// Most pool objects one manifest may hold back from the sweep: twice the file ceiling.
pub fn max_retiring(limits: &Limits) -> usize {
    limits.max_files.saturating_mul(2)
}

/// Never write a manifest no reader would load: `read_manifest_with` refuses one over
/// [`MAX_MANIFEST_BYTES`].
pub(crate) fn check_manifest_len(len: u64, files: usize) -> Result<()> {
    if len > MAX_MANIFEST_BYTES {
        return Err(S2Error::Invalid(format!(
            "manifest would be {len} bytes for {files} files, over the {MAX_MANIFEST_BYTES} byte limit; publish fewer files"
        )));
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_manifest_over_the_reader_cap_is_refused_at_publish_and_the_cap_itself_is_allowed() {
        assert!(check_manifest_len(MAX_MANIFEST_BYTES, 1).is_ok());
        let e = check_manifest_len(MAX_MANIFEST_BYTES + 1, 7).unwrap_err().to_string();
        assert!(e.contains("7 files") && e.contains(&MAX_MANIFEST_BYTES.to_string()), "{e}");
    }
}
