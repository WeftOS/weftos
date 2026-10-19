# Trajectory Atlas: egui render stress test (2026-10-18)

Can egui draw the atlas views of [docs/plans/trajectory-atlas.md](../plans/trajectory-atlas.md) at our target sizes? The harness is `crates/clawft-gui-egui/examples/atlas_stress.rs`, run with `scripts/build.sh atlas-bench` (`ATLAS_BENCH_ONLY=<substring>` picks scenes). It uses synthetic data from a fixed seed, so runs are repeatable.

**Machine**: MacBook Pro, Apple M5 Max, built-in Retina XDR, 1600x1000 pt window, native release build, eframe 0.34 (glow), vsync off, 300 timed frames per scene after 30 warm-up. The camera zooms and pans continuously, so every frame is rebuilt and re-tessellated (worst case, no caching).

| scene | points drawn | frame p50 ms | p95 | p99 | max | fps @p50 | frames >16.7 ms | build p50 ms |
|---|---|---|---|---|---|---|---|---|
| treemap 10k · circle per dot | 10000 | 2.08 | 6.01 | 13.53 | 31.91 | 480 | 2/300 | 0.08 |
| treemap 10k · one mesh | 10000 | 0.67 | 3.69 | 9.10 | 15.06 | 1485 | 0/300 | 0.15 |
| treemap 50k · one mesh | 50000 | 4.19 | 7.29 | 9.62 | 22.40 | 239 | 3/300 | 1.01 |
| treemap 100k · one mesh | 100000 | 8.27 | 9.97 | 12.08 | 14.17 | 121 | 0/300 | 2.29 |
| treemap↔umap morph 10k · mesh | 10000 | 0.70 | 3.12 | 5.94 | 21.59 | 1432 | 1/300 | 0.17 |
| treemap↔umap morph 50k · mesh | 50000 | 3.99 | 6.42 | 12.19 | 26.66 | 251 | 1/300 | 1.08 |
| 3-D forest 2k runs (all turns) · mesh | 110669 | 8.57 | 10.90 | 14.57 | 23.72 | 117 | 1/300 | 2.69 |
| 3-D forest 10k runs (all turns) · mesh | 575362 | 53.79 | 64.40 | 69.33 | 73.78 | 19 | 300/300 | 14.14 |
| timeline 5k msgs · virtualized + minimap | 10040 | 0.48 | 2.69 | 3.89 | 8.33 | 2075 | 0/300 | 0.07 |
| timeline 50k msgs · virtualized + minimap | 100040 | 2.11 | 4.86 | 9.81 | 21.08 | 474 | 2/300 | 0.75 |
| 3-D forest 10k runs · LOD (≤12 beads per stalk) · mesh | 110447 | 9.03 | 17.76 | 58.83 | 115.36 | 111 | 17/300 | 2.71 |
| 3-D forest 50k runs · LOD (≤12 beads per stalk) · mesh | 553578 | 60.45 | 86.02 | 151.68 | 273.43 | 17 | 300/300 | 13.69 |

The last two rows are the level-of-detail variant (at most 12 evenly spaced beads per
stalk), from a second run filtered to the forest scenes. That run's full-forest row repeated
the first result (53.16 ms p50).

## Verdict

**egui handles every 2-D view and the single-run timeline with plenty of headroom.**

- **Treemap, colored and sized dots:** 0.67 ms per frame at 10k runs and 8.3 ms at
  100k runs. Even at 100k runs, no frame was over 16.7 ms.
- **Animated treemap↔UMAP transition:** 0.70 ms at 10k and 4.0 ms at 50k.
- **Virtualized transcript plus lane minimap:** 0.48 ms per frame for 5k messages and 2.1 ms
  for 50k, against the 16 ms bar for T6.
- **Batching:** one `Mesh` per frame instead of one `circle_filled` call per dot is about 3×
  faster at 10k (0.67 ms against 2.08 ms p50). Use the mesh path.

**The 3-D forest at full detail is beyond egui's CPU painter.**

- **Where the ceiling is:** CPU projection and tessellation top out around 100–150k drawn
  points per frame (about 1M vertices) at 60 fps.
- **Full detail fails:** 10k runs with every turn drawn is 575k points, which ran at 19 fps.
- **LOD helps at the target size:** with at most 12 beads per stalk, 10k runs is 110k points
  at 111 fps p50. But p95 was 17.8 ms and 17 of 300 frames went over 16.7 ms, with spikes
  up to 115 ms. That's usable, but not smooth.
- **LOD doesn't scale:** 50k runs with LOD is back to 554k points and 17 fps.

**Decision for T4/T5:** stay on egui. Draw the 2-D views and the timeline with the mesh painter as
tested here. The 3-D forest, and any view past about 150k points, moves to the GPU:
- per-run point data uploaded once to a vertex buffer;
- drawn from an `egui_glow` paint callback with an instanced shader that does projection in
  the vertex stage.

Each frame then costs only uniform updates, and the same glow path works in WebGL2 for the
wasm build.

The three.js / Babylon.js client is the fallback if the callback path can't hold 60 fps at
1M+ points. That callback is **not measured yet**. It is the first task of T5, and the same
harness should gain a `forest-gpu` scene to measure it.

## Not covered

- **wasm/WebGL2 frame times:** the VS Code panel and browser. Expect them to be slower than
  native. Needs a browser run before T4 is called done.
- **Real data:** the harness uses synthetic dots and a lognormal length distribution
  (1–5000 turns). Text shaping in the timeline is a single line per row; real bodies with
  wrapping will cost more but stay bounded by the virtualized row count.
- **Interaction cost:** hit-testing and hover over 100k dots. That needs a spatial index
  (grid or BVH), planned with T4.
