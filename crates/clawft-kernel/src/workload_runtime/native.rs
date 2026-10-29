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
use crate::workload_pkg::manifest::MACOS_AARCH64;
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

/// Binary key this host runs: [`MACOS_AARCH64`] on Apple Silicon macOS, else
/// the CPU arch ([`host_arch`]).
pub fn host_target() -> Option<&'static str> {
    if std::env::consts::OS == "macos" && std::env::consts::ARCH == "aarch64" {
        Some(MACOS_AARCH64)
    } else {
        host_arch()
    }
}

/// Mach-O `cputype` for arm64 (`CPU_TYPE_ARM64`).
pub const CPU_TYPE_ARM64: u32 = 0x0100_000c;

/// Mach-O `filetype` for an executable (`MH_EXECUTE`). Dylibs, bundles and
/// object files are not runnable payloads.
pub const MH_EXECUTE: u32 = 2;

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
    /// Thin 64-bit little-endian Mach-O executable (`MH_EXECUTE`, header of
    /// at least 16 bytes) with this `cputype`. Fat/universal, 32-bit,
    /// big-endian, dylib and truncated files are not recognised (ship a
    /// thin arm64 slice).
    MachO(u32),
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
    } else if bytes.len() >= 16
        && bytes[..4] == [0xcf, 0xfa, 0xed, 0xfe]
        && u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]) == MH_EXECUTE
    {
        PayloadKind::MachO(u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]))
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
    /// macOS route: no per-cog uid; the cog runs as the daemon user.
    daemon_user: bool,
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
        // Apple Silicon macOS prefers its Mach-O binary; every other host
        // (and a macOS host without one) looks up its CPU arch, as before.
        let key = host_target()
            .filter(|t| p.binaries.contains_key(*t))
            .unwrap_or(host);
        if let Some(b) = p.binaries.get(key) {
            return Ok((key.to_string(), b.bytes.as_slice(), false));
        }
        Err(RuntimeError::AdmissionRefused(format!(
            "package has no {key} binary (has: {}); native never emulates",
            p.binaries.keys().cloned().collect::<Vec<_>>().join(",")
        )))
    }

    fn check_payload(&self, arch: &str, bytes: &[u8]) -> Result<String, RuntimeError> {
        check_payload_on(std::env::consts::OS, self.cfg.allow_interpreted, arch, bytes)
    }

    /// Admission with `has_run_as` saying whether an unprivileged identity
    /// is available (the adapter's, or the instance's own).
    /// `instance_run_as` says the controller asked this instance for its own
    /// identity, which the macOS route cannot give.
    fn admit_as(
        &self,
        w: &VerifiedWorkload,
        has_run_as: bool,
        instance_run_as: bool,
    ) -> Result<Admission, RuntimeError> {
        let p = w.signed(NATIVE_ID)?;
        let (arch, bytes, emulated) = self.pick(p)?;
        let mut notes = vec![self.check_payload(&arch, bytes)?];
        if arch == MACOS_AARCH64
            && let Some(m) =
                macos_route_refusal(super::supervise::running_as_root(), instance_run_as)
        {
            return Err(RuntimeError::AdmissionRefused(m));
        }
        if super::supervise::running_as_root() && !has_run_as {
            return Err(RuntimeError::AdmissionRefused(
                "host runs as root and no unprivileged run_as user is configured".into(),
            ));
        }
        if arch == MACOS_AARCH64 {
            notes.push(
                "isolation: runs as the daemon user (no per-cog uid on macOS; \
                 ADR-122 section 4a does not apply)"
                    .into(),
            );
        }
        let ram = if arch == MACOS_AARCH64 {
            "ram not enforced on macOS (RLIMIT_AS is ignored there)".to_string()
        } else {
            format!("ram {} MiB", p.spec.resources.ram_mb)
        };
        notes.push(format!(
            "limits: {ram}, console {} s, output {} B",
            p.spec.console.max_runtime_secs, p.spec.console.output_limit_bytes
        ));
        Ok(Admission {
            runtime: NATIVE_ID.into(),
            arch,
            emulated,
            notes,
        })
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
        let (run_as, groups) = if inst.daemon_user {
            (None, Vec::new())
        } else {
            effective_identity(self.cfg.run_as, &inst.host)
        };
        Supervised::spawn(LaunchSpec {
            program: inst.program.clone(),
            args,
            env: inst.host.env(&data.to_string_lossy()),
            cwd: data,
            limits,
            output_limit: inst.spec.console.output_limit_bytes,
            run_as,
            groups,
        })
    }

    fn stamp(&self, h: &InstanceHandle, mut ev: RunEvidence) -> RunEvidence {
        ev.runtime = NATIVE_ID.into();
        ev.instance_id = h.instance_id.clone();
        ev
    }
}

