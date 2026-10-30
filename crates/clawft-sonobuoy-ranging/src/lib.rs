//! `clawft-sonobuoy-ranging` — inter-buoy acoustic ranging primitives (WEFT-535).
//!
//! Scaffold follow-up to G1 (sensor-position uncertainty) closed in
//! `.planning/sonobuoy/RANGING.md` and ADR-078. Ships pure geometry +
//! OWTT helpers with **no** kernel / mesh deps.
//!
//! # Scope (v2 scaffold)
//!
//! - One-way travel-time (OWTT) range from travel time + sound speed
//! - Pairwise range observation with optional covariance
//! - Dense distance matrix `D(t)` assembly for N buoys
//! - Synthetic-field fixtures for unit tests
//!
//! Full OWTT+Doppler protocol, TSHL clock sync, multipath SSP inversion,
//! and mesh wire integration are deferred (see RANGING.md §0).

#![warn(missing_docs)]

use serde::{Deserialize, Serialize};

/// Crate version string for diagnostics.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Default nominal sound speed in seawater (m/s) used when no SSP is known.
///
/// Mackenzie / UNESCO mid-latitude surface reference ~1500 m/s; RANGING.md
/// notes residual bias must be corrected via in-situ SSP estimation.
pub const DEFAULT_SOUND_SPEED_MPS: f64 = 1500.0;

/// Opaque buoy identifier within a ranging field.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BuoyId(pub String);

impl BuoyId {
    /// Construct from any string-like value.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

impl std::fmt::Display for BuoyId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One-way travel-time range observation between two buoys.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RangeObservation {
    /// Source (transmitting) buoy.
    pub from: BuoyId,
    /// Destination (receiving) buoy.
    pub to: BuoyId,
    /// One-way travel time in seconds.
    pub travel_time_s: f64,
    /// Sound-speed estimate used for conversion (m/s).
    pub sound_speed_mps: f64,
    /// Optional 1σ range uncertainty in metres.
    pub sigma_m: Option<f64>,
    /// Observation epoch (unix seconds); optional for offline fixtures.
    pub epoch_unix: Option<u64>,
}

impl RangeObservation {
    /// Convert travel time to geometric range: `r = c · τ`.
    pub fn range_m(&self) -> f64 {
        self.travel_time_s * self.sound_speed_mps
    }

    /// Build an observation from travel time with default sound speed.
    pub fn from_travel_time(from: BuoyId, to: BuoyId, travel_time_s: f64) -> Self {
        Self {
            from,
            to,
            travel_time_s,
            sound_speed_mps: DEFAULT_SOUND_SPEED_MPS,
            sigma_m: None,
            epoch_unix: None,
        }
    }
}

/// Convert one-way travel time to range given a sound-speed profile scalar.
///
/// `travel_time_s` must be non-negative; `sound_speed_mps` must be positive.
/// Returns `None` when inputs are invalid.
pub fn owtt_range_m(travel_time_s: f64, sound_speed_mps: f64) -> Option<f64> {
    if !travel_time_s.is_finite()
        || !sound_speed_mps.is_finite()
        || travel_time_s < 0.0
        || sound_speed_mps <= 0.0
    {
        return None;
    }
    Some(travel_time_s * sound_speed_mps)
}

/// Dense inter-buoy distance matrix `D ∈ R^{N×N}` (metres).
///
/// Diagonal is zero. Missing pairs remain `None`. Indices align with
/// [`DistanceMatrix::buoys`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DistanceMatrix {
    /// Ordered buoy IDs (row/column order).
    pub buoys: Vec<BuoyId>,
    /// Row-major optional ranges; length = N².
    pub ranges_m: Vec<Option<f64>>,
}

impl DistanceMatrix {
    /// Allocate an empty N×N matrix for the given buoy set.
    pub fn empty(buoys: Vec<BuoyId>) -> Self {
        let n = buoys.len();
        let mut ranges_m = vec![None; n * n];
        for i in 0..n {
            ranges_m[i * n + i] = Some(0.0);
        }
        Self { buoys, ranges_m }
    }

    /// Number of buoys (matrix dimension).
    pub fn len(&self) -> usize {
        self.buoys.len()
    }

