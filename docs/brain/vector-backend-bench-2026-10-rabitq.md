# RaBitQ in the vector backend bench (2026-10-11, card 3e43be6d)

Follow-up to [`vector-backend-bench-2026-07.md`](vector-backend-bench-2026-07.md) and item A8 of
[`../research/watch-review-2026-10.md`](../research/watch-review-2026-10.md). Question: does
`ruvector-rabitq` 2.3.0 change the zvec-edge question (a small, fast, low-memory vector index for
edge nodes)?

## What the crate is (read from the 2.3.0 source and its BENCHMARK.md)

- A **flat scan**, not a graph. Random rotation, 1-bit codes, XNOR-popcount over every code, top-k
  heap. `RabitqPlusIndex` then re-ranks the best `k * rerank_factor` candidates with exact f32 L2.
- It has no HNSW and no distance hook, so it cannot be a quantizer "in front of" the kernel's HNSW.
  It is measured here **standalone, as a replacement index**. Metric is L2 (cosine only if vectors
  are unit-normalised).
- `RabitqPlusIndex` always keeps the f32 originals (even at `rerank_factor = 1`), so it is not a
  memory saving. The pure 1-bit `RabitqIndex` (no originals) was **not** benchmarked.
- Search is single-threaded; `from_vectors_parallel` (rayon) is the only parallel path.
- The crate's own BENCHMARK.md numbers (100% recall@10 at x20, n=100k, D=128) come from a
  clustered-Gaussian set with sigma 0.6; ours is much harder (below), so they do not transfer.

## Integration (bench-only, no product change)

- Runner: `crates/clawft-kernel/benches/vector_backend_bench/rabitq_runner.rs`, included from
  `vector_backend_bench.rs` behind the `rabitq-bench` feature of `clawft-kernel`.
- Dependency: `ruvector-rabitq = "2.3"` in the workspace table, optional in `clawft-kernel`, pulled
  only by `rabitq-bench`. No other crate uses it.
- Harness changes: every arm now also reports `qps_1t` (1 / mean latency, serial queries) and
  `qps_mt` (all 18 cores, each thread runs the query set 3 times). `WEFT_BENCH_ONLY=hnsw,rabitq`
  selects arms by prefix so each arm runs in a clean process for a cleaner RSS.

## Setup

- Dataset: the harness's clustered Gaussian, seed 42, 384-dim, 300 clusters, centre std 1.0,
  intra-cluster std 2.2, 100 queries, k = 10, brute-force ground truth. Sizes 10K and 100K.
- Machine: Apple M5 Max, 18 cores, 128 GB, macOS, release-profile `cargo bench`, rustc 1.95.
- Commands (fresh run, same machine, same day, one arm per process):

```text
F="--features diskann,rabitq-bench"
WEFT_BENCH_ONLY=<hnsw|rabitq|diskann|hybrid> cargo bench -p clawft-kernel --bench vector_backend_bench $F
WEFT_BENCH_N=100000 WEFT_BENCH_ONLY=<hnsw|rabitq> cargo bench -p clawft-kernel --bench vector_backend_bench $F
```

## Results, n = 10,000

`recall` is against the ground truth of the arm's own metric (HNSW and `unit` arms: cosine; others:
squared L2). `rss` is whole-process and includes the corpus and ground truth; `idx MB` is the
crate's `memory_bytes()` (originals + codes + rotation) where available.

| arm | metric | recall@10 | p50 us | QPS 1 thread | QPS 18 threads | build | rss MB | idx MB |
|---|---|---|---|---|---|---|---|---|
| hnsw | cosine | 0.493 | 809 | 191 | 159 | ~0 (incremental) | 77 | - |
| diskann PQ-on | sqL2 | 0.994 | 304 | 3,143 | 3,647 | 94.1 s | 98 | disk 18.9 MB |
| diskann PQ-off | sqL2 | 0.994 | 268 | 3,599 | 3,681 | 56.1 s | 98 | disk 18.0 MB |
| hybrid | cosine | 0.421 | 444 | 746 | 244 | 93.7 s | 118 | disk 18.9 MB |
| rabitq flat f32 (exact) | sqL2 | 1.000 | 1,292 | 730 | 7,718 | 2 ms | 70 | 14.8 |
| rabitq+ x1 | sqL2 | 0.265 | 103 | 9,609 | 122,978 | 493 ms | 89 | 15.8 |
| rabitq+ x5 | sqL2 | 0.559 | 110 | 8,703 | 92,160 | 516 ms | 105 | 15.8 |
| rabitq+ x20 | sqL2 | 0.794 | 180 | 5,024 | 61,742 | 519 ms (79 ms rayon) | 125 | 15.8 |
| rabitq+ x50 | sqL2 | 0.910 | 255 | 3,856 | 47,707 | 514 ms | 140 | 15.8 |
| rabitq+ unit-norm x20 | cosine | 0.792 | 187 | 5,174 | 67,767 | 509 ms | 170 | - |
| rabitq+ unit-norm x50 | cosine | 0.899 | 268 | 3,372 | 45,558 | 525 ms | 184 | - |

