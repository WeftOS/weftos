//! Action-conditioned linear dynamics `ẑ = z + A·a + b` (plan W5, default `no_std` path).
//!
//! `A` is `LATENT_DIM × ACTION_CODE_DIM` (192 × 32), `b` is `LATENT_DIM`. Two fits share one
//! loss, the importance-weighted mean `‖z + A·a + b − z_{t+1}‖²`:
//!
//! - [`ActionLinearModel::fit_ridge`]: closed-form ridge least squares on the 33-d augmented
//!   action `x = [a; 1]` (normal equations, one 33 × 33 solve shared by all 192 outputs). Used
//!   by the offline edge surface.
//! - [`ActionLinearModel::sgd_step`]: one normalised-LMS gradient step per batch. Used by the
//!   streaming-merge surface. With a null action it reduces exactly to the old residual EMA
//!   `b ← (1 − lr)·b + lr·mean(z_{t+1} − z_t)`.
//!
//! # Weight blob (`ModelCheckpoint.weights`, codec `0x02` raw)
//!
//! ```text
//! magic "LWAL" (4) | format u8 = 1 | reserved u8 = 0 | latent_dim u16 LE | action_dim u16 LE
//! A row-major (latent_dim × action_dim) f32 LE | b (latent_dim) f32 LE
//! ```
//!
//! [`ActionLinearModel::from_weights`] also accepts the legacy 768-byte residual blob (192 f32,
//! the pre-W5 residual-mean stub) as `A = 0, b = residual`.

use alloc::vec;
use alloc::vec::Vec;

use weftos_worldmodel_core::{
    zero_latent, Action, Latent, TrainingSample, WorldModelError, WorldModelResult, LATENT_DIM,
};

use super::replay::ImportanceReplayBuffer;
use crate::action_encoder::ACTION_CODE_DIM;

/// Magic prefix of the action-linear weight blob.
pub const ACTION_LINEAR_MAGIC: [u8; 4] = *b"LWAL";
/// Format version byte.
pub const ACTION_LINEAR_FORMAT_VERSION: u8 = 1;
/// Header bytes before the f32 payload.
pub const ACTION_LINEAR_HEADER_LEN: usize = 10;
/// Total blob length for the v1 192 × 32 model.
pub const ACTION_LINEAR_BLOB_LEN: usize =
    ACTION_LINEAR_HEADER_LEN + (LATENT_DIM * ACTION_CODE_DIM + LATENT_DIM) * 4;
/// Default ridge penalty on `A` (per unit of sample weight).
pub const DEFAULT_RIDGE_LAMBDA: f32 = 1e-3;

/// Augmented input width `[a; 1]`.
const AUG: usize = ACTION_CODE_DIM + 1;

/// Action-conditioned linear `pred_φ`: `ẑ = z + A·a + b`.
#[derive(Debug, Clone, PartialEq)]
pub struct ActionLinearModel {
    /// Row-major `LATENT_DIM × ACTION_CODE_DIM` action matrix.
    pub a: Vec<f32>,
    /// Action-free residual (bias).
    pub b: Latent,
}

impl Default for ActionLinearModel {
    fn default() -> Self {
        Self::zero()
    }
}

impl ActionLinearModel {
    /// `A = 0, b = 0` (copy-last).
    pub fn zero() -> Self {
        Self { a: vec![0.0; LATENT_DIM * ACTION_CODE_DIM], b: zero_latent() }
    }

    /// `A = 0, b = residual` (the pre-W5 residual-mean model).
    pub fn from_residual(residual: Latent) -> Self {
        Self { a: vec![0.0; LATENT_DIM * ACTION_CODE_DIM], b: residual }
    }

    /// `ẑ = z + A·a + b`.
    pub fn predict(&self, z_t: &Latent, action: &Action) -> Latent {
        let mut out = *z_t;
        for (d, o) in out.iter_mut().enumerate() {
            let row = &self.a[d * ACTION_CODE_DIM..(d + 1) * ACTION_CODE_DIM];
            let mut acc = self.b[d];
            for (w, x) in row.iter().zip(action.code.iter()) {
                acc += w * x;
            }
            *o += acc;
        }
        out
    }

