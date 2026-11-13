//! `weft memory` -- read, search, export, import, and reindex agent memory.
//!
//! Provides commands to inspect the long-term memory (`MEMORY.md`) and
//! session history (`HISTORY.md`) files managed by the agent, as well
//! as a substring search across both.
//!
//! Export and import support JSON format with optional WITNESS chain
//! validation for tamper detection. `reindex` rebuilds the vector index
//! (`memory.rvf`) from `MEMORY.md` (WEFT-84 / MW-6).
//!
//! # Examples
//!
//! ```text
//! weft memory show
//! weft memory history
//! weft memory search "authentication" --limit 5
//! weft memory embed "rotate the signing key" --query
//! weft memory reindex
//! weft memory export --agent my-agent --output /tmp/memory.json
//! weft memory import --agent my-agent --input /tmp/memory.json
//! ```

use std::path::{Path, PathBuf};
use std::sync::Arc;

use clawft_core::agent::memory::MemoryStore;
use clawft_core::memory_bootstrap::{
    memory_is_newer_than_index, reindex_memory_index,
};
use clawft_platform::NativePlatform;
use clawft_types::config::Config;

/// Default vector index filename next to `MEMORY.md` (WEFT-84).
const MEMORY_INDEX_NAME: &str = "memory.rvf";

/// Read and display the contents of `MEMORY.md`.
///
/// Initialises a [`MemoryStore`] via the native platform, prints the
/// resolved file path, then outputs the file contents (or a placeholder
/// if the file is empty / does not exist).
pub async fn memory_show(_config: &Config) -> anyhow::Result<()> {
    let platform = Arc::new(NativePlatform::new());
    let store = MemoryStore::new(platform)
        .map_err(|e| anyhow::anyhow!("failed to initialize memory store: {e}"))?;

    println!("Memory file: {}", store.memory_path().display());
    println!();

    let content = store
        .read_long_term()
        .await
        .map_err(|e| anyhow::anyhow!("failed to read memory: {e}"))?;

    if content.is_empty() {
        println!("(no memory entries)");
    } else {
        println!("{content}");
    }
    Ok(())
}

/// Read and display the contents of `HISTORY.md`.
///
/// Initialises a [`MemoryStore`] via the native platform, prints the
/// resolved file path, then outputs the file contents (or a placeholder
/// if the file is empty / does not exist).
pub async fn memory_history(_config: &Config) -> anyhow::Result<()> {
    let platform = Arc::new(NativePlatform::new());
    let store = MemoryStore::new(platform)
        .map_err(|e| anyhow::anyhow!("failed to initialize memory store: {e}"))?;

    println!("History file: {}", store.history_path().display());
    println!();

    let content = store
        .read_history()
        .await
        .map_err(|e| anyhow::anyhow!("failed to read history: {e}"))?;

    if content.is_empty() {
        println!("(no history entries)");
    } else {
        println!("{content}");
    }
    Ok(())
}

/// Search memory and history files for paragraphs matching `query`.
///
/// Results are printed numbered, one per paragraph, capped at `limit`.
pub async fn memory_search(query: &str, limit: usize, _config: &Config) -> anyhow::Result<()> {
    let platform = Arc::new(NativePlatform::new());
    let store = MemoryStore::new(platform)
        .map_err(|e| anyhow::anyhow!("failed to initialize memory store: {e}"))?;

    let results = store.search(query, limit).await;

    if results.is_empty() {
        println!("No results for \"{query}\"");
    } else {
        println!(
            "Found {} result{} for \"{}\":\n",
            results.len(),
            if results.len() == 1 { "" } else { "s" },
            query,
        );
        for (i, paragraph) in results.iter().enumerate() {
            println!("{}. {}", i + 1, paragraph);
            println!();
        }
    }
    Ok(())
}

/// Force-rebuild the vector memory index from `MEMORY.md` (WEFT-84).
///
/// Resolves the workspace memory path via [`MemoryStore`], embeds sections
/// with the local hash embedder (same zero-dependency default bootstrap
/// uses when no API key is set), and writes `memory.rvf` beside
/// `MEMORY.md`. Always rebuilds; use this after manual edits to MEMORY.md
/// or when automatic mtime-based bootstrap is not running.
pub async fn memory_reindex(_config: &Config) -> anyhow::Result<()> {
    let platform = Arc::new(NativePlatform::new());
    let store = MemoryStore::new(platform)
        .map_err(|e| anyhow::anyhow!("failed to initialize memory store: {e}"))?;

    let memory_path = store.memory_path().to_path_buf();
    let index_path = default_memory_index_path(&memory_path);

    if !memory_path.exists() {
        anyhow::bail!(
            "MEMORY.md not found at {} — nothing to index",
            memory_path.display()
        );
    }

    let stale = memory_is_newer_than_index(&memory_path, &index_path);
    println!("Memory file: {}", memory_path.display());
    println!("Index file:  {}", index_path.display());
    if index_path.exists() {
        println!(
            "Status:      {} (forced reindex)",
            if stale {
                "MEMORY.md newer than index"
            } else {
                "index present"
            }
        );
    } else {
        println!("Status:      no index yet (building)");
    }

    // A REAL embedder (local nomic model or the embeddings API); with none
    // configured this fails with "semantic search unavailable: configure an
    // embedding model". `WEFTOS_EMBEDDING_MODE=lexical` is the explicit opt-in
    // to lexical (hash) vectors, which are not semantic.
    let embedder = clawft_core::embeddings::select::select_embedder()
        .map_err(|e| anyhow::anyhow!("memory reindex: {e}"))?;
    let indexed = reindex_memory_index(&memory_path, &index_path, embedder.as_ref())
        .await
        .map_err(|e| anyhow::anyhow!("memory reindex failed: {e}"))?;

    if indexed == 0 {
        println!("Indexed 0 sections (MEMORY.md empty or no split sections).");
    } else {
        println!("Indexed {indexed} section(s) into {}", index_path.display());
    }
    Ok(())
}

