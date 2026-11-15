# Compositional UI — Roadmap

**Purpose**: a single consolidated list of what's done, what's queued,
and what's deferred. Sources the fragmented lists that already exist
across AGENDA / sessions / ADRs / spec. Update this file as work moves.

Last updated: 2026-04-21 (M1.5.1α shipped — built-in system components functional).

---

## ✅ Done (Rounds 1–3)

### Round 1 — Foundations
- **`foundations.md`** — four predicates, two-tier model, observation
  tapestry, digital-exhaust = intent, active-radar loop.
- **`AGENDA.md`** — scope, Cooper + Evans + Rasmus + AIOS + Cedar
  anchors, nine research questions.
- **9 session findings** (~31,900 words): historical canon, agentic
  UI SOTA, shared realtime, ontology→UI, renderer+egui, protocol
  design, dev-panel embedding, governance, agentic-OS framing.

### Round 2 — Decisions & protocol
- **14 ADRs** (`adrs/adr-001`…`adr-014`, all *Proposed — Round 2*).
  ADR-013 (`ui://canvas`) and ADR-014 (Modal modality split) added
  2026-04-19 after the demo-lab crosswalk surfaced gaps; ADR-001 and
  ADR-006 amended in place. Canon count reconciled to **21 primitives
  (20 Tier-A + 1 Tier-B)** after the 2026-04-20 amendment re-promoted
  `ui://tabs` as row 21 (distinct from `ui://dock`).
- **`protocol-spec.md`** — WSP-0.1 full spec (11k words, 20 §, full
  CDDL appendix, three wire example flows).

### Milestones shipped
- **M0 — VSCode/Cursor extension scaffold** at
  `extensions/vscode-weft-panel/` — daemon ↔ extension ↔ webview wire
  loop, reload-survival, two sample RPC verbs.
- **M1 — egui-wasm surface in the Cursor webview.**
  `clawft-gui-egui` compiles to `wasm32-unknown-unknown` via wasm-pack;
  target-conditional `Live` (tokio+UDS on native, postMessage bridge on
  wasm); shared monotonic `now_ms()`, unified reply-channel API.
  Extension host loads the bundle from `webview/wasm/`, installs
  `window.__weftPostToHost`, proxies the four poll RPCs to the daemon.
  Build script: `scripts/build-wasm.sh`. Commits `1204f7a`, `894de99`.
- **M1 polish (2026-04-20)** — `web-time` migration (wasm-safe
  Instant/SystemTime), PNG boot logo (PNG-format pin for wasm image
  loaders), VSCode extension wasm hot-reload (`fs.watch` + cache-bust
  token re-render), `ui/` gitignore. Commit `6e2df4d`.
- **M1 canon implementation (2026-04-20)** — full 21-item canon
  landed across four commits:
  - `eae8c17` foundation: `CanonWidget` trait + `CanonResponse`
    (topology/doppler/range/bearing) + `Pressable` reference + ADR-001
    row 21 amendment.
  - `853b9a8` retrofit: 8 existing blocks wrapped (Chip, Stack, Strip,
    StreamView, Table, Tree, Gauge, Plot).
  - `54557f5` new primitives: 11 missing items (Field, Toggle, Select,
    Slider, Grid, Dock, Sheet, Modal, Media, Canvas, Tabs). Adds
    `egui_dock 0.14` dep.
  - `7ddc5ee` merge into `development-0.7.0` + 7 head-metadata smoke
    tests (identity URIs, toggle affordances, modal-freeze on consent
    flows, canvas transform default, field value kind tags, grid has
    no affordances, tabs exposes switch-tab).
- **Canon demo lab in the panel (2026-04-20)** — `canon_demos.rs` +
  `Blocks | Canon` section toggle in the Desktop Blocks window. 20
  per-primitive demos with live `CanonResponse` footer read-out
  (identity / variant / bearing / latency). Commit `7c0c523`.
- **M1.5 — First real panel driven by a surface description
  (2026-04-20)** — the app-layer acceptance milestone from session-10
  §7 landed in nine commits on `development-0.7.0`, culminating in
  merge `0e32e67`. Four parallel-agent streams + three expert reviews
  + two review-driven reliability patches + one integration pass.
  Details in the dedicated section below.

