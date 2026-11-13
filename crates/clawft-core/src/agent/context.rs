//! Context builder for assembling LLM prompts.
//!
//! Combines the system prompt, active skill prompts, long-term memory,
//! and conversation history into a message list suitable for the LLM
//! pipeline. Ported from Python `nanobot/agent/context.py`.
//!
//! # Message assembly order
//!
//! 1. **System prompt** (role=`"system"`) -- identity and instructions
//! 2. **Active skill prompts** (role=`"system"`) -- prefixed with `# Skill: {name}`
//! 3. **Memory context** (role=`"system"`) -- prefixed with `# Relevant Memory:`
//!    By default the entire `MEMORY.md`. With `agents.memory_recall` on (WEFT-732, RMM
//!    arXiv:2503.08026) and a query, at most `top_m` retrieved snippets tagged `[m1]`…
//!    with a citation instruction ([`super::memory_recall`]); the dump stays the fail-open
//!    path. See `docs/research/rmm-reflective-memory-management.md` (ws06-memory).
//! 4. **Conversation history** -- recent messages from the session
//!
//! The current user message is **not** added here; the caller appends it.

use std::collections::HashMap;
use std::path::Path;
#[cfg(feature = "native")]
use std::path::PathBuf;
use std::sync::Arc;
#[cfg(feature = "native")]
use std::time::SystemTime;

use crate::runtime::Mutex;
use tracing::{debug, warn};

use clawft_platform::Platform;
use clawft_types::config::AgentsConfig;
use clawft_types::session::Session;

use super::agents::AgentDefinition;
use super::helpers::render_template;
use super::memory::MemoryStore;
use super::skills::SkillsLoader;

/// Cached bootstrap file entry with modification-time tracking.
#[cfg(feature = "native")]
#[derive(Debug, Clone)]
struct CachedFile {
    content: String,
    mtime: SystemTime,
}

/// Cache for bootstrap files, keyed by resolved path.
///
/// Uses `tokio::sync::Mutex` because the cache is checked/updated
/// inside async `build_system_prompt()`. The critical section is
/// short (one HashMap lookup + optional fs::metadata call).
#[cfg(feature = "native")]
type BootstrapCache = Arc<Mutex<HashMap<PathBuf, CachedFile>>>;

/// On browser, mtime-based caching is not available, so we use a
/// simple empty struct as a no-op placeholder.
#[cfg(not(feature = "native"))]
type BootstrapCache = Arc<Mutex<()>>;

// Re-export the canonical LlmMessage from pipeline::traits so that
// consumers of context.rs use the same type as the rest of the pipeline.
pub use crate::pipeline::traits::LlmMessage;

/// Builder for assembling LLM context from multiple sources.
///
/// Combines configuration, memory, skills, and session history into
/// a structured message list for the LLM pipeline. Each source is
/// read asynchronously through the platform abstraction.
///
/// # Usage
///
/// ```rust,ignore
/// let ctx = ContextBuilder::new(config, memory, skills, platform);
/// let system_prompt = ctx.build_system_prompt().await;
/// let messages = ctx.build_messages(&session, &["research"]).await;
/// ```
pub struct ContextBuilder<P: Platform> {
    config: AgentsConfig,
    memory: Arc<MemoryStore<P>>,
    skills: Arc<SkillsLoader<P>>,
    platform: Arc<P>,
    bootstrap_cache: BootstrapCache,
    compression_config: Option<CompressionConfig>,
    /// Retrieved memory with citations (WEFT-732); `None` = full `MEMORY.md` dump.
    memory_recall: Option<Arc<super::memory_recall::MemoryRecall>>,
}

impl<P: Platform> ContextBuilder<P> {
    /// Create a new context builder.
    ///
    /// # Arguments
    ///
    /// * `config` -- Agent configuration (system prompt, memory window, etc.)
    /// * `memory` -- Shared memory store for reading long-term memory
    /// * `skills` -- Shared skills loader for reading active skill prompts
    /// * `platform` -- Platform abstraction for filesystem/env access
    pub fn new(
        config: AgentsConfig,
        memory: Arc<MemoryStore<P>>,
        skills: Arc<SkillsLoader<P>>,
        platform: Arc<P>,
    ) -> Self {
        Self {
            config,
            memory,
            skills,
            platform,
            bootstrap_cache: {
                #[cfg(feature = "native")]
                {
                    Arc::new(Mutex::new(HashMap::new()))
                }
                #[cfg(not(feature = "native"))]
                {
                    Arc::new(Mutex::new(()))
                }
            },
            compression_config: None,
            memory_recall: None,
        }
        .with_recall_from_config()
    }

    /// Build the memory recall from `agents.memory_recall` (off by default: `None`).
    fn with_recall_from_config(self) -> Self {
        let recall = super::memory_recall::from_config(&self.config.memory_recall);
        self.with_memory_recall(recall)
    }

    /// Attach retrieved memory (WEFT-732). With it, [`build_messages_with_query`](Self::build_messages_with_query)
    /// injects at most `top_m` cited-id snippets instead of the whole `MEMORY.md`.
    pub fn with_memory_recall(mut self, recall: Option<Arc<super::memory_recall::MemoryRecall>>) -> Self {
        self.memory_recall = recall;
        self
    }

    /// The memory consolidator from `agents.memory_consolidation` over this builder's memory
    /// files (WEFT-347 / WEFT-733), or `None` when it is off.
    pub fn consolidator_from_config(&self) -> Option<crate::agent::learning::MemoryConsolidator<P>> {
        let c = &self.config.memory_consolidation;
        if !c.enabled {
            return None;
        }
        let store = MemoryStore::with_paths(
            self.memory.memory_path().clone(),
            self.memory.history_path().clone(),
            self.platform.clone(),
        );
        let cfg = crate::agent::learning::ConsolidationConfig::every_k_turns(c.every_k_turns).with_topic_merge(c.topic_merge);
        Some(crate::agent::learning::MemoryConsolidator::new(store, cfg))
    }

    /// The attached memory recall, for the agent loop's post-turn attribution.
    pub fn memory_recall(&self) -> Option<&Arc<super::memory_recall::MemoryRecall>> {
        self.memory_recall.as_ref()
    }

