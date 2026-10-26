//! WEFT-720 Phase E E2E: `ecc.spatial` insert → query → branch → diff + replay.
//!
//! Runs against the daemon's `ecc.spatial.*` RPCs over the kernel `SpatialService`
//! (reattached 2026-10-04; it was ignored from 2026-07-31 while the RPC surface was gone).

#![cfg(unix)]
#![cfg(feature = "ecc")]

use std::sync::Arc;
use std::time::Duration;

use clawft_kernel::boot::Kernel;
use clawft_platform::NativePlatform;
use clawft_types::config::{AgentDefaults, AgentsConfig, Config, KernelConfig};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::watch;

fn base_config() -> Config {
    Config {
        agents: AgentsConfig {
            defaults: AgentDefaults {
                workspace: "~/.clawft/workspace".into(),
                model: "test/model".into(),
                max_tokens: 1024,
                temperature: 0.5,
                max_tool_iterations: 5,
                memory_window: 10,
            },
            ..AgentsConfig::default()
        },
        ..Config::default()
    }
}

fn minimal_kernel_config(chain_dir: &std::path::Path) -> KernelConfig {
    KernelConfig {
        governance: Default::default(),
        profile: None,
        shared_services: Default::default(),
        enabled: true,
        max_processes: 64,
        health_check_interval_secs: 5,
        brand: "WeftOS".to_string(),
        cluster: None,
        // Never the operator chain under ~/.clawft (isolated per boot).
        chain: Some(clawft_types::config::ChainConfig::isolated_in(chain_dir)),
        resource_tree: None,
        vector: None,
        profiles: None,
        pairing: None,
        mesh: None,
        anchor: None,
        ipc_tcp: None,
        llm: None,
        agent: None,
        spatial: Some(clawft_types::config::SpatialConfig { enabled: true, ..Default::default() }),
    }
}

async fn spawn_test_daemon() -> (
    tempfile::TempDir,
    std::path::PathBuf,
    watch::Sender<bool>,
) {
    let chain_dir = tempfile::tempdir().unwrap().keep();
    let (tmp, sock, tx, _kernel) = spawn_daemon_on(&chain_dir).await;
    (tmp, sock, tx)
}

type KernelHandle = Arc<tokio::sync::RwLock<Kernel<NativePlatform>>>;

/// Boot a daemon whose chain checkpoint lives in `chain_dir` (reuse the dir to restart).
async fn spawn_daemon_on(
    chain_dir: &std::path::Path,
) -> (tempfile::TempDir, std::path::PathBuf, watch::Sender<bool>, KernelHandle) {
    let tmp = tempfile::tempdir().unwrap();
    let socket_path = tmp.path().join("kernel.sock");
    let platform = NativePlatform::new();
    let kernel = Kernel::boot(base_config(), minimal_kernel_config(chain_dir), Arc::new(platform))
        .await
        .expect("kernel boot");
    let kernel = Arc::new(tokio::sync::RwLock::new(kernel));
    let handle = Arc::clone(&kernel);

    let listener = UnixListener::bind(&socket_path).unwrap();
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let accept_kernel = Arc::clone(&kernel);
    let accept_shutdown_tx = shutdown_tx.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                result = listener.accept() => {
                    match result {
                        Ok((stream, _)) => {
                            let k = Arc::clone(&accept_kernel);
                            let tx = accept_shutdown_tx.clone();
                            tokio::spawn(clawft_weave::daemon::handle_connection(stream, k, tx));
                        }
                        Err(_) => break,
                    }
                }
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() { break; }
                }
            }
        }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    (tmp, socket_path, shutdown_tx, handle)
}

async fn rpc(
    socket: &std::path::Path,
    method: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    let v = rpc_raw(socket, method, params).await;
    assert!(
        v.get("ok").and_then(|x| x.as_bool()).unwrap_or(false),
        "RPC {method} failed: {v}"
    );
    v.get("result").cloned().unwrap_or(serde_json::json!({}))
}

/// Full response envelope, success or not.
async fn rpc_raw(
    socket: &std::path::Path,
    method: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    let stream = UnixStream::connect(socket).await.expect("connect");
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let req = serde_json::json!({
        "id": "t",
        "proto": 1, "method": method,
        "params": params,
        "auth": "admin",
    });
    let mut line = serde_json::to_string(&req).unwrap();
    line.push('\n');
    writer.write_all(line.as_bytes()).await.unwrap();
    let mut resp_line = String::new();
    reader.read_line(&mut resp_line).await.unwrap();
    serde_json::from_str(resp_line.trim()).expect("json")
}

