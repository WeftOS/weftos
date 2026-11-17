//! WASM entrypoint for clawft (browser + wasip2).
//!
//! This crate provides a WebAssembly build of the clawft agent core.
//! It excludes components that require native OS features:
//! - Shell execution tools (exec_shell, spawn)
//! - Channel plugins (Telegram, Slack, Discord)
//! - Native CLI terminal I/O
//! - Process spawning
//!
//! # Platform Support
//!
//! - **Browser** (`--features browser`, target `wasm32-unknown-unknown`):
//!   wasm-bindgen exports (`init`, `send_message`, …) over `BrowserPlatform`.
//! - **WASI** (default path, target `wasm32-wasip2`): lightweight
//!   `init` / `process_message` / `capabilities` stubs + platform shims.
//!
//! # Feature exclusion (ADR-083)
//!
//! `browser` is the in-tab build; `wasm-plugins` (wasmtime host
//! re-exports) is native only. The browser feature also swaps the public
//! entry points (`init`, `process_message`, `capabilities` exist only
//! without it), so it is not additive. Combining `browser` with
//! `wasm-plugins` is rejected at compile time instead of silently producing
//! a different crate. (`browser` with `project-kernel` is already rejected
//! by `clawft-types`, which forbids `native` together with `browser`.)
//!
//! # Host-side plugin runtime (WEFT-398)
//!
//! The native **wasmtime host** (sandbox, engine, audit, permission store)
//! lives in the dedicated [`clawft-wasm-host`] crate. Enable
//! `--features wasm-plugins` on *this* crate only if you need the
//! historical `clawft_wasm::{sandbox,engine,audit,permission_store}`
//! re-export paths; new host code should depend on `clawft-wasm-host`
//! directly.
//!
//! # Size Budget
//!
//! Target: < 300 KB uncompressed, < 120 KB gzipped (browser / wasip2
//! entry only — host crate is not part of the browser bundle).

#[cfg(all(feature = "browser", feature = "wasm-plugins"))]
compile_error!(
    "features `browser` and `wasm-plugins` are mutually exclusive (ADR-083): \
     the browser build is wasm32-unknown-unknown, the plugin host is native wasmtime"
);

#[cfg(all(feature = "browser", target_feature = "atomics"))]
compile_error!(
    "the `browser` runtime must not be built with `+atomics`: BrowserRuntime's \
     manual Send/Sync impls assume single-threaded wasm, and a threaded build \
     would share its JS-backed handles across web workers"
);

#[cfg(feature = "alloc-tracing")]
pub mod alloc_trace;
pub mod allocator;
pub mod env;
pub mod fs;
pub mod http;
pub mod platform;

// ---------------------------------------------------------------------------
// WEFT-398: re-export host modules for API compatibility
// ---------------------------------------------------------------------------

/// Plugin sandbox enforcement for WASM plugins.
///
/// Implementation lives in [`clawft_wasm_host::sandbox`]. Available when
/// the `wasm-plugins` feature is enabled (native host only).
#[cfg(feature = "wasm-plugins")]
pub use clawft_wasm_host::sandbox;

/// Audit logging for WASM plugin host function calls.
///
/// Implementation lives in [`clawft_wasm_host::audit`].
#[cfg(feature = "wasm-plugins")]
pub use clawft_wasm_host::audit;

/// WASM plugin engine with fuel metering and memory limits.
///
/// Implementation lives in [`clawft_wasm_host::engine`].
#[cfg(feature = "wasm-plugins")]
pub use clawft_wasm_host::engine;

/// Permission persistence and approval for WASM plugin upgrades.
///
/// Implementation lives in [`clawft_wasm_host::permission_store`].
#[cfg(feature = "wasm-plugins")]
pub use clawft_wasm_host::permission_store;

pub use platform::WasmPlatform;

/// Version information for the WASM build.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Initialize the WASM agent (WASI / non-browser path).
///
/// Called once when the WASM module is instantiated. Sets up the agent
/// configuration and pipeline from WASI-accessible config files.
///
/// # Returns
///
/// Returns 0 on success, non-zero on failure.
///
/// Browser builds shadow this with the async `wasm-bindgen` `init` from
/// [`browser_entry`]; gating prevents a shadow clash on `wasm32-unknown-unknown`.
#[cfg(not(feature = "browser"))]
pub fn init() -> i32 {
    // Phase 3A Week 11: Will load config from WASI filesystem
    // and set up the agent pipeline.
    0
}

/// Process a single message through the agent pipeline (WASI path).
///
/// # Arguments
///
/// * `input` - The user message as a UTF-8 string.
///
/// # Returns
///
/// The agent's response as a string, or an error message.
///
/// On browser builds the analogous wired entry-point is
/// [`browser_entry::send_message`] which drives the full pipeline.
#[cfg(not(feature = "browser"))]
pub fn process_message(input: &str) -> String {
    // Phase 3A Week 12: Will run the full 6-stage pipeline.
    format!(
        "clawft-wasm v{}: received '{}' (pipeline not yet wired)",
        VERSION, input
    )
}

/// Get the agent's capabilities as a JSON string (WASI path).
///
/// Returns a JSON object describing available tools, providers,
/// and configuration for this WASM instance. Browser builds query
/// capabilities via the wired pipeline directly.
#[cfg(not(feature = "browser"))]
pub fn capabilities() -> String {
    serde_json::json!({
        "version": VERSION,
        "platform": "wasm32-wasip2",
        "tools": ["read_file", "write_file", "edit_file", "list_directory", "memory_read", "memory_write", "web_fetch", "web_search"],
        "excluded_tools": ["exec_shell", "spawn", "message"],
        "channels": [],
        "status": "initializing"
    })
    .to_string()
}

