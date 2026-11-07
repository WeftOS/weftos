//! `agent.sources` (agent directory, mesh view): what one node knows about its
//! agents, and the same for every peer.
//!
//! A node's document is `{node_id, fetched_at, agents, exposures}`: the rows
//! of `agent.list` and the document of `expose.list`. The agent directory (the
//! MCP `agent_list` / `agent_get` tools) joins them with the declared agents;
//! this method only fetches.
//!
//! | Params | Class | Result |
//! |---|---|---|
//! | none | Read | `{nodes: [this node]}` |
//! | `node` | Admin | `{nodes: [that peer]}` |
//! | `mesh: true` | Admin | `{nodes: [this node, every known peer]}` |
//!
//! A peer is asked on the signed `workload.ctl` wire as the read-only
//! node-admin method `agent.sources`, which the peer answers only when it
//! lists this node's key as a controller (the same wire as `dashboard.status`
//! and the mesh bot directory). A peer that does not answer is a block with
//! `ok: false` and its error; it never fails the whole listing, and nothing
//! is reported for it from memory.

use std::sync::OnceLock;

use clawft_rpc::Response;
use serde_json::{Value, json};

use crate::rpc_ext::{ExtCall, ExtFuture, KernelRef};

/// The node-admin method and the RPC name.
pub const METHOD: &str = "agent.sources";

static KERNEL: OnceLock<KernelRef> = OnceLock::new();

/// Remember the kernel so a peer's request can be answered (set when the
/// node's workload host is served). First call wins.
pub fn set_kernel(k: KernelRef) -> bool {
    KERNEL.set(k).is_ok()
}

fn parse_node(params: &Value) -> Result<Option<String>, String> {
    match params.get("node") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(n)) if !n.is_empty() && n.len() <= 128 => Ok(Some(n.clone())),
        Some(_) => Err("`node` must be a non-empty node id string".into()),
    }
}

fn parse_mesh(params: &Value) -> Result<bool, String> {
    match params.get("mesh") {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err("`mesh` must be true or false".into()),
    }
}

/// This node's agents and exposures. `node_id` is `None` outside the mesh.
pub async fn local_document(kernel: &KernelRef, node_id: Option<&str>) -> Value {
    let rows: Vec<Value> = {
        let k = kernel.read().await;
        let mut infos: Vec<crate::protocol::ProcessInfo> = k
            .supervisor()
            .list_agents()
            .iter()
            .map(|e| crate::protocol::ProcessInfo {
                row_id: crate::protocol::process_row_id_for_pid(e.pid),
                pid: e.pid,
                agent_id: e.agent_id.clone(),
                state: e.state.to_string(),
                memory_bytes: e.resource_usage.memory_bytes,
                cpu_time_ms: e.resource_usage.cpu_time_ms,
                parent_pid: e.parent_pid,
            })
            .collect();
        infos.sort_by_key(|e| e.pid);
        infos.iter().filter_map(|i| serde_json::to_value(i).ok()).collect()
    };
    let (exposures, exposures_error) = match crate::expose_rt::global() {
        Some(reg) => (crate::expose_rpc::list_json(&reg), Value::Null),
        None => (Value::Null, json!("the exposure registry is not running on this node")),
    };
    json!({
        "node_id": node_id,
        "fetched_at": chrono::Utc::now(),
        "agents": rows,
        "exposures": exposures,
        "exposures_error": exposures_error,
    })
}

/// A peer's answer for `agent.sources` (the host already checked the key).
pub async fn serve_peer(requester: &str) -> Result<Value, String> {
    let kernel = KERNEL.get().ok_or("the agent directory is not ready on this node")?;
    tracing::info!(%requester, "agent.sources request from a mesh controller");
    Ok(local_document(kernel, local_id().as_deref()).await)
}

fn block(node: &str, out: Result<Value, String>) -> Value {
    match out {
        Ok(mut doc) => {
            doc["ok"] = json!(true);
            doc["node_id"] = json!(node);
            doc
        }
        Err(e) => json!({ "node_id": node, "ok": false, "error": e, "fetched_at": chrono::Utc::now() }),
    }
}

