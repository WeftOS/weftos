//! `weaver dashboard` subcommand: the daemon-native dashboard reporter.
//!
//! - `weaver dashboard status [--node <id>] [--json]`: reporter state, no token material.
//! - `weaver dashboard actions [--json]`: recent actions the dashboard queued for
//!   this node and their outcomes (local; no payloads). Read.
//! - `weaver dashboard rotate-token [--node <id>]`: rotate the node's dashboard
//!   token. Local by default; `--node` sends it to that peer over the signed
//!   mesh wire (the peer must be a pinned operator peer that lists this node's
//!   key as a controller). Admin.

use clap::{Parser, Subcommand};
use serde_json::{Value, json};

use crate::protocol;

/// Dashboard reporter subcommand.
#[derive(Parser)]
#[command(about = "The daemon's WeftOS dashboard reporter: status and token rotation")]
pub struct DashboardArgs {
    #[command(subcommand)]
    pub action: DashboardAction,
}

/// Dashboard subcommands.
#[derive(Subcommand)]
pub enum DashboardAction {
    /// Show the reporter's state (enabled, last heartbeat, last rotation).
    Status {
        /// Ask this peer instead of the local daemon (mesh node id).
        #[arg(long)]
        node: Option<String>,
        /// Print the raw JSON.
        #[arg(long)]
        json: bool,
    },
    /// List recent dashboard actions (install, update, remove, pair) and how they ended.
    Actions {
        /// Print the raw JSON.
        #[arg(long)]
        json: bool,
    },
    /// Rotate the dashboard token: the old one is revoked, the new one saved 0600.
    RotateToken {
        /// Rotate on this peer instead of the local daemon (mesh node id).
        #[arg(long)]
        node: Option<String>,
    },
}

fn params(node: &Option<String>) -> Value {
    match node {
        Some(n) => json!({ "node": n }),
        None => json!({}),
    }
}

/// The status document as lines.
pub fn render_status(v: &Value) -> String {
    if v["enabled"] != true {
        return "dashboard reporter: off ([dashboard] enabled = true in ~/.weftos/weave.toml to turn it on)\n".into();
    }
    let s = |k: &str| v[k].as_str().unwrap_or("-").to_owned();
    let mut out = String::new();
    out += &format!("dashboard reporter: on, every {}s\n", v["interval_secs"]);
    out += &format!("  url             {}\n  node            {}\n  installation    {}\n", s("url"), s("node_id"), s("installation_id"));
    out += &format!("  mesh id         {}\n", v["mesh_id"].as_str().unwrap_or("none (no MeshId on this node)"));
    out += &format!("  token file      {} ({})\n", s("token_file"), s("token_file_check"));
    out += &format!("  last heartbeat  {} at {}\n", s("last_heartbeat"), s("last_heartbeat_at"));
    out += &format!("  failures        {} in a row\n", v["consecutive_failures"]);
    out += &format!("  last rotation   {} at {}\n", s("last_rotation"), s("last_rotation_at"));
    if v["token_unpersisted"] == true {
        out += "  WARNING         a rotated token is held in memory and not yet saved to the token file\n";
    }
    out
}

/// The actions document as lines.
pub fn render_actions(v: &Value) -> String {
    let list = v["actions"].as_array().cloned().unwrap_or_default();
    if v["enabled"] == false {
        return "dashboard reporter: off ([dashboard] enabled = true in ~/.weftos/weave.toml to turn it on)\n".into();
    }
    if list.is_empty() {
        return "no dashboard actions received yet\n".into();
    }
    let mut out = format!("{} recorded, {} queued\n", v["recorded"], v["queued"]);
    for a in list {
        let s = |k: &str| a[k].as_str().unwrap_or("-").to_owned();
        out += &format!("  {}  {:<8} {:<10} {}{}\n", s("received_at"), s("kind"), s("status"), s("id"), if a["acked"] == true { "" } else { "  (not yet acknowledged)" });
        if let Some(e) = a["result"]["error"].as_str() {
            out += &format!("      {e}\n");
        }
    }
    out
}

/// Run the dashboard subcommand.
pub async fn run(args: DashboardArgs) -> anyhow::Result<()> {
    let mut client = clawft_rpc::connect_or_bail().await?;
    match args.action {
        DashboardAction::Status { node, json } => {
            let resp = client.call(protocol::Request::with_params("dashboard.status", params(&node))).await?;
            if !resp.ok {
                anyhow::bail!(resp.error.unwrap_or_else(|| "unknown error".into()));
            }
            let v = resp.result.unwrap_or_default();
            if json {
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                print!("{}", render_status(&v));
            }
        }
        DashboardAction::Actions { json } => {
            let resp = client.call(protocol::Request::with_params("dashboard.actions", json!({}))).await?;
            if !resp.ok {
                anyhow::bail!(resp.error.unwrap_or_else(|| "unknown error".into()));
            }
            let v = resp.result.unwrap_or_default();
            if json {
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                print!("{}", render_actions(&v));
            }
        }
        DashboardAction::RotateToken { node } => {
            let resp = client.call(protocol::Request::with_params("dashboard.token.rotate", params(&node))).await?;
            if !resp.ok {
                anyhow::bail!(resp.error.unwrap_or_else(|| "unknown error".into()));
            }
            let v = resp.result.unwrap_or_default();
            let place = node.as_deref().map_or("this node".to_owned(), |n| format!("node {n}"));
            println!("dashboard token rotated on {place} (rotated_at {}); the old token is revoked", v["rotated_at"]);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_renders_off_and_on_without_a_token() {
        assert!(render_status(&json!({"enabled": false})).contains("off"));
        let t = render_status(&json!({
            "enabled": true, "interval_secs": 60, "url": "https://d", "node_id": "n", "installation_id": "i",
            "token_file": "/t", "token_file_check": "ok", "last_heartbeat": "ok",
            "last_heartbeat_at": "now", "consecutive_failures": 0, "token_unpersisted": true
        }));
        assert!(t.contains("every 60s") && t.contains("last heartbeat  ok") && t.contains("WARNING"), "{t}");
        assert!(t.contains("mesh id         none"), "{t}");
        let id = "ab".repeat(32);
        let t = render_status(&json!({ "enabled": true, "mesh_id": id }));
        assert!(t.contains(&format!("mesh id         {id}")), "{t}");
    }

    #[test]
    fn actions_render_outcomes_and_errors() {
        let t = render_actions(&json!({"queued": 0, "recorded": 1, "actions": [
            {"id": "a1", "kind": "install", "status": "failed", "received_at": "t", "acked": true,
             "result": {"error": "not implemented in this build (ADR-108 P3/P4)"}}]}));
        assert!(t.contains("install") && t.contains("failed") && t.contains("not implemented"), "{t}");
        assert!(render_actions(&json!({"queued": 0, "recorded": 0, "actions": []})).contains("no dashboard actions"));
    }

    #[test]
    fn node_flag_becomes_the_node_param() {
        assert_eq!(params(&None), json!({}));
        assert_eq!(params(&Some("n-1".into())), json!({"node": "n-1"}));
    }
}