    /// Load a bootstrap file, using the mtime cache to skip disk reads
    /// when the file has not been modified since last load.
    ///
    /// Returns `Some(content)` if the file exists and is non-empty,
    /// `None` otherwise.
    #[cfg(feature = "native")]
    async fn load_cached_file(
        platform: &Arc<P>,
        cache: &BootstrapCache,
        file_path: &Path,
    ) -> Option<String> {
        // Check current mtime via tokio::fs::metadata (platform trait
        // does not expose metadata, so we bypass it for this stat call).
        let current_mtime = match tokio::fs::metadata(file_path).await {
            Ok(meta) => meta.modified().ok(),
            Err(_) => return None, // file does not exist or is inaccessible
        };

        // Cache hit: mtime matches
        {
            let cache_guard = cache.lock().await;
            if let Some(cached) = cache_guard.get(file_path)
                && current_mtime == Some(cached.mtime)
            {
                return Some(cached.content.clone());
            }
        }

        // Cache miss or stale: re-read from disk
        match platform.fs().read_to_string(file_path).await {
            Ok(content) if !content.trim().is_empty() => {
                if let Some(mtime) = current_mtime {
                    let mut cache_guard = cache.lock().await;
                    cache_guard.insert(
                        file_path.to_path_buf(),
                        CachedFile {
                            content: content.clone(),
                            mtime,
                        },
                    );
                }
                Some(content)
            }
            _ => None,
        }
    }

    /// Load a bootstrap file (browser version without mtime caching).
    ///
    /// On browser/WASM, `tokio::fs::metadata` is not available, so we
    /// skip the mtime cache and read directly through the platform.
    #[cfg(not(feature = "native"))]
    async fn load_cached_file(
        platform: &Arc<P>,
        _cache: &BootstrapCache,
        file_path: &Path,
    ) -> Option<String> {
        match platform.fs().read_to_string(file_path).await {
            Ok(content) if !content.trim().is_empty() => Some(content),
            _ => None,
        }
    }

    /// Build the system prompt from configuration.
    ///
    /// Assembles the core identity prompt. This includes:
    /// - A static identity header describing the assistant
    /// - Workspace path information
    /// - Memory file paths for the agent to reference
    ///
    /// The returned string is suitable for a `role="system"` message.
    pub async fn build_system_prompt(&self) -> String {
        let workspace = &self.config.defaults.workspace;
        let model = &self.config.defaults.model;

        let mut parts = Vec::new();

        // Load bootstrap files from the workspace (AGENTS.md, SOUL.md, etc.)
        // Searches: workspace root first, then .clawft/ subdirectory.
        // Uses mtime-based cache to skip disk reads when files have not changed.
        let bootstrap_files = ["SOUL.md", "IDENTITY.md", "AGENTS.md", "USER.md", "TOOLS.md"];
        let mut loaded_files = HashMap::new();
        for filename in &bootstrap_files {
            let home = self.platform.fs().home_dir();
            if let Some(home) = home {
                let ws_path = expand_workspace(workspace, &home);
                let candidates = [
                    ws_path.join(filename),
                    ws_path.join(".clawft").join(filename),
                ];
                for file_path in &candidates {
                    debug!(file = %file_path.display(), "checking for bootstrap file");
                    if let Some(content) =
                        Self::load_cached_file(&self.platform, &self.bootstrap_cache, file_path)
                            .await
                    {
                        debug!(file = %filename, bytes = content.len(), "loaded bootstrap file");
                        loaded_files.insert(*filename, content);
                        break; // First match wins
                    }
                }
                if !loaded_files.contains_key(filename) {
                    debug!(file = %filename, workspace = %ws_path.display(), "bootstrap file not found");
                }
            }
        }

        // If SOUL.md or IDENTITY.md exists, use it as the identity preamble
        // instead of the hardcoded default. The config section is always appended.
        let has_custom_identity =
            loaded_files.contains_key("SOUL.md") || loaded_files.contains_key("IDENTITY.md");

        if has_custom_identity {
            // Custom identity from bootstrap files
            if let Some(soul) = loaded_files.get("SOUL.md") {
                parts.push(format!("## SOUL.md\n\n{soul}"));
            }
            if let Some(identity) = loaded_files.get("IDENTITY.md") {
                parts.push(format!("## IDENTITY.md\n\n{identity}"));
            }
            // Append configuration context
            parts.push(format!(
                "## Configuration\n\n\
                Model: {model}\n\
                Workspace: {workspace}\n\
                Memory: {workspace}/memory/MEMORY.md\n\
                History: {workspace}/memory/HISTORY.md\n\
                Skills: {workspace}/skills/\n\n\
                You have access to tools that allow you to:\n\
                - Read, write, and edit files\n\
                - Execute shell commands\n\
                - Search the web and fetch web pages\n\
                - Send messages to users on chat channels"
            ));
        } else {
            // Default identity when no SOUL.md or IDENTITY.md exists
            parts.push(format!(
                "# clawft\n\n\
                You are clawft, a helpful AI assistant. You have access to tools that allow you to:\n\
                - Read, write, and edit files\n\
                - Execute shell commands\n\
                - Search the web and fetch web pages\n\
                - Send messages to users on chat channels\n\n\
                ## Configuration\n\
                Model: {model}\n\
                Workspace: {workspace}\n\
                Memory: {workspace}/memory/MEMORY.md\n\
                History: {workspace}/memory/HISTORY.md\n\
                Skills: {workspace}/skills/"
            ));
        }

        // Append remaining bootstrap files (AGENTS.md, USER.md, TOOLS.md)
        for filename in &["AGENTS.md", "USER.md", "TOOLS.md"] {
            if let Some(content) = loaded_files.get(filename) {
                parts.push(format!("## {filename}\n\n{content}"));
            }
        }

        parts.join("\n\n---\n\n")
    }

