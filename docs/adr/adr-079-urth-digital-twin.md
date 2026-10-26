# ADR-079: Urth — multi-scale digital twin, sparse-first (Snow Crash north star)

**Date**: 2026-07-30  
**Status**: Accepted (vision / direction); R1/R2 first live region published (2026-10-25)  
**Updated**: 2026-10-25  
**Deciders**: product + spatial (owner: WeftOS world is not only rooms — we build toward a shared, navigable **Urth** that densifies where sensors and people care)  
**Depends-On**: ADR-056 (BVH), ADR-078 (structure from capture), free-form quilt, multi-cam  
**Relates-To**: ADR-077 (edge capture), LeWM latent world models (compose later), mesh (ADR-026), **ADR-095** / **Graph Views** (multi-scale association is purpose-scoped fusion Views per region/LOD — not one planet-wide unbounded graph; DiskANN for embedding volume)

## Naming

**Urth** is the product name for WeftOS’s sparse-first planetary digital twin.

> **Rename candidate (2026-09-26, not decided):** stylized mark **uvRTH** = **Region Tessellated Holograph**, also a letter-play on **rUv + Earth** (credit rUv’s naming scheme). See `docs/research/uvrth-name-candidate.md`. This ADR’s name stays **Urth** until product agrees.

- Deliberately **not** “Earth” / “Google Earth” as a product label (trademark / confusion hygiene).  
- Physical planet still uses **WGS84 / ECEF** geodesy under the hood — Urth is the **software world**, not a second planet.  
- Prefer `urth`, `region/urth/…` in IDs and UI copy.

## Context

Local splat capture, free-form quilts, and BVH objects give **high-detail islands**. The product north star is larger:

> **Urth** — a continuous shared digital twin of the planet, starting with almost no data, filled by **local observations**, **open geospatial feeds**, and multi-agent contribution over time.

Inspired by **Snow Crash** (and related continuous-world fiction): not a single 3D file of the globe, but a **live, multi-resolution world** where:

- most of the planet is **cheap base layers** (terrain, roads, imagery)  
- **places people capture** become dense (splats, objects, volumes)  
- the same query language works at room and city scale  

We already have the seeds: free-form **regions**, camera-stats contributions, BVH leaves, chain audit. We lack the **scale hierarchy**, **base-map ingest**, and **world-building doctrine**.

## Decision

### 1. Urth is a multi-scale region graph, not one mesh

| LOD | Content | Source |
|-----|---------|--------|
| **L0 Planetary** | Ellipsoid, continents, oceans | Open elevation / coastline |
| **L1 Country / state** | Admin boundaries, major transport | OSM, government open data |
| **L2 City** | Streets, buildings footprints | OSM, Microsoft Building Footprints, etc. |
| **L3 Site / campus** | Detailed basemap + sparse captures | Local + OSM |
| **L4 Room / zone** | Free-form quilt + WM_OBJECT | Capture pipeline (ADR-078) |
| **L5 Object** | Instance AABBs, affordances | Structure stage + human/agent |

BVH (or sharded BVHs) indexes leaves at each LOD; coarse leaves **contain** finer ones (parent region IDs). Root region id: `urth` (or `region/urth`).

Spoken tessellation ladder (nano → micro → mini → meso → normal → macro → major → super → inf) maps onto L0–L5; see `docs/research/uvrth-tessellation-scales.md`. Not a wire-format change.

### 2. Sparse-first is the only viable start

At t=0 Urth has **tiny** high-detail area (your captures) + **global low-detail scaffolding** from open feeds. Empty space is honest:

- `unknown` / `unobserved` volumes, not fake generative terrain as truth  
- Generative fill is **optional cosmetic**, always labeled non-metric  

### 3. Free-form quilt is the L4–L5 densification engine

Every phone/Pi/multi-cam contribution with known camera stats **improves a region** of Urth. Seams OK; multi-evidence Objects improve over time ([splat-freeform-quilt.md](../weftos/splat-freeform-quilt.md)).

### 4. Base map is a first-class ingest, separate from splats

Open feeds bootstrap L0–L3 without any Gaussian train:

| Feed class | Examples | Leaf kinds |
|------------|----------|------------|
| Terrain | SRTM, Copernicus DEM | WM_SURFACE / volume shells |
| Vector map | OpenStreetMap | roads, buildings as AABBs/polylines → leaves |
| Imagery | Map tiles, Sentinel (where licensed) | appearance basemap, not GS |
| Places | Overpass POIs | WM_OBJECT stubs (low confidence) |
| Weather / traffic | optional live layers | Event leaves |

### 5. Coordinate frames

| Frame | Use |
|-------|-----|
| **ECEF / WGS84** | Global identity of regions (planet reference) |
| **ENU local** | Capture sessions, quilts, indoor BVH |
| **Transform** | `T_ecef_local` stored on region |

Indoor captures without GPS still attach to a **site** region with surveyed or approximate geo anchor.

### 6. Multi-agent / multi-node contribution

Any WeftOS edge (phone, Pi head, fixed cams, future vehicles) is a **writer** of contributions under governance:

- capability tokens (who may write which region)  
- chain audit of structure updates  
- conflict = branch or confidence contest, not silent overwrite  

Snow Crash vibe: **shared continuous Urth**, local clients show densified LOD where data exists.

### 7. Expert ownership: world-builder

A dedicated **world-builder** agent/expert owns:

- LOD policy and region taxonomy  
- open-data source selection & licensing  
- fusion rules (basemap vs quilt vs generative)  
- “what to capture next” for Urth densification  
- coordination with reconstruction curator (`~/llm` reconstruction registry)  

Definition: `.grok/agents/world-builder.md` (and Claude-compatible twin as needed).

### 8. Non-goals (now)

- Photoreal whole-planet Gaussian splat  
- Competing with third-party “Earth” products as a basemap host at planetary tile scale in v1  
- Treating generative cities as survey truth  
- Single monolithic BVH of the planet in one process (shard by region)  
- Product naming that collides with major map brands  

## Consequences

### Positive

- Distinct, memorable product name (**Urth**)  
- Local capture work **composes** into a global story  
- Sparse-first avoids waiting for impossible full coverage  
- Open data accelerates L0–L3 without sensors  
- Clear expert for multi-year world-building decisions  

### Negative / costs

- Geo stack complexity (projections, licensing, tile ops)  
- Shard/sync of spatial indices  
- Temptation to fake density with generative content  

## Implementation phases

| Phase | Plane | Work |
|-------|-------|------|
| **E0** | **WEFT-713** | This ADR + urth-digital-twin.md + world-builder agent |
| **E1** | *(later)* | Region hierarchy: urth → site → room IDs + ECEF↔ENU. **Slice landed 2026-10-25 (R1/R2):** one room-level region leaf (`region/urth/meso/test-room`, a tape-measured `shell_measure`, proof `MEASURED`) publishes through `ecc.spatial.region.publish`, is fetched by region id with `ecc.spatial.region.get`, and answers `ecc.spatial.query` (`with_payload`) with its provenance. Still open: the site anchor / `T_ecef_local` (the leaf says `unanchored`), the urth → site hierarchy, and chain checkpointing between clean shutdowns. **Restart (2026-10-26):** at boot the kernel rebuilds the BVH from the `spatial_bvh` events on the loaded chain (`SpatialService::restore_from_chain`, before the chain sink attaches so nothing is appended twice); the region comes back by id with its provenance (`published_region_survives_daemon_restart`). The chain is written at clean shutdown, so a crash loses events since the last checkpoint. Test: `clawft-weave/tests/spatial_cli_e2e.rs`. |
| **E2** | *(later)* | OSM/DEM basemap ingest → coarse WM_* leaves for one pilot city/site |
| **E3** | *(later)* | Attach free-form quilts under site regions |
| **E4** | *(later)* | Multi-node contribution + governance |
| **E5** | *(later)* | Client LOD streaming (coarse globe → dense splat rooms) |

## References

- `docs/weftos/urth-digital-twin.md`  
- `docs/weftos/splat-freeform-quilt.md`  
- `docs/weftos/splat-to-world-model.md`  
- ADR-056, ADR-078, ADR-077  
- Snow Crash (fiction north star — continuous shared space, not a product spec)  
