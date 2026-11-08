//! `agent.directory {since_etag?, mesh?}`: the joined agent directory
//! (declared, running, exposed; `weftos::agent_directory`) with an etag, so a
//! poller (Tapestry's roster) can ask "has anything changed?" cheaply.
//!
//! | Params | Class | Result |
//! |---|---|---|
//! | none / `since_etag` | Read | `{etag, directory}` for this node |
//! | `mesh: true` | Admin | the same, with every known peer joined in |
//!
//! With `since_etag` equal to the current etag the answer is
//! `{etag, not_modified: true}` and carries no directory. The etag changes
//! when an agent starts or stops, an exposure changes, a declared package
//! changes, or a peer goes down or comes back. A peer that does not answer is
//! named in `directory.sources.nodes[]` with its error; it is never shown as
//! an empty list. A malformed parameter is refused, never defaulted.

use clawft_rpc::Response;
use serde_json::{Value, json};
use weftos::agent_directory::{Declared, NodeInput, etag, join, load_declared};

use crate::rpc_ext::{ExtCall, ExtFuture};

/// Parsed, validated parameters.
#[derive(Debug, PartialEq)]
pub struct Params {
    pub since_etag: Option<String>,
    pub mesh: bool,
}

/// Parse the parameters and apply the tier rule: `mesh: true` needs Admin.
pub fn parse_params(params: &Value, is_admin: bool) -> Result<Params, String> {
    let since_etag = match params.get("since_etag") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if !s.is_empty() && s.len() <= 128 => Some(s.clone()),
        Some(_) => return Err("`since_etag` must be a non-empty string of at most 128 characters".into()),
    };
    let mesh = match params.get("mesh") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err("`mesh` must be true or false".into()),
    };
    if mesh && !is_admin {
        return Err("asking other nodes for their agents needs Admin".into());
    }
    Ok(Params { since_etag, mesh })
}

/// Join and answer. Pure: the caller supplies what it read.
pub fn respond(since_etag: Option<&str>, declared: &Declared, nodes: &[NodeInput]) -> Result<Value, String> {
    if let Err(d) = &declared.rows
        && nodes.iter().all(|n| n.running.is_err() && n.exposed.is_err())
    {
        let n = nodes.first();
        return Err(format!(
            "agent directory unavailable: declared: {d}; running: {}; exposed: {}",
            n.and_then(|n| n.running.as_ref().err()).map_or("", String::as_str),
            n.and_then(|n| n.exposed.as_ref().err()).map_or("", String::as_str)
        ));
    }
    let directory = join(declared, nodes);
    let tag = etag(&directory);
    if since_etag == Some(tag.as_str()) {
        return Ok(json!({ "etag": tag, "not_modified": true }));
    }
    Ok(json!({ "etag": tag, "directory": directory }))
}

fn load_declared_here() -> Declared {
    match std::env::current_dir() {
        Ok(cwd) => load_declared(&cwd, std::env::var("WEFTOS_AGENTS_DIR").ok().as_deref()),
        Err(e) => Declared {
            rows: Err(format!("cwd: {e}")),
            teams: Err(format!("cwd: {e}")),
            origin: json!({"origin": null}),
        },
    }
}

async fn handle_inner(params: Value, kernel: &crate::rpc_ext::KernelRef, is_admin: bool) -> Result<Value, String> {
    let p = parse_params(&params, is_admin)?;
    let doc = crate::agent_sources_rpc::handle_inner(json!({ "mesh": p.mesh }), kernel, is_admin).await?;
    let blocks = doc
        .get("nodes")
        .and_then(Value::as_array)
        .ok_or("agent.sources returned no nodes array")?;
    let nodes: Vec<NodeInput> = blocks
        .iter()
        .map(|b| {
            let mut n = NodeInput::from_block(b);
            if !p.mesh {
                n.node = None; // local view: rows carry no node field
            }
            n
        })
        .collect();
    let declared = tokio::task::spawn_blocking(load_declared_here)
        .await
        .map_err(|e| format!("declared agents: {e}"))?;
    respond(p.since_etag.as_deref(), &declared, &nodes)
}

