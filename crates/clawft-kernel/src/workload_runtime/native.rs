//! Native-process adapter (ADR-100 section 3): the verified binary is
//! written under the adapter's data root and run unprivileged under
//! `[console]` / `[resources]` limits by [`super::supervise`].

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use clawft_types::placement::{AttrValue, Capability, CapabilityId, Provenance};
use tokio::sync::Mutex;

use super::cog_spec::CogSpec;
use super::evidence::RunEvidence;
use super::host_contract::HostContract;
use super::supervise::{LaunchSpec, ProcLimits, Supervised};
use super::types::{
    Admission, ControlMode, InstanceHandle, InstanceState, InstanceStatus, RuntimeError,
    SignedPayload, VerifiedWorkload, WorkloadConfig, WorkloadRuntime,
};
use crate::workload_governance::NetworkPolicy;

/// Adapter id.
pub const NATIVE_ID: &str = "native";

/// Package arch name for this host, if cogs are built for it.
pub fn host_arch() -> Option<&'static str> {
    match std::env::consts::ARCH {
        "aarch64" => Some("aarch64"),
        "arm" => Some("armv7"),
        "x86_64" => Some("x86_64"),
        _ => None,
    }
}

/// ELF `e_machine` for a package arch.
pub fn elf_machine(arch: &str) -> Option<u16> {
    match arch {
        "aarch64" => Some(183),
        "armv7" => Some(40),
        "x86_64" => Some(62),
        _ => None,
    }
}

/// What kind of executable a payload is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadKind {
    /// ELF with this `e_machine`.
    Elf(u16),
    /// `#!` script.
    Script,
    /// Anything else.
    Unknown,
}

/// Classify payload bytes.
pub fn classify(bytes: &[u8]) -> PayloadKind {
    if bytes.len() >= 20 && bytes[..4] == *b"\x7fELF" {
        let m = [bytes[18], bytes[19]];
        let machine = if bytes[5] == 2 {
            u16::from_be_bytes(m)
        } else {
            u16::from_le_bytes(m)
        };
        PayloadKind::Elf(machine)
    } else if bytes.starts_with(b"#!") {
        PayloadKind::Script
    } else {
        PayloadKind::Unknown
    }
}

/// Native adapter configuration.
#[derive(Debug, Clone)]
pub struct NativeConfig {
    /// Root under which instance directories are created.
    pub root: PathBuf,
    /// `(uid, gid)` to drop to when the host runs as root.
    pub run_as: Option<(u32, u32)>,
    /// Accept signed `#!` script payloads (development and tests only;
    /// production packages carry ELF binaries).
    pub allow_interpreted: bool,
}

struct Instance {
    dir: PathBuf,
    program: PathBuf,
    spec: CogSpec,
    args: Vec<String>,
    host: HostContract,
    proc: Option<Supervised>,
    last: Option<RunEvidence>,
}

/// Native-process runtime.
pub struct NativeRuntime {
    cfg: NativeConfig,
    instances: Mutex<HashMap<String, Instance>>,
}

impl NativeRuntime {
    /// New adapter rooted at `cfg.root`.
    pub fn new(cfg: NativeConfig) -> Self {
        Self {
            cfg,
            instances: Mutex::new(HashMap::new()),
        }
    }

