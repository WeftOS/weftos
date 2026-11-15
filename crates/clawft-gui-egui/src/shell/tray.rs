//! Bottom tray — frosted-glass bar with service status chips.

use eframe::egui;

use crate::live::{Connection, Snapshot};

pub const TRAY_HEIGHT: f32 = 42.0;

/// Abstract status of a tray service.
#[derive(Copy, Clone)]
pub enum Ok {
    On,
    Warn,
    Off,
}

/// Identity of a tray chip. The click handler maps this to the
/// substrate subtree to show in the detail window (see
/// [`chip_subtree`]).
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum ChipId {
    Kernel,
    Mesh,
    ExoChain,
    /// Ontology navigator — tree view of the whole substrate with
    /// schema-matched value viewers. See `.planning/explorer/PROJECT-PLAN.md`.
    Explorer,
}

impl ChipId {
    /// Human label for the detail window title.
    pub fn label(self) -> &'static str {
        match self {
            ChipId::Kernel => "Kernel",
            ChipId::Mesh => "Mesh",
            ChipId::ExoChain => "ExoChain",
            ChipId::Explorer => "Explorer",
        }
    }

    /// Substrate path this chip reflects. Shown in the detail window
    /// header so you can see which ontology subtree you're looking at.
    pub fn substrate_path(self) -> &'static str {
        match self {
            ChipId::Kernel => "substrate/kernel/status",
            ChipId::Mesh => "substrate/mesh/status",
            ChipId::ExoChain => "substrate/chain/status",
            ChipId::Explorer => "substrate/",
        }
    }
}

/// Return the raw substrate value backing this chip, if present.
/// Explorer has no single backing value — it walks the tree itself.
pub fn chip_subtree(chip: ChipId, snap: &Snapshot) -> Option<&serde_json::Value> {
    match chip {
        ChipId::Kernel => snap.status.as_ref(),
        ChipId::Mesh => snap.mesh_status.as_ref(),
        ChipId::ExoChain => snap.chain_status.as_ref(),
        ChipId::Explorer => None,
    }
}

impl Ok {
    fn color(self) -> egui::Color32 {
        match self {
            Ok::On => crate::theming::palette::SHELL_OK,
            Ok::Warn => crate::theming::palette::BLOCK_TEXT_WARN,
            Ok::Off => crate::theming::palette::TEXT_LABEL_FAINT,
        }
    }
}

/// Render the tray at the bottom of `rect`. Left cluster is the launcher
/// and clock; right cluster is the service chips.
///
/// `open_chip` carries the currently-focused chip (the one whose detail
/// window is open). Clicking a chip toggles that chip's window; clicking
/// a different chip swaps focus.
pub fn paint(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    snap: &Snapshot,
    launcher_active: &mut bool,
    open_chip: &mut Option<ChipId>,
) {
    let tray_rect = egui::Rect::from_min_size(
        egui::pos2(rect.left(), rect.bottom() - TRAY_HEIGHT),
        egui::vec2(rect.width(), TRAY_HEIGHT),
    );
    let painter = ui.painter_at(tray_rect);

    // Frosted glass: dark semi-transparent fill + 1px highlight at top.
    painter.rect_filled(tray_rect, 0.0, crate::theming::palette::TRAY_PANEL_FILL);
    painter.line_segment(
        [tray_rect.left_top(), tray_rect.right_top()],
        egui::Stroke::new(1.0, crate::theming::palette::TRAY_HAIRLINE),
    );

    let inner_rect = tray_rect.shrink2(egui::vec2(12.0, 6.0));
    // `scope_builder` (vs. raw `new_child`) wraps the child Ui in
    // `remember_min_rect` + `advance_cursor_after_rect`, so the child
    // Ui's own widget entry is finalised against its actual bounds
    // before egui runs hit-testing for the next frame. Without this,
    // the tray's child Ui is registered with `Rect::NOTHING` and the
    // chips' / launcher's hover and click responses never lock in.
    ui.scope_builder(egui::UiBuilder::new().max_rect(inner_rect), |ui| {
        ui.horizontal_centered(|ui| {
            // ── Left: launcher ────────────────────────────────
            let btn_text = egui::RichText::new("⏾ Blocks").monospace();
            if ui.selectable_label(*launcher_active, btn_text).clicked() {
                *launcher_active = !*launcher_active;
            }

            ui.add_space(10.0);
            ui.label(
                egui::RichText::new("WeftOS")
                    .small()
                    .color(crate::theming::palette::TRAY_TEXT_DIM),
            );

            // ── Right: service chips + clock ─────────────────
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    egui::RichText::new(clock_text())
                        .monospace()
                        .color(crate::theming::palette::TRAY_TEXT),
                );
                ui.add_space(12.0);

                // Services (right-to-left, so list them in reverse read order)
                for (id, label, glyph, status) in services(snap).iter().rev() {
                    let is_open = *open_chip == Some(*id);
                    if chip(ui, glyph, label, *status, is_open).clicked() {
                        *open_chip = if is_open { None } else { Some(*id) };
                    }
                }
            });
        });
    });
}

