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
        // The local daemon's own build (`kernel.status`); optional, an older
        // daemon may not report it.
        let local_version = client
            .simple_call("kernel.status")
            .await
            .ok()
            .and_then(|r| r.result)
            .and_then(|r| r["build"]["version"].as_str().map(str::to_string));
        Ok(json!({
            "reachable": true,
            "local_weaver_version": local_version,
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
        let mut snap = self.source.snapshot().await?;
        let summary = if snap["reachable"] == true {
            let n = snap["nodes"].as_array().map_or(0, Vec::len);
            let healthy = snap["summary"]["healthy_nodes"].as_u64().unwrap_or(0);
            let total = snap["summary"]["total_nodes"].as_u64().unwrap_or(n as u64);
            let versions = version_report(&snap);
            let line = versions["line"].as_str().unwrap_or_default().to_string();
            snap["versions"] = versions;
            format!("{healthy}/{total} nodes healthy; {line}")
        } else {
            "can't reach the daemon to check; node health unknown".into()
        };
        Ok(ToolOutcome { summary, result: snap })
    }
}

/// The weaver version of each node, compact. The local daemon reports its own
/// build; a peer row carries one only when the daemon supplies it, and the
/// report says so plainly when it does not, rather than leaving it to guess.
fn version_report(snap: &Value) -> Value {
    let mut by_node: Vec<Value> = Vec::new();
    if let Some(v) = snap["local_weaver_version"].as_str() {
        by_node.push(json!({"node": "local daemon", "weaver_version": v}));
    }
    for row in snap["nodes"].as_array().into_iter().flatten() {
        let v = row["weaver_version"].as_str().or_else(|| row["version"].as_str());
        if let Some(v) = v {
            let name = row["name"].as_str().or_else(|| row["node_id"].as_str()).unwrap_or("node");
            by_node.push(json!({"node": name, "weaver_version": v}));
        }
    }
    let total = snap["nodes"].as_array().map_or(0, Vec::len);
    let mut distinct: Vec<&str> = by_node.iter().filter_map(|e| e["weaver_version"].as_str()).collect();
    distinct.sort_unstable();
    distinct.dedup();
    let line = match (by_node.len(), distinct.as_slice()) {
        (0, _) => "weaver version not reported by the daemon".to_string(),
        (n, [one]) if n >= total.max(1) => format!("all {n} reporting node(s) run {one}"),
        _ => format!("weaver versions: {}", distinct.join(", ")),
    };
    let complete = !by_node.is_empty() && by_node.len() >= total.max(1);
    json!({
        "line": line,
        "by_node": by_node,
        "complete": complete,
        "note": if complete { Value::Null } else { json!("only the nodes listed in by_node reported a version; do not guess the others") },
    })
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
    async fn health_reports_weaver_versions() {
        let snap = json!({"reachable": true, "local_weaver_version": "0.8.4-rc.11",
            "summary": {"healthy_nodes": 2, "total_nodes": 2},
            "nodes": [{"name": "a", "weaver_version": "0.8.4-rc.11"}, {"name": "b", "version": "0.8.4-rc.11"}]});
        let out = NodesHealth { source: Arc::new(StaticHealth(snap)) }.call(json!({})).await.unwrap();
        assert!(out.summary.contains("2/2 nodes healthy") && out.summary.contains("0.8.4-rc.11"), "{}", out.summary);
        assert_eq!(out.result["versions"]["by_node"].as_array().unwrap().len(), 3);
        assert_eq!(out.result["versions"]["complete"], true);
    }

    #[tokio::test]
    async fn missing_peer_versions_are_said_not_guessed() {
        let snap = json!({"reachable": true, "local_weaver_version": "0.8.4-rc.11",
            "summary": {"healthy_nodes": 3, "total_nodes": 3},
            "nodes": [{"name": "a"}, {"name": "b"}, {"name": "c"}]});
        let out = NodesHealth { source: Arc::new(StaticHealth(snap)) }.call(json!({})).await.unwrap();
        assert_eq!(out.result["versions"]["complete"], false);
        assert!(out.result["versions"]["note"].as_str().unwrap().contains("do not guess"));
        let none = json!({"reachable": true, "summary": {}, "nodes": []});
        let out = NodesHealth { source: Arc::new(StaticHealth(none)) }.call(json!({})).await.unwrap();
        assert!(out.summary.contains("not reported"), "{}", out.summary);
    }

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
