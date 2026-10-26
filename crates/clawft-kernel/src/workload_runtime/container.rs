//! Container adapters: Apple `container`, Docker / OrbStack and Podman
//! (ADR-100 section 3). This is the real start path for cog workloads; the
//! simulated start in [`crate::container::ContainerManager`] remains for
//! service-registry tests only.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use clawft_types::placement::{AttrValue, Capability, CapabilityId, Provenance};
use tokio::sync::Mutex;

use super::cog_spec::CogSpec;
use super::container_cmd::{
    self as cmd, CommandRunner, DATA_DIR, Engine, RunLimits, RunSpec, evidence_from, write_context,
};
pub use super::container_cmd::{ENGINE_STARTUP_SECS, container_name};
use super::container_relay as relay;
use super::evidence::RunEvidence;
use super::native::{PayloadKind, classify, elf_machine, instance_id};
use super::types::{
    Admission, ControlMode, InstanceHandle, InstanceState, InstanceStatus, RuntimeError,
    VerifiedWorkload, WorkloadConfig, WorkloadRuntime,
};
use crate::workload_governance::NetworkPolicy;

/// Container adapter configuration.
#[derive(Debug, Clone)]
pub struct ContainerRuntimeConfig {
    /// Engine.
    pub engine: Engine,
    /// Operator-pinned base image (`name@sha256:...`), present locally.
    pub base_image: String,
    /// Directory for build contexts and env files.
    pub work_root: PathBuf,
    /// Arches the engine runs natively (`aarch64` on Apple silicon).
    pub arches_native: Vec<String>,
    /// Arches it can run under emulation (`armv7` on OrbStack).
    pub arches_emulated: Vec<String>,
    /// Operator opt-in to emulated placement (never automatic).
    pub allow_emulated: bool,
    /// Host UDP port to publish to the cog's feed port, if any.
    pub feed_host_port: Option<u16>,
    /// Host address the feed port is published on. Loopback by default;
    /// set a LAN address (or `0.0.0.0`) so an ESP32 elsewhere on the LAN
    /// can reach a containerized cog.
    pub feed_publish_ip: std::net::IpAddr,
    /// Network to attach, if not the engine default.
    pub network: Option<String>,
    /// Engine variant (`orbstack`, `engine`, `desktop`) for `provides()`.
    pub variant: Option<String>,
    /// Engine version for `provides()`.
    pub version: Option<String>,
    /// Timeout for build / run / stop CLI calls.
    pub cli_timeout: Duration,
}

impl ContainerRuntimeConfig {
    /// Defaults for an Apple-silicon Mac.
    pub fn new(
        engine: Engine,
        base_image: impl Into<String>,
        work_root: impl Into<PathBuf>,
    ) -> Self {
        Self {
            engine,
            base_image: base_image.into(),
            work_root: work_root.into(),
            arches_native: vec!["aarch64".into()],
            arches_emulated: Vec::new(),
            allow_emulated: false,
            feed_host_port: None,
            feed_publish_ip: std::net::IpAddr::from([127, 0, 0, 1]),
            network: None,
            variant: None,
            version: None,
            cli_timeout: Duration::from_secs(300),
        }
    }
}

struct Instance {
    name: String,
    tag: String,
    arch: String,
    dir: PathBuf,
    env_file: PathBuf,
    spec: CogSpec,
    args: Vec<String>,
    csi_port: u16,
    relay: bool,
    /// `uid:gid` the instance runs as when the broker allocated one and the
    /// image does not bake it in (no relay).
    user: Option<String>,
    /// Host path of the broker socket mounted into the container.
    cogbus_mount: Option<PathBuf>,
    started: bool,
    last: Option<RunEvidence>,
}

/// A container-engine runtime.
pub struct ContainerRuntime {
    cfg: ContainerRuntimeConfig,
    runner: Arc<dyn CommandRunner>,
    instances: Mutex<HashMap<String, Instance>>,
}

impl ContainerRuntime {
    /// New adapter.
    pub fn new(cfg: ContainerRuntimeConfig, runner: Arc<dyn CommandRunner>) -> Self {
        Self {
            cfg,
            runner,
            instances: Mutex::new(HashMap::new()),
        }
    }

    async fn call(&self, args: Vec<String>, limit: usize) -> Result<cmd::CmdOutput, RuntimeError> {
        self.runner
            .run(self.cfg.engine.cli(), &args, self.cfg.cli_timeout, limit)
            .await
    }