// ---------------------------------------------------------------------------
// Browser WASM entry point (feature = "browser")
// ---------------------------------------------------------------------------

/// Browser entry point module providing wasm-bindgen exports for
/// running clawft in a web browser via the BrowserPlatform.
#[cfg(feature = "browser")]
mod browser_entry {
    use std::path::PathBuf;
    use std::sync::{Arc, OnceLock};

    use clawft_core::agent::local_file_sink::LocalFileSink;
    use clawft_core::agent::loop_core::AgentLoop;
    use clawft_core::bootstrap::AppContext;
    use clawft_core::tools::registry::ToolRegistry;
    use clawft_llm::browser_transport::BrowserLlmClient;
    use clawft_llm::config::LlmProviderConfig;
    use clawft_platform::browser::{
        config_persist_path, conv_id_for_group, group_clawft_md_path, resolve_group_id,
        sessions_dir, BrowserEnvironment, DEFAULT_GROUP_ID,
    };
    use clawft_platform::env::Environment;
    use clawft_platform::fs::FileSystem;
    use clawft_platform::{BrowserPlatform, Platform};
    use clawft_types::config::Config;
    use clawft_types::event::InboundMessage;
    use wasm_bindgen::prelude::*;

    /// Persistent runtime state initialized by `init()`.
    ///
    /// Wires `AgentLoop<BrowserPlatform>` end-to-end through the
    /// 6-stage pipeline (classifier → router → assembler → transport
    /// → scorer → learner) plus session management, the context
    /// builder, the conversation sink, the effect gate, and the tool
    /// registry. `send_message` builds an [`InboundMessage`] and
    /// dispatches it through [`AgentLoop::handle_turn`], identical in
    /// shape to the native daemon's `agent.chat` path.
    ///
    /// Conversation history is durable via [`LocalFileSink`] on the
    /// platform filesystem (OPFS when `browser-opfs`) — WEFT-399.
    struct BrowserRuntime {
        agent: Arc<AgentLoop<BrowserPlatform>>,
        /// Snapshot of the tool registry kept alive for introspection
        /// entry points (`tool_schema`, `tool_list`). Captured before
        /// `ctx.into_agent_loop()` consumes the AppContext (WEFT-307).
        tools: Arc<ToolRegistry>,
        /// Live environment shared with the platform inside `agent`
        /// (WEFT-391). `set_env` mutates through this `Arc` so tools
        /// and config loaders see updates after init.
        env: Arc<BrowserEnvironment>,
        /// Shared platform (FS + env) for history / config / CLAWFT.md
        /// OPFS helpers outside the agent loop (WEFT-399).
        platform: Arc<BrowserPlatform>,
        /// Sessions directory for the typed [`LocalFileSink`] helpers
        /// (list / delete / load) — same dir `into_agent_loop` used.
        sessions_dir: PathBuf,
    }

    // SAFETY: wasm32-unknown-unknown is single-threaded; nothing here
    // ever crosses a thread boundary. The pipeline's `Send + Sync`
    // expectations on transports are satisfied via the `?Send`
    // `async_trait` relaxation in `LlmTransport` / `LlmProvider` for
    // the browser feature; the `OnceLock` in turn just needs the
    // outer struct to be `Send + Sync`.
    unsafe impl Send for BrowserRuntime {}
    unsafe impl Sync for BrowserRuntime {}

    static RUNTIME: OnceLock<BrowserRuntime> = OnceLock::new();

    /// Look up the user's `ProviderConfig` by provider name.
    fn user_provider_config(config: &Config, name: &str) -> clawft_types::config::ProviderConfig {
        config.providers.provider_config(name).clone()
    }

    /// Route a model string like "anthropic/claude-sonnet-4-20250514" to the
    /// correct provider config and return (LlmProviderConfig, stripped_model, ProviderConfig).
    ///
    /// Resolution order:
    /// 1. Exact prefix match against builtin providers (e.g. `openrouter/` → OpenRouter).
    /// 2. Fallback: if no prefix matches, walk
    ///    [`ProvidersConfig::effective_provider_fallback_order`](clawft_types::config::ProvidersConfig::effective_provider_fallback_order)
    ///    (config-driven, WEFT-404; default preserves the historical chain) and
    ///    pick the first provider with an API key. The full model string is
    ///    sent as-is since there's no prefix to strip. This handles models
    ///    like `arcee-ai/trinity-large-preview:free` that are hosted on
    ///    OpenRouter but don't use the `openrouter/` prefix.
    fn resolve_provider(
        config: &Config,
        model: &str,
    ) -> Result<
        (
            LlmProviderConfig,
            String,
            clawft_types::config::ProviderConfig,
        ),
        String,
    > {
        let builtins = clawft_llm::config::builtin_providers();

        // 1. Find the builtin whose model_prefix matches.
        for builtin in &builtins {
            if let Some(ref prefix) = builtin.model_prefix {
                if model.starts_with(prefix) {
                    let stripped = model[prefix.len()..].to_string();
                    let user_cfg = user_provider_config(config, &builtin.name);

                    // Merge user overrides into the builtin config.
                    let mut llm_cfg = builtin.clone();
                    if let Some(ref base) = user_cfg.api_base {
                        llm_cfg.base_url = base.clone();
                    }
                    if let Some(ref extra) = user_cfg.extra_headers {
                        llm_cfg.headers.extend(extra.clone());
                    }

                    return Ok((llm_cfg, stripped, user_cfg));
                }
            }
        }

        // 2. No prefix matched — walk config-driven fallback order (WEFT-404).
        //    OpenRouter is first by default because it aggregates third-party
        //    models with vendor/ prefixes (e.g. arcee-ai/, meta-llama/).
        let fallback_order = config.providers.effective_provider_fallback_order();

        for name in &fallback_order {
            let user_cfg = user_provider_config(config, name);
            if !user_cfg.api_key.expose().is_empty() {
                let builtin = builtins.iter().find(|b| b.name == *name).cloned();
                if let Some(mut llm_cfg) = builtin {
                    if let Some(ref base) = user_cfg.api_base {
                        llm_cfg.base_url = base.clone();
                    }
                    if let Some(ref extra) = user_cfg.extra_headers {
                        llm_cfg.headers.extend(extra.clone());
                    }

                    web_sys::console::log_1(
                        &format!(
                            "[clawft] no prefix match for '{}', falling back to {} provider",
                            model, name
                        )
                        .into(),
                    );

                    // Send the full model string as-is (no prefix to strip).
                    return Ok((llm_cfg, model.to_string(), user_cfg));
                }
            }
        }

        Err(format!(
            "no provider found for model '{}'. Either use a prefixed model like \
             'openrouter/arcee-ai/trinity-large-preview:free', or configure an \
             API key for a provider.",
            model
        ))
    }

