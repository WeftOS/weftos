//! Automatic spatial gates for Urth ingest (`.claude/skills/urth-spatial-checks`, ADR-079).
//!
//! Every Urth layer that enters the BVH goes through [`check_layer`]; every join through
//! [`check_join`]. A [`Report`] holds findings of two kinds:
//!
//! - `Refuse`: a fact or a hard refusal from the rules reference (wrong frame for the numbers,
//!   transform that does not round-trip, non-finite coordinates, a join across frames or with
//!   no overlap). The ingest stops and returns the report.
//! - `Warn`: a policy row. The policy numbers in `urth-frames-and-rules.md` are a draft awaiting
//!   owner review (2026-10-25), so they are reported with `draft_policy: true` and never refuse.
//!
//! Conventions: WGS 84, f64; `Wgs84Deg` is longitude-first (x = lon, y = lat, z = ellipsoidal
//! height); `EnuLocal` is metres, x east, y north, z up, right-handed.

use serde_json::{Value, json};

const A: f64 = 6_378_137.0;
const F: f64 = 1.0 / 298.257_223_563;
const E2: f64 = F * (2.0 - F);
/// Round-trip tolerance for `T_ecef_local` (rules reference, gate 1): one millimetre.
const ROUND_TRIP_M: f64 = 1.0e-3;

/// Frame a layer says its numbers are in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Frame {
    /// ECEF metres (EPSG:4978).
    Ecef,
    /// Longitude, latitude (degrees), ellipsoidal height (m).
    Wgs84Deg,
    /// Local ENU metres around the region anchor.
    EnuLocal,
}

/// Level of detail, `L0` (planet) to `L5` (object).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Lod {
    /// Planet-scale region.
    L0,
    /// Country / state.
    L1,
    /// City.
    L2,
    /// Site / campus.
    L3,
    /// Room / zone.
    L4,
    /// Object.
    L5,
}

impl Lod {
    /// Expected span of the largest dimension in metres (rules reference gate 4; draft policy).
    fn span_m(self) -> (f64, f64) {
        match self {
            Lod::L0 => (1.0e6, 4.1e7),
            Lod::L1 => (1.0e5, 5.0e6),
            Lod::L2 => (1.0e4, 5.0e4),
            Lod::L3 => (100.0, 2_000.0),
            Lod::L4 => (2.0, 50.0),
            Lod::L5 => (0.01, 10.0),
        }
    }
}

/// Site anchor behind a region's `T_ecef_local`.
#[derive(Debug, Clone, Copy)]
pub struct Anchor {
    /// Geodetic latitude, degrees.
    pub lat_deg: f64,
    /// Longitude, degrees.
    pub lon_deg: f64,
    /// Ellipsoidal height, metres.
    pub h_m: f64,
}

/// Geodetic to ECEF.
pub fn geodetic_to_ecef(lat_deg: f64, lon_deg: f64, h: f64) -> [f64; 3] {
    let (lat, lon) = (lat_deg.to_radians(), lon_deg.to_radians());
    let n = A / (1.0 - E2 * lat.sin().powi(2)).sqrt();
    [(n + h) * lat.cos() * lon.cos(), (n + h) * lat.cos() * lon.sin(), (n * (1.0 - E2) + h) * lat.sin()]
}

impl Anchor {
    fn basis(&self) -> [[f64; 3]; 3] {
        let (lat, lon) = (self.lat_deg.to_radians(), self.lon_deg.to_radians());
        [
            [-lon.sin(), lon.cos(), 0.0],
            [-lat.sin() * lon.cos(), -lat.sin() * lon.sin(), lat.cos()],
            [lat.cos() * lon.cos(), lat.cos() * lon.sin(), lat.sin()],
        ]
    }

    /// ENU metres to ECEF metres.
    pub fn enu_to_ecef(&self, p: [f64; 3]) -> [f64; 3] {
        let (o, b) = (geodetic_to_ecef(self.lat_deg, self.lon_deg, self.h_m), self.basis());
        std::array::from_fn(|i| o[i] + b[0][i] * p[0] + b[1][i] * p[1] + b[2][i] * p[2])
    }

