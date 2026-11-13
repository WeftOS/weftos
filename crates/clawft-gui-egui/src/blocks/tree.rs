use std::collections::BTreeMap;

use eframe::egui;

use super::DemoState;
use crate::live::Snapshot;

pub fn show(ui: &mut egui::Ui, state: &mut DemoState, snap: &Snapshot) {
    ui.heading("Tree — Services");
    ui.label("Live service registry grouped by service_type.");
    ui.separator();

    let Some(services) = &snap.services else {
        ui.label("daemon offline — no service list");
        return;
    };

    if services.is_empty() {
        ui.label("no services registered");
        return;
    }

    // Group: service_type → [(name, health)].
    let mut buckets: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for s in services {
        let stype = s
            .get("service_type")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string();
        let name = s
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string();
        let health = s
            .get("health")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string();
        buckets.entry(stype).or_default().push((name, health));
    }

    for (stype, entries) in &buckets {
        // Persistent open/close using a leaked &'static key is impractical;
        // reuse the existing tree_open set with owned keys coerced via
        // Box::leak is overkill — just use CollapsingHeader which manages
        // its own state via id_source.
        egui::CollapsingHeader::new(format!("{stype} ({})", entries.len()))
            .default_open(true)
            .id_salt(format!("svc-{stype}"))
            .show(ui, |ui| {
                for (name, health) in entries {
                    ui.horizontal(|ui| {
                        let color = match health.as_str() {
                            "registered" | "healthy" | "ok" => {
                                crate::theming::palette::BLOCK_TEXT_OK
                            }
                            "warn" | "warning" => crate::theming::palette::BLOCK_TEXT_WARN,
                            "error" | "unhealthy" => crate::theming::palette::BLOCK_TEXT_ERR,
                            _ => egui::Color32::GRAY,
                        };
                        let (rect, _) =
                            ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
                        ui.painter().circle_filled(rect.center(), 4.0, color);
                        ui.monospace(name);
                        ui.label(egui::RichText::new(format!("({health})")).weak().small());
                    });
                }
            });
    }

    // state is unused for real-data tree but keep the arg for API symmetry.
    let _ = state;
}
