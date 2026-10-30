//! JSON-serializable graphify bench report for CI artifacts (WEFT-370).

use serde::{Deserialize, Serialize};

use super::extraction::ExtractionStats;
use super::graph_ops::GraphOpsSuiteResult;
use super::thresholds::{GraphifyBenchThresholds, ThresholdEvaluation};
use super::{TICKET, VERSION};

/// Pass/fail status for one named metric.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricStatus {
    /// Metric name (`extraction_files_per_sec`, `graph_ops_total`, …).
    pub name: String,
    /// Whether the metric passed its threshold.
    pub passed: bool,
    /// Observed value (human-readable).
    pub observed: String,
    /// Limit (human-readable).
    pub limit: String,
    /// Optional extra detail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Full report written by the harness for a bench run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraphifyBenchReport {
    /// Ticket id.
    pub ticket: String,
    /// Crate version.
    pub version: String,
    /// Thresholds used.
    pub thresholds: GraphifyBenchThresholds,
    /// Extraction stats if measured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extraction: Option<ExtractionStats>,
    /// Graph-ops suite if measured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_ops: Option<GraphOpsSuiteResult>,
    /// Per-metric statuses.
    pub metrics: Vec<MetricStatus>,
    /// Overall pass.
    pub passed: bool,
    /// Free-form notes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

impl GraphifyBenchReport {
    /// Build a report from an evaluation + optional raw metrics.
    pub fn from_evaluation(
        thresholds: GraphifyBenchThresholds,
        evaluation: ThresholdEvaluation,
        extraction: Option<ExtractionStats>,
        graph_ops: Option<GraphOpsSuiteResult>,
    ) -> Self {
        Self {
            ticket: TICKET.into(),
            version: VERSION.into(),
            thresholds,
            extraction,
            graph_ops,
            metrics: evaluation.metrics,
            passed: evaluation.passed,
            notes: None,
        }
    }

    /// Pretty JSON for CI collectors.
    pub fn to_json_pretty(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Compact JSON.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// Attach a note.
    pub fn with_notes(mut self, notes: impl Into<String>) -> Self {
        self.notes = Some(notes.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::extraction::{ExtractionHarness, CI_ENTITIES_PER_FILE, CI_FILE_COUNT};
    use crate::bench::graph_ops::{GraphOpsHarness, CI_NODE_COUNT};

    #[test]
    fn report_json_contains_ticket() {
        let thresholds = GraphifyBenchThresholds::default();
        let mut ex = ExtractionHarness::new();
        ex.run_mock_batch(CI_FILE_COUNT, CI_ENTITIES_PER_FILE).unwrap();
        let stats = ex.stats().unwrap();
        let mut g = GraphOpsHarness::new();
        let suite = g.run_suite(CI_NODE_COUNT);
        let eval = thresholds.evaluate(Some(&stats), Some(&suite), None);
        let report = GraphifyBenchReport::from_evaluation(
            thresholds,
            eval,
            Some(stats),
            Some(suite),
        );
        let json = report.to_json_pretty().unwrap();
        assert!(json.contains("WEFT-370"));
        assert!(json.contains("extraction_files_per_sec"));
        assert!(json.contains("graph_ops_total"));
        assert!(report.passed);
    }

    #[test]
    fn with_notes_sets_and_replaces() {
        let thresholds = GraphifyBenchThresholds::default();
        let eval = thresholds.evaluate(None, None, None);
        let report = GraphifyBenchReport::from_evaluation(thresholds, eval, None, None);
        let report = report.with_notes("first");
        assert_eq!(report.notes.as_deref(), Some("first"));
        let report = report.with_notes("second");
        assert_eq!(report.notes.as_deref(), Some("second"));
    }
}