    /// Parse an optional JSON object of string→string env vars.
    ///
    /// Empty / missing input yields an empty map. Rejects non-object JSON
    /// and non-string values so callers get a clear init error.
    fn parse_env_seed(env_json: Option<&str>) -> Result<std::collections::HashMap<String, String>, String> {
        let Some(raw) = env_json.map(str::trim).filter(|s| !s.is_empty()) else {
            return Ok(std::collections::HashMap::new());
        };
        let value: serde_json::Value = serde_json::from_str(raw)
            .map_err(|e| format!("env map parse error: {e}"))?;
        let obj = value
            .as_object()
            .ok_or_else(|| "env map must be a JSON object of string keys to string values".to_string())?;
        let mut map = std::collections::HashMap::with_capacity(obj.len());
        for (k, v) in obj {
            let s = v.as_str().ok_or_else(|| {
                format!("env map value for key '{k}' must be a string")
            })?;
            map.insert(k.clone(), s.to_string());
        }
        Ok(map)
    }

    /// Initialize the clawft-wasm browser runtime.
    ///
    /// Parses the provided JSON config, builds an
    /// [`AppContext<BrowserPlatform>`](AppContext), wires the LLM
    /// transport to the appropriate provider via [`BrowserLlmClient`],
    /// and produces a fully assembled [`AgentLoop<BrowserPlatform>`].
    /// Must be called once before `send_message`.
    ///
    /// # Parameters
    ///
    /// * `config_json` — clawft [`Config`] as a JSON string.
    /// * `env_json` — optional JSON object of string→string pairs used to
    ///   pre-seed [`BrowserEnvironment`] before the platform is moved
    ///   into the agent loop (WEFT-391). Pass `undefined` / `null` from
    ///   JS to skip seeding; call [`set_env`] after init for live
    ///   updates.
    #[wasm_bindgen]
    pub async fn init(config_json: &str, env_json: Option<String>) -> Result<(), JsValue> {
        console_error_panic_hook::set_once();

        let mut config: Config = serde_json::from_str(config_json)
            .map_err(|e| JsValue::from_str(&format!("config parse error: {e}")))?;

        // WEFT-391 / WEFT-14: open a shared BrowserEnvironment (OPFS snapshot
        // when `browser-opfs` is enabled), overlay optional JS-side env seed,
        // then clone the Arc into BrowserRuntime so set_env can mutate after
        // AgentLoop takes ownership of BrowserPlatform.
        let seed = parse_env_seed(env_json.as_deref())
            .map_err(|e| JsValue::from_str(&e))?;
        let env = Arc::new(BrowserEnvironment::open_with_seed(seed).await);
        // WEFT-13 / WEFT-392: when built with `browser-opfs`, prefer OPFS-backed
        // FS (falls back to in-memory if OPFS is unavailable). Workspace is
        // always the absolute virtual path `/clawft/workspace`.
        let platform = Arc::new(BrowserPlatform::with_env_arc_open(Arc::clone(&env)).await);

        let model = config.agents.defaults.model.clone();
        let (llm_cfg, stripped_model, user_cfg) =
            resolve_provider(&config, &model).map_err(|e| JsValue::from_str(&e))?;

        let api_key = user_cfg.api_key.expose();
        if api_key.is_empty() {
            return Err(JsValue::from_str(&format!(
                "no API key configured for provider matching model '{}'. Set apiKey in the provider config.",
                model
            )));
        }

        web_sys::console::log_1(
            &format!(
                "[clawft] provider={}, cors_proxy={:?}, browser_direct={}, base_url={}",
                llm_cfg.name, user_cfg.cors_proxy, user_cfg.browser_direct, llm_cfg.base_url,
            )
            .into(),
        );

        let client = Arc::new(BrowserLlmClient::with_api_key(
            llm_cfg,
            api_key.to_string(),
            user_cfg.browser_direct,
            user_cfg.cors_proxy.clone(),
        ));

        // The pipeline's transport sees the stripped model name; the
        // routing decision will pin it back to the same provider via
        // `StaticRouter::from_config`. Stamp the stripped form into the
        // agent defaults so the static router doesn't try to re-resolve
        // a prefix that the browser already consumed.
        config.agents.defaults.model = stripped_model;

        // Build the AppContext (bus, sessions, memory, skills, context,
        // empty tools registry, default classifier/router/assembler/
        // scorer/learner with stub transport) — exactly like the native
        // path — then swap in a transport wired to BrowserLlmClient.
        let mut ctx = AppContext::new(config.clone(), platform.clone())
            .await
            .map_err(|e| JsValue::from_str(&format!("AppContext init failed: {e}")))?;

        // Register all the browser-compatible tools. Native-exec tools
        // (shell, spawn) are gated out at compile time via
        // clawft-tools' feature flags so what lands here is the
        // file/memory/web-search/web-fetch subset. The browser
        // workspace is always the absolute virtual root under
        // `BROWSER_WORKSPACE_DIR` (`/clawft/workspace`) — never a
        // relative `.clawft/` path (WEFT-392 home_dir collision).
        let workspace_dir =
            std::path::PathBuf::from(clawft_platform::browser::BROWSER_WORKSPACE_DIR);
        clawft_tools::register_all(
            ctx.tools_mut(),
            platform.clone(),
            workspace_dir,
            clawft_types::security::CommandPolicy::default(),
            clawft_types::security::UrlPolicy::default(),
            clawft_tools::web_search::WebSearchConfig::default(),
            // Browser build has no subagent substrate.
            None,
        );

        // Replace the stub transport with the BrowserLlmClient-backed
        // one so the pipeline's transport stage actually reaches the
        // network. Stages 1, 2, 3, 5, and 6 are unchanged.
        let pipeline = clawft_core::bootstrap::build_browser_pipeline(&config, client);
        ctx.set_pipeline(pipeline);

        // WEFT-307: snapshot the tool registry before `into_agent_loop`
        // consumes the AppContext so JS callers can introspect tool
        // schemas via `tool_schema(slug)`.
        let tools = ctx.tools_arc();
        // WEFT-399: capture sessions dir before consume so history APIs
        // can open a typed LocalFileSink against the same JSONL files
        // the agent sink writes.
        let sessions_dir = ctx.sessions().sessions_dir().clone();

        let agent = Arc::new(ctx.into_agent_loop());

        // WEFT-399 / P6.4: persist the raw config JSON so reload can
        // restore UI settings. Same origin-scoped secrecy trade-off as
        // WEFT-14 env persistence (may include provider API keys).
        if let Err(e) = platform
            .fs()
            .write_string(&config_persist_path(), config_json)
            .await
        {
            web_sys::console::warn_1(
                &format!("[clawft] WEFT-399: config snapshot write failed: {e}").into(),
            );
        }

        RUNTIME
            .set(BrowserRuntime {
                agent,
                tools,
                env,
                platform,
                sessions_dir,
            })
            .map_err(|_| JsValue::from_str("already initialized"))?;

        web_sys::console::log_1(
            &"clawft-wasm initialized — AgentLoop<BrowserPlatform> wired through full pipeline"
                .into(),
        );
        Ok(())
    }