    /// ECEF metres to ENU metres.
    pub fn ecef_to_enu(&self, p: [f64; 3]) -> [f64; 3] {
        let (o, b) = (geodetic_to_ecef(self.lat_deg, self.lon_deg, self.h_m), self.basis());
        let d = [p[0] - o[0], p[1] - o[1], p[2] - o[2]];
        std::array::from_fn(|i| b[i][0] * d[0] + b[i][1] * d[1] + b[i][2] * d[2])
    }
}

/// One layer offered to ingest.
pub struct Layer<'a> {
    /// Layer or region name, for the report.
    pub name: &'a str,
    /// Frame the layer declares.
    pub frame: Frame,
    /// Level of detail.
    pub lod: Lod,
    /// Coordinates as received.
    pub points: &'a [[f64; 3]],
    /// Bounds the layer declares, `[minx, miny, minz, maxx, maxy, maxz]`.
    pub declared_bounds: Option<[f64; 6]>,
    /// Site anchor of the region, when it has one.
    pub anchor: Option<Anchor>,
    /// Null geometries counted at read time.
    pub nulls: usize,
    /// Invalid geometries counted at read time.
    pub invalid: usize,
    /// The caller dropped or repaired the nulls and invalids above.
    pub nulls_handled: bool,
}

/// How serious a finding is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Stops the ingest.
    Refuse,
    /// Reported only.
    Warn,
}

/// One gate finding.
#[derive(Debug, Clone)]
pub struct Finding {
    /// Gate name.
    pub gate: &'static str,
    /// Refuse or warn.
    pub severity: Severity,
    /// The rule is a draft policy row awaiting owner review.
    pub draft_policy: bool,
    /// What is wrong and what to check next.
    pub message: String,
}

/// Gate results for one ingest.
#[derive(Debug, Clone, Default)]
pub struct Report {
    /// Findings, in gate order.
    pub findings: Vec<Finding>,
    /// Raw axis ranges `[min, max]` for x, y, z as received (tier 1: read the numbers).
    pub ranges: Option<[[f64; 2]; 3]>,
}

impl Report {
    fn add(&mut self, gate: &'static str, severity: Severity, draft_policy: bool, message: String) {
        self.findings.push(Finding { gate, severity, draft_policy, message });
    }

    /// Any finding that stops the ingest.
    pub fn refused(&self) -> bool {
        self.findings.iter().any(|f| f.severity == Severity::Refuse)
    }

    /// Readable refusal text.
    pub fn refusal_text(&self) -> String {
        let lines: Vec<String> = self
            .findings
            .iter()
            .filter(|f| f.severity == Severity::Refuse)
            .map(|f| format!("[{}] {}", f.gate, f.message))
            .collect();
        format!("urth spatial gates refused the ingest: {}", lines.join("; "))
    }

    /// JSON for RPC responses.
    pub fn to_json(&self) -> Value {
        json!({
            "refused": self.refused(),
            "ranges": self.ranges,
            "findings": self.findings.iter().map(|f| json!({
                "gate": f.gate,
                "severity": if f.severity == Severity::Refuse { "refuse" } else { "warn" },
                "draft_policy": f.draft_policy,
                "message": f.message,
            })).collect::<Vec<_>>(),
        })
    }
}

fn bounds(points: &[[f64; 3]]) -> Option<[[f64; 2]; 3]> {
    let mut it = points.iter().filter(|p| p.iter().all(|v| v.is_finite()));
    let first = it.next()?;
    let mut r = [[first[0]; 2], [first[1]; 2], [first[2]; 2]];
    for p in it {
        for (a, v) in r.iter_mut().zip(p) {
            a[0] = a[0].min(*v);
            a[1] = a[1].max(*v);
        }
    }
    Some(r)
}

fn norm(p: &[f64; 3]) -> f64 {
    p.iter().map(|v| v * v).sum::<f64>().sqrt()
}

