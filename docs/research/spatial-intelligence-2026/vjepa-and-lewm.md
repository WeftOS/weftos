# S6 — V-JEPA 2 / 2.1 vs WeftOS LeWM

**Date:** 2026-09-21  
**Status:** Session research capture (not an ADR)  
**Priority:** **B** — research / optional adapter  
**Parent:** [README.md](./README.md)  
**WeftOS canon:** [ADR-090](../../adr/adr-090-lewm-ecc-decoupling-invariant.md), [ADR-056](../../adr/adr-056-bvh-spatial-index.md), [ADR-078](../../adr/adr-078-splat-feeds-world-model.md), [ADR-088](../../adr/adr-088-bvh-leaf-vector-ref.md), [ADR-093](../../adr/adr-093-bvh-hnsw-phase-f-join.md)

**One-line:** Meta’s latent predictive video world model **confirms** ADR-090 — prediction lives *under* ECC, not instead of it. Dense tokens may fill `index_ids::VISUAL_FEATURES`. They never become BVH leaves.

---

## 1. What shipped (2025–2026)

### 1.1 V-JEPA 2 (2025-06)

Meta FAIR released **Video Joint Embedding Predictive Architecture 2** on 11 June 2025 (code 25 June). It is a **latent** world model: an encoder maps raw video to embeddings; a predictor maps those embeddings (plus optional action context) to *future embeddings*. It does **not** reconstruct pixels.

