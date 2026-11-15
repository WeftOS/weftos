//! [`LatticeServiceFacade`]: the `call` / `list_services` / `health` surface.

use std::sync::Mutex;

use serde_json::{json, Value};
use weftos_worldmodel_core::{
    LatticeApi, NodeId, ObservationFrame, SubscriptionId, WorldModelError, LATENT_DIM,
    LATTICE_METHODS,
};

use super::errors::{LatticeServiceError, LatticeServiceResult};
use super::params::{latent_json, param_action, param_bytes, param_latent, param_u64};
use super::{LATTICE_SERVICE_NAME, LATTICE_SERVICE_TYPE};

/// Health status mirror (avoids depending on clawft-kernel).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FacadeHealth {
    /// Service is healthy.
    Healthy,
    /// Service is degraded but answering.
    Degraded,
    /// Service is down.
    Unhealthy,
}

impl FacadeHealth {
    /// Wire / JSON string.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Degraded => "degraded",
            Self::Unhealthy => "unhealthy",
        }
    }
}

/// Service info mirror for [`LatticeServiceFacade::list_services`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FacadeServiceInfo {
    /// Service name.
    pub name: String,
    /// Service type label.
    pub service_type: String,
    /// Whether the service is currently healthy.
    pub healthy: bool,
}

/// ServiceApi-shaped adapter over any [`LatticeApi`].
///
/// Thread-safe via interior `Mutex` so protocol adapters can share it.
pub struct LatticeServiceFacade<L: LatticeApi> {
    lattice: Mutex<L>,
    /// When false, `health` reports unhealthy (tests / drain).
    online: bool,
}

impl<L: LatticeApi> LatticeServiceFacade<L> {
    /// Wrap a lattice implementation.
    pub fn new(lattice: L) -> Self {
        Self {
            lattice: Mutex::new(lattice),
            online: true,
        }
    }

    /// Mark the facade offline (health → unhealthy).
    pub fn set_online(&mut self, online: bool) {
        self.online = online;
    }

    /// Access the inner lattice (tests / advanced hosts).
    pub fn with_lattice<R>(&self, f: impl FnOnce(&mut L) -> R) -> R {
        let mut guard = self.lattice.lock().expect("lattice mutex poisoned");
        f(&mut guard)
    }

    /// `ServiceApi::list_services` equivalent.
    pub fn list_services(&self) -> Vec<FacadeServiceInfo> {
        vec![FacadeServiceInfo {
            name: LATTICE_SERVICE_NAME.into(),
            service_type: LATTICE_SERVICE_TYPE.into(),
            healthy: self.online,
        }]
    }

    /// `ServiceApi::health` equivalent.
    pub fn health(&self, service: &str) -> LatticeServiceResult<FacadeHealth> {
        if service != LATTICE_SERVICE_NAME {
            return Err(LatticeServiceError::ServiceNotFound(service.into()));
        }
        Ok(if self.online {
            FacadeHealth::Healthy
        } else {
            FacadeHealth::Unhealthy
        })
    }

    /// `ServiceApi::call` equivalent — dispatches the seven LatticeApi methods.
    pub fn call(
        &self,
        service: &str,
        method: &str,
        params: Value,
    ) -> LatticeServiceResult<Value> {
        if service != LATTICE_SERVICE_NAME {
            return Err(LatticeServiceError::ServiceNotFound(service.into()));
        }
        if !self.online {
            return Err(LatticeServiceError::WorldModel(WorldModelError::Unavailable));
        }

        let mut lattice = self.lattice.lock().expect("lattice mutex poisoned");
        match method {
            "observe" => {
                let bytes = param_bytes(&params, "bytes")?;
                let ts = param_u64(&params, "timestamp_ms").unwrap_or(0);
                let dim = param_u64(&params, "latent_dim").unwrap_or(LATENT_DIM as u64) as u16;
                let frame = ObservationFrame {
                    bytes: &bytes,
                    latent_dim: dim,
                    timestamp_ms: ts,
                };
                let z = lattice.observe(frame)?;
                Ok(latent_json(&z))
            }
            "observe_node" => {
                let node = param_u64(&params, "node").unwrap_or(0);
                let bytes = param_bytes(&params, "bytes")?;
                let ts = param_u64(&params, "timestamp_ms").unwrap_or(0);
                let dim = param_u64(&params, "latent_dim").unwrap_or(LATENT_DIM as u64) as u16;
                let frame = ObservationFrame {
                    bytes: &bytes,
                    latent_dim: dim,
                    timestamp_ms: ts,
                };
                let z = lattice.observe_node(NodeId(node), frame)?;
                Ok(json!({
                    "node": node,
                    "latent": z.as_slice(),
                    "latent_dim": LATENT_DIM,
                }))
            }
            "predict" => {
                let z = param_latent(&params, "z_t")?;
                let action = param_action(&params)?;
                let z_hat = lattice.predict(&z, &action)?;
                Ok(latent_json(&z_hat))
            }
            "plan" => {
                let z = param_latent(&params, "z_t")?;
                let horizon = param_u64(&params, "horizon").unwrap_or(4) as usize;
                let plan = lattice.plan(&z, horizon)?;
                Ok(json!({
                    "horizon": plan.steps.len(),
                    "kind": format!("{:?}", plan.kind),
                    "steps": plan.steps.iter().map(|s| json!({
                        "action": s.action.code.as_slice(),
                        "z_hat_dim": s.z_hat.len(),
                    })).collect::<Vec<_>>(),
                }))
            }
            "recall" => {
                let z = param_latent(&params, "query")?;
                let k = param_u64(&params, "k").unwrap_or(5) as usize;
                let hits = lattice.recall(&z, k)?;
                Ok(json!({
                    "hits": hits.iter().map(|h| json!({
                        "node": h.node.0,
                        "score": h.score,
                        "latent_dim": h.latent_dim,
                    })).collect::<Vec<_>>(),
                }))
            }
            "subscribe_surprise" => {
                let handle = param_u64(&params, "handle").unwrap_or(0);
                lattice.subscribe_surprise(SubscriptionId(handle))?;
                Ok(json!({"subscribed": "surprise", "handle": handle}))
            }
            "subscribe_drift" => {
                let handle = param_u64(&params, "handle").unwrap_or(0);
                lattice.subscribe_drift(SubscriptionId(handle))?;
                Ok(json!({"subscribed": "drift", "handle": handle}))
            }
            other => Err(LatticeServiceError::MethodNotFound(other.into())),
        }
    }

    /// List the seven method names (stable order).
    pub fn method_names() -> Vec<&'static str> {
        LATTICE_METHODS.iter().map(|m| m.as_str()).collect()
    }
}

impl Default for LatticeServiceFacade<weftos_worldmodel_impls::StubLattice> {
    fn default() -> Self {
        Self::new(weftos_worldmodel_impls::StubLattice::default())
    }
}

/// Build the default stub-backed facade (service scaffolding / tests).
pub fn default_lattice_service() -> LatticeServiceFacade<weftos_worldmodel_impls::StubLattice> {
    LatticeServiceFacade::default()
}
