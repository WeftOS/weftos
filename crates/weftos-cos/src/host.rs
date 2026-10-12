//! Wiring: config -> app config -> routes -> tools -> agent -> server. Used
//! by the `weftos-cos` binary and by `weft cos serve`.

use std::sync::Arc;

use clawft_platform::NativePlatform;
use clawft_platform::Platform;
use clawft_types::config::Config;

use crate::CosError;
use crate::agent::CosAgent;
use crate::config::CosConfig;
use crate::brain::Brain;
use crate::model::{self, ModelPlan};
use crate::routing::ModelRoutes;
use crate::tools::board::BoardPropose;
use crate::tools::cards::{BoardClaim, BoardNote, BoardRead, CardCurrent, Focus};
use crate::tools::harness::{HarnessClient, NotesRecord, ProjectsActivity};
use crate::tools::kickoff::WorkKickoff;
use crate::tools::nodes::{DaemonHealthSource, DaemonRpc, MeshRoutes, NodesHealth};
use crate::tools::{ToolLog, ToolRegistry};

/// What the host assembled, for the caller to report.
pub struct Built {
    pub agent: CosAgent,
    pub quick_model: String,
    pub plan_model: String,
    pub fallback_models: Vec<String>,
    /// The keyed cloud providers found (names only), in preference order.
    pub cloud_providers: Vec<String>,
    /// Endpoint of the local server, when the quick or plan model is local.
    pub local_api_base: Option<String>,
    /// The installation brain, when it could be bound.
    pub brain: Option<Arc<Brain>>,
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
    let clouds = model::detect_clouds(&app, &model::env_present);
    let plan = ModelPlan::resolve(&cfg.models, &clouds);
    let routes = ModelRoutes::from_plan(&app, &cfg.models, &plan);
    let local_api_base = [&plan.quick, &plan.plan]
        .iter()
        .any(|m| model::is_local_route(m))
        .then(|| app.providers.local.api_base.clone().unwrap_or_else(|| cfg.models.local_api_base.clone()));

    let mut unbound = Vec::new();
    let brain = match Brain::open(&cfg.brain, &std::env::current_dir().unwrap_or_default()) {
        Ok(b) => Some(Arc::new(b)),
        Err(e) => {
            unbound.push(("brain".into(), e.to_string()));
            None
        }
    };
    let focus = Focus::default();
    let mut tools = ToolRegistry::new()
        .with(Arc::new(BoardPropose { staging_dir: cfg.staging_dir.clone() }))
        .with(Arc::new(WorkKickoff { staging_dir: cfg.staging_dir.clone(), runtime_command: cfg.team.runtime_command.clone() }))
        .with(Arc::new(NodesHealth { source: Arc::new(DaemonHealthSource) }))
        .with(Arc::new(MeshRoutes { rpc: Arc::new(DaemonRpc) }));
    let client = match HarnessClient::from_config(&cfg.harness) {
        Ok(client) => Some(Arc::new(client)),
        Err(e) => {
            let reason = e.to_string();
            for t in ["projects.activity", "board.read", "board.claim", "board.note", "card.current"] {
                unbound.push((t.into(), reason.clone()));
            }
            None
        }
    };
    if let Some(client) = &client {
        tools = tools
            .with(Arc::new(ProjectsActivity { client: client.clone() }))
            .with(Arc::new(BoardRead { client: client.clone(), focus: focus.clone() }))
            .with(Arc::new(BoardClaim { client: client.clone(), focus: focus.clone() }))
            .with(Arc::new(BoardNote { client: client.clone(), focus: focus.clone() }))
            .with(Arc::new(CardCurrent { client: client.clone(), focus: focus.clone() }));
    }
    if client.is_some() || brain.is_some() {
        tools = tools.with(Arc::new(NotesRecord { client: client.clone(), brain: brain.clone() }));
    } else {
        unbound.push(("notes.record".into(), "no harness credential and no brain".into()));
    }

    let agent = CosAgent::new(routes, tools, ToolLog::new(cfg.tool_log.clone()));
    Ok(Built {
        agent,
        quick_model: plan.quick,
        plan_model: plan.plan,
        fallback_models: plan.fallback,
        cloud_providers: clouds.iter().map(|c| c.provider.to_string()).collect(),
        local_api_base,
        brain,
        unbound,
    })
}
