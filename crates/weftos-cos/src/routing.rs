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
use clawft_core::pipeline::llm_adapter::create_adapter_from_config;
use clawft_core::pipeline::transport::LlmProvider;
use clawft_llm::ProviderRouter;
use clawft_types::config::Config;

use crate::config::ModelsConfig;

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

    /// Resolve `model_id` through the app config's provider routing.
    pub fn from_app_config(app: &Config, model_id: &str) -> Self {
        let mut cfg = app.clone();
        cfg.agents.defaults.model = model_id.to_string();
        Self::new(model_id, create_adapter_from_config(&cfg))
    }
}

pub struct ModelRoutes {
    quick: Route,
    plan: Route,
    threshold: f32,
    analyzer: TaskComplexityAnalyzer,
}

const PLAN_CUES: &[&str] = &[
    "plan", "priorit", "roadmap", "strategy", "trade-off", "tradeoff", "compare", "decide",
    "what should i", "what should we", "sequence", "schedule the", "break down", "design",
];

impl ModelRoutes {
    pub fn new(quick: Route, plan: Route, threshold: f32) -> Self {
        Self { quick, plan, threshold: threshold.clamp(0.0, 1.0), analyzer: TaskComplexityAnalyzer::new() }
    }

    /// Build both routes from the app config and the host's model settings.
    /// Quick defaults to `agents.defaults.model`; plan defaults to quick.
    pub fn from_app_config(app: &Config, models: &ModelsConfig) -> Self {
        let quick_id = models.quick.clone().unwrap_or_else(|| app.agents.defaults.model.clone());
        let plan_id = models.plan.clone().unwrap_or_else(|| quick_id.clone());
        let quick = Route::from_app_config(app, &quick_id);
        let plan = if plan_id == quick_id { quick.clone() } else { Route::from_app_config(app, &plan_id) };
        Self::new(quick, plan, models.plan_threshold)
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
