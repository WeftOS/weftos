//! Colour snapshot of the public GUI surfaces (tessellation level).
//!
//! Each test renders one surface through egui at a fixed size and clock,
//! tessellates it, and (when `GUI_SNAP_OUT` is set) writes every vertex colour
//! to `$GUI_SNAP_OUT/<surface>.txt`. Run the same file at two commits and diff
//! the output directories to prove a refactor left every rendered colour
//! unchanged. Without the env var the tests only check that each surface
//! renders and is deterministic.
//!
//! In-crate surfaces that need private access (VT100 terminal, chat panel)
//! are covered by `cfg(test)` tests in the crate; they write into the same
//! directory. See `docs/development/gui-colour-snapshot.md` and
//! `scripts/gui-colour-snapshot.sh`.

#![cfg(not(target_arch = "wasm32"))]

#[path = "../src/snap_support.rs"]
mod snap_support;

use clawft_gui_egui::explorer::conversation_graph::ConversationGraphPanel;
use clawft_gui_egui::explorer::viewers::{self, graph_edit};
use clawft_gui_egui::live::{Connection, Snapshot};
use clawft_gui_egui::shell::{boot, grid, sidebar, tray, workspace};
use serde_json::{Value, json};
use snap_support::{render_checked, render_once};

fn setup(ctx: &egui::Context) {
    clawft_gui_egui::theming::apply(ctx);
    egui_extras::install_image_loaders(ctx);
}

fn full_rect() -> egui::Rect {
    egui::Rect::from_min_size(
        egui::Pos2::ZERO,
        egui::vec2(snap_support::SCREEN.0, snap_support::SCREEN.1),
    )
}

fn viewer(name: &str, path: &'static str, value: Value) {
    render_checked(name, setup, vec![], || {
        let value = value.clone();
        move |ui: &mut egui::Ui| viewers::dispatch(ui, path, &value)
    });
}

/// For viewers that feed the process-global time-series sample history
/// (sparklines): a second in-process run would see a longer history by
/// design, so render once.
fn viewer_once(name: &str, path: &'static str, value: Value) {
    render_once(name, setup, vec![], move |ui: &mut egui::Ui| {
        viewers::dispatch(ui, path, &value)
    });
}

fn graph_value() -> Value {
    json!({
        "nodes": [
            { "id": "n1", "label": "Oscillator", "kind": "osc", "pos": [0.0, 0.0] },
            { "id": "n2", "label": "Filter", "kind": "vcf", "pos": [1.0, 0.4] },
            { "id": "n3", "label": "Output", "kind": "out", "pos": [2.0, 0.0] }
        ],
        "edges": [
            { "source": "n1", "target": "n2", "kind": "audio" },
            { "source": "n2", "target": "n3", "kind": "audio" }
        ]
    })
}

fn sine(n: usize) -> Vec<f64> {
    (0..n)
        .map(|i| (i as f64 * 0.3).sin() * 0.8)
        .collect::<Vec<_>>()
}

#[test]
fn colsnap_viewer_graph() {
    viewer("viewer_graph", "substrate/test/graph", graph_value());
}

#[test]
fn colsnap_viewer_graph_edit() {
    render_checked("viewer_graph_edit", setup, vec![], || {
        let v = graph_value();
        move |ui: &mut egui::Ui| graph_edit::paint(ui, "substrate/test/patch", &v)
    });
}

#[test]
fn colsnap_viewer_mesh_nodes() {
    viewer(
        "viewer_mesh_nodes",
        "substrate/mesh/status",
        json!({
            "total_nodes": 4, "healthy_nodes": 3,
            "nodes": [
                { "id": "node-a", "status": "healthy", "age_ms": 1200 },
                { "id": "node-b", "status": "down", "age_ms": 0 },
                { "id": "node-c", "status": "degraded", "age_ms": 9000 },
                { "id": "node-d", "status": "healthy", "age_ms": 40 }
            ]
        }),
    );
}

#[test]
fn colsnap_viewer_health() {
    viewer_once(
        "viewer_health",
        "substrate/n-1/health",
        json!({ "kind": "health", "rssi": -47, "free_heap": 184_000_u64, "uptime_s": 3600 }),
    );
}

#[test]
fn colsnap_viewer_health_weak_signal() {
    viewer_once(
        "viewer_health_weak",
        "substrate/n-2/health",
        json!({ "kind": "health", "rssi": -91, "free_heap": 12_000_u64 }),
    );
}

#[test]
fn colsnap_viewer_lineage() {
    viewer(
        "viewer_lineage",
        "substrate/n-d/derived/transcript/n-6f3a9c/mic/meta/lineage",
        json!({
            "kind": "lineage",
            "source_paths": ["substrate/n-6f3a9c/sensor/mic"],
            "via_actor": "whisper",
            "derivation": "stt",
            "target_path": "substrate/n-d/derived/transcript/n-6f3a9c/mic",
            "ts": "2026-04-23T00:00:00Z"
        }),
    );
}

