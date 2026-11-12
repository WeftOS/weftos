//! WEFT-264 — real WASM terminal renderer.
//!
//! `alacritty_terminal` does not compile for `wasm32-unknown-unknown`
//! (platform tty / polling crates). This module is the browser path:
//! a pure-Rust [`vt100::Parser`] grid model + the same daemon RPC
//! surface as the native panel (`terminal.spawn` / `substrate.read` /
//! `terminal.write` / `terminal.resize` / `terminal.close`).
//!
//! Compiled for:
//! - `target_arch = "wasm32"` as the production panel implementation
//! - host `cfg(test)` so the grid/RPC helpers are unit-tested without
//!   a wasm target (see `terminal.rs` include).
//!
//! Feature parity with the native `alacritty` path (best-effort):
//! - ANSI colors (16 + 256 + RGB), bold/italic/underline/inverse/dim
//! - Scrollback via `Screen::set_scrollback` + wheel handler
//! - Mouse selection + copy/paste
//! - Multi-tab [`TerminalPanel`] (`HashMap<SessionKey, Terminal>`)

// Host `cfg(test)` re-includes this module without calling the paint
// path; allow dead_code so unit tests don't fight unused-fn warnings
// that only fire on native-test builds (wasm uses every helper).
#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use std::collections::HashMap;
use std::sync::Arc;

use eframe::egui;
use serde_json::Value;

use crate::live::{self, Command, Live, ReplyRx};

/// How often to poll the output substrate path for new chunks.
const OUTPUT_POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// Default terminal cell metrics when font metrics aren't ready.
const FALLBACK_CELL_W: f32 = 8.0;
const FALLBACK_CELL_H: f32 = 16.0;

/// Scrollback line budget (matches native alacritty default / WEFT-262).
const SCROLLBACK_LINES: usize = 10_000;

/// Pixels-per-wheel-line scaling for scrollback.
const WHEEL_LINE_PX: f32 = 20.0;

/// Map key for a terminal tab.
///
/// Starts as a provisional `local-tab-…` id minted on
/// [`TerminalPanel::new_tab`]. After `terminal.spawn` returns, the
/// panel rekeys the entry to the daemon SessionId.
pub type SessionKey = String;

/// Inclusive selection range in screen-cell coordinates (visible grid
/// after scrollback offset is applied).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CellPos {
    row: u16,
    col: u16,
}

/// Per-tab terminal state — daemon RPC + vt100 grid.
pub struct Terminal {
    // ── RPC machinery (daemon-side PTY) ──────────────────────────
    session_id: Option<String>,
    output_path: Option<String>,
    shell: Option<String>,
    spawn_pending: Option<ReplyRx>,
    output_pending: Option<ReplyRx>,
    last_output_poll: Option<web_time::Instant>,
    last_seen_tick: u64,
    last_size: (u16, u16),
    last_error: Option<String>,

    // ── Terminal model (vt100 grid + vte parser) ─────────────────
    parser: vt100::Parser,
    /// Widget id allocated once per Terminal so input events route
    /// to the correct focus-tracked region.
    widget_id: Option<egui::Id>,
    instance_seq: u64,
    /// `true` while a primary-button mouse drag is in progress.
    dragging_selection: bool,
    /// Active selection (start, end) in visible-cell coords, or None.
    selection: Option<(CellPos, CellPos)>,
}

impl Default for Terminal {
    fn default() -> Self {
        Self {
            session_id: None,
            output_path: None,
            shell: None,
            spawn_pending: None,
            output_pending: None,
            last_output_poll: None,
            last_seen_tick: 0,
            last_size: (0, 0),
            last_error: None,
            parser: vt100::Parser::new(24, 80, SCROLLBACK_LINES),
            widget_id: None,
            instance_seq: next_instance_seq(),
            dragging_selection: false,
            selection: None,
        }
    }
}

impl Terminal {
    /// Daemon session id once `terminal.spawn` has resolved.
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// Shell path reported by the spawn reply, if any.
    pub fn shell(&self) -> Option<&str> {
        self.shell.as_deref()
    }

    /// Last spawn / decode error, if the session failed.
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// Drain RPCs, lazy-spawn, and poll output without painting.
    pub fn tick(&mut self, live: &Arc<Live>) {
        self.drain_spawn_reply();
        self.drain_output_reply();
        if self.session_id.is_none() && self.spawn_pending.is_none() && self.last_error.is_none() {
            self.fire_spawn(live);
        }
        self.maybe_poll_output(live);
    }

    /// Tick RPCs then paint the grid.
    pub fn paint(&mut self, ui: &mut egui::Ui, live: &Arc<Live>) {
        self.tick(live);
        self.paint_ui(ui, live);
    }

