//! Top bar, side panel (run / backend / homography / layers / sync, per-node z bars, person readout,
//! accuracy and CSI tables, room plan) and the time-series plots with a scrubbable playhead.

use eframe::egui::{self, Color32, RichText};
use egui_plot::{Legend, Line, Plot, PlotPoint, PlotPoints, Text, VLine};

use crate::RoomScope;
use crate::model;
use crate::view::{WARN, z_color};

// Status colours, the same values the Cogs companion apps use.
const GREEN: Color32 = Color32::from_rgb(70, 190, 110);
const AMBER: Color32 = Color32::from_rgb(230, 170, 40);
const GREY: Color32 = Color32::from_rgb(140, 140, 140);

const NODE_COLORS: [Color32; 8] = [
    Color32::from_rgb(255, 99, 71),
    Color32::from_rgb(65, 170, 255),
    Color32::from_rgb(255, 200, 40),
    Color32::from_rgb(120, 220, 120),
    Color32::from_rgb(220, 120, 255),
    Color32::from_rgb(0, 210, 200),
    Color32::from_rgb(255, 150, 200),
    Color32::from_rgb(200, 200, 200),
];

pub fn fmt_t(s: f64) -> String {
    let m = (s / 60.0).floor();
    format!("{}:{:04.1}", m as i64, s - 60.0 * m)
}

pub fn f2(v: Option<f64>, d: usize) -> String {
    v.map_or("–".into(), |x| format!("{x:.d$}"))
}

pub fn top_bar(s: &mut RoomScope, ui: &mut egui::Ui) {
    ui.horizontal_wrapped(|ui| {
        ui.strong("weft-room-scope");
        ui.separator();
        let mut run = s.run.clone();
        egui::ComboBox::from_id_salt("run")
            .selected_text(&run)
            .show_ui(ui, |ui| {
                for r in &s.runs {
                    ui.selectable_value(&mut run, r.clone(), r);
                }
            });
        if run != s.run {
            s.run = run;
            s.t = 0.0;
            s.loaded = None;
            s.error = None;
        }
        let ids: Vec<String> = s
            .loaded
            .as_ref()
            .map(|l| l.tracks.iter().map(|t| t.0.clone()).collect())
            .unwrap_or_default();
        ui.label("tracker");
        egui::ComboBox::from_id_salt("backend")
            .selected_text(&s.backend)
            .show_ui(ui, |ui| {
                for id in &ids {
                    ui.selectable_value(&mut s.backend, id.clone(), id);
                }
            });
        ui.label("homography");
        let before = s.hsel.clone();
        egui::ComboBox::from_id_salt("hsel")
            .selected_text(&s.hsel)
            .show_ui(ui, |ui| {
                if let Some(l) = &s.loaded {
                    for k in l.meta.homographies.keys() {
                        ui.selectable_value(&mut s.hsel, k.clone(), k);
                    }
                }
            });
        if s.hsel != before {
            s.set_h();
        }
        ui.separator();
        if ui
            .button(if s.playing { "Pause" } else { "Play" })
            .clicked()
        {
            s.playing = !s.playing;
        }
        if ui.button("-1 s").clicked() {
            s.t = (s.t - 1.0).max(0.0);
        }
        if ui.button("+1 s").clicked() {
            s.t += 1.0;
        }
        egui::ComboBox::from_id_salt("rate")
            .selected_text(format!("{}x", s.rate))
            .width(50.0)
            .show_ui(ui, |ui| {
                for r in [0.5, 1.0, 2.0, 4.0, 8.0] {
                    ui.selectable_value(&mut s.rate, r, format!("{r}x"));
                }
            });
        if let Some(l) = &s.loaded {
            let ep = s.epoch(l);
            ui.label(
                RichText::new(format!(
                    "video {} / {}   epoch {:.2}",
                    fmt_t(s.t),
                    fmt_t(l.meta.duration),
                    ep
                ))
                .monospace(),
            );
        }
    });
}

