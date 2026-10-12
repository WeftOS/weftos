//! Package digest enforcement (ADR-121 §4): before an installed agent package
//! starts, its files are checked against the digest the installation pinned.
//!
//! The digest definition belongs to the catalog (O1). Until it lands here,
//! [`PackageDigest`] is the seam and [`Sha256SortedFiles`] is the default:
//! `sha256` over `"<relative path>\0<file sha256 hex>\n"` for every regular
//! file under the package directory, sorted by relative path (`/` separators).
//! A symlink, a non-UTF-8 name or an unreadable file is an error, never skipped,
//! so a package cannot hide content from the digest.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// Most files a package may hold; a bigger tree is refused, not truncated.
pub const MAX_FILES: usize = 10_000;

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

/// The default digest: see the module docs.
#[derive(Debug, Default, Clone, Copy)]
pub struct Sha256SortedFiles;

fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) -> Result<(), DigestError> {
    let unread = |p: &Path, e: &dyn std::fmt::Display| DigestError::Unreadable(format!("{}: {e}", p.display()));
    for entry in std::fs::read_dir(dir).map_err(|e| unread(dir, &e))? {
        let entry = entry.map_err(|e| unread(dir, &e))?;
        let path = entry.path();
        let ty = entry.file_type().map_err(|e| unread(&path, &e))?;
        if ty.is_symlink() {
            return Err(unread(&path, &"symlink"));
        }
        if ty.is_dir() {
            walk(root, &path, out)?;
        } else if ty.is_file() {
            let rel = path.strip_prefix(root).map_err(|e| unread(&path, &e))?;
            let rel: Option<Vec<&str>> = rel.components().map(|c| c.as_os_str().to_str()).collect();
            let rel = rel.ok_or_else(|| unread(&path, &"non-UTF-8 name"))?.join("/");
            out.push((rel, path));
            if out.len() > MAX_FILES {
                return Err(unread(root, &format!("more than {MAX_FILES} files")));
            }
        }
    }
    Ok(())
}

impl PackageDigest for Sha256SortedFiles {
    fn digest(&self, dir: &Path) -> Result<String, DigestError> {
        let mut files = Vec::new();
        walk(dir, dir, &mut files)?;
        files.sort();
        let mut all = Sha256::new();
        for (rel, path) in files {
            let bytes = std::fs::read(&path).map_err(|e| DigestError::Unreadable(format!("{}: {e}", path.display())))?;
            all.update(rel.as_bytes());
            all.update([0]);
            all.update(hex::encode(Sha256::digest(&bytes)).as_bytes());
            all.update(b"\n");
        }
        Ok(format!("sha256:{}", hex::encode(all.finalize())))
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