    /// Paint header + grid + input without draining RPCs.
    pub fn paint_ui(&mut self, ui: &mut egui::Ui, live: &Arc<Live>) {
        paint_header(ui, &self.shell, &self.session_id);

        if let Some(err) = &self.last_error {
            ui.colored_label(
                crate::theming::palette::TERM_ERROR_TEXT,
                format!("error: {err}"),
            );
            ui.separator();
        }

        let avail = ui.available_size_before_wrap();
        let (cell_w, cell_h) = cell_metrics(ui);
        let (rect, response) = ui.allocate_exact_size(avail, egui::Sense::click_and_drag());

        let widget_id = *self
            .widget_id
            .get_or_insert_with(|| egui::Id::new(("weft-terminal-vt100", self.instance_seq)));

        if response.clicked() {
            response.request_focus();
        }

        let painter = ui.painter_at(rect);

        let cols = ((rect.width() / cell_w).floor() as i32).clamp(20, 400) as u16;
        let rows = ((rect.height() / cell_h).floor() as i32).clamp(8, 200) as u16;
        if (rows, cols) != self.last_size {
            self.last_size = (rows, cols);
            self.parser.screen_mut().set_size(rows, cols);
            self.fire_resize(live, rows, cols);
        }

        // Global background.
        painter.rect_filled(rect, 0.0, named_color_bg());

        // Wheel-scroll into scrollback.
        let wheel_lines = if response.hovered() {
            let dy = ui.input(|i| i.smooth_scroll_delta.y);
            if dy.abs() >= 0.5 {
                (dy / WHEEL_LINE_PX).round() as i32
            } else {
                0
            }
        } else {
            0
        };
        if wheel_lines != 0 {
            let cur = self.parser.screen().scrollback() as i32;
            // Positive dy (scroll up) → increase scrollback offset.
            let next = (cur + wheel_lines).max(0) as usize;
            self.parser.screen_mut().set_scrollback(next);
        }

        self.handle_selection(ui, &response, rect, cell_w, cell_h);

        paint_grid(&painter, self.parser.screen(), rect, cell_w, cell_h);
        paint_selection(
            &painter,
            self.selection,
            self.parser.screen().size(),
            rect,
            cell_w,
            cell_h,
        );

        if response.has_focus() {
            let cursor_visible_blink = ui.input(|i| i.time).fract() < 0.55;
            if cursor_visible_blink && !self.parser.screen().hide_cursor() {
                paint_cursor(&painter, self.parser.screen(), rect, cell_w, cell_h, true);
            }
            self.handle_clipboard(ui, live);
            let bytes = collect_input_bytes(ui);
            if !bytes.is_empty() {
                self.fire_write(live, &bytes);
                // Typing returns viewport to bottom.
                self.parser.screen_mut().set_scrollback(0);
            }
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(120));
        } else if !self.parser.screen().hide_cursor() {
            paint_cursor(&painter, self.parser.screen(), rect, cell_w, cell_h, false);
        }

