//! Governance for placed workloads (ADR-099 sections 4 and 7).
//!
//! - [`effect`]: the workload effect fields and their 5D mapping.
//! - [`policy`]: governed `workload.*` actions, the default-deny rule set
//!   (distributed per ADR-092) and explicit permit rules.
//! - [`gate`]: [`WorkloadGate`], which enforces the policy, checks subject
//!   revocations and chains every decision (ADR-022).
//!
//! Revocation of package ids, signer keys and artifact hashes lives in
//! [`crate::revocation`].

pub mod effect;
pub mod gate;
pub mod policy;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_computer;

pub use effect::{NetworkPolicy, NodeTrustTier, PackageTrust, WorkloadEffect, WorkloadRefs, WorkloadRequest};
pub use gate::{
    WorkloadGate, chain_revocations, event_kind_for, revoke_and_record, unrevoke_and_record,
};
pub use policy::{
    CATALOG_PRINCIPAL, COMPUTER_OWNER_PERMIT_ID, DEFAULT_DENY_RULE_ID, EFFECT_CEILING_RULE_ID, GOVERNED_ACTIONS, SUPERVISOR_PRINCIPAL,
    WorkloadPermitRule, computer_owner_permit, default_rules, project_supervisor_permit, install_default_rules, is_governed_action,
};