#[cfg(all(feature = "placement", unix))]
fn local_id() -> Option<String> {
    crate::workload_place_rpc::local_mesh_node_id()
}
#[cfg(not(all(feature = "placement", unix)))]
fn local_id() -> Option<String> {
    None
}

#[cfg(all(feature = "placement", unix))]
async fn remote(kernel: &KernelRef, node: &str) -> Result<Value, String> {
    crate::workload_place_rpc::node_admin(kernel, node, METHOD, json!({})).await
}
#[cfg(not(all(feature = "placement", unix)))]
async fn remote(_k: &KernelRef, _n: &str) -> Result<Value, String> {
    Err("asking another node needs a unix build with mesh placement".into())
}

#[cfg(all(feature = "placement", unix))]
async fn peers(kernel: &KernelRef) -> Result<Vec<String>, String> {
    crate::workload_place_rpc::directory_peer_ids(kernel).await
}
#[cfg(not(all(feature = "placement", unix)))]
async fn peers(_k: &KernelRef) -> Result<Vec<String>, String> {
    Ok(Vec::new())
}

async fn local_block(kernel: &KernelRef, me: Option<&str>) -> Value {
    block(me.unwrap_or("local"), Ok(local_document(kernel, me).await))
}

async fn handle_inner(params: Value, kernel: &KernelRef, is_admin: bool) -> Result<Value, String> {
    let node = parse_node(&params)?;
    let mesh = parse_mesh(&params)?;
    if node.is_some() && mesh {
        return Err("give `node` or `mesh`, not both".into());
    }
    let me = local_id();
    let is_me = |n: &str| me.as_deref() == Some(n);
    if (mesh || node.as_deref().is_some_and(|n| !is_me(n))) && !is_admin {
        return Err("asking another node for its agents needs Admin".into());
    }
    let local = local_block(kernel, me.as_deref());
    if mesh {
        let mut nodes = vec![local.await];
        // A peer list that cannot be read is named, not hidden.
        match peers(kernel).await {
            Ok(ids) => {
                for id in ids.iter().filter(|n| !is_me(n)) {
                    nodes.push(block(id, remote(kernel, id).await));
                }
            }
            Err(e) => nodes.push(json!({ "node_id": null, "ok": false, "error": format!("peer list: {e}") })),
        }
        return Ok(json!({ "nodes": nodes }));
    }
    match node {
        Some(n) if !is_me(&n) => Ok(json!({ "nodes": [block(&n, remote(kernel, &n).await)] })),
        _ => Ok(json!({ "nodes": [local.await] })),
    }
}

/// Route handler for `agent.sources`.
pub fn handle(call: ExtCall) -> ExtFuture {
    Box::pin(async move {
        let admin = call.ctx.caps.allows(crate::capability::Capability::Admin);
        match handle_inner(call.params, &call.ctx.kernel, admin).await {
            Ok(v) => Response::success(v),
            Err(e) => Response::error(e),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_are_validated_not_defaulted() {
        assert_eq!(parse_node(&json!({})).unwrap(), None);
        assert_eq!(parse_node(&json!({"node": "n1"})).unwrap().as_deref(), Some("n1"));
        for bad in [json!({"node": ""}), json!({"node": 5}), json!({"node": "x".repeat(129)})] {
            assert!(parse_node(&bad).is_err(), "{bad}");
        }
        assert!(parse_mesh(&json!({"mesh": "yes"})).is_err());
        assert!(parse_mesh(&json!({"mesh": true})).unwrap());
    }

    #[test]
    fn an_unreachable_peer_is_a_block_with_its_error() {
        let b = block("n2", Err("timed out".into()));
        assert_eq!(b["ok"], false);
        assert_eq!(b["error"], "timed out");
        assert_eq!(b["node_id"], "n2");
        assert!(b.get("agents").is_none(), "nothing is reported for an unreachable node");
        let ok = block("n3", Ok(json!({"agents": []})));
        assert_eq!(ok["ok"], true);
    }
}
