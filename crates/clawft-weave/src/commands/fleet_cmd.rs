//! `weaver fleet` subcommand (fleet manager P1).
//!
//! - `weaver fleet status`: the `fleet.snapshot` document as a table (or raw
//!   JSON with `--json`): every node this daemon knows, its trust tier, last
//!   heartbeat, placed cogs and location.
//! - `weaver fleet location set <node> --site .. --room ..`: record where a
//!   node is (`fleet.location.set`, Admin, recorded on the chain).

use clap::{Parser, Subcommand};
use comfy_table::{Table, presets};
use serde_json::{Value, json};

use crate::protocol;

/// Fleet manager subcommand.
#[derive(Parser)]
#[command(about = "Fleet manager: what this daemon knows about every node, and where they are")]
pub struct FleetArgs {
    #[command(subcommand)]
    pub action: FleetAction,
}

/// Fleet subcommands.
#[derive(Subcommand)]
pub enum FleetAction {
    /// Show every known node with trust, last announce, placed cogs and location.
    Status {
        /// Print the raw snapshot JSON (every field with its provenance).
        #[arg(long)]
        json: bool,
    },

    /// Operator-set physical location labels.
    Location {
        #[command(subcommand)]
        action: LocationAction,
    },
}

/// Location subcommands.
#[derive(Subcommand)]
pub enum LocationAction {
    /// Set a node's site and room (Admin; recorded on the chain).
    Set {
        /// Node id (or the id an edge node checks in with).
        node: String,
        /// Site: building, lab or rig.
        #[arg(long)]
        site: String,
        /// Room within the site.
        #[arg(long)]
        room: String,
    },
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Run the fleet subcommand.
pub async fn run(args: FleetArgs) -> anyhow::Result<()> {
    let mut client = clawft_rpc::connect_or_bail().await?;
    match args.action {
        FleetAction::Status { json } => {
            let resp = client.simple_call("fleet.snapshot").await?;
            if !resp.ok {
                anyhow::bail!(resp.error.unwrap_or_else(|| "unknown error".into()));
            }
            let snap = resp.result.unwrap_or_default();
            if json {
                println!("{}", serde_json::to_string_pretty(&snap)?);
            } else {
                print!("{}", render(&snap, now_secs()));
            }
        }
        FleetAction::Location { action: LocationAction::Set { node, site, room } } => {
            let params = json!({ "node": node, "site": site, "room": room });
            let resp = client
                .call(protocol::Request::with_params("fleet.location.set", params))
                .await?;
            if !resp.ok {
                anyhow::bail!(resp.error.unwrap_or_else(|| "unknown error".into()));
            }
            let v = resp.result.unwrap_or_default();
            println!(
                "{} is at {} / {} (chain event {})",
                v["node"].as_str().unwrap_or(&node),
                v["site"].as_str().unwrap_or("?"),
                v["room"].as_str().unwrap_or("?"),
                v["chain"]["sequence"]
            );
        }
    }
    Ok(())
}

fn short(id: &str) -> String {
    if id.chars().count() > 12 {
        format!("{}...", id.chars().take(8).collect::<String>())
    } else {
        id.to_owned()
    }
}

/// First 12 hex characters of a MeshId, `-` for none; the full id is under the table.
fn short_mesh_id(id: &str) -> String {
    if id.chars().count() > 12 { format!("{}...", id.chars().take(12).collect::<String>()) } else { id.to_owned() }
}

fn age(now: u64, then: u64) -> String {
    let d = now.saturating_sub(then);
    match d {
        0..=59 => format!("{d}s ago"),
        60..=3599 => format!("{}m ago", d / 60),
        3600..=86_399 => format!("{}h ago", d / 3600),
        _ => format!("{}d ago", d / 86_400),
    }
}

fn val<'a>(n: &'a Value, section: &str) -> &'a Value {
    &n[section]["value"]
}

/// `0.42 /4 cores`, marked `(peer)` when the peer reported it about itself.
fn load_cell(l: &Value, provenance: Option<&str>) -> String {
    let Some(l1) = l["load1"].as_f64() else { return "-".to_owned() };
    let cores = l["cores"].as_u64().map(|c| format!(" /{c} cores")).unwrap_or_default();
    let who = if provenance == Some("peer_claimed") { " (peer)" } else { "" };
    format!("{l1:.2}{cores}{who}")
}

fn cell(v: &Value) -> String {
    v.as_str().map_or_else(|| "-".to_owned(), str::to_owned)
}

