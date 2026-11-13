//! Canon primitive demos — runs every primitive with sample data so
//! the Blocks window in the panel proves the retrofit round-trips.
//!
//! Each `show_<primitive>` call instantiates the canon widget, runs
//! `.show(ui)`, and records the last interaction's IRI, bearing,
//! variant, and latency into the shared state so the Blocks window can
//! render a footer read-out for the four return-signals.

use eframe::egui;
use egui_dock::{DockState, NodeIndex, TabViewer};

use crate::canon::{
    self, AgentThread, Canvas, Chip, ChipTone, Dock, Field, FieldKind, FieldValue, Gauge, Grid,
    Media, MediaFit, Modal, Modality, Pressable, Select, Sheet, Slider, Stack, StreamView, Strip,
    Tabs, ThreadDock, ThreadDockState, ThreadPhase, Thresholds, Toggle, ToggleStyle, Tree,
    TreeNode,
};
use crate::canon::{CanonResponse, CanonWidget};

/// Every canon primitive gets its own demo tab in the panel.
#[derive(Copy, Clone, PartialEq, Eq)]
pub enum CanonKind {
    Pressable,
    Chip,
    Stack,
    Strip,
    StreamView,
    Table,
    Tree,
    Gauge,
    Plot,
    Field,
    Toggle,
    Select,
    Slider,
    Grid,
    Dock,
    Sheet,
    Modal,
    Media,
    Canvas,
    Tabs,
    /// WEFT-284 — column-per-agent parallel output.
    ThreadDock,
}

impl CanonKind {
    pub const ALL: [(CanonKind, &'static str); 21] = [
        (CanonKind::Pressable, "Pressable"),
        (CanonKind::Chip, "Chip"),
        (CanonKind::Stack, "Stack"),
        (CanonKind::Strip, "Strip"),
        (CanonKind::StreamView, "StreamView"),
        (CanonKind::Table, "Table"),
        (CanonKind::Tree, "Tree"),
        (CanonKind::Gauge, "Gauge"),
        (CanonKind::Plot, "Plot"),
        (CanonKind::Field, "Field"),
        (CanonKind::Toggle, "Toggle"),
        (CanonKind::Select, "Select"),
        (CanonKind::Slider, "Slider"),
        (CanonKind::Grid, "Grid"),
        (CanonKind::Dock, "Dock"),
        (CanonKind::Sheet, "Sheet"),
        (CanonKind::Modal, "Modal"),
        (CanonKind::Media, "Media"),
        (CanonKind::Canvas, "Canvas"),
        (CanonKind::Tabs, "Tabs"),
        (CanonKind::ThreadDock, "ThreadDock"),
    ];
}

/// Per-primitive state that has to persist across frames.
pub struct CanonDemoState {
    pub press_count: u32,
    pub chip_selected: usize,
    pub gauge_value: f64,
    pub field_text: FieldValue,
    pub field_number: FieldValue,
    pub field_choice: FieldValue,
    pub toggle_switch: bool,
    pub toggle_check: bool,
    pub select_idx: usize,
    pub slider_value: f64,
    pub tabs_idx: usize,
    pub modal_open_for: Option<Modality>,
    pub dock_state: Option<DockState<String>>,
    /// WEFT-284 demo dock with ≥2 parallel agent columns.
    pub thread_dock: ThreadDockState,
    pub last_response_note: String,
}

impl Default for CanonDemoState {
    fn default() -> Self {
        Self {
            press_count: 0,
            chip_selected: 0,
            gauge_value: 63.0,
            field_text: FieldValue::Text(String::from("edit me")),
            field_number: FieldValue::Number(42.0),
            field_choice: FieldValue::Choice(1),
            toggle_switch: true,
            toggle_check: false,
            select_idx: 0,
            slider_value: 0.5,
            tabs_idx: 0,
            modal_open_for: None,
            dock_state: None,
            thread_dock: sample_thread_dock(),
            last_response_note: String::new(),
        }
    }
}

/// Seed ≥2 parallel agent streams for the Blocks / ThreadDock demo.
fn sample_thread_dock() -> ThreadDockState {
    ThreadDockState::with_threads([
        AgentThread::new("swarm/coder", "coder")
            .phase(ThreadPhase::Streaming)
            .token_rate(38.0)
            .variant(101)
            .lines([
                "planning worktree layout…",
                "editing crates/clawft-canon/src/thread_dock.rs",
                "appending unit tests for focus lane",
            ]),
        AgentThread::new("swarm/reviewer", "reviewer")
            .phase(ThreadPhase::Thinking)
            .token_rate(0.0)
            .variant(102)
            .lines([
                "watching coder stream",
                "checking column-per-thread invariant",
            ]),
        AgentThread::new("swarm/tester", "tester")
            .phase(ThreadPhase::Idle)
            .variant(103)
            .lines(["awaiting green tests…"]),
    ])
}

pub fn show(ui: &mut egui::Ui, kind: CanonKind, state: &mut CanonDemoState) {
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let label = CanonKind::ALL
                .iter()
                .copied()
                .find(|(k, _)| *k == kind)
                .map(|(_, l)| l)
                .unwrap_or("?");
            ui.heading(label);
            ui.label(
                egui::RichText::new(identity_for(kind))
                    .monospace()
                    .small()
                    .color(egui::Color32::from_gray(130)),
            );
            ui.separator();

            match kind {
                CanonKind::Pressable => show_pressable(ui, state),
                CanonKind::Chip => show_chip(ui, state),
                CanonKind::Stack => show_stack(ui),
                CanonKind::Strip => show_strip(ui),
                CanonKind::StreamView => show_stream_view(ui),
                CanonKind::Table => show_table(ui),
                CanonKind::Tree => show_tree(ui),
                CanonKind::Gauge => show_gauge(ui, state),
                CanonKind::Plot => show_plot(ui),
                CanonKind::Field => show_field(ui, state),
                CanonKind::Toggle => show_toggle(ui, state),
                CanonKind::Select => show_select(ui, state),
                CanonKind::Slider => show_slider(ui, state),
                CanonKind::Grid => show_grid(ui),
                CanonKind::Dock => show_dock(ui, state),
                CanonKind::Sheet => show_sheet(ui),
                CanonKind::Modal => show_modal(ui, state),
                CanonKind::Media => show_media(ui),
                CanonKind::Canvas => show_canvas(ui),
                CanonKind::Tabs => show_tabs(ui, state),
                CanonKind::ThreadDock => show_thread_dock(ui, state),
            }

            if !state.last_response_note.is_empty() {
                ui.add_space(12.0);
                ui.separator();
                ui.label(egui::RichText::new("Last CanonResponse").strong().small());
                ui.monospace(&state.last_response_note);
            }
        });
}

