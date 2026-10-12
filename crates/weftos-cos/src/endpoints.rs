//! Remote endpoints (cos.v1.1): per-endpoint bearer tokens and the tailnet
//! address the API may be served on.
//!
//! A token is created by `weftos-cos endpoint add <name>`, stored mode 0600 as
//! `<tokens_dir>/<name>.token`, and presented as `Authorization: Bearer ...`.
//! Requests from a loopback peer need no token (the local console); every
//! other peer must present one, and acts only as the endpoint it belongs to.

use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};

use crate::CosError;
use crate::conversation::{HOST_ENDPOINT, valid_endpoint_name};

/// Token file suffix.
const SUFFIX: &str = ".token";

/// Whether `ip` is a Tailscale address: IPv4 in 100.64.0.0/10.
pub fn is_tailnet_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.octets()[0] == 100 && (v4.octets()[1] & 0xC0) == 64,
        IpAddr::V6(_) => false,
    }
}

/// The tailnet IPv4 in the output of `tailscale ip -4`. Anything that is not
/// a tailnet address is refused, never replaced by a wildcard.
pub fn parse_tailnet_ip(output: &str) -> Result<Ipv4Addr, CosError> {
    let line = output
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .ok_or_else(|| CosError::Config("tailscale reported no IPv4 address; is it logged in?".into()))?;
    let ip: Ipv4Addr = line.parse().map_err(|_| CosError::Config(format!("tailscale ip -4 answered {line:?}, not an IPv4 address")))?;
    if !is_tailnet_ip(IpAddr::V4(ip)) {
        return Err(CosError::Config(format!("{ip} is not a tailnet address (100.64.0.0/10); refusing to bind it")));
    }
    Ok(ip)
}

/// This machine's Tailscale IPv4, from `$WEFTOS_TAILSCALE_BIN`, `tailscale`
/// on the path, or the macOS app's binary.
pub fn tailnet_ip() -> Result<Ipv4Addr, CosError> {
    let mut candidates: Vec<String> = Vec::new();
    if let Ok(bin) = std::env::var("WEFTOS_TAILSCALE_BIN") {
        candidates.push(bin);
    }
    candidates.push("tailscale".into());
    candidates.push("/Applications/Tailscale.app/Contents/MacOS/Tailscale".into());
    let mut last = String::from("tailscale was not found");
    for bin in candidates {
        match std::process::Command::new(&bin).args(["ip", "-4"]).output() {
            Ok(out) if out.status.success() => return parse_tailnet_ip(&String::from_utf8_lossy(&out.stdout)),
            Ok(out) => last = format!("{bin} ip -4 failed: {}", String::from_utf8_lossy(&out.stderr).trim()),
            Err(e) => last = format!("{bin}: {e}"),
        }
    }
    Err(CosError::Config(format!("cannot resolve the tailnet address: {last}")))
}

/// Who a request is from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Caller {
    /// A loopback peer: the local console, trusted without a token.
    Local,
    /// A remote peer holding the token of this endpoint.
    Endpoint(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthError {
    /// No bearer token on a non-loopback request.
    Missing,
    /// A token no endpoint holds.
    Invalid,
}

/// Directory of per-endpoint token files.
#[derive(Debug, Clone)]
pub struct TokenStore {
    dir: PathBuf,
}

impl TokenStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn path_of(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{name}{SUFFIX}"))
    }

    /// Create the token for `name` and return the file's path. The token is
    /// only ever written to that file. An existing endpoint is refused;
    /// delete its file to rotate.
    pub fn add(&self, name: &str) -> Result<PathBuf, CosError> {
        if !valid_endpoint_name(name) {
            return Err(CosError::Config(format!("invalid endpoint name {name:?}: use 1 to 32 characters of a-z, 0-9, _ or -")));
        }
        if name == HOST_ENDPOINT {
            return Err(CosError::Config("`host` is the local endpoint and takes no token".into()));
        }
        std::fs::create_dir_all(&self.dir).map_err(|e| CosError::Config(format!("create {}: {e}", self.dir.display())))?;
        let path = self.path_of(name);
        let token = format!("wfe_{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
        write_private_new(&path, &token)?;
        Ok(path)
    }

    /// Endpoint names that have a token file.
    pub fn names(&self) -> Vec<String> {
        let mut out: Vec<String> = std::fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| e.file_name().to_str().and_then(|n| n.strip_suffix(SUFFIX)).map(str::to_string))
            .filter(|n| valid_endpoint_name(n) && n != HOST_ENDPOINT)
            .collect();
        out.sort();
        out
    }

    /// The endpoint whose token is `presented`. Files readable by group or
    /// others are ignored, so a loosened token stops working rather than
    /// being trusted. Read per call, so a new endpoint needs no restart.
    pub fn verify(&self, presented: &str) -> Option<String> {
        if presented.is_empty() {
            return None;
        }
        let mut found = None;
        for name in self.names() {
            let path = self.path_of(&name);
            if !is_private(&path) {
                continue;
            }
            let Ok(token) = std::fs::read_to_string(&path) else { continue };
            // No early exit: every file is compared.
            if constant_time_eq(token.trim().as_bytes(), presented.as_bytes()) {
                found = Some(name);
            }
        }
        found
    }
}

