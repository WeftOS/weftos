use eframe::egui;

use crate::live::{Connection, Snapshot};

pub fn show(ui: &mut egui::Ui, snap: &Snapshot) {
    ui.heading("Native GUI spike — egui / eframe");
    ui.add_space(4.0);
    ui.label(
        "Ports the 12 core UI blocks from gui/src/blocks/*.tsx into Rust-native \
         egui widgets. Status/Code/Table/Tree/Terminal are wired to the live \
         kernel daemon; the rest still use mock data.",
    );
    ui.add_space(12.0);

    ui.group(|ui| {
        ui.label(egui::RichText::new("Daemon link").strong());
        match snap.connection {
            Connection::Connecting => {
                ui.label("Connecting to kernel daemon…");
            }
            Connection::Connected => {
                ui.label(
                    egui::RichText::new("Connected — polling kernel.status every 1s")
                        .color(crate::theming::palette::BLOCK_TEXT_OK),
                );
                if let Some(status) = &snap.status {
                    let state = status.get("state").and_then(|v| v.as_str()).unwrap_or("?");
                    let uptime = status
                        .get("uptime_secs")
                        .and_then(|v| v.as_f64())
                        .unwrap_or(0.0);
                    let procs = status
                        .get("process_count")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let svcs = status
                        .get("service_count")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    ui.label(format!(
                        "state={state}  uptime={}  processes={procs}  services={svcs}",
                        fmt_duration(uptime)
                    ));
                }
                ui.label(format!("tick #{}", snap.tick));
            }
            Connection::Disconnected => {
                ui.label(
                    egui::RichText::new("Offline — is the daemon running?")
                        .color(crate::theming::palette::SHELL_ERR_SOFT),
                );
                ui.label("Start it with: weaver kernel start");
            }
        }
        if let Some(err) = &snap.last_error {
            ui.label(
                egui::RichText::new(format!("last error: {err}"))
                    .small()
                    .weak(),
            );
        }
    });
}

fn fmt_duration(secs: f64) -> String {
    let s = secs as u64;
    let h = s / 3600;
    let m = (s % 3600) / 60;
    let sec = s % 60;
    if h > 0 {
        format!("{h}h{m}m{sec}s")
    } else if m > 0 {
        format!("{m}m{sec}s")
    } else {
        format!("{sec}s")
    }
}
