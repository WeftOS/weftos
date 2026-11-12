//! Agent Workspace mode — freeform multi-agent stage (ADR-073 Phase C / WEFT-687).
//!
//! Launchable from the sidebar (`SidebarTarget::Workspace`). Does **not**
//! replace the calm 0.8 stock desktop; it is an optional shell mode.
//!
//! Responsibilities:
//! - Own [`WindowManager`] + [`WindowIntentBus`]
//! - Apply intents from keyboard / GUI / external (MCP/voice) producers
//! - Attention bus: rate-limited visual glow when agents need human
//! - Paint the freeform stage (chrome + pane bodies)

use std::collections::HashMap;
use std::time::Duration;

use eframe::egui;
use web_time::Instant;

use super::window_intent::{
    intent_from_key, ArrangeMode, ExternalIntentSource, ExternalIntentTx, IntentEnvelope,
    IntentSource, KeyModifiers, PaneId, PaneKind, WindowIntent, WindowIntentBus,
    WindowIntentProducer,
};
use super::window_manager::{
    AttentionState, WindowManager, RESIZE_HANDLE, TITLE_BAR_H,
};
use crate::theming::Tokens;

/// Minimum interval between attention events for the same pane.
const ATTENTION_COOLDOWN: Duration = Duration::from_secs(3);
/// How long the glow stays after a needs-human event.
const ATTENTION_HOLD: Duration = Duration::from_secs(12);

/// Agent Workspace controller.
pub struct AgentWorkspace {
    pub wm: WindowManager,
    pub bus: WindowIntentBus,
    /// External producer (MCP / voice). Always present; idle until wired.
    external_tx: ExternalIntentTx,
    external_src: ExternalIntentSource,
    /// Per-pane last attention accept time (rate limit).
    attention_last: HashMap<String, Instant>,
    /// Per-pane glow expiry.
    attention_until: HashMap<String, Instant>,
    /// Demo seed done once when first opened empty.
    seeded: bool,
    /// Status line for chrome.
    pub status: String,
}

impl Default for AgentWorkspace {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentWorkspace {
    pub fn new() -> Self {
        let (external_tx, external_src) = ExternalIntentSource::channel();
        let mut wm = WindowManager::new();
        if let Some(path) = WindowManager::default_layout_path() {
            wm = wm.with_layout_path(path);
        }
        Self {
            wm,
            bus: WindowIntentBus::new(),
            external_tx,
            external_src,
            attention_last: HashMap::new(),
            attention_until: HashMap::new(),
            seeded: false,
            status: "Agent Workspace · freeform stage".into(),
        }
    }

    /// MCP / voice producers obtain this tx to push intents.
    pub fn external_tx(&self) -> ExternalIntentTx {
        self.external_tx.clone()
    }

    /// Ensure a multi-pane demo wall exists (≥2 apps + 1 agent).
    pub fn ensure_demo_seed(&mut self) {
        if self.seeded || self.wm.pane_count() > 0 {
            self.seeded = true;
            return;
        }
        self.wm.spawn(
            PaneKind::App {
                app: "terminal".into(),
            },
            Some(PaneId::new("demo-terminal")),
        );
        self.wm.spawn(
            PaneKind::App {
                app: "chat".into(),
            },
            Some(PaneId::new("demo-chat")),
        );
        let agent = self
            .wm
            .spawn(
                PaneKind::Agent {
                    session_id: "demo-agent-1".into(),
                    role: "coder".into(),
                    title: "Coder".into(),
                    background: false,
                },
                Some(PaneId::new("demo-agent")),
            )
            .expect("agent pane");
        self.wm.append_body(
            &agent,
            "Scaffolding freeform WM…\nImplemented drag/resize + snap.\nAwaiting review (needs human).\n",
        );
        self.wm.arrange(ArrangeMode::Grid);
        self.seeded = true;
        self.status = "Seeded demo wall: Terminal + Chat + Coder agent".into();
    }

    /// Push from GUI chrome.
    pub fn push_gui(&mut self, intent: WindowIntent) {
        self.bus.push(intent, IntentSource::Gui);
    }

