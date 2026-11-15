//! Memory bootstrap: index existing MEMORY.md into a vector store.
//!
//! On first startup (or when the index file is missing / stale), this module
//! reads the workspace's `MEMORY.md`, splits it into sections, embeds each
//! section, and stores the results in an [`RvfStore`] for later semantic
//! search.
//!
//! # Staleness (WEFT-84 / MW-6)
//!
//! The index is rebuilt when:
//! - the index file does not exist, or
//! - `MEMORY.md` has a newer mtime than the index file, or
//! - the caller requests a forced reindex via [`reindex_memory_index`].
//!
//! This module is gated behind the `rvf` feature flag.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use tracing::{debug, info, warn};

use crate::embeddings::Embedder;
use crate::embeddings::rvf_backend::{RvfError, RvfStore};

/// Errors that can occur during memory bootstrap.
#[non_exhaustive]
#[derive(Debug)]
pub enum BootstrapError {
    /// An I/O error occurred reading files.
    Io(std::io::Error),
    /// An embedding error occurred.
    Embedding(crate::embeddings::EmbeddingError),
    /// An RVF store error occurred.
    Store(RvfError),
}

impl std::fmt::Display for BootstrapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BootstrapError::Io(e) => write!(f, "bootstrap I/O error: {e}"),
            BootstrapError::Embedding(e) => write!(f, "bootstrap embedding error: {e}"),
            BootstrapError::Store(e) => write!(f, "bootstrap store error: {e}"),
        }
    }
}

impl std::error::Error for BootstrapError {}

impl From<std::io::Error> for BootstrapError {
    fn from(e: std::io::Error) -> Self {
        BootstrapError::Io(e)
    }
}

impl From<crate::embeddings::EmbeddingError> for BootstrapError {
    fn from(e: crate::embeddings::EmbeddingError) -> Self {
        BootstrapError::Embedding(e)
    }
}

impl From<RvfError> for BootstrapError {
    fn from(e: RvfError) -> Self {
        BootstrapError::Store(e)
    }
}

/// Bootstrap the memory vector index from an existing MEMORY.md file.
///
/// # Algorithm
///
/// 1. If the index exists and is **not** older than `MEMORY.md`, skip
///    (return `0`). Otherwise rebuild.
/// 2. Read the MEMORY.md file at `memory_path`.
/// 3. Split the content into sections by `## ` headers or double-newline
///    paragraphs.
/// 4. Embed each section using the provided [`Embedder`].
/// 5. Store embeddings in an [`RvfStore`] and compact (persist) it.
/// 6. Return the number of sections indexed.
///
/// # Arguments
///
/// * `memory_path` - Path to the MEMORY.md file.
/// * `index_path` - Path where the index file will be stored (e.g. `memory.rvf`).
/// * `embedder` - The embedder to use for generating vectors.
///
/// # Errors
///
/// Returns [`BootstrapError`] if reading the memory file, embedding, or
/// persisting the index fails.
pub async fn bootstrap_memory_index(
    memory_path: &Path,
    index_path: &Path,
    embedder: &dyn Embedder,
) -> Result<usize, BootstrapError> {
    bootstrap_memory_index_inner(memory_path, index_path, embedder, false).await
}

/// Force a full rebuild of the memory vector index from MEMORY.md.
///
/// Always reindexes when `MEMORY.md` exists and is non-empty, even if the
/// index file is present and newer. Use from `weft memory reindex` or any
/// operator path that needs an explicit refresh (WEFT-84).
///
/// Returns the number of sections indexed (same contract as
/// [`bootstrap_memory_index`]).
pub async fn reindex_memory_index(
    memory_path: &Path,
    index_path: &Path,
    embedder: &dyn Embedder,
) -> Result<usize, BootstrapError> {
    bootstrap_memory_index_inner(memory_path, index_path, embedder, true).await
}

