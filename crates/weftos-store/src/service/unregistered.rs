//! Why a governed request to a store this daemon has not registered was refused (WO-069 lane J3).
//!
//! A store's registry is per daemon (ADR-126 section 3): the daemon that created the store holds
//! its grants. A follower daemon, a second machine with only a read-only key, reads a project's
//! buckets through the S2 worktree verbs (names first, no registration) but never registers the
//! store, so a governed request reaches `gate` as "unknown store" before any grant is looked at.
//! The refusal is right; the reason was not. When the bucket does exist and this node can see it,
//! the accurate reason is "this node is not a writer of it". The probe runs only on this refusal
//! path, so no request that passes the gate costs a call more.

use crate::StoreName;
use crate::access::Op;
use crate::backend::BackendError;
use crate::service::{Caller, StoreService};

impl StoreService {
    /// The reason text for a governed `op` on `name`, which this daemon has not registered.
    ///
    /// Only a local caller (an operator on this host) is told more than "unknown store": a mesh
    /// caller has no business learning which buckets a backend holds, so it gets no probe either.
    pub(super) fn unregistered_reason(&self, who: &Caller, name: &StoreName, op: Op) -> String {
        let unknown = format!("unknown store {name}");
        if who.remote {
            return unknown;
        }
        match self.admin.bucket_exists(&name.bucket()) {
            Ok(false) => unknown,
            Ok(true) if matches!(op, Op::Put | Op::Copy | Op::Delete) => format!(
                "this node is not a writer of {name}: the store is not registered on this daemon and it holds no grant for it (a follower); \
                 a write needs the store's owner daemon to grant this node, or a write key held by this node"
            ),
            Ok(true) => format!(
                "{name} is not registered on this daemon, so a governed {} of it is not served here; \
                 the store's owner daemon serves it (a follower reads it through the worktree verbs)",
                if op == Op::Get { "get" } else { "read" }
            ),
            Err(BackendError::Forbidden(_)) => format!("{unknown} or no access"),
            Err(e) => format!("{unknown} (could not check whether its bucket exists: {e})"),
        }
    }
}

#[cfg(test)]
#[path = "unregistered_tests.rs"]
mod tests;
