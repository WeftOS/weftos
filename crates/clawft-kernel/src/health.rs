//! Health monitoring subsystem.
//!
//! The [`HealthSystem`] aggregates health checks from all registered
//! services and produces an overall [`OverallHealth`] status.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::service::ServiceRegistry;

/// Health status for a single service.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HealthStatus {
    /// Service is operating normally.
    Healthy,
    /// Service is operational but degraded.
    Degraded(String),
    /// Service is not operational.
    Unhealthy(String),
    /// Health status could not be determined (e.g. timeout).
    Unknown,
}

impl std::fmt::Display for HealthStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HealthStatus::Healthy => write!(f, "healthy"),
            HealthStatus::Degraded(msg) => write!(f, "degraded: {msg}"),
            HealthStatus::Unhealthy(msg) => write!(f, "unhealthy: {msg}"),
            HealthStatus::Unknown => write!(f, "unknown"),
        }
    }
}

/// Aggregated health status for the entire kernel.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum OverallHealth {
    /// All services are healthy.
    Healthy,
    /// Some services are degraded or unhealthy.
    Degraded {
        /// Services that are not fully healthy.
        unhealthy_services: Vec<String>,
    },
    /// All services are unhealthy or no services registered.
    Down,
}

impl std::fmt::Display for OverallHealth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OverallHealth::Healthy => write!(f, "healthy"),
            OverallHealth::Degraded { unhealthy_services } => {
                write!(f, "degraded ({})", unhealthy_services.join(", "))
            }
            OverallHealth::Down => write!(f, "down"),
        }
    }
}

/// Health monitoring system.
///
/// Periodically checks all registered services and aggregates their
/// health into an overall status.
pub struct HealthSystem {
    check_interval_secs: u64,
}

impl HealthSystem {
    /// Create a new health system with the given check interval.
    pub fn new(check_interval_secs: u64) -> Self {
        Self {
            check_interval_secs,
        }
    }

    /// Get the configured check interval in seconds.
    pub fn check_interval_secs(&self) -> u64 {
        self.check_interval_secs
    }

    /// Run a single health check cycle against all services.
    pub async fn aggregate(
        &self,
        registry: &Arc<ServiceRegistry>,
    ) -> (OverallHealth, Vec<(String, HealthStatus)>) {
        let results = registry.health_all().await;

        if results.is_empty() {
            return (OverallHealth::Down, results);
        }

        let mut unhealthy = Vec::new();
        let mut all_unhealthy = true;

        for (name, status) in &results {
            match status {
                HealthStatus::Healthy => {
                    debug!(service = %name, "health check: healthy");
                    all_unhealthy = false;
                }
                HealthStatus::Degraded(msg) => {
                    warn!(service = %name, reason = %msg, "health check: degraded");
                    unhealthy.push(name.clone());
                    all_unhealthy = false;
                }
                HealthStatus::Unhealthy(msg) => {
                    warn!(service = %name, reason = %msg, "health check: unhealthy");
                    unhealthy.push(name.clone());
                }
                HealthStatus::Unknown => {
                    warn!(service = %name, "health check: unknown");
                    unhealthy.push(name.clone());
                }
            }
        }

        let overall = if unhealthy.is_empty() {
            OverallHealth::Healthy
        } else if all_unhealthy {
            OverallHealth::Down
        } else {
            OverallHealth::Degraded {
                unhealthy_services: unhealthy,
            }
        };

        (overall, results)
    }
}

// ── K2b-G2: Liveness and readiness probes (os-patterns) ─────────

/// Result of a liveness or readiness probe.
#[non_exhaustive]
#[cfg(feature = "os-patterns")]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProbeResult {
    /// Service is live (responding to probes).
    Live,
    /// Service is not live.
    NotLive { reason: String },
    /// Service is ready to accept traffic.
    Ready,
    /// Service is not ready.
    NotReady { reason: String },
}

/// Configuration for liveness and readiness probes.
#[cfg(feature = "os-patterns")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeConfig {
    /// How often to check liveness (default: 10s).
    pub liveness_interval_secs: u64,
    /// How often to check readiness (default: 5s).
    pub readiness_interval_secs: u64,
    /// Number of consecutive failures before marking as failed.
    pub failure_threshold: u32,
    /// Number of consecutive successes before marking as recovered.
    pub success_threshold: u32,
}

#[cfg(feature = "os-patterns")]
impl Default for ProbeConfig {
    fn default() -> Self {
        Self {
            liveness_interval_secs: 10,
            readiness_interval_secs: 5,
            failure_threshold: 3,
            success_threshold: 1,
        }
    }
}

