//! Optional pass/fail thresholds for graphify benches (WEFT-370 / GRAPH-041).
//!
//! Defaults are intentionally loose so CI mock runs stay green; tighten via
//! JSON or field overrides for local regression gates. Regression detection
//! uses a simple ratio against a provided baseline (`max_regression_ratio`).

use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::extraction::ExtractionStats;
use super::graph_ops::{GraphOpsSuiteResult, MemoryEstimate};
use super::report::MetricStatus;

/// Full set of graphify bench thresholds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraphifyBenchThresholds {
    /// Extraction throughput bounds.
    pub extraction: ExtractionThresholds,
    /// Graph-ops suite bounds.
    pub graph_ops: GraphOpsThresholds,
    /// Memory soft budget.
    pub memory: MemoryThresholds,
    /// Regression: fail if measured/baseline > this (default 1.10 = +10%).
    pub max_regression_ratio: f64,
}

impl Default for GraphifyBenchThresholds {
    fn default() -> Self {
        Self {
            extraction: ExtractionThresholds::default(),
            graph_ops: GraphOpsThresholds::default(),
            memory: MemoryThresholds::default(),
            max_regression_ratio: 1.10,
        }
    }
}

impl GraphifyBenchThresholds {
    /// Parse from JSON.
    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }

    /// Pretty JSON.
    pub fn to_json_pretty(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Evaluate available metrics (any `None` is skipped and treated as pass).
    pub fn evaluate(
        &self,
        extraction: Option<&ExtractionStats>,
        graph_ops: Option<&GraphOpsSuiteResult>,
        baseline_total_ms: Option<u64>,
    ) -> ThresholdEvaluation {
        let mut metrics = Vec::new();
        let mut passed = true;

        if let Some(stats) = extraction {
            let m = self.extraction.check(stats);
            if !m.passed {
                passed = false;
            }
            metrics.push(m);
        }

        if let Some(suite) = graph_ops {
            let m = self.graph_ops.check(suite);
            if !m.passed {
                passed = false;
            }
            metrics.push(m);

            let mem = self.memory.check(&suite.memory);
            if !mem.passed {
                passed = false;
            }
            metrics.push(mem);
        }

        if let (Some(suite), Some(base_ms)) = (graph_ops, baseline_total_ms) {
            let measured = suite.total_duration.as_millis() as f64;
            let baseline = base_ms as f64;
            let ratio = if baseline > 0.0 {
                measured / baseline
            } else {
                0.0
            };
            let reg_passed = ratio <= self.max_regression_ratio;
            if !reg_passed {
                passed = false;
            }
            metrics.push(MetricStatus {
                name: "regression_total".into(),
                passed: reg_passed,
                observed: format!("{measured:.0}ms (ratio={ratio:.3})"),
                limit: format!(
                    "≤ {:.0}% of baseline {base_ms}ms",
                    self.max_regression_ratio * 100.0
                ),
                detail: None,
            });
        }

        ThresholdEvaluation { passed, metrics }
    }
}

/// Aggregate evaluation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThresholdEvaluation {
    /// Overall pass.
    pub passed: bool,
    /// Per-metric statuses.
    pub metrics: Vec<MetricStatus>,
}

/// Extraction throughput thresholds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExtractionThresholds {
    /// Minimum mean files/sec (mock path). Default is very low for CI safety;
    /// GRAPH-041 aspirational target is 1000 files/sec on real AST.
    pub min_mean_files_per_sec: f64,
}

impl Default for ExtractionThresholds {
    fn default() -> Self {
        // Loose: even a slow CI host mock batch should clear 10 files/sec.
        Self {
            min_mean_files_per_sec: 10.0,
        }
    }
}

impl ExtractionThresholds {
    /// Check extraction stats.
    pub fn check(&self, stats: &ExtractionStats) -> MetricStatus {
        let passed = stats.mean_files_per_sec >= self.min_mean_files_per_sec;
        MetricStatus {
            name: "extraction_files_per_sec".into(),
            passed,
            observed: format!("{:.1} files/s", stats.mean_files_per_sec),
            limit: format!("≥ {:.1} files/s", self.min_mean_files_per_sec),
            detail: Some(format!(
                "files={} entities={} duration_ms={}",
                stats.total_files,
                stats.total_entities,
                stats.total_duration.as_millis()
            )),
        }
    }
}

