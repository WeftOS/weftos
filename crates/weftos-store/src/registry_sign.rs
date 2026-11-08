//! Signed registry entries (ADR-126 S2). The registry in the project bucket is written by the
//! daemon, but anything with project write could replace an entry and point followers at the wrong
//! worktree. A sidecar object `worktrees.sig/<slug>.json` carries a signature over the exact bytes
//! of `worktrees/<slug>.json`; a follower that holds the daemon's public key can tell a real entry
//! from a forged or edited one. Signing is injected through two small traits, so this crate adds no
//! crypto of its own and no new key scheme: the daemon supplies the node identity it already has.

use crate::overlay::{ObjectBackend, Result, S2Error, validate_key};
use crate::share_fetch::{REGISTRY_PREFIX, RegistryEntry, write_registry_entry};
use serde::{Deserialize, Serialize};

pub const SIG_PREFIX: &str = "worktrees.sig/";

pub trait EntrySigner {
    /// Stable id of the signing identity (for a key, its public half).
    fn signer_id(&self) -> String;
    fn alg(&self) -> &'static str;
    fn sign(&self, msg: &[u8]) -> Vec<u8>;
}

pub trait EntryVerifier {
    /// True only if `sig` is `signer`'s signature over `msg` under `alg`, and `signer` is trusted.
    fn verify(&self, signer: &str, alg: &str, msg: &[u8], sig: &[u8]) -> bool;
}

#[derive(Debug, Serialize, Deserialize)]
struct Sidecar {
    signer: String,
    alg: String,
    sig: String, // hex
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trust {
    Verified,
    /// No signature object for this entry.
    Unsigned,
    /// A signature exists but does not verify (edited entry, wrong or untrusted signer).
    Bad,
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

/// Write the entry, then its signature over the bytes as stored.
pub fn write_registry_entry_signed<B: ObjectBackend>(
    b: &B,
    project_bucket: &str,
    e: &RegistryEntry,
    signer: &dyn EntrySigner,
) -> Result<()> {
    write_registry_entry(b, project_bucket, e)?;
    let key = format!("{REGISTRY_PREFIX}{}.json", e.slug);
    let bytes = b
        .get(project_bucket, &key)?
        .ok_or_else(|| S2Error::Backend("registry entry vanished after write".into()))?;
    let side = Sidecar {
        signer: signer.signer_id(),
        alg: signer.alg().into(),
        sig: hex(&signer.sign(&bytes)),
    };
    b.put(
        project_bucket,
        &format!("{SIG_PREFIX}{}.json", e.slug),
        &serde_json::to_vec(&side)?,
    )?;
    Ok(())
}

/// Check one entry against its signature object. Entries are matched by the bytes in the store,
/// so a verified entry is exactly what the signer wrote.
pub fn verify_registry_entry<B: ObjectBackend>(
    b: &B,
    project_bucket: &str,
    slug: &str,
    verifier: &dyn EntryVerifier,
) -> Result<Trust> {
    validate_key(slug)?;
    let Some(bytes) = b.get(project_bucket, &format!("{REGISTRY_PREFIX}{slug}.json"))? else {
        return Err(S2Error::Invalid(format!("no registry entry for {slug}")));
    };
    let Some(side) = b.get(project_bucket, &format!("{SIG_PREFIX}{slug}.json"))? else {
        return Ok(Trust::Unsigned);
    };
    let Ok(side) = serde_json::from_slice::<Sidecar>(&side) else {
        return Ok(Trust::Bad);
    };
    let ok = unhex(&side.sig)
        .is_some_and(|sig| verifier.verify(&side.signer, &side.alg, &bytes, &sig));
    Ok(if ok { Trust::Verified } else { Trust::Bad })
}

/// Every registry entry with its trust level, newest first. With `require_signed`, anything not
/// `Verified` is dropped from the result (and so never reaches a follower).
pub fn list_shared_verified<B: ObjectBackend>(
    b: &B,
    project_bucket: &str,
    verifier: &dyn EntryVerifier,
    require_signed: bool,
) -> Result<Vec<(RegistryEntry, Trust)>> {
    let mut out = Vec::new();
    for e in crate::share_fetch::list_shared(b, project_bucket)? {
        let t = verify_registry_entry(b, project_bucket, &e.slug, verifier)?;
        if !require_signed || t == Trust::Verified {
            out.push((e, t));
        }
    }
    Ok(out)
}

/// Daemon-side: drop an entry and its signature object together, so no orphan signature stays
/// behind to be paired with a later entry for a recreated slug.
pub fn remove_registry_entry_signed<B: ObjectBackend>(
    b: &B,
    project_bucket: &str,
    slug: &str,
) -> Result<()> {
    crate::follower::remove_registry_entry(b, project_bucket, slug)?;
    b.delete(project_bucket, &format!("{SIG_PREFIX}{slug}.json"))
}

/// Entries with their trust, and the slugs that were refused or unreadable.
pub type TrustedList = (Vec<(RegistryEntry, Trust)>, Vec<String>);

/// [`crate::share_fetch::list_shared_lenient`] through a trust check. With `require_signed`, an
/// entry that is not `Verified` is moved to the second list (the unreadable slugs), so a follower
/// is told it exists and was refused instead of silently not seeing it. Without it, every entry is
/// kept, and the caller gets each entry's trust to show.
pub fn list_trusted_lenient<B: ObjectBackend>(
    b: &B,
    project_bucket: &str,
    verifier: &dyn EntryVerifier,
    require_signed: bool,
) -> Result<TrustedList> {
    let (entries, mut bad) = crate::share_fetch::list_shared_lenient(b, project_bucket)?;
    let mut out = Vec::new();
    for e in entries {
        let t = verify_registry_entry(b, project_bucket, &e.slug, verifier)?;
        if require_signed && t != Trust::Verified {
            bad.push(e.slug);
        } else {
            out.push((e, t));
        }
    }
    bad.sort();
    Ok((out, bad))
}

/// [`crate::follower::poll`] over a registry filtered by [`list_trusted_lenient`].
pub fn poll_verified<B: ObjectBackend>(
    b: &B,
    project_bucket: &str,
    state: &mut crate::follower::FollowState,
    verifier: &dyn EntryVerifier,
    require_signed: bool,
    allow_all_gone: bool,
) -> Result<Vec<crate::follower::Change>> {
    let (entries, bad) = list_trusted_lenient(b, project_bucket, verifier, require_signed)?;
    crate::follower::diff_poll(
        entries.into_iter().map(|(e, _)| e).collect(),
        bad,
        state,
        allow_all_gone,
    )
}