fn identity_for(kind: CanonKind) -> &'static str {
    match kind {
        CanonKind::Pressable => "ui://pressable",
        CanonKind::Chip => "ui://chip",
        CanonKind::Stack => "ui://stack",
        CanonKind::Strip => "ui://strip",
        CanonKind::StreamView => "ui://stream-view",
        CanonKind::Table => "ui://table",
        CanonKind::Tree => "ui://tree",
        CanonKind::Gauge => "ui://gauge",
        CanonKind::Plot => "ui://plot",
        CanonKind::Field => "ui://field",
        CanonKind::Toggle => "ui://toggle",
        CanonKind::Select => "ui://select",
        CanonKind::Slider => "ui://slider",
        CanonKind::Grid => "ui://grid",
        CanonKind::Dock => "ui://dock",
        CanonKind::Sheet => "ui://sheet",
        CanonKind::Modal => "ui://modal",
        CanonKind::Media => "ui://media",
        CanonKind::Canvas => "ui://canvas",
        CanonKind::Tabs => "ui://tabs",
        CanonKind::ThreadDock => "ui://thread-dock",
    }
}

fn note(state: &mut CanonDemoState, resp: &CanonResponse) {
    if resp.acted() {
        state.last_response_note = format!(
            "{}  variant={}  bearing={}  latency={}",
            resp.identity,
            resp.variant,
            resp.bearing.affordance.as_deref().unwrap_or("(none)"),
            resp.range
                .latency_ms()
                .map(|l| format!("{l:.1}ms"))
                .unwrap_or_else(|| "—".into()),
        );
    }
}

// ── Per-primitive demos ─────────────────────────────────────────────

