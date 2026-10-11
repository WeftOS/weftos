# Large-scale 3DGS + LOD — appearance twin of Urth BVH (S5)

**Date:** 2026-09-21  
**Status:** Session research capture (not an ADR)  
**Parent:** [README.md](./README.md) · **Pri A**  
**WeftOS already decided:** BVH is the geometric index (ADR-056). Appearance vs structure is dual output (ADR-078). Urth is sparse-first L0–L5 (ADR-079). Generative fill is cosmetic, never metric truth. Planet-scale BVH shards by region — residual, not filed.

**Honesty rule:** city-scale Gaussian papers solve **photoreal appearance at km²**. They do **not** replace `clawft-bvh` AABBs, OSM footprints, or chain-backed Objects. Octree-GS is the appearance twin of our AABB hierarchy — **twins, not one tree.**

---

## 1. Appearance LOD vs geometric BVH LOD (they are twins, not one tree)

Two independent hierarchies already exist in WeftOS doctrine. 2024–2026 large-scale 3DGS work is almost entirely about the **left** column. Mixing them into one splat-octree is how you lose agent queries.

| | **Appearance LOD** (splat / SOG) | **Geometric BVH LOD** (Urth / `clawft-bvh`) |
|---|---|---|
| **Job** | What it *looks like* at this camera distance | What *is there* (AABB, identity, occupancy) |
| **Primitive** | Anisotropic Gaussians, anchors, SH, opacity | AABB (broad-phase) + tagged leaf payload |
| **Tree** | Octree / chunk / LightGaussian compression levels | BVH over region → site → room → object |
| **Query** | Rasterize / stream visible primitives | `query_sphere`, ray, overlap, chain audit |
| **SoT** | SOG / ply / quilt layers (`appearance_ref`) | BVH + chain (ADR-056 / 078) |
| **LOD trigger** | Projected size, camera distance, chunk | Urth L0–L5 + frustum + confidence |
| **Failure mode** | Too many primitives in the sorter (FPS cliff) | Single-process OOM; fake density |
| **2025 analog** | Octree-GS, CityGaussian LoD, LODGE | ADR-079 region graph (already decided) |

```
        appearance LOD (SOG / GS)              geometric LOD (BVH)
        ─────────────────────────              ───────────────────
 L2     coarse city GS / map tiles             OSM footprints, streets
 L3     site octree / aerial LoD               campus AABBs, anchors
 L4     quilt layers + chunk stream            zone AABB, SPLAT_SCENE
 L5     optional gaussian_subset               WM_OBJECT / SURFACE / VOLUME

        same region_id  ────────────────────►  parent/child region graph
        (geohash / urth/…)                     ECEF identity, ENU capture
```

**Why they must stay twins**

1. **Capacity math is different.** VastGaussian: a 32 GB GPU trains ~11 M Gaussians; Mip-NeRF 360 Garden (<100 m²) already wants ~5.8 M for fidelity. CityGaussian: RTX 3090 24 GB OOMs above ~11 M; MatrixCity 2.7 km² wants 20–25 M. That is **one campus of appearance**, not a planet of structure.
2. **Agents do not query Gaussians.** ADR-078: “Source of truth for ‘what is where’ is BVH + chain, not the SOG file alone.” `WM_OBJECT` payload may hold `appearance_ref.kind = gaussian_subset|none` — optional, never inverted.
3. **LOD selection is different.** Octree-GS picks *anchor LOD ℓ from observation distance* so the rasterizer stays real-time. BVH LOD picks *which Object leaves exist at this Urth level* so `query_sphere` stays cheap. Collapsing both into “one octree of Gaussians” makes occupancy, affordances, and chain audit unqueryable.
4. **Sparsity is honest only on the geometric side.** Appearance can compress distant blocks (CityGaussian LightGaussian LoD 0/1/2). Geometry must keep `unobserved` volumes — not a blurry Gaussian blob pretending to be a building.

rUv WorldGraph already practices the split in the browser: SuperSplat draws the splat; WASM overlay draws rooms/sensors/provenance (WorldGraph ADR-200/201/202). That is the commercial form of ADR-078. Do not import their graph as our BVH; compose the overlay pattern.

---

## 2. What shipped (live sources, 2024 → 2026-09)

