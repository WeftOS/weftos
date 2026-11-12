# GUI colour snapshot

Proves that a refactor of `clawft-gui-egui` (for example moving colour literals
into `theming::palette`) left every rendered colour unchanged.

## What it does

Each `colsnap_*` test renders one surface through egui at a fixed screen size
(1280x800) and a fixed clock, runs four frames, tessellates the last one, and
writes every vertex (position, uv, premultiplied sRGBA) and every text string
to `$GUI_SNAP_OUT/<surface>.txt`. Run the suite at two commits and compare the
two output directories: any change to a fill, stroke, glyph tint or image tint
shows up as a vertex whose colour differs.

This is a tessellation-level check, not a pixel render. It is stricter than
pixels for colour equality (nothing is blended, sampled or anti-aliased before
the comparison) and needs no GPU or new dependency. It does not check that
glyph shapes or texture contents look right.

Surfaces: every substrate viewer, the conversation graph, the boot splash, the
desktop grid, tray, sidebar and agent workspace, all `apps::*` screens, the
chat panel, both terminal renderers (alacritty and vt100) fed a fixed ANSI
stream, and a gallery of stock egui widgets under `theming::apply`.

Where the tests live: `tests/colour_snapshot.rs` (public API),
`src/snap_tests.rs` (apps, need crate-private `Desktop`), and `colsnap_*` tests
inside `explorer/chat.rs`, `explorer/terminal.rs` and
`explorer/terminal_vt100.rs` (private fields). They share `src/snap_support.rs`
and the `#[cfg(test)]` `Live::test_stub`, an offline `Live` with no transport.

## Running it

On a weftos-ci lane (not the Mac), once per commit:

    scripts/gui-colour-snapshot.sh /home/ci/snap-base      # at the old commit
    scripts/gui-colour-snapshot.sh /home/ci/snap-tip       # at the new commit
    scripts/gui-colour-compare.py /home/ci/snap-base /home/ci/snap-tip

The old commit does not contain the harness: copy the files named above (plus
the `snap_support`/`snap_tests` module lines in `lib.rs` and the `Live::test_stub`
block in `live.rs`) into a scratch checkout of it.

The compare script prints one line per surface: vertex counts, whether the full
dump is identical (`strict`), whether every colour is identical (`colours`),
the number of differing vertices and the largest per-channel difference. It
exits non-zero on any colour difference.

## Known run-to-run variance

Two surfaces contain text derived from time or a global counter, so they are
checked on their colour view only (positions and strings ignored):

- chat panel and chat app: conversation ids embed `now_ms()` and a counter;
- tray: wall-clock `HH:MM` text (glyph positions move, colours do not).

`viewer_health*` and `viewer_time_series` render once per process because the
sparkline history is process-global.

## Interaction states

`colsnap_state_*` tests drive a surface with scripted input events (a `Script` in
`snap_support.rs`: hover, press and hold, click, drag, key, text) over a fixed
number of frames at the same fixed clock and size, then dump the last frame.
Widget positions come from a probe render of the same surface, so scripts do not
hard-code layout. Each dump is rendered twice and must be identical. Files are
named `state_<surface>_<state>`.

| Surface | States |
| --- | --- |
| Widget gallery (`state_gal_*`, stock egui widgets) | hover, press, click for link, button, small button, disabled, checkbox, radio, selectable label, toggle, text edit, combo box, collapsing header, slider, drag value; Tab focus stops 1 to 14; Space and Enter on the focused button; text edit focus, typed text, select all, drag selection; slider drag; combo open, entry hover, press, pick; collapsing open and hover |
| Same gallery with the WeftOS visuals in effect (`state_galvis_*`) | the same set; see the note below |
| Patch editor (`state_edit_*`) | hover and press on nodes and toolbar buttons; select each node; deselect; selected node with the pointer on another; connect mode, connect source, hover on target, connect done, connect to self, back to select, selection then connect; add node, dirty, Reset hover, press and click; delete; node drag held and released; inspector label and kind (hover, focus, typed, select all), remove-edge hover, press and click; Tab stops 1 to 8; empty graph |
| Conversation graph, fixture (`state_cg_*`) | select each node; dim each edge kind and intent and all; level-of-detail collapse; clusters off; scrub pinned before, at, after the data and at the ends; full-history hint; ArrowLeft, ArrowRight and L keys; pointer hover, press and click (not wired to any colour today) |
| Conversation graph, every role x state x edge kind (`state_cgfull_*`) | plain with each of two topics, selected nodes, LOD collapse, clusters off, all edges dimmed, all intents dimmed, pinned |
| Services and scheduler apps (`state_app_*`) | filter tab hover, press, click; row action buttons hover, press, click; Tab stops; scheduler add button hover, press, click; add form open; form buttons hover, press; failed submit showing the error line; name, interval and command fields focus, typed text, select all; Tab stops |
| Chat panel (`state_chat_*`) | streaming turn in flight: thinking, partial plain, partial markdown, partial list, unclosed code fence, long, done, error; Send and Clear hover and press; draft focus, typed text, select all; Tab stops; click on Send |
| Alacritty terminal (`state_term_native_*`) | hover, focus click, selection in progress, selection after release, single-row and reversed selection, selection cleared by a click, copy with a selection. Each selection dump asserts the overlay colour is present. The vt100 terminal was already covered by `terminal_vt100` |

Notes:

- `theming::apply` installs the WeftOS `Visuals` and then overwrites them with
  egui's defaults, because `set_global_style` receives a `Style` that carries
  default visuals. The older dumps (`themed_widgets`, the apps) therefore show
  egui's stock widget colours for hover, press and focus. The `state_galvis_*`
  tests set `Tokens::visuals()` again (through the test-only `visuals_for_test`
  accessor) so the tints, and the tokens behind them, are exercised.
- The chat panel's heartbeat dot takes its green channel from the wall clock.
  In-flight chat dumps mask that colour (`rgba=#MASKED`) in both the check and
  the written file; every other colour is exact.
- Overlapping topic hulls in the conversation graph are drawn in `HashMap`
  order, which differs between runs. `state_cgfull_*` uses one topic per dump for
  that reason.

## Not covered

Surfaces that need a live daemon; pixel rendering and glyph shapes; the
heartbeat dot's green channel (masked); the settings app (it renders nothing
without published config), so its category list and detail form are covered only
through the gallery widgets they are built from; the vt100 terminal's focus and
keyboard states.
