//! `[dashboard]` in `~/.weftos/weave.toml`: the daemon-native reporter to the
//! WeftOS dashboard (replaces the `heartbeat.sh` systemd timer).
//!
//! ```toml
//! [dashboard]
//! enabled = true
//! url = "https://weftos-dashboard.vercel.app"
//! node_id = "a5e70b99-a919-4ff1-9f44-5fcba69d439c"
//! installation_id = "photo-gallery"
//! token_file = "/home/weftos/.weftos/dashboard/node.token"   # 0600, owned by the daemon's uid
//! interval_secs = 60
//! # gateway_url = "http://127.0.0.1:8080"                    # reported as-is when set
//! # units = ["weftos.service", "weftos-gateway.service"]     # systemd --user units to report
//! # allow_remote_rotate = true                               # accept a mesh rotate from a controller
//! # report_workspaces = true                                 # report project checkouts + git state
//! # kind = "machine"                                          # machine | project-daemon | dev | vm
//! ```
//!
//! Off by default. A section that is present but invalid stops the reporter
//! from starting (the daemon keeps running) and names the key at fault.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

/// Shortest and longest accepted `interval_secs`.
pub const MIN_INTERVAL_SECS: u64 = 5;
/// See [`MIN_INTERVAL_SECS`].
pub const MAX_INTERVAL_SECS: u64 = 3600;
const MAX_UNITS: usize = 16;

/// The `[dashboard]` section.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DashboardConfig {
    /// Master switch (default off).
    #[serde(default)]
    pub enabled: bool,
    /// Dashboard base URL: `https://`, or `http://` to a loopback host.
    #[serde(default)]
    pub url: String,
    /// This node's id in the dashboard (a UUID).
    #[serde(default)]
    pub node_id: String,
    /// Installation name reported as `installation_id` and `host`.
    #[serde(default)]
    pub installation_id: String,
    /// Where the `wft_` node token lives (absolute; 0600; owned by this uid).
    #[serde(default)]
    pub token_file: Option<PathBuf>,
    /// Seconds between heartbeats.
    #[serde(default = "default_interval")]
    pub interval_secs: u64,
    /// Gateway URL to report; absent, derived from `[gateway]` when its API is on.
    #[serde(default)]
    pub gateway_url: Option<String>,
    /// `systemctl --user` units whose state is reported (Linux, when present).
    #[serde(default = "default_units")]
    pub units: Vec<String>,
    /// Accept `dashboard.token.rotate` from a mesh controller (default on;
    /// the controller must still be in `workload-host.json`).
    #[serde(default = "yes")]
    pub allow_remote_rotate: bool,
    /// Report each registered project's checkout and git state as
    /// `report.workspaces` (default on; ADR-108). Counts and refnames only.
    #[serde(default = "yes")]
    pub report_workspaces: bool,
    /// What kind of node this is, reported as `report.kind` (default `machine`).
    #[serde(default)]
    pub kind: NodeKind,
}

/// `[dashboard] kind`: what the node is, for the dashboard's fleet view.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum NodeKind {
    /// A whole machine running the user daemon (the default).
    #[default]
    Machine,
    /// A per-project daemon.
    ProjectDaemon,
    /// A developer workstation or checkout.
    Dev,
    /// A virtual machine or container host.
    Vm,
}

fn default_interval() -> u64 {
    60
}
fn default_units() -> Vec<String> {
    vec!["weftos.service".into(), "weftos-gateway.service".into()]
}
fn yes() -> bool {
    true
}

impl Default for DashboardConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            url: String::new(),
            node_id: String::new(),
            installation_id: String::new(),
            token_file: None,
            interval_secs: default_interval(),
            gateway_url: None,
            units: default_units(),
            allow_remote_rotate: true,
            report_workspaces: true,
            kind: NodeKind::default(),
        }
    }
}

fn loopback_host(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "[::1]")
}

impl DashboardConfig {
    /// Parse the `dashboard` table of a merged config value (absent: disabled).
    pub fn from_config(root: &Value) -> Result<Self, String> {
        match root.get("dashboard") {
            None => Ok(Self::default()),
            Some(v) => serde_json::from_value(v.clone()).map_err(|e| format!("[dashboard]: {e}")),
        }
    }

