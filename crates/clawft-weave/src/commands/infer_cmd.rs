//! `weaver infer` (ADR-101): inference placement on this node.
//!
//! - `weaver infer status [--json]`: roles, where each resolves (a local
//!   server, a cogbus service, or a peer), the proxy listeners and whether the
//!   role is exposed to the mesh. Read.
//! - `weaver infer expose <role> [--audience local|paired|nodes:a,b|project:p]
//!   [--off]`: let the role leave this node for the mesh (`infer.expose`), or
//!   stop. `--audience` also sets who may use it (the ADR-120 exposure
//!   `infer.<role>`). Admin. Peers still need `infer.allow` (direction
//!   `serve`) or `mesh.serve_peers` in `inference.json`.

use clap::{Parser, Subcommand};
use serde_json::{Value, json};

use crate::protocol;

/// Inference subcommand.
#[derive(Parser)]
#[command(about = "Inference placement: status, expose")]
pub struct InferArgs {
    #[command(subcommand)]
    pub action: InferAction,
}

/// Inference subcommands.
#[derive(Subcommand)]
pub enum InferAction {
    /// Show every role and where it is served.
    Status {
        /// Print the raw JSON.
        #[arg(long)]
        json: bool,
    },
    /// Expose a role to the mesh (or stop with --off).
    Expose {
        /// Role name (`vision`).
        role: String,
        /// Who may use it: local, paired, nodes:<a,b> or project:<id>.
        #[arg(long, conflicts_with = "off")]
        audience: Option<String>,
        /// Stop exposing the role.
        #[arg(long)]
        off: bool,
    },
}

async fn call(method: &str, params: Value) -> anyhow::Result<Value> {
    let mut client = clawft_rpc::connect_or_bail().await?;
    let resp = client.call(protocol::Request::with_params(method, params)).await?;
    if !resp.ok {
        anyhow::bail!(resp.error.unwrap_or_else(|| "unknown error".into()));
    }
    Ok(resp.result.unwrap_or_default())
}

fn serves(v: &Value) -> String {
    match v {
        Value::Null => "-".into(),
        Value::String(s) => s.clone(),
        o => match (o.get("remote").and_then(Value::as_str), o.get("svc").and_then(Value::as_str)) {
            (Some(n), _) => format!("remote:{n}"),
            (_, Some(t)) => format!("svc:{t}"),
            _ => o.to_string(),
        },
    }
}

/// The `infer.status` document as a table.
pub fn render_status(v: &Value) -> String {
    let mut out = format!("node {}\n", v["node_id"].as_str().unwrap_or("-"));
    let mesh = &v["mesh"];
    out += &format!(
        "mesh: {}{}\n",
        if mesh["available"] == true { "available" } else { "unavailable" },
        mesh["note"].as_str().map(|n| format!(" ({n})")).unwrap_or_default()
    );
    let roles = v["roles"].as_array().cloned().unwrap_or_default();
    if roles.is_empty() {
        return out + "no roles\n";
    }
    out += &format!("{:<16} {:<8} {:<10} {:<9} {:<30} {}\n", "ROLE", "MODE", "STATE", "EXPOSED", "SERVES", "MODEL / WHY");
    for r in roles {
        let why = r["reason"].as_str().map(String::from).or_else(|| r["model"].as_str().map(String::from)).unwrap_or_else(|| "-".into());
        out += &format!(
            "{:<16} {:<8} {:<10} {:<9} {:<30} {}\n",
            r["role"].as_str().unwrap_or("-"),
            r["mode"].as_str().unwrap_or("-"),
            r["state"].as_str().unwrap_or("-"),
            if r["exposed"] == true { "yes" } else { "no" },
            serves(&r["serves"]),
            why
        );
    }
    out
}

/// Run the inference subcommand.
pub async fn run(args: InferArgs) -> anyhow::Result<()> {
    match args.action {
        InferAction::Status { json } => {
            let v = call("infer.status", json!({})).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                print!("{}", render_status(&v));
            }
        }
        InferAction::Expose { role, audience, off } => {
            let id = format!("infer.{role}");
            let v = call("infer.expose", json!({"role": role, "exposed": !off})).await?;
            if let (false, Some(a)) = (off, audience) {
                let row = call("expose.set", json!({"id": id, "state": "on", "audience": a})).await?;
                println!("{id}: audience {}", row["audience"].as_str().unwrap_or(&a));
            }
            println!("{}: {}", v["role"].as_str().unwrap_or(&role), if v["exposed"] == true { "exposed to the mesh" } else { "not exposed" });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_shows_where_each_role_is_served() {
        let v = json!({"node_id": "venq", "mesh": {"available": true, "note": "hub"}, "roles": [
            {"role": "vision", "mode": "svc", "state": "serving", "exposed": true, "serves": {"svc": "svc.infer/venq-npu"}, "model": "qwen3-vl-4b", "reason": null},
            {"role": "hermes", "mode": "adopted", "state": "waiting", "exposed": false, "serves": null, "reason": "down"}]});
        let t = render_status(&v);
        assert!(t.contains("svc:svc.infer/venq-npu") && t.contains("qwen3-vl-4b") && t.contains("serving"), "{t}");
        assert!(t.contains("hermes") && t.contains("down"), "{t}");
        assert!(render_status(&json!({"node_id": "n", "mesh": {}, "roles": []})).contains("no roles"));
    }

    #[test]
    fn audience_and_off_do_not_combine() {
        use clap::Parser;
        assert!(InferArgs::try_parse_from(["infer", "expose", "vision", "--audience", "paired"]).is_ok());
        assert!(InferArgs::try_parse_from(["infer", "expose", "vision", "--off"]).is_ok());
        assert!(InferArgs::try_parse_from(["infer", "expose", "vision", "--off", "--audience", "paired"]).is_err());
        assert!(InferArgs::try_parse_from(["infer", "status", "--json"]).is_ok());
    }
}