        let _ = widget_id;
    }

    pub fn close(&mut self, live: &Arc<Live>) {
        self.fire_close(live);
        self.output_pending = None;
        self.spawn_pending = None;
    }

    fn handle_selection(
        &mut self,
        ui: &egui::Ui,
        response: &egui::Response,
        rect: egui::Rect,
        cell_w: f32,
        cell_h: f32,
    ) {
        let (rows, cols) = self.parser.screen().size();
        if response.drag_started_by(egui::PointerButton::Primary)
            && let Some(pos) = ui.ctx().pointer_interact_pos()
            && let Some(cell) = pixel_to_cell(rect, cell_w, cell_h, rows, cols, pos)
        {
            self.selection = Some((cell, cell));
            self.dragging_selection = true;
        }
        if self.dragging_selection
            && response.dragged_by(egui::PointerButton::Primary)
            && let Some(pos) = ui.ctx().pointer_interact_pos()
            && let Some(cell) = pixel_to_cell(rect, cell_w, cell_h, rows, cols, pos)
            && let Some((start, _)) = self.selection
        {
            self.selection = Some((start, cell));
        }
        if self.dragging_selection && response.drag_stopped_by(egui::PointerButton::Primary) {
            self.dragging_selection = false;
            // Drop zero-width (single-point) selections so they don't
            // leave a sticky highlight after a bare click-drag that
            // never moved.
            if let Some((a, b)) = self.selection
                && a == b
            {
                self.selection = None;
            }
        }
        // Bare click (no drag) clears any existing selection.
        if response.clicked() && !self.dragging_selection && !response.dragged() {
            self.selection = None;
        }
    }

    fn handle_clipboard(&mut self, ui: &egui::Ui, live: &Arc<Live>) {
        let mut want_copy = false;
        let mut paste_text: Option<String> = None;
        ui.input(|i| {
            for ev in &i.events {
                match ev {
                    egui::Event::Copy | egui::Event::Cut => want_copy = true,
                    egui::Event::Paste(s) => paste_text = Some(s.clone()),
                    _ => {}
                }
            }
        });
        if want_copy
            && let Some(text) = self.selection_to_string()
            && !text.is_empty()
        {
            ui.ctx().output_mut(|o| {
                o.commands.push(egui::OutputCommand::CopyText(text));
            });
        }
        if let Some(text) = paste_text
            && !text.is_empty()
        {
            self.fire_write(live, text.as_bytes());
        }
    }

    /// Extract selected text from the visible screen (WEFT-260 parity).
    fn selection_to_string(&self) -> Option<String> {
        let (a, b) = self.selection?;
        let (start, end) = normalize_range(a, b);
        let text = self.parser.screen().contents_between(
            start.row,
            start.col,
            end.row,
            // contents_between end_col is exclusive in spirit of "up to";
            // vt100 docs: end_col is the last column *index* included via
            // write up to end_col — check: "followed by … end_col".
            // Looking at the source: for end row it writes `end_col` as width
            // starting at 0, so end_col is exclusive width. Pass end.col+1.
            end.col.saturating_add(1),
        );
        if text.is_empty() {
            None
        } else {
            Some(text)
        }
    }

    fn fire_spawn(&mut self, live: &Arc<Live>) {
        let (tx, rx) = live::reply_channel();
        self.spawn_pending = Some(rx);
        live.submit(Command::Raw {
            method: "terminal.spawn".into(),
            params: serde_json::json!({ "rows": 24, "cols": 80 }),
            reply: Some(tx),
        });
    }

    fn drain_spawn_reply(&mut self) {
        let Some(rx) = self.spawn_pending.as_mut() else {
            return;
        };
        match live::try_recv_reply(rx) {
            live::TryReply::Done(Ok(value)) => {
                self.spawn_pending = None;
                let session_id = value
                    .get("session_id")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let output_path = value
                    .get("output_path")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let shell = value
                    .get("shell")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                if session_id.is_none() || output_path.is_none() {
                    self.last_error =
                        Some("terminal.spawn reply missing session_id / output_path".into());
                    return;
                }
                self.session_id = session_id;
                self.output_path = output_path;
                self.shell = shell;
                self.last_error = None;
            }
            live::TryReply::Done(Err(err)) => {
                self.spawn_pending = None;
                self.last_error = Some(format!("spawn: {err}"));
            }
            live::TryReply::Closed => {
                self.spawn_pending = None;
                self.last_error = Some("spawn: transport closed".into());
            }
            live::TryReply::Empty => {}
        }
    }

    fn maybe_poll_output(&mut self, live: &Arc<Live>) {
        let Some(path) = self.output_path.clone() else {
            return;
        };
        if self.output_pending.is_some() {
            return;
        }
        let due = match self.last_output_poll {
            Some(t) => t.elapsed() >= OUTPUT_POLL,
            None => true,
        };
        if !due {
            return;
        }
        let (tx, rx) = live::reply_channel();
        self.output_pending = Some(rx);
        self.last_output_poll = Some(web_time::Instant::now());
        live.submit(Command::Raw {
            method: "substrate.read".into(),
            params: serde_json::json!({ "path": path }),
            reply: Some(tx),
        });
    }

    fn drain_output_reply(&mut self) {
        let Some(rx) = self.output_pending.as_mut() else {
            return;
        };
        match live::try_recv_reply(rx) {
            live::TryReply::Done(Ok(value)) => {
                self.output_pending = None;
                self.handle_output_value(value);
            }
            live::TryReply::Done(Err(_)) | live::TryReply::Closed => {
                self.output_pending = None;
            }
            live::TryReply::Empty => {}
        }
    }

    fn handle_output_value(&mut self, value: Value) {
        let tick = value.get("tick").and_then(Value::as_u64).unwrap_or(0);
        if tick != 0 && tick <= self.last_seen_tick {
            return;
        }
        let chunk = value.get("value").cloned().unwrap_or(Value::Null);
        let Some(obj) = chunk.as_object() else {
            return;
        };
        let data_b64 = obj.get("data").and_then(Value::as_str).unwrap_or("");
        let exit = obj.get("exit").and_then(Value::as_bool).unwrap_or(false);
        if !data_b64.is_empty() {
            use base64::Engine;
            match base64::engine::general_purpose::STANDARD.decode(data_b64.as_bytes()) {
                Ok(bytes) => {
                    self.parser.process(&bytes);
                }
                Err(e) => {
                    self.last_error = Some(format!("base64 decode: {e}"));
                }
            }
        }
        if exit {
            self.session_id = None;
            self.output_path = None;
        }
        self.last_seen_tick = tick;
    }

    fn fire_write(&mut self, live: &Arc<Live>, bytes: &[u8]) {
        let Some(id) = self.session_id.clone() else {
            return;
        };
        use base64::Engine;
        let data = base64::engine::general_purpose::STANDARD.encode(bytes);
        live.submit(Command::Raw {
            method: "terminal.write".into(),
            params: serde_json::json!({
                "session_id": id,
                "data": data,
            }),
            reply: Some(live::reply_channel().0),
        });
    }

    fn fire_resize(&mut self, live: &Arc<Live>, rows: u16, cols: u16) {
        let Some(id) = self.session_id.clone() else {
            return;
        };
        live.submit(Command::Raw {
            method: "terminal.resize".into(),
            params: serde_json::json!({
                "session_id": id,
                "rows": rows,
                "cols": cols,
            }),
            reply: Some(live::reply_channel().0),
        });
    }

    fn fire_close(&mut self, live: &Arc<Live>) {
        let Some(id) = self.session_id.take() else {
            return;
        };
        live.submit(Command::Raw {
            method: "terminal.close".into(),
            params: serde_json::json!({ "session_id": id }),
            reply: Some(live::reply_channel().0),
        });
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        // Explorer's `close()` drives `terminal.close` explicitly.
    }
}

// ── WEFT-263 multi-tab panel (wasm path) ────────────────────────────

/// Multi-tab terminal host — same API as the native panel.
pub struct TerminalPanel {
    sessions: HashMap<SessionKey, Terminal>,
    order: Vec<SessionKey>,
    active: SessionKey,
}