    /// Build the complete message list for an LLM call.
    ///
    /// Assembles messages in the canonical order:
    /// 1. System prompt (role=`"system"`)
    /// 2. Active skill prompts (role=`"system"`, one per skill)
    /// 3. Long-term memory context (role=`"system"`, if non-empty)
    /// 4. Conversation history from `session.get_history(memory_window)`
    ///
    /// The current user message is **not** included -- the caller adds
    /// it after calling this method.
    ///
    /// # Arguments
    ///
    /// * `session` -- Current conversation session
    /// * `active_skills` -- Names of skills to include in context
    pub async fn build_messages(
        &self,
        session: &Session,
        active_skills: &[String],
    ) -> Vec<LlmMessage> {
        let system_prompt = self.build_system_prompt().await;
        self.build_messages_inner(session, system_prompt, active_skills, None, None)
            .await
    }

    /// Like [`build_messages`](Self::build_messages), with the user's message as the memory
    /// retrieval query (WEFT-732). Without an attached recall this is `build_messages`.
    pub async fn build_messages_with_query(
        &self,
        session: &Session,
        active_skills: &[String],
        query: &str,
    ) -> Vec<LlmMessage> {
        let system_prompt = self.build_system_prompt().await;
        self.build_messages_inner(session, system_prompt, active_skills, None, Some(query))
            .await
    }

    /// Get a reference to the agent configuration.
    pub fn config(&self) -> &AgentsConfig {
        &self.config
    }

    /// Enable context compression with the given configuration.
    ///
    /// When compression is enabled, [`build_messages_compressed`](Self::build_messages_compressed)
    /// will apply a sliding-window strategy to keep the context within
    /// the token budget.
    pub fn with_compression(mut self, config: CompressionConfig) -> Self {
        self.compression_config = Some(config);
        self
    }

    /// Build messages with context compression applied.
    ///
    /// Assembles the full message list (same as [`build_messages`](Self::build_messages)),
    /// then compresses it according to the configured [`CompressionConfig`].
    /// If no compression config was set via [`with_compression`](Self::with_compression),
    /// uses [`CompressionConfig::default()`].
    ///
    /// Returns a [`CompressedContext`] containing the (possibly compressed)
    /// messages and metadata about the compression operation.
    pub async fn build_messages_compressed(
        &self,
        session: &Session,
        active_skills: &[String],
    ) -> CompressedContext {
        let messages = self.build_messages(session, active_skills).await;
        let config = self
            .compression_config
            .as_ref()
            .cloned()
            .unwrap_or_default();
        compress_context(messages, &config)
    }

    /// Build messages with LLM-backed compression (ADR-058 Phase 2.3).
    ///
    /// Like [`build_messages_compressed`](Self::build_messages_compressed), but
    /// aged turns are summarized by `summarizer` (the local Hermes provider in
    /// production, via [`LlmSummarizer`]) instead of the first-sentence
    /// extractive heuristic. On summarizer error it degrades to the heuristic
    /// rather than aborting the turn (see [`compress_context_with`]).
    pub async fn build_messages_compressed_with(
        &self,
        session: &Session,
        active_skills: &[String],
        summarizer: &dyn Summarizer,
    ) -> CompressedContext {
        let messages = self.build_messages(session, active_skills).await;
        let config = self
            .compression_config
            .as_ref()
            .cloned()
            .unwrap_or_default();
        compress_context_with(messages, &config, summarizer).await
    }

    /// Assemble a prefix-stable, window-capped prompt (ADR-058 Phase 2.2).
    ///
    /// Order is `[stable head] → [graft block] → [rolling tail]`:
    /// - **head** = the leading system messages (system prompt, skills,
    ///   memory / pinned facts). Byte-identical turn-to-turn for the same
    ///   inputs, so the serving layer reuses its KV (`--cache-reuse` /
    ///   `--slot-save-path`). Never reshuffled or trimmed.
    /// - **graft block** = L2-retrieved text, inserted right after the head.
    ///   Phase 2 passes none (`&[]`); Phase 3 supplies the graft set.
    /// - **rolling tail** = recent dialogue, trimmed oldest-first so the total
    ///   window never exceeds the budget (`max_window_tokens` minus reserved
    ///   output).
    ///
    /// The guarantee `window_tokens <= max_window_tokens` holds whenever the
    /// head + graft fit the budget; if they do not, the head is kept regardless
    /// (correctness over budget) and a warning is logged.
    pub async fn build_windowed(
        &self,
        session: &Session,
        active_skills: &[String],
        graft_block: &[LlmMessage],
        config: &WindowConfig,
    ) -> WindowedContext {
        let all = self.build_messages(session, active_skills).await;
        assemble_windowed(all, graft_block, config)
    }

    /// Build a system prompt message for a specific agent definition.
    ///
    /// Prepends the agent's `system_prompt` (with template rendering)
    /// to the standard system prompt.  If the agent has no custom
    /// system prompt, only the standard prompt is returned.
    ///
    /// # Arguments
    ///
    /// * `agent` -- The agent definition to inject
    /// * `args` -- Arguments string for template variable expansion
    pub async fn build_system_prompt_for_agent(
        &self,
        agent: &AgentDefinition,
        args: &str,
    ) -> String {
        let base = self.build_system_prompt().await;

        match &agent.system_prompt {
            Some(template) => {
                let rendered = render_template(template, args, &agent.variables);
                format!("# Agent: {}\n\n{}\n\n---\n\n{}", agent.name, rendered, base)
            }
            None => base,
        }
    }

    /// Build the complete message list for an agent-aware LLM call.
    ///
    /// Like [`build_messages`](Self::build_messages) but additionally:
    /// - Uses the agent's system prompt (template-rendered)
    /// - Activates the agent's declared skills
    /// - Appends any extra skill instructions as a system message
    ///
    /// # Arguments
    ///
    /// * `session` -- Current conversation session
    /// * `agent` -- Agent definition to use
    /// * `args` -- Arguments string for template rendering
    /// * `extra_skill_instructions` -- Optional additional skill text to inject
    pub async fn build_messages_for_agent(
        &self,
        session: &Session,
        agent: &AgentDefinition,
        args: &str,
        extra_skill_instructions: Option<&str>,
    ) -> Vec<LlmMessage> {
        let system_prompt = self.build_system_prompt_for_agent(agent, args).await;
        self.build_messages_inner(
            session,
            system_prompt,
            &agent.skills,
            extra_skill_instructions,
            None,
        )
        .await
    }