    /// Boundary validation (only an enabled section is checked). Does not
    /// touch the filesystem; see [`check_token_file`].
    pub fn validate(&self) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }
        self.base_url()?;
        uuid::Uuid::parse_str(&self.node_id)
            .map_err(|e| format!("[dashboard] node_id {:?} is not a UUID: {e}", self.node_id))?;
        let id = &self.installation_id;
        if id.is_empty() || id.len() > 128 || !id.chars().all(|c| c.is_ascii_graphic() || c == ' ') {
            return Err("[dashboard] installation_id must be 1..=128 printable ASCII characters".into());
        }
        match &self.token_file {
            Some(p) if p.is_absolute() => {}
            Some(p) => return Err(format!("[dashboard] token_file {} must be an absolute path", p.display())),
            None => return Err("[dashboard] token_file is required when enabled".into()),
        }
        if !(MIN_INTERVAL_SECS..=MAX_INTERVAL_SECS).contains(&self.interval_secs) {
            return Err(format!(
                "[dashboard] interval_secs must be {MIN_INTERVAL_SECS}..={MAX_INTERVAL_SECS}"
            ));
        }
        if self.units.len() > MAX_UNITS {
            return Err(format!("[dashboard] units lists more than {MAX_UNITS} entries"));
        }
        for u in &self.units {
            let ok = !u.is_empty()
                && u.len() <= 128
                && !u.starts_with('-')
                && u.chars().all(|c| c.is_ascii_alphanumeric() || "@:._-".contains(c));
            if !ok {
                return Err(format!("[dashboard] unit {u:?} is not a systemd unit name"));
            }
        }
        if let Some(g) = &self.gateway_url
            && (g.len() > 512 || !(g.starts_with("http://") || g.starts_with("https://")))
        {
            return Err("[dashboard] gateway_url must be an http(s) URL".into());
        }
        Ok(())
    }

    /// The base URL without a trailing slash, or why it is refused: https, or
    /// http to loopback only (the bearer token must not cross a network in clear).
    pub fn base_url(&self) -> Result<String, String> {
        let url = self.url.trim_end_matches('/');
        let (rest, https) = match (url.strip_prefix("https://"), url.strip_prefix("http://")) {
            (Some(r), _) => (r, true),
            (None, Some(r)) => (r, false),
            _ => return Err(format!("[dashboard] url {:?} must start with https://", self.url)),
        };
        let authority = rest.split('/').next().unwrap_or("");
        if authority.is_empty() || authority.contains('@') || rest.contains(['?', '#']) {
            return Err(format!(
                "[dashboard] url {:?} must be scheme://host[:port][/path] with no credentials, query or fragment",
                self.url
            ));
        }
        if !https {
            let host = if let Some(end) = authority.strip_prefix('[').and(authority.find(']')) {
                &authority[..=end]
            } else {
                authority.split(':').next().unwrap_or("")
            };
            if !loopback_host(host) {
                return Err(format!(
                    "[dashboard] url {:?}: plain http is only accepted for a loopback host; use https",
                    self.url
                ));
            }
        }
        Ok(url.to_owned())
    }

    /// The token file path (validated config only).
    pub fn token_path(&self) -> Result<&Path, String> {
        self.token_file.as_deref().ok_or_else(|| "[dashboard] token_file is not set".to_string())
    }
}

/// Refuse a token file that is not a regular 0600 file owned by this process's
/// uid, or that sits in a directory others can write.
#[cfg(unix)]
pub fn check_token_file(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let meta = std::fs::symlink_metadata(path)
        .map_err(|e| format!("token file {}: {e}", path.display()))?;
    if !meta.file_type().is_file() {
        return Err(format!("token file {} is not a regular file (symlinks are refused)", path.display()));
    }
    let mode = meta.permissions().mode() & 0o777;
    if mode != 0o600 {
        return Err(format!(
            "token file {} has mode {mode:03o}; it must be 600 (chmod 600 {})",
            path.display(),
            path.display()
        ));
    }
    // SAFETY: geteuid has no preconditions and cannot fail.
    let me = unsafe { libc::geteuid() };
    if meta.uid() != me {
        return Err(format!(
            "token file {} is owned by uid {}, not the daemon's uid {me}",
            path.display(),
            meta.uid()
        ));
    }
    if let Some(dir) = path.parent() {
        let d = std::fs::metadata(dir).map_err(|e| format!("token directory {}: {e}", dir.display()))?;
        if d.permissions().mode() & 0o022 != 0 {
            return Err(format!(
                "token directory {} is writable by group or others; chmod go-w it",
                dir.display()
            ));
        }
    }
    Ok(())
}

/// Non-unix platforms have no uid or mode to check, so the reporter refuses.
#[cfg(not(unix))]
pub fn check_token_file(path: &Path) -> Result<(), String> {
    Err(format!("token file {}: the dashboard reporter needs a unix uid and 0600 mode", path.display()))
}

/// Read the token (trimmed) after [`check_token_file`]. The value is never
/// logged; it must be printable ASCII without whitespace, so it is safe in a header.
pub fn read_token(path: &Path) -> Result<String, String> {
    check_token_file(path)?;
    let raw = std::fs::read_to_string(path).map_err(|e| format!("token file {}: {e}", path.display()))?;
    let t = raw.trim();
    if t.is_empty() {
        return Err(format!("token file {} is empty", path.display()));
    }
    if t.len() > 512 || !t.chars().all(|c| c.is_ascii_graphic()) {
        return Err(format!("token file {} does not hold a token", path.display()));
    }
    Ok(t.to_owned())
}