    /// Importance-weighted mean `‖ẑ − z_{t+1}‖²` over `samples` (`NaN` when no weight).
    pub fn loss<'a>(&self, samples: impl IntoIterator<Item = &'a TrainingSample>) -> f32 {
        let (mut acc, mut w_sum) = (0.0f32, 0.0f32);
        for s in samples {
            let w = ImportanceReplayBuffer::weight(s);
            if w <= 0.0 {
                continue;
            }
            let z_hat = self.predict(&s.z_t, &s.action);
            let e: f32 = z_hat.iter().zip(s.z_tp1.iter()).map(|(p, y)| (p - y) * (p - y)).sum();
            acc += w * e;
            w_sum += w;
        }
        if w_sum > 0.0 { acc / w_sum } else { f32::NAN }
    }

    /// Whether `bytes` carries the `LWAL` magic.
    pub fn is_blob(bytes: &[u8]) -> bool {
        bytes.len() >= 4 && bytes[..4] == ACTION_LINEAR_MAGIC
    }

    /// Encode as the `LWAL` blob (see module docs).
    pub fn to_weights(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(ACTION_LINEAR_BLOB_LEN);
        out.extend_from_slice(&ACTION_LINEAR_MAGIC);
        out.push(ACTION_LINEAR_FORMAT_VERSION);
        out.push(0);
        out.extend_from_slice(&(LATENT_DIM as u16).to_le_bytes());
        out.extend_from_slice(&(ACTION_CODE_DIM as u16).to_le_bytes());
        for v in self.a.iter().chain(self.b.iter()) {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out
    }

    /// Decode an `LWAL` blob, or a legacy 768-byte residual blob.
    pub fn from_weights(bytes: &[u8]) -> WorldModelResult<Self> {
        if !Self::is_blob(bytes) {
            if bytes.len() == LATENT_DIM * 4 {
                return Ok(Self::from_residual(read_f32s::<LATENT_DIM>(bytes)));
            }
            return Err(WorldModelError::SegmentCodec);
        }
        if bytes.len() != ACTION_LINEAR_BLOB_LEN || bytes[4] != ACTION_LINEAR_FORMAT_VERSION {
            return Err(WorldModelError::SegmentCodec);
        }
        let latent = u16::from_le_bytes([bytes[6], bytes[7]]) as usize;
        let action = u16::from_le_bytes([bytes[8], bytes[9]]) as usize;
        if latent != LATENT_DIM {
            return Err(WorldModelError::dim_mismatch(latent as u16));
        }
        if action != ACTION_CODE_DIM {
            return Err(WorldModelError::Other("action-linear blob: action_dim must be 32"));
        }
        let body = &bytes[ACTION_LINEAR_HEADER_LEN..];
        let n_a = LATENT_DIM * ACTION_CODE_DIM;
        let a = body[..n_a * 4]
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        let b = read_f32s::<LATENT_DIM>(&body[n_a * 4..]);
        Ok(Self { a, b })
    }

    /// Ridge least squares on the importance-weighted samples.
    ///
    /// Minimises `Σ w ‖A·a + b − (z_{t+1} − z_t)‖² + λ·Σw·‖A‖²_F` (the bias is not penalised)
    /// through the 33 × 33 normal equations in `f64`. `None` when no sample carries weight or
    /// the system is numerically singular (it is positive definite for `λ > 0`).
    pub fn fit_ridge<'a>(
        samples: impl IntoIterator<Item = &'a TrainingSample>,
        lambda: f32,
    ) -> Option<Self> {
        // Augmented system [G | R]: G = Σ w x xᵀ (AUG × AUG), R = Σ w x rᵀ (AUG × LATENT_DIM).
        let cols = AUG + LATENT_DIM;
        let mut m = vec![0.0f64; AUG * cols];
        let mut w_sum = 0.0f64;
        let mut x = [0.0f64; AUG];
        for s in samples {
            let w = ImportanceReplayBuffer::weight(s) as f64;
            if w <= 0.0 {
                continue;
            }
            w_sum += w;
            for (xi, ai) in x.iter_mut().zip(s.action.code.iter()) {
                *xi = *ai as f64;
            }
            x[ACTION_CODE_DIM] = 1.0;
            for i in 0..AUG {
                let wx = w * x[i];
                if wx == 0.0 {
                    continue;
                }
                let row = &mut m[i * cols..(i + 1) * cols];
                for j in 0..AUG {
                    row[j] += wx * x[j];
                }
                for d in 0..LATENT_DIM {
                    row[AUG + d] += wx * (s.z_tp1[d] - s.z_t[d]) as f64;
                }
            }
        }
        if w_sum <= 0.0 {
            return None;
        }
        let ridge = (lambda.max(0.0) as f64) * w_sum;
        for i in 0..ACTION_CODE_DIM {
            m[i * cols + i] += ridge;
        }
        solve_in_place(&mut m, AUG, cols)?;
        let mut model = Self::zero();
        for d in 0..LATENT_DIM {
            for j in 0..ACTION_CODE_DIM {
                model.a[d * ACTION_CODE_DIM + j] = m[j * cols + AUG + d] as f32;
            }
            model.b[d] = m[ACTION_CODE_DIM * cols + AUG + d] as f32;
        }
        Some(model)
    }

    /// One normalised-LMS step on `batch`; returns the weighted loss before the update.
    ///
    /// `θ ← θ − lr · mean_w(e xᵀ) / mean_w(‖x‖²)` with `e = ẑ − z_{t+1}`, `x = [a; 1]`. Stable
    /// for `0 < lr < 2`. A batch with no positive weight leaves the model unchanged.
    pub fn sgd_step(&mut self, batch: &[TrainingSample], lr: f32) -> f32 {
        let mut grad_a = vec![0.0f32; LATENT_DIM * ACTION_CODE_DIM];
        let mut grad_b = zero_latent();
        let (mut w_sum, mut norm_sum, mut loss) = (0.0f32, 0.0f32, 0.0f32);
        for s in batch {
            let w = ImportanceReplayBuffer::weight(s);
            if w <= 0.0 {
                continue;
            }
            let z_hat = self.predict(&s.z_t, &s.action);
            let x_norm: f32 = 1.0 + s.action.code.iter().map(|v| v * v).sum::<f32>();
            for d in 0..LATENT_DIM {
                let e = z_hat[d] - s.z_tp1[d];
                loss += w * e * e;
                grad_b[d] += w * e;
                let row = &mut grad_a[d * ACTION_CODE_DIM..(d + 1) * ACTION_CODE_DIM];
                for (g, a) in row.iter_mut().zip(s.action.code.iter()) {
                    *g += w * e * a;
                }
            }
            w_sum += w;
            norm_sum += w * x_norm;
        }
        if w_sum <= 0.0 {
            return f32::NAN;
        }
        // mean(e xᵀ) / mean(‖x‖²) = Σ w e xᵀ / Σ w ‖x‖².
        let step = lr / norm_sum;
        for (p, g) in self.a.iter_mut().zip(grad_a.iter()) {
            *p -= step * g;
        }
        for (p, g) in self.b.iter_mut().zip(grad_b.iter()) {
            *p -= step * g;
        }
        loss / w_sum
    }
}

