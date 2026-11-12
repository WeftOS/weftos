//! In-crate colour-snapshot tests for the desktop apps (`apps::*::show`).
//!
//! They need `Desktop` / `Live` state that is crate-private to construct, so
//! they live here rather than in `tests/colour_snapshot.rs`. Same output
//! contract: `GUI_SNAP_OUT=<dir>` writes one text dump per surface. The
//! private-field surfaces (terminals, chat panel) have their own `colsnap_*`
//! tests next to the code. See `docs/development/gui-colour-snapshot.md`.

use std::sync::Arc;

use serde_json::json;

use crate::live::{Connection, Live, Snapshot};
use crate::shell::desktop::Desktop;
use crate::shell::sidebar::{AppsTab, LogsTab, NetworkTab};
use crate::snap_support::{SCREEN, render_checked, render_checked_colours};

fn setup(ctx: &egui::Context) {
    crate::theming::apply(ctx);
}

fn rect() -> egui::Rect {
    egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(SCREEN.0, SCREEN.1))
}

#[allow(clippy::field_reassign_with_default)]
fn snapshot() -> Snapshot {
    let mut s = Snapshot::default();
    s.connection = Connection::Connected;
    s.status = Some(json!({
        "state": "healthy", "uptime_ms": 3_600_000u64,
        "process_count": 3, "service_count": 3
    }));
    s.services = Some(vec![
        json!({ "name": "mesh-listener", "status": "healthy", "state": "running", "cpu_percent": 42.0, "restarts": 0 }),
        json!({ "name": "rpc-gateway", "status": "healthy", "state": "running", "cpu_percent": 9.0, "restarts": 1 }),
        json!({ "name": "audit-sink", "status": "at_risk", "state": "stopped", "cpu_percent": 88.0, "restarts": 4 }),
    ]);
    s.processes = Some(vec![
        json!({ "pid": 1, "agent_id": "kernel", "state": "running", "memory_bytes": 1_048_576u64, "cpu_time_ms": 1200u64 }),
        json!({ "pid": 2, "agent_id": "mic-capture", "state": "sleeping", "memory_bytes": 524_288u64, "cpu_time_ms": 80u64 }),
    ]);
    s.logs = Some(vec![
        json!({ "level": "info", "msg": "boot ok" }),
        json!({ "level": "warn", "msg": "audit-sink slow" }),
        json!({ "level": "error", "msg": "gate denied" }),
    ]);
    s.mesh_status = Some(json!({ "total_nodes": 4, "healthy_nodes": 3 }));
    s.chain_status = Some(json!({
        "available": true, "chain_id": "c0", "sequence": 9, "event_count": 120
    }));
    s.network_wifi = Some(json!({ "state": "connected", "iface": "wlan0" }));
    s.network_ethernet = Some(json!({ "state": "disconnected" }));
    s.network_battery = Some(json!({ "present": true, "percent": 18, "charging": false }));
    s.bluetooth = Some(json!({ "present": true, "enabled": true }));
    s.audio_mic = Some(json!({
        "available": true, "rms_db": -41.2, "peak_db": -17.1, "sample_rate": 16000
    }));
    s.cron_jobs = Some(vec![
        json!({ "id": "j1", "name": "nightly", "command": "backup", "interval_secs": 3600,
                "enabled": true, "fire_count": 3 }),
        json!({ "id": "j2", "name": "paused", "command": "scan", "interval_secs": 60,
                "enabled": false, "fire_count": 0 }),
    ]);
    s.ecc_status = Some(json!({ "enabled": true, "hnsw_entries": 12, "crossref_count": 3 }));
    s.last_error = Some("example error line".into());
    s.tick = 7;
    s
}

