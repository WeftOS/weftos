//! The parts of a cog's `cog.toml` the runtime enforces (ADR-100 sections
//! 2-4): `[console]` limits, `[resources]` and the `[config]` CLI surface,
//! plus building a [`VerifiedWorkload`] from a verified package.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde::Deserialize;

use super::cog_devices::{DeviceDecl, RawDevice, parse_devices};
use super::cog_decl::{
    InputDecl, OutputDecl, RawInput, RawOutput, parse_inputs, parse_interfaces, parse_outputs,
};
use super::types::{Binary, RuntimeError, SignedPayload, VerifiedWorkload, WorkloadSource};
use crate::workload_pkg::manifest::valid_cog_id;
use crate::workload_pkg::verify::check_file;
use crate::workload_pkg::{FileSource, VerifiedPackage};

/// Default `[console].max_runtime_secs` when a cog does not set one.
pub const DEFAULT_MAX_RUNTIME_SECS: u64 = 15;
/// Largest `max_runtime_secs` honored.
pub const MAX_RUNTIME_SECS: u64 = 3600;
/// Default `[console].output_limit_bytes`.
pub const DEFAULT_OUTPUT_LIMIT: usize = 64 * 1024;
/// Largest output limit honored.
pub const MAX_OUTPUT_LIMIT: usize = 16 * 1024 * 1024;
/// Memory cap when `[resources].ram_mb` is absent.
pub const DEFAULT_RAM_MB: u32 = 256;
/// Largest `ram_mb` honored.
pub const MAX_RAM_MB: u32 = 64 * 1024;
/// Longest single argument value accepted.
pub const MAX_ARG_LEN: usize = 128;
/// Most extra arguments accepted.
pub const MAX_ARGS: usize = 32;

/// Enforced `[console]` limits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsoleLimits {
    /// Exact argument strings a console run may use.
    pub allowed_commands: Vec<String>,
    /// Wall-clock cap for a console run.
    pub max_runtime_secs: u64,
    /// Captured bytes per stream.
    pub output_limit_bytes: usize,
}

/// Enforced `[resources]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resources {
    /// Memory cap in MiB.
    pub ram_mb: u32,
    /// CPU share in percent of one core (1..=400).
    pub cpu_pct: u32,
}

/// Parsed, validated cog manifest subset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CogSpec {
    /// `[cog].id`.
    pub id: String,
    /// `[cog].version`.
    pub version: String,
    /// Console limits.
    pub console: ConsoleLimits,
    /// Resource limits.
    pub resources: Resources,
    /// `cli_arg` flags that take a value.
    pub value_args: BTreeSet<String>,
    /// `cli_arg` flags of boolean options (no value).
    pub flag_args: BTreeSet<String>,
    /// `[cog].interfaces`: what the native half opens (empty when absent).
    pub interfaces: Vec<String>,
    /// `[[outputs]]` topic declarations.
    pub outputs: Vec<OutputDecl>,
    /// `[[inputs]]` topic declarations.
    pub inputs: Vec<InputDecl>,
    /// `[[devices]]`: the devices this cog owns (see `cog_devices`).
    pub devices: Vec<DeviceDecl>,
}

#[derive(Deserialize)]
struct RawToml {
    cog: RawCog,
    #[serde(default)]
    console: Option<RawConsole>,
    #[serde(default)]
    resources: Option<RawResources>,
    #[serde(default)]
    config: BTreeMap<String, toml::Value>,
    #[serde(default)]
    outputs: Vec<RawOutput>,
    #[serde(default)]
    inputs: Vec<RawInput>,
    #[serde(default)]
    devices: Vec<RawDevice>,
}