pub fn side(s: &mut RoomScope, ui: &mut egui::Ui) {
    if s.loaded.is_none() {
        return;
    }
    ui.horizontal_wrapped(|ui| {
        ui.label("Layers");
        ui.checkbox(&mut s.layers.cam, "1 camera");
        ui.checkbox(&mut s.layers.grid, "1 grid/markers");
        ui.checkbox(&mut s.layers.site, "1 walls/nodes/links");
        ui.checkbox(&mut s.layers.track, "2 person");
        ui.checkbox(&mut s.layers.csi, "3 CSI links");
        ui.checkbox(&mut s.layers.heat, "3 heat on camera");
    });
    ui.horizontal(|ui| {
        ui.label("Sync offset");
        let r = ui.add(
            egui::Slider::new(&mut s.offset, -10.0..=10.0)
                .step_by(0.1)
                .suffix(" s"),
        );
        if ui.small_button("0").clicked() {
            s.offset = 0.0;
        }
        if r.changed() || r.drag_stopped() {
            s.offsets.insert(s.run.clone(), s.offset);
        }
        if r.drag_stopped() || (r.changed() && !r.dragged()) {
            s.save_offsets();
        }
    });
    ui.label(RichText::new("Video origin = START cue - 3 s; the offset is added to video time to get wall-clock. Align on the clap.").small().weak());
    let Some(l) = s.loaded.as_ref() else { return };
    let tr = s.track(l);
    let be = tr.map_or("none", |t| t.backend_id());
    ui.label(
        RichText::new(format!("Track backend: {be}"))
            .strong()
            .color(GREEN),
    );
    if let Some(tr) = tr {
        for n in &tr.notes {
            ui.label(RichText::new(n).small().weak());
        }
    }
    ui.separator();
    plan_and_bars(s, ui);
    ui.separator();
    person(s, ui);
    ui.separator();
    crate::tables::accuracy(s, ui);
    ui.separator();
    crate::tables::csi_tables(s, ui);
    ui.separator();
    ui.label(RichText::new("All values ESTIMATED / uncalibrated. Floor mapping is a planar 4-marker homography (lens not undistorted). Front/back is a heuristic unless the backend says keypoints. CSI z is uncalibrated per-node motion band power deviation; the heat is not a localisation.").small().color(WARN));
}

fn plan_and_bars(s: &RoomScope, ui: &mut egui::Ui) {
    let Some(l) = s.loaded.as_ref() else { return };
    let k = l.csi.as_ref().and_then(|c| c.index(s.epoch(l)));
    let tr = s.track(l);
    let hit = tr.and_then(|t| t.at(s.t, 0.25));
    let trail: Vec<[f64; 2]> = match (tr, hit) {
        (Some(t), Some((i, _))) => t.track[i.saturating_sub(50)..=i]
            .iter()
            .rev()
            .map_while(|f| s.person_xy(l, f))
            .collect(),
        _ => Vec::new(),
    };
    let cur = hit.and_then(|(_, f)| s.person_xy(l, f).map(|xy| (f, xy)));
    ui.label(RichText::new("Room plan (north up, metres)").strong());
    crate::plan::show(
        ui,
        &l.meta,
        &l.proj,
        l.csi.as_ref().filter(|_| s.layers.csi),
        k,
        &trail,
        cur,
    );
    ui.label(RichText::new("Per-node z (robust z vs the empty-run calibration window)").strong());
    let Some(csi) = &l.csi else {
        ui.label("no CSI data for this run");
        return;
    };
    for nid in csi.node_ids() {
        let nd = &csi.nodes[&nid];
        let z = k.and_then(|k| nd.z.get(k).copied().flatten());
        let gate = k.map_or(Some("no-data".to_string()), |k| {
            nd.gate.get(k).cloned().flatten()
        });
        let fps = k.and_then(|k| nd.fps.get(k).copied().flatten());
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!(
                    "n{nid} {} @{}",
                    nd.board.as_deref().unwrap_or("").replace("ESP32-", ""),
                    nd.spot.as_deref().unwrap_or("?")
                ))
                .monospace(),
            );
            let (rect, _) = ui.allocate_exact_size(egui::vec2(150.0, 12.0), egui::Sense::hover());
            let p = ui.painter_at(rect);
            p.rect_filled(rect, 0.0, Color32::from_gray(45));
            let x =
                |z: f64| rect.left() + ((z.clamp(-3.0, 9.0) + 3.0) / 12.0) as f32 * rect.width();
            if let Some(z) = z {
                let (a, b) = (x(0.0), x(z));
                let c = if gate.is_some() {
                    Color32::from_gray(100)
                } else {
                    z_color(Some(z))
                };
                p.rect_filled(
                    egui::Rect::from_x_y_ranges(a.min(b)..=a.max(b), rect.y_range()),
                    0.0,
                    c,
                );
            }
            p.vline(
                x(0.0),
                rect.y_range(),
                egui::Stroke::new(1.0, Color32::GRAY),
            );
            let txt = format!("z {} · {} fps", f2(z, 1), f2(fps, 1));
            match gate {
                Some(g) => ui.label(
                    RichText::new(format!("{txt} · GATED {g}"))
                        .monospace()
                        .color(AMBER),
                ),
                None => ui.label(RichText::new(txt).monospace()),
            };
        });
    }
    ui.label(RichText::new("bar scale z -3 … +9; gated windows (rate < 15 fps, stale, no data) are excluded from links, heat and agreement").small().weak());
}