    async fn call_ok(&self, args: Vec<String>, what: &str) -> Result<cmd::CmdOutput, RuntimeError> {
        let out = self.call(args, 64 * 1024).await?;
        if out.status != Some(0) {
            let tail: String = out
                .stderr
                .chars()
                .rev()
                .take(400)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            return Err(RuntimeError::Backend(format!(
                "{} {what} failed ({:?}): {tail}",
                self.cfg.engine.cli(),
                out.status
            )));
        }
        Ok(out)
    }

    fn stamp(&self, iid: &str, mut ev: RunEvidence) -> RunEvidence {
        ev.runtime = self.cfg.engine.runtime_id().into();
        ev.instance_id = iid.into();
        ev
    }

    fn run_spec<'a>(
        &'a self,
        inst: &'a Instance,
        name: &'a str,
        detach: bool,
        args: &'a [String],
    ) -> RunSpec<'a> {
        RunSpec {
            name,
            tag: &inst.tag,
            arch: &inst.arch,
            limits: RunLimits {
                ram_mb: inst.spec.resources.ram_mb,
                cpu_pct: inst.spec.resources.cpu_pct,
            },
            env_file: &inst.env_file,
            feed_publish: self
                .cfg
                .feed_host_port
                .map(|h| (self.cfg.feed_publish_ip, h, inst.csi_port)),
            network: self.cfg.network.as_deref(),
            detach,
            ingest_relay: inst.relay,
            user: inst.user.as_deref(),
            cogbus_mount: inst.cogbus_mount.as_deref(),
            args,
        }
    }

    async fn inspect(&self, name: &str) -> Option<(String, Option<i32>)> {
        let out = self
            .call(vec!["inspect".into(), name.into()], 256 * 1024)
            .await
            .ok()?;
        if out.status != Some(0) {
            return None;
        }
        cmd::parse_inspect(self.cfg.engine, &out.stdout)
    }
}

#[async_trait]
impl WorkloadRuntime for ContainerRuntime {
    fn id(&self) -> &str {
        self.cfg.engine.runtime_id()
    }

    fn provides(&self) -> Vec<Capability> {
        let list =
            |v: &[String]| AttrValue::List(v.iter().map(|a| AttrValue::from(a.as_str())).collect());
        let Ok(id) = CapabilityId::new(self.cfg.engine.capability_id()) else {
            return Vec::new();
        };
        let mut cap = Capability::new(id, Provenance::Probed)
            .with_attr("arches_native", list(&self.cfg.arches_native))
            .with_attr("arches_emulated", list(&self.cfg.arches_emulated));
        if let Some(v) = &self.cfg.variant {
            cap = cap.with_attr("variant", v.as_str());
        }
        if let Some(v) = &self.cfg.version {
            cap = cap.with_attr("version", v.as_str());
        }
        vec![cap]
    }

    async fn admit(&self, w: &VerifiedWorkload) -> Result<Admission, RuntimeError> {
        let rid = self.cfg.engine.runtime_id();
        let p = w.signed(rid)?;
        cmd::validate_base_image(&self.cfg.base_image)?;
        let native = self
            .cfg
            .arches_native
            .iter()
            .find(|a| p.binaries.contains_key(*a));
        let emulated = self
            .cfg
            .arches_emulated
            .iter()
            .find(|a| p.binaries.contains_key(*a));
        let (arch, emu) = match (native, emulated) {
            (Some(a), _) => (a.clone(), false),
            (None, Some(a)) if self.cfg.allow_emulated => (a.clone(), true),
            (None, Some(a)) => {
                return Err(RuntimeError::AdmissionRefused(format!(
                    "{a} runs only emulated here and emulation is operator opt-in"
                )));
            }
            (None, None) => {
                return Err(RuntimeError::AdmissionRefused(format!(
                    "{rid} runs none of the package arches"
                )));
            }
        };
        match classify(&p.binaries[&arch].bytes) {
            PayloadKind::Elf(m) if Some(m) == elf_machine(&arch) => {}
            other => {
                return Err(RuntimeError::AdmissionRefused(format!(
                    "{arch} payload is not a matching Linux ELF ({other:?})"
                )));
            }
        }
        Ok(Admission {
            runtime: rid.into(),
            arch,
            emulated: emu,
            notes: vec![format!("base {}", self.cfg.base_image)],
        })
    }