#[cfg(feature = "os-patterns")]
impl ProbeConfig {
    /// Derive a [`ProbeConfig`] using an optional learned
    /// [`HealthThresholdModel`](crate::eml_kernel::HealthThresholdModel).
    ///
    /// The model's two output heads `(degraded, failed)` map onto
    /// `(success_threshold, failure_threshold)` respectively: the
    /// `failed` head sets the number of consecutive probe failures
    /// before the service is marked failed, and the `degraded` head
    /// is treated as the number of consecutive successes required to
    /// mark recovery (a higher value makes recovery more conservative).
    ///
    /// When `model` is `None` or untrained, returns
    /// [`Self::default`] — i.e. `failure_threshold=3`,
    /// `success_threshold=1` — exactly reproducing today's behaviour.
    ///
    /// `service_type`, `history_depth`, and `recent_latency_ms` feed
    /// the EML model when it is trained; callers without those
    /// signals can pass `(0, 0, 0.0)`.
    ///
    /// NOTE(eml-swap): wired — Finding #5 (HealthThresholdModel).
    pub fn from_model(
        model: Option<&crate::eml_kernel::HealthThresholdModel>,
        service_type: u32,
        history_depth: u32,
        recent_latency_ms: f64,
    ) -> Self {
        match model {
            Some(m) if m.is_trained() => {
                let (degraded, failed) = m.predict(service_type, history_depth, recent_latency_ms);
                Self {
                    failure_threshold: failed,
                    success_threshold: degraded.max(1),
                    ..Self::default()
                }
            }
            _ => Self::default(),
        }
    }
}

/// Tracks consecutive probe results for threshold-based decisions.
#[cfg(feature = "os-patterns")]
#[derive(Debug, Clone)]
pub struct ProbeState {
    /// Consecutive liveness failures.
    pub liveness_failures: u32,
    /// Consecutive readiness failures.
    pub readiness_failures: u32,
    /// Consecutive readiness successes (for recovery).
    pub readiness_successes: u32,
    /// Whether the service is currently considered live.
    pub is_live: bool,
    /// Whether the service is currently considered ready.
    pub is_ready: bool,
}

#[cfg(feature = "os-patterns")]
impl Default for ProbeState {
    fn default() -> Self {
        Self {
            liveness_failures: 0,
            readiness_failures: 0,
            readiness_successes: 0,
            is_live: true,
            is_ready: true,
        }
    }
}

#[cfg(feature = "os-patterns")]
impl ProbeState {
    /// Record a liveness probe result.
    ///
    /// Returns `true` if the service should be restarted (failures >= threshold).
    pub fn record_liveness(&mut self, result: &ProbeResult, config: &ProbeConfig) -> bool {
        match result {
            ProbeResult::Live => {
                self.liveness_failures = 0;
                self.is_live = true;
                false
            }
            ProbeResult::NotLive { .. } => {
                self.liveness_failures += 1;
                if self.liveness_failures >= config.failure_threshold {
                    self.is_live = false;
                    true
                } else {
                    false
                }
            }
            _ => false, // readiness results ignored here
        }
    }

