# OlmoEarth embeddings for Urth

**Date:** 2026-10-23. **Status:** research note, no code changed. Evaluated from primary pages fetched today; the v1.3 tech report PDF could not be read as text (see Risks).

## Summary and verdict

**Verdict: adapt, with a hard legal gate first.** OlmoEarth v1.3 gives a free, anonymous, global 10 m annual embedding layer (2023-2025) that fits Urth's cheap L0-L3 scaffolding and "find similar places" queries. Do not ship anything built on it until counsel reads the OlmoEarth Artifact License, because it bars military/defense uses (including surveillance, policing, intelligence) and extractive-industry uses, and requires those bans to pass downstream.

It is an observation-derived feature layer, so it honours metric-over-generative if labelled correctly. It does not replace OSM, DEM or splats; it augments them.

**Owner direction (2026-10-23):** we probably won't use the embeddings directly. The main value is the pretraining dataset as a recipe for building our own open-source Earth-observation set. See [As a recipe for our own dataset](#as-a-recipe-for-our-own-dataset).

## What it is

- Ai2's OlmoEarth is a family of multimodal, spatio-temporal Earth-observation foundation models. Sizes: Nano 1.4-1.7M, Tiny 6.2-12.5M, Small 35.6M, Base 89-114M, Large 308M encoder params (v1 to v1.2). https://github.com/allenai/olmoearth_pretrain
- v1.3 ships one model, Base. Model card: ViT-Base backbone 128M, Perceiver 14.3M, linear student 0.1M; backbone tokens are 768-d, projected to 128-d. https://huggingface.co/allenai/OlmoEarth-v1_3-Base
- Inputs: Sentinel-1, Sentinel-2 and Landsat, a year of time series, resampled to 10 m; pretraining used up to 12 monthly timesteps. https://huggingface.co/blog/allenai/olmoearth-v1-3 and https://allenai.org/blog/olmoearth-models
- Output: one 128-d vector per 10 m pixel per year; the first 64 dims work alone as a smaller embedding. Year-to-year change is a per-pixel vector comparison, no training. https://huggingface.co/blog/allenai/olmoearth-v1-3
- Pretraining set: 285,288 samples of 2.56 km x 2.56 km, 12.2 TB, CC BY 4.0 on the dataset card. Also includes OSM, SRTM, WorldCover, canopy height and CDL as supervision maps. https://huggingface.co/datasets/allenai/olmoearth_pretrain_dataset
- Patch size is not stated on any page I read. Not verified.
- Note the narrative page says Landsat 8; the blog says Landsat generally. https://narrative.allen.ai/olmoearth-embeddings

## Licence

Three artifacts, one licence family, one inconsistency.

| Artifact | Licence | Source |
|---|---|---|
| Code (olmoearth_pretrain) | OlmoEarth Artifact License | https://github.com/allenai/olmoearth_pretrain |
| Weights (v1.3 Base) | OlmoEarth Artifact License, "intended for use in accordance with" Ai2 Responsible Use Guidelines | https://huggingface.co/allenai/OlmoEarth-v1_3-Base |
| Precomputed embeddings | OlmoEarth Artifact License (LICENSE.txt in the bucket page) | https://source.coop/olmoearth/olmoearth-v1-3-embeddings |
| Pretraining dataset | CC BY 4.0 | https://huggingface.co/datasets/allenai/olmoearth_pretrain_dataset |

