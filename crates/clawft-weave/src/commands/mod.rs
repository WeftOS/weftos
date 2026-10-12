//! Command implementations for `weaver`.

pub mod agent_cmd;
pub mod app_cmd;
pub mod bench_cmd;
pub mod bench_eml;
pub mod chain_cmd;
pub mod cog_cmd;
pub mod cluster_cmd;
#[cfg(any(feature = "mesh", feature = "exochain"))]
pub mod cluster_facts;
#[cfg(unix)]
pub mod console_cmd;
pub mod cron_cmd;
pub mod custody_cmd;
pub mod daemon_restart;
pub mod doctor_cmd;
pub mod dashboard_cmd;
pub mod expose_cmd;
pub mod route_cmd;
pub mod fleet_cmd;
pub mod ecc_cmd;
pub mod graphify_cmd;
pub mod init_cmd;
pub mod ipc_cmd;
pub mod kernel_children;
pub mod kernel_cmd;
pub mod leaf_cmd;
#[cfg(all(unix, feature = "exochain", feature = "placement"))]
pub mod project_cmd;
pub mod migrate_cmd;
#[cfg(all(unix, feature = "mesh"))]
pub mod mesh_cmd;
#[cfg(all(unix, feature = "mesh"))]
pub mod mesh_install;
#[cfg(all(unix, feature = "mesh"))]
pub mod mesh_install_key;
#[cfg(all(unix, feature = "mesh"))]
pub mod mesh_nonce;
#[cfg(all(unix, feature = "mesh", feature = "placement"))]
pub mod mesh_pair_cmd;
pub mod resource_cmd;
pub mod service_cmd;
pub mod soul_cmd;
pub mod topology_cmd;
pub mod update_cmd;
pub mod update_flow;
pub mod update_install;
pub mod update_release;
pub mod update_signature;
#[cfg(test)]
mod update_test_support;
#[cfg(test)]
mod update_tests;
#[cfg(test)]
mod update_tests_hardening;
#[cfg(test)]
mod update_tests_signature;
#[cfg(test)]
mod update_tests_rollback;
pub mod vault_cmd;
pub mod versions_cmd;
#[cfg(all(feature = "ecc", feature = "exochain"))]
pub mod model_cmd;
pub mod workload_cmd;
#[cfg(all(feature = "ecc", feature = "exochain"))]
pub mod workload_pack;
#[cfg(all(feature = "placement", unix))]
pub mod workload_place_cmd;
#[cfg(all(feature = "placement", unix))]
pub mod workload_node_cmd;
#[cfg(all(feature = "placement", unix))]
pub mod cog_checkout_cmd;
#[cfg(all(feature = "placement", unix))]
pub mod cog_checkout_verbs;

use std::path::Path;

use clawft_platform::Platform;
use clawft_types::config::Config;
use clawft_types::routing::RoutingConfig;

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
    pub global_routing: RoutingConfig,
    /// Workspace overlay routing, when cwd `.clawft/config.json` existed.
    pub workspace_routing: Option<RoutingConfig>,
}

/// Load configuration from the given path override or via auto-discovery.
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
        // Explicit override is treated as pure global (no workspace split).
        let global_routing = config.routing.clone();
        return Ok(LoadedConfig {
            config,
            global_routing,
            workspace_routing: None,
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
    let config: Config = serde_json::from_value(layers.merged())?;

    Ok(LoadedConfig {
        config,
        global_routing: global.routing,
        workspace_routing,
    })
}
