//! Container engine command lines (pure; unit-tested without an engine).
//!
//! The image is a minimal layer over an operator-pinned base (by digest)
//! holding only the one verified binary, built locally: no registry pull
//! other than the pinned base, which must already be present.

use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;

use super::evidence::RunEvidence;
use super::types::RuntimeError;

/// A container engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    /// Apple `container` (per-container VM).
    Apple,
    /// Docker-compatible engine (OrbStack, Docker Engine / Desktop).
    Docker,
    /// Podman.
    Podman,
}

impl Engine {
    /// CLI program.
    pub fn cli(self) -> &'static str {
        match self {
            Engine::Apple => "container",
            Engine::Docker => "docker",
            Engine::Podman => "podman",
        }
    }

    /// Adapter id.
    pub fn runtime_id(self) -> &'static str {
        match self {
            Engine::Apple => "container.apple",
            Engine::Docker => "container.docker",
            Engine::Podman => "container.podman",
        }
    }

    /// Capability id this adapter advertises.
    pub fn capability_id(self) -> &'static str {
        match self {
            Engine::Apple => "runtime.container.apple",
            Engine::Docker => "runtime.container.docker",
            Engine::Podman => "runtime.container.podman",
        }
    }

    /// Platform argument for a package arch.
    pub fn platform_args(self, arch: &str) -> Result<Vec<String>, RuntimeError> {
        let (docker, apple) = match arch {
            "aarch64" => ("linux/arm64", "arm64"),
            "armv7" => ("linux/arm/v7", "arm"),
            "x86_64" => ("linux/amd64", "amd64"),
            _ => {
                return Err(RuntimeError::InvalidConfig(format!(
                    "no platform for {arch}"
                )));
            }
        };
        Ok(match self {
            Engine::Apple => vec!["--arch".into(), apple.into()],
            _ => vec!["--platform".into(), docker.into()],
        })
    }
}

/// Unprivileged uid:gid inside the container (`nobody`).
pub const CONTAINER_USER: &str = "65534:65534";
/// Task (process + thread) cap for a cog container.
pub const PIDS_LIMIT: u32 = 64;
/// Where the binary lives in the image.
pub const COG_PATH: &str = "/cog";
/// Writable tmpfs for `COGNITUM_COG_DATA_DIR`.
pub const DATA_DIR: &str = "/data";

/// Validate an operator-pinned base image reference (`name@sha256:<64 hex>`).
pub fn validate_base_image(r: &str) -> Result<(), RuntimeError> {
    let ok = r.split_once("@sha256:").is_some_and(|(name, digest)| {
        !name.is_empty()
            && name.len() <= 200
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"./:_-".contains(&b))
            && crate::workload_pkg::codec::is_lower_hex(digest, 64)
    });
    if ok {
        Ok(())
    } else {
        Err(RuntimeError::InvalidConfig(
            "base image must be pinned by digest (name@sha256:<64 hex>)".into(),
        ))
    }
}

/// Dockerfile for the one-binary layer. The user is `run --user` when the
/// instance has its own identity (ADR-122 gap 1), so one image serves every
/// instance of a cog.
pub fn dockerfile(base_image: &str) -> String {
    format!(
        "FROM {base_image}\nCOPY cog {COG_PATH}\nUSER {CONTAINER_USER}\nWORKDIR /\nENTRYPOINT [\"{COG_PATH}\"]\n"
    )
}

/// `uid:gid` of an instance with an allocated identity.
pub fn user_of(id: &super::host_contract::CogIdentity) -> String {
    format!("{}:{}", id.uid, id.gid)
}

/// Image tag for a binary: `weftos-cog/<id>:<16 hex of its BLAKE3>`.
pub fn image_tag(cog_id: &str, blake3: &str) -> String {
    format!(
        "{}{cog_id}:{}",
        crate::container::COG_IMAGE_PREFIX,
        &blake3[..16.min(blake3.len())]
    )
}

/// `build` command.
pub fn build_cmd(
    engine: Engine,
    arch: &str,
    tag: &str,
    ctx: &Path,
) -> Result<Vec<String>, RuntimeError> {
    let mut v = vec!["build".to_string()];
    v.extend(engine.platform_args(arch)?);
    v.extend(["-t".into(), tag.into(), ctx.to_string_lossy().into_owned()]);
    Ok(v)
}

/// Resource and isolation flags for a run.
#[derive(Debug, Clone)]
pub struct RunLimits {
    /// Memory cap in MiB.
    pub ram_mb: u32,
    /// CPU share in percent of one core.
    pub cpu_pct: u32,
}