fn show_pressable(ui: &mut egui::Ui, state: &mut CanonDemoState) {
    ui.label("Four style variants + disabled state.");
    ui.horizontal(|ui| {
        for (label, style) in [
            ("Primary", canon::pressable::PressableStyle::Primary),
            ("Secondary", canon::pressable::PressableStyle::Secondary),
            ("Ghost", canon::pressable::PressableStyle::Ghost),
            ("Destructive", canon::pressable::PressableStyle::Destructive),
        ] {
            let resp = Pressable::new(("demo.pressable", label), label)
                .style(style)
                .tooltip(format!("fires wsp.activate — {label}"))
                .show(ui);
            if resp.inner.clicked() {
                state.press_count += 1;
            }
            note(state, &resp);
        }
        let _ = Pressable::new("demo.pressable.disabled", "Disabled")
            .enabled(false)
            .show(ui);
    });
    ui.label(format!("Clicks: {}", state.press_count));
}

fn show_chip(ui: &mut egui::Ui, state: &mut CanonDemoState) {
    ui.label("Four ChipTones + a selectable group.");
    ui.horizontal_wrapped(|ui| {
        for (i, (label, tone)) in [
            ("ok", ChipTone::Ok),
            ("warn", ChipTone::Warn),
            ("info", ChipTone::Info),
            ("crit", ChipTone::Crit),
        ]
        .iter()
        .enumerate()
        {
            let resp = Chip::new(("demo.chip", i), *label).tone(*tone).show(ui);
            note(state, &resp);
        }
    });
    ui.add_space(8.0);
    ui.label("Selectable group:");
    ui.horizontal(|ui| {
        for (i, label) in ["alpha", "beta", "gamma", "delta"].iter().enumerate() {
            let resp = Chip::new(("demo.chip.sel", i), *label)
                .selectable(state.chip_selected == i)
                .show(ui);
            if resp.inner.clicked() {
                state.chip_selected = i;
            }
            note(state, &resp);
        }
    });
    ui.label(format!(
        "selected = {}",
        ["alpha", "beta", "gamma", "delta"][state.chip_selected]
    ));
}

fn show_stack(ui: &mut egui::Ui) {
    ui.label("Horizontal and vertical stacks.");
    Stack::new("demo.stack.h")
        .horizontal()
        .body(|ui| {
            for i in 0..4 {
                ui.label(format!("h-cell-{i}"));
            }
        })
        .show(ui);
    ui.add_space(6.0);
    Stack::new("demo.stack.v")
        .vertical()
        .body(|ui| {
            for i in 0..3 {
                ui.label(format!("v-cell-{i}"));
            }
        })
        .show(ui);
}

fn show_strip(ui: &mut egui::Ui) {
    use canon::CellSize;
    ui.label("Fixed-ratio 3-column strip.");
    Strip::new("demo.strip")
        .horizontal()
        .cells(vec![
            CellSize::Relative(0.25),
            CellSize::Remainder,
            CellSize::Exact(120.0),
        ])
        .body(|strip| {
            strip.cell(|ui| {
                ui.painter()
                    .rect_filled(ui.max_rect(), 4.0, crate::theming::palette::DEMO_BLOCK_A);
                ui.label("25%");
            });
            strip.cell(|ui| {
                ui.painter()
                    .rect_filled(ui.max_rect(), 4.0, crate::theming::palette::DEMO_BLOCK_B);
                ui.label("remainder");
            });
            strip.cell(|ui| {
                ui.painter()
                    .rect_filled(ui.max_rect(), 4.0, crate::theming::palette::DEMO_BLOCK_C);
                ui.label("120px");
            });
        })
        .show(ui);
}

fn show_stream_view(ui: &mut egui::Ui) {
    ui.label("Live-tailing view — static sample lines, stick-to-bottom.");
    let sample: Vec<String> = (0..40)
        .map(|i| format!("[tick {i:03}] substrate event — no-op"))
        .collect();
    StreamView::new("demo.stream")
        .lines(&sample)
        .max_height(220.0)
        .show(ui);
}

