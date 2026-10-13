//! `weaver mesh import`: take a mesh record from a node this node already
//! trusts (ADR-114 §2). For a node that cannot learn it by pairing: the mesh
//! was created on a machine it is not ADR-108-paired with.
//!
//! The record is verified exactly as [`learn`](crate::mesh_genesis::learn)
//! does (shape, id derivation, node and user signatures). Then the record's
//! `signer` must be a key this node already trusts; there is no
//! trust-on-first-use. Trusted means one of:
//!
//! - an entry with that `key` in `workload-peers.json` at tier `pinned` or
//!   `paired`;
//! - a `controllers` key in `workload-host.json` (how a node trusts the
//!   operator's machine);
//! - a node in `mesh-pairings.json` whose id derives from the key.

use std::path::Path;

use clawft_kernel::licence::MeshId;
#[cfg(all(feature = "placement", unix))]
use clawft_types::placement::TrustTier;

/// The placement peers file (`workload_place_policy::PEERS_FILE`), named here so
/// the refusal message builds on every target.
const PEERS_FILE_NAME: &str = "workload-peers.json";

use crate::mesh_genesis::{self, Origin, SignedMeshRecord, StoredRecord};

/// How the signer is trusted, or why it is not. Never `Ok` by default.
pub fn signer_trust(dir: &Path, signer: &[u8; 32]) -> Result<&'static str, String> {
    let node = clawft_kernel::node_id_from_pubkey(signer);
    let hex = hex::encode(signer);
    // The placement trust files exist only on Unix builds with `placement`;
    // elsewhere a mesh pairing is the only way to trust a signer.
    #[cfg(all(feature = "placement", unix))]
    {
        let peers = crate::workload_place_policy::load_peer_keys(dir)?;
        if peers.iter().any(|(t, k)| k == signer && matches!(t, TrustTier::Pinned | TrustTier::Paired)) {
            return Ok("workload-peers.json (pinned or paired)");
        }
        if let Some(cfg) = crate::workload_host_serve::load_host_config(dir)?
            && cfg.controllers.contains(&hex)
        {
            return Ok("workload-host.json controllers");
        }
    }
    if mesh_pairings_has(dir, &node)? {
        return Ok("mesh-pairings.json");
    }
    Err(format!(
        "mesh record refused: it is signed by node {node} (key {hex}), which this node does not trust. \
         Trust it first: add {{\"key\": \"{hex}\", \"tier\": \"paired\"}} to {} in the runtime dir, \
         list the key under `controllers` in workload-host.json, or pair with that node. \
         Nothing was imported",
        PEERS_FILE_NAME
    ))
}

fn mesh_pairings_has(dir: &Path, node: &str) -> Result<bool, String> {
    let path = dir.join(crate::mesh_pairings::PAIRINGS_FILE);
    let Ok(m) = std::fs::metadata(&path) else { return Ok(false) };
    if m.len() > 256 * 1024 {
        return Err("mesh-pairings.json is too large".into());
    }
    let text = std::fs::read_to_string(&path).map_err(|e| format!("mesh-pairings.json: {e}"))?;
    let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("mesh-pairings.json: {e}"))?;
    Ok(v["pairings"].as_array().is_some_and(|a| a.iter().any(|p| p["peer_node"] == node)))
}

/// What an import did.
#[derive(Debug, PartialEq, Eq)]
pub enum Imported {
    /// Written now.
    Written(MeshId),
    /// The same record was already here.
    AlreadyHeld(MeshId),
}

/// Import `signed`. `local` is the Seed licence id, if configured.
pub fn import(dir: &Path, signed: &SignedMeshRecord, local: Option<MeshId>) -> Result<Imported, String> {
    let id = signed.verify()?;
    let signer: [u8; 32] = hex::decode(&signed.signer)
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or("mesh record: signer must be 64 hex")?;
    signer_trust(dir, &signer)?;
    if let Some(have) = local
        && have != id
    {
        return Err(format!(
            "mesh record refused: this node's Seed licence names mesh {} and the record is for {}",
            have.to_hex(),
            id.to_hex()
        ));
    }
    match mesh_genesis::load(dir) {
        Ok(Some(s)) if s.signed.record.mesh_id == signed.record.mesh_id => return Ok(Imported::AlreadyHeld(id)),
        Ok(Some(s)) => {
            return Err(format!(
                "mesh record refused: this node holds mesh {} and the record is for {}",
                s.signed.record.mesh_id, signed.record.mesh_id
            ));
        }
        Err(e) => return Err(format!("mesh record refused: the one on disk cannot be used ({e})")),
        Ok(None) => {}
    }
    let node = clawft_kernel::node_id_from_pubkey(&signer);
    mesh_genesis::publish(dir, &StoredRecord { signed: signed.without_forward(), origin: Origin::Imported { node } })?;
    Ok(Imported::Written(id))
}

/// Parse an exported record (a bare `SignedMeshRecord`, or a stored
/// `mesh.json`, whose `origin` is local and ignored).
pub fn parse(text: &str) -> Result<SignedMeshRecord, String> {
    if text.len() > 16 * 1024 {
        return Err("mesh record: file is over 16 KiB".into());
    }
    let v: serde_json::Value = serde_json::from_str(text).map_err(|e| format!("mesh record: {e}"))?;
    let inner = if v.get("signed").is_some() { v["signed"].clone() } else { v };
    serde_json::from_value(inner).map_err(|e| format!("mesh record: {e}"))
}