/// The snapshot as a table plus the daemon-wide lines. `now` is unix seconds.
pub fn render(snap: &Value, now: u64) -> String {
    let mut table = Table::new();
    table.load_preset(presets::UTF8_FULL_CONDENSED);
    table.set_header(vec![
        "Node", "State", "Trust", "Address", "Last announce", "Mesh", "Mesh ID", "Load", "Cogs", "Location",
    ]);
    for n in snap["nodes"].as_array().into_iter().flatten() {
        let id = n["node_id"].as_str().unwrap_or("?");
        // A node whose announced name is only its id is named by the hostname the daemon reports.
        let name = n["name"]["value"]
            .as_str()
            .filter(|s| *s != id)
            .or_else(|| n["host"]["value"]["hostname"].as_str())
            .map_or_else(|| short(id), str::to_owned);
        let local = if n["local"] == true { " (this node)" } else { "" };
        let facts = val(n, "facts");
        let trust = facts["trust_tier"].as_str().map_or_else(
            || "-".to_owned(),
            |t| match facts["tier_source"].as_str() {
                Some(s) => format!("{t} ({s})"),
                None => t.to_owned(),
            },
        );
        let seen = val(n, "cluster")["last_announce_unix"]
            .as_u64()
            .map_or_else(|| "-".to_owned(), |t| age(now, t));
        let mesh = match val(n, "mesh") {
            m if m.is_object() => format!(
                "{}{} {}{}",
                m["class"].as_str().unwrap_or("?"),
                if m["verified"] == true { "" } else { " unverified" },
                m["heartbeat"].as_str().unwrap_or(""),
                match (m["rtt_ms"].as_f64(), m["last_seen_unix"].as_u64()) {
                    (Some(r), Some(t)) => format!(" {r:.0}ms, pong {}", age(now, t)),
                    _ => String::new(),
                }
            )
            .trim()
            .to_owned(),
            _ => "-".to_owned(),
        };
        let mesh_id = n["mesh_id"]["value"].as_str().map_or_else(|| "-".to_owned(), short_mesh_id);
        let load = load_cell(val(n, "load"), n["load"]["provenance"].as_str());
        let cogs = val(n, "instances").as_array().map_or(0, Vec::len);
        let loc = match val(n, "location") {
            l if l.is_object() => format!("{} / {}", cell(&l["site"]), cell(&l["room"])),
            _ => "-".to_owned(),
        };
        let revoked = if val(n, "revoked").is_object() { " REVOKED" } else { "" };
        table.add_row(vec![
            format!("{name}{local}{revoked}"),
            cell(&val(n, "cluster")["state"]),
            trust,
            cell(&val(n, "announced")["address"]),
            seen,
            mesh,
            mesh_id,
            load,
            cogs.to_string(),
            loc,
        ]);
    }
    let mut out = format!("{table}\n");
    if let Some(m) = snap["licence"]["value"]["mesh_id"].as_str() {
        out.push_str(&format!("mesh id: {m}\n"));
    }
    let revoked = snap["revocations"]["value"].as_array().map_or(0, Vec::len);
    if revoked > 0 {
        out.push_str(&format!("revoked hosts: {revoked}\n"));
    }
    out.push_str(
        "name and address are announced by the peer; location is operator-set; trust and last announce are observed by this daemon.\n",
    );
    for d in snap["degraded"].as_array().into_iter().flatten().filter_map(Value::as_str) {
        out.push_str(&format!("not available: {d}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn snap() -> Value {
        json!({
            "nodes": [
                { "node_id": "aaaaaaaaaaaaaaaaaaaa", "local": true, "name": { "value": "aaaaaaaaaaaaaaaaaaaa" },
                  "mesh_id": { "value": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef" },
                  "host": { "value": { "hostname": "mac" } },
                  "announced": { "value": { "address": "10.0.0.2:9" } },
                  "cluster": { "value": { "state": "active", "last_announce_unix": 990 } },
                  "facts": { "value": { "trust_tier": "pinned", "tier_source": "operator" } },
                  "instances": { "value": [{}, {}] },
                  "location": { "value": { "site": "Lab", "room": "R1" } } },
                { "node_id": "cccccccccccccccccccc",
                  "mesh": { "value": { "class": "node", "verified": true, "heartbeat": "alive",
                                       "rtt_ms": 4.2, "last_seen_unix": 995 } },
                  "load": { "value": { "load1": 0.42, "load5": 0.3, "cores": 4 }, "provenance": "peer_claimed" } },
                { "node_id": "bbbbbbbbbbbbbbbbbbbb",
                  "cluster": { "value": { "state": "suspect", "last_announce_unix": 100 } },
                  "mesh": { "value": { "class": "leaf", "verified": false, "heartbeat": "suspect" } },
                  "revoked": { "value": { "reason": "x" } } },
            ],
            "licence": { "value": { "mesh_id": "mesh-1" } },
            "revocations": { "value": [{}] },
            "degraded": ["placement: placement control plane not started"],
        })
    }

    #[test]
    fn status_table_shows_trust_age_cogs_location_and_gaps() {
        let out = render(&snap(), 1000);
        assert!(out.contains("mac (this node)"), "{out}");
        assert!(out.contains("pinned (operator)"), "{out}");
        assert!(out.contains("10s ago") && out.contains("15m ago"), "{out}");
        assert!(out.contains("Lab / R1") && out.contains("10.0.0.2:9"), "{out}");
        assert!(out.contains("bbbbbbbb... REVOKED"), "{out}");
        assert!(out.contains("leaf unverified suspect"), "{out}");
        assert!(out.contains("node alive 4ms, pong 5s ago"), "{out}");
        assert!(out.contains("0.42 /4 cores (peer)"), "{out}");
        assert!(out.contains("0123456789ab..."), "this node's MeshId is in its row: {out}");
        assert!(out.contains("mesh id: mesh-1") && out.contains("revoked hosts: 1"), "{out}");
        assert!(out.contains("not available: placement:"), "{out}");
    }

    #[test]
    fn an_empty_snapshot_still_renders() {
        let out = render(&json!({}), 1);
        assert!(out.contains("Node"), "{out}");
    }

    #[test]
    fn the_verbs_parse() {
        let cmd = FleetArgs::command();
        cmd.clone()
            .try_get_matches_from(["fleet", "status", "--json"])
            .unwrap();
        cmd.clone()
            .try_get_matches_from(["fleet", "location", "set", "n1", "--site", "Lab", "--room", "R1"])
            .unwrap();
        assert!(
            cmd.try_get_matches_from(["fleet", "location", "set", "n1", "--site", "Lab"]).is_err(),
            "room is required"
        );
    }
}