#[test]
fn colsnap_viewer_sensor() {
    viewer(
        "viewer_sensor",
        "substrate/n-1/sensor/mic",
        json!({ "kind": "mic", "raw": { "frame": 1 }, "summary": { "rms_db": -41.2 } }),
    );
}

#[test]
fn colsnap_viewer_waveform() {
    viewer(
        "viewer_waveform",
        "substrate/n-1/sensor/wave",
        json!({ "samples": sine(256), "sample_rate": 16000 }),
    );
}

#[test]
fn colsnap_viewer_audio_meter() {
    viewer(
        "viewer_audio_meter",
        "substrate/sensor/mic",
        json!({ "rms_db": -41.2, "peak_db": -17.1, "available": true, "sample_rate": 16000, "tick": 214 }),
    );
}

#[test]
fn colsnap_viewer_connection_badge() {
    for s in ["connected", "disconnected", "absent"] {
        viewer(
            match s {
                "connected" => "viewer_conn_connected",
                "disconnected" => "viewer_conn_disconnected",
                _ => "viewer_conn_absent",
            },
            "substrate/network/wifi",
            json!({ "state": s, "ssid": "home-wifi", "rssi": -47 }),
        );
    }
}

#[test]
fn colsnap_viewer_depth_map() {
    let depths: Vec<u32> = (0..64)
        .map(|i| if i % 11 == 0 { 65535 } else { 200 + i * 40 })
        .collect();
    viewer(
        "viewer_depth_map",
        "substrate/sensor/tof",
        json!({ "width": 8, "height": 8, "depths_mm": depths }),
    );
}

#[test]
fn colsnap_viewer_process_table() {
    viewer(
        "viewer_process_table",
        "substrate/kernel/processes",
        json!([
            { "pid": 1, "agent_id": "kernel", "state": "running", "memory_bytes": 1_048_576_u64, "cpu_time_ms": 1200_u64, "parent_pid": null },
            { "pid": 2, "agent_id": "mic-capture", "state": "sleeping", "memory_bytes": 524288_u64, "cpu_time_ms": 80_u64, "parent_pid": 1 },
            { "pid": 3, "agent_id": "worker", "state": "exited", "memory_bytes": 0_u64, "cpu_time_ms": 5_u64, "parent_pid": 1 }
        ]),
    );
}

#[test]
fn colsnap_viewer_chain_tail() {
    viewer(
        "viewer_chain_tail",
        "substrate/chain/tail",
        json!([
            { "seq": 1, "ts": "2026-04-23T00:00:00Z", "kind": "tick" },
            { "seq": 2, "ts": "2026-04-23T00:00:01Z", "kind": "gate.deny", "detail": "x" },
            { "seq": 3, "ts": "2026-04-23T00:00:02Z", "kind": "node.register" }
        ]),
    );
}

#[test]
fn colsnap_viewer_json_fallback() {
    viewer(
        "viewer_json_fallback",
        "substrate/misc/blob",
        json!({ "alpha": 1, "beta": [true, null, "text"], "gamma": { "deep": 2.5 } }),
    );
}

#[test]
fn colsnap_viewer_time_series() {
    // The sample history is process-global, so render once.
    render_once("viewer_time_series", setup, vec![], |ui: &mut egui::Ui| {
        viewers::dispatch(ui, "substrate/sensor/temp", &json!(21.5))
    });
}

#[test]
fn colsnap_viewer_pcm_chunk() {
    use base64::Engine;
    let bytes: Vec<u8> = (0..512u32)
        .flat_map(|i| (((i as f32 * 0.2).sin() * 12000.0) as i16).to_le_bytes())
        .collect();
    let data = base64::engine::general_purpose::STANDARD.encode(bytes);
    viewer(
        "viewer_pcm_chunk",
        "substrate/sensor/pcm",
        json!({ "data": data, "sample_rate": 16000, "format": "i16le", "encoding": "base64" }),
    );
}

#[test]
fn colsnap_conversation_graph_fixture() {
    render_checked("conversation_graph", setup, vec![], || {
        let mut p = ConversationGraphPanel::with_fixture();
        move |ui: &mut egui::Ui| p.show(ui)
    });
}

#[test]
fn colsnap_conversation_graph_ecc_off() {
    render_checked("conversation_graph_ecc_off", setup, vec![], || {
        let mut p = ConversationGraphPanel::default();
        move |ui: &mut egui::Ui| p.show(ui)
    });
}