impl Default for TerminalPanel {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalPanel {
    /// Panel with a single empty (not-yet-spawned) tab selected.
    pub fn new() -> Self {
        let key = mint_local_tab_key();
        let mut sessions = HashMap::new();
        sessions.insert(key.clone(), Terminal::default());
        Self {
            sessions,
            order: vec![key.clone()],
            active: key,
        }
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    pub fn active_key(&self) -> &str {
        &self.active
    }

    pub fn order(&self) -> &[SessionKey] {
        &self.order
    }

    pub fn sessions(&self) -> &HashMap<SessionKey, Terminal> {
        &self.sessions
    }

    pub fn active_terminal(&self) -> &Terminal {
        self.sessions
            .get(&self.active)
            .expect("active key always present in sessions")
    }

    pub fn active_terminal_mut(&mut self) -> &mut Terminal {
        let key = self.active.clone();
        self.sessions
            .get_mut(&key)
            .expect("active key always present in sessions")
    }

    pub fn new_tab(&mut self) -> SessionKey {
        let key = mint_local_tab_key();
        self.sessions.insert(key.clone(), Terminal::default());
        self.order.push(key.clone());
        self.active = key.clone();
        key
    }

    pub fn select(&mut self, key: &str) {
        if self.sessions.contains_key(key) {
            self.active = key.to_string();
        }
    }

    pub fn close_tab(&mut self, live: &Arc<Live>, key: &str) {
        if !self.sessions.contains_key(key) {
            return;
        }
        let closed_idx = self.order.iter().position(|k| k == key);
        if let Some(mut term) = self.sessions.remove(key) {
            term.close(live);
        }
        self.order.retain(|k| k != key);

        if self.order.is_empty() {
            let fresh = mint_local_tab_key();
            self.sessions
                .insert(fresh.clone(), Terminal::default());
            self.order.push(fresh.clone());
            self.active = fresh;
            return;
        }

        if self.active == key {
            let next = closed_idx
                .and_then(|i| self.order.get(i).cloned())
                .or_else(|| {
                    closed_idx.and_then(|i| {
                        i.checked_sub(1)
                            .and_then(|j| self.order.get(j).cloned())
                    })
                })
                .or_else(|| self.order.last().cloned())
                .expect("order non-empty after close");
            self.active = next;
        }
    }

    pub fn close(&mut self, live: &Arc<Live>) {
        for term in self.sessions.values_mut() {
            term.close(live);
        }
        self.sessions.clear();
        self.order.clear();
        let key = mint_local_tab_key();
        self.sessions.insert(key.clone(), Terminal::default());
        self.order.push(key.clone());
        self.active = key;
    }

    pub fn paint(&mut self, ui: &mut egui::Ui, live: &Arc<Live>) {
        for term in self.sessions.values_mut() {
            term.tick(live);
        }
        self.rekey_spawned();

        paint_tab_strip(ui, self, live);

        if !self.sessions.contains_key(&self.active) {
            if let Some(first) = self.order.first().cloned() {
                self.active = first;
            } else {
                let _ = self.new_tab();
            }
        }

        let active = self.active.clone();
        if let Some(term) = self.sessions.get_mut(&active) {
            term.paint_ui(ui, live);
        }
    }

    fn rekey_spawned(&mut self) {
        let mut renames: Vec<(String, String)> = Vec::new();
        for key in &self.order {
            if let Some(term) = self.sessions.get(key)
                && let Some(sid) = term.session_id()
                && sid != key.as_str()
                && !self.sessions.contains_key(sid)
            {
                renames.push((key.clone(), sid.to_string()));
            }
        }
        for (old, new) in renames {
            if let Some(term) = self.sessions.remove(&old) {
                self.sessions.insert(new.clone(), term);
            }
            if let Some(slot) = self.order.iter_mut().find(|k| *k == &old) {
                *slot = new.clone();
            }
            if self.active == old {
                self.active = new;
            }
        }
    }
}

fn mint_local_tab_key() -> SessionKey {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(1);
    let n = N.fetch_add(1, Ordering::Relaxed);
    format!("local-tab-{n:04}")
}

fn paint_tab_strip(ui: &mut egui::Ui, panel: &mut TerminalPanel, live: &Arc<Live>) {
    let order = panel.order.clone();
    let active = panel.active.clone();
    let mut select_key: Option<String> = None;
    let mut close_key: Option<String> = None;
    let mut want_new = false;

    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        for key in &order {
            let selected = key == &active;
            let label = tab_label(panel.sessions.get(key));
            let fill = if selected {
                crate::theming::palette::ROW_SELECTED_FILL
            } else {
                crate::theming::palette::ROW_IDLE_FILL
            };
            let stroke = if selected {
                egui::Stroke::new(1.0, crate::theming::palette::ROW_SELECTED_STROKE)
            } else {
                egui::Stroke::new(1.0, crate::theming::palette::ROW_IDLE_STROKE)
            };
            let frame = egui::Frame::new()
                .fill(fill)
                .stroke(stroke)
                .inner_margin(egui::Margin::symmetric(8, 3))
                .corner_radius(4.0);

            frame.show(ui, |ui| {
                ui.horizontal(|ui| {
                    let tab_resp = ui
                        .add(
                            egui::Label::new(
                                egui::RichText::new(&label)
                                    .small()
                                    .monospace()
                                    .strong()
                                    .color(if selected {
                                        crate::theming::palette::ROW_SELECTED_TEXT
                                    } else {
                                        crate::theming::palette::ROW_IDLE_TEXT
                                    }),
                            )
                            .sense(egui::Sense::click()),
                        )
                        .on_hover_text(key);
                    if tab_resp.clicked() {
                        select_key = Some(key.clone());
                    }
                    let close_resp = ui
                        .add(
                            egui::Label::new(
                                egui::RichText::new("×")
                                    .small()
                                    .color(crate::theming::palette::TERM_EXIT_TEXT),
                            )
                            .sense(egui::Sense::click()),
                        )
                        .on_hover_text("Close tab");
                    if close_resp.clicked() {
                        close_key = Some(key.clone());
                    }
                });
            });
        }

        let new_resp = ui
            .add(
                egui::Button::new(
                    egui::RichText::new("+")
                        .small()
                        .strong()
                        .color(crate::theming::palette::TERM_OK_HINT),
                )
                .frame(false),
            )
            .on_hover_text("New terminal tab");
        if new_resp.clicked() {
            want_new = true;
        }
    });
    ui.separator();

    if let Some(key) = select_key {
        panel.select(&key);
    }
    if let Some(key) = close_key {
        panel.close_tab(live, &key);
    }
    if want_new {
        panel.new_tab();
    }
}

fn tab_label(term: Option<&Terminal>) -> String {
    let Some(term) = term else {
        return "terminal".into();
    };
    if let Some(shell) = term.shell() {
        let base = shell.rsplit('/').next().unwrap_or(shell);
        if let Some(sid) = term.session_id() {
            let short = short_id(sid);
            return format!("{base} · {short}");
        }
        return base.to_string();
    }
    if let Some(sid) = term.session_id() {
        return short_id(sid);
    }
    if term.last_error().is_some() {
        return "error".into();
    }
    "starting…".into()
}