/// Return `true` when `MEMORY.md` is newer than the index (or the index is
/// missing / unreadable), so a rebuild is warranted.
///
/// Exposed for tests and for CLI status messaging.
pub fn memory_is_newer_than_index(memory_path: &Path, index_path: &Path) -> bool {
    if !index_path.exists() {
        return memory_path.exists();
    }
    if !memory_path.exists() {
        return false;
    }
    match (file_mtime(memory_path), file_mtime(index_path)) {
        (Some(mem), Some(idx)) => mem > idx,
        // If we cannot read either mtime, prefer reindexing when memory
        // exists so we never leave a silently stale index.
        _ => true,
    }
}

/// Sidecar path used by the real RVF backend (`path` + `.sidecar.json`).
pub fn memory_index_sidecar_path(index_path: &Path) -> PathBuf {
    let mut s = index_path.as_os_str().to_owned();
    s.push(".sidecar.json");
    PathBuf::from(s)
}

/// Remove the index file and any companion sidecar so a rebuild starts clean.
pub fn remove_memory_index(index_path: &Path) {
    if index_path.exists()
        && let Err(e) = std::fs::remove_file(index_path)
    {
        warn!(
            path = %index_path.display(),
            error = %e,
            "failed to remove stale memory index"
        );
    }
    let sidecar = memory_index_sidecar_path(index_path);
    if sidecar.exists()
        && let Err(e) = std::fs::remove_file(&sidecar)
    {
        warn!(
            path = %sidecar.display(),
            error = %e,
            "failed to remove stale memory index sidecar"
        );
    }
}

async fn bootstrap_memory_index_inner(
    memory_path: &Path,
    index_path: &Path,
    embedder: &dyn Embedder,
    force: bool,
) -> Result<usize, BootstrapError> {
    // 1. Skip when index is present, not forced, and MEMORY.md is not newer.
    if index_path.exists() && !force {
        if !memory_is_newer_than_index(memory_path, index_path) {
            info!(
                path = %index_path.display(),
                "memory index already exists and is up to date, skipping bootstrap"
            );
            return Ok(0);
        }
        info!(
            memory = %memory_path.display(),
            index = %index_path.display(),
            "MEMORY.md is newer than index, re-indexing"
        );
    } else if force && index_path.exists() {
        info!(
            path = %index_path.display(),
            "forced reindex requested, rebuilding memory index"
        );
    }

    // Clear any prior index (and sidecar) so create/compact starts fresh.
    // Real RVF also clears inside ensure_ready; this keeps the stub and
    // force-reindex paths consistent and avoids leftover entries.
    if index_path.exists() || memory_index_sidecar_path(index_path).exists() {
        remove_memory_index(index_path);
    }

    // 2. Read MEMORY.md
    if !memory_path.exists() {
        info!(
            path = %memory_path.display(),
            "MEMORY.md not found, skipping bootstrap"
        );
        return Ok(0);
    }

    let content = std::fs::read_to_string(memory_path)?;
    if content.trim().is_empty() {
        info!("MEMORY.md is empty, skipping bootstrap");
        return Ok(0);
    }

    debug!(
        path = %memory_path.display(),
        bytes = content.len(),
        "read MEMORY.md for bootstrap"
    );

    // 3. Split into sections
    let sections = split_into_sections(&content);
    if sections.is_empty() {
        info!("no sections found in MEMORY.md");
        return Ok(0);
    }

    debug!(count = sections.len(), "split MEMORY.md into sections");

    // 4. Embed each section
    let mut store = RvfStore::create(Some(index_path));
    let mut indexed = 0;

    for (i, section) in sections.iter().enumerate() {
        let trimmed = section.trim();
        if trimmed.is_empty() {
            continue;
        }

        match embedder.embed(trimmed).await {
            Ok(embedding) => {
                let id = format!("memory-section-{i}");
                let metadata = serde_json::json!({
                    "source": "MEMORY.md",
                    "section_index": i,
                    "text": trimmed,
                    "char_count": trimmed.len(),
                });
                match store.ingest(id, embedding, metadata) {
                    Ok(()) => indexed += 1,
                    Err(e) => {
                        warn!(
                            section = i,
                            error = %e,
                            "failed to ingest section, skipping"
                        );
                    }
                }
            }
            Err(e) => {
                warn!(
                    section = i,
                    error = %e,
                    "failed to embed section, skipping"
                );
            }
        }
    }

    // 5. Persist the store
    store.compact()?;

    info!(
        indexed,
        total_sections = sections.len(),
        path = %index_path.display(),
        force,
        "memory bootstrap complete"
    );

    Ok(indexed)
}