fn chip(ui: &mut egui::Ui, glyph: &str, tip: &str, status: Ok, active: bool) -> egui::Response {
    // Frame::show's response only senses hover, so we draw the chrome
    // inside a Frame and then *re-interact* the outer rect with
    // click+hover sense so the chip becomes a button. Active chips get
    // a brighter fill so you can see which detail window is open.
    let fill = if active {
        crate::theming::palette::TRAY_CHIP_HOVER_FILL
    } else {
        crate::theming::palette::TRAY_CHIP_FILL
    };
    let frame_resp = egui::Frame::new()
        .fill(fill)
        .corner_radius(8.0)
        .inner_margin(egui::Margin::symmetric(8, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
                ui.painter()
                    .circle_filled(rect.center(), 4.0, status.color());
                ui.label(
                    egui::RichText::new(glyph)
                        .monospace()
                        .color(crate::theming::palette::TRAY_TEXT_BRIGHT),
                );
            });
        })
        .response;
    // Promote the frame rect to an interactive widget. `interact` with
    // a stable id keyed on the chip tip means the click state survives
    // across frames even though the outer rect is recomputed each pass.
    let id = ui.make_persistent_id(("weft_tray_chip", tip));
    let resp = ui
        .interact(frame_resp.rect, id, egui::Sense::click())
        .on_hover_text(tip);
    ui.add_space(6.0);
    resp
}

/// Determine service statuses from the live snapshot.
///
/// Tray chips reflect subsystems that have a durable status surface:
/// - **Kernel** ← `snap.connection` (daemon-socket reachability)
/// - **Mesh** ← `substrate/mesh/status` (cluster.status RPC)
/// - **ExoChain** ← `substrate/chain/status` (chain.status RPC)
/// - **Explorer** — ontology navigator; tracks daemon connection so
///   you can tell at a glance whether walking the tree will work.
///
/// Wi-Fi / Bluetooth / Audio / ToF chips were retired here — their
/// substrate paths still flow into `Snapshot` (adapters intact) and
/// will be viewed through the Explorer instead, avoiding bespoke
/// chips that only duplicate a JSON field.
///
/// Order here determines visual order: the **last entry renders at
/// the far right** (right-to-left layout reverses the iteration).
fn services(snap: &Snapshot) -> Vec<(ChipId, &'static str, &'static str, Ok)> {
    let kernel = match snap.connection {
        Connection::Connected => Ok::On,
        Connection::Connecting => Ok::Warn,
        Connection::Disconnected => Ok::Off,
    };

    let mesh = mesh_state_to_ok(&snap.mesh_status);
    let exochain = chain_state_to_ok(&snap.chain_status);
    // Explorer is a tool, not a data surface — green whenever the
    // daemon is reachable, because that's when the tree is walkable.
    let explorer = kernel;

    vec![
        (ChipId::Kernel, "Kernel", "◉ kernel", kernel),
        (ChipId::Mesh, "Mesh", "⌖ mesh", mesh),
        (ChipId::ExoChain, "ExoChain", "⛓ chain", exochain),
        (ChipId::Explorer, "Explorer", "⌸ explore", explorer),
    ]
}

/// Map a `substrate/mesh/status` Replace value to a tray chip status.
/// `total_nodes > 0` with `healthy_nodes == total_nodes` → green;
/// some nodes degraded → amber; `available: false` or no data → grey.
fn mesh_state_to_ok(v: &Option<serde_json::Value>) -> Ok {
    let Some(obj) = v.as_ref() else {
        return Ok::Off;
    };
    if obj.get("available").and_then(|b| b.as_bool()) == Some(false) {
        return Ok::Off;
    }
    let total = obj.get("total_nodes").and_then(|n| n.as_u64()).unwrap_or(0);
    let healthy = obj
        .get("healthy_nodes")
        .and_then(|n| n.as_u64())
        .unwrap_or(0);
    match (total, healthy) {
        (0, _) => Ok::Off,
        (t, h) if h == t => Ok::On,
        _ => Ok::Warn,
    }
}

/// Map a `substrate/chain/status` Replace value to a tray chip status.
/// `available: true` → green; `available: false` (including missing
/// `exochain` feature) → grey.
fn chain_state_to_ok(v: &Option<serde_json::Value>) -> Ok {
    let Some(obj) = v.as_ref() else {
        return Ok::Off;
    };
    // On the success path ChainAdapter injects `available: true`.
    // On the failure path it emits `{available: false, reason}`.
    match obj.get("available").and_then(|b| b.as_bool()) {
        Some(true) => Ok::On,
        _ => Ok::Off,
    }
}

fn clock_text() -> String {
    let now = chrono_utc_local_ish();
    format!("{:02}:{:02}", now.0, now.1)
}

/// Crude local clock via std::time. We don't want a chrono dep bump here;
/// seconds-since-UTC-midnight is close enough for a status bar mock.
fn chrono_utc_local_ish() -> (u32, u32) {
    use web_time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let day = secs % 86_400;
    let hour = (day / 3600) as u32;
    let minute = ((day % 3600) / 60) as u32;
    (hour, minute)
}