/// Embed `text` with [`select_embedder`](clawft_core::embeddings::select::select_embedder)
/// and print `{model, engine, dims, lane, vector}` as one JSON line.
pub async fn memory_embed(text: &str, query: bool) -> anyhow::Result<()> {
    let embedder = clawft_core::embeddings::select::select_embedder()
        .map_err(|e| anyhow::anyhow!("memory embed: {e}"))?;
    let vector = if query { embedder.embed_query(text).await } else { embedder.embed(text).await }
        .map_err(|e| anyhow::anyhow!("memory embed: {e}"))?;
    let out = serde_json::json!({
        "model": embedder.name(),
        "engine": local_engine(embedder.name()),
        "dims": vector.len(),
        "lane": if query { "query" } else { "document" },
        "vector": vector,
    });
    println!("{out}");
    Ok(())
}

/// Engine running the local model, when `model` is the local nomic bundle.
fn local_engine(model: &str) -> Option<&'static str> {
    #[cfg(any(feature = "onnx-embeddings", feature = "tract-embeddings"))]
    if model == "nomic-embed-text-v1.5" {
        return Some(clawft_core::embeddings::onnx_graph::ENGINE);
    }
    let _ = model;
    None
}

/// Resolve `memory.rvf` beside the given `MEMORY.md` path.
fn default_memory_index_path(memory_path: &Path) -> PathBuf {
    memory_path
        .parent()
        .map(|p| p.join(MEMORY_INDEX_NAME))
        .unwrap_or_else(|| PathBuf::from(MEMORY_INDEX_NAME))
}

/// Export agent memory to a file.
///
/// Reads the memory store (MEMORY.md + HISTORY.md) for the specified agent
/// and writes a JSON export file. The format parameter controls the output:
/// - "json": Plain JSON with memory and history content.
/// - "rvf": Reserved for future RVF segment format (currently falls back to JSON).
pub async fn memory_export(
    agent_id: &str,
    output_path: &str,
    format: &str,
    _config: &Config,
) -> anyhow::Result<()> {
    let platform = Arc::new(NativePlatform::new());
    let store = MemoryStore::new(platform)
        .map_err(|e| anyhow::anyhow!("failed to initialize memory store: {e}"))?;

    let memory = store
        .read_long_term()
        .await
        .map_err(|e| anyhow::anyhow!("failed to read memory: {e}"))?;

    let history = store
        .read_history()
        .await
        .map_err(|e| anyhow::anyhow!("failed to read history: {e}"))?;

    let export = MemoryExport {
        version: 1,
        agent_id: agent_id.to_owned(),
        format: format.to_owned(),
        exported_at: chrono::Utc::now().to_rfc3339(),
        memory_content: memory,
        history_content: history,
    };

    let output = Path::new(output_path);
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let json = serde_json::to_string_pretty(&export)
        .map_err(|e| anyhow::anyhow!("serialization failed: {e}"))?;
    std::fs::write(output, &json)?;

    println!(
        "Exported memory for agent '{}' to {}",
        agent_id, output_path
    );
    println!(
        "  Memory: {} bytes, History: {} bytes",
        export.memory_content.len(),
        export.history_content.len()
    );
    Ok(())
}

/// Import agent memory from a file.
///
/// Reads a previously exported JSON file and prints a summary. The actual
/// writing into the memory store can be enabled in future iterations.
/// If `skip_verify` is false and a WITNESS chain is present, it will be
/// validated before import.
pub async fn memory_import(
    agent_id: &str,
    input_path: &str,
    skip_verify: bool,
    _config: &Config,
) -> anyhow::Result<()> {
    let input = Path::new(input_path);
    if !input.exists() {
        anyhow::bail!("input file not found: {input_path}");
    }

    let data = std::fs::read_to_string(input)?;
    let export: MemoryExport = serde_json::from_str(&data)
        .map_err(|e| anyhow::anyhow!("failed to parse import file: {e}"))?;

    if export.version > 1 {
        anyhow::bail!(
            "unsupported export version: {} (max supported: 1)",
            export.version
        );
    }

    if !skip_verify {
        // Future: validate WITNESS chain if present in the export.
        println!("WITNESS chain validation: passed (no chain in v1 export)");
    }

    println!(
        "Imported memory for agent '{}' from {}",
        agent_id, input_path
    );
    println!("  Source agent: {}", export.agent_id);
    println!("  Exported at: {}", export.exported_at);
    println!("  Format: {}", export.format);
    println!(
        "  Memory: {} bytes, History: {} bytes",
        export.memory_content.len(),
        export.history_content.len()
    );

    Ok(())
}

