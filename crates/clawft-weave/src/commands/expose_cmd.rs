//! `weaver expose` (ADR-120): what this node offers to the mesh.
//!
//! - `weaver expose list [--json]`: every exposure with audience, state, whether
//!   it is announced and served, and 24 h use. Read.
//! - `weaver expose on <id> [--audience local|paired|nodes:a,b|project:p]`,
//!   `off <id>`, `pause <id>`: a local decision, persisted. Off withdraws the
//!   advertisement at once and refuses new requests. Admin.
//! - `weaver expose off --all`: every exposure off. Admin.
//! - `weaver expose log [<id>] [--limit N] [--json]`: recent requests (caller,
//!   bytes, duration, outcome; never bodies). Admin.
//!
//! `[mesh.expose] enabled = false` in `~/.weftos/weave.toml` is the master switch.

use clap::{Parser, Subcommand};
use serde_json::{Value, json};

use crate::protocol;

/// Exposure subcommand.
#[derive(Parser)]
#[command(about = "What this node exposes to the mesh: list, on, off, pause, log")]
pub struct ExposeArgs {
    #[command(subcommand)]
    pub action: ExposeAction,
}

/// Exposure subcommands.
#[derive(Subcommand)]
pub enum ExposeAction {
    /// Show every exposure.
    List {
        /// Print the raw JSON.
        #[arg(long)]
        json: bool,
    },
    /// Switch an exposure on (to its audience; `local` unless widened).
    On {
        /// Exposure id (`infer.chat`, ...).
        id: String,
        /// Who may use it: local, paired, nodes:<a,b> or project:<id>.
        #[arg(long)]
        audience: Option<String>,
    },
    /// Switch an exposure off, or every exposure with `--all`.
    Off {
        /// Exposure id.
        #[arg(required_unless_present = "all", conflicts_with = "all")]
        id: Option<String>,
        /// Switch every exposure off.
        #[arg(long)]
        all: bool,
    },
    /// Pause an exposure (not served; stays registered).
    Pause {
        /// Exposure id.
        id: String,
    },
    /// Show recent requests to exposures.
    Log {
        /// Only this exposure.
        id: Option<String>,
        /// Newest N records.
        #[arg(long, default_value_t = 50)]
        limit: usize,
        /// Print the raw JSON.
        #[arg(long)]
        json: bool,
    },
}

