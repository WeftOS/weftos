//! Release authenticity for `weaver update`: an Ed25519 signature by the
//! WeaveLogic release key over a per-release hash list.
//!
//! Release CI (`scripts/release/sign-release.sh`) writes two assets:
//!
//! - `weftos-release.json`: `{"schema":1,"kind":"weftos-release","tag":"v…",
//!   "published":"<RFC 3339 UTC>","assets":{"<file>":"<sha256 hex>",…}}`,
//!   listing every file uploaded to the release, `dist-manifest.json` included.
//! - `weftos-release.json.sig`: the hex Ed25519 signature over
//!   `"weftos-release-v1\n"` followed by the exact bytes of that file.
//!
//! The signing key is a dedicated release key, separate from the COG-008 cog
//! signing key, so a cog signature can never pass as a release signature or
//! the reverse. The `"weftos-release-v1\n"` prefix stays as a second layer.
//!
//! The verifying key is compiled in: [`WEFTOS_RELEASE_PUBKEY_HEX`]. There is no file, variable or
//! flag that swaps it; tests hand a throwaway key to [`Trust::Pinned`] through
//! the update context. The operator's signer-key revocation list (the one cog
//! installs honour) can revoke it, which makes `weaver update` refuse
//! everything. The one runtime escape is `--insecure-skip-signature`, which
//! skips this check (sha256 checks still run) and says so loudly.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow, bail};
use chrono::{DateTime, Utc};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use clawft_types::runtime_paths::{self as rp, RUNTIME_DIR_ENV};
use cog_repo::{RevokedKeys, SUBJECTS_FILE_NAME};

/// The pinned WeaveLogic release public key (Ed25519, hex). Generated
/// 2026-10-03 as a dedicated release key; its seed is the
/// `WEAVELOGIC_RELEASE_KEY` secret of the `weftos-cogs` environment.
/// `scripts/release/sign-release.sh` refuses any other signing key.
pub const WEFTOS_RELEASE_PUBKEY_HEX: &str = "8ac2a3019b257f86f42f46a81d8578b2df339abdfa16140a00b0d4b3bfeef919";

/// [`WEFTOS_RELEASE_PUBKEY_HEX`] as a verifying key.
pub fn release_key() -> VerifyingKey {
    let raw: [u8; 32] = hex::decode(WEFTOS_RELEASE_PUBKEY_HEX)
        .expect("pinned release pubkey hex")
        .try_into()
        .expect("32 bytes");
    VerifyingKey::from_bytes(&raw).expect("pinned release pubkey is a valid Ed25519 key")
}

/// The signed hash list's asset name.
pub const SIGNED_DOC: &str = "weftos-release.json";
/// The detached signature's asset name.
pub const SIGNATURE: &str = "weftos-release.json.sig";
/// Domain-separation prefix prepended to the document before signing.
pub const DOMAIN: &[u8] = b"weftos-release-v1\n";
/// The manifest's entry in the signed hash list.
pub const MANIFEST: &str = "dist-manifest.json";
/// A latest release signed longer ago than this gets a warning.
pub const STALE_DAYS: i64 = 90;

/// Which releases `weaver update` accepts.
// Built once per run; boxing the key would only add indirection.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum Trust {
    /// Only releases whose hash list verifies under `key`, and only while
    /// `key` is not in `revoked`.
    Pinned { key: VerifyingKey, revoked: RevokedKeys },
    /// `--insecure-skip-signature`: authenticity is not checked.
    Skip,
}

impl Trust {
    /// The compiled-in WeaveLogic release key ([`WEFTOS_RELEASE_PUBKEY_HEX`]), checked
    /// against the operator's signer-key revocations (see
    /// [`load_revocations`]). Returns warnings to print alongside.
    pub fn pinned() -> anyhow::Result<(Self, Vec<String>)> {
        let env = std::env::var(RUNTIME_DIR_ENV).ok();
        let cwd = std::env::current_dir().ok();
        let (revoked, warnings) = load_revocations(env.as_deref(), cwd.as_deref(), rp::home_dir().as_deref())?;
        Ok((Self::pinned_with(revoked), warnings))
    }

    /// [`Trust::pinned`] with an explicit revocation list.
    pub fn pinned_with(revoked: RevokedKeys) -> Self {
        Trust::Pinned { key: release_key(), revoked }
    }
}