fn person(s: &RoomScope, ui: &mut egui::Ui) {
    let Some(l) = s.loaded.as_ref() else { return };
    ui.label(RichText::new("Person").strong());
    let Some((_, f)) = s.track(l).and_then(|t| t.at(s.t, 0.25)) else {
        ui.label("no track frame");
        return;
    };
    let Some(xy) = s.person_xy(l, f) else {
        ui.label("nobody visible");
        return;
    };
    let mut lines = vec![format!(
        "floor {:.2}, {:.2} m  conf {}",
        xy[0],
        xy[1],
        f2(f.conf, 2)
    )];
    if let Some(e) = f.ellipse_m {
        lines.push(format!(
            "ellipse (room, approx) a {:.2} b {:.2} m @ {:.0}°",
            e.a,
            e.b,
            e.ang.to_degrees()
        ));
    }
    lines.push(format!(
        "heading {} ({})",
        f.heading_rad
            .map_or("–".into(), |h| format!("{:.0}°", h.to_degrees())),
        f.heading_source.as_deref().unwrap_or("none")
    ));
    if let Some(r) = l.meta.router() {
        let d: Vec<String> = l
            .meta
            .spots()
            .iter()
            .map(|(id, p, _)| format!("router-{id} {:.2} m", model::seg_dist(xy, r, *p)))
            .collect();
        lines.push(format!("distance to link: {}", d.join(", ")));
    }
    for t in lines {
        ui.label(RichText::new(t).monospace().small());
    }
}

/// Split a series at gaps (None) into separate line pieces.
fn pieces(xs: impl Iterator<Item = (f64, Option<f64>)>) -> Vec<Vec<[f64; 2]>> {
    let mut out: Vec<Vec<[f64; 2]>> = vec![Vec::new()];
    for (x, y) in xs {
        match y {
            Some(y) => out.last_mut().expect("non-empty").push([x, y]),
            None if !out.last().expect("non-empty").is_empty() => out.push(Vec::new()),
            None => {}
        }
    }
    out.retain(|p| !p.is_empty());
    out
}

