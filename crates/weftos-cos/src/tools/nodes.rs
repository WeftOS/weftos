//! `nodes.health`: heartbeat and route state, read-only. The source is a
//! trait so tests never touch a daemon; the runtime source asks the local
//! `weaver` daemon for `cluster.status` and `cluster.nodes` over its RPC
//! socket and reports "unreachable" instead of guessing.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Gate, Tool, ToolOutcome, ToolSpec};
use crate::CosError;

#[async_trait]
pub trait NodeHealthSource: Send + Sync {
    /// A JSON snapshot: `{reachable, nodes: [...], summary: {...}}`.
    async fn snapshot(&self) -> Result<Value, CosError>;
}

/// Reads the local daemon over RPC. Read-only methods only.
pub struct DaemonHealthSource;

#[async_trait]
impl NodeHealthSource for DaemonHealthSource {
    async fn snapshot(&self) -> Result<Value, CosError> {
        let Some(mut client) = clawft_rpc::DaemonClient::connect().await else {
            return Ok(json!({"reachable": false, "reason": "weaver daemon not reachable on this machine"}));
        };
        let status = client.simple_call("cluster.status").await.map_err(|e| CosError::Tool {
            name: "nodes.health".into(),
            reason: e.to_string(),
        })?;
        let nodes = client.simple_call("cluster.nodes").await.ok();
        Ok(json!({
            "reachable": true,
            "summary": status.result.unwrap_or(Value::Null),
            "nodes": nodes.and_then(|r| r.result).unwrap_or(Value::Array(vec![])),
        }))
    }
}

pub struct NodesHealth {
    pub source: Arc<dyn NodeHealthSource>,
}

#[async_trait]
impl Tool for NodesHealth {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            id: "nodes.health",
            description: "Heartbeat and route state of the mesh nodes. Read-only.",
            parameters: json!({"type": "object", "properties": {}}),
            gate: Gate::Open,
        }
    }

    async fn call(&self, _args: Value) -> Result<ToolOutcome, CosError> {
        let snap = self.source.snapshot().await?;
        let summary = if snap["reachable"] == true {
            let n = snap["nodes"].as_array().map_or(0, Vec::len);
            let healthy = snap["summary"]["healthy_nodes"].as_u64().unwrap_or(0);
            let total = snap["summary"]["total_nodes"].as_u64().unwrap_or(n as u64);
            format!("{healthy}/{total} nodes healthy")
        } else {
            "daemon unreachable; no node state".into()
        };
        Ok(ToolOutcome { summary, result: snap })
    }
}

/// A fixed snapshot for tests and the typed dev mode.
pub struct StaticHealth(pub Value);

#[async_trait]
impl NodeHealthSource for StaticHealth {
    async fn snapshot(&self) -> Result<Value, CosError> {
        Ok(self.0.clone())
    }
}

/// Read-only access to the local daemon's RPC. `Ok(None)` means the daemon
/// is not reachable; an `Err` is a refused or failed call.
#[async_trait]
pub trait MeshRpc: Send + Sync {
    async fn call(&self, method: &str) -> Result<Option<Value>, CosError>;
}

/// The runtime source: the weaver daemon's socket.
pub struct DaemonRpc;

#[async_trait]
impl MeshRpc for DaemonRpc {
    async fn call(&self, method: &str) -> Result<Option<Value>, CosError> {
        if !clawft_rpc::handshake::is_read_only_method(method) {
            return Err(CosError::Tool { name: "mesh.routes".into(), reason: format!("{method} is not a read-only method") });
        }
        let Some(mut client) = clawft_rpc::DaemonClient::connect().await else { return Ok(None) };
        let resp = client.simple_call(method).await.map_err(|e| CosError::Tool { name: "mesh.routes".into(), reason: e.to_string() })?;
        if let Some(err) = resp.error {
            return Err(CosError::Tool { name: "mesh.routes".into(), reason: format!("{method}: {err}") });
        }
        Ok(Some(resp.result.unwrap_or(Value::Null)))
    }
}

/// `mesh.routes`: how messages and nodes are wired right now: the daemon's
/// IPC topics (the routes) and per-node facts. Read-only methods only.
pub struct MeshRoutes {
    pub rpc: Arc<dyn MeshRpc>,
}

#[async_trait]
impl Tool for MeshRoutes {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            id: "mesh.routes",
            description: "Message routes (IPC topics) and per-node facts from the local weaver daemon. Read-only.",
            parameters: json!({"type": "object", "properties": {}}),
            gate: Gate::Open,
        }
    }

    async fn call(&self, _args: Value) -> Result<ToolOutcome, CosError> {
        let Some(topics) = self.rpc.call("ipc.topics").await? else {
            return Ok(ToolOutcome {
                summary: "daemon unreachable; no route state".into(),
                result: json!({"reachable": false, "reason": "weaver daemon not reachable on this machine"}),
            });
        };
        // Facts are optional: an older daemon may not serve them.
        let facts = self.rpc.call("cluster.facts").await.ok().flatten().unwrap_or(Value::Null);
        let n = topics.as_array().map_or_else(|| topics.get("topics").and_then(Value::as_array).map_or(0, Vec::len), Vec::len);
        Ok(ToolOutcome {
            summary: format!("{n} route(s) on the daemon"),
            result: json!({"reachable": true, "topics": topics, "facts": facts}),
        })
    }
}
