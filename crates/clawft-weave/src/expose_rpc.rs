//! Operator verbs for exposures (ADR-120):
//!
//! - `expose.list` (Read): every exposure with audience, state, announced and
//!   why it is refused, if it is; plus the master switch.
//! - `expose.set {id, state: on|off|paused, audience?, raw?}` (Admin): a local
//!   decision, persisted. Switching off withdraws the advertisement at once.
//!   `raw` (topic exposures only, ADR-122 section 14) allows raw data, and
//!   only for an audience of named nodes; `on` from the CLI always says it.
//! - `expose.off_all` (Admin): every exposure off.
//! - `expose.log {id?, limit?}` (Admin): recent activity, newest last, and
//!   whether the chain verifies. Never request bodies.
//!
//! Turning an `infer.<role>` exposure on goes through the same operator act as
//! `infer.expose` (the role must be configured and the mesh available).

use clawft_kernel::exposure::registry::now_ms;
use clawft_kernel::exposure::{Audience, Exposure, ExposureRegistry, ExposureState};
use clawft_rpc::Response;
use serde::Deserialize;
use serde_json::{Value, json};

const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 1000;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SetParams {
    id: String,
    state: String,
    #[serde(default)]
    audience: Option<String>,
    #[serde(default)]
    raw: Option<bool>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct LogParams {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

fn parse_state(s: &str) -> Result<ExposureState, String> {
    match s {
        "on" => Ok(ExposureState::On),
        "off" => Ok(ExposureState::Off),
        "paused" | "pause" => Ok(ExposureState::Paused),
        other => Err(format!("unknown state {other:?} (on, off, paused)")),
    }
}

pub(crate) fn row(reg: &ExposureRegistry, e: &Exposure) -> Value {
    let refused = reg.check_serve(&e.id).err().map(|r| r.reason());
    json!({
        "id": e.id, "kind": e.kind, "resource": e.resource, "audience": e.audience.label(),
        "api": e.api, "limits": e.limits, "state": e.state.as_str(), "announced": e.announced,
        "served": refused.is_none(), "refused": refused, "raw": e.raw,
    })
}

/// The `expose.list` document.
pub fn list_json(reg: &ExposureRegistry) -> Value {
    let items: Vec<Value> = reg.list().iter().map(|e| row(reg, e)).collect();
    let counts = reg.activity().counts(now_ms(), clawft_kernel::exposure::registry::DAY_MS);
    let items: Vec<Value> = items
        .into_iter()
        .map(|mut v| {
            let c = v["id"].as_str().and_then(|i| counts.get(i)).cloned().unwrap_or_default();
            v["last_used"] = json!(c.last_ts_ms);
            v["n24"] = json!(c.total);
            v["err24"] = json!(c.failed);
            v
        })
        .collect();
    json!({"enabled": reg.master_enabled(), "node_id": reg.node_id(), "exposures": items})
}

#[cfg(all(feature = "placement", unix))]
pub(crate) async fn flush_withdrawals() {
    if let Some(hub) = crate::infer_wire::state().and_then(|s| s.hub.clone()) {
        hub.flush_withdrawals().await;
    }
}
#[cfg(not(all(feature = "placement", unix)))]
pub(crate) async fn flush_withdrawals() {}

/// `infer.<role>` goes through the inference operator act.
#[cfg(all(feature = "placement", unix))]
pub(crate) fn route_infer(id: &str, state: ExposureState) -> Result<bool, String> {
    let Some(role) = id.strip_prefix("infer.") else { return Ok(false) };
    let Some(st) = crate::infer_wire::state() else {
        return Err("inference placement is off (no inference.json in the runtime directory)".into());
    };
    match state {
        ExposureState::Paused => Ok(false),
        s => crate::infer_rpc::set_role_exposed(st, role, s == ExposureState::On).map(|()| true),
    }
}
#[cfg(not(all(feature = "placement", unix)))]
pub(crate) fn route_infer(_: &str, _: ExposureState) -> Result<bool, String> {
    Ok(false)
}

fn set(reg: &ExposureRegistry, p: SetParams) -> Result<Value, String> {
    let state = parse_state(&p.state)?;
    let audience = p.audience.as_deref().map(Audience::parse).transpose()?;
    let Some(cur) = reg.get(&p.id) else {
        return Err(format!("unknown exposure {:?}", p.id));
    };
    if p.raw == Some(true) {
        if cur.kind != clawft_kernel::exposure::ExposureKind::Topic {
            return Err("--raw applies to topic exposures only".into());
        }
        if !matches!(audience.as_ref().unwrap_or(&cur.audience), Audience::Nodes(_)) {
            return Err("raw data is exposed only to named nodes: use --audience nodes:<a,b> with --raw".into());
        }
    }
    if let Some(a) = audience {
        reg.set_audience(&p.id, a, "cli")?;
    }
    if let Some(raw) = p.raw.filter(|r| *r != cur.raw) {
        reg.set_raw(&p.id, raw, "cli")?;
    }
    if !route_infer(&p.id, state)? {
        reg.set_state(&p.id, state, "cli")?;
    }
    // Remote subscribers of a topic that is no longer served end now.
    #[cfg(all(feature = "placement", unix))]
    crate::cogbus_mesh::withdraw_if_narrowed(&p.id);
    let e = reg.get(&p.id).ok_or("exposure vanished")?;
    Ok(row(reg, &e))
}

/// Dispatch one `expose.*` verb.
pub async fn handle(method: &str, params: Value) -> Response {
    let Some(reg) = crate::expose_rt::global() else {
        return Response::error("the exposure registry is not running on this daemon".to_string());
    };
    handle_with(&reg, method, params).await
}

/// [`handle`] against a given registry.
pub async fn handle_with(reg: &ExposureRegistry, method: &str, params: Value) -> Response {
    match method {
        "expose.list" => Response::success(list_json(reg)),
        "expose.set" => {
            let p: SetParams = match serde_json::from_value(params) {
                Ok(p) => p,
                Err(e) => return Response::error(format!("invalid params: {e}")),
            };
            match set(reg, p) {
                Ok(v) => {
                    flush_withdrawals().await;
                    Response::success(v)
                }
                Err(e) => Response::error(e),
            }
        }
        "expose.off_all" => {
            let ids = reg.off_all("cli");
            for id in &ids {
                // Keep the inference table in step so a later `on` is an explicit act.
                let _ = route_infer(id, ExposureState::Off);
            }
            flush_withdrawals().await;
            Response::success(json!({"switched_off": ids, "enabled": reg.master_enabled()}))
        }
        "expose.log" => {
            let p: LogParams = if params.is_null() {
                LogParams::default()
            } else {
                match serde_json::from_value(params) {
                    Ok(p) => p,
                    Err(e) => return Response::error(format!("invalid params: {e}")),
                }
            };
            let limit = p.limit.unwrap_or(DEFAULT_LIMIT);
            if limit == 0 || limit > MAX_LIMIT {
                return Response::error(format!("limit must be 1..={MAX_LIMIT}"));
            }
            if let Some(id) = &p.id
                && reg.get(id).is_none()
            {
                return Response::error(format!("unknown exposure {id:?}"));
            }
            let chain = reg.activity().verify().err();
            Response::success(json!({
                "records": reg.activity().query(p.id.as_deref(), limit),
                "chain": chain.clone().unwrap_or_else(|| "ok".into()),
                "chain_ok": chain.is_none(),
            }))
        }
        _ => Response::error(format!("unknown method {method}")),
    }
}

#[cfg(test)]
#[path = "expose_rpc_tests.rs"]
mod tests;
