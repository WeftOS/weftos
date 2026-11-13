use eframe::egui;

use crate::live::Snapshot;

pub fn show(ui: &mut egui::Ui, snap: &Snapshot) {
    ui.heading("Status");
    ui.label("Live metric cards — pulled from `kernel.status` each tick.");
    ui.separator();

    let (state, uptime, procs, max_procs, svcs, health_interval) = match &snap.status {
        Some(s) => (
            s.get("state")
                .and_then(|v| v.as_str())
                .unwrap_or("—")
                .to_string(),
            s.get("uptime_secs").and_then(|v| v.as_f64()).unwrap_or(0.0),
            s.get("process_count").and_then(|v| v.as_u64()).unwrap_or(0),
            s.get("max_processes").and_then(|v| v.as_u64()).unwrap_or(0),
            s.get("service_count").and_then(|v| v.as_u64()).unwrap_or(0),
            s.get("health_check_interval_secs")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
        ),
        None => ("offline".into(), 0.0, 0, 0, 0, 0),
    };

    ui.horizontal_wrapped(|ui| {
        card(ui, "Kernel state", &state, "", kind_for_state(&state));
        card(ui, "Uptime", &fmt_duration(uptime), "", Kind::Ok);
        let pct = if max_procs > 0 {
            procs as f32 / max_procs as f32
        } else {
            0.0
        };
        card(
            ui,
            "Processes",
            &format!("{}/{}", procs, max_procs),
            "",
            if pct >= 0.9 {
                Kind::Crit
            } else if pct >= 0.7 {
                Kind::Warn
            } else {
                Kind::Ok
            },
        );
        card(ui, "Services", &svcs.to_string(), "", Kind::Ok);
        card(
            ui,
            "Health check",
            &health_interval.to_string(),
            "s",
            Kind::Ok,
        );

        // Poller freshness — tick number, age of the last poll, and RTT.
        let (value, unit, kind) = match snap.last_tick_at_ms {
            Some(t) => {
                let age_ms = (crate::live::now_ms() - t).max(0.0) as u64;
                let kind = if age_ms > 5_000 {
                    Kind::Crit
                } else if age_ms > 2_000 {
                    Kind::Warn
                } else {
                    Kind::Ok
                };
                (
                    format!("#{} · {age_ms}ms ago", snap.tick),
                    String::new(),
                    kind,
                )
            }
            None => ("—".to_string(), String::new(), Kind::Warn),
        };
        card(ui, "Poll", &value, &unit, kind);
        if let Some(d) = snap.last_tick_dur_ms {
            card(ui, "Poll RTT", &format!("{}", d as u64), "ms", Kind::Ok);
        }
    });
}

#[derive(Copy, Clone)]
enum Kind {
    Ok,
    Warn,
    Crit,
}

fn kind_for_state(s: &str) -> Kind {
    match s {
        "running" => Kind::Ok,
        "booting" | "shutting_down" => Kind::Warn,
        _ => Kind::Crit,
    }
}

fn card(ui: &mut egui::Ui, label: &str, value: &str, unit: &str, kind: Kind) {
    let (border, text) = match kind {
        Kind::Ok => (
            crate::theming::palette::BLOCK_BAR_OK,
            crate::theming::palette::BLOCK_TEXT_OK,
        ),
        Kind::Warn => (
            crate::theming::palette::BLOCK_BAR_WARN,
            crate::theming::palette::BLOCK_TEXT_WARN,
        ),
        Kind::Crit => (
            crate::theming::palette::BLOCK_BAR_ERR,
            crate::theming::palette::BLOCK_TEXT_ERR,
        ),
    };
    egui::Frame::new()
        .fill(egui::Color32::from_gray(22))
        .stroke(egui::Stroke::new(1.0, border))
        .corner_radius(6.0)
        .inner_margin(egui::Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.set_min_width(140.0);
            ui.label(egui::RichText::new(label).small().weak());
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(value)
                        .monospace()
                        .size(22.0)
                        .color(text),
                );
                if !unit.is_empty() {
                    ui.label(egui::RichText::new(unit).small().weak());
                }
            });
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