    fn pick<'a>(&self, p: &'a SignedPayload) -> Result<(String, &'a [u8], bool), RuntimeError> {
        let host = host_arch()
            .ok_or_else(|| RuntimeError::AdmissionRefused("unsupported host arch".into()))?;
        if let Some(b) = p.binaries.get(host) {
            return Ok((host.to_string(), b.bytes.as_slice(), false));
        }
        Err(RuntimeError::AdmissionRefused(format!(
            "package has no {host} binary (has: {}); native never emulates",
            p.binaries.keys().cloned().collect::<Vec<_>>().join(",")
        )))
    }

    fn check_payload(&self, arch: &str, bytes: &[u8]) -> Result<String, RuntimeError> {
        let refuse = |m: String| Err(RuntimeError::AdmissionRefused(m));
        match classify(bytes) {
            PayloadKind::Elf(m) => {
                if std::env::consts::OS != "linux" {
                    return refuse(format!(
                        "native on {} cannot run Linux ELF binaries",
                        std::env::consts::OS
                    ));
                }
                if Some(m) != elf_machine(arch) {
                    return refuse(format!("ELF machine {m} is not {arch}"));
                }
                Ok(format!("ELF {arch} on linux"))
            }
            PayloadKind::Script if self.cfg.allow_interpreted => Ok("interpreted payload".into()),
            PayloadKind::Script => refuse("script payloads are disabled".into()),
            PayloadKind::Unknown => refuse("payload is not an executable".into()),
        }
    }

    fn launch(
        &self,
        inst: &Instance,
        args: Vec<String>,
        cpu_secs: Option<u64>,
    ) -> Result<Supervised, RuntimeError> {
        let data = inst.dir.join("data");
        let mut limits = ProcLimits::for_ram_mb(inst.spec.resources.ram_mb);
        limits.cpu_secs = cpu_secs;
        Supervised::spawn(LaunchSpec {
            program: inst.program.clone(),
            args,
            env: inst.host.env(&data.to_string_lossy()),
            cwd: data,
            limits,
            output_limit: inst.spec.console.output_limit_bytes,
            run_as: self.cfg.run_as,
        })
    }

    fn stamp(&self, h: &InstanceHandle, mut ev: RunEvidence) -> RunEvidence {
        ev.runtime = NATIVE_ID.into();
        ev.instance_id = h.instance_id.clone();
        ev
    }
}

/// Instance id: `<cog>-<8 hex of config hash>-<node>`.
pub fn instance_id(w: &VerifiedWorkload, cfg: &WorkloadConfig) -> Result<String, RuntimeError> {
    if !crate::workload_pkg::manifest::valid_token(&cfg.node_id, 64) {
        return Err(RuntimeError::InvalidConfig(
            "node id must be a plain token".into(),
        ));
    }
    let mut h = blake3::Hasher::new();
    h.update(w.id.as_bytes());
    h.update(w.version.as_bytes());
    if let Ok(p) = w.signed("") {
        h.update(p.package_id.as_bytes());
    }
    h.update(
        serde_json::to_string(&cfg.mode)
            .unwrap_or_default()
            .as_bytes(),
    );
    h.update(cfg.args.join("\u{1f}").as_bytes());
    h.update(cfg.host.csi_bind.to_string().as_bytes());
    // Another project (or controller) placing the same package must not land in
    // this instance's directory and inherit its data/. Unowned ids are unchanged.
    if let Some(owner) = &cfg.host.owner {
        h.update(b"\x1fowner\x1f");
        h.update(owner.as_bytes());
    }
    Ok(format!(
        "{}-{}-{}",
        w.id,
        &h.finalize().to_hex()[..8],
        cfg.node_id
    ))
}

#[async_trait]
impl WorkloadRuntime for NativeRuntime {
    fn id(&self) -> &str {
        NATIVE_ID
    }

    fn provides(&self) -> Vec<Capability> {
        let arches: Vec<AttrValue> = host_arch().into_iter().map(AttrValue::from).collect();
        let Ok(id) = CapabilityId::new("runtime.native") else {
            return Vec::new();
        };
        vec![
            Capability::new(id, Provenance::Probed)
                .with_attr("arches_native", AttrValue::List(arches))
                .with_attr("arches_emulated", AttrValue::List(Vec::new()))
                .with_attr("os", std::env::consts::OS),
        ]
    }

    async fn admit(&self, w: &VerifiedWorkload) -> Result<Admission, RuntimeError> {
        let p = w.signed(NATIVE_ID)?;
        let (arch, bytes, emulated) = self.pick(p)?;
        let mut notes = vec![self.check_payload(&arch, bytes)?];
        if super::supervise::running_as_root() && self.cfg.run_as.is_none() {
            return Err(RuntimeError::AdmissionRefused(
                "host runs as root and no unprivileged run_as user is configured".into(),
            ));
        }
        notes.push(format!(
            "limits: ram {} MiB, console {} s, output {} B",
            p.spec.resources.ram_mb,
            p.spec.console.max_runtime_secs,
            p.spec.console.output_limit_bytes
        ));
        Ok(Admission {
            runtime: NATIVE_ID.into(),
            arch,
            emulated,
            notes,
        })
    }

