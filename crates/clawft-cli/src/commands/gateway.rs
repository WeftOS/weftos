//! `weft gateway` -- start channels and the agent processing loop.
//!
//! The gateway initializes configured channel plugins (Telegram, Slack,
//! Discord), starts them in background tasks, wires the [`AgentLoop`] for
//! message processing, and runs an outbound dispatch loop that routes
//! responses back to the originating channel.
//!
//! # Lifecycle
//!
//! ```text
//! 1. Load config & bootstrap AppContext (bus, sessions, tools, pipeline)
//! 2. Register + init enabled channel factories
//! 3. Start all channels (each in its own tokio task)
//! 4. Start background services (CronService, HeartbeatService)
//! 5. Start MCP config file watcher (WEFT-493; drain-and-swap on edit)
//! 6. Spawn the agent loop (consumes inbound, produces outbound)
//! 7. Spawn the outbound dispatch loop (routes outbound to channels)
//! 8. Wait for Ctrl+C, then gracefully shut everything down
//! ```
//!
//! # Example
//!
//! ```text
//! weft gateway
//! weft gateway --config /path/to/config.json
//! ```

use std::path::PathBuf;
use std::sync::Arc;

use clap::Args;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

#[cfg(feature = "channels")]
use clawft_channels::PluginHost;
#[cfg(feature = "channels")]
use clawft_channels::discord::DiscordChannelFactory;
#[cfg(feature = "channels")]
use clawft_channels::slack::SlackChannelFactory;
#[cfg(feature = "channels")]
use clawft_channels::telegram::TelegramChannelFactory;
#[cfg(all(feature = "channels", feature = "api"))]
use clawft_channels::web::{WebChannelFactory, WebPublisher};
use clawft_core::bootstrap::AppContext;
use clawft_platform::NativePlatform;
#[cfg(feature = "services")]
use clawft_services::cron_service::CronService;
#[cfg(feature = "services")]
use clawft_services::heartbeat::HeartbeatService;

#[cfg(feature = "channels")]
use crate::markdown::dispatch::MarkdownDispatcher;

#[cfg(feature = "channels")]
use super::make_channel_host;

#[cfg(feature = "api")]
use clawft_services::api::bridge::{
    AgentBridge, BusBridge, ChannelBridge, ConfigBridge, MemoryBridge, SessionBridge, SkillBridge,
    ToolBridge, VoiceBridge,
};
#[cfg(feature = "api")]
use clawft_services::api::broadcaster::TopicBroadcaster;
#[cfg(feature = "api")]
use clawft_services::api::{AgentInfo, ApiState};

/// Arguments for the `weft gateway` subcommand.
#[derive(Args)]
pub struct GatewayArgs {
    /// Config file path (overrides auto-discovery).
    #[arg(short, long)]
    pub config: Option<String>,

    /// Enable intelligent routing (requires vector-memory feature).
    #[arg(long)]
    pub intelligent_routing: bool,

    /// Serve the API over plain HTTP on a non-loopback address.
    ///
    /// A bearer token grants the full API, so a non-loopback bind is
    /// refused unless TLS is terminated in front of the gateway and this
    /// is set (or `gateway.dangerously_plain_http`).
    #[arg(long)]
    pub dangerously_plain_http: bool,

    /// Serve the built cog-manager web console from this directory under
    /// `/console/` (overrides `gateway.staticDir`).
    #[arg(long)]
    pub static_dir: Option<String>,
}

/// Resolve the cron JSONL storage path.
///
/// Tries `~/.clawft/cron.jsonl`, falls back to `~/.nanobot/cron.jsonl`.
#[cfg(feature = "services")]
fn resolve_cron_storage_path() -> std::path::PathBuf {
    if let Some(home) = dirs::home_dir() {
        let clawft_path = home.join(".clawft").join("cron.jsonl");
        if clawft_path.parent().is_some_and(|p| p.exists()) {
            return clawft_path;
        }
        let nanobot_path = home.join(".nanobot").join("cron.jsonl");
        if nanobot_path.parent().is_some_and(|p| p.exists()) {
            return nanobot_path;
        }
        return clawft_path;
    }
    std::path::PathBuf::from("cron.jsonl")
}