/// Load `[dashboard]` from the user's `weave.toml` (missing file: disabled).
pub async fn load(home: &Path) -> Result<DashboardConfig, String> {
    use clawft_platform::{Platform, config_loader};
    let platform = clawft_platform::NativePlatform::new();
    let path = crate::user_daemon::user_weave_toml(home);
    let v = config_loader::load_weave_toml_file(platform.fs(), &path)
        .await
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let cfg = DashboardConfig::from_config(&v)?;
    cfg.validate()?;
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn good() -> DashboardConfig {
        DashboardConfig {
            enabled: true,
            url: "https://weftos-dashboard.vercel.app".into(),
            node_id: "a5e70b99-a919-4ff1-9f44-5fcba69d439c".into(),
            installation_id: "photo-gallery".into(),
            token_file: Some("/home/weftos/.weftos/dashboard/node.token".into()),
            ..Default::default()
        }
    }

    #[test]
    fn absent_section_is_off_and_a_disabled_one_is_not_validated() {
        let c = DashboardConfig::from_config(&json!({})).unwrap();
        assert!(!c.enabled);
        c.validate().unwrap();
        DashboardConfig::from_config(&json!({"dashboard": {"enabled": false, "url": "garbage"}}))
            .unwrap()
            .validate()
            .unwrap();
    }

    #[test]
    fn a_complete_section_parses_with_defaults() {
        let c = DashboardConfig::from_config(&json!({"dashboard": {
            "enabled": true, "url": "https://d.example/", "node_id": "a5e70b99-a919-4ff1-9f44-5fcba69d439c",
            "installation_id": "pg", "token_file": "/x/node.token"}}))
        .unwrap();
        c.validate().unwrap();
        assert_eq!(c.interval_secs, 60);
        assert_eq!(c.base_url().unwrap(), "https://d.example");
        assert!(c.allow_remote_rotate);
        assert_eq!(c.units.len(), 2);
    }

    #[test]
    fn unknown_keys_and_bad_values_are_refused_naming_the_key() {
        assert!(DashboardConfig::from_config(&json!({"dashboard": {"enabeld": true}})).is_err());
        for (mutate, needle) in [
            (Box::new(|c: &mut DashboardConfig| c.node_id = "nope".into()) as Box<dyn Fn(&mut DashboardConfig)>, "node_id"),
            (Box::new(|c| c.installation_id.clear()), "installation_id"),
            (Box::new(|c| c.token_file = None), "token_file"),
            (Box::new(|c| c.token_file = Some("rel/token".into())), "absolute"),
            (Box::new(|c| c.interval_secs = 1), "interval_secs"),
            (Box::new(|c| c.units = vec!["--now".into()]), "unit"),
            (Box::new(|c| c.gateway_url = Some("ftp://x".into())), "gateway_url"),
        ] {
            let mut c = good();
            mutate(&mut c);
            let e = c.validate().unwrap_err();
            assert!(e.contains(needle), "{e}");
        }
    }

    #[test]
    fn plain_http_is_only_for_loopback_and_urls_carry_no_credentials() {
        let mut c = good();
        for ok in ["https://d.example", "http://127.0.0.1:3000", "http://localhost:3000/x", "http://[::1]:3000"] {
            c.url = ok.into();
            c.base_url().unwrap_or_else(|e| panic!("{ok}: {e}"));
        }
        for bad in [
            "http://d.example", "http://10.0.0.5:3000", "ftp://d", "d.example", "https://u:p@d.example",
            "https://d.example/?x=1", "https://", "http://127.0.0.1.evil.test",
        ] {
            c.url = bad.into();
            assert!(c.base_url().is_err(), "{bad} was accepted");
        }
    }

    #[cfg(unix)]
    #[test]
    fn token_file_must_be_a_0600_regular_file_in_a_private_directory() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let p = dir.path().join("node.token");
        assert!(check_token_file(&p).unwrap_err().contains("node.token"), "missing");
        std::fs::write(&p, "wft_abc\n").unwrap();
        for mode in [0o644, 0o640, 0o660, 0o400, 0o700] {
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
            let e = check_token_file(&p).unwrap_err();
            assert!(e.contains("must be 600"), "{mode:o}: {e}");
            assert!(read_token(&p).is_err());
        }
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        check_token_file(&p).unwrap();
        assert_eq!(read_token(&p).unwrap(), "wft_abc");

        let link = dir.path().join("link.token");
        std::os::unix::fs::symlink(&p, &link).unwrap();
        assert!(check_token_file(&link).unwrap_err().contains("not a regular file"));

        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o770)).unwrap();
        assert!(check_token_file(&p).unwrap_err().contains("directory"));
    }

    #[cfg(unix)]
    #[test]
    fn an_empty_or_odd_token_file_is_not_a_token() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let p = dir.path().join("t");
        for body in ["", "  \n", "wft_a b", "wft_\u{e9}"] {
            std::fs::write(&p, body).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
            assert!(read_token(&p).is_err(), "{body:?}");
        }
    }
}