fn cell(v: &Value, k: &str) -> String {
    match &v[k] {
        Value::Null => "-".into(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// The `expose.list` document as a table.
pub fn render_list(v: &Value) -> String {
    let mut out = String::new();
    if v["enabled"] != true {
        out += "MASTER SWITCH OFF ([mesh.expose] enabled = false): nothing is announced or served\n";
    }
    let items = v["exposures"].as_array().cloned().unwrap_or_default();
    if items.is_empty() {
        out += "no exposures registered: nothing is offered to the mesh\n";
        return out;
    }
    out += &format!("{:<24} {:<10} {:<10} {:<8} {:<9} {:<6} {}\n", "ID", "KIND", "AUDIENCE", "STATE", "ANNOUNCED", "24H", "RESOURCE / WHY NOT SERVED");
    for e in items {
        let note = match e["refused"].as_str() {
            Some(why) => format!("{} (refused: {why})", cell(&e, "resource")),
            None => cell(&e, "resource"),
        };
        out += &format!(
            "{:<24} {:<10} {:<10} {:<8} {:<9} {:<6} {}\n",
            cell(&e, "id"),
            cell(&e, "kind"),
            cell(&e, "audience"),
            cell(&e, "state"),
            if e["announced"] == true { "yes" } else { "no" },
            cell(&e, "n24"),
            note
        );
    }
    out
}

/// The `expose.log` document as lines.
pub fn render_log(v: &Value) -> String {
    let mut out = String::new();
    if v["chain_ok"] != true {
        out += &format!("WARNING: activity chain does not verify: {}\n", cell(v, "chain"));
    }
    let recs = v["records"].as_array().cloned().unwrap_or_default();
    if recs.is_empty() {
        out += "no activity recorded\n";
        return out;
    }
    out += &format!("{:<15} {:<22} {:<14} {:<12} {:>8} {:>8} {:>7}  {}\n", "TIME(ms)", "EXPOSURE", "CALLER", "USER", "IN", "OUT", "MS", "OUTCOME");
    for r in recs {
        out += &format!(
            "{:<15} {:<22} {:<14} {:<12} {:>8} {:>8} {:>7}  {}\n",
            cell(&r, "ts_ms"),
            cell(&r, "exposure"),
            cell(&r, "caller_node").chars().take(14).collect::<String>(),
            cell(&r, "caller_user"),
            cell(&r, "bytes_in"),
            cell(&r, "bytes_out"),
            cell(&r, "duration_ms"),
            cell(&r, "outcome")
        );
    }
    out
}

async fn call(method: &str, params: Value) -> anyhow::Result<Value> {
    let mut client = clawft_rpc::connect_or_bail().await?;
    let resp = client.call(protocol::Request::with_params(method, params)).await?;
    if !resp.ok {
        anyhow::bail!(resp.error.unwrap_or_else(|| "unknown error".into()));
    }
    Ok(resp.result.unwrap_or_default())
}

fn print_row(v: &Value) {
    println!("{}: {} for {} (announced {}, served {})", cell(v, "id"), cell(v, "state"), cell(v, "audience"), cell(v, "announced"), cell(v, "served"));
    if let Some(why) = v["refused"].as_str() {
        println!("  not served: {why}");
    }
}

/// Run the exposure subcommand.
pub async fn run(args: ExposeArgs) -> anyhow::Result<()> {
    match args.action {
        ExposeAction::List { json } => {
            let v = call("expose.list", json!({})).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                print!("{}", render_list(&v));
            }
        }
        ExposeAction::On { id, audience } => {
            let mut p = json!({"id": id, "state": "on"});
            if let Some(a) = audience {
                p["audience"] = json!(a);
            }
            print_row(&call("expose.set", p).await?);
        }
        ExposeAction::Off { id, all } => {
            if all {
                let v = call("expose.off_all", json!({})).await?;
                let ids = v["switched_off"].as_array().map_or(0, Vec::len);
                println!("switched off {ids} exposure(s); advertisements withdrawn");
            } else if let Some(id) = id {
                print_row(&call("expose.set", json!({"id": id, "state": "off"})).await?);
            }
        }
        ExposeAction::Pause { id } => {
            print_row(&call("expose.set", json!({"id": id, "state": "paused"})).await?);
        }
        ExposeAction::Log { id, limit, json } => {
            let v = call("expose.log", json!({"id": id, "limit": limit})).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                print!("{}", render_log(&v));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_shows_why_an_exposure_is_not_served_and_the_master_switch() {
        let v = json!({"enabled": false, "exposures": [
            {"id": "infer.chat", "kind": "inference", "audience": "paired", "state": "off", "announced": true,
             "n24": 3, "resource": "runtime:LlamaCpp", "refused": "switched off locally"}]});
        let t = render_list(&v);
        assert!(t.contains("MASTER SWITCH OFF"), "{t}");
        assert!(t.contains("infer.chat") && t.contains("refused: switched off locally"), "{t}");
        assert!(render_list(&json!({"enabled": true, "exposures": []})).contains("nothing is offered"));
    }

    #[test]
    fn log_warns_on_a_broken_chain_and_never_prints_bodies() {
        let v = json!({"chain_ok": false, "chain": "chain broken at seq 3", "records": [
            {"ts_ms": 1, "exposure": "infer.chat", "caller_node": "n-peer", "caller_user": null,
             "bytes_in": 10, "bytes_out": 20, "duration_ms": 5, "outcome": "ok"}]});
        let t = render_log(&v);
        assert!(t.contains("WARNING") && t.contains("chain broken at seq 3"), "{t}");
        assert!(t.contains("n-peer") && t.contains("ok"), "{t}");
        assert!(render_log(&json!({"chain_ok": true, "records": []})).contains("no activity"));
    }

    #[test]
    fn off_needs_an_id_or_all_but_not_both() {
        use clap::Parser;
        assert!(ExposeArgs::try_parse_from(["expose", "off"]).is_err());
        assert!(ExposeArgs::try_parse_from(["expose", "off", "--all"]).is_ok());
        assert!(ExposeArgs::try_parse_from(["expose", "off", "infer.chat", "--all"]).is_err());
        assert!(ExposeArgs::try_parse_from(["expose", "off", "infer.chat"]).is_ok());
    }
}
