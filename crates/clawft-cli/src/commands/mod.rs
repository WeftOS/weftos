//! CLI command implementations for `weft`.
//!
//! Each subcommand is implemented in its own module:
//!
//! - [`agent`] -- Interactive agent session or single-message mode.
//! - [`gateway`] -- Channel gateway (Telegram, Slack, etc.) + agent loop.
//! - [`help_cmd`] -- Topic-aware help (`weft help [topic]`).
//! - [`status`] -- Configuration diagnostics.

pub mod agent;
pub mod agent_daemon;
pub mod agents_cmd;
pub mod analyze_cmd;
pub mod assess_cmd;
pub mod channels;
pub mod config_cmd;
pub mod cos_cmd;
pub mod cron;
pub mod doctor;
pub mod daemon_conn;
pub mod daemon_fallback;
pub mod daemon_guard;
pub mod delegate_cmd;
pub mod gateway;
pub mod help_cmd;
pub mod mcp_cmd;
#[cfg(feature = "services")]
pub mod mcp_attach;
pub mod mcp_profile;
#[cfg(feature = "services")]
pub mod mcp_server;
#[cfg(feature = "services")]
pub mod mcp_window;
pub mod memory_cmd;
pub mod onboard;
pub mod plugin_registry;
pub mod plugins_cmd;
pub mod project_adopt;
pub mod project_cmd;
pub mod routing_cmd;
pub mod security_cmd;
pub mod sessions;
pub mod skills_cmd;
pub mod status;
pub mod swarm_cmd;
pub mod token_cmd;
pub mod tools_cmd;
#[cfg(feature = "api")]
pub mod ui_cmd;
#[cfg(feature = "voice")]
pub mod voice;
#[cfg(feature = "voice")]
pub mod voice_daemon_brain;
#[cfg(feature = "voice")]
pub mod voice_watch;
pub mod workspace_cmd;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use clawft_core::tools::registry::ToolRegistry;
use clawft_platform::Platform;
use clawft_types::config::Config;

/// Load configuration from the given path override or via auto-discovery.
///
/// Loaded configuration with split routing layers for PermissionResolver.
///
/// WEFT-10: `global_routing` + `workspace_routing` are kept separate so
/// `enforce_workspace_ceiling` can clamp elevated workspace grants.
/// `config` is the deep-merged view for agents/tools/providers.
#[derive(Debug, Clone)]
pub struct LoadedConfig {
    /// Deep-merged config (defaults filled) for non-security consumers.
    pub config: Config,
    /// System-wide routing (weave.toml + home JSON only).
    pub global_routing: clawft_types::routing::RoutingConfig,
    /// Workspace overlay routing, when cwd `.clawft/config.json` existed.
    pub workspace_routing: Option<clawft_types::routing::RoutingConfig>,
    /// WEFT-604: which config layer last set `agents.defaults.model`
    /// (`default`, `global:…`, `workspace:…`, or `cli:--config`).
    pub agents_model_source: &'static str,
}

/// If `config_override` is provided, loads from that path. Otherwise,
/// uses the platform's config discovery chain:
/// 1. `CLAWFT_CONFIG` env var
/// 2. `~/.clawft/config.json`
/// 3. `~/.nanobot/config.json`
///
/// Returns a default `Config` if no config file is found.
pub async fn load_config<P: Platform>(
    platform: &P,
    config_override: Option<&str>,
) -> anyhow::Result<Config> {
    Ok(load_config_layered(platform, config_override).await?.config)
}

