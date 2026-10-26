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
use crate::workload_runtime::{CogIdentity, HostContract, VerifiedWorkload, WorkloadSource};

/// Whether a cog's uid is its own (ADR-122 gap 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Isolation {
    /// The cog runs under a uid no other cog and not the daemon has: the
    /// broker's peer check tells it apart from every other cog.
    PerCogUid,
    /// The cog runs under the daemon's (or the adapter's) uid, shared with
    /// every other cog on that route. The peer check proves only "a process
    /// of that user".
    SharedUid,
}

impl Isolation {
    /// The word shown in `cogbus list` and the report.
    pub fn as_str(self) -> &'static str {
        match self {
            Isolation::PerCogUid => "per-cog-uid",
            Isolation::SharedUid => "shared-uid",
        }
    }
}

/// How an instance on one route reaches the broker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CogbusAttach {
    /// Socket path handed to the cog.
    pub socket: String,
    /// Uid the route starts cogs as; the broker checks the peer against it.
    pub uid: Option<u32>,
    /// The identity allocated for this instance; `None` keeps the adapter's.
    pub identity: Option<CogIdentity>,
    /// Whether `uid` is the instance's own.
    pub isolation: Isolation,
    /// `require_cog_uid` is on: a cog that uses the bus must not share a uid.
    pub refuse_shared: bool,
    /// Why the instance is not isolated, when it is not (shown, not parsed).
    pub note: Option<String>,
}

impl CogbusAttach {
    /// A route whose cogs all run as `uid`.
    pub fn shared(socket: impl Into<String>, uid: u32) -> Self {
        Self {
            socket: socket.into(),
            uid: Some(uid),
            identity: None,
            isolation: Isolation::SharedUid,
            refuse_shared: false,
            note: None,
        }
    }
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
    /// Whether that uid is the instance's own.
    pub isolation: Isolation,
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
    /// The instance is gone for good (unloaded, torn down): free whatever
    /// `attach` allocated for it. A stop is not this; the instance may start
    /// again under the same uid.
    fn release(&self, _instance_id: &str) {}
    /// A placement that took an identity from `attach` never registered:
    /// give it back.
    fn abandon(&self, _uid: u32) {}
}

/// Attached at place, before the instance id is known. Dropped unconsumed
/// (a load that failed), it gives the allocated identity back.
pub(super) struct CogbusPending {
    uid: Option<u32>,
    isolation: Isolation,
    outputs: Vec<OutputDecl>,
    inputs: Vec<InputDecl>,
    bus: Arc<dyn CogbusRegistrar>,
    owns_identity: bool,
}

impl Drop for CogbusPending {
    fn drop(&mut self) {
        if let (true, Some(uid)) = (self.owns_identity, self.uid) {
            self.bus.abandon(uid);
        }
    }
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
    ) -> Result<(HostContract, Option<CogbusPending>), Refusal> {
        let (Some(bus), WorkloadSource::SignedPackage(p)) = (self.cogbus.get(), &w.source) else {
            return Ok((contract, None));
        };
        if w.kind != crate::workload_pkg::KIND_COG {
            return Ok((contract, None));
        }
        let Some(a) = bus.attach(route) else {
            return Ok((contract, None));
        };
        let owns_identity = a.identity.is_some();
        // From here the pending guard owns the identity: every return below
        // (and a failed load later) hands it back.
        let pending = CogbusPending {
            uid: a.uid,
            isolation: a.isolation,
            outputs: p.spec.outputs.clone(),
            inputs: p.spec.inputs.clone(),
            bus: bus.clone(),
            owns_identity,
        };
        let uses_bus = !pending.outputs.is_empty() || !pending.inputs.is_empty();
        if a.isolation == Isolation::SharedUid && a.refuse_shared && uses_bus {
            return Err(Refusal::new(
                RefusalCode::Admission,
                format!(
                    "cogbus: require_cog_uid is on and this cog would share a uid ({})",
                    a.note.as_deref().unwrap_or("no per-cog uid on this daemon")
                ),
            ));
        }
        let mut contract = contract.with_cogbus(a.socket);
        if let Some(id) = a.identity {
            contract = contract.with_run_as(id);
        }
        Ok((contract, Some(pending)))
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
        let (Some(mut p), Some(bus)) = (pending, self.cogbus.get()) else {
            return Ok(None);
        };
        let r = CogbusRegistration {
            instance_id: instance_id.to_string(),
            cog_id: cog_id.to_string(),
            project_id,
            controller_node: controller_node.to_string(),
            token: contract.token.clone(),
            uid: p.uid,
            isolation: p.isolation,
            outputs: std::mem::take(&mut p.outputs),
            inputs: std::mem::take(&mut p.inputs),
        };
        // A refused registration drops `p` armed: the identity goes back.
        bus.register(&r)
            .map_err(|e| Refusal::new(RefusalCode::Runtime, format!("cogbus: {e}")))?;
        p.owns_identity = false;
        Ok(Some(r))
    }

    /// The instance is gone for good: free its uid.
    pub(super) fn bus_forget(&self, r: Option<&CogbusRegistration>) {
        if let (Some(r), Some(bus)) = (r, self.cogbus.get()) {
            bus.release(&r.instance_id);
        }
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