/// Everything a `run` needs.
#[derive(Debug, Clone)]
pub struct RunSpec<'a> {
    /// Container name.
    pub name: &'a str,
    /// Image tag.
    pub tag: &'a str,
    /// Package arch.
    pub arch: &'a str,
    /// Limits.
    pub limits: RunLimits,
    /// Env file (0600, holds the token).
    pub env_file: &'a Path,
    /// `(host address, host UDP port, container feed port)` to publish.
    pub feed_publish: Option<(std::net::IpAddr, u16, u16)>,
    /// Network to attach, if not the engine default.
    pub network: Option<&'a str>,
    /// Detached (instances) or foreground (console runs).
    pub detach: bool,
    /// The image runs the ingest relay as entrypoint (starts as root with
    /// only [`super::container_relay::RELAY_CAPS`]; the cog drops to
    /// `nobody`).
    pub ingest_relay: bool,
    /// `uid:gid` to run the cog as, instead of the image's user (a
    /// non-relay image only: a relay image bakes its drop target in).
    pub user: Option<&'a str>,
    /// Host path of the broker socket, bind-mounted at
    /// [`super::host_contract::COGBUS_IN_CONTAINER`].
    pub cogbus_mount: Option<&'a Path>,
    /// Cog arguments.
    pub args: &'a [String],
}

/// `run` command.
pub fn run_cmd(engine: Engine, s: &RunSpec<'_>) -> Result<Vec<String>, RuntimeError> {
    let mut v = vec!["run".to_string()];
    v.push(if s.detach { "-d" } else { "--rm" }.into());
    v.extend(["--name".into(), s.name.into()]);
    v.extend(engine.platform_args(s.arch)?);
    v.extend([
        "--memory".into(),
        format!("{}M", s.limits.ram_mb),
        "--cpus".into(),
        match engine {
            // Apple allocates whole vCPUs.
            Engine::Apple => s.limits.cpu_pct.div_ceil(100).max(1).to_string(),
            _ => format!("{:.2}", f64::from(s.limits.cpu_pct) / 100.0),
        },
        "--read-only".into(),
        "--cap-drop".into(),
        "ALL".into(),
    ]);
    if s.ingest_relay {
        for c in super::container_relay::RELAY_CAPS {
            v.extend(["--cap-add".into(), c.into()]);
        }
    }
    v.extend([
        "--tmpfs".into(),
        DATA_DIR.into(),
        "--env-file".into(),
        s.env_file.to_string_lossy().into_owned(),
        "--label".into(),
        "weftos.workload=cog".into(),
    ]);
    match engine {
        // Apple `container` has neither `--pids-limit` nor
        // `--security-opt`. The task cap is RLIMIT_NPROC instead: the cog
        // runs as a non-root user alone in its VM, so the per-user limit
        // caps its processes and threads. no_new_privs comes from the relay
        // (when present); otherwise the cog runs as `nobody` with every
        // capability dropped.
        Engine::Apple => v.extend([
            "--ulimit".into(),
            format!("nproc={PIDS_LIMIT}:{PIDS_LIMIT}"),
        ]),
        _ => v.extend([
            "--pids-limit".into(),
            PIDS_LIMIT.to_string(),
            "--security-opt".into(),
            "no-new-privileges".into(),
        ]),
    }
    if let Some((ip, host, inner)) = s.feed_publish {
        let addr = match ip {
            std::net::IpAddr::V4(a) => a.to_string(),
            std::net::IpAddr::V6(a) => format!("[{a}]"),
        };
        v.extend(["-p".into(), format!("{addr}:{host}:{inner}/udp")]);
    }
    if let Some(n) = s.network {
        v.extend(["--network".into(), n.into()]);
    }
    if let Some(u) = s.user {
        v.extend(["--user".into(), u.into()]);
    }
    if let Some(sock) = s.cogbus_mount {
        v.extend([
            "--mount".into(),
            format!(
                "type=bind,src={},dst={}",
                sock.to_string_lossy(),
                super::host_contract::COGBUS_IN_CONTAINER
            ),
        ]);
    }
    v.push(s.tag.into());
    v.extend(s.args.iter().cloned());
    Ok(v)
}

/// `stop` command.
pub fn stop_cmd(name: &str, grace: Duration) -> Vec<String> {
    vec![
        "stop".into(),
        "-t".into(),
        grace.as_secs().max(1).to_string(),
        name.into(),
    ]
}

/// Force-remove command.
pub fn rm_cmd(engine: Engine, name: &str) -> Vec<String> {
    match engine {
        Engine::Apple => vec!["delete".into(), "--force".into(), name.into()],
        _ => vec!["rm".into(), "-f".into(), name.into()],
    }
}

/// Remove-image command.
pub fn rmi_cmd(engine: Engine, tag: &str) -> Vec<String> {
    match engine {
        Engine::Apple => vec!["image".into(), "delete".into(), tag.into()],
        _ => vec!["rmi".into(), tag.into()],
    }
}

