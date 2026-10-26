//! Native adapter: admission, limits, host contract, lifecycle. The payload
//! is a signed `#!` script so the tests run on any Unix host.

use std::time::{Duration, Instant};

use super::host_contract::HostContract;
use super::native::{NativeConfig, NativeRuntime, host_arch};
use super::test_support::{fake_elf, mode_of, signed_workload};
use super::types::{
    InstanceState, RunMode, RuntimeError, VerifiedWorkload, WorkloadConfig, WorkloadRuntime,
};

const SCRIPT: &str = r#"#!/bin/sh
case "$1" in
  --once)
    printf '{"cycle":1,"bind":"%s","tok_len":%d,"home":"%s","nofile":"%s","core":"%s","vmem":"%s","args":"%s"}\n' \
      "$COG_CSI_BIND" "${#COGNITUM_COG_TOKEN}" "$HOME" "$(ulimit -n)" "$(ulimit -c)" "$(ulimit -v)" "$*"
    touch "$COGNITUM_COG_DATA_DIR/wrote" && echo "data ok" >&2 ;;
  --spin) sleep 30 ;;
  --flood) i=0; while [ $i -lt 3000 ]; do echo "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"; i=$((i+1)); done ;;
  --interval) while :; do echo '{"tick":1}'; sleep 1; done ;;
esac
"#;

fn toml(max_runtime: u64, output_limit: usize) -> String {
    format!(
        r#"[cog]
id = "script-cog"
version = "0.1.0"

[config.threshold]
type = "float"
cli_arg = "--threshold"

[config.verbose]
type = "boolean"
cli_arg = "--verbose"

[resources]
ram_mb = 64
cpu_pct = 50

[console]
allowed_commands = ["--once", "--once --threshold 1.5", "--spin", "--flood"]
max_runtime_secs = {max_runtime}
output_limit_bytes = {output_limit}
"#
    )
}

fn arch() -> &'static str {
    host_arch().expect("tests need a supported host arch")
}

fn runtime(root: &std::path::Path, allow_interpreted: bool) -> NativeRuntime {
    NativeRuntime::new(NativeConfig {
        root: root.to_path_buf(),
        run_as: None,
        allow_interpreted,
    })
}

fn cfg(mode: RunMode) -> WorkloadConfig {
    WorkloadConfig {
        mode,
        args: Vec::new(),
        host: HostContract::new("127.0.0.1:15006".parse().unwrap()),
        node_id: "node-a".into(),
    }
}

#[test]
fn provides_runtime_native_for_the_host_arch() {
    let rt = runtime(std::path::Path::new("/nonexistent"), false);
    let caps = rt.provides();
    assert_eq!(caps.len(), 1);
    assert_eq!(caps[0].id.as_str(), "runtime.native");
    let json = serde_json::to_value(&caps[0]).unwrap();
    assert_eq!(json["attrs"]["arches_native"], serde_json::json!([arch()]));
    assert_eq!(json["provenance"], "probed");
    // No egress restriction exists for native processes yet.
    assert_eq!(
        rt.network_exposure(),
        crate::workload_governance::NetworkPolicy::Egress
    );
}

#[tokio::test]
async fn admission_refuses_scripts_foreign_elves_missing_arches_and_store_pins() {
    let fx = signed_workload(&toml(15, 4096), &[(arch(), SCRIPT.as_bytes())]);
    let rt = runtime(&fx.root.join("rt"), false);
    let err = rt.admit(&fx.workload).await.unwrap_err();
    assert!(
        matches!(err, RuntimeError::AdmissionRefused(ref m) if m.contains("script")),
        "{err}"
    );

    // An ELF for another machine never runs natively (on macOS no ELF does).
    let elf = fake_elf(if arch() == "x86_64" { 183 } else { 62 });
    let fx2 = signed_workload(&toml(15, 4096), &[(arch(), &elf)]);
    assert!(matches!(
        rt.admit(&fx2.workload).await,
        Err(RuntimeError::AdmissionRefused(_))
    ));

    let other = if arch() == "armv7" {
        "aarch64"
    } else {
        "armv7"
    };
    let fx3 = signed_workload(&toml(15, 4096), &[(other, SCRIPT.as_bytes())]);
    let err = runtime(&fx.root.join("rt3"), true)
        .admit(&fx3.workload)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("never emulates"), "{err}");

    let pin = VerifiedWorkload::store_pin("cognitum", "fall-detect", "1.0.0", None).unwrap();
    assert!(matches!(
        rt.admit(&pin).await,
        Err(RuntimeError::AdmissionRefused(_))
    ));
}

