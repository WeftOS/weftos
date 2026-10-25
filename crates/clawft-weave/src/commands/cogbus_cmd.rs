//! `weaver cogbus` (ADR-122): the node's cog data plane broker.
//!
//! - `weaver cogbus list [--json]`: every topic with schema, class, writer
//!   instance, subscriber count, messages and drops. Read.

use clap::{Parser, Subcommand};
use serde_json::{Value, json};

use crate::protocol;

/// Cogbus subcommand.
#[derive(Parser)]
#[command(about = "The node's cog data plane broker: list topics")]
pub struct CogbusArgs {
    #[command(subcommand)]
    pub action: CogbusAction,
}

/// Cogbus subcommands.
#[derive(Subcommand)]
pub enum CogbusAction {
    /// Show every topic on this node.
    List {
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

/// The `cogbus.list` document as a table.
pub fn render_list(v: &Value) -> String {
    if v["running"] != true {
        return "cogbus is not running on this daemon: cogs are placed without WEFTOS_COGBUS\n".into();
    }
    let mut out = format!("socket: {}\n", cell(v, "socket"));
    let topics = v["topics"].as_array().cloned().unwrap_or_default();
    if topics.is_empty() {
        out += "no topics published yet\n";
        return out;
    }
    out += &format!("{:<36} {:<16} {:<8} {:<28} {:>5} {:>9} {:>7}\n", "TOPIC", "SCHEMA", "CLASS", "WRITER", "SUBS", "MESSAGES", "DROPS");
    for t in topics {
        out += &format!(
            "{:<36} {:<16} {:<8} {:<28} {:>5} {:>9} {:>7}\n",
            cell(&t, "topic"),
            cell(&t, "schema"),
            cell(&t, "class"),
            cell(&t, "writer"),
            cell(&t, "subscribers"),
            cell(&t, "messages"),
            cell(&t, "drops")
        );
    }
    out
}

/// Run the cogbus subcommand.
pub async fn run(args: CogbusArgs) -> anyhow::Result<()> {
    match args.action {
        CogbusAction::List { json } => {
            let mut client = clawft_rpc::connect_or_bail().await?;
            let resp = client.call(protocol::Request::with_params("cogbus.list", json!({}))).await?;
            if !resp.ok {
                anyhow::bail!(resp.error.unwrap_or_else(|| "unknown error".into()));
            }
            let v = resp.result.unwrap_or_default();
            if json {
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                print!("{}", render_list(&v));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_shows_topics_and_says_when_the_broker_is_down() {
        let v = json!({"running": true, "socket": "/run/weftos/cogbus.sock", "topics": [
            {"topic": "telemetry/venq-board", "schema": "telemetry.v1", "class": "reading",
             "writer": "board-telemetry-1", "subscribers": 2, "messages": 100, "drops": 3}]});
        let t = render_list(&v);
        assert!(t.contains("telemetry/venq-board") && t.contains("board-telemetry-1"), "{t}");
        assert!(t.contains("100") && t.contains("reading"), "{t}");
        assert!(render_list(&json!({"running": true, "socket": "s", "topics": []})).contains("no topics"));
        assert!(render_list(&json!({"running": false})).contains("not running"));
    }

    #[test]
    fn list_parses_with_and_without_json() {
        assert!(CogbusArgs::try_parse_from(["cogbus", "list"]).is_ok());
        assert!(CogbusArgs::try_parse_from(["cogbus", "list", "--json"]).is_ok());
        assert!(CogbusArgs::try_parse_from(["cogbus"]).is_err());
    }
}
