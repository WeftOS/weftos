//! Wiring: config -> app config -> routes -> tools -> agent -> server. Used
//! by the `weftos-cos` binary and by `weft cos serve`.

use std::sync::Arc;

use clawft_platform::NativePlatform;
use clawft_platform::Platform;
use clawft_types::config::Config;

use crate::CosError;
use crate::agent::CosAgent;
use crate::config::CosConfig;
use crate::routing::ModelRoutes;
use crate::tools::board::BoardPropose;
use crate::tools::harness::{HarnessClient, NotesRecord, ProjectsActivity};
use crate::tools::kickoff::WorkKickoff;
use crate::tools::nodes::{DaemonHealthSource, NodesHealth};
use crate::tools::{ToolLog, ToolRegistry};

/// What the host assembled, for the caller to report.
pub struct Built {
    pub agent: CosAgent,
    pub quick_model: String,
    pub plan_model: String,
    /// Tools that could not be bound, with the reason (no harness token, for
    /// example). The agent still runs with the rest.
    pub unbound: Vec<(String, String)>,
}

/// Load the app config the way `weft agent` does: an explicit file, or the
/// layered loader (`weave.toml`, home JSON, cwd `.clawft/config.json`).
pub async fn load_app_config(cfg: &CosConfig) -> Result<Config, CosError> {
    if let Some(path) = &cfg.app_config {
        let text = std::fs::read_to_string(path).map_err(|e| CosError::Config(format!("app config {}: {e}", path.display())))?;
        let value: serde_json::Value = serde_json::from_str(&text).map_err(|e| CosError::Config(format!("app config parse: {e}")))?;
        let normalized = clawft_platform::config_loader::normalize_keys(value);
        return serde_json::from_value(normalized).map_err(|e| CosError::Config(format!("app config: {e}")));
    }
    let platform = NativePlatform::new();
    let layers = clawft_platform::config_loader::load_config_layers(platform.fs(), platform.env())
        .await
        .map_err(|e| CosError::Config(format!("load app config: {e}")))?;
    serde_json::from_value(layers.merged()).map_err(|e| CosError::Config(format!("app config: {e}")))
}

/// Build the agent with every tool that can be bound from `cfg`.
pub async fn build(cfg: &CosConfig) -> Result<Built, CosError> {
    let app = load_app_config(cfg).await?;
    let routes = ModelRoutes::from_app_config(&app, &cfg.models);
    let quick_model = routes.quick().model_id.clone();
    let plan_model = routes.plan().model_id.clone();

    let mut unbound = Vec::new();
    let mut tools = ToolRegistry::new()
        .with(Arc::new(BoardPropose { staging_dir: cfg.staging_dir.clone() }))
        .with(Arc::new(WorkKickoff { staging_dir: cfg.staging_dir.clone(), runtime_command: cfg.team.runtime_command.clone() }))
        .with(Arc::new(NodesHealth { source: Arc::new(DaemonHealthSource) }));
    match HarnessClient::from_config(&cfg.harness) {
        Ok(client) => {
            let client = Arc::new(client);
            tools = tools
                .with(Arc::new(ProjectsActivity { client: client.clone() }))
                .with(Arc::new(NotesRecord { client }));
        }
        Err(e) => {
            let reason = e.to_string();
            unbound.push(("projects.activity".into(), reason.clone()));
            unbound.push(("notes.record".into(), reason));
        }
    }

    let agent = CosAgent::new(routes, tools, ToolLog::new(cfg.tool_log.clone()));
    Ok(Built { agent, quick_model, plan_model, unbound })
}
