//! Memory read/write tools.
//!
//! Two tools that read from and write to a workspace-level MEMORY.md file.
//! The memory file lives at `~/.clawft/workspace/memory/MEMORY.md` (with
//! fallback to `~/.nanobot/workspace/memory/MEMORY.md`).
//!
//! When the `vector-memory` feature is enabled, search queries use the
//! [`VectorStore`](clawft_core::vector_store::VectorStore) with a REAL embedding
//! model chosen by [`select_embedder`](clawft_core::embeddings::select::select_embedder)
//! (local nomic-embed-text-v1.5, or the embeddings API). With no model the tool
//! returns "semantic search unavailable: configure an embedding model"; it never
//! substitutes hashed vectors. `WEFTOS_EMBEDDING_MODE=lexical` is the explicit
//! opt-in to lexical results. Without the `vector-memory` feature, a plain
//! substring paragraph match is used (a build-time choice, not semantic).
//!
//! Tool I/O against MEMORY.md is not the same as prompt injection: the
//! agent loop still dumps the whole file as `# Relevant Memory:` today.
//! RMM (arXiv:2503.08026, ws06-memory) splits those paths — retrieve
//! Top-K / inject Top-M in `ContextBuilder`, keep these tools as
//! explicit read/write. See
//! `docs/research/rmm-reflective-memory-management.md`.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use clawft_core::tools::registry::{Tool, ToolError};
use clawft_platform::Platform;
use serde_json::json;
use tracing::debug;

/// Relative path from home dir to the memory file.
const MEMORY_REL_PATH: &str = ".clawft/workspace/memory/MEMORY.md";

/// Fallback relative path (nanobot compatibility).
const MEMORY_FALLBACK_REL_PATH: &str = ".nanobot/workspace/memory/MEMORY.md";

/// Resolve the memory file path using the platform's home directory.
///
/// Tries `~/.clawft/workspace/memory/MEMORY.md` first, then falls back
/// to `~/.nanobot/workspace/memory/MEMORY.md`.
async fn resolve_memory_path<P: Platform>(platform: &P) -> Result<PathBuf, ToolError> {
    let home = platform
        .fs()
        .home_dir()
        .ok_or_else(|| ToolError::ExecutionFailed("unable to determine home directory".into()))?;

    let primary = home.join(MEMORY_REL_PATH);
    if platform.fs().exists(&primary).await {
        return Ok(primary);
    }

    let fallback = home.join(MEMORY_FALLBACK_REL_PATH);
    if platform.fs().exists(&fallback).await {
        return Ok(fallback);
    }

    // Default to primary path (will be created on write).
    Ok(primary)
}

