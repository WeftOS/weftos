//! A computer workload's health, judged from the real in-daemon hub's
//! per-connection `last_seen_ms`: a desktop registers, pings, goes quiet, is
//! judged stale, pings again, is healthy, and drops. The adapter's clock is
//! injected (anchored on the hub's own stamp) so no test sleeps for 45 s.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use clawft_kernel::workload_kind::{
    ComputerKind, Health, HealthSample, KIND_COMPUTER, SampleState, WorkloadKind, computer_workload,
};
use clawft_kernel::workload_pkg::ManifestEnvelope;
use clawft_kernel::workload_runtime::{
    ComputerRuntime, HostContract, InstanceHandle, InstanceState, RunMode, WorkloadConfig, WorkloadRuntime,
};
use serde_json::json;
use weft_hub::hub::now_secs;

use crate::computer_hub_svc::{CONFIG_DIR, CONFIG_FILE, start};

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

async fn poll(rt: &ComputerRuntime, h: &InstanceHandle) -> Health {
    let st = rt.status(h).await;
    ComputerKind.judge(&HealthSample {
        state: match st.state {
            InstanceState::Loaded => SampleState::Loaded,
            InstanceState::Running => SampleState::Running,
            InstanceState::Exited => SampleState::Exited,
            InstanceState::Degraded => SampleState::Degraded,
            InstanceState::Unknown => SampleState::Unknown,
        },
        exit_code: st.exit_code,
        continuous: true,
        heartbeat_age_ms: rt.heartbeat_age_ms(h).await,
    })
}

#[tokio::test]
async fn health_follows_the_real_hubs_heartbeat_for_the_desktop() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join(CONFIG_DIR);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(CONFIG_FILE), HUB_TOML).unwrap();
    let svc = start(tmp.path()).await.unwrap().expect("hub starts");
    let hub = svc.hub().clone();

    // The desktop's tool server registers as machine `xpc`, in process.
    let tok = svc
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

    // The adapter reads the hub; "now" is anchored on the hub's own stamp.
    let now = Arc::new(AtomicU64::new(0));
    let clock = {
        let now = now.clone();
        Arc::new(move || now.load(Ordering::SeqCst))
    };
    let rt = ComputerRuntime::with_clock(svc.clone(), clock);
    let w = computer_workload(&ManifestEnvelope::new(
        KIND_COMPUTER,
        json!({"desktop": "xpc", "host_kind": "desktop", "capabilities": ["screen.capture"], "access": "own"}),
    ))
    .unwrap();
    let cfg = WorkloadConfig {
        mode: RunMode::Listener,
        args: Vec::new(),
        host: HostContract::default_feed(),
        node_id: "n".into(),
    };
    let h = rt.load(&w, &cfg).await.unwrap();
    rt.start(&h).await.unwrap();
    assert_eq!(h.workload_id, "xpc", "the instance names its hub connection");

    // Registered a moment ago: healthy.
    let seen0 = hub.last_seen_ms("xpc").expect("registered");
    now.store(seen0 + 2_000, Ordering::SeqCst);
    assert_eq!(poll(&rt, &h).await, Health::Healthy);

    // The desktop goes quiet for longer than 15 s x 3: stale, named.
    now.store(seen0 + 61_000, Ordering::SeqCst);
    match poll(&rt, &h).await {
        Health::Unhealthy(why) => assert!(why.starts_with("hub heartbeat stale: last seen 61s ago"), "{why}"),
        other => panic!("expected stale, got {other:?}"),
    }

    // It pings again: the hub stamps the frame and the instance recovers.
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    hub.on_text(cid, r#"{"method":"ping","ts_ms":1}"#);
    let seen1 = hub.last_seen_ms("xpc").unwrap();
    assert!(seen1 > seen0, "a ping advances the hub's last_seen_ms");
    now.store(seen1 + 2_000, Ordering::SeqCst);
    assert_eq!(poll(&rt, &h).await, Health::Healthy);

    // It drops: no connection is its own reason.
    hub.disconnect(cid);
    assert!(hub.last_seen_ms("xpc").is_none());
    match poll(&rt, &h).await {
        Health::Unhealthy(why) => assert_eq!(why, "no hub connection for this desktop"),
        other => panic!("expected no connection, got {other:?}"),
    }
    svc.shutdown();
}
