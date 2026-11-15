//! Container adapters against a recording [`CommandRunner`] (no engine
//! needed). Live engine runs are in `tests_live`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;

use super::container::{ContainerRuntime, ContainerRuntimeConfig, container_name};
use super::container_cmd::{CmdOutput, CommandRunner, Engine, validate_base_image};
use super::host_contract::HostContract;
use super::test_support::{fake_elf, mode_of, signed_workload};
use super::types::{InstanceState, RunMode, RuntimeError, WorkloadConfig, WorkloadRuntime};

pub(super) const BASE: &str =
    "python@sha256:392307d22300de8b5986851a12d9176dfc0fc073e65bf6523ebd7dcbeb23564e";
pub(super) const TOML: &str = r#"[cog]
id = "anomaly-detect"
version = "1.2.0"

[resources]
ram_mb = 64
cpu_pct = 150

[console]
allowed_commands = ["--once"]
max_runtime_secs = 15
output_limit_bytes = 2048
"#;

/// Records every call; answers by subcommand.
#[derive(Default)]
pub(super) struct Recorder {
    calls: Mutex<Vec<Vec<String>>>,
    fail_build: bool,
    time_out_runs: bool,
}

impl Recorder {
    pub(super) fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().unwrap().clone()
    }
    pub(super) fn find(&self, sub: &str) -> Vec<String> {
        self.calls()
            .into_iter()
            .find(|c| c[0] == sub)
            .unwrap_or_else(|| panic!("no {sub} call"))
    }
}

