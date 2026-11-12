//! Colour-snapshot harness support (test-only).
//!
//! Runs a surface through egui for a fixed number of frames at a fixed
//! screen size and clock, tessellates the last frame, and renders every
//! vertex (position, uv, premultiplied sRGBA colour) plus each mesh's
//! texture and clip rect to a stable text form. Two such dumps from two
//! commits can be diffed line by line: any colour change anywhere in a
//! surface (fill, stroke, glyph tint, image tint) shows up as a vertex
//! whose colour differs.
//!
//! This file is shared: `src/lib.rs` compiles it under `cfg(test)` (for
//! the in-crate surfaces that need private access) and
//! `tests/colour_snapshot.rs` includes it with `#[path]`. It therefore
//! depends on `egui` only. See `docs/development/gui-colour-snapshot.md`.

#![allow(dead_code)]

use std::fmt::Write as _;

/// Fixed logical screen size used by every surface.
pub const SCREEN: (f32, f32) = (1280.0, 800.0);

/// Frames run per surface. The last frame is the one dumped; the earlier
/// ones let egui's sizing/animation passes settle.
pub const FRAMES: usize = 4;

/// Clock step between frames, in seconds.
pub const FRAME_DT: f64 = 0.05;

/// Run `f` for [`FRAMES`] frames in a fresh root `Ui` on `ctx`, returning the
/// stable text dump of the final frame's tessellation.
///
/// `events` are `(frame index, event)` pairs injected into that frame (clicks,
/// drags, key presses).
pub fn render<F: FnMut(&mut egui::Ui)>(
    ctx: &egui::Context,
    events: Vec<(usize, egui::Event)>,
    mut f: F,
) -> String {
    let mut dump = String::new();
    for i in 0..FRAMES {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(SCREEN.0, SCREEN.1),
            )),
            time: Some(1.0 + i as f64 * FRAME_DT),
            predicted_dt: FRAME_DT as f32,
            events: events
                .iter()
                .filter(|(frame, _)| *frame == i)
                .map(|(_, e)| e.clone())
                .collect(),
            ..Default::default()
        };
        let out = ctx.run_ui(raw, |ui| f(ui));
        let ppp = out.pixels_per_point;
        let mut texts = String::new();
        for cs in &out.shapes {
            collect_texts(&cs.shape, &mut texts);
        }
        let prims = ctx.tessellate(out.shapes, ppp);
        if i == FRAMES - 1 {
            dump = format!("{texts}{}", dump_primitives(&prims));
        }
    }
    dump
}

/// One `T` line per text shape: position, fallback colour and the text itself.
/// Not needed for the colour comparison (glyph vertices carry the colours) but
/// it makes a diff readable and shows when a time-dependent string moved.
fn collect_texts(shape: &egui::Shape, out: &mut String) {
    match shape {
        egui::Shape::Vec(v) => {
            for s in v {
                collect_texts(s, out);
            }
        }
        egui::Shape::Text(t) => {
            let c = t.fallback_color;
            let _ = writeln!(
                out,
                "T pos=({:.2},{:.2}) fallback=#{:02x}{:02x}{:02x}{:02x} text={:?}",
                t.pos.x,
                t.pos.y,
                c.r(),
                c.g(),
                c.b(),
                c.a(),
                t.galley.text()
            );
        }
        _ => {}
    }
}

/// Stable text form of a tessellation: one header line per primitive and one
/// line per vertex. Positions and uvs are rounded to 1/100 so float noise
/// cannot hide or fake a difference; colours are exact.
pub fn dump_primitives(prims: &[egui::epaint::ClippedPrimitive]) -> String {
    let mut s = String::new();
    for (pi, p) in prims.iter().enumerate() {
        match &p.primitive {
            egui::epaint::Primitive::Mesh(m) => {
                let _ = writeln!(
                    s,
                    "P{pi} mesh tex={:?} clip=[{:.2},{:.2},{:.2},{:.2}] verts={} idx={}",
                    m.texture_id,
                    p.clip_rect.min.x,
                    p.clip_rect.min.y,
                    p.clip_rect.max.x,
                    p.clip_rect.max.y,
                    m.vertices.len(),
                    m.indices.len()
                );
                for (vi, v) in m.vertices.iter().enumerate() {
                    let c = v.color;
                    let _ = writeln!(
                        s,
                        "P{pi}.v{vi} pos=({:.2},{:.2}) uv=({:.3},{:.3}) rgba=#{:02x}{:02x}{:02x}{:02x}",
                        v.pos.x,
                        v.pos.y,
                        v.uv.x,
                        v.uv.y,
                        c.r(),
                        c.g(),
                        c.b(),
                        c.a()
                    );
                }
            }
            egui::epaint::Primitive::Callback(_) => {
                let _ = writeln!(s, "P{pi} callback");
            }
        }
    }
    s
}

/// Write `dump` to `$GUI_SNAP_OUT/<name>.txt` when the env var is set. Returns
/// the number of lines. When unset the dump is only sanity-checked.
pub fn emit(name: &str, dump: &str) -> usize {
    let lines = dump.lines().count();
    assert!(lines > 0, "surface {name} produced no primitives");
    if let Some(dir) = std::env::var_os("GUI_SNAP_OUT") {
        let dir = std::path::PathBuf::from(dir);
        std::fs::create_dir_all(&dir).expect("create GUI_SNAP_OUT");
        std::fs::write(dir.join(format!("{name}.txt")), dump).expect("write dump");
    }
    lines
}

