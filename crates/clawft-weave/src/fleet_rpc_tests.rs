//! Tests for [`crate::fleet_rpc`] and the `cluster.nodes` last-seen fix.

use super::*;
use crate::capability::{CallerCapabilities, Capability, required_capability};
use crate::node_facts_rpc::FactsEntry;
use clawft_kernel::node_facts::TierSource;
use clawft_kernel::node_facts_advert::sign_node_facts;
use clawft_kernel::{NodePlatform, NodeState, PeerNode};
use clawft_types::config::{ChainConfig, Config, GovernanceConfig, KernelConfig, OutsideProjectPolicy};
use clawft_types::placement::{Capability as FactCap, CapabilityId, NodeFacts, Provenance, TrustTier};
use ed25519_dalek::SigningKey;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::RwLock;

fn peer(id: &str, name: &str, heartbeat_unix: i64) -> PeerNode {
    let t = chrono::DateTime::from_timestamp(heartbeat_unix, 0).unwrap();
    PeerNode {
        id: id.into(),
        name: name.into(),
        platform: NodePlatform::Edge,
        state: NodeState::Active,
        address: Some("192.168.1.9:9000".into()),
        first_seen: t,
        last_heartbeat: t,
        capabilities: Vec::new(),
        labels: Default::default(),
    }
}

fn facts_entry(local: bool) -> FactsEntry {
    let key = SigningKey::from_bytes(&[9u8; 32]);
    let id = clawft_kernel::node_id_from_pubkey(&key.verifying_key().to_bytes());
    let mut f = NodeFacts::new(id.clone(), 1_000, 600, 1);
    f.capabilities = vec![
        FactCap::new(CapabilityId::new("mem.system").unwrap(), Provenance::Probed).with_attr("total", 8_000_000_000i64),
    ];
    FactsEntry {
        node_id: id,
        local,
        trust_tier: TrustTier::Paired,
        tier_source: Some(TierSource::Operator),
        received_at: 1_000,
        expires_at: 1_600,
        delta_seq: 0,
        signed: sign_node_facts(&f, &key).unwrap(),
        facts: f,
    }
}

fn raw() -> Raw {
    let facts = facts_entry(false);
    let facts_id = facts.node_id.clone();
    Raw {
        local_id: "local-node".into(),
        peers: vec![peer("zeta", "zeta-box", 1_700_000_000), peer(&facts_id, "alpha-box", 1_700_000_100)],
        facts: vec![facts],
        mesh: Some(vec![MeshPeer {
            node_id: "zeta".into(),
            class: "leaf",
            verified: true,
            licensed: false,
            connected_at: chrono::DateTime::from_timestamp(1_700_000_050, 0).unwrap(),
            heartbeat: Some("alive"),
            last_seen: None,
            rtt_ms: None,
            missed_pongs: None,
            load: None,
        }]),
        revoked: vec![clawft_kernel::revocation::RevokedHost {
            host_id: "bad-node".into(),
            revoked_at: 5,
            reason: "leaked key".into(),
        }],
        local_load: None,
        local_hostname: Some("studio-mac.local".into()),
    }
}

fn controller() -> Value {
    json!({
        "controller": "local-node",
        "targets": [{ "node_id": "zeta", "tier": "paired", "reachable": true }],
        "unsettled": 0,
        "instances": [
            { "placement": { "instance_id": "i1", "node_id": "zeta", "workload": "cog-a", "decision_id": "d1", "project_id": "P1" },
              "lifecycle": { "state": "running", "restarts": 2, "reschedules": 0, "has_error": false } },
            { "placement": { "instance_id": "i2", "node_id": "local-node", "workload": "cog-b", "decision_id": "d2" },
              "lifecycle": null },
        ],
    })
}

fn node<'a>(snap: &'a Value, id: &str) -> &'a Value {
    snap["nodes"].as_array().unwrap().iter().find(|n| n["node_id"] == id).unwrap_or_else(|| panic!("no node {id}"))
}