    async fn load(
        &self,
        w: &VerifiedWorkload,
        cfg: &WorkloadConfig,
    ) -> Result<InstanceHandle, RuntimeError> {
        // Checked before admission: a config error on any host.
        if cfg
            .host
            .ingest_upstream
            .is_some_and(|u| u.to_string() != super::container_relay::COG_INGEST_ADDR)
        {
            return Err(RuntimeError::InvalidConfig(
                "a native cog posts to this node's 127.0.0.1:80; run the ingest bridge there"
                    .into(),
            ));
        }
        let adm = self.admit(w).await?;
        let p = w.signed(NATIVE_ID)?;
        cfg.mode.validate()?;
        cfg.host.validate()?;
        p.spec.validate_args(&cfg.args)?;
        let iid = instance_id(w, cfg)?;
        let mut map = self.instances.lock().await;
        if map.contains_key(&iid) {
            return Err(RuntimeError::InvalidState(format!(
                "{iid} is already loaded"
            )));
        }
        let dir = self.cfg.root.join(&iid);
        let program = dir.join(format!("cog-{}", w.id));
        let bytes = p.binaries[&adm.arch].bytes.clone();
        let (d, prog) = (dir.clone(), program.clone());
        let run_as = self.cfg.run_as;
        tokio::task::spawn_blocking(move || stage(&d, &prog, &bytes, run_as))
            .await
            .map_err(|e| RuntimeError::Backend(e.to_string()))??;
        let mut args = cfg.mode.args();
        args.extend(cfg.args.iter().cloned());
        map.insert(
            iid.clone(),
            Instance {
                dir,
                program,
                spec: p.spec.clone(),
                args,
                host: cfg.host.clone(),
                proc: None,
                last: None,
            },
        );
        Ok(InstanceHandle {
            runtime: NATIVE_ID.into(),
            instance_id: iid,
            workload_id: w.id.clone(),
            store_installed: false,
        })
    }

    async fn staged_payload(&self, h: &InstanceHandle) -> Option<std::path::PathBuf> {
        self.instances.lock().await.get(&h.instance_id).map(|i| i.program.clone())
    }

    async fn start(&self, h: &InstanceHandle) -> Result<(), RuntimeError> {
        let mut map = self.instances.lock().await;
        let inst = map
            .get_mut(&h.instance_id)
            .ok_or_else(|| RuntimeError::UnknownInstance(h.instance_id.clone()))?;
        if let Some(p) = inst.proc.as_mut()
            && p.try_exit().is_none()
        {
            return Err(RuntimeError::InvalidState("already running".into()));
        }
        let once = inst.args.first().map(String::as_str) == Some("--once");
        let cpu = once.then_some(inst.spec.console.max_runtime_secs + 1);
        let proc = self.launch(inst, inst.args.clone(), cpu)?;
        inst.proc = Some(proc);
        Ok(())
    }

    async fn stop(&self, h: &InstanceHandle, grace: Duration) -> Result<RunEvidence, RuntimeError> {
        let mut map = self.instances.lock().await;
        let inst = map
            .get_mut(&h.instance_id)
            .ok_or_else(|| RuntimeError::UnknownInstance(h.instance_id.clone()))?;
        let Some(proc) = inst.proc.take() else {
            return inst
                .last
                .clone()
                .ok_or_else(|| RuntimeError::InvalidState("not started".into()));
        };
        let ev = self.stamp(h, proc.terminate(grace).await);
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
        if let Some(p) = inst.proc {
            p.terminate(Duration::from_secs(2)).await;
        }
        tokio::fs::remove_dir_all(&inst.dir)
            .await
            .map_err(|e| RuntimeError::Backend(format!("remove instance dir: {e}")))
    }

