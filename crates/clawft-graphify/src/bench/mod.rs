//! Graphify extraction + graph_ops benchmark harness (WEFT-370 / GRAPH-041).
//!
//! # Scope
//!
//! - **Extraction throughput** — mock or pluggable [`extraction::FileExtractor`]
//!   timing, plus `build::build` assembly timing.
//! - **Graph ops** — construct / neighbor / degree / cluster / god_nodes /
//!   subgraph / dedup / trace on synthetic graphs up to 50K nodes.
//! - **Soft memory estimate** — structural model for the GRAPH-041
//!   "50K entities &lt; 500 MB" check (not process RSS).
//! - **Token reduction** — corpus tokens vs graph-query surface tokens.
//! - **Regression gate** — optional baseline comparison (`max_regression_ratio`,
//!   default +10%).
//!
//! Default CI path uses mock data only (no tree-sitter, no LLM, sub-second).
//! Criterion benches live in `benches/extraction.rs` and `benches/graph_ops.rs`.
//!
//! # Quick start
//!
//! ```rust
//! use clawft_graphify::bench::extraction::{ExtractionHarness, CI_FILE_COUNT, CI_ENTITIES_PER_FILE};
//! use clawft_graphify::bench::graph_ops::{GraphOpsHarness, CI_NODE_COUNT};
//! use clawft_graphify::bench::thresholds::GraphifyBenchThresholds;
//! use clawft_graphify::bench::report::GraphifyBenchReport;
//!
//! let thresholds = GraphifyBenchThresholds::default();
//!
//! let mut ex = ExtractionHarness::new();
//! ex.run_mock_batch(CI_FILE_COUNT, CI_ENTITIES_PER_FILE).unwrap();
//! let stats = ex.stats().unwrap();
//!
//! let mut g = GraphOpsHarness::new();
//! let suite = g.run_suite(CI_NODE_COUNT);
//!
//! let eval = thresholds.evaluate(Some(&stats), Some(&suite), None);
//! let report = GraphifyBenchReport::from_evaluation(thresholds, eval, Some(stats), Some(suite));
//! assert!(report.passed);
//! ```

pub mod extraction;
pub mod fixtures;
pub mod graph_ops;
pub mod report;
pub mod thresholds;

pub use extraction::{
    BuildSample, CI_ENTITIES_PER_FILE, CI_FILE_COUNT, ExtractionBenchError, ExtractionHarness,
    ExtractionSample, ExtractionStats, FileExtractor, MockExtractor, TARGET_FILE_COUNT_1K,
    TARGET_FILE_COUNT_10K,
};
pub use fixtures::{
    estimate_graph_bytes, estimate_query_tokens, make_calls, make_entity, synthetic_extraction,
    synthetic_extractions, synthetic_graph, synthetic_graph_with_density, token_reduction_ratio,
};
pub use graph_ops::{
    BENCH_NODE_COUNT_1K, BENCH_NODE_COUNT_10K, BENCH_NODE_COUNT_50K, CI_NODE_COUNT, GraphOp,
    GraphOpsHarness, GraphOpsSample, GraphOpsSuiteResult, MemoryEstimate, TokenReductionSample,
};
pub use report::{GraphifyBenchReport, MetricStatus};
pub use thresholds::{
    ExtractionThresholds, GraphOpsThresholds, GraphifyBenchThresholds, MemoryThresholds,
    ThresholdEvaluation,
};

/// Ticket this module implements.
pub const TICKET: &str = "WEFT-370";

/// Crate version string for report metadata.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Serde helpers: Duration as whole milliseconds.
pub(crate) mod duration_ms {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(d: &Duration, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u64(d.as_millis() as u64)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
    where
        D: Deserializer<'de>,
    {
        let ms = u64::deserialize(deserializer)?;
        Ok(Duration::from_millis(ms))
    }
}
