//! Data model and geometry for the room scope. Everything site-specific (homographies, markers, walls,
//! node spots, cues) is read at runtime from the data directory; nothing about a site is compiled in.

use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub type Mat3 = [[f64; 3]; 3];

/// Source-frame size all image coordinates refer to (the overhead camera's pixel grid).
pub const IMG_W: f64 = 1280.0;
pub const IMG_H: f64 = 720.0;

#[derive(Clone, Debug, Deserialize)]
pub struct Cue {
    pub epoch: f64,
    pub msg: String,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq)]
pub struct Ellipse {
    pub cx: f64,
    pub cy: f64,
    pub a: f64,
    pub b: f64,
    pub ang: f64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Points3 {
    pub head: Option<[f64; 2]>,
    pub l_shoulder: Option<[f64; 2]>,
    pub r_shoulder: Option<[f64; 2]>,
}

/// One frame of the shared tracker schema `room-scope.track.v1`.
#[derive(Clone, Debug, Deserialize)]
pub struct TrackFrame {
    pub t: f64,
    pub present: bool,
    #[serde(default)]
    pub conf: Option<f64>,
    #[serde(default)]
    pub xy: Option<[f64; 2]>,
    #[serde(default)]
    pub raw_xy: Option<[f64; 2]>,
    #[serde(default)]
    pub floor_px: Option<[f64; 2]>,
    #[serde(default)]
    pub centroid_px: Option<[f64; 2]>,
    #[serde(default)]
    pub far_px: Option<[f64; 2]>,
    #[serde(default)]
    pub ellipse_px: Option<Ellipse>,
    #[serde(default)]
    pub ellipse_m: Option<Ellipse>,
    #[serde(default)]
    pub heading_rad: Option<f64>,
    #[serde(default)]
    pub heading_source: Option<String>,
    #[serde(default)]
    pub keypoints_px: Option<Vec<[f64; 3]>>,
    #[serde(default)]
    pub points3_px: Option<Points3>,
    #[serde(default)]
    pub area_px: Option<f64>,
    #[serde(default)]
    pub touches_border: Option<bool>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Track {
    #[serde(default)]
    pub backend: Option<String>,
    #[serde(default)]
    pub present_frac: Option<f64>,
    #[serde(default)]
    pub notes: Vec<String>,
    pub track: Vec<TrackFrame>,
}

impl Track {
    pub fn backend_id(&self) -> &str {
        self.backend.as_deref().unwrap_or("cpu-blob")
    }