/// Run the per-layer gates: 1 frame and bounds, 3 nulls and non-finite, 4 magnitude, 5 anchor.
pub fn check_layer(l: &Layer) -> Report {
    let mut r = Report::default();

    // Gate 3: nulls, invalid, non-finite coordinates.
    let bad = l.points.iter().filter(|p| p.iter().any(|v| !v.is_finite())).count();
    if bad > 0 {
        r.add("nulls", Severity::Refuse, false, format!("{}: {bad} points have NaN or infinite coordinates", l.name));
    }
    if (l.nulls > 0 || l.invalid > 0) && !l.nulls_handled {
        r.add("nulls", Severity::Refuse, false, format!("{}: {} null and {} invalid geometries not yet handled; they vanish in joins or give wrong areas", l.name, l.nulls, l.invalid));
    }
    let Some(rng) = bounds(l.points) else {
        r.add("bounds", Severity::Refuse, false, format!("{}: no finite points to place", l.name));
        return r;
    };
    r.ranges = Some(rng);

    // Gate 1: the numbers must fit the frame the layer claims.
    let finite: Vec<&[f64; 3]> = l.points.iter().filter(|p| p.iter().all(|v| v.is_finite())).collect();
    let (max_norm, min_norm) = finite.iter().fold((0.0_f64, f64::MAX), |(mx, mn), p| (mx.max(norm(p)), mn.min(norm(p))));
    let ecef_like = min_norm > 6.0e6 && max_norm < 6.5e6;
    match l.frame {
        Frame::EnuLocal if ecef_like => r.add(
            "frame",
            Severity::Refuse,
            false,
            format!("{}: labelled ENU but every point has magnitude {min_norm:.0}..{max_norm:.0} m, which is ECEF; the T_ecef_local was recorded but never applied (assign instead of transform)", l.name),
        ),
        Frame::EnuLocal if max_norm > 1.0e5 => r.add(
            "frame",
            Severity::Refuse,
            false,
            format!("{}: labelled ENU but points reach {max_norm:.0} m from the origin; a local frame is not that wide (rules reference 1.3)", l.name),
        ),
        Frame::Ecef if !ecef_like => r.add(
            "frame",
            Severity::Refuse,
            false,
            format!("{}: labelled ECEF but magnitudes are {min_norm:.3e}..{max_norm:.3e} m, not near Earth radius 6.4e6; local numbers or lost coordinates (point near the ECEF origin)", l.name),
        ),
        Frame::Wgs84Deg => {
            let (x, y) = (rng[0], rng[1]);
            if x[0] < -180.0 || x[1] > 180.0 || y[0] < -90.0 || y[1] > 90.0 {
                let swapped = y[0] >= -180.0 && y[1] <= 180.0 && x[0] >= -90.0 && x[1] <= 90.0;
                let why = if swapped { "latitude and longitude are probably swapped" } else { "these are projected or ECEF metres, not degrees" };
                r.add("frame", Severity::Refuse, false, format!("{}: labelled WGS84 degrees but x spans {:.4}..{:.4}, y spans {:.4}..{:.4}; {why}", l.name, x[0], x[1], y[0], y[1]));
            } else if let Some(a) = l.anchor {
                let (cx, cy) = ((x[0] + x[1]) / 2.0, (y[0] + y[1]) / 2.0);
                let d = |lon: f64, lat: f64| ((lon - a.lon_deg).powi(2) + (lat - a.lat_deg).powi(2)).sqrt();
                if d(cx, cy) > 1.0 && d(cy, cx) < 1.0 {
                    r.add("frame", Severity::Refuse, false, format!("{}: centroid ({cx:.4}, {cy:.4}) is {:.1} deg from the anchor but ({cy:.4}, {cx:.4}) is {:.2} deg; axis order is swapped (x must be longitude)", l.name, d(cx, cy), d(cy, cx)));
                }
            }
            if cx_is_null_island(rng) {
                r.add("frame", Severity::Refuse, false, format!("{}: sits at (0, 0), open ocean in the Gulf of Guinea; lost coordinates until proven otherwise", l.name));
            }
        }
        _ => {}
    }

    // Declared bounds against the data (label changed, numbers did not).
    if let Some(d) = l.declared_bounds {
        let tol = 1.0e-6_f64.max(1.0e-9 * max_norm);
        let off = (0..3).any(|i| (d[i] - rng[i][0]).abs() > tol || (d[i + 3] - rng[i][1]).abs() > tol);
        if off {
            r.add("bounds", Severity::Refuse, false, format!("{}: declared bounds {d:?} do not match the data ranges {rng:?}", l.name));
        }
    }

    // Gate 1, region transform: ENU -> ECEF -> ENU must return the point within a millimetre.
    if let (Frame::EnuLocal, Some(a)) = (l.frame, l.anchor)
        && let Some(p) = finite.first()
    {
        let back = a.ecef_to_enu(a.enu_to_ecef(**p));
        let err = norm(&[back[0] - p[0], back[1] - p[1], back[2] - p[2]]);
        if err > ROUND_TRIP_M {
            r.add("transform", Severity::Refuse, false, format!("{}: T_ecef_local round trip is off by {err:.4} m (limit 0.001 m)", l.name));
        }
    }

    // Gate 4: magnitude against the LOD table (draft policy).
    let span_m = match l.frame {
        Frame::Wgs84Deg => {
            let lat_mid = ((rng[1][0] + rng[1][1]) / 2.0).to_radians();
            (rng[1][1] - rng[1][0]).abs().max((rng[0][1] - rng[0][0]).abs() * lat_mid.cos()) * 111_320.0
        }
        _ => (0..3).map(|i| rng[i][1] - rng[i][0]).fold(0.0, f64::max),
    };
    let (lo, hi) = l.lod.span_m();
    if span_m < lo || span_m > hi {
        let hint = if (span_m * 1000.0 >= lo && span_m * 1000.0 <= hi) || (span_m / 1000.0 >= lo && span_m / 1000.0 <= hi) {
            "; off by exactly 1000 fits, check a mm/m or m/km slip"
        } else {
            ""
        };
        r.add("magnitude", Severity::Warn, true, format!("{}: largest span {span_m:.3} m is outside the {:?} range {lo}..{hi} m{hint}", l.name, l.lod));
    }

    // Gate 5: every region at L3 and below carries an anchor (draft policy).
    if l.lod >= Lod::L3 && l.anchor.is_none() {
        r.add("anchor", Severity::Warn, true, format!("{}: no site anchor (lat, lon, ellipsoidal height, datum, accuracy); the region cannot be placed on the globe or hand-checked", l.name));
    }
    r
}

