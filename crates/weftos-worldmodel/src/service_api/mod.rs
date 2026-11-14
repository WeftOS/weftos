//! LatticeApi exposed through a ServiceApi-shaped facade (WEFT-527 / ADR-035).
//!
//! The kernel's [`clawft_kernel::ServiceApi`] trait lives behind a heavy
//! dependency graph. To keep LeWM decoupled from the kernel (ADR-090), this
//! module implements the **same call surface** (`call` / `list_services` /
//! `health`) over a [`LatticeApi`] implementor. Hosts (including the future
//! `clawft-worldmodel-service` binary, WEFT-524) can:
//!
//! 1. Use [`LatticeServiceFacade`] directly, or
//! 2. Bridge into the kernel by forwarding `ServiceApi::call` to
//!    [`LatticeServiceFacade::call`].
//!
//! Service name: [`LATTICE_SERVICE_NAME`] (`"worldmodel.lattice"`).
//! Methods: the seven [`LatticeMethod`] names.

use std::sync::Mutex;

use serde_json::{json, Value};
use weftos_worldmodel_core::{
    zero_latent, Action, Latent, LatticeApi, LatticeMethod, NodeId, ObservationFrame,
    SubscriptionId, WorldModelError, WorldModelResult, LATENT_DIM, LATENT_DIM_U16, LATTICE_METHODS,
};

/// Registered service name for LatticeApi hosts.
pub const LATTICE_SERVICE_NAME: &str = "worldmodel.lattice";

/// Service type label returned by [`LatticeServiceFacade::list_services`].
pub const LATTICE_SERVICE_TYPE: &str = "worldmodel";

/// Errors from the ServiceApi-shaped lattice facade.
#[derive(Debug, Clone, PartialEq)]
pub enum LatticeServiceError {
    /// Unknown service name.
    ServiceNotFound(String),
    /// Unknown method on the lattice service.
    MethodNotFound(String),
    /// Parameter decoding / schema error.
    BadParams(String),
    /// Underlying world-model error.
    WorldModel(WorldModelError),
}

impl std::fmt::Display for LatticeServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ServiceNotFound(s) => write!(f, "service not found: {s}"),
            Self::MethodNotFound(m) => write!(f, "method not found: {m}"),
            Self::BadParams(m) => write!(f, "bad params: {m}"),
            Self::WorldModel(e) => write!(f, "world model: {e}"),
        }
    }
}

impl std::error::Error for LatticeServiceError {}

impl From<WorldModelError> for LatticeServiceError {
    fn from(e: WorldModelError) -> Self {
        Self::WorldModel(e)
    }
}

/// Result alias for facade calls.
pub type LatticeServiceResult<T> = Result<T, LatticeServiceError>;

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

fn param_bytes(params: &Value, key: &str) -> LatticeServiceResult<Vec<u8>> {
    match params.get(key) {
        Some(Value::String(s)) => Ok(s.as_bytes().to_vec()),
        Some(Value::Array(arr)) => {
            let mut out = Vec::with_capacity(arr.len());
            for v in arr {
                let n = v.as_u64().ok_or_else(|| {
                    LatticeServiceError::BadParams(format!("non-integer byte in {key}"))
                })?;
                out.push(n as u8);
            }
            Ok(out)
        }
        Some(Value::Null) | None => Ok(Vec::new()),
        _ => Err(LatticeServiceError::BadParams(format!(
            "expected string or byte array for {key}"
        ))),
    }
}

fn param_u64(params: &Value, key: &str) -> Option<u64> {
    params.get(key).and_then(|v| v.as_u64())
}