    /// Nearest frame to video time `t`, within `tol` seconds.
    pub fn at(&self, t: f64, tol: f64) -> Option<(usize, &TrackFrame)> {
        let tr = &self.track;
        if tr.is_empty() {
            return None;
        }
        let i = tr.partition_point(|f| f.t < t);
        let cand = [i.saturating_sub(1), i.min(tr.len() - 1)];
        let k = cand
            .into_iter()
            .min_by(|a, b| (tr[*a].t - t).abs().total_cmp(&(tr[*b].t - t).abs()))?;
        ((tr[k].t - t).abs() <= tol).then(|| (k, &tr[k]))
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct CsiNode {
    pub z: Vec<Option<f64>>,
    #[serde(default)]
    pub fps: Vec<Option<f64>>,
    pub gate: Vec<Option<String>>,
    #[serde(default)]
    pub spot: Option<String>,
    #[serde(default)]
    pub board: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Csi {
    pub t0_epoch: f64,
    pub dt: f64,
    pub n: usize,
    pub nodes: BTreeMap<String, CsiNode>,
}

impl Csi {
    pub fn index(&self, epoch: f64) -> Option<usize> {
        let k = ((epoch - self.t0_epoch) / self.dt).round();
        (k >= 0.0 && (k as usize) < self.n).then_some(k as usize)
    }

    /// Node ids in numeric order.
    pub fn node_ids(&self) -> Vec<String> {
        let mut v: Vec<String> = self.nodes.keys().cloned().collect();
        v.sort_by_key(|s| s.parse::<i64>().unwrap_or(i64::MAX));
        v
    }

    /// z at index k when the window is not gated.
    pub fn live_z(&self, node: &str, k: usize) -> Option<f64> {
        let n = self.nodes.get(node)?;
        if n.gate.get(k).cloned().flatten().is_some() {
            return None;
        }
        n.z.get(k).copied().flatten()
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct FramesIndex {
    pub fps: f64,
    pub count: usize,
    pub pattern: String,
    pub width: u32,
    pub height: u32,
}

impl FramesIndex {
    pub fn index_at(&self, t: f64) -> usize {
        ((t * self.fps).round().max(0.0) as usize).min(self.count.saturating_sub(1))
    }

    /// `f{:05}.jpg` style pattern -> file name.
    pub fn file_name(&self, i: usize) -> String {
        match self.pattern.split_once("{:05}") {
            Some((a, b)) => format!("{a}{i:05}{b}"),
            None => format!("f{i:05}.jpg"),
        }
    }
}

/// A run's geometry and timing from `<run>_meta.json`.
#[derive(Clone, Debug)]
pub struct Meta {
    pub run: String,
    pub duration: f64,
    pub video_origin_epoch: f64,
    pub cues: Vec<Cue>,
    pub markers: Vec<(String, [f64; 2])>,
    pub homographies: BTreeMap<String, Mat3>,
    pub room_max: [f64; 2],
    pub site: Value,
    pub deviation_note: Option<String>,
}

fn v2(v: &Value) -> Option<[f64; 2]> {
    Some([v.get(0)?.as_f64()?, v.get(1)?.as_f64()?])
}

impl Meta {
    pub fn from_json(v: &Value) -> Result<Self, String> {
        let mut homographies = BTreeMap::new();
        if let Some(hs) = v["homographies"].as_object() {
            for (k, h) in hs {
                let m: Mat3 = serde_json::from_value(h.clone())
                    .map_err(|e| format!("homography {k}: {e}"))?;
                homographies.insert(k.clone(), m);
            }
        }
        if homographies.is_empty() {
            return Err("meta has no homographies".into());
        }
        let mut markers: Vec<(String, [f64; 2])> = v["markers"]["markers"]
            .as_object()
            .map(|m| {
                m.iter()
                    .filter_map(|(k, x)| Some((k.clone(), v2(&x["room_xy_m"])?)))
                    .collect()
            })
            .unwrap_or_default();
        markers.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(Self {
            run: v["run"].as_str().unwrap_or("?").to_string(),
            duration: v["video_info"]["duration"].as_f64().unwrap_or(0.0),
            video_origin_epoch: v["video_origin_epoch"]
                .as_f64()
                .ok_or("meta: no video_origin_epoch")?,
            cues: serde_json::from_value(v["cues"].clone()).unwrap_or_default(),
            markers,
            homographies,
            room_max: v2(&v["site"]["room"]["max"]).unwrap_or([5.0, 5.0]),
            site: v["site"]["site"].clone(),
            deviation_note: v["deviations"]["operator_report"]
                .as_str()
                .map(String::from),
        })
    }

    pub fn router(&self) -> Option<[f64; 2]> {
        v2(&self.site["router"]["position"])
    }

    /// (spot id, position, node ids at that spot)
    pub fn spots(&self) -> Vec<(String, [f64; 2], Vec<String>)> {
        let nodes = self.site["nodes"].as_array().cloned().unwrap_or_default();
        self.site["spots"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|s| {
                        let id = s["id"].as_str()?.to_string();
                        let ids = nodes
                            .iter()
                            .filter(|n| n["spot"].as_str() == Some(id.as_str()))
                            .filter_map(|n| n["csi_id"].as_i64().map(|i| i.to_string()))
                            .collect();
                        Some((id, v2(&s["position"])?, ids))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Axis-aligned rectangles (kind, min, max) of walls / openings / objects for the plan view.
    pub fn rects(&self, key: &str) -> Vec<(String, [f64; 2], [f64; 2])> {
        self.site[key]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|o| {
                        let kind = o["kind"].as_str().unwrap_or("").to_string();
                        Some((kind, v2(&o["min"])?, v2(&o["max"])?))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

// ---------- geometry ----------

pub fn inv3(m: &Mat3) -> Mat3 {
    let [a, b, c] = m[0];
    let [d, e, f] = m[1];
    let [g, h, i] = m[2];
    let (aa, bb, cc) = (e * i - f * h, -(d * i - f * g), d * h - e * g);
    let det = a * aa + b * bb + c * cc;
    [
        [aa / det, -(b * i - c * h) / det, (b * f - c * e) / det],
        [bb / det, (a * i - c * g) / det, -(a * f - c * d) / det],
        [cc / det, -(a * h - b * g) / det, (a * e - b * d) / det],
    ]
}

/// Apply a homography; returns (x, y, w). w <= 0 means the point is behind the projective horizon.
pub fn apply(h: &Mat3, x: f64, y: f64) -> (f64, f64, f64) {
    let w = h[2][0] * x + h[2][1] * y + h[2][2];
    (
        (h[0][0] * x + h[0][1] * y + h[0][2]) / w,
        (h[1][0] * x + h[1][1] * y + h[1][2]) / w,
        w,
    )
}

pub fn seg_dist(p: [f64; 2], a: [f64; 2], b: [f64; 2]) -> f64 {
    let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
    let l2 = dx * dx + dy * dy;
    let t = if l2 > 0.0 {
        (((p[0] - a[0]) * dx + (p[1] - a[1]) * dy) / l2).clamp(0.0, 1.0)
    } else {
        0.0
    };
    (p[0] - (a[0] + t * dx)).hypot(p[1] - (a[1] + t * dy))
}

/// Clip segment a-b to a convex quad (Cyrus-Beck). Works for either winding.
pub fn clip_to_quad(
    a: [f64; 2],
    b: [f64; 2],
    quad: &[[f64; 2]; 4],
) -> Option<([f64; 2], [f64; 2])> {
    let area: f64 = (0..4)
        .map(|i| {
            let (p, q) = (quad[i], quad[(i + 1) % 4]);
            p[0] * q[1] - q[0] * p[1]
        })
        .sum();
    let ccw = area > 0.0;
    let d = [b[0] - a[0], b[1] - a[1]];
    let (mut t0, mut t1) = (0.0f64, 1.0f64);
    for i in 0..4 {
        let (p, q) = (quad[i], quad[(i + 1) % 4]);
        let e = [q[0] - p[0], q[1] - p[1]];
        let n = if ccw { [-e[1], e[0]] } else { [e[1], -e[0]] };
        let num = n[0] * (a[0] - p[0]) + n[1] * (a[1] - p[1]);
        let den = n[0] * d[0] + n[1] * d[1];
        if den.abs() < 1e-12 {
            if num < 0.0 {
                return None;
            }
            continue;
        }
        let t = -num / den;
        if den > 0.0 {
            t0 = t0.max(t)
        } else {
            t1 = t1.min(t)
        }
        if t0 > t1 {
            return None;
        }
    }
    Some((
        [a[0] + t0 * d[0], a[1] + t0 * d[1]],
        [a[0] + t1 * d[0], a[1] + t1 * d[1]],
    ))
}

/// Colour for a z value: <= 0 blue-grey, rising through yellow to red at z >= 6; None = grey.
pub fn z_rgb(z: Option<f64>) -> [u8; 3] {
    match z {
        None => [120, 120, 120],
        Some(z) if z <= 0.0 => [90, 130, 170],
        Some(z) => {
            let u = (z / 6.0).min(1.0);
            [
                255,
                (230.0 * (1.0 - u) + 40.0 * u) as u8,
                (60.0 * (1.0 - u)) as u8,
            ]
        }
    }
}

// ---------- loading ----------

pub fn read_json(p: &Path) -> Result<Value, String> {
    let s = std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))?;
    serde_json::from_str(&s).map_err(|e| format!("{}: {e}", p.display()))
}

/// Runs available in `<data>/data/` (files named `<run>_meta.json`).
pub fn list_runs(data: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(data.join("data"))
        .map(|rd| {
            rd.filter_map(|e| e.ok()?.file_name().into_string().ok())
                .filter_map(|n| n.strip_suffix("_meta.json").map(String::from))
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

/// Tracker backends with a track file for this run: `<run>_track.<backend>.json`.
pub fn list_backends(data: &Path, run: &str) -> Vec<(String, PathBuf)> {
    let prefix = format!("{run}_track.");
    let mut v: Vec<(String, PathBuf)> = std::fs::read_dir(data.join("data"))
        .map(|rd| {
            rd.filter_map(|e| {
                let e = e.ok()?;
                let n = e.file_name().into_string().ok()?;
                let id = n.strip_prefix(&prefix)?.strip_suffix(".json")?.to_string();
                (!id.is_empty()).then(|| (id, e.path()))
            })
            .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}