    async fn load(
        &self,
        w: &VerifiedWorkload,
        cfg: &WorkloadConfig,
    ) -> Result<InstanceHandle, RuntimeError> {
        let adm = self.admit(w).await?;
        let p = w.signed(self.id())?;
        cfg.mode.validate()?;
        cfg.host.validate()?;
        p.spec.validate_args(&cfg.args)?;
        let iid = instance_id(w, cfg)?;
        if self.instances.lock().await.contains_key(&iid) {
            return Err(RuntimeError::InvalidState(format!(
                "{iid} is already loaded"
            )));
        }
        let bin = &p.binaries[&adm.arch];
        let dir = self.cfg.work_root.join(&iid);
        let ctx = dir.join("ctx");
        let env_file = dir.join("env");
        let env = cfg.host.env(DATA_DIR);
        // ADR-122 gap 1: the broker's identity for this instance, if any.
        let ident = cfg.host.run_as.clone();
        let upstream = relay::relay_upstream(&cfg.host, self.cfg.network.as_deref())?;
        let df = match upstream {
            Some(up) => relay::dockerfile(&self.cfg.base_image, up, ident.as_ref().map(|i| (i.uid, i.gid))),
            None => cmd::dockerfile(&self.cfg.base_image),
        };
        let (bytes, c2, e2) = (bin.bytes.clone(), ctx.clone(), env_file.clone());
        tokio::task::spawn_blocking(move || {
            write_context(&c2, &e2, &bytes, &df, &env)?;
            match upstream {
                Some(_) => std::fs::write(c2.join(relay::RELAY_FILE), relay::RELAY_SCRIPT)
                    .map_err(|e| RuntimeError::Backend(format!("build context: {e}"))),
                None => Ok(()),
            }
        })
        .await
        .map_err(|e| RuntimeError::Backend(e.to_string()))??;
        let mut tag = cmd::image_tag(&w.id, &bin.blake3);
        if upstream.is_some()
            && let Some(i) = &ident
        {
            // A relay image bakes the identity in, so it is part of the tag.
            tag = format!("{tag}-u{}g{}", i.uid, i.gid);
        }
        let built = self
            .call_ok(
                cmd::build_cmd(self.cfg.engine, &adm.arch, &tag, &ctx)?,
                "build",
            )
            .await;
        if let Err(e) = built {
            let _ = tokio::fs::remove_dir_all(&dir).await;
            return Err(e);
        }
        let mut args = cfg.mode.args();
        args.extend(cfg.args.iter().cloned());
        self.instances.lock().await.insert(
            iid.clone(),
            Instance {
                name: container_name(&iid),
                tag,
                arch: adm.arch,
                dir,
                env_file,
                spec: p.spec.clone(),
                args,
                csi_port: cfg.host.csi_bind.port(),
                relay: upstream.is_some(),
                user: ident.as_ref().filter(|_| upstream.is_none()).map(cmd::user_of),
                cogbus_mount: cfg.host.cogbus_mount.as_ref().map(PathBuf::from),
                started: false,
                last: None,
            },
        );
        Ok(InstanceHandle {
            runtime: self.id().into(),
            instance_id: iid,
            workload_id: w.id.clone(),
            store_installed: false,
        })
    }

    async fn start(&self, h: &InstanceHandle) -> Result<(), RuntimeError> {
        let mut map = self.instances.lock().await;
        let inst = map
            .get_mut(&h.instance_id)
            .ok_or_else(|| RuntimeError::UnknownInstance(h.instance_id.clone()))?;
        if inst.started {
            return Err(RuntimeError::InvalidState("already started".into()));
        }
        let name = inst.name.clone();
        let args = inst.args.clone();
        let _ = self.call(cmd::rm_cmd(self.cfg.engine, &name), 4096).await;
        self.call_ok(
            cmd::run_cmd(self.cfg.engine, &self.run_spec(inst, &name, true, &args))?,
            "run",
        )
        .await?;
        inst.started = true;
        Ok(())
    }

    async fn stop(&self, h: &InstanceHandle, grace: Duration) -> Result<RunEvidence, RuntimeError> {
        let mut map = self.instances.lock().await;
        let inst = map
            .get_mut(&h.instance_id)
            .ok_or_else(|| RuntimeError::UnknownInstance(h.instance_id.clone()))?;
        if !inst.started {
            return inst
                .last
                .clone()
                .ok_or_else(|| RuntimeError::InvalidState("not started".into()));
        }
        let t0 = std::time::Instant::now();
        let _ = self.call(cmd::stop_cmd(&inst.name, grace), 4096).await;
        let exit = self.inspect(&inst.name).await.and_then(|(_, c)| c);
        let logs = self
            .call(
                vec!["logs".into(), inst.name.clone()],
                inst.spec.console.output_limit_bytes,
            )
            .await?;
        let _ = self
            .call(cmd::rm_cmd(self.cfg.engine, &inst.name), 4096)
            .await;
        inst.started = false;
        let ev = self.stamp(&h.instance_id, evidence_from(&logs, &inst.args, exit, t0));
        inst.last = Some(ev.clone());
        Ok(ev)
    }

