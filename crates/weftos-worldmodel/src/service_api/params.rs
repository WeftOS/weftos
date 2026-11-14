//! Per-method JSON parameter decoding for the facade `call` dispatch.

use serde_json::{json, Value};
use weftos_worldmodel_core::{
    zero_latent, Action, Latent, WorldModelError, LATENT_DIM, LATENT_DIM_U16,
};

use super::errors::{LatticeServiceError, LatticeServiceResult};

pub(super) fn param_bytes(params: &Value, key: &str) -> LatticeServiceResult<Vec<u8>> {
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

pub(super) fn param_u64(params: &Value, key: &str) -> Option<u64> {
    params.get(key).and_then(|v| v.as_u64())
}

pub(super) fn param_latent(params: &Value, key: &str) -> LatticeServiceResult<Latent> {
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

pub(super) fn param_action(params: &Value) -> LatticeServiceResult<Action> {
    let mut action = Action::null();
    if let Some(arr) = params.get("action").and_then(|v| v.as_array()) {
        for (i, v) in arr.iter().take(32).enumerate() {
            action.code[i] = v.as_f64().unwrap_or(0.0) as f32;
        }
    }
    Ok(action)
}

pub(super) fn latent_json(z: &Latent) -> Value {
    json!({
        "latent": z.as_slice(),
        "latent_dim": LATENT_DIM,
    })
}
