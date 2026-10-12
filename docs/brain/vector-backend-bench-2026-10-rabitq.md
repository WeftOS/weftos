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
- Why HNSW QPS at 18 threads is still below its 1-thread QPS (see the addendum).
- Per-arm RSS free of the corpus (only whole-process RSS and the crate's `memory_bytes`).

## Addendum: HNSW regression diagnosed and fixed (card f91ab271)

The harness was not the cause: dataset generator, `ef_search=14`, `ef_construction=200`, cosine,
and `instant-distance 0.6.1` are unchanged since the July run (`077d670e9`). The product path
regressed in `clawft-kernel` after July, in two places.

1. WEFT-385 region-guided search was on by default (`HnswEmlConfig::path_predict_enabled`).
   `HnswService::search` then stopped traversing the HNSW graph and scored only the 2 nearest of 16
   k-means regions, cloning the path table on every query under the service lock. With 300 clusters
   that is a recall cap, not a speed knob: recall@10 0.943 -> 0.493, deterministic. It is now
   opt-in (default `false`).
2. The adaptive-ef layer called `HnswStore::set_ef_search` from inside `search`. That rebuilds the
   whole graph (instant-distance bakes `ef_search` into the built index). Once the EML models
   trained (every 1000 searches) and the learned ef differed from the configured one, searches
   rebuilt the index under the lock. This is the 100K "10 ms/query" and the throughput that fell
   as threads were added. The prediction is now advisory; `search` never retunes ef.

Harness fix: HNSW builds its graph on the first query, so the old runs charged that cost to the
latency/QPS means and reported build as `~0`. `run_backend` now does one untimed warm-up query
after `flush()`, so graph construction lands in `build_ms`.

Same machine, HNSW arm alone, release profile, `WEFT_BENCH_ONLY=hnsw`:

| n | state | recall@10 | p50 us | p95 us | QPS 1 thread | QPS 18 threads | build ms |
|---|---|---|---|---|---|---|---|
| 10K | before (HEAD, guided on) | 0.493 | 1,553 | 1,871 | 115 (includes lazy build) | 93 | ~0 (mis-reported) |
| 10K | after | 0.939 | 459 | 585 | 2,132 | 321 | 29,372 |
| 100K | before (table above) | 0.577 | 10,165 | - | 18 | 12 | ~0 (mis-reported) |
| 100K | after | 0.909 | 628 | 899 | 1,512 | 243 | 476,007 |

Still open: QPS at 18 threads is below 1-thread QPS because `HnswService::search` holds the store
mutex and the EML mutex for the whole query (`HnswStore::query` takes `&mut self` for the lazy
rebuild), and EML training/recall checks also run under that lock. The graph build is slow
(29 s at 10K, 476 s at 100K at `ef_construction=200`, 384-dim); that was always the cost and is
now visible. Regression tests: `default_search_recall_floor`, `search_never_changes_ef_search`
(`hnsw_service.rs`) and `path_predict_is_opt_in` (`hnsw_eml.rs`).


## Addendum 2 (2026-10-12): lock split and build time (cards 249077fb, 63d21285)

Machine: Apple M-series, release profile, `WEFT_BENCH_ONLY=hnsw`, 384-dim, 100 queries, seed 42,
`ef_search=14`. One run per row; the graph seed is entropy-derived, so recall moves a point or
two between runs (see the efc=150 row below, which beat efc=200).

### What was wrong, what changed

1. **Reads were serialized and could stall on training.** `HnswService::search` held the store
   mutex and the EML mutex for the whole query, and the thread that crossed `train_every_n`
   trained four models inline. The bench's wall time is the slowest thread, so one stalled
   trainer set the 18-thread QPS (245 with EML bookkeeping, 66,918 with it disabled, same build).
   Now: the store is an `RwLock` and searches share the read lock through the new
   `HnswStore::query_shared`; graph builds run off-lock (`rebuild_job` / `install_index`, guarded
   by a layout generation so a build never installs over a delete or upsert); EML bookkeeping is
   `try_lock` and skipped when busy; training runs on its own short-lived thread
   (`HnswEmlManager::set_auto_train(false)` + `train_due`). Only the very first build blocks
   searches; later rebuilds are done by one thread while the rest answer from the old graph plus
   the brute-force tail.
2. **Graph build was dominated by the distance function.** `1 - cosine` recomputed both norms with
   serial f32 sums on every distance call. Points now cache `1/||v||` and use an 8-accumulator dot
   product. `instant-distance 0.6.1` already builds in parallel (rayon) and fixes M at 32, so
   `ef_construction` is the only build knob.

### Results (ef_construction=200, the default)

| n | state | recall@10 | p50 us | QPS 1 thread | QPS 18 threads | build ms |
|---|---|---|---|---|---|---|
| 10K | before | 0.939 | 459 | 2,132 | 321 | 29,372 |
| 10K | after | 0.939 | 106 | 9,055 | 113,949 | 2,833 |
| 100K | before | 0.909 | 628 | 1,512 | 243 | 476,007 |
| 100K | after | 0.912 | 228 | 3,862 | 55,084 | 66,867 |

Multi-thread QPS now scales above 1-thread (12.6x at 10K, 14x at 100K).

### ef_construction sweep (after)

| n | efc | recall@10 | build ms | QPS 1t | QPS mt |
|---|---|---|---|---|---|
| 10K | 40 | 0.897 | 669 | 13,775 | 185,247 |
| 10K | 64 | 0.924 | 1,994 | 9,026 | 119,797 |
| 10K | 100 | 0.938 | 2,584 | 7,285 | 120,287 |
| 10K | 200 | 0.941 | 3,253 | 7,654 | 100,844 |
| 100K | 40 | 0.687 | 6,120 | 7,950 | 112,723 |
| 100K | 64 | 0.752 | 16,377 | 6,288 | 94,310 |
| 100K | 100 | 0.863 | 25,917 | 5,561 | 80,898 |
| 100K | 150 | 0.956 | 47,568 | 4,360 | 60,075 |
| 100K | 200 | 0.912 | 66,867 | 3,862 | 55,084 |
| 100K | 100, ef_search=24 | 0.974 | 32,024 | 4,375 | 54,232 |

### Decision

Defaults stay at `ef_construction=200`: it is the only setting that held recall@10 >= 0.9 at both
sizes under a fixed `ef_search=14` (budget: about 3 s at 10K, about 70 s at 100K, down from 29 s
and 476 s). Lowering efc to 100 saves roughly 60 percent of build time at 100K but drops recall
below 0.9 unless `ef_search` is raised to about 24, which is the cheaper lever when build time
matters; that trade is a per-deployment choice, so it is a bench knob
(`WEFT_BENCH_HNSW_EFC`, `WEFT_BENCH_HNSW_EF`) and not a changed default.

### Not established

- Single runs; the efc=150 versus efc=200 inversion at 100K shows run-to-run recall noise of at
  least four points at that size. Treat the sweep as a shape, not as exact recall values.
- Synthetic clustered data only; real-embedding recall unchanged from the earlier caveat.
- The brute-force tail and the index values still map by position, so deletes and upserts between
  rebuilds can make the stale graph point at the wrong entry (pre-existing in `HnswStore`; the new
  layout generation only stops an off-lock build from being installed over such a change).

## Addendum 3 — stale-mapping fix re-benched (2026-10-12)

The stale-graph mapping fix (14b2eb909: graph values are sequence numbers resolved at query
time) was first benched under heavy machine load (load average ~29) and looked slower. Re-run on
a quieter machine (load ~6-8), 10K, dim 384, release, `WEFT_BENCH_ONLY=hnsw`, three runs:

| Run | recall@10 | p50 µs | p95 µs | qps 1 thread | qps 18 threads | build ms |
|---|---|---|---|---|---|---|
| before the fix | 0.936 | 93.3 | 130.0 | 10,023 | — | — |
| after, run 1 | 0.937 | 97.2 | 137.2 | 9,841 | 117,051 | 2,337 |
| after, run 2 | 0.941 | 98.8 | 129.9 | 9,910 | 127,795 | 2,377 |
| after, run 3 | 0.938 | 77.5 | 113.0 | 12,162 | 131,137 | 2,722 |

No measurable regression: latency and single-thread throughput are within run-to-run noise of the
pre-fix numbers, and recall is unchanged.
