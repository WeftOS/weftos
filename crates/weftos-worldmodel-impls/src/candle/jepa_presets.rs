//! Named [`JepaTrainConfig`] presets: the default (lane U), lane J's collapse fix and lane G's
//! collapsing settings.

use super::*;

impl Default for JepaTrainConfig {
    /// The lane U θ-headroom winner: [`Self::lane_j`] with the learning rate on a cosine to
    /// 0.1× over the run, VICReg hinge weight 2 and `τ = 0.995`. Five-seed ensemble median θ
    /// ratio 0.724 on Metal (fast math) and 0.713 on CPU, against 0.905 / 0.826 for `lane_j`.
    fn default() -> Self {
        Self { lr_final_ratio: 0.1, vic_var_weight: 2.0, ema_tau: 0.995, ..Self::lane_j() }
    }
}

impl JepaTrainConfig {
    /// The lane J bake-off winner and the default until lane U: [`Self::lane_g`] plus batch 32,
    /// `τ = 0.99`, and the VICReg hinge (weight 1) and correlation decorrelation (weight 0.04)
    /// on the online latents. Fixes the collapse, but its Metal ensemble median θ ratio is
    /// 0.905, over the 0.9 bar.
    pub fn lane_j() -> Self {
        Self { batch_size: 32, ema_tau: 0.99, vic_var_weight: 1.0, vic_cov_weight: 0.04, vic_correlation: true, ..Self::lane_g() }
    }

    /// The lane G settings (std sliced term, `K = 64`, constant `τ = 0.996`, no multi-step or
    /// VICReg terms), which collapse the default ViT-tiny to rank one. Kept for comparison.
    pub fn lane_g() -> Self {
        Self {
            epochs: 10,
            lr: 1e-3,
            lr_final_ratio: 1.0,
            weight_decay: 0.01,
            batch_size: 16,
            seed: 0,
            ema_tau: 0.996,
            prediction_weight: 1.0,
            iso_weight: 1.0,
            offdiag_weight: 0.0,
            slice_weight: 1.0,
            slice_projections: 64,
            slice_test: SliceTest::Std,
            ema_tau_start: None,
            multistep_weight: 0.0,
            vic_var_weight: 0.0,
            vic_cov_weight: 0.0,
            vic_correlation: false,
            pos_embed_std: VIT_POS_EMBED_STD,
            predictor_refit_epochs: 0,
            diagnostics: false,
            holdout_days: 1,
            encoder: VitTinyConfig::default(),
            predictor: AdaLnConfig::default(),
            action_from_scalar: synthetic::action_code,
            eval: EvalConfig::default(),
            device: ComputeDevice::Cpu,
            numerics: JepaNumerics::default(),
        }
    }
}
