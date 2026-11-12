# WeftOS — DESIGN.md

**Status**: 0.1 draft — 2026-05-01.
**Owner**: GUI workstream (ws08).
**Scope**: visual + interaction contract for every WeftOS surface, native and webview. Source of truth for the `weftos-design` skill (`.claude/skills/weftos-design/`), the `surface_host::compose()` runtime, the egui theming layer, and every TOML surface fixture.

This is *not* a brand book and *not* a component library. It is the ruleset that decides *whether a surface is a WeftOS surface*. The composer primitives are the brushes; this document is the grammar.

---

## 1. Operating principles

1. **Substrate-first.** Every surface binds to substrate paths. No bespoke "demo data." If the daemon is offline, the surface renders an explicit empty state — never silent emptiness.
2. **Declarative by default.** New OS tools land as TOML surface descriptions consumed by `surface_host::compose()`. Native egui code is reserved for primitives that the composer doesn't yet host (Explorer tree, Terminal, Chat) and is *graduated to TOML* as the composer gains primitives.
3. **Calm shell, loud signals.** The desktop chrome is dark, neutral, and silent. Color is reserved for state (ok / warn / crit / accent-on-focus). No gradients in chrome; gradients only in data viz where they encode magnitude.
4. **Honest empty states.** Three failure modes are distinguishable on every surface: connection down, in-flight, no data yet. Never one ambiguous blank.
5. **Affordance dispatch is the only write path.** Affordances declared in TOML produce `PendingDispatch { verb, params, source_path }` records, drained by the host and submitted as RPC. No host-side write code per app.
6. **Accessibility is non-negotiable.** WCAG 2.1 AA contrast on every text-on-surface pair. Every interactive primitive has a keyboard path. Every state encoded by color is also encoded by glyph or text.

---

## 2. Tokens

The egui theme at `crates/clawft-gui-egui/src/theming/` is the runtime carrier. This section is the *spec*; the runtime must match.

The module is `theming/mod.rs` (`Tokens`, `apply`), `theming/palette.rs` (exact-value named colours for surfaces not yet folded into `Tokens`, plus the `data_rgb` / `data_rgba` helpers for data-derived colours), and `theming/ansi.rs` (the terminal 16-colour / xterm-256 table). `theming/palette_contract.rs` pins every palette and ANSI value. The `audit-theme.sh` D-TK01 check exempts the whole `theming/` directory.