### Round 3 — App Layer planning (2026-04-20)
- **`session-10-app-layer.md`** (292 lines) — anchor doc. 3 modes
  (`single-app` / `desktop` / `ide`) × 4 inputs (`pointer` / `touch` /
  `voice` / `hybrid`). Avatar as `single-app × voice` archetype with
  `foreign://avatar` + narration contract. Four reference panels
  (WeftOS Admin, Project OS, Deployment Monitor, Avatar Companion)
  cross-walked against modes and inputs.
- **5 new ADRs** (all *Proposed — Round 3*):
  - **ADR-015** App manifest — TOML schema, `supported_modes`,
    `supported_inputs`, `narration`, `permissions`, `entry_points`,
    three authoring paths, semver backwards-compat rules.
  - **ADR-016** Surface description format — TOML-with-extensions +
    equivalent Rust builder, tree of typed primitive nodes, binding
    expression language (shared with narration), conditional
    rendering, local compositions, mode-specific variants, headless
    test harness.
  - **ADR-017** Ontology adapter contract — `OntologyAdapter` trait,
    `TopicDecl` + `StateDelta`, back-pressure policy, 7 reference
    adapters (`kernel`, `git`, `gh`, `workspace`, `fs`, `lsp`,
    `deployment`), sensitivity + ADR-012 capture binding,
    `substrate/meta/adapter/*/health` for offline awareness.
  - **ADR-018** IDE bridge protocol — 4 linkage components, six
    `substrate/editor/*` topics, seven `ide.*` affordances,
    follow-editor manifest DSL, chrome-match palette bridge, VSCode
    postMessage transport (first class) + JetBrains / Neovim
    deferred, handshake + capability negotiation, security + governance.
  - **ADR-019** Input modality + `foreign://avatar` + narration —
    Part A input axis adaptation rules (hit-target floor, hover
    semantics, gesture mapping, chrome-per-mode matrix, safe-area
    insets). Part B **amends ADR-006** with `invocations: [pointer |
    touch | voice | gesture]` on every `Affordance`. Part C adds
    `foreign://avatar` Tier-B kind + narration contract in app
    manifests + voice-triggered launcher + mandatory caption
    `StreamView` for accessibility.

---

## ✅ M1.5 — delivered 2026-04-20

Merged to `development-0.7.0` as `0e32e67`. Acceptance
(session-10 §7) met 5/5:

1. `clawft-app` crate with manifest parser + JSON registry. ✅
2. `clawft-surface` crate with IR + composer + expression lang. ✅
3. Kernel adapter refactored to emit `substrate/kernel/*` topics as
   deltas via the ADR-017 trait. ✅
4. WeftOS Admin launchable in `ide × pointer` and `desktop × pointer`. ✅
5. Desktop Blocks window has a new `Apps` section. ✅
   (`BlockKind` retained as parallel Blocks demos — deletion deferred
   to M1.6+ when more apps cover the remaining slots.)

### What shipped

- **`clawft-app`** (new crate) — ADR-015 subset. `AppManifest` struct,
  TOML parse + serde round-trip, `Mode`/`Input`/`EntryPoint`/
  `Permission` enums (canonical location), `AppRegistry` backed by
  JSON at `$XDG_DATA_HOME/weftos/apps.json`, `governance::Gate` trait
  with `NoopGate` + `StrictGate`, 24 tests green.
- **`clawft-surface`** (new crate) — ADR-016 subset. `SurfaceTree` IR,
  TOML parser + Rust builder producing identical IRs, hand-rolled RD
  parser for binding expression language (literals / `$path` reads /
  field access / `count`/`filter`/`len`/`first`/`last`/`fmt_*` /
  binops / single-level lambdas), `when:` conditional rendering,
  composer runtime driving Pressable / Chip / Gauge / Table /
  StreamView / Stack / Strip / Grid (13 other primitives emit
  visible TODO labels pending later milestones), headless render
  test harness, 36 tests green.
- **`clawft-substrate`** (new crate) — ADR-017 trait. `OntologyAdapter`
  + `TopicDecl { path, shape, refresh_hint, sensitivity,
  buffer_policy, max_len }` + `StateDelta { Append | Replace |
  Remove }` + `Subscription`. `KernelAdapter` emits the four
  `substrate/kernel/*` topics. `Substrate` state tree tracks
  subscriptions with `JoinHandle`s, exposes `close_all()` for graceful
  shutdown. Log-ring trim (1000 entries) and watermark-overflow
  safety (synthetic "log window overflow" warn entry rather than
  re-emitting everything). 22 tests green.