/// Route handler for `agent.directory`.
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

    fn declared(ids: &[&str]) -> Declared {
        Declared {
            rows: Ok(ids.iter().map(|i| json!({"id": i, "version": "1", "state": "active"})).collect()),
            teams: Ok(vec![]),
            origin: json!({"origin": "disk"}),
        }
    }
    fn block(node: &str, agents: &[(u64, &str)]) -> NodeInput {
        NodeInput::from_block(&json!({
            "node_id": node, "ok": true,
            "agents": agents.iter().map(|(p, a)| json!({"pid": p, "agent_id": a, "state": "running"})).collect::<Vec<_>>(),
            "exposures": {"exposures": []},
        }))
    }

    #[test]
    fn an_unchanged_directory_is_not_modified() {
        let d = declared(&["developer"]);
        let nodes = vec![block("n1", &[(1, "developer")])];
        let first = respond(None, &d, &nodes).unwrap();
        let etag = first["etag"].as_str().unwrap().to_string();
        assert!(first["directory"]["agents"].is_array());
        assert!(first.get("not_modified").is_none());
        let again = respond(Some(&etag), &d, &nodes).unwrap();
        assert_eq!(again["not_modified"], true);
        assert_eq!(again["etag"], etag.as_str());
        assert!(again.get("directory").is_none(), "no directory body when not modified");
        // A stale etag gets the full directory.
        let stale = respond(Some("dir1-stale"), &d, &nodes).unwrap();
        assert!(stale["directory"].is_object());
    }

    #[test]
    fn starting_or_stopping_an_agent_changes_the_etag() {
        let d = declared(&["developer"]);
        let stopped = respond(None, &d, &[block("n1", &[])]).unwrap()["etag"].clone();
        let running = respond(None, &d, &[block("n1", &[(9, "developer")])]).unwrap();
        assert_ne!(stopped, running["etag"]);
        let e = running["etag"].as_str().unwrap();
        let after_stop = respond(Some(e), &d, &[block("n1", &[])]).unwrap();
        assert!(after_stop.get("not_modified").is_none(), "a stop is a change");
        assert_eq!(after_stop["etag"], stopped);
    }

    #[test]
    fn a_down_peer_is_named_never_an_empty_list() {
        let d = declared(&["developer"]);
        let nodes = vec![
            block("n1", &[(1, "developer")]),
            NodeInput::from_block(&json!({"node_id": "n2", "ok": false, "error": "timed out"})),
        ];
        let r = respond(None, &d, &nodes).unwrap();
        let n2 = &r["directory"]["sources"]["nodes"][1];
        assert_eq!(n2["node"], "n2");
        assert_eq!(n2["ok"], false);
        assert_eq!(n2["running"]["error"], "timed out");
        // The peer coming back changes the etag.
        let back = respond(None, &d, &[block("n1", &[(1, "developer")]), block("n2", &[])]).unwrap();
        assert_ne!(r["etag"], back["etag"]);
    }

    #[test]
    fn nothing_readable_is_an_error_not_an_empty_directory() {
        let bad = Declared { rows: Err("no tree".into()), teams: Err("no tree".into()), origin: json!({}) };
        let down = vec![NodeInput::from_block(&json!({"node_id": "n1", "ok": false, "error": "dead"}))];
        let e = respond(None, &bad, &down).unwrap_err();
        assert!(e.contains("no tree") && e.contains("dead"), "{e}");
        assert!(respond(None, &bad, &[]).is_err());
    }

    #[test]
    fn params_are_validated_and_mesh_needs_admin() {
        assert_eq!(parse_params(&json!({}), false).unwrap(), Params { since_etag: None, mesh: false });
        assert_eq!(
            parse_params(&json!({"since_etag": "x", "mesh": true}), true).unwrap(),
            Params { since_etag: Some("x".into()), mesh: true }
        );
        let denied = parse_params(&json!({"mesh": true}), false).unwrap_err();
        assert!(denied.contains("Admin"), "{denied}");
        for bad in [
            json!({"since_etag": 5}),
            json!({"since_etag": ""}),
            json!({"since_etag": "x".repeat(129)}),
            json!({"mesh": "yes"}),
        ] {
            assert!(parse_params(&bad, true).is_err(), "{bad}");
        }
    }

    #[test]
    fn classified_read_and_registered() {
        use crate::capability::{Capability, required_capability};
        assert_eq!(required_capability("agent.directory"), Capability::Read);
        assert!(crate::rpc_ext::builtin_route_names().iter().any(|(n, c)| *n == "agent.directory" && *c == Capability::Read));
    }
}