/// Run the gateway command.
///
/// Loads configuration, bootstraps the [`AppContext`], registers all
/// enabled channels, starts them, then runs the agent loop and outbound
/// dispatch loop until Ctrl+C triggers graceful shutdown.
pub async fn run(args: GatewayArgs) -> anyhow::Result<()> {
    // If the channels feature is disabled, bail early with a helpful message.
    #[cfg(not(feature = "channels"))]
    {
        let _ = args;
        anyhow::bail!(
            "the gateway command requires the 'channels' feature. \
             Rebuild with: cargo build -p clawft-cli --features channels"
        );
    }

    #[cfg(feature = "channels")]
    {
        run_with_channels(args).await
    }
}

/// Inner implementation when the `channels` feature is enabled.
#[cfg(feature = "channels")]
async fn run_with_channels(args: GatewayArgs) -> anyhow::Result<()> {
    let platform = Arc::new(NativePlatform::new());
    let mut loaded = super::load_config_layered(&*platform, args.config.as_deref()).await?;
    loaded.config.gateway.dangerously_plain_http |= args.dangerously_plain_http;
    if args.static_dir.is_some() {
        loaded.config.gateway.static_dir = args.static_dir.clone();
    }
    // Prefer explicit `--config` / CLAWFT_CONFIG, else discovery chain.
    let config_watch_path = args
        .config
        .as_ref()
        .map(PathBuf::from)
        .or_else(|| super::discover_config_path(&*platform));
    run_with_config(
        loaded.config,
        args.intelligent_routing,
        None,
        Some(loaded.global_routing),
        loaded.workspace_routing,
        config_watch_path,
    )
    .await
}

/// `/console/` static dir, CSP origins and the tailnet-identity mint, from
/// `gateway.*` config. Everything is off unless configured.
#[cfg(all(feature = "channels", feature = "api"))]
fn console_options(
    config: &clawft_types::config::Config,
) -> clawft_services::api::console::ConsoleOptions {
    use clawft_services::api::console::{ConsoleOptions, DaemonTokenMinter, TailnetMint, TailscaleCli};
    let g = &config.gateway;
    let t = &g.tailnet_identity;
    let tailnet = if t.enabled {
        if t.allowed_logins.is_empty() {
            warn!("gateway.tailnetIdentity.enabled but allowedLogins is empty: nobody can mint");
        }
        let minter = DaemonTokenMinter(Arc::new(clawft_services::api::DaemonKernelFacade::new()));
        Some(Arc::new(TailnetMint::new(
            &t.allowed_logins,
            t.ttl_secs,
            &g.cors_origins,
            Arc::new(TailscaleCli::new(t.tailscale_bin.clone())),
            Arc::new(minter),
        )))
    } else {
        None
    };
    ConsoleOptions {
        static_dir: g.static_dir.clone(),
        connect_src: g.console_connect_src.clone(),
        tailnet,
    }
}

/// Refuse to serve the API on a non-loopback address over plain HTTP unless
/// the operator opted in (ADR-102). Runs first in [`run_with_config`].
#[cfg(feature = "channels")]
fn check_api_bind(config: &clawft_types::config::Config) -> anyhow::Result<()> {
    #[cfg(feature = "api")]
    if config.gateway.api_enabled {
        clawft_services::api::auth::validate_bind_policy(
            &config.gateway.host,
            config.gateway.dangerously_plain_http,
        )
        .map_err(|e| anyhow::anyhow!(e))?;
    }
    #[cfg(not(feature = "api"))]
    let _ = config;
    Ok(())
}