    /// Core message assembly logic shared by [`build_messages`] and
    /// [`build_messages_for_agent`].
    ///
    /// Assembles messages in order: system prompt, skill prompts,
    /// optional extra instructions, memory context, and conversation
    /// history.
    async fn build_messages_inner(
        &self,
        session: &Session,
        system_prompt: String,
        active_skills: &[String],
        extra_instructions: Option<&str>,
        query: Option<&str>,
    ) -> Vec<LlmMessage> {
        let mut messages = Vec::new();

        // 1. System prompt
        messages.push(LlmMessage {
            role: "system".into(),
            content: system_prompt,
            tool_call_id: None,
            tool_calls: None,
        });

        // 2. Active skill prompts
        for skill_name in active_skills {
            match self.skills.get_skill(skill_name).await {
                Some(skill) => {
                    if let Some(ref prompt) = skill.prompt {
                        messages.push(LlmMessage {
                            role: "system".into(),
                            content: format!("# Skill: {}\n\n{}", skill.name, prompt),
                            tool_call_id: None,
                            tool_calls: None,
                        });
                    }
                }
                None => match self.skills.load_skill(skill_name).await {
                    Ok(skill) => {
                        if let Some(ref prompt) = skill.prompt {
                            messages.push(LlmMessage {
                                role: "system".into(),
                                content: format!("# Skill: {}\n\n{}", skill.name, prompt),
                                tool_call_id: None,
                                tool_calls: None,
                            });
                        }
                    }
                    Err(e) => {
                        warn!(
                            skill = %skill_name,
                            error = %e,
                            "failed to load skill for context"
                        );
                    }
                },
            }
        }

        // 3. Extra skill instructions (e.g. from SkillRegistry)
        if let Some(instructions) = extra_instructions
            && !instructions.trim().is_empty()
        {
            messages.push(LlmMessage {
                role: "system".into(),
                content: format!("# Skill Instructions\n\n{instructions}"),
                tool_call_id: None,
                tool_calls: None,
            });
        }

        // 4. Memory context.
        // With a memory recall and a query (WEFT-732): at most top_m retrieved snippets,
        // tagged [m1]..., with a citation instruction. Otherwise (fail-open) the whole
        // MEMORY.md as before. docs/research/rmm-reflective-memory-management.md
        match self.memory.read_long_term().await {
            Ok(memory) if !memory.trim().is_empty() => {
                let picked = match (self.memory_recall.as_ref(), query) {
                    (Some(recall), Some(q)) => recall.select(&session.key, q, &memory),
                    _ => Vec::new(),
                };
                let content = if picked.is_empty() {
                    format!("# Relevant Memory:\n\n{memory}")
                } else {
                    super::memory_recall::MemoryRecall::render(&picked)
                };
                messages.push(LlmMessage {
                    role: "system".into(),
                    content,
                    tool_call_id: None,
                    tool_calls: None,
                });
            }
            Ok(_) => {}
            Err(e) => {
                warn!(error = %e, "failed to read long-term memory for context");
            }
        }

        // 5. Conversation history (truncated to memory_window)
        let window = self.config.defaults.memory_window.max(0) as usize;
        let history = session.get_history(window);
        for msg in history {
            let role = msg
                .get("role")
                .and_then(|v| v.as_str())
                .unwrap_or("user")
                .to_string();
            let content = msg
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            messages.push(LlmMessage {
                role,
                content,
                tool_call_id: None,
                tool_calls: None,
            });
        }

        messages
    }
}

// ── Context compression & tokenizer (re-exported) ────────────────────
//
// The token counter (ADR-058 Phase 2.1) and compression types/functions
// (Phase 2.3) live in sibling modules. They are re-exported here so the
// public path `clawft_core::agent::context::{count_tokens, compress_context,
// CompressionConfig, ...}` is unchanged for existing callers.

pub use super::tokenizer::count_tokens;

pub use super::context_compress::{
    CompressedContext, CompressionConfig, CompressionMetadata, SummarizeError, Summarizer,
    compress_context, compress_context_with,
};

#[cfg(feature = "native")]
pub use super::context_compress::LlmSummarizer;

// ── Prefix-stable windowed assembly (ADR-058 Phase 2.2) ───────────────

/// Default live-window cap, in tokens (ADR-060: Hermes served at ctx 32768).
pub const DEFAULT_WINDOW_TOKENS: usize = 32_768;

/// Budget configuration for prefix-stable windowed assembly.
#[derive(Debug, Clone)]
pub struct WindowConfig {
    /// Hard cap on the assembled live window, in tokens. Defaults to the
    /// ADR-060 served context size ([`DEFAULT_WINDOW_TOKENS`]).
    pub max_window_tokens: usize,
    /// Tokens reserved within the cap for the model's reply (kept out of the
    /// prompt budget). Defaults to 0.
    pub reserve_output_tokens: usize,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            max_window_tokens: DEFAULT_WINDOW_TOKENS,
            reserve_output_tokens: 0,
        }
    }
}

/// Result of prefix-stable windowed assembly.
#[derive(Debug, Clone)]
pub struct WindowedContext {
    /// Assembled messages: `[stable head] → [graft block] → [rolling tail]`.
    pub messages: Vec<LlmMessage>,
    /// Number of leading messages forming the prefix-stable head.
    pub head_len: usize,
    /// Number of graft-block messages inserted after the head (0 in Phase 2).
    pub graft_len: usize,
    /// Token count of the assembled window.
    pub window_tokens: usize,
    /// `true` if rolling-tail messages were dropped to honor the budget.
    pub tail_truncated: bool,
}

impl WindowedContext {
    /// The prefix-stable head serialized to a byte string, for cache-key / KV
    /// reuse and for asserting head stability turn-to-turn. Role and content
    /// are joined with control separators so distinct messages cannot collide.
    pub fn head_bytes(&self) -> String {
        let mut s = String::new();
        for m in &self.messages[..self.head_len] {
            s.push_str(&m.role);
            s.push('\u{0}');
            s.push_str(&m.content);
            s.push('\u{1}');
        }
        s
    }
}

