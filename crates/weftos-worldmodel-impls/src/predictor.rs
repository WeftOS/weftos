//! Action-conditioned latent predictors (`pred_φ`: z_t, a_t → ẑ_{t+1}`).
//!
//! # Runtime path (WEFT-529)
//!
//! [`LinearPredPhi`] is the default production predictor. Untrained it is a
//! weights-free linear residual map from the 32-d action code into the 192-d
//! latent (`ẑ_i = z_i + scale · a_{i mod 32}`). Built with
//! [`LinearPredPhi::from_checkpoint_weights`] it serves the learned
//! action-conditioned model `ẑ = z + A·a + b` that both training surfaces
//! emit ([`crate::training::ActionLinearModel`]). The AdaLN `pred_φ` lives
//! under `crate::candle` (feature `candle`).

use alloc::sync::Arc;

use weftos_worldmodel_core::{
    zero_latent, Action, Latent, Predictor, WorldModelResult, LATENT_DIM,
};

use crate::training::ActionLinearModel;

/// Stub predictor: returns the prior mean (zero latent), ignoring inputs.
///
/// Useful when a host wants a pure no-op that does not echo `z_t` (contrast
/// [`IdentityPredictor`] / core `NullPredictor`).
#[derive(Debug, Default, Clone, Copy)]
pub struct NullPredictor;

impl Predictor for NullPredictor {
    fn predict(&self, _z_t: &Latent, _action: &Action) -> WorldModelResult<Latent> {
        Ok(zero_latent())
    }
}

/// Identity predictor: `ẑ_{t+1} = z_t` (action ignored).
///
/// Matches `weftos_worldmodel_core::NullPredictor` semantics; preferred name
/// in the impls crate so "null" means zero prior and "identity" means hold.
#[derive(Debug, Default, Clone, Copy)]
pub struct IdentityPredictor;

impl Predictor for IdentityPredictor {
    fn predict(&self, z_t: &Latent, _action: &Action) -> WorldModelResult<Latent> {
        Ok(*z_t)
    }
}

/// Linear action-conditioned predictor `pred_φ`.
///
/// ```text
/// untrained: ẑ_i = z_i + scale * a_{i mod 32}
/// learned:   ẑ   = z + A·a + b          (scale ignored)
/// ```
///
/// This is the default runtime dynamics model used by [`crate::planner::CemPlanner`].
/// Deterministic, `no_std`, and unit-testable. The learned model is shared
/// behind an `Arc`, so clones (planner rollouts, lattice copies) are cheap.
#[derive(Debug, Clone)]
pub struct LinearPredPhi {
    /// Multiplier on the action residual of the untrained map.
    pub scale: f32,
    /// Learned `A`, `b` from a training checkpoint, when loaded.
    learned: Option<Arc<ActionLinearModel>>,
}

impl Default for LinearPredPhi {
    fn default() -> Self {
        Self::new(1.0)
    }
}

impl LinearPredPhi {
    /// Construct the untrained map with an explicit residual scale.
    pub const fn new(scale: f32) -> Self {
        Self { scale, learned: None }
    }

    /// Serve a learned action-linear model.
    pub fn from_model(model: ActionLinearModel) -> Self {
        Self { scale: 1.0, learned: Some(Arc::new(model)) }
    }

    /// Build from `ModelCheckpoint.weights` produced by either training surface (`LWAL`
    /// blob, or the legacy 768-byte residual).
    pub fn from_checkpoint_weights(weights: &[u8]) -> WorldModelResult<Self> {
        ActionLinearModel::from_weights(weights).map(Self::from_model)
    }

    /// Learned model, when one is loaded.
    pub fn learned(&self) -> Option<&ActionLinearModel> {
        self.learned.as_deref()
    }
}

impl Predictor for LinearPredPhi {
    fn predict(&self, z_t: &Latent, action: &Action) -> WorldModelResult<Latent> {
        if let Some(m) = &self.learned {
            return Ok(m.predict(z_t, action));
        }
        let mut out = *z_t;
        let code = &action.code;
        let n_a = code.len(); // 32
        for i in 0..LATENT_DIM {
            out[i] += self.scale * code[i % n_a];
        }
        Ok(out)
    }
}

/// Alias used in docs / service wiring for the production `pred_φ` path.
pub type PredPhi = LinearPredPhi;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_is_zero() {
        let mut z = zero_latent();
        z[0] = 3.0;
        assert_eq!(
            NullPredictor.predict(&z, &Action::null()).unwrap(),
            zero_latent()
        );
    }

    #[test]
    fn identity_holds() {
        let mut z = zero_latent();
        z[10] = 2.0;
        assert_eq!(IdentityPredictor.predict(&z, &Action::null()).unwrap(), z);
    }

    #[test]
    fn linear_pred_phi_applies_action_residual() {
        let mut z = zero_latent();
        z[0] = 1.0;
        z[32] = 2.0;
        let mut a = Action::null();
        a.code[0] = 0.5;
        let pred = LinearPredPhi::new(2.0);
        let out = pred.predict(&z, &a).unwrap();
        // dim 0 and 32 both get + scale * a[0]
        assert!((out[0] - (1.0 + 2.0 * 0.5)).abs() < 1e-5);
        assert!((out[32] - (2.0 + 2.0 * 0.5)).abs() < 1e-5);
        // untouched channel stays put
        assert!((out[1] - 0.0).abs() < 1e-5);
    }

    #[test]
    fn from_checkpoint_weights_serves_the_learned_model() {
        let mut m = ActionLinearModel::zero();
        m.a[0] = 2.0; // z[0] += 2 * a[0]
        m.b[1] = 0.5;
        let pred = LinearPredPhi::from_checkpoint_weights(&m.to_weights()).unwrap();
        let mut a = Action::null();
        a.code[0] = 0.25;
        let out = pred.predict(&zero_latent(), &a).unwrap();
        assert!((out[0] - 0.5).abs() < 1e-6 && (out[1] - 0.5).abs() < 1e-6);
        assert!(out[32].abs() < 1e-6, "learned path does not broadcast the action");
        assert!(pred.learned().is_some() && LinearPredPhi::default().learned().is_none());
        assert!(LinearPredPhi::from_checkpoint_weights(&[0u8; 7]).is_err());
    }

    #[test]
    fn null_action_is_identity_under_linear_pred() {
        let mut z = zero_latent();
        z[5] = -1.25;
        let out = LinearPredPhi::default()
            .predict(&z, &Action::null())
            .unwrap();
        assert_eq!(out, z);
    }
}
