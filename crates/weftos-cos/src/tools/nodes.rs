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

/// Why the daemon could not be dialed: `Unreachable` means no daemon answered
/// at the endpoint the resolver picked (the owner may well be fine);
/// `Refused` means one answered but the connection was not accepted.
pub(crate) enum Dial {
    Unreachable(String),
    Refused(String),
}

/// Dial the daemon the way the `weaver` CLI does: through the endpoint
/// resolver (`WEFTOS_RUNTIME_DIR`, the project manifest, then the user
/// daemon's `~/.weftos/run`), not the legacy `~/.clawft` socket alone.
pub(crate) async fn dial_resolved(res: &clawft_rpc::resolve::Resolution) -> Result<clawft_rpc::DaemonClient, Dial> {
    match clawft_rpc::DaemonClient::connect_resolved(res).await {
        Ok(c) => Ok(c.client),
        Err(clawft_rpc::ConnectError::Unreachable { resolution, .. }) => {
            Err(Dial::Unreachable(format!("no daemon is answering at {}", resolution.socket.display())))
        }
        Err(e) => Err(Dial::Refused(e.to_string())),
    }
}

pub(crate) async fn dial() -> Result<clawft_rpc::DaemonClient, Dial> {
    match clawft_rpc::resolve::resolve(&clawft_rpc::resolve::ResolveFlags::default()) {
        Ok(res) => dial_resolved(&res).await,
        Err(e) => Err(Dial::Refused(e.to_string())),
    }
}

/// The honest wording: not being able to reach the daemon says nothing
/// about the nodes.
fn not_checked(why: &str) -> Value {
    json!({
        "reachable": false,
        "health_known": false,
        "reason": format!("I can't reach the weaver daemon to check ({why}); that says nothing about whether the nodes are healthy"),
    })
}

/// Reads the local daemon over RPC. Read-only methods only.
pub struct DaemonHealthSource;

#[async_trait]
impl NodeHealthSource for DaemonHealthSource {
    async fn snapshot(&self) -> Result<Value, CosError> {
        let mut client = match dial().await {
            Ok(c) => c,
            Err(Dial::Unreachable(why) | Dial::Refused(why)) => return Ok(not_checked(&why)),
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
            "can't reach the daemon to check; node health unknown".into()
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
        let mut client = match dial().await {
            Ok(c) => c,
            Err(Dial::Unreachable(_)) => return Ok(None),
            Err(Dial::Refused(why)) => return Err(CosError::Tool { name: "mesh.routes".into(), reason: why }),
        };
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
                summary: "can't reach the daemon to check; route state unknown".into(),
                result: not_checked("no daemon answered at the resolved endpoint"),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unreachable_daemon_is_not_reported_as_unhealthy() {
        let dir = tempfile::tempdir().unwrap();
        let res = clawft_rpc::resolve::resolve_with(&clawft_rpc::resolve::ResolveInputs {
            env_runtime: Some(dir.path().to_string_lossy().into_owned()),
            ..Default::default()
        })
        .unwrap();
        let Err(Dial::Unreachable(why)) = dial_resolved(&res).await else { panic!("expected unreachable") };
        assert!(why.contains(&dir.path().to_string_lossy().into_owned()) || why.contains("kernel.sock"), "{why}");
        let snap = not_checked(&why);
        assert_eq!(snap["reachable"], false);
        assert_eq!(snap["health_known"], false);
        let tool = NodesHealth { source: Arc::new(StaticHealth(snap)) };
        let out = tool.call(json!({})).await.unwrap();
        assert!(out.summary.contains("can't reach") && !out.summary.contains("unhealthy"), "{}", out.summary);
        assert!(out.result["reason"].as_str().unwrap().contains("I can't reach the weaver daemon to check"));
    }
}