- **`clawft-gui-egui` integration** — `surface_host` module exposes
  `render_headless`. `Desktop` gains `app_registry`, `selected_app`,
  `app_surfaces` fields; new `PanelSection::Apps` shown alongside
  `Blocks` and `Canon`. WeftOS Admin auto-installs from a bundled
  fixture on boot so the Apps tab always has content. `live/`
  internally driven by `KernelAdapter` + `Substrate`; public
  `Live`/`Snapshot` API preserved byte-for-byte.
- **End-to-end acceptance test** — `tests/admin_app_e2e.rs` wires a
  `CannedKernelAdapter` → `Substrate::subscribe_adapter` → parsed
  admin surface → `surface_host::render_headless` → asserts non-empty
  responses + presence of `ui://gauge`/`ui://table` primitives. One
  test, four layers, full M1.5 acceptance contract. ✅

### How the work was done (process notes for future milestones)

Four parallel worktrees off `development-0.7.0` at `d3d214f`:
`m15/app`, `m15/surface`, `m15/kernel-adapter`, `m15/integration`.
Three implementer agents + three reviewer agents + two review-driven
fix agents + one integration agent. Reviewer agents found:
- A latent parser bug in `clawft-surface` (`parse("x - 1")` lost the
  RHS) — fixed on branch before integration.
- Watermark-overflow re-emission in `clawft-substrate` logs + missing
  ring trim — both fixed on branch before integration.
- Duplicate type definitions across crates (`Permission`/
  `PermissionReq`, `Mode`/`Input`, `OntologySnapshot`) — unified by
  the integration agent in three checkpoint refactor commits.

Total landed: 91 tests passing on the four M1.5 crates
(24 + 36 + 22 + 9 including the new e2e test). Clippy clean.
`scripts/build.sh check` green. Native and wasm builds both intact.

### Commit trail

```
0e32e67 merge(m1.5): M1.5 complete — app layer + admin panel rendering from surface description
87e9591 test(m1.5): end-to-end admin-app render integration test
0fbc544 feat(m1.5): WeftOS Admin app — manifest + surface + Desktop Apps section
f5e40c3 refactor(m1.5): break surface -> gui-egui dep cycle; wasm-gate substrate
d98c8ff refactor(m1.5): unify Mode/Input + OntologySnapshot across surface/app/substrate
7a6d9c3 refactor(m1.5): unify Permission enum across clawft-app and clawft-substrate
d382745 Merge branch 'm15/kernel-adapter' into m15/integration
bb13cbc Merge branch 'm15/surface' into m15/integration
fb0ebfd Merge branch 'm15/app' into m15/integration
0c8858c fix(m1.5-substrate): log ring trim + watermark overflow safety + tracked subscriptions
55234e0 fix(m1.5-surface): parser RHS-loss on subtraction + count arity + scope docs
75e3373 feat(m1.5): clawft-surface crate — description IR + composer + binding eval
b397330 feat(m1.5): clawft-substrate crate + kernel adapter refactor
0540dea feat(m1.5): clawft-app crate — manifest parser + JSON registry
```

### Review-deferred follow-ups (track as M1.5-tail beads)

These were flagged by expert review and accepted as non-blockers:

- **clawft-app** — `UnknownMode` validation error variant is dead
  code (serde rejects out-of-set values at parse). Either wire a
  Rust-constructed-manifest check or delete the variant.
  Registry corruption recovery is currently "return JSON error to
  caller"; a quarantine/backup path is M1.6+ polish. `uninstall`
  while enabled doesn't yet run the ADR-015 §Lifecycle teardown
  (surfaces/subscriptions/affordances), because those hooks don't
  exist yet at the compositor level.
- **clawft-surface** — `.first`/`.last` as field access not supported
  (only as function calls). `sort(list, key)` ADR-016 §5 function not
  implemented. Scientific (`1e5`) / hex (`0xff`) number literals not
  accepted. User-defined compositions (`[compositions.*]`) not
  parsed. All documented in `lib.rs` header under "M1.5 scope
  reductions".