#[test]
fn snapshot_merges_every_source_and_labels_each_field() {
    let mut labels = BTreeMap::new();
    labels.insert("zeta".to_string(), Location { site: "Lab".into(), room: "R2".into(), set_at: 77 });
    let extra = Extra {
        controller: Some(controller()),
        infer: Some(json!({ "roles": [] })),
        licence: Some(json!({ "mesh_id": "m1" })),
        labels: Some(Ok(labels)),
        ..Extra::default()
    };
    let snap = assemble(raw(), extra, 1234);

    assert_eq!((snap["schema"].as_u64(), snap["fetched_at"].as_u64()), (Some(1), Some(1234)));
    assert_eq!(snap["source"], "daemon");
    assert_eq!(snap["degraded"], json!([]));
    // The local node leads even though no peer row names it.
    assert_eq!(snap["nodes"][0]["node_id"], "local-node");
    assert_eq!(snap["nodes"][0]["local"], true);
    assert_eq!(snap["nodes"][0]["instances"]["value"][0]["placement"]["workload"], "cog-b");

    let z = node(&snap, "zeta");
    assert_eq!(z["cluster"]["provenance"], "daemon_observed");
    assert_eq!(z["cluster"]["value"]["last_announce"], "2023-11-14T22:13:20Z");
    assert_eq!(z["cluster"]["value"]["last_announce_unix"], 1_700_000_000);
    assert_eq!(z["mesh"]["provenance"], "daemon_observed");
    assert_eq!(z["mesh"]["value"]["class"], "leaf");
    assert_eq!(z["mesh"]["value"]["heartbeat"], "alive");
    assert_eq!(z["location"]["provenance"], "operator_claimed");
    assert_eq!(z["location"]["value"]["room"], "R2");
    assert_eq!(z["instances"]["value"][0]["lifecycle"]["restarts"], 2);
    assert_eq!(z["instances"]["provenance"], "daemon_observed");

    let alpha = snap["nodes"].as_array().unwrap().iter().find(|n| n["name"]["value"] == "alpha-box").unwrap();
    assert_eq!(alpha["facts"]["provenance"], "signed_fact");
    assert_eq!(alpha["facts"]["value"]["trust_tier"], "paired");
    assert!(alpha["facts"]["value"]["signed"].is_object(), "the envelope travels so a client can re-verify");

    let bad = node(&snap, "bad-node");
    assert_eq!(bad["revoked"]["value"]["reason"], "leaked key");
    assert_eq!(snap["revocations"]["value"][0]["host_id"], "bad-node");
    assert_eq!(snap["licence"]["provenance"], "daemon_observed");
    assert_eq!(snap["infer"]["provenance"], "daemon_observed");
    assert_eq!(snap["placement"]["value"]["controller"], "local-node");
    assert!(z["location"]["value"].get("unknown_node").is_none());
    // Every node-level section is a {value, provenance} pair.
    for n in snap["nodes"].as_array().unwrap() {
        for key in ["cluster", "facts", "mesh", "revoked", "instances", "location"] {
            if let Some(f) = n.get(key) {
                assert!(f["provenance"].is_string() && f.get("value").is_some(), "{key} on {}", n["node_id"]);
            }
        }
    }
}

#[test]
fn every_field_is_labelled_with_who_vouches_for_it() {
    let extra = Extra {
        controller: Some(controller()),
        licence: Some(licence_summary(&json!({
            "mesh_id": "m1", "genesis_pinned": true,
            "binding": { "state": "bound", "mesh_id": "m1", "seq": 3, "grant_fingerprint": "ab12",
                         "orphaned": false, "device_id": "dev", "grant_pubkey": "k", "record": { "sig": "s" } },
        }))),
        ..Extra::default()
    };
    let snap = assemble(raw(), extra, 1);
    let z = node(&snap, "zeta");
    // Peer-supplied: the name and the announced platform/address.
    assert_eq!(z["name"]["provenance"], "peer_claimed");
    assert_eq!(z["announced"]["provenance"], "peer_claimed");
    assert_eq!(z["announced"]["value"]["address"], "192.168.1.9:9000");
    assert!(z["cluster"]["value"].get("address").is_none(), "no announced field under the observed section");
    assert!(z["cluster"]["value"].get("platform").is_none());
    assert_eq!(z["cluster"]["provenance"], "daemon_observed");
    // Operator-decided: target tiers.
    assert_eq!(snap["placement"]["value"]["targets"]["provenance"], "operator_claimed");
    // Licence: observed section, signed binding.
    assert_eq!(snap["licence"]["provenance"], "daemon_observed");
    assert_eq!(snap["licence"]["value"]["binding"]["provenance"], "signed_fact");
    assert_eq!(snap["licence"]["value"]["binding"]["value"]["grant_fingerprint"], "ab12");
}