`apply(ctx)` installs the tokens' `egui::Visuals` through `Tokens::style()` (`style.visuals = Tokens::visuals()`), so the widget tints, selection colours and `override_text_color` in §2.1 are what egui draws. `theming::apply_contract` fails if the context's visuals ever differ from `Tokens::visuals()` (before 2026-11-12 egui's stock visuals silently replaced them).

### 2.1 Palette

| Role | Token | sRGB | Notes |
|---|---|---|---|
| Surface 0 (app root) | `bg_app` | `#08080A` | Behind everything. Wallpaper paints over. |
| Surface 1 (panels, dock, tray) | `bg_panel` | `#0E0E12` | |
| Surface 1s (sidebar) | `bg_sidebar` | `#2A2A30` | warm-lifted charcoal — visibly distinct from `bg_app` while remaining a charcoal grey, no chromatic cast. Sidebar only. |
| Surface 2 (windows, cards, inset) | `bg_surface` | `#16161C` | |
| Surface 3 (hover) | `bg_hover` | `#202028` | |
| Surface 4 (active / selected) | `bg_active` | `#2C2C36` | |
| Stroke hair | `stroke_hair` | `#FFFFFF @ 14/255` | 1px separators inside panels |
| Stroke soft | `stroke_soft` | `#FFFFFF @ 24/255` | 1px panel + window edges |
| Text primary | `text_primary` | `#E0DEE8` | Body, headings |
| Text secondary | `text_secondary` | `#AAA8B4` | Labels, monospace meta |
| Text dim | `text_dim` | `#706E7A` | Captions, hints |
| Accent | `accent` | `#C4A25C` | Focus / active *only*. Sampled from logo. |
| Accent dim | `accent_dim` | `#C4A25C @ 80/255` | Selection background |
| State ok | `ok` | `#6EC896` | Healthy, success, online |
| State warn | `warn` | `#DCAF55` | Degraded, attention |
| State crit | `crit` | `#DC5F5F` | Failed, offline, security |

**Banned in chrome**: pure white, pure black foregrounds, saturated brand colors, gradients on backgrounds, decorative color tints on the wallpaper. **Allowed in viz**: gradients on `ui://gauge`, `ui://heatmap`, `ui://plot`, `ui://waveform` only when encoding a scalar.

The wallpaper is monochrome. Color appears only as: (a) state on a chip / pill / glyph, (b) accent on a focused widget, (c) data in a viz primitive. Anywhere else — including ambient backgrounds — color is a bug.

### 2.2 Type scale

| Style | Family | Size | Use |
|---|---|---|---|
| Heading | Proportional | 18 px | Window titles, app headers (one per pane) |
| Body | Proportional | 13 px | Default |
| Small | Proportional | 11 px | Captions, hints, dim labels |
| Mono | Monospace | 12.5 px | Paths, IDs, code, terminals |
| Button | Proportional | 13 px | Same as body |

No additional sizes without an entry here. Heading is *not* used twice in the same window.

### 2.3 Spacing & radius

- Item spacing: `6 × 6` px (egui `spacing.item_spacing`).
- Button padding: `10 × 5` px.
- Panel inner margin: `8` px.
- Window inner margin: `8` px.
- Indent: `14` px.
- Corner radius: `4` px (panels, cards, buttons), `3` px tight, `8` px windows (= `rounding × 2`).
- Stroke width: `1.0` px everywhere; never `2`.

### 2.4 Motion

- Wallpaper warped-grid: ~60 fps, ambient. No reaction to interaction.
- Hover: instant; no ease.
- Selection / focus: instant.
- Window open / close: respect egui defaults; no extra easing.
- Toasts (e.g. Copy Path confirmation): visible 1500 ms, then drop. Match `COPY_TOAST_DURATION` at `explorer/mod.rs:75`.
- Stream / plot animation: data-driven; no decorative sweep.

### 2.5 Iconography

- Glyph set: egui-builtin Phosphor / system fallbacks. No custom icon font in 0.7.x.
- Use glyphs **only** alongside text; never as the sole label on a primary action. Tray chips are the lone exception (icon + small caption beneath).
- State glyphs (ok/warn/crit) are paired with the corresponding state color.

---

## 3. Composer primitives — usage rules

The `surface_host::compose()` runtime hosts 23 primitives. Authoring rules below; full prop tables in `.claude/skills/weftos-design/references/primitives.md`.

### 3.1 Layout

| Primitive | When | When NOT |
|---|---|---|
| `ui://stack` | Vertical or horizontal flow of heterogeneous children. The default container. | When children are uniform list items — use `ui://table`. |
| `ui://grid` | Fixed-column dashboard tiles or app launcher. | Variable-width content; nest `ui://stack`s inside grid cells if needed. |
| `ui://tabs` | ≤ 6 sibling views of one logical surface. | Navigation between *apps* — use the dock. |
| `ui://strip` | Horizontal rail of small status / chip elements. | Anything that needs to wrap. |
| `ui://dock` | App-level navigation bar. One per surface, top or left. | Inside a window's body. |
| `ui://sidebar` | Desktop-level collapsible nav with sections + nested items. One per desktop. | App-level nav (use `ui://dock`). |
| `ui://sheet` | Bottom sheet for modal-but-not-blocking content. | Critical confirmations — use `ui://modal`. |
| `ui://modal` | Blocking confirmation or single-task entry. | Anything the user might want to dismiss by clicking out — use `ui://sheet`. |

### 3.2 Data display

| Primitive | When | When NOT |
|---|---|---|
| `ui://chip` | Single state value, optionally tone-colored from a substrate field. | Tappable navigation — use `ui://pressable`. |
| `ui://gauge` | Bounded scalar (0..max) with a known range. | Unbounded counts — use `ui://chip` with the number. |
| `ui://table` | Tabular records with sortable columns. | < 4 rows — use `ui://stack` of `ui://chip`s. |
| `ui://tree` | Hierarchical paths (substrate, filesystem). | Anything whose depth > 3 typically — use `ui://table` with a path column. |
| `ui://plot` | Time series with quantitative y-axis. | Categorical comparisons — use `ui://table`. |
| `ui://heatmap` | 2-D scalar field. | Sparse data — use `ui://plot`. |
| `ui://waveform` | Audio PCM, raw or windowed. | Non-audio signals — use `ui://plot`. |
| `ui://stream` | Append-only log lines, latest-first. | Bounded record sets — use `ui://table`. |
| `ui://canvas` | Composer escape hatch for ad-hoc drawing. | Anything reachable with the other primitives. |
| `ui://media` | Image / video / audio file rendering. | Rich text — use a `ui://stack` of styled labels. |

### 3.3 Input

| Primitive | When | When NOT |
|---|---|---|
| `ui://pressable` | Any tappable region. The general button. | Toggle on/off — use `ui://toggle`. |
| `ui://toggle` | Boolean substrate value the user can flip. | Tri-state — use `ui://select`. |
| `ui://slider` | Bounded numeric input. | Discrete steps — use `ui://select`. |
| `ui://select` | One of N enumerated choices. | > 12 choices — use `ui://field` with autocomplete. |
| `ui://field` | Free text or constrained string entry. | Multi-line code — use a viewer with `Field::Code`. |

### 3.4 Adoption

| Primitive | Status | Notes |
|---|---|---|
| `ui://foreign` | Composer escape into native egui code. | **Discouraged**. Every use is a TODO to graduate to a real primitive. Listed in `weftos-design audit` output. |

### 3.5 Required slots on every container

- `empty_state`: rendered when bindings resolve to no data. **Never** silently empty.
- `loading_state`: rendered while the first poll is in flight.
- `offline_state`: rendered when connection is `Disconnected`.

If a primitive doesn't define these, the host renders the canonical fallback at `desktop.rs:render_empty_hint` semantics. The skill's `audit-surface.sh` flags surfaces that don't override these on user-facing apps (chips are exempt — their parent window's empty hint covers them).

