//! Trajectory Atlas render stress test (docs/plans/trajectory-atlas.md, T4–T6).
//!
//! Answers one question: can egui's own painter draw the atlas views at the
//! sizes the plan targets, or do we need a custom shader / a three.js client?
//! Runs a fixed list of scripted scenes on synthetic data, times every frame
//! (vsync off), writes a Markdown report and exits.
//!
//!   scripts/build.sh atlas-bench            # report to target/atlas-bench.md
//!   ATLAS_BENCH_OUT=path ATLAS_BENCH_FRAMES=300 scripts/build.sh atlas-bench

use eframe::egui::{self, Color32, Mesh, Pos2, Rect, Shape, Vec2};
use std::time::Instant;

const WARMUP: usize = 30;
const LOD_BEADS: u32 = 12;
const GROUPS: [(&str, f32); 6] = [
    ("fulfilled", 0.70),
    ("partial", 0.09),
    ("failed", 0.06),
    ("no_response", 0.04),
    ("not_a_request", 0.06),
    ("clarifying", 0.05),
];
const PALETTE: [Color32; 6] = [
    Color32::from_rgb(98, 160, 84),
    Color32::from_rgb(232, 196, 72),
    Color32::from_rgb(214, 80, 72),
    Color32::from_rgb(80, 112, 176),
    Color32::from_rgb(140, 140, 140),
    Color32::from_rgb(160, 96, 176),
];

/// Tiny deterministic PRNG so every run draws the same corpus.
struct Rng(u64);
impl Rng {
    fn f(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 40) as f32 / (1u64 << 24) as f32
    }
    fn normal(&mut self) -> f32 {
        let (a, b) = (self.f().max(1e-6), self.f());
        (-2.0 * a.ln()).sqrt() * (std::f32::consts::TAU * b).cos()
    }
}

struct Run {
    group: usize,
    color: usize,
    len: u32,
    tree: Pos2,
    umap: Pos2,
}

fn corpus(n: usize) -> Vec<Run> {
    let mut r = Rng(0x9E37_79B9_7F4A_7C15);
    let mut runs: Vec<Run> = (0..n)
        .map(|_| {
            let x = r.f();
            let mut acc = 0.0;
            let group = GROUPS
                .iter()
                .position(|g| {
                    acc += g.1;
                    x < acc
                })
                .unwrap_or(0);
            // Second classifier agrees ~75% of the time.
            let color = if r.f() < 0.75 {
                group
            } else {
                (r.f() * 6.0) as usize % 6
            };
            let len = (r.normal() * 1.1 + 3.5).exp().clamp(1.0, 5000.0) as u32;
            let blob = (r.f() * 9.0) as usize;
            let c = Pos2::new(
                (blob % 3) as f32 * 0.33 + 0.16,
                (blob / 3) as f32 * 0.33 + 0.16,
            );
            let umap = c + Vec2::new(r.normal(), r.normal()) * 0.06;
            Run {
                group,
                color,
                len,
                tree: Pos2::ZERO,
                umap,
            }
        })
        .collect();
    layout_treemap(&mut runs);
    runs
}

