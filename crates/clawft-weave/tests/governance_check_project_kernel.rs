//! `governance.check` on a REAL project-profile kernel with an overlay.
//!
//! Regression for the fail-open where `OverlayGate` did not implement
//! `GateBackend::check_detailed`, so the RPC answered `no_governance_engine`
//! on every project kernel and overlay denies were silently ignored.
//!
//! The kernel is booted in-process from a child root built by the kernel's
//! own test builder (certificate, signed parent policy and pins all checked),
//! then the overlay is written and the call goes through the daemon's
//! connection handler, so routing, capability check and the real handler
//! (`governance_check_rpc::handle`) are all on the path. No daemon process.
//!
//! One-test binary: it sets process-wide state and enters the project profile.

use std::sync::Arc;
use std::time::Duration;

use clawft_kernel::boot::Kernel;
use clawft_platform::NativePlatform;
use clawft_types::config::{ChainConfig, Config, KernelConfig, KernelProfile};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{RwLock, watch};

async fn rpc(sock: &std::path::Path, method: &str, params: Value) -> Value {
    let (r, mut w) = UnixStream::connect(sock).await.unwrap().into_split();
    let req = json!({"id": "t", "proto": 1, "method": method, "params": params, "auth": "admin"});
    w.write_all(format!("{req}\n").as_bytes()).await.unwrap();
    let mut line = String::new();
    BufReader::new(r).read_line(&mut line).await.unwrap();
    serde_json::from_str(line.trim()).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn governance_check_rpc_on_a_project_kernel_enforces_overlay_rules() {
    let run = tempfile::tempdir().unwrap();
    // Pin the run root without touching the environment, so nothing here can
    // resolve the real `~/.weftos/run`.
    clawft_types::runtime_paths::set_user_profile_at(run.path());

    let mut config = Config::default();
    let mut kc = KernelConfig {
        profile: Some(KernelProfile::Project),
        chain: Some(ChainConfig::isolated_in(&tempfile::tempdir().unwrap().keep())),
        ..KernelConfig::default()
    };
    config.kernel = kc.clone();
    clawft_weave::project_hooks::adjust_services(&mut config, &mut kc);
    assert!(clawft_weave::project_profile::is_project_profile());

    let paths = clawft_kernel::overlay_runtime::test_support::install_child_fixture(run.path());
    std::fs::write(
        paths.overlay().expect("overlay path"),
        "[[deny]]\nid = \"d1\"\nactions = [\"tool.shell_exec\"]\nreason = \"no shell\"\n\n\
         [[require_approval]]\nactions = [\"cron.add\"]\n",
    )
    .unwrap();

    let kernel = Kernel::boot_with_node_key(config, kc, Arc::new(NativePlatform::new()), Some([3u8; 32]))
        .await
        .expect("project kernel boots with the overlay");
    let kernel = Arc::new(RwLock::new(kernel));
    let sock = run.path().join("kernel.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    let (tx, _rx) = watch::channel(false);
    let (k, t) = (Arc::clone(&kernel), tx.clone());
    tokio::spawn(async move {
        while let Ok((s, _)) = listener.accept().await {
            tokio::spawn(clawft_weave::daemon::handle_connection(s, Arc::clone(&k), t.clone()));
        }
    });
    tokio::time::sleep(Duration::from_millis(30)).await;

    // 1. Overlay `[[deny]]` -> deny, naming the rule id. Never `no_governance_engine`.
    let deny = rpc(&sock, "governance.check", json!({"agent_id": "tapestry", "tool": "shell_exec"})).await;
    assert_ne!(deny["error_kind"], "no_governance_engine", "fail-open regression: {deny}");
    assert_eq!(deny["ok"], true, "{deny}");
    assert_eq!(deny["result"]["decision"], "deny", "{deny}");
    let named = deny["result"]["reason"].to_string() + &deny["result"]["evaluated_rules"].to_string();
    assert!(named.contains("d1"), "deny must name overlay rule d1: {deny}");

    // 2. Overlay `[[require_approval]]` -> escalate.
    let esc = rpc(&sock, "governance.check", json!({"agent_id": "tapestry", "action": "cron.add"})).await;
    assert_ne!(esc["error_kind"], "no_governance_engine", "{esc}");
    assert_eq!(esc["ok"], true, "{esc}");
    assert_eq!(esc["result"]["decision"], "escalate_to_human", "{esc}");

    // 3. An action no rule covers still answers (a verdict, not "no engine").
    let ok = rpc(&sock, "governance.check", json!({"agent_id": "tapestry", "tool": "read_file"})).await;
    assert_eq!(ok["ok"], true, "{ok}");
    assert_ne!(ok["error_kind"], "no_governance_engine", "{ok}");
    assert_eq!(ok["result"]["decision"], "permit", "{ok}");
}
