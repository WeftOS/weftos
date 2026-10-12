//! Quick vs plan model selection on top of WeftOS's existing provider routing.
//!
//! Each route is a `provider/model` id. The provider comes from
//! `clawft_core::pipeline::llm_adapter::create_adapter_from_config`, so the
//! app's provider overrides (`providers.local.apiBase`, keys by env), the
//! local-Hermes alias and the placement hook all apply exactly as they do for
//! `weft agent`. The adapter receives the bare model name, as the pipeline
//! does.
//!
//! A turn is a planning turn when it carries a planning cue or when
//! `TaskComplexityAnalyzer` scores it at or above the threshold (ADR-026's
//! 30% by default). Everything else is a quick turn on the fast local model.

use std::sync::Arc;

use clawft_core::complexity::TaskComplexityAnalyzer;
use clawft_core::pipeline::llm_adapter::create_adapter_from_config_with_reasoning;
use clawft_core::pipeline::transport::LlmProvider;
use clawft_llm::{ProviderRouter, ReasoningMode};
use clawft_types::config::Config;

use crate::config::ModelsConfig;
use crate::model;

/// Which tier a turn was routed to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Quick,
    Plan,
}

impl Tier {
    pub fn label(self) -> &'static str {
        match self {
            Tier::Quick => "quick",
            Tier::Plan => "plan",
        }
    }
}

/// One resolved route: the id the status line shows, the bare model the
/// adapter is called with, and the provider.
#[derive(Clone)]
pub struct Route {
    pub model_id: String,
    pub bare_model: String,
    pub provider: Arc<dyn LlmProvider>,
}

impl Route {
    pub fn new(model_id: impl Into<String>, provider: Arc<dyn LlmProvider>) -> Self {
        let model_id = model_id.into();
        let (_, bare_model) = ProviderRouter::strip_prefix(&model_id);
        Self { model_id, bare_model, provider }
    }

    /// Resolve `model_id` through the app config's provider routing, with a
    /// reasoning mode for local providers (`None` leaves the server default).
    pub fn from_app_config(app: &Config, model_id: &str, reasoning: Option<ReasoningMode>) -> Self {
        let mut cfg = app.clone();
        cfg.agents.defaults.model = model_id.to_string();
        Self::new(model_id, create_adapter_from_config_with_reasoning(&cfg, reasoning))
    }
}

/// The reasoning switch for a tier, from `models.reasoning`: `auto` thinks on
/// planning turns and not on quick ones (a quick turn is a tool call or a
/// short spoken answer; a model that thinks first can spend its whole budget
/// before the first word), `on`/`off` apply to both, `default` leaves the
/// server template alone. Only local providers act on it.
fn reasoning_for(setting: &str, tier: Tier) -> Option<ReasoningMode> {
    match (setting, tier) {
        ("default", _) => None,
        ("on", _) | ("auto", Tier::Plan) => Some(ReasoningMode::On),
        _ => Some(ReasoningMode::Off),
    }
}

pub struct ModelRoutes {
    quick: Route,
    plan: Route,
    fallback: Vec<Route>,
    threshold: f32,
    analyzer: TaskComplexityAnalyzer,
}

const PLAN_CUES: &[&str] = &[
    "plan", "priorit", "roadmap", "strategy", "trade-off", "tradeoff", "compare", "decide",
    "what should i", "what should we", "sequence", "schedule the", "break down", "design",
];

impl ModelRoutes {
    pub fn new(quick: Route, plan: Route, threshold: f32) -> Self {
        Self { quick, plan, fallback: Vec::new(), threshold: threshold.clamp(0.0, 1.0), analyzer: TaskComplexityAnalyzer::new() }
    }

    /// Routes to retry on, in order, when the primary one cannot answer.
    pub fn with_fallback(mut self, fallback: Vec<Route>) -> Self {
        self.fallback = fallback;
        self
    }

    /// Build the routes from the app config and the host's model settings:
    /// the local default, and the keyed cloud providers for planning and as
    /// the fallback chain (see [`crate::model`]). `providers.local.apiBase`
    /// is set from `models.local_api_base` unless the app config already has
    /// one.
    pub fn from_app_config(app: &Config, models: &ModelsConfig) -> Self {
        let clouds = model::detect_clouds(app, &model::env_present);
        Self::from_plan(app, models, &model::ModelPlan::resolve(models, &clouds))
    }