/// Load config with split global / workspace routing layers (WEFT-10).
pub async fn load_config_layered<P: Platform>(
    platform: &P,
    config_override: Option<&str>,
) -> anyhow::Result<LoadedConfig> {
    if let Some(path_str) = config_override {
        let path = Path::new(path_str);
        if !platform.fs().exists(path).await {
            anyhow::bail!("config file not found: {path_str}");
        }
        let contents = platform
            .fs()
            .read_to_string(path)
            .await
            .map_err(|e| anyhow::anyhow!("failed to read config: {e}"))?;
        let value: serde_json::Value = serde_json::from_str(&contents)
            .map_err(|e| anyhow::anyhow!("failed to parse config: {e}"))?;
        let normalized = clawft_platform::config_loader::normalize_keys(value);
        let config: Config = serde_json::from_value(normalized)?;
        // WEFT-176: install white-label brand for Discord/CLI/banner surfaces.
        config.install_brand();
        let global_routing = config.routing.clone();
        return Ok(LoadedConfig {
            config,
            global_routing,
            workspace_routing: None,
            agents_model_source: "cli:--config",
        });
    }

    let layers = clawft_platform::config_loader::load_config_layers(
        platform.fs(),
        platform.env(),
    )
    .await
    .map_err(|e| anyhow::anyhow!("failed to load config: {e}"))?;

    let global: Config = serde_json::from_value(layers.global.clone())?;
    let workspace_routing = match layers.workspace.clone() {
        Some(ws_val) => {
            let ws: Config = serde_json::from_value(ws_val)?;
            Some(ws.routing)
        }
        None => None,
    };
    let agents_model_source = agents_model_layer_source(&layers);
    let config: Config = serde_json::from_value(layers.merged())?;
    // WEFT-176: install white-label brand for Discord/CLI/banner surfaces.
    config.install_brand();

    Ok(LoadedConfig {
        config,
        global_routing: global.routing,
        workspace_routing,
        agents_model_source,
    })
}

/// WEFT-604: name the config layer that last set `agents.defaults.model`.
///
/// Workspace overlay wins over the merged global layer (weave.toml + home
/// JSON). When neither sets the key, serde defaults apply → `"default"`.
fn agents_model_layer_source(
    layers: &clawft_platform::config_loader::ConfigLayers,
) -> &'static str {
    if let Some(ref ws) = layers.workspace
        && json_path_is_set(ws, &["agents", "defaults", "model"])
    {
        return "workspace:.clawft/config.json";
    }
    if json_path_is_set(&layers.global, &["agents", "defaults", "model"]) {
        return "global:weave.toml|home-config";
    }
    "default"
}

fn json_path_is_set(value: &serde_json::Value, path: &[&str]) -> bool {
    let mut cur = value;
    for key in path {
        match cur.get(*key) {
            Some(next) => cur = next,
            None => return false,
        }
    }
    match cur {
        serde_json::Value::String(s) => !s.is_empty(),
        serde_json::Value::Null => false,
        _ => true,
    }
}

/// Expand `~/` prefixes in workspace paths to the user's home directory.
pub fn expand_workspace(raw: &str) -> PathBuf {
    if let Some(rest) = raw.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(raw)
}

/// Discover the config file path (for display in `weft status`).
pub fn discover_config_path<P: Platform>(platform: &P) -> Option<PathBuf> {
    let home = platform.fs().home_dir();
    clawft_platform::config_loader::discover_config_path(platform.env(), home)
}

/// Options for [`register_core_tools_with`].
///
/// Defaults match historical agent/gateway behaviour (MCP re-export +
/// delegation when configured). `weft mcp-server` narrows these via
/// serve profiles (ADR-076 / WEFT-699).
#[derive(Debug, Clone, Copy)]
pub struct CoreToolRegisterOpts {
    /// Register proxied tools from configured non-`internal_only` MCP servers.
    pub register_mcp: bool,
    /// Register `delegate_task` when an Anthropic key is available.
    pub register_delegation: bool,
}

impl Default for CoreToolRegisterOpts {
    fn default() -> Self {
        Self {
            register_mcp: true,
            register_delegation: true,
        }
    }
}

/// Register the core set of tools into a [`ToolRegistry`].
///
/// This is the shared tool setup used by `weft agent`, `weft gateway`, and
/// `weft mcp-server`. It:
///
/// 1. Builds security policies (command + URL) from config.
/// 2. Registers all built-in tools via [`clawft_tools::register_all`].
/// 3. Registers MCP server tools (proxied from configured MCP servers).
/// 4. Registers the delegation tool (feature-gated).
///
/// Callers that need additional tools (e.g. `MessageTool` with a bus reference)
/// should register them separately after calling this function.
pub async fn register_core_tools<P: Platform + 'static>(
    registry: &mut ToolRegistry,
    config: &Config,
    platform: Arc<P>,
) {
    register_core_tools_with(registry, config, platform, CoreToolRegisterOpts::default()).await;
}

