//! `weaver cogbus` (ADR-122): the node's cog data plane broker.
//!
//! - `weaver cogbus list [--json]`: every topic with schema, class, writer
//!   instance, subscriber count, messages and drops, then the recent
//!   refusals (time, action, instance, topic, outcome). Read.

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
        out += &render_refusals(v);
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
    out += &render_refusals(v);
    out
}

fn outcome(r: &Value) -> String {
    match r["outcome"].as_str() {
        Some("refused") => {
            let n = r["suppressed"].as_u64().unwrap_or(0);
            let more = if n > 0 { format!(" (+{n} more)") } else { String::new() };
            format!("{}{more}: {}", cell(r, "code"), cell(r, "reason"))
        }
        Some("stopped") => format!("STOPPED after {} refusals", cell(r, "refusals")),
        _ => cell(r, "outcome"),
    }
}

/// The recent refusals of a `cogbus.list` document, newest last.
pub fn render_refusals(v: &Value) -> String {
    let rows = v["refusals"].as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        return "no recent refusals\n".into();
    }
    let mut out = format!("\nrecent refusals:\n{:<20} {:<18} {:<28} {:<32} OUTCOME\n", "TIME", "ACTION", "INSTANCE", "TOPIC");
    for r in rows {
        let when = r["ts_ms"]
            .as_i64()
            .and_then(chrono::DateTime::from_timestamp_millis)
            .map_or_else(|| "-".into(), |t| t.format("%Y-%m-%d %H:%M:%S").to_string());
        out += &format!("{when:<20} {:<18} {:<28} {:<32} {}\n", cell(&r, "action"), cell(&r, "instance"), cell(&r, "topic"), outcome(&r));
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
    fn list_shows_recent_refusals_with_folded_counts_and_stops() {
        let v = json!({"running": true, "socket": "s", "topics": [], "refusals": [
            {"ts_ms": 1_700_000_000_000_i64, "action": "cogbus.subscribe", "instance": "reader-1",
             "project": null, "topic": "audio.pcm/mic0", "outcome": "refused", "code": "denied",
             "reason": "cogbus.subscribe denied by governance: no audio", "suppressed": 7},
            {"ts_ms": 1_700_000_001_000_i64, "action": "cogbus.stop", "instance": "reader-1",
             "project": null, "topic": "-", "outcome": "stopped", "refusals": 20}]});
        let t = render_list(&v);
        assert!(t.contains("recent refusals") && t.contains("audio.pcm/mic0"), "{t}");
        assert!(t.contains("denied (+7 more): cogbus.subscribe denied"), "{t}");
        assert!(t.contains("STOPPED after 20 refusals"), "{t}");
        assert!(render_list(&json!({"running": true, "socket": "s", "topics": []})).contains("no recent refusals"));
    }

    #[test]
    fn list_parses_with_and_without_json() {
        assert!(CogbusArgs::try_parse_from(["cogbus", "list"]).is_ok());
        assert!(CogbusArgs::try_parse_from(["cogbus", "list", "--json"]).is_ok());
        assert!(CogbusArgs::try_parse_from(["cogbus"]).is_err());
    }
}