fn cx_is_null_island(r: [[f64; 2]; 3]) -> bool {
    r[0][0].abs() < 1e-9 && r[0][1].abs() < 1e-9 && r[1][0].abs() < 1e-9 && r[1][1].abs() < 1e-9
}

/// Gate 2: row accounting for one join of `left` against `right`.
pub fn check_join(left: &Layer, right: &Layer, left_rows: usize, out_rows: usize) -> Report {
    let mut r = Report::default();
    if left.frame != right.frame {
        r.add("join", Severity::Refuse, false, format!("{} ({:?}) and {} ({:?}) are in different frames; the join would run and return wrong rows", left.name, left.frame, right.name, right.frame));
    }
    if let (Some(a), Some(b)) = (bounds(left.points), bounds(right.points))
        && (0..3).any(|i| a[i][1] < b[i][0] || b[i][1] < a[i][0])
    {
        r.add("join", Severity::Refuse, false, format!("{} and {} do not overlap on every axis; zero rows is guaranteed and would look like a data result", left.name, right.name));
    }
    if left_rows > 0 && out_rows == 0 {
        r.add("join", Severity::Refuse, false, format!("{left_rows} left rows in, 0 out; no spatial relationship, usually a frame mismatch"));
    }
    if left_rows > 0 {
        let rate = out_rows as f64 / left_rows as f64;
        let msg = |v: &str| format!("match rate {:.1}% ({out_rows} of {left_rows}): {v}", rate * 100.0);
        if rate > 1.0 {
            r.add("join", Severity::Warn, true, msg("one-to-many; policy would refuse to aggregate until a rule (largest overlap, split by area, keep all labelled) is chosen"));
        } else if rate < 0.90 {
            r.add("join", Severity::Warn, true, msg("policy would refuse below 90%; report and investigate"));
        } else if rate < 0.98 {
            r.add("join", Severity::Warn, true, msg("list and explain the unmatched rows before proceeding"));
        }
    }
    r
}