/// Graph-ops suite thresholds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraphOpsThresholds {
    /// Max total suite duration for the measured scale (CI uses small N).
    pub max_total_ms: u64,
}

impl Default for GraphOpsThresholds {
    fn default() -> Self {
        // CI scale (128 nodes) should finish well under 5s even on slow hosts.
        Self {
            max_total_ms: 5_000,
        }
    }
}

impl GraphOpsThresholds {
    /// Check suite total duration.
    pub fn check(&self, suite: &GraphOpsSuiteResult) -> MetricStatus {
        let ms = suite.total_duration.as_millis() as u64;
        let passed = ms <= self.max_total_ms;
        MetricStatus {
            name: "graph_ops_total".into(),
            passed,
            observed: format!("{ms}ms (n={})", suite.node_count),
            limit: format!("≤ {}ms", self.max_total_ms),
            detail: Some(format!("ops={}", suite.samples.len())),
        }
    }

    /// Convenience: duration limit as [`Duration`].
    pub fn max_total(&self) -> Duration {
        Duration::from_millis(self.max_total_ms)
    }
}

/// Memory soft budget (structural estimate, not RSS).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryThresholds {
    /// Max estimated bytes (default 500 MiB — GRAPH-041 AC).
    pub max_estimated_bytes: u64,
}

impl Default for MemoryThresholds {
    fn default() -> Self {
        Self {
            max_estimated_bytes: 500 * 1024 * 1024,
        }
    }
}

impl MemoryThresholds {
    /// Check memory estimate against budget.
    pub fn check(&self, mem: &MemoryEstimate) -> MetricStatus {
        let passed = mem.estimated_bytes <= self.max_estimated_bytes;
        MetricStatus {
            name: "memory_estimate".into(),
            passed,
            observed: format!(
                "{} bytes (~{:.1} MiB, n={}, e={})",
                mem.estimated_bytes,
                mem.estimated_bytes as f64 / (1024.0 * 1024.0),
                mem.node_count,
                mem.edge_count
            ),
            limit: format!("≤ {} bytes", self.max_estimated_bytes),
            detail: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::extraction::{CI_ENTITIES_PER_FILE, CI_FILE_COUNT, ExtractionHarness};
    use crate::bench::graph_ops::{CI_NODE_COUNT, GraphOpsHarness};

    #[test]
    fn default_thresholds_pass_ci_mock() {
        let t = GraphifyBenchThresholds::default();
        let mut ex = ExtractionHarness::new();
        ex.run_mock_batch(CI_FILE_COUNT, CI_ENTITIES_PER_FILE)
            .unwrap();
        let stats = ex.stats().unwrap();

        let mut g = GraphOpsHarness::new();
        let suite = g.run_suite(CI_NODE_COUNT);

        let eval = t.evaluate(Some(&stats), Some(&suite), None);
        assert!(eval.passed, "metrics: {:?}", eval.metrics);
    }

    #[test]
    fn regression_fails_when_over_10_percent() {
        let t = GraphifyBenchThresholds::default();
        let mut g = GraphOpsHarness::new();
        let suite = g.run_suite(64);
        // Pretend baseline was extremely fast.
        let eval = t.evaluate(None, Some(&suite), Some(1));
        // Suite almost certainly > 1ms total → fails regression.
        assert!(!eval.passed);
        assert!(
            eval.metrics
                .iter()
                .any(|m| m.name == "regression_total" && !m.passed)
        );
    }

    #[test]
    fn thresholds_json_roundtrip() {
        let json = r#"{
            "extraction": { "min_mean_files_per_sec": 100.0 },
            "graph_ops": { "max_total_ms": 1000 },
            "memory": { "max_estimated_bytes": 104857600 },
            "max_regression_ratio": 1.10
        }"#;
        let t = GraphifyBenchThresholds::from_json(json).unwrap();
        assert!((t.extraction.min_mean_files_per_sec - 100.0).abs() < 1e-9);
        assert_eq!(t.graph_ops.max_total_ms, 1000);
    }
}
