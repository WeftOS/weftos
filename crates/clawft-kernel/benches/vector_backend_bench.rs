//! Vector backend comparison bench (WEFT-366).
//!
//! Compares [`HnswBackend`] vs [`DiskAnnBackend`] (PQ on/off, when the
//! `diskann` feature is compiled) vs [`HybridBackend`] at a parameterized
//! scale: recall@10 vs brute-force ground truth, insert throughput, query
//! latency p50/p95, approximate RSS, and on-disk size where applicable.
//!
//! Hand-rolled (`harness = false`, plain `fn main()`) rather than
//! criterion-driven: criterion's repeated-sampling model doesn't fit a
//! single parameterized run emitting custom fields (recall, RSS, disk
//! size) as JSON, and would multiply the cost of building large indices.
//! Insert/build are timed once per (backend, config); query latency is
//! sampled per-query across the whole query set for a real p50/p95.
//!
//! ## Running
//!
//! ```text
//! cargo bench -p clawft-kernel --bench vector_backend_bench
//! cargo bench -p clawft-kernel --bench vector_backend_bench --features diskann
//! WEFT_BENCH_N=1000000 WEFT_BENCH_DIM=384 cargo bench -p clawft-kernel \
//!     --bench vector_backend_bench --features diskann
//! ```
//!
//! Env vars (all optional): `WEFT_BENCH_N` (default 10_000),
//! `WEFT_BENCH_DIM` (default 384), `WEFT_BENCH_QUERIES` (default 100),
//! `WEFT_BENCH_SEED` (default 42, fixed for reproducibility).
//!
//! ## Data distribution
//!
//! Uniform-random vectors are unrealistically easy for ANN in high
//! dimensions (everything is nearly orthogonal, so recall reads 1.0
//! everywhere and tells us nothing). Instead: `N_CLUSTERS` fixed cluster
//! centers are drawn from an isotropic Gaussian, then both the corpus and
//! the query set are sampled around those centers with
//! `INTRA_CLUSTER_STD` noise, tuned so cosine-space clusters meaningfully
//! overlap and recall@10 is discriminative between exact (brute-force)
//! and approximate (HNSW/DiskANN) search.
//!
//! ## Distance metric mismatch (a load-bearing finding, not a footgun)
//!
//! [`HnswBackend`] and the DiskANN *stub* both use cosine distance. The
//! **real** `ruvector-diskann` backend (feature = "diskann") uses squared
//! Euclidean (`l2_squared`, see `ruvector_diskann::index`). Ground truth
//! is therefore computed twice (cosine, and — only when `diskann` is
//! compiled — squared Euclidean) and each backend is scored against the
//! ground truth matching its actual metric. Do not compare `recall_at_10`
//! across backends with different `metric` values.
//!
//! [`HybridBackend`] fuses hot (cosine) and cold (real DiskANN: L2²)
//! via Reciprocal Rank Fusion (RRF; WEFT-661) so raw-distance scale
//! cannot drown cold hits. Hybrid `recall_at_10` is still scored against
//! cosine ground truth — see the `notes` field on the `hybrid` JSON line.
//!
//! ## Timing method
//!
//! `insert_vps` times only the insert loop. `flush()` (which, for the
//! real DiskANN backend, triggers Vamana graph `build()` — search
//! silently returns empty results if `build()` was never called; see
//! `DiskAnnBackend::search`'s `Err(_) => Vec::new()`) is timed separately
//! as `build_ms`, NOT included in `insert_vps`. Query latency wraps each
//! individual `search()` call; `p50_us`/`p95_us` come from the sorted
//! per-query latency vector.
//!
//! RSS is read via `ps -o rss= -p $$` after each backend's insert+build
//! step — approximate: it's whole-process (not backend-attributed) RSS,
//! and backend variants run sequentially in one process with allocators
//! rarely returning freed pages, so later readings are a cumulative
//! upper bound, not a clean per-backend number.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Instant;

use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;
use rand_distr::{Distribution, Normal};
use serde::Serialize;

#[cfg(feature = "rabitq-bench")]
#[path = "vector_backend_bench/rabitq_runner.rs"]
mod rabitq_runner;

use clawft_kernel::hnsw_service::HnswServiceConfig;
use clawft_kernel::{
    DiskAnnBackend, DiskAnnConfig, EvictionPolicy, HnswBackend, HybridBackend, HybridConfig,
    VectorBackend,
};

// ── Synthetic data ───────────────────────────────────────────────────────

