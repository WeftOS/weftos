//! Signed registry entries (ADR-126 S2). The registry in the project bucket is written by the
//! daemon, but anything with project write could replace an entry and point followers at the wrong
//! worktree. A sidecar object `worktrees.sig/<slug>.json` carries a signature over the exact bytes
//! of `worktrees/<slug>.json`; a follower that holds the daemon's public key can tell a real entry
//! from a forged or edited one. Signing is injected through two small traits, so this crate adds no
//! crypto of its own and no new key scheme: the daemon supplies the node identity it already has.
//!
//! What is signed is not the bare bytes but `"weftos-registry-v1\0" || registry bucket || \0 || slug ||
//! \0 || generation || \0 || entry bytes`. The prefix keeps a signature made here from being a valid
//! signature in any other protocol that uses the same key; the bucket and slug stop an entry and its
//! signature being replayed into another project or under another name; the generation (carried in
//! the signature object, increasing with every write) lets a follower refuse an older signed entry
//! put back later (rollback). A follower that has never seen the worktree has no earlier generation to
//! compare with, so rollback protection starts from its first sight of an entry.

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
    /// Increases with every signed write of this worktree's entry. Part of the signed message.
    generation: u64,
    sig: String, // hex
}

const DOMAIN: &[u8] = b"weftos-registry-v1\0";

/// The exact bytes a signer signs and a verifier checks.
pub fn signing_message(registry_bucket: &str, slug: &str, generation: u64, entry_bytes: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(DOMAIN.len() + registry_bucket.len() + slug.len() + entry_bytes.len() + 32);
    m.extend_from_slice(DOMAIN);
    m.extend_from_slice(registry_bucket.as_bytes());
    m.push(0);
    m.extend_from_slice(slug.as_bytes());
    m.push(0);
    m.extend_from_slice(generation.to_string().as_bytes());
    m.push(0);
    m.extend_from_slice(entry_bytes);
    m
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

/// Write the entry, then its signature over the bytes as stored. The generation is one more than
/// the signature object already there, or the entry's `updated_ms` if that is larger, so it keeps
/// increasing even when an entry was removed and written again later.
pub fn write_registry_entry_signed<B: ObjectBackend>(
    b: &B,
    project_bucket: &str,
    e: &RegistryEntry,
    signer: &dyn EntrySigner,
) -> Result<()> {
    let sig_key = format!("{SIG_PREFIX}{}.json", e.slug);
    let prev = b
        .get(project_bucket, &sig_key)?
        .and_then(|d| serde_json::from_slice::<Sidecar>(&d).ok())
        .map_or(0, |s| s.generation);
    let generation = prev.saturating_add(1).max(e.updated_ms);
    write_registry_entry(b, project_bucket, e)?;
    let key = format!("{REGISTRY_PREFIX}{}.json", e.slug);
    let bytes = b
        .get(project_bucket, &key)?
        .ok_or_else(|| S2Error::Backend("registry entry vanished after write".into()))?;
    let side = Sidecar {
        signer: signer.signer_id(),
        alg: signer.alg().into(),
        generation,
        sig: hex(&signer.sign(&signing_message(project_bucket, &e.slug, generation, &bytes))),
    };
    b.put(project_bucket, &sig_key, &serde_json::to_vec(&side)?)?;
    Ok(())
}

/// Check `bytes` (the object `worktrees/<slug>.json` as read) against its signature object.
/// Returns the trust level and, when signed, the signature's generation.
fn verify_bytes<B: ObjectBackend>(
    b: &B,
    project_bucket: &str,
    slug: &str,
    bytes: &[u8],
    verifier: &dyn EntryVerifier,
) -> Result<(Trust, u64)> {
    let Some(side) = b.get(project_bucket, &format!("{SIG_PREFIX}{slug}.json"))? else {
        return Ok((Trust::Unsigned, 0));
    };
    let Ok(side) = serde_json::from_slice::<Sidecar>(&side) else {
        return Ok((Trust::Bad, 0));
    };
    let msg = signing_message(project_bucket, slug, side.generation, bytes);
    let ok = unhex(&side.sig).is_some_and(|sig| verifier.verify(&side.signer, &side.alg, &msg, &sig));
    Ok(if ok { (Trust::Verified, side.generation) } else { (Trust::Bad, 0) })
}

/// Check one entry against its signature object. Entries are matched by the bytes in the store,
/// so a verified entry is exactly what the signer wrote. The entry's body must also name `slug`:
/// a signature is for the object at that key, and a body claiming another worktree is `Bad`.
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
    match serde_json::from_slice::<RegistryEntry>(&bytes) {
        Ok(e) if e.slug == slug => verify_bytes(b, project_bucket, slug, &bytes, verifier).map(|v| v.0),
        _ => Ok(Trust::Bad),
    }
}