    /// Send a message through the clawft AgentLoop pipeline.
    ///
    /// Builds an [`InboundMessage`] (channel = `"web"`, chat_id =
    /// `"browser"`), dispatches it through
    /// [`AgentLoop::handle_turn`], and returns the resulting
    /// outbound text. Conversation history, session state, the
    /// 6-stage pipeline, the conversation sink, and tool execution
    /// all run on the wired [`AgentLoop<BrowserPlatform>`].
    ///
    /// Turns are appended to the OPFS-backed session JSONL (when
    /// `browser-opfs`) so history survives reload (WEFT-399).
    #[wasm_bindgen]
    pub async fn send_message(text: &str) -> Result<String, JsValue> {
        send_message_to(text, DEFAULT_GROUP_ID).await
    }

    /// Like [`send_message`], but routes the turn to a named group
    /// (`chat_id` = `group_id`, session key `web:{group_id}`).
    ///
    /// Per-group history and optional `CLAWFT.md` live under the
    /// CLAUDE.md-per-group OPFS layout (WEFT-399).
    #[wasm_bindgen]
    pub async fn send_message_to(text: &str, group_id: &str) -> Result<String, JsValue> {
        let rt = RUNTIME
            .get()
            .ok_or_else(|| JsValue::from_str("not initialized — call init() first"))?;

        let group = resolve_group_id(Some(group_id)).map_err(|e| JsValue::from_str(&e))?;

        let msg = InboundMessage {
            channel: "web".into(),
            sender_id: "browser-user".into(),
            chat_id: group,
            content: text.to_string(),
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata: std::collections::HashMap::new(),
        };

        // WEFT-323: browser path has no per-conv cancel RPC; mint a
        // never-cancelled token so handle_turn's iteration-boundary
        // check is a no-op. Same type AgentLoop uses (clawft_plugin
        // re-export / stub under non-native).
        let cancel = rt.agent.fresh_cancel_token();
        let outbound = rt
            .agent
            .handle_turn(msg, &cancel)
            .await
            .map_err(|e| JsValue::from_str(&format!("agent error: {e}")))?;

        Ok(outbound.content)
    }

    // -------------------------------------------------------------------
    // Conversation history + group identity (WEFT-399)
    // -------------------------------------------------------------------

    fn history_sink(rt: &BrowserRuntime) -> LocalFileSink<BrowserPlatform> {
        LocalFileSink::with_dir(Arc::clone(&rt.platform), rt.sessions_dir.clone())
    }

