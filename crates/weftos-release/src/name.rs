//! Release store names and object keys (ADR-126 §2).

use crate::error::{invalid, Result};

/// `weftos://<mesh>/projects/<ULID>/releases/<channel>/store`.
///
/// The bucket (`r-<ulid>-<h12(channel)>`) is S1's name-to-bucket mapping; a backend is already
/// bound to one bucket, so this crate only carries the name. It is written into every signed
/// manifest, so a manifest cannot be replayed into another project's or channel's store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseStoreName {
    mesh: String,
    project: ulid::Ulid,
    channel: String,
}

impl ReleaseStoreName {
    pub fn new(mesh: &str, project: &str, channel: &str) -> Result<Self> {
        if mesh.is_empty()
            || mesh.len() > 63
            || !mesh.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '.'))
            || !mesh.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
            || mesh.ends_with(['.', '-'])
        {
            return Err(invalid("mesh", "1-63 lowercase letters, digits, '-' or '.', alphanumeric at both ends"));
        }
        let project: ulid::Ulid =
            project.parse().map_err(|_| invalid("project", "not a ULID"))?;
        validate_channel(channel)?;
        Ok(Self { mesh: mesh.to_string(), project, channel: channel.to_string() })
    }

    pub fn channel(&self) -> &str {
        &self.channel
    }

    /// The canonical name, as recorded in manifests.
    pub fn uri(&self) -> String {
        format!("weftos://{}/projects/{}/releases/{}/store", self.mesh, self.project, self.channel)
    }
}

impl std::fmt::Display for ReleaseStoreName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.uri())
    }
}

/// Lowercase (bucket names are lowercase), 1-63 characters, starts alphanumeric.
pub fn validate_channel(channel: &str) -> Result<()> {
    let ok = !channel.is_empty()
        && channel.len() <= 63
        && channel.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && channel
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'))
        && !channel.contains("..");
    if ok {
        Ok(())
    } else {
        Err(invalid("channel", "1-63 lowercase letters, digits, '-', '_' or '.', starting alphanumeric"))
    }
}

/// Names Windows will not make files of, with or without an extension (`con`, `con.txt`).
const WINDOWS_DEVICE_NAMES: [&str; 22] = [
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8", "com9", "lpt1", "lpt2", "lpt3", "lpt4",
    "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// 1-128 of `[a-z0-9._-]`, not starting with `.` or `-`, not ending with `.`, no `..`, and not a Windows
/// device name. A release key is `releases/<id>/manifest.json` and `tombstones/<id>`, and RustFS on a
/// Windows host stores a key as a file: it rejects `: * ? " | < >`, folds case (so `Build1` and `build1`
/// would be one object there and two on Linux), drops a trailing `.`, and cannot make `con` or `nul`.
/// Lowercase-only plus these rules makes every id safe on both. (Uppercase is refused rather than escaped:
/// an escape would make the same build id name two different keys depending on the host.)
pub fn validate_build_id(id: &str) -> Result<()> {
    let stem = id.split('.').next().unwrap_or("");
    let ok = !id.is_empty()
        && id.len() <= 128
        && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'))
        && !id.starts_with(['.', '-'])
        && !id.ends_with('.')
        && !id.contains("..")
        && !WINDOWS_DEVICE_NAMES.contains(&stem);
    if ok {
        Ok(())
    } else {
        Err(invalid(
            "build id",
            "1-128 lowercase letters, digits, '-', '_', '.', not starting with '.' or '-', not ending with '.', not a Windows device name (con, nul, com1...)",
        ))
    }
}

pub(crate) const BLOBS: &str = "blobs/";
pub(crate) const RELEASES: &str = "releases/";
pub(crate) const TOMBSTONES: &str = "tombstones/";
pub(crate) const QUARANTINE: &str = "quarantine/";

pub(crate) fn blob_key(hash: &str) -> String {
    format!("{BLOBS}{hash}")
}

pub(crate) fn manifest_key(build_id: &str) -> String {
    format!("{RELEASES}{build_id}/manifest.json")
}

pub(crate) fn quarantine_key(hash: &str) -> String {
    format!("{QUARANTINE}{hash}")
}

pub(crate) fn tombstone_key(build_id: &str) -> String {
    format!("{TOMBSTONES}{build_id}")
}

/// `releases/<id>/manifest.json` -> `<id>`; anything else (other files, deeper paths) -> `None`.
pub(crate) fn build_id_from_manifest_key(key: &str) -> Option<&str> {
    let rest = key.strip_prefix(RELEASES)?;
    let id = rest.strip_suffix("/manifest.json")?;
    (!id.contains('/') && validate_build_id(id).is_ok()).then_some(id)
}