    /// Poll external + drain bus + apply.
    pub fn tick(&mut self) {
        for env in self.external_src.poll_intents() {
            self.bus
                .push(env.intent, env.source);
        }
        let envelopes = self.bus.drain();
        for env in envelopes {
            self.apply(env);
        }
        // Expire attention glows.
        let now = Instant::now();
        self.attention_until.retain(|id, until| {
            if *until <= now {
                if let Some(p) = self.wm.get_mut(&PaneId::new(id.clone())) {
                    p.attention = AttentionState::Calm;
                }
                false
            } else {
                true
            }
        });
        self.wm.save_if_dirty();
    }

    fn apply(&mut self, env: IntentEnvelope) {
        let IntentEnvelope { intent, source } = env;
        match intent {
            WindowIntent::Spawn { kind, id } => {
                let bg = matches!(
                    &kind,
                    PaneKind::Agent {
                        background: true,
                        ..
                    }
                );
                match self.wm.spawn(kind, id) {
                    Some(pid) => {
                        self.status = format!("Spawned {pid} ({source:?})");
                    }
                    None if bg => {
                        self.status = "Background agent spawn (no pane — explicit opt-in)".into();
                    }
                    None => {
                        self.status = "Spawn failed".into();
                    }
                }
            }
            WindowIntent::Focus { id } => {
                if self.wm.focus(&id) {
                    self.status = format!("Focused {id}");
                }
            }
            WindowIntent::Arrange { mode } => {
                self.wm.arrange(mode);
                self.status = format!("Arranged {mode:?}");
            }
            WindowIntent::Close { id, .. } => {
                if self.wm.close(&id) {
                    self.status = format!("Closed {id}");
                }
            }
            WindowIntent::Summarize { id, max_chars } => {
                let max = max_chars.unwrap_or(120);
                if let Some(s) = self.wm.summarize(&id, max) {
                    self.status = format!("Summary [{id}]: {s}");
                }
            }
            WindowIntent::Attach { tool_id, agent_id } => {
                if self.wm.attach(&tool_id, &agent_id) {
                    self.status = format!("Attached {tool_id} → {agent_id}");
                }
            }
        }
    }

    /// Rate-limited attention signal. Returns true if accepted.
    pub fn signal_attention(&mut self, id: &PaneId, reason: &str) -> bool {
        let now = Instant::now();
        if let Some(last) = self.attention_last.get(id.as_str())
            && now.duration_since(*last) < ATTENTION_COOLDOWN {
                return false;
            }
        self.attention_last.insert(id.0.clone(), now);
        self.attention_until
            .insert(id.0.clone(), now + ATTENTION_HOLD);
        self.wm.set_attention(id, AttentionState::NeedsHuman);
        if let Some(p) = self.wm.get_mut(id) {
            p.body_text
                .push_str(&format!("⚠ needs human: {reason}\n"));
        }
        self.status = format!("Attention: {id} — {reason}");
        true
    }