#[test]
fn colsnap_boot_splash_hold_frame() {
    // Elapsed 1.5 s sits in the BOOT_HOLD plateau (alpha 1.0), so wall-clock
    // jitter cannot change a vertex colour. `sfx_played = true` keeps the
    // scuttle sound out of the test.
    render_checked("boot_splash_hold", setup, vec![], || {
        let started = web_time::Instant::now() - std::time::Duration::from_millis(1500);
        move |ui: &mut egui::Ui| {
            let mut sfx = true;
            let _ = boot::show(ui, started, &mut sfx);
        }
    });
}

#[test]
fn colsnap_desktop_grid_wallpaper() {
    render_checked("desktop_grid", setup, vec![], || {
        move |ui: &mut egui::Ui| grid::paint(ui, full_rect(), 1.0)
    });
}

#[allow(clippy::field_reassign_with_default)]
fn busy_snapshot() -> Snapshot {
    let mut s = Snapshot::default();
    s.connection = Connection::Connected;
    s.status = Some(json!({ "state": "healthy", "uptime_ms": 3_600_000u64 }));
    s.mesh_status = Some(json!({ "total_nodes": 4, "healthy_nodes": 3 }));
    s.chain_status = Some(json!({ "available": true, "chain_id": "c", "sequence": 9 }));
    s.network_wifi = Some(json!({ "state": "connected", "iface": "wlan0" }));
    s.network_battery = Some(json!({ "present": true, "percent": 18, "charging": false }));
    s.bluetooth = Some(json!({ "present": true, "enabled": true }));
    s.tick = 7;
    s
}

#[test]
fn colsnap_desktop_tray_connected() {
    render_checked("desktop_tray_connected", setup, vec![], || {
        let snap = busy_snapshot();
        let mut launcher = false;
        let mut open: Option<tray::ChipId> = None;
        move |ui: &mut egui::Ui| tray::paint(ui, full_rect(), &snap, &mut launcher, &mut open)
    });
}

#[test]
fn colsnap_desktop_tray_disconnected() {
    render_checked("desktop_tray_disconnected", setup, vec![], || {
        let snap = Snapshot {
            connection: Connection::Disconnected,
            last_error: Some("daemon unreachable".into()),
            ..Default::default()
        };
        let mut launcher = true;
        let mut open: Option<tray::ChipId> = Some(tray::ChipId::Kernel);
        move |ui: &mut egui::Ui| tray::paint(ui, full_rect(), &snap, &mut launcher, &mut open)
    });
}

#[test]
fn colsnap_desktop_sidebar() {
    render_checked("desktop_sidebar", setup, vec![], || {
        let snap = busy_snapshot();
        let sb = sidebar::Sidebar::default();
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(240.0, 800.0));
        move |ui: &mut egui::Ui| {
            let _ = sidebar::paint(ui, rect, &sb, &snap);
        }
    });
}

#[test]
fn colsnap_desktop_workspace_demo_wall() {
    render_checked("desktop_workspace", setup, vec![], || {
        let mut ws = workspace::AgentWorkspace::new();
        // `new()` loads (and later saves) a layout file from the user's data
        // dir; start from an empty window manager so the surface cannot depend
        // on, or write to, that file.
        ws.wm = clawft_gui_egui::shell::window_manager::WindowManager::new();
        move |ui: &mut egui::Ui| workspace::show(ui, full_rect(), &mut ws)
    });
}

/// Stock egui widgets under the WeftOS `Visuals`/`Style`, so a change to
/// `theming::apply` (hover/active/selection/stroke colours) is caught even
/// when no WeftOS surface sets those colours explicitly.
#[test]
fn colsnap_themed_widget_gallery() {
    render_checked("themed_widgets", setup, vec![], || {
        let mut text = String::from("editable text");
        let mut on = true;
        let mut val = 0.4_f32;
        let mut sel = 1usize;
        move |ui: &mut egui::Ui| {
            ui.heading("Heading");
            ui.label("label");
            ui.weak("weak label");
            ui.hyperlink_to("link", "https://example.invalid");
            ui.separator();
            let _ = ui.button("button");
            let _ = ui.add_enabled(false, egui::Button::new("disabled"));
            ui.checkbox(&mut on, "checkbox");
            ui.radio_value(&mut sel, 1, "radio on");
            ui.radio_value(&mut sel, 2, "radio off");
            ui.add(egui::Slider::new(&mut val, 0.0..=1.0));
            ui.text_edit_singleline(&mut text);
            let _ = ui.selectable_label(true, "selected");
            let _ = ui.selectable_label(false, "unselected");
            ui.add(egui::ProgressBar::new(0.6).text("60%"));
            ui.collapsing("collapsing", |ui| {
                ui.label("inside");
            });
            egui::Grid::new("g").striped(true).show(ui, |ui| {
                ui.label("a");
                ui.label("b");
                ui.end_row();
                ui.label("c");
                ui.label("d");
                ui.end_row();
            });
        }
    });
}