/// Squarified-enough treemap: groups split the unit square in slice-and-dice
/// order by size, then runs grid-pack inside their group cell.
fn layout_treemap(runs: &mut [Run]) {
    let mut counts = [0usize; 6];
    runs.iter().for_each(|r| counts[r.group] += 1);
    let mut order: Vec<usize> = (0..6).collect();
    order.sort_by_key(|&g| std::cmp::Reverse(counts[g]));
    let total = runs.len() as f32;
    let mut cells = [Rect::NOTHING; 6];
    let mut rest = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
    let mut left = total;
    for (i, &g) in order.iter().enumerate() {
        let frac = counts[g] as f32 / left.max(1.0);
        let (cell, rem) = if i % 2 == 0 {
            let w = rest.width() * frac;
            (
                Rect::from_min_size(rest.min, Vec2::new(w, rest.height())),
                Rect::from_min_max(Pos2::new(rest.min.x + w, rest.min.y), rest.max),
            )
        } else {
            let h = rest.height() * frac;
            (
                Rect::from_min_size(rest.min, Vec2::new(rest.width(), h)),
                Rect::from_min_max(Pos2::new(rest.min.x, rest.min.y + h), rest.max),
            )
        };
        cells[g] = cell.shrink(0.004);
        rest = rem;
        left -= counts[g] as f32;
    }
    let mut seen = [0usize; 6];
    for r in runs.iter_mut() {
        let c = cells[r.group];
        let cols = ((counts[r.group] as f32 * c.width() / c.height().max(1e-6))
            .sqrt()
            .ceil() as usize)
            .max(1);
        let rows = counts[r.group].div_ceil(cols).max(1);
        let i = seen[r.group];
        seen[r.group] += 1;
        r.tree = Pos2::new(
            c.min.x + (i % cols) as f32 / cols as f32 * c.width(),
            c.min.y + (i / cols) as f32 / rows as f32 * c.height(),
        );
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Draw {
    Circles,
    Mesh,
}

#[derive(Clone, Copy)]
enum View {
    Treemap,
    /// Animated treemap → UMAP transition, ping-ponging.
    Morph,
    /// 3-D forest: every turn of every run as a point on a vertical stalk.
    Forest,
    /// Same forest at level of detail: at most
    /// `LOD_BEADS` evenly spaced beads per run (full beads only when zoomed in).
    ForestLod,
    /// Virtualized 5k-message transcript with a lane minimap.
    Timeline(usize),
}

struct Scene {
    name: &'static str,
    runs: usize,
    view: View,
    draw: Draw,
}

const SCENES: &[Scene] = &[
    Scene {
        name: "treemap 10k · circle per dot",
        runs: 10_000,
        view: View::Treemap,
        draw: Draw::Circles,
    },
    Scene {
        name: "treemap 10k · one mesh",
        runs: 10_000,
        view: View::Treemap,
        draw: Draw::Mesh,
    },
    Scene {
        name: "treemap 50k · one mesh",
        runs: 50_000,
        view: View::Treemap,
        draw: Draw::Mesh,
    },
    Scene {
        name: "treemap 100k · one mesh",
        runs: 100_000,
        view: View::Treemap,
        draw: Draw::Mesh,
    },
    Scene {
        name: "treemap↔umap morph 10k · mesh",
        runs: 10_000,
        view: View::Morph,
        draw: Draw::Mesh,
    },
    Scene {
        name: "treemap↔umap morph 50k · mesh",
        runs: 50_000,
        view: View::Morph,
        draw: Draw::Mesh,
    },
    Scene {
        name: "3-D forest 2k runs (all turns) · mesh",
        runs: 2_000,
        view: View::Forest,
        draw: Draw::Mesh,
    },
    Scene {
        name: "3-D forest 10k runs (all turns) · mesh",
        runs: 10_000,
        view: View::Forest,
        draw: Draw::Mesh,
    },
    Scene {
        name: "3-D forest 10k runs · LOD (≤12 beads per stalk) · mesh",
        runs: 10_000,
        view: View::ForestLod,
        draw: Draw::Mesh,
    },
    Scene {
        name: "3-D forest 50k runs · LOD (≤12 beads per stalk) · mesh",
        runs: 50_000,
        view: View::ForestLod,
        draw: Draw::Mesh,
    },
    Scene {
        name: "timeline 5k msgs · virtualized + minimap",
        runs: 0,
        view: View::Timeline(5_000),
        draw: Draw::Mesh,
    },
    Scene {
        name: "timeline 50k msgs · virtualized + minimap",
        runs: 0,
        view: View::Timeline(50_000),
        draw: Draw::Mesh,
    },
];

struct Bench {
    scene: usize,
    frame: usize,
    frames: usize,
    runs: Vec<Run>,
    last: Option<Instant>,
    times: Vec<f32>,
    paint_ms: Vec<f32>,
    points: usize,
    report: Vec<String>,
    out: String,
}

impl Bench {
    fn load(&mut self) {
        while self.scene < SCENES.len()
            && !std::env::var("ATLAS_BENCH_ONLY")
                .map_or(true, |f| SCENES[self.scene].name.contains(&f))
        {
            self.scene += 1;
        }
        let Some(s) = SCENES.get(self.scene) else {
            return;
        };
        self.runs = corpus(s.runs);
        self.frame = 0;
        self.times.clear();
        self.paint_ms.clear();
        self.last = None;
    }

    fn finish_scene(&mut self) {
        let mut t = self.times.clone();
        t.sort_by(f32::total_cmp);
        let mut p = self.paint_ms.clone();
        p.sort_by(f32::total_cmp);
        let q = |v: &[f32], f: f32| v[((v.len() - 1) as f32 * f) as usize];
        let over16 = t.iter().filter(|&&x| x > 16.7).count();
        self.report.push(format!(
            "| {} | {} | {:.2} | {:.2} | {:.2} | {:.2} | {:.0} | {}/{} | {:.2} |",
            SCENES[self.scene].name,
            self.points,
            q(&t, 0.5),
            q(&t, 0.95),
            q(&t, 0.99),
            t[t.len() - 1],
            1000.0 / q(&t, 0.5),
            over16,
            t.len(),
            q(&p, 0.5),
        ));
    }

    fn write_report(&self) {
        let mut s = String::from("# Trajectory Atlas — egui render stress test\n\n");
        s += &format!(
            "eframe 0.34 (glow), vsync off, {} timed frames per scene after {} warm-up. \
             `frame` = wall time between frames (CPU build + tessellate + GPU present); \
             `build` = time spent producing shapes in our code.\n\n",
            self.frames, WARMUP
        );
        s += "| scene | points drawn | frame p50 ms | p95 | p99 | max | fps @p50 | frames >16.7 ms | build p50 ms |\n";
        s += "|---|---|---|---|---|---|---|---|---|\n";
        for line in &self.report {
            s += line;
            s.push('\n');
        }
        std::fs::write(&self.out, &s).expect("write report");
        println!("{s}\nreport: {}", self.out);
    }
}

/// One hexagon fan per dot, appended to a shared mesh (7 verts, 6 tris).
fn dot(mesh: &mut Mesh, c: Pos2, r: f32, col: Color32) {
    let base = mesh.vertices.len() as u32;
    mesh.colored_vertex(c, col);
    for k in 0..6 {
        let a = k as f32 * std::f32::consts::TAU / 6.0;
        mesh.colored_vertex(c + Vec2::new(a.cos(), a.sin()) * r, col);
    }
    for k in 0..6u32 {
        mesh.add_triangle(base, base + 1 + k, base + 1 + (k + 1) % 6);
    }
}

fn paint_dots(
    ui: &egui::Ui,
    draw: Draw,
    pts: impl Iterator<Item = (Pos2, f32, Color32)>,
    clip: Rect,
) -> usize {
    let painter = ui.painter();
    let mut n = 0;
    match draw {
        Draw::Circles => {
            for (p, r, c) in pts {
                if clip.contains(p) {
                    painter.circle_filled(p, r, c);
                    n += 1;
                }
            }
        }
        Draw::Mesh => {
            // egui meshes index with u32, but split anyway to keep each
            // under ~1M vertices for the glow backend.
            let mut mesh = Mesh::default();
            for (p, r, c) in pts {
                if !clip.contains(p) {
                    continue;
                }
                dot(&mut mesh, p, r, c);
                n += 1;
                if mesh.vertices.len() > 900_000 {
                    painter.add(Shape::mesh(std::mem::take(&mut mesh)));
                }
            }
            painter.add(Shape::mesh(mesh));
        }
    }
    n
}

impl eframe::App for Bench {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if self.scene >= SCENES.len() {
            return;
        }
        let now = Instant::now();
        if let Some(prev) = self.last.replace(now)
            && self.frame > WARMUP
        {
            self.times.push((now - prev).as_secs_f32() * 1000.0);
        }
        let s = &SCENES[self.scene];
        let rect = ui.max_rect();
        // Slow continuous zoom/pan so every frame re-tessellates new geometry.
        let t = self.frame as f32 / 60.0;
        let zoom = 1.0 + 0.25 * (t * 0.7).sin();
        let pan = Vec2::new((t * 0.5).cos(), (t * 0.4).sin()) * 20.0;
        let map = |p: Pos2| {
            rect.center() + ((p - Pos2::new(0.5, 0.5)) * rect.size().min_elem() * 0.95 * zoom) + pan
        };
        let build = Instant::now();
        self.points = match s.view {
            View::Treemap => {
                let dots = self.runs.iter().map(|r| {
                    (
                        map(r.tree),
                        (r.len as f32).sqrt().clamp(1.0, 8.0) * 0.6,
                        PALETTE[r.color],
                    )
                });
                paint_dots(ui, s.draw, dots, rect)
            }
            View::Morph => {
                let k = 0.5 - 0.5 * (t * 1.5).cos();
                let k = k * k * (3.0 - 2.0 * k);
                let dots = self.runs.iter().map(|r| {
                    let p = r.tree.lerp(r.umap, k);
                    (
                        map(p),
                        (r.len as f32).sqrt().clamp(1.0, 8.0) * 0.6,
                        PALETTE[r.color],
                    )
                });
                paint_dots(ui, s.draw, dots, rect)
            }
            View::Forest | View::ForestLod => {
                let lod = matches!(s.view, View::ForestLod);
                // Orbiting camera; CPU perspective projection of every turn.
                let yaw = t * 0.3;
                let (sy, cy) = yaw.sin_cos();
                let pitch: f32 = 0.55;
                let (sp, cp) = pitch.sin_cos();
                let f = rect.height() * 1.2;
                let dots = self.runs.iter().flat_map(move |r| {
                    let (x0, z0) = (r.tree.x - 0.5, r.tree.y - 0.5);
                    let col = PALETTE[r.color];
                    let full = r.len.min(400);
                    let n = if lod { full.min(LOD_BEADS) } else { full };
                    let step = full as f32 / n.max(1) as f32;
                    (0..n).map(move |i| (x0, i as f32 * step * 0.0015, z0, col))
                });
                let proj = dots.filter_map(move |(x, y, z, col)| {
                    let (xr, zr) = (x * cy - z * sy, x * sy + z * cy);
                    let (yr, zc) = (y * cp - zr * sp, y * sp + zr * cp + 1.6);
                    (zc > 0.1).then(|| {
                        (
                            rect.center() + Vec2::new(xr, -yr) * (f / zc),
                            (1.6 / zc).clamp(0.5, 2.5),
                            col,
                        )
                    })
                });
                paint_dots(ui, s.draw, proj, rect)
            }
            View::Timeline(n) => timeline(ui, n, self.frame),
        };
        self.paint_ms.push(build.elapsed().as_secs_f32() * 1000.0);
        ui.painter().text(
            rect.left_top() + Vec2::new(8.0, 8.0),
            egui::Align2::LEFT_TOP,
            format!("{} · frame {}/{}", s.name, self.frame, self.frames + WARMUP),
            egui::FontId::monospace(14.0),
            Color32::WHITE,
        );
        self.frame += 1;
        if self.frame > self.frames + WARMUP {
            self.finish_scene();
            self.scene += 1;
            self.load();
            if self.scene >= SCENES.len() {
                self.write_report();
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                return;
            }
        }
        ui.ctx().request_repaint();
    }
}

/// Virtualized transcript: minimap lanes over all `n` messages on the left,
/// `show_rows` list on the right auto-scrolling a few rows per frame.
fn timeline(ui: &mut egui::Ui, n: usize, frame: usize) -> usize {
    let rect = ui.max_rect();
    let mini = Rect::from_min_size(
        rect.min + Vec2::new(0.0, 30.0),
        Vec2::new(60.0, rect.height() - 30.0),
    );
    let mut mesh = Mesh::default();
    let row_h = mini.height() / n as f32;
    for i in 0..n {
        let y = mini.min.y + i as f32 * row_h;
        let h = row_h.max(0.5);
        for lane in 0..5 {
            if (i * 7 + lane * 13) % (lane + 2) == 0 {
                let x = mini.min.x + lane as f32 * 12.0;
                mesh.add_colored_rect(
                    Rect::from_min_size(Pos2::new(x, y), Vec2::new(10.0, h)),
                    PALETTE[(i + lane) % 6],
                );
            }
        }
    }
    ui.painter().add(Shape::mesh(mesh));
    let list = Rect::from_min_max(Pos2::new(mini.max.x + 12.0, mini.min.y), rect.max);
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(list));
    let row = 22.0;
    let offset = (frame as f32 * 37.0) % (n as f32 * row);
    let mut shown = 0;
    egui::ScrollArea::vertical()
        .auto_shrink(false)
        .vertical_scroll_offset(offset)
        .show_rows(&mut child, row, n, |ui, range| {
            for i in range {
                shown += 1;
                ui.horizontal(|ui| {
                    ui.colored_label(PALETTE[i % 6], format!("#{i:05}"));
                    ui.label(if i % 9 == 0 {
                        "Bash {\"command\": \"scripts/build.sh check\"} failed"
                    } else {
                        "assistant · reasoning about the next step in this run…"
                    });
                });
            }
        });
    n * 2 + shown
}

fn main() -> eframe::Result {
    let frames = std::env::var("ATLAS_BENCH_FRAMES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300);
    let out = std::env::var("ATLAS_BENCH_OUT").unwrap_or_else(|_| "target/atlas-bench.md".into());
    let options = eframe::NativeOptions {
        vsync: false,
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1600.0, 1000.0])
            .with_title("atlas-bench"),
        ..Default::default()
    };
    eframe::run_native(
        "atlas-bench",
        options,
        Box::new(move |_cc| {
            let mut b = Bench {
                scene: 0,
                frame: 0,
                frames,
                runs: Vec::new(),
                last: None,
                times: Vec::new(),
                paint_ms: Vec::new(),
                points: 0,
                report: Vec::new(),
                out,
            };
            b.load();
            Ok(Box::new(b))
        }),
    )
}