fn param_latent(params: &Value, key: &str) -> LatticeServiceResult<Latent> {
    let arr = params
        .get(key)
        .and_then(|v| v.as_array())
        .ok_or_else(|| LatticeServiceError::BadParams(format!("missing latent array `{key}`")))?;
    if arr.len() != LATENT_DIM {
        return Err(WorldModelError::LatentDimMismatch {
            expected: LATENT_DIM_U16,
            got: arr.len() as u16,
        }
        .into());
    }
    let mut z = zero_latent();
    for (i, v) in arr.iter().enumerate() {
        z[i] = v.as_f64().unwrap_or(0.0) as f32;
    }
    Ok(z)
}

fn param_action(params: &Value) -> LatticeServiceResult<Action> {
    let mut action = Action::null();
    if let Some(arr) = params.get("action").and_then(|v| v.as_array()) {
        for (i, v) in arr.iter().take(32).enumerate() {
            action.code[i] = v.as_f64().unwrap_or(0.0) as f32;
        }
    }
    Ok(action)
}

fn latent_json(z: &Latent) -> Value {
    json!({
        "latent": z.as_slice(),
        "latent_dim": LATENT_DIM,
    })
}

/// Typed helper: invoke observe via the facade with a borrowed frame.
pub fn observe_json<L: LatticeApi>(
    facade: &LatticeServiceFacade<L>,
    bytes: &[u8],
    timestamp_ms: u64,
) -> LatticeServiceResult<Value> {
    facade.call(
        LATTICE_SERVICE_NAME,
        LatticeMethod::Observe.as_str(),
        json!({
            "bytes": String::from_utf8_lossy(bytes),
            "timestamp_ms": timestamp_ms,
            "latent_dim": LATENT_DIM_U16,
        }),
    )
}

/// Ensure a method string is one of the seven LatticeApi methods.
pub fn parse_lattice_method(name: &str) -> Option<LatticeMethod> {
    LATTICE_METHODS
        .iter()
        .copied()
        .find(|m| m.as_str() == name)
}

/// Direct typed dispatch onto a mutable lattice (single-threaded hosts).
pub fn dispatch_typed<L: LatticeApi>(
    lattice: &mut L,
    method: LatticeMethod,
    params: &Value,
) -> WorldModelResult<Value> {
    match method {
        LatticeMethod::Observe => {
            let bytes = match params.get("bytes") {
                Some(Value::String(s)) => s.as_bytes().to_vec(),
                _ => Vec::new(),
            };
            let ts = params
                .get("timestamp_ms")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let z = lattice.observe(ObservationFrame {
                bytes: &bytes,
                latent_dim: LATENT_DIM_U16,
                timestamp_ms: ts,
            })?;
            Ok(latent_json(&z))
        }
        LatticeMethod::ObserveNode => {
            let node = params.get("node").and_then(|v| v.as_u64()).unwrap_or(0);
            let bytes = match params.get("bytes") {
                Some(Value::String(s)) => s.as_bytes().to_vec(),
                _ => Vec::new(),
            };
            let ts = params
                .get("timestamp_ms")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let z = lattice.observe_node(
                NodeId(node),
                ObservationFrame {
                    bytes: &bytes,
                    latent_dim: LATENT_DIM_U16,
                    timestamp_ms: ts,
                },
            )?;
            Ok(json!({"node": node, "latent": z.as_slice(), "latent_dim": LATENT_DIM}))
        }
        LatticeMethod::Predict => {
            let z = json_to_latent(params, "z_t")?;
            let mut action = Action::null();
            if let Some(arr) = params.get("action").and_then(|v| v.as_array()) {
                for (i, v) in arr.iter().take(32).enumerate() {
                    action.code[i] = v.as_f64().unwrap_or(0.0) as f32;
                }
            }
            let z_hat = lattice.predict(&z, &action)?;
            Ok(latent_json(&z_hat))
        }
        LatticeMethod::Plan => {
            let z = json_to_latent(params, "z_t")?;
            let horizon = params
                .get("horizon")
                .and_then(|v| v.as_u64())
                .unwrap_or(4) as usize;
            let plan = lattice.plan(&z, horizon)?;
            Ok(json!({"horizon": plan.steps.len()}))
        }
        LatticeMethod::Recall => {
            let z = json_to_latent(params, "query")?;
            let k = params.get("k").and_then(|v| v.as_u64()).unwrap_or(5) as usize;
            let hits = lattice.recall(&z, k)?;
            Ok(json!({"count": hits.len()}))
        }
        LatticeMethod::SubscribeSurprise => {
            let h = params.get("handle").and_then(|v| v.as_u64()).unwrap_or(0);
            lattice.subscribe_surprise(SubscriptionId(h))?;
            Ok(json!({"ok": true}))
        }
        LatticeMethod::SubscribeDrift => {
            let h = params.get("handle").and_then(|v| v.as_u64()).unwrap_or(0);
            lattice.subscribe_drift(SubscriptionId(h))?;
            Ok(json!({"ok": true}))
        }
        _ => Err(WorldModelError::Other("unknown lattice method")),
    }
}