pub fn plots(s: &mut RoomScope, ui: &mut egui::Ui) {
    let Some(l) = s.loaded.as_ref() else { return };
    let dur = l.meta.duration;
    let origin = l.meta.video_origin_epoch + s.offset;
    let h = (ui.available_height() - 10.0) / 2.0;
    let mut seek: Option<f64> = None;
    let cues: Vec<(f64, String)> = l
        .meta
        .cues
        .iter()
        .filter(|c| !c.msg.starts_with("DONE"))
        .map(|c| {
            let lbl: String = c
                .msg
                .trim_start_matches("STEP ")
                .split([':', '(', '{', ' '])
                .next()
                .unwrap_or("")
                .chars()
                .take(10)
                .collect();
            (c.epoch - origin, lbl.trim().to_string())
        })
        .collect();
    let r = Plot::new("z_plot")
        .height(h.max(80.0))
        .link_axis("room_t", [true, false])
        .allow_drag(false)
        .allow_zoom(false)
        .allow_scroll(false)
        .allow_boxed_zoom(false)
        .allow_double_click_reset(false)
        .include_x(0.0)
        .include_x(dur)
        .include_y(-4.0)
        .include_y(10.0)
        .y_axis_label("z (clamped -8..15)")
        .legend(Legend::default())
        .show(ui, |pu| {
            if let Some(csi) = &l.csi {
                for (ci, nid) in csi.node_ids().iter().enumerate() {
                    let nd = &csi.nodes[nid];
                    let col = NODE_COLORS[ci % NODE_COLORS.len()];
                    let series = (0..csi.n).map(|k| {
                        let x = csi.t0_epoch + k as f64 * csi.dt - origin;
                        let live = nd.gate.get(k).cloned().flatten().is_none();
                        (
                            x,
                            nd.z.get(k)
                                .copied()
                                .flatten()
                                .filter(|_| live)
                                .map(|z| z.clamp(-8.0, 15.0)),
                        )
                    });
                    for pc in pieces(series) {
                        pu.line(
                            Line::new(format!("n{nid}"), PlotPoints::from(pc))
                                .color(col)
                                .width(1.2),
                        );
                    }
                }
            }
            for (x, lbl) in &cues {
                pu.vline(VLine::new("", *x).color(GREY).width(1.0));
                pu.text(
                    Text::new("", PlotPoint::new(*x, 9.5), RichText::new(lbl).small())
                        .anchor(egui::Align2::LEFT_TOP)
                        .color(Color32::LIGHT_GRAY),
                );
            }
            pu.vline(
                VLine::new("", s.t)
                    .color(Color32::from_rgb(255, 51, 85))
                    .width(2.0),
            );
        });
    if (r.response.clicked() || r.response.dragged())
        && let Some(p) = r.response.interact_pointer_pos()
    {
        seek = Some(r.transform.value_from_position(p).x);
    }
    let tr = s.track(l);
    let r2 = Plot::new("xy_plot")
        .height(h.max(80.0))
        .link_axis("room_t", [true, false])
        .allow_drag(false)
        .allow_zoom(false)
        .allow_scroll(false)
        .allow_boxed_zoom(false)
        .allow_double_click_reset(false)
        .include_x(0.0)
        .include_x(dur)
        .include_y(0.0)
        .include_y(4.0)
        .y_axis_label("person m")
        .x_axis_label("video s")
        .legend(Legend::default())
        .show(ui, |pu| {
            if let Some(tr) = tr {
                for (i, name, col) in [
                    (0usize, "x (E)", Color32::from_rgb(255, 128, 128)),
                    (1, "y (N)", Color32::from_rgb(128, 255, 128)),
                ] {
                    for pc in pieces(
                        tr.track
                            .iter()
                            .map(|f| (f.t, if f.present { f.xy.map(|v| v[i]) } else { None })),
                    ) {
                        pu.line(Line::new(name, PlotPoints::from(pc)).color(col).width(1.2));
                    }
                }
            }
            pu.vline(
                VLine::new("", s.t)
                    .color(Color32::from_rgb(255, 51, 85))
                    .width(2.0),
            );
        });
    if (r2.response.clicked() || r2.response.dragged())
        && let Some(p) = r2.response.interact_pointer_pos()
    {
        seek = Some(r2.transform.value_from_position(p).x);
    }
    if let Some(t) = seek {
        s.t = t.clamp(0.0, dur);
    }
}