/// Every registry entry with its trust level, newest first. With `require_signed`, anything not
/// `Verified` is dropped from the result (and so never reaches a follower).
pub fn list_shared_verified<B: ObjectBackend>(
    b: &B,
    project_bucket: &str,
    verifier: &dyn EntryVerifier,
    require_signed: bool,
) -> Result<Vec<(RegistryEntry, Trust)>> {
    let (v, _refused) = crate::share_fetch::read_registry(b, project_bucket)?;
    let mut out = Vec::new();
    for r in v {
        let (t, _) = verify_bytes(b, project_bucket, &r.key_slug, &r.bytes, verifier)?;
        if !require_signed || t == Trust::Verified {
            out.push((r.entry, t));
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

/// [`TrustedList`] with each entry's signature generation (0 if unsigned).
pub type TrustedGenList = (Vec<(RegistryEntry, Trust, u64)>, Vec<String>);

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
    let (v, bad) = list_trusted_gen(b, project_bucket, verifier, require_signed)?;
    Ok((v.into_iter().map(|(e, t, _)| (e, t)).collect(), bad))
}

/// [`list_trusted_lenient`] that also returns each entry's signature generation (0 if unsigned).
pub fn list_trusted_gen<B: ObjectBackend>(
    b: &B,
    project_bucket: &str,
    verifier: &dyn EntryVerifier,
    require_signed: bool,
) -> Result<TrustedGenList> {
    let (v, mut bad) = crate::share_fetch::read_registry(b, project_bucket)?;
    let mut out = Vec::new();
    for r in v {
        // the signature is checked against the bytes that were parsed, under the key they came from
        let (t, generation) = verify_bytes(b, project_bucket, &r.key_slug, &r.bytes, verifier)?;
        if require_signed && t != Trust::Verified {
            bad.push(r.key_slug);
        } else {
            out.push((r.entry, t, generation));
        }
    }
    bad.sort();
    Ok((out, bad))
}

/// [`crate::follower::poll`] over a registry filtered by [`list_trusted_gen`]. The follower also
/// remembers the highest signature generation it has verified per worktree (`FollowState`) and
/// refuses an entry signed with a lower one: a rollback to an older signed entry is reported as
/// unreadable like any other refused entry, and the previous state is kept.
pub fn poll_verified<B: ObjectBackend>(
    b: &B,
    project_bucket: &str,
    state: &mut crate::follower::FollowState,
    verifier: &dyn EntryVerifier,
    require_signed: bool,
    allow_all_gone: bool,
) -> Result<Vec<crate::follower::Change>> {
    let (entries, mut bad) = list_trusted_gen(b, project_bucket, verifier, require_signed)?;
    let mut keep = Vec::new();
    let mut gens = Vec::new();
    for (e, t, generation) in entries {
        if t == Trust::Verified {
            if state.generations.get(&e.slug).is_some_and(|seen| generation < *seen) {
                bad.push(e.slug);
                continue;
            }
            gens.push((e.slug.clone(), generation));
        }
        keep.push(e);
    }
    bad.sort();
    let changes = crate::follower::diff_poll(keep, bad, state, allow_all_gone)?;
    for (slug, generation) in gens {
        let g = state.generations.entry(slug).or_insert(0);
        *g = (*g).max(generation);
    }
    Ok(changes)
}
