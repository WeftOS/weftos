//! End to end (GBF-003, owner decisions 1 to 4 of 2026-11-15): a computer is
//! placed for a desktop through the daemon's real host (`local_host`, the
//! signed `workload.place` path, the real governance gate with the built-in
//! owner permit), supervised by the adapter over the real in-daemon hub. The
//! desktop pings (Healthy), goes quiet (Unhealthy, an alert, no restart), pings
//! again (Healthy). The adapter's clock is moved by the test.
//!
//! The hub is started with `start`, not `start_global`: a process-global hub
//! would change what the other tests in this crate see (their `adapter_ready`
//! and route lists).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use clawft_kernel::chain::{ChainManager, EVENT_KIND_WORKLOAD_LIFECYCLE};
use clawft_kernel::workload_ctl::msg::CtlOutcome;
use clawft_kernel::workload_ctl::{CtlRequest, CtlResponse, LifecycleState, WorkloadHostService};
use ed25519_dalek::SigningKey;
use serde_json::{Value, json};
use weft_hub::hub::now_secs;

use crate::computer_hub_svc::{CONFIG_DIR, CONFIG_FILE, start};
use crate::workload_host_serve::{HostParts, local_host};
use crate::workload_place_rpc::tests::{anchors, exchange};

const HUB_TOML: &str = r#"
[listen]
addr = "127.0.0.1:0"

[[auth.minters]]
id = "weftos-xpc"
host_kind = "desktop"
key_file = "keys/xpc.key"

[[machines]]
id = "xpc"
description = "aepod-xpc"
minter = "weftos-xpc"
"#;

fn computer_body() -> Value {
    json!({"desktop": "xpc", "host_kind": "desktop", "capabilities": ["screen.capture"], "access": "own"})
}

async fn call(svc: &WorkloadHostService, key: &SigningKey, node: &str, method: &str, body: Value) -> CtlOutcome {
    let now = chrono::Utc::now().timestamp_millis() as u64;
    let req = CtlRequest::new(key, method, node, now, 60_000, Some("a".repeat(64)), body);
    let resp = svc.handle(method, &req.sign(key).unwrap(), None).await;
    serde_json::from_str::<CtlResponse>(&resp.payload).unwrap().outcome
}

