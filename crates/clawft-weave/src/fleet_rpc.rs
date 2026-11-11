//! `fleet.snapshot` and `fleet.location.set` (fleet manager P1).
//!
//! `fleet.snapshot` (Read) is the one read-only document a fleet console
//! needs: every node this daemon knows, with what is known about it and where
//! each fact came from. It composes sources the daemon already holds and
//! contacts no peer and no device (that is why it does not call
//! `workload.status`, a Write verb because it does):
//!
//! | section | source | provenance |
//! |---|---|---|
//! | `name` | the peer's own announced name | `peer_claimed` |
//! | `cluster` | state, first seen and `last_announce` (when a verified join, recovery or announce last arrived; per-pong liveness is `mesh.last_seen`) | `daemon_observed` |
//! | `announced` | platform and address the peer announced (an unverified peer chooses them) | `peer_claimed` |
//! | `facts` | signed node facts cache (`cluster.facts`) | `signed_fact` |
//! | `mesh` | live connection detail (class, verified, heartbeat, last pong, smoothed RTT, missed pongs) | `daemon_observed` |
//! | `mesh_id` | this daemon's own MeshId (local node only; no peer wire carries one) | `daemon_observed` |
//! | `host` | this daemon's own hostname (local node only) | `daemon_observed` |
//! | `load` | load average, cores, memory: this daemon's own, or what a verified peer attached to its last pong | `daemon_observed` (own) / `peer_claimed` (peer) |
//! | `revoked` | host revocation list (`mesh.revoked`) | `daemon_observed` |
//! | `instances` | placement controller's own records and lifecycle (no error text) | `daemon_observed` |
//! | `location` | operator labels (`fleet.location.set`) | `operator_claimed` |
//! | `infer`, `licence`, `placement`, `revocations` | `infer.status`, binding summary (state, mesh id, seq, fingerprint; the signed record itself is withheld), controller targets, revocation list | `daemon_observed`; the licence `binding` is `signed_fact`; `targets` are `operator_claimed` (the tier is the operator's) |
//!
//! Read is machine-wide. A caller whose token is scoped to a project sees only
//! that project's instances; any other caller sees all of them.
//!
//! `self_reported` is the fifth label: heartbeat fields an edge node says
//! about itself (the cog-host roster). The daemon holds none today.
//!
//! `mesh.last_seen` and `mesh.rtt_ms` come from the liveness ping/pong between
//! verified peers (`mesh.ping`); they are `null` until a pong has been counted
//! (an unverified peer is never pinged), never a zero that would read as a
//! perfect link.
//!
//! `fleet.location.set` (Admin) records `{node, site, room}` on the chain and
//! saves it ([`crate::fleet_labels`]).

use std::collections::BTreeMap;

use clawft_kernel::boot::Kernel;
use clawft_platform::NativePlatform;
use clawft_rpc::Response;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::fleet_labels::{self, Location};
use crate::rpc_ext::{ExtCall, ExtFuture, KernelRef};

/// Snapshot document version.
pub const SCHEMA: u32 = 1;
/// How long a console may reuse a snapshot, seconds.
pub const TTL_SECS: u64 = 5;

/// Where a field came from.
pub mod provenance {
    /// Signed by the node it describes, verified by this daemon.
    pub const SIGNED_FACT: &str = "signed_fact";
    /// Observed or held by this daemon.
    pub const DAEMON_OBSERVED: &str = "daemon_observed";
    /// Announced by the peer itself over the mesh, not authenticated.
    pub const PEER_CLAIMED: &str = "peer_claimed";
    /// Set by an operator (location labels, target tiers).
    pub const OPERATOR_CLAIMED: &str = "operator_claimed";
    /// Said by an unauthenticated edge node about itself; display only.
    pub const SELF_REPORTED: &str = "self_reported";
}
use provenance::{DAEMON_OBSERVED, OPERATOR_CLAIMED, PEER_CLAIMED, SIGNED_FACT};

