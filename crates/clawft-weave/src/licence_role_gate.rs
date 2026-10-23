//! The licence verbs are machine-level, served by the licence holder only
//! (ADR-106 phase 3, board card e5e20dfc).
//!
//! The Seed licence path (checkouts, approvals, the binding, the clock floor)
//! belongs to the machine, not to a user or a project. In service mode
//! (ADR-103) the machine's licence path runs in exactly one daemon: the one
//! that holds the mesh service's reserved licence topics, the cluster
//! owner's. A collapsed daemon is its own machine and serves them too.
//!
//! This gate runs before the scope gate. On any other daemon it refuses
//! every licence verb with the reason, the holder (uid and user, as the
//! mesh service names it) and the command to run there. Nothing is
//! forwarded: an Admin token on one user's daemon is no authority on the
//! owner's, and reaching the owner's socket would need a new cross-user
//! trust path. `workload.node.binding` is the exception: it answers (with
//! the same redirect) so `weaver workload node status` and `weaver doctor`
//! show where the licence path is.
//!
//! The scope gate lets these verbs through outside a project on the daemon
//! that serves them (Read ones to anyone, Admin ones to Admin, as the
//! capability table says): they are mesh-wide by design and part of no
//! project.

use crate::rpc_ext::{Denial, GateFuture, GateRequest};

/// Every licence verb: `licence_rpc`, `licence_checkout_rpc`,
/// `licence_checkout_verbs`, and `cog_check_rpc`. A test pins it to those
/// modules' lists.
pub const LICENCE_VERBS: &[&str] = &[
    "workload.node.bind",
    "workload.node.unbind",
    "workload.node.binding",
    "workload.node.reset-floor",
    "workload.cog.checkout",
    "workload.cog.checkout.approve",
    "workload.cog.checkout.status",
    "workload.cog.checkout.release",
    "workload.cog.checkout.renew",
    "workload.cog.checkout.list",
    "cog.check_run",
    "cog.licence.import",
    "cog.licence.status",
    "cog.licence.claims",
    "cog.licence.revoked",
];

/// Answered on every daemon (with the redirect where it is not served).
pub const ROLE_STATUS_VERB: &str = "workload.node.binding";

/// Error kind of the refusal.
pub const NOT_HERE_KIND: &str = "licence_not_here";

/// True for a licence verb.
pub fn is_licence_verb(method: &str) -> bool {
    LICENCE_VERBS.contains(&method)
}

/// The refusal for `method` on this daemon, if it is a licence verb this
/// daemon does not serve right now. `refusal` is the holder state's reason.
pub fn decide(method: &str, refusal: Option<String>) -> Result<(), Denial> {
    if !is_licence_verb(method) || method == ROLE_STATUS_VERB {
        return Ok(());
    }
    match refusal {
        Some(why) => Err(Denial::new(NOT_HERE_KIND, why)),
        None => Ok(()),
    }
}

/// Gate: licence verbs only on the licence holder (or a collapsed daemon).
pub fn licence_role_gate<'a>(req: &'a GateRequest<'a>) -> GateFuture<'a> {
    Box::pin(async move { decide(req.method, current_refusal()) })
}

#[cfg(all(feature = "placement", unix))]
fn current_refusal() -> Option<String> {
    crate::licence_boot::holder_refusal()
}

#[cfg(not(all(feature = "placement", unix)))]
fn current_refusal() -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_non_holder_refuses_every_licence_verb_but_the_role_status() {
        let why = Some("not the holder: run it as uid 501".to_string());
        for m in LICENCE_VERBS {
            let r = decide(m, why.clone());
            if *m == ROLE_STATUS_VERB {
                assert!(r.is_ok(), "{m} answers with the redirect");
            } else {
                let e = r.unwrap_err();
                assert_eq!(e.kind, NOT_HERE_KIND, "{m}");
                assert!(e.message.contains("uid 501"), "{m}: {}", e.message);
            }
            assert!(decide(m, None).is_ok(), "{m} on the holder or a collapsed daemon");
        }
        assert!(decide("workload.place", why).is_ok(), "not a licence verb");
    }

    #[cfg(all(feature = "placement", unix))]
    #[test]
    fn the_list_is_exactly_the_licence_modules_verbs() {
        let mut served: Vec<&str> = crate::licence_rpc::METHODS.to_vec();
        served.extend(crate::licence_checkout_rpc::METHODS);
        served.extend(crate::licence_checkout_verbs::METHODS);
        served.extend(crate::cog_check_rpc::METHODS);
        served.extend(crate::cog_licence_rpc::METHODS);
        served.sort();
        served.dedup();
        let mut ours = LICENCE_VERBS.to_vec();
        ours.sort();
        assert_eq!(ours, served);
    }
}