    /// True when no buoys are registered.
    pub fn is_empty(&self) -> bool {
        self.buoys.is_empty()
    }

    fn index_of(&self, id: &BuoyId) -> Option<usize> {
        self.buoys.iter().position(|b| b == id)
    }

    /// Set a (possibly asymmetric) range observation. Also mirrors to
    /// the reverse pair when `symmetric` is true (OWTT reciprocity
    /// assumption for slowly-moving surface fields).
    pub fn set(&mut self, from: &BuoyId, to: &BuoyId, range_m: f64, symmetric: bool) -> bool {
        let (Some(i), Some(j)) = (self.index_of(from), self.index_of(to)) else {
            return false;
        };
        let n = self.buoys.len();
        self.ranges_m[i * n + j] = Some(range_m);
        if symmetric {
            self.ranges_m[j * n + i] = Some(range_m);
        }
        true
    }

    /// Read range `D[i,j]` if known.
    pub fn get(&self, from: &BuoyId, to: &BuoyId) -> Option<f64> {
        let (i, j) = (self.index_of(from)?, self.index_of(to)?);
        let n = self.buoys.len();
        self.ranges_m[i * n + j]
    }

    /// Ingest a batch of [`RangeObservation`]s (symmetric by default).
    pub fn ingest(&mut self, obs: impl IntoIterator<Item = RangeObservation>, symmetric: bool) {
        for o in obs {
            let _ = self.set(&o.from, &o.to, o.range_m(), symmetric);
        }
    }

    /// Fraction of off-diagonal pairs with a known range.
    pub fn fill_ratio(&self) -> f64 {
        let n = self.buoys.len();
        if n < 2 {
            return 1.0;
        }
        let total = n * (n - 1);
        let mut known = 0usize;
        for i in 0..n {
            for j in 0..n {
                if i != j && self.ranges_m[i * n + j].is_some() {
                    known += 1;
                }
            }
        }
        known as f64 / total as f64
    }
}