/// The revocation files `weaver update` reads, from explicit inputs: the
/// user-level ones (`$WEFTOS_RUNTIME_DIR` when set; else the user daemon's
/// `~/.weftos/run` and the legacy `~/.clawft`), and the project-level one of
/// the project around `cwd`, if any. Unlike [`rp::RuntimePaths::resolve`],
/// the user-level lists never depend on the working directory.
pub fn revocation_files(env: Option<&str>, cwd: Option<&Path>, home: Option<&Path>) -> anyhow::Result<(Vec<PathBuf>, Option<PathBuf>)> {
    if let Some(dir) = env.map(str::trim).filter(|d| !d.is_empty()) {
        return Ok((vec![Path::new(dir).join(SUBJECTS_FILE_NAME)], None));
    }
    let home = home.ok_or_else(|| {
        anyhow!("cannot locate the signer revocation list: no home directory and {RUNTIME_DIR_ENV} is unset")
    })?;
    let user = vec![rp::user_runtime_root(home).join(SUBJECTS_FILE_NAME), home.join(".clawft").join(SUBJECTS_FILE_NAME)];
    let project = cwd
        .and_then(|c| rp::find_project_dir(c, Some(home)))
        .map(|p| p.join(".weftos").join("runtime").join(SUBJECTS_FILE_NAME));
    Ok((user, project))
}

/// Union of the user-level and project-level signer revocations. A user-level
/// file that cannot be read or parsed is an error (fail closed). A malformed
/// project-level file (say, in a cloned repository) is ignored with a warning,
/// so it can neither block updates nor hide a user-level revocation.
pub fn load_revocations(env: Option<&str>, cwd: Option<&Path>, home: Option<&Path>) -> anyhow::Result<(RevokedKeys, Vec<String>)> {
    let (user, project) = revocation_files(env, cwd, home)?;
    let mut revoked = RevokedKeys::none();
    for p in &user {
        revoked.merge(RevokedKeys::load(p).map_err(|e| anyhow!("cannot read the signer revocation list: {e}"))?);
    }
    let mut warnings = Vec::new();
    if let Some(p) = project {
        match RevokedKeys::load(&p) {
            Ok(r) => revoked.merge(r),
            Err(e) => warnings.push(format!("warning: ignoring the project revocation list ({e}); the user-level list still applies")),
        }
    }
    Ok((revoked, warnings))
}

/// Fail closed when `key` is revoked. Nothing signed by it is trusted, and a
/// new trusted key can only arrive with a new `weaver`, out of band.
pub fn check_not_revoked(key: &VerifyingKey, revoked: &RevokedKeys) -> anyhow::Result<()> {
    let hex = hex::encode(key.to_bytes());
    if revoked.contains(&hex) {
        bail!(
            "the release signing key {hex} is revoked on this machine (signer_key in revoked_subjects.json), \
             so weaver update will not install anything. Reinstall weftos out of band: get a weaver build that pins \
             the replacement key from a source you trust (the WeaveLogic key-rotation announcement), not via weaver update"
        );
    }
    Ok(())
}