- **clawft-substrate** — `substrate/meta/adapter/<id>/health` topic
  not yet emitted (ADR-017 §7). Log event-driven ingest is still a
  periodic poll fallback because the daemon RPC doesn't expose a
  streaming log endpoint yet.
- **Cross-cutting** — variant_id stamping through real widgets + the
  honest governance-gated affordance intersection are both stubbed
  (identity mapping) in the composer; real wiring lands with M2's
  active-radar loop.

---

## ✅ M1.5.1 (α) — delivered 2026-04-21

Landed as 5 commits on `development-0.7.0`. The M1.5 architecture was
proven to render a surface description, but three unobvious gaps
meant the admin app felt non-functional when opened in the Cursor
extension:
(a) clawft-app panicked on wasm due to `std::time` use;
(b) affordances rendered but did nothing (composer discarded them);
(c) tray chips for Mesh/ExoChain/DeFi/WiFi/Bluetooth were stubbed,
    placeholder constants, or presence-only heuristics.
This slice closes all three end-to-end. The admin panel now kills
processes and restarts services through real RPC verbs, and the
tray reflects real system state via five adapters.

### Commit trail

```
22aed89 feat(m1.5.1d): mesh + chain adapters, drop DeFi vapor chip
1f19162 feat(m1.5.1c): BluetoothAdapter — host-local via /sys/class
01d141f feat(m1.5.1b): NetworkAdapter — WiFi/ethernet/battery via /sys/class
ee34748 feat(m1.5.1a): admin app affordances end-to-end + layout polish
ad33a0c fix(m1.5): wasm-safe SystemTime in clawft-app registry
```

### What shipped

- **fix(m1.5)** — `clawft-app::registry::AppRegistry::install()` now
  uses `web-time::SystemTime` instead of `std::time`. This was the
  original wasm panic ("time not implemented on this platform") that
  took down the extension webview on load.
- **M1.5.1a — admin app affordances end-to-end + layout polish**
  - Two new daemon RPC verbs: `kernel.kill-process(pid)` and
    `kernel.restart-service(name)`. Extension allowlist updated.
  - Composer return type widened to `ComposeOutcome { responses,
    dispatches }`. Table row-click + gauge action-button produce
    PendingDispatches; desktop shell submits them via
    `live.submit(Command::Raw)`.
  - `clawft-substrate::projection` — shared module projects
    `kernel.ps` rows with `name` and `cpu` aliases so the admin
    fixture's `[pid, name, cpu]` column binding resolves under both
    the native KernelAdapter and the wasm fallback.
    `explode_services_by_name` emits per-name sub-paths so the
    gauge's `substrate/kernel/services/<name>/cpu_percent` binding
    resolves.
  - StreamView gains `desired_width` + `wrap_lines` so the log pane
    stops overflowing narrow webviews. Grid children wrapped in
    Frame::group so the 2×2 admin layout reads as cards.
  - Offline banner at top of app pane when daemon unreachable.
- **M1.5.1b — NetworkAdapter** (new `clawft-substrate::network`) —
  reads `/sys/class/net/*` + `/sys/class/power_supply/*` directly.
  Emits `substrate/network/{wifi,ethernet,battery}`. No nmcli /
  NetworkManager / bluez dependency.
- **M1.5.1c — BluetoothAdapter** (new `clawft-substrate::bluetooth`)
  — reads `/sys/class/bluetooth` + `/sys/class/rfkill`. Emits
  `substrate/bluetooth`.
- **M1.5.1d — MeshAdapter + ChainAdapter** (new
  `clawft-substrate::{mesh,chain}`) — poll the existing
  `cluster.status` / `cluster.nodes` / `chain.status` RPC verbs.
  Emit `substrate/mesh/{status,nodes}` + `substrate/chain/status`.
  On daemons without the `exochain` feature, emits
  `{available: false}` so the tray shows grey instead of pretending.
  **DeFi chip removed** from the tray — survey confirmed no module,
  no service, no registration code exists anywhere in the workspace.

### Tray before → after