/// Assemble `[head] → [graft] → [tail]` under a token budget (sync core of
/// [`ContextBuilder::build_windowed`], split out so it is testable without a
/// platform). The head is the leading run of `system` messages; everything
/// after is the rolling dialogue tail, trimmed newest-first to fit.
fn assemble_windowed(
    all: Vec<LlmMessage>,
    graft_block: &[LlmMessage],
    config: &WindowConfig,
) -> WindowedContext {
    let head_len = all.iter().take_while(|m| m.role == "system").count();
    let mut iter = all.into_iter();
    let head: Vec<LlmMessage> = (&mut iter).take(head_len).collect();
    let tail: Vec<LlmMessage> = iter.collect();

    let budget = config
        .max_window_tokens
        .saturating_sub(config.reserve_output_tokens);
    let head_tokens: usize = head.iter().map(|m| count_tokens(&m.content)).sum();
    let graft_tokens: usize = graft_block.iter().map(|m| count_tokens(&m.content)).sum();

    if head_tokens + graft_tokens > budget {
        warn!(
            head_tokens,
            graft_tokens,
            budget,
            "windowed assembly: stable head + graft exceed budget; tail dropped"
        );
    }

    // Remaining budget for the rolling tail after the (non-negotiable) head and
    // the graft block. Fill newest-first, then restore chronological order.
    let mut remaining = budget
        .saturating_sub(head_tokens)
        .saturating_sub(graft_tokens);
    let mut kept_rev: Vec<LlmMessage> = Vec::new();
    let mut tail_truncated = false;
    for m in tail.into_iter().rev() {
        let cost = count_tokens(&m.content);
        if cost <= remaining {
            remaining -= cost;
            kept_rev.push(m);
        } else {
            tail_truncated = true;
            break;
        }
    }
    kept_rev.reverse();

    let graft_len = graft_block.len();
    let mut messages = Vec::with_capacity(head.len() + graft_len + kept_rev.len());
    messages.extend(head);
    messages.extend(graft_block.iter().cloned());
    messages.extend(kept_rev);

    let window_tokens: usize = messages.iter().map(|m| count_tokens(&m.content)).sum();
    WindowedContext {
        messages,
        head_len,
        graft_len,
        window_tokens,
        tail_truncated,
    }
}

