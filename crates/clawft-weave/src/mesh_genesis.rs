//! Mesh genesis: a `MeshId` for a mesh that has no Seed licence (ADR-114 §2,
//! ADR-106 section 3).
//!
//! The authority of a `weftos://` name is the mesh's `MeshId`. With a Seed
//! licence the id is `MeshId::derive(genesis_pin, mesh_nonce)` from
//! `kernel.mesh`. Without one, `weaver mesh create` on the mesh's primary
//! mints it with the **same** function:
//!
//! ```text
//! mesh_id = MeshId::derive(owner user public key, fresh 32-byte mesh nonce)
//! ```
//!
//! (the user key takes the place of the genesis pin; `MESH_ID_DOMAIN` hashing
//! is shared, so ids from both schemes are 64 lowercase hex and compare equal
//! only when the inputs do).
//!
//! The result is a signed record at `<runtime>/mesh/mesh.json`. Nothing in it
//! is secret: the nonce is public by ADR-106, keys are public halves. Two
//! signatures over `RECORD_DOMAIN \n <canonical record JSON>`: the **user
//! key** (the owner made this mesh) and the primary's **node key**, which is
//! the key a member pins when it pairs, so a member can check a record it is
//! handed against the key it already trusts ([`learn`]).
//!
//! Precedence at boot ([`resolve`]): a licence id wins; otherwise the record.
//! A record that disagrees with the licence is reported, never obeyed. A
//! record that cannot be read or verified is an error, not "no mesh".

use std::path::{Path, PathBuf};

use clawft_kernel::licence::MeshId;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

/// Directory under the runtime dir.
pub const RECORD_DIR: &str = "mesh";
/// The record file.
pub const RECORD_FILE: &str = "mesh.json";
/// Domain tag of the node-key signature.
pub const RECORD_DOMAIN: &str = "weftos.mesh.record.v1";
/// Domain tag of the user-key signature.
pub const RECORD_USER_DOMAIN: &str = "weftos.mesh.record.user.v1";
const MAX_RECORD_BYTES: u64 = 16 * 1024;

/// What the signatures cover. Field order is the canonical order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeshRecord {
    pub version: u32,
    /// 64 lowercase hex.
    pub mesh_id: String,
    /// RFC 3339, UTC.
    pub created_at: String,
    /// The owner's user public key, 64 hex.
    pub created_by: String,
    /// The 32-byte mesh nonce, 64 hex (public).
    pub mesh_nonce: String,
}

/// A record with its two signatures; this is what travels in a `pair` payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedMeshRecord {
    pub record: MeshRecord,
    /// The signing node's public key, 64 hex.
    pub signer: String,
    /// Node-key signature, 128 hex.
    pub signature: String,
    /// User-key signature, 128 hex.
    pub user_signature: String,
}

/// Where this node's record came from (local metadata, not signed).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Origin {
    Genesis,
    Learned { node: String },
}

/// The file's content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredRecord {
    pub signed: SignedMeshRecord,
    pub origin: Origin,
}

/// Where the node's `MeshId` came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdSource {
    Licence,
    Genesis,
    Learned { node: String },
}

impl IdSource {
    pub fn describe(&self) -> String {
        match self {
            Self::Licence => "licence".into(),
            Self::Genesis => "genesis".into(),
            Self::Learned { node } => format!("learned from {node}"),
        }
    }
}

/// The node's id and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub id: MeshId,
    pub source: IdSource,
    /// With a licence id: a stored record that names a different mesh.
    pub conflicting_record: Option<MeshId>,
}

pub fn record_path(dir: &Path) -> PathBuf {
    dir.join(RECORD_DIR).join(RECORD_FILE)
}

fn hex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 || !s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return None;
    }
    hex::decode(s).ok()?.try_into().ok()
}

fn sig_of(s: &str) -> Option<Signature> {
    if s.len() != 128 || !s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return None;
    }
    Some(Signature::from_bytes(&hex::decode(s).ok()?.try_into().ok()?))
}

fn signed_bytes(domain: &str, r: &MeshRecord) -> Result<Vec<u8>, String> {
    let json = serde_json::to_string(r).map_err(|e| e.to_string())?;
    Ok(format!("{domain}\n{json}").into_bytes())
}