/// Simple paragraph-level search within memory content.
///
/// Splits the content into paragraphs (separated by blank lines) and returns
/// those that contain the query string (case-insensitive).
#[cfg(any(test, not(feature = "vector-memory")))]
fn search_paragraphs(content: &str, query: &str) -> Vec<String> {
    let query_lower = query.to_lowercase();
    content
        .split("\n\n")
        .filter(|paragraph| paragraph.to_lowercase().contains(&query_lower))
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

/// The selected embedder, kept for the process (a model load is expensive).
/// Only a successful selection is cached, so installing a model later works.
#[cfg(feature = "vector-memory")]
fn memory_embedder() -> Result<Arc<dyn clawft_core::embeddings::Embedder>, clawft_core::embeddings::EmbeddingError> {
    use std::sync::OnceLock;
    static EMBEDDER: OnceLock<Arc<dyn clawft_core::embeddings::Embedder>> = OnceLock::new();
    if let Some(e) = EMBEDDER.get() {
        return Ok(e.clone());
    }
    let e = clawft_core::embeddings::select::select_embedder()?;
    Ok(EMBEDDER.get_or_init(|| e).clone())
}

/// Vector-based semantic search over paragraphs using cosine similarity.
///
/// Splits content into paragraphs, embeds each (document lane) and the query
/// (query lane) with `embedder`, and returns paragraphs ranked by cosine
/// similarity via `VectorStore`. Embedding failures are errors, not fallbacks.
#[cfg(feature = "vector-memory")]
async fn search_paragraphs_with(
    embedder: &dyn clawft_core::embeddings::Embedder,
    content: &str,
    query: &str,
    max_results: usize,
) -> Result<Vec<String>, clawft_core::embeddings::EmbeddingError> {
    use clawft_core::vector_store::VectorStore;
    use std::collections::HashMap;

    let paragraphs: Vec<&str> = content
        .split("\n\n")
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .collect();

    if paragraphs.is_empty() || query.is_empty() {
        return Ok(Vec::new());
    }

    let mut store = VectorStore::new();
    for (i, paragraph) in paragraphs.iter().enumerate() {
        let embedding = embedder.embed(paragraph).await?;
        store.add(
            format!("p{i}"),
            paragraph.to_string(),
            embedding,
            vec![],
            HashMap::new(),
        );
    }

    let query_embedding = embedder.embed_query(query).await?;
    let results = store.search(&query_embedding, max_results);
    Ok(results.into_iter().map(|r| r.text).collect())
}

/// [`search_paragraphs_with`] using the process embedder from [`memory_embedder`].
/// Errors with "semantic search unavailable ..." when no model is configured.
#[cfg(feature = "vector-memory")]
async fn search_paragraphs_vector(
    content: &str,
    query: &str,
    max_results: usize,
) -> Result<Vec<String>, clawft_core::embeddings::EmbeddingError> {
    if content.trim().is_empty() || query.is_empty() {
        return Ok(Vec::new());
    }
    let embedder = memory_embedder()?;
    search_paragraphs_with(embedder.as_ref(), content, query, max_results).await
}

// ---------------------------------------------------------------------------
// MemoryReadTool
// ---------------------------------------------------------------------------

/// Read from the workspace memory file.
///
/// If a `query` is provided, returns matching paragraphs. Otherwise returns
/// the full content of `MEMORY.md`.
pub struct MemoryReadTool<P: Platform> {
    platform: Arc<P>,
}

impl<P: Platform> MemoryReadTool<P> {
    /// Create a new `MemoryReadTool`.
    pub fn new(platform: Arc<P>) -> Self {
        Self { platform }
    }
}

#[cfg_attr(not(feature = "browser"), async_trait)]
#[cfg_attr(feature = "browser", async_trait(?Send))]
impl<P: Platform + 'static> Tool for MemoryReadTool<P> {
    fn name(&self) -> &str {
        "memory_read"
    }

    fn description(&self) -> &str {
        "Read from the workspace memory file. Optionally filter by a search query."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Optional search query to filter memory content"
                }
            },
            "required": []
        })
    }

    async fn execute(&self, args: serde_json::Value) -> Result<serde_json::Value, ToolError> {
        let memory_path = resolve_memory_path(self.platform.as_ref()).await?;

        debug!(path = %memory_path.display(), "reading memory");

        if !self.platform.fs().exists(&memory_path).await {
            return Ok(json!({
                "content": "",
                "message": "No memory file found"
            }));
        }

        let content = self
            .platform
            .fs()
            .read_to_string(&memory_path)
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("failed to read memory: {}", e)))?;

        let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");

        if query.is_empty() {
            Ok(json!({ "content": content }))
        } else {
            #[cfg(feature = "vector-memory")]
            let matches = search_paragraphs_vector(&content, query, 20)
                .await
                .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;

            #[cfg(not(feature = "vector-memory"))]
            let matches = search_paragraphs(&content, query);

            Ok(json!({
                "query": query,
                "matches": matches,
                "count": matches.len(),
            }))
        }
    }
}

// ---------------------------------------------------------------------------
// MemoryWriteTool
// ---------------------------------------------------------------------------

/// Content that echoes the L2 graft block's rendering (`session_tier.rs`
/// `render_block`) rather than stating a fact. Marker-based, deliberately
/// narrow: only the two strings that rendering actually produces.
fn is_graft_echo(content: &str) -> bool {
    content.contains("[chain_seq ") || content.contains("# Recalled context")
}

/// Write to the workspace memory file.
///
/// Supports `append` (default) and `overwrite` modes. Creates the file
/// and parent directories if they do not exist.
pub struct MemoryWriteTool<P: Platform> {
    platform: Arc<P>,
}