fn show_table(ui: &mut egui::Ui) {
    use canon::TableColumn;
    ui.label("Sortable table with sample rows.");
    let columns = [
        TableColumn::new("agent").min_width(140.0),
        TableColumn::new("pid").min_width(60.0),
        TableColumn::new("status").remainder(),
    ];
    let rows = [
        ("coder-agent", 42u32, "running"),
        ("reviewer-agent", 99, "idle"),
        ("tester-agent", 17, "running"),
    ];
    let (_resp, _outcome) = canon::Table::new("demo.table", &columns)
        .rows(rows.len())
        .render(|row, idx| {
            let (a, p, s) = rows[idx];
            row.col(|ui| {
                ui.label(a);
            });
            row.col(|ui| {
                ui.monospace(p.to_string());
            });
            row.col(|ui| {
                ui.label(s);
            });
        })
        .show_with_outcome(ui);
}

fn show_tree(ui: &mut egui::Ui) {
    // Canon Tree takes a single root. Wrap demo nodes in a synthetic
    // root branch so the hierarchy shows three top-level items.
    let root = TreeNode::branch(
        "substrate",
        vec![
            TreeNode::branch(
                "kernel",
                vec![
                    TreeNode::leaf("ipc"),
                    TreeNode::branch(
                        "services",
                        vec![
                            TreeNode::leaf("mesh"),
                            TreeNode::leaf("gate"),
                            TreeNode::leaf("rpc"),
                        ],
                    ),
                ],
            ),
            TreeNode::leaf("daemon"),
            TreeNode::leaf("rpc"),
        ],
    );
    let (_, _outcome) = Tree::new("demo.tree")
        .root(&root)
        .default_open_depth(2)
        .show_with_outcome(ui);
}

fn show_gauge(ui: &mut egui::Ui, state: &mut CanonDemoState) {
    ui.label("Linear gauge with threshold colouring.");
    let thresholds = Thresholds {
        warn_at: 0.6,
        crit_at: 0.85,
    };
    Gauge::new("demo.gauge", state.gauge_value, (0.0, 100.0))
        .thresholds(thresholds)
        .label("health")
        .show(ui);
    ui.horizontal(|ui| {
        if ui.button("−5").clicked() {
            state.gauge_value = (state.gauge_value - 5.0).max(0.0);
        }
        if ui.button("+5").clicked() {
            state.gauge_value = (state.gauge_value + 5.0).min(100.0);
        }
        ui.label(format!("value = {:.0}", state.gauge_value));
    });
}

fn show_plot(ui: &mut egui::Ui) {
    let points: Vec<(f64, f64)> = (0..200)
        .map(|i| {
            let x = i as f64 * 0.05;
            (x, (x * 2.0).sin() + 0.3 * (x * 7.0).sin())
        })
        .collect();
    canon::Plot::new("demo.plot").points(&points).show(ui);
}

fn show_field(ui: &mut egui::Ui, state: &mut CanonDemoState) {
    ui.label("Text / Number / Choice variants — one per row.");
    Field::new(
        "demo.field.text",
        FieldKind::text("placeholder"),
        &mut state.field_text,
    )
    .show(ui);
    Field::new(
        "demo.field.num",
        FieldKind::number(0.0, 100.0, 1.0),
        &mut state.field_number,
    )
    .show(ui);
    const CHOICES: &[&str] = &["apples", "pears", "figs", "quince"];
    Field::new(
        "demo.field.choice",
        FieldKind::choice(CHOICES),
        &mut state.field_choice,
    )
    .show(ui);
    ui.add_space(6.0);
    ui.monospace(format!("text={:?}", state.field_text));
    ui.monospace(format!("num ={:?}", state.field_number));
    ui.monospace(format!("pick={:?}", state.field_choice));
}

fn show_toggle(ui: &mut egui::Ui, state: &mut CanonDemoState) {
    ui.label("Switch and Checkbox styles bound to separate bools.");
    Toggle::new(
        "demo.toggle.switch",
        "switch style",
        &mut state.toggle_switch,
    )
    .style(ToggleStyle::Switch)
    .show(ui);
    Toggle::new(
        "demo.toggle.check",
        "checkbox style",
        &mut state.toggle_check,
    )
    .style(ToggleStyle::Checkbox)
    .show(ui);
    ui.monospace(format!(
        "switch={}  check={}",
        state.toggle_switch, state.toggle_check
    ));
}