/// Route handler for both methods.
pub fn handle(call: ExtCall) -> ExtFuture {
    Box::pin(async move {
        match call.method.as_str() {
            "fleet.snapshot" => {
                let project = call.ctx.verified_project.as_ref().map(|p| p.as_str().to_owned());
                Response::success(snapshot(&call.ctx.kernel, project.as_deref()).await)
            }
            "fleet.location.set" => location_set(call.params, &call.ctx.kernel).await,
            other => Response::error(format!("unknown method {other}")),
        }
    })
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn field(value: Value, provenance: &str) -> Value {
    json!({ "value": value, "provenance": provenance })
}

/// A daemon-wide section that may be unavailable (with the reason).
fn section(value: Option<Value>, why: &str, provenance: &str, degraded: &mut Vec<String>, name: &str) -> Value {
    match value {
        Some(v) => field(v, provenance),
        None => {
            degraded.push(format!("{name}: {why}"));
            json!({ "value": null, "provenance": provenance, "unavailable": why })
        }
    }
}

/// What one pass over the kernel collects (everything synchronous).
pub(crate) struct Raw {
    pub local_id: String,
    pub peers: Vec<clawft_kernel::PeerNode>,
    pub facts: Vec<crate::node_facts_rpc::FactsEntry>,
    /// `None` when the mesh runtime is not in this process (service mode).
    pub mesh: Option<Vec<MeshPeer>>,
    pub revoked: Vec<clawft_kernel::revocation::RevokedHost>,
    /// This daemon's own load sample (`LoadSample` JSON), when the OS gives one.
    pub local_load: Option<Value>,
    /// This machine's hostname, so a console can match the daemon to the same host seen by
    /// other sources (tailnet, Seed agent).
    pub local_hostname: Option<String>,
}

/// One live mesh connection.
pub(crate) struct MeshPeer {
    pub node_id: String,
    pub class: &'static str,
    pub verified: bool,
    pub licensed: bool,
    pub connected_at: chrono::DateTime<chrono::Utc>,
    /// `alive`, `suspect` or `dead`; `None` when not tracked.
    pub heartbeat: Option<&'static str>,
    /// Last counted liveness pong; `None` until one arrives.
    pub last_seen: Option<chrono::DateTime<chrono::Utc>>,
    /// Smoothed round-trip time, milliseconds; `None` until measured.
    pub rtt_ms: Option<f64>,
    /// Pings that timed out since the last counted pong.
    pub missed_pongs: Option<u32>,
    /// The load the peer attached to its last pong (`LoadSample` JSON).
    pub load: Option<Value>,
}

#[cfg(feature = "mesh")]
fn mesh_peers(k: &Kernel<NativePlatform>) -> Option<Vec<MeshPeer>> {
    use clawft_kernel::HeartbeatState::{Alive, Dead, Suspect};
    let rt = k.a2a_router().mesh_runtime()?;
    Some(
        rt.peer_details()
            .into_iter()
            .map(|d| MeshPeer {
                node_id: d.node_id,
                class: d.class.as_str(),
                verified: d.verified,
                licensed: d.licensed,
                connected_at: d.connected_at,
                heartbeat: match d.heartbeat {
                    Some(Alive) => Some("alive"),
                    Some(Suspect) => Some("suspect"),
                    Some(Dead) => Some("dead"),
                    _ => None,
                },
                last_seen: d.last_seen.map(chrono::DateTime::<chrono::Utc>::from),
                rtt_ms: d.rtt_ms,
                missed_pongs: d.missed_pongs,
                load: d.load.map(|l| l.to_json()),
            })
            .collect(),
    )
}
#[cfg(not(feature = "mesh"))]
fn mesh_peers(_: &Kernel<NativePlatform>) -> Option<Vec<MeshPeer>> {
    None
}

pub(crate) fn collect(k: &Kernel<NativePlatform>) -> Raw {
    let membership = k.cluster_membership();
    let peers = membership
        .list_peers()
        .iter()
        .filter_map(|(id, _, _)| membership.get_peer(id))
        .collect();
    Raw {
        local_id: membership.local_node_id().to_owned(),
        peers,
        facts: crate::node_facts_rpc::facts_entries(membership, None),
        mesh: mesh_peers(k),
        revoked: k.revocation_list().list_revoked(),
        local_load: local_load(),
        local_hostname: local_hostname(),
    }
}

/// This machine's name, read once per daemon run. On macOS `hostname` is often the DHCP name
/// (`Mac.example.net`), so the machine's own `LocalHostName` is used there (the name tailscale
/// and Bonjour show); elsewhere `hostname`.
fn local_hostname() -> Option<String> {
    static HOST: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    HOST.get_or_init(|| {
        let run = |cmd: &str, args: &[&str]| -> Option<String> {
            let out = std::process::Command::new(cmd).args(args).output().ok()?;
            let h = String::from_utf8(out.stdout).ok()?.trim().to_owned();
            (out.status.success() && !h.is_empty() && h.len() <= 253).then_some(h)
        };
        let mac = if cfg!(target_os = "macos") { run("scutil", &["--get", "LocalHostName"]) } else { None };
        mac.or_else(|| run("hostname", &[]))
    })
    .clone()
}

#[cfg(feature = "mesh")]
fn local_load() -> Option<Value> {
    clawft_kernel::mesh_load::LoadSample::local().map(|l| l.to_json())
}
#[cfg(not(feature = "mesh"))]
fn local_load() -> Option<Value> {
    None
}

/// Daemon-wide inputs besides [`Raw`].
#[derive(Default)]
pub(crate) struct Extra {
    pub controller: Option<Value>,
    pub infer: Option<Value>,
    pub infer_why: String,
    pub licence: Option<Value>,
    pub labels: Option<Result<BTreeMap<String, Location>, String>>,
}

type Nodes = BTreeMap<String, Map<String, Value>>;

fn touch<'a>(nodes: &'a mut Nodes, id: &str) -> &'a mut Map<String, Value> {
    nodes.entry(id.to_owned()).or_insert_with(|| {
        let mut m = Map::new();
        m.insert("node_id".into(), json!(id));
        m
    })
}