impl<P: Platform> MemoryWriteTool<P> {
    /// Create a new `MemoryWriteTool`.
    pub fn new(platform: Arc<P>) -> Self {
        Self { platform }
    }
}

#[cfg_attr(not(feature = "browser"), async_trait)]
#[cfg_attr(feature = "browser", async_trait(?Send))]
impl<P: Platform + 'static> Tool for MemoryWriteTool<P> {
    fn name(&self) -> &str {
        "memory_write"
    }

    fn description(&self) -> &str {
        "Write to the workspace memory file. Supports append and overwrite modes."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "content": {
                    "type": "string",
                    "description": "The content to write to memory"
                },
                "mode": {
                    "type": "string",
                    "description": "Write mode: 'append' (default) or 'overwrite'",
                    "enum": ["append", "overwrite"]
                }
            },
            "required": ["content"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> Result<serde_json::Value, ToolError> {
        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("missing required field: content".to_string()))?;

        // Refuse graft-echo: recalled-context blocks are the loop's own
        // retrieval scaffolding. Written back into MEMORY.md they reload as
        // "Relevant Memory" on every subsequent turn — a self-amplifying
        // feedback loop observed live (a runaway tool loop appended 20 of
        // them, most empty). The error text tells the model what to do
        // instead, so a well-behaved retry still lands the actual fact.
        if is_graft_echo(content) {
            return Err(ToolError::InvalidArgs(
                "memory_write rejects recalled-context blocks ('[chain_seq …]' / \
                 '# Recalled context'): that text is retrieval output, not a fact. \
                 State the fact to remember in your own words instead."
                    .to_string(),
            ));
        }

        let mode = args
            .get("mode")
            .and_then(|v| v.as_str())
            .unwrap_or("append");

        let memory_path = resolve_memory_path(self.platform.as_ref()).await?;

        debug!(path = %memory_path.display(), mode, "writing memory");

        match mode {
            "overwrite" => {
                self.platform
                    .fs()
                    .write_string(&memory_path, content)
                    .await
                    .map_err(|e| {
                        ToolError::ExecutionFailed(format!("failed to write memory: {}", e))
                    })?;
            }
            _ => {
                // Prepend a newline separator if the file already has content.
                let separator = if self.platform.fs().exists(&memory_path).await {
                    "\n\n"
                } else {
                    ""
                };

                let to_write = format!("{}{}", separator, content);
                self.platform
                    .fs()
                    .append_string(&memory_path, &to_write)
                    .await
                    .map_err(|e| {
                        ToolError::ExecutionFailed(format!("failed to append memory: {}", e))
                    })?;
            }
        }

        Ok(json!({
            "message": format!("Successfully wrote {} bytes to memory (mode: {})", content.len(), mode),
            "path": memory_path.to_string_lossy(),
        }))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use clawft_platform::NativePlatform;
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_memory_path() -> PathBuf {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        std::env::temp_dir().join(format!(
            "clawft_memory_test_{pid}_{id}/workspace/memory/MEMORY.md"
        ))
    }

    // -- search_paragraphs unit tests --------------------------------------

    #[test]
    fn test_search_paragraphs_basic() {
        let content = "First paragraph about Rust.\n\nSecond paragraph about Python.\n\nThird paragraph about Rust and Go.";
        let matches = search_paragraphs(content, "rust");
        assert_eq!(matches.len(), 2);
        assert!(matches[0].contains("Rust"));
        assert!(matches[1].contains("Rust"));
    }

    #[test]
    fn test_search_paragraphs_case_insensitive() {
        let content = "Hello World\n\nGoodbye World";
        let matches = search_paragraphs(content, "HELLO");
        assert_eq!(matches.len(), 1);
        assert!(matches[0].contains("Hello"));
    }

    #[test]
    fn test_search_paragraphs_no_match() {
        let content = "Some text here\n\nMore text there";
        let matches = search_paragraphs(content, "nonexistent");
        assert!(matches.is_empty());
    }

    #[test]
    fn test_search_paragraphs_empty_content() {
        let matches = search_paragraphs("", "anything");
        assert!(matches.is_empty());
    }

    // -- MemoryReadTool tests (with custom path) ---------------------------

    // These tests write directly to a temporary path and then read using
    // the platform. Since resolve_memory_path uses home_dir, we test the
    // search_paragraphs function directly and the Tool trait integration
    // via a path that exists.

    #[tokio::test]
    async fn test_memory_read_no_file() {
        let platform = Arc::new(NativePlatform::new());
        let tool = MemoryReadTool::new(platform);

        // This tests the real resolve path + file existence check.
        // If no memory file exists, it should return empty content.
        let result = tool.execute(json!({})).await.unwrap();
        // It's either empty content or actual memory content (if running on
        // a dev machine with ~/.clawft). Either way it shouldn't error.
        assert!(result.get("content").is_some() || result.get("message").is_some());
    }

    #[tokio::test]
    async fn test_memory_write_and_read_roundtrip() {
        let platform = Arc::new(NativePlatform::new());
        let memory_path = temp_memory_path();

        // Create parent dirs and write a file
        if let Some(parent) = memory_path.parent() {
            tokio::fs::create_dir_all(parent).await.unwrap();
        }
        tokio::fs::write(&memory_path, "# Test Memory\n\nKey decision: use Rust")
            .await
            .unwrap();

        // Read back directly (bypassing resolve_memory_path)
        let content = platform.fs().read_to_string(&memory_path).await.unwrap();
        assert!(content.contains("Key decision"));

        // Test paragraph search
        let matches = search_paragraphs(&content, "decision");
        assert_eq!(matches.len(), 1);
        assert!(matches[0].contains("use Rust"));

        // Cleanup
        if let Some(root) = memory_path
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
        {
            let _ = tokio::fs::remove_dir_all(root).await;
        }
    }

    #[tokio::test]
    async fn test_memory_write_overwrite_mode() {
        let platform = Arc::new(NativePlatform::new());
        let memory_path = temp_memory_path();

        if let Some(parent) = memory_path.parent() {
            tokio::fs::create_dir_all(parent).await.unwrap();
        }

        // Write initial content
        platform
            .fs()
            .write_string(&memory_path, "original content")
            .await
            .unwrap();

        // Overwrite
        platform
            .fs()
            .write_string(&memory_path, "new content")
            .await
            .unwrap();

        let content = platform.fs().read_to_string(&memory_path).await.unwrap();
        assert_eq!(content, "new content");
        assert!(!content.contains("original"));

        // Cleanup
        if let Some(root) = memory_path
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
        {
            let _ = tokio::fs::remove_dir_all(root).await;
        }
    }

    #[tokio::test]
    async fn test_memory_write_append_mode() {
        let platform = Arc::new(NativePlatform::new());
        let memory_path = temp_memory_path();

        if let Some(parent) = memory_path.parent() {
            tokio::fs::create_dir_all(parent).await.unwrap();
        }

        // Write initial content
        platform
            .fs()
            .write_string(&memory_path, "first")
            .await
            .unwrap();

        // Append
        platform
            .fs()
            .append_string(&memory_path, "\n\nsecond")
            .await
            .unwrap();

        let content = platform.fs().read_to_string(&memory_path).await.unwrap();
        assert!(content.contains("first"));
        assert!(content.contains("second"));

        // Cleanup
        if let Some(root) = memory_path
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
        {
            let _ = tokio::fs::remove_dir_all(root).await;
        }
    }

    #[tokio::test]
    async fn test_memory_write_tool_missing_content() {
        let platform = Arc::new(NativePlatform::new());
        let tool = MemoryWriteTool::new(platform);

        let err = tool.execute(json!({})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)));
    }

    #[tokio::test]
    async fn test_resolve_memory_path_returns_primary() {
        let platform = NativePlatform::new();
        // Should not error -- always returns a path even if file doesn't exist
        let path = resolve_memory_path(&platform).await.unwrap();
        assert!(
            path.to_string_lossy().contains(".clawft")
                || path.to_string_lossy().contains(".nanobot")
        );
    }

    // -- vector search tests (feature-gated) ---------------------------------

    #[cfg(feature = "vector-memory")]
    mod vector_tests {
        use super::super::*;
        use clawft_core::embeddings::select::LexicalEmbedder;
        use clawft_core::embeddings::test_support::real_embedder;

        const CONTENT: &str = "Rust is a systems programming language.\n\nPython is great for scripting.\n\nRust provides memory safety without a garbage collector.\n\nJava runs on the JVM.";

        /// Real embedder (skipped with a printed reason unless `onnx-embeddings` +
        /// `WEFTOS_TEST_EMBED_MODEL_DIR`). The query shares no content word with the
        /// paragraph it must rank first. The full ranking is printed; only the top
        /// result and "not behind the Java paragraph" are asserted, because the model's
        /// order of the remaining paragraphs is evidence, not a contract.
        #[tokio::test]
        #[cfg_attr(not(feature = "real-embedder-tests"), ignore = "needs --features real-embedder-tests and WEFTOS_TEST_EMBED_MODEL_DIR (nomic-embed-text-v1.5 bundle)")]
        async fn vector_search_returns_semantically_similar_paragraphs() {
            let Some(real) = real_embedder() else { return };
            let results = search_paragraphs_with(
                &real,
                CONTENT,
                "how does the language stop dangling pointers without needing a collector",
                4,
            )
            .await
            .unwrap();
            assert_eq!(results.len(), 4);
            assert!(
                results[0].contains("memory safety"),
                "top result should be the memory-safety paragraph: {results:?}"
            );
            eprintln!("real-embedder ranking: {results:?}");
            let pos = |needle: &str| results.iter().position(|r| r.contains(needle)).unwrap();
            assert!(pos("memory safety") < pos("JVM") && pos("memory safety") < pos("Python"), "{results:?}");
        }

        /// No model, no API key, no opt-in: an error naming what to configure,
        /// never hashed results.
        #[tokio::test]
        async fn vector_search_without_a_model_is_an_error_not_a_hash_result() {
            if clawft_core::embeddings::select::lexical_mode_requested()
                || std::env::var("OPENAI_API_KEY").is_ok_and(|k| !k.is_empty())
                || search_paragraphs_vector("a", "a", 1).await.is_ok()
            {
                eprintln!("SKIP: this host has a model, an API key, or lexical mode on");
                return;
            }
            let err = search_paragraphs_vector(CONTENT, "memory safety", 2).await.unwrap_err();
            let msg = err.to_string();
            assert!(msg.contains("semantic search unavailable"), "{msg}");
            assert!(msg.contains("configure an embedding model"), "{msg}");
        }

        /// The explicit lexical embedder works and matches shared words (PLUMBING).
        #[tokio::test]
        async fn vector_search_lexical_mode_matches_shared_words_plumbing() {
            let results = search_paragraphs_with(&LexicalEmbedder::new(), CONTENT, "Rust memory safety", 2)
                .await
                .unwrap();
            assert!(results[0].contains("memory safety"), "{results:?}");
        }

        #[tokio::test]
        async fn vector_search_empty_content_returns_empty() {
            let results = search_paragraphs_vector("", "query", 5).await.unwrap();
            assert!(results.is_empty());
        }

        #[tokio::test]
        async fn vector_search_empty_query_returns_empty() {
            let results = search_paragraphs_vector("some content", "", 5).await.unwrap();
            assert!(results.is_empty());
        }

        #[tokio::test]
        async fn vector_search_respects_max_results_plumbing() {
            let content = "Paragraph A\n\nParagraph B\n\nParagraph C\n\nParagraph D";
            let results = search_paragraphs_with(&LexicalEmbedder::new(), content, "paragraph", 2)
                .await
                .unwrap();
            assert!(results.len() <= 2, "should respect max_results");
        }

        #[tokio::test]
        async fn vector_search_single_paragraph_plumbing() {
            let content = "Only one paragraph here about Rust programming.";
            let results = search_paragraphs_with(&LexicalEmbedder::new(), content, "Rust", 5)
                .await
                .unwrap();
            assert_eq!(results.len(), 1);
            assert!(results[0].contains("Rust"));
        }
    }
}