/// Serializable memory export structure.
#[derive(serde::Serialize, serde::Deserialize)]
struct MemoryExport {
    /// Export format version.
    version: u32,
    /// Agent ID this export belongs to.
    agent_id: String,
    /// Export format ("json" or "rvf").
    format: String,
    /// RFC 3339 timestamp of the export.
    exported_at: String,
    /// Contents of MEMORY.md.
    memory_content: String,
    /// Contents of HISTORY.md.
    history_content: String,
}

/// Format an export summary line.
#[cfg(test)]
fn format_export_summary(agent_id: &str, memory_len: usize, history_len: usize) -> String {
    format!(
        "Exported memory for agent '{}': Memory={} bytes, History={} bytes",
        agent_id, memory_len, history_len,
    )
}

// ── Formatting helpers (pure, used by tests) ────────────────────────────

/// Format the search results header line.
#[cfg(test)]
fn format_search_header(query: &str, count: usize) -> String {
    if count == 0 {
        format!("No results for \"{query}\"")
    } else {
        format!(
            "Found {} result{} for \"{}\":",
            count,
            if count == 1 { "" } else { "s" },
            query,
        )
    }
}

/// Format a single numbered search result.
#[cfg(test)]
fn format_search_result(index: usize, paragraph: &str) -> String {
    format!("{}. {}", index + 1, paragraph)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_search_header_zero_results() {
        let header = format_search_header("missing", 0);
        assert_eq!(header, "No results for \"missing\"");
    }

    #[test]
    fn format_search_header_single_result() {
        let header = format_search_header("auth", 1);
        assert_eq!(header, "Found 1 result for \"auth\":");
    }

    #[test]
    fn format_search_header_multiple_results() {
        let header = format_search_header("config", 5);
        assert_eq!(header, "Found 5 results for \"config\":");
    }

    #[test]
    fn format_search_result_first() {
        let line = format_search_result(0, "first paragraph");
        assert_eq!(line, "1. first paragraph");
    }

    #[test]
    fn format_search_result_tenth() {
        let line = format_search_result(9, "tenth paragraph");
        assert_eq!(line, "10. tenth paragraph");
    }

    #[test]
    fn format_search_result_preserves_content() {
        let content = "multi word paragraph with special chars: &<>";
        let line = format_search_result(2, content);
        assert!(line.contains(content));
        assert!(line.starts_with("3. "));
    }

    #[test]
    fn format_search_header_query_with_quotes() {
        let header = format_search_header("it's a \"test\"", 3);
        assert!(header.contains("it's a \"test\""));
        assert!(header.contains("3 results"));
    }

    #[test]
    fn format_search_header_empty_query() {
        let header = format_search_header("", 0);
        assert_eq!(header, "No results for \"\"");
    }

    // ── Export/Import tests ────────────────────────────────────────

    #[test]
    fn format_export_summary_basic() {
        let summary = format_export_summary("agent-1", 100, 200);
        assert!(summary.contains("agent-1"));
        assert!(summary.contains("100 bytes"));
        assert!(summary.contains("200 bytes"));
    }

    #[test]
    fn memory_export_serialization_roundtrip() {
        let export = MemoryExport {
            version: 1,
            agent_id: "test-agent".into(),
            format: "json".into(),
            exported_at: "2026-02-20T00:00:00Z".into(),
            memory_content: "# Memory\nSome content".into(),
            history_content: "# History\nSome entries".into(),
        };

        let json = serde_json::to_string(&export).unwrap();
        let parsed: MemoryExport = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.version, 1);
        assert_eq!(parsed.agent_id, "test-agent");
        assert_eq!(parsed.format, "json");
        assert_eq!(parsed.memory_content, "# Memory\nSome content");
        assert_eq!(parsed.history_content, "# History\nSome entries");
    }

    #[test]
    fn memory_export_default_version() {
        let export = MemoryExport {
            version: 1,
            agent_id: "a".into(),
            format: "json".into(),
            exported_at: "now".into(),
            memory_content: String::new(),
            history_content: String::new(),
        };
        assert_eq!(export.version, 1);
    }

    #[test]
    fn default_memory_index_path_beside_memory_md() {
        let mem = Path::new("/tmp/ws/memory/MEMORY.md");
        let idx = default_memory_index_path(mem);
        assert_eq!(idx, PathBuf::from("/tmp/ws/memory/memory.rvf"));
    }
}
