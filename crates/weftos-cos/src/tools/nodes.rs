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