fn read_f32s<const N: usize>(bytes: &[u8]) -> [f32; N] {
    let mut out = [0.0f32; N];
    for (o, c) in out.iter_mut().zip(bytes.chunks_exact(4)) {
        *o = f32::from_le_bytes([c[0], c[1], c[2], c[3]]);
    }
    out
}

/// Gauss-Jordan elimination with partial pivoting on a row-major `n × cols` augmented matrix
/// `[G | R]`; on success the right block holds `G⁻¹ R`. `None` on a near-zero pivot.
fn solve_in_place(m: &mut [f64], n: usize, cols: usize) -> Option<()> {
    let abs = |v: f64| if v < 0.0 { -v } else { v };
    for col in 0..n {
        let pivot = (col..n).max_by(|&i, &j| {
            abs(m[i * cols + col]).partial_cmp(&abs(m[j * cols + col])).unwrap_or(core::cmp::Ordering::Equal)
        })?;
        if abs(m[pivot * cols + col]) < 1e-12 {
            return None;
        }
        if pivot != col {
            for k in 0..cols {
                m.swap(pivot * cols + k, col * cols + k);
            }
        }
        let inv = 1.0 / m[col * cols + col];
        for k in col..cols {
            m[col * cols + k] *= inv;
        }
        for row in 0..n {
            if row == col {
                continue;
            }
            let f = m[row * cols + col];
            if f == 0.0 {
                continue;
            }
            for k in col..cols {
                m[row * cols + k] -= f * m[col * cols + k];
            }
        }
    }
    Some(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use weftos_worldmodel_core::SensorClass;

    /// xorshift noise in [-1, 1).
    pub(crate) struct Rng(pub u64);
    impl Rng {
        pub(crate) fn next(&mut self) -> f32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            ((self.0 >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
        }
    }

    /// Labelled SYNTHETIC fixture: `z_{t+1} = z_t + A*·a + b* + noise` with a random 32-d
    /// action code per step. Returns the samples and the true model.
    pub(crate) fn action_fixture(
        n: usize,
        class: SensorClass,
        noise: f32,
        seed: u64,
    ) -> (Vec<TrainingSample>, ActionLinearModel) {
        let mut rng = Rng(seed);
        let mut truth = ActionLinearModel::zero();
        for v in truth.a.iter_mut() {
            *v = 0.05 * rng.next();
        }
        for v in truth.b.iter_mut() {
            *v = 0.01 * rng.next();
        }
        let mut out = Vec::with_capacity(n);
        for t in 0..n {
            let mut z = zero_latent();
            for v in z.iter_mut() {
                *v = rng.next();
            }
            let mut action = Action::null();
            for v in action.code.iter_mut() {
                *v = rng.next();
            }
            let mut next = truth.predict(&z, &action);
            for v in next.iter_mut() {
                *v += noise * rng.next();
            }
            let mut s = TrainingSample::basic(class, z, next, t as u64 * 100);
            s.action = action;
            out.push(s);
        }
        (out, truth)
    }

    #[test]
    fn blob_roundtrip_and_legacy_residual() {
        let (_, truth) = action_fixture(1, SensorClass::Imu, 0.0, 3);
        let blob = truth.to_weights();
        assert_eq!(blob.len(), ACTION_LINEAR_BLOB_LEN);
        assert!(ActionLinearModel::is_blob(&blob));
        assert_eq!(ActionLinearModel::from_weights(&blob).unwrap(), truth);

        let mut r = zero_latent();
        r[3] = 1.5;
        let legacy: Vec<u8> = r.iter().flat_map(|v| v.to_le_bytes()).collect();
        let m = ActionLinearModel::from_weights(&legacy).unwrap();
        assert_eq!(m.b, r);
        assert!(m.a.iter().all(|&v| v == 0.0));

        assert!(ActionLinearModel::from_weights(&blob[..20]).is_err());
        assert!(ActionLinearModel::from_weights(&[1, 2, 3]).is_err());
    }

    #[test]
    fn ridge_recovers_the_true_dynamics_and_beats_the_residual_mean() {
        let (data, truth) = action_fixture(300, SensorClass::Imu, 0.001, 11);
        let (train, held) = data.split_at(200);
        let fit = ActionLinearModel::fit_ridge(train, DEFAULT_RIDGE_LAMBDA).expect("fit");
        let max_err = fit.a.iter().zip(truth.a.iter()).map(|(a, b)| (a - b).abs()).fold(0.0, f32::max);
        assert!(max_err < 5e-3, "A error {max_err}");

        let mut mean = zero_latent();
        for s in train {
            for (m, r) in mean.iter_mut().zip(s.residual().iter()) {
                *m += r / train.len() as f32;
            }
        }
        let residual_mean = ActionLinearModel::from_residual(mean);
        let (e_fit, e_mean) = (fit.loss(held), residual_mean.loss(held));
        assert!(e_fit < 0.01 * e_mean, "held-out fit {e_fit} vs residual-mean {e_mean}");
    }

    #[test]
    fn ridge_with_null_actions_is_the_residual_mean() {
        let mut z0 = zero_latent();
        let mut z1 = zero_latent();
        z0[0] = 1.0;
        z1[0] = 3.0;
        let s = [TrainingSample::basic(SensorClass::Imu, z0, z1, 0); 4];
        let fit = ActionLinearModel::fit_ridge(&s, DEFAULT_RIDGE_LAMBDA).unwrap();
        assert!((fit.b[0] - 2.0).abs() < 1e-5);
        assert!(fit.a.iter().all(|v| v.abs() < 1e-9));
        assert!(ActionLinearModel::fit_ridge(core::iter::empty::<&TrainingSample>(), 1e-3).is_none());
    }

    #[test]
    fn streaming_sgd_converges_over_a_few_hundred_samples() {
        let (data, _) = action_fixture(400, SensorClass::RgbD, 0.001, 5);
        let (stream, held) = data.split_at(300);
        let mut m = ActionLinearModel::zero();
        let initial = m.loss(held);
        for t in 0..stream.len() {
            let lo = t.saturating_sub(7);
            m.sgd_step(&stream[lo..=t], 0.5);
        }
        let fin = m.loss(held);
        assert!(fin < 0.05 * initial, "held-out loss {initial} -> {fin}");
    }

    #[test]
    fn sgd_with_null_action_is_the_old_residual_ema() {
        let mut z0 = zero_latent();
        let mut z1 = zero_latent();
        z1[0] = 1.0;
        z0[1] = 2.0;
        z1[1] = 2.0;
        let s = [TrainingSample::basic(SensorClass::Imu, z0, z1, 0)];
        let mut m = ActionLinearModel::zero();
        m.sgd_step(&s, 0.25);
        assert!((m.b[0] - 0.25).abs() < 1e-6);
        m.sgd_step(&s, 0.25);
        assert!((m.b[0] - (0.75 * 0.25 + 0.25)).abs() < 1e-6);
        assert!(m.b[1].abs() < 1e-9);
    }
}