#[tokio::test]
async fn console_run_gets_the_host_contract_a_clean_env_rlimits_and_a_private_data_dir() {
    let fx = signed_workload(&toml(15, 4096), &[(arch(), SCRIPT.as_bytes())]);
    let rt = runtime(&fx.root.join("rt"), true);
    let c = cfg(RunMode::Once);
    let h = rt.load(&fx.workload, &c).await.unwrap();
    let dir = fx.root.join("rt").join(&h.instance_id);
    assert_eq!(mode_of(&dir.join("data")), 0o700);
    assert_eq!(mode_of(&dir.join("cog-script-cog")), 0o555);

    let ev = rt.console(&h, "--once --threshold 1.5").await.unwrap();
    assert!(ev.succeeded(), "{ev:?}");
    assert_eq!(ev.runtime, "native");
    let lines = ev.json_lines();
    assert_eq!(lines.len(), 1, "{}", ev.stdout);
    let l = &lines[0];
    assert_eq!(l["bind"], "127.0.0.1:15006");
    assert_eq!(l["tok_len"], 64);
    assert_eq!(l["home"], "", "parent environment must not leak");
    assert_eq!(l["nofile"], "256");
    assert_eq!(l["core"], "0");
    if cfg!(target_os = "linux") {
        // [resources].ram_mb = 64 as RLIMIT_AS, in KiB (macOS does not
        // implement RLIMIT_AS, so it is only asserted on Linux).
        assert_eq!(l["vmem"], "65536");
    }
    assert_eq!(l["args"], "--once --threshold 1.5");
    assert!(dir.join("data/wrote").exists());
    assert!(ev.stderr.contains("data ok"));
    // The chain summary carries no output and no token.
    let audit = ev.audit().to_string();
    assert!(!audit.contains("127.0.0.1:15006") && !audit.contains(c.host.token.expose()));
}

#[tokio::test]
async fn console_limits_are_enforced() {
    let fx = signed_workload(&toml(1, 1024), &[(arch(), SCRIPT.as_bytes())]);
    let rt = runtime(&fx.root.join("rt"), true);
    let h = rt.load(&fx.workload, &cfg(RunMode::Once)).await.unwrap();

    let t0 = Instant::now();
    let ev = rt.console(&h, "--spin").await.unwrap();
    assert!(ev.killed_for_timeout, "{ev:?}");
    assert!(!ev.succeeded());
    assert!(
        t0.elapsed() < Duration::from_secs(8),
        "killed near max_runtime_secs"
    );

    let ev = rt.console(&h, "--flood").await.unwrap();
    assert!(ev.truncated);
    assert!(ev.stdout.len() <= 1024);
    assert!(ev.stdout_bytes > 100_000);

    let err = rt.console(&h, "--interval 1").await.unwrap_err();
    assert!(
        matches!(err, RuntimeError::InvalidConfig(_)),
        "not in allowed_commands"
    );
}

#[tokio::test]
async fn load_validates_arguments_against_the_config_surface() {
    let fx = signed_workload(&toml(15, 4096), &[(arch(), SCRIPT.as_bytes())]);
    let rt = runtime(&fx.root.join("rt"), true);
    for bad in [
        vec!["--rm-rf".to_string()],
        vec!["--threshold".into()],
        vec!["--threshold".into(), "$(reboot)".into()],
        vec!["--once".into()],
    ] {
        let mut c = cfg(RunMode::Once);
        c.args = bad.clone();
        assert!(
            matches!(
                rt.load(&fx.workload, &c).await,
                Err(RuntimeError::InvalidConfig(_))
            ),
            "{bad:?}"
        );
    }
    let mut c = cfg(RunMode::Once);
    c.args = vec!["--threshold".into(), "2.5".into(), "--verbose".into()];
    rt.load(&fx.workload, &c).await.unwrap();
    let mut c = cfg(RunMode::Interval { secs: 0 });
    c.node_id = "node-b".into();
    assert!(rt.load(&fx.workload, &c).await.is_err());
}

