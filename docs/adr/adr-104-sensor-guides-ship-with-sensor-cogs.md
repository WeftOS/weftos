# ADR-104: Sensor guides ship with sensor cogs

- **Status**: Accepted (2026-10-01, owner: "make this a thing we provide with all of our sensor cogs, and maybe other places too")
- **Successor**: [COG-104](https://github.com/WeftOS/weftos-cogs/blob/v0.1.2/docs/decisions/COG-104-sensor-guides.md) in WeftOS/weftos-cogs (Cogs extraction, 2026-10-27). Cogs owns guide packaging, the guide format and renderer, companion integration and per-cog content. The Cogs product crates it names now live in that repository without the `weftos-` prefix. This record stays as written.
- **Date**: 2026-10-01
- **Updated**: 2026-10-01. A1: companion apps share one shell, `weftos-cog-companion` (decision 5); second sensor, `sen0628-tof` and `weft-tof-scope`; `grid` diagram added (decision 1).
- **Deciders**: Owner / platform
- **Depends-On**: ADR-099 (governed workload placement), ADR-100 (cog workload kind)
- **First implementation**: `crates/weftos-sensor-guide` (format, validation, egui renderer, `guide-check`); `crates/weftos-ecg-scope` (first companion app); the `sen0213-ecg` cog in the cogs repo (first guide, served at `/guide`)

## Context

A sensor cog is only useful once someone has wired the sensor correctly, placed it well, enabled the bus, installed the cog, and checked the signal. That knowledge used to live in READMEs, which are not available where the work happens: at the bench, next to a live signal. The first companion app, `weft-ecg-scope`, showed that the hook-up checklist, the live signal and the explanation of each step belong together. The owner wants that for every sensor cog, and the content reusable elsewhere (docs sites, the Cognitum store page).

## Decision

1. **Every sensor cog ships a guide** in `src/cogs/<id>/guide/`:
   - `guide.toml` (schema 1) holds the page order, `[links]` from checklist step ids to pages, and the data the diagrams are drawn from:
     - `[header]`: header pins used, with colours and destinations, plus pins to avoid and why;
     - `[[parts]]` and `[[wires]]`: endpoints as `part.pin`;
     - `[[placements]]`: normalised pad positions on a front-view body;
     - `[[flow]]`: the signal chain;
     - `[grid]` (A1): the zone map for matrix sensors, with orientation labels.
   - One CommonMark file per page. The first `# ` line is the title and the first `> ` line the summary. A fenced block with info string `diagram` whose body is `header`, `wiring`, `placements` or `flow` embeds that diagram (`grid` added in A1).
2. **The cog serves its guide** at `GET /guide` on its export port as `{"toml": "...", "pages": {id: markdown}}`, compiled in with `include_str!`. A client therefore always sees the guide that matches the installed cog version. The cog's unit tests check that the embedded page list matches `guide.toml`.
3. **`weftos-sensor-guide` is the one renderer.** It parses and validates a bundle and renders it in egui, natively and in WASM. It provides searchable page navigation, Markdown through `egui_commonmark`, and the four diagrams painted from data. `validate()` rejects:
   - links to missing pages;
   - wire endpoints that aren't declared part pins;
   - header pins outside 1-40, or listed as both used and to avoid;
   - pads outside 0..1;
   - diagram fences with no data behind them.

   `guide-check <dir>...` runs the same validation in a cog's gate.
4. **Companion apps link checklist steps to pages** through `[links]`, with a "?" next to each step. The app owns the live checks; the guide owns the explanation.
5. **Companion apps share one shell (A1, `crates/weftos-cog-companion`).** A sensor app implements `SensorApp`: its live view, its checklist steps, its calibration panel and any extra export polling. The shell provides:
   - the connection bar;
   - start, stop and test-run;
   - the four connection steps;
   - the cog settings, generated from the cog's manifest;
   - the Guide tab;
   - native and WASM entry points.

   Apps are `crates/weftos-<name>-scope`, built with `scripts/build.sh scope <name>` and `scope-web <name>`.
6. **Authoring rules:**
   - Facts must match the cog source and its ADR; mark anything unverified.
   - Trust pin labels over wire colours.
   - Keep tables to about three short columns, and use sections for long fixes.
   - The renderer maps arrows, `>=`/`<=` signs and ticks to ASCII, because egui's default fonts lack them, so either form is fine in the source.
   - Medical-adjacent sensors set `medical = false` and carry a safety page.

## Consequences

- A new sensor cog gets its companion app's Guide tab, diagrams and validation for the cost of writing `guide.toml` and Markdown. No new rendering code is needed.
- The Markdown also renders on GitHub and static docs sites. A future HTML export (for the Cognitum store page or a docs site) can reuse `guide.toml` for the diagrams.
- A guide adds a few KB to the cog binary (the `sen0213-ecg` guide is 10 pages).
- The format is versioned (`schema`). Renderers reject an unknown schema rather than guess.

## Alternatives considered

- **Guide content compiled into each companion app:** this duplicates content per app, drifts from the cog version, and can't be read by other clients.
- **Static HTML pages:** they don't render in egui and they lose the live checklist link.
- **Diagrams as images:** they can't be validated against the wiring data, are heavier, and need image loaders in the WASM build (WEFT-577 keeps those out).
