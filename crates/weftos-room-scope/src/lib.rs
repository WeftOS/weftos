//! weft-room-scope: an overhead-camera room viewer. It shows recorded camera frames with a room
//! coordinate grid (through the inverse floor homography), a tracked person (ellipse with front/back,
//! from any tracker backend that writes the `room-scope.track.v1` schema) and per-node CSI deviation
//! overlays (link colours, z bars, naive link-weighted heat). All heavy work is precomputed by the
//! preprocessing scripts; the app reads a private data directory and embeds no site data.
//!
//! Data directory layout (`--data <dir>`):
//! - `data/<run>_meta.json`: cues, video origin epoch, homographies, markers, site layout
//! - `data/<run>_track.<backend>.json`: tracker output (`room-scope.track.v1`)
//! - `data/<run>_track_eval.<backend>.json`: per-marker error during still phases (optional)
//! - `data/<run>_csi.json`, `data/csi_summary.json`: CSI z per node and summary (optional)
//! - `cache/frames/<run>/index.json` + JPEGs: camera frames keyed by video time

pub mod frames;
pub mod heat;
pub mod model;
mod panels;
pub mod plan;
mod shot;
mod tables;
pub mod view;

use eframe::egui::{self, Color32, RichText};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;

use heat::HeatGrid;
use model::{Csi, FramesIndex, Meta, Track};
use view::{Layers, Proj, Screen};

const PLOTS_MIN_H: f32 = 300.0;

/// Command-line options.
#[derive(Clone, Debug, Default)]
pub struct Opts {
    pub data: Option<PathBuf>,
    pub run: Option<String>,
    pub t: Option<f64>,
    pub backend: Option<String>,
    pub heat: bool,
    pub play: bool,
}

impl Opts {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut o = Opts::default();
        let mut it = args.into_iter();
        while let Some(a) = it.next() {
            let mut val = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value"));
            match a.as_str() {
                "--data" => o.data = Some(PathBuf::from(val("--data")?)),
                "--run" => o.run = Some(val("--run")?),
                "--t" => o.t = Some(val("--t")?.parse().map_err(|e| format!("--t: {e}"))?),
                "--backend" => o.backend = Some(val("--backend")?),
                "--heat" => o.heat = true,
                "--play" => o.play = true,
                "-h" | "--help" => return Err(USAGE.into()),
                other => return Err(format!("unknown argument {other}\n{USAGE}")),
            }
        }
        if o.data.is_none() {
            o.data = std::env::var_os("ROOM_SCOPE_DATA").map(PathBuf::from);
        }
        Ok(o)
    }
}

pub const USAGE: &str = "weft-room-scope --data <dir> [--run <name>] [--t <video s>] [--backend <id>] [--heat] [--play]\n  (or ROOM_SCOPE_DATA=<dir>; COMPANION_SCREENSHOT=<file.bmp> for a self-screenshot)";

pub struct Loaded {
    pub meta: Meta,
    pub proj: Proj,
    pub tracks: Vec<(String, Track)>,
    pub evals: BTreeMap<String, Value>,
    pub csi: Option<Csi>,
    pub frames: Option<frames::FrameSource>,
    pub heat: HeatGrid,
}

pub struct RoomScope {
    pub data: PathBuf,
    pub runs: Vec<String>,
    pub run: String,
    pub hsel: String,
    pub backend: String,
    pub loaded: Option<Loaded>,
    pub summary: Option<Value>,
    pub error: Option<String>,
    pub t: f64,
    pub playing: bool,
    pub rate: f64,
    pub offset: f64,
    pub offsets: BTreeMap<String, f64>,
    pub layers: Layers,
    heat_tex: Option<egui::TextureHandle>,
    start_t: Option<f64>,
}

impl RoomScope {
    pub fn new(o: Opts) -> Self {
        let data = o.data.clone().unwrap_or_default();
        let runs = model::list_runs(&data);
        let run = o
            .run
            .clone()
            .filter(|r| runs.contains(r))
            .or_else(|| runs.first().cloned())
            .unwrap_or_default();
        let offsets: BTreeMap<String, f64> = model::read_json(&data.join("cache/room-scope.json"))
            .ok()
            .and_then(|v| serde_json::from_value(v["sync_offset_s"].clone()).ok())
            .unwrap_or_default();
        let mut s = Self {
            summary: model::read_json(&data.join("data/csi_summary.json")).ok(),
            error: if runs.is_empty() {
                Some(format!(
                    "no runs (data/<run>_meta.json) under {:?}. {USAGE}",
                    data
                ))
            } else {
                None
            },
            data,
            runs,
            run,
            hsel: "corrected".into(),
            backend: o.backend.clone().unwrap_or_else(|| "cpu-blob".into()),
            loaded: None,
            t: 0.0,
            playing: o.play,
            rate: 1.0,
            offset: 0.0,
            offsets,
            layers: Layers {
                heat: o.heat,
                ..Layers::default()
            },
            heat_tex: None,
            start_t: o.t,
        };
        s.offset = s.offsets.get(&s.run).copied().unwrap_or(0.0);
        s
    }