fn short_id(id: &str) -> String {
    if id.len() > 14 {
        format!("{}…", &id[..12])
    } else {
        id.to_owned()
    }
}

fn next_instance_seq() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(1);
    N.fetch_add(1, Ordering::Relaxed)
}

fn cell_metrics(ui: &egui::Ui) -> (f32, f32) {
    let style = ui.style().clone();
    let font_id = egui::TextStyle::Monospace.resolve(&style);
    let cell_w = ui.ctx().fonts_mut(|f| f.glyph_width(&font_id, 'M'));
    let cell_h = ui.text_style_height(&egui::TextStyle::Monospace);
    let cell_w = if cell_w > 0.0 {
        cell_w
    } else {
        FALLBACK_CELL_W
    };
    let cell_h = if cell_h > 0.0 {
        cell_h
    } else {
        FALLBACK_CELL_H
    };
    (cell_w, cell_h)
}

fn paint_grid(
    painter: &egui::Painter,
    screen: &vt100::Screen,
    rect: egui::Rect,
    cell_w: f32,
    cell_h: f32,
) {
    let (rows, cols) = screen.size();
    let global_bg = named_color_bg();
    let font_id = egui::FontId::new(cell_h * 0.85, egui::FontFamily::Monospace);

    for row in 0..rows {
        for col in 0..cols {
            let Some(cell) = screen.cell(row, col) else {
                continue;
            };
            if cell.is_wide_continuation() {
                continue;
            }
            let x = rect.min.x + f32::from(col) * cell_w;
            let y = rect.min.y + f32::from(row) * cell_h;

            let mut fg = color_for_fg(cell.fgcolor());
            let mut bg = color_for_bg(cell.bgcolor());
            if cell.inverse() {
                std::mem::swap(&mut fg, &mut bg);
            }
            if cell.dim() {
                fg = fg.linear_multiply(0.7);
            }

            let cell_width = if cell.is_wide() {
                cell_w * 2.0
            } else {
                cell_w
            };

            if bg != global_bg {
                let bg_rect = egui::Rect::from_min_size(
                    egui::pos2(x, y),
                    egui::vec2(cell_width + 0.5, cell_h + 0.5),
                );
                painter.rect_filled(bg_rect, 0.0, bg);
            }

            let contents = cell.contents();
            if !contents.is_empty() && contents != " " {
                // Paint first non-space char (combining chars rare in TUI).
                if let Some(c) = contents.chars().next()
                    && c != ' ' && c != '\0'
                {
                    paint_glyph(
                        painter,
                        egui::pos2(x + cell_width * 0.5, y + cell_h * 0.5),
                        c,
                        &font_id,
                        fg,
                        cell.bold(),
                        cell.italic(),
                    );
                }
            }

            if cell.underline() {
                let yu = y + cell_h - 1.5;
                painter.line_segment(
                    [egui::pos2(x, yu), egui::pos2(x + cell_width, yu)],
                    egui::Stroke::new(1.0, fg),
                );
            }
        }
    }
}

/// Synthetic bold / italic (WEFT-261 parity).
fn paint_glyph(
    painter: &egui::Painter,
    center: egui::Pos2,
    c: char,
    font_id: &egui::FontId,
    color: egui::Color32,
    bold: bool,
    italic: bool,
) {
    const ITALIC_ANGLE: f32 = 0.165;

    if !italic {
        painter.text(
            center,
            egui::Align2::CENTER_CENTER,
            c,
            font_id.clone(),
            color,
        );
        if bold {
            painter.text(
                egui::pos2(center.x + 0.5, center.y),
                egui::Align2::CENTER_CENTER,
                c,
                font_id.clone(),
                color,
            );
        }
        return;
    }

    let galley = painter.layout_no_wrap(c.to_string(), font_id.clone(), color);
    let pos = center - galley.size() * 0.5;
    let mut shape = egui::epaint::TextShape::new(pos, galley, color)
        .with_angle_and_anchor(ITALIC_ANGLE, egui::Align2::CENTER_CENTER);
    painter.add(shape.clone());
    if bold {
        shape.pos.x += 0.5;
        painter.add(shape);
    }
}

fn paint_selection(
    painter: &egui::Painter,
    selection: Option<(CellPos, CellPos)>,
    size: (u16, u16),
    rect: egui::Rect,
    cell_w: f32,
    cell_h: f32,
) {
    let Some((a, b)) = selection else {
        return;
    };
    let (start, end) = normalize_range(a, b);
    let (rows, cols) = size;
    let overlay = crate::theming::palette::TERM_SELECTION_OVERLAY;

    for row in start.row..=end.row {
        if row >= rows {
            break;
        }
        let (col_lo, col_hi) = if start.row == end.row {
            (start.col, end.col.saturating_add(1))
        } else if row == start.row {
            (start.col, cols)
        } else if row == end.row {
            (0, end.col.saturating_add(1))
        } else {
            (0, cols)
        };
        let col_hi = col_hi.min(cols);
        if col_hi <= col_lo {
            continue;
        }
        let x0 = rect.min.x + f32::from(col_lo) * cell_w;
        let x1 = rect.min.x + f32::from(col_hi) * cell_w;
        let y0 = rect.min.y + f32::from(row) * cell_h;
        let y1 = y0 + cell_h;
        painter.rect_filled(
            egui::Rect::from_min_max(egui::pos2(x0, y0), egui::pos2(x1, y1)),
            0.0,
            overlay,
        );
    }
}