#[async_trait]
impl CommandRunner for Recorder {
    async fn run(
        &self,
        _p: &str,
        args: &[String],
        _t: Duration,
        limit: usize,
    ) -> Result<CmdOutput, RuntimeError> {
        self.calls.lock().unwrap().push(args.to_vec());
        let ok = |stdout: &str| CmdOutput {
            status: Some(0),
            stdout: stdout[..stdout.len().min(limit)].to_string(),
            stdout_bytes: stdout.len() as u64,
            ..CmdOutput::default()
        };
        Ok(match args[0].as_str() {
            "build" if self.fail_build => CmdOutput {
                status: Some(1),
                stderr: "no base".into(),
                ..CmdOutput::default()
            },
            "run" if self.time_out_runs && args[1] == "--rm" => CmdOutput {
                timed_out: true,
                ..CmdOutput::default()
            },
            "run" if args[1] == "--rm" => ok("{\"anomalies\":[],\"stats\":{}}\n"),
            "inspect" => ok(r#"[{"State":{"Status":"exited","ExitCode":0}}]"#),
            "logs" => ok("{\"anomalies\":[]}\n"),
            _ => ok(""),
        })
    }
}

pub(super) fn aarch64_elf() -> Vec<u8> {
    fake_elf(183)
}

pub(super) fn rt(engine: Engine, root: &std::path::Path, rec: Arc<Recorder>) -> ContainerRuntime {
    let mut c = ContainerRuntimeConfig::new(engine, BASE, root);
    c.arches_emulated = vec!["armv7".into()];
    c.feed_host_port = Some(25006);
    c.variant = Some("orbstack".into());
    ContainerRuntime::new(c, rec)
}

pub(super) fn cfg() -> WorkloadConfig {
    WorkloadConfig {
        mode: RunMode::Once,
        args: Vec::new(),
        host: HostContract::default_feed(),
        node_id: "mac-1".into(),
    }
}

#[test]
fn each_engine_provides_its_capability_id() {
    let rec = Arc::new(Recorder::default());
    for (e, id) in [
        (Engine::Apple, "runtime.container.apple"),
        (Engine::Docker, "runtime.container.docker"),
        (Engine::Podman, "runtime.container.podman"),
    ] {
        let caps = rt(e, std::path::Path::new("/x"), rec.clone()).provides();
        assert_eq!(caps[0].id.as_str(), id);
        let j = serde_json::to_value(&caps[0]).unwrap();
        assert_eq!(j["attrs"]["arches_native"], serde_json::json!(["aarch64"]));
        assert_eq!(j["attrs"]["arches_emulated"], serde_json::json!(["armv7"]));
        assert_eq!(j["attrs"]["variant"], "orbstack");
    }
}

#[test]
fn base_image_must_be_digest_pinned() {
    assert!(validate_base_image(BASE).is_ok());
    for bad in [
        "python:3.12",
        "python@sha256:abc",
        "Evil@sha256:392307d22300de8b5986851a12d9176dfc0fc073e65bf6523ebd7dcbeb23564e",
    ] {
        assert!(validate_base_image(bad).is_err(), "{bad}");
    }
}

#[tokio::test]
async fn admission_prefers_native_arches_and_emulates_only_on_opt_in() {
    let rec = Arc::new(Recorder::default());
    let root = tempfile::tempdir().unwrap();
    let native = signed_workload(
        TOML,
        &[("aarch64", &aarch64_elf()), ("armv7", &fake_elf(40))],
    );
    let adm = rt(Engine::Docker, root.path(), rec.clone())
        .admit(&native.workload)
        .await
        .unwrap();
    assert_eq!((adm.arch.as_str(), adm.emulated), ("aarch64", false));

    let armv7 = signed_workload(TOML, &[("armv7", &fake_elf(40))]);
    let r = rt(Engine::Docker, root.path(), rec.clone());
    assert!(
        matches!(r.admit(&armv7.workload).await, Err(RuntimeError::AdmissionRefused(m)) if m.contains("opt-in"))
    );
    let mut c = ContainerRuntimeConfig::new(Engine::Docker, BASE, root.path());
    c.arches_emulated = vec!["armv7".into()];
    c.allow_emulated = true;
    let adm = ContainerRuntime::new(c, rec.clone())
        .admit(&armv7.workload)
        .await
        .unwrap();
    assert!(adm.emulated);

    let wrong = signed_workload(TOML, &[("aarch64", &fake_elf(62))]);
    assert!(
        r.admit(&wrong.workload).await.is_err(),
        "x86_64 ELF labelled aarch64"
    );
    let mut c = ContainerRuntimeConfig::new(Engine::Docker, "python:latest", root.path());
    c.arches_native = vec!["aarch64".into()];
    assert!(
        ContainerRuntime::new(c, rec)
            .admit(&native.workload)
            .await
            .is_err(),
        "unpinned base"
    );
}

#[tokio::test]
async fn load_builds_a_one_binary_image_and_keeps_the_token_off_command_lines() {
    let rec = Arc::new(Recorder::default());
    let root = tempfile::tempdir().unwrap();
    let fx = signed_workload(TOML, &[("aarch64", &aarch64_elf())]);
    let r = rt(Engine::Docker, root.path(), rec.clone());
    let c = cfg();
    let h = r.load(&fx.workload, &c).await.unwrap();

    let build = rec.find("build");
    assert_eq!(&build[1..3], ["--platform", "linux/arm64"]);
    assert!(build[4].starts_with("weftos-cog/anomaly-detect:"));
    let dir = root.path().join(&h.instance_id);
    let df = std::fs::read_to_string(dir.join("ctx/Dockerfile")).unwrap();
    assert!(df.starts_with(&format!("FROM {BASE}\n")));
    assert!(df.contains("USER 65534:65534"));
    assert_eq!(std::fs::read(dir.join("ctx/cog")).unwrap(), aarch64_elf());
    assert_eq!(mode_of(&dir.join("env")), 0o600);
    let env = std::fs::read_to_string(dir.join("env")).unwrap();
    assert!(env.contains("COG_CSI_BIND=0.0.0.0:5006"));
    assert!(env.contains(&format!("COGNITUM_COG_TOKEN={}", c.host.token.expose())));
    assert!(env.contains("COGNITUM_COG_DATA_DIR=/data"));

    r.start(&h).await.unwrap();
    let run = rec.find("run");
    let s = run.join(" ");
    for want in [
        "-d",
        "--read-only",
        "--cap-drop ALL",
        "--memory 64M",
        "--cpus 1.50",
        "--tmpfs /data",
        "--pids-limit 64",
        "no-new-privileges",
        "-p 127.0.0.1:25006:5006/udp",
        "--once",
    ] {
        assert!(s.contains(want), "missing {want} in {s}");
    }
    assert!(s.contains(&format!("--name {}", container_name(&h.instance_id))));
    for call in rec.calls() {
        assert!(
            !call.join(" ").contains(c.host.token.expose()),
            "token on a command line"
        );
    }
}

#[tokio::test]
async fn apple_uses_arch_and_whole_cpus() {
    let rec = Arc::new(Recorder::default());
    let root = tempfile::tempdir().unwrap();
    let fx = signed_workload(TOML, &[("aarch64", &aarch64_elf())]);
    let r = rt(Engine::Apple, root.path(), rec.clone());
    let h = r.load(&fx.workload, &cfg()).await.unwrap();
    assert_eq!(&rec.find("build")[1..3], ["--arch", "arm64"]);
    r.start(&h).await.unwrap();
    let s = rec.find("run").join(" ");
    assert!(
        s.contains("--arch arm64") && s.contains("--cpus 2") && !s.contains("--pids-limit"),
        "{s}"
    );
    // Apple has no --pids-limit: the task cap is RLIMIT_NPROC for the
    // non-root cog user.
    assert!(
        s.contains("--ulimit nproc=64:64"),
        "no task cap on Apple: {s}"
    );
    r.unload(h).await.unwrap();
    let calls = rec.calls();
    assert!(
        calls
            .iter()
            .any(|c| c[..2] == ["delete".to_string(), "--force".to_string()])
    );
    assert!(
        calls
            .iter()
            .any(|c| c[..2] == ["image".to_string(), "delete".to_string()])
    );
}

#[tokio::test]
async fn stop_collects_logs_and_exit_code_then_removes_the_container() {
    let rec = Arc::new(Recorder::default());
    let root = tempfile::tempdir().unwrap();
    let fx = signed_workload(TOML, &[("aarch64", &aarch64_elf())]);
    let r = rt(Engine::Docker, root.path(), rec.clone());
    let h = r.load(&fx.workload, &cfg()).await.unwrap();
    assert!(matches!(
        r.stop(&h, Duration::from_secs(3)).await,
        Err(RuntimeError::InvalidState(_))
    ));
    r.start(&h).await.unwrap();
    assert!(matches!(
        r.console(&h, "--once").await,
        Err(RuntimeError::InvalidState(_))
    ));
    assert_eq!(r.status(&h).await.state, InstanceState::Exited);
    let ev = r.stop(&h, Duration::from_secs(3)).await.unwrap();
    assert_eq!(ev.exit_code, Some(0));
    assert_eq!(ev.json_lines().len(), 1);
    assert_eq!(ev.runtime, "container.docker");
    let calls = rec.calls();
    let stop = calls.iter().position(|c| c[0] == "stop").unwrap();
    assert_eq!(calls[stop][1..3], ["-t".to_string(), "3".to_string()]);
    assert!(calls[stop..].iter().any(|c| c[0] == "rm"));
    r.unload(h.clone()).await.unwrap();
    assert!(!root.path().join(&h.instance_id).exists());
}

#[tokio::test]
async fn console_runs_in_the_foreground_and_times_out_with_a_kill() {
    let root = tempfile::tempdir().unwrap();
    let fx = signed_workload(TOML, &[("aarch64", &aarch64_elf())]);
    let rec = Arc::new(Recorder::default());
    let r = rt(Engine::Docker, root.path(), rec.clone());
    let h = r.load(&fx.workload, &cfg()).await.unwrap();
    let ev = r.console(&h, "--once").await.unwrap();
    assert!(ev.succeeded() && ev.json_lines().len() == 1, "{ev:?}");
    assert!(rec.find("run").contains(&"--rm".to_string()));
    assert!(matches!(
        r.console(&h, "--interval 1").await,
        Err(RuntimeError::InvalidConfig(_))
    ));

    let slow = Arc::new(Recorder {
        time_out_runs: true,
        ..Recorder::default()
    });
    let r = rt(
        Engine::Docker,
        root.path().join("b").as_path(),
        slow.clone(),
    );
    let h = r.load(&fx.workload, &cfg()).await.unwrap();
    let ev = r.console(&h, "--once").await.unwrap();
    assert!(ev.killed_for_timeout && ev.exit_code.is_none());
    let rm = slow.calls().into_iter().rfind(|c| c[0] == "rm").unwrap();
    assert!(rm[2].ends_with("-console"));
}

#[tokio::test]
async fn failed_build_leaves_nothing_behind() {
    let rec = Arc::new(Recorder {
        fail_build: true,
        ..Recorder::default()
    });
    let root = tempfile::tempdir().unwrap();
    let fx = signed_workload(TOML, &[("aarch64", &aarch64_elf())]);
    let r = rt(Engine::Podman, root.path(), rec);
    let err = r.load(&fx.workload, &cfg()).await.unwrap_err();
    assert!(
        matches!(err, RuntimeError::Backend(ref m) if m.contains("podman build")),
        "{err}"
    );
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[test]
fn inspect_output_parses_for_each_engine_shape() {
    use super::container_cmd::parse_inspect;
    let apple_v1 = r#"[{"id":"x","status":{"state":"running","networks":[]}}]"#;
    let apple_old = r#"[{"id":"x","status":"stopped"}]"#;
    let docker = r#"[{"State":{"Status":"exited","ExitCode":3}}]"#;
    assert_eq!(
        parse_inspect(Engine::Apple, apple_v1),
        Some(("running".into(), None))
    );
    assert_eq!(
        parse_inspect(Engine::Apple, apple_old),
        Some(("stopped".into(), None))
    );
    assert_eq!(
        parse_inspect(Engine::Docker, docker),
        Some(("exited".into(), Some(3)))
    );
    assert_eq!(parse_inspect(Engine::Docker, "not json"), None);
}

#[tokio::test]
async fn feed_publish_address_and_network_exposure_follow_the_config() {
    use crate::workload_governance::NetworkPolicy;
    let rec = Arc::new(Recorder::default());
    let root = tempfile::tempdir().unwrap();
    let fx = signed_workload(TOML, &[("aarch64", &aarch64_elf())]);
    let mut c = ContainerRuntimeConfig::new(Engine::Docker, BASE, root.path());
    c.feed_host_port = Some(25006);
    c.feed_publish_ip = "192.0.2.20".parse().unwrap();
    let r = ContainerRuntime::new(c.clone(), rec.clone());
    // Default bridge network: the container can reach the internet.
    assert_eq!(r.network_exposure(), NetworkPolicy::Egress);
    let h = r.load(&fx.workload, &cfg()).await.unwrap();
    r.start(&h).await.unwrap();
    let s = rec.find("run").join(" ");
    assert!(s.contains("-p 192.0.2.20:25006:5006/udp"), "{s}");

    c.feed_publish_ip = "::".parse().unwrap();
    c.network = Some("none".into());
    let r6 = ContainerRuntime::new(c.clone(), rec.clone());
    assert_eq!(r6.network_exposure(), NetworkPolicy::None);
    c.network = Some("host".into());
    assert_eq!(
        ContainerRuntime::new(c, rec.clone()).network_exposure(),
        NetworkPolicy::Egress
    );
    let mut c2 = cfg();
    c2.node_id = "mac-2".into();
    let h6 = r6.load(&fx.workload, &c2).await.unwrap();
    r6.start(&h6).await.unwrap();
    let runs: Vec<String> = rec
        .calls()
        .into_iter()
        .filter(|c| c[0] == "run")
        .map(|c| c.join(" "))
        .collect();
    assert!(runs[1].contains("-p [::]:25006:5006/udp"), "{}", runs[1]);
}

#[test]
fn container_name_stays_inside_the_dns_label_limit_for_a_max_length_id() {
    use super::container_cmd::CONTAINER_NAME_MAX;
    let id_a = format!("{}-a", "x".repeat(160));
    let id_b = format!("{}-b", "x".repeat(160));
    assert_eq!(id_a.len(), 162);
    let (a, b) = (container_name(&id_a), container_name(&id_b));
    for n in [&a, &b] {
        assert!(n.len() <= CONTAINER_NAME_MAX, "{n} is {} long", n.len());
        // The console sibling adds "-console" and must still fit a hostname.
        assert!(format!("{n}-console").len() <= 63);
        assert!(n.starts_with("weftos-"));
        assert!(n.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'));
        assert!(!n.ends_with('-'));
    }
    assert_ne!(a, b, "ids differing only past the cut must not collide");
    assert_eq!(a, container_name(&id_a), "stable");
    // A short id is unchanged.
    assert_eq!(container_name("Cog_1.x"), "weftos-cog-1-x");
}
