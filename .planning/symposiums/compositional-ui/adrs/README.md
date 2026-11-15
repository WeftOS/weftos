# Compositional UI Symposium — ADR Index

These ADRs are symposium-scoped (numbered 001 onward within this
folder) and do not collide with the repo-wide `docs/adr/` numbering.
All are **Proposed — symposium round 2** pending round-table review.

Each ADR follows: Status / Context / Decision / Consequences /
Alternatives considered / Related. The canon (four predicates,
Tier-A vs Tier-B split, active-radar loop, non-negotiable privacy)
is enforced by every ADR — none proposes changes to it.

| # | Title | One-line summary |
|---|-------|------------------|
| 001 | Primitive canon (20 items, amended 2026-04-19) | Freezes 19 Tier-A primitives + 1 Tier-B (`ui://foreign`); IRI stem + four-predicate check + egui crate per primitive. Amended to reconcile with spec (adds `ui://stream-view`) and to add `ui://canvas`. |
| 002 | Two-tier model & ForeignSurface promotion | No Tier-C; six canonical wrapper types ship day one; four-stage promotion gate (usage / proof / ADR / canon amendment). |
| 003 | WSP transport bindings | Local UDS + JSON-RPC, mesh CBOR in `MessagePayload::Wsp`, browser JSON-RPC over WebSocket — one CDDL, two encodings, no new rail. |
| 004 | Schema shape — state-diff + event-sourced | Surface tree is state-diff with `surface_version`; observation tapestry is event-sourced with `seq`; CRDT only inside leaf payloads. |
| 005 | WSP verb set (17 verbs) | Seventeen namespaced verbs: session (2), surfaces (4), subscribe (2), observe (2), invoke/mutate (2), governance/consent (3), introspection (2). |
| 006 | Primitive head (mandatory fields, amended 2026-04-19) | Every primitive head carries `state`, `affordances`, `confidence`, `variant-id`, `mutation-axes`, `privacy-flags`; missing any = malformed at kernel. Adds optional `tooltip`; `reorderable` as per-affordance flag. |
| 007 | Active-radar return schema | `CanonResponse` extends `egui::Response`; topology / doppler / range / bearing emitted on `ux/returns`; every echo stamps its variant-id. |
| 008 | Goal aggregate + per-goal delegation | `Goal` is an Evans aggregate root, chain-anchored; every WSP call carries `goal_id`; delegation is per-goal; `GovernanceGate` binding extended with `goal_id`. |
| 009 | Mission Console: seat / witness / anti-corruption | NORAD discipline for agent-composed shared surfaces: every participant gets a seat, every verb is witnessed on-chain, `ContextMap` enforces bounded contexts. |
| 010 | Reverse-DDD primitive mapping | Canonical table: aggregate root → Surface, entity → Chip, value → Field, invariant → AffordanceGate, context boundary → IRI namespace + governance, anti-corruption → IRI translation. |
| 011 | Dev-panel embedding — Option C hybrid | egui-wasm in VSCode `WebviewView` + `weft-capture` native sidecar for mic/camera/screen; M0/M1/M2 milestones; zero-business-logic extension. |
| 012 | Capture privacy + tray chip | `privacy-flags` structural at kernel boundary; consent per-goal with expiry on seal; kernel (not agent) composes persistent non-dismissible tray chip when capture is live. |
| 013 | `ui://canvas` primitive | Adds the 18th Tier-A primitive — freeform 2D painter with `draw / hit-test / pan / zoom / snapshot` affordances. Closes Fractal Clock / paint_bezier / pan_zoom / dancing_strings / painting gap. |
| 014 | Modal modality split | Extends `ui://modal` state with `modality: "modal" \| "floating" \| "tool" \| "toast"`; one primitive, four behaviours; governance interruption-priority per modality. |
| 015–019 | App layer (Round 3) | Not yet summarised in this index — see `ROADMAP.md`'s Round 3 section for titles (app manifest, surface description, ontology adapter contract, IDE bridge protocol, input modality + avatar). Index hygiene gap, not a content gap; flagged here rather than silently left out. |
| 020 | Sensor & telemetry display primitives | Adds `ui://radar-sensor`, `ui://spectrogram`, `ui://attitude-3d`, `ui://timeline-scrubber` (rows 22–25) + a `ui://gauge` circular-heading shape variant, found via a real-hardware crosswalk (XENSIV sensor shield dashboard). Canon count reaches 25, the foundations.md ceiling. Also flags a separate ADR-001/ROADMAP/code drift on row 20 (`ui://foreign` vs. the shipped `ui://thread-dock`). |

## Authority + cross-reference

- **Canon source**: `../foundations.md` — four predicates, Tier-A/B,
  active-radar loop, non-negotiable privacy. All twelve ADRs cite
  at least one foundation element.
- **Research source**: `../session-1-historical-canon.md` through
  `../session-9-agentic-os-canon.md`. Every ADR cites at least two
  sessions.
- **Agenda**: `../AGENDA.md` — RQ1..RQ9 mapping. The ADR track is
  the Round 2 synthesis deliverable.
- **Renderer**: egui is canonical. Renderer-disposability (AGENDA
  success criterion 1) is proved when a second renderer can
  implement these ADRs without touching composition logic.

## Reading order

For a first pass, read in dependency order: **001 → 002 → 006 →
003 → 004 → 005 → 007 → 010 → 008 → 009 → 011 → 012 → 013 → 014**.
Each builds on the ones before it. For a second pass (conformance),
read **006 → 007 → 008 → 012** as the "kernel boundary" quartet —
these are the structural invariants that make foundations' non-
negotiables type-system facts rather than conventions.

## Amendment log

- **2026-04-19** — Canon crosswalk against `https://www.egui.rs/#demo`
  (via `weft-demo-lab`) surfaced three gaps: no primitive for
  freeform 2D drawing (Fractal Clock, paint_bezier, pan_zoom),
  modal semantics too narrow for floating tool palettes, and
  tooltip/reorderable affordances missing from the head. Resolved
  by **ADR-013** (add `ui://canvas`), **ADR-014** (Modal modality
  split), and in-place amendments to **ADR-001** (canon table)
  and **ADR-006** (head fields). Coverage after amendments: 20/20
  visible demo behaviors either mapped to canon primitives or
  already composable.
- **2026-11-06** — Real-hardware crosswalk (Infineon XENSIV sensor
  shield dashboard, not a demo surface) surfaced four more gaps: no
  primitive for a live FMCW/Doppler radar display, an audio/vibration
  spectrogram, a 3D attitude/calibration view, or a shared multi-stream
  record/replay transport — plus a compass-dial case that is a
  `ui://gauge` variant, not a new primitive. Proposed by **ADR-020**,
  not yet resolved (pending round-table review, unlike the 2026-04-19
  entry above which cites already-resolved ADRs). ADR-020 also flags
  that `ui://foreign` (ADR-001 row 20) was never implemented in
  `clawft-canon` and `ui://thread-dock` shipped in its place, contrary
  to both ADR-001's table and `ROADMAP.md`'s M2 section — unreconciled,
  owner decision needed.
