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

## Not covered

Hover, pressed and focus states of widgets other than the terminal click and
drag; selected and connect modes of the graph editor; the conversation graph
under interaction; a streaming chat turn; surfaces that need a live daemon.