    async fn unload(&self, h: InstanceHandle) -> Result<(), RuntimeError> {
        let inst = self
            .instances
            .lock()
            .await
            .remove(&h.instance_id)
            .ok_or_else(|| RuntimeError::UnknownInstance(h.instance_id.clone()))?;
        let _ = self
            .call(cmd::rm_cmd(self.cfg.engine, &inst.name), 4096)
            .await;
        let _ = self
            .call(cmd::rmi_cmd(self.cfg.engine, &inst.tag), 4096)
            .await;
        tokio::fs::remove_dir_all(&inst.dir)
            .await
            .map_err(|e| RuntimeError::Backend(format!("remove work dir: {e}")))
    }

    async fn status(&self, h: &InstanceHandle) -> InstanceStatus {
        let (name, started, last) = {
            let map = self.instances.lock().await;
            let Some(inst) = map.get(&h.instance_id) else {
                return InstanceStatus::of(InstanceState::Unknown);
            };
            (inst.name.clone(), inst.started, inst.last.clone())
        };
        if !started {
            return match last {
                Some(ev) => InstanceStatus {
                    state: InstanceState::Exited,
                    exit_code: ev.exit_code,
                    detail: None,
                },
                None => InstanceStatus::of(InstanceState::Loaded),
            };
        }
        match self.inspect(&name).await {
            Some((s, code)) if s == "running" => InstanceStatus {
                state: InstanceState::Running,
                exit_code: code,
                detail: Some(s),
            },
            Some((s, code)) if s == "exited" || s == "stopped" => InstanceStatus {
                state: InstanceState::Exited,
                exit_code: code,
                detail: Some(s),
            },
            Some((s, code)) => InstanceStatus {
                state: InstanceState::Degraded,
                exit_code: code,
                detail: Some(s),
            },
            None => InstanceStatus {
                state: InstanceState::Degraded,
                exit_code: None,
                detail: Some("inspect failed".into()),
            },
        }
    }

    fn control_mode(&self) -> ControlMode {
        ControlMode::Managed
    }

    /// `none` has no network. Every other network (the engine's default
    /// bridge, `host`, or a named one) can reach the internet as far as
    /// this adapter knows, so the gate is told `egress`.
    fn network_exposure(&self) -> NetworkPolicy {
        match self.cfg.network.as_deref() {
            Some("none") => NetworkPolicy::None,
            _ => NetworkPolicy::Egress,
        }
    }

    async fn console(
        &self,
        h: &InstanceHandle,
        command: &str,
    ) -> Result<RunEvidence, RuntimeError> {
        let (inst_args, spec_line, name, max, limit) = {
            let map = self.instances.lock().await;
            let inst = map
                .get(&h.instance_id)
                .ok_or_else(|| RuntimeError::UnknownInstance(h.instance_id.clone()))?;
            if inst.started {
                return Err(RuntimeError::InvalidState(
                    "stop the running instance before a console run (sensor feed contention)"
                        .into(),
                ));
            }
            let args = inst.spec.console_args(command)?;
            let name = format!("{}-console", inst.name);
            let line = cmd::run_cmd(self.cfg.engine, &self.run_spec(inst, &name, false, &args))?;
            (
                args,
                line,
                name,
                inst.spec.console.max_runtime_secs,
                inst.spec.console.output_limit_bytes,
            )
        };
        // The cap is the cog's wall clock plus a fixed allowance for the
        // engine to create (and, for Apple, boot) the container.
        let cap = Duration::from_secs(max + ENGINE_STARTUP_SECS);
        let t0 = std::time::Instant::now();
        let out = self
            .runner
            .run(self.cfg.engine.cli(), &spec_line, cap, limit)
            .await?;
        let mut ev = evidence_from(&out, &inst_args, out.status, t0);
        if out.timed_out {
            let _ = self.call(cmd::rm_cmd(self.cfg.engine, &name), 4096).await;
            ev.killed_for_timeout = true;
            ev.exit_code = None;
        }
        Ok(self.stamp(&h.instance_id, ev))
    }
}
