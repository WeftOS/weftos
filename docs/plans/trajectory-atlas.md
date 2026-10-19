# Trajectory Atlas — corpus-scale agent-run visualization

**Date**: 2026-10-18
**Status**: Plan (no code yet). Needs an ADR before T2 is claimed (see "Decisions").
**Source**: Ian Johnson (@enjalot), posts of 2026-10-18 —
[overview video](https://x.com/enjalot/status/2111813743302582519),
[classifier disagreement](https://x.com/enjalot/status/2111813744942588064),
[single-trajectory timeline](https://x.com/enjalot/status/2111813748138873092).
We use the ideas only. No code was published, and none is copied.

## What the posts show

Two linked views over many agent transcripts. His corpus is Claude Code sessions,
the same shape as ours (Bash, TaskOutput, `mcp__claude-in-chrome__*` rows).

**1. Corpus view (thousands of trajectories)**

| Technique | What it does |
|---|---|
| Grouped treemap | One cell per trajectory, grouped by a metadata or computed column (e.g. `fulfilled 7,979 · partial 1,006 · failed 623 · no_response 392 · clarifying 249`). |
| Color = classification | Color by outcome, or by a *second* classifier, so disagreement shows as off-color dots inside a group. |
| Size = length | Dot area tracks turn count. |
| Zoom to "beads on a string" | Smooth zoom turns a dot into a spiral or path of its turns, colored per turn. |
| 3-D "forest" | The same layout tilted into 3-D: each trajectory is a vertical stalk of turns, so long runs stand up out of the field. |
| Semantic layout | Animated transition from treemap to a UMAP projection of trajectory embeddings. |
| Color by cluster | Semantic clusters as a color column. |
| Cheap computed columns | Run an LLM classifier over every trajectory to add a column, then compare two classifiers visually instead of reading one "% agreement" number. |

**2. Single-trajectory view (5k+ messages, instant scroll)**

- A vertical timeline. On the left is a canvas minimap of the whole run with lanes for role, kind,
  errors, agents and events. On the right is a legend with per-category counts, used as filters.
- "Lenses" choose what expands: outline only, conversation, tool calls, everything,
  failures in context, labeled evidence, report evidence.
- Highlight-and-step: for example "30 highlighted · Recorded errors: Error", with Prev and Next buttons.
- Hover previews. Full bodies load only when brought into focus.

**Performance tricks he lists**

- Virtualized transcript: only a few screenfuls become elements.
- Structure before detail: wireframes and previews first, then bodies on focus.
- Separate access paths for catalog, headers, bodies and search. Opening one run never loads the corpus.
- Workers do filtering, Parquet reads and search, off the UI thread.
- A canvas minimap instead of thousands of DOM nodes.
- Calibrated row-height estimates plus a small scroll buffer.
- Shared in-flight body reads, and a body cache of about 32 MiB.
- Small files (under 24 MiB) fetched whole; larger files read selectively with range reads.
- Search skips irrelevant shards and frees decoded data as it goes.

## Why it fits WeftOS

We already have most of the parts. What's missing is the corpus-scale view.

| Need | What exists | Gap |
|---|---|---|
| Trajectory data | Claude Code sessions in `~/.claude/projects` (69 projects, 4,585 `.jsonl` files, 8.5 GB); weft conversation turns anchored on the ExoChain (`ChainEvent`, `chain_seq` + `uid`, ADR-069); `voice.trace` (`clawft-weave/src/voice_trace.rs`); learner `Trajectory` (`clawft-core/src/pipeline/traits.rs`); Codex/Grok session readers (ruflo cost tracker) | One normalized catalog across sources |
| Identity / reverse-resolve | ADR-069 Panopticon: every projection keyed by `chain_seq` or `uid` | The atlas must obey D2 for weft-native runs |
| Embeddings | e5 embedder (`clawft-kernel/src/embedding_e5.rs`), `SessionIndexer` (`clawft-core/src/session_indexer.rs`), RVF/HNSW store | 2-D/3-D projection (UMAP or similar) |
| Clustering | `clawft-graphify` label propagation + SASE (`cluster.rs`, `cluster_sase.rs`) | Run it over trajectory embeddings, not only code graphs |
| Classifiers | Local LLM lab (`~/llm`, GLM-4.7-Flash), AI Gateway CoS, metaharness scoring | A "computed column" job + store |
| Rendering | egui GUI (`clawft-gui-egui`, egui 0.34, glow, WASM target via VS Code panel); custom painter technique; ADR-067 conversation graph (G1–G5 scaffolded, all phases still `Scaffolded`) | Treemap layout, instanced dot rendering at 10k+ items, 3-D tilt, virtualized long transcript |

ADR-067 (Proposed, 2026-07-04) handles **one** conversation as a small causal
graph, and states it needs no layout engine because conversations are tens to
hundreds of nodes. The atlas is the other end: thousands of runs, each up to
thousands of messages. It complements ADR-067 rather than replacing it. Its
single-run timeline becomes the "zoom in" target, and ADR-067's causal graph stays
the view for causal structure within a conversation.

## Architecture

```
 sources ──► ingest (backend) ──► catalog shards (Parquet / Arrow IPC) ──► clients
  Claude Code jsonl      normalize to RunRecord + MessageRecord        egui atlas (desktop + wasm)
  weft chain turns       computed columns: outcome, length,            clawft-ui route (optional)
  voice.trace            classifier labels, cluster id, embedding,
  Codex / Grok           2-D + 3-D projection coords
```

Backend-heavy, thin client. This matches the CoS direction (thin client, services do
the work). The client never parses raw `.jsonl`. It reads three access paths:

1. **Catalog**: one row per run (id, source, project, start/end, turn count, outcome,
   label columns, cluster, x/y/z per layout). Small enough to load whole (10k runs
   at roughly 200 bytes is about 2 MB).
2. **Headers**: per run, one compact row per message (role, kind, tool, error flag,
   agent, byte offset and length, estimated height). This feeds the minimap and the
   virtualized list.
3. **Bodies**: fetched by offset and length on focus, through an LRU of about 32 MiB with
   de-duplicated in-flight reads. A run file under 24 MiB is fetched once; larger
   files use range reads.

Search is a separate sharded index: the existing HNSW for semantic search, plus a
per-shard term filter so irrelevant shards are skipped.

Privacy: transcripts contain secrets and client data. Ingest runs
the existing credential redaction before writing shards. Shards stay local, or on
PG for the mesh, and never go into the public repo. Client-confidential projects are
excluded by default (`~/Clients/**` opt-in only).

## Phases

Each phase is one board card. Each has an observable done condition.

| # | Card | Done when |
|---|---|---|
| T1 | **Run catalog ingest** (`weftos-trajectory` crate, backend): normalize Claude Code `.jsonl` and weft chain turns into `RunRecord`/`MessageRecord`; write catalog + header + body shards; redaction pass; `weaver atlas ingest` | 4,585 local sessions ingest; catalog row count matches file count; a fixture with seeded fake keys shows them redacted; re-ingest is incremental |
| T2 | **Computed columns**: outcome classifier (fulfilled/partial/failed/no_response/clarifying) on the local model; length; error count; tool mix; second-classifier slot | Columns are stored per run with model id + prompt hash; agreement matrix between two classifiers printed by `weaver atlas compare` |
| T3 | **Embed + project + cluster**: e5 run embeddings, 2-D and 3-D projection, graphify clustering → cluster column | Coordinates stored; same input → same layout (seeded); cluster labels auto-named |
| T4 | **egui corpus view**: squarified treemap grouped by any column, instanced dots (size = length, color = any column), pan/zoom, legend filters | 10k runs at 60 fps on the MacBook in the native build, and at ≥30 fps in the wasm build; frame-time numbers recorded |
| T5 | **Layout transitions**: treemap ↔ UMAP animation; 3-D tilt "forest" (turns as stalks) | Transitions interpolate without a frame over 33 ms at 10k runs |
| T6 | **Single-run timeline**: virtualized list over headers, canvas minimap lanes, lenses, highlight + Prev/Next, hover preview, bodies on focus | 5k-message run scrolls end to end with no frame over 16 ms; body cache stays ≤ 32 MiB |
| T7 | **Beads zoom**: zooming into a corpus dot morphs it into its turn path, then opens T6 | Seamless at any zoom; opening a run loads only that run's header shard |
| T8 | **weft-native runs**: chain turns keyed by `chain_seq`/`uid` (ADR-069 D2); "open in conversation graph" jumps to ADR-067 view | A dot from a weft run resolves back to its chain atom |

Order: T1 → T2/T3 in parallel → T4 → T6 → T5/T7 → T8. T1 + T4 + T6 is the first
usable slice: browse every Claude Code session we have, colored by outcome, and
open any one instantly.

## Decisions to make (ADR before T2)

1. **Shard format.** Parquet (his choice: columnar, range-readable, with an `arrow`/`parquet`
   crate dependency) or RVF segments (rUv-native, already in the stack). Recommendation:
   RVF for embeddings and search, which is where it's already used, and Arrow IPC or Parquet
   for the catalog and headers, because columnar range reads are exactly the need. Settle it in the ADR.
2. **Projection algorithm.** UMAP has no mature pure-Rust crate that also builds for WASM.
   Options: compute on the backend in Rust (port or crate, to be evaluated), or
   use PaCMAP/t-SNE, or use a random-projection baseline. The client only ever reads coordinates.
3. **Render path for 10k+ instanced dots in egui.** The egui `Painter` with a mesh batch
   may be enough. If not, use an `egui_glow` paint callback with an instanced shader. 3-D needs
   the callback path either way. Measure this in T4 before committing to it.
4. **Where it lives.** A new Explorer app in `clawft-gui-egui`. The React `clawft-ui`
   route stays optional.
5. **Corpus scope.** Which sources are on by default, and the opt-in rule for
   client-confidential projects.

## Uses beyond browsing

- **Metaharness (ADR-096)**: see which harness edits move runs from `failed` to
  `fulfilled`, by grouping by harness version and coloring by outcome.
- **Lane health**: spot long, error-heavy runs across our agent lanes (developer, ruvpr, Doc)
  without reading them.
- **Classifier QA**: the agreement view checks our own scoring models before trusting them.
- **CoS**: the same catalog lets the CoS cite "runs like this one" with evidence.