impl SignedMeshRecord {
    /// Check shape, the id derivation and both signatures. Returns the id.
    pub fn verify(&self) -> Result<MeshId, String> {
        let r = &self.record;
        if r.version != 1 {
            return Err(format!("mesh record: unsupported version {}", r.version));
        }
        let user = hex32(&r.created_by).ok_or("mesh record: created_by must be 64 lowercase hex")?;
        let nonce = hex32(&r.mesh_nonce).ok_or("mesh record: mesh_nonce must be 64 lowercase hex")?;
        let id = MeshId::from_hex(&r.mesh_id).ok_or("mesh record: mesh_id must be 64 lowercase hex")?;
        if MeshId::derive(&user, &nonce) != id {
            return Err("mesh record refused: mesh_id is not derived from created_by and mesh_nonce".into());
        }
        let user_vk = VerifyingKey::from_bytes(&user).map_err(|_| "mesh record: created_by is not an Ed25519 key")?;
        let signer = hex32(&self.signer).ok_or("mesh record: signer must be 64 lowercase hex")?;
        let signer_vk = VerifyingKey::from_bytes(&signer).map_err(|_| "mesh record: signer is not an Ed25519 key")?;
        let (sig, usig) = (
            sig_of(&self.signature).ok_or("mesh record: signature must be 128 lowercase hex")?,
            sig_of(&self.user_signature).ok_or("mesh record: user_signature must be 128 lowercase hex")?,
        );
        signer_vk
            .verify_strict(&signed_bytes(RECORD_DOMAIN, r)?, &sig)
            .map_err(|_| "mesh record refused: the signer's signature does not verify")?;
        user_vk
            .verify_strict(&signed_bytes(RECORD_USER_DOMAIN, r)?, &usig)
            .map_err(|_| "mesh record refused: the creator's signature does not verify")?;
        Ok(id)
    }
}

/// Read the stored record. `Ok(None)` only when there is no file.
pub fn load(dir: &Path) -> Result<Option<StoredRecord>, String> {
    let path = record_path(dir);
    let meta = match std::fs::symlink_metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    if !meta.file_type().is_file() || meta.len() > MAX_RECORD_BYTES {
        return Err(format!("{} is not a regular file under {MAX_RECORD_BYTES} bytes", path.display()));
    }
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let stored: StoredRecord = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    stored.signed.verify().map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(Some(stored))
}

/// Publish `stored` at the record path, failing if a file is already there.
fn publish(dir: &Path, stored: &StoredRecord) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let d = dir.join(RECORD_DIR);
    std::fs::create_dir_all(&d).map_err(|e| format!("{}: {e}", d.display()))?;
    let tmp = d.join(format!(".mesh.{}.{:016x}.tmp", std::process::id(), rand::random::<u64>()));
    let text = serde_json::to_string_pretty(stored).map_err(|e| e.to_string())?;
    let written = (|| {
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.write_all(b"\n")?;
        f.sync_all()?;
        std::fs::hard_link(&tmp, record_path(dir))
    })();
    let _ = std::fs::remove_file(&tmp);
    written.map_err(|e| match e.kind() {
        std::io::ErrorKind::AlreadyExists => "a mesh record already exists here; refusing to replace it".to_owned(),
        _ => format!("{}: {e}", record_path(dir).display()),
    })
}

/// Mint this mesh's id and write the record. Refuses when any id already
/// names the mesh: a licence id, or a record on disk (even an unreadable one).
pub fn create(
    dir: &Path,
    node_key: &SigningKey,
    user_key: &SigningKey,
    licence: Option<MeshId>,
    now_rfc3339: &str,
) -> Result<SignedMeshRecord, String> {
    if let Some(id) = licence {
        return Err(format!(
            "this node already has a MeshId from its Seed licence ({}); nothing to create. \
             `weaver mesh id` shows it",
            id.to_hex()
        ));
    }
    match load(dir) {
        Ok(Some(s)) => {
            return Err(format!(
                "this mesh already has a MeshId ({}, {}); refusing to create another",
                s.signed.record.mesh_id,
                origin_text(&s.origin)
            ));
        }
        Err(e) => return Err(format!("a mesh record exists but cannot be used ({e}); refusing to create another")),
        Ok(None) => {}
    }
    let mut nonce = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut nonce);
    let user_pk = user_key.verifying_key().to_bytes();
    let record = MeshRecord {
        version: 1,
        mesh_id: MeshId::derive(&user_pk, &nonce).to_hex(),
        created_at: now_rfc3339.to_owned(),
        created_by: hex::encode(user_pk),
        mesh_nonce: hex::encode(nonce),
    };
    let signed = SignedMeshRecord {
        signature: hex::encode(node_key.sign(&signed_bytes(RECORD_DOMAIN, &record)?).to_bytes()),
        user_signature: hex::encode(user_key.sign(&signed_bytes(RECORD_USER_DOMAIN, &record)?).to_bytes()),
        signer: hex::encode(node_key.verifying_key().to_bytes()),
        record,
    };
    publish(dir, &StoredRecord { signed: signed.clone(), origin: Origin::Genesis })?;
    Ok(signed)
}

