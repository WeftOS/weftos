//! Registry auto-load of the learned `pred_φ` (lane H).
//!
//! After [`DefaultWorldModel::commit_models_at_tick`] commits an `Encode` checkpoint
//! for a sensor class, the facade rebuilds that class's predictor from the checkpoint:
//!
//! - raw / `LWAL` weights (codec `0x01` / `0x02`) become a [`LinearPredPhi`]
//!   (`ẑ = z + A·a + b`) through [`LinearPredPhi::from_checkpoint_weights`];
//! - safetensors weights (codec `0x03`, feature `candle`) go through
//!   [`weftos_worldmodel_impls::active_candle_model`] and, when they hold an AdaLN
//!   `pred_φ`, become that class's [`AdaLnPredictor`].
//!
//! Predictors are kept per class, so a commit for class X never changes what class Y
//! predicts. A gate veto or [`DefaultWorldModel::rollback_model`] rebuilds from the
//! registry's restored active slot, which is the last-good checkpoint, or drops the
//! class back to the default predictor when no last-good checkpoint exists.
//!
//! # Which predictor answers
//!
//! [`DefaultWorldModel::predict_for`] resolves, first match wins:
//!
//! 1. the class's auto-loaded AdaLN (feature `candle`),
//! 2. the class's auto-loaded linear model,
//! 3. the host-set AdaLN from [`DefaultWorldModel::with_adaln`] (feature `candle`),
//! 4. the default [`DefaultWorldModel::predictor`] (untrained map).
//!
//! [`DefaultWorldModel::predict`] has no class argument. It uses the class of the most
//! recent `Encode` commit or rollback, and the default predictor before any.
//!
//! A checkpoint whose payload cannot be loaded (junk bytes, a safetensors blob that is
//! not an AdaLN predictor, or safetensors without feature `candle`) leaves that class on
//! the default predictor. The refresh methods report this with `false`.

use alloc::vec::Vec;

use weftos_worldmodel_core::{
    Action, HotSwapOutcome, Latent, ModelHotSwap, Predictor, SensorClass, SmallModelKind,
    WorldModelResult,
};
use weftos_worldmodel_impls::{
    codec_for_weights, LinearPredPhi, LEWM_MODEL_SEGMENT_CODEC_SAFETENSORS,
};
#[cfg(feature = "candle")]
use weftos_worldmodel_impls::{active_candle_model, AdaLnPredictor, CandleModel};

use crate::DefaultWorldModel;

/// Per-class predictors rebuilt from the registry's active `Encode` checkpoints.
#[derive(Debug, Clone, Default)]
pub(crate) struct ClassModels {
    linear: Vec<(SensorClass, LinearPredPhi)>,
    #[cfg(feature = "candle")]
    adaln: Vec<(SensorClass, AdaLnPredictor)>,
    last: Option<SensorClass>,
}

fn put<T>(slots: &mut Vec<(SensorClass, T)>, class: SensorClass, value: T) {
    match slots.iter_mut().find(|(c, _)| *c == class) {
        Some(slot) => slot.1 = value,
        None => slots.push((class, value)),
    }
}

fn get<T>(slots: &[(SensorClass, T)], class: SensorClass) -> Option<&T> {
    slots.iter().find(|(c, _)| *c == class).map(|(_, v)| v)
}

impl ClassModels {
    fn clear(&mut self, class: SensorClass) {
        self.linear.retain(|(c, _)| *c != class);
        #[cfg(feature = "candle")]
        self.adaln.retain(|(c, _)| *c != class);
    }
}

impl DefaultWorldModel {
    /// Rebuild `class`'s predictor from its active `Encode` checkpoint.
    ///
    /// Returns `true` when a learned predictor is now serving `class`, `false` when the
    /// class is on the default predictor (empty slot or unloadable payload).
    pub fn refresh_class_predictor(&mut self, class: SensorClass) -> bool {
        self.class_models.clear(class);
        let Some(cp) = self.sensor_models.active(class, SmallModelKind::Encode) else {
            return false;
        };
        if codec_for_weights(&cp.weights) == LEWM_MODEL_SEGMENT_CODEC_SAFETENSORS {
            #[cfg(feature = "candle")]
            if let Ok(Some(CandleModel::Predictor(p))) =
                active_candle_model(&self.sensor_models, class)
            {
                put(&mut self.class_models.adaln, class, p);
                return true;
            }
            return false;
        }
        match LinearPredPhi::from_checkpoint_weights(&cp.weights) {
            Ok(p) => {
                put(&mut self.class_models.linear, class, p);
                true
            }
            Err(_) => false,
        }
    }

    /// Rebuild every class's predictor from the registry (after direct registry edits).
    pub fn refresh_predictors(&mut self) {
        for &class in &SensorClass::ALL {
            self.refresh_class_predictor(class);
        }
    }

    /// Roll back one registry slot to its last-good checkpoint and, for `Encode`,
    /// rebuild the class's predictor from the restored weights (or drop to the default).
    pub fn rollback_model(
        &mut self,
        class: SensorClass,
        kind: SmallModelKind,
    ) -> WorldModelResult<HotSwapOutcome> {
        let outcome = ModelHotSwap::rollback(&mut self.sensor_models, class, kind)?;
        self.note_encode_outcome(class, kind, outcome);
        Ok(outcome)
    }

    /// Refresh after a registry outcome. `Committed` and `RolledBack` on an `Encode` slot
    /// rebuild the class's predictor and make it the class [`Self::predict`] answers for.
    pub(crate) fn note_encode_outcome(
        &mut self,
        class: SensorClass,
        kind: SmallModelKind,
        outcome: HotSwapOutcome,
    ) {
        if kind != SmallModelKind::Encode {
            return;
        }
        if matches!(
            outcome,
            HotSwapOutcome::Committed { .. } | HotSwapOutcome::RolledBack { .. }
        ) {
            self.refresh_class_predictor(class);
            self.class_models.last = Some(class);
        }
    }

    /// Whether `class` has an auto-loaded learned predictor (linear or AdaLN).
    pub fn has_learned_predictor(&self, class: SensorClass) -> bool {
        #[cfg(feature = "candle")]
        if get(&self.class_models.adaln, class).is_some() {
            return true;
        }
        get(&self.class_models.linear, class).is_some()
    }

    /// `ẑ_{t+1}` for `class`. See the module docs for the resolution order.
    pub fn predict_for(
        &self,
        class: SensorClass,
        z_t: &Latent,
        action: &Action,
    ) -> WorldModelResult<Latent> {
        #[cfg(feature = "candle")]
        if let Some(p) = get(&self.class_models.adaln, class) {
            return p.predict(z_t, action);
        }
        if let Some(p) = get(&self.class_models.linear, class) {
            return p.predict(z_t, action);
        }
        self.predict_default(z_t, action)
    }

    /// `ẑ_{t+1}` for the class of the last `Encode` commit or rollback, else the default.
    pub fn predict(&self, z_t: &Latent, action: &Action) -> WorldModelResult<Latent> {
        match self.class_models.last {
            Some(class) => self.predict_for(class, z_t, action),
            None => self.predict_default(z_t, action),
        }
    }

    /// Host-set AdaLN when present, otherwise [`Self::predictor`].
    fn predict_default(&self, z_t: &Latent, action: &Action) -> WorldModelResult<Latent> {
        #[cfg(feature = "candle")]
        if let Some(p) = &self.adaln {
            return p.predict(z_t, action);
        }
        self.predictor.predict(z_t, action)
    }
}
