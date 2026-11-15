//! CI integration tests for the WEFT-370 graphify bench harness.
//!
//! No tree-sitter, no LLM, no multi-minute runs — mock extraction + small
//! synthetic graphs only. Heavy scales are `#[ignore]` soak tests.

use std::time::Duration;

use clawft_graphify::bench::extraction::{
    CI_ENTITIES_PER_FILE, CI_FILE_COUNT, ExtractionHarness, MockExtractor, TARGET_FILE_COUNT_1K,
};
use clawft_graphify::bench::fixtures::{estimate_graph_bytes, synthetic_graph};
use clawft_graphify::bench::graph_ops::{
    BENCH_NODE_COUNT_50K, CI_NODE_COUNT, GraphOp, GraphOpsHarness,
};
use clawft_graphify::bench::report::GraphifyBenchReport;
use clawft_graphify::bench::thresholds::GraphifyBenchThresholds;
use clawft_graphify::bench::{TICKET, VERSION};

#[test]
fn end_to_end_mock_harness_passes_default_thresholds() {
    let thresholds = GraphifyBenchThresholds::default();

    let mut ex = ExtractionHarness::new();
    let sample = ex
        .run_mock_batch(CI_FILE_COUNT, CI_ENTITIES_PER_FILE)
        .expect("mock batch");
    assert_eq!(sample.files, CI_FILE_COUNT);
    assert_eq!(sample.entities, CI_FILE_COUNT * CI_ENTITIES_PER_FILE);
    assert!(sample.files_per_sec().is_finite());

    let (_, build, kg) = ex.run_build_batch(CI_FILE_COUNT, CI_ENTITIES_PER_FILE);
    assert_eq!(build.files, CI_FILE_COUNT);
    assert_eq!(kg.entity_count(), CI_FILE_COUNT * CI_ENTITIES_PER_FILE);

    let stats = ex.stats().expect("stats");
    assert_eq!(stats.count, 1);

    let mut g = GraphOpsHarness::new();
    let suite = g.run_suite(CI_NODE_COUNT);
    assert_eq!(suite.node_count, CI_NODE_COUNT);
    assert!(suite.sample(GraphOp::Cluster).is_some());
    assert!(suite.sample(GraphOp::GodNodes).is_some());
    assert!(suite.memory.under_budget);
    let tr = suite.token_reduction.as_ref().expect("token reduction");
    assert!(tr.reduction_ratio > 0.5);

    let eval = thresholds.evaluate(Some(&stats), Some(&suite), None);
    assert!(eval.passed, "metrics: {:?}", eval.metrics);

    let report = GraphifyBenchReport::from_evaluation(thresholds, eval, Some(stats), Some(suite));
    assert!(report.passed);
    assert_eq!(report.ticket, TICKET);
    assert_eq!(report.version, VERSION);
    assert!(report.metrics.iter().all(|m| m.passed));

    let json = report.to_json_pretty().expect("json");
    assert!(json.contains("WEFT-370"));
    assert!(json.contains("extraction_files_per_sec"));
    assert!(json.contains("graph_ops_total"));
    assert!(json.contains("memory_estimate"));
}

#[test]
fn file_extractor_trait_path_records_samples() {
    let mut harness = ExtractionHarness::new();
    let mut mock = MockExtractor::new(2).named("mock-ci");
    harness.run_batch(&mut mock, 8).expect("batch");
    harness.run_batch(&mut mock, 8).expect("batch");
    let stats = harness.stats().unwrap();
    assert_eq!(stats.count, 2);
    assert_eq!(stats.total_files, 16);
    assert_eq!(stats.total_entities, 32);
}

#[test]
fn injected_samples_drive_stats_without_extractor() {
    use clawft_graphify::bench::extraction::ExtractionSample;

    let mut harness = ExtractionHarness::new();
    harness.push_sample(ExtractionSample {
        extractor: "fixture".into(),
        files: 100,
        entities: 400,
        relationships: 300,
        duration: Duration::from_millis(50),
        note: Some("replay".into()),
    });
    let stats = harness.stats().unwrap();
    assert_eq!(stats.total_files, 100);
    // 100 files / 0.05s = 2000 files/s
    assert!((stats.mean_files_per_sec - 2000.0).abs() < 1.0);
}

#[test]
fn memory_estimate_50k_under_500mb_soft_budget() {
    // Structural estimate only — does not allocate 50K nodes in CI.
    let bytes = estimate_graph_bytes(50_000, 100_000);
    assert!(
        bytes < 500 * 1024 * 1024,
        "soft estimate {bytes} exceeds 500 MiB"
    );

    let thresholds = GraphifyBenchThresholds::default();
    let kg = synthetic_graph(256);
    let mem = clawft_graphify::bench::graph_ops::MemoryEstimate {
        node_count: kg.entity_count(),
        edge_count: kg.relationship_count(),
        estimated_bytes: estimate_graph_bytes(kg.entity_count(), kg.relationship_count()),
        budget_bytes: thresholds.memory.max_estimated_bytes,
        under_budget: true,
    };
    let status = thresholds.memory.check(&mem);
    assert!(status.passed);
}

#[test]
fn thresholds_json_is_ci_loadable() {
    let json = r#"{
        "extraction": { "min_mean_files_per_sec": 50.0 },
        "graph_ops": { "max_total_ms": 2000 },
        "memory": { "max_estimated_bytes": 524288000 },
        "max_regression_ratio": 1.10
    }"#;
    let t = GraphifyBenchThresholds::from_json(json).expect("parse");
    assert!((t.extraction.min_mean_files_per_sec - 50.0).abs() < 1e-9);
    assert_eq!(t.graph_ops.max_total_ms, 2000);
}

#[test]
fn regression_gate_passes_when_within_10_percent() {
    let t = GraphifyBenchThresholds::default();
    let mut g = GraphOpsHarness::new();
    let suite = g.run_suite(64);
    // Baseline equal to measured → ratio 1.0 ≤ 1.10
    let baseline_ms = suite.total_duration.as_millis() as u64;
    let eval = t.evaluate(None, Some(&suite), Some(baseline_ms.max(1)));
    assert!(eval.passed, "metrics: {:?}", eval.metrics);
}

/// Optional 1K-file mock soak — ignored in normal CI.
#[test]
#[ignore = "optional 1K-file mock extraction soak; run with -- --ignored"]
fn soak_extraction_one_thousand_mock_files() {
    let mut h = ExtractionHarness::new();
    let sample = h.run_mock_batch(TARGET_FILE_COUNT_1K, 8).expect("1k mock");
    assert_eq!(sample.files, TARGET_FILE_COUNT_1K);
    // Mock path should clear the aspirational 1K files/sec target easily.
    assert!(
        sample.files_per_sec() >= 1000.0,
        "got {} files/s",
        sample.files_per_sec()
    );
}

/// Optional 50K-node suite — ignored in normal CI (can take several seconds).
#[test]
#[ignore = "optional 50K-node graph_ops suite; run with -- --ignored"]
fn soak_graph_ops_fifty_thousand_nodes() {
    let mut h = GraphOpsHarness::new();
    let result = h.run_suite(BENCH_NODE_COUNT_50K);
    assert_eq!(result.node_count, BENCH_NODE_COUNT_50K);
    assert!(result.memory.under_budget);
    assert!(
        result.total_duration < Duration::from_secs(120),
        "50K suite took {:?}",
        result.total_duration
    );
}