fn json_to_latent(params: &Value, key: &str) -> WorldModelResult<Latent> {
    let arr = params
        .get(key)
        .and_then(|v| v.as_array())
        .ok_or(WorldModelError::InvalidObservation)?;
    if arr.len() != LATENT_DIM {
        return Err(WorldModelError::dim_mismatch(arr.len() as u16));
    }
    let mut z = zero_latent();
    for (i, v) in arr.iter().enumerate() {
        z[i] = v.as_f64().unwrap_or(0.0) as f32;
    }
    Ok(z)
}

#[cfg(test)]
mod tests {
    use super::*;
    use weftos_worldmodel_impls::StubLattice;

    fn facade() -> LatticeServiceFacade<StubLattice> {
        default_lattice_service()
    }

    fn zero_json() -> Value {
        json!(vec![0.0f32; LATENT_DIM])
    }

    #[test]
    fn seven_methods_registered() {
        assert_eq!(LATTICE_METHODS.len(), 7);
        assert_eq!(LatticeServiceFacade::<StubLattice>::method_names().len(), 7);
        for m in LATTICE_METHODS {
            assert!(parse_lattice_method(m.as_str()).is_some());
        }
    }

    #[test]
    fn list_and_health() {
        let f = facade();
        let services = f.list_services();
        assert_eq!(services.len(), 1);
        assert_eq!(services[0].name, LATTICE_SERVICE_NAME);
        assert!(services[0].healthy);
        assert_eq!(
            f.health(LATTICE_SERVICE_NAME).unwrap(),
            FacadeHealth::Healthy
        );
        assert!(matches!(
            f.health("nope"),
            Err(LatticeServiceError::ServiceNotFound(_))
        ));
    }

    #[test]
    fn observe_happy_and_dim_error() {
        let f = facade();
        let ok = f
            .call(
                LATTICE_SERVICE_NAME,
                "observe",
                json!({"bytes": "frame", "timestamp_ms": 1}),
            )
            .unwrap();
        assert_eq!(ok["latent_dim"], LATENT_DIM);

        let err = f
            .call(
                LATTICE_SERVICE_NAME,
                "observe",
                json!({"bytes": "x", "latent_dim": 64}),
            )
            .unwrap_err();
        assert!(matches!(
            err,
            LatticeServiceError::WorldModel(WorldModelError::LatentDimMismatch { got: 64, .. })
        ));
    }

    #[test]
    fn observe_node_happy_and_service_error() {
        let f = facade();
        let ok = f
            .call(
                LATTICE_SERVICE_NAME,
                "observe_node",
                json!({"node": 9, "bytes": "n", "timestamp_ms": 2}),
            )
            .unwrap();
        assert_eq!(ok["node"], 9);

        assert!(matches!(
            f.call("other", "observe_node", json!({})),
            Err(LatticeServiceError::ServiceNotFound(_))
        ));
    }