#[tokio::test]
async fn interval_lifecycle_start_status_stop_unload() {
    let fx = signed_workload(&toml(15, 4096), &[(arch(), SCRIPT.as_bytes())]);
    let root = fx.root.join("rt");
    let rt = runtime(&root, true);
    let h = rt
        .load(&fx.workload, &cfg(RunMode::Interval { secs: 1 }))
        .await
        .unwrap();
    assert_eq!(rt.status(&h).await.state, InstanceState::Loaded);
    assert!(
        rt.load(&fx.workload, &cfg(RunMode::Interval { secs: 1 }))
            .await
            .is_err(),
        "duplicate"
    );

    rt.start(&h).await.unwrap();
    assert_eq!(rt.status(&h).await.state, InstanceState::Running);
    assert!(matches!(
        rt.start(&h).await,
        Err(RuntimeError::InvalidState(_))
    ));
    let err = rt.console(&h, "--once").await.unwrap_err();
    assert!(err.to_string().contains("feed contention"), "{err}");

    tokio::time::sleep(Duration::from_millis(1500)).await;
    let ev = rt.stop(&h, Duration::from_secs(2)).await.unwrap();
    assert_eq!(ev.signal, Some(libc::SIGTERM), "{ev:?}");
    assert!(!ev.json_lines().is_empty());
    assert_eq!(rt.status(&h).await.state, InstanceState::Exited);

    rt.unload(h.clone()).await.unwrap();
    assert!(!root.join(&h.instance_id).exists());
    assert_eq!(rt.status(&h).await.state, InstanceState::Unknown);
}

#[tokio::test]
async fn staging_over_a_directory_left_by_an_earlier_run_replaces_it() {
    use std::os::unix::fs::PermissionsExt;
    let fx = signed_workload(&toml(15, 4096), &[(arch(), SCRIPT.as_bytes())]);
    let rt = runtime(&fx.root.join("rt"), true);
    let c = cfg(RunMode::Listener);
    // What a daemon that restarted leaves behind: the instance directory with
    // a read-only binary, a populated `data/` and a stray file in it.
    let iid = super::native::instance_id(&fx.workload, &c).unwrap();
    let dir = fx.root.join("rt").join(&iid);
    std::fs::create_dir_all(dir.join("data")).unwrap();
    std::fs::write(dir.join("data/old-state"), b"x").unwrap();
    std::fs::write(dir.join("cog-script-cog"), b"#!/bin/sh\nexit 9\n").unwrap();
    std::fs::set_permissions(dir.join("cog-script-cog"), std::fs::Permissions::from_mode(0o555)).unwrap();
    std::fs::set_permissions(dir.join("data"), std::fs::Permissions::from_mode(0o700)).unwrap();

    let h = rt.load(&fx.workload, &c).await.expect("load over stale staging");
    assert_eq!(h.instance_id, iid);
    assert!(!dir.join("data/old-state").exists(), "old data is cleared");
    let staged = std::fs::read(dir.join("cog-script-cog")).unwrap();
    assert_eq!(staged, SCRIPT.as_bytes(), "the new binary is staged");
    rt.start(&h).await.unwrap();
    assert_eq!(rt.status(&h).await.state, InstanceState::Running);
    rt.unload(h).await.unwrap();
    assert!(!dir.exists());
}

#[tokio::test]
async fn a_file_or_symlink_where_the_directory_goes_is_replaced_not_followed() {
    let fx = signed_workload(&toml(15, 4096), &[(arch(), SCRIPT.as_bytes())]);
    let rt = runtime(&fx.root.join("rt"), true);
    let c = cfg(RunMode::Listener);
    let iid = super::native::instance_id(&fx.workload, &c).unwrap();
    std::fs::create_dir_all(fx.root.join("rt")).unwrap();
    let victim = fx.root.join("victim");
    std::fs::create_dir_all(&victim).unwrap();
    std::fs::write(victim.join("keep"), b"k").unwrap();
    std::os::unix::fs::symlink(&victim, fx.root.join("rt").join(&iid)).unwrap();
    let h = rt.load(&fx.workload, &c).await.expect("load over a symlink");
    assert!(victim.join("keep").exists(), "the symlink target is untouched");
    assert!(!std::fs::symlink_metadata(fx.root.join("rt").join(&iid)).unwrap().file_type().is_symlink());
    rt.unload(h).await.unwrap();
}