/// Run the gateway with a pre-loaded [`Config`].
///
/// This is the shared inner function used by both `weft gateway` and
/// `weft ui`. The `static_dir` parameter, when `Some`, enables SPA-style
/// static file serving for the built frontend.
///
/// `global_routing` / `workspace_routing` (WEFT-10) feed the
/// PermissionResolver ceiling when provided.
///
/// `config_watch_path` (WEFT-493) is the file the MCP config watcher
/// observes for hot-reload. When `None`, discovery falls back to the
/// platform config path, then `~/.clawft/config.json`.
#[cfg(feature = "channels")]
pub async fn run_with_config(
    config: clawft_types::config::Config,
    intelligent_routing: bool,
    static_dir: Option<String>,
    global_routing: Option<clawft_types::routing::RoutingConfig>,
    workspace_routing: Option<clawft_types::routing::RoutingConfig>,
    config_watch_path: Option<PathBuf>,
) -> anyhow::Result<()> {
    // Refuse an unsafe API bind before anything is started or touched.
    check_api_bind(&config)?;

    info!("starting weft gateway");

    let platform = Arc::new(NativePlatform::new());

    // ── Bootstrap AppContext (bus, sessions, tools, pipeline) ────────
    let mut ctx = AppContext::new(config.clone(), platform.clone())
        .await
        .map_err(|e| anyhow::anyhow!("failed to bootstrap app context: {e}"))?;
    if let Some(global) = global_routing {
        ctx = ctx.with_routing_layers(global, workspace_routing);
    }

    // WEFT-185: construct a shared AgentBus so multi-agent / A2A hosts
    // can register workers and run SwarmCoordinator workflows.
    let _agent_bus = ctx.enable_agent_bus(None);

    // Register core tools (built-in + MCP proxied + delegation).
    super::register_core_tools(ctx.tools_mut(), &config, platform.clone()).await;

    // Register message tool (needs bus reference, cannot go in register_all).
    let bus_ref = ctx.bus().clone();
    ctx.tools_mut()
        .register(Arc::new(clawft_tools::message_tool::MessageTool::new(
            bus_ref,
        )));

    info!(tools = ctx.tools().len(), "tool registry initialized");

    // ── MCP config hot-reload watcher (WEFT-493 / WEFT-187) ───────────
    //
    // Long-lived gateway is the production host for tools.mcp_servers.
    // Seed McpServerManager from the user config file and keep the
    // notify watcher alive until shutdown so `weft mcp add/remove`
    // (atomic write) is observed without a process restart.
    #[cfg(feature = "services")]
    let _mcp_config_watcher = {
        let watch_path = config_watch_path
            .or_else(|| super::discover_config_path(&*platform))
            .or_else(|| dirs::home_dir().map(|h| h.join(".clawft").join("config.json")));
        match watch_path {
            Some(path) => {
                match clawft_services::mcp::boot_mcp_manager_with_watcher(path).await {
                    Ok(boot) => {
                        info!(
                            path = %boot.path.display(),
                            seeded = boot.initial.added,
                            "MCP config watcher armed for gateway lifetime"
                        );
                        Some(boot)
                    }
                    Err(e) => {
                        warn!(
                            error = %e,
                            "failed to start MCP config watcher; hot-reload disabled this run"
                        );
                        None
                    }
                }
            }
            None => {
                warn!("no config path for MCP watcher; hot-reload disabled this run");
                None
            }
        }
    };
    #[cfg(not(feature = "services"))]
    let _ = config_watch_path;

    // Wire the live LLM-backed pipeline so real provider calls work.
    ctx.enable_live_llm();

    // Intelligent routing.
    if intelligent_routing {
        #[cfg(feature = "vector-memory")]
        {
            info!("intelligent routing enabled for gateway");
        }
        #[cfg(not(feature = "vector-memory"))]
        {
            anyhow::bail!(
                "intelligent routing requires the 'vector-memory' feature. \
                 Rebuild with: cargo build --features vector-memory"
            );
        }
    }

    // Clone shared references before consuming AppContext.
    let bus = ctx.bus().clone();

    // ── Cancellation token (shared by all background tasks) ─────────
    let cancel = CancellationToken::new();

    // ── API server (optional, feature-gated) ────────────────────────
    //
    // The broadcaster is created here so it can be shared between the
    // API (WebSocket/SSE handlers) and the outbound dispatch loop.
    #[cfg(feature = "api")]
    let api_broadcaster: Option<Arc<TopicBroadcaster>> = if config.gateway.api_enabled {
        Some(Arc::new(TopicBroadcaster::new()))
    } else {
        None
    };

    #[cfg(not(feature = "api"))]
    let api_broadcaster: Option<()> = None;

    // WEFT-306: wire the render_ui tool to the broadcaster so agent-
    // emitted CanvasCommands fan out to dashboard `/canvas` clients on
    // the `canvas` topic. `register` replaces the unwired instance
    // installed by `clawft_tools::register_all`. We must do this BEFORE
    // `build_api_state(&ctx, ...)` snapshots the tool registry.
    #[cfg(feature = "api")]
    if let Some(ref broadcaster) = api_broadcaster {
        let canvas_publisher: Arc<dyn clawft_tools::render_ui::CanvasPublisher> =
            Arc::new(BroadcasterCanvasPublisher {
                broadcaster: broadcaster.clone(),
            });
        ctx.tools_mut().register(Arc::new(
            clawft_tools::render_ui::RenderUiTool::with_publisher(canvas_publisher),
        ));
        debug!("render_ui tool wired to canvas topic broadcaster");
    }

    #[cfg(feature = "api")]
    let api_handle: Option<tokio::task::JoinHandle<()>> = if config.gateway.api_enabled {
        let broadcaster = api_broadcaster.clone().expect("broadcaster created above");
        let mut api_state = build_api_state(&ctx, &config, broadcaster);
        api_state.mcp = super::mcp_server::build_gateway_mount(&config, platform.clone()).await;
        let cors_origins = config.gateway.cors_origins.clone();
        let api_host = config.gateway.host.clone();
        let port = config.gateway.api_port;
        let addr = format!("{api_host}:{port}");
        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .map_err(|e| anyhow::anyhow!("failed to bind API listener on {addr}: {e}"))?;
        info!(addr = %addr, "REST/WS API listening");
        eprintln!("API listening on http://{}:{}", api_host, port);
        let api_cancel = cancel.clone();
        let api_static_dir = static_dir.clone();
        let console = console_options(&config);
        let handle = tokio::spawn(async move {
            if let Err(e) = clawft_services::api::serve_with(
                listener,
                api_state,
                &cors_origins,
                api_static_dir.as_deref(),
                console,
                api_cancel.cancelled_owned(),
            )
            .await
            {
                error!(error = %e, "API server exited with error");
            }
        });
        Some(handle)
    } else {
        None
    };

    #[cfg(not(feature = "api"))]
    let api_handle: Option<tokio::task::JoinHandle<()>> = None;

    // ── Channel setup ───────────────────────────────────────────────
    let host = make_channel_host(bus.clone());
    let plugin_host = Arc::new(PluginHost::new(host));

    let mut any_channel = false;

    // Telegram
    let telegram_has_token = !config.channels.telegram.token.is_empty()
        || config
            .channels
            .telegram
            .token_env
            .as_ref()
            .is_some_and(|v| !v.is_empty());
    if config.channels.telegram.enabled && telegram_has_token {
        plugin_host
            .register_factory(Arc::new(TelegramChannelFactory))
            .await;
        // In-process hand-off to the channel factory: the token must survive.
        let telegram_config = clawft_types::secret::to_value_exposing_secrets(&config.channels.telegram)?;
        plugin_host
            .init_channel("telegram", &telegram_config)
            .await
            .map_err(|e| anyhow::anyhow!("failed to init telegram channel: {e}"))?;
        info!("telegram channel initialized");
        any_channel = true;
    }

    // Slack
    let slack_has_token = !config.channels.slack.bot_token.is_empty()
        || config
            .channels
            .slack
            .bot_token_env
            .as_ref()
            .is_some_and(|v| !v.is_empty());
    if config.channels.slack.enabled && slack_has_token {
        plugin_host
            .register_factory(Arc::new(SlackChannelFactory))
            .await;
        let slack_config = clawft_types::secret::to_value_exposing_secrets(&config.channels.slack)?;
        plugin_host
            .init_channel("slack", &slack_config)
            .await
            .map_err(|e| anyhow::anyhow!("failed to init slack channel: {e}"))?;
        info!("slack channel initialized");
        any_channel = true;
    }

    // Discord
    let discord_has_token = !config.channels.discord.token.is_empty()
        || config
            .channels
            .discord
            .token_env
            .as_ref()
            .is_some_and(|v| !v.is_empty());
    if config.channels.discord.enabled && discord_has_token {
        plugin_host
            .register_factory(Arc::new(DiscordChannelFactory))
            .await;
        let discord_config = clawft_types::secret::to_value_exposing_secrets(&config.channels.discord)?;
        plugin_host
            .init_channel("discord", &discord_config)
            .await
            .map_err(|e| anyhow::anyhow!("failed to init discord channel: {e}"))?;
        info!("discord channel initialized");
        any_channel = true;
    }

    // Web channel — register when the API (and its broadcaster) is enabled.
    #[cfg(feature = "api")]
    if config.gateway.api_enabled
        && let Some(ref broadcaster) = api_broadcaster
    {
        let publisher: Arc<dyn WebPublisher> = Arc::new(BroadcasterPublisher {
            broadcaster: broadcaster.clone(),
        });
        // WEFT-163: the web channel's `is_allowed` defers to the
        // gateway's auth middleware. M2-A wired the auth middleware
        // unconditionally on the API router, so when the API is up
        // the auth gate is also up. If a future build flag turns
        // auth off, this flag must follow it.
        let auth_enabled = true;
        plugin_host
            .register_factory(Arc::new(WebChannelFactory::new(publisher, auth_enabled)))
            .await;
        plugin_host
            .init_channel("web", &serde_json::json!({}))
            .await
            .map_err(|e| anyhow::anyhow!("failed to init web channel: {e}"))?;
        info!("web channel initialized");
        any_channel = true;
    }

    if !any_channel && !config.gateway.api_enabled {
        anyhow::bail!(
            "no channels are enabled and API is disabled. \
             Enable at least one channel (e.g., telegram, slack, discord) \
             with credentials, or set gateway.api_enabled = true."
        );
    }

    // ── Start all channels ──────────────────────────────────────────
    let start_results = plugin_host.start_all().await;
    for (name, result) in &start_results {
        match result {
            Ok(()) => info!(channel = %name, "channel started"),
            Err(e) => {
                error!(channel = %name, error = %e, "channel failed to start");
            }
        }
    }

    let started_count = start_results.iter().filter(|(_, r)| r.is_ok()).count();
    if started_count == 0 && !config.gateway.api_enabled {
        anyhow::bail!("no channels started successfully and API is disabled");
    }

    // ── Background services ──────────────────────────────────────────

    #[cfg(feature = "services")]
    let (cron_handle, heartbeat_handle) = {
        // CronService
        let inbound_tx = bus.inbound_sender();
        let cron_storage = resolve_cron_storage_path();
        let cron_handle = match CronService::new(cron_storage, inbound_tx.clone()).await {
            Ok(cron_service) => {
                let cron_cancel = cancel.clone();
                let svc = std::sync::Arc::new(cron_service);
                let svc_clone = svc.clone();
                info!("cron service initialized");
                Some(tokio::spawn(async move {
                    if let Err(e) = svc_clone.start(cron_cancel).await {
                        error!(error = %e, "cron service exited with error");
                    }
                }))
            }
            Err(e) => {
                warn!(error = %e, "failed to initialize cron service, skipping");
                None
            }
        };

        // HeartbeatService
        let heartbeat_handle = if config.gateway.heartbeat_interval_minutes > 0 {
            let svc = HeartbeatService::new(
                config.gateway.heartbeat_interval_minutes,
                config.gateway.heartbeat_prompt.clone(),
                inbound_tx,
            );
            let hb_cancel = cancel.clone();
            info!(
                interval_minutes = config.gateway.heartbeat_interval_minutes,
                "heartbeat service started"
            );
            Some(tokio::spawn(async move {
                if let Err(e) = svc.start(hb_cancel).await {
                    error!(error = %e, "heartbeat service exited with error");
                }
            }))
        } else {
            debug!("heartbeat service disabled (interval=0)");
            None
        };

        (cron_handle, heartbeat_handle)
    };

    #[cfg(not(feature = "services"))]
    let (cron_handle, heartbeat_handle): (
        Option<tokio::task::JoinHandle<()>>,
        Option<tokio::task::JoinHandle<()>>,
    ) = {
        debug!("services feature disabled, skipping cron and heartbeat");
        (None, None)
    };

    // ── Agent loop (inbound processing) ─────────────────────────────
    // WEFT-67: attach skill-autogen when opted in via skills.autogen.enabled.
    // WEFT-66: wire active TrajectoryLearner into AutogenConfig.
    let trajectory_learner = ctx.trajectory_learner();
    let mut agent = ctx.into_agent_loop().with_cancel(cancel.clone());
    if config.skills.autogen.enabled {
        use clawft_core::agent::skill_autogen::{AutogenConfig, PatternDetector};
        use std::sync::Mutex;
        let mut autogen_cfg = AutogenConfig {
            enabled: true,
            threshold: config.skills.autogen.threshold,
            max_pending: config.skills.autogen.max_pending,
            install_dir: None,
            learner: None,
        };
        if let Some(learner) = trajectory_learner {
            autogen_cfg = autogen_cfg.with_learner(learner);
        }
        let detector = Arc::new(Mutex::new(PatternDetector::new(autogen_cfg)));
        agent = agent.with_autogen(detector);
        info!(
            threshold = config.skills.autogen.threshold,
            max_pending = config.skills.autogen.max_pending,
            learning = config.pipeline.learner == "trajectory",
            "skill autogen enabled (gateway pattern detection active)"
        );
    }

    // WEFT-348: attach Phase 4 skill auto-promoter when enabled.
    if config.skills.promotion.enabled {
        use clawft_core::agent::skill_autogen::{SkillPromotionConfig, SkillPromoter};
        use std::sync::Mutex;
        let promo_cfg = SkillPromotionConfig::from_types(&config.skills.promotion);
        let promoter = Arc::new(Mutex::new(SkillPromoter::new(promo_cfg)));
        agent = agent.with_skill_promoter(promoter);
        info!(
            threshold = config.skills.promotion.threshold,
            "skill auto-promotion enabled (gateway, .claude/skills → .clawft/skills)"
        );
    }

    let agent_handle = tokio::spawn(async move {
        if let Err(e) = agent.run().await {
            error!(error = %e, "agent loop exited with error");
        }
    });

    // ── Outbound dispatch loop ──────────────────────────────────────
    let cancel_for_dispatch = cancel.clone();
    let bus_for_dispatch = bus.clone();
    let plugin_host_for_dispatch = plugin_host.clone();
    let md_dispatcher = MarkdownDispatcher::new();

    // Clone the broadcaster for the dispatch loop (if API is enabled).
    #[cfg(feature = "api")]
    let dispatch_broadcaster = api_broadcaster.clone();

    let dispatch_handle = tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;

                _ = cancel_for_dispatch.cancelled() => {
                    info!("outbound dispatch loop shutting down");
                    break;
                }

                msg = bus_for_dispatch.consume_outbound() => {
                    match msg {
                        Some(mut outbound) => {
                            debug!(
                                channel = %outbound.channel,
                                chat_id = %outbound.chat_id,
                                "dispatching outbound message"
                            );
                            // Convert markdown to channel-specific format.
                            outbound.content = md_dispatcher.convert(
                                &outbound.channel,
                                &outbound.content,
                            );

                            // Route through the plugin host — all channels
                            // (telegram, slack, discord, web) are registered.
                            if let Err(e) = plugin_host_for_dispatch
                                .send_to_channel(&outbound)
                                .await
                            {
                                error!(
                                    channel = %outbound.channel,
                                    chat_id = %outbound.chat_id,
                                    error = %e,
                                    "outbound dispatch failed"
                                );
                            }

                            // For non-web channels, also broadcast to
                            // WebSocket/SSE subscribers so the web dashboard
                            // can display messages from all channels.
                            // (Web channel messages are already broadcast by
                            // WebChannel::send().)
                            #[cfg(feature = "api")]
                            if outbound.channel != "web"
                                && let Some(ref bc) = dispatch_broadcaster
                            {
                                let topic = format!("sessions:{}", outbound.chat_id);
                                let msg = serde_json::json!({
                                    "type": "message",
                                    "role": "assistant",
                                    "content": &outbound.content,
                                    "session_key": &outbound.chat_id,
                                    "channel": &outbound.channel,
                                    "timestamp": chrono::Utc::now().to_rfc3339()
                                });
                                let bc = bc.clone();
                                let chat_id = outbound.chat_id.clone();
                                tokio::spawn(async move {
                                    bc.publish(&topic, msg).await;
                                    bc.publish("sessions", serde_json::json!({
                                        "type": "message_added",
                                        "session_key": &chat_id
                                    })).await;
                                });
                            }
                        }
                        None => {
                            info!("outbound bus closed, dispatch loop exiting");
                            break;
                        }
                    }
                }
            }
        }
    });

    let api_status = if config.gateway.api_enabled {
        " + API"
    } else {
        ""
    };
    info!(
        channels = started_count,
        api = config.gateway.api_enabled,
        "gateway running"
    );
    eprintln!(
        "gateway running ({started_count} channel{}{api_status}) -- press Ctrl+C to stop",
        if started_count == 1 { "" } else { "s" }
    );

    // ── Wait for shutdown signal ────────────────────────────────────
    tokio::signal::ctrl_c().await?;
    eprintln!("\nshutting down...");
    info!("received shutdown signal");

    // Spawn a force-exit handler: second Ctrl+C or 10s timeout kills the process.
    tokio::spawn(async {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                eprintln!("forced exit");
            }
            _ = tokio::time::sleep(std::time::Duration::from_secs(10)) => {
                eprintln!("shutdown timed out, forcing exit");
            }
        }
        std::process::exit(1);
    });

    // 1. Cancel the dispatch loop and channel tasks.
    cancel.cancel();

    // 2. Stop all channels (cancels their tasks).
    let stop_results = plugin_host.stop_all().await;
    for (name, result) in &stop_results {
        match result {
            Ok(()) => info!(channel = %name, "channel stopped"),
            Err(e) => {
                warn!(channel = %name, error = %e, "channel stop error");
            }
        }
    }

    // 3. Drop the plugin host so its Arc<ChannelHost> (which holds a bus
    //    clone) is released. Then drop our bus reference. Once all senders
    //    are gone, the agent loop's consume_inbound() returns None and exits.
    drop(plugin_host);
    drop(bus);

    // 4. Await background services.
    if let Some(h) = cron_handle {
        let _ = h.await;
    }
    if let Some(h) = heartbeat_handle {
        let _ = h.await;
    }

    // 5. Await background tasks.
    let _ = dispatch_handle.await;
    let _ = agent_handle.await;

    // 6. Await API server (if running).
    if let Some(h) = api_handle {
        let _ = h.await;
    }

    info!("gateway shutdown complete");
    Ok(())
}