    /// Keyboard producer: read egui input, enqueue intents.
    pub fn poll_keyboard(&mut self, ctx: &egui::Context) {
        let mut mods = KeyModifiers::default();
        ctx.input(|i| {
            mods.mod_key = i.modifiers.command || i.modifiers.ctrl;
            mods.shift = i.modifiers.shift;
            mods.alt = i.modifiers.alt;
        });
        let focused = self.wm.focused.clone();
        let keys = ["g", "c", "s", "n", "w"];
        for key in keys {
            let pressed = ctx.input(|i| {
                // Match by name via egui Key where possible.
                match key {
                    "g" => i.key_pressed(egui::Key::G),
                    "c" => i.key_pressed(egui::Key::C),
                    "s" => i.key_pressed(egui::Key::S),
                    "n" => i.key_pressed(egui::Key::N),
                    "w" => i.key_pressed(egui::Key::W),
                    _ => false,
                }
            });
            if pressed
                && let Some(intent) = intent_from_key(mods, key, focused.as_ref()) {
                    // Fill demo agent title for spawn chord.
                    let intent = match intent {
                        WindowIntent::Spawn {
                            kind:
                                PaneKind::Agent {
                                    session_id,
                                    role,
                                    title: _,
                                    background,
                                },
                            id,
                        } => {
                            let n = self.wm.pane_count() + 1;
                            WindowIntent::Spawn {
                                kind: PaneKind::Agent {
                                    session_id,
                                    role,
                                    title: format!("Agent {n}"),
                                    background,
                                },
                                id,
                            }
                        }
                        other => other,
                    };
                    self.bus.push(intent, IntentSource::Keyboard);
                }
        }
    }
}

// ── Paint ───────────────────────────────────────────────────────────

/// Toolbar height under the stage heading.
const TOOLBAR_H: f32 = 36.0;
const HEADING_H: f32 = 40.0;

/// Paint Agent Workspace into `rect` (body right of sidebar).
pub fn show(ui: &mut egui::Ui, rect: egui::Rect, ws: &mut AgentWorkspace) {
    ws.ensure_demo_seed();
    ws.poll_keyboard(ui.ctx());
    ws.tick();

    let tokens = Tokens::default();
    let painter = ui.painter_at(rect);

    // Dim stage backdrop (wallpaper shows through slightly).
    painter.rect_filled(
        rect,
        0.0,
        crate::theming::palette::WORKSPACE_SCRIM,
    );

    // Heading
    painter.text(
        egui::pos2(rect.left() + 16.0, rect.top() + 12.0),
        egui::Align2::LEFT_TOP,
        "Agent Workspace",
        egui::FontId::proportional(18.0),
        tokens.text_primary,
    );
    painter.text(
        egui::pos2(rect.left() + 200.0, rect.top() + 16.0),
        egui::Align2::LEFT_TOP,
        &ws.status,
        egui::FontId::proportional(12.0),
        tokens.text_dim,
    );

    // Toolbar
    let toolbar = egui::Rect::from_min_max(
        egui::pos2(rect.left(), rect.top() + HEADING_H),
        egui::pos2(rect.right(), rect.top() + HEADING_H + TOOLBAR_H),
    );
    paint_toolbar(ui, toolbar, ws, &tokens);

    // Stage
    let stage = egui::Rect::from_min_max(
        egui::pos2(rect.left() + 8.0, toolbar.bottom() + 4.0),
        egui::pos2(rect.right() - 8.0, rect.bottom() - 8.0),
    );
    // Stage border
    ui.painter_at(stage).rect_stroke(
        stage,
        egui::CornerRadius::same(4),
        egui::Stroke::new(1.0, tokens.stroke_soft),
        egui::epaint::StrokeKind::Inside,
    );

    // Pointer interaction
    let pointer = ui.input(|i| i.pointer.interact_pos());
    let down = ui.input(|i| i.pointer.primary_down());
    ws.wm.interact_pointer(stage, pointer, down);

    // Paint panes back-to-front
    let order = ws.wm.paint_order();
    // Collect pane clones for paint to avoid borrow fights mid-loop.
    let panes: Vec<_> = order
        .iter()
        .filter_map(|&i| ws.wm.panes.get(i).cloned())
        .collect();
    let focused = ws.wm.focused.clone();

    for pane in &panes {
        let abs = pane.rect.to_abs(stage);
        paint_pane(ui, abs, pane, focused.as_ref() == Some(&pane.id), &tokens, ws);
    }

    // Attention chips strip (rate-limited visual bus)
    paint_attention_chips(ui, stage, ws, &tokens);
}

fn paint_toolbar(ui: &mut egui::Ui, rect: egui::Rect, ws: &mut AgentWorkspace, tokens: &Tokens) {
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    child.add_space(12.0);

    if child
        .add(egui::Button::new("＋ Agent").fill(tokens.bg_active))
        .on_hover_text("Spawn agent pane (visible-agent rule)")
        .clicked()
    {
        let n = ws.wm.pane_count() + 1;
        ws.push_gui(WindowIntent::Spawn {
            kind: PaneKind::Agent {
                session_id: String::new(),
                role: "agent".into(),
                title: format!("Agent {n}"),
                background: false,
            },
            id: None,
        });
    }
    if child.button("＋ Terminal").clicked() {
        ws.push_gui(WindowIntent::Spawn {
            kind: PaneKind::App {
                app: "terminal".into(),
            },
            id: None,
        });
    }
    if child.button("＋ Chat").clicked() {
        ws.push_gui(WindowIntent::Spawn {
            kind: PaneKind::App {
                app: "chat".into(),
            },
            id: None,
        });
    }
    child.separator();
    if child.button("Grid").on_hover_text("Mod+Shift+G").clicked() {
        ws.push_gui(WindowIntent::Arrange {
            mode: ArrangeMode::Grid,
        });
    }
    if child
        .button("Cascade")
        .on_hover_text("Mod+Shift+C")
        .clicked()
    {
        ws.push_gui(WindowIntent::Arrange {
            mode: ArrangeMode::Cascade,
        });
    }
    if child.button("Stack").on_hover_text("Mod+Shift+S").clicked() {
        ws.push_gui(WindowIntent::Arrange {
            mode: ArrangeMode::Stack,
        });
    }
    child.separator();
    if child
        .button("Summarize")
        .on_hover_text("Summarize focused agent wall")
        .clicked()
        && let Some(id) = ws.wm.focused.clone() {
            ws.push_gui(WindowIntent::Summarize {
                id,
                max_chars: Some(120),
            });
        }
    if child
        .button("⚠ Attention")
        .on_hover_text("Signal needs-human on focused pane (rate-limited)")
        .clicked()
        && let Some(id) = ws.wm.focused.clone() {
            ws.signal_attention(&id, "manual demo");
        }
    if child.button("Close").on_hover_text("Mod+W").clicked()
        && let Some(id) = ws.wm.focused.clone() {
            ws.push_gui(WindowIntent::Close {
                id,
                cancel_agent: false,
            });
        }

    child.add_space(8.0);
    child.label(
        egui::RichText::new(format!("{} panes", ws.wm.pane_count()))
            .color(tokens.text_dim)
            .small(),
    );
}

fn paint_pane(
    ui: &mut egui::Ui,
    abs: egui::Rect,
    pane: &super::window_manager::Pane,
    focused: bool,
    tokens: &Tokens,
    ws: &mut AgentWorkspace,
) {
    let painter = ui.painter_at(abs);

    // Attention glow ring
    let glow = matches!(pane.attention, AttentionState::NeedsHuman);
    if glow {
        let expand = 3.0;
        let glow_rect = abs.expand(expand);
        painter.rect_stroke(
            glow_rect,
            egui::CornerRadius::same(6),
            egui::Stroke::new(2.5, tokens.accent),
            egui::epaint::StrokeKind::Outside,
        );
        // Soft outer halo
        painter.rect_stroke(
            glow_rect.expand(3.0),
            egui::CornerRadius::same(8),
            egui::Stroke::new(1.0, tokens.accent_dim),
            egui::epaint::StrokeKind::Outside,
        );
    }

    // Body fill
    painter.rect_filled(abs, egui::CornerRadius::same(4), tokens.bg_surface);
    let border = if focused {
        tokens.accent
    } else {
        tokens.stroke_soft
    };
    painter.rect_stroke(
        abs,
        egui::CornerRadius::same(4),
        egui::Stroke::new(if focused { 1.5 } else { 1.0 }, border),
        egui::epaint::StrokeKind::Inside,
    );

    // Title bar
    let title_rect = egui::Rect::from_min_max(
        abs.min,
        egui::pos2(abs.right(), abs.top() + TITLE_BAR_H),
    );
    painter.rect_filled(
        title_rect,
        egui::CornerRadius {
            nw: 4,
            ne: 4,
            sw: 0,
            se: 0,
        },
        if focused {
            tokens.bg_active
        } else {
            tokens.bg_panel
        },
    );

    let title = WindowManager::pane_title(pane);
    painter.text(
        egui::pos2(title_rect.left() + 10.0, title_rect.center().y),
        egui::Align2::LEFT_CENTER,
        if glow {
            format!("● {title}")
        } else {
            title.clone()
        },
        egui::FontId::proportional(12.5),
        if glow {
            tokens.accent
        } else {
            tokens.text_primary
        },
    );

    // Close button
    let close_rect = egui::Rect::from_center_size(
        egui::pos2(title_rect.right() - 14.0, title_rect.center().y),
        egui::vec2(16.0, 16.0),
    );
    let close_id = egui::Id::new(("ws-close", pane.id.as_str()));
    let close_resp = ui.interact(close_rect, close_id, egui::Sense::click());
    painter.text(
        close_rect.center(),
        egui::Align2::CENTER_CENTER,
        "×",
        egui::FontId::proportional(14.0),
        if close_resp.hovered() {
            tokens.crit
        } else {
            tokens.text_dim
        },
    );
    if close_resp.clicked() {
        ws.push_gui(WindowIntent::Close {
            id: pane.id.clone(),
            cancel_agent: false,
        });
    }

    // Title bar drag
    let drag_rect = egui::Rect::from_min_max(
        title_rect.min,
        egui::pos2(close_rect.left() - 4.0, title_rect.bottom()),
    );
    let drag_id = egui::Id::new(("ws-drag", pane.id.as_str()));
    let drag_resp = ui.interact(drag_rect, drag_id, egui::Sense::click_and_drag());
    if drag_resp.drag_started()
        && let Some(pos) = drag_resp.interact_pointer_pos() {
            // grab_offset is pointer relative to pane min
            let grab = pos - abs.min;
            ws.wm.begin_drag(pane.id.clone(), grab);
        }
    if drag_resp.clicked() {
        ws.push_gui(WindowIntent::Focus {
            id: pane.id.clone(),
        });
    }

    // Resize handles (edges + corners)
    install_resize_handles(ui, abs, &pane.id, ws);

    // Body content
    let body = egui::Rect::from_min_max(
        egui::pos2(abs.left() + 8.0, title_rect.bottom() + 4.0),
        egui::pos2(abs.right() - 8.0, abs.bottom() - 8.0),
    );
    let mut body_ui = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(body)
            .layout(egui::Layout::top_down(egui::Align::LEFT)),
    );
    egui::ScrollArea::vertical()
        .id_salt(("ws-body", pane.id.as_str()))
        .auto_shrink([false, false])
        .show(&mut body_ui, |ui| {
            if let Some(summary) = &pane.summary {
                ui.colored_label(tokens.accent, format!("Summary: {summary}"));
                ui.add_space(4.0);
            }
            match &pane.kind {
                PaneKind::Agent {
                    session_id, role, ..
                } => {
                    ui.monospace(format!("session: {session_id}"));
                    ui.monospace(format!("role: {role}"));
                    ui.separator();
                    ui.label(egui::RichText::new(&pane.body_text).monospace().small());
                }
                PaneKind::App { app } => {
                    ui.label(format!("Stock app pane: {app}"));
                    ui.label(
                        egui::RichText::new(
                            "Freeform host — opens concurrent with agents. Full app bodies remain available on the calm desktop sidebar.",
                        )
                        .small()
                        .color(tokens.text_secondary),
                    );
                    ui.separator();
                    ui.label(egui::RichText::new(&pane.body_text).monospace().small());
                }
                PaneKind::Tool { tool, attached_to } => {
                    ui.label(format!("Tool: {tool}"));
                    if let Some(a) = attached_to {
                        ui.label(format!("Attached to agent pane: {a}"));
                    } else {
                        ui.colored_label(tokens.text_dim, "Not attached");
                    }
                    ui.label(egui::RichText::new(&pane.body_text).monospace().small());
                }
            }
        });
}

