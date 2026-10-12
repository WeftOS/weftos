//! ADR-120 on the placement table: `infer.<role>` roles in the exposure
//! registry, the serve check, and peer withdrawals.

use std::sync::Arc;

use super::{PlacementTable, SERVICE_PREFIX};
use crate::exposure::{Audience, ExposureKind, ExposureRegistry, ExposureSpec, ExposureState, Refusal};

impl PlacementTable {
    /// Use `reg` (the daemon's shared, persisted registry) for exposures.
    pub fn with_exposures(mut self, reg: Arc<ExposureRegistry>) -> Self {
        self.exposures = reg;
        self
    }

    /// The exposure registry (ADR-120).
    pub fn exposures(&self) -> &Arc<ExposureRegistry> {
        &self.exposures
    }

    /// Exposure id of `role` (also its advertisement name).
    pub fn exposure_id(role: &str) -> String {
        format!("{SERVICE_PREFIX}{role}")
    }

    /// Why `role` may not be served to peers now, if the registry says so.
    pub fn serve_refusal(&self, role: &str) -> Option<Refusal> {
        self.exposures.check_serve(&Self::exposure_id(role)).err()
    }

    /// Mirror `role`'s exposure state into the registry: registered whenever
    /// it is exposed to the mesh, and announced exactly when a local
    /// instance exists to advertise (the one condition both the advertisement
    /// and the serving path test).
    pub(super) fn sync_exposure(&self, role: &str) {
        let (exposed, local) = {
            let g = self.inner.read().unwrap();
            (g.exposed.contains(role), g.local.get(role).cloned())
        };
        let id = Self::exposure_id(role);
        if !exposed && self.exposures.get(&id).is_none() {
            return;
        }
        let (resource, api) = local
            .as_ref()
            .map_or(("runtime".to_string(), "openai-chat".to_string()), |l| (format!("runtime:{}", l.runtime), l.api.clone()));
        let mut spec = ExposureSpec::new(&id, ExposureKind::Inference, &resource, &api);
        spec.audience = Audience::Paired;
        spec.state = if exposed { ExposureState::On } else { ExposureState::Off };
        self.exposures.register(spec);
        self.exposures.set_announced(&id, exposed && local.is_some());
    }

    /// A peer withdrew its advertisement for `name` (`infer.<role>`): forget
    /// it now rather than waiting out the TTL. Only the sender's own entry.
    pub fn withdraw_remote(&self, sender: &str, name: &str) -> bool {
        let Some(role) = name.strip_prefix(SERVICE_PREFIX) else { return false };
        let changed = {
            let mut g = self.inner.write().unwrap();
            let had = g.remote.get_mut(role).is_some_and(|per| per.remove(sender).is_some());
            g.remote.retain(|_, per| !per.is_empty());
            had
        };
        if changed {
            self.bump();
        }
        changed
    }
}
