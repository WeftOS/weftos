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

/// 1-128 of `[A-Za-z0-9._-]`, not starting with `.` or `-`, no `..`.
pub fn validate_build_id(id: &str) -> Result<()> {
    let ok = !id.is_empty()
        && id.len() <= 128
        && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !id.starts_with(['.', '-'])
        && !id.contains("..");
    if ok {
        Ok(())
    } else {
        Err(invalid("build id", "1-128 of letters, digits, '-', '_', '.', not starting with '.' or '-'"))
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