| Chip | Before M1.5.1α | After M1.5.1α |
|------|---------------|--------------|
| Kernel | ✅ real | ✅ real (unchanged) |
| Mesh | presence-only | MeshAdapter, live peer/shard counts |
| ExoChain | presence-only | ChainAdapter, live chain.status |
| DeFi | fake presence | **removed** (vapor) |
| Wi-Fi | hardcoded `On` | NetworkAdapter, /sys/class/net |
| Bluetooth | hardcoded `Off` | BluetoothAdapter, /sys/class/bluetooth |

### Test coverage

- `clawft-substrate`: 46/46 green (28 pre-existing + 9 network +
  5 bluetooth + 2 mesh + 2 chain). Adapter-level unit tests run
  against fake sysfs roots (tempfile-backed) so the adapters are
  testable on hosts without wireless/bluetooth/battery hardware.
- `clawft-app`: 24/24 green (registry tests exercise
  `SystemTime::now` path).
- `clawft-gui-egui`: all head-tests + admin_app_e2e +
  surface_headless_render green.
- `scripts/build.sh check` green on workspace.
- `cargo check --target wasm32-unknown-unknown --no-default-features`
  green.
- `tsc --noEmit` on the extension clean.
- Extension wasm bundle rebuilds clean.
- Clippy clean on all touched crates.

### Wasm / extension state

Native (`weft-gui-egui`) gets live tray + functional admin app.
**Wasm (Cursor extension webview)** gets functional admin app
(kernel verbs reach the daemon through the postMessage→UDS bridge)
but tray chips for Mesh/ExoChain/WiFi/Bluetooth render grey until
the substrate-over-postMessage bridge lands in M1.6+. This is
documented inside each adapter — the path is understood; the work
is scheduled alongside editor-in.

---

## 🚧 In flight / next up

Round 3 planning reshaped the milestone order. M1.5 + M1.5.1 are
done; M1.6 is next.

### M1.6 — IDE bridge editor-in
Adds `substrate/editor/*` topics (the `workspace` adapter from
ADR-017 §5). Wires the VSCode postMessage transport per ADR-018 §5.
Project-OS reference panel (session-10 §6.2 top row) becomes viable.

### M1.7 — IDE bridge editor-out
`ide.*` affordance catalog per ADR-018 §2. Install-time + per-
invocation governance prompts for destructive verbs. Project-OS panel
can reach back into the editor (open-file, jump-to-line, quick-pick).

### M1.8 — Touch adaptation
Compositor hit-target floor, gesture mapping, safe-area insets per
ADR-019 Part A. Deployment Monitor (session-10 §6.3 row 2) viable on
tablet. Default-detection table wired into host launch paths.

### M1.9 — Voice + avatar + narration
`foreign://avatar` Tier-B shell, `weft-capture` sidecar promoted from
M2, narration template parser, voice-triggered launcher, caption
`StreamView`. Avatar Companion panel (session-10 §6.4) viable.
ADR-006 amendment (Affordance `invocations` field) lands across all
21 canon primitives.