    async fn status(&self, h: &InstanceHandle) -> InstanceStatus {
        let mut map = self.instances.lock().await;
        let Some(inst) = map.get_mut(&h.instance_id) else {
            return InstanceStatus::of(InstanceState::Unknown);
        };
        match inst.proc.as_mut() {
            None if inst.last.is_some() => InstanceStatus {
                state: InstanceState::Exited,
                exit_code: inst.last.as_ref().and_then(|e| e.exit_code),
                detail: None,
            },
            None => InstanceStatus::of(InstanceState::Loaded),
            Some(p) => match p.try_exit() {
                None => InstanceStatus::of(InstanceState::Running),
                Some(s) => InstanceStatus {
                    state: InstanceState::Exited,
                    exit_code: s.code(),
                    detail: None,
                },
            },
        }
    }

    fn control_mode(&self) -> ControlMode {
        ControlMode::Managed
    }

    /// A native process has the host's network: nothing here restricts
    /// egress yet (nftables / landlock are deferred, ADR-100 section 4),
    /// so the gate is told `egress`.
    fn network_exposure(&self) -> NetworkPolicy {
        NetworkPolicy::Egress
    }

    async fn console(
        &self,
        h: &InstanceHandle,
        command: &str,
    ) -> Result<RunEvidence, RuntimeError> {
        let proc = {
            let mut map = self.instances.lock().await;
            let inst = map
                .get_mut(&h.instance_id)
                .ok_or_else(|| RuntimeError::UnknownInstance(h.instance_id.clone()))?;
            if let Some(p) = inst.proc.as_mut()
                && p.try_exit().is_none()
            {
                return Err(RuntimeError::InvalidState(
                    "stop the running instance before a console run (sensor feed contention)"
                        .into(),
                ));
            }
            let args = inst.spec.console_args(command)?;
            let max = inst.spec.console.max_runtime_secs;
            (self.launch(inst, args, Some(max + 1))?, max)
        };
        let ev = proc.0.wait_for(Duration::from_secs(proc.1)).await;
        Ok(self.stamp(h, ev))
    }
}

fn stage(
    dir: &std::path::Path,
    program: &std::path::Path,
    bytes: &[u8],
    run_as: Option<(u32, u32)>,
) -> Result<(), RuntimeError> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let io = |e: std::io::Error| RuntimeError::Backend(format!("stage {}: {e}", dir.display()));
    // `load` calls this only for an instance this adapter does not hold, so what
    // is already here is left over from an earlier run (a daemon that restarted,
    // a crash mid-stage). Instance ids are deterministic, so it is the same
    // instance's directory: its `data/` is the cog's own state and is kept. The
    // rest (the read-only binary, anything else) is replaced. Nothing is followed
    // through a symlink: a link in the way is removed, not entered.
    match std::fs::symlink_metadata(dir) {
        Ok(m) if m.is_dir() => {
            for entry in std::fs::read_dir(dir).map_err(io)? {
                let entry = entry.map_err(io)?;
                let path = entry.path();
                if entry.file_name() == "data" {
                    continue;
                }
                let kind = entry.file_type().map_err(io)?;
                if kind.is_dir() {
                    std::fs::remove_dir_all(&path).map_err(io)?;
                } else {
                    std::fs::remove_file(&path).map_err(io)?;
                }
            }
        }
        Ok(_) => std::fs::remove_file(dir).map_err(io)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(io(e)),
    }
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(dir)
        .map_err(io)?;
    let data = dir.join("data");
    // `data/` is kept only if it is a real directory; a link or file there is replaced.
    match std::fs::symlink_metadata(&data) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => {
            std::fs::remove_file(&data).map_err(io)?;
            std::fs::DirBuilder::new().mode(0o700).create(&data).map_err(io)?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::DirBuilder::new().mode(0o700).create(&data).map_err(io)?;
        }
        Err(e) => return Err(io(e)),
    }
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).map_err(io)?;
    std::fs::write(program, bytes).map_err(io)?;
    std::fs::set_permissions(program, std::fs::Permissions::from_mode(0o555)).map_err(io)?;
    if let Some((uid, gid)) = run_as {
        std::os::unix::fs::chown(&data, Some(uid), Some(gid)).map_err(io)?;
    }
    Ok(())
}
