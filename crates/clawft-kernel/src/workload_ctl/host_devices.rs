//! The host's use of the device ledger (`device_owner`): admit a cog's
//! `[[devices]]` at place, hand the owner its `WEFTOS_DEVICES`, apply the
//! privileged grant when the cog has its own uid, and give it all back when
//! the instance is gone.

use std::sync::Arc;

use serde_json::{Value, json};

use super::device_owner::{DeviceApplier, DeviceLedger, DeviceReservation};
use super::host_service::WorkloadHostService;
use super::msg::{Refusal, RefusalCode};
use crate::workload_runtime::{HostContract, VerifiedWorkload, WorkloadSource};

impl WorkloadHostService {
    /// Carry out privileged grants with `a` (first call wins). Without one the
    /// ledger still refuses claims; nodes are not re-owned.
    pub fn set_device_applier(&self, a: Arc<dyn DeviceApplier>) -> bool {
        self.device_applier.set(a).is_ok()
    }

    /// The node's device ledger.
    pub fn devices(&self) -> &Arc<DeviceLedger> {
        &self.devices
    }

    /// Admit the cog's devices and arguments; the contract carries the owner's
    /// device list and nothing for a cog that owns none.
    pub(super) fn device_prepare(
        &self,
        w: &VerifiedWorkload,
        args: &[String],
        contract: HostContract,
    ) -> Result<(HostContract, Option<DeviceReservation>), Refusal> {
        let decls = match &w.source {
            WorkloadSource::SignedPackage(p) if w.kind == crate::workload_pkg::KIND_COG => p.spec.devices.as_slice(),
            _ => &[],
        };
        let r = self
            .devices
            .admit(&w.id, decls, args, contract.run_as.as_ref())
            .map_err(|e| Refusal::new(RefusalCode::Admission, e.0))?;
        Ok(match r {
            Some(r) => (contract.with_devices(r.paths().to_vec()), Some(r)),
            None => (contract, None),
        })
    }

    /// The loaded instance owns the reservation; apply the grant it planned.
    pub(super) fn device_bind(&self, r: Option<DeviceReservation>, instance: &str) -> Result<(), Refusal> {
        let Some(r) = r else { return Ok(()) };
        let ops = r.plan().apply.clone();
        r.bind(instance);
        if let (false, Some(a)) = (ops.is_empty(), self.device_applier.get())
            && let Err(e) = a.apply(&ops)
        {
            self.device_forget(instance);
            return Err(Refusal::new(RefusalCode::Runtime, format!("devices: cannot give the nodes to the cog's uid: {e}")));
        }
        Ok(())
    }

    /// The instance is gone for good: drop its claim and restore the nodes.
    pub(super) fn device_forget(&self, instance: &str) {
        let restore = self.devices.release(instance);
        if let (false, Some(a)) = (restore.is_empty(), self.device_applier.get())
            && let Err(e) = a.apply(&restore)
        {
            tracing::warn!(target: "workload", instance, error = %e, "could not restore device owners");
        }
    }

    /// `devices` and `device_isolation` for an instance's status row.
    pub(super) fn device_row(&self, instance: &str) -> Value {
        match self.devices.claim_of(instance) {
            Some((devs, iso)) => json!({ "devices": devs, "device_isolation": iso.as_str() }),
            None => json!({}),
        }
    }
}