/// Synthetic N-buoy ring field for unit tests (equal spacing on a circle).
///
/// Returns ground-truth Euclidean distances for the chord between
/// consecutive buoys and the full distance matrix.
pub fn synthetic_ring_field(n: usize, radius_m: f64) -> (Vec<BuoyId>, DistanceMatrix, f64) {
    assert!(n >= 2, "need at least 2 buoys");
    let buoys: Vec<BuoyId> = (0..n).map(|i| BuoyId::new(format!("buoy-{i}"))).collect();
    let mut mat = DistanceMatrix::empty(buoys.clone());
    let positions: Vec<(f64, f64)> = (0..n)
        .map(|i| {
            let theta = 2.0 * std::f64::consts::PI * (i as f64) / (n as f64);
            (radius_m * theta.cos(), radius_m * theta.sin())
        })
        .collect();
    for i in 0..n {
        for j in (i + 1)..n {
            let dx = positions[i].0 - positions[j].0;
            let dy = positions[i].1 - positions[j].1;
            let d = (dx * dx + dy * dy).sqrt();
            mat.set(&buoys[i], &buoys[j], d, true);
        }
    }
    // Chord length between consecutive buoys on the ring.
    let chord = 2.0 * radius_m * (std::f64::consts::PI / n as f64).sin();
    (buoys, mat, chord)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owtt_basic() {
        // 1 s · 1500 m/s = 1500 m
        assert!((owtt_range_m(1.0, 1500.0).unwrap() - 1500.0).abs() < 1e-9);
        assert!(owtt_range_m(-0.1, 1500.0).is_none());
        assert!(owtt_range_m(1.0, 0.0).is_none());
    }

    #[test]
    fn owtt_rejects_invalid() {
        assert!(owtt_range_m(-1.0, 1500.0).is_none());
        assert!(owtt_range_m(f64::NAN, 1500.0).is_none());
        assert!(owtt_range_m(f64::INFINITY, 1500.0).is_none());
        assert!(owtt_range_m(1.0, 0.0).is_none());
        assert!(owtt_range_m(1.0, -1500.0).is_none());
        assert!(owtt_range_m(1.0, f64::NAN).is_none());
        assert_eq!(owtt_range_m(0.0, 1500.0), Some(0.0));
    }

    #[test]
    fn observation_range() {
        let o = RangeObservation::from_travel_time(BuoyId::new("a"), BuoyId::new("b"), 0.1);
        assert!((o.range_m() - 150.0).abs() < 1e-9);
    }

    #[test]
    fn distance_matrix_ingest_synthetic() {
        let (buoys, mat, chord) = synthetic_ring_field(4, 100.0);
        assert_eq!(mat.len(), 4);
        assert!((mat.fill_ratio() - 1.0).abs() < 1e-12);
        let d01 = mat.get(&buoys[0], &buoys[1]).unwrap();
        assert!(
            (d01 - chord).abs() < 1e-9,
            "expected chord {chord}, got {d01}"
        );
        assert_eq!(mat.get(&buoys[0], &buoys[0]), Some(0.0));
    }

    #[test]
    fn serde_roundtrip_matrix() {
        let (_buoys, mat, _) = synthetic_ring_field(3, 50.0);
        let json = serde_json::to_string(&mat).unwrap();
        let back: DistanceMatrix = serde_json::from_str(&json).unwrap();
        assert_eq!(back.len(), 3);
        assert!((back.fill_ratio() - 1.0).abs() < 1e-12);
    }

    fn ids(n: usize) -> Vec<BuoyId> {
        (0..n).map(|i| BuoyId::new(format!("b{i}"))).collect()
    }

    #[test]
    fn matrix_empty_diagonal_and_len() {
        let m = DistanceMatrix::empty(ids(3));
        assert_eq!(m.len(), 3);
        assert!(!m.is_empty());
        assert_eq!(m.ranges_m.len(), 9);
        for i in 0..3 {
            for j in 0..3 {
                let expected = if i == j { Some(0.0) } else { None };
                assert_eq!(m.ranges_m[i * 3 + j], expected);
            }
        }
        let e = DistanceMatrix::empty(Vec::new());
        assert_eq!(e.len(), 0);
        assert!(e.is_empty());
    }

    #[test]
    fn matrix_set_asymmetric_fills_one_cell() {
        let b = ids(2);
        let mut m = DistanceMatrix::empty(b.clone());
        assert!(m.set(&b[0], &b[1], 42.0, false));
        assert_eq!(m.get(&b[0], &b[1]), Some(42.0));
        assert_eq!(m.get(&b[1], &b[0]), None);
    }

    #[test]
    fn matrix_set_symmetric_fills_both_cells() {
        let b = ids(2);
        let mut m = DistanceMatrix::empty(b.clone());
        assert!(m.set(&b[0], &b[1], 42.0, true));
        assert_eq!(m.get(&b[0], &b[1]), Some(42.0));
        assert_eq!(m.get(&b[1], &b[0]), Some(42.0));
    }

    #[test]
    fn matrix_set_unknown_id_changes_nothing() {
        let b = ids(2);
        let mut m = DistanceMatrix::empty(b.clone());
        let before = m.ranges_m.clone();
        let unknown = BuoyId::new("nope");
        assert!(!m.set(&unknown, &b[1], 1.0, true));
        assert!(!m.set(&b[0], &unknown, 1.0, true));
        assert_eq!(m.ranges_m, before);
    }

    #[test]
    fn matrix_get_unknown_id_is_none() {
        let b = ids(2);
        let m = DistanceMatrix::empty(b.clone());
        let unknown = BuoyId::new("nope");
        assert_eq!(m.get(&unknown, &b[0]), None);
        assert_eq!(m.get(&b[0], &unknown), None);
        assert_eq!(m.get(&unknown, &unknown), None);
    }

    #[test]
    fn matrix_fill_ratio_cases() {
        assert!((DistanceMatrix::empty(Vec::new()).fill_ratio() - 1.0).abs() < 1e-12);
        assert!((DistanceMatrix::empty(ids(1)).fill_ratio() - 1.0).abs() < 1e-12);

        let b = ids(3);
        let mut m = DistanceMatrix::empty(b.clone());
        assert!(m.fill_ratio().abs() < 1e-12);
        assert!(m.set(&b[0], &b[1], 10.0, true));
        assert!((m.fill_ratio() - 1.0 / 3.0).abs() < 1e-12);
    }

    #[test]
    fn version_nonempty() {
        assert!(!VERSION.is_empty());
    }
}