---

## 4. Surface archetypes

Every WeftOS app is one of these five shapes. New shapes require a DESIGN.md amendment.

### 4.1 `app-window` — full app, e.g. Files, Settings, Network

```
┌── Title bar ─────────────────── _ □ × ┐
│  Heading              [optional toolbar] │
├──────────────────────────────────────────┤
│ ┌── ui://dock ──┐ ┌── content ────────┐ │
│ │ section A     │ │                   │ │
│ │ section B     │ │  ui://stack /     │ │
│ │ section C     │ │  ui://grid /      │ │
│ │               │ │  ui://table       │ │
│ └───────────────┘ │                   │ │
│                   └───────────────────┘ │
└──────────────────────────────────────────┘
```

- Default size `880 × 580`. Resizable.
- Heading once, top-left.
- Dock left when sections > 3; tabs top when ≤ 3.
- Content area uses one root container.

### 4.2 `chip-detail` — substrate subsystem inspector, e.g. Kernel, Mesh, ExoChain chips

- Default size `420 × 320`.
- Header: title + monospace substrate path + `connection_pill`.
- Body: composer surface bound to a substrate subtree (`subscriptions = [...]` in fixture).
- Empty hint **mandatory** — see `desktop.rs:render_empty_hint`.

### 4.3 `tile-grid` — launchers, dashboards, gallery

- Tile = `ui://pressable` containing `ui://stack` { icon, label, optional `ui://chip` }.
- Tile size `120 × 96` baseline; integer multiples allowed.
- 6 columns at default window width.

### 4.4 `list-detail` — explorer, settings, file picker

- Left rail: `ui://tree` or `ui://table` of records.
- Right pane: detail viewer dispatched on selection.
- Selection survives expand/collapse.

### 4.5 `stream` — terminal, logs, chat, narration tail

- One scroll region, append-only.
- Filter chips at top via `ui://strip`.
- Auto-scroll to tail unless user scrolled up.