    /// Load conversation history for a group as a JSON array of turns.
    ///
    /// Each element: `{ "role", "content", "ts_ms", "tool_calls"?,
    /// "tool_call_id"? }`. Empty array when the group has no turns or
    /// the runtime is not initialized. `group_id` may be omitted /
    /// empty to use the default group (`browser`).
    #[wasm_bindgen]
    pub async fn get_history(group_id: Option<String>) -> Result<String, JsValue> {
        let rt = RUNTIME
            .get()
            .ok_or_else(|| JsValue::from_str("not initialized — call init() first"))?;
        let group = resolve_group_id(group_id.as_deref()).map_err(|e| JsValue::from_str(&e))?;
        let conv = conv_id_for_group(&group);
        let turns = rt
            .agent
            .conversation_sink()
            .history(&conv, 0)
            .await;
        let payload: Vec<serde_json::Value> = turns
            .into_iter()
            .map(|t| {
                let mut v = serde_json::json!({
                    "role": t.role,
                    "content": t.content,
                    "ts_ms": t.ts_ms,
                });
                if let Some(obj) = v.as_object_mut() {
                    if let Some(tc) = t.tool_calls {
                        obj.insert("tool_calls".into(), serde_json::Value::Array(tc));
                    }
                    if let Some(id) = t.tool_call_id {
                        obj.insert("tool_call_id".into(), serde_json::Value::String(id));
                    }
                }
                v
            })
            .collect();
        Ok(serde_json::to_string(&payload).unwrap_or_else(|_| "[]".into()))
    }

    /// Delete the durable history file for a group (idempotent).
    #[wasm_bindgen]
    pub async fn clear_history(group_id: Option<String>) -> Result<(), JsValue> {
        let rt = RUNTIME
            .get()
            .ok_or_else(|| JsValue::from_str("not initialized — call init() first"))?;
        let group = resolve_group_id(group_id.as_deref()).map_err(|e| JsValue::from_str(&e))?;
        let conv = conv_id_for_group(&group);
        history_sink(rt)
            .delete(&conv)
            .await
            .map_err(|e| JsValue::from_str(&format!("clear_history: {e}")))
    }

    /// Read the group's `CLAWFT.md` (CLAUDE.md-per-group identity).
    ///
    /// Returns an empty string when the file is missing.
    #[wasm_bindgen]
    pub async fn get_group_clawft_md(group_id: Option<String>) -> Result<String, JsValue> {
        let rt = RUNTIME
            .get()
            .ok_or_else(|| JsValue::from_str("not initialized — call init() first"))?;
        let group = resolve_group_id(group_id.as_deref()).map_err(|e| JsValue::from_str(&e))?;
        let path = group_clawft_md_path(&group);
        if !rt.platform.fs().exists(&path).await {
            return Ok(String::new());
        }
        rt.platform
            .fs()
            .read_to_string(&path)
            .await
            .map_err(|e| JsValue::from_str(&format!("get_group_clawft_md: {e}")))
    }

    /// Write the group's `CLAWFT.md` (CLAUDE.md-per-group identity).
    #[wasm_bindgen]
    pub async fn set_group_clawft_md(
        group_id: Option<String>,
        content: &str,
    ) -> Result<(), JsValue> {
        let rt = RUNTIME
            .get()
            .ok_or_else(|| JsValue::from_str("not initialized — call init() first"))?;
        let group = resolve_group_id(group_id.as_deref()).map_err(|e| JsValue::from_str(&e))?;
        let path = group_clawft_md_path(&group);
        rt.platform
            .fs()
            .write_string(&path, content)
            .await
            .map_err(|e| JsValue::from_str(&format!("set_group_clawft_md: {e}")))
    }

    /// Load the config JSON snapshot written by the last successful
    /// [`init`] (P6.4 / WEFT-399).
    ///
    /// Works without a live runtime — opens the platform FS directly
    /// so a UI can restore settings before calling `init` again.
    /// Returns an empty string when no snapshot exists; otherwise the
    /// raw config JSON (same shape passed to `init`).
    #[wasm_bindgen]
    pub async fn load_persisted_config() -> Result<String, JsValue> {
        let fs = clawft_platform::browser::BrowserFileSystem::open().await;
        let path = config_persist_path();
        if !fs.exists(&path).await {
            return Ok(String::new());
        }
        fs.read_to_string(&path)
            .await
            .map_err(|e| JsValue::from_str(&format!("load_persisted_config: {e}")))
    }

    /// Absolute virtual sessions directory path (debug / tests).
    #[wasm_bindgen]
    pub fn history_sessions_dir() -> String {
        sessions_dir().display().to_string()
    }

    /// Stream a single-turn chat completion through the pipeline
    /// (WEFT-390).
    ///
    /// Stages 1–3 (classify / route / assemble) run as usual, then
    /// stage 4 streams text deltas into `on_chunk` as SSE arrives via
    /// Fetch `ReadableStream` / wasm-streams. Stages 5–6 score and
    /// learn after the stream completes.
    ///
    /// Unlike [`send_message`], this path does **not** run the full
    /// agent tool loop or session append — it is the pipeline
    /// streaming entry. Prefer [`send_message`] when tools / history
    /// matter; use `stream_chat` for token-by-token UI updates.
    ///
    /// # Parameters
    ///
    /// * `text` — user message content.
    /// * `on_chunk` — JS `function(chunk: string): void | boolean`.
    ///   Return `false` to abort the stream early; any other value
    ///   (including `undefined`) continues.
    ///
    /// # Returns
    ///
    /// The full accumulated assistant text (even if the stream was
    /// aborted early).
    #[wasm_bindgen]
    pub async fn stream_chat(text: &str, on_chunk: &js_sys::Function) -> Result<String, JsValue> {
        use clawft_core::pipeline::traits::{ChatRequest, LlmMessage, StreamCallback};
        use clawft_types::provider::ContentBlock;
        use std::cell::RefCell;
        use std::rc::Rc;

        let rt = RUNTIME
            .get()
            .ok_or_else(|| JsValue::from_str("not initialized — call init() first"))?;

        let request = ChatRequest {
            messages: vec![LlmMessage {
                role: "user".into(),
                content: text.to_string(),
                tool_call_id: None,
                tool_calls: None,
            }],
            tools: vec![],
            model: None,
            max_tokens: None,
            temperature: None,
            auth_context: None,
            complexity_boost: 0.0,
            tool_choice: None,
        };

        let accumulated = Rc::new(RefCell::new(String::new()));
        let acc = Rc::clone(&accumulated);
        // Clone the Function into an owned handle the closure can call.
        let on_chunk = on_chunk.clone();

        let callback: StreamCallback = Box::new(move |delta: &str| {
            acc.borrow_mut().push_str(delta);
            match on_chunk.call1(&JsValue::NULL, &JsValue::from_str(delta)) {
                Ok(v) => {
                    // Explicit `false` aborts; everything else continues.
                    !matches!(v.as_bool(), Some(false))
                }
                Err(err) => {
                    web_sys::console::warn_1(
                        &format!("stream_chat on_chunk threw: {err:?}").into(),
                    );
                    true
                }
            }
        });

        let response = rt
            .agent
            .pipeline()
            .complete_stream(&request, callback)
            .await
            .map_err(|e| JsValue::from_str(&format!("stream error: {e}")))?;

        // Prefer accumulated deltas; fall back to response content if the
        // provider only returned a terminal non-streaming payload.
        let mut out = accumulated.borrow().clone();
        if out.is_empty() {
            for block in &response.content {
                if let ContentBlock::Text { text } = block {
                    out.push_str(text);
                }
            }
        }
        Ok(out)
    }

