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
    f: F,
) -> String {
    render_frames(ctx, FRAMES, events, f)
}

/// [`render`] with an explicit frame count, for interaction scripts that need
/// more than [`FRAMES`] frames (hover, then press, then release, then settle).
pub fn render_frames<F: FnMut(&mut egui::Ui)>(
    ctx: &egui::Context,
    frames: usize,
    events: Vec<(usize, egui::Event)>,
    mut f: F,
) -> String {
    let mut dump = String::new();
    for i in 0..frames {
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
        if i == frames - 1 {
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

// ---------------------------------------------------------------------------
// Interaction states (hover / pressed / focus / selection)
// ---------------------------------------------------------------------------

/// Frames left after the last scripted event so animations (hover fades,
/// collapsing-header open) and one-frame-late focus have settled.
pub const SETTLE_FRAMES: usize = 4;

/// A scripted sequence of input events at fixed frames, built step by step.
///
/// Each step lands on the next frame (a click takes three), so a script is a
/// plain function of its calls and replays identically in every run. The fixed
/// clock and screen size of [`render_frames`] apply.
#[derive(Clone, Debug)]
pub struct Script {
    events: Vec<(usize, egui::Event)>,
    frame: usize,
}

impl Default for Script {
    fn default() -> Self {
        Self::new()
    }
}

fn button_event(pos: egui::Pos2, pressed: bool) -> egui::Event {
    egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: egui::Modifiers::default(),
    }
}

impl Script {
    /// Empty script; the first step lands on frame 1 so frame 0 is a plain
    /// layout pass.
    pub fn new() -> Self {
        Self {
            events: Vec::new(),
            frame: 1,
        }
    }

    fn at(&mut self, e: egui::Event) {
        self.events.push((self.frame, e));
    }

    /// Move the pointer to `pos` (hover).
    pub fn hover(mut self, pos: egui::Pos2) -> Self {
        self.at(egui::Event::PointerMoved(pos));
        self.frame += 1;
        self
    }

    /// Move to `pos` and press the primary button, leaving it held.
    pub fn press(mut self, pos: egui::Pos2) -> Self {
        self.at(egui::Event::PointerMoved(pos));
        self.frame += 1;
        self.at(button_event(pos, true));
        self.frame += 1;
        self
    }

    /// Move the pointer (button state unchanged), e.g. to drag.
    pub fn drag_to(mut self, pos: egui::Pos2) -> Self {
        self.at(egui::Event::PointerMoved(pos));
        self.frame += 1;
        self
    }

    /// Release the primary button at `pos`.
    pub fn release(mut self, pos: egui::Pos2) -> Self {
        self.at(button_event(pos, false));
        self.frame += 1;
        self
    }

    /// Full click: move, press, release.
    pub fn click(self, pos: egui::Pos2) -> Self {
        self.press(pos).release(pos)
    }

    /// Press and release a key (with `modifiers`) in one frame.
    pub fn key(mut self, key: egui::Key, modifiers: egui::Modifiers) -> Self {
        for pressed in [true, false] {
            self.at(egui::Event::Key {
                key,
                physical_key: Some(key),
                pressed,
                repeat: false,
                modifiers,
            });
        }
        self.frame += 1;
        self
    }

    /// Typed text.
    pub fn text(mut self, s: &str) -> Self {
        self.at(egui::Event::Text(s.to_owned()));
        self.frame += 1;
        self
    }

    /// Skip `n` frames.
    pub fn wait(mut self, n: usize) -> Self {
        self.frame += n;
        self
    }

    /// Total frames to run: every scripted frame plus [`SETTLE_FRAMES`].
    pub fn frames(&self) -> usize {
        self.frame + SETTLE_FRAMES
    }

    pub fn events(&self) -> Vec<(usize, egui::Event)> {
        self.events.clone()
    }
}

/// Render `mk()` once on a fresh context under `script` and return the dump
/// of the final frame (no determinism check, nothing written): used to locate
/// widgets for the next step of a script.
pub fn probe<F: FnMut(&mut egui::Ui)>(
    setup: impl Fn(&egui::Context),
    script: &Script,
    mk: impl Fn() -> F,
) -> String {
    let c = egui::Context::default();
    setup(&c);
    render_frames(&c, script.frames(), script.events(), mk())
}

/// [`render_checked`] for a [`Script`]: two fresh runs must be identical.
pub fn render_script_checked<F: FnMut(&mut egui::Ui)>(
    name: &str,
    setup: impl Fn(&egui::Context),
    script: &Script,
    mk: impl Fn() -> F,
) -> String {
    let c1 = egui::Context::default();
    setup(&c1);
    let a = render_frames(&c1, script.frames(), script.events(), mk());
    let c2 = egui::Context::default();
    setup(&c2);
    let b = render_frames(&c2, script.frames(), script.events(), mk());
    if a != b {
        emit(&format!("{name}.nondeterministic-a"), &a);
        emit(&format!("{name}.nondeterministic-b"), &b);
    }
    assert_eq!(a, b, "surface {name} is not deterministic across runs");
    emit(name, &a);
    a
}

/// [`render_script_checked`] comparing only the colour view, with colours for
/// which `mask` returns true replaced by a fixed token in both the comparison
/// and the written dump. For a surface whose only run-to-run variance is a
/// wall-clock-derived colour or text (the chat heartbeat dot).
pub fn render_script_masked<F: FnMut(&mut egui::Ui)>(
    name: &str,
    setup: impl Fn(&egui::Context),
    script: &Script,
    mk: impl Fn() -> F,
    mask: impl Fn(&str) -> bool,
) -> String {
    let c1 = egui::Context::default();
    setup(&c1);
    let a = mask_colours(
        &render_frames(&c1, script.frames(), script.events(), mk()),
        &mask,
    );
    let c2 = egui::Context::default();
    setup(&c2);
    let b = mask_colours(
        &render_frames(&c2, script.frames(), script.events(), mk()),
        &mask,
    );
    assert_eq!(
        colour_view(&a),
        colour_view(&b),
        "surface {name}: colours differ between two runs"
    );
    emit(name, &a);
    a
}

/// Replace every `rgba=#rrggbbaa` whose hex digits satisfy `mask` with
/// `rgba=#MASKED`. Panics if nothing matched, so a mask that stopped matching
/// cannot silently hide nothing.
pub fn mask_colours(dump: &str, mask: &dyn Fn(&str) -> bool) -> String {
    let mut out = String::with_capacity(dump.len());
    let mut hit = 0usize;
    for line in dump.lines() {
        if let Some(i) = line.find("rgba=#") {
            let hex = &line[i + 6..i + 14];
            if mask(hex) {
                out.push_str(&line[..i]);
                out.push_str("rgba=#MASKED\n");
                hit += 1;
                continue;
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    assert!(hit > 0, "mask matched no vertex");
    out
}

/// Top-left of the first text shape whose string contains `needle`.
pub fn text_origin(dump: &str, needle: &str) -> Option<egui::Pos2> {
    for line in dump.lines() {
        let Some(rest) = line.strip_prefix("T pos=(") else {
            continue;
        };
        let Some(i) = line.find(" text=") else {
            continue;
        };
        if !line[i..].contains(needle) {
            continue;
        }
        let (xy, _) = rest.split_once(')')?;
        let (x, y) = xy.split_once(',')?;
        return Some(egui::pos2(x.parse().ok()?, y.parse().ok()?));
    }
    None
}

/// A point just inside the first text shape containing `needle` (a few points
/// right of and below its origin), which lands inside the widget that owns it.
pub fn text_target(dump: &str, needle: &str) -> egui::Pos2 {
    text_origin(dump, needle).unwrap_or_else(|| {
        panic!(
            "text {needle:?} not found; texts present: {:?}",
            dump.lines()
                .filter(|l| l.starts_with("T pos="))
                .collect::<Vec<_>>()
        )
    }) + egui::vec2(4.0, 6.0)
}

/// Centre of a monospace label of `chars` characters at `char_w` points per
/// character whose galley starts at the text origin of `needle` (a painter
/// label centred on a node).
pub fn text_centre(dump: &str, needle: &str, chars: usize, char_w: f32) -> egui::Pos2 {
    let o = text_origin(dump, needle).unwrap_or_else(|| panic!("text {needle:?} not found"));
    o + egui::vec2(chars as f32 * char_w * 0.5, 6.0)
}

/// [`render_script_checked`] comparing only the colour view (see
/// [`render_checked_colours`]): for chat surfaces with counter-derived text.
pub fn render_script_colours<F: FnMut(&mut egui::Ui)>(
    name: &str,
    setup: impl Fn(&egui::Context),
    script: &Script,
    mk: impl Fn() -> F,
) -> String {
    let c1 = egui::Context::default();
    setup(&c1);
    let a = render_frames(&c1, script.frames(), script.events(), mk());
    let c2 = egui::Context::default();
    setup(&c2);
    let b = render_frames(&c2, script.frames(), script.events(), mk());
    assert_eq!(
        colour_view(&a),
        colour_view(&b),
        "surface {name}: colours differ between two runs"
    );
    emit(name, &a);
    a
}

// ---------------------------------------------------------------------------
// Widget gallery states (shared by the public-API and in-crate suites)
// ---------------------------------------------------------------------------

pub fn gallery_surface() -> impl FnMut(&mut egui::Ui) {
    let mut text = String::from("editable text");
    let mut on = true;
    let mut val = 0.375_f32;
    let mut drag = 77.0_f64;
    let mut sel = 1usize;
    let mut combo = 0usize;
    let mut flag = false;
    move |ui: &mut egui::Ui| {
        ui.heading("Heading");
        ui.label("label");
        ui.hyperlink_to("link", "https://example.invalid");
        ui.separator();
        let _ = ui.button("button");
        let _ = ui.small_button("small");
        let _ = ui.add_enabled(false, egui::Button::new("disabled"));
        ui.checkbox(&mut on, "checkbox");
        ui.radio_value(&mut sel, 1, "radio on");
        ui.radio_value(&mut sel, 2, "radio off");
        ui.horizontal(|ui| {
            ui.add(egui::Slider::new(&mut val, 0.0..=1.0));
            ui.add(egui::DragValue::new(&mut drag));
        });
        ui.text_edit_singleline(&mut text);
        let _ = ui.selectable_label(true, "selected");
        let _ = ui.selectable_label(false, "unselected");
        ui.toggle_value(&mut flag, "toggle");
        egui::ComboBox::from_label("combo")
            .selected_text(["alpha", "beta", "gamma"][combo])
            .show_ui(ui, |ui| {
                for (i, n) in ["alpha", "beta", "gamma"].iter().enumerate() {
                    ui.selectable_value(&mut combo, i, *n);
                }
            });
        ui.collapsing("collapsing", |ui| {
            ui.label("inside");
        });
        ui.add(egui::ProgressBar::new(0.6).text("60%"));
    }
}

pub fn gallery_pointer(prefix: &str, setup: &dyn Fn(&egui::Context)) {
    let gal = |name: &str, script: &Script| {
        render_script_checked(&format!("{prefix}_{name}"), setup, script, gallery_surface)
    };
    let base = probe(setup, &Script::new(), gallery_surface);
    // (name, point on the widget)
    let mut targets: Vec<(&str, egui::Pos2)> = [
        ("link", "link"),
        ("button", "button"),
        ("small", "small"),
        ("disabled", "disabled"),
        ("checkbox", "checkbox"),
        ("radio_off", "radio off"),
        ("unselected", "unselected"),
        ("selected", "selected"),
        ("toggle", "toggle"),
        ("textedit", "editable text"),
        ("combo", "alpha"),
        ("collapsing", "collapsing"),
    ]
    .iter()
    .map(|(n, needle)| (*n, text_target(&base, needle)))
    .collect();
    // Slider rail is left of its value box; the drag value is the "77" box.
    let value_box = text_origin(&base, "0.375").expect("slider value box");
    targets.push(("slider", value_box + egui::vec2(-50.0, 6.0)));
    targets.push(("dragvalue", text_target(&base, "77")));

    gal("plain", &Script::new());
    for (name, p) in &targets {
        gal(&format!("hover_{name}"), &Script::new().hover(*p));
        gal(&format!("press_{name}"), &Script::new().press(*p));
        gal(&format!("click_{name}"), &Script::new().click(*p));
    }
}

pub fn gallery_focus_keyboard(prefix: &str, setup: &dyn Fn(&egui::Context)) {
    let gal = |name: &str, script: &Script| {
        render_script_checked(&format!("{prefix}_{name}"), setup, script, gallery_surface)
    };
    // Tab through every focusable widget, one stop per dump.
    for n in 1..=14usize {
        let mut s = Script::new();
        for _ in 0..n {
            s = s.key(egui::Key::Tab, egui::Modifiers::NONE);
        }
        gal(&format!("tab_{n:02}"), &s);
    }
    // Tab to the button then activate it with Space and with Enter.
    let tab1 = Script::new().key(egui::Key::Tab, egui::Modifiers::NONE);
    gal(
        "tab_space",
        &tab1.clone().key(egui::Key::Space, egui::Modifiers::NONE),
    );
    gal(
        "tab_enter",
        &tab1.key(egui::Key::Enter, egui::Modifiers::NONE),
    );
}

pub fn gallery_text_edit(prefix: &str, setup: &dyn Fn(&egui::Context)) {
    let gal = |name: &str, script: &Script| {
        render_script_checked(&format!("{prefix}_{name}"), setup, script, gallery_surface)
    };
    let base = probe(setup, &Script::new(), gallery_surface);
    let te = text_target(&base, "editable text");
    let origin = text_origin(&base, "editable text").unwrap();
    gal("edit_focus", &Script::new().click(te));
    gal("edit_typed", &Script::new().click(te).text("XY"));
    gal(
        "edit_select_all",
        &Script::new()
            .click(te)
            .key(egui::Key::A, egui::Modifiers::COMMAND),
    );
    // Drag-select from the start of the text to its middle, button still held.
    gal(
        "edit_drag_select",
        &Script::new()
            .press(origin + egui::vec2(1.0, 6.0))
            .drag_to(origin + egui::vec2(40.0, 6.0))
            .drag_to(origin + egui::vec2(60.0, 6.0)),
    );
    gal(
        "edit_drag_select_released",
        &Script::new()
            .press(origin + egui::vec2(1.0, 6.0))
            .drag_to(origin + egui::vec2(40.0, 6.0))
            .release(origin + egui::vec2(40.0, 6.0)),
    );
}

pub fn gallery_slider_and_combo(prefix: &str, setup: &dyn Fn(&egui::Context)) {
    let gal = |name: &str, script: &Script| {
        render_script_checked(&format!("{prefix}_{name}"), setup, script, gallery_surface)
    };
    let base = probe(setup, &Script::new(), gallery_surface);
    let value_box = text_origin(&base, "0.375").expect("slider value box");
    let rail = value_box + egui::vec2(-50.0, 6.0);
    // Drag the slider handle along the rail, button held and then released.
    gal(
        "slider_drag",
        &Script::new()
            .press(rail)
            .drag_to(rail + egui::vec2(20.0, 0.0))
            .drag_to(rail + egui::vec2(30.0, 0.0)),
    );
    gal(
        "slider_drag_released",
        &Script::new()
            .press(rail)
            .drag_to(rail + egui::vec2(20.0, 0.0))
            .release(rail + egui::vec2(20.0, 0.0)),
    );
    // Open the combo box, then hover and click a popup entry.
    let combo = text_target(&base, "alpha");
    let opened = probe(setup, &Script::new().click(combo), gallery_surface);
    let beta = text_target(&opened, "beta");
    gal("combo_open", &Script::new().click(combo));
    gal(
        "combo_hover_entry",
        &Script::new().click(combo).wait(2).hover(beta),
    );
    gal(
        "combo_press_entry",
        &Script::new().click(combo).wait(2).press(beta),
    );
    gal(
        "combo_picked",
        &Script::new().click(combo).wait(2).click(beta),
    );
    // Collapsing header: open, then hover the header while open.
    let header = text_target(&base, "collapsing");
    gal(
        "collapsing_open_hover",
        &Script::new().click(header).wait(6).hover(header),
    );
}