#[test]
fn the_licence_summary_withholds_the_signed_record_and_keys() {
    let l = licence_summary(&json!({
        "mesh_id": "m1",
        "binding": { "state": "bound", "mesh_id": "m1", "seq": 3, "grant_fingerprint": "ab12", "orphaned": false,
                     "device_id": "dev-secret", "grant_pubkey": "pk-secret", "steward_node_id": "st", "record": { "sig": "sig-secret" } },
        "steward": { "pubkey": "steward-secret" },
    }));
    let text = l.to_string();
    for secret in ["dev-secret", "pk-secret", "sig-secret", "steward-secret", "\"st\""] {
        assert!(!text.contains(secret), "{secret} leaked: {text}");
    }
    assert_eq!(licence_summary(&json!({ "mesh_id": "m1", "binding": null }))["binding"]["value"], Value::Null);
}

#[test]
fn a_project_scoped_caller_sees_only_its_own_instances() {
    let mut c = controller();
    filter_to_project(&mut c, None);
    assert_eq!(c["instances"].as_array().unwrap().len(), 2, "machine-level caller sees all");
    filter_to_project(&mut c, Some("P2"));
    assert!(c["instances"].as_array().unwrap().is_empty());
    let mut c = controller();
    filter_to_project(&mut c, Some("P1"));
    let rows = c["instances"].as_array().unwrap();
    assert_eq!((rows.len(), rows[0]["placement"]["instance_id"].as_str()), (1, Some("i1")));
}

#[test]
fn a_label_for_an_unknown_id_is_flagged() {
    let mut labels = BTreeMap::new();
    labels.insert("c6-01".to_string(), Location { site: "Lab".into(), room: "R".into(), set_at: 1 });
    labels.insert("zeta".to_string(), Location { site: "Lab".into(), room: "R".into(), set_at: 1 });
    let snap = assemble(raw(), Extra { labels: Some(Ok(labels)), ..Extra::default() }, 1);
    assert_eq!(node(&snap, "c6-01")["unknown_node"], true);
    assert!(node(&snap, "zeta").get("unknown_node").is_none());
}

#[test]
fn rtt_and_last_seen_are_null_until_measured_never_a_zero() {
    let snap = assemble(raw(), Extra::default(), 1);
    let mesh = &node(&snap, "zeta")["mesh"]["value"];
    assert!(mesh["rtt_ms"].is_null());
    assert!(mesh["last_seen"].is_null());

    let mut r = raw();
    let m = &mut r.mesh.as_mut().unwrap()[0];
    m.rtt_ms = Some(12.345);
    m.last_seen = chrono::DateTime::from_timestamp(1_700_000_200, 0);
    let snap = assemble(r, Extra::default(), 1);
    let mesh = &node(&snap, "zeta")["mesh"]["value"];
    assert_eq!(mesh["rtt_ms"], 12.3);
    assert_eq!(mesh["last_seen_unix"], 1_700_000_200);
}

