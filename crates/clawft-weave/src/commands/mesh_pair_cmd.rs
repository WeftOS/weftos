//! `weaver mesh pair request|list|cancel` (ADR-108 P2b): pending requests to
//! pair this node with another for project work. A request is recorded on
//! this node and reported to the dashboard on the next heartbeat; a member
//! approves it there, and the approval comes back as a `pair` action that
//! writes the trust files on both nodes. Nothing here touches a trust file.

use std::io::Write;

use anyhow::{Result, bail};
use clap::Subcommand;
use serde_json::{Value, json};

use crate::protocol;

/// `weaver mesh pair`.
#[derive(Subcommand, Debug)]
pub enum PairCmd {
    /// Ask to pair with a node (its mesh node id, as the dashboard shows it).
    Request {
        /// Mesh node id of the other side (32 hex), from its `mesh_identity`.
        #[arg(long = "with")]
        with_node: String,
        /// Project ULID the pairing is for (repeatable; none means any project).
        #[arg(long = "project")]
        projects: Vec<String>,
        /// Print the raw JSON reply.
        #[arg(long)]
        json: bool,
    },
    /// List this node's pending requests.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Withdraw a pending request.
    Cancel {
        /// The request id from `list`.
        request_id: String,
        #[arg(long)]
        json: bool,
    },
}

async fn call(method: &str, params: Value) -> Result<Value> {
    let mut client = clawft_rpc::connect_or_bail().await?;
    let resp = client.call(protocol::Request::with_params(method, params)).await?;
    if !resp.ok {
        bail!(resp.error.unwrap_or_else(|| "unknown error".into()));
    }
    Ok(resp.result.unwrap_or_default())
}

/// One line per request.
pub fn render_requests(v: &Value) -> String {
    let Some(list) = v["requests"].as_array().filter(|l| !l.is_empty()) else {
        return "no pending pair requests\n".to_owned();
    };
    let mut out = String::new();
    for r in list {
        let projects = r["projects"].as_array().map(|p| p.len()).unwrap_or(0);
        let scope = if projects == 0 { "any project".to_owned() } else { format!("{projects} project(s)") };
        out.push_str(&format!(
            "{}  with {}  {}  since {}\n",
            r["request_id"].as_str().unwrap_or("?"),
            r["with_node"].as_str().unwrap_or("?"),
            scope,
            r["requested_at"].as_str().unwrap_or("?")
        ));
    }
    out
}

/// Shown before the reply when this node reports no address to dial.
pub fn no_advertise_warning(reply: &Value) -> Option<&'static str> {
    reply.get("advertise").is_some_and(Value::is_null).then_some(
        "warning: this node has no advertise address (its mesh listens on loopback or is off). \
         The primary will accept the pairing but cannot dial back. Set `[kernel.mesh] listen_addr` \
         (or the workload-host advertise) to a tailnet address to fix it.",
    )
}

/// Run one `pair` verb, writing to `w` (tests capture it).
pub async fn run(cmd: PairCmd, w: &mut dyn Write) -> Result<()> {
    match cmd {
        PairCmd::Request { with_node, projects, json: raw } => {
            let v = call("mesh.pair.request", json!({ "with_node": with_node, "projects": projects })).await?;
            if raw {
                writeln!(w, "{}", serde_json::to_string_pretty(&v)?)?;
            } else {
                if let Some(warn) = no_advertise_warning(&v) {
                    writeln!(w, "{warn}")?;
                }
                writeln!(
                    w,
                    "pair request {} recorded for node {}; it reaches the dashboard on the next heartbeat. \
                     Compare the fingerprints shown there with both machines before approving.",
                    v["request"]["request_id"].as_str().unwrap_or("?"),
                    v["request"]["with_node"].as_str().unwrap_or("?")
                )?;
            }
        }
        PairCmd::List { json: raw } => {
            let v = call("mesh.pair.list", json!({})).await?;
            if raw {
                writeln!(w, "{}", serde_json::to_string_pretty(&v)?)?;
            } else {
                write!(w, "{}", render_requests(&v))?;
            }
        }
        PairCmd::Cancel { request_id, json: raw } => {
            let v = call("mesh.pair.cancel", json!({ "request_id": request_id })).await?;
            if raw {
                writeln!(w, "{}", serde_json::to_string_pretty(&v)?)?;
            } else if v["cancelled"] == true {
                writeln!(w, "cancelled {request_id}")?;
            } else {
                writeln!(w, "no pending request {request_id}")?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warns_only_when_this_node_has_no_advertise_address() {
        let w = no_advertise_warning(&json!({ "advertise": null })).unwrap();
        assert!(w.contains("cannot dial back") && w.contains("[kernel.mesh] listen_addr"), "{w}");
        assert!(no_advertise_warning(&json!({})).is_none(), "an older daemon reply carries no field: no guess");
        assert!(no_advertise_warning(&json!({ "advertise": "100.64.0.1:9471" })).is_none());
    }

    #[test]
    fn requests_render_one_line_each_or_say_none() {
        assert_eq!(render_requests(&json!({ "requests": [] })), "no pending pair requests\n");
        let v = json!({ "requests": [
            { "request_id": "r1", "with_node": "0123456789abcdef0123456789abcdef", "projects": [], "requested_at": "2026-10-08T00:00:00Z" },
            { "request_id": "r2", "with_node": "fedcba9876543210fedcba9876543210", "projects": ["01K00000000000000000000000"], "requested_at": "2026-10-08T00:00:01Z" },
        ] });
        let out = render_requests(&v);
        assert!(out.contains("r1  with 0123456789abcdef0123456789abcdef  any project"), "{out}");
        assert!(out.contains("r2  with fedcba9876543210fedcba9876543210  1 project(s)"), "{out}");
    }
}
