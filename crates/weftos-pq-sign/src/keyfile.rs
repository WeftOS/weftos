//! On-disk ML-DSA-65 key files.
//!
//! Layout (version 2): `"WPQK" | version u8 = 2 | algo u8 = 1 (ML-DSA-65) |
//! reserved [0, 0] | public key (1952) | seed ξ (32)`.
//!
//! Version 2 stores the FIPS 204 key-generation seed, the private key form
//! RustCrypto's `ml-dsa` uses (it deprecates the 4032-byte expanded secret
//! key). Version 1 stored `public key | expanded secret key` for the
//! PQClean backend; it never shipped in a release, and loading one is an
//! error that says to delete and regenerate the key.
//!
//! Files are written owner-only (0600 on Unix) through a `create_new`
//! temporary file, synced, then renamed into place, so a crash never leaves
//! a half-written key under the real name. Loading refuses a symlink, a
//! non-regular file, or (on Unix) a file readable by group or others; it
//! does not repair permissions. Key bytes are never logged or printed.

use std::io::Write;
use std::path::Path;

use zeroize::Zeroizing;

use crate::mldsa65::{KEYPAIR_LEN, SigningKey};
use crate::{Error, Result};

const MAGIC: &[u8; 4] = b"WPQK";
const VERSION: u8 = 2;
const VERSION_1_EXPANDED: u8 = 1;
const ALGO_ML_DSA_65: u8 = 1;
const HEADER_LEN: usize = 8;

/// Total size of a version-2 ML-DSA-65 key file.
pub const FILE_LEN: usize = HEADER_LEN + KEYPAIR_LEN;

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
    if bytes.len() >= HEADER_LEN && &bytes[..4] == MAGIC && bytes[4] == VERSION_1_EXPANDED {
        return Err(key_err(
            path,
            "is a version 1 key file (PQClean expanded secret key, pre-release); delete it and restart to generate a version 2 key",
        ));
    }
    if bytes.len() != FILE_LEN {
        return Err(key_err(path, format!("is {} bytes, expected {FILE_LEN}", bytes.len())));
    }
    if &bytes[..4] != MAGIC || bytes[4] != VERSION || bytes[5] != ALGO_ML_DSA_65 {
        return Err(key_err(path, "has an unknown header (not an ML-DSA-65 v2 key file)"));
    }
    SigningKey::from_keypair_bytes(&bytes[HEADER_LEN..])
}

/// Load the key at `path`, or generate and save one if nothing is there.
/// The boolean is `true` when a new key was generated.
pub fn load_or_create(path: &Path) -> Result<(SigningKey, bool)> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => load(path).map(|k| (k, false)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let key = SigningKey::try_generate()?;
            save_new(path, &key)?;
            Ok((key, true))
        }
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
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

    #[test]
    fn version_1_file_is_refused_with_a_clear_reason() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("k");
        let mut v1 = b"WPQK".to_vec();
        v1.extend_from_slice(&[1, 1, 0, 0]);
        v1.resize(8 + 1952 + 4032, 0);
        write_owner_only(&p, &v1);
        let err = load(&p).unwrap_err().to_string();
        assert!(err.contains("version 1"), "{err}");
    }

    #[test]
    fn file_layout_is_header_public_key_seed() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("k");
        let k = SigningKey::generate();
        save_new(&p, &k).unwrap();
        let b = std::fs::read(&p).unwrap();
        assert_eq!(&b[..8], b"WPQK\x02\x01\x00\x00");
        assert_eq!(&b[8..8 + 1952], k.verifying_key().as_bytes());
        assert_eq!(b.len(), 8 + 1952 + 32);
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
