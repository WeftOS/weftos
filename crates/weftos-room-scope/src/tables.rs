//! Side-panel tables: tracker accuracy per marker (per backend) and the CSI agreement / hold-out
//! false-alarm summaries.

use eframe::egui::{self, RichText};
use serde_json::Value;

use crate::RoomScope;
use crate::panels::f2;
use crate::view::WARN;

fn err_of(row: &Value, key: &str) -> Option<f64> {
    row[key]["mean_err_m"].as_f64()
}

pub fn accuracy(s: &RoomScope, ui: &mut egui::Ui) {
    let Some(l) = s.loaded.as_ref() else { return };
    ui.label(
        RichText::new("Tracker accuracy at markers (still phases, corrected homography)").strong(),
    );
    if l.evals
        .values()
        .all(|e| e["stations"].as_array().is_none_or(|a| a.is_empty()))
    {
        let pf = s.track(l).and_then(|t| t.present_frac);
        ui.label(format!(
            "No station steps in this run. Tracker detections: {}",
            f2(pf.map(|p| p * 100.0), 2) + "% of frames"
        ));
        return;
    }
    let ids: Vec<&String> = l.evals.keys().collect();
    egui::Grid::new("acc").striped(true).show(ui, |ui| {
        ui.label("step");
        ui.label("marker");
        for id in &ids {
            ui.label(RichText::new(id.as_str()).strong());
        }
        ui.label("mirrored H (cpu-blob)");
        ui.end_row();
        let base = l
            .evals
            .get("cpu-blob")
            .or_else(|| l.evals.values().next())
            .cloned()
            .unwrap_or(Value::Null);
        for (i, row) in base["stations"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .enumerate()
        {
            ui.label(row["label"].as_str().unwrap_or(""));
            let m = row["marker"].as_str().unwrap_or("");
            let cm = row["cue_marker"]
                .as_str()
                .filter(|c| *c != m)
                .map(|c| format!(" (cue M{c})"))
                .unwrap_or_default();
            ui.label(format!("M{m}{cm}"));
            for id in &ids {
                let r = &l.evals[*id]["stations"][i];
                ui.label(format!(
                    "{} m  ({}/{})",
                    f2(err_of(r, "corrected/floor_px"), 2),
                    r["n"],
                    r["n_frames"]
                ));
            }
            ui.label(format!("{} m", f2(err_of(row, "mirrored/floor_px"), 2)));
            ui.end_row();
        }
    });
    if let Some(n) = &l.meta.deviation_note {
        ui.label(RichText::new(n).small().color(WARN));
    }
}

pub fn csi_tables(s: &RoomScope, ui: &mut egui::Ui) {
    let Some(sm) = &s.summary else { return };
    ui.label(
        RichText::new("Agreement: z vs distance to the node's link (this run, sync 0)").strong(),
    );
    match sm["agreement"][&s.run].as_object() {
        Some(ag) => {
            let mut rows: Vec<(&String, &Value)> = ag.iter().collect();
            rows.sort_by_key(|(k, _)| k.parse::<i64>().unwrap_or(0));
            egui::Grid::new("agree").striped(true).show(ui, |ui| {
                for h in ["node", "r", "rho", "n", "z in view", "z out"] {
                    ui.label(h);
                }
                ui.end_row();
                for (nid, r) in rows {
                    ui.label(format!("n{nid}"));
                    ui.label(f2(r["pearson_z_vs_link_dist"].as_f64(), 2));
                    ui.label(f2(r["spearman_z_vs_link_dist"].as_f64(), 2));
                    ui.label(r["n_pairs"].to_string());
                    ui.label(f2(r["mean_z_person_in_view"].as_f64(), 2));
                    ui.label(f2(r["mean_z_out_phase"].as_f64(), 2));
                    ui.end_row();
                }
            });
            ui.label(RichText::new("Negative r = z rises as the person nears the link (expected direction). MEASURED, this run only.").small().weak());
        }
        None => {
            ui.label(RichText::new("no person in this run: no agreement metric").weak());
        }
    }
    ui.label(RichText::new("Hold-out false-alarm rate (empty run, z > 3, 2 Hz windows)").strong());
    if let Some(fa) = sm["holdout_false_alarm"].as_object() {
        let mut rows: Vec<(&String, &Value)> = fa.iter().collect();
        rows.sort_by_key(|(k, _)| k.parse::<i64>().unwrap_or(0));
        egui::Grid::new("fa").striped(true).show(ui, |ui| {
            for h in [
                "node",
                "hold gated",
                "n ungated",
                "FA z>3",
                "FA |z|>3",
                "FA z>3 incl. excl.",
            ] {
                ui.label(h);
            }
            ui.end_row();
            for (nid, r) in rows {
                ui.label(format!("n{nid}"));
                ui.label(f2(r["hold_gated_frac"].as_f64().map(|v| v * 100.0), 0) + "%");
                ui.label(r["n_hold_ungated"].to_string());
                ui.label(f2(r["fa_rate_z_gt_3"].as_f64().map(|v| v * 100.0), 1) + "%");
                ui.label(f2(r["fa_rate_abs_z_gt_3"].as_f64().map(|v| v * 100.0), 1) + "%");
                let incl = &sm["holdout_false_alarm_incl_excluded"][nid.as_str()]["fa_rate_z_gt_3"];
                ui.label(f2(incl.as_f64().map(|v| v * 100.0), 1) + "%");
                ui.end_row();
            }
        });
    }
    if let Some(ex) = sm["exclusions"].as_object() {
        let w: Vec<String> = ex["windows_video_s"]
            .as_array()
            .map(|a| a.iter().map(|p| format!("{}–{} s", p[0], p[1])).collect())
            .unwrap_or_default();
        ui.label(
            RichText::new(format!(
                "Excluded from baseline and hold-out: video {} (±{} s): {}. Last column = without the exclusion.",
                w.join(", "),
                ex["pad_s"],
                ex["reason"].as_str().unwrap_or("")
            ))
            .small()
            .weak(),
        );
    }
}
