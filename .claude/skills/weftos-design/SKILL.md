---
name: weftos-design
description: Authoritative design discipline for WeftOS surfaces — tokens, composer-primitive usage, surface archetypes, empty/loading/offline contract, a11y floor, and OOB stock-desktop manifest. Use whenever creating, reviewing, or modifying a TOML surface fixture, the egui theming layer, or any user-visible WeftOS pane. Codifies docs/DESIGN.md and provides scaffold + audit scripts.
---

> Scope: native egui/TOML surfaces only. Web pages and web dashboards use `design-taste-frontend`; this skill's tokens (including the monospace offline hint) do not apply there, and taste's bans do not apply here.

# WeftOS Design — Operational Skill

This skill is the *enforcement + scaffolding* arm of `docs/DESIGN.md`.
DESIGN.md is the source of truth; this skill carries the levers, references,
and lint scripts that make the doc executable. Read DESIGN.md first; this
file is a working manual, not a re-spec.

## When to invoke

- **Authoring a new surface** (TOML fixture under `crates/clawft-surface/fixtures/` or `crates/clawft-app/fixtures/`).
- **Editing an existing surface** — re-audit before commit.
- **Touching `crates/clawft-gui-egui/src/theming.rs`** or any color literal in `crates/clawft-gui-egui/src/shell/`.
- **Adding a stock app** to the OOB manifest (DESIGN.md §9).
- **Reviewing a PR** that touches `ws08-weftos-gui`.

## Core levers

### Lever 1 — Tokens

`references/tokens.md` is the machine-readable mirror of DESIGN.md §2.
The egui theme at `crates/clawft-gui-egui/src/theming.rs` must match.
Drift is caught by `scripts/audit-theme.sh`.

Don't introduce new color literals in egui code. If you find yourself
typing `Color32::from_rgb(...)` in `shell/` or `explorer/`, stop and
either (a) use a token from `Tokens`, or (b) propose a new token in
DESIGN.md §2 first.

### Lever 2 — Primitive choice

`references/primitives.md` is the full prop table + rule excerpts.
Decision flow:

1. **Layout question?** → `stack` (default) → `grid` (regular tiles) →
   `tabs` (≤ 6 sibling views) → `dock` (app-level nav) → `strip` (small
   horizontal rail).
2. **Data display?** → match the data shape:
   - bounded scalar → `gauge`
   - unbounded scalar → `chip`
   - records (≥ 4) → `table`
   - hierarchical paths → `tree`
   - time series → `plot`
   - 2-D scalar field → `heatmap`
   - audio PCM → `waveform`
   - log lines → `stream`
   - file/image/audio → `media`
3. **Input?** → match the input shape:
   - tap → `pressable`
   - bool → `toggle`
   - bounded number → `slider`
   - 1-of-N (≤ 12) → `select`
   - text → `field`
4. **None of the above** → `canvas` (last resort) or `foreign` (escape
   hatch, must carry a TODO).

### Lever 3 — Archetype

Every WeftOS app is one of 5 archetypes (DESIGN.md §4):
`app-window` / `chip-detail` / `tile-grid` / `list-detail` / `stream`.
Pick one before authoring. New archetypes require a DESIGN.md amendment.

`references/archetypes.md` has TOML skeletons per archetype.
`scripts/scaffold-surface.sh --archetype <id> --substrate <path>`
produces a starting fixture wired against the chosen substrate root.

### Lever 4 — Empty / loading / offline

DESIGN.md §5 makes these mandatory on every user-visible surface
(chips are exempt — their parent window's empty hint covers them).
The audit script flags surfaces missing any of the three.

Render rules:
- **Loading**: italic dim text, no spinner.
- **Empty**: italic dim text + optional remediation pressable.
- **Offline**: tone=`crit` chip + monospace remediation hint.

### Lever 5 — Affordance dispatch

DESIGN.md §6. A surface only writes through declared `on_tap` /
`on_change` verbs that produce `PendingDispatch`. No host-side write
code per app. The audit script flags any TOML that declares a verb
not in the manifest's `influences` list.

### Lever 6 — Accessibility

DESIGN.md §7 enforces:
- WCAG AA contrast (audit script verifies).
- Tab order = TOML declaration order.
- ESC closes topmost modal.
- 22×22 px hit targets.
- State encoded by glyph + color, never color alone.

## Workflow — adding a new stock app

1. Confirm slot in DESIGN.md §9 (OOB manifest).
2. Pick archetype.
3. Run scaffold:
   ```bash
   bash .claude/skills/weftos-design/scripts/scaffold-surface.sh \
     --archetype app-window \
     --substrate substrate/fs \
     --out crates/clawft-app/fixtures/weftos-files.toml
   ```
4. Fill bindings from the substrate path's known schema.
5. Add empty/loading/offline states.
6. Add manifest entry at `crates/clawft-app/fixtures/weftos-<id>.toml`.
7. Register in `desktop.rs` stock-app list (TBD when the dock lands).
8. Audit:
   ```bash
   bash .claude/skills/weftos-design/scripts/audit-surface.sh \
     crates/clawft-app/fixtures/weftos-files.toml
   ```
9. Run `scripts/build.sh check + clippy + test`.

## Workflow — reviewing a surface change

```bash
bash .claude/skills/weftos-design/scripts/audit-surface.sh \
  crates/clawft-surface/fixtures/weftos-chip-kernel.toml
```

Returns 0 on clean, prints `<file>:<line>: <rule-id>: <message>` per
violation. Exit non-zero blocks merge if integrated into pre-commit.

## Workflow — auditing the theme

```bash
bash .claude/skills/weftos-design/scripts/audit-theme.sh
```

Greps the GUI crate for raw color literals, matches them against
`references/tokens.md`, flags drift.

## Reference files

- `references/tokens.md` — palette + type + spacing tables, machine-parseable.
- `references/primitives.md` — every primitive, props, rules, examples.
- `references/archetypes.md` — TOML skeletons for all 5 archetypes.
- `references/oob-manifest.md` — the 12 stock apps + their substrate roots + status.

## Templates

- `templates/app-window.toml`
- `templates/chip-detail.toml`
- `templates/tile-grid.toml`
- `templates/list-detail.toml`
- `templates/stream.toml`

Each is a runnable starting point — drop in, swap substrate paths, render.

## Hard rules (non-negotiable)

1. No `Color32::from_rgb` outside `theming.rs`. Existing offenders in
   `shell/desktop.rs` and `shell/grid.rs` are tracked under WEFTOS-DESIGN-1
   for graduation.
2. No new `ui://foreign` without a `# WEFTOS-DESIGN: TODO graduate to ui://X` comment.
3. No surface without empty + loading + offline.
4. No new heading larger than the 18 px Heading style.
5. No motion in chrome other than the wallpaper grid.
6. Every interactive primitive has a tab-order declaration via TOML order.

## Soft rules (require justification in PR description)

1. Window default size other than `880 × 580` for `app-window` archetype.
2. More than 6 tabs in a single `ui://tabs`.
3. Use of `ui://canvas` (every use is a TODO).
4. Tile size other than `120 × 96` (or integer multiples).

## Failure modes

- **Surface renders blank** → audit didn't catch a missing empty state.
  Fix the audit script; add a regression test.
- **Color literal drift** → `audit-theme.sh` missed a file.
  Extend its glob.
- **Affordance silently fails** → verb not in manifest `influences`.
  audit-surface should have flagged it.

## Versioning

This skill versions with `docs/DESIGN.md`. When DESIGN.md gets a
breaking change, bump the skill's reference files in the same commit.