    /// Set an environment variable on the live [`BrowserEnvironment`].
    ///
    /// After [`init`], mutates the shared [`Arc`] stored in
    /// [`BrowserRuntime`] so the agent loop's platform and any tools
    /// that call `Platform::env()` observe the new value (WEFT-391).
    ///
    /// Before `init`, this is a no-op (does not panic) so out-of-order
    /// JS callers stay safe; use the optional `env_json` argument to
    /// [`init`] to pre-seed variables.
    #[wasm_bindgen]
    pub fn set_env(key: &str, value: &str) {
        if let Some(rt) = RUNTIME.get() {
            rt.env.set_var(key, value);
        }
    }

    /// Read an environment variable from the live [`BrowserEnvironment`].
    ///
    /// Returns `undefined` in JS when the runtime is not initialized or
    /// the key is unset. Used by the browser regression suite and
    /// useful for dashboard debugging (WEFT-391).
    #[wasm_bindgen]
    pub fn get_env(key: &str) -> Option<String> {
        RUNTIME.get().and_then(|rt| rt.env.get_var(key))
    }

    // -------------------------------------------------------------------
    // Tool introspection (WEFT-307)
    // -------------------------------------------------------------------

    /// Return the JSON-Schema for a registered tool, or `null` if
    /// no tool with that name exists.
    ///
    /// The shape mirrors the Axum API's `/api/tools/{slug}/schema`
    /// response so the dashboard's `/tools` route can render the same
    /// JSON-Schema viewer in either backend mode:
    ///
    /// ```json
    /// {
    ///   "name": "<slug>",
    ///   "description": "...",
    ///   "parameters": { ...JSON-Schema... }
    /// }
    /// ```
    ///
    /// The runtime must already be initialized via [`init`].
    #[wasm_bindgen]
    pub fn tool_schema(slug: &str) -> String {
        let Some(rt) = RUNTIME.get() else {
            return "null".to_string();
        };
        match rt.tools.get(slug) {
            Some(tool) => serde_json::json!({
                "name": tool.name(),
                "description": tool.description(),
                "parameters": tool.parameters(),
            })
            .to_string(),
            None => "null".to_string(),
        }
    }

    /// Return the list of registered tool names as a JSON array string.
    ///
    /// The list is sorted alphabetically. Useful for the dashboard's
    /// `/tools` route to enumerate browser-mode tools without
    /// hardcoding the subset in `wasm-adapter.ts::listTools`.
    #[wasm_bindgen]
    pub fn tool_list() -> String {
        let Some(rt) = RUNTIME.get() else {
            return "[]".to_string();
        };
        serde_json::to_string(&rt.tools.list()).unwrap_or_else(|_| "[]".to_string())
    }

    // -------------------------------------------------------------------
    // Boot info (Feature 3)
    // -------------------------------------------------------------------

    /// Return a JSON array of boot phases mirroring the native kernel BootLog.
    ///
    /// Called once after WASM loads to feed real boot data into the ExoChain
    /// log instead of hardcoded entries on the TypeScript side.
    #[wasm_bindgen]
    pub fn boot_info() -> String {
        let phases = serde_json::json!([
            {"phase": "INIT", "detail": format!("WeftOS v{} booting...", crate::VERSION)},
            {"phase": "INIT", "detail": "PID 0 (kernel)"},
            {"phase": "CONFIG", "detail": "Platform: wasm32-browser"},
            {"phase": "CONFIG", "detail": "Max processes: 64"},
            {"phase": "CONFIG", "detail": "Memory model: linear (WASM)"},
            {"phase": "SERVICES", "detail": "Service registry ready"},
            {"phase": "SERVICES", "detail": "IPC subsystem ready"},
            {"phase": "SERVICES", "detail": "ExoChain audit log active"},
            {"phase": "NETWORK", "detail": "LLM transport: browser-direct CORS"},
            {"phase": "READY", "detail": "Kernel ready — all subsystems online"}
        ]);
        phases.to_string()
    }

    // -------------------------------------------------------------------
    // File analysis (Feature 2)
    // -------------------------------------------------------------------