#[test]
fn missing_sources_are_reported_not_hidden() {
    let mut r = raw();
    r.mesh = None;
    let extra = Extra { labels: Some(Err("fleet-locations.json is not valid JSON".into())), ..Extra::default() };
    let snap = assemble(r, extra, 1);
    let d: Vec<&str> = snap["degraded"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
    for want in ["mesh:", "location: fleet-locations.json", "placement:", "infer:", "licence:"] {
        assert!(d.iter().any(|s| s.starts_with(want)), "{want} in {d:?}");
    }
    assert!(snap["placement"]["value"].is_null());
    assert_eq!(snap["placement"]["unavailable"], "placement control plane not started");
    assert!(node(&snap, "zeta").get("mesh").is_none());
    // Still a usable document: the cluster rows are there.
    assert_eq!(node(&snap, "zeta")["cluster"]["value"]["state"], "active");
}

#[test]
fn cluster_nodes_reports_last_announce_from_the_peer_record() {
    let row = crate::protocol::ClusterNodeInfo::from_peer(
        "zeta",
        &NodeState::Active,
        &NodePlatform::Edge,
        Some(peer("zeta", "zeta-box", 1_700_000_000)),
    );
    assert_eq!(row.last_announce, "2023-11-14T22:13:20Z");
    assert_eq!(row.name, "zeta-box");
    assert_eq!(row.address.as_deref(), Some("192.168.1.9:9000"));
    let gone = crate::protocol::ClusterNodeInfo::from_peer("zeta", &NodeState::Active, &NodePlatform::Edge, None);
    assert_eq!((gone.last_announce.as_str(), gone.name.as_str()), ("", "zeta"));
}

#[test]
fn classification_snapshot_is_read_and_location_is_admin() {
    let (anon, write, admin) = (
        CallerCapabilities::anonymous(),
        CallerCapabilities::from_scopes(["write"]),
        CallerCapabilities::from_scopes(["admin"]),
    );
    assert_eq!(required_capability("fleet.snapshot"), Capability::Read);
    assert!(anon.allows_method("fleet.snapshot"));
    for m in ["fleet.location.set", "fleet.location.clear", "fleet.anything"] {
        assert_eq!(required_capability(m), Capability::Admin, "{m}");
        assert!(!anon.allows_method(m) && !write.allows_method(m), "{m}");
        assert!(admin.allows_method(m), "{m}");
    }
}

#[test]
fn outside_a_project_the_snapshot_passes_and_the_label_needs_admin() {
    use crate::scope_gate::decide;
    assert!(decide(OutsideProjectPolicy::ReadOnly, "fleet.snapshot", false, || false).is_ok());
    assert!(decide(OutsideProjectPolicy::ReadOnly, "fleet.location.set", false, || false).is_err());
    assert!(decide(OutsideProjectPolicy::ReadOnly, "fleet.location.set", true, || false).is_ok());
    assert!(decide(OutsideProjectPolicy::DenyAll, "fleet.snapshot", false, || false).is_err());
}

async fn kernel() -> KernelRef {
    let kcfg = KernelConfig {
        governance: GovernanceConfig { outside_project: Some(OutsideProjectPolicy::ReadOnly) },
        chain: Some(ChainConfig::isolated_in(&tempfile::tempdir().unwrap().keep())),
        ..KernelConfig::default()
    };
    let k = clawft_kernel::boot::Kernel::boot(Config::default(), kcfg, Arc::new(clawft_platform::NativePlatform::new()))
        .await
        .expect("kernel boots");
    Arc::new(RwLock::new(k))
}

async fn wire(kernel: &KernelRef, method: &str, auth: &str, params: &str) -> Response {
    let (client, server) = tokio::io::duplex(1 << 20);
    let (tx, _rx) = tokio::sync::watch::channel(false);
    tokio::spawn(crate::daemon::handle_connection(server, Arc::clone(kernel), tx));
    let (r, mut w) = tokio::io::split(client);
    let line = format!(r#"{{"method":"{method}","params":{params},"auth":"{auth}","proto":1,"id":"1"}}"#);
    w.write_all(format!("{line}\n").as_bytes()).await.unwrap();
    let mut out = String::new();
    BufReader::new(r).read_line(&mut out).await.unwrap();
    serde_json::from_str(&out).unwrap()
}

#[tokio::test]
async fn the_snapshot_is_served_over_the_wire_to_a_read_caller_outside_any_project() {
    let k = kernel().await;
    let r = wire(&k, "fleet.snapshot", "read", "null").await;
    assert!(r.ok, "{:?}", r.error);
    let snap = r.result.unwrap();
    assert_eq!(snap["schema"], 1);
    let local = snap["local_node_id"].as_str().unwrap();
    assert!(snap["nodes"].as_array().unwrap().iter().any(|n| n["node_id"] == local && n["local"] == true));
}

#[tokio::test]
async fn a_read_caller_cannot_set_a_label_over_the_wire() {
    let k = kernel().await;
    let r = wire(&k, "fleet.location.set", "read", r#"{"node":"n","site":"s","room":"r"}"#).await;
    assert!(!r.ok);
    assert!(r.error.unwrap().contains("permission denied"));
    let r = wire(&k, "fleet.location.set", "write", r#"{"node":"n","site":"s","room":"r"}"#).await;
    assert!(!r.ok);
}

#[cfg(feature = "exochain")]
#[tokio::test]
async fn setting_a_location_saves_it_and_records_it_on_the_chain() {
    let k = kernel().await;
    let dir = tempfile::tempdir().unwrap();
    let params = json!({ "node": "c6-01", "site": "Plant 2", "room": "Room 14" });
    let r = set_location(dir.path(), params, &k).await;
    assert!(r.ok, "{:?}", r.error);
    let v = r.result.unwrap();
    assert_eq!((v["site"].as_str(), v["room"].as_str()), (Some("Plant 2"), Some("Room 14")));
    assert!(v["previous"].is_null());
    assert_eq!(v["chain"]["hash"].as_str().unwrap().len(), 64);

    let saved = fleet_labels::load(dir.path()).unwrap();
    assert_eq!(saved["c6-01"].room, "Room 14");

    // The audit event is on the chain with the label in its payload.
    let chain = k.read().await.chain_manager().cloned().unwrap();
    let ev = chain
        .tail(50)
        .into_iter()
        .find(|e| e.kind == fleet_labels::CHAIN_KIND)
        .expect("fleet.location.set event on the chain");
    assert_eq!(ev.source, fleet_labels::CHAIN_SOURCE);
    assert_eq!(ev.payload.as_ref().unwrap()["node"], "c6-01");
    assert_eq!(ev.sequence, v["chain"]["sequence"].as_u64().unwrap());

    // A second set returns the first as `previous`.
    let r = set_location(dir.path(), json!({ "node": "c6-01", "site": "Plant 3", "room": "Room 1" }), &k).await;
    assert_eq!(r.result.unwrap()["previous"]["site"], "Plant 2");
}

#[cfg(feature = "exochain")]
#[tokio::test]
async fn a_bad_label_request_changes_nothing() {
    let k = kernel().await;
    let dir = tempfile::tempdir().unwrap();
    for params in [
        json!({ "node": "bad node", "site": "s", "room": "r" }),
        json!({ "node": "n", "site": "", "room": "r" }),
        json!({ "node": "n", "site": "s" }),
        json!({ "node": "n", "site": "s", "room": "r", "extra": 1 }),
    ] {
        assert!(!set_location(dir.path(), params, &k).await.ok);
    }
    assert!(!dir.path().join(fleet_labels::FILE).exists());
    let chain = k.read().await.chain_manager().cloned().unwrap();
    assert!(chain.tail(200).iter().all(|e| e.kind != fleet_labels::CHAIN_KIND));
}

#[test]
fn load_is_labelled_by_who_measured_it() {
    let mut r = raw();
    r.local_load = Some(json!({ "load1": 0.5, "load5": 0.4 }));
    r.mesh.as_mut().unwrap()[0].load = Some(json!({ "load1": 2.0, "load5": 1.0, "cores": 4 }));
    let snap = assemble(r, Extra::default(), 1);
    let me = node(&snap, "local-node");
    assert_eq!((me["load"]["value"]["load1"].as_f64(), me["load"]["provenance"].as_str()), (Some(0.5), Some("daemon_observed")));
    let z = node(&snap, "zeta");
    assert_eq!((z["load"]["value"]["cores"].as_u64(), z["load"]["provenance"].as_str()), (Some(4), Some("peer_claimed")));
    assert!(node(&snap, "bad-node").get("load").is_none(), "no sample, no section");
}

#[test]
fn the_local_node_carries_this_machines_hostname() {
    let snap = assemble(raw(), Extra::default(), 1);
    let me = node(&snap, "local-node");
    assert_eq!((me["host"]["value"]["hostname"].as_str(), me["host"]["provenance"].as_str()), (Some("studio-mac.local"), Some("daemon_observed")));
    assert!(node(&snap, "zeta").get("host").is_none(), "only the local node");
}