    /// Record a readiness probe result.
    ///
    /// Returns the readiness state change:
    /// - `Some(false)` if service should be removed from registry
    /// - `Some(true)` if service should be re-added (recovered)
    /// - `None` if no state change
    pub fn record_readiness(&mut self, result: &ProbeResult, config: &ProbeConfig) -> Option<bool> {
        match result {
            ProbeResult::Ready => {
                self.readiness_failures = 0;
                self.readiness_successes += 1;
                if !self.is_ready && self.readiness_successes >= config.success_threshold {
                    self.is_ready = true;
                    Some(true) // recovered
                } else {
                    None
                }
            }
            ProbeResult::NotReady { .. } => {
                self.readiness_successes = 0;
                self.readiness_failures += 1;
                if self.is_ready && self.readiness_failures >= config.failure_threshold {
                    self.is_ready = false;
                    Some(false) // became unready
                } else {
                    None
                }
            }
            _ => None, // liveness results ignored here
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::{ServiceType, SystemService};
    use async_trait::async_trait;

    struct HealthyService;

    #[async_trait]
    impl SystemService for HealthyService {
        fn name(&self) -> &str {
            "healthy-svc"
        }
        fn service_type(&self) -> ServiceType {
            ServiceType::Core
        }
        async fn start(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            Ok(())
        }
        async fn stop(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            Ok(())
        }
        async fn health_check(&self) -> HealthStatus {
            HealthStatus::Healthy
        }
    }

    struct UnhealthyService;

    #[async_trait]
    impl SystemService for UnhealthyService {
        fn name(&self) -> &str {
            "unhealthy-svc"
        }
        fn service_type(&self) -> ServiceType {
            ServiceType::Core
        }
        async fn start(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            Ok(())
        }
        async fn stop(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            Ok(())
        }
        async fn health_check(&self) -> HealthStatus {
            HealthStatus::Unhealthy("test failure".into())
        }
    }

    #[tokio::test]
    async fn aggregate_all_healthy() {
        let registry = Arc::new(ServiceRegistry::new());
        registry.register(Arc::new(HealthyService)).unwrap();

        let health = HealthSystem::new(30);
        let (overall, results) = health.aggregate(&registry).await;

        assert!(matches!(overall, OverallHealth::Healthy));
        assert_eq!(results.len(), 1);
    }

    #[tokio::test]
    async fn aggregate_mixed() {
        let registry = Arc::new(ServiceRegistry::new());
        registry.register(Arc::new(HealthyService)).unwrap();
        registry.register(Arc::new(UnhealthyService)).unwrap();

        let health = HealthSystem::new(30);
        let (overall, results) = health.aggregate(&registry).await;

        assert!(matches!(overall, OverallHealth::Degraded { .. }));
        assert_eq!(results.len(), 2);
    }

    #[tokio::test]
    async fn aggregate_all_unhealthy() {
        let registry = Arc::new(ServiceRegistry::new());
        registry.register(Arc::new(UnhealthyService)).unwrap();

        let health = HealthSystem::new(30);
        let (overall, _) = health.aggregate(&registry).await;

        assert!(matches!(overall, OverallHealth::Down));
    }

    #[tokio::test]
    async fn aggregate_empty_registry() {
        let registry = Arc::new(ServiceRegistry::new());
        let health = HealthSystem::new(30);
        let (overall, results) = health.aggregate(&registry).await;

        assert!(matches!(overall, OverallHealth::Down));
        assert!(results.is_empty());
    }

    #[test]
    fn health_status_display() {
        assert_eq!(HealthStatus::Healthy.to_string(), "healthy");
        assert_eq!(
            HealthStatus::Degraded("slow".into()).to_string(),
            "degraded: slow"
        );
        assert_eq!(
            HealthStatus::Unhealthy("crash".into()).to_string(),
            "unhealthy: crash"
        );
        assert_eq!(HealthStatus::Unknown.to_string(), "unknown");
    }

    #[test]
    fn overall_health_display() {
        assert_eq!(OverallHealth::Healthy.to_string(), "healthy");
        assert_eq!(OverallHealth::Down.to_string(), "down");
        assert_eq!(
            OverallHealth::Degraded {
                unhealthy_services: vec!["svc-a".into(), "svc-b".into()]
            }
            .to_string(),
            "degraded (svc-a, svc-b)"
        );
    }

    #[test]
    fn check_interval() {
        let health = HealthSystem::new(15);
        assert_eq!(health.check_interval_secs(), 15);
    }

    // ── K2b-G2: Probe tests (os-patterns) ────────────────────────

    #[cfg(feature = "os-patterns")]
    mod probe_tests {
        use super::super::*;

        #[test]
        fn probe_config_default() {
            let config = ProbeConfig::default();
            assert_eq!(config.liveness_interval_secs, 10);
            assert_eq!(config.readiness_interval_secs, 5);
            assert_eq!(config.failure_threshold, 3);
            assert_eq!(config.success_threshold, 1);
        }

        #[test]
        fn probe_config_from_model_untrained_matches_default() {
            // Finding #5: untrained HealthThresholdModel must reproduce
            // the hardcoded defaults exactly.
            let model = crate::eml_kernel::HealthThresholdModel::new();
            assert!(!model.is_trained());

            let from_none = ProbeConfig::from_model(None, 0, 0, 0.0);
            let from_untrained = ProbeConfig::from_model(Some(&model), 0, 100, 50.0);
            let defaults = ProbeConfig::default();

            assert_eq!(from_none.failure_threshold, defaults.failure_threshold);
            assert_eq!(from_none.success_threshold, defaults.success_threshold);
            assert_eq!(from_untrained.failure_threshold, defaults.failure_threshold);
            assert_eq!(from_untrained.success_threshold, defaults.success_threshold);
        }

        #[test]
        fn probe_config_from_trained_model_uses_prediction() {
            // Finding #5: with a trained HealthThresholdModel, the
            // config thresholds are sourced from the model.
            let model = crate::eml_kernel::HealthThresholdModel::new();
            let mut json = serde_json::to_value(&model).unwrap();
            if let Some(inner) = json.get_mut("inner").and_then(|v| v.as_object_mut()) {
                inner.insert("trained".into(), serde_json::Value::Bool(true));
            }
            let forced: crate::eml_kernel::HealthThresholdModel =
                serde_json::from_value(json).unwrap();
            assert!(forced.is_trained());

            let cfg = ProbeConfig::from_model(Some(&forced), 0, 100, 50.0);
            // Thresholds are clamped to [1,20] inside the model.
            assert!((1..=20).contains(&cfg.failure_threshold));
            assert!((1..=20).contains(&cfg.success_threshold));
            // failed >= degraded (enforced by HealthThresholdModel).
            assert!(cfg.failure_threshold >= cfg.success_threshold);
        }

        #[test]
        fn probe_config_serde_roundtrip() {
            let config = ProbeConfig {
                liveness_interval_secs: 15,
                readiness_interval_secs: 10,
                failure_threshold: 5,
                success_threshold: 2,
            };
            let json = serde_json::to_string(&config).unwrap();
            let restored: ProbeConfig = serde_json::from_str(&json).unwrap();
            assert_eq!(restored.liveness_interval_secs, 15);
            assert_eq!(restored.failure_threshold, 5);
        }

        #[test]
        fn probe_result_serde_roundtrip() {
            let results = vec![
                ProbeResult::Live,
                ProbeResult::NotLive {
                    reason: "oom".into(),
                },
                ProbeResult::Ready,
                ProbeResult::NotReady {
                    reason: "init".into(),
                },
            ];
            for result in results {
                let json = serde_json::to_string(&result).unwrap();
                let restored: ProbeResult = serde_json::from_str(&json).unwrap();
                assert_eq!(restored, result);
            }
        }

        #[test]
        fn probe_state_default_is_live_and_ready() {
            let state = ProbeState::default();
            assert!(state.is_live);
            assert!(state.is_ready);
            assert_eq!(state.liveness_failures, 0);
            assert_eq!(state.readiness_failures, 0);
        }

        #[test]
        fn liveness_resets_on_success() {
            let config = ProbeConfig {
                failure_threshold: 3,
                ..Default::default()
            };
            let mut state = ProbeState {
                liveness_failures: 2,
                ..Default::default()
            };

            let restart = state.record_liveness(&ProbeResult::Live, &config);
            assert!(!restart);
            assert_eq!(state.liveness_failures, 0);
            assert!(state.is_live);
        }

        #[test]
        fn liveness_triggers_restart_at_threshold() {
            let config = ProbeConfig {
                failure_threshold: 3,
                ..Default::default()
            };
            let mut state = ProbeState::default();

            // 2 failures: no restart
            assert!(!state.record_liveness(
                &ProbeResult::NotLive {
                    reason: "hang".into()
                },
                &config
            ));
            assert!(!state.record_liveness(
                &ProbeResult::NotLive {
                    reason: "hang".into()
                },
                &config
            ));
            assert!(state.is_live);

            // 3rd failure: restart
            assert!(state.record_liveness(
                &ProbeResult::NotLive {
                    reason: "hang".into()
                },
                &config
            ));
            assert!(!state.is_live);
        }

        #[test]
        fn readiness_removes_at_threshold() {
            let config = ProbeConfig {
                failure_threshold: 2,
                ..Default::default()
            };
            let mut state = ProbeState::default();

            assert!(
                state
                    .record_readiness(
                        &ProbeResult::NotReady {
                            reason: "init".into()
                        },
                        &config
                    )
                    .is_none()
            );
            let change = state.record_readiness(
                &ProbeResult::NotReady {
                    reason: "init".into(),
                },
                &config,
            );
            assert_eq!(change, Some(false)); // should be removed
            assert!(!state.is_ready);
        }

        #[test]
        fn readiness_recovery_re_adds() {
            let config = ProbeConfig {
                failure_threshold: 1,
                success_threshold: 1,
                ..Default::default()
            };
            let mut state = ProbeState::default();

            // Make it unready
            state.record_readiness(
                &ProbeResult::NotReady {
                    reason: "init".into(),
                },
                &config,
            );
            assert!(!state.is_ready);

            // Recover
            let change = state.record_readiness(&ProbeResult::Ready, &config);
            assert_eq!(change, Some(true));
            assert!(state.is_ready);
        }

        #[test]
        fn threshold_prevents_flapping() {
            let config = ProbeConfig {
                failure_threshold: 3,
                success_threshold: 2,
                ..Default::default()
            };
            let mut state = ProbeState::default();

            // One failure shouldn't change anything
            assert!(
                state
                    .record_readiness(&ProbeResult::NotReady { reason: "x".into() }, &config)
                    .is_none()
            );
            assert!(state.is_ready);

            // One success resets failures
            assert!(
                state
                    .record_readiness(&ProbeResult::Ready, &config)
                    .is_none()
            );
            assert!(state.is_ready);
        }

        #[test]
        fn default_probe_returns_live_ready() {
            // Default liveness/readiness should return Live/Ready
            let live = ProbeResult::Live;
            let ready = ProbeResult::Ready;
            assert_eq!(live, ProbeResult::Live);
            assert_eq!(ready, ProbeResult::Ready);
        }
    }
}