fn file_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

/// Split markdown content into sections.
///
/// Splitting strategy:
/// 1. First try splitting by `## ` headers (level-2 markdown headings).
/// 2. If that produces fewer than 2 sections, fall back to splitting by
///    double newlines (paragraphs).
/// 3. Filter out empty sections.
fn split_into_sections(content: &str) -> Vec<String> {
    // Try header-based splitting first
    let header_sections: Vec<String> = split_by_headers(content);
    if header_sections.len() >= 2 {
        return header_sections;
    }

    // Fall back to paragraph splitting
    split_by_paragraphs(content)
}

/// Split by `## ` markdown headers, keeping the header with its content.
fn split_by_headers(content: &str) -> Vec<String> {
    let mut sections = Vec::new();
    let mut current = String::new();

    for line in content.lines() {
        if line.starts_with("## ") && !current.trim().is_empty() {
            sections.push(current.trim().to_string());
            current = String::new();
        }
        current.push_str(line);
        current.push('\n');
    }

    if !current.trim().is_empty() {
        sections.push(current.trim().to_string());
    }

    sections.into_iter().filter(|s| !s.is_empty()).collect()
}

/// Split by double newlines (paragraph breaks).
fn split_by_paragraphs(content: &str) -> Vec<String> {
    content
        .split("\n\n")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    // Every test here uses `ApiEmbedder::lexical` (hash vectors) on purpose: they check
    // index bootstrap, staleness and round-tripping, where equal text must map to equal
    // vectors. None asserts semantic recall; do not add ranking assertions here.
    use super::*;
    use crate::embeddings::api_embedder::ApiEmbedder;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::thread;
    use std::time::Duration;

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(label: &str) -> std::path::PathBuf {
        let n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        std::env::temp_dir().join(format!("clawft_bootstrap_test_{label}_{pid}_{n}"))
    }

    #[test]
    fn split_by_headers_basic() {
        let content = "# Title\n\nIntro text.\n\n## Section One\n\nContent one.\n\n## Section Two\n\nContent two.";
        let sections = split_by_headers(content);
        assert_eq!(sections.len(), 3);
        assert!(sections[0].starts_with("# Title"));
        assert!(sections[1].starts_with("## Section One"));
        assert!(sections[2].starts_with("## Section Two"));
    }

    #[test]
    fn split_by_headers_no_headers() {
        let content = "Just some plain text\nwith no headers.";
        let sections = split_by_headers(content);
        assert_eq!(sections.len(), 1);
    }

    #[test]
    fn split_by_paragraphs_basic() {
        let content = "First paragraph.\n\nSecond paragraph.\n\nThird paragraph.";
        let sections = split_by_paragraphs(content);
        assert_eq!(sections.len(), 3);
        assert_eq!(sections[0], "First paragraph.");
        assert_eq!(sections[1], "Second paragraph.");
        assert_eq!(sections[2], "Third paragraph.");
    }

    #[test]
    fn split_into_sections_uses_headers_when_available() {
        let content = "## One\n\nContent.\n\n## Two\n\nMore content.";
        let sections = split_into_sections(content);
        assert_eq!(sections.len(), 2);
    }

    #[test]
    fn split_into_sections_falls_back_to_paragraphs() {
        let content = "Para one.\n\nPara two.\n\nPara three.";
        let sections = split_into_sections(content);
        assert_eq!(sections.len(), 3);
    }

    #[test]
    fn split_into_sections_empty() {
        let sections = split_into_sections("");
        assert!(sections.is_empty());
    }

    #[test]
    fn split_into_sections_whitespace_only() {
        let sections = split_into_sections("   \n\n   \n\n   ");
        assert!(sections.is_empty());
    }

    #[tokio::test]
    async fn bootstrap_creates_index() {
        let dir = temp_dir("creates_index");
        std::fs::create_dir_all(&dir).unwrap();

        let memory_path = dir.join("MEMORY.md");
        let index_path = dir.join("memory.rvf.json");

        std::fs::write(
            &memory_path,
            "## Facts\n\nThe sky is blue.\n\n## Notes\n\nRust is fast.",
        )
        .unwrap();

        let embedder = ApiEmbedder::lexical(64);
        let count = bootstrap_memory_index(&memory_path, &index_path, &embedder)
            .await
            .unwrap();

        assert_eq!(count, 2);
        assert!(index_path.exists());

        // Verify we can reopen the store
        let store = RvfStore::open(&index_path).unwrap();
        assert_eq!(store.len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn bootstrap_skips_existing_index() {
        let dir = temp_dir("skips_existing");
        std::fs::create_dir_all(&dir).unwrap();

        let memory_path = dir.join("MEMORY.md");
        let index_path = dir.join("memory.rvf.json");

        // Write memory first, then a newer index, so mtime check keeps skip.
        std::fs::write(&memory_path, "## Stuff\n\nContent.").unwrap();
        // Ensure index mtime is >= memory mtime on coarse filesystems.
        thread::sleep(Duration::from_millis(20));
        std::fs::write(&index_path, "{}").unwrap();

        let embedder = ApiEmbedder::lexical(64);
        let count = bootstrap_memory_index(&memory_path, &index_path, &embedder)
            .await
            .unwrap();

        assert_eq!(count, 0, "should skip when index already exists and is current");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn bootstrap_skips_missing_memory() {
        let dir = temp_dir("missing_memory");
        std::fs::create_dir_all(&dir).unwrap();

        let memory_path = dir.join("MEMORY.md"); // does not exist
        let index_path = dir.join("memory.rvf.json");

        let embedder = ApiEmbedder::lexical(64);
        let count = bootstrap_memory_index(&memory_path, &index_path, &embedder)
            .await
            .unwrap();

        assert_eq!(count, 0, "should skip when MEMORY.md is missing");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn bootstrap_skips_empty_memory() {
        let dir = temp_dir("empty_memory");
        std::fs::create_dir_all(&dir).unwrap();

        let memory_path = dir.join("MEMORY.md");
        let index_path = dir.join("memory.rvf.json");

        std::fs::write(&memory_path, "").unwrap();

        let embedder = ApiEmbedder::lexical(64);
        let count = bootstrap_memory_index(&memory_path, &index_path, &embedder)
            .await
            .unwrap();

        assert_eq!(count, 0, "should skip when MEMORY.md is empty");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn bootstrap_paragraph_splitting() {
        let dir = temp_dir("paragraphs");
        std::fs::create_dir_all(&dir).unwrap();

        let memory_path = dir.join("MEMORY.md");
        let index_path = dir.join("memory.rvf.json");

        std::fs::write(
            &memory_path,
            "First fact about something.\n\nSecond fact about another thing.\n\nThird fact.",
        )
        .unwrap();

        let embedder = ApiEmbedder::lexical(64);
        let count = bootstrap_memory_index(&memory_path, &index_path, &embedder)
            .await
            .unwrap();

        assert_eq!(count, 3);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn bootstrap_index_is_searchable() {
        let dir = temp_dir("searchable");
        std::fs::create_dir_all(&dir).unwrap();

        let memory_path = dir.join("MEMORY.md");
        let index_path = dir.join("memory.rvf.json");

        std::fs::write(
            &memory_path,
            "## Rust\n\nRust is a systems programming language.\n\n## Python\n\nPython is interpreted.",
        )
        .unwrap();

        let embedder = ApiEmbedder::lexical(64);
        bootstrap_memory_index(&memory_path, &index_path, &embedder)
            .await
            .unwrap();

        // Reopen and search
        let store = RvfStore::open(&index_path).unwrap();
        let query_emb = embedder.lexical_vector("rust programming");
        let results = store.query(&query_emb, 2);
        assert_eq!(results.len(), 2);

        // Verify metadata contains text
        let first = &results[0];
        assert!(first.metadata.get("text").is_some());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// WEFT-84: when MEMORY.md is edited after the index was built, bootstrap
    /// must rebuild so semantic search finds the new content.
    #[tokio::test]
    async fn bootstrap_reindexes_when_memory_newer() {
        let dir = temp_dir("mtime_reindex");
        std::fs::create_dir_all(&dir).unwrap();

        let memory_path = dir.join("MEMORY.md");
        let index_path = dir.join("memory.rvf.json");
        let embedder = ApiEmbedder::lexical(64);

        // Two ## headers so section split is stable (not paragraph fallback).
        std::fs::write(
            &memory_path,
            "## Alpha\n\nALPHA_ONLY unique token about widgets.\n\n## Misc\n\nOther old note.",
        )
        .unwrap();

        let count1 = bootstrap_memory_index(&memory_path, &index_path, &embedder)
            .await
            .unwrap();
        assert_eq!(count1, 2);
        assert!(index_path.exists());

        // Coarse FS mtime resolution (HFS+/FAT) can be 1s; sleep so rewrite is newer.
        thread::sleep(Duration::from_millis(1100));

        std::fs::write(
            &memory_path,
            "## Beta\n\nBETA_ONLY unique token about gadgets.\n\n## Misc\n\nOther new note.",
        )
        .unwrap();

        assert!(
            memory_is_newer_than_index(&memory_path, &index_path),
            "MEMORY.md mtime should exceed index mtime after rewrite"
        );

        let count2 = bootstrap_memory_index(&memory_path, &index_path, &embedder)
            .await
            .unwrap();
        assert_eq!(count2, 2, "should reindex when MEMORY.md is newer");

        let store = RvfStore::open(&index_path).unwrap();
        assert_eq!(store.len(), 2);

        let query_emb = embedder.lexical_vector("BETA_ONLY unique token gadgets");
        let results = store.query(&query_emb, 2);
        assert!(!results.is_empty());
        let texts: Vec<&str> = results
            .iter()
            .filter_map(|r| r.metadata.get("text").and_then(|v| v.as_str()))
            .collect();
        let joined = texts.join("\n");
        assert!(
            joined.contains("BETA_ONLY"),
            "search should surface reindexed content, got: {joined}"
        );
        assert!(
            !joined.contains("ALPHA_ONLY"),
            "stale sections should not remain after reindex, got: {joined}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// WEFT-84: `reindex_memory_index` always rebuilds even when the index is
    /// current relative to MEMORY.md mtime.
    #[tokio::test]
    async fn force_reindex_rebuilds_current_index() {
        let dir = temp_dir("force_reindex");
        std::fs::create_dir_all(&dir).unwrap();

        let memory_path = dir.join("MEMORY.md");
        let index_path = dir.join("memory.rvf.json");
        let embedder = ApiEmbedder::lexical(64);

        std::fs::write(
            &memory_path,
            "## Facts\n\nFORCE_REINDEX_TOKEN for zebra quilts.\n\n## More\n\nSecondary section.",
        )
        .unwrap();

        let count1 = bootstrap_memory_index(&memory_path, &index_path, &embedder)
            .await
            .unwrap();
        assert_eq!(count1, 2);

        // Index is up to date — plain bootstrap should skip.
        let skipped = bootstrap_memory_index(&memory_path, &index_path, &embedder)
            .await
            .unwrap();
        assert_eq!(skipped, 0, "up-to-date index should skip without force");

        // Forced reindex still rebuilds.
        let count2 = reindex_memory_index(&memory_path, &index_path, &embedder)
            .await
            .unwrap();
        assert_eq!(count2, 2, "force reindex should rebuild");

        let store = RvfStore::open(&index_path).unwrap();
        let query_emb = embedder.lexical_vector("FORCE_REINDEX_TOKEN zebra quilts");
        let results = store.query(&query_emb, 2);
        assert!(!results.is_empty());
        let joined: String = results
            .iter()
            .filter_map(|r| r.metadata.get("text").and_then(|v| v.as_str()))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("FORCE_REINDEX_TOKEN"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn memory_is_newer_when_index_missing() {
        let dir = temp_dir("mtime_missing_index");
        std::fs::create_dir_all(&dir).unwrap();
        let memory_path = dir.join("MEMORY.md");
        let index_path = dir.join("memory.rvf.json");
        std::fs::write(&memory_path, "x").unwrap();
        assert!(memory_is_newer_than_index(&memory_path, &index_path));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
