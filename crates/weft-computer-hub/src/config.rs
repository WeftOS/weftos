//! Hub configuration, loaded from a TOML file. Which machines may serve and
//! which minters are trusted are data here, never code (owner decision D2).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use xai_tool_protocol::HostKind;

/// Everything that can make a config file unusable. Loading never falls back
/// to a default for a malformed file.
#[derive(Debug, PartialEq, Eq)]
pub enum ConfigError {
    Read(String),
    Parse(String),
    Invalid(String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read(m) => write!(f, "config read failed: {m}"),
            Self::Parse(m) => write!(f, "config parse failed: {m}"),
            Self::Invalid(m) => write!(f, "config invalid: {m}"),
        }
    }
}
impl std::error::Error for ConfigError {}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    listen: RawListen,
    #[serde(default)]
    auth: RawAuth,
    #[serde(default)]
    machines: Vec<RawMachine>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawListen {
    #[serde(default = "default_addr")]
    addr: String,
}
impl Default for RawListen {
    fn default() -> Self {
        Self {
            addr: default_addr(),
        }
    }
}
fn default_addr() -> String {
    "127.0.0.1:9490".to_owned()
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAuth {
    #[serde(default = "default_max_ttl")]
    max_ttl_secs: u64,
    /// `jti` values refused even when signature and expiry are good.
    #[serde(default)]
    revoked_jti: Vec<String>,
    #[serde(default)]
    minters: Vec<RawMinter>,
}
fn default_max_ttl() -> u64 {
    24 * 3600
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMinter {
    id: String,
    host_kind: String,
    key_file: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMachine {
    id: String,
    #[serde(default)]
    description: String,
    minter: String,
}

/// A trusted token minter. `host_kind` is what the hub reports for every
/// server connected under this minter's credentials.
#[derive(Clone)]
pub struct Minter {
    pub id: String,
    pub host_kind: HostKind,
    pub key: Vec<u8>,
}

impl std::fmt::Debug for Minter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Minter")
            .field("id", &self.id)
            .field("host_kind", &self.host_kind)
            .field("key", &"<redacted>")
            .finish()
    }
}

/// One machine allowed to register as a tool server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Machine {
    pub id: String,
    pub description: String,
    /// The only minter whose credentials may serve this machine.
    pub minter: String,
}

#[derive(Debug, Clone)]
pub struct HubConfig {
    pub addr: String,
    pub max_ttl_secs: u64,
    pub revoked_jti: Vec<String>,
    pub minters: HashMap<String, Minter>,
    pub machines: HashMap<String, Machine>,
}

/// Minimum accepted key length, bytes.
pub const MIN_KEY_BYTES: usize = 32;

pub fn parse_host_kind(s: &str) -> Result<HostKind, ConfigError> {
    match s {
        "desktop" => Ok(HostKind::Desktop),
        "container" => Ok(HostKind::Container),
        "sandbox" => Ok(HostKind::Sandbox),
        other => Err(ConfigError::Invalid(format!(
            "host_kind {other:?} is not one of desktop, container, sandbox"
        ))),
    }
}

/// Read a key file: hex text, at least [`MIN_KEY_BYTES`] bytes decoded, and
/// (on unix) not readable by group or other.
pub fn read_key_file(path: &Path) -> Result<Vec<u8>, ConfigError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(path)
            .map_err(|e| ConfigError::Read(format!("{}: {e}", path.display())))?;
        if meta.permissions().mode() & 0o077 != 0 {
            return Err(ConfigError::Invalid(format!(
                "key file {} must not be accessible by group or other (chmod 600)",
                path.display()
            )));
        }
    }
    let text = std::fs::read_to_string(path)
        .map_err(|e| ConfigError::Read(format!("{}: {e}", path.display())))?;
    let key = decode_hex(text.trim())
        .ok_or_else(|| ConfigError::Invalid(format!("{}: key is not hex", path.display())))?;
    if key.len() < MIN_KEY_BYTES {
        return Err(ConfigError::Invalid(format!(
            "{}: key is {} bytes, need at least {MIN_KEY_BYTES}",
            path.display(),
            key.len()
        )));
    }
    Ok(key)
}