/// Decide who a request is from. A loopback peer is `Local`; any other peer
/// (or an unknown one) needs a valid bearer token.
pub fn authorize(peer: Option<IpAddr>, authorization: Option<&str>, store: &TokenStore) -> Result<Caller, AuthError> {
    if peer.is_some_and(|ip| ip.is_loopback()) {
        return Ok(Caller::Local);
    }
    let header = authorization.ok_or(AuthError::Missing)?;
    let token = header.strip_prefix("Bearer ").map(str::trim).filter(|t| !t.is_empty()).ok_or(AuthError::Missing)?;
    store.verify(token).map(Caller::Endpoint).ok_or(AuthError::Invalid)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(unix)]
fn is_private(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).map(|m| m.permissions().mode() & 0o077 == 0).unwrap_or(false)
}

#[cfg(not(unix))]
fn is_private(path: &Path) -> bool {
    path.exists()
}

fn write_private_new(path: &Path, token: &str) -> Result<(), CosError> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            CosError::Config(format!("{} already exists; delete it to issue a new token", path.display()))
        } else {
            CosError::Config(format!("create {}: {e}", path.display()))
        }
    })?;
    f.write_all(token.as_bytes()).map_err(|e| CosError::Config(format!("write {}: {e}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_cgnat_addresses_are_tailnet() {
        for ok in ["100.64.0.1", "100.90.170.87", "100.127.255.254"] {
            assert!(is_tailnet_ip(ok.parse().unwrap()), "{ok}");
        }
        for bad in ["0.0.0.0", "127.0.0.1", "192.168.1.5", "10.0.0.1", "100.63.255.255", "100.128.0.1", "::1", "fd7a:115c:a1e0::1"] {
            assert!(!is_tailnet_ip(bad.parse().unwrap()), "{bad}");
        }
    }

    #[test]
    fn tailscale_output_is_parsed_or_refused() {
        assert_eq!(parse_tailnet_ip("100.101.102.103\n").unwrap(), Ipv4Addr::new(100, 101, 102, 103));
        for bad in ["", "\n", "0.0.0.0\n", "192.168.1.9\n", "127.0.0.1", "not an ip", "fd7a:115c::1"] {
            assert!(matches!(parse_tailnet_ip(bad), Err(CosError::Config(_))), "{bad:?}");
        }
    }

    #[test]
    fn add_writes_a_private_token_and_verify_finds_its_endpoint() {
        let dir = tempfile::tempdir().unwrap();
        let store = TokenStore::new(dir.path().join("eps"));
        let path = store.add("mentra").unwrap();
        let token = std::fs::read_to_string(&path).unwrap();
        assert!(token.starts_with("wfe_") && token.len() >= 64);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert_eq!(store.verify(&token).as_deref(), Some("mentra"));
        assert_eq!(store.verify("wfe_nope"), None);
        assert_eq!(store.verify(""), None);
        assert_eq!(store.names(), ["mentra"]);
        // A second add does not rotate silently.
        assert!(matches!(store.add("mentra"), Err(CosError::Config(_))));
    }

    #[test]
    fn bad_names_and_host_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let store = TokenStore::new(dir.path());
        for bad in ["", "host", "Has Space", "../x", "a/b", &"x".repeat(33)] {
            assert!(matches!(store.add(bad), Err(CosError::Config(_))), "{bad:?}");
        }
        assert!(store.names().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_loosened_token_file_stops_working() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = TokenStore::new(dir.path());
        let path = store.add("mentra").unwrap();
        let token = std::fs::read_to_string(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(store.verify(&token), None);
    }

    #[test]
    fn loopback_needs_no_token_and_everyone_else_does() {
        let dir = tempfile::tempdir().unwrap();
        let store = TokenStore::new(dir.path());
        let token = std::fs::read_to_string(store.add("mentra").unwrap()).unwrap();
        let lo: Option<IpAddr> = Some("127.0.0.1".parse().unwrap());
        let tail: Option<IpAddr> = Some("100.64.0.9".parse().unwrap());
        assert_eq!(authorize(lo, None, &store), Ok(Caller::Local));
        assert_eq!(authorize(tail, None, &store), Err(AuthError::Missing));
        assert_eq!(authorize(tail, Some("Basic abc"), &store), Err(AuthError::Missing));
        assert_eq!(authorize(tail, Some("Bearer wfe_wrong"), &store), Err(AuthError::Invalid));
        assert_eq!(authorize(tail, Some(&format!("Bearer {token}")), &store), Ok(Caller::Endpoint("mentra".into())));
        // An unknown peer is not trusted as loopback.
        assert_eq!(authorize(None, None, &store), Err(AuthError::Missing));
    }
}