fn rfc3339(t: chrono::DateTime<chrono::Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Build the snapshot document from collected inputs.
pub(crate) fn assemble(raw: Raw, extra: Extra, now: u64) -> Value {
    let mut degraded: Vec<String> = Vec::new();
    let mut nodes: Nodes = BTreeMap::new();
    touch(&mut nodes, &raw.local_id).insert("local".into(), json!(true));
    if let Some(h) = &raw.local_hostname {
        touch(&mut nodes, &raw.local_id).insert("host".into(), field(json!({ "hostname": h }), DAEMON_OBSERVED));
    }
    if let Some(l) = &raw.local_load {
        touch(&mut nodes, &raw.local_id).insert("load".into(), field(l.clone(), DAEMON_OBSERVED));
    }
    for d in raw.mesh.iter().flatten() {
        if let Some(l) = &d.load {
            // Sent by the peer over its verified connection; not signed.
            touch(&mut nodes, &d.node_id).insert("load".into(), field(l.clone(), PEER_CLAIMED));
        }
    }
    for p in &raw.peers {
        let n = touch(&mut nodes, &p.id);
        n.insert("name".into(), field(json!(p.name), PEER_CLAIMED));
        n.insert(
            "announced".into(),
            field(json!({ "platform": p.platform.to_string(), "address": p.address }), PEER_CLAIMED),
        );
        n.insert(
            "cluster".into(),
            field(
                json!({
                    "state": p.state.to_string(),
                    "first_seen": rfc3339(p.first_seen),
                    "last_announce": rfc3339(p.last_heartbeat),
                    "last_announce_unix": p.last_heartbeat.timestamp(),
                }),
                DAEMON_OBSERVED,
            ),
        );
    }
    for f in &raw.facts {
        touch(&mut nodes, &f.node_id).insert(
            "facts".into(),
            field(serde_json::to_value(f).unwrap_or(Value::Null), SIGNED_FACT),
        );
    }
    match &raw.mesh {
        Some(details) => {
            for d in details {
                touch(&mut nodes, &d.node_id).insert(
                    "mesh".into(),
                    field(
                        json!({
                            "connected": true,
                            "class": d.class,
                            "verified": d.verified,
                            "licensed": d.licensed,
                            "connected_at": rfc3339(d.connected_at),
                            "heartbeat": d.heartbeat,
                            "last_seen": d.last_seen.map(rfc3339),
                            "last_seen_unix": d.last_seen.map(|t| t.timestamp()),
                            "rtt_ms": d.rtt_ms.map(|r| (r * 10.0).round() / 10.0),
                            "missed_pongs": d.missed_pongs,
                        }),
                        DAEMON_OBSERVED,
                    ),
                );
            }
        }
        None => degraded.push("mesh: runtime is not in this process, peer connection detail unavailable".into()),
    }
    for r in &raw.revoked {
        touch(&mut nodes, &r.host_id).insert(
            "revoked".into(),
            field(json!({ "revoked_at": r.revoked_at, "reason": r.reason }), DAEMON_OBSERVED),
        );
    }

    let mut by_node: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    if let Some(c) = &extra.controller {
        for row in c["instances"].as_array().into_iter().flatten() {
            if let Some(node) = row["placement"]["node_id"].as_str() {
                by_node.entry(node.to_owned()).or_default().push(row.clone());
            }
        }
    }
    for (node, rows) in by_node {
        touch(&mut nodes, &node).insert("instances".into(), field(Value::Array(rows), DAEMON_OBSERVED));
    }

    match &extra.labels {
        Some(Ok(labels)) => {
            for (node, loc) in labels {
                // A label for an id nothing else here knows (an edge node that
                // only checks in to a cog host, or a typo) is flagged.
                let unknown = !nodes.contains_key(node);
                let n = touch(&mut nodes, node);
                if unknown {
                    n.insert("unknown_node".into(), json!(true));
                }
                n.insert(
                    "location".into(),
                    field(serde_json::to_value(loc).unwrap_or(Value::Null), OPERATOR_CLAIMED),
                );
            }
        }
        Some(Err(why)) => degraded.push(format!("location: {why}")),
        None => degraded.push("location: runtime directory not initialised".into()),
    }

    let controller = section(
        extra.controller.as_ref().map(|c| {
            json!({
                "controller": c["controller"],
                "unsettled": c["unsettled"],
                // The tier of each target is the operator's decision; `reachable` is observed.
                "targets": field(c["targets"].clone(), OPERATOR_CLAIMED),
            })
        }),
        "placement control plane not started",
        DAEMON_OBSERVED,
        &mut degraded,
        "placement",
    );
    let infer = section(extra.infer, &extra.infer_why, DAEMON_OBSERVED, &mut degraded, "infer");
    // Only this daemon's own MeshId is known here: a peer's is not carried on any wire it sends,
    // so no other node gets one (a peer's membership of this mesh is not a MeshId observation).
    if let Some(id) = extra.licence.as_ref().and_then(|l| l["mesh_id"].as_str()) {
        touch(&mut nodes, &raw.local_id).insert("mesh_id".into(), field(json!(id), DAEMON_OBSERVED));
    }
    let licence = section(extra.licence, "licence runtime not started", DAEMON_OBSERVED, &mut degraded, "licence");
    let revocations = field(
        json!(raw.revoked.iter().map(|r| json!({
            "host_id": r.host_id, "revoked_at": r.revoked_at, "reason": r.reason
        })).collect::<Vec<_>>()),
        DAEMON_OBSERVED,
    );

    let mut list: Vec<Value> = nodes.into_values().map(Value::Object).collect();
    list.sort_by_key(|n| {
        let local = n["node_id"] != json!(raw.local_id);
        let name = n["name"]["value"].as_str().unwrap_or_else(|| n["node_id"].as_str().unwrap_or("")).to_owned();
        (local, name)
    });
    json!({
        "schema": SCHEMA,
        "source": "daemon",
        "fetched_at": now,
        "ttl_secs": TTL_SECS,
        "local_node_id": raw.local_id,
        "degraded": degraded,
        "nodes": list,
        "placement": controller,
        "infer": infer,
        "licence": licence,
        "revocations": revocations,
    })
}

/// The `fleet.snapshot` document. Read-only; contacts no peer.
///
/// `project` is the caller's verified project, if its token is scoped to one:
/// such a caller sees only that project's placed instances.
pub async fn snapshot(kernel: &KernelRef, project: Option<&str>) -> Value {
    let raw = {
        let k = kernel.read().await;
        collect(&k)
    };
    let (infer, infer_why) = infer_status().await;
    let labels = match fleet_labels::dir() {
        Some(d) => {
            let d = d.to_path_buf();
            Some(
                tokio::task::spawn_blocking(move || fleet_labels::load(&d))
                    .await
                    .unwrap_or_else(|_| Err("label read failed".into())),
            )
        }
        None => None,
    };
    let mut controller = controller_view();
    if let Some(c) = controller.as_mut() {
        filter_to_project(c, project);
    }
    let extra = Extra { controller, infer, infer_why, licence: licence_status(), labels };
    assemble(raw, extra, now_secs())
}

/// For a project-scoped caller keep only that project's instances (an
/// instance with no project belongs to the machine, so it is dropped too).
/// `None` (a machine-level caller) keeps everything.
pub(crate) fn filter_to_project(controller: &mut Value, project: Option<&str>) {
    let Some(project) = project else { return };
    if let Some(rows) = controller["instances"].as_array_mut() {
        rows.retain(|r| r["placement"]["project_id"].as_str() == Some(project));
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LocationParams {
    node: String,
    site: String,
    room: String,
}

/// `fleet.location.set`: validate, require a chain, save, then chain-record.
async fn location_set(params: Value, kernel: &KernelRef) -> Response {
    let Some(dir) = fleet_labels::dir() else {
        return Response::error("fleet labels unavailable: runtime directory not initialised");
    };
    set_location(dir, params, kernel).await
}

pub(crate) async fn set_location(dir: &std::path::Path, params: Value, kernel: &KernelRef) -> Response {
    let p: LocationParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => return Response::error(format!("invalid params: {e}")),
    };
    if let Err(e) = fleet_labels::validate(&p.node, &p.site, &p.room) {
        return Response::error(e);
    }
    #[cfg(feature = "exochain")]
    {
        let chain = kernel.read().await.chain_manager().cloned();
        let Some(chain) = chain else {
            return Response::error("a location label is recorded on the chain, and this kernel has none");
        };
        let (node, site, room) = (p.node.clone(), p.site.trim().to_owned(), p.room.trim().to_owned());
        let d = dir.to_path_buf();
        // One critical section: read `previous`, record on the chain FIRST (a
        // label that was set is always on the chain), then save. If saving
        // fails the event stands and the caller is told to retry.
        let done = tokio::task::spawn_blocking({
            let (node, site, room) = (node.clone(), site.clone(), room.clone());
            move || -> Result<Value, String> {
                let _g = fleet_labels::lock();
                let previous = fleet_labels::load(&d)?.get(&node).cloned();
                let ev = chain.append(
                    fleet_labels::CHAIN_SOURCE,
                    fleet_labels::CHAIN_KIND,
                    Some(json!({ "node": node, "site": site, "room": room, "previous": previous })),
                );
                let hash: String = ev.hash.iter().map(|b| format!("{b:02x}")).collect();
                match fleet_labels::set_locked(&d, &node, &site, &room, now_secs()) {
                    Ok((_, loc)) => Ok(json!({
                        "node": node, "site": loc.site, "room": loc.room, "set_at": loc.set_at,
                        "previous": previous, "chain": { "sequence": ev.sequence, "hash": hash },
                    })),
                    Err(e) => Err(format!(
                        "recorded on the chain (event {}) but not saved: {e}; run the command again",
                        ev.sequence
                    )),
                }
            }
        })
        .await;
        match done {
            Ok(Ok(v)) => Response::success(v),
            Ok(Err(e)) => Response::error(e),
            Err(_) => Response::error("label task failed; run the command again"),
        }
    }
    #[cfg(not(feature = "exochain"))]
    {
        let _ = (dir, kernel);
        Response::error("a location label is recorded on the chain, and this build has none")
    }
}

#[path = "fleet_rpc_sources.rs"]
mod sources;
#[cfg(test)]
use sources::licence_summary;
use sources::{controller_view, infer_status, licence_status};

#[cfg(test)]
#[path = "fleet_rpc_tests.rs"]
mod tests;
