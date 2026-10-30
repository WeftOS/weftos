//! On-disk ML-DSA-65 key files.
//!
//! Layout (version 1): `"WPQK" | version u8 = 1 | algo u8 = 1 (ML-DSA-65) |
//! reserved [0, 0] | public key | secret key`.
//!
//! Files are written owner-only (0600 on Unix) through a `create_new`
//! temporary file, synced, then renamed into place, so a crash never leaves
//! a half-written key under the real name. Loading refuses a symlink, a
//! non-regular file, or (on Unix) a file readable by group or others; it
//! does not repair permissions. Key bytes are never logged or printed.

use std::io::Write;
use std::path::Path;

use zeroize::Zeroizing;

use crate::mldsa65::{PUBLIC_KEY_LEN, SECRET_KEY_LEN, SigningKey};
use crate::{Error, Result};

const MAGIC: &[u8; 4] = b"WPQK";
const VERSION: u8 = 1;
const ALGO_ML_DSA_65: u8 = 1;
const HEADER_LEN: usize = 8;

/// Total size of a version-1 ML-DSA-65 key file.
pub const FILE_LEN: usize = HEADER_LEN + PUBLIC_KEY_LEN + SECRET_KEY_LEN;

fn key_err(path: &Path, reason: impl Into<String>) -> Error {
    Error::KeyFile {
        path: path.display().to_string(),
        reason: reason.into(),
    }
}

/// Write `key` to `path`, which must not exist yet.
pub fn save_new(path: &Path, key: &SigningKey) -> Result<()> {
    if std::fs::symlink_metadata(path).is_ok() {
        return Err(key_err(path, "already exists; refusing to overwrite a key"));
    }
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let mut body = Zeroizing::new(Vec::with_capacity(FILE_LEN));
    body.extend_from_slice(MAGIC);
    body.extend_from_slice(&[VERSION, ALGO_ML_DSA_65, 0, 0]);
    body.extend_from_slice(&key.to_keypair_bytes());

    let tmp = path.with_extension("tmp-new");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp)?;
    let written = f.write_all(&body).and_then(|()| f.sync_all());
    drop(f);
    if let Err(e) = written.and_then(|()| std::fs::rename(&tmp, path)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}

/// Load a key written by [`save_new`], checking file type, permissions,
/// header and the key pair self-check.
pub fn load(path: &Path) -> Result<SigningKey> {
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.file_type().is_file() {
        return Err(key_err(path, "is not a regular file (symlinks are refused)"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o077 != 0 {
            return Err(key_err(path, "is readable or writable beyond its owner (chmod 600)"));
        }
    }
    let bytes = Zeroizing::new(std::fs::read(path)?);
    if bytes.len() != FILE_LEN {
        return Err(key_err(path, format!("is {} bytes, expected {FILE_LEN}", bytes.len())));
    }
    if &bytes[..4] != MAGIC || bytes[4] != VERSION || bytes[5] != ALGO_ML_DSA_65 {
        return Err(key_err(path, "has an unknown header (not an ML-DSA-65 v1 key file)"));
    }
    SigningKey::from_keypair_bytes(&bytes[HEADER_LEN..])
}

/// Load the key at `path`, or generate and save one if nothing is there.
/// The boolean is `true` when a new key was generated.
pub fn load_or_create(path: &Path) -> Result<(SigningKey, bool)> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => load(path).map(|k| (k, false)),
        #[cfg(feature = "mldsa")]
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let key = SigningKey::generate();
            save_new(path, &key)?;
            Ok((key, true))
        }
        #[cfg(not(feature = "mldsa"))]
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(Error::Unsupported),
        Err(e) => Err(e.into()),
    }
}

#[cfg(all(test, feature = "mldsa"))]
mod tests {
    use super::*;

    #[test]
    fn create_then_load_returns_the_same_key() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("node.mldsa65");
        let (a, created) = load_or_create(&p).unwrap();
        assert!(created);
        let (b, created) = load_or_create(&p).unwrap();
        assert!(!created);
        assert_eq!(a.verifying_key(), b.verifying_key());
        assert_eq!(std::fs::metadata(&p).unwrap().len() as usize, FILE_LEN);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn save_new_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("k");
        save_new(&p, &SigningKey::generate()).unwrap();
        assert!(save_new(&p, &SigningKey::generate()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn permissive_file_and_symlink_are_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("k");
        save_new(&p, &SigningKey::generate()).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(load(&p), Err(Error::KeyFile { .. })));
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&p, &link).unwrap();
        assert!(matches!(load(&link), Err(Error::KeyFile { .. })));
    }

    #[test]
    fn corrupt_header_and_length_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("k");
        save_new(&p, &SigningKey::generate()).unwrap();
        let mut b = std::fs::read(&p).unwrap();
        b[4] = 9;
        write_owner_only(&p, &b);
        assert!(load(&p).is_err());
        write_owner_only(&p, &b[..100]);
        assert!(load(&p).is_err());
    }

    fn write_owner_only(p: &Path, b: &[u8]) {
        std::fs::write(p, b).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }
}