fn show_select(ui: &mut egui::Ui, state: &mut CanonDemoState) {
    const OPTIONS: &[&str] = &["dev", "staging", "prod", "sandbox"];
    Select::new("demo.select", "environment", OPTIONS, &mut state.select_idx).show(ui);
    ui.monospace(format!(
        "selected = {} (idx {})",
        OPTIONS[state.select_idx], state.select_idx
    ));
}

fn show_slider(ui: &mut egui::Ui, state: &mut CanonDemoState) {
    Slider::new("demo.slider", "ratio", &mut state.slider_value, 0.0, 1.0)
        .suffix(" ratio")
        .show(ui);
    ui.monospace(format!("value = {:.3}", state.slider_value));
}

fn show_grid(ui: &mut egui::Ui) {
    ui.label("3×3 grid.");
    Grid::new("demo.grid", 3, |ui| {
        for r in 0..3 {
            for c in 0..3 {
                egui::Frame::new()
                    .fill(egui::Color32::from_gray(28))
                    .corner_radius(3.0)
                    .inner_margin(egui::Margin::symmetric(10, 6))
                    .show(ui, |ui| {
                        ui.set_min_size(egui::vec2(70.0, 32.0));
                        ui.label(egui::RichText::new(format!("{r},{c}")).monospace().small());
                    });
            }
            ui.end_row();
        }
    })
    .show(ui);
}

// ── Dock demo: DockState<String> + minimal TabViewer ─────────────────

struct DemoTabViewer;

impl TabViewer for DemoTabViewer {
    type Tab = String;
    fn title(&mut self, tab: &mut Self::Tab) -> egui::WidgetText {
        tab.as_str().into()
    }
    fn ui(&mut self, ui: &mut egui::Ui, tab: &mut Self::Tab) {
        ui.label(format!("body of '{}'", tab));
        ui.label("Drag a tab header to split/reorder.");
    }
}

fn show_dock(ui: &mut egui::Ui, state: &mut CanonDemoState) {
    ui.label("egui_dock-backed workspace. Drag tabs to split/reorder.");
    let dock_state = state.dock_state.get_or_insert_with(|| {
        let mut d = DockState::new(vec!["overview".into(), "logs".into()]);
        let [_old, _new] =
            d.main_surface_mut()
                .split_right(NodeIndex::root(), 0.5, vec!["metrics".into()]);
        d
    });
    // Render the dock in a fixed-height child so the rest of the demo
    // page stays scrollable. Then separately announce the canon Dock
    // wrapper so the response-note footer stays wired.
    let dock_height = 260.0;
    let (dock_rect, _resp) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), dock_height),
        egui::Sense::hover(),
    );
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(dock_rect));
    let mut viewer = DemoTabViewer;
    let _ = Dock::new("demo.dock", dock_state, &mut viewer).show(&mut child);
    // Re-sync the outer layout cursor past the allocated rect (egui
    // does this for us when we go through allocate_exact_size, so
    // nothing extra required).
    let _ = _resp;
}

fn show_sheet(ui: &mut egui::Ui) {
    ui.label("Scrollable sheet — 120 lines of sample content.");
    Sheet::new("demo.sheet", |ui: &mut egui::Ui| {
        for i in 0..120 {
            ui.label(format!("line {i:03} — lorem ipsum dolor sit amet"));
        }
    })
    .max_height(260.0)
    .show(ui);
}

fn show_modal(ui: &mut egui::Ui, state: &mut CanonDemoState) {
    ui.label("Click to open each modality.");
    ui.horizontal(|ui| {
        for m in [
            Modality::Modal,
            Modality::Floating,
            Modality::Tool,
            Modality::Toast,
        ] {
            if ui.button(format!("{m:?}")).clicked() {
                state.modal_open_for = Some(m);
            }
        }
        if ui.button("close").clicked() {
            state.modal_open_for = None;
        }
    });

    if let Some(m) = state.modal_open_for {
        let title = format!("{m:?} demo");
        Modal::new("demo.modal", m, title, |ui: &mut egui::Ui| {
            ui.label("Body of the modal.");
            ui.label("Each modality renders differently — blocking scrim, floating window, tool hover, auto-dismiss toast.");
        })
        .open(true)
        .show(ui);
        if matches!(m, Modality::Toast) {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(200));
        }
    }
}