/// Fixed cluster count — enough that recall is discriminative without
/// making brute-force ground truth (O(n*q*dim)) expensive at 1M scale.
const N_CLUSTERS: usize = 300;
/// Per-dimension std of the cluster centers themselves.
const CLUSTER_CENTER_STD: f32 = 1.0;
/// Per-dimension std of noise added around a cluster center. Tuned (see
/// module docs) so recall@10 is well below 1.0 but well above 0 at the
/// default smoke scale — if a future scale change makes this trivially
/// perfect again, tighten this rather than declaring victory.
const INTRA_CLUSTER_STD: f32 = 2.2;

/// `HnswServiceConfig::ef_search` for the bench's HNSW instances (hnsw
/// backend + Hybrid's hot tier). Deliberately below the crate default of
/// 100: at n=10K, `ef_search=100` gives HNSW enough candidates to
/// re-rank its way to the exact top-10 regardless of cluster overlap
/// (graph-based ANN is robust to high dimensionality — the classic
/// curse-of-dimensionality failure mode is a tree-index problem, not a
/// graph one). ef_search is HNSW's real speed/recall knob in production,
/// so a realistic lower value here is more representative than chasing
/// recall<1.0 through data distribution alone.
const HNSW_EF_SEARCH: usize = 14;

// Note: recall_at_10 for the hnsw arm varies a point or two run-to-run
// (observed 0.93-0.95 at n=10K, seed=42) despite fixed-seed data, because
// `HnswStore::rebuild_index` (clawft-core's `embeddings/hnsw_store.rs`)
// builds via `instant_distance::Builder::default()` without `.seed(...)`,
// and instant-distance seeds layer assignment from OS entropy when
// unseeded — graph topology isn't reproducible even with identical data.
// Existing clawft-core gap, out of scope here, but worth flagging: an
// exact recall regression currently can't be reproduced from a CI log.

fn make_centers(rng: &mut ChaCha8Rng, dim: usize) -> Vec<Vec<f32>> {
    let dist = Normal::new(0.0f32, CLUSTER_CENTER_STD).expect("valid normal params");
    (0..N_CLUSTERS)
        .map(|_| (0..dim).map(|_| dist.sample(rng)).collect())
        .collect()
}

fn sample_points(rng: &mut ChaCha8Rng, centers: &[Vec<f32>], count: usize) -> Vec<Vec<f32>> {
    let dist = Normal::new(0.0f32, INTRA_CLUSTER_STD).expect("valid normal params");
    (0..count)
        .map(|i| {
            let c = &centers[i % centers.len()];
            c.iter().map(|&cv| cv + dist.sample(rng)).collect()
        })
        .collect()
}

// ── Distance metrics (for ground truth only — backends compute their own) ─

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Metric {
    Cosine,
    SqEuclidean,
}

impl Metric {
    fn label(self) -> &'static str {
        match self {
            Metric::Cosine => "cosine",
            Metric::SqEuclidean => "sqeuclidean",
        }
    }
}

/// Mirrors `vector_diskann::cosine_distance` (1 - cosine similarity).
fn cosine_distance(a: &[f32], b: &[f32]) -> f32 {
    let mut dot = 0.0f32;
    let mut norm_a = 0.0f32;
    let mut norm_b = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }
    let denom = norm_a.sqrt() * norm_b.sqrt();
    if denom < f32::EPSILON {
        return 1.0;
    }
    1.0 - (dot / denom)
}

/// Mirrors `ruvector_diskann::distance::l2_squared` (real backend's metric).
fn sqeuclidean_distance(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| (x - y) * (x - y)).sum()
}

fn distance(metric: Metric, a: &[f32], b: &[f32]) -> f32 {
    match metric {
        Metric::Cosine => cosine_distance(a, b),
        Metric::SqEuclidean => sqeuclidean_distance(a, b),
    }
}

fn brute_force_topk(data: &[Vec<f32>], query: &[f32], k: usize, metric: Metric) -> Vec<u64> {
    let mut scored: Vec<(u64, f32)> = data
        .iter()
        .enumerate()
        .map(|(i, v)| (i as u64, distance(metric, query, v)))
        .collect();
    scored.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(k);
    scored.into_iter().map(|(id, _)| id).collect()
}