/// Bridges [`TopicBroadcaster`] to the [`WebPublisher`] trait so the
/// [`WebChannel`] can publish messages to WebSocket/SSE subscribers.
#[cfg(all(feature = "api", feature = "channels"))]
struct BroadcasterPublisher {
    broadcaster: Arc<TopicBroadcaster>,
}

#[cfg(all(feature = "api", feature = "channels"))]
#[async_trait::async_trait]
impl WebPublisher for BroadcasterPublisher {
    async fn publish(&self, topic: &str, message: serde_json::Value) {
        self.broadcaster.publish(topic, message).await;
    }
}

/// Bridges [`TopicBroadcaster`] to the
/// [`clawft_tools::render_ui::CanvasPublisher`] trait so the
/// `render_ui` tool can fan validated CanvasCommands out to
/// `/canvas` clients via the `canvas` WebSocket topic (WEFT-306).
#[cfg(feature = "api")]
struct BroadcasterCanvasPublisher {
    broadcaster: Arc<TopicBroadcaster>,
}

#[cfg(feature = "api")]
#[async_trait::async_trait]
impl clawft_tools::render_ui::CanvasPublisher for BroadcasterCanvasPublisher {
    async fn publish(&self, topic: &str, message: serde_json::Value) {
        self.broadcaster.publish(topic, message).await;
    }
}

