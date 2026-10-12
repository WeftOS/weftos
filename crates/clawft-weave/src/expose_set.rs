//! The `expose.set` dashboard action (ADR-120 X3, node half): the dashboard
//! asks this node to switch one of its exposures off or pause it.
//!
//! It is `weaver expose off|pause` through the same registry code, with the
//! acting source recorded as `dashboard` in the exposure audit (chain). The
//! dashboard may only narrow what is offered: `on` is refused with a reason
//! (ADR-120 default, pending the owner). Local decisions always win: a master
//! switch that is off refuses the action, and an exposure the operator already
//! switched off stays off (a pause never relaxes it).
//!
//! The payload is untrusted: unknown fields are denied, and it is never echoed
//! back in an error.

use std::sync::Arc;

use async_trait::async_trait;
use clawft_kernel::exposure::{ExposureRegistry, ExposureState};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::dashboard_actions::{Action, ActionHandler, ActionOutcome};

/// The action kind.
pub const KIND: &str = "expose.set";
/// Acting source recorded in the exposure audit.
pub const ACTOR: &str = "dashboard";

/// The payload as the dashboard queues it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExposeSetPayload {
    pub id: String,
    pub state: String,
}

/// Answers `expose.set` actions against the daemon's exposure registry.
pub struct ExposeSetHandler {
    /// Tests inject one; otherwise the daemon's global registry is used.
    reg: Option<Arc<ExposureRegistry>>,
}

impl ExposeSetHandler {
    pub fn new(reg: Arc<ExposureRegistry>) -> Self {
        Self { reg: Some(reg) }
    }

    /// For `default_handlers()`: resolves the daemon's registry when an action
    /// arrives, and fails clearly when it is not running.
    pub fn from_process() -> Self {
        Self { reg: None }
    }
}

fn failed(error: &str, refused: bool) -> ActionOutcome {
    ActionOutcome { status: "failed", result: json!({ "error": error, "kind": KIND, "refused": refused }) }
}

#[async_trait]
impl ActionHandler for ExposeSetHandler {
    fn kind(&self) -> &str {
        KIND
    }

    async fn handle(&self, action: &Action) -> ActionOutcome {
        let Some(reg) = self.reg.clone().or_else(crate::expose_rt::global) else {
            return failed("the exposure registry is not running on this node", false);
        };
        let out = apply(&reg, action.payload.clone());
        if out.status == "succeeded" {
            crate::expose_rpc::flush_withdrawals().await;
        }
        out
    }
}

/// Validate, refuse what local policy forbids, apply off/pause.
pub fn apply(reg: &ExposureRegistry, payload: Value) -> ActionOutcome {
    let Ok(p) = serde_json::from_value::<ExposeSetPayload>(payload) else {
        return failed("payload is not a valid expose.set request ({id, state})", true);
    };
    let state = match p.state.as_str() {
        "off" => ExposureState::Off,
        "paused" => ExposureState::Paused,
        "on" => return failed("the dashboard may switch exposures off or pause them, not on; turn it on locally (weaver expose on)", true),
        _ => return failed("state must be off or paused", true),
    };
    let Some(current) = reg.get(&p.id) else {
        return failed("unknown exposure id", true);
    };
    if !reg.master_enabled() {
        return failed("refused by local policy: the master switch ([mesh.expose] enabled) is off on this node", true);
    }
    // Local always wins: an exposure the operator switched off stays off.
    if current.state == ExposureState::Off {
        return ActionOutcome {
            status: "succeeded",
            result: json!({ "id": p.id, "state": "off", "changed": false, "note": "already off locally; left as is" }),
        };
    }
    let routed = match crate::expose_rpc::route_infer(&p.id, state) {
        Ok(r) => r,
        Err(e) => return failed(&format!("refused by local policy: {e}"), true),
    };
    let done = if routed { reg.get(&p.id).ok_or_else(|| "exposure vanished".to_string()) } else { reg.set_state(&p.id, state, ACTOR) };
    match done {
        Ok(e) => ActionOutcome {
            status: "succeeded",
            result: json!({ "id": e.id, "state": e.state.as_str(), "changed": true, "announced": e.announced }),
        },
        Err(e) => failed(&e, true),
    }
}

#[cfg(test)]
#[path = "expose_set_tests.rs"]
mod tests;