/// Render a surface twice, each time on a fresh context with a fresh closure
/// from `mk`, and assert the dumps are identical, so a base-vs-tip diff cannot
/// be explained by nondeterminism. Returns the dump from the first run.
pub fn render_checked<F: FnMut(&mut egui::Ui)>(
    name: &str,
    setup: impl Fn(&egui::Context),
    events: Vec<(usize, egui::Event)>,
    mk: impl Fn() -> F,
) -> String {
    let c1 = egui::Context::default();
    setup(&c1);
    let a = render(&c1, events.clone(), mk());
    let c2 = egui::Context::default();
    setup(&c2);
    let b = render(&c2, events, mk());
    if a != b {
        emit(&format!("{name}.nondeterministic-a"), &a);
        emit(&format!("{name}.nondeterministic-b"), &b);
    }
    assert_eq!(a, b, "surface {name} is not deterministic across runs");
    emit(name, &a);
    a
}

/// Render once (for surfaces with process-global state that makes a second
/// in-process run differ by design, e.g. the time-series sample history).
pub fn render_once<F: FnMut(&mut egui::Ui)>(
    name: &str,
    setup: impl Fn(&egui::Context),
    events: Vec<(usize, egui::Event)>,
    f: F,
) -> String {
    let c = egui::Context::default();
    setup(&c);
    let a = render(&c, events, f);
    emit(name, &a);
    a
}

/// Pointer events that click at `click` (frame 0 press, frame 1 release, which
/// focuses a terminal) and then start dragging from `from` to `to`, leaving
/// the drag in progress on the final frame.
pub fn click_then_drag(
    click: egui::Pos2,
    from: egui::Pos2,
    to: egui::Pos2,
) -> Vec<(usize, egui::Event)> {
    let btn = |pos, pressed| egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: egui::Modifiers::default(),
    };
    vec![
        (0, egui::Event::PointerMoved(click)),
        (0, btn(click, true)),
        (1, btn(click, false)),
        (2, egui::Event::PointerMoved(from)),
        (2, btn(from, true)),
        (3, egui::Event::PointerMoved(to)),
    ]
}

/// Fixed ANSI test stream for the terminal renderers: SGR attributes, the 16
/// standard and bright foreground/background colours, 256-colour and RGB.
pub fn ansi_test_stream() -> Vec<u8> {
    let mut s = String::new();
    s.push_str(
        "\x1b[1mBOLD\x1b[0m \x1b[2mdim\x1b[0m \x1b[3mital\x1b[0m \x1b[4munder\x1b[0m \
         \x1b[7minv\x1b[0m \x1b[9mstrike\x1b[0m\r\n",
    );
    for i in (30..=37).chain(90..=97) {
        s.push_str(&format!("\x1b[{i}mFG{i}\x1b[0m "));
    }
    s.push_str("\r\n");
    for i in (40..=47).chain(100..=107) {
        s.push_str(&format!("\x1b[{i}m BG{i} \x1b[0m"));
    }
    s.push_str("\r\n");
    s.push_str(
        "\x1b[38;5;196mIDX196\x1b[0m \x1b[48;5;21m bg21 \x1b[0m \x1b[38;5;244mIDX244\x1b[0m\r\n",
    );
    s.push_str("\x1b[38;2;12;200;99mRGB\x1b[0m \x1b[48;2;200;10;60m rgb-bg \x1b[0m\r\n");
    s.push_str("\x1b[1;31;47mbold red on white\x1b[0m \x1b[7;32mrev green\x1b[0m\r\nprompt$ ");
    s.into_bytes()
}

/// Reduce a dump to colours only: text lines keep their fallback colour,
/// vertex lines keep index and colour, primitive headers are kept whole.
/// Positions, uvs and string contents are dropped, so a surface whose only
/// run-to-run variance is a time- or counter-derived string (conversation
/// ids, "Ns ago") compares equal when every colour is unchanged.
pub fn colour_view(dump: &str) -> String {
    let mut out = String::new();
    for line in dump.lines() {
        if line.starts_with("T ") {
            if let Some(i) = line.find("fallback=") {
                let _ = writeln!(out, "T {}", line[i..].split(' ').next().unwrap_or(""));
            }
        } else if let (Some(sp), Some(i)) = (line.find(' '), line.find("rgba=")) {
            let _ = writeln!(out, "{} {}", &line[..sp], &line[i..]);
        } else {
            let _ = writeln!(out, "{line}");
        }
    }
    out
}

/// Like [`render_checked`] but asserts only that the *colour view* of two runs
/// is identical. For surfaces with time/counter-derived text (conversation
/// ids and ages in the chat panel). The full dump is still written.
pub fn render_checked_colours<F: FnMut(&mut egui::Ui)>(
    name: &str,
    setup: impl Fn(&egui::Context),
    events: Vec<(usize, egui::Event)>,
    mk: impl Fn() -> F,
) -> String {
    let c1 = egui::Context::default();
    setup(&c1);
    let a = render(&c1, events.clone(), mk());
    let c2 = egui::Context::default();
    setup(&c2);
    let b = render(&c2, events, mk());
    assert_eq!(
        colour_view(&a),
        colour_view(&b),
        "surface {name}: colours differ between two runs"
    );
    emit(name, &a);
    a
}
