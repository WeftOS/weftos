//! The node's cog data plane broker (ADR-122) as the workload host sees it.
//!
//! The broker lives in the daemon (`weftos-cogbus`, wired by `clawft-weave`);
//! the host only calls a [`CogbusRegistrar`] at the points where it already
//! issues and revokes the ingest token:
//!
//! - place: before load, [`CogbusRegistrar::attach`] says whether the route
//!   reaches the broker socket; if so the contract gets `WEFTOS_COGBUS` and
//!   `WEFTOS_COGBUS_TOKEN`. After load, the instance is registered with its
//!   project, the uid it runs as and its manifest `[[outputs]]` /
//!   `[[inputs]]`;
//! - stop, a lease stop, a revocation, a failed start and unload revoke it;
//!   start registers it again.
//!
//! A broker that is down, or a route it cannot reach, means no registrar or
//! no attach: the cog runs without the two variables.

use std::sync::Arc;

use clawft_types::secret::SecretString;

use super::host_service::WorkloadHostService;
use super::msg::{Refusal, RefusalCode};
use crate::workload_runtime::cog_decl::{InputDecl, OutputDecl};
use crate::workload_runtime::{HostContract, VerifiedWorkload, WorkloadSource};

/// How an instance on one route reaches the broker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CogbusAttach {
    /// Socket path handed to the cog.
    pub socket: String,
    /// Uid the route starts cogs as; the broker checks the peer against it.
    pub uid: Option<u32>,
}

/// What the broker is told about one placed instance.
#[derive(Debug, Clone)]
pub struct CogbusRegistration {
    /// Instance id on this node.
    pub instance_id: String,
    /// Cog id.
    pub cog_id: String,
    /// Project that placed it.
    pub project_id: Option<String>,
    /// Node id of the placing controller (whose store takes a project-less
    /// instance's `store.vector` readings, as with the ingest bridge).
    pub controller_node: String,
    /// The instance token (the contract's, see `host_contract`).
    pub token: SecretString,
    /// Uid it runs as, if known.
    pub uid: Option<u32>,
    /// Manifest `[[outputs]]`.
    pub outputs: Vec<OutputDecl>,
    /// Manifest `[[inputs]]`.
    pub inputs: Vec<InputDecl>,
}

/// The daemon's broker, behind the host.
pub trait CogbusRegistrar: Send + Sync {
    /// The socket for cogs on `route`, or `None` when that route cannot
    /// reach it (a container or VM has no view of the host socket in B1).
    fn attach(&self, route: &str) -> Option<CogbusAttach>;
    /// Accept the instance's token.
    fn register(&self, r: &CogbusRegistration) -> Result<(), String>;
    /// Stop accepting it and close its connections.
    fn revoke(&self, instance_id: &str);
}

/// Attached at place, before the instance id is known.
pub(super) struct CogbusPending {
    uid: Option<u32>,
    outputs: Vec<OutputDecl>,
    inputs: Vec<InputDecl>,
}

impl WorkloadHostService {
    /// Wire the node's broker (first call wins).
    pub fn set_cogbus(&self, r: Arc<dyn CogbusRegistrar>) -> bool {
        self.cogbus.set(r).is_ok()
    }

    /// The broker wiring, if any.
    pub fn cogbus(&self) -> Option<&Arc<dyn CogbusRegistrar>> {
        self.cogbus.get()
    }

    /// Hand a cog on `route` the broker socket, when there is one for it.
    pub(super) fn bus_prepare(
        &self,
        route: &str,
        w: &VerifiedWorkload,
        contract: HostContract,
    ) -> (HostContract, Option<CogbusPending>) {
        let (Some(bus), WorkloadSource::SignedPackage(p)) = (self.cogbus.get(), &w.source) else {
            return (contract, None);
        };
        if w.kind != crate::workload_pkg::KIND_COG {
            return (contract, None);
        }
        let Some(a) = bus.attach(route) else {
            return (contract, None);
        };
        let pending = CogbusPending {
            uid: a.uid,
            outputs: p.spec.outputs.clone(),
            inputs: p.spec.inputs.clone(),
        };
        (contract.with_cogbus(a.socket), Some(pending))
    }

    /// Register a loaded instance.
    pub(super) fn bus_lease(
        &self,
        pending: Option<CogbusPending>,
        instance_id: &str,
        cog_id: &str,
        (project_id, controller_node): (Option<String>, &str),
        contract: &HostContract,
    ) -> Result<Option<CogbusRegistration>, Refusal> {
        let (Some(p), Some(bus)) = (pending, self.cogbus.get()) else {
            return Ok(None);
        };
        let r = CogbusRegistration {
            instance_id: instance_id.to_string(),
            cog_id: cog_id.to_string(),
            project_id,
            controller_node: controller_node.to_string(),
            token: contract.token.clone(),
            uid: p.uid,
            outputs: p.outputs,
            inputs: p.inputs,
        };
        bus.register(&r)
            .map_err(|e| Refusal::new(RefusalCode::Runtime, format!("cogbus: {e}")))?;
        Ok(Some(r))
    }

    /// Accept the instance again (start after stop). Registering an already
    /// registered instance is refused by the broker, so revoke first.
    pub(super) fn bus_on(&self, r: Option<&CogbusRegistration>) -> Result<(), Refusal> {
        let (Some(r), Some(bus)) = (r, self.cogbus.get()) else {
            return Ok(());
        };
        bus.revoke(&r.instance_id);
        bus.register(r)
            .map_err(|e| Refusal::new(RefusalCode::Runtime, format!("cogbus: {e}")))
    }

    /// Stop accepting the instance.
    pub(super) fn bus_off(&self, r: Option<&CogbusRegistration>) {
        if let (Some(r), Some(bus)) = (r, self.cogbus.get()) {
            bus.revoke(&r.instance_id);
        }
    }
}