---

## 5. Empty / loading / offline contract

A surface has three displayable non-data states. Each must render a specific shape:

**Loading** — first poll in flight.
- Body: italic dim text, e.g. *"Waiting for first poll tick (~1s)."*
- Spinner forbidden (egui spinners read as "infinite work" in calm chrome).

**Empty (no data yet)** — connected, but bound paths return null.
- Body: italic dim text describing *what* would appear here.
- Optional secondary line: a `ui://pressable` to install / configure / start a service.

**Offline** — daemon link `Disconnected`.
- Top of pane: `ui://chip` with `tone = crit`, label *"◉ Demo mode — kernel daemon offline."*
- Below: optional remediation *"Start with: `weaver kernel start`"* in monospace.

Color and glyph in lockstep — never color alone.

**Identity strip rule.** The first line of the sidebar shows `WeftOS v<version>`. The second line is the **instance name** — read from `substrate/config/identity/instance_name`. If unset, the loader falls back to the working-directory basename (e.g. `clawft`, `weftos-prod`). Never blank. Editable in Settings → Identity → Instance name.

**Kernel chip placement.** The Kernel chip — daemon-link indicator (green=connected, amber=connecting, red=disconnected) — sits *immediately below* the identity strip, inside the sidebar header (still above any divider that separates header from menu). It is the second-most-important element in the sidebar after identity itself. There is no separate "disconnected" pill anywhere; the Kernel chip is the single source of truth for daemon liveness.

**No tray / status bar.** The desktop has no bottom bar, top bar, or tray. System-wide signals (connection, identity) live inside the sidebar header. Subsystem chips live nested under their owning app in the sidebar. The wallpaper region is reserved entirely for app windows and the empty-state caption — nothing else paints over it.

**No clock in the desktop chrome.** The OS does not render a wall clock. Clocks belong inside apps that need them (Scheduler, Logs timestamps, Chat). Users with the OS-level locale already see the system clock from the host environment.

**Sidebar surface tier.** The sidebar uses a *visibly* distinct charcoal from the rest of the chrome so the divide between launcher and workspace reads instantly even on cheap monitors. `bg_sidebar = #2A2A30` — pure desaturated **lifted neutral charcoal**, clearly lighter than `bg_app` (`#08080A`). **No hue cast.** Not warm, not cool, not purple, not blue. Two tiers of grey, that's all. A future light theme will define `bg_sidebar` separately; the dark theme is the only authoritative palette for 0.8.x.

**Canonical sidebar layout (frozen).** The sidebar is **identical on every screen** — no per-app variants, no truncation, no reflow. Any drift is a bug. Spec, top to bottom:

```
WIDTH: 220px exactly. FILL: bg_sidebar #2A2A30. RIGHT EDGE: 1px stroke #FFFFFF @ 24/255.
NO rounded corners. NO drop shadow. NO outer padding.

[ HEADER — 110px ]
  · 12px left padding, 14px top padding
  · Line 1: monospace 13px '#AAA8B4' → "WeftOS v0.8.0"
  · Line 2: monospace 13px '#AAA8B4' → instance name (e.g. "clawft")
  · Line 3 (8px below line 2): KERNEL CHIP, full-width minus 12px L/R padding,
    32px tall, 4px rounded outline #FFFFFF@24, contents:
      [● dot 8px] · [label "Kernel"] · [thin sep dot] · [state text]
    State color rule:
      connected   → dot #6EC896, "Kernel" + state in #AAA8B4 dim grey, NO bright label color
      connecting  → dot #DCAF55, same dim text
      disconnected→ dot #DC5F5F, label "Kernel" + state "disconnected" in #DC5F5F (only when offline)

[ DIVIDER ]
  · 1px line #FFFFFF @ 18/255, full sidebar width

[ MENU — vertical stack, 32px row height, 12px L padding, 13px label, dim grey #AAA8B4 ]
  · Row format: [16px monoline icon] [8px gap] [label]  + optional ▾ chevron right-aligned for groups
  · Selected row: full-width tile (bg_active) with a 3px left edge stripe in `accent`
                  (accent = active). A hovered row is bg_hover only, with no stripe.
  · Items in this exact order, every screen:
      Files            (folder icon)
      Processes        (equalizer-sliders icon)
      Services         (satellite-dish icon)
      Network ▾        (globe icon, expandable group)
      Settings         (slider-with-knob icon)
      Scheduler        (clock-face icon)
      Monitor          (bar-chart icon)
      Logs ▾           (three-horizontal-lines icon, expandable group)
      Terminal         (prompt-arrow icon)
      Chat             (speech-bubble icon)
      Admin            (shield icon)
      Explorer         (compass icon)
      Apps ▾           (grid-of-nine-squares icon, expandable group)

  · When a group is expanded, sub-items render below at +16px indent, 28px row,
    label prefixed by "· " in dim grey #707080. Group state from
    substrate/desktop/sidebar/expanded[]. The active screen's sub-item
    (if any) gets the selected-tile treatment.

[ FOOTER — pinned to bottom edge ]
  · 1px divider #FFFFFF @ 14/255
  · 32px row: [◀ glyph 12px] [8px gap] [label "collapse"] in dim grey #AAA8B4
```