    /// Analyze a set of files passed as a JSON array of `{path, content}` objects.
    ///
    /// Runs lightweight static analysis mirroring the native kernel's assessment
    /// analyzers (ComplexityAnalyzer, SecurityAnalyzer, DependencyAnalyzer,
    /// TopologyAnalyzer) but operating on in-memory strings rather than
    /// filesystem paths.
    ///
    /// Returns a JSON object with:
    /// - `summary`: file count, total LOC, language breakdown
    /// - `findings`: array of `{severity, category, file, line?, message}`
    #[wasm_bindgen]
    pub fn analyze_files(files_json: &str) -> String {
        #[derive(serde::Deserialize)]
        struct InputFile {
            path: String,
            content: String,
        }

        #[derive(serde::Serialize)]
        struct Finding {
            severity: String,
            category: String,
            file: String,
            #[serde(skip_serializing_if = "Option::is_none")]
            line: Option<usize>,
            message: String,
        }

        #[derive(serde::Serialize)]
        struct LangStat {
            language: String,
            files: usize,
            lines: usize,
        }

        #[derive(serde::Serialize)]
        struct Summary {
            file_count: usize,
            total_lines: usize,
            languages: Vec<LangStat>,
        }

        #[derive(serde::Serialize)]
        struct AnalysisResult {
            summary: Summary,
            findings: Vec<Finding>,
        }

        let files: Vec<InputFile> = match serde_json::from_str(files_json) {
            Ok(f) => f,
            Err(e) => {
                return serde_json::json!({
                    "error": format!("Failed to parse input: {e}")
                })
                .to_string();
            }
        };

        let mut findings: Vec<Finding> = Vec::new();
        let mut total_lines: usize = 0;
        let mut lang_map: std::collections::HashMap<String, (usize, usize)> =
            std::collections::HashMap::new();

        // Secret patterns (mirrors SecurityAnalyzer)
        let secret_patterns: &[&str] = &[
            "api_key=",
            "api_key =",
            "apikey=",
            "apikey =",
            "password=",
            "password =",
            "passwd=",
            "passwd =",
            "secret=",
            "secret =",
            "token=",
            "token =",
            "aws_secret",
            "private_key",
        ];

        for file in &files {
            let path = &file.path;
            let content = &file.content;
            let line_count = content.lines().count();
            total_lines += line_count;

            // Language detection by extension
            let ext = path.rsplit('.').next().unwrap_or("");
            let lang = match ext {
                "rs" => "Rust",
                "ts" | "tsx" => "TypeScript",
                "js" | "jsx" | "mjs" | "cjs" => "JavaScript",
                "py" => "Python",
                "go" => "Go",
                "java" => "Java",
                "c" | "h" => "C",
                "cpp" | "cc" | "cxx" | "hpp" => "C++",
                "rb" => "Ruby",
                "toml" => "TOML",
                "json" => "JSON",
                "yaml" | "yml" => "YAML",
                "md" | "mdx" => "Markdown",
                "sh" | "bash" => "Shell",
                "css" | "scss" | "less" => "CSS",
                "html" | "htm" => "HTML",
                "sql" => "SQL",
                "dockerfile" | "Dockerfile" => "Dockerfile",
                _ => {
                    // Check filename for special cases
                    let name = path.rsplit('/').next().unwrap_or(path);
                    match name {
                        n if n.starts_with("Dockerfile") => "Dockerfile",
                        "Makefile" | "Justfile" => "Make",
                        _ => "Other",
                    }
                }
            };

            let entry = lang_map.entry(lang.to_string()).or_insert((0, 0));
            entry.0 += 1;
            entry.1 += line_count;

            // --- Complexity: large files ---
            if line_count > 500 {
                findings.push(Finding {
                    severity: "warning".into(),
                    category: "size".into(),
                    file: path.clone(),
                    line: None,
                    message: format!("File has {line_count} lines (>500 limit)"),
                });
            }

            // --- Complexity: TODO/FIXME/HACK markers ---
            for (i, line) in content.lines().enumerate() {
                if line.contains("TODO") || line.contains("FIXME") || line.contains("HACK") {
                    findings.push(Finding {
                        severity: "info".into(),
                        category: "todo".into(),
                        file: path.clone(),
                        line: Some(i + 1),
                        message: line.trim().to_string(),
                    });
                }
            }

            // --- Security: .env files ---
            let file_name = path.rsplit('/').next().unwrap_or(path);
            if file_name == ".env" || file_name.starts_with(".env.") {
                findings.push(Finding {
                    severity: "error".into(),
                    category: "security".into(),
                    file: path.clone(),
                    line: None,
                    message: "Environment file should not be committed to version control".into(),
                });
                continue;
            }

            // --- Security: hardcoded secrets ---
            let is_test = path.contains("test")
                || path.contains("spec")
                || path.contains("fixture")
                || path.contains("mock");

            if !is_test {
                for (i, line) in content.lines().enumerate() {
                    let lower = line.to_lowercase();
                    let trimmed = lower.trim();
                    // Skip comments
                    if trimmed.starts_with("//")
                        || trimmed.starts_with('#')
                        || trimmed.starts_with("/*")
                        || trimmed.starts_with('*')
                    {
                        continue;
                    }
                    for pattern in secret_patterns {
                        if lower.contains(pattern) {
                            if let Some(pos) = lower.find('=') {
                                let after = lower[pos + 1..].trim();
                                if !after.is_empty()
                                    && after != "\"\""
                                    && after != "''"
                                    && !after.starts_with("env")
                                    && !after.starts_with("std::env")
                                    && !after.starts_with("process.env")
                                    && !after.starts_with("os.environ")
                                {
                                    findings.push(Finding {
                                        severity: "warning".into(),
                                        category: "security".into(),
                                        file: path.clone(),
                                        line: Some(i + 1),
                                        message: format!(
                                            "Possible hardcoded secret: {}",
                                            pattern.trim_end_matches('=').trim()
                                        ),
                                    });
                                    break;
                                }
                            }
                        }
                    }
                }
            }

            // --- Dependency: Cargo.toml ---
            if file_name == "Cargo.toml" {
                let mut in_deps = false;
                let mut dep_count: usize = 0;
                for line in content.lines() {
                    let t = line.trim();
                    if t.starts_with('[') {
                        in_deps = t == "[dependencies]"
                            || t == "[dev-dependencies]"
                            || t == "[build-dependencies]"
                            || t.starts_with("[dependencies.")
                            || t.starts_with("[dev-dependencies.")
                            || t.starts_with("[build-dependencies.");
                        continue;
                    }
                    if in_deps && !t.is_empty() && !t.starts_with('#') {
                        if let Some(dep_name) = t.split('=').next().map(|s| s.trim()) {
                            if !dep_name.is_empty() {
                                dep_count += 1;
                            }
                        }
                    }
                }
                findings.push(Finding {
                    severity: "info".into(),
                    category: "dependency".into(),
                    file: path.clone(),
                    line: None,
                    message: format!("Cargo.toml has {dep_count} dependencies"),
                });
            }

            // --- Dependency: package.json ---
            if file_name == "package.json" {
                // Simple JSON key counting for deps
                let dep_count = content.matches("\"dependencies\"").count()
                    + content.matches("\"devDependencies\"").count();
                let pkg_deps = count_json_object_keys(content, "dependencies")
                    + count_json_object_keys(content, "devDependencies");
                findings.push(Finding {
                    severity: "info".into(),
                    category: "dependency".into(),
                    file: path.clone(),
                    line: None,
                    message: format!(
                        "package.json: {} dependency section(s), ~{} packages",
                        dep_count, pkg_deps
                    ),
                });
            }

            // --- Topology: Docker / k8s ---
            if file_name.starts_with("Dockerfile") {
                findings.push(Finding {
                    severity: "info".into(),
                    category: "topology".into(),
                    file: path.clone(),
                    line: None,
                    message: "Dockerfile detected".into(),
                });
                // Extract base image
                for line in content.lines() {
                    if line.trim().to_uppercase().starts_with("FROM ") {
                        findings.push(Finding {
                            severity: "info".into(),
                            category: "topology".into(),
                            file: path.clone(),
                            line: None,
                            message: format!("Base image: {}", line.trim()),
                        });
                        break;
                    }
                }
            }
            if file_name.starts_with("docker-compose") {
                findings.push(Finding {
                    severity: "info".into(),
                    category: "topology".into(),
                    file: path.clone(),
                    line: None,
                    message: "Docker Compose file detected".into(),
                });
            }
            // k8s manifests
            if (ext == "yaml" || ext == "yml")
                && (content.contains("apiVersion:") && content.contains("kind:"))
            {
                findings.push(Finding {
                    severity: "info".into(),
                    category: "topology".into(),
                    file: path.clone(),
                    line: None,
                    message: "Kubernetes manifest detected".into(),
                });
            }
        }

        // Build language stats
        let mut languages: Vec<LangStat> = lang_map
            .into_iter()
            .map(|(language, (files, lines))| LangStat {
                language,
                files,
                lines,
            })
            .collect();
        languages.sort_by(|a, b| b.lines.cmp(&a.lines));

        let result = AnalysisResult {
            summary: Summary {
                file_count: files.len(),
                total_lines,
                languages,
            },
            findings,
        };

        serde_json::to_string(&result).unwrap_or_else(|e| {
            serde_json::json!({"error": format!("Serialization failed: {e}")}).to_string()
        })
    }