/// Recovers the numeric row index this harness assigned at insert time
/// (`insert(i as u64, &format!("v{i}"), ...)`) from a `SearchResult.key`.
///
/// Recall is scored via `key`, not `SearchResult.id`: the real DiskANN
/// backend's `search()` (`vector_diskann.rs`, `feature = "diskann"` arm)
/// hardcodes `SearchResult::new(0, r.id, r.distance, ..)` — numeric `id`
/// is a literal `0` for every hit, regardless of which vector matched.
/// Scoring against `.id` measured recall at ~0.01 (id 0 is coincidentally
/// in ~1% of ground-truth top-10s) — not a data/config problem, that bug.
/// Keying off `key` sidesteps it for a real quality signal; the bug is
/// still flagged in the `notes` field below — anything downstream of
/// `VectorBackend::search()` that trusts `.id` against a real (non-stub)
/// DiskAnnBackend is silently broken today.
fn key_to_row(key: &str) -> u64 {
    key.strip_prefix('v')
        .and_then(|s| s.parse().ok())
        .unwrap_or(u64::MAX)
}

fn recall_at_k(predicted: &[u64], truth: &[u64]) -> f64 {
    if truth.is_empty() {
        return 0.0;
    }
    let truth_set: HashSet<u64> = truth.iter().copied().collect();
    let hits = predicted.iter().filter(|id| truth_set.contains(id)).count();
    hits as f64 / truth.len() as f64
}

// ── Env config ────────────────────────────────────────────────────────────

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

// ── Measurement helpers ──────────────────────────────────────────────────

fn percentile(sorted: &[f64], pct: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((pct / 100.0) * (sorted.len() as f64 - 1.0)).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// Approximate whole-process RSS in MB via `ps`. See module docs for why
/// this is a cumulative upper bound, not a clean per-backend number.
fn read_rss_mb() -> f64 {
    let pid = std::process::id();
    std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .and_then(|s| s.trim().parse::<f64>().ok())
        .map(|kb| kb / 1024.0)
        .unwrap_or(0.0)
}

fn dir_size(path: &Path) -> u64 {
    let mut total = 0u64;
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            total += dir_size(&p);
        } else if let Ok(meta) = entry.metadata() {
            total += meta.len();
        }
    }
    total
}

fn bench_scratch_dir(label: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push("weftos-vector-bench");
    p.push(label);
    let _ = std::fs::remove_dir_all(&p);
    p
}

// ── Output ───────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct BenchOutcome {
    backend: String,
    n: usize,
    dim: usize,
    feature_real: bool,
    metric: &'static str,
    insert_vps: f64,
    recall_at_10: f64,
    p50_us: f64,
    p95_us: f64,
    rss_mb: f64,
    disk_bytes: u64,
    build_ms: f64,
    /// 1 / mean per-query latency, one thread, queries issued serially.
    qps_1t: f64,
    /// All-core throughput: every thread runs the whole query set 3 times.
    qps_mt: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    notes: Option<String>,
}

#[allow(clippy::too_many_arguments)]
fn run_backend(
    name: &str,
    backend: &dyn VectorBackend,
    feature_real: bool,
    metric: Metric,
    data: &[Vec<f32>],
    queries: &[Vec<f32>],
    ground_truth: &[Vec<u64>],
    dim: usize,
    disk_dir: Option<&Path>,
    notes: Option<String>,
) -> BenchOutcome {
    let n = data.len();

    let t0 = Instant::now();
    for (i, v) in data.iter().enumerate() {
        backend
            .insert(i as u64, &format!("v{i}"), v, serde_json::json!({}))
            .expect("bench insert should not fail (capacity is unbounded in this harness)");
    }
    let insert_elapsed_s = t0.elapsed().as_secs_f64().max(1e-9);
    let insert_vps = n as f64 / insert_elapsed_s;

    // Real DiskANN needs an explicit build() (via flush()) before search
    // returns anything at all; timed separately from insert throughput.
    let build_t0 = Instant::now();
    backend.flush().expect("bench flush should not fail");
    // HNSW builds its graph lazily on the first query; run one untimed
    // query so that cost lands in `build_ms`, not in the latency/QPS means.
    if let Some(qv) = queries.first() {
        let _ = backend.search(qv, 10);
    }
    let build_ms = build_t0.elapsed().as_secs_f64() * 1000.0;

    let rss_mb = read_rss_mb();

    let mut latencies_us = Vec::with_capacity(queries.len());
    let mut recalls = Vec::with_capacity(queries.len());
    for (qi, qv) in queries.iter().enumerate() {
        let qt0 = Instant::now();
        let results = backend.search(qv, 10);
        latencies_us.push(qt0.elapsed().as_secs_f64() * 1e6);
        let predicted: Vec<u64> = results.iter().map(|r| key_to_row(&r.key)).collect();
        recalls.push(recall_at_k(&predicted, &ground_truth[qi]));
    }
    latencies_us.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let p50_us = percentile(&latencies_us, 50.0);
    let p95_us = percentile(&latencies_us, 95.0);
    let recall_at_10 = recalls.iter().sum::<f64>() / recalls.len().max(1) as f64;
    let qps_1t = 1e6 / (latencies_us.iter().sum::<f64>() / latencies_us.len().max(1) as f64);
    let qps_mt = mt_qps(queries, &|qv| {
        let _ = backend.search(qv, 10);
    });

    let disk_bytes = disk_dir.map(dir_size).unwrap_or(0);

    BenchOutcome {
        backend: name.to_string(),
        n,
        dim,
        feature_real,
        metric: metric.label(),
        insert_vps,
        recall_at_10,
        p50_us,
        p95_us,
        rss_mb,
        disk_bytes,
        build_ms,
        qps_1t,
        qps_mt,
        notes,
    }
}

