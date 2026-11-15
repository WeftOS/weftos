//! Criterion bench: knowledge-graph operations (GRAPH-041 / WEFT-370).
//!
//! Times construct / neighbor / degree / cluster / god_nodes / subgraph at
//! 1K / 10K / (optional) 50K node scales on synthetic graphs.
//!
//! Run:
//! ```bash
//! cargo bench -p clawft-graphify --bench graph_ops
//! ```

use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};

use clawft_graphify::analyze;
use clawft_graphify::bench::fixtures::synthetic_graph;
use clawft_graphify::bench::graph_ops::{
    BENCH_NODE_COUNT_1K, BENCH_NODE_COUNT_10K, BENCH_NODE_COUNT_50K, GraphOpsHarness,
};
use clawft_graphify::cluster;
use clawft_graphify::entity::EntityId;

fn graph_ops_suite_scales(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph_ops_suite");
    // 50K is heavy — only a few samples via measurement_time defaults.
    for n in [BENCH_NODE_COUNT_1K, BENCH_NODE_COUNT_10K] {
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.iter(|| {
                let mut h = GraphOpsHarness::new();
                let result = h.run_suite(n);
                black_box(result.total_duration);
            });
        });
    }
    group.finish();
}

fn graph_ops_individual(c: &mut Criterion) {
    // Build once per size outside the timed loop for pure op cost.
    let kg_1k = synthetic_graph(BENCH_NODE_COUNT_1K);
    let kg_10k = synthetic_graph(BENCH_NODE_COUNT_10K);

    let mut group = c.benchmark_group("graph_ops_neighbor_scan");
    for (label, kg) in [("1k", &kg_1k), ("10k", &kg_10k)] {
        group.bench_function(label, |b| {
            let ids: Vec<EntityId> = kg.entity_ids().cloned().collect();
            b.iter(|| {
                let mut touch = 0usize;
                for id in &ids {
                    touch = touch.wrapping_add(kg.neighbors(id).len());
                }
                black_box(touch);
            });
        });
    }
    group.finish();

    let mut group = c.benchmark_group("graph_ops_cluster");
    for (label, kg) in [("1k", &kg_1k), ("10k", &kg_10k)] {
        group.bench_function(label, |b| {
            b.iter(|| {
                let communities = cluster::cluster(kg);
                black_box(communities.len());
            });
        });
    }
    group.finish();

    // WEFT-516: label-propagation vs SASE on the same synthetic graphs.
    let mut group = c.benchmark_group("graph_ops_cluster_method");
    for (label, kg) in [("1k", &kg_1k), ("10k", &kg_10k)] {
        group.bench_function(format!("label_prop/{label}"), |b| {
            b.iter(|| {
                let communities =
                    cluster::cluster_with(kg, cluster::ClusterMethod::LabelPropagation);
                black_box(communities.len());
            });
        });
        group.bench_function(format!("sase/{label}"), |b| {
            b.iter(|| {
                let communities = cluster::cluster_with(kg, cluster::ClusterMethod::Sase);
                black_box(communities.len());
            });
        });
    }
    group.finish();

    let mut group = c.benchmark_group("graph_ops_god_nodes");
    for (label, kg) in [("1k", &kg_1k), ("10k", &kg_10k)] {
        group.bench_function(label, |b| {
            b.iter(|| {
                let gods = analyze::god_nodes(kg, 10);
                black_box(gods.len());
            });
        });
    }
    group.finish();
}

fn graph_ops_construct_50k(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph_ops_construct");
    group.sample_size(10);
    group.bench_function("50k", |b| {
        b.iter(|| {
            let kg = synthetic_graph(BENCH_NODE_COUNT_50K);
            black_box(kg.entity_count());
        });
    });
    group.finish();
}

criterion_group!(
    benches,
    graph_ops_suite_scales,
    graph_ops_individual,
    graph_ops_construct_50k
);
criterion_main!(benches);
