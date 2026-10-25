# urth-spatial-checks

Diagnosis skill for spatial results that are wrong without failing, adapted for
Urth (ADR-079). Used by the `world-builder` agent and anyone doing geospatial
ingest in WeftOS.

## Provenance

Adapted on 2026-10-25 from a `spatial-data-interrogator` skill the owner supplied
as a zip. The zip named no author, source or licence, so nothing was copied
verbatim. The method (look at the geometry before trusting its metadata, then
frame, row-count, validity, magnitude and ground-truth gates) is kept, and the
text was rewritten for Urth: ECEF/WGS84 identity, ENU region frames and
`T_ecef_local`, the L0–L5 LOD table, embeddings and raster registration, and Rust
`geo`/`proj` drift.

The original's projection reference was an empty template meant for its author
to fill. Urth's version, `references/urth-frames-and-rules.md`, is a draft for
owner review: rows marked *policy* are choices, the rest is checkable geodesy.

Registries were not searched for an alternative because the owner supplied this
skill; record a source and licence here if one turns up.

## Files

- `SKILL.md`: the method and the Urth-specific checks.
- `references/urth-frames-and-rules.md`: frames, projections, resampling, join
  refusal and anchor rules (draft).
- `references/library-drift.md`: loud API breaks in Python geo libraries and the
  Rust `geo` / `proj` crates.