/// Multi-thread QPS: each of `available_parallelism` threads runs the whole
/// query set 3 times; total queries / wall time.
fn mt_qps(queries: &[Vec<f32>], f: &(dyn Fn(&[f32]) + Sync)) -> f64 {
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let t0 = Instant::now();
    std::thread::scope(|sc| {
        for _ in 0..threads {
            sc.spawn(|| {
                for _ in 0..3 {
                    for qv in queries {
                        f(qv);
                    }
                }
            });
        }
    });
    (threads * 3 * queries.len()) as f64 / t0.elapsed().as_secs_f64().max(1e-9)
}

fn print_table(outcomes: &[BenchOutcome]) {
    println!();
    println!(
        "{:<20} {:>8} {:>7} {:>13} {:>9} {:>10} {:>10} {:>9} {:>12} {:>10} {:>9} {:>9}",
        "backend",
        "n",
        "metric",
        "insert_vps",
        "recall10",
        "p50_us",
        "p95_us",
        "rss_mb",
        "disk_bytes",
        "build_ms",
        "qps_1t",
        "qps_mt"
    );
    for o in outcomes {
        println!(
            "{:<20} {:>8} {:>7} {:>13.0} {:>9.3} {:>10.1} {:>10.1} {:>9.1} {:>12} {:>10.1} {:>9.0} {:>9.0}",
            o.backend,
            o.n,
            o.metric,
            o.insert_vps,
            o.recall_at_10,
            o.p50_us,
            o.p95_us,
            o.rss_mb,
            o.disk_bytes,
            o.build_ms,
            o.qps_1t,
            o.qps_mt,
        );
    }
    for o in outcomes {
        if let Some(n) = &o.notes {
            println!("note[{}]: {}", o.backend, n);
        }
    }
}

// ── Main ─────────────────────────────────────────────────────────────────