    /// [`Self::from_app_config`] for an already resolved plan.
    pub fn from_plan(app: &Config, models: &ModelsConfig, plan: &model::ModelPlan) -> Self {
        let mut app = app.clone();
        if app.providers.local.api_base.is_none() {
            app.providers.local.api_base = Some(models.local_api_base.clone());
        }
        let quick = Route::from_app_config(&app, &plan.quick, reasoning_for(&models.reasoning, Tier::Quick));
        let plan_route = if plan.plan == plan.quick {
            quick.clone()
        } else {
            Route::from_app_config(&app, &plan.plan, reasoning_for(&models.reasoning, Tier::Plan))
        };
        Self::new(quick, plan_route, models.plan_threshold)
            .with_fallback(plan.fallback.iter().map(|id| Route::from_app_config(&app, id, reasoning_for(&models.reasoning, Tier::Quick))).collect())
    }

    /// The routes to try for a turn whose first choice is `primary`: it, then
    /// (for a planning turn) the quick route, then the fallbacks; no model
    /// twice.
    pub fn chain<'a>(&'a self, primary: &'a Route) -> Vec<&'a Route> {
        let mut out: Vec<&Route> = vec![primary];
        for r in std::iter::once(&self.quick).chain(self.fallback.iter()) {
            if !out.iter().any(|o| o.model_id == r.model_id) {
                out.push(r);
            }
        }
        out
    }

    pub fn fallback(&self) -> &[Route] {
        &self.fallback
    }

    /// Pick the route for an utterance.
    pub fn pick(&self, text: &str) -> (Tier, &Route) {
        let lower = text.to_lowercase();
        let cued = PLAN_CUES.iter().any(|c| lower.contains(c));
        let score = self.analyzer.analyze(text);
        if cued || score >= self.threshold {
            (Tier::Plan, &self.plan)
        } else {
            (Tier::Quick, &self.quick)
        }
    }

    pub fn quick(&self) -> &Route {
        &self.quick
    }

    pub fn plan(&self) -> &Route {
        &self.plan
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    struct Nop;
    #[async_trait]
    impl LlmProvider for Nop {
        async fn complete(
            &self,
            _: &str,
            _: &[serde_json::Value],
            _: &[serde_json::Value],
            _: Option<i32>,
            _: Option<f64>,
            _: Option<&serde_json::Value>,
        ) -> Result<serde_json::Value, String> {
            Err("nop".into())
        }
    }

    fn routes() -> ModelRoutes {
        ModelRoutes::new(
            Route::new("local/fast-model", Arc::new(Nop)),
            Route::new("anthropic/big-model", Arc::new(Nop)),
            0.3,
        )
    }

    #[test]
    fn reasoning_follows_the_tier() {
        assert_eq!(reasoning_for("auto", Tier::Quick), Some(ReasoningMode::Off));
        assert_eq!(reasoning_for("auto", Tier::Plan), Some(ReasoningMode::On));
        assert_eq!(reasoning_for("on", Tier::Quick), Some(ReasoningMode::On));
        assert_eq!(reasoning_for("off", Tier::Plan), Some(ReasoningMode::Off));
        assert_eq!(reasoning_for("default", Tier::Plan), None);
    }

    #[test]
    fn strips_prefix_for_the_adapter() {
        let r = Route::new("local/hermes-3", Arc::new(Nop));
        assert_eq!(r.bare_model, "hermes-3");
        assert_eq!(r.model_id, "local/hermes-3");
    }

    #[test]
    fn short_status_turn_is_quick() {
        let r = routes();
        let (tier, route) = r.pick("where are we");
        assert_eq!(tier, Tier::Quick);
        assert_eq!(route.model_id, "local/fast-model");
    }

    #[test]
    fn planning_cue_escalates() {
        let r = routes();
        let (tier, route) = r.pick("prioritise the console work for next week");
        assert_eq!(tier, Tier::Plan);
        assert_eq!(route.model_id, "anthropic/big-model");
    }
}
