//! Workload runtime adapters (mesh-placement-09; ADR-099 section 5,
//! ADR-100 sections 3-5).
//!
//! - [`types`]: the [`WorkloadRuntime`] trait (id, provides, admit, load,
//!   start, stop, unload, status, control_mode, console) and its values.
//! - [`native`]: unprivileged native process under `[console]` /
//!   `[resources]` limits and rlimits ([`supervise`]), emitting
//!   [`RunEvidence`].
//! - [`container`]: Apple `container`, Docker / OrbStack and Podman,
//!   running a one-binary image built locally from the verified artifact
//!   over an operator-pinned base ([`container_cmd`]), fronted by an
//!   ingest relay when the cog's loopback is not the node's
//!   ([`container_relay`]).
//! - [`seed`] / [`seed_ops`]: the `remote.api` adapter for a Cognitum Seed
//!   (its own HTTP API; pinned store cogs only; pairing; governed,
//!   backed-up firmware upgrade), over [`seed_http`].
//! - [`infer`]: `infer.llamacpp`, `infer.mlx-lm` and `infer.ollama` for model
//!   servers (ADR-101 section 4): adopt a running server, or manage one.
//! - [`host_contract`]: `COG_CSI_BIND`, `COG_SENSOR_URL`,
//!   `COGNITUM_COG_TOKEN`, `COGNITUM_COG_DATA_DIR`.
//! - [`host`]: [`WorkloadHost`], which gates each transition and chains it
//!   as a `workload.*` event.
//!
//! WASM adapters, when built, use `clawft-wasm-host` (ADR-099 decision 4);
//! the kernel `wasm_runner` is not used for cogs.

pub mod cog_decl;
pub mod cog_spec;
pub mod container;
pub mod container_cmd;
pub mod container_relay;
pub mod evidence;
pub mod fleet_inventory;
pub mod fleet_mcp;
pub mod host;
pub mod host_contract;
pub mod host_seed;
pub mod infer;
pub mod logical;
pub mod native;
pub mod seed;
pub mod seed_bind;
pub mod seed_client;
pub mod seed_creds;
pub mod seed_http;
pub mod seed_ops;
pub mod seed_tls;
pub mod seed_types;
pub mod supervise;
pub mod types;

#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests_cog_decl;
#[cfg(test)]
mod tests_container;
#[cfg(test)]
mod tests_container_cogbus;
#[cfg(test)]
mod tests_container_relay;
#[cfg(test)]
mod tests_fleet_inventory;
#[cfg(test)]
mod tests_host;
#[cfg(test)]
mod tests_live;
#[cfg(test)]
mod tests_native;
#[cfg(test)]
mod tests_wasmtime;
#[cfg(test)]
mod tests_seed;
#[cfg(test)]
mod tests_seed_bind;
#[cfg(all(test, feature = "mesh", feature = "ecc", feature = "exochain", feature = "native"))]
mod tests_seed_bind_v2;
#[cfg(test)]
mod tests_seed_host;
#[cfg(test)]
mod tests_seed_install;
#[cfg(test)]
mod tests_seed_ops;
#[cfg(test)]
mod tests_seed_pin;
#[cfg(test)]
mod tests_seed_transport;

pub use cog_spec::{CogSpec, ConsoleLimits, Resources};
pub use container::{ContainerRuntime, ContainerRuntimeConfig};
pub use container_cmd::{CommandRunner, Engine, SystemRunner};
pub use evidence::RunEvidence;
pub use fleet_inventory::{
    FleetCandidate, FleetCheck, FleetError, FleetInventory, FleetMcp, ReadOnlyFleet,
};
pub use fleet_mcp::{COGNITUM_MCP_URL, OAuthTokens};
pub use host::{RUNTIME_CHAIN_SOURCE, WorkloadHost};
pub use host_contract::{CogIdentity, HostContract};
pub use infer::{
    InferConfig, InferFlavor, InferMode, InferRuntime, InferenceSpec, ManagedConfig, RestartPolicy,
};
pub use logical::{
    CAP_PROJECT_LOGICAL, ChildIdentity, ChildLauncher, ChildProbe, ChildRef, ChildSpec, LOGICAL_ID, LogicalRuntime,
};
pub use native::{NativeConfig, NativeRuntime};
pub use seed::{SeedApiRuntime, SeedConfig, SeedPin};
pub use seed_bind::{
    BindError, BindRecord, Binding, SeedBinder, SignedBind, attest_seed_facts, seed_node_id, sign_bind,
};
#[cfg(all(feature = "mesh", feature = "ecc", feature = "exochain", feature = "native"))]
pub use seed_bind::{StewardBind, UnbindOutcome, grant_fingerprint};
pub use seed_creds::FileCredentials;
pub use seed_http::{HttpSeedTransport, SeedCredentials, SeedTransport};
pub use seed_ops::{SeedBackup, UpgradeOutcome};
pub use types::{
    Admission, ControlMode, InferencePayload, InstanceHandle, InstanceState, InstanceStatus,
    LinkSecurity, Preemption, ProjectPayload, RunMode, RuntimeError, VerifiedWorkload, WorkloadConfig, WorkloadRuntime,
    WorkloadSource,
};