fn pixel_to_cell(
    rect: egui::Rect,
    cell_w: f32,
    cell_h: f32,
    rows: u16,
    cols: u16,
    pos: egui::Pos2,
) -> Option<CellPos> {
    if !rect.contains(pos) {
        return None;
    }
    let local_x = (pos.x - rect.min.x).max(0.0);
    let local_y = (pos.y - rect.min.y).max(0.0);
    let col = (local_x / cell_w).floor() as i32;
    let row = (local_y / cell_h).floor() as i32;
    if row < 0 || col < 0 || row as u16 >= rows || col as u16 >= cols {
        return None;
    }
    Some(CellPos {
        row: row as u16,
        col: col as u16,
    })
}

fn normalize_range(a: CellPos, b: CellPos) -> (CellPos, CellPos) {
    if (a.row, a.col) <= (b.row, b.col) {
        (a, b)
    } else {
        (b, a)
    }
}

fn paint_cursor(
    painter: &egui::Painter,
    screen: &vt100::Screen,
    rect: egui::Rect,
    cell_w: f32,
    cell_h: f32,
    filled: bool,
) {
    // cursor_position is absolute (not scrollback-relative). When the
    // user is scrolled into history the cursor may be off-viewport —
    // only paint when scrollback is 0 (bottom).
    if screen.scrollback() != 0 {
        return;
    }
    let (row, col) = screen.cursor_position();
    let (rows, cols) = screen.size();
    if row >= rows || col >= cols {
        return;
    }
    let x = rect.min.x + f32::from(col) * cell_w;
    let y = rect.min.y + f32::from(row) * cell_h;
    let r = egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(cell_w, cell_h));
    let color = named_color_fg().linear_multiply(if filled { 0.65 } else { 0.45 });
    if filled {
        painter.rect_filled(r, 0.0, color);
    } else {
        painter.rect_stroke(
            r,
            0.0,
            egui::Stroke::new(1.0, color),
            egui::StrokeKind::Inside,
        );
    }
}

fn paint_header(ui: &mut egui::Ui, shell: &Option<String>, session_id: &Option<String>) {
    ui.horizontal(|ui| {
        ui.heading("Terminal");
        ui.separator();
        match (session_id, shell) {
            (Some(id), Some(shell)) => {
                ui.label(
                    egui::RichText::new(format!("{shell}  ·  {id}"))
                        .monospace()
                        .small()
                        .color(crate::theming::palette::TERM_LABEL),
                );
            }
            (Some(id), None) => {
                ui.label(
                    egui::RichText::new(id)
                        .monospace()
                        .small()
                        .color(crate::theming::palette::TERM_LABEL),
                );
            }
            (None, _) => {
                ui.label(
                    egui::RichText::new("starting…")
                        .italics()
                        .small()
                        .color(crate::theming::palette::TEXT_DISCLOSURE),
                );
            }
        }
    });
    ui.separator();
}

fn collect_input_bytes(ui: &egui::Ui) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    ui.input(|i| {
        for ev in &i.events {
            match ev {
                egui::Event::Text(s) => out.extend_from_slice(s.as_bytes()),
                egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } => {
                    if let Some(bytes) = key_to_bytes(*key, *modifiers) {
                        out.extend_from_slice(&bytes);
                    }
                }
                _ => {}
            }
        }
    });
    out
}

fn key_to_bytes(key: egui::Key, modifiers: egui::Modifiers) -> Option<Vec<u8>> {
    if modifiers.ctrl
        && !modifiers.alt
        && !modifiers.shift
        && let Some(letter) = ctrl_letter(key)
    {
        return Some(vec![letter & 0x1f]);
    }
    let bytes: &[u8] = match key {
        egui::Key::Enter => b"\r",
        egui::Key::Tab => b"\t",
        egui::Key::Backspace => b"\x7f",
        egui::Key::Escape => b"\x1b",
        egui::Key::ArrowUp => b"\x1b[A",
        egui::Key::ArrowDown => b"\x1b[B",
        egui::Key::ArrowRight => b"\x1b[C",
        egui::Key::ArrowLeft => b"\x1b[D",
        egui::Key::Home => b"\x1b[H",
        egui::Key::End => b"\x1b[F",
        egui::Key::PageUp => b"\x1b[5~",
        egui::Key::PageDown => b"\x1b[6~",
        egui::Key::Delete => b"\x1b[3~",
        egui::Key::Insert => b"\x1b[2~",
        egui::Key::F1 => b"\x1bOP",
        egui::Key::F2 => b"\x1bOQ",
        egui::Key::F3 => b"\x1bOR",
        egui::Key::F4 => b"\x1bOS",
        egui::Key::F5 => b"\x1b[15~",
        egui::Key::F6 => b"\x1b[17~",
        egui::Key::F7 => b"\x1b[18~",
        egui::Key::F8 => b"\x1b[19~",
        egui::Key::F9 => b"\x1b[20~",
        egui::Key::F10 => b"\x1b[21~",
        egui::Key::F11 => b"\x1b[23~",
        egui::Key::F12 => b"\x1b[24~",
        _ => return None,
    };
    Some(bytes.to_vec())
}