The active screen is signaled by:
1. Highlighting the corresponding leaf row (or group + sub-item).
2. Nothing else changes — width, fill, divider, header, footer are byte-identical across all screens. If a future surface needs ambient state in the sidebar, it goes through the Kernel chip pattern, not a per-screen mutation.

**Mockups must obey this spec.** Renders that drop the identity strip, change sidebar fill, swap menu order, omit the footer collapse handle, or mutate the Kernel chip layout are non-canonical and need re-rendering.

**Clock rule.** The tray clock displays the user's local time by default, formatted from the OS locale (`HH:MM` 24h or `h:mm AM/PM` per locale). Hover-tooltip shows UTC + IANA zone. The user may override the displayed zone via Settings → Identity → Time zone (substrate key `config/identity/timezone`). Never hard-code UTC.

---

## 6. Affordance dispatch contract

Every interactive primitive that mutates state declares a `verb` + `params`:

```toml
[[surfaces.root.children]]
type    = "ui://pressable"
id      = "/root/restart"
attrs   = { label = "Restart service" }
on_tap  = { verb = "kernel.restart-service", params = { name = "$row.name" } }
```

The host (`compose.rs`) collects these into `PendingDispatch` records, drained by the calling code (`desktop.rs:render_selected_app:706`) and submitted via `Live::submit`.

Rules:
1. Never declare a verb the user's permission level can't authorize. Use the `permissions` array on the manifest (`weftos-admin.toml:influences`) to hint a preflight check.
2. Confirm-dangerous: deletes / kills / restarts open a `ui://modal` first.
3. After-tap feedback: tone-changes on bound chips drive themselves via the next substrate poll. Don't paint an optimistic state.

---

## 7. Accessibility floor

- Contrast: every text-on-bg pair ≥ 4.5:1 (WCAG AA). Verify with `weftos-design audit`.
- Tab order: every interactive primitive reachable in declared TOML order.
- ESC closes the topmost modal/sheet.
- Hit target ≥ `22 × 22` px (egui `interact_size.y`).
- Color-blind: state encoded by glyph + color, never color only. Verify chip rendering at `tray.rs:66-68`.
- Focus ring: `accent` 1px stroke on focused widget. Never suppress.

---

## 8. Out-of-the-box (OOB) requirement

The desktop must be useful with **zero substrate data and zero installed apps**. On a freshly booted daemon with no adapters yet attached, every surface in §9 must render its empty state with a clear next step (install / configure / start / explore). A "stock" desktop that fails to communicate why a panel is empty is a **DESIGN.md violation**.

---

## 9. Stock desktop manifest (0.8.x target)

The OOB desktop ships with these surfaces installed and visible from boot. Authoritative list; the `weftos-design` skill validates that every entry has a corresponding TOML at `crates/clawft-app/fixtures/` or `crates/clawft-surface/fixtures/`.

**Sidebar layout** (replaces the dock-app rows). The desktop sidebar is the single launcher surface — collapsible (collapsed = icon-only rail) and slide-out (off-screen left, edge-handle to bring it back). Each entry is one of two kinds:

- **Leaf** — clicking opens the app directly. Example: Files.
- **Group** — expandable in place; reveals nested leaves. Example: Apps → Built-in / Installed / Developer.

| Slot | Kind | App id / target | Archetype | Substrate roots |
|---|---|---|---|---|
| Sidebar header | identity | `WeftOS v<x.y.z>` + instance_name (`config/identity/instance_name` ∥ cwd basename) | — | `config/identity/*` |
| Sidebar header | chip | (built-in) Kernel — connection indicator | chip-detail | `substrate/kernel/status` |
| Sidebar header | leaf | (built-in) Explorer — substrate browser | list-detail | `substrate/` (whole) |
| Sidebar 1 | leaf | `app://weftos.files` | app-window | `substrate/fs/*`, `derived/*` |
| Sidebar 2 | leaf | `app://weftos.processes` | app-window | `substrate/kernel/processes` |
| Sidebar 3 | leaf | `app://weftos.services` | app-window | `substrate/kernel/services` |
| Sidebar 4 | group | **Network** → Mesh / Wi-Fi / Bluetooth | app-window | `substrate/mesh/*`, `substrate/wifi/*`, `substrate/bluetooth/*` |
| Sidebar 5 | leaf | `app://weftos.settings` | app-window | `substrate/config/*` |
| Sidebar 6 | leaf | `app://weftos.scheduler` | app-window | `substrate/scheduler/*` |
| Sidebar 7 | leaf | `app://weftos.monitor` | tile-grid + plots | `substrate/kernel/*`, `substrate/sensors/*` |
| Sidebar 8 | group | **Logs** → System / Witness chain (ExoChain) | stream | `derived/logs/*`, `substrate/exochain/*` |
| Sidebar 9 | leaf | `app://weftos.terminal` | stream (native) | n/a |
| Sidebar 10 | leaf | `app://weftos.chat` | stream (native) | `derived/chat/*` |
| Sidebar 11 | leaf | `app://weftos.admin` | app-window | `substrate/kernel/*` (existing reference app) |
| Sidebar 12 | group | **Apps** → Built-in / Installed / Developer | tile-grid | (browses installed apps) |

Each has a mandatory `empty_state` + `offline_state` per §5.

---

## 10. Compliance: how the skill enforces this doc

`.claude/skills/weftos-design/` ships:

- `references/tokens.md` — the §2 table machine-readable.
- `references/primitives.md` — per-primitive prop reference + rule excerpts from §3.
- `references/archetypes.md` — §4 templates as TOML skeletons.
- `scripts/scaffold-surface.sh` — generate a TOML stub from `--archetype <id> --substrate <path>`.
- `scripts/audit-surface.sh` — lint a TOML against §2 tokens, §3 usage rules, §5 empty/loading/offline coverage, §7 a11y; report violations with file:line.
- `scripts/audit-theme.sh` — lint `theming/` + `desktop.rs:render_empty_hint` etc. against §2.

DESIGN.md changes require running `scripts/audit-surface.sh` against every fixture in `crates/clawft-surface/fixtures/` and `crates/clawft-app/fixtures/` before commit.

---

## 11. Versioning

DESIGN.md is versioned with the workspace (`workspace.package.version`). Breaking-grammar changes (new mandatory slot, removed primitive, palette change) require a minor bump and a migration note in the same commit. Token-level changes (single shade, spacing tweak) are patch-level.

---

## 12. Open questions (0.8.x)

- **Layered theming.** Today there's one Tokens struct. A "high-contrast" theme and a "kiosk" theme are likely needed before 1.0.
- **Localization.** All strings in fixtures are en-US literals. A `i18n://` binding scheme is open.
- **Voice surface.** The voice channel needs surface conventions for partial transcripts, wake-word state, mic privacy. Tracked under WEFT-205 / SC-1.
- **Foreign primitive deprecation.** `ui://foreign` is the current escape hatch; the goal is to graduate every use into a real primitive. Each instance must carry a `# WEFTOS-DESIGN: TODO graduate to ui://X` comment.
