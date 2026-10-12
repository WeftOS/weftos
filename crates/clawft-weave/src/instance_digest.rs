//! Package digest enforcement (ADR-121 §4): before an installed agent package
//! starts, its files are checked against the digest the installation pinned.
//!
//! The digest definition belongs to the catalog and lives in one place,
//! `weftos::init::digest` (see its module docs). [`Sha256SortedFiles`] is the
//! default [`PackageDigest`] and only calls it. A symlink, a non-UTF-8 name,
//! an unreadable file or an empty directory is an error, never skipped.

use std::path::Path;

/// Why a digest could not be computed or did not match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DigestError {
    /// The package directory or a file in it could not be read, or holds a symlink.
    Unreadable(String),
    /// The pinned digest is not `sha256:<64 hex>` (or bare hex).
    BadPin(String),
    /// Computed digest differs from the pin.
    Mismatch {
        /// The pinned digest, normalised to `sha256:<hex>`.
        expected: String,
        /// What the files hash to.
        actual: String,
    },
}

impl std::fmt::Display for DigestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreadable(e) => write!(f, "package files unreadable: {e}"),
            Self::BadPin(p) => write!(f, "pinned digest {p:?} is not sha256:<64 hex>"),
            Self::Mismatch { expected, actual } => write!(f, "digest mismatch: pinned {expected}, files hash to {actual}"),
        }
    }
}

/// How a package directory is turned into a digest.
pub trait PackageDigest: Send + Sync {
    /// `sha256:<hex>` of the package under `dir`.
    fn digest(&self, dir: &Path) -> Result<String, DigestError>;
}

/// The default digest: `weftos::init::digest::dir_digest`, as `sha256:<hex>`.
#[derive(Debug, Default, Clone, Copy)]
pub struct Sha256SortedFiles;

impl PackageDigest for Sha256SortedFiles {
    fn digest(&self, dir: &Path) -> Result<String, DigestError> {
        weftos::init::digest::dir_digest(dir)
            .map(|hex| format!("sha256:{hex}"))
            .map_err(|e| DigestError::Unreadable(e.to_string()))
    }
}

/// `sha256:<hex>` for a pin written as `sha256:<hex>` or bare hex; `None` if malformed.
pub fn normalize_pin(pin: &str) -> Option<String> {
    let hex = pin.strip_prefix("sha256:").unwrap_or(pin);
    (hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit())).then(|| format!("sha256:{}", hex.to_ascii_lowercase()))
}

/// Check `dir` against `pin`. `Ok` carries the verified digest.
pub fn verify(digester: &dyn PackageDigest, dir: &Path, pin: &str) -> Result<String, DigestError> {
    let expected = normalize_pin(pin).ok_or_else(|| DigestError::BadPin(pin.to_owned()))?;
    let actual = digester.digest(dir)?;
    if actual == expected { Ok(actual) } else { Err(DigestError::Mismatch { expected, actual }) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkg() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join("sub")).unwrap();
        std::fs::write(d.path().join("agent.md"), "hello").unwrap();
        std::fs::write(d.path().join("sub/skill.md"), "skill").unwrap();
        d
    }

    #[test]
    fn digest_is_stable_and_verifies() {
        let d = pkg();
        let a = Sha256SortedFiles.digest(d.path()).unwrap();
        assert_eq!(a, Sha256SortedFiles.digest(d.path()).unwrap());
        assert_eq!(verify(&Sha256SortedFiles, d.path(), &a).unwrap(), a);
        assert_eq!(verify(&Sha256SortedFiles, d.path(), a.trim_start_matches("sha256:")).unwrap(), a);
    }

    #[test]
    fn edits_additions_and_renames_change_the_digest() {
        let d = pkg();
        let a = Sha256SortedFiles.digest(d.path()).unwrap();
        std::fs::write(d.path().join("agent.md"), "hellO").unwrap();
        let b = Sha256SortedFiles.digest(d.path()).unwrap();
        assert_ne!(a, b);
        std::fs::write(d.path().join("extra.md"), "x").unwrap();
        let c = Sha256SortedFiles.digest(d.path()).unwrap();
        assert_ne!(b, c);
        std::fs::rename(d.path().join("extra.md"), d.path().join("other.md")).unwrap();
        assert_ne!(c, Sha256SortedFiles.digest(d.path()).unwrap());
    }

    #[test]
    fn mismatch_malformed_pin_and_missing_dir_refuse() {
        let d = pkg();
        let a = Sha256SortedFiles.digest(d.path()).unwrap();
        std::fs::write(d.path().join("agent.md"), "tampered").unwrap();
        match verify(&Sha256SortedFiles, d.path(), &a) {
            Err(DigestError::Mismatch { expected, actual }) => {
                assert_eq!(expected, a);
                assert_ne!(actual, a);
            }
            other => panic!("expected mismatch, got {other:?}"),
        }
        assert!(matches!(verify(&Sha256SortedFiles, d.path(), "nope"), Err(DigestError::BadPin(_))));
        assert!(matches!(verify(&Sha256SortedFiles, &d.path().join("missing"), &a), Err(DigestError::Unreadable(_))));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_is_refused_not_skipped() {
        let d = pkg();
        std::os::unix::fs::symlink("/etc/hosts", d.path().join("link")).unwrap();
        assert!(matches!(Sha256SortedFiles.digest(d.path()), Err(DigestError::Unreadable(_))));
    }
}