fn desk() -> Desktop {
    let dir = std::env::temp_dir().join(format!("colsnap-registry-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("registry dir");
    Desktop::with_registry_path(dir.join("apps.json"))
}

fn live() -> Arc<Live> {
    Live::test_stub(snapshot())
}

macro_rules! app_test {
    ($test:ident, $name:expr, |$ui:ident, $d:ident, $l:ident, $s:ident| $body:expr) => {
        app_test!($test, $name, render_checked, |$ui, $d, $l, $s| $body);
    };
    ($test:ident, $name:expr, $render:ident, |$ui:ident, $d:ident, $l:ident, $s:ident| $body:expr) => {
        #[test]
        fn $test() {
            let l = live();
            let snap = snapshot();
            $render($name, setup, vec![], || {
                let mut $d = desk();
                let $l = l.clone();
                let $s = snap.clone();
                move |$ui: &mut egui::Ui| $body
            });
        }
    };
}

app_test!(colsnap_app_admin, "app_admin", |ui, d, l, s| {
    crate::apps::admin::show(ui, rect(), &mut d, &l, &s)
});
// Conversation ids embed `now_ms()` and a process-global counter, so only the
// colour view is stable between two runs.
app_test!(
    colsnap_app_chat,
    "app_chat",
    render_checked_colours,
    |ui, d, l, s| { crate::apps::chat::show(ui, rect(), &mut d, &l, &s) }
);
app_test!(colsnap_app_explorer, "app_explorer", |ui, d, l, s| {
    crate::apps::explorer::show(ui, rect(), &mut d, &l, &s)
});
app_test!(colsnap_app_files, "app_files", |ui, d, l, s| {
    crate::apps::files::show(ui, rect(), &mut d, &l, &s)
});
app_test!(colsnap_app_monitor, "app_monitor", |ui, d, l, s| {
    crate::apps::monitor::show(ui, rect(), &mut d, &l, &s)
});
app_test!(colsnap_app_processes, "app_processes", |ui, d, l, s| {
    crate::apps::processes::show(ui, rect(), &mut d, &l, &s)
});
app_test!(colsnap_app_services, "app_services", |ui, d, l, s| {
    crate::apps::services::show(ui, rect(), &mut d, &l, &s)
});
app_test!(colsnap_app_scheduler, "app_scheduler", |ui, d, l, s| {
    crate::apps::scheduler::show(ui, rect(), &mut d, &l, &s)
});
app_test!(colsnap_app_settings, "app_settings", |ui, d, l, s| {
    crate::apps::settings::show(ui, rect(), &mut d, &l, &s)
});
app_test!(colsnap_app_terminal, "app_terminal", |ui, d, l, s| {
    crate::apps::terminal::show(ui, rect(), &mut d, &l, &s)
});
app_test!(colsnap_app_logs_system, "app_logs_system", |ui, d, l, s| {
    crate::apps::logs::show(ui, rect(), &mut d, &l, &s, LogsTab::System)
});
app_test!(
    colsnap_app_logs_witness,
    "app_logs_witness",
    |ui, d, l, s| { crate::apps::logs::show(ui, rect(), &mut d, &l, &s, LogsTab::WitnessChain) }
);
app_test!(
    colsnap_app_launcher_builtin,
    "app_launcher_builtin",
    |ui, d, l, s| { crate::apps::launcher::show(ui, rect(), &mut d, &l, &s, AppsTab::BuiltIn) }
);
app_test!(
    colsnap_app_launcher_installed,
    "app_launcher_installed",
    |ui, d, l, s| { crate::apps::launcher::show(ui, rect(), &mut d, &l, &s, AppsTab::Installed) }
);
app_test!(
    colsnap_app_launcher_developer,
    "app_launcher_developer",
    |ui, d, l, s| { crate::apps::launcher::show(ui, rect(), &mut d, &l, &s, AppsTab::Developer) }
);
app_test!(
    colsnap_app_network_mesh,
    "app_network_mesh",
    |ui, d, l, s| { crate::apps::network::show(ui, rect(), &mut d, &l, &s, NetworkTab::Mesh) }
);
app_test!(
    colsnap_app_network_wifi,
    "app_network_wifi",
    |ui, d, l, s| { crate::apps::network::show(ui, rect(), &mut d, &l, &s, NetworkTab::WiFi) }
);
app_test!(
    colsnap_app_network_bluetooth,
    "app_network_bluetooth",
    |ui, d, l, s| { crate::apps::network::show(ui, rect(), &mut d, &l, &s, NetworkTab::Bluetooth) }
);