    fn save_offsets(&self) {
        let p = self.data.join("cache/room-scope.json");
        let doc = serde_json::json!({ "sync_offset_s": self.offsets });
        if let Err(e) = std::fs::write(&p, doc.to_string()) {
            eprintln!("could not save {}: {e}", p.display());
        }
    }

    pub fn load(&mut self, ctx: &egui::Context) {
        let d = self.data.join("data");
        let run = self.run.clone();
        let res = (|| -> Result<Loaded, String> {
            let meta = Meta::from_json(&model::read_json(&d.join(format!("{run}_meta.json")))?)?;
            let h = *meta
                .homographies
                .get(&self.hsel)
                .or_else(|| meta.homographies.values().next())
                .ok_or("no homography")?;
            let proj = Proj::new(h);
            let mut tracks = Vec::new();
            let mut evals = BTreeMap::new();
            for (id, path) in model::list_backends(&self.data, &run) {
                let tr: Track = serde_json::from_value(model::read_json(&path)?)
                    .map_err(|e| format!("{}: {e}", path.display()))?;
                if let Ok(ev) = model::read_json(&d.join(format!("{run}_track_eval.{id}.json"))) {
                    evals.insert(id.clone(), ev);
                }
                tracks.push((id, tr));
            }
            let csi = model::read_json(&d.join(format!("{run}_csi.json")))
                .ok()
                .and_then(|v| serde_json::from_value(v).ok());
            let fdir = self.data.join("cache/frames").join(&run);
            let frames = model::read_json(&fdir.join("index.json"))
                .ok()
                .and_then(|v| serde_json::from_value::<FramesIndex>(v).ok())
                .map(|ix| frames::FrameSource::new(fdir, ix, ctx));
            let heat = HeatGrid::new(&proj, &meta, 0.4);
            Ok(Loaded {
                meta,
                proj,
                tracks,
                evals,
                csi,
                frames,
                heat,
            })
        })();
        match res {
            Ok(l) => {
                if !l.tracks.iter().any(|(id, _)| *id == self.backend)
                    && let Some((id, _)) = l.tracks.first()
                {
                    self.backend = id.clone();
                }
                self.loaded = Some(l);
                self.error = None;
            }
            Err(e) => {
                self.loaded = None;
                self.error = Some(e);
            }
        }
        self.offset = self.offsets.get(&self.run).copied().unwrap_or(0.0);
        if let Some(t) = self.start_t.take() {
            self.t = t;
        }
    }

    pub fn set_h(&mut self) {
        if let Some(l) = &mut self.loaded
            && let Some(h) = l.meta.homographies.get(&self.hsel)
        {
            l.proj = Proj::new(*h);
            l.heat = HeatGrid::new(&l.proj, &l.meta, 0.4);
        }
    }

    pub fn epoch(&self, l: &Loaded) -> f64 {
        l.meta.video_origin_epoch + self.t + self.offset
    }