    #[test]
    fn predict_happy_and_bad_latent() {
        let f = facade();
        let ok = f
            .call(
                LATTICE_SERVICE_NAME,
                "predict",
                json!({"z_t": zero_json(), "action": vec![0.0f32; 32]}),
            )
            .unwrap();
        assert_eq!(ok["latent_dim"], LATENT_DIM);

        let err = f
            .call(
                LATTICE_SERVICE_NAME,
                "predict",
                json!({"z_t": vec![0.0f32; 8]}),
            )
            .unwrap_err();
        assert!(matches!(
            err,
            LatticeServiceError::WorldModel(WorldModelError::LatentDimMismatch { .. })
        ));
    }

    #[test]
    fn plan_happy_and_missing_latent() {
        let f = facade();
        let ok = f
            .call(
                LATTICE_SERVICE_NAME,
                "plan",
                json!({"z_t": zero_json(), "horizon": 5}),
            )
            .unwrap();
        assert_eq!(ok["horizon"], 5);

        let err = f
            .call(LATTICE_SERVICE_NAME, "plan", json!({"horizon": 2}))
            .unwrap_err();
        assert!(matches!(err, LatticeServiceError::BadParams(_)));
    }

    #[test]
    fn recall_happy_and_method_not_found() {
        let f = facade();
        let ok = f
            .call(
                LATTICE_SERVICE_NAME,
                "recall",
                json!({"query": zero_json(), "k": 3}),
            )
            .unwrap();
        assert!(ok["hits"].as_array().unwrap().is_empty());

        assert!(matches!(
            f.call(LATTICE_SERVICE_NAME, "nope", json!({})),
            Err(LatticeServiceError::MethodNotFound(_))
        ));
    }

    #[test]
    fn subscribe_surprise_and_drift() {
        let f = facade();
        let s = f
            .call(
                LATTICE_SERVICE_NAME,
                "subscribe_surprise",
                json!({"handle": 11}),
            )
            .unwrap();
        assert_eq!(s["handle"], 11);
        let d = f
            .call(
                LATTICE_SERVICE_NAME,
                "subscribe_drift",
                json!({"handle": 12}),
            )
            .unwrap();
        assert_eq!(d["subscribed"], "drift");

        f.with_lattice(|lat| {
            assert!(lat.surprise_subs.contains(&SubscriptionId(11)));
            assert!(lat.drift_subs.contains(&SubscriptionId(12)));
        });
    }

    #[test]
    fn offline_returns_unavailable() {
        let mut f = facade();
        f.set_online(false);
        assert_eq!(f.health(LATTICE_SERVICE_NAME).unwrap(), FacadeHealth::Unhealthy);
        assert!(matches!(
            f.call(LATTICE_SERVICE_NAME, "observe", json!({})),
            Err(LatticeServiceError::WorldModel(WorldModelError::Unavailable))
        ));
    }

    #[test]
    fn observe_json_helper() {
        let f = facade();
        let v = observe_json(&f, b"hello", 42).unwrap();
        assert_eq!(v["latent"].as_array().unwrap().len(), LATENT_DIM);
    }

    #[test]
    fn dispatch_typed_all_methods() {
        let mut lat = StubLattice::default();
        for m in LATTICE_METHODS {
            let params = match m {
                LatticeMethod::Observe | LatticeMethod::ObserveNode => {
                    json!({"bytes": "x", "node": 1, "timestamp_ms": 0})
                }
                LatticeMethod::Predict => json!({"z_t": zero_json(), "action": vec![0.0f32; 32]}),
                LatticeMethod::Plan => json!({"z_t": zero_json(), "horizon": 2}),
                LatticeMethod::Recall => json!({"query": zero_json(), "k": 1}),
                LatticeMethod::SubscribeSurprise | LatticeMethod::SubscribeDrift => {
                    json!({"handle": 0})
                }
                _ => json!({}),
            };
            dispatch_typed(&mut lat, *m, &params).unwrap();
        }
    }
}