#[tokio::test]
async fn spatial_insert_query_branch_diff_replay() {
    let (_tmp, sock, shutdown) = spawn_test_daemon().await;

    // status
    let st = rpc(&sock, "ecc.spatial.status", serde_json::json!({})).await;
    assert_eq!(st["backend"], "bvh-memory");
    assert_eq!(st["len"], 0);

    // insert on main
    let ins = rpc(
        &sock,
        "ecc.spatial.insert",
        serde_json::json!({
            "tag": "wm_object",
            "aabb": "0,0,0,1,1,1",
            "identity": "object",
            "branch": 0,
        }),
    )
    .await;
    let leaf_a = ins["leaf_id"].as_u64().expect("leaf_id");
    assert_eq!(leaf_a, 0);

    // query point
    let q = rpc(
        &sock,
        "ecc.spatial.query",
        serde_json::json!({
            "kind": "point",
            "at": "0.5,0.5,0.5",
            "branch": 0,
        }),
    )
    .await;
    let hits = q["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["leaf_id"], leaf_a);

    // OQ4 filter wrapper: wrong tag → empty
    let qf = rpc(
        &sock,
        "ecc.spatial.query",
        serde_json::json!({
            "kind": "aabb",
            "region": "-1,-1,-1,2,2,2",
            "branch": 0,
            "filter_tags": "wm_surface",
        }),
    )
    .await;
    assert!(qf["hits"].as_array().unwrap().is_empty());

    // derive branch
    let der = rpc(
        &sock,
        "ecc.spatial.branch.derive",
        serde_json::json!({
            "parent": 0,
            "name": "exp",
            "note": "e2e",
        }),
    )
    .await;
    let child = der["branch_id"].as_u64().unwrap();
    assert_eq!(child, 1);

    // mutate child: add second leaf + remove first
    let ins2 = rpc(
        &sock,
        "ecc.spatial.insert",
        serde_json::json!({
            "tag": "wm_volume",
            "aabb": "5,0,0,6,1,1",
            "branch": child,
        }),
    )
    .await;
    let leaf_b = ins2["leaf_id"].as_u64().unwrap();

    let _ = rpc(
        &sock,
        "ecc.spatial.remove",
        serde_json::json!({
            "branch": child,
            "leaf_id": leaf_a,
        }),
    )
    .await;

    // branch diff
    let diff = rpc(
        &sock,
        "ecc.spatial.diff",
        serde_json::json!({
            "a": 0,
            "b": child,
            "region": "-10,-10,-10,10,10,10",
        }),
    )
    .await;
    let entries = diff["entries"].as_array().unwrap();
    assert!(
        entries.iter().any(|e| {
            e["leaf_id"] == leaf_a && e["kind"] == "removed"
        }),
        "expected removed {leaf_a}: {entries:?}"
    );
    assert!(
        entries.iter().any(|e| {
            e["leaf_id"] == leaf_b && e["kind"] == "added"
        }),
        "expected added {leaf_b}: {entries:?}"
    );

    // chain events non-empty
    let events = rpc(&sock, "ecc.spatial.events", serde_json::json!({})).await;
    assert!(events["count"].as_u64().unwrap() >= 4);

    // replay
    let replay = rpc(&sock, "ecc.spatial.replay", serde_json::json!({})).await;
    assert_eq!(replay["ok"], true);
    assert!(replay["events_replayed"].as_u64().unwrap() >= 4);

    // post-replay query still finds main leaf
    let q2 = rpc(
        &sock,
        "ecc.spatial.query",
        serde_json::json!({
            "kind": "point",
            "at": "0.5,0.5,0.5",
            "branch": 0,
        }),
    )
    .await;
    assert_eq!(q2["hits"].as_array().unwrap().len(), 1);

    let _ = shutdown.send(true);
}

/// R1/R2: publish a measured room shell as a region leaf through the live `ecc.spatial.*` path,
/// fetch it by region id, and answer spatial queries with its provenance. The evidence line is
/// the ADR-111 contract vector (tape-measured 6.40 x 3.66 x 2.44 m shell, proof MEASURED).
#[tokio::test]
async fn region_publish_get_and_spatial_query_carry_provenance() {
    let (_tmp, sock, shutdown) = spawn_test_daemon().await;
    let line = include_str!("../../../contracts/sensors/vectors/evidence/valid/shell_measure.jsonl");
    let evidence: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
    let region = "region/urth/meso/test-room";

    let pubd = rpc(&sock, "ecc.spatial.region.publish", serde_json::json!({ "evidence": evidence })).await;
    assert_eq!(pubd["region_id"], region);
    let leaf_id = pubd["leaf_id"].as_u64().unwrap();
    // Gates ran: nothing refused; the only finding is the draft "no site anchor" warning.
    assert_eq!(pubd["gates"]["refused"], false);
    let f = pubd["gates"]["findings"].as_array().unwrap();
    assert_eq!(f.len(), 1);
    assert_eq!((f[0]["gate"].as_str(), f[0]["severity"].as_str(), f[0]["draft_policy"].as_bool()), (Some("anchor"), Some("warn"), Some(true)));

    // Retrieve by region id.
    let got = rpc(&sock, "ecc.spatial.region.get", serde_json::json!({ "region_id": region })).await;
    assert_eq!(got["leaf"]["leaf_id"], leaf_id);
    // The BVH bound is f32 (broad phase); the exact measured extent stays in the payload as f64.
    let want = [0.0, 0.0, 0.0, 6.4, 3.66, 2.44];
    for (got_v, w) in got["leaf"]["aabb"].as_array().unwrap().iter().zip(want) {
        assert!((got_v.as_f64().unwrap() - w).abs() < 1e-5);
    }
    assert_eq!(got["leaf"]["payload"]["extent"]["max"], serde_json::json!([6.4, 3.66, 2.44]));
    let prov = &got["leaf"]["payload"]["provenance"];
    assert_eq!(prov["proof"], "MEASURED");
    assert_eq!(prov["receipt"], "shell-2026-10-03-a");
    assert_eq!(prov["uncertainty_m"], 0.01);
    assert_eq!(got["leaf"]["payload"]["anchor"]["status"], "unanchored");
    assert_eq!(got["leaf"]["payload"]["appearance_is_metric"], false);

    // Spatial query inside the shell returns the region with provenance.
    let q = rpc(
        &sock,
        "ecc.spatial.query",
        serde_json::json!({ "kind": "point", "at": "3.2,1.8,1.0", "with_payload": true }),
    )
    .await;
    let hits = q["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["leaf_id"], leaf_id);
    assert_eq!(hits[0]["leaf"]["payload"]["region_id"], region);
    assert_eq!(hits[0]["leaf"]["payload"]["provenance"]["proof"], "MEASURED");

    // Outside the shell nothing is claimed: no hit, no fill leaf.
    for at in ["7.0,1.8,1.0", "3.2,-0.5,1.0", "3.2,1.8,3.0"] {
        let q = rpc(&sock, "ecc.spatial.query", serde_json::json!({ "kind": "point", "at": at })).await;
        assert!(q["hits"].as_array().unwrap().is_empty(), "{at} must stay unobserved");
    }
    let st = rpc(&sock, "ecc.spatial.status", serde_json::json!({})).await;
    assert_eq!(st["len"], 1, "publishing must add exactly the one measured leaf");

    // Publishing the same region twice is refused; an unknown region is not invented.
    let again = rpc_raw(&sock, "ecc.spatial.region.publish", serde_json::json!({ "evidence": evidence })).await;
    assert_eq!(again["ok"], false);
    let none = rpc_raw(&sock, "ecc.spatial.region.get", serde_json::json!({ "region_id": "region/urth/meso/nowhere" })).await;
    assert_eq!(none["ok"], false);

    // Bad frame is refused rather than guessed.
    let mut bad = evidence.clone();
    bad["frame"] = "ecef".into();
    bad["region"] = "region/urth/meso/other".into();
    let r = rpc_raw(&sock, "ecc.spatial.region.publish", serde_json::json!({ "evidence": bad })).await;
    assert_eq!(r["ok"], false);

    let _ = shutdown.send(true);
}

/// A published region survives a daemon restart: the BVH is rebuilt from the chain at boot and
/// the region comes back by id with its provenance, and nothing extra appears.
#[tokio::test]
async fn published_region_survives_daemon_restart() {
    let chain_dir = tempfile::tempdir().unwrap();
    let line = include_str!("../../../contracts/sensors/vectors/evidence/valid/shell_measure.jsonl");
    let evidence: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
    let region = "region/urth/meso/test-room";

    let (_t1, sock1, down1, kernel1) = spawn_daemon_on(chain_dir.path()).await;
    let pubd = rpc(&sock1, "ecc.spatial.region.publish", serde_json::json!({ "evidence": evidence })).await;
    let leaf_id = pubd["leaf_id"].as_u64().unwrap();
    // Clean shutdown writes the chain checkpoint.
    kernel1.write().await.shutdown().await.expect("shutdown");
    let _ = down1.send(true);
    // The chain lock is released when the last kernel handle drops; wait for the accept loop
    // to let go of its clone.
    for _ in 0..100 {
        if Arc::strong_count(&kernel1) == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(Arc::strong_count(&kernel1), 1, "daemon tasks still hold the kernel");
    drop(kernel1);

    let (_t2, sock2, down2, _kernel2) = spawn_daemon_on(chain_dir.path()).await;
    let st = rpc(&sock2, "ecc.spatial.status", serde_json::json!({})).await;
    assert_eq!(st["len"], 1, "exactly the one published leaf, no duplicates, no fill");
    let got = rpc(&sock2, "ecc.spatial.region.get", serde_json::json!({ "region_id": region })).await;
    assert_eq!(got["leaf"]["leaf_id"], leaf_id);
    let prov = &got["leaf"]["payload"]["provenance"];
    assert_eq!(prov["proof"], "MEASURED");
    assert_eq!(prov["receipt"], "shell-2026-10-03-a");
    assert_eq!(got["leaf"]["payload"]["anchor"]["status"], "unanchored");
    let out = rpc(&sock2, "ecc.spatial.query", serde_json::json!({ "kind": "point", "at": "7.0,1.8,1.0" })).await;
    assert!(out["hits"].as_array().unwrap().is_empty());

    // Restart did not re-append the replay: a third boot still holds one leaf.
    let again = rpc_raw(&sock2, "ecc.spatial.region.publish", serde_json::json!({ "evidence": evidence })).await;
    assert_eq!(again["ok"], false, "already published, refused after restart too");
    let _ = down2.send(true);
}
