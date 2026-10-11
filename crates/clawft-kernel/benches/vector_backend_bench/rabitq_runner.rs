//! RaBitQ arm for `vector_backend_bench` (feature `rabitq-bench`).
//!
//! `ruvector-rabitq` 2.3.0 is a flat-scan ANN: a random rotation, 1-bit
//! codes, an XNOR-popcount scan over every code, and (for `RabitqPlusIndex`)
//! an exact f32 rerank of the best `k * rerank_factor` candidates. It has no
//! graph and exposes no distance hook, so it cannot sit "in front of" the
//! kernel's HNSW; it is measured standalone as a replacement for the index.
//! Estimated and exact distances are L2, so recall is scored against both
//! the squared-Euclidean and the cosine brute-force ground truth.

use std::time::Instant;

use ruvector_rabitq::{AnnIndex, FlatF32Index, RabitqPlusIndex};

use super::{mt_qps, percentile, read_rss_mb, recall_at_k, BenchOutcome};

const ROTATION_SEED: u64 = 42;

#[allow(clippy::too_many_arguments)]
fn measure(
    name: &str,
    index: &dyn AnnIndex,
    insert_vps: f64,
    build_ms: f64,
    data_len: usize,
    queries: &[Vec<f32>],
    gt_l2: &[Vec<u64>],
    gt_cos: &[Vec<u64>],
    dim: usize,
    cosine_primary: bool,
    notes: String,
) -> BenchOutcome {
    let rss_mb = read_rss_mb();
    let mut lat = Vec::with_capacity(queries.len());
    let (mut r_l2, mut r_cos) = (0.0, 0.0);
    for (qi, qv) in queries.iter().enumerate() {
        let t = Instant::now();
        let res = index.search(qv, 10).expect("rabitq search");
        lat.push(t.elapsed().as_secs_f64() * 1e6);
        let ids: Vec<u64> = res.iter().map(|r| r.id as u64).collect();
        r_l2 += recall_at_k(&ids, &gt_l2[qi]);
        r_cos += recall_at_k(&ids, &gt_cos[qi]);
    }
    let qps_1t = 1e6 / (lat.iter().sum::<f64>() / lat.len() as f64);
    lat.sort_by(|a, b| a.total_cmp(b));
    let nq = queries.len() as f64;
    let qps_mt = mt_qps(queries, &|qv| {
        let _ = index.search(qv, 10);
    });
    BenchOutcome {
        backend: name.to_string(),
        n: data_len,
        dim,
        feature_real: true,
        metric: if cosine_primary {
            "cosine"
        } else {
            "sqeuclidean"
        },
        insert_vps,
        recall_at_10: if cosine_primary { r_cos } else { r_l2 } / nq,
        p50_us: percentile(&lat, 50.0),
        p95_us: percentile(&lat, 95.0),
        rss_mb,
        disk_bytes: 0,
        build_ms,
        qps_1t,
        qps_mt,
        notes: Some(format!(
            "{notes} recall_at_10 vs {} GT; vs {} GT = {:.3}. \
             index_mem_mb={:.2} (crate memory_bytes). rss_mb is whole-process \
             and includes the corpus.",
            if cosine_primary {
                "cosine"
            } else {
                "sqeuclidean"
            },
            if cosine_primary {
                "sqeuclidean"
            } else {
                "cosine"
            },
            if cosine_primary { r_l2 } else { r_cos } / nq,
            index.memory_bytes() as f64 / 1048576.0
        )),
    }
}

fn fill(index: &mut dyn AnnIndex, data: &[Vec<f32>]) -> (f64, f64) {
    let t = Instant::now();
    for (i, v) in data.iter().enumerate() {
        index.add(i, v.clone()).expect("rabitq add");
    }
    let s = t.elapsed().as_secs_f64().max(1e-9);
    (data.len() as f64 / s, s * 1000.0)
}

pub fn run(
    out: &mut Vec<BenchOutcome>,
    data: &[Vec<f32>],
    queries: &[Vec<f32>],
    gt_cos: &[Vec<u64>],
    gt_l2: &[Vec<u64>],
    dim: usize,
) {
    let mut flat = FlatF32Index::new(dim);
    let (vps, ms) = fill(&mut flat, data);
    out.push(measure(
        "rabitq-flat_f32",
        &flat,
        vps,
        ms,
        data.len(),
        queries,
        gt_l2,
        gt_cos,
        dim,
        false,
        "crate's exact baseline.".into(),
    ));

    for rf in [1usize, 5, 20, 50] {
        eprintln!("[vector_backend_bench] running rabitq+ rerank x{rf}...");
        let mut idx = RabitqPlusIndex::new(dim, ROTATION_SEED, rf);
        let (vps, ms) = fill(&mut idx, data);
        let par_ms = if rf == 20 {
            let items: Vec<(usize, Vec<f32>)> = data.iter().cloned().enumerate().collect();
            let t = Instant::now();
            let _ = RabitqPlusIndex::from_vectors_parallel(dim, ROTATION_SEED, rf, items)
                .expect("parallel build");
            t.elapsed().as_secs_f64() * 1000.0
        } else {
            f64::NAN
        };
        let par_note = if par_ms.is_nan() {
            String::new()
        } else {
            format!(" (rayon from_vectors_parallel = {par_ms:.0} ms)")
        };
        out.push(measure(
            &format!("rabitq+_x{rf}"), &idx, vps, ms, data.len(), queries, gt_l2, gt_cos, dim, false,
            format!("raw vectors: 1-bit sym scan + exact rerank of k*{rf}; build_ms = serial add loop{par_note}."),
        ));
    }

    // Unit-normalised copies: L2 order equals cosine order, so these arms are
    // scored against the cosine ground truth the HNSW arm uses.
    let unit = |v: &Vec<f32>| -> Vec<f32> {
        let n = v
            .iter()
            .map(|x| x * x)
            .sum::<f32>()
            .sqrt()
            .max(f32::EPSILON);
        v.iter().map(|x| x / n).collect()
    };
    let udata: Vec<Vec<f32>> = data.iter().map(unit).collect();
    let uqueries: Vec<Vec<f32>> = queries.iter().map(unit).collect();
    for rf in [5usize, 20, 50] {
        eprintln!("[vector_backend_bench] running rabitq+ unit-norm rerank x{rf}...");
        let mut idx = RabitqPlusIndex::new(dim, ROTATION_SEED, rf);
        let (vps, ms) = fill(&mut idx, &udata);
        out.push(measure(
            &format!("rabitq+unit_x{rf}"),
            &idx,
            vps,
            ms,
            data.len(),
            &uqueries,
            gt_l2,
            gt_cos,
            dim,
            true,
            format!("unit-normalised vectors, rerank of k*{rf}; build_ms = serial add loop."),
        ));
    }
}