    pub fn track<'a>(&self, l: &'a Loaded) -> Option<&'a Track> {
        l.tracks
            .iter()
            .find(|(id, _)| *id == self.backend)
            .map(|(_, t)| t)
    }

    /// Person position in room metres under the selected homography.
    pub fn person_xy(&self, l: &Loaded, f: &model::TrackFrame) -> Option<[f64; 2]> {
        if !f.present {
            return None;
        }
        if self.hsel == "corrected"
            && let Some(xy) = f.xy
        {
            return Some(xy);
        }
        f.floor_px.map(|p| l.proj.to_room(p))
    }

    fn camera(&mut self, ui: &mut egui::Ui) {
        let Some(l) = &mut self.loaded else { return };
        let avail = ui.available_size();
        // leave room below the camera for the two time-series plots
        let w = avail.x.min((avail.y - PLOTS_MIN_H).max(200.0) * 16.0 / 9.0);
        let (rect, _) = ui.allocate_exact_size(egui::vec2(w, w * 9.0 / 16.0), egui::Sense::hover());
        let p = ui.painter_at(rect);
        p.rect_filled(rect, 0.0, Color32::BLACK);
        if let Some(fs) = &mut l.frames {
            fs.want(ui.ctx(), self.t);
            if self.layers.cam
                && let Some(tex) = &fs.texture
            {
                p.image(
                    tex.id(),
                    rect,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    Color32::WHITE,
                );
            }
            if let Some(e) = &fs.error {
                p.text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    e,
                    egui::FontId::proportional(14.0),
                    view::WARN,
                );
            }
        } else {
            p.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "no frames: run prep_video.py (cache/frames/<run>/index.json)",
                egui::FontId::proportional(14.0),
                view::WARN,
            );
        }
        let l = self.loaded.as_ref().expect("loaded");
        let sc = Screen { rect };
        let epoch = self.epoch(l);
        let k = l.csi.as_ref().and_then(|c| c.index(epoch));
        if self.layers.heat
            && let (Some(csi), Some(k)) = (&l.csi, k)
        {
            let img = l.heat.image(&l.meta, csi, k);
            let tex = self.heat_tex.get_or_insert_with(|| {
                ui.ctx()
                    .load_texture("room-scope-heat", img.clone(), egui::TextureOptions::LINEAR)
            });
            tex.set(img, egui::TextureOptions::LINEAR);
            p.image(
                tex.id(),
                rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                Color32::WHITE,
            );
        }
        if self.layers.grid {
            view::grid_and_markers(&p, &l.proj, &sc, &l.meta);
        }
        if self.layers.site {
            view::site(
                &p,
                &l.proj,
                &sc,
                &l.meta,
                l.csi.as_ref(),
                k,
                self.layers.csi,
            );
        }
        if self.layers.track
            && let Some(tr) = self.track(l)
        {
            let hit = tr.at(self.t, 0.25);
            let trail = hit
                .map(|(i, _)| &tr.track[i.saturating_sub(30)..=i])
                .unwrap_or(&[]);
            // only the latest unbroken run of detections, so gaps are not bridged by a line
            let trail: Vec<model::TrackFrame> = trail
                .iter()
                .rev()
                .take_while(|f| f.present)
                .cloned()
                .collect();
            let f = hit.map(|(_, f)| f);
            let xy = f.and_then(|f| self.person_xy(l, f));
            view::person(&p, &l.proj, &sc, &trail, f, xy);
            p.text(
                rect.right_top() + egui::vec2(-8.0, 8.0),
                egui::Align2::RIGHT_TOP,
                format!("track: {}", tr.backend_id()),
                egui::FontId::proportional(14.0),
                view::PERSON,
            );
        }
        let bar = egui::Rect::from_min_max(egui::pos2(rect.left(), rect.bottom() - 18.0), rect.max);
        p.rect_filled(bar, 0.0, Color32::from_black_alpha(150));
        p.text(bar.left_center() + egui::vec2(6.0, 0.0), egui::Align2::LEFT_CENTER,
            "ESTIMATED: planar homography, wide-angle lens not undistorted; positions away from the 4 markers are approximate.",
            egui::FontId::proportional(12.0), view::WARN);
    }
}

impl eframe::App for RoomScope {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        #[cfg(not(target_arch = "wasm32"))]
        shot::poll(&ctx);
        // keep ticking while idle so frame decodes and the self-screenshot timer are picked up
        ctx.request_repaint_after(std::time::Duration::from_millis(200));
        if self.loaded.is_none() && self.error.is_none() {
            self.load(&ctx);
        }
        if self.playing {
            let dt = ctx.input(|i| f64::from(i.stable_dt).min(0.2));
            let dur = self.loaded.as_ref().map_or(0.0, |l| l.meta.duration);
            self.t = (self.t + dt * self.rate).min(dur);
            if self.t >= dur {
                self.playing = false;
            }
            ctx.request_repaint();
        }
        egui::Panel::top("room_scope_top").show_inside(ui, |ui| panels::top_bar(self, ui));
        egui::Panel::right("room_scope_side")
            .resizable(true)
            .default_size(430.0)
            .show_inside(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| panels::side(self, ui));
            });
        egui::CentralPanel::default().show_inside(ui, |ui| {
            if let Some(e) = &self.error {
                ui.label(RichText::new(e).color(view::WARN));
                return;
            }
            self.camera(ui);
            panels::plots(self, ui);
        });
    }
}

/// Native entry point.
#[cfg(not(target_arch = "wasm32"))]
pub fn run_native(o: Opts) -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1600.0, 1000.0])
            .with_min_inner_size([1000.0, 640.0])
            .with_title("weft-room-scope"),
        ..Default::default()
    };
    eframe::run_native(
        "weft-room-scope",
        options,
        Box::new(move |_cc| Ok(Box::new(RoomScope::new(o)))),
    )
}
