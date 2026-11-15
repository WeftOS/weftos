//! The daemon's gate for `workload.*` actions (ADR-099 section 4).
//!
//! One builder serves every daemon path that decides a workload action: the
//! placement control plane and the `workload.install` / `workload.unload`
//! catalog verbs. It is a [`WorkloadGate`], not the kernel's general
//! `GovernanceGate`, because only the workload gate can express a permit:
//!
//! - default deny: with no `workload-permits.json` entry matching the
//!   action, kind, trust, tier and network, the action is denied and the
//!   denial chained (`workload.refuse` or the action's own kind);
//! - a matching permit lets the action through and chains it as a permit;
//! - the kernel's revocation list is attached, so a revoked package, signer
//!   key or artifact hash is denied on every path;
//! - the effective governance rules (parent policy plus overlay) apply.
//!
//! The permits file is read when the gate is built, and the install verbs
//! build a fresh gate per call, so an edited file takes effect without a
//! restart and a broken file fails closed.

use std::path::Path;
use std::sync::Arc;

use clawft_kernel::boot::Kernel;
use clawft_kernel::chain::ChainManager;
use clawft_kernel::governance::GovernanceRule;
use clawft_kernel::revocation::RevocationList;
use clawft_kernel::workload_governance::{WorkloadGate, WorkloadPermitRule};
use clawft_platform::NativePlatform;
use tokio::sync::RwLock;

use crate::workload_place_policy::load_permits;

/// Effective governance for the gate: rules, risk threshold, human approval.
pub type Effective = (Vec<GovernanceRule>, f64, bool);

/// Highest threshold a workload gate runs at, whatever the rules ask for.
const MAX_THRESHOLD: f64 = 0.95;

/// A workload gate over the operator's permits in `dir`, chaining to
/// `chain` and consulting `revocations` (a required argument of the gate). `effective` is the project
/// kernel's rule set; `None` uses the shipped default-deny rules.
pub fn build(
    dir: &Path,
    chain: &Arc<ChainManager>,
    effective: Option<Effective>,
    revocations: Arc<RevocationList>,
) -> Result<Arc<WorkloadGate>, String> {
    build_with(dir, chain, effective, revocations, Vec::new())
}

/// [`build`] plus built-in permits that ship with a wired feature (the
/// `computer` owner permit, GBF-003). They come first, so an operator permit
/// with the same id is refused as a duplicate rather than silently shadowing.
pub fn build_with(
    dir: &Path,
    chain: &Arc<ChainManager>,
    effective: Option<Effective>,
    revocations: Arc<RevocationList>,
    builtin: Vec<WorkloadPermitRule>,
) -> Result<Arc<WorkloadGate>, String> {
    let base = match effective {
        Some((rules, threshold, human)) => {
            WorkloadGate::with_rules(threshold.min(MAX_THRESHOLD), human, rules, revocations)
        }
        None => WorkloadGate::new(MAX_THRESHOLD, false, revocations),
    };
    let mut g = base.with_chain(chain.clone());
    for p in builtin.into_iter().chain(load_permits(dir)?) {
        g = g.with_permit(p)?;
    }
    Ok(Arc::new(g))
}

/// [`build`] from a booted kernel: its chain, effective rules and
/// revocation list. Fails closed without a chain (decisions are chained).
pub async fn from_kernel(
    kernel: &Arc<RwLock<Kernel<NativePlatform>>>,
    dir: &Path,
) -> Result<Arc<WorkloadGate>, String> {
    let k = kernel.read().await;
    let chain = k
        .chain_manager()
        .cloned()
        .ok_or("workload governance needs the kernel chain (decisions are chained)")?;
    let effective = k
        .governance_overlay()
        .map(|o| o.effective_rules_and_hash().0);
    let revocations = k.revocation_list().clone();
    drop(k);
    build(dir, &chain, effective, revocations)
}