#[derive(Deserialize)]
struct RawCog {
    id: String,
    version: String,
    interfaces: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct RawConsole {
    #[serde(default)]
    allowed_commands: Vec<String>,
    max_runtime_secs: Option<u64>,
    output_limit_bytes: Option<usize>,
}

#[derive(Deserialize)]
struct RawResources {
    ram_mb: Option<u32>,
    cpu_pct: Option<u32>,
}

fn bad(msg: impl Into<String>) -> RuntimeError {
    RuntimeError::InvalidConfig(msg.into())
}

fn valid_flag(s: &str) -> bool {
    s.len() > 2
        && s.len() <= 64
        && s.starts_with("--")
        && s[2..]
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// A plain argument value: printable, no whitespace or shell metacharacters.
pub fn valid_value(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_ARG_LEN
        && (!s.starts_with('-') || s.parse::<f64>().is_ok())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:-+/".contains(&b))
}

impl CogSpec {
    /// Parse `cog.toml` bytes.
    pub fn parse(bytes: &[u8]) -> Result<Self, RuntimeError> {
        let text = std::str::from_utf8(bytes).map_err(|_| bad("cog.toml is not UTF-8"))?;
        let raw: RawToml = toml::from_str(text).map_err(|e| bad(format!("cog.toml: {e}")))?;
        if !valid_cog_id(&raw.cog.id) {
            return Err(bad(format!("bad cog id {:?}", raw.cog.id)));
        }
        let console = raw.console.unwrap_or(RawConsole {
            allowed_commands: Vec::new(),
            max_runtime_secs: None,
            output_limit_bytes: None,
        });
        let max_runtime_secs = console.max_runtime_secs.unwrap_or(DEFAULT_MAX_RUNTIME_SECS);
        if !(1..=MAX_RUNTIME_SECS).contains(&max_runtime_secs) {
            return Err(bad(format!(
                "max_runtime_secs must be 1..={MAX_RUNTIME_SECS}"
            )));
        }
        let output_limit_bytes = console.output_limit_bytes.unwrap_or(DEFAULT_OUTPUT_LIMIT);
        if !(1..=MAX_OUTPUT_LIMIT).contains(&output_limit_bytes) {
            return Err(bad(format!(
                "output_limit_bytes must be 1..={MAX_OUTPUT_LIMIT}"
            )));
        }
        let res = raw.resources.unwrap_or(RawResources {
            ram_mb: None,
            cpu_pct: None,
        });
        let ram_mb = res.ram_mb.unwrap_or(DEFAULT_RAM_MB);
        let cpu_pct = res.cpu_pct.unwrap_or(100);
        if !(1..=MAX_RAM_MB).contains(&ram_mb) || !(1..=400).contains(&cpu_pct) {
            return Err(bad(
                "resources out of range (ram_mb 1..=65536, cpu_pct 1..=400)",
            ));
        }
        let (mut value_args, mut flag_args) = (BTreeSet::new(), BTreeSet::new());
        for (key, v) in &raw.config {
            let Some(arg) = v.get("cli_arg").and_then(|a| a.as_str()) else {
                continue;
            };
            if !valid_flag(arg) {
                return Err(bad(format!("config.{key}: bad cli_arg {arg:?}")));
            }
            if v.get("type").and_then(|t| t.as_str()) == Some("boolean") {
                flag_args.insert(arg.to_string());
            } else {
                value_args.insert(arg.to_string());
            }
        }
        let interfaces = parse_interfaces(raw.cog.interfaces)?;
        let outputs = parse_outputs(raw.outputs)?;
        let inputs = parse_inputs(raw.inputs)?;
        let devices = parse_devices(raw.devices)?;
        let spec = Self {
            id: raw.cog.id,
            version: raw.cog.version,
            console: ConsoleLimits {
                allowed_commands: console.allowed_commands,
                max_runtime_secs,
                output_limit_bytes,
            },
            resources: Resources { ram_mb, cpu_pct },
            value_args,
            flag_args,
            interfaces,
            outputs,
            inputs,
            devices,
        };
        for c in &spec.console.allowed_commands {
            spec.split_command(c)?;
        }
        Ok(spec)
    }

    /// Validate place-time arguments against the `[config]` CLI surface.
    pub fn validate_args(&self, args: &[String]) -> Result<(), RuntimeError> {
        if args.len() > MAX_ARGS {
            return Err(bad(format!("more than {MAX_ARGS} arguments")));
        }
        let mut it = args.iter();
        while let Some(a) = it.next() {
            if self.flag_args.contains(a) {
                continue;
            }
            if a == "--once" || a == "--interval" {
                return Err(bad(format!("{a} is set by the run mode, not by args")));
            }
            if !self.value_args.contains(a) {
                return Err(bad(format!(
                    "argument {a:?} is not a config cli_arg of this cog"
                )));
            }
            match it.next() {
                Some(v) if valid_value(v) => {}
                _ => return Err(bad(format!("{a} needs a plain value"))),
            }
        }
        Ok(())
    }

    /// Argv tail for a console command, which must be exactly one of
    /// `[console].allowed_commands`.
    pub fn console_args(&self, command: &str) -> Result<Vec<String>, RuntimeError> {
        if !self.console.allowed_commands.iter().any(|c| c == command) {
            return Err(bad(format!(
                "console command {command:?} is not in [console].allowed_commands"
            )));
        }
        self.split_command(command)
    }

    fn split_command(&self, command: &str) -> Result<Vec<String>, RuntimeError> {
        let parts: Vec<String> = command.split_whitespace().map(str::to_string).collect();
        if parts.len() > MAX_ARGS
            || parts
                .iter()
                .any(|p| p.len() > MAX_ARG_LEN || p.bytes().any(|b| b.is_ascii_control()))
        {
            return Err(bad(format!("unsafe console command {command:?}")));
        }
        Ok(parts)
    }
}

impl VerifiedWorkload {
    /// Build a cog workload from a verified package. Every binary and the
    /// `cog.toml` are read again through `source` and re-checked against
    /// their pinned hashes, so the bytes handed to an adapter are the bytes
    /// that were signed.
    pub fn from_package(
        pkg: &VerifiedPackage,
        source: &dyn FileSource,
    ) -> Result<Self, RuntimeError> {
        let body = &pkg.body;
        let read = |f| {
            let bytes = source
                .read(f)
                .map_err(|e| RuntimeError::AdmissionRefused(e.to_string()))?;
            check_file(f, &bytes).map_err(|e| RuntimeError::AdmissionRefused(e.to_string()))?;
            Ok::<_, RuntimeError>(bytes)
        };
        let spec = CogSpec::parse(&read(&body.cog_toml)?)?;
        if spec.id != body.id {
            return Err(RuntimeError::AdmissionRefused(format!(
                "cog.toml id {:?} does not match package id {:?}",
                spec.id, body.id
            )));
        }
        let mut binaries = BTreeMap::new();
        for (arch, f) in &body.binaries {
            binaries.insert(
                arch.clone(),
                Binary {
                    blake3: f.blake3.clone(),
                    bytes: Arc::new(read(f)?),
                },
            );
        }
        let signer_keys = pkg
            .envelope
            .signatures
            .iter()
            .filter(|s| pkg.signers.iter().any(|a| a.key_id == s.key_id))
            .map(|s| s.public_key.clone())
            .collect();
        let mut artifact_hashes: Vec<String> = body.files().map(|f| f.blake3.clone()).collect();
        artifact_hashes.sort();
        artifact_hashes.dedup();
        Ok(Self {
            kind: pkg.envelope.kind.clone(),
            id: body.id.clone(),
            version: body.version.clone(),
            source: WorkloadSource::SignedPackage(SignedPayload {
                package_id: pkg.package_id.clone(),
                signer_keys,
                spec,
                binaries,
                artifact_hashes,
            }),
        })
    }

    /// A device-store cog reference. Only the Seed adapter accepts it, and
    /// only when the operator pinned `(id, version)`.
    pub fn store_pin(
        registry: &str,
        id: &str,
        version: &str,
        sha256: Option<&str>,
    ) -> Result<Self, RuntimeError> {
        if !valid_cog_id(id) || !valid_value(version) || !valid_cog_id(registry) {
            return Err(bad("store pin needs a valid registry, cog id and version"));
        }
        if let Some(h) = sha256
            && !crate::workload_pkg::codec::is_lower_hex(h, 64)
        {
            return Err(bad("store pin sha256 must be 64 lower-case hex"));
        }
        Ok(Self {
            kind: crate::workload_pkg::KIND_COG.to_string(),
            id: id.to_string(),
            version: version.to_string(),
            source: WorkloadSource::StorePin {
                registry: registry.to_string(),
                sha256: sha256.map(str::to_string),
            },
        })
    }

    /// What the revocation list is checked against for this workload:
    /// `(package id, signer keys, artifact hashes)`. Never all empty: a
    /// store pin or project workload gets a synthetic package id, so a
    /// request built from any workload names something.
    pub fn revocation_refs(&self) -> (String, Vec<String>, Vec<String>) {
        match &self.source {
            WorkloadSource::SignedPackage(p) => (
                p.package_id.clone(),
                p.signer_keys.clone(),
                p.artifact_hashes.clone(),
            ),
            WorkloadSource::StorePin { .. } => (
                format!("store.{}.{}", self.id, self.version),
                Vec::new(),
                Vec::new(),
            ),
            WorkloadSource::Project(p) => (
                format!("project.{}.{}", p.project_id, p.cert_serial),
                Vec::new(),
                Vec::new(),
            ),
            WorkloadSource::Inference(_) => (
                format!("inference.{}.{}", self.id, self.version),
                Vec::new(),
                Vec::new(),
            ),
        }
    }

    /// The signed payload, or an admission refusal naming the adapter.
    pub fn signed(&self, runtime: &str) -> Result<&SignedPayload, RuntimeError> {
        match &self.source {
            WorkloadSource::SignedPackage(p) => Ok(p),
            WorkloadSource::StorePin { .. } => Err(RuntimeError::AdmissionRefused(format!(
                "{runtime} runs only signed packages; store pins go through the Seed adapter"
            ))),
            WorkloadSource::Project(_) => Err(RuntimeError::AdmissionRefused(format!(
                "{runtime} runs only signed packages; project workloads are not supported yet"
            ))),
            WorkloadSource::Inference(_) => Err(RuntimeError::AdmissionRefused(format!(
                "{runtime} runs only signed packages; inference workloads go through the infer.* adapters"
            ))),
        }
    }
}