/// Build an [`ApiState`] from an [`AppContext`] by extracting shared Arc
/// references and wrapping them in bridge implementations.
///
/// The `broadcaster` parameter is the shared [`TopicBroadcaster`] that is
/// also passed to the outbound dispatch loop for publishing events.
///
/// Must be called BEFORE `ctx.into_agent_loop()`, which consumes the context.
#[cfg(all(feature = "api", feature = "channels"))]
fn build_api_state(
    ctx: &AppContext<NativePlatform>,
    config: &clawft_types::config::Config,
    broadcaster: Arc<TopicBroadcaster>,
) -> ApiState {
    use clawft_services::api::auth::DaemonTokenValidator;

    // One daemon facade serves kernel routes and token validation (ADR-102).
    let facade = Arc::new(clawft_services::api::DaemonKernelFacade::new());

    let tool_bridge = ToolBridge::new(ctx.tools_arc());
    let session_bridge = SessionBridge::new(ctx.sessions().clone());
    let bus_bridge = BusBridge::new(ctx.bus().clone());
    let skill_bridge = SkillBridge::new(ctx.skills().clone());
    let memory_bridge = MemoryBridge::new(ctx.memory().clone());
    // WEFT-168: enable save_config persistence when we can identify a
    // canonical config path. Default to `~/.clawft/config.json`; when
    // CLAWFT_CONFIG is set, honour it. If no home dir is available
    // (rare on locked-down hosts), fall back to the legacy read-only
    // bridge — save_config will then return an explicit error.
    let config_save_path = std::env::var("CLAWFT_CONFIG")
        .ok()
        .map(std::path::PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".clawft").join("config.json")));
    let config_bridge = match config_save_path {
        Some(path) => ConfigBridge::with_save_path(config.clone(), path),
        None => ConfigBridge::new(config.clone()),
    };
    let channel_bridge = ChannelBridge::from_config(&config.channels, config.gateway.api_enabled);

    // Discover agents from the 3-level hierarchy (workspace > user > builtin).
    let user_agents_dir = dirs::home_dir().map(|h| h.join(".clawft").join("agents"));
    let workspace_agents_dir = {
        let ws = config.workspace_path();
        let d = ws.join("agents");
        if d.is_dir() { Some(d) } else { None }
    };
    let agent_bridge = match clawft_core::agent::agents::AgentRegistry::discover(
        workspace_agents_dir.as_deref(),
        user_agents_dir.as_deref(),
        vec![],
    ) {
        Ok(registry) => {
            let infos: Vec<AgentInfo> = registry
                .list()
                .into_iter()
                .map(|def| AgentInfo {
                    name: def.name.clone(),
                    description: def.description.clone(),
                    model: def
                        .model
                        .clone()
                        .unwrap_or_else(|| config.agents.defaults.model.clone()),
                    skills: def.skills.clone(),
                })
                .collect();
            tracing::info!(count = infos.len(), "discovered agents for API");
            AgentBridge::new(infos)
        }
        Err(e) => {
            tracing::warn!(error = %e, "agent discovery failed, API will show empty list");
            AgentBridge::empty()
        }
    };

    // WEFT-218: wire runtime voice pipeline state to TopicBroadcaster so
    // dashboard clients subscribed to `voice:status` receive live updates
    // (not MSW-only). POST /api/voice/status and native producers both fan out.
    let voice_bridge = VoiceBridge::with_broadcaster(
        config.voice.clone(),
        config.providers.clone(),
        broadcaster.clone(),
    );

    ApiState {
        tools: Arc::new(tool_bridge),
        sessions: Arc::new(session_bridge),
        agents: Arc::new(agent_bridge),
        bus: Arc::new(bus_bridge),
        auth: Arc::new(DaemonTokenValidator::new(facade.clone())),
        skills: Arc::new(skill_bridge),
        memory: Arc::new(memory_bridge),
        config: Arc::new(config_bridge),
        channels: Arc::new(channel_bridge),
        voice: Arc::new(voice_bridge),
        broadcaster,
        // ADR-102 D6: facade routes forward to the kernel daemon; 503 when it is down.
        kernel_facade: facade,
        // WEFT-40/305: the agent pipeline's own ring, so the admin routing
        // history and /monitoring show live traffic, tokens and latency.
        routing_history: ctx.decision_history(),
        // WEFT-48/49: rate limiter from routing config. Not yet the same Arc as
        // TieredRouter (build_router_from_config keeps it internal); admin
        // metrics/flush still work for ops and tests. Shared wiring is a
        // follow-up if live traffic must appear on this surface.
        rate_limiter: Arc::new(clawft_core::pipeline::rate_limiter::RateLimiter::new(
            config.routing.rate_limiting.window_seconds,
            config.routing.rate_limiting.global_rate_limit_rpm,
        )),
        health_cache: Default::default(),
        mcp: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_args_defaults() {
        let args = GatewayArgs {
            config: None,
            intelligent_routing: false,
            dangerously_plain_http: false,
            static_dir: None,
        };
        assert!(args.config.is_none());
    }

    fn bind_config(host: &str, plain: bool) -> clawft_types::config::Config {
        let mut c = clawft_types::config::Config::default();
        c.gateway.api_enabled = true;
        c.gateway.host = host.into();
        c.gateway.dangerously_plain_http = plain;
        c
    }

    #[cfg(feature = "api")]
    #[test]
    fn api_bind_guard_classifies_loopback_by_ip() {
        for h in ["127.0.0.1", "127.0.0.2", "::1", "[::1]", "::ffff:127.0.0.1", "localhost"] {
            assert!(check_api_bind(&bind_config(h, false)).is_ok(), "{h}");
        }
        for h in ["0.0.0.0", "::", "192.0.2.10", "::ffff:192.0.2.10", "gw.example"] {
            let e = check_api_bind(&bind_config(h, false)).unwrap_err().to_string();
            assert!(e.contains("--dangerously-plain-http"), "{h}: {e}");
            assert!(check_api_bind(&bind_config(h, true)).is_ok(), "{h}");
        }
        // Not serving the API: nothing to guard.
        let mut off = bind_config("0.0.0.0", false);
        off.gateway.api_enabled = false;
        assert!(check_api_bind(&off).is_ok());
    }

    /// `run_with_config` itself enforces the guard, before it boots anything
    /// (so this touches no state): an unsafe bind returns the refusal.
    #[cfg(feature = "api")]
    #[tokio::test]
    async fn run_with_config_refuses_plain_http_off_loopback() {
        let err = run_with_config(bind_config("0.0.0.0", false), false, None, None, None, None)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("plain HTTP") && err.contains("--dangerously-plain-http"), "{err}");
    }

    #[test]
    fn gateway_args_with_config() {
        let args = GatewayArgs {
            config: Some("/tmp/gw-config.json".into()),
            intelligent_routing: false,
            dangerously_plain_http: false,
            static_dir: None,
        };
        assert_eq!(args.config.as_deref(), Some("/tmp/gw-config.json"));
    }

    #[cfg(feature = "services")]
    #[test]
    fn resolve_cron_storage_path_returns_valid() {
        let path = resolve_cron_storage_path();
        assert!(path.to_string_lossy().contains("cron.jsonl"));
    }
}
