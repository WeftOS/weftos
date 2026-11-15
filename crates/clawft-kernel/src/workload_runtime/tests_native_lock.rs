//! `load` must not hold the adapter's instance-map lock while it stages (and
//! chowns a possibly huge `data/`): a concurrent `status` must not wait.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::host_contract::HostContract;
use super::native::{NativeConfig, NativeRuntime, STAGE_HOLD, host_arch, instance_id};
use super::test_support::signed_workload;
use super::types::{InstanceState, RunMode, RuntimeError, WorkloadConfig, WorkloadRuntime};

const TOML: &str = r#"[cog]
id = "lock-cog"
version = "0.1.0"

[resources]
ram_mb = 64
cpu_pct = 50

[console]
allowed_commands = ["--once"]
max_runtime_secs = 5
output_limit_bytes = 1024
"#;

fn cfg(node: &str) -> WorkloadConfig {
    WorkloadConfig {
        mode: RunMode::Once,
        args: Vec::new(),
        host: HostContract::new("127.0.0.1:15006".parse().unwrap()),
        node_id: node.into(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// The guard is held on purpose across the awaits: it parks the load's
// spawn_blocking `stage` (a std thread), never another async task.
#[allow(clippy::await_holding_lock)]
async fn status_is_not_blocked_while_another_instance_stages() {
    let arch = host_arch().expect("supported host arch");
    let fx = signed_workload(TOML, &[(arch, b"#!/bin/sh\nexit 0\n")]);
    let root = fx.root.join("rt");
    let rt = Arc::new(NativeRuntime::new(NativeConfig {
        root: root.clone(),
        run_as: None,
        allow_interpreted: true,
    }));
    // One instance already loaded: what a status call asks about.
    let first = rt.load(&fx.workload, &cfg("node-a")).await.unwrap();

    // A second one whose staging is held, as a slow chown of a huge data/ is.
    let slow_cfg = cfg("node-b");
    let slow_id = instance_id(&fx.workload, &slow_cfg).unwrap();
    let hold = Arc::new(Mutex::new(()));
    let guard = hold.lock().unwrap();
    *STAGE_HOLD.lock().unwrap() = Some((root.join(&slow_id), hold.clone()));
    let (rt2, w2) = (rt.clone(), fx.workload.clone());
    let loading = tokio::spawn(async move { rt2.load(&w2, &slow_cfg).await });
    // Let the load reach the held stage.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!loading.is_finished(), "the load should be parked in stage");

    let st = tokio::time::timeout(Duration::from_secs(2), rt.status(&first))
        .await
        .expect("status blocked behind a staging load");
    assert_eq!(st.state, InstanceState::Loaded);

    // The id being staged is claimed: a second load of it is refused, not raced.
    let dup = rt.load(&fx.workload, &cfg("node-b")).await.unwrap_err();
    assert!(matches!(dup, RuntimeError::InvalidState(_)), "{dup}");

    *STAGE_HOLD.lock().unwrap() = None;
    drop(guard);
    let h = loading.await.unwrap().unwrap();
    assert_eq!(rt.status(&h).await.state, InstanceState::Loaded);
    // And the claim is released: a failed/finished load leaves nothing behind.
    assert!(matches!(
        rt.load(&fx.workload, &cfg("node-b")).await,
        Err(RuntimeError::InvalidState(ref m)) if m.contains("already loaded")
    ));
}