fn main() {
    let n = env_usize("WEFT_BENCH_N", 10_000);
    let dim = env_usize("WEFT_BENCH_DIM", 384);
    let q = env_usize("WEFT_BENCH_QUERIES", 100);
    let seed = env_u64("WEFT_BENCH_SEED", 42);
    let diskann_compiled = cfg!(feature = "diskann");

    eprintln!(
        "[vector_backend_bench] n={n} dim={dim} queries={q} seed={seed} diskann_feature={diskann_compiled}"
    );

    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let centers = make_centers(&mut rng, dim);
    let data = sample_points(&mut rng, &centers, n);
    let queries = sample_points(&mut rng, &centers, q);

    eprintln!("[vector_backend_bench] brute-force ground truth (cosine)...");
    let gt_cosine: Vec<Vec<u64>> = queries
        .iter()
        .map(|qv| brute_force_topk(&data, qv, 10, Metric::Cosine))
        .collect();

    let gt_sqeuclidean: Option<Vec<Vec<u64>>> = if diskann_compiled || cfg!(feature = "rabitq-bench") {
        eprintln!("[vector_backend_bench] brute-force ground truth (sqeuclidean)...");
        Some(
            queries
                .iter()
                .map(|qv| brute_force_topk(&data, qv, 10, Metric::SqEuclidean))
                .collect(),
        )
    } else {
        None
    };

    let mut outcomes = Vec::new();

    // `WEFT_BENCH_ONLY=hnsw,rabitq` runs just those arms (prefix match), so
    // per-arm RSS can be read from a clean process. Unset = all arms.
    let only = std::env::var("WEFT_BENCH_ONLY").ok();
    let want = |arm: &str| only.as_deref().is_none_or(|o| o.split(',').any(|a| arm.starts_with(a.trim())));

    #[cfg(feature = "rabitq-bench")]
    if want("rabitq") {
        rabitq_runner::run(
            &mut outcomes,
            &data,
            &queries,
            &gt_cosine,
            gt_sqeuclidean.as_ref().expect("computed for rabitq-bench"), dim);
    }

    // ── HNSW ──────────────────────────────────────────────────────────
    if want("hnsw") {
        eprintln!("[vector_backend_bench] running hnsw...");
        let backend = HnswBackend::new(HnswServiceConfig {
            ef_search: HNSW_EF_SEARCH,
            ef_construction: 200,
            default_dimensions: dim,
        });
        outcomes.push(run_backend(
            "hnsw",
            &backend,
            false,
            Metric::Cosine,
            &data,
            &queries,
            &gt_cosine,
            dim,
            None,
            None,
        ));
    }

    // ── DiskANN ───────────────────────────────────────────────────────
    if want("diskann") && diskann_compiled {
        let gt = gt_sqeuclidean.as_ref().expect("computed above when diskann_compiled");
        for (label, use_pq) in [("diskann-pq_on", true), ("diskann-pq_off", false)] {
            eprintln!("[vector_backend_bench] running {label}...");
            let dir = bench_scratch_dir(label);
            let cfg = DiskAnnConfig {
                dimensions: dim,
                use_pq,
                data_path: dir.to_string_lossy().into_owned(),
                ..DiskAnnConfig::default()
            };
            let backend = DiskAnnBackend::new(cfg);
            outcomes.push(run_backend(
                label,
                &backend,
                true,
                Metric::SqEuclidean,
                &data,
                &queries,
                gt,
                dim,
                Some(&dir),
                Some(
                    "recall_at_10 is scored via SearchResult.key (see key_to_row), not \
                     SearchResult.id -- vector_diskann.rs's real-backend search() hardcodes \
                     id=0 for every hit (DiskAnnBackend::search, `SearchResult::new(0, r.id, \
                     ..)`), a correctness bug independent of this bench. build_ms is Vamana \
                     graph build() time, NOT included in insert_vps."
                        .to_string(),
                ),
            ));
            let _ = std::fs::remove_dir_all(&dir);
        }
    } else if want("diskann") {
        eprintln!("[vector_backend_bench] running diskann (stub)...");
        let dir = bench_scratch_dir("diskann-stub");
        let cfg = DiskAnnConfig {
            dimensions: dim,
            data_path: dir.to_string_lossy().into_owned(),
            ..DiskAnnConfig::default()
        };
        let backend = DiskAnnBackend::new(cfg);
        outcomes.push(run_backend(
            "diskann (stub)",
            &backend,
            false,
            Metric::Cosine,
            &data,
            &queries,
            &gt_cosine,
            dim,
            Some(&dir),
            None,
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── Hybrid ────────────────────────────────────────────────────────
    if want("hybrid") {
        eprintln!("[vector_backend_bench] running hybrid...");
        let dir = bench_scratch_dir("hybrid-cold");
        // Deliberately smaller than n so the cold (DiskANN) tier is
        // actually exercised at bench scale instead of everything
        // fitting in the hot HNSW tier (default hot_capacity=50_000
        // would make Hybrid == HNSW-with-a-mirrored-cold-tier at n<=50K).
        let hot_capacity = (n / 10).max(100);
        let diskann_cfg = DiskAnnConfig {
            dimensions: dim,
            data_path: dir.to_string_lossy().into_owned(),
            ..DiskAnnConfig::default()
        };
        let hybrid_cfg = HybridConfig {
            hot_capacity,
            promotion_threshold: 3,
            eviction_policy: EvictionPolicy::Lru,
        };
        let backend = HybridBackend::new(
            HnswServiceConfig {
                ef_search: HNSW_EF_SEARCH,
                ef_construction: 200,
                default_dimensions: dim,
            },
            diskann_cfg,
            hybrid_cfg,
        );
        let notes = if diskann_compiled {
            Some(
                "hot tier=cosine (HNSW), cold tier=sqeuclidean (real DiskANN); \
                 merge_results uses Reciprocal Rank Fusion (RRF, k=60; WEFT-661) \
                 so incomparable raw distances are not sorted. recall_at_10 is \
                 still measured against cosine ground truth — interpret hybrid \
                 recall accordingly."
                    .to_string(),
            )
        } else {
            None
        };
        outcomes.push(run_backend(
            "hybrid",
            &backend,
            diskann_compiled,
            Metric::Cosine,
            &data,
            &queries,
            &gt_cosine,
            dim,
            Some(&dir),
            notes,
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    for o in &outcomes {
        println!("{}", serde_json::to_string(o).expect("BenchOutcome serializes"));
    }
    print_table(&outcomes);
}