fn ctrl_letter(key: egui::Key) -> Option<u8> {
    match key {
        egui::Key::A => Some(b'a'),
        egui::Key::B => Some(b'b'),
        egui::Key::C => Some(b'c'),
        egui::Key::D => Some(b'd'),
        egui::Key::E => Some(b'e'),
        egui::Key::F => Some(b'f'),
        egui::Key::G => Some(b'g'),
        egui::Key::H => Some(b'h'),
        egui::Key::I => Some(b'i'),
        egui::Key::J => Some(b'j'),
        egui::Key::K => Some(b'k'),
        egui::Key::L => Some(b'l'),
        egui::Key::M => Some(b'm'),
        egui::Key::N => Some(b'n'),
        egui::Key::O => Some(b'o'),
        egui::Key::P => Some(b'p'),
        egui::Key::Q => Some(b'q'),
        egui::Key::R => Some(b'r'),
        egui::Key::S => Some(b's'),
        egui::Key::T => Some(b't'),
        egui::Key::U => Some(b'u'),
        egui::Key::V => Some(b'v'),
        egui::Key::W => Some(b'w'),
        egui::Key::X => Some(b'x'),
        egui::Key::Y => Some(b'y'),
        egui::Key::Z => Some(b'z'),
        _ => None,
    }
}

fn color_for_fg(c: vt100::Color) -> egui::Color32 {
    match c {
        vt100::Color::Default => named_color_fg(),
        vt100::Color::Idx(i) => indexed_color(i),
        vt100::Color::Rgb(r, g, b) => crate::theming::palette::data_rgb(r, g, b),
    }
}

fn color_for_bg(c: vt100::Color) -> egui::Color32 {
    match c {
        vt100::Color::Default => named_color_bg(),
        vt100::Color::Idx(i) => indexed_color(i),
        vt100::Color::Rgb(r, g, b) => crate::theming::palette::data_rgb(r, g, b),
    }
}

fn named_color_fg() -> egui::Color32 {
    crate::theming::ansi::DEFAULT_FG
}

fn named_color_bg() -> egui::Color32 {
    crate::theming::ansi::DEFAULT_BG
}

fn indexed_color(i: u8) -> egui::Color32 {
    crate::theming::ansi::indexed(i)
}