| Fact | Source |
|------|--------|
| 1.2B-parameter family; ViT-L/H/g checkpoints (300M / 600M / ~1B) | [Meta blog](https://ai.meta.com/blog/v-jepa-2-world-model-benchmarks/), [GitHub](https://github.com/facebookresearch/vjepa2) |
| Action-free pre-train on **>1 million hours** of internet video + ~1 million images | [arXiv:2506.09985](https://arxiv.org/abs/2506.09985) |
| Two-stage: actionless JEPA pre-train, then **V-JEPA 2-AC** action-conditioned post-train | same |
| **V-JEPA 2-AC:** 300M-parameter transformer, block-causal attention; **<62 hours** unlabeled DROID robot video | paper §3.1 |
| Zero-shot Franka arms in **two labs**; image-goal MPC (CEM); **no** env-specific data, task reward, or calibration | paper abstract |
| Pick-and-place with visual subgoals: **65–80%** success on novel objects / rooms | [blog](https://ai.meta.com/blog/v-jepa-2-world-model-benchmarks/) |
| Planning ~**16 s/step** vs ~4 min for a Cosmos-class latent-diffusion baseline (MarkTechPost report of Meta numbers) | [MarkTechPost 2025-06-12](https://www.marktechpost.com/2025/06/12/meta-ai-releases-v-jepa-2-open-source-self-supervised-world-models-for-understanding-prediction-and-planning/) |

Reported frozen-probe / aligned-LLM numbers (paper + README):

| Benchmark | V-JEPA 2 | Note |
|-----------|----------|------|
| Something-Something v2 (probe) | 77.3 top-1 | motion understanding |
| Epic-Kitchens-100 anticipation | 39.7 recall-at-5 | +44% relative vs prior SOTA |
| Diving48 (probe) | 90.2 | |
| PerceptionTest (8B LLM-aligned) | 84.0 | video QA |
| TempCompass (8B) | 76.9 | video QA |

Meta also shipped three **physical-reasoning** evals that still show a large human gap (humans ~85–95%): [IntPhys 2](https://arxiv.org/abs/2506.09849), [MVPBench](https://arxiv.org/abs/2506.09987), [CausalVQA](https://arxiv.org/abs/2506.09943). Those are useful as **honesty tests** for any LeWM claim, not as Urth geometry metrics.

Code + checkpoints: [facebookresearch/vjepa2](https://github.com/facebookresearch/vjepa2) (majority **MIT**; a few dataset utils Apache-2.0). Paper: CC BY 4.0. Hub: [HF collection](https://huggingface.co/collections/facebook/v-jepa-2-6841bad8413014e185b497a6). Site: [ai.meta.com/vjepa](https://ai.meta.com/vjepa/).

### 1.2 V-JEPA 2.1 (2026-03)

Repo stamp **2026-03-16**; paper [arXiv:2603.14482](https://arxiv.org/abs/2603.14482) submitted 15 Mar 2026 (v3 11 Jun 2026). Title: *Unlocking Dense Features in Video Self-Supervised Learning*.

The 2.0 encoder was strong at **global** motion and anticipation; dense (patch-level) maps were grainy. 2.1 changes the **recipe**, not the JEPA thesis:

1. **Dense predictive loss** — visible *and* masked tokens contribute, so the encoder cannot skip spatial/temporal grounding.
2. **Deep self-supervision** — the same objective at multiple intermediate encoder layers.
3. **Multi-modal tokenizers** — unified image + video training.
4. **Scale** — ViT-B/L/g/G at 384 px (80M → 2B params).

Reported dense / world-model lifts:

| Task | V-JEPA 2.1 |
|------|------------|
| Ego4D short-term object-interaction anticipation | 7.71 mAP |
| EPIC-KITCHENS high-level action anticipation | 40.8 Recall@5 |
| Real-robot grasp vs V-JEPA 2-AC | **+20 points** success (zero-shot Franka, new rooms) |
| TartanDrive navigation | 5.687 ATE |
| NYUv2 depth (linear probe) | 0.307 RMSE |
| Something-Something v2 | 77.7 |

**Why 2.1 matters for WeftOS more than 2.0:** temporally consistent **dense** tokens are the first JEPA artifact that looks like a visual `VectorRef` payload — per-patch / per-object features you can index, not a single scene embedding.

Paper license on arXiv is **CC BY-NC-ND 4.0**. Weights in the same MIT repo. Product use of *text* vs *weights* is therefore **not** the same license; cite both.

---

## 2. What it is **not**

| Not | Why |
|-----|-----|
| Not a **metric twin** | Latent tokens have no ECEF/ENU pose, no AABB, no chain_seq. |
| Not a **BVH** | No containment / ray / frustum. ADR-056 already forbids treating a similarity index as geometry. |
| Not **pixel-video “worlds”** (Genie / Cosmos class) | JEPA predicts in **embedding space**. Still not survey truth. |
| Not ECC reasoning | A predictor that “imagines” a grasp does not mint a `WM_OBJECT` or a causal edge. |
| Not a planetary train | 1M hours of internet video is **not** Urth L0–L2. Do not propose a globe-scale V-JEPA. |
| Not a replacement for COLMAP / VGGT / W1 partition | Depth linear-probe on NYUv2 ≠ geometric partition of a quilt. |

Li’s taxonomy (S1): V-JEPA is a **planner / simulator in latent space**, not a **renderer**, and not Urth’s **structure SoT**.

---

## 3. Exact WeftOS seam

### 3.1 LeWM is the counterpart (optional sub-layer)

| | V-JEPA 2 / 2.1 | WeftOS **LeWM** |
|--|----------------|-----------------|
| Job | Latent predict + (AC) action-conditioned rollouts | Learned perceptual / predictive substrate over sensors |
| Host | Python FAIR stack; HuggingFace / torch.hub | `weftos-worldmodel-*` + `clawft-worldmodel-service` (not yet a runtime dep) |
| Authority | Robot MPC scores imagined embeddings | **ECC remains authority** — ADR-090 **R1–R5** |
| Write path | Plans stay in the robot loop | Impulse / Observation only (`WmWriteKind`); no `CausalEdgeMutate`, no `ReasoningOverride` |
| Absent | N/A (the model *is* the product) | **R2 / R5:** sensor + ECC paths run with WM missing |

ADR-090 is the load-bearing invariant. V-JEPA is **evidence that the industry agrees** latent WMs are useful *and* that they sit beside, not above, an explicit controller. In WeftOS the “controller” is ECC + BVH + chain, not CEM on a Franka.

Do **not** evolve LeWM policies that violate R1–R5. A V-JEPA adapter that published `ReasoningOverride` or skipped causal-edge evaluation is a **facade reject**, not a research success.

### 3.2 Dense features → visual HNSW `index_id`

Reserved today in `weftos-leaf-types`:

```text
index_ids::ECC_HNSW         = 0   // default kernel HNSW
index_ids::VISUAL_FEATURES  = 1   // DINO-style; V-JEPA 2.1 is a candidate embedder
index_ids::LANGUAGE_FEATURES = 2  // CLIP / SigLIP
```

(`crates/weftos-leaf-types/src/spatial/vector_ref.rs`, ADR-088.)

Proposed compose (not filed):

1. W1 geometric partition still emits `WM_*` with **`vector: None`** (WEFT-709 default).
2. Optional producer: freeze V-JEPA 2.1, pool dense tokens over a leaf AABB / mask → insert into a **visual** HNSW namespace (`index_id = 1`).
3. Write `VectorRef { index_id: VISUAL_FEATURES, vector_id }` on the payload.
4. Graph Views **F4** attach those as **soft edges** (`k`, `min_score`). **F9 promote** still requires geometry + chain evidence.
5. ADR-093 helpers already join spatial-first or feature-first. No second similarity index inside the BVH.

Pool **after** a metric AABB exists. Do not invert the order (embed first, invent a box later).

### 3.3 Crate / leaf map

| Seam | Role for V-JEPA |
|------|-----------------|
| `clawft-kernel::lewm_invariant` | Every WM → kernel write hits `check_wm_write` |
| `clawft-bvh` / `SpatialBackend` | Unchanged. Geometry SoT. |
| `weftos-leaf-types::spatial::VectorRef` | Optional handle; visual namespace |
| `clawft-bvh::vector_join` | Resolve `index_id = 1` after a spatial query |
| `WM_OBJECT` / `WM_SURFACE` / `WM_VOLUME` | Stay AABB + tag + payload. Features are *on* them, not *instead of* them. |
| Graph Views F4 / F8 / F9 | Soft visual edges; optional dual-branch score; promote to BVH |
| splat pipeline W1 export | `bvh_published: false` today — visual index is **not** a substitute for live publish |

### 3.4 Occupancy / OccWorld parallel

rUv OccWorld (see [ruv-worldgraph-vs-weftos.md](../ruv-worldgraph-vs-weftos.md) §3) predicts **future occupancy voxels**. V-JEPA 2-AC predicts **future visual latents** given actions. Both are **priors**. Neither is the twin. A LeWM impulse may *suggest* a `WM_VOLUME` candidate; F9 + chain still mint the leaf.

---

## 4. Apply / compose / watch

| Stance | What |
|--------|------|
| **Watch (now)** | 2.1 dense-feature quality, license of weights vs paper (code and checkpoint MIT; paper CC BY-NC-ND), GPU cost of ViT-g/G @384 only (B tier is cheap), IntPhys-2 honesty gap. |
| **Compose (when a producer exists)** | Frozen 2.1 encoder as one **visual embedder** behind `VISUAL_FEATURES`. Same join as DINO. Start with the distilled **ViT-B/16, 80M parameters, 384 px** checkpoint (`vjepa2_1_vitb_dist_vitG_384.pt`) from the MIT-licensed [facebookresearch/vjepa2](https://github.com/facebookresearch/vjepa2) repo (V-JEPA 2.1 announced 2026-03-16; licence confirmed MIT in the 2026-10 watch review, `../watch-review-2026-10.md` A5). It runs on the CUDA host or Apple Silicon, so the giant-model GPU cost concern does not apply to this tier. |
| **Optional LeWM adapter (later)** | V-JEPA 2-AC-style action-conditioned predictor as a **rollout prior** for robot / capture-head planning. Facade-enforced. |
| **Do not apply** | “Replace BVH with JEPA tokens.” “Train V-JEPA on the planet.” “Treat 65–80% grasp as Urth occupancy.” |

**What "producer" means here.** A producer is a running pipeline that emits metric-anchored image regions: a camera (or splat render) source that yields frames, plus a step that has already produced a `WM_*` leaf with an AABB (W1 geometric partition, or a camera cog's detections) to pool tokens over. Today no such pipeline exists: W1 export is `bvh_published: false` and the 12x12 ft room plan (`docs/plans/world-model-data-and-training.md`) has no camera cog yet. The trigger is the first camera cog or splat-capture head that publishes frames with poses; until then the 80M checkpoint stays unloaded and no ticket is warranted.

Pilot, if ever: **one L4 quilt site**, freeze the encoder, attach `VectorRef` on a handful of `WM_OBJECT` leaves, query spatial-first then visual-rank. Measure whether F4 soft edges reduce identity mistakes vs CLIP/DINO — MetaHarness receipt, not a silent champion swap (ADR-096).

---

## 5. Risks

| Risk | Mitigation |
|------|------------|
| **Generative / latent as truth** | Label every JEPA rollout **non-metric**. Unobserved Urth stays `unobserved`. |
| **GPU / VRAM** | ViT-g 384 and ViT-G 2B are not Mac-laptop defaults. Edge path = smaller ViT-B/L or skip. |
| **License split** | Code MIT; 2.1 **paper** CC BY-NC-ND. Confirm weight TOS before any product embedder. OSM-style provenance on the `index_id` producer. |
| **Closed control loop** | V-JEPA 2-AC planning is a **lab demo**, not WeftOS daemon RPC. |
| **Shortcut physics** | IntPhys 2 / CausalVQA: models near chance on violation-of-expectation. Do not advertise “intuitive physics” in Urth copy. |
| **Schema temptation** | Inlining 1024-d tokens into BVH payloads was already rejected (ADR-088 Option 3). |
| **Mac `decord`** | Upstream notes no official macOS `decord`. Research jobs stay Linux/CUDA or a documented fork. |

---

## 6. Mapping table (keep this)

| V-JEPA artifact | WeftOS home | Must not become |
|-----------------|-------------|-----------------|
| Encoder tokens (esp. 2.1 dense) | HNSW `VISUAL_FEATURES` + `VectorRef` | BVH node payload / AABB |
| Predictor rollouts | LeWM Observation / Impulse | Causal edge, `WM_*` mint |
| V-JEPA 2-AC CEM plan | Optional robot/capture prior | SpatialService query result |
| Image goal / subgoal | ViewSpec / agent task, not geometry | Region id `urth/…` |
| Physical-reasoning benches | Honesty eval for LeWM claims | Urth LOD metric |

**Doctrine restated:** sparse-first metric geometry (BVH + chain) is the twin. Latent prediction is an **optional sub-layer**. ECC is authority. Dense features may become a visual `index_id`. **Never replace BVH.**
)
