//! LatticeApi exposed through a ServiceApi-shaped facade (WEFT-527 / ADR-035).
//!
//! The kernel's [`clawft_kernel::ServiceApi`] trait lives behind a heavy
//! dependency graph. To keep LeWM decoupled from the kernel (ADR-090), this
//! module implements the **same call surface** (`call` / `list_services` /
//! `health`) over a [`LatticeApi`] implementor. Hosts (including the future
//! `clawft-worldmodel-service` binary, WEFT-524) can:
//!
//! 1. Use [`LatticeServiceFacade`] directly, or
//! 2. Bridge into the kernel by forwarding `ServiceApi::call` to
//!    [`LatticeServiceFacade::call`].
//!
//! Service name: [`LATTICE_SERVICE_NAME`] (`"worldmodel.lattice"`).
//! Methods: the seven [`LatticeMethod`] names.

mod errors;
mod facade;
mod params;
mod typed;

pub use errors::{LatticeServiceError, LatticeServiceResult};
pub use facade::{
    default_lattice_service, FacadeHealth, FacadeServiceInfo, LatticeServiceFacade,
};
pub use typed::{dispatch_typed, observe_json, parse_lattice_method};

/// Registered service name for LatticeApi hosts.
pub const LATTICE_SERVICE_NAME: &str = "worldmodel.lattice";

/// Service type label returned by [`LatticeServiceFacade::list_services`].
pub const LATTICE_SERVICE_TYPE: &str = "worldmodel";

#[cfg(test)]
mod tests;
