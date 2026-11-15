//! The package-less place path for kind `computer` (owner decisions 1 and 2,
//! 2026-11-15; ADR GBF-003).
//!
//! A computer has no signed package to fetch: the order carries the kind's
//! manifest body, and the body names the desktop by id. This module turns that
//! order into a [`VerifiedWorkload`]. It does not load anything; `place`
//! continues with the same governance, journal and lifecycle code every other
//! kind uses.
//!
//! What it checks, in order:
//!
//! 1. the order routes to the `computer` adapter and its workload name is the
//!    desktop id (a computer's workload id is its desktop, which is also how the
//!    adapter finds the hub connection);
//! 2. the body passes [`ComputerBody::parse`], which holds the desktop id format
//!    (`[a-z0-9][a-z0-9_-]*`, at most 64), the host kind, the capabilities and
//!    the `own` / `granted` access rule;
//! 3. the governance gate admits `workload.place` for the **requester's**
//!    principal (the signing controller's node id) at operator-attested trust:
//!    see `computer_owner_permit`. The host's own gated `load` then checks the
//!    node's principal too.
//!
//! The desktop need not be connected to the hub yet: a desktop is minted, then
//! registers when its tool server starts, so a place that arrives first is
//! legitimate. Until it connects, the adapter reports "no hub connection for
//! this desktop" and the supervisor alerts after the detection window. A node
//! with no computer adapter (no hub) refuses the order in `place`, before this.

use std::sync::Arc;

use serde_json::Value;

use super::host_service::{PlaceBody, WorkloadHostService, refuse};
use super::msg::{CtlRequest, Refusal, RefusalCode};
use crate::workload_governance::policy::ACTION_PLACE;
use crate::workload_kind::{ComputerBody, KIND_COMPUTER, computer_workload};
use crate::workload_pkg::ManifestEnvelope;
use crate::workload_runtime::{VerifiedWorkload, WorkloadHost};

impl WorkloadHostService {
    /// Validate and admit a package-less `computer` order; see the module docs.
    pub(super) fn computer_order(
        &self,
        b: &PlaceBody,
        body: &Value,
        route: &str,
        host: &Arc<WorkloadHost>,
        req: &CtlRequest,
    ) -> Result<VerifiedWorkload, Refusal> {
        if route != KIND_COMPUTER {
            return Err(refuse(
                RefusalCode::InvalidRequest,
                format!("a computer order routes to the {KIND_COMPUTER} adapter, not {route}"),
            ));
        }
        let desktop = ComputerBody::parse(body)
            .map_err(|e| refuse(RefusalCode::InvalidRequest, format!("computer body: {e}")))?
            .desktop;
        if desktop != b.name {
            return Err(refuse(
                RefusalCode::InvalidRequest,
                "the placed name must be the desktop id",
            ));
        }
        let w = computer_workload(&ManifestEnvelope::new(KIND_COMPUTER, body.clone()))
            .map_err(|e| refuse(RefusalCode::InvalidRequest, format!("computer body: {e}")))?;
        host.check_principal(&req.requester, ACTION_PLACE, &w)
            .map_err(|e| crate::workload_ctl::host_service::runtime_refusal(&e))?;
        Ok(w)
    }
}