fn show_media(ui: &mut egui::Ui) {
    ui.label("Inline media primitive — renders the bundled boot logo.");
    Media::new("demo.media", "bytes://weftos-gold.png")
        .fit(MediaFit::Contain)
        .max_size(egui::vec2(280.0, 240.0))
        .show(ui);
}

fn show_canvas(ui: &mut egui::Ui) {
    ui.label("Drag to pan, scroll to zoom. Rings redraw in the transformed frame.");
    let size = egui::vec2(ui.available_width(), 260.0);
    Canvas::new("demo.canvas", size, |painter, rect, transform| {
        let origin = rect.center() + transform.offset;
        for ring in 1..=6 {
            let r = ring as f32 * 24.0 * transform.scale;
            painter.circle_stroke(
                origin,
                r,
                egui::Stroke::new(1.0, egui::Color32::from_gray(80 + ring as u8 * 20)),
            );
        }
        painter.text(
            origin,
            egui::Align2::CENTER_CENTER,
            "ui://canvas",
            egui::FontId::monospace(12.0),
            egui::Color32::from_gray(200),
        );
    })
    .show(ui);
}

fn show_tabs(ui: &mut egui::Ui, state: &mut CanonDemoState) {
    const LABELS: &[&str] = &["Overview", "Metrics", "Logs"];
    const BODIES: &[&str] = &[
        "3 peers • 12 topics • uptime 4h13m",
        "cpu 18%  mem 32%  fps 59",
        "[INFO] mesh listener on 0.0.0.0:9489\n[INFO] peer connected: leaf-abc",
    ];
    Tabs::new(
        "demo.tabs",
        LABELS,
        &mut state.tabs_idx,
        |ui: &mut egui::Ui, idx: usize| {
            egui::Frame::new()
                .fill(egui::Color32::from_gray(22))
                .corner_radius(4.0)
                .inner_margin(egui::Margin::symmetric(12, 10))
                .show(ui, |ui| {
                    ui.label(BODIES[idx]);
                });
        },
    )
    .show(ui);
}

/// WEFT-284 — column-per-agent parallel streams (≥2 columns).
fn show_thread_dock(ui: &mut egui::Ui, state: &mut CanonDemoState) {
    ui.label(
        "Column-per-agent dock (never interleaved). Click a column header \
         to switch the focus lane; non-focused columns keep streaming with \
         presence pips.",
    );
    ui.horizontal(|ui| {
        if ui.button("tick streams").clicked() {
            let n = state
                .thread_dock
                .get("swarm/coder")
                .map(|t| t.lines.len())
                .unwrap_or(0);
            let _ = state.append_demo_tick(n);
        }
        if ui.button("reset sample").clicked() {
            state.thread_dock = sample_thread_dock();
        }
        if let Some(f) = state.thread_dock.focused() {
            ui.label(
                egui::RichText::new(format!(
                    "focus={}  phase={}  columns={}",
                    f.label,
                    f.phase.as_str(),
                    state.thread_dock.len()
                ))
                .small()
                .monospace(),
            );
        }
    });
    ui.add_space(6.0);
    let resp = ThreadDock::new("demo.thread_dock", &mut state.thread_dock)
        .min_height(220.0)
        .max_height(280.0)
        .show_close(true)
        .tooltip("WEFT-284 ThreadDock — parallel agent columns")
        .show(ui);
    note(state, &resp);
}

impl CanonDemoState {
    /// Advance demo agent streams by one synthetic tick (smoke ≥2 streams).
    fn append_demo_tick(&mut self, n: usize) -> bool {
        let a = self
            .thread_dock
            .append_line("swarm/coder", format!("coder tick {n}"));
        let b = self
            .thread_dock
            .append_line("swarm/reviewer", format!("reviewer tick {n}"));
        let _ = self
            .thread_dock
            .set_phase("swarm/coder", ThreadPhase::Streaming);
        let _ = self
            .thread_dock
            .set_phase("swarm/reviewer", ThreadPhase::Streaming);
        let _ = self
            .thread_dock
            .set_token_rate("swarm/coder", 40.0 + n as f32);
        a && b
    }
}