/// Expand a workspace path, replacing `~/` with the actual home directory.
fn expand_workspace(workspace: &str, home: &std::path::Path) -> std::path::PathBuf {
    if let Some(rest) = workspace.strip_prefix("~/") {
        home.join(rest)
    } else {
        std::path::PathBuf::from(workspace)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::memory::MemoryStore;
    use crate::agent::skills::SkillsLoader;
    use clawft_platform::NativePlatform;
    use clawft_types::config::AgentDefaults;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(prefix: &str) -> PathBuf {
        let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        std::env::temp_dir().join(format!("clawft_ctx_test_{prefix}_{pid}_{id}"))
    }

    fn test_config() -> AgentsConfig {
        AgentsConfig {
            defaults: AgentDefaults {
                workspace: "~/.clawft/workspace".into(),
                model: "test-model/v1".into(),
                max_tokens: 4096,
                temperature: 0.5,
                max_tool_iterations: 10,
                memory_window: 5,
            },
            ..AgentsConfig::default()
        }
    }

    /// Helper to create a ContextBuilder backed by temp directories.
    async fn setup(
        prefix: &str,
    ) -> (
        ContextBuilder<NativePlatform>,
        PathBuf,
        Arc<MemoryStore<NativePlatform>>,
        Arc<SkillsLoader<NativePlatform>>,
    ) {
        let dir = temp_dir(prefix);
        let mem_dir = dir.join("memory");
        let skills_dir = dir.join("skills");

        let platform = Arc::new(NativePlatform::new());

        let memory = Arc::new(MemoryStore::with_paths(
            mem_dir.join("MEMORY.md"),
            mem_dir.join("HISTORY.md"),
            platform.clone(),
        ));

        let skills = Arc::new(SkillsLoader::with_dir(skills_dir.clone(), platform.clone()));

        let ctx = ContextBuilder::new(test_config(), memory.clone(), skills.clone(), platform);

        (ctx, dir, memory, skills)
    }

    #[tokio::test]
    async fn build_system_prompt_contains_identity() {
        let (ctx, dir, _, _) = setup("prompt").await;

        let prompt = ctx.build_system_prompt().await;

        assert!(prompt.contains("clawft"));
        assert!(prompt.contains("test-model/v1"));
        assert!(prompt.contains("MEMORY.md"));
        assert!(prompt.contains("HISTORY.md"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn build_messages_includes_system_prompt() {
        let (ctx, dir, _, _) = setup("sys_msg").await;
        let session = Session::new("test:1");

        let messages = ctx.build_messages(&session, &[]).await;

        assert!(!messages.is_empty());
        assert_eq!(messages[0].role, "system");
        assert!(messages[0].content.contains("clawft"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn build_messages_includes_memory() {
        let (ctx, dir, memory, _) = setup("memory").await;

        memory
            .write_long_term("important fact: Rust is fast")
            .await
            .unwrap();

        let session = Session::new("test:2");
        let messages = ctx.build_messages(&session, &[]).await;

        // Should have system prompt + memory message
        let memory_msg = messages
            .iter()
            .find(|m| m.content.contains("Relevant Memory"));
        assert!(memory_msg.is_some());
        assert!(memory_msg.unwrap().content.contains("Rust is fast"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// Real-embedder test: skipped with a printed reason unless the model bundle is
    /// available (`onnx-embeddings` + `WEFTOS_TEST_EMBED_MODEL_DIR`). The query
    /// shares no word with the snippet it must retrieve.
    #[cfg(feature = "vector-memory")]
    #[tokio::test]
    async fn memory_recall_with_a_query_injects_cited_snippets_not_the_whole_file() {
        if crate::embeddings::test_support::real_embedder().is_none() {
            return;
        }
        let (ctx, dir, memory, _) = setup("recall").await;
        memory
            .write_long_term("The user has a dog named Rex.\n\nThe user prefers tea over coffee.\n\n[[graft debris]]")
            .await
            .unwrap();
        let recall = crate::agent::memory_recall::from_config(&clawft_types::config::MemoryRecallConfig {
            enabled: true,
            top_k: 20,
            top_m: 1,
            model_dir: std::env::var(crate::embeddings::test_support::TEST_MODEL_DIR_ENV).ok(),
            lexical: false,
        });
        assert!(recall.is_some());
        let ctx = ctx.with_memory_recall(recall);
        let session = Session::new("test:recall");

        let with_query = ctx.build_messages_with_query(&session, &[], "which hot drink does the person like best?").await;
        let mem = with_query.iter().find(|m| m.content.contains("Relevant Memory")).unwrap();
        assert!(mem.content.contains("[m1] The user prefers tea over coffee."), "{}", mem.content);
        assert!(!mem.content.contains("Rex") && !mem.content.contains("graft debris"), "top_m = 1, no dump");

        // No query (or no recall): the full-file dump stays the fail-open path.
        let plain = ctx.build_messages(&session, &[]).await;
        let dump = plain.iter().find(|m| m.content.contains("Relevant Memory")).unwrap();
        assert!(dump.content.starts_with("# Relevant Memory:") && dump.content.contains("Rex"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn build_messages_skips_empty_memory() {
        let (ctx, dir, _, _) = setup("no_mem").await;
        let session = Session::new("test:3");

        let messages = ctx.build_messages(&session, &[]).await;

        // No "Relevant Memory" message when memory is empty
        let memory_msg = messages
            .iter()
            .find(|m| m.content.contains("Relevant Memory"));
        assert!(memory_msg.is_none());

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn build_messages_includes_skill_prompts() {
        let (ctx, dir, _, skills) = setup("skills").await;

        // Create a skill on disk and load it
        let skills_dir = dir.join("skills");
        tokio::fs::create_dir_all(skills_dir.join("research"))
            .await
            .unwrap();
        tokio::fs::write(
            skills_dir.join("research").join("skill.json"),
            r#"{"name":"research","description":"Research skill","variables":["topic"]}"#,
        )
        .await
        .unwrap();
        tokio::fs::write(
            skills_dir.join("research").join("prompt.md"),
            "You are a research expert.",
        )
        .await
        .unwrap();

        // Load it into cache
        skills.load_skill("research").await.unwrap();

        let session = Session::new("test:4");
        let messages = ctx.build_messages(&session, &["research".into()]).await;

        let skill_msg = messages
            .iter()
            .find(|m| m.content.contains("# Skill: research"));
        assert!(skill_msg.is_some());
        assert!(skill_msg.unwrap().content.contains("research expert"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn build_messages_includes_conversation_history() {
        let (ctx, dir, _, _) = setup("history").await;

        let mut session = Session::new("test:5");
        session.add_message("user", "What is Rust?", None);
        session.add_message("assistant", "Rust is a systems language.", None);
        session.add_message("user", "Tell me more.", None);

        let messages = ctx.build_messages(&session, &[]).await;

        // Should have system prompt + 3 history messages
        // (memory_window=5, so all 3 fit)
        let history_roles: Vec<&str> = messages
            .iter()
            .skip(1) // skip system prompt
            .map(|m| m.role.as_str())
            .collect();

        assert_eq!(history_roles, vec!["user", "assistant", "user"]);

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn build_messages_truncates_history_to_window() {
        let (ctx, dir, _, _) = setup("truncate").await;

        let mut session = Session::new("test:6");
        for i in 0..20 {
            session.add_message("user", &format!("message {i}"), None);
        }

        let messages = ctx.build_messages(&session, &[]).await;

        // memory_window=5, so only last 5 history messages
        let history_msgs: Vec<&LlmMessage> = messages.iter().filter(|m| m.role == "user").collect();
        assert_eq!(history_msgs.len(), 5);
        assert!(history_msgs[0].content.contains("message 15"));
        assert!(history_msgs[4].content.contains("message 19"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn build_messages_order_is_correct() {
        let (ctx, dir, memory, skills) = setup("order").await;

        // Set up memory
        memory.write_long_term("a long-term fact").await.unwrap();

        // Set up a skill
        let skills_dir = dir.join("skills");
        tokio::fs::create_dir_all(skills_dir.join("test_skill"))
            .await
            .unwrap();
        tokio::fs::write(
            skills_dir.join("test_skill").join("skill.json"),
            r#"{"name":"test_skill","description":"Test","variables":[]}"#,
        )
        .await
        .unwrap();
        tokio::fs::write(
            skills_dir.join("test_skill").join("prompt.md"),
            "skill prompt content",
        )
        .await
        .unwrap();
        skills.load_skill("test_skill").await.unwrap();

        // Set up session
        let mut session = Session::new("test:7");
        session.add_message("user", "hello", None);

        let messages = ctx.build_messages(&session, &["test_skill".into()]).await;

        // Order: system, skill, memory, history
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0].role, "system");
        assert!(messages[0].content.contains("clawft"));
        assert_eq!(messages[1].role, "system");
        assert!(messages[1].content.contains("# Skill: test_skill"));
        assert_eq!(messages[2].role, "system");
        assert!(messages[2].content.contains("Relevant Memory"));
        assert_eq!(messages[3].role, "user");
        assert_eq!(messages[3].content, "hello");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn build_messages_loads_uncached_skill_on_demand() {
        let (ctx, dir, _, _) = setup("on_demand").await;

        // Create skill on disk but do NOT pre-load it
        let skills_dir = dir.join("skills");
        tokio::fs::create_dir_all(skills_dir.join("lazy_skill"))
            .await
            .unwrap();
        tokio::fs::write(
            skills_dir.join("lazy_skill").join("skill.json"),
            r#"{"name":"lazy_skill","description":"Lazy","variables":[]}"#,
        )
        .await
        .unwrap();
        tokio::fs::write(
            skills_dir.join("lazy_skill").join("prompt.md"),
            "loaded on demand",
        )
        .await
        .unwrap();

        let session = Session::new("test:8");
        let messages = ctx.build_messages(&session, &["lazy_skill".into()]).await;

        let skill_msg = messages
            .iter()
            .find(|m| m.content.contains("# Skill: lazy_skill"));
        assert!(skill_msg.is_some());

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn build_messages_handles_missing_skill_gracefully() {
        let (ctx, dir, _, _) = setup("missing_skill").await;
        let session = Session::new("test:9");

        // Request a skill that does not exist
        let messages = ctx.build_messages(&session, &["nonexistent".into()]).await;

        // Should just have system prompt, no error
        assert!(!messages.is_empty());
        assert_eq!(messages[0].role, "system");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[test]
    fn expand_workspace_with_tilde() {
        let home = PathBuf::from("/home/testuser");
        let expanded = expand_workspace("~/.clawft/workspace", &home);
        assert_eq!(expanded, PathBuf::from("/home/testuser/.clawft/workspace"));
    }

    #[test]
    fn expand_workspace_absolute() {
        let home = PathBuf::from("/home/testuser");
        let expanded = expand_workspace("/opt/workspace", &home);
        assert_eq!(expanded, PathBuf::from("/opt/workspace"));
    }

    #[test]
    fn config_accessor() {
        let config = test_config();
        let platform = Arc::new(NativePlatform::new());
        let memory = Arc::new(MemoryStore::with_paths(
            PathBuf::from("/tmp/m.md"),
            PathBuf::from("/tmp/h.md"),
            platform.clone(),
        ));
        let skills = Arc::new(SkillsLoader::with_dir(
            PathBuf::from("/tmp/skills"),
            platform.clone(),
        ));
        let ctx = ContextBuilder::new(config.clone(), memory, skills, platform);
        assert_eq!(ctx.config().defaults.memory_window, 5);
    }

    #[tokio::test]
    async fn llm_message_has_expected_fields() {
        let msg = LlmMessage {
            role: "system".into(),
            content: "test content".into(),
            tool_call_id: Some("tc-1".into()),
            tool_calls: None,
        };
        assert_eq!(msg.role, "system");
        assert_eq!(msg.content, "test content");
        assert_eq!(msg.tool_call_id.as_deref(), Some("tc-1"));
    }

    // ── Agent definition integration tests ───────────────────────────

    use crate::agent::agents::AgentDefinition;
    use std::collections::HashMap;

    fn test_agent() -> AgentDefinition {
        let mut vars = HashMap::new();
        vars.insert("lang".into(), "Rust".into());
        AgentDefinition {
            name: "researcher".into(),
            description: "Research agent".into(),
            model: Some("custom-model/v2".into()),
            system_prompt: Some("You are a ${lang} researcher. Arguments: $ARGUMENTS".into()),
            skills: vec![],
            allowed_tools: vec!["read_file".into()],
            max_turns: Some(5),
            variables: vars,
            source_path: None,
        }
    }

    #[tokio::test]
    async fn build_system_prompt_for_agent_renders_template() {
        let (ctx, dir, _, _) = setup("agent_prompt").await;
        let agent = test_agent();

        let prompt = ctx
            .build_system_prompt_for_agent(&agent, "topic1 topic2")
            .await;

        // Should contain the rendered agent prompt
        assert!(prompt.contains("# Agent: researcher"));
        assert!(prompt.contains("You are a Rust researcher"));
        assert!(prompt.contains("Arguments: topic1 topic2"));
        // Should also contain the base system prompt
        assert!(prompt.contains("clawft"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn build_system_prompt_for_agent_without_custom_prompt() {
        let (ctx, dir, _, _) = setup("agent_no_prompt").await;
        let mut agent = test_agent();
        agent.system_prompt = None;

        let prompt = ctx.build_system_prompt_for_agent(&agent, "").await;

        // Should just be the base prompt, no "# Agent:" header
        assert!(!prompt.contains("# Agent:"));
        assert!(prompt.contains("clawft"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn build_messages_for_agent_includes_agent_system_prompt() {
        let (ctx, dir, _, _) = setup("agent_msgs").await;
        let agent = test_agent();
        let session = Session::new("test:agent1");

        let messages = ctx
            .build_messages_for_agent(&session, &agent, "my-args", None)
            .await;

        assert!(!messages.is_empty());
        assert_eq!(messages[0].role, "system");
        assert!(messages[0].content.contains("# Agent: researcher"));
        assert!(messages[0].content.contains("Rust researcher"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn build_messages_for_agent_injects_extra_skill_instructions() {
        let (ctx, dir, _, _) = setup("agent_skill_instr").await;
        let agent = test_agent();
        let session = Session::new("test:agent2");

        let messages = ctx
            .build_messages_for_agent(&session, &agent, "", Some("Always cite your sources."))
            .await;

        let skill_instr = messages
            .iter()
            .find(|m| m.content.contains("# Skill Instructions"));
        assert!(skill_instr.is_some());
        assert!(
            skill_instr
                .unwrap()
                .content
                .contains("Always cite your sources.")
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn build_messages_for_agent_skips_empty_skill_instructions() {
        let (ctx, dir, _, _) = setup("agent_empty_instr").await;
        let agent = test_agent();
        let session = Session::new("test:agent3");

        let messages = ctx
            .build_messages_for_agent(&session, &agent, "", Some("   "))
            .await;

        let skill_instr = messages
            .iter()
            .find(|m| m.content.contains("# Skill Instructions"));
        assert!(skill_instr.is_none());

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn build_messages_for_agent_includes_memory() {
        let (ctx, dir, memory, _) = setup("agent_memory").await;
        let agent = test_agent();

        memory
            .write_long_term("agent-level memory test")
            .await
            .unwrap();

        let session = Session::new("test:agent4");
        let messages = ctx
            .build_messages_for_agent(&session, &agent, "", None)
            .await;

        let mem_msg = messages
            .iter()
            .find(|m| m.content.contains("Relevant Memory"));
        assert!(mem_msg.is_some());
        assert!(mem_msg.unwrap().content.contains("agent-level memory test"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn build_messages_for_agent_includes_history() {
        let (ctx, dir, _, _) = setup("agent_history").await;
        let agent = test_agent();

        let mut session = Session::new("test:agent5");
        session.add_message("user", "hello agent", None);
        session.add_message("assistant", "hello user", None);

        let messages = ctx
            .build_messages_for_agent(&session, &agent, "", None)
            .await;

        let user_msgs: Vec<_> = messages.iter().filter(|m| m.role == "user").collect();
        assert_eq!(user_msgs.len(), 1);
        assert_eq!(user_msgs[0].content, "hello agent");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    // ── ContextBuilder compression integration ─────────────────────

    #[tokio::test]
    async fn build_messages_compressed_integration() {
        let (ctx, dir, _, _) = setup("compressed").await;

        let ctx = ctx.with_compression(CompressionConfig {
            max_context_tokens: 100_000,
            recent_message_count: 10,
            compression_enabled: true,
        });

        let mut session = Session::new("test:compress1");
        session.add_message("user", "Hello", None);
        session.add_message("assistant", "Hi there!", None);

        let result = ctx.build_messages_compressed(&session, &[]).await;

        // Within budget, so no compression.
        assert_eq!(result.metadata.compression_ratio, 1.0);
        assert!(result.messages[0].role == "system");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn build_messages_compressed_uses_default_config() {
        let (ctx, dir, _, _) = setup("default_compress").await;
        // No with_compression() call -- should use defaults.

        let session = Session::new("test:compress2");
        let result = ctx.build_messages_compressed(&session, &[]).await;

        // Default budget is 8192, system prompt is small, should not compress.
        assert_eq!(result.metadata.compression_ratio, 1.0);

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    // ── Prefix-stable windowed assembly (Phase 2.2) ──────────────────

    fn wmsg(role: &str, content: &str) -> LlmMessage {
        LlmMessage {
            role: role.into(),
            content: content.into(),
            tool_call_id: None,
            tool_calls: None,
        }
    }

    #[test]
    fn windowed_order_is_head_graft_tail() {
        let all = vec![
            wmsg("system", "sys prompt"),
            wmsg("system", "# Skill: x"),
            wmsg("user", "u1"),
            wmsg("assistant", "a1"),
        ];
        let graft = vec![wmsg("system", "# Grafted: recalled fact")];
        let cfg = WindowConfig::default();
        let w = assemble_windowed(all, &graft, &cfg);

        assert_eq!(w.head_len, 2);
        assert_eq!(w.graft_len, 1);
        // head, then graft, then tail.
        assert_eq!(w.messages[0].content, "sys prompt");
        assert_eq!(w.messages[1].content, "# Skill: x");
        assert_eq!(w.messages[2].content, "# Grafted: recalled fact");
        assert_eq!(w.messages[3].content, "u1");
        assert_eq!(w.messages[4].content, "a1");
        assert!(!w.tail_truncated);
    }

    #[test]
    fn windowed_head_is_prefix_stable_across_tail_changes() {
        let head = vec![
            wmsg("system", "stable system prompt"),
            wmsg("system", "# Skill: y"),
        ];

        let mut a = head.clone();
        a.push(wmsg("user", "first question"));
        let mut b = head.clone();
        b.push(wmsg("user", "first question"));
        b.push(wmsg("assistant", "an answer"));
        b.push(wmsg("user", "second question"));

        let cfg = WindowConfig::default();
        let wa = assemble_windowed(a, &[], &cfg);
        let wb = assemble_windowed(b, &[], &cfg);

        // The head bytes must be byte-identical even though the tail grew.
        assert_eq!(wa.head_bytes(), wb.head_bytes());
        assert_eq!(wa.head_len, wb.head_len);
    }

    #[test]
    fn windowed_never_exceeds_cap() {
        // Small head, many tail turns, tiny budget.
        let mut all = vec![wmsg("system", "short head")];
        for i in 0..200 {
            all.push(wmsg(
                "user",
                &format!("user turn {i} with a fair number of words to spend the token budget"),
            ));
            all.push(wmsg(
                "assistant",
                &format!("assistant reply {i} of moderate length here"),
            ));
        }
        let cfg = WindowConfig {
            max_window_tokens: 200,
            reserve_output_tokens: 0,
        };
        let w = assemble_windowed(all, &[], &cfg);

        assert!(
            w.window_tokens <= cfg.max_window_tokens,
            "window {} exceeded cap {}",
            w.window_tokens,
            cfg.max_window_tokens
        );
        assert!(
            w.tail_truncated,
            "expected the tail to be trimmed under a tiny cap"
        );
        // The most-recent turn must survive the trim.
        assert!(w.messages.last().unwrap().content.contains("reply 199"));
    }

    #[test]
    fn windowed_reserve_output_tightens_budget() {
        let mut all = vec![wmsg("system", "head")];
        for i in 0..50 {
            all.push(wmsg(
                "user",
                &format!("turn {i} with several words to consume budget"),
            ));
        }
        let cfg = WindowConfig {
            max_window_tokens: 200,
            reserve_output_tokens: 150,
        };
        let w = assemble_windowed(all, &[], &cfg);
        // Effective budget is 50; the assembled window must respect it.
        assert!(
            w.window_tokens <= 50,
            "window {} exceeded effective budget 50",
            w.window_tokens
        );
    }

    #[tokio::test]
    async fn build_windowed_head_stable_across_turns() {
        let (ctx, dir, _, _) = setup("windowed_stable").await;

        let mut session = Session::new("test:windowed1");
        session.add_message("user", "first", None);
        let w1 = ctx
            .build_windowed(&session, &[], &[], &WindowConfig::default())
            .await;

        // Grow the dialogue; the stable head must not change byte-for-byte.
        session.add_message("assistant", "reply", None);
        session.add_message("user", "second", None);
        let w2 = ctx
            .build_windowed(&session, &[], &[], &WindowConfig::default())
            .await;

        assert_eq!(w1.head_bytes(), w2.head_bytes());
        assert!(w1.head_len >= 1 && w1.messages[0].role == "system");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
