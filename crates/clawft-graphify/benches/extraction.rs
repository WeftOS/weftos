//! Criterion bench: extraction throughput (GRAPH-041 / WEFT-370).
//!
//! Measures mock AST-equivalent extraction batches and `build::build` merge
//! at several scales. Real tree-sitter extraction can be added later behind
//! feature flags; this path stays dependency-light and reproducible.
//!
//! Run:
//! ```bash
//! cargo bench -p clawft-graphify --bench extraction
//! ```

use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};

use clawft_graphify::bench::extraction::{ExtractionHarness, MockExtractor, TARGET_FILE_COUNT_1K};
use clawft_graphify::bench::fixtures::synthetic_extractions;
use clawft_graphify::build;

fn extraction_mock_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("extraction_mock");
    for n_files in [32usize, 128, 512, TARGET_FILE_COUNT_1K] {
        group.bench_with_input(BenchmarkId::from_parameter(n_files), &n_files, |b, &n| {
            b.iter(|| {
                let mut harness = ExtractionHarness::new();
                let mut mock = MockExtractor::new(8);
                let sample = harness.run_batch(&mut mock, n).expect("mock");
                black_box(sample.entities);
            });
        });
    }
    group.finish();
}

fn extraction_build_merge(c: &mut Criterion) {
    let mut group = c.benchmark_group("extraction_build");
    for n_files in [32usize, 128, 512] {
        group.bench_with_input(BenchmarkId::from_parameter(n_files), &n_files, |b, &n| {
            // Pre-generate extractions so we time build only.
            let extractions = synthetic_extractions(n, 8);
            b.iter(|| {
                let kg = build::build(black_box(&extractions));
                black_box(kg.entity_count());
            });
        });
    }
    group.finish();
}

criterion_group!(benches, extraction_mock_throughput, extraction_build_merge);
criterion_main!(benches);