/// A verified hash list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedRelease {
    pub tag: String,
    pub published: DateTime<Utc>,
    pub assets: BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Doc {
    schema: u32,
    kind: String,
    tag: String,
    published: String,
    assets: BTreeMap<String, String>,
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn is_hash(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

/// Verify `sig_text` over `doc` with `key`, then parse the hash list. Nothing
/// in `doc` is read before the signature checks out.
pub fn verify(doc: &[u8], sig_text: &str, key: &VerifyingKey) -> anyhow::Result<SignedRelease> {
    let raw = hex::decode(sig_text.trim()).map_err(|_| anyhow!("{SIGNATURE} is not hex"))?;
    let raw: [u8; 64] = raw.try_into().map_err(|_| anyhow!("{SIGNATURE} is not a 64-byte Ed25519 signature"))?;
    let mut msg = DOMAIN.to_vec();
    msg.extend_from_slice(doc);
    key.verify(&msg, &Signature::from_bytes(&raw))
        .map_err(|_| anyhow!("release signature rejected: {SIGNED_DOC} is not signed by the WeaveLogic release key"))?;
    let d: Doc = serde_json::from_slice(doc).with_context(|| format!("{SIGNED_DOC} is signed but malformed"))?;
    if d.schema != 1 || d.kind != "weftos-release" {
        bail!("{SIGNED_DOC} has schema {} kind {:?}; this weaver understands schema 1 weftos-release", d.schema, d.kind);
    }
    if let Some((name, h)) = d.assets.iter().find(|(_, h)| !is_hash(h)) {
        bail!("{SIGNED_DOC} lists a malformed sha256 {h:?} for {name}");
    }
    let published = DateTime::parse_from_rfc3339(&d.published)
        .map_err(|_| anyhow!("{SIGNED_DOC} has a malformed published time {:?}", d.published))?
        .with_timezone(&Utc);
    Ok(SignedRelease { tag: d.tag, published, assets: d.assets })
}

impl SignedRelease {
    /// Require the signed list to name `name` with exactly `sha256`.
    pub fn check(&self, name: &str, sha256: &str) -> anyhow::Result<()> {
        match self.assets.get(name) {
            Some(h) if h == sha256 => Ok(()),
            Some(h) => bail!("{name} does not match the signed release: signed {h}, downloaded {sha256}"),
            None => bail!("{name} is not listed in the signed release {SIGNED_DOC}"),
        }
    }

    /// A warning when the latest release was signed more than
    /// [`STALE_DAYS`] ago: a mirror or attacker may be holding back newer ones.
    pub fn staleness_warning(&self, now: DateTime<Utc>) -> Option<String> {
        let days = (now - self.published).num_days();
        (days > STALE_DAYS).then(|| {
            format!(
                "warning: the latest release ({}) was signed {days} days ago ({}). If WeaveLogic has published newer \
                 releases, something is serving you an old one.",
                self.tag,
                self.published.format("%Y-%m-%d")
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn key(n: u8) -> SigningKey {
        SigningKey::from_bytes(&[n; 32])
    }

    fn signed(k: &SigningKey, doc: &str) -> String {
        let mut m = DOMAIN.to_vec();
        m.extend_from_slice(doc.as_bytes());
        hex::encode(k.sign(&m).to_bytes())
    }

    const DOC: &str = r#"{"schema":1,"kind":"weftos-release","tag":"v1.0.0","published":"2026-10-01T12:00:00Z","assets":{"dist-manifest.json":"0000000000000000000000000000000000000000000000000000000000000000"}}"#;

    #[test]
    fn good_signature_parses_and_checks() {
        let k = key(1);
        let s = verify(DOC.as_bytes(), &format!("{}\n", signed(&k, DOC)), &k.verifying_key()).unwrap();
        assert_eq!(s.tag, "v1.0.0");
        assert!(s.check(MANIFEST, &"0".repeat(64)).is_ok());
        assert!(s.check(MANIFEST, &"1".repeat(64)).is_err());
        assert!(s.check("other", &"0".repeat(64)).is_err());
    }

    #[test]
    fn wrong_key_edited_doc_and_garbage_are_rejected() {
        let k = key(1);
        let sig = signed(&k, DOC);
        assert!(verify(DOC.as_bytes(), &sig, &key(2).verifying_key()).is_err());
        let edited = DOC.replace("v1.0.0", "v9.0.0");
        assert!(verify(edited.as_bytes(), &sig, &k.verifying_key()).is_err());
        assert!(verify(DOC.as_bytes(), "zz", &k.verifying_key()).is_err());
        assert!(verify(DOC.as_bytes(), "abcd", &k.verifying_key()).is_err());
    }

    #[test]
    fn a_signature_without_the_domain_prefix_is_rejected() {
        // What a COG-008 signature over raw bytes looks like.
        let k = key(1);
        let raw = hex::encode(k.sign(DOC.as_bytes()).to_bytes());
        assert!(verify(DOC.as_bytes(), &raw, &k.verifying_key()).is_err());
    }

    #[test]
    fn signed_but_malformed_docs_are_rejected() {
        let k = key(1);
        for doc in [
            DOC.replace("\"schema\":1", "\"schema\":2"),
            DOC.replace("weftos-release\"", "cog\""),
            DOC.replace(&"0".repeat(64), &"A".repeat(64)),
            DOC.replace("\"tag\"", "\"extra\":1,\"tag\""),
            DOC.replace("2026-10-01T12:00:00Z", "yesterday"),
            DOC.replace(",\"published\":\"2026-10-01T12:00:00Z\"", ""),
        ] {
            assert!(verify(doc.as_bytes(), &signed(&k, &doc), &k.verifying_key()).is_err(), "{doc}");
        }
    }

    #[test]
    fn production_trust_is_the_dedicated_release_key() {
        let Trust::Pinned { key, .. } = Trust::pinned_with(RevokedKeys::none()) else { panic!() };
        assert_eq!(hex::encode(key.to_bytes()), WEFTOS_RELEASE_PUBKEY_HEX);
        // Never the COG-008 cog-signing key: the two must not overlap.
        assert_ne!(WEFTOS_RELEASE_PUBKEY_HEX, cog_repo::WEAVELOGIC_PUBKEY_HEX);
    }

    #[test]
    fn a_revoked_key_is_refused_with_out_of_band_advice() {
        let k = key(1).verifying_key();
        assert!(check_not_revoked(&k, &RevokedKeys::none()).is_ok());
        let revoked = RevokedKeys::from_keys([hex::encode(k.to_bytes()).to_uppercase()]);
        let e = check_not_revoked(&k, &revoked).unwrap_err().to_string();
        assert!(e.contains("revoked") && e.contains("out of band"), "{e}");
    }

    #[test]
    fn an_old_release_gets_a_staleness_warning() {
        let k = key(1);
        let s = verify(DOC.as_bytes(), &signed(&k, DOC), &k.verifying_key()).unwrap();
        let at = |d: &str| DateTime::parse_from_rfc3339(d).unwrap().with_timezone(&Utc);
        assert!(s.staleness_warning(at("2026-12-01T00:00:00Z")).is_none());
        let w = s.staleness_warning(at("2027-01-15T00:00:00Z")).unwrap();
        assert!(w.contains("signed 105 days ago") && w.contains("2026-10-01"), "{w}");
    }
}
