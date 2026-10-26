//! Who may fetch which project over the mesh (ADR-108 P3b, contract section 3):
//! the gate on top of [`crate::project_fetch_grants`] (the file pairing writes).
//!
//! `project.fetch` is default deny. The primary serves it to a caller only when
//! both hold at the moment of the call:
//!
//! 1. the caller's node key is listed in `workload-peers.json` **with a key**
//!    at tier `pinned` or `paired` (an entry without a key trusts an address,
//!    not a node, and never earns fetch access);
//! 2. `project-fetch.json` has a grant naming that node id and that project
//!    ULID; a grant that carries `peer_ed25519` must also name the key that
//!    signed the request (the key the peer entry pins).
//!
//! Both files are re-read per check, so removing the peer or the grant stops
//! the next request.
//!
//! [`FetchPeerPolicy`] is the host's controller policy: controllers may call
//! anything, a peer with a grant may call only `workload.describe` (to be
//! learned by the member's control plane) and `project.fetch`.

use std::path::{Path, PathBuf};

use clawft_kernel::node_id_from_pubkey;
use clawft_kernel::workload_ctl::msg::{ControllerPolicy, method};
use clawft_types::placement::TrustTier;

use crate::project_fetch_grants::{Grant, Grants};
use crate::workload_place_policy::load_peer_keys;

/// `(tier, key)` of the listed peer whose node id is `node_id`: `None` when it
/// is not listed, or listed without a key.
pub fn listed_peer(dir: &Path, node_id: &str) -> Result<Option<(TrustTier, [u8; 32])>, String> {
    Ok(load_peer_keys(dir)?.into_iter().find(|(_, k)| node_id_from_pubkey(k) == node_id))
}

fn tier_ok(t: TrustTier) -> bool {
    matches!(t, TrustTier::Pinned | TrustTier::Paired)
}

/// A grant applies to `key` when it names no key, or names this one.
fn grant_matches_key(g: &Grant, key: &[u8; 32]) -> bool {
    g.peer_ed25519.as_deref().is_none_or(|h| h.eq_ignore_ascii_case(&hex::encode(key)))
}

/// Check that the verified requester `node_id` may fetch `project` from this
/// node. `Err` is the reason shown to the caller; it never names other peers
/// or projects.
pub fn authorize(dir: &Path, node_id: &str, project: &str) -> Result<TrustTier, String> {
    let listed = listed_peer(dir, node_id).map_err(|e| format!("peer list unreadable: {e}"))?;
    let Some((tier, key)) = listed.filter(|(t, _)| tier_ok(*t)) else {
        return Err("fetch refused: the caller is not a pinned or paired peer of this node".into());
    };
    let grants = Grants::load(dir).map_err(|e| format!("fetch grants unreadable: {e}"))?;
    let ok = grants
        .for_peer(node_id)
        .into_iter()
        .any(|g| grant_matches_key(g, &key) && g.projects.iter().any(|p| p == project));
    if !ok {
        return Err("fetch refused: no fetch grant for this peer and project".into());
    }
    Ok(tier)
}

/// The host's controller policy with fetch peers admitted for the two
/// methods a fetch needs.
pub struct FetchPeerPolicy {
    controllers: Vec<[u8; 32]>,
    dir: PathBuf,
}

impl FetchPeerPolicy {
    /// `controllers` from `workload-host.json` (plus this node's own key);
    /// `dir` is the runtime dir holding the peer and grant files.
    pub fn new(controllers: Vec<[u8; 32]>, dir: PathBuf) -> Self {
        Self { controllers, dir }
    }

    /// Listed at a fetching tier under this very key, with at least one
    /// project granted to it.
    fn fetch_peer(&self, key: &[u8; 32]) -> bool {
        let node = node_id_from_pubkey(key);
        let listed = matches!(listed_peer(&self.dir, &node), Ok(Some((t, k))) if tier_ok(t) && &k == key);
        listed
            && Grants::load(&self.dir).is_ok_and(|g| {
                g.for_peer(&node).into_iter().any(|g| grant_matches_key(g, key) && !g.projects.is_empty())
            })
    }
}

impl ControllerPolicy for FetchPeerPolicy {
    fn allows(&self, public_key: &[u8; 32]) -> bool {
        self.controllers.contains(public_key)
    }

    fn allows_method(&self, public_key: &[u8; 32], m: &str) -> bool {
        self.allows(public_key)
            || (matches!(m, method::DESCRIBE | method::PROJECT_FETCH) && self.fetch_peer(public_key))
    }

    fn allows_node(&self, node_id: &str) -> bool {
        self.controllers
            .iter()
            .any(|k| clawft_kernel::node_id_from_pubkey(k) == node_id)
    }
}

#[cfg(test)]
#[path = "project_fetch_policy_tests.rs"]
mod tests;