### M2 (revised) — Active-radar loop closed end-to-end
Unchanged in spirit but now instruments a real app layer:
- `ThreadDock` primitive with per-agent columns (R2's `MeshPanel`) —
  ships as an app-level composition over Canon primitives (Stack +
  Tree + StreamView), not a new primitive.
- `variant-id` + `substrate/ux/returns` wired through real widgets —
  topology / doppler / range / bearing capture per WSP §9.
- `Goal` aggregate (ADR-008) in `crates/clawft-kernel/src/goal.rs`
  with five invariants.
- `GovernanceGate::check` signature extended to read `goal_id` from
  the context JSON. No new backend — reuse existing.
- `TrajectoryRecorder` per-goal scoping + seal-as-pattern-promotion.

### Kernel & substrate wiring (unscheduled — runs in parallel)
- Add `MessagePayload::Wsp { frame: Vec<u8> }` variant to
  `crates/clawft-kernel/src/ipc.rs` (ADR-003).
- Implement WSP 17 verbs (ADR-005) in a `clawft-wsp` crate that
  binds `clawft-rpc` on UDS + the mesh envelope carrier.
- CDDL compile-time validation harness for the 21 primitives.

### Tier-B `ForeignSurface` stubs (M2+)
- `browser` (wry/viewport), `terminal` (egui_term), `video`
  (gstreamer+egui_wgpu), `xwindow` (XComposite), `app` (IPC viewport),
  `camera|mic|screen` (OS capture with the mandatory tray chip).
- `avatar` — lands in M1.9 per ADR-019 Part C.

### Perf quick wins *(ship anytime)*
- SSH compression + ControlMaster (config snippet in chat).
- Optional `WEFT_DEMO_FPS_CAP` env read in `weft-demo-lab` to clamp
  `ctx.request_repaint_after()` to e.g. 30 ms for X-over-WAN sessions.
- wasm bundle size optimise (release-lto + wasm-opt-from-source →
  ~1.3 MB gzipped). Current debug bundle: 5.0 MB.

### Canon crosswalk finding — queued for round-table (2026-11-06)
A real-hardware crosswalk (Infineon XENSIV sensor shield dashboard on
Arduino VENTUNO Q, built in Figma from the 21-item canon only) found
4 primitive gaps + 1 variant gap, written up as **ADR-020**
(`adrs/adr-020-sensor-telemetry-primitives.md`): `ui://radar-sensor`,
`ui://spectrogram`, `ui://attitude-3d`, `ui://timeline-scrubber`
(canon → 25, the foundations.md ceiling), plus a `ui://gauge`
circular-heading variant. Not implemented; not yet round-table
reviewed. ADR-020 also surfaces a pre-existing, unrelated drift: ADR-001
row 20 (`ui://foreign`) was never built in `clawft-canon`;
`ui://thread-dock` shipped in that slot instead, contrary to both
ADR-001's table and this file's own M2 section above ("ships as an
app-level composition ... not a new primitive"). Needs an owner
decision on ADR-020 and a separate reconciliation pass on the
thread-dock/foreign row — neither started here.

---

## 🛠️ Round 3 — open questions carried over

Aggregate of `## Open questions` sections across the Round-1 session
docs + `protocol-spec §18` + Round-3 ADR deferrals. Each belongs to a
Round-3+ session or ADR.

### App-layer questions opened by Session 10
- **App distribution.** Crates.io? Private registry? IPFS/content-
  addressed? Not urgent for M1.5 — local filesystem install only.
- **App sandboxing.** Rust apps have full process access; declarative
  apps are sandboxed by construction (read declared topics only). Do
  we need a middle tier? Deferred until first community app.
- **Localisation.** Narration rules in English only for M1.9. i18n is
  a Round 4 concern.
- **Multiple avatars.** Can a session host two avatars (technical +
  executive assistant)? Not designed in ADR-019; out of scope M1.9.
- **Ontology shape URIs.** ADR-017 uses `ontology://<x>` placeholders.
  Full ontology-shape spec is TBD in ADR-020+ (paired with the SHACL
  vs TopologySchema decision below).

### Cross-substrate identity (session-7, session-8, protocol-spec §18)
Rasmus unsolved #1 at the "multiple WeftOS substrates collaborating"
layer. ExoChain identity maps inside one substrate; cross-substrate
delegation is unspecified. Proposal: follow-up symposium track plus an
ADR.

### Observation back-pressure (protocol-spec §18, session-6)
What happens when a greedy consumer falls behind the `ux/returns`
stream? Drop-oldest vs coalesce vs apply per-channel quality
gradients. Needs a field experiment.

### Mission Console projection negotiation (session-3, session-9)
Rooms with heterogeneous renderer capabilities — browser-based
participant + native participant + TTY observer — need a per-
participant projection of the same surface. Spec stubs in WSP but
negotiation protocol is undefined.

### OT vs event-sourced for the composition spine (session-3)
R3 picked event-sourced spine + CRDT leaves; worth a follow-up
empirical validation on large Mission Console rooms.

### Variant reconciliation across participants (session-3, session-6)
When two participants render different GEPA variants of the same
primitive, what's the audit representation? How is "which variant
won" recovered?

### Offline / partition behaviour over our mesh (session-3)
A peer drops, rejoins with a divergent local history — we have the
chain primitives, but the replay story for Mission Console rooms is
sketched only.

### Permissions-as-affordances vs permissions-as-filters (session-3, session-8)
If the primitive already lists the affordances it offers, is
permission a gate on *which* affordances appear, or a later filter
when the user/agent tries to invoke one? The answer shapes the tray
chip surface. (Partially addressed by ADR-015 install-time consent +
ADR-018 per-invocation governance; full resolution deferred.)

### RVF / WSP relationship (protocol-spec §18)
Two wire protocols in the same substrate. Same CBOR shop, different
purposes. Do they share envelopes? A common CDDL root? An explicit
alignment ADR is owed.

### Ontology manifest vs lazy `ontology.describe` (session-4, session-6)
Session-4 proposed a full manifest per session; session-6 kept
`ontology.describe` as a per-IRI lookup. Not both — choose, write ADR.

### Voice grammar — AccessKit vs phrase-map (session-5)
Do we lean on AccessKit's auto-generated grammar, or define our own
phrase map per ontology type? ADR-019 §Part B picked canonical-name +
optional per-app `utterances` — AccessKit integration remains open.

### Ontology-sprawl policy (session-4)
When every primitive has an IRI, who prunes stale ones? Governance
role assignment needed.

### SHACL vs TopologySchema (session-4)
Both shape-constraint candidates for the geometry declarations.
Pilot each against our existing ontology-navigator schemas and
choose. ADR-017's shape-URI placeholders await this.

### CBOR-over-WebSocket perf (protocol-spec §18)
Browser renderers receive WSP as JSON-RPC. Native renderers receive
CBOR over mesh. Cost/benefit of also shipping CBOR over WebSocket
for high-rate observation.

### Cursor-specific integration points (session-7)
Cursor's own chat pane is closed but visible. R7 proposed reading it
as a `foreign://` signal. Concrete implementation path not yet
specified. ADR-018 specifies the *bridge* but not this specific
read-only capture.

### Confidence-visual uniformity (session-5, session-7)
Every primitive exposes confidence; every renderer visualises it
differently if unspecified. Decide one visual language (icon, halo,
opacity gradient, badge) and apply everywhere.

### Goal vs Task vs Milestone naming (session-8)
R8 landed on "Goal = Evans aggregate"; related concepts are currently
unnamed. Likely not an ADR until M2 forces the question.

---

## 📚 Repo landmarks for this workstream

| Thing | Path |
|-|-|
| Canon | `.planning/symposiums/compositional-ui/foundations.md` |
| Agenda | `.planning/symposiums/compositional-ui/AGENDA.md` |
| Research | `.planning/symposiums/compositional-ui/session-*.md` |
| Decisions | `.planning/symposiums/compositional-ui/adrs/adr-*.md` |
| Spec | `.planning/symposiums/compositional-ui/protocol-spec.md` |
| This file | `.planning/symposiums/compositional-ui/ROADMAP.md` |
| Main shell | `crates/clawft-gui-egui/` |
| Canon primitives | `crates/clawft-gui-egui/src/canon/` |
| Canon demos | `crates/clawft-gui-egui/src/canon_demos.rs` |
| Surface host glue | `crates/clawft-gui-egui/src/surface_host/` |
| Desktop compositor (Blocks + Canon + Apps sections) | `crates/clawft-gui-egui/src/shell/desktop.rs` |
| Demo lab bin | `crates/clawft-gui-egui/src/bin/demo_lab.rs` |
| Theming tokens | `crates/clawft-gui-egui/src/theming.rs` |
| M0 extension | `extensions/vscode-weft-panel/` |
| App manifest + registry | `crates/clawft-app/` |
| Surface description IR + composer | `crates/clawft-surface/` |
| Ontology adapter trait + `KernelAdapter` + `Substrate` | `crates/clawft-substrate/` |
| WeftOS Admin manifest fixture | `crates/clawft-app/fixtures/weftos-admin.toml` |
| WeftOS Admin desktop surface fixture | `crates/clawft-surface/fixtures/weftos-admin-desktop.toml` |
| End-to-end M1.5 acceptance test | `crates/clawft-gui-egui/tests/admin_app_e2e.rs` |

---

## Commit graph since the pivot

```
0e32e67 merge(m1.5): M1.5 complete — app layer + admin panel rendering from surface description
87e9591 test(m1.5): end-to-end admin-app render integration test
0fbc544 feat(m1.5): WeftOS Admin app — manifest + surface + Desktop Apps section
f5e40c3 refactor(m1.5): break surface -> gui-egui dep cycle; wasm-gate substrate
d98c8ff refactor(m1.5): unify Mode/Input + OntologySnapshot across surface/app/substrate
7a6d9c3 refactor(m1.5): unify Permission enum across clawft-app and clawft-substrate
d382745 Merge branch 'm15/kernel-adapter' into m15/integration
bb13cbc Merge branch 'm15/surface' into m15/integration
fb0ebfd Merge branch 'm15/app' into m15/integration
0c8858c fix(m1.5-substrate): log ring trim + watermark overflow safety + tracked subscriptions
55234e0 fix(m1.5-surface): parser RHS-loss on subtraction + count arity + scope docs
75e3373 feat(m1.5): clawft-surface crate — description IR + composer + binding eval
b397330 feat(m1.5): clawft-substrate crate + kernel adapter refactor
0540dea feat(m1.5): clawft-app crate — manifest parser + JSON registry
d3d214f plan(symposium): Round 3 — app layer session + 5 ADRs + roadmap refresh
7c0c523 feat(gui): canon demo lab — 20 primitive demos in the WeftOS panel
7ddc5ee merge(canon): foundation + retrofit + new primitives — full 21-item canon landed
6e2df4d feat(gui+ext): M1 wasm-compat polish — web-time, PNG logo, extension hotload
ab25a49 merge(canon): new primitives — 11 missing canon items (Field/…)
c937620 merge(canon): retrofit — 8 existing blocks wrapped in CanonWidget trait
54557f5 feat(canon): new primitives — Field/Toggle/Select/Slider/Grid/Dock/Sheet/Modal/Media/Canvas/Tabs
853b9a8 feat(canon): retrofit — wrap 7 existing blocks in CanonWidget trait
eae8c17 feat(canon): foundation — CanonWidget trait + CanonResponse + Pressable reference
894de99 feat(gui): M1 (part 2) — wasm bundle + VSCode extension hosts egui in Cursor
1204f7a feat(gui): M1 (part 1) — clawft-gui-egui compiles to wasm32-unknown-unknown
c0470d3 plan(symposium): canon reconciled — 19 Tier-A + 1 Tier-B (StreamView + Canvas + Modal modality)
7452ea3 plan(symposium): consolidated ROADMAP — done/in-flight/Round 3 open
fad8488 feat(gui): M0 — Cursor/VSCode extension scaffold for WeftOS panel
f826102 plan(symposium): Round 1 research complete — 9 findings docs, 32k words
```

---

## Handoff: what's live, what's next

**Running workspace state** (as of `0e32e67`):

- `weaver kernel start` brings up the daemon on `.weftos/runtime/
  kernel.sock`.
- `cargo run -p clawft-gui-egui --bin weft-gui-egui` launches the
  native shell in `desktop × pointer` mode.
- Opening the launcher → Blocks window now has three sections:
  **Blocks** (12 legacy demos), **Canon** (20 canon-primitive demos),
  **Apps** (WeftOS Admin reference, auto-installed).
- VSCode extension at `extensions/vscode-weft-panel/` loads the same
  panel as a webview, with wasm hot-reload on edits via the cargo-
  watch + build-wasm.sh loop documented in `extension.ts`.

**Immediate follow-up beads** (pick any, no blocking order):

1. **M1.6 editor-in adapter** — ship `crates/clawft-workspace-adapter`
   emitting `substrate/editor/{buffer,cursor,diagnostics,focus,
   terminal,tasks}` per ADR-018 §2. VSCode-side relay in the existing
   extension's `extension.ts`. Unlocks the Project OS reference
   panel (session-10 §6.2).
2. **M1.5 review follow-up beads** — the three deferred items from
   the "Review-deferred follow-ups" section above. None block M1.6;
   each is <30 LoC.
3. **ROADMAP hygiene** — this doc's markdownlint warnings on
   blanks-around-headings (pre-existing; cosmetic).
4. **Delete/keep decision on `BlockKind`** — session-10 rec. 13 asked
   for deletion once the admin app covers the demos. The Apps
   section is additive right now. A real decision is probably M1.7+
   when more apps exist.

**What M1.5 proves**: the reverse-DDD mapping from session-4 (aggregate
→ Surface, entity → Chip, value → Field) now has a working
implementation. A declarative TOML document describes a panel; the
composer reads it + a live ontology snapshot + renders canon
primitives. This is the architectural payoff the canon was always
aimed at. Everything from M1.6 onward is filling out specific
adapters and wiring specific apps.
