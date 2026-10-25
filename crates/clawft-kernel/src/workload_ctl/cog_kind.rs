//! The cog kind's requirements (ADR-100 section 2): how a verified cog
//! package becomes a placement [`WorkloadSpec`].
//!
//! Per binary arch, three routes, in the order a kind prefers them:
//!
//! - `<arch>-native`: `cpu.arch.<arch>`, `os.linux` (cog binaries are Linux
//!   ELF) and a `runtime.native` whose `arches_native` has the arch;
//! - `<arch>-container`: a `runtime.container.*` running the arch natively;
//! - `<arch>-emulated`: a `runtime.container.*` emulating the arch
//!   (placed only when the operator sets `allow_emulated`).
//!
//! `[cog].interfaces` become common requirements (`i2c` needs `bus.i2c`,
//! `camera` anything under `media.camera`, ...); the legacy
//! `hardware_requirement` board list is not used.
//!
//! Memory is `[resources].ram_mb`; the policy needs a paired node (native
//! isolation is weak until landlock / seccomp, ADR-100 section 3) and lists
//! the package id, signer keys and artifact hashes as revocable refs.

use clawft_types::placement::engine::{
    Execution, ExecutionVariant, PlacementPolicy, TrustTier, WorkloadRequirements, WorkloadSpec,
};
use clawft_types::placement::{
    AttrPredicate, CapabilityId, MemoryDemand, PlacementTypeError, Requirement,
};

use crate::workload_runtime::cog_decl::{InterfaceNeed, interface_need};
use crate::workload_runtime::VerifiedWorkload;

/// Arch preference order when a package ships several binaries.
const ARCH_ORDER: &[&str] = &["aarch64", "armv7", "x86_64"];

fn exact(id: &str) -> Result<Requirement, PlacementTypeError> {
    Ok(Requirement::exact(CapabilityId::new(id)?))
}

fn runtime(prefix: &str, attr: &str, arch: &str) -> Result<Requirement, PlacementTypeError> {
    Requirement::prefix(prefix)?.try_with_where(AttrPredicate::has(attr, arch))
}

/// The three routes for one arch.
pub fn arch_routes(arch: &str) -> Result<Vec<ExecutionVariant>, PlacementTypeError> {
    Ok(vec![
        ExecutionVariant {
            name: format!("{arch}-native"),
            execution: Execution::Native,
            requirements: vec![
                exact(&format!("cpu.arch.{arch}"))?,
                exact("os.linux")?,
                runtime("runtime.native", "arches_native", arch)?,
            ],
        },
        ExecutionVariant {
            name: format!("{arch}-container"),
            execution: Execution::Native,
            requirements: vec![runtime("runtime.container", "arches_native", arch)?],
        },
        ExecutionVariant {
            name: format!("{arch}-emulated"),
            execution: Execution::Emulated,
            requirements: vec![runtime("runtime.container", "arches_emulated", arch)?],
        },
    ])
}

/// The route kind a variant name ends with (`native`, `container`,
/// `emulated`), which the target maps to an adapter.
pub fn route_of(variant: &str) -> &str {
    variant.rsplit('-').next().unwrap_or(variant)
}

/// The arch a variant name starts with.
pub fn arch_of(variant: &str) -> &str {
    variant.split('-').next().unwrap_or(variant)
}

/// The common requirements a cog's `[cog].interfaces` add: one per
/// interface, `none` adds nothing.
fn interface_requirements(names: &[String]) -> Result<Vec<Requirement>, String> {
    let mut out = Vec::new();
    for n in names {
        let req = match interface_need(n).map_err(|e| e.to_string())? {
            None => continue,
            Some(InterfaceNeed::Exact(id)) => exact(id),
            Some(InterfaceNeed::Prefix(p)) => Requirement::prefix(p),
        };
        out.push(req.map_err(|e| e.to_string())?);
    }
    Ok(out)
}

/// Placement spec for a verified cog package (signed packages only; store
/// pins are placed on an operator-assigned Seed node id, not by requirements).
pub fn cog_workload_spec(w: &VerifiedWorkload) -> Result<WorkloadSpec, String> {
    let p = w.signed("placement").map_err(|e| e.to_string())?;
    let mut arches: Vec<&str> = ARCH_ORDER
        .iter()
        .copied()
        .filter(|a| p.binaries.contains_key(*a))
        .collect();
    // Unknown arches (a package may ship more) come after the known ones.
    let mut extra: Vec<&str> = p
        .binaries
        .keys()
        .map(String::as_str)
        .filter(|a| !ARCH_ORDER.contains(a))
        .collect();
    extra.sort_unstable();
    arches.extend(extra);
    if arches.is_empty() {
        return Err("package has no binaries".into());
    }
    let mut variants = Vec::new();
    for a in arches {
        variants.extend(arch_routes(a).map_err(|e| e.to_string())?);
    }
    let mut refs = vec![p.package_id.clone()];
    refs.extend(p.signer_keys.iter().cloned());
    refs.extend(p.artifact_hashes.iter().cloned());
    let spec = WorkloadSpec {
        kind: w.kind.clone(),
        name: w.id.clone(),
        requirements: WorkloadRequirements {
            common: interface_requirements(&p.spec.interfaces)?,
            variants,
            memory: MemoryDemand {
                host_bytes: u64::from(p.spec.resources.ram_mb) << 20,
                accel_bytes: 0,
            },
        },
        policy: PlacementPolicy {
            min_trust: TrustTier::Paired,
            revocable_refs: refs,
            ..PlacementPolicy::default()
        },
    };
    spec.validate().map_err(|e| e.to_string())?;
    Ok(spec)
}
