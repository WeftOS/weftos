//! Node facts probing, local advertisement and the facts TTL cache
//! (ADR-099 section 2, card mesh-placement-03).
//!
//! - [`host`]: the [`ProbeHost`] seam (real [`SystemHost`], fakes in tests).
//! - [`probe`]: orchestration, shared helpers, [`ProbeConfig`].
//! - `macos` / `linux`: OS probes (CPU, OS, memory, storage, Metal, ANE,
//!   board class, binfmt emulation).
//! - `runtimes`: native, Docker/OrbStack, Podman, Apple container,
//!   llama-server, mlx_lm, ollama.
//! - `accel`: vendor accelerators, only when their tool or device exists
//!   (nvidia-smi, hailortcli, Coral `/dev/apex_*` or USB id).
//! - `media` / `devices` (Linux): cameras, codec, ALSA; buses, MCU bridge,
//!   USB SDRs, power monitors (Ventuno cog plan, section 4.6).
//! - `libs`: library/toolchain versions (`x.lib.*`: CUDA, ROCm, Core ML, MLX,
//!   PyTorch build, ONNX Runtime providers; ADR-120).
//! - [`measured`]: `perf.*` results from the conformance harness.
//! - [`cache`]: the verified-facts TTL cache held by `ClusterMembership`.
//!
//! The data type ([`clawft_types::placement::NodeFacts`]) lives in
//! clawft-types; signing lives in [`crate::node_facts_advert`], beside the
//! coarse `SignedCapabilityAdvertisement`.

mod accel;
pub mod devices;
pub mod host;
pub mod libs;
pub mod linux;
pub mod macos;
pub mod media;
pub mod measured;
pub mod probe;
pub mod runtimes;

#[cfg(any(feature = "mesh", feature = "exochain"))]
pub mod cache;

#[cfg(test)]
mod fake_host;
#[cfg(test)]
mod tests_libs;
#[cfg(test)]
mod tests_linux;
#[cfg(test)]
mod tests_mac;
#[cfg(test)]
mod tests_ventuno;

pub use host::{ProbeHost, SystemHost};
pub use probe::{
    Collected, DEFAULT_FACTS_TTL_SECS, EmulationCache, ProbeConfig, build_facts, probe_capabilities,
    refresh_live, valid_image_ref,
};

#[cfg(any(feature = "mesh", feature = "exochain"))]
pub use cache::{CacheError, CachedNodeFacts, InsertOutcome, NodeFactsCache, TierSource};

/// Probe `host`, build facts for the node owning `key`, and sign them.
#[cfg(any(feature = "mesh", feature = "exochain"))]
pub fn probe_and_sign(
    host: &dyn ProbeHost,
    cfg: &ProbeConfig,
    key: &ed25519_dalek::SigningKey,
    now: u64,
    seq: u64,
    ttl_secs: u64,
) -> Result<crate::node_facts_advert::SignedNodeFacts, crate::node_facts_advert::NodeFactsAdvertError>
{
    let node_id = crate::node_registry::node_id_from_pubkey(&key.verifying_key().to_bytes());
    let facts = build_facts(&node_id, now, ttl_secs, seq, probe_capabilities(host, cfg));
    crate::node_facts_advert::sign_node_facts(&facts, key)
}
