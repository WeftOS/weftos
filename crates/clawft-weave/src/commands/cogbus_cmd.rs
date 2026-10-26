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
    out += &render_isolation(v);
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

/// The socket's mode and group, the uid mode, and each instance's isolation.
pub fn render_isolation(v: &Value) -> String {
    let mut out = String::new();
    if v["socket_mode"].is_string() {
        let grp = if v["socket_group_applied"] == true { "group cogbus".to_string() } else { format!("daemon's group ({})", cell(v, "socket_note")) };
        out += &format!("socket mode {} uid {} gid {}, {grp}\n", cell(v, "socket_mode"), cell(v, "socket_uid"), cell(v, "socket_gid"));
    }
    let iso = &v["isolation"];
    if iso.is_object() {
        out += &format!("cog uids: {}", cell(iso, "mode"));
        if let Some(why) = iso["reason"].as_str() {
            out += &format!(" ({why})");
        }
        if iso["require_cog_uid"] == true {
            out += ", require_cog_uid on";
        }
        out += "\n";
        for i in iso["instances"].as_array().into_iter().flatten() {
            out += &format!("  {:<28} {:<14} uid {}\n", cell(i, "instance"), cell(i, "isolation"), cell(i, "uid"));
        }
    }
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
    fn list_marks_shared_uid_instances_and_the_socket_group() {
        let v = json!({"running": true, "socket": "s", "topics": [],
            "socket_mode": "0660", "socket_uid": 0, "socket_gid": 990, "socket_group_applied": true, "socket_note": null,
            "isolation": {"mode": "shared-uid", "reason": "the daemon cannot switch uids (unprivileged user daemon)",
                "require_cog_uid": false, "group": "cogbus",
                "instances": [{"instance": "radar-1", "cog": "radar", "uid": 501, "isolation": "shared-uid"}]}});
        let t = render_list(&v);
        assert!(t.contains("socket mode 0660 uid 0 gid 990, group cogbus"), "{t}");
        assert!(t.contains("cog uids: shared-uid (the daemon cannot switch uids"), "{t}");
        assert!(t.contains("radar-1") && t.contains("shared-uid") && t.contains("uid 501"), "{t}");
        let keep = json!({"running": true, "socket": "s", "topics": [], "socket_mode": "0660",
            "socket_uid": 501, "socket_gid": 20, "socket_group_applied": false, "socket_note": "no cogbus group requested"});
        assert!(render_list(&keep).contains("daemon's group (no cogbus group requested)"));
    }

    #[test]
    fn list_parses_with_and_without_json() {
        assert!(CogbusArgs::try_parse_from(["cogbus", "list"]).is_ok());
        assert!(CogbusArgs::try_parse_from(["cogbus", "list", "--json"]).is_ok());
        assert!(CogbusArgs::try_parse_from(["cogbus"]).is_err());
    }
}