// ── Unit tests (host + wasm) ────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn handle_output_value_advances_parser() {
        let mut t = Terminal::default();
        // base64("hi\n") = "aGkK"
        let chunk = json!({
            "tick": 1,
            "value": { "data": "aGkK", "ts_ms": 0 },
        });
        t.handle_output_value(chunk);
        let cell0 = t.parser.screen().cell(0, 0).expect("cell 0,0");
        let cell1 = t.parser.screen().cell(0, 1).expect("cell 0,1");
        assert_eq!(cell0.contents(), "h");
        assert_eq!(cell1.contents(), "i");
        assert_eq!(t.last_seen_tick, 1);
    }

    #[test]
    fn handle_output_value_dedupes_on_same_tick() {
        let mut t = Terminal::default();
        // base64("a") = "YQ=="
        let chunk = json!({
            "tick": 5,
            "value": { "data": "YQ==", "ts_ms": 0 },
        });
        t.handle_output_value(chunk.clone());
        t.handle_output_value(chunk);
        let cell0 = t.parser.screen().cell(0, 0).expect("cell 0,0");
        let cell1 = t.parser.screen().cell(0, 1).expect("cell 0,1");
        assert_eq!(cell0.contents(), "a");
        // If double-applied, col 1 would also be 'a'.
        assert!(cell1.contents().is_empty() || cell1.contents() == " ");
    }

    #[test]
    fn handle_output_value_marks_exit() {
        let mut t = Terminal::default();
        t.session_id = Some("t-deadbeef0001".into());
        t.output_path = Some("substrate/x/derived/terminal/t-deadbeef0001".into());
        let chunk = json!({
            "tick": 3,
            "value": { "data": "", "ts_ms": 0, "exit": true },
        });
        t.handle_output_value(chunk);
        assert!(t.session_id.is_none());
        assert!(t.output_path.is_none());
    }

    #[test]
    fn handle_output_value_ansi_color_red() {
        let mut t = Terminal::default();
        // ESC[31mR ESC[m — red 'R'
        let bytes = b"\x1b[31mR\x1b[m";
        use base64::Engine;
        let data = base64::engine::general_purpose::STANDARD.encode(bytes);
        t.handle_output_value(json!({
            "tick": 1,
            "value": { "data": data, "ts_ms": 0 },
        }));
        let cell = t.parser.screen().cell(0, 0).expect("cell");
        assert_eq!(cell.contents(), "R");
        assert_eq!(cell.fgcolor(), vt100::Color::Idx(1));
    }

    #[test]
    fn key_to_bytes_arrows_and_specials() {
        let m = egui::Modifiers::default();
        assert_eq!(key_to_bytes(egui::Key::Enter, m).unwrap(), b"\r");
        assert_eq!(key_to_bytes(egui::Key::Backspace, m).unwrap(), b"\x7f");
        assert_eq!(key_to_bytes(egui::Key::Tab, m).unwrap(), b"\t");
        assert_eq!(key_to_bytes(egui::Key::Escape, m).unwrap(), b"\x1b");
        assert_eq!(key_to_bytes(egui::Key::ArrowUp, m).unwrap(), b"\x1b[A");
        assert_eq!(key_to_bytes(egui::Key::ArrowDown, m).unwrap(), b"\x1b[B");
        assert_eq!(key_to_bytes(egui::Key::ArrowLeft, m).unwrap(), b"\x1b[D");
        assert_eq!(key_to_bytes(egui::Key::ArrowRight, m).unwrap(), b"\x1b[C");
    }

    #[test]
    fn key_to_bytes_ctrl_letter_emits_control_byte() {
        let ctrl = egui::Modifiers {
            ctrl: true,
            ..Default::default()
        };
        assert_eq!(key_to_bytes(egui::Key::C, ctrl).unwrap(), vec![0x03]);
        assert_eq!(key_to_bytes(egui::Key::D, ctrl).unwrap(), vec![0x04]);
        assert_eq!(key_to_bytes(egui::Key::U, ctrl).unwrap(), vec![0x15]);
    }

    #[test]
    fn scrollback_wheel_moves_offset() {
        let mut t = Terminal::default();
        // Feed enough lines to populate scrollback.
        let mut bytes = Vec::new();
        for i in 0..50 {
            bytes.extend_from_slice(format!("line {i}\r\n").as_bytes());
        }
        t.parser.process(&bytes);
        assert_eq!(t.parser.screen().scrollback(), 0);
        t.parser.screen_mut().set_scrollback(5);
        assert_eq!(t.parser.screen().scrollback(), 5);
        t.parser.screen_mut().set_scrollback(0);
        assert_eq!(t.parser.screen().scrollback(), 0);
    }

    #[test]
    fn selection_round_trips_via_contents_between() {
        let mut t = Terminal::default();
        t.parser.process(b"hello");
        t.selection = Some((
            CellPos { row: 0, col: 0 },
            CellPos { row: 0, col: 4 },
        ));
        let s = t.selection_to_string().unwrap_or_default();
        assert_eq!(s, "hello");
    }

    #[test]
    fn pixel_to_cell_maps_origin() {
        let cell_w = FALLBACK_CELL_W;
        let cell_h = FALLBACK_CELL_H;
        let rect = egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(cell_w * 80.0, cell_h * 24.0),
        );
        let cell = pixel_to_cell(rect, cell_w, cell_h, 24, 80, egui::pos2(1.0, 1.0))
            .expect("origin must map");
        assert_eq!(cell, CellPos { row: 0, col: 0 });
    }

    #[test]
    fn pixel_to_cell_outside_returns_none() {
        let cell_w = FALLBACK_CELL_W;
        let cell_h = FALLBACK_CELL_H;
        let rect = egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(cell_w * 80.0, cell_h * 24.0),
        );
        assert!(
            pixel_to_cell(
                rect,
                cell_w,
                cell_h,
                24,
                80,
                egui::pos2(rect.max.x + 100.0, 0.0)
            )
            .is_none()
        );
    }

    #[test]
    fn panel_starts_with_one_tab() {
        let p = TerminalPanel::new();
        assert_eq!(p.len(), 1);
        assert!(!p.is_empty());
        assert!(p.sessions().contains_key(p.active_key()));
        assert_eq!(p.order().len(), 1);
        assert!(p.active_key().starts_with("local-tab-"));
    }

    #[test]
    fn panel_new_tab_selects_fresh_session() {
        let mut p = TerminalPanel::new();
        let first = p.active_key().to_string();
        let second = p.new_tab();
        assert_eq!(p.len(), 2);
        assert_eq!(p.active_key(), second);
        assert_ne!(first, second);
        assert!(p.sessions().contains_key(&first));
        assert!(p.sessions().contains_key(&second));
    }

    #[test]
    fn panel_select_switches_active() {
        let mut p = TerminalPanel::new();
        let first = p.active_key().to_string();
        let second = p.new_tab();
        assert_eq!(p.active_key(), second);
        p.select(&first);
        assert_eq!(p.active_key(), first);
        p.select("no-such-tab");
        assert_eq!(p.active_key(), first);
    }

    #[test]
    fn panel_rekey_spawned_moves_to_session_id() {
        let mut p = TerminalPanel::new();
        let local = p.active_key().to_string();
        {
            let t = p.active_terminal_mut();
            t.session_id = Some("t-deadbeef0001".into());
            t.output_path = Some("substrate/x/derived/terminal/t-deadbeef0001".into());
            t.shell = Some("/bin/zsh".into());
        }
        p.rekey_spawned();
        assert!(!p.sessions().contains_key(&local));
        assert!(p.sessions().contains_key("t-deadbeef0001"));
        assert_eq!(p.active_key(), "t-deadbeef0001");
        assert_eq!(p.order(), &["t-deadbeef0001".to_string()]);
        assert_eq!(p.active_terminal().shell(), Some("/bin/zsh"));
    }

    #[test]
    fn panel_rekey_preserves_order_across_multiple_tabs() {
        let mut p = TerminalPanel::new();
        let a = p.active_key().to_string();
        let b = p.new_tab();
        p.sessions.get_mut(&a).unwrap().session_id = Some("sess-a".into());
        p.sessions.get_mut(&b).unwrap().session_id = Some("sess-b".into());
        p.rekey_spawned();
        assert_eq!(p.order(), &["sess-a".to_string(), "sess-b".to_string()]);
        assert_eq!(p.active_key(), "sess-b");
        assert_eq!(p.len(), 2);
    }

    #[test]
    fn tab_label_uses_shell_basename_and_short_id() {
        let mut t = Terminal::default();
        t.session_id = Some("t-deadbeef0001".into());
        t.shell = Some("/bin/zsh".into());
        assert_eq!(tab_label(Some(&t)), "zsh · t-deadbeef0001");
        t.session_id = Some("t-deadbeef0001extra".into());
        assert_eq!(tab_label(Some(&t)), "zsh · t-deadbeef00…");
        assert_eq!(tab_label(None), "terminal");
        let empty = Terminal::default();
        assert_eq!(tab_label(Some(&empty)), "starting…");
    }

    #[test]
    fn indexed_color_delegates_to_theming_ansi() {
        // Values are pinned in `theming::palette_contract`.
        for i in [0u8, 1, 15, 16, 196, 232, 255] {
            assert_eq!(indexed_color(i), crate::theming::ansi::indexed(i));
        }
    }

    #[test]
    fn resize_updates_screen_size() {
        let mut t = Terminal::default();
        assert_eq!(t.parser.screen().size(), (24, 80));
        t.parser.screen_mut().set_size(40, 120);
        assert_eq!(t.parser.screen().size(), (40, 120));
    }
}