/// Parse `(running, exit_code)` from `inspect` output.
pub fn parse_inspect(engine: Engine, out: &str) -> Option<(String, Option<i32>)> {
    let v: serde_json::Value = serde_json::from_str(out.trim()).ok()?;
    let first = v.as_array().and_then(|a| a.first()).unwrap_or(&v);
    match engine {
        Engine::Apple => {
            // container 1.0: {"status": {"state": "running", ...}};
            // earlier releases: {"status": "running"}.
            let st = first.get("status")?;
            let status = st.get("state").unwrap_or(st).as_str()?.to_string();
            Some((status, None))
        }
        _ => {
            let st = first.get("State")?;
            let status = st.get("Status")?.as_str()?.to_string();
            let code = st
                .get("ExitCode")
                .and_then(|c| c.as_i64())
                .map(|c| c as i32);
            Some((status, code))
        }
    }
}

/// Output of one engine CLI call.
#[derive(Debug, Clone, Default)]
pub struct CmdOutput {
    /// Exit status (`None` when killed on timeout).
    pub status: Option<i32>,
    /// Stdout (capped by the runner).
    pub stdout: String,
    /// Stderr (capped by the runner).
    pub stderr: String,
    /// Total stdout bytes.
    pub stdout_bytes: u64,
    /// Total stderr bytes.
    pub stderr_bytes: u64,
    /// Whether the call hit its timeout.
    pub timed_out: bool,
}

/// Runs engine CLI commands. Mocked in unit tests; [`SystemRunner`] in use.
#[async_trait]
pub trait CommandRunner: Send + Sync {
    /// Run `program args`, capturing at most `limit` bytes per stream.
    async fn run(
        &self,
        program: &str,
        args: &[String],
        timeout: Duration,
        limit: usize,
    ) -> Result<CmdOutput, RuntimeError>;
}

/// Runs commands with `tokio::process`.
pub struct SystemRunner;

#[async_trait]
impl CommandRunner for SystemRunner {
    async fn run(
        &self,
        program: &str,
        args: &[String],
        timeout: Duration,
        limit: usize,
    ) -> Result<CmdOutput, RuntimeError> {
        let mut child = tokio::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| RuntimeError::Backend(format!("{program}: {e}")))?;
        let (o, e) = (child.stdout.take(), child.stderr.take());
        let work = async { tokio::join!(capture(o, limit), capture(e, limit), child.wait()) };
        match tokio::time::timeout(timeout, work).await {
            Ok((out, err, st)) => Ok(CmdOutput {
                status: st.ok().and_then(|s| s.code()),
                stdout: out.text(),
                stderr: err.text(),
                stdout_bytes: out.total(),
                stderr_bytes: err.total(),
                timed_out: false,
            }),
            Err(_) => Ok(CmdOutput {
                timed_out: true,
                ..CmdOutput::default()
            }),
        }
    }
}

async fn capture<R: tokio::io::AsyncRead + Unpin>(
    s: Option<R>,
    limit: usize,
) -> super::evidence::Capture {
    use tokio::io::AsyncReadExt;
    let mut cap = super::evidence::Capture::new(limit);
    if let Some(mut s) = s {
        let mut buf = [0u8; 8192];
        while let Ok(n) = s.read(&mut buf).await {
            if n == 0 {
                break;
            }
            cap.push(&buf[..n]);
        }
    }
    cap
}

/// Container-name-safe form of an instance id.
pub fn container_name(iid: &str) -> String {
    let s: String = iid
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    format!("weftos-{s}")
}

/// Allowance on top of `max_runtime_secs` for container creation / VM boot.
pub const ENGINE_STARTUP_SECS: u64 = 20;

pub(super) fn evidence_from(
    out: &CmdOutput,
    args: &[String],
    exit: Option<i32>,
    t0: std::time::Instant,
) -> RunEvidence {
    let limit_hit =
        out.stdout_bytes > out.stdout.len() as u64 || out.stderr_bytes > out.stderr.len() as u64;
    RunEvidence {
        args: args.to_vec(),
        exit_code: exit,
        elapsed_ms: t0.elapsed().as_millis() as u64,
        stdout: out.stdout.clone(),
        stderr: out.stderr.clone(),
        stdout_bytes: out.stdout_bytes,
        stderr_bytes: out.stderr_bytes,
        truncated: limit_hit,
        ..RunEvidence::default()
    }
}

pub(super) fn write_context(
    ctx: &std::path::Path,
    env_file: &std::path::Path,
    bytes: &[u8],
    dockerfile: &str,
    env: &[(String, String)],
) -> Result<(), RuntimeError> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let io = |e: std::io::Error| RuntimeError::Backend(format!("build context: {e}"));
    std::fs::create_dir_all(ctx).map_err(io)?;
    std::fs::write(ctx.join("Dockerfile"), dockerfile).map_err(io)?;
    let bin = ctx.join("cog");
    std::fs::write(&bin, bytes).map_err(io)?;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o555)).map_err(io)?;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(env_file)
        .map_err(io)?;
    for (k, v) in env {
        if v.contains('\n') {
            return Err(RuntimeError::InvalidConfig(format!(
                "{k} contains a newline"
            )));
        }
        writeln!(f, "{k}={v}").map_err(io)?;
    }
    Ok(())
}