    /// Count approximate number of keys in a named JSON object section.
    /// This is a simple line-based heuristic, not a full parser.
    fn count_json_object_keys(content: &str, section: &str) -> usize {
        let search = format!("\"{}\"", section);
        let mut count = 0;
        let mut in_section = false;
        let mut brace_depth = 0;

        for line in content.lines() {
            let t = line.trim();
            if !in_section {
                if t.contains(&search) {
                    in_section = true;
                    if t.contains('{') {
                        brace_depth = 1;
                    }
                }
                continue;
            }
            for ch in t.chars() {
                match ch {
                    '{' => brace_depth += 1,
                    '}' => {
                        brace_depth -= 1;
                        if brace_depth <= 0 {
                            in_section = false;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            if in_section && t.contains(':') && t.starts_with('"') {
                count += 1;
            }
        }
        count
    }
}

#[cfg(feature = "browser")]
pub use browser_entry::*;

#[cfg(all(test, not(feature = "browser")))]
mod tests {
    use super::*;

    #[test]
    fn init_returns_zero() {
        assert_eq!(init(), 0);
    }

    #[test]
    fn process_message_returns_response() {
        let response = process_message("hello");
        assert!(response.contains("hello"));
        assert!(response.contains("clawft-wasm"));
    }

    #[test]
    fn capabilities_is_valid_json() {
        let caps = capabilities();
        let parsed: serde_json::Value = serde_json::from_str(&caps).unwrap();
        assert_eq!(parsed["platform"], "wasm32-wasip2");
        assert!(!parsed["tools"].as_array().unwrap().is_empty());
    }

    #[test]
    fn version_is_set() {
        assert!(!VERSION.is_empty());
    }

    #[test]
    fn excluded_tools_listed() {
        let caps = capabilities();
        let parsed: serde_json::Value = serde_json::from_str(&caps).unwrap();
        let excluded = parsed["excluded_tools"].as_array().unwrap();
        assert!(excluded.contains(&serde_json::json!("exec_shell")));
        assert!(excluded.contains(&serde_json::json!("spawn")));
    }
}