/// A member accepts the primary's record. It must verify, be signed by
/// `pinned` (the key the pairing pins), and not disagree with an id this node
/// already has (`local`, or a record on disk). Learning the same id twice is
/// a no-op.
pub fn learn(
    dir: &Path,
    signed: &SignedMeshRecord,
    pinned: &[u8; 32],
    node: &str,
    local: Option<MeshId>,
) -> Result<MeshId, String> {
    let id = signed.verify()?;
    if signed.signer != hex::encode(pinned) {
        return Err("mesh record refused: it is not signed by the primary this pairing pins".into());
    }
    if let Some(have) = local
        && have != id
    {
        return Err(format!(
            "mesh record refused: this node is already in mesh {} and the primary offers {}",
            have.to_hex(),
            id.to_hex()
        ));
    }
    match load(dir) {
        Ok(Some(s)) if s.signed.record.mesh_id == signed.record.mesh_id => return Ok(id),
        Ok(Some(s)) => {
            return Err(format!(
                "mesh record refused: this node holds mesh {} and the primary offers {}",
                s.signed.record.mesh_id, signed.record.mesh_id
            ));
        }
        Err(e) => return Err(format!("mesh record refused: the one on disk cannot be used ({e})")),
        Ok(None) => {}
    }
    publish(dir, &StoredRecord { signed: signed.clone(), origin: Origin::Learned { node: node.to_owned() } })?;
    Ok(id)
}

fn origin_text(o: &Origin) -> String {
    match o {
        Origin::Genesis => IdSource::Genesis.describe(),
        Origin::Learned { node } => IdSource::Learned { node: node.clone() }.describe(),
    }
}

/// The id this node uses. `licence` is the Seed licence id (from
/// `kernel.mesh`), when configured. `Ok(None)`: no mesh id exists. `Err`: a
/// record is present but unusable and no licence id overrides it.
pub fn resolve(dir: &Path, licence: Option<MeshId>) -> Result<Option<Resolved>, String> {
    let stored = load(dir);
    if let Some(id) = licence {
        let conflicting_record = match &stored {
            Ok(Some(s)) => MeshId::from_hex(&s.signed.record.mesh_id).filter(|r| *r != id),
            _ => None,
        };
        return Ok(Some(Resolved { id, source: IdSource::Licence, conflicting_record }));
    }
    Ok(stored?.map(|s| Resolved {
        id: MeshId::from_hex(&s.signed.record.mesh_id).expect("verified by load"),
        source: match s.origin {
            Origin::Genesis => IdSource::Genesis,
            Origin::Learned { node } => IdSource::Learned { node },
        },
        conflicting_record: None,
    }))
}

/// Put `mesh_id` (64 lowercase hex, omitted when unset) and, when this node
/// holds a usable record, `mesh_record` (the signed record, for a dashboard
/// to hand a pairing member) into a heartbeat `report`. Neither is ever
/// trimmed by the budget. An unusable record is left out, not guessed at.
pub fn attach(report: &mut serde_json::Value, id: Option<MeshId>, dir: Option<&Path>) {
    let Some(id) = id else { return };
    report["mesh_id"] = serde_json::json!(id.to_hex());
    if let Some(Ok(Some(s))) = dir.map(load)
        && s.signed.record.mesh_id == id.to_hex()
    {
        report["mesh_record"] = serde_json::json!(s.signed);
    }
}

/// `weaver project add weftos://<authority>/...`: the name must be this
/// mesh's. Refuses when the node has no MeshId, or the authority is another
/// mesh; both ids are named.
pub fn check_authority(local: Option<MeshId>, authority: &crate::weftos_uri::Authority) -> Result<(), String> {
    let name = authority.to_string();
    match local {
        None => Err(format!(
            "this node has no MeshId, so it cannot tell whether {name} is its mesh. Create one on the mesh's primary \
             with `weaver mesh create` (or pair with the primary, or configure a Seed licence), then retry"
        )),
        Some(id) if id.to_hex() != name => Err(format!(
            "the name is for mesh {name} but this node's mesh is {}; refusing to add a project from another mesh",
            id.to_hex()
        )),
        Some(_) => Ok(()),
    }
}

/// `weaver mesh adopt-licence`: confirm this node uses the licence's id.
/// Refuses when there is no licence id, or a stored record disagrees.
pub fn adopt_licence(dir: &Path, licence: Option<MeshId>) -> Result<MeshId, String> {
    let id = licence.ok_or(
        "no Seed licence on this node: kernel.mesh.genesis_hash and kernel.mesh.mesh_nonce are not both set \
         (without a licence, `weaver mesh create` mints the MeshId)",
    )?;
    let r = resolve(dir, Some(id))?.expect("licence id given");
    if let Some(other) = r.conflicting_record {
        return Err(format!(
            "the Seed licence names mesh {} but this node's mesh record names {}; refusing to adopt. \
             The licence id is the one in use. If the record is stale, move {} aside",
            id.to_hex(),
            other.to_hex(),
            record_path(dir).display()
        ));
    }
    Ok(id)
}

#[cfg(test)]
#[path = "mesh_genesis_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "mesh_genesis_wire_tests.rs"]
mod wire_tests;