## Results, n = 100,000 (HNSW and RaBitQ only; DiskANN/hybrid build is hours at this size)

| arm | metric | recall@10 | p50 us | QPS 1 thread | QPS 18 threads | build | rss MB | idx MB |
|---|---|---|---|---|---|---|---|---|
| hnsw | cosine | 0.577 | 10,165 | 18 | 12 | ~0 | 683 | - |
| rabitq flat f32 (exact) | sqL2 | 1.000 | 12,405 | 79 | 850 | 15 ms | 638 | 148 |
| rabitq+ x5 | sqL2 | 0.451 | 504 | 1,977 | 27,408 | 4.8 s | 768 | 152 |
| rabitq+ x20 | sqL2 | 0.703 | 605 | 1,631 | 25,519 | 4.8 s | 1,023 | 152 |
| rabitq+ x50 | sqL2 | 0.822 | 740 | 1,297 | 20,753 | 4.8 s | 1,159 | 152 |
| rabitq+ unit-norm x50 | cosine | 0.812 | 727 | 1,310 | 21,421 | 4.7 s | 1,544 | - |

Recall of the RaBitQ arms against the *other* metric's ground truth is in each arm's `note[...]`
line of the JSON/table output (for example x20 at 10K: 0.551 against cosine ground truth).

## Read this before trusting the table

- **The HNSW arm looks broken, not representative.** In July the same harness gave recall 0.943 and
  p50 398 us at 10K; today it gives 0.493 and 809 us, and at 100K it takes 10 ms per query with
  18 QPS and gets *slower* with threads (QPS 12 on 18 threads). I did not investigate (out of
  scope; the July note already records that `ef_search = 14` and an unseeded `instant_distance`
  build make it noisy, and neither explains a 2x drop in recall and latency). Any "RaBitQ is
  Nx faster than HNSW" figure from this table mostly measures that arm, not HNSW in general.
- **The dataset is hard for 1-bit codes.** Intra-cluster noise (2.2) is more than twice the
  centre spread (1.0), so neighbours are only weakly closer than strangers. Recall depends on
  rerank width: x50 reaches 0.91 at 10K and 0.82 at 100K; the crate's claim of 100% recall needs
  x20 on much cleaner data. Real embeddings are likely easier; this bench cannot say.
- **Recall falls as n grows at a fixed rerank factor** (x20: 0.794 at 10K, 0.703 at 100K), so
  holding recall costs a wider rerank, i.e. more exact-distance work, as the corpus grows.
- RSS is cumulative within a process; the rabitq arms share one process, so later rows include
  earlier indexes. Use `idx MB` for the index footprint.
- One run per configuration; no variance estimate. Single run of 100 queries per arm.

## Conclusion: does it change the zvec-edge question?

No, not on this evidence.

1. **It is a different shape of index.** A linear scan with a cheap 1-bit prefilter. Per-query
   cost still grows with n (180 us at 10K, 605 us at 100K, about 3.4x for 10x data), where a
   graph index should grow far slower. It would be a candidate for small to mid collections
   (tens of thousands of vectors) on a node, not a general replacement for the HNSW/DiskANN pair.
2. **No memory saving as shipped.** `RabitqPlusIndex` keeps the f32 originals: 15.8 MB at 10K and
   152 MB at 100K, the same order as the exact flat index (14.8 / 148 MB). The 20x compression in
   the crate's BENCHMARK.md applies to the pure 1-bit index, which cannot rerank and had 8% recall@10 even on
   its own easy data. For an edge node, memory is the point of zvec, and this does not deliver it.
3. **Real strengths:** build is trivial (0.5 s serial for 10K, 4.8 s for 100K, no 56 to 94 s Vamana
   build), insert is incremental (about 20K vectors/s), and per-query latency at 10K is 3 to 10x
   better than the exact scan with a tunable recall knob. That is attractive compared with
   DiskANN's build economics, which was the July blocker.
4. **What would change the answer:** (a) a recall check on real embeddings (our harness is
   synthetic and hard); (b) benchmarking the originals-free `RabitqIndex` plus rerank against
   originals memory-mapped from disk; (c) a fix or explanation of the HNSW regression so the
   comparison is against a healthy HNSW; (d) a graph (or IVF) layer so cost stops scaling with n.

Verdict: keep `ruvector-rabitq` on watch; do not adopt. Worth a follow-up card for (a) and (c).

## Gates

`cargo check` and `cargo clippy -D warnings` on `clawft-kernel --benches` pass with
`--features diskann,rabitq-bench`, with `--features diskann`, and with default features.

## Not established

- Real-embedding recall; recall under deletes or updates; `persist` module not exercised.
- The cause of the HNSW arm's difference from July.
- Per-arm RSS free of the corpus (only whole-process RSS and the crate's `memory_bytes`).