What the licence text says (read from https://raw.githubusercontent.com/allenai/olmoearth_pretrain/main/LICENSE via a summarising fetch, so quote-check the original before relying on it):

- Free grant to use, reproduce, modify, display, distribute; derivatives and synthetic data allowed.
- Commercial use is neither granted nor prohibited by name. It is not a non-commercial licence. Treat "commercial allowed" as probable, not confirmed.
- Prohibited: military and defense applications (weapons, operations, intelligence gathering, surveillance or policing); extractive activities (drilling, mining, deforestation planning).
- Redistribution: cite Ai2, link or include the agreement, and "include and require the use restrictions" in all downstream distribution.
- Terminates automatically on breach or on a patent suit against the artifacts.

Why this matters for WeftOS: the use restrictions are field-of-use and flow down to our customers. Any customer in defense, policing or extractive industries would be excluded. That is a product-wide constraint, not just a dependency note. Whether the embedding values (derived outputs) count as "Artifacts" is stated for the bucket, so assume yes.

## Access and runtime

- Precomputed product: yes. Global for 2023, 2024, 2025 (as of the 2026-10-22 release); 2017-2022 selected countries only, global backfill "planned over the coming weeks". Land, shorelines and persistent water, 59.5 S to 83.6 N, no Antarctica. https://source.coop/olmoearth/olmoearth-v1-3-embeddings
- Update cadence: annual by design; no stated schedule beyond the backfill. Not verified.
- Format: Zarr v3 (`embeddings.zarr`), one group per UTM zone utm01-utm60 in EPSG:326NN, array (time, band, y, x), shards 2048x2048 px x 128 bands x 1 year, inner chunks 256x256 x 64 bands, zstd. Provenance in GeoParquet, coverage mask GeoTIFF. Same source.
- dtype int8, signed square-root quantisation: `(|q|/127.5)^2 * sign(q)`; -128 is nodata. Same source.
- Access: public anonymous S3, `s3://olmoearth/olmoearth-v1-3-embeddings/embeddings.zarr`, endpoint `https://data.source.coop`, no key. Same source. Size about 150 TB per year (blog). Browser Explorer: https://olmoearth.allenai.org/embeddings
- On-demand alternative: OlmoEarth Studio exports Cloud-Optimized GeoTIFF at 10/20/40/80 m, int8 -127..127, from Nano (128-d), Tiny (192-d) or Base (768-d), for any polygon, 1-12 monthly periods. Studio is gated ("reach out"). https://allenai.org/blog/olmoearth-embeddings
- Self-compute: weights and code are public, installed with Python 3.12 and uv (`uv sync --locked`), embeddings via rslearn per https://github.com/allenai/rslearn/blob/master/docs/examples/OlmoEarthEmbeddings.md. Runtime is PyTorch. ONNX or other exports: not mentioned anywhere I read, and a search found none. Hardware needs are not stated; the final v1.3 pretraining cost about USD 2,720 of rented H100 time (blog), so training is out of scope but inference of a 140M model is modest. GPU requirement for inference: not verified.

## Fit with Urth

Grounding in the repo: ADR-079 defines L0-L5, sparse-first, honest `unobserved`, base map as a first-class ingest separate from splats (OSM, DEM, imagery), ENU/ECEF frames, and says generative fill is cosmetic and non-metric. ADR-056 is the BVH; ADR-078 is splat-to-world-model; ADR-095 and `docs/research/diskann-and-large-scale-indexes.md` say HNSW is primary for live use and DiskANN is the deferred cold tier, with Graph Views per region, never planet-wide. Relevant crates: `clawft-bvh`, `weftos-worldmodel*`. ADR-111 and `docs/plans/world-model-data-and-training.md` do not exist in this working tree, so I could not check them. Not verified.

**LOD fit**
- L0-L1 planet/country: weak. Pixel-level vectors are overkill; use coarse aggregates (below) or nothing.
- L2 city: good. Land-cover/change/similarity over a city at 10 m, complementing OSM footprints.
- L3 site/campus: useful as context (10 m pixels are 1 to 100 per building), not as detail.
- L4-L5: no. Splat quilts and object leaves own that; 10 m cannot see rooms.

**Sparse-first and fusion honesty.** The vectors are derived from real Sentinel and Landsat observations, so they are measured-then-encoded, not generated. Honest handling: store them as an `observed-derived` appearance feature with provenance (model id, year, scene provenance tables from the GeoParquet, licence tag), never as geometry. Nodata (-128) maps directly to `unobserved`. They carry no metric positional claim beyond 10 m pixel footprint. They must not feed splat training or geometry confidence. A caveat: the embedding is a learned abstraction, so a nearest-neighbour label is a prediction, and should be tagged as such.

**Landing in RVF/HNSW.** Do not ingest per pixel. Aggregate (mean in dequantised space, or GeM per arXiv 2603.02080) into cells, e.g. 100 m (10x10 px) or 1 km, one vector per cell attached to a region leaf, ids like `region/urth/<rank>/<cell>`. Storage per km^2 (my arithmetic, raw, uncompressed):

| Representation | Per km^2 per year | 100 km^2 city, 1 year |
|---|---|---|
| 10 m pixels, 128-d int8 | 1.28 MB | 128 MB |
| 10 m pixels, 64-d int8 | 0.64 MB | 64 MB |
| 10 m pixels, 128-d f32 | 5.1 MB | 512 MB |
| 100 m cells, 128-d int8 | 12.8 KB | 1.3 MB |
| 1 km cells, 128-d int8 | 128 B | 12.8 KB |

Cross-check: the vendor's 150 TB per year over about 149 M km^2 of land is about 1 MB/km^2, consistent with the 128-d int8 row. Keep pixel-level data in the remote Zarr and fetch windows on demand; only index aggregates. Multi-year change: store one vector per year per cell and a delta scalar. HNSW over cells is fine for a region; a continent of 100 m cells (about 10^9 vectors) is a DiskANN-tier job, not HNSW.

**Replace or augment basemap?** Augment. It does not give roads, footprints, elevation or named places, so it cannot replace OSM or the DEM. It can give land-cover/change layers, similarity search, and few-label mapping (the vendor's mangrove demo) for rural areas where OSM is thin. It is not a substitute for imagery tiles, which are display assets.

**Benchmarks (all vendor-reported).**
- v1.3 128-d ranks first on 20 of 27 evaluations, 64-d on 18 of 27, against AlphaEarth Foundations and Tessera v2. Ai2's own evals. https://huggingface.co/blog/allenai/olmoearth-v1-3
- v1 models "outperform" Prithvi, Galileo and Satlas; on kNN, on par or better than AlphaEarth, after fine-tuning "substantially" better; charts only, no numbers extracted. Ai2's own. https://allenai.org/blog/olmoearth-models
- Clay: no comparison found. Independent evaluation: arXiv 2603.02080 (Corley et al.) uses OlmoEarth, AlphaEarth and Tessera for a pooling study, but it is not a head-to-head ranking and I did not extract its numbers. https://arxiv.org/abs/2603.02080
- AlphaEarth's own licence: not checked here. Do not assume it is friendlier; it was not part of this review.

## As a recipe for our own dataset

Owner direction, 2026-10-23: we probably won't use the embeddings directly. The pretraining dataset is useful as a worked example of how to build Urth's first Earth-observation dataset from open sources. The details below come from the dataset card and `docs/Pretraining-Dataset.md` in `allenai/olmoearth_pretrain`, both read 2026-10-23.

**Sources, all open, with their licences as the card states them:**

| Layer | Source | Licence on the card | Resolution in the dataset |
|---|---|---|---|
| Optical | Sentinel-2 L2A, 12 bands (ESA) | Copernicus Sentinel Data and Service Legal Notice | 10, 20, 40 m |
| Radar | Sentinel-1 IW GRD VV+VH (via Microsoft Planetary Computer) | CC BY 4.0 | 10 m |
| Optical/thermal | Landsat 8/9 OLI-TIRS, 11 bands (NASA/USGS) | public domain | 10 m (B8), 20 m |
| Weather | ERA5 monthly, 6 variables (ECMWF) | CC BY | one 2560 m pixel |
| Elevation | SRTM (USGS) | public domain | 10 m |
| Land cover | ESA WorldCover 2021 | CC BY 4.0 | 10 m |
| Cropland | ESA WorldCereal 2021; USDA CDL (US, annual) | CC BY 4.0; public domain | 10 m |
| Canopy height | WRI / Meta canopy height maps | CC BY 4.0 | 10 m (from ~1 m) |
| Features | OpenStreetMap | **ODbL (share-alike)** | rasterized to 2.5 m |

**How they built it:**
- **Sampling is driven by OSM.** They picked about 120 OSM feature categories and enumerated every 2560 m UTM grid cell containing at least one feature of each category. Then they sampled up to 10,000 cells per category. That gave 285,288 samples. The category list is in `olmoearth_pretrain/dataset_creation/openstreetmap/osm_sampling.go`.
- **Grid.** Cells are 2560 m × 2560 m in local UTM, with corners on multiples of 2560 m, named like `EPSG:32610_248_-2176`. Resolutions are powers of two from 10 m.
- **Time.** Each sample covers a 360-day window, mosaicked into twelve 30-day timesteps for S1, S2, Landsat and ERA5. Static layers are attached when they intersect. In the US, 75% of windows are centred on NAIP image dates; elsewhere they are uniform over 2016–2024. Missing modalities or timesteps are left out rather than filled.
- **Layout.** One GeoTIFF per modality per sample, with timesteps on the band axis, plus per-modality CSVs of timestep windows. Files are shipped as 50 GB WebDataset tars. For training, they are re-cut into 1280 m sub-samples in H5 files. A 1,000-sample subset exists.

**What to take for Urth:**
1. **Use the same grid.** A 2560 m UTM cell with power-of-two pyramids maps cleanly onto Urth's site and city LODs and onto our BVH leaves. Matching it keeps their samples, or their 1,000-sample subset, comparable with ours.
2. **Sample by OSM feature categories.** This gives class balance without hand-labelling. It suits sparse-first densification: start with the categories our customers care about.
3. **Keep the licence stack per layer.** The dataset is CC BY 4.0 overall, but OSM-derived layers carry ODbL share-alike, and the Sentinel-2 notice needs attribution. Record a licence tag per layer in provenance, and keep ODbL-derived rasters out of anything we sell as a closed product.
4. **Don't fill gaps.** Their "leave out what is missing" matches fusion honesty: a missing timestep is `unobserved`, not interpolated.
5. **Consider seeding from their data.** Their dataset (CC BY 4.0, 12.2 TB), or the 1,000-sample subset, could seed our first world-model or Urth experiments. The per-source licences above still flow through. It is not covered by the field-of-use limits in the OlmoEarth Artifact License, because that licence covers the code, weights and embeddings, not this dataset. Confirm that reading before relying on it.

**Not stated on the pages read:** how cloud masking is done and how values are normalized. The dataset-creation guide (`docs/Dataset-Creation.md`) is the next thing to read.

## Risks and gaps

1. **Field-of-use licence** with flow-down. Biggest risk; may conflict with defense, policing or extractive-industry customers and with any "Urth is open to all" stance. Needs legal review before any shipping or hosted use. Mirroring the data also triggers the redistribution terms.
2. **Licence text read via a summary fetch**, not verbatim. I could not confirm the commercial-use wording; treat as unverified until the LICENSE file is read directly.
3. **Release is one day old** (2026-10-22); backfill of 2017-2022 is pending and the bucket layout may change. Tech report PDF (https://allenai.org/papers/olmoearth-v1-3) was binary and unread, so numbers, patch size and the quantisation caveats are unverified.
4. Optical gaps: cloudy and polar regions, and Antarctica is excluded. Nodata must propagate as `unobserved`.
5. 10 m resolution is irrelevant below L3; 10-20 km UTM-zone seams and antimeridian gaps in zones 1 and 60.
6. Embedding dimensions are not stable across model versions; vectors from v1.3 and Studio Base (768-d) are not comparable. Pin model id and quantisation in metadata.
7. No ONNX path means no Rust-native inference; self-compute needs a Python sidecar.
8. Brand: "OlmoEarth" attribution is required by licence; keep Urth naming separate.

## Proposed first spike

**Goal:** show that 100 m aggregated OlmoEarth vectors attach to Urth region leaves, honestly labelled, and answer a similarity query.

1. Legal gate (before any data leaves the laptop): obtain the verbatim LICENSE.txt, record the commercial and flow-down analysis in this file. Research reads of public data are allowed meanwhile.
2. Pick one 10 km x 10 km area (one city edge) and read its 2025 window from the Zarr store with a Python script outside the workspace, dequantise, aggregate to 100 m cells (10,000 vectors, 128-d).
3. Write the cells as `observed-derived` feature leaves with provenance and licence tags into a scratch RVF/HNSW index; nodata cells become `unobserved`.
4. Query: pick 3 labelled cells (water, urban, crop) and return top-20 neighbours.

**Done when:** (a) the legal note is signed off by the owner; (b) 10,000 cells load into HNSW with provenance, licence and year fields; (c) the three queries return neighbours whose OSM/WorldCover class matches in at least 80% of cases, with the measured figure recorded; (d) nodata cells are visibly `unobserved` and not zero vectors; (e) wall time and index size are recorded against the table above.

## Sources

- https://narrative.allen.ai/olmoearth-embeddings (start page: 10 m, 128-d, 1.3 trillion embeddings, 2016-2025 imagery claim, no licence terms)
- https://allenai.org/olmoearth
- https://huggingface.co/blog/allenai/olmoearth-v1-3
- https://allenai.org/blog/olmoearth-embeddings
- https://allenai.org/blog/olmoearth-models
- https://huggingface.co/allenai/OlmoEarth-v1_3-Base
- https://huggingface.co/datasets/allenai/olmoearth_pretrain_dataset
- https://github.com/allenai/olmoearth_pretrain/blob/main/docs/Pretraining-Dataset.md
- https://source.coop/olmoearth/olmoearth-v1-3-embeddings
- https://github.com/allenai/olmoearth_pretrain and its LICENSE
- https://allenai.org/papers/olmoearth-v1-3 (unread binary), arXiv 2511.13655 (v1 paper, via search only), https://arxiv.org/abs/2603.02080
- Repo: `docs/adr/adr-079-urth-digital-twin.md`, `docs/adr/adr-056-bvh-spatial-index.md`, `docs/adr/adr-078-splat-feeds-world-model.md`, `docs/handoff-urth-spatial.md`, `docs/research/uvrth-tessellation-scales.md`, `docs/research/diskann-and-large-scale-indexes.md`