/// Why the macOS route must be refused at admission, if it must: it has no
/// per-cog uid, so it cannot honour a requested identity, and a root daemon
/// would run the cog as root.
pub(super) fn macos_route_refusal(is_root: bool, run_as_requested: bool) -> Option<String> {
    if is_root {
        Some("the macOS route runs as the daemon user and is refused when the daemon runs as root".into())
    } else if run_as_requested {
        Some("a run_as identity was requested but the macOS route runs as the daemon user and cannot give one".into())
    } else {
        None
    }
}

/// Payload admission for binary key `arch` on a daemon running `host_os`.
/// The `aarch64-macos` key takes only thin arm64 Mach-O on a macOS daemon;
/// every other key takes only its Linux ELF (or a dev script).
pub(super) fn check_payload_on(
    host_os: &str,
    allow_interpreted: bool,
    arch: &str,
    bytes: &[u8],
) -> Result<String, RuntimeError> {
    let refuse = |m: String| Err(RuntimeError::AdmissionRefused(m));
    let macos_route = arch == MACOS_AARCH64;
    match classify(bytes) {
        PayloadKind::Elf(_) if macos_route => refuse("ELF payload on the macOS route".into()),
        PayloadKind::Elf(m) => {
            if host_os != "linux" {
                return refuse(format!("native on {host_os} cannot run Linux ELF binaries"));
            }
            if Some(m) != elf_machine(arch) {
                return refuse(format!("ELF machine {m} is not {arch}"));
            }
            Ok(format!("ELF {arch} on linux"))
        }
        PayloadKind::MachO(_) if !macos_route => {
            refuse(format!("Mach-O payload on the {arch} route (Linux ELF only)"))
        }
        PayloadKind::MachO(_) if host_os != "macos" => {
            refuse(format!("native on {host_os} cannot run Mach-O binaries"))
        }
        PayloadKind::MachO(c) if c != CPU_TYPE_ARM64 => {
            refuse(format!("Mach-O cputype {c:#x} is not arm64"))
        }
        PayloadKind::MachO(_) => Ok("Mach-O arm64 on macos".into()),
        PayloadKind::Script if allow_interpreted => Ok("interpreted payload".into()),
        PayloadKind::Script => refuse("script payloads are disabled".into()),
        PayloadKind::Unknown => refuse("payload is not an executable".into()),
    }
}

/// Instance id: `<cog>-<32 hex of the config and owner hash>-<node>`.
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
        &h.finalize().to_hex()[..32],
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
        self.admit_as(w, self.cfg.run_as.is_some(), false)
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
        let adm = self.admit_as(
            w,
            self.cfg.run_as.is_some() || cfg.host.run_as.is_some(),
            cfg.host.run_as.is_some(),
        )?;
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
        let daemon_user = adm.arch == MACOS_AARCH64;
        if daemon_user {
            tracing::warn!(
                cog = %w.id,
                "macOS native cog runs as the daemon user: no per-cog uid, no uid isolation"
            );
        }
        let run_as = if daemon_user {
            None
        } else {
            effective_identity(self.cfg.run_as, &cfg.host).0
        };
        let owner = cfg.host.owner.clone();
        tokio::task::spawn_blocking(move || stage(&d, &prog, &bytes, run_as, owner.as_deref()))
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
                daemon_user,
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

/// The `(uid, gid)` and supplementary groups an instance launches with: the
/// identity the broker allocated for it, else the adapter's `run_as`.
pub fn effective_identity(
    adapter: Option<(u32, u32)>,
    host: &HostContract,
) -> (Option<(u32, u32)>, Vec<u32>) {
    match &host.run_as {
        Some(id) => (Some((id.uid, id.gid)), id.groups.clone()),
        None => (adapter, Vec::new()),
    }
}

/// File in an instance directory naming its owner (the full BLAKE3 of the
/// owner key, hex). The instance id carries a truncated part of that hash, so
/// a controller that grinds a colliding id would otherwise inherit the kept
/// `data/` of another project's instance; the marker is compared in full.
pub(super) const OWNER_MARKER: &str = ".owner";

/// The full owner hash that goes in the marker.
pub(super) fn owner_hash(owner: &str) -> String {
    blake3::hash(owner.as_bytes()).to_hex().to_string()
}

/// What an instance directory's marker says.
enum Marker {
    /// Names this owner.
    Matches,
    /// Names another owner.
    Other,
    /// Absent, empty or not a full hash (a torn write): no owner is named.
    Missing,
}

fn read_marker(dir: &std::path::Path, want: &str) -> Marker {
    match std::fs::read_to_string(dir.join(OWNER_MARKER)) {
        Ok(t) => {
            let t = t.trim();
            if t == want {
                Marker::Matches
            } else if t.len() == 64 && t.bytes().all(|c| c.is_ascii_hexdigit()) {
                Marker::Other
            } else {
                Marker::Missing
            }
        }
        Err(_) => Marker::Missing,
    }
}