#[tokio::test]
async fn a_computer_is_placed_supervised_alerted_never_restarted_and_recovers() {
    let tmp = tempfile::tempdir().unwrap();
    let hub_dir = tmp.path().join(CONFIG_DIR);
    std::fs::create_dir_all(&hub_dir).unwrap();
    std::fs::write(hub_dir.join(CONFIG_FILE), HUB_TOML).unwrap();
    let hub_svc = start(tmp.path()).await.unwrap().expect("hub starts");
    let hub = hub_svc.hub().clone();
    let clock_ms = Arc::new(AtomicU64::new(0));
    let c = clock_ms.clone();
    let adapter = clawft_kernel::workload_runtime::ComputerRuntime::with_clock(
        hub_svc.clone(),
        Arc::new(move || c.load(Ordering::SeqCst)),
    );

    // The daemon's own host: the real constructor, the real gate with the built-in permit.
    let key = SigningKey::from_bytes(&[5; 32]);
    let id = clawft_kernel::node_id_from_pubkey(&key.verifying_key().to_bytes());
    let chain = Arc::new(ChainManager::new(0, 1000));
    let revocations = Arc::new(clawft_kernel::revocation::RevocationList::new(tmp.path().join("revoked_hosts.json")));
    let gate = crate::workload_gate::build_with(
        tmp.path(),
        &chain,
        None,
        revocations.clone(),
        crate::workload_place_rpc::computer_permits(&id, true),
    )
    .unwrap();
    assert!(gate.permits().iter().any(|p| p.id == "COMPUTER-OWNER-PERMIT"), "the hub is up, so the permit ships");
    let svc = local_host(HostParts {
        key: &key,
        dir: tmp.path(),
        chain: &chain,
        gate,
        exchange: exchange(&chain),
        anchors: anchors(&key),
        facts: Arc::new(|| None),
        serving: None,
        container: None,
        ingest: None,
        revocations,
        computer: Some(adapter),
    })
    .unwrap();

    // Place the computer: no package, only the desktop's declaration.
    let placed = call(
        &svc,
        &key,
        &id,
        "workload.place",
        json!({"name": "xpc", "variant": "computer", "start": true,
               "config": {"mode": {"mode": "listener"}}, "computer": computer_body()}),
    )
    .await;
    let iid = match placed {
        CtlOutcome::Ok { result } => result["instance_id"].as_str().expect("instance id").to_string(),
        CtlOutcome::Refused { refusal } => panic!("place refused: {refusal:?}"),
    };
    assert_eq!(svc.lifecycle_of(&iid).await, Some(LifecycleState::Running));

    // A package-less order for a cog is still refused by the same host.
    let cog = call(
        &svc,
        &key,
        &id,
        "workload.place",
        json!({"name": "some-cog", "variant": "aarch64-native", "start": true, "config": {"mode": {"mode": "listener"}}}),
    )
    .await;
    assert!(matches!(cog, CtlOutcome::Refused { .. }), "{cog:?}");

    // The desktop's tool server registers and pings.
    let tok = hub_svc
        .mint(&json!({"machine": "xpc", "sub": "mathew", "roles": ["server"]}), now_secs())
        .unwrap();
    let authed = hub
        .authenticate(tok["token"].as_str(), weft_hub::passkey::Role::Server, now_secs())
        .unwrap();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let hello = serde_json::from_value(json!({
        "protocol_version": "1.0.0", "kind": "tool_server",
        "server_id": "xpc", "metadata": {"hostname": "xpc"}
    }))
    .unwrap();
    let (cid, _ack) = hub.register(&authed, &hello, tx).unwrap();
    let seen0 = hub.last_seen_ms("xpc").expect("registered");
    let base = chrono::Utc::now().timestamp_millis() as u64;
    let at = |step: u64| base + step * 15_000;

    // Healthy: heard 2 s ago.
    clock_ms.store(seen0 + 2_000, Ordering::SeqCst);
    assert!(svc.supervise(at(1)).await.is_empty());
    assert_eq!(svc.lifecycle_of(&iid).await, Some(LifecycleState::Running));

    // Quiet for 61 s: three polls make it Unhealthy, with an alert naming the reason.
    clock_ms.store(seen0 + 61_000, Ordering::SeqCst);
    assert!(svc.supervise(at(2)).await.is_empty(), "miss 1");
    assert!(svc.supervise(at(3)).await.is_empty(), "miss 2");
    let steps = svc.supervise(at(4)).await;
    assert!(steps.iter().any(|s| s.to == LifecycleState::Unhealthy && s.reason.contains("hub heartbeat stale")), "{steps:?}");
    assert!(!steps.iter().any(|s| s.to == LifecycleState::Restarting), "never restarted: {steps:?}");
    assert_eq!(svc.lifecycle_of(&iid).await, Some(LifecycleState::Unhealthy));
    assert!(svc.supervise(at(5)).await.is_empty(), "no restart on the next poll either");
    assert_eq!(svc.lifecycle_of(&iid).await, Some(LifecycleState::Unhealthy));
    let alerts: Vec<_> = chain
        .tail(chain.len())
        .into_iter()
        .filter(|e| e.kind == EVENT_KIND_WORKLOAD_LIFECYCLE)
        .filter_map(|e| e.payload)
        .filter(|p| p["alert"] == true)
        .collect();
    assert_eq!(alerts.len(), 1, "{alerts:?}");
    assert!(alerts[0]["reason"].as_str().unwrap().contains("hub heartbeat stale"));
    assert_eq!(alerts[0]["restart"], "never");

    // It pings again and recovers.
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    hub.on_text(cid, r#"{"method":"ping","ts_ms":1}"#);
    let seen1 = hub.last_seen_ms("xpc").unwrap();
    assert!(seen1 > seen0);
    clock_ms.store(seen1 + 2_000, Ordering::SeqCst);
    let back = svc.supervise(at(6)).await;
    assert!(back.iter().any(|s| s.to == LifecycleState::Running && s.reason == "health recovered"), "{back:?}");
    assert_eq!(svc.lifecycle_of(&iid).await, Some(LifecycleState::Running));

    hub.disconnect(cid);
    hub_svc.shutdown();
}