/// Tier 1: an SVG scatter of the raw coordinates with plain labelled axes, no basemap, no
/// reprojection, so the axis ranges are visible next to the points.
pub fn raw_snapshot_svg(layers: &[&Layer]) -> String {
    let all: Vec<[f64; 3]> = layers.iter().flat_map(|l| l.points.iter().copied()).collect();
    let Some(r) = bounds(&all) else { return String::from("<svg xmlns=\"http://www.w3.org/2000/svg\"/>") };
    let (w, h, m) = (480.0, 360.0, 40.0);
    let sx = |v: f64| m + (v - r[0][0]) / (r[0][1] - r[0][0]).max(1e-12) * (w - 2.0 * m);
    let sy = |v: f64| h - m - (v - r[1][0]) / (r[1][1] - r[1][0]).max(1e-12) * (h - 2.0 * m);
    let mut s = format!("<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{w}\" height=\"{h}\">");
    s += &format!("<text x=\"{m}\" y=\"{}\" font-size=\"10\">x {:.6} .. {:.6}</text>", h - 8.0, r[0][0], r[0][1]);
    s += &format!("<text x=\"4\" y=\"14\" font-size=\"10\">y {:.6} .. {:.6}</text>", r[1][0], r[1][1]);
    for (i, l) in layers.iter().enumerate() {
        let colour = ["#c00", "#06c", "#080", "#a60"][i % 4];
        for p in l.points.iter().filter(|p| p.iter().all(|v| v.is_finite())) {
            s += &format!("<circle cx=\"{:.1}\" cy=\"{:.1}\" r=\"2\" fill=\"{colour}\"><title>{}</title></circle>", sx(p[0]), sy(p[1]), l.name);
        }
    }
    s + "</svg>"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layer<'a>(name: &'a str, frame: Frame, lod: Lod, pts: &'a [[f64; 3]]) -> Layer<'a> {
        Layer { name, frame, lod, points: pts, declared_bounds: None, anchor: None, nulls: 0, invalid: 0, nulls_handled: false }
    }

    // Synthetic site anchor for tests (not a surveyed point).
    const SITE: Anchor = Anchor { lat_deg: 43.65, lon_deg: -79.38, h_m: 76.0 };

    #[test]
    fn geodesy_matches_the_known_ellipsoid_points() {
        let e = geodetic_to_ecef(0.0, 0.0, 0.0);
        assert!((e[0] - 6_378_137.0).abs() < 1e-6 && e[1].abs() < 1e-6 && e[2].abs() < 1e-6);
        let p = geodetic_to_ecef(90.0, 0.0, 0.0);
        assert!((p[2] - 6_356_752.314_245).abs() < 1e-3, "polar radius, got {}", p[2]);
    }

    #[test]
    fn measured_room_shell_passes_with_only_the_draft_anchor_warning() {
        // contracts/sensors/vectors/evidence/valid/shell_measure.jsonl: min [0,0,0], max [6.4,3.66,2.44].
        let pts = [[0.0, 0.0, 0.0], [6.4, 3.66, 2.44]];
        let mut l = layer("region/urth/meso/test-room", Frame::EnuLocal, Lod::L4, &pts);
        l.declared_bounds = Some([0.0, 0.0, 0.0, 6.4, 3.66, 2.44]);
        let r = check_layer(&l);
        assert!(!r.refused(), "{}", r.refusal_text());
        assert_eq!(r.findings.len(), 1);
        assert_eq!(r.findings[0].gate, "anchor");
        assert!(r.findings[0].draft_policy && r.findings[0].severity == Severity::Warn);
    }

    #[test]
    fn unapplied_transform_is_refused() {
        // A room whose leaves are still ECEF although the region says ENU.
        let pts = [SITE.enu_to_ecef([0.0, 0.0, 0.0]), SITE.enu_to_ecef([6.4, 3.66, 2.44])];
        let mut l = layer("room", Frame::EnuLocal, Lod::L4, &pts);
        l.anchor = Some(SITE);
        let r = check_layer(&l);
        assert!(r.refused());
        assert!(r.refusal_text().contains("never applied"), "{}", r.refusal_text());
    }

    #[test]
    fn swapped_axes_are_refused() {
        // Site at lat 43.65, lon -79.38 stored latitude-first under a longitude-first label.
        let pts = [[43.650, -79.381, 76.0], [43.651, -79.380, 76.0]];
        let mut l = layer("site", Frame::Wgs84Deg, Lod::L3, &pts);
        l.anchor = Some(SITE);
        let r = check_layer(&l);
        assert!(r.refused() && r.refusal_text().contains("swapped"), "{}", r.refusal_text());
        // Same points the right way round are accepted.
        let ok = [[-79.381, 43.650, 76.0], [-79.380, 43.651, 76.0]];
        let mut l = layer("site", Frame::Wgs84Deg, Lod::L3, &ok);
        l.anchor = Some(SITE);
        assert!(!check_layer(&l).refused());
    }

    #[test]
    fn lost_coordinates_and_wrong_labels_are_refused() {
        let origin = [[0.0, 0.0, 0.0]];
        assert!(check_layer(&layer("a", Frame::Wgs84Deg, Lod::L3, &origin)).refused(), "null island");
        assert!(check_layer(&layer("b", Frame::Ecef, Lod::L4, &origin)).refused(), "ECEF origin");
        let metres = [[630_000.0, 4_830_000.0, 0.0]];
        assert!(check_layer(&layer("c", Frame::Wgs84Deg, Lod::L2, &metres)).refused(), "UTM metres as degrees");
        let nan = [[f64::NAN, 1.0, 0.0]];
        assert!(check_layer(&layer("d", Frame::EnuLocal, Lod::L4, &nan)).refused());
    }

    #[test]
    fn declared_bounds_must_match_the_data() {
        let pts = [[0.0, 0.0, 0.0], [6.4, 3.66, 2.44]];
        let mut l = layer("room", Frame::EnuLocal, Lod::L4, &pts);
        l.declared_bounds = Some([0.0, 0.0, 0.0, 6400.0, 3660.0, 2440.0]);
        assert!(check_layer(&l).refused());
    }

    #[test]
    fn unhandled_nulls_are_refused_and_handled_ones_pass() {
        let pts = [[0.0, 0.0, 0.0], [6.4, 3.66, 2.44]];
        let mut l = layer("room", Frame::EnuLocal, Lod::L4, &pts);
        l.nulls = 3;
        assert!(check_layer(&l).refused());
        l.nulls_handled = true;
        assert!(!check_layer(&l).refused());
    }

    #[test]
    fn magnitude_is_a_draft_warning_not_a_refusal() {
        // A room recorded in millimetres: 6400 m wide at L4.
        let pts = [[0.0, 0.0, 0.0], [6400.0, 3660.0, 2440.0]];
        let r = check_layer(&layer("room", Frame::EnuLocal, Lod::L4, &pts));
        let m = r.findings.iter().find(|f| f.gate == "magnitude").expect("magnitude warning");
        assert!(m.draft_policy && m.severity == Severity::Warn && m.message.contains("1000"));
        assert!(!r.refused());
    }

    #[test]
    fn joins_across_frames_or_with_no_overlap_are_refused_and_rates_only_warn() {
        let a = [[0.0, 0.0, 0.0], [6.4, 3.66, 2.44]];
        let b = [[1.0, 1.0, 1.0], [2.0, 2.0, 2.0]];
        let far = [[100.0, 100.0, 100.0], [101.0, 101.0, 101.0]];
        let la = layer("a", Frame::EnuLocal, Lod::L4, &a);
        let lb = layer("b", Frame::EnuLocal, Lod::L4, &b);
        let lf = layer("far", Frame::EnuLocal, Lod::L4, &far);
        let lw = layer("w", Frame::Wgs84Deg, Lod::L4, &b);
        assert!(!check_join(&la, &lb, 100, 99).refused());
        assert!(check_join(&la, &lw, 100, 99).refused());
        assert!(check_join(&la, &lf, 100, 0).refused());
        assert!(check_join(&la, &lb, 100, 0).refused(), "zero rows out");
        let low = check_join(&la, &lb, 100, 40);
        assert!(!low.refused() && low.findings.iter().all(|f| f.draft_policy));
        let many = check_join(&la, &lb, 100, 140);
        assert!(!many.refused() && many.findings.len() == 1);
    }

    #[test]
    fn snapshot_shows_the_raw_ranges() {
        let pts = [[0.0, 0.0, 0.0], [6.4, 3.66, 2.44]];
        let l = layer("room", Frame::EnuLocal, Lod::L4, &pts);
        let svg = raw_snapshot_svg(&[&l]);
        assert!(svg.contains("x 0.000000 .. 6.400000") && svg.contains("y 0.000000 .. 3.660000"));
    }
}