Curated index (updated **2026-09**): [DeepLabc/LargeScale_3DGS](https://github.com/DeepLabc/LargeScale_3DGS). City & aerial + street/driving + feed-forward 4D. Use it as the watchlist; do not treat every entry as a WeftOS dependency.

### 2.1 CityGaussian / CityGaussianV2 (the city-block recipe)

| | **CityGaussian (CityGS)** | **CityGaussianV2** |
|---|---|---|
| Venue | ECCV 2024 · [arXiv:2404.01133](https://arxiv.org/abs/2404.01133) | **ICLR 2025** · [arXiv:2411.00771](https://arxiv.org/abs/2411.00771) |
| Code | [github.com/Linketic/CityGaussian](https://github.com/Linketic/CityGaussian) (main = V2 on Gaussian Lightning v0.10.1; `V1-Original` branch) | same repo |
| Project | [dekuliutesla.github.io/citygs](https://dekuliutesla.github.io/citygs/) | [dekuliutesla.github.io/CityGaussianV2](https://dekuliutesla.github.io/CityGaussianV2/) |
| License | **CC BY-NC-SA 4.0** — pattern only, not a crate dep | same |

**V1 pattern (steal):**

1. Train a **coarse global Gaussian prior** (30k iters) so blocks share geometry and SSIM-based camera assignment is meaningful.
2. **Contract** unbounded Gaussians to a cube (Mip-NeRF 360-style); uniform grid on the contracted cube so empty sky does not spawn empty GPUs.
3. **Assign cameras** if the pose is inside the block **or** dropping the block’s Gaussians changes SSIM above ε (contribution, not naive AABB of cameras).
4. Fine-tune blocks in parallel in *uncontracted* world space; concat Gaussians whose means fall in the block AABB. Global prior is what makes concat not a seam disaster.
5. **Appearance LoD:** LightGaussian compression at 50% / 34% / 25% → LoD 2/1/0. **Block-wise** (not point-wise) distance → one LoD per block; MAD bounds kill floater-inflated AABBs; frustum test on the 8 corners. MatrixCity: no-LoD ~21.6 FPS / 23.7 M GS; with LoD ~53.7 FPS at nearly LoD-2 quality. Project page: 25 M GS @ 18 FPS without LoD → 36 FPS with LoD (A100). Distance bands they used: 0–200 m / 200–400 m / 400 m+.

**V1 limits they admit:** static-scene assumption; **joint aerial+street training degrades** (Horizon-GS exists because of this); V1 LoD is still a TODO on the V2 main branch.

**V2 pattern (steal geometry, still not BVH):** 2DGS surfaces instead of unstructured 3DGS blobs; decomposed-gradient densification + depth regression; **elongation filter** to stop 2DGS Gaussian-count explosion; parallel pipeline **10× compression, ≥25% train-time, 50% VRAM**. They added a TnT-style geometry protocol for large scenes (F1, not just PSNR). 2025-10-18: joint pose + 3DGS with [VGGT-X](https://github.com/Linketic/VGGT-X) — compose with [feedforward-reconstruction.md](./feedforward-reconstruction.md) (S3), do not wait on overnight COLMAP for every site.

**VRAM reality check from their FAQ:** downsample images, drop `max_cache_num`, raise `prune_ratio`. Blocks with **<50 images are skipped** to avoid overfitting — a region with too-fine geohash will silently not train. That is a WeftOS ingest bug waiting to happen if we shard L4 quilts too small.

### 2.2 Octree-GS — LOD-structured Gaussians (appearance octree)

- **TPAMI 2025** (accepted 2025-05-08; DOI [10.1109/TPAMI.2025.3568201](https://doi.org/10.1109/TPAMI.2025.3568201); PMID 40338715) · preprint [arXiv:2403.17898](https://arxiv.org/abs/2403.17898) (v2 2024-10-17)
- Project: [city-super.github.io/octree-gs](https://city-super.github.io/octree-gs/) · code: [github.com/city-super/Octree-GS](https://github.com/city-super/Octree-GS)
- **Octree-AnyGS** (2024-09): same LOD wrapper over explicit 3D-GS / 2D-GS and neural Scaffold-GS.

Sparse SfM cloud → octree on a bounded volume → **anchor Gaussians per LOD**. Render: for each occupied voxel, pick ℓ from observation center, invoke anchors **up to** that level. Densify with **grow-and-prune**; **progressive training** so fine levels are not born before coarse ones exist. Claim: up to **10×** vs large-scene SOTA, real-time, generalizes across Gaussian families.

**WeftOS read:** this is the closest *shape* to a BVH — hierarchical spatial bins, coarser primitives far away. It is still an **appearance** structure (anchors emit Gaussians for rasterization). Do not store `WM_OBJECT` identity on octree anchors. Do **key** Octree-GS / AnyGS LODs to the same `region_id` as the BVH parent AABB so streaming can load appearance LoD 0 while structure is already queryable.

### 2.3 VastGaussian — first partition-and-merge 3DGS (CVPR 2024)

- [arXiv:2402.17427](https://arxiv.org/abs/2402.17427) · [vastgaussian.github.io](https://vastgaussian.github.io/)
- **Progressive cells** + **airspace-aware visibility** for which cameras and points belong in a cell (not 2D grid of poses). Parallel train, concat+filter merge.
- **Decoupled appearance modeling** — exposure/lighting variation is a per-image embedding, not baked into geometry. Direct analog of free-form quilt lighting seams ([splat-freeform-quilt.md](../../weftos/splat-freeform-quilt.md) §10).
- Campus scene: **27.4 M** Gaussians vs 8.9 M modified 3DGS; ~2.5–3 h; 1080p **126 FPS** (Mill-19) / **172 FPS** (UrbanScene3D) on 3090-class. No appearance LoD — FPS comes from “we actually finished the train,” not from distance compression. CityGaussian cites VastGaussian as the prior that still left the zoom-out sorter bottleneck.

### 2.4 LODGE — chunk stream + opacity blend (NeurIPS 2025 spotlight)

- [arXiv:2505.23158](https://arxiv.org/abs/2505.23158) (v2 2025-10-29) · [lodge-gs.github.io](https://lodge-gs.github.io/)
- Google / Google DeepMind / CTU Prague. **Mobile-memory** target.
- Build coarser LODs by **depth-aware 3D smoothing → importance prune → fine-tune**. Unlike Octree-GS, they **do not recompute the active Gaussian list every frame**: cluster cameras into chunks, **precompute** active sets, render the two nearest chunks with **opacity blending** so chunk boundaries do not pop (they call out a black car as the tell).
- Reported: **257 FPS** vs Zip-NeRF 0.09; **219** vs Octree-GS 119; **280** vs H3DGS 33. Quality matched, memory fits phones.

**WeftOS read:** this is Urth **E5 client LOD streaming** for appearance — coarse globe tiles, then two nearest L4 quilt chunks with a blend. Structure streaming is a different problem (BVH shard fetch).

### 2.5 Horizon-GS — aerial ↔ street (CVPR 2025)

- [arXiv:2412.01745](https://arxiv.org/abs/2412.01745) · [city-super.github.io/horizon-gs](https://city-super.github.io/horizon-gs/) · code [InternRobotics/HorizonGS](https://github.com/InternRobotics/HorizonGS) (also mirrored as OpenRobotLab)
- CityGaussian’s own conclusion: mixing aerial and street in one train **hurts**. Horizon-GS: **chunked LOD anchors** (Octree-GS-inspired), two-phase train — coarse whole-scene (`K_aerial`) then street-detail fine (`K`). New aerial-to-ground dataset.
- **WeftOS read:** L2/L3 (drone / OSM / Sentinel) and L4 (phone / Pi / multi-cam) must not share one Gaussian train. Quilt contributions already separate by `contribution_id`; keep aerial basemap appearance off the room SOG.

### 2.6 CityGS-X — skip partition-merge (ICCV 2025)

- [arXiv:2503.23044](https://arxiv.org/abs/2503.23044) · [lifuguan.github.io/CityGS-X](https://lifuguan.github.io/CityGS-X/) · code [github.com/gyy456/CityGS-X](https://github.com/gyy456/CityGS-X)
- **PH²-3D** (parallelized hybrid hierarchical 3D). **Abandons merge-and-partition**; batch-level multi-task render; **dynamic LoD voxels** across GPUs. **5 000+ images in ~5 h on 4× 4090**; competing partition pipelines OOM.
- **WeftOS read:** attractive for **one L3 site** on a small GPU pack. Not a planetary architecture — “no partition” means “no *training* partition for this scene,” not “one process holds Earth.”

### 2.7 CityGo — proxy buildings + residual Gaussians (closest L2 hybrid)

- [arXiv:2505.21041](https://arxiv.org/abs/2505.21041) · code [github.com/LiYukeee/CityGo](https://github.com/LiYukeee/CityGo) (ACM MM / SIGGRAPH Asia 2025 listing; list also tags ACM MM 2025)
- Textured **proxy meshes** from MVS (GIS building masks) + **residual Gaussians** where proxy≠photo + **surrounding** Gaussians with importance downsample.
- Area-H: 3DGS 40 M GS / 6.3 GB / 34 FPS vs CityGo **4.7 M / 741 MB / 161 FPS**. Proxy mesh **~62×** smaller than MVS mesh. Jetson-class: ≥20 FPS on 1.5 km².

**WeftOS read:** this is OSM / Microsoft Building Footprints as `WM_SURFACE` / building AABBs (L2 structure) plus residual GS as L2/L3 **appearance**, not a second geometric truth. Residual GS must stay `appearance_ref`, labeled non-metric if the proxy came from OSM (confidence ~0.4 per Urth fusion rules).

### 2.8 Watch (sourced, not productize this cycle)

| Item | Why watch | URL |
|---|---|---|
| **A LoD of Gaussians** (SIGGRAPH 2026) | Out-of-core train+render on **one consumer GPU**, no chunk artifacts; Gaussian hierarchy + Sequential Point Trees. Explicit GS. Claims LoD train *and* render. | [doi:10.1145/3799902.3811076](https://doi.org/10.1145/3799902.3811076) · [arXiv HTML](https://arxiv.org/html/2507.01110) |
| **GaussianCity** (CVPR 2025) | *Generative* unbounded city GS — cosmetic only under ADR-079 | [arXiv:2406.06526](https://arxiv.org/abs/2406.06526) |
| **HUG** (ICCV 2025) | Hierarchical urban GS, block-based aerial | listed on LargeScale_3DGS 2025 |
| **GigaSLAM** (ACM MM 2025) | Hierarchical GS *inside SLAM* — L4 live densify | [arXiv:2503.08071](https://arxiv.org/abs/2503.08071) |
| **Atlas** on-device city GS (2026-09) | VR / phone city-scale; name collides with World Labs Atlas — different paper | [arXiv:2609.02352](https://arxiv.org/abs/2609.02352) |
| **BlitzGS** | “Lightning” city-scale train | [arXiv:2605.13794](https://arxiv.org/abs/2605.13794) |
| **H-3DGS / Hierarchical 3DGS** | LODGE outdoor baseline; city-block hierarchy | LODGE comparisons |
| **Grendel-GS** | Scale-out 3DGS training (CityGS-X related work) | CityGS-X README |

---

## 3. Region sharding patterns we can steal

ADR-079 non-goal: “Single monolithic BVH of the planet in one process (**shard by region**).” Urth doc risk: “Single BVH OOM → shard by geohash / region.” The 3DGS literature is a catalog of **how they sharded appearance**. Map the *ideas* onto region IDs, not onto a second spatial index.

| Pattern | Who | Steal for WeftOS | Do not steal |
|---|---|---|---|
| **Coarse global prior, then block fine-tune** | CityGaussian | Quilt **periodic retrain** of a site: one cheap global SOG prior, then contribution patches. Stops floaters at seams. | Training the prior as geometric SoT |
| **Contract-then-grid** | CityGaussian | Only for **appearance** partition of a bounded L3 site. | Contracting ECEF into a cube as Urth identity (WGS84 stays) |
| **Visibility / contribution camera assign** | CityGaussian SSIM drop; VastGaussian airspace visibility | When ingesting a contribution into region R: keep frames that *see* R, not only cameras whose pose AABB overlaps R | SSIM as a BVH query |
| **Airspace-aware cells** | VastGaussian | Outdoor L3: cameras above a block still train that block (drone over a building) | 2D geohash of camera lat/lon only — misses nadir |
| **Decoupled appearance embed** | VastGaussian | Quilt lighting/exposure: per-contribution appearance code, shared structure Objects | Baking Tuesday overcast into WM_SURFACE |
| **Concat in original frame after per-block filter** | CityGaussian, VastGaussian | Quilt `layer_stack` then bake: filter Gaussians to the region AABB in **ENU**, concat | Averaging Gaussian parameters across unaligned frames (quilt §2.4 already forbids this) |
| **Block-wise LoD, not per-Gaussian** | CityGaussian | Client: pick SOG LoD per **region AABB**, not per splat. Matches BVH leaf granularity. | Point-wise distance (they measured it 2× slower) |
| **MAD / floater AABB** | CityGaussian | `SPLAT_SCENE` AABB from sparse cloud must not be floater-max; same bug as W0 export | Using raw min/max of GS means as WM_VOLUME |
| **Skip starved shards** | CityGaussian FAQ (<50 images) | If a geohash cell has too few views, **do not train**; attach as `unobserved` + maybe structure-only | Silent empty SOG that looks like a hole in the city |
| **Precomputed chunk active-sets + opacity blend** | LODGE | E5 streaming: two nearest L4 chunks, blend; no per-frame rebuild | Rebuilding the BVH every camera move |
| **Different LOD depth for aerial vs street** | Horizon-GS `K_aerial` vs `K` | L2 tiles vs L4 quilt — different appearance budgets, same `region_id` | One train mixing drone orbit and phone walk |
| **Proxy mesh + residual GS** | CityGo | L2 OSM / footprint AABBs + residual appearance | Residual GS as building metric extents |
| **PH²-3D / no merge** | CityGS-X | Optional **single-site** train backend on 4×GPU | Replacing geohash shards at planet scale |
| **Out-of-core hierarchy** | A LoD of Gaussians (watch) | Future: one GPU, no chunk pops | Waiting on SIGGRAPH 2026 code before E1 |

**Geohash / region key (WeftOS-shaped):**

```
region/urth/<geohash-or-admin>/…     ECEF identity (L0–L2)
region/urth/…/<site_id>              ENU origin + T_ecef_local (L3)
region/urth/…/<site_id>/<zone_id>    quilt AABB (L4)
```

Appearance artifacts (`appearance/layers.json`, `fused.sog`, optional octree-gs checkpoint) hang off the **same** ids. BVH parent pointers are the geometric tree. Gaussian octrees / chunks are **payloads of** `SPLAT_SCENE` / quilt layers, not siblings of `LeafId`.

---

## 4. What NOT to do (one planetary Gaussian train)

CityGaussian’s own numbers are the proof: **1.5–2.7 km² already wants 20 M+ Gaussians and still needs LoD to stay real-time.** VastGaussian: Garden-sized rooms eat half a 32 GB card. Scaling that to Earth is not a bigger YAML.

| Anti-pattern | Why it fails | What we do instead |
|---|---|---|
| **One planetary 3DGS train** | VRAM, sorter, COLMAP, lighting, dynamics, licensing | L0–L2 open basemap; GS only where we captured (ADR-079 sparse-first) |
| **One octree to rule appearance and structure** | Agents cannot `query_sphere` SH coefficients; chain cannot audit a Gaussian mean | Twins: SOG LoD + BVH LOD, joined by `region_id` / `appearance_ref` |
| **Put every Gaussian in the BVH** | Leaf explosion; ADR-056 AABB is broad-phase for *objects*, not 25 M ellipsoids | `SPLAT_SCENE` one AABB per job/layer; Objects are instances |
| **Geohash so fine that shards starve** | CityGS skips blocks with <50 views; “holes” look like missing city | Min-view gate; merge starved cells up one geohash precision |
| **Camera-in-cell only assignment** | Misses airspace / looking-in; edge floaters | Visibility/contribution assign (Vast / CityGS) |
| **Joint aerial + street one SOG** | CityGS conclusion; Horizon-GS exists | Separate appearance layers; structure merge by instance_id |
| **Generative city as survey** | GaussianCity, InceptionGS, etc. | Cosmetic, labeled non-metric (ADR-079 §2) |
| **Fork CityGaussian into `crates/`** | **CC BY-NC-SA 4.0** | Pattern in docs; Brush / in-tree train backends stay ours ([splat-train-backends.md](../../weftos/splat-train-backends.md)) |
| **CityGS-X “no partition” as planet policy** | 5k images / one scene / 4 GPUs | Allowed as an L3 site backend, not Urth root |
| **V1 LoD assumed present on V2 main** | Repo TODO: “Support of V1 style LoD” still open | If we prototype CityGS-style LoD, pin the *paper*, not `main` |
| **Treat SuperSplat overlay as geometry SoT** | rUv ADR-200 is a viewer | Compose overlay; BVH remains query truth |

---

## 5. Map to Urth L2 city / L3 site / L4 quilt

| Urth LOD | Geometry (BVH) | Appearance (3DGS analog) | Writer | Paper to copy |
|---|---|---|---|---|
| **L0 Planetary** | Ellipsoid, coarse DEM | Globe imagery (licensed). **No GS.** | System | — |
| **L1 Admin** | Borders, major roads | Optional tiles. **No GS.** | System + OSM | — |
| **L2 City** | Street graph, **building footprints → WM_OBJECT stubs / WM_SURFACE** | Map tiles; optional **CityGo proxy + residual GS** for a *pilot city*, never planet | Open data; residual GS only if captured | CityGo; CityGaussian *block size* as geohash precision hint |
| **L3 Site / campus** | Parcel AABBs, ENU origin, anchors | One **bounded** GS train: CityGS-X *or* VastGaussian cells *or* Horizon-GS if aerial+street | Ops + capture | CityGS-X (single-site scale-out); Horizon-GS (two-phase); VastGaussian (visibility cells) |
| **L4 Zone / room** | Quilt Object AABB; `SPLAT_SCENE` | Free-form **layer_stack** SOGs; Octree-GS / LODGE LoD for render; LODGE chunks for stream | Capture edges | Octree-GS (in-memory LoD); LODGE (stream + blend); CityGS global-prior-then-patch (retrain) |
| **L5 Object** | Instance AABB, affordances | Optional `gaussian_subset`; usually none | Structure + human | ADR-078 payload; not a 3DGS paper |

**Fly-through (client, north star — Urth §8):**

1. Globe: L0–L2 basemap only (cheap).
2. City: footprints + optional residual GS for the *one* densified neighborhood.
3. Site: L3 SOG LoD 0 (coarse anchors) while BVH already answers “what buildings exist.”
4. Enter zone: LODGE-style load two L4 chunks, opacity blend; BVH Objects overlay (SuperSplat pattern).
5. Object pick: `WM_OBJECT` + chain provenance — never a Gaussian id.

**Quilt seam (already designed, now with paper names):**

- Layer stack = CityGaussian concat-after-filter.
- Periodic full retrain = CityGaussian “global prior + block fine-tune” at **region** granularity, not planet.
- Lighting seams = VastGaussian decoupled appearance.
- Aerial vs phone = Horizon-GS two-phase / two layers.
- Client FPS = Octree-GS + CityGS block LoD + LODGE chunks.

---

## 6. Apply / compose / watch

Exact WeftOS seam: **no new spatial index.** Appearance LoD hangs off existing `region_id` / `SPLAT_SCENE` (`0x5350_0001`) / quilt `layers[]`. Structure stays `clawft-bvh` AABB + `WM_*` tags (`0x5350_0010`–`0015`). Train backends stay Brush-default ([splat-train-backends.md](../../weftos/splat-train-backends.md)). MetaHarness is not a runtime dep (ADR-096).

### Apply (this cycle — patterns in docs / tickets, not a 3DGS crate)

- **Twin-tree doctrine** in world-builder + splat pipeline copy: appearance LoD ≠ BVH LoD; join key = `region/urth/…`.
- **Shard key:** geohash/admin for L2, site ENU AABB for L3, quilt zone for L4. Min-view gate before any GS job.
- **Camera assignment:** contribution frames that *see* the region (visibility), not pose-in-cell only.
- **Client streaming sketch (E5):** LODGE two-chunk + opacity blend for SOG layers; BVH streamed independently.
- **L2 hybrid (pilot city only):** OSM footprints as structure; residual GS optional and non-metric unless capture-backed.

### Compose (do not reinvent)

- **S3 VGGT / VGGT-X** already wired in CityGaussian’s `doc/vggt_x.md` (2025-10-18) — pose-poor sites skip overnight COLMAP.
- **S1 Marble dual export** — product language for appearance vs collider; we already decided this (ADR-078).
- **S7 rUv SuperSplat overlay** — browser pattern for L4: splat + AABB/provenance cards. Geometry SoT stays WeftOS BVH ([ruv-worldgraph-vs-weftos.md](../ruv-worldgraph-vs-weftos.md)).
- **Quilt Q2–Q4** — layer stack + periodic bake is the CityGaussian concat/retrain story without their license.

### Watch (do not productize)

- A LoD of Gaussians out-of-core (SIGGRAPH 2026) — code landed, licence read 2026-10-11 (see §7a): **research-only, non-commercial. Pattern only; no code or checkpoints into weftos.** Re-evaluate chunking as a design reference, not a dependency.
- CityGS-X PH²-3D as a *site* backend if we ever grow a multi-GPU train box.
- Atlas / BlitzGS / HUG / GigaSLAM — list hygiene via DeepLabc/LargeScale_3DGS (re-pull when E1 starts).
- GaussianCity-class generators — cosmetic only.
- CityGaussian V1 LoD landing on V2 `main` — still a TODO as of the live README.

---

## 7. Risks

| Risk | Mitigation |
|---|---|
| Generative-as-truth | ADR-079: unknown stays unknown; residual GS labeled |
| GPU / VRAM | Never train above site; LoD for render; skip starved shards |
| License | CityGaussian **CC BY-NC-SA 4.0** — no copy into weftos; HuggingFace checkpoints similarly encumbered |
| Closed / no-data | CityGo dataset “not publicly available” — pattern only |
| Name collision | World Labs **Atlas** (S1) ≠ 2026 on-device GS **Atlas** paper |
| Floater AABBs polluting BVH | MAD / percentile bounds before `SPLAT_SCENE` publish (W0 residual) |
| Aerial/street one train | Separate layers; Horizon-GS if we ever fuse |
| Partition hyperparams | CityGS-X and A LoD of Gaussians exist because partition is brittle — keep shard policy in world-builder, not in a copied YAML |

---

## 7a. A LoD of Gaussians licence (read 2026-10-11)

Source: `LICENSE.md` in [FelixWindisch/LoDOfGaussians](https://github.com/FelixWindisch/LoDOfGaussians), `main` at commit `73f547341547ae021ff036ad74fdf73a1240b450` (pushed 2026-07-03). GitHub reports the licence as `NOASSERTION`. The file does not state its own terms: it says the original 3DGS and Hierarchical-3DGS (Inria / TUW) licences "also apply" to every part built on them, and it embeds the Hierarchical-3D-Gaussians licence text. The README says the release relies on the gsplat rasterizer and builds `simple-knn` and `gaussianhierarchy` submodules; those carry their own licences, which were not read.

| Term | What it says |
|---|---|
| Commercial use | **Prohibited** without prior explicit consent of the licensors (Inria). Use is "for research and/or evaluation purposes only"; research users "both academic and industrial" are allowed, free of charge. |
| Redistribution | Only under the same licence, with a complete copy of it and unmodified copyright, patent, trademark and attribution notices. |
| Derivative works | May carry extra terms only if the research-use limitation still applies to them. |
| Sublicensing | No right to sublicense. |
| Attribution | Notices must be retained; citing the publications is "strongly encouraged". |
| Field of use | Research / evaluation only. No other use without prior consent. |
| Patents | No patent grant or patent clause beyond the retained-notice duty. The software is described as in registration with the APP (French programme registry). |
| Warranty | None. |

**Decision:** the licence decides it. Same class as CityGaussian (section 7): usable to read and to benchmark in a research setting, not copyable into weftos, not shippable in a cog or product, no derivative checkpoints. Verdict moves from "still watch, licence read pending" to **watch the paper's chunking ideas as a pattern only; no code path**. A commercial route would need a written consent from Inria, which no ticket should assume.

---

## 8. Sources

Fetched 2026-09-21.

- [DeepLabc/LargeScale_3DGS](https://github.com/DeepLabc/LargeScale_3DGS) — **Last updated: 2026-09**
- CityGaussian ECCV 2024: [arXiv:2404.01133](https://arxiv.org/abs/2404.01133) · [project](https://dekuliutesla.github.io/citygs/) · [HTML](https://arxiv.org/html/2404.01133)
- CityGaussianV2 ICLR 2025: [arXiv:2411.00771](https://arxiv.org/abs/2411.00771) · [project](https://dekuliutesla.github.io/CityGaussianV2/) · [code](https://github.com/Linketic/CityGaussian)
- Octree-GS TPAMI 2025: [arXiv:2403.17898](https://arxiv.org/abs/2403.17898) · [IEEE](https://doi.org/10.1109/TPAMI.2025.3568201) · [project](https://city-super.github.io/octree-gs/) · [code](https://github.com/city-super/Octree-GS)
- VastGaussian CVPR 2024: [arXiv:2402.17427](https://arxiv.org/abs/2402.17427) · [project](https://vastgaussian.github.io/)
- LODGE NeurIPS 2025 spotlight: [arXiv:2505.23158](https://arxiv.org/abs/2505.23158) · [project](https://lodge-gs.github.io/)
- Horizon-GS CVPR 2025: [arXiv:2412.01745](https://arxiv.org/abs/2412.01745) · [project](https://city-super.github.io/horizon-gs/) · [code](https://github.com/InternRobotics/HorizonGS)
- CityGS-X ICCV 2025: [arXiv:2503.23044](https://arxiv.org/abs/2503.23044) · [project](https://lifuguan.github.io/CityGS-X/) · [code](https://github.com/gyy456/CityGS-X) · [open access](https://openaccess.thecvf.com/content/ICCV2025/html/Gao_CityGS-X_A_Scalable_Architecture_for_Efficient_and_Geometrically_Accurate_Large-Scale_ICCV_2025_paper.html)
- CityGo: [arXiv:2505.21041](https://arxiv.org/abs/2505.21041) · [code](https://github.com/LiYukeee/CityGo)
- A LoD of Gaussians SIGGRAPH 2026: [arXiv HTML](https://arxiv.org/html/2507.01110) · [code and LICENSE.md](https://github.com/FelixWindisch/LoDOfGaussians) (read 2026-10-11, commit 73f5473)
- WeftOS: [ADR-079](../../adr/adr-079-urth-digital-twin.md) · [urth-digital-twin.md](../../weftos/urth-digital-twin.md) · [ADR-078](../../adr/adr-078-splat-feeds-world-model.md) · [ADR-056](../../adr/adr-056-bvh-spatial-index.md) · [splat-freeform-quilt.md](../../weftos/splat-freeform-quilt.md) · [splat-to-world-model.md](../../weftos/splat-to-world-model.md)
- rUv: WorldGraph ADR-200 SuperSplat overlay (via `search_ruvnet`)

---

## 9. Recommended tickets (do **not** file unless asked)

Plane still has **no open spatial tickets**; residuals live in docs ([README](./README.md) queue). Suggested new / tightened residuals:

1. **Appearance LOD keyed to Urth L2–L4 `region_id`** — Octree-GS / CityGaussian block LoD as *payload of* `SPLAT_SCENE` / quilt layers; never a second index. (Already listed as a session residual.)
2. **Planet-scale BVH shard by geohash/region** — still residual #10; steal VastGaussian visibility assign + CityGS min-view skip + LODGE two-chunk stream. E1 region hierarchy is the blocker.
3. **Quilt camera assign = visibility, not pose-in-AABB** — extend contribution ingest (Q1) with “does this frame see region R” (airspace-aware).
4. **Floater-robust `SPLAT_SCENE` AABB** — MAD/percentile before W0 publish; CityGS §3.3.
5. **L2 CityGo-style hybrid (pilot only)** — OSM building AABBs as `WM_SURFACE` / stubs; optional residual GS as non-metric `appearance_ref`. Depends on E2 OSM ingest.
6. **E5 appearance stream: LODGE chunk + opacity blend** — client; independent of BVH shard RPC.
7. **License gate** — world-builder checklist: no CC-BY-NC-SA CityGaussian code/weights in weftos; HuggingFace TeslaYang123 checkpoints are research-only.
8. **Do not** file “implement CityGS-X in clawft-bvh” or “planetary Gaussian train.”

**Out of scope for these tickets:** committing to `master`, adding MetaHarness as a crate dep, taking down Forge :3333/:3000.