pub fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || s.is_empty() {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

impl HubConfig {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| ConfigError::Read(format!("{}: {e}", path.display())))?;
        let base = path.parent().unwrap_or_else(|| Path::new("."));
        Self::from_toml(&text, base)
    }

    /// Parse and validate. Relative key files resolve against `base`.
    pub fn from_toml(text: &str, base: &Path) -> Result<Self, ConfigError> {
        let raw: RawConfig = toml::from_str(text).map_err(|e| ConfigError::Parse(e.to_string()))?;
        let mut minters = HashMap::new();
        for m in raw.auth.minters {
            if m.id.is_empty() || m.id.contains('.') {
                return Err(ConfigError::Invalid(format!(
                    "minter id {:?} must be non-empty and contain no '.'",
                    m.id
                )));
            }
            let host_kind = parse_host_kind(&m.host_kind)?;
            let key_path = if m.key_file.is_absolute() {
                m.key_file
            } else {
                base.join(m.key_file)
            };
            let key = read_key_file(&key_path)?;
            if minters
                .insert(
                    m.id.clone(),
                    Minter {
                        id: m.id.clone(),
                        host_kind,
                        key,
                    },
                )
                .is_some()
            {
                return Err(ConfigError::Invalid(format!("duplicate minter {:?}", m.id)));
            }
        }
        if minters.is_empty() {
            return Err(ConfigError::Invalid(
                "no [[auth.minters]]: the hub refuses to run without a trusted minter".into(),
            ));
        }
        let mut machines = HashMap::new();
        for m in raw.machines {
            xai_tool_protocol::ServerId::new(m.id.clone())
                .map_err(|e| ConfigError::Invalid(format!("machine id {:?}: {e}", m.id)))?;
            if !minters.contains_key(&m.minter) {
                return Err(ConfigError::Invalid(format!(
                    "machine {:?} names unknown minter {:?}",
                    m.id, m.minter
                )));
            }
            let machine = Machine {
                id: m.id.clone(),
                description: m.description,
                minter: m.minter,
            };
            if machines.insert(m.id.clone(), machine).is_some() {
                return Err(ConfigError::Invalid(format!(
                    "duplicate machine {:?}",
                    m.id
                )));
            }
        }
        if machines.is_empty() {
            return Err(ConfigError::Invalid(
                "no [[machines]]: nothing may register, refusing to start".into(),
            ));
        }
        if raw.auth.max_ttl_secs == 0 {
            return Err(ConfigError::Invalid("auth.max_ttl_secs must be > 0".into()));
        }
        Ok(Self {
            addr: raw.listen.addr,
            max_ttl_secs: raw.auth.max_ttl_secs,
            revoked_jti: raw.auth.revoked_jti,
            minters,
            machines,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_file(dir: &Path, name: &str, hex: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, hex).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        p
    }
    const HEX32: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

    fn good(dir: &Path) -> String {
        key_file(dir, "k", HEX32);
        r#"
[[auth.minters]]
id = "xpc-mint"
host_kind = "desktop"
key_file = "k"
[[machines]]
id = "xpc"
description = "aepod-xpc"
minter = "xpc-mint"
"#
        .to_owned()
    }

    #[test]
    fn loads_good_config_with_loopback_default() {
        let d = tempfile::tempdir().unwrap();
        let c = HubConfig::from_toml(&good(d.path()), d.path()).unwrap();
        assert_eq!(c.addr, "127.0.0.1:9490");
        assert_eq!(c.machines["xpc"].minter, "xpc-mint");
        assert_eq!(c.minters["xpc-mint"].host_kind, HostKind::Desktop);
    }

    #[test]
    fn refuses_unknown_field_empty_machines_and_unknown_minter() {
        let d = tempfile::tempdir().unwrap();
        let g = good(d.path());
        assert!(matches!(
            HubConfig::from_toml(&format!("bogus = 1\n{g}"), d.path()),
            Err(ConfigError::Parse(_))
        ));
        let no_machines = g.split("[[machines]]").next().unwrap().to_owned();
        assert!(matches!(
            HubConfig::from_toml(&no_machines, d.path()),
            Err(ConfigError::Invalid(m)) if m.contains("no [[machines]]")
        ));
        let bad_minter = g.replace("minter = \"xpc-mint\"", "minter = \"nope\"");
        assert!(matches!(
            HubConfig::from_toml(&bad_minter, d.path()),
            Err(ConfigError::Invalid(m)) if m.contains("unknown minter")
        ));
    }

    #[test]
    fn refuses_short_or_non_hex_key_and_bad_host_kind() {
        let d = tempfile::tempdir().unwrap();
        let g = good(d.path());
        key_file(d.path(), "k", "0011");
        assert!(
            matches!(HubConfig::from_toml(&g, d.path()), Err(ConfigError::Invalid(m)) if m.contains("need at least"))
        );
        key_file(d.path(), "k", "zz");
        assert!(
            matches!(HubConfig::from_toml(&g, d.path()), Err(ConfigError::Invalid(m)) if m.contains("not hex"))
        );
        key_file(d.path(), "k", HEX32);
        let bad = g.replace("desktop", "mainframe");
        assert!(
            matches!(HubConfig::from_toml(&bad, d.path()), Err(ConfigError::Invalid(m)) if m.contains("host_kind"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn refuses_world_readable_key_file() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let g = good(d.path());
        std::fs::set_permissions(d.path().join("k"), std::fs::Permissions::from_mode(0o644))
            .unwrap();
        assert!(
            matches!(HubConfig::from_toml(&g, d.path()), Err(ConfigError::Invalid(m)) if m.contains("chmod 600"))
        );
    }
}