/// Like [`register_core_tools`], with toggles for MCP re-export and delegation.
pub async fn register_core_tools_with<P: Platform + 'static>(
    registry: &mut ToolRegistry,
    config: &Config,
    platform: Arc<P>,
    opts: CoreToolRegisterOpts,
) {
    let command_policy = agent::build_command_policy(&config.tools.command_policy);
    let url_policy = agent::build_url_policy(&config.tools.url_policy);
    let workspace = expand_workspace(&config.agents.defaults.workspace);
    let web_search_config = agent::build_web_search_config(&config.tools);

    clawft_tools::register_all(
        registry,
        platform,
        workspace,
        command_policy,
        url_policy,
        web_search_config,
        // In-process CLI has no daemon substrate to spawn subagents into.
        None,
    );

    if opts.register_mcp {
        let _mcp_sessions = crate::mcp_tools::register_mcp_tools(config, registry).await;
    }

    if opts.register_delegation {
        // Pass the Anthropic provider API key from config as a fallback for delegation.
        let anthropic_key = config.providers.anthropic.api_key.expose();
        let config_api_key = if anthropic_key.is_empty() {
            None
        } else {
            Some(anthropic_key)
        };
        crate::mcp_tools::register_delegation(&config.delegation, registry, config_api_key);
    }
}

/// Build an `Arc<ChannelHost>` implementation that bridges the channel
/// system to a `MessageBus` inbound sender.
///
/// This is the glue between `clawft-channels::PluginHost` (which expects
/// an `Arc<dyn ChannelHost>`) and `clawft-core::bus::MessageBus`.
#[cfg(feature = "channels")]
pub fn make_channel_host(
    bus: Arc<clawft_core::bus::MessageBus>,
) -> Arc<dyn clawft_channels::ChannelHost> {
    Arc::new(BusChannelHost { bus })
}

/// A [`ChannelHost`] implementation backed by a [`MessageBus`].
///
/// Routes inbound messages from channel plugins into the message bus
/// for consumption by the agent loop.
#[cfg(feature = "channels")]
struct BusChannelHost {
    bus: Arc<clawft_core::bus::MessageBus>,
}

#[cfg(feature = "channels")]
#[async_trait::async_trait]
impl clawft_channels::ChannelHost for BusChannelHost {
    async fn deliver_inbound(
        &self,
        msg: clawft_types::event::InboundMessage,
    ) -> Result<(), clawft_types::error::ChannelError> {
        self.bus
            .publish_inbound(msg)
            .map_err(|e| clawft_types::error::ChannelError::Other(e.to_string()))
    }

    async fn register_command(
        &self,
        _cmd: clawft_channels::Command,
    ) -> Result<(), clawft_types::error::ChannelError> {
        // Command registration is a no-op for now; the agent does not
        // expose channel commands yet.
        Ok(())
    }

    async fn publish_inbound(
        &self,
        channel: &str,
        sender_id: &str,
        chat_id: &str,
        content: &str,
        media: Vec<String>,
        metadata: std::collections::HashMap<String, serde_json::Value>,
    ) -> Result<(), clawft_types::error::ChannelError> {
        let msg = clawft_types::event::InboundMessage {
            channel: channel.to_owned(),
            sender_id: sender_id.to_owned(),
            chat_id: chat_id.to_owned(),
            content: content.to_owned(),
            timestamp: chrono::Utc::now(),
            media,
            metadata,
        };
        self.deliver_inbound(msg).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_workspace_tilde() {
        let expanded = expand_workspace("~/.clawft/workspace");
        assert!(!expanded.to_string_lossy().starts_with('~'));
        assert!(expanded.to_string_lossy().contains(".clawft"));
    }

    #[test]
    fn expand_workspace_absolute() {
        let expanded = expand_workspace("/opt/workspace");
        assert_eq!(expanded, PathBuf::from("/opt/workspace"));
    }

    #[test]
    fn expand_workspace_relative() {
        let expanded = expand_workspace("workspace");
        assert_eq!(expanded, PathBuf::from("workspace"));
    }
}
