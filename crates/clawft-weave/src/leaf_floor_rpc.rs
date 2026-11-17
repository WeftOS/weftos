//! `leaf.reset_floor` (card e821c460): recover a reflashed leaf.
//!
//! A full flash wipes the device's sequence counter, so the parent refuses
//! its first frame (`expected 44, got 1`) against the durable replay floor
//! (ADR-103). `weaver leaf reset-floor <id>` removes that one floor so the
//! sequence may restart at 1; replay protection then runs again from the new
//! floor. The leaf's enrollment is untouched, so only the same certified key
//! can resume.
//!
//! | method | capability | params -> result |
//! |---|---|---|
//! | `leaf.reset_floor` | Admin | `{leaf_id}` -> `{leaf_id, old_seq, reset}` |
//!
//! Outside a project this is a user-level Admin act (`scope_gate`). Refusals
//! carry an `error_kind`: `leaf_bad_id`, `leaf_connected` (the leaf holds a
//! route to this daemon, so its counter is live), `leaf_no_ingress`,
//! `leaf_no_chain` (an unrecorded reset is refused). The reset is recorded on
//! the chain as `leaf.floor.reset` (who, leaf, old floor, time) before the
//! floor is removed; a removal that then fails is recorded as
//! `leaf.floor.reset_failed`.

use serde::Deserialize;
use serde_json::json;

use crate::protocol::Response;
use crate::rpc_ext::{ExtCall, ExtFuture};

/// Chain source of both events.
pub const SOURCE: &str = "leaf";
/// Chain kind recorded before the floor is removed.
pub const KIND_RESET: &str = "leaf.floor.reset";
/// Chain kind recorded when the removal then failed.
pub const KIND_FAILED: &str = "leaf.floor.reset_failed";

#[derive(Deserialize)]
struct Params {
    leaf_id: String,
}

pub fn handle(call: ExtCall) -> ExtFuture {
    Box::pin(async move {
        match call.method.as_str() {
            "leaf.reset_floor" => reset(&call).await,
            other => Response::error(format!("unknown leaf method {other}")),
        }
    })
}

async fn reset(call: &ExtCall) -> Response {
    let p: Params = match serde_json::from_value(call.params.clone()) {
        Ok(p) => p,
        Err(e) => return Response::error(format!("invalid params: {e}")),
    };
    if !clawft_kernel::mesh_leaf::is_leaf_id(&p.leaf_id) {
        return Response::error_with_kind("leaf_bad_id", "leaf_id must be 32 lowercase hex characters");
    }
    let id = p.leaf_id;
    let k = call.ctx.kernel.read().await;
    let Some(rt) = k.a2a_router().mesh_runtime().cloned() else {
        return Response::error_with_kind("leaf_no_ingress", "this daemon runs no leaf listener");
    };
    let Some(ingress) = rt.leaf_ingress().cloned() else {
        return Response::error_with_kind("leaf_no_ingress", "this daemon runs no leaf listener");
    };
    #[cfg(feature = "exochain")]
    let chain = k.chain_manager().cloned();
    drop(k);

    // The same lock serialises a leaf's check, delivery and floor commit, so
    // no frame is mid-commit while the floor is removed.
    let _guard = ingress.lock.lock().await;
    let topic = weftos_leaf_types::push_topic(&id);
    if rt.peer_ids().contains(&id) || !rt.peers_for_topic(&topic).is_empty() {
        return Response::error_with_kind(
            "leaf_connected",
            format!("leaf {id} is connected; disconnect it (power it off) before resetting its replay floor"),
        );
    }
    #[cfg(feature = "exochain")]
    let Some(chain) = chain else {
        return Response::error_with_kind("leaf_no_chain", "no chain is running; a reset must be recorded");
    };
    #[cfg(not(feature = "exochain"))]
    return Response::error_with_kind("leaf_no_chain", "built without a chain; a reset must be recorded");

    #[cfg(feature = "exochain")]
    {
        let old = ingress.floor_of(&id);
        let payload = json!({
            "leaf_id": id,
            "by": call.ctx.peer_key.clone().unwrap_or_else(|| "local".into()),
            "old_seq": old.map(|(s, _)| s),
            "old_digest": old.map(|(_, d)| hex_lower(&d)),
            "at": chrono::Utc::now().to_rfc3339(),
        });
        chain.append(SOURCE, KIND_RESET, Some(payload.clone()));
        if let Err(e) = ingress.reset_floor(&id) {
            let mut failed = payload;
            failed["error"] = json!(e.to_string());
            chain.append(SOURCE, KIND_FAILED, Some(failed));
            return Response::error_with_kind("leaf_reset_failed", format!("could not remove the floor: {e}"));
        }
        Response::success(json!({ "leaf_id": id, "old_seq": old.map(|(s, _)| s), "reset": true }))
    }
}

#[cfg(feature = "exochain")]
fn hex_lower(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[cfg(test)]
#[path = "leaf_floor_rpc_tests.rs"]
mod tests;
