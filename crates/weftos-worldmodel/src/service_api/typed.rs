//! Typed helpers: `observe_json`, method parsing and direct typed dispatch.

use serde_json::{json, Value};
use weftos_worldmodel_core::{
    zero_latent, Action, Latent, LatticeApi, LatticeMethod, NodeId, ObservationFrame,
    SubscriptionId, WorldModelError, WorldModelResult, LATENT_DIM, LATENT_DIM_U16, LATTICE_METHODS,
};

use super::errors::LatticeServiceResult;
use super::facade::LatticeServiceFacade;
use super::params::latent_json;
use super::LATTICE_SERVICE_NAME;

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