fn install_resize_handles(
    ui: &mut egui::Ui,
    abs: egui::Rect,
    id: &PaneId,
    ws: &mut AgentWorkspace,
) {
    let h = RESIZE_HANDLE;
    // Edges
    let edges = [
        (
            egui::Rect::from_min_max(
                egui::pos2(abs.left(), abs.top() + h),
                egui::pos2(abs.left() + h, abs.bottom() - h),
            ),
            true,
            false,
            false,
            false,
        ),
        (
            egui::Rect::from_min_max(
                egui::pos2(abs.right() - h, abs.top() + h),
                egui::pos2(abs.right(), abs.bottom() - h),
            ),
            false,
            true,
            false,
            false,
        ),
        (
            egui::Rect::from_min_max(
                egui::pos2(abs.left() + h, abs.top()),
                egui::pos2(abs.right() - h, abs.top() + h),
            ),
            false,
            false,
            true,
            false,
        ),
        (
            egui::Rect::from_min_max(
                egui::pos2(abs.left() + h, abs.bottom() - h),
                egui::pos2(abs.right() - h, abs.bottom()),
            ),
            false,
            false,
            false,
            true,
        ),
    ];
    // Corners
    let corners = [
        (
            egui::Rect::from_min_size(abs.min, egui::vec2(h, h)),
            true,
            false,
            true,
            false,
        ),
        (
            egui::Rect::from_min_size(
                egui::pos2(abs.right() - h, abs.top()),
                egui::vec2(h, h),
            ),
            false,
            true,
            true,
            false,
        ),
        (
            egui::Rect::from_min_size(
                egui::pos2(abs.left(), abs.bottom() - h),
                egui::vec2(h, h),
            ),
            true,
            false,
            false,
            true,
        ),
        (
            egui::Rect::from_min_size(
                egui::pos2(abs.right() - h, abs.bottom() - h),
                egui::vec2(h, h),
            ),
            false,
            true,
            false,
            true,
        ),
    ];

    for (i, (rect, l, r, t, b)) in edges.iter().chain(corners.iter()).enumerate() {
        let sense = ui.interact(
            *rect,
            egui::Id::new(("ws-rz", id.as_str(), i)),
            egui::Sense::drag(),
        );
        if sense.drag_started() {
            ws.wm.begin_resize(id.clone(), *l, *r, *t, *b);
        }
    }
}