/// Write the marker so a reader never sees half of it and a crash never leaves
/// it empty: a temp file, fsynced, renamed over, then the directory fsynced.
fn write_marker(dir: &std::path::Path, hash: &str) -> std::io::Result<()> {
    use std::io::Write;
    let tmp = dir.join(".owner.tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(format!("{hash}\n").as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, dir.join(OWNER_MARKER))?;
    std::fs::File::open(dir)?.sync_all()
}

/// Make `path` (and what is under it, links not followed) belong to `uid:gid`.
fn chown_tree(path: &std::path::Path, uid: u32, gid: u32) -> std::io::Result<()> {
    std::os::unix::fs::lchown(path, Some(uid), Some(gid))?;
    if std::fs::symlink_metadata(path)?.is_dir() {
        for e in std::fs::read_dir(path)? {
            chown_tree(&e?.path(), uid, gid)?;
        }
    }
    Ok(())
}

#[cfg(test)]
thread_local! {
    /// Test hook: make the next `stage` on this thread fail right after it has
    /// cleared the old contents, as a crash there would.
    pub(super) static CRASH_AFTER_CLEAR: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub(super) fn stage(
    dir: &std::path::Path,
    program: &std::path::Path,
    bytes: &[u8],
    run_as: Option<(u32, u32)>,
    owner: Option<&str>,
) -> Result<(), RuntimeError> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let io = |e: std::io::Error| RuntimeError::Backend(format!("stage {}: {e}", dir.display()));
    let want = owner.map(owner_hash);
    let existing = std::fs::symlink_metadata(dir);
    let is_dir = existing.as_ref().is_ok_and(|m| m.is_dir());
    // An owned instance only ever reuses a directory that names the same owner
    // in full. Another owner's (or an unmarked one that holds data) is refused,
    // never emptied: it may be a live project's state.
    if is_dir {
        let has_marker = std::fs::symlink_metadata(dir.join(OWNER_MARKER)).is_ok();
        let has_state = std::fs::symlink_metadata(dir.join("data")).is_ok();
        match &want {
            None if has_marker => {
                return Err(RuntimeError::Backend(format!(
                    "stage {}: the instance directory belongs to an owner; an unowned instance does not reuse it",
                    dir.display()
                )));
            }
            Some(w) => match read_marker(dir, w) {
                Marker::Matches => {}
                Marker::Other => {
                    return Err(RuntimeError::Backend(format!(
                        "stage {}: the instance directory belongs to another owner (id collision); not reused",
                        dir.display()
                    )));
                }
                Marker::Missing if has_state => {
                    return Err(RuntimeError::Backend(format!(
                        "stage {}: the instance directory has data and no valid owner marker (it predates owner markers); remove it to place this instance",
                        dir.display()
                    )));
                }
                Marker::Missing => {}
            },
            None => {}
        }
    }
    // `load` calls this only for an instance this adapter does not hold, so what
    // is already here is left over from an earlier run (a daemon that restarted,
    // a crash mid-stage). Instance ids are deterministic, so it is the same
    // instance's directory: its `data/` is the cog's own state and is kept. The
    // rest (the read-only binary, anything else) is replaced. Nothing is followed
    // through a symlink: a link in the way is removed, not entered.
    if existing.is_ok() && !is_dir {
        std::fs::remove_file(dir).map_err(io)?;
    } else if let Err(e) = &existing
        && e.kind() != std::io::ErrorKind::NotFound
    {
        return Err(io(std::io::Error::new(e.kind(), e.to_string())));
    }
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(dir)
        .map_err(io)?;
    // The marker first, atomically, before anything is cleared: a crash from here
    // on leaves a directory that already names its owner. A matching marker is
    // never deleted.
    if let Some(w) = &want
        && !matches!(read_marker(dir, w), Marker::Matches)
    {
        write_marker(dir, w).map_err(io)?;
    }
    for entry in std::fs::read_dir(dir).map_err(io)? {
        let entry = entry.map_err(io)?;
        let name = entry.file_name();
        if name == "data" || name == OWNER_MARKER {
            continue;
        }
        let path = entry.path();
        if entry.file_type().map_err(io)?.is_dir() {
            std::fs::remove_dir_all(&path).map_err(io)?;
        } else {
            std::fs::remove_file(&path).map_err(io)?;
        }
    }
    #[cfg(test)]
    if CRASH_AFTER_CLEAR.with(|c| c.replace(false)) {
        return Err(RuntimeError::Backend("injected crash after the directory was cleared".into()));
    }
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
    // The cog's own state is private to the instance's uid; kept files from an
    // earlier run are brought to it too (no link is followed).
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).map_err(io)?;
    if let Some((uid, gid)) = run_as {
        chown_tree(&data, uid, gid).map_err(io)?;
    }
    std::fs::write(program, bytes).map_err(io)?;
    std::fs::set_permissions(program, std::fs::Permissions::from_mode(0o555)).map_err(io)?;
    Ok(())
}