fn paint_attention_chips(
    ui: &mut egui::Ui,
    stage: egui::Rect,
    ws: &AgentWorkspace,
    tokens: &Tokens,
) {
    let needing: Vec<_> = ws
        .wm
        .panes
        .iter()
        .filter(|p| matches!(p.attention, AttentionState::NeedsHuman))
        .collect();
    if needing.is_empty() {
        return;
    }
    let chip_y = stage.bottom() - 22.0;
    let mut x = stage.left() + 8.0;
    let painter = ui.painter();
    for p in needing {
        let label = format!("● {}", WindowManager::pane_title(p));
        let galley = painter.layout_no_wrap(
            label.clone(),
            egui::FontId::proportional(11.0),
            tokens.accent,
        );
        let w = galley.size().x + 16.0;
        let chip = egui::Rect::from_min_size(egui::pos2(x, chip_y - 10.0), egui::vec2(w, 20.0));
        painter.rect_filled(chip, egui::CornerRadius::same(10), tokens.bg_active);
        painter.rect_stroke(
            chip,
            egui::CornerRadius::same(10),
            egui::Stroke::new(1.0, tokens.accent),
            egui::epaint::StrokeKind::Inside,
        );
        painter.galley(
            egui::pos2(chip.left() + 8.0, chip.center().y - galley.size().y / 2.0),
            galley,
            tokens.accent,
        );
        x += w + 6.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_creates_two_apps_plus_agent() {
        let mut ws = AgentWorkspace::new();
        // Clear any loaded layout for hermetic test.
        ws.wm = WindowManager::new();
        ws.seeded = false;
        ws.ensure_demo_seed();
        assert!(ws.wm.pane_count() >= 3);
        let apps = ws
            .wm
            .panes
            .iter()
            .filter(|p| matches!(p.kind, PaneKind::App { .. }))
            .count();
        let agents = ws
            .wm
            .panes
            .iter()
            .filter(|p| matches!(p.kind, PaneKind::Agent { .. }))
            .count();
        assert!(apps >= 2);
        assert!(agents >= 1);
    }

    #[test]
    fn attention_rate_limited() {
        let mut ws = AgentWorkspace::new();
        ws.wm = WindowManager::new();
        let id = ws
            .wm
            .spawn(
                PaneKind::Agent {
                    session_id: "s".into(),
                    role: "r".into(),
                    title: "T".into(),
                    background: false,
                },
                Some(PaneId::new("a")),
            )
            .unwrap();
        assert!(ws.signal_attention(&id, "first"));
        assert!(!ws.signal_attention(&id, "spam"));
    }

    #[test]
    fn external_mcp_tx_reaches_tick() {
        let mut ws = AgentWorkspace::new();
        ws.wm = WindowManager::new();
        let tx = ws.external_tx();
        tx.submit_mcp(WindowIntent::Spawn {
            kind: PaneKind::App {
                app: "logs".into(),
            },
            id: Some(PaneId::new("from-mcp")),
        })
        .unwrap();
        ws.tick();
        assert!(ws.wm.get(&PaneId::new("from-mcp")).is_some());
    }

    #[test]
    fn spawn_intent_visible_agent_default() {
        let mut ws = AgentWorkspace::new();
        ws.wm = WindowManager::new();
        ws.push_gui(WindowIntent::Spawn {
            kind: PaneKind::Agent {
                session_id: "x".into(),
                role: "coder".into(),
                title: "C".into(),
                background: false,
            },
            id: None,
        });
        ws.tick();
        assert_eq!(ws.wm.pane_count(), 1);
    }
}
