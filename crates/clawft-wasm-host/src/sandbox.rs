//! Plugin sandbox enforcement for WASM plugins.
//!
//! Provides [`PluginSandbox`] -- the runtime security context for a loaded
//! WASM plugin. All host function implementations validate operations against
//! the sandbox before executing any side-effecting operation.
//!
//! This module also provides the validation functions for HTTP requests,
//! file access, and environment variable access.

use std::collections::HashSet;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use clawft_plugin::error::WasmHostError;
use clawft_plugin::{PluginPermissions, PluginResourceConfig, VoiceCapability};

// ---------------------------------------------------------------------------
// NetworkAllowlist
// ---------------------------------------------------------------------------

/// Parsed network allowlist supporting exact match and wildcard subdomains.
pub struct NetworkAllowlist {
    /// Allow all hosts (`"*"` entry).
    pub allow_all: bool,
    /// Exact hostnames (lowercase).
    pub exact: HashSet<String>,
    /// Wildcard suffixes (e.g., `".example.com"` for `"*.example.com"`).
    pub wildcard_suffixes: Vec<String>,
}

impl NetworkAllowlist {
    /// Build from the `network` field of `PluginPermissions`.
    pub fn from_permissions(network: &[String]) -> Self {
        let mut exact = HashSet::new();
        let mut wildcard_suffixes = Vec::new();
        let mut allow_all = false;

        for entry in network {
            if entry == "*" {
                allow_all = true;
            } else if let Some(suffix) = entry.strip_prefix("*.") {
                wildcard_suffixes.push(format!(".{}", suffix.to_lowercase()));
            } else {
                exact.insert(entry.to_lowercase());
            }
        }

        Self {
            allow_all,
            exact,
            wildcard_suffixes,
        }
    }

    /// Check whether a hostname is allowed.
    pub fn is_allowed(&self, host: &str) -> bool {
        if self.allow_all {
            return true;
        }
        let host_lower = host.to_lowercase();
        if self.exact.contains(&host_lower) {
            return true;
        }
        for suffix in &self.wildcard_suffixes {
            if host_lower.ends_with(suffix) {
                return true;
            }
        }
        false
    }
}

// ---------------------------------------------------------------------------
// RateCounter
// ---------------------------------------------------------------------------

/// Simple fixed-window rate counter.
///
/// Tracks the number of events within a sliding window. When the window
/// expires, the counter resets.
pub struct RateCounter {
    /// Maximum allowed count per window.
    pub limit: u64,
    /// Current count in the active window.
    pub count: AtomicU64,
    /// Start of the current window.
    pub window_start: Mutex<Instant>,
    /// Window duration.
    pub window_duration: Duration,
}

impl RateCounter {
    /// Create a new rate counter.
    pub fn new(limit: u64, window_duration: Duration) -> Self {
        Self {
            limit,
            count: AtomicU64::new(0),
            window_start: Mutex::new(Instant::now()),
            window_duration,
        }
    }

    /// Try to increment the counter. Returns `true` if within limit.
    pub fn try_increment(&self) -> bool {
        let mut start = self.window_start.lock().unwrap();
        if start.elapsed() >= self.window_duration {
            *start = Instant::now();
            self.count.store(1, Ordering::Relaxed);
            true
        } else {
            let prev = self.count.fetch_add(1, Ordering::Relaxed);
            prev < self.limit
        }
    }
}

// ---------------------------------------------------------------------------
// PluginSandbox
// ---------------------------------------------------------------------------

/// Runtime sandbox state for a loaded WASM plugin.
///
/// Holds the permission set, resource budgets, and rate-limit counters.
/// Created once per plugin instance and passed to all host function
/// implementations.
pub struct PluginSandbox {
    /// Plugin identifier (for logging).
    pub plugin_id: String,
    /// Declared permissions from the plugin manifest.
    pub permissions: PluginPermissions,
    /// Canonicalized allowed filesystem paths (resolved at load time).
    pub allowed_paths_canonical: Vec<PathBuf>,
    /// Fuel budget per invocation.
    pub fuel_budget: u64,
    /// Memory limit in bytes.
    pub memory_limit: usize,
    /// HTTP rate limiter state.
    pub http_rate: RateCounter,
    /// Log rate limiter state.
    pub log_rate: RateCounter,
    /// Network allowlist as a parsed set (for fast lookup).
    pub network_allowlist: NetworkAllowlist,
    /// Env var allowlist as a set (for fast lookup).
    pub env_var_allowlist: HashSet<String>,
    /// Env var patterns that trigger warnings.
    pub env_var_sensitive_patterns: Vec<regex::Regex>,
    /// Voice capability granted to this plugin (WEFT-556 / SC-10).
    ///
    /// Populated at load time AFTER
    /// [`clawft_plugin::manifest::validate_voice_capability`] has
    /// confirmed the manifest's [`VoiceCapability`] is fully covered by
    /// the operator's [`VoiceGrants`]. The value stored here is the
    /// **manifest's request**, not the grant matrix — the load-time
    /// validator ensures the request is a subset, so guarding on the
    /// stored request is equivalent to guarding on the grant.
    ///
    /// `None` means the plugin has no voice access; every voice host
    /// call short-circuits to [`WasmHostError::CapabilityDenied`].
    pub voice: Option<VoiceCapability>,
}

impl PluginSandbox {
    /// Build a sandbox from a manifest's permissions and resource config.
    pub fn from_manifest(
        plugin_id: String,
        permissions: PluginPermissions,
        resources: &PluginResourceConfig,
    ) -> Self {
        // Canonicalize allowed filesystem paths at load time.
        // Paths that do not exist or cannot be canonicalized are silently
        // skipped -- they will simply never match any access check.
        let allowed_paths_canonical = permissions
            .filesystem
            .iter()
            .filter_map(|p| {
                // Expand ~ to home directory
                let expanded: String = if let Some(rest) = p.strip_prefix("~/") {
                    if let Some(home) = dirs::home_dir() {
                        home.join(rest).to_string_lossy().into_owned()
                    } else {
                        p.clone()
                    }
                } else {
                    p.clone()
                };
                std::fs::canonicalize(&expanded).ok()
            })
            .collect();

        let env_var_allowlist: HashSet<String> = permissions.env_vars.iter().cloned().collect();

        let env_var_sensitive_patterns = vec![
            regex::Regex::new(r"(?i)_SECRET").unwrap(),
            regex::Regex::new(r"(?i)_PASSWORD").unwrap(),
            regex::Regex::new(r"(?i)_TOKEN").unwrap(),
        ];

        Self {
            network_allowlist: NetworkAllowlist::from_permissions(&permissions.network),
            allowed_paths_canonical,
            fuel_budget: resources.max_fuel,
            memory_limit: resources.max_memory_mb.saturating_mul(1024 * 1024),
            http_rate: RateCounter::new(
                resources.max_http_requests_per_minute,
                Duration::from_secs(60),
            ),
            log_rate: RateCounter::new(
                resources.max_log_messages_per_minute,
                Duration::from_secs(60),
            ),
            env_var_allowlist,
            env_var_sensitive_patterns,
            permissions,
            plugin_id,
            voice: None,
        }
    }

    /// Attach the validated voice capability for this plugin
    /// (WEFT-556 / SC-10).
    ///
    /// Should be called by the loader AFTER
    /// [`clawft_plugin::manifest::validate_voice_capability`] has
    /// confirmed the request is fully covered by the operator's grant.
    /// Passing `None` (or an empty `VoiceCapability`) leaves the plugin
    /// with no voice access; every voice host call will short-circuit.
    pub fn with_voice_capability(mut self, voice: Option<VoiceCapability>) -> Self {
        self.voice = voice.filter(|v| !v.is_empty());
        self
    }

    /// Check whether the plugin may receive a voice transcript publish
    /// for `topic` (WEFT-556 / SC-10).
    ///
    /// The host MUST consult this before forwarding any substrate
    /// transcript publish to the plugin. Returns
    /// [`WasmHostError::CapabilityDenied`] when:
    ///
    /// - The plugin has no voice capability at all, OR
    /// - `read_transcripts` is not granted, OR
    /// - The plugin declared a non-empty `transcript_topics` allowlist
    ///   and `topic` is not in it.
    pub fn check_can_read_transcript(&self, topic: &str) -> Result<(), WasmHostError> {
        let Some(voice) = &self.voice else {
            return Err(WasmHostError::CapabilityDenied {
                capability: "voice.read_transcripts".into(),
            });
        };
        if !voice.read_transcripts {
            return Err(WasmHostError::CapabilityDenied {
                capability: "voice.read_transcripts".into(),
            });
        }
        if !voice.transcript_topics.is_empty()
            && !voice.transcript_topics.iter().any(|t| t == topic)
        {
            return Err(WasmHostError::CapabilityDenied {
                capability: format!("voice.transcript_topic:{topic}"),
            });
        }
        Ok(())
    }

    /// Check whether the plugin may dispatch commands derived from a
    /// voice transcript (WEFT-556 / SC-10).
    pub fn check_can_dispatch_command(&self) -> Result<(), WasmHostError> {
        match &self.voice {
            Some(v) if v.dispatch_commands => Ok(()),
            _ => Err(WasmHostError::CapabilityDenied {
                capability: "voice.dispatch_commands".into(),
            }),
        }
    }

    /// Check whether the plugin may call `host.synthesize_audio(text)`
    /// (WEFT-556 / SC-10).
    pub fn check_can_synthesize_audio(&self) -> Result<(), WasmHostError> {
        match &self.voice {
            Some(v) if v.synthesize_audio => Ok(()),
            _ => Err(WasmHostError::CapabilityDenied {
                capability: "voice.synthesize_audio".into(),
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// HTTP Validation
// ---------------------------------------------------------------------------

/// Errors from HTTP request validation.
#[derive(Debug, thiserror::Error)]
pub enum HttpValidationError {
    #[error("invalid URL: {0}")]
    InvalidUrl(String),
    #[error("scheme not allowed: {0}")]
    DisallowedScheme(String),
    #[error("host not in network allowlist: {0}")]
    HostNotAllowed(String),
    #[error("request to private/reserved IP denied: {0}")]
    PrivateIp(IpAddr),
    #[error("rate limit exceeded: HTTP requests")]
    RateLimited,
    #[error("request body too large: {actual} bytes, max {max}")]
    BodyTooLarge { actual: usize, max: usize },
    #[error("network access not permitted")]
    NetworkDenied,
}

const BLOCKED_SCHEMES: &[&str] = &["file", "data", "javascript", "ftp", "gopher"];
const MAX_REQUEST_BODY: usize = 1_048_576; // 1 MB

/// Validate an HTTP request against the plugin's sandbox permissions.
///
/// Returns the parsed URL if the request is permitted. Returns `Err` with a
/// specific reason if denied. Does NOT execute the request.
pub fn validate_http_request(
    sandbox: &PluginSandbox,
    _method: &str,
    url_str: &str,
    body: Option<&str>,
) -> Result<url::Url, HttpValidationError> {
    // 1. Parse URL
    let url =
        url::Url::parse(url_str).map_err(|e| HttpValidationError::InvalidUrl(e.to_string()))?;

    // 2. Scheme check
    let scheme = url.scheme();
    if BLOCKED_SCHEMES.contains(&scheme) || (scheme != "http" && scheme != "https") {
        return Err(HttpValidationError::DisallowedScheme(scheme.to_string()));
    }

    // 3. Network allowlist
    if sandbox.permissions.network.is_empty() {
        return Err(HttpValidationError::NetworkDenied);
    }
    let host = url
        .host_str()
        .ok_or_else(|| HttpValidationError::InvalidUrl("no host in URL".into()))?;
    if !sandbox.network_allowlist.is_allowed(host) {
        return Err(HttpValidationError::HostNotAllowed(host.to_string()));
    }

    // 4. SSRF check for literal IPs
    if let Some(ip) = url
        .host()
        .and_then(|h| match h {
            url::Host::Ipv4(v4) => Some(IpAddr::V4(v4)),
            url::Host::Ipv6(v6) => Some(IpAddr::V6(v6)),
            _ => None,
        })
        .filter(is_private_ip)
    {
        return Err(HttpValidationError::PrivateIp(ip));
    }

    // 5. Rate limit
    if !sandbox.http_rate.try_increment() {
        return Err(HttpValidationError::RateLimited);
    }

    // 6. Body size
    if let Some(body) = body.filter(|b| b.len() > MAX_REQUEST_BODY) {
        return Err(HttpValidationError::BodyTooLarge {
            actual: body.len(),
            max: MAX_REQUEST_BODY,
        });
    }

    Ok(url)
}

/// Check whether an IP address is in a private/reserved range.
///
/// Blocks RFC 1918, loopback, link-local, CGN, null, and IPv6 equivalents.
pub fn is_private_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            o[0] == 10 // 10.0.0.0/8
                || (o[0] == 172 && (16..=31).contains(&o[1])) // 172.16.0.0/12
                || (o[0] == 192 && o[1] == 168) // 192.168.0.0/16
                || o[0] == 127 // 127.0.0.0/8
                || (o[0] == 169 && o[1] == 254) // 169.254.0.0/16
                || (o[0] == 100 && (64..=127).contains(&o[1])) // 100.64.0.0/10
                || o[0] == 0 // 0.0.0.0/8
        }
        IpAddr::V6(v6) => {
            // Handle IPv4-mapped IPv6 (::ffff:x.x.x.x)
            if let Some(mapped_v4) = v6.to_ipv4_mapped() {
                return is_private_ip(&IpAddr::V4(mapped_v4));
            }
            v6.is_loopback() // ::1
                || v6.segments()[0] == 0xfe80 // fe80::/10 link-local
                || v6.segments()[0] & 0xfe00 == 0xfc00 // fc00::/7 ULA
                || v6.is_unspecified() // ::
        }
    }
}

// ---------------------------------------------------------------------------
// File Validation
// ---------------------------------------------------------------------------

/// Errors from file access validation.
#[derive(Debug, thiserror::Error)]
pub enum FileValidationError {
    #[error("filesystem access denied: path outside sandbox")]
    OutsideSandbox,
    #[error("path does not exist or cannot be resolved")]
    CannotResolve,
    #[error("symlink points outside sandbox: {0}")]
    SymlinkEscape(PathBuf),
    #[error("file too large: {actual} bytes, max {max}")]
    FileTooLarge { actual: u64, max: u64 },
    #[error("filesystem access not permitted")]
    FsDenied,
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Maximum file read size (8 MB).
pub const MAX_READ_SIZE: u64 = 8 * 1024 * 1024;
/// Maximum file write size (4 MB).
pub const MAX_WRITE_SIZE: usize = 4 * 1024 * 1024;

/// Validate a file access request against the plugin's sandbox.
///
/// `write` indicates whether this is a write operation. For writes,
/// the parent directory is canonicalized (since the file may not exist yet).
/// Returns the canonical path if access is permitted.
pub fn validate_file_access(
    sandbox: &PluginSandbox,
    path: &Path,
    write: bool,
) -> Result<PathBuf, FileValidationError> {
    // 0. Check that filesystem permissions exist at all
    if sandbox.allowed_paths_canonical.is_empty() {
        return Err(FileValidationError::FsDenied);
    }

    // 1. Canonicalize the path
    let canonical = if write {
        let parent = path.parent().ok_or(FileValidationError::CannotResolve)?;
        let parent_canonical =
            std::fs::canonicalize(parent).map_err(|_| FileValidationError::CannotResolve)?;
        let filename = path.file_name().ok_or(FileValidationError::CannotResolve)?;
        parent_canonical.join(filename)
    } else {
        std::fs::canonicalize(path).map_err(|_| FileValidationError::CannotResolve)?
    };

    // 2. Sandbox containment check
    let in_sandbox = sandbox
        .allowed_paths_canonical
        .iter()
        .any(|allowed| canonical.starts_with(allowed));
    if !in_sandbox {
        return Err(FileValidationError::OutsideSandbox);
    }

    // 3. Symlink traversal check on the original (non-canonical) path
    let mut accumulated = PathBuf::new();
    for component in path.components() {
        accumulated.push(component);
        if accumulated.is_symlink() {
            let link_target = std::fs::read_link(&accumulated)?;
            let resolved = if link_target.is_absolute() {
                link_target
            } else {
                accumulated
                    .parent()
                    .unwrap_or(Path::new("/"))
                    .join(&link_target)
            };
            let resolved_canonical = std::fs::canonicalize(&resolved)
                .map_err(|_| FileValidationError::SymlinkEscape(accumulated.clone()))?;

            let symlink_in_sandbox = sandbox
                .allowed_paths_canonical
                .iter()
                .any(|allowed| resolved_canonical.starts_with(allowed));
            if !symlink_in_sandbox {
                return Err(FileValidationError::SymlinkEscape(accumulated));
            }
        }
    }

    // 4. Size check (for reads only)
    if let (false, Ok(metadata)) = (write, std::fs::metadata(&canonical))
        && metadata.len() > MAX_READ_SIZE
    {
        return Err(FileValidationError::FileTooLarge {
            actual: metadata.len(),
            max: MAX_READ_SIZE,
        });
    }

    Ok(canonical)
}

// ---------------------------------------------------------------------------
// Environment Variable Validation
// ---------------------------------------------------------------------------

/// Hardcoded deny list -- never accessible regardless of allowlist.
const HARDCODED_ENV_DENY: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "SHELL",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
];

/// Validate an environment variable access request.
///
/// Returns `Some(value)` if the variable is permitted and set.
/// Returns `None` if the variable is not permitted OR not set.
/// Never returns an error -- the plugin cannot distinguish denial from absence.
pub fn validate_env_access(sandbox: &PluginSandbox, var_name: &str) -> Option<String> {
    // 1. Check hardcoded deny list
    if HARDCODED_ENV_DENY.contains(&var_name) {
        tracing::debug!(
            plugin = %sandbox.plugin_id,
            var = var_name,
            "env var access denied: hardcoded deny list"
        );
        return None;
    }

    // 2. Check allowlist
    if !sandbox.env_var_allowlist.contains(var_name) {
        tracing::debug!(
            plugin = %sandbox.plugin_id,
            var = var_name,
            "env var access denied: not in allowlist"
        );
        return None;
    }

    // 3. Warn on sensitive-looking vars (even if in allowlist)
    for pattern in &sandbox.env_var_sensitive_patterns {
        if pattern.is_match(var_name) {
            tracing::warn!(
                plugin = %sandbox.plugin_id,
                var = var_name,
                "plugin accessing sensitive-pattern env var (approved via allowlist)"
            );
            break;
        }
    }

    // 4. Read from environment
    std::env::var(var_name).ok()
}

// ---------------------------------------------------------------------------
// Plugin Size Validation
// ---------------------------------------------------------------------------

/// Maximum uncompressed WASM module size (300 KB).
pub const MAX_WASM_SIZE_UNCOMPRESSED: u64 = 300 * 1024;
/// Maximum gzipped WASM module size (120 KB).
pub const MAX_WASM_SIZE_GZIPPED: u64 = 120 * 1024;
/// Maximum plugin directory size (10 MB).
pub const MAX_PLUGIN_DIR_SIZE: u64 = 10 * 1024 * 1024;

/// Validate the size of a WASM module at install/load time.
pub fn validate_wasm_size(uncompressed_size: u64) -> Result<(), String> {
    if uncompressed_size > MAX_WASM_SIZE_UNCOMPRESSED {
        return Err(format!(
            "WASM module too large: {} bytes (max {} bytes)",
            uncompressed_size, MAX_WASM_SIZE_UNCOMPRESSED
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Log Validation
// ---------------------------------------------------------------------------

/// Maximum log message size (4 KB).
pub const MAX_LOG_MESSAGE_SIZE: usize = 4096;

/// Longest prefix of `s` that is at most `max` bytes and ends on a char boundary.
pub(crate) fn truncate_utf8(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Validate and potentially truncate a log message.
///
/// Returns the (possibly truncated) message and whether it was rate-limited.
pub fn validate_log_message(sandbox: &PluginSandbox, message: &str) -> (String, bool) {
    // Rate limit check
    if !sandbox.log_rate.try_increment() {
        return (String::new(), true);
    }

    // Size limit
    let truncated = if message.len() > MAX_LOG_MESSAGE_SIZE {
        let mut msg = truncate_utf8(message, MAX_LOG_MESSAGE_SIZE - 16).to_string();
        msg.push_str("... [truncated]");
        msg
    } else {
        message.to_string()
    };

    (truncated, false)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;


    /// Create a temp dir and return its canonical path.
    ///
    /// On macOS, `temp_dir()` is under `/var/folders/...` while `/var` is a
    /// symlink to `/private/var`. Without canonicalizing, intermediate-symlink
    /// checks in [`validate_file_access`] report `SymlinkEscape("/var")` for
    /// otherwise valid in-sandbox paths.
    fn canonical_temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(name);
        let _ = std::fs::create_dir_all(&dir);
        std::fs::canonicalize(&dir).expect("canonicalize temp dir")
    }


    /// Helper to create a sandbox with given network permissions.
    fn sandbox_with_network(network: Vec<String>) -> PluginSandbox {
        let permissions = PluginPermissions {
            network,
            ..Default::default()
        };
        PluginSandbox::from_manifest(
            "test-plugin".into(),
            permissions,
            &PluginResourceConfig::default(),
        )
    }

    /// Helper to create a sandbox with given env var permissions.
    fn sandbox_with_env(env_vars: Vec<String>) -> PluginSandbox {
        let permissions = PluginPermissions {
            env_vars,
            ..Default::default()
        };
        PluginSandbox::from_manifest(
            "test-plugin".into(),
            permissions,
            &PluginResourceConfig::default(),
        )
    }

    /// Helper to create a sandbox with given filesystem permissions.
    fn sandbox_with_fs(filesystem: Vec<String>) -> PluginSandbox {
        let permissions = PluginPermissions {
            filesystem,
            ..Default::default()
        };
        PluginSandbox::from_manifest(
            "test-plugin".into(),
            permissions,
            &PluginResourceConfig::default(),
        )
    }

    // -- NetworkAllowlist tests --

    #[test]
    fn test_network_allowlist_exact_match() {
        let al = NetworkAllowlist::from_permissions(&["api.example.com".into()]);
        assert!(al.is_allowed("api.example.com"));
        assert!(al.is_allowed("API.EXAMPLE.COM")); // case insensitive
        assert!(!al.is_allowed("other.example.com"));
    }

    #[test]
    fn test_network_allowlist_wildcard_match() {
        let al = NetworkAllowlist::from_permissions(&["*.example.com".into()]);
        assert!(al.is_allowed("sub.example.com"));
        assert!(al.is_allowed("deep.sub.example.com"));
    }

    #[test]
    fn test_network_allowlist_wildcard_no_bare() {
        let al = NetworkAllowlist::from_permissions(&["*.example.com".into()]);
        assert!(!al.is_allowed("example.com")); // bare domain not matched
    }

    #[test]
    fn test_network_allowlist_allow_all() {
        let al = NetworkAllowlist::from_permissions(&["*".into()]);
        assert!(al.is_allowed("anything.example.com"));
        assert!(al.is_allowed("localhost"));
    }

    #[test]
    fn test_network_allowlist_empty_denies_all() {
        let al = NetworkAllowlist::from_permissions(&[]);
        assert!(!al.is_allowed("api.example.com"));
        assert!(!al.is_allowed("localhost"));
    }

    // -- is_private_ip tests --

    #[test]
    fn test_is_private_ip_loopback() {
        assert!(is_private_ip(&"127.0.0.1".parse().unwrap()));
        assert!(is_private_ip(&"127.0.0.2".parse().unwrap()));
        assert!(is_private_ip(&"127.255.255.255".parse().unwrap()));
    }

    #[test]
    fn test_is_private_ip_rfc1918() {
        assert!(is_private_ip(&"10.0.0.1".parse().unwrap()));
        assert!(is_private_ip(&"10.255.255.255".parse().unwrap()));
        assert!(is_private_ip(&"172.16.0.1".parse().unwrap()));
        assert!(is_private_ip(&"172.31.255.255".parse().unwrap()));
        assert!(is_private_ip(&"192.168.0.1".parse().unwrap()));
        assert!(is_private_ip(&"192.168.255.255".parse().unwrap()));
    }

    #[test]
    fn test_is_private_ip_not_rfc1918() {
        assert!(!is_private_ip(&"172.15.0.1".parse().unwrap()));
        assert!(!is_private_ip(&"172.32.0.1".parse().unwrap()));
    }

    #[test]
    fn test_is_private_ip_link_local() {
        assert!(is_private_ip(&"169.254.0.1".parse().unwrap()));
        assert!(is_private_ip(&"169.254.169.254".parse().unwrap())); // cloud metadata
    }

    #[test]
    fn test_is_private_ip_cgn() {
        assert!(is_private_ip(&"100.64.0.1".parse().unwrap()));
        assert!(is_private_ip(&"100.127.255.255".parse().unwrap()));
        assert!(!is_private_ip(&"100.63.255.255".parse().unwrap()));
        assert!(!is_private_ip(&"100.128.0.0".parse().unwrap()));
    }

    #[test]
    fn test_is_private_ip_null() {
        assert!(is_private_ip(&"0.0.0.0".parse().unwrap()));
        assert!(is_private_ip(&"0.0.0.1".parse().unwrap()));
    }

    #[test]
    fn test_is_private_ip_ipv6_loopback() {
        assert!(is_private_ip(&"::1".parse().unwrap()));
    }

    #[test]
    fn test_is_private_ip_ipv6_mapped_v4() {
        assert!(is_private_ip(
            &"::ffff:127.0.0.1".parse::<IpAddr>().unwrap()
        ));
        assert!(is_private_ip(&"::ffff:10.0.0.1".parse::<IpAddr>().unwrap()));
    }

    #[test]
    fn test_is_private_ip_public() {
        assert!(!is_private_ip(&"8.8.8.8".parse().unwrap()));
        assert!(!is_private_ip(&"1.1.1.1".parse().unwrap()));
        assert!(!is_private_ip(&"203.0.113.1".parse().unwrap()));
    }

    // -- RateCounter tests --

    #[test]
    fn test_rate_counter_within_limit() {
        let counter = RateCounter::new(10, Duration::from_secs(60));
        for _ in 0..10 {
            assert!(counter.try_increment());
        }
    }

    #[test]
    fn test_rate_counter_exceeds_limit() {
        let counter = RateCounter::new(10, Duration::from_secs(60));
        for _ in 0..10 {
            assert!(counter.try_increment());
        }
        // 11th should fail
        assert!(!counter.try_increment());
    }

    #[test]
    fn test_rate_counter_resets_after_window() {
        let counter = RateCounter::new(10, Duration::from_millis(1));
        for _ in 0..10 {
            assert!(counter.try_increment());
        }
        assert!(!counter.try_increment());
        // Wait for window to expire
        std::thread::sleep(Duration::from_millis(5));
        // Should reset
        assert!(counter.try_increment());
    }

    // -- HTTP validation tests --

    #[test]
    fn t01_http_allowed_domain() {
        let sandbox = sandbox_with_network(vec!["api.example.com".into()]);
        let result = validate_http_request(&sandbox, "GET", "https://api.example.com/data", None);
        assert!(result.is_ok());
    }

    #[test]
    fn t02_http_denied_domain() {
        let sandbox = sandbox_with_network(vec!["api.example.com".into()]);
        let result = validate_http_request(&sandbox, "GET", "https://evil.example.com/data", None);
        assert!(matches!(
            result,
            Err(HttpValidationError::HostNotAllowed(_))
        ));
    }

    #[test]
    fn t03_http_file_scheme_blocked() {
        let sandbox = sandbox_with_network(vec!["*".into()]);
        let result = validate_http_request(&sandbox, "GET", "file:///etc/passwd", None);
        assert!(matches!(
            result,
            Err(HttpValidationError::DisallowedScheme(_))
        ));
    }

    #[test]
    fn t04_http_data_scheme_blocked() {
        let sandbox = sandbox_with_network(vec!["*".into()]);
        let result = validate_http_request(&sandbox, "GET", "data:text/html,hello", None);
        assert!(matches!(
            result,
            Err(HttpValidationError::DisallowedScheme(_))
        ));
    }

    #[test]
    fn t05_http_loopback_blocked() {
        let sandbox = sandbox_with_network(vec!["*".into()]);
        let result = validate_http_request(&sandbox, "GET", "http://127.0.0.1/", None);
        assert!(matches!(result, Err(HttpValidationError::PrivateIp(_))));
    }

    #[test]
    fn t06_http_ipv4_mapped_ipv6_loopback_blocked() {
        let sandbox = sandbox_with_network(vec!["*".into()]);
        let result = validate_http_request(&sandbox, "GET", "http://[::ffff:127.0.0.1]/", None);
        assert!(matches!(result, Err(HttpValidationError::PrivateIp(_))));
    }

    #[test]
    fn t07_http_cloud_metadata_blocked() {
        let sandbox = sandbox_with_network(vec!["*".into()]);
        let result = validate_http_request(
            &sandbox,
            "GET",
            "http://169.254.169.254/latest/meta-data/",
            None,
        );
        assert!(matches!(result, Err(HttpValidationError::PrivateIp(_))));
    }

    #[test]
    fn t08_http_rfc1918_blocked() {
        let sandbox = sandbox_with_network(vec!["*".into()]);
        let result = validate_http_request(&sandbox, "GET", "http://10.0.0.1/", None);
        assert!(matches!(result, Err(HttpValidationError::PrivateIp(_))));
    }

    #[test]
    fn t09_http_empty_network_denied() {
        let sandbox = sandbox_with_network(vec![]);
        let result = validate_http_request(&sandbox, "GET", "https://api.example.com/", None);
        assert!(matches!(result, Err(HttpValidationError::NetworkDenied)));
    }

    #[test]
    fn t10_http_rate_limit_exceeded() {
        let permissions = PluginPermissions {
            network: vec!["*".into()],
            ..Default::default()
        };
        let resources = PluginResourceConfig {
            max_http_requests_per_minute: 10,
            ..PluginResourceConfig::default()
        };
        let sandbox = PluginSandbox::from_manifest("test-plugin".into(), permissions, &resources);
        // Make 10 successful requests
        for i in 0..10 {
            let url = format!("https://example.com/{i}");
            assert!(
                validate_http_request(&sandbox, "GET", &url, None).is_ok(),
                "request {i} should succeed"
            );
        }
        // 11th should fail
        let result = validate_http_request(&sandbox, "GET", "https://example.com/11", None);
        assert!(matches!(result, Err(HttpValidationError::RateLimited)));
    }

    #[test]
    fn t11_http_body_too_large() {
        let sandbox = sandbox_with_network(vec!["*".into()]);
        let large_body = "x".repeat(2 * 1024 * 1024); // 2 MB
        let result =
            validate_http_request(&sandbox, "POST", "https://example.com/", Some(&large_body));
        assert!(matches!(
            result,
            Err(HttpValidationError::BodyTooLarge { .. })
        ));
    }

    #[test]
    fn t12_http_wildcard_subdomain_match() {
        let sandbox = sandbox_with_network(vec!["*.example.com".into()]);
        let result = validate_http_request(&sandbox, "GET", "https://sub.example.com/", None);
        assert!(result.is_ok());
    }

    #[test]
    fn t13_http_wildcard_no_bare_domain() {
        let sandbox = sandbox_with_network(vec!["*.example.com".into()]);
        let result = validate_http_request(&sandbox, "GET", "https://example.com/", None);
        assert!(matches!(
            result,
            Err(HttpValidationError::HostNotAllowed(_))
        ));
    }

    // -- Environment variable access tests --

    #[test]
    fn t23_env_access_allowed_and_set() {
        // Set the env var for this test
        // SAFETY: This test is single-threaded and the var name is unique.
        unsafe {
            std::env::set_var("CLAWFT_TEST_PLUGIN_VAR_T23", "test_value_t23");
        }
        let sandbox = sandbox_with_env(vec!["CLAWFT_TEST_PLUGIN_VAR_T23".into()]);
        let result = validate_env_access(&sandbox, "CLAWFT_TEST_PLUGIN_VAR_T23");
        assert_eq!(result, Some("test_value_t23".into()));
        unsafe {
            std::env::remove_var("CLAWFT_TEST_PLUGIN_VAR_T23");
        }
    }

    #[test]
    fn t24_env_access_allowed_but_not_set() {
        let sandbox = sandbox_with_env(vec!["NONEXISTENT_VAR_CLAWFT_T24".into()]);
        let result = validate_env_access(&sandbox, "NONEXISTENT_VAR_CLAWFT_T24");
        assert_eq!(result, None);
    }

    #[test]
    fn t25_env_access_not_in_allowlist() {
        let sandbox = sandbox_with_env(vec!["ALLOWED_VAR".into()]);
        let result = validate_env_access(&sandbox, "NOT_ALLOWED_VAR");
        assert_eq!(result, None);
    }

    #[test]
    fn t26_env_access_openai_key_denied() {
        let sandbox = sandbox_with_env(vec![]); // Not in allowlist
        let result = validate_env_access(&sandbox, "OPENAI_API_KEY");
        assert_eq!(result, None);
    }

    #[test]
    fn t27_env_access_empty_allowlist() {
        let sandbox = sandbox_with_env(vec![]);
        let result = validate_env_access(&sandbox, "ANY_VAR");
        assert_eq!(result, None);
    }

    #[test]
    fn test_env_access_hardcoded_deny() {
        // Even if in allowlist, hardcoded deny list blocks access
        let sandbox = sandbox_with_env(vec!["PATH".into(), "HOME".into(), "OPENAI_API_KEY".into()]);
        assert_eq!(validate_env_access(&sandbox, "PATH"), None);
        assert_eq!(validate_env_access(&sandbox, "HOME"), None);
        assert_eq!(validate_env_access(&sandbox, "OPENAI_API_KEY"), None);
        assert_eq!(validate_env_access(&sandbox, "AWS_SECRET_ACCESS_KEY"), None);
        assert_eq!(validate_env_access(&sandbox, "ANTHROPIC_API_KEY"), None);
    }

    // -- File validation tests --

    #[test]
    fn t21_fs_empty_permissions_denied() {
        let sandbox = sandbox_with_fs(vec![]);
        let result = validate_file_access(&sandbox, Path::new("/tmp/test.txt"), false);
        assert!(matches!(result, Err(FileValidationError::FsDenied)));
    }

    #[test]
    fn t14_fs_read_within_allowed_path() {
        // Create a temp directory and file
        let dir = canonical_temp_dir("clawft_test_t14");
        let file = dir.join("test.txt");
        std::fs::write(&file, "content").unwrap();

        let sandbox = sandbox_with_fs(vec![dir.to_string_lossy().to_string()]);
        let result = validate_file_access(&sandbox, &file, false);
        assert!(result.is_ok(), "expected Ok, got: {result:?}");

        // Cleanup
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn t15_fs_read_outside_allowed_path() {
        let dir = canonical_temp_dir("clawft_test_t15_allowed");

        let sandbox = sandbox_with_fs(vec![dir.to_string_lossy().to_string()]);
        // /etc/hosts should exist on Linux but is outside sandbox
        let result = validate_file_access(&sandbox, Path::new("/etc/hosts"), false);
        assert!(matches!(result, Err(FileValidationError::OutsideSandbox)));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn t18_fs_write_within_allowed_path() {
        let dir = canonical_temp_dir("clawft_test_t18");

        let sandbox = sandbox_with_fs(vec![dir.to_string_lossy().to_string()]);
        let file = dir.join("new_file.txt");
        let result = validate_file_access(&sandbox, &file, true);
        assert!(result.is_ok());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn t19_fs_write_outside_allowed_path() {
        let dir = canonical_temp_dir("clawft_test_t19_allowed");

        let sandbox = sandbox_with_fs(vec![dir.to_string_lossy().to_string()]);
        let result = validate_file_access(&sandbox, Path::new("/etc/new_file.txt"), true);
        assert!(matches!(result, Err(FileValidationError::OutsideSandbox)));

        let _ = std::fs::remove_dir_all(&dir);
    }

    // -- Sandbox creation tests --

    #[test]
    fn test_sandbox_from_manifest_defaults() {
        let sandbox = PluginSandbox::from_manifest(
            "test".into(),
            PluginPermissions::default(),
            &PluginResourceConfig::default(),
        );
        assert_eq!(sandbox.plugin_id, "test");
        assert_eq!(sandbox.fuel_budget, 1_000_000_000);
        assert_eq!(sandbox.memory_limit, 16 * 1024 * 1024);
        assert!(sandbox.allowed_paths_canonical.is_empty());
        assert!(sandbox.env_var_allowlist.is_empty());
        assert!(!sandbox.network_allowlist.allow_all);
    }

    #[test]
    fn test_sandbox_custom_resources() {
        let resources = PluginResourceConfig {
            max_fuel: 500_000_000,
            max_memory_mb: 32,
            ..PluginResourceConfig::default()
        };

        let sandbox =
            PluginSandbox::from_manifest("custom".into(), PluginPermissions::default(), &resources);
        assert_eq!(sandbox.fuel_budget, 500_000_000);
        assert_eq!(sandbox.memory_limit, 32 * 1024 * 1024);
    }

    // -- WASM size validation --

    #[test]
    fn t36_wasm_size_too_large() {
        let result = validate_wasm_size(400 * 1024); // 400 KB > 300 KB limit
        assert!(result.is_err());
    }

    #[test]
    fn test_wasm_size_within_limit() {
        let result = validate_wasm_size(200 * 1024); // 200 KB < 300 KB limit
        assert!(result.is_ok());
    }

    #[test]
    fn test_wasm_size_exactly_at_limit() {
        let result = validate_wasm_size(300 * 1024); // Exactly at limit
        assert!(result.is_ok());
    }

    // -- Log validation tests --

    #[test]
    fn t33_log_within_rate_limit() {
        let sandbox = PluginSandbox::from_manifest(
            "test".into(),
            PluginPermissions::default(),
            &PluginResourceConfig::default(),
        );
        for _ in 0..100 {
            let (msg, rate_limited) = validate_log_message(&sandbox, "test message");
            assert!(!rate_limited);
            assert_eq!(msg, "test message");
        }
    }

    #[test]
    fn t34_log_rate_limited() {
        let resources = PluginResourceConfig {
            max_log_messages_per_minute: 5,
            ..PluginResourceConfig::default()
        };
        let sandbox =
            PluginSandbox::from_manifest("test".into(), PluginPermissions::default(), &resources);
        // First 5 should succeed
        for _ in 0..5 {
            let (_, rate_limited) = validate_log_message(&sandbox, "msg");
            assert!(!rate_limited);
        }
        // 6th should be rate-limited
        let (_, rate_limited) = validate_log_message(&sandbox, "msg");
        assert!(rate_limited);
    }

    #[test]
    fn t35_log_message_truncated() {
        let sandbox = PluginSandbox::from_manifest(
            "test".into(),
            PluginPermissions::default(),
            &PluginResourceConfig::default(),
        );
        let long_msg = "x".repeat(8192); // 8 KB > 4 KB limit
        let (msg, _) = validate_log_message(&sandbox, &long_msg);
        assert!(msg.len() <= MAX_LOG_MESSAGE_SIZE);
        assert!(msg.ends_with("... [truncated]"));
    }

    #[test]
    fn log_truncation_respects_char_boundaries() {
        let sandbox = PluginSandbox::from_manifest(
            "test".into(),
            PluginPermissions::default(),
            &PluginResourceConfig::default(),
        );
        // 3-byte chars; the 4080-byte cut lands mid-character.
        let msg = format!("a{}", "\u{20AC}".repeat(5000));
        let (out, _) = validate_log_message(&sandbox, &msg);
        assert!(out.len() <= MAX_LOG_MESSAGE_SIZE);
        assert!(out.ends_with("... [truncated]"));
        assert_eq!(truncate_utf8("\u{1F600}", 2), "");
        assert_eq!(truncate_utf8("abc", 10), "abc");
    }

    // -- Cross-cutting tests --

    #[test]
    fn t44_independent_rate_counters() {
        let sandbox_a = sandbox_with_network(vec!["*".into()]);
        let sandbox_b = sandbox_with_network(vec!["*".into()]);

        // Exhaust sandbox A's rate limit
        for i in 0..10 {
            let url = format!("https://example.com/{i}");
            assert!(validate_http_request(&sandbox_a, "GET", &url, None).is_ok());
        }
        let result = validate_http_request(&sandbox_a, "GET", "https://example.com/11", None);
        assert!(matches!(result, Err(HttpValidationError::RateLimited)));

        // Sandbox B should still work
        let result = validate_http_request(&sandbox_b, "GET", "https://example.com/1", None);
        assert!(result.is_ok());
    }

    // -- Additional filesystem security tests --

    #[test]
    fn t16_fs_path_traversal_blocked() {
        // T16: Path traversal via ../../ is blocked by canonicalization
        let dir = canonical_temp_dir("clawft_test_t16");

        let sandbox = sandbox_with_fs(vec![dir.to_string_lossy().to_string()]);
        // Attempt to escape via path traversal
        let traversal_path = dir.join("..").join("..").join("etc").join("passwd");
        let result = validate_file_access(&sandbox, &traversal_path, false);
        assert!(
            matches!(
                result,
                Err(FileValidationError::OutsideSandbox) | Err(FileValidationError::CannotResolve)
            ),
            "expected OutsideSandbox or CannotResolve, got: {result:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn t17_fs_symlink_escape_blocked() {
        // T17: Symlink pointing outside sandbox is detected
        let dir = canonical_temp_dir("clawft_test_t17");

        let link_path = dir.join("escape_link");
        // Create symlink pointing outside sandbox (to /etc/hosts)
        let _ = std::fs::remove_file(&link_path); // clean up any stale link
        if std::os::unix::fs::symlink("/etc/hosts", &link_path).is_ok() {
            let sandbox = sandbox_with_fs(vec![dir.to_string_lossy().to_string()]);
            let result = validate_file_access(&sandbox, &link_path, false);
            // Should detect symlink escape: either OutsideSandbox (canonicalization
            // resolves to /etc/hosts) or SymlinkEscape (component walk detects it)
            assert!(
                result.is_err(),
                "symlink escape should be blocked, got: {result:?}"
            );
            match result.unwrap_err() {
                FileValidationError::OutsideSandbox | FileValidationError::SymlinkEscape(_) => {} // expected
                other => panic!("unexpected error variant: {other:?}"),
            }
        }
        // else: symlink creation failed (e.g., running as non-root in restricted env), skip

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn t20_fs_large_file_blocked() {
        // T20: File larger than 8 MB is rejected for reads
        let dir = canonical_temp_dir("clawft_test_t20");
        let large_file = dir.join("large.bin");

        // Create a file > 8 MB (we'll use a sparse approach -- write small data then
        // use set_len to extend it without writing all bytes)
        {
            let file = std::fs::File::create(&large_file).unwrap();
            file.set_len(9 * 1024 * 1024).unwrap(); // 9 MB
        }

        let sandbox = sandbox_with_fs(vec![dir.to_string_lossy().to_string()]);
        let result = validate_file_access(&sandbox, &large_file, false);
        assert!(
            matches!(result, Err(FileValidationError::FileTooLarge { .. })),
            "expected FileTooLarge, got: {result:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn t22_fs_symlink_write_escape_blocked() {
        // T22: Write via symlink pointing outside sandbox is blocked
        let dir = canonical_temp_dir("clawft_test_t22");
        let outside = canonical_temp_dir("clawft_test_t22_outside");
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::create_dir_all(&outside);

        let link_path = dir.join("escape_write_link");
        let _ = std::fs::remove_file(&link_path);
        let target_file = outside.join("written.txt");
        if std::os::unix::fs::symlink(&target_file, &link_path).is_ok() {
            let sandbox = sandbox_with_fs(vec![dir.to_string_lossy().to_string()]);
            let result = validate_file_access(&sandbox, &link_path, true);
            // Should detect the symlink escape
            assert!(
                result.is_err(),
                "symlink write escape should be blocked, got: {result:?}"
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&outside);
    }

    // -- Validate WASM gzip size --

    #[test]
    fn test_wasm_size_constants() {
        assert_eq!(MAX_WASM_SIZE_UNCOMPRESSED, 300 * 1024);
        assert_eq!(MAX_WASM_SIZE_GZIPPED, 120 * 1024);
        assert_eq!(MAX_PLUGIN_DIR_SIZE, 10 * 1024 * 1024);
    }

    #[test]
    fn test_max_read_write_constants() {
        assert_eq!(MAX_READ_SIZE, 8 * 1024 * 1024);
        assert_eq!(MAX_WRITE_SIZE, 4 * 1024 * 1024);
        assert_eq!(MAX_LOG_MESSAGE_SIZE, 4096);
    }

    // ── WEFT-556 / SC-10: voice capability runtime gating tests ─────

    fn empty_sandbox() -> PluginSandbox {
        PluginSandbox::from_manifest(
            "voice-test".into(),
            PluginPermissions::default(),
            &PluginResourceConfig::default(),
        )
    }

    #[test]
    fn voice_default_sandbox_has_no_voice_capability() {
        let sb = empty_sandbox();
        assert!(sb.voice.is_none());
    }

    #[test]
    fn voice_with_capability_filters_empty_to_none() {
        let sb = empty_sandbox().with_voice_capability(Some(VoiceCapability::default()));
        // An all-false VoiceCapability is treated as "no voice access"
        // so the sandbox should NOT store it.
        assert!(sb.voice.is_none());
    }

    #[test]
    fn voice_check_read_transcript_denies_without_capability() {
        let sb = empty_sandbox();
        let err = sb
            .check_can_read_transcript("weftos.voice.transcripts.v1")
            .expect_err("must deny");
        assert_eq!(
            err,
            WasmHostError::CapabilityDenied {
                capability: "voice.read_transcripts".into(),
            }
        );
    }

    #[test]
    fn voice_check_read_transcript_denies_when_only_synthesize() {
        // Plugin granted synthesize_audio but NOT read_transcripts ->
        // transcript publishes must still be denied.
        let voice = VoiceCapability {
            synthesize_audio: true,
            ..Default::default()
        };
        let sb = empty_sandbox().with_voice_capability(Some(voice));
        let err = sb
            .check_can_read_transcript("any.topic")
            .expect_err("must deny");
        assert_eq!(
            err,
            WasmHostError::CapabilityDenied {
                capability: "voice.read_transcripts".into(),
            }
        );
    }

    #[test]
    fn voice_check_read_transcript_allows_when_granted() {
        let voice = VoiceCapability {
            read_transcripts: true,
            ..Default::default()
        };
        let sb = empty_sandbox().with_voice_capability(Some(voice));
        assert!(sb.check_can_read_transcript("any.topic").is_ok());
    }

    #[test]
    fn voice_check_read_transcript_enforces_topic_allowlist() {
        let voice = VoiceCapability {
            read_transcripts: true,
            transcript_topics: vec!["weftos.voice.transcripts.v1".into()],
            ..Default::default()
        };
        let sb = empty_sandbox().with_voice_capability(Some(voice));
        assert!(
            sb.check_can_read_transcript("weftos.voice.transcripts.v1")
                .is_ok()
        );
        let err = sb
            .check_can_read_transcript("other.topic")
            .expect_err("must deny non-listed topic");
        match err {
            WasmHostError::CapabilityDenied { capability } => {
                assert_eq!(capability, "voice.transcript_topic:other.topic");
            }
            #[allow(unreachable_patterns)]
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn voice_check_dispatch_command_denies_without_capability() {
        let sb = empty_sandbox();
        let err = sb.check_can_dispatch_command().expect_err("must deny");
        assert_eq!(
            err,
            WasmHostError::CapabilityDenied {
                capability: "voice.dispatch_commands".into(),
            }
        );
    }

    #[test]
    fn voice_check_dispatch_command_denies_when_only_read() {
        // read_transcripts alone does NOT grant dispatch.
        let voice = VoiceCapability {
            read_transcripts: true,
            ..Default::default()
        };
        let sb = empty_sandbox().with_voice_capability(Some(voice));
        assert!(sb.check_can_dispatch_command().is_err());
    }

    #[test]
    fn voice_check_dispatch_command_allows_when_granted() {
        let voice = VoiceCapability {
            dispatch_commands: true,
            ..Default::default()
        };
        let sb = empty_sandbox().with_voice_capability(Some(voice));
        assert!(sb.check_can_dispatch_command().is_ok());
    }

    #[test]
    fn voice_check_synthesize_audio_denies_without_capability() {
        let sb = empty_sandbox();
        let err = sb.check_can_synthesize_audio().expect_err("must deny");
        assert_eq!(
            err,
            WasmHostError::CapabilityDenied {
                capability: "voice.synthesize_audio".into(),
            }
        );
    }

    #[test]
    fn voice_check_synthesize_audio_denies_when_only_read() {
        let voice = VoiceCapability {
            read_transcripts: true,
            ..Default::default()
        };
        let sb = empty_sandbox().with_voice_capability(Some(voice));
        assert!(sb.check_can_synthesize_audio().is_err());
    }

    #[test]
    fn voice_check_synthesize_audio_allows_when_granted() {
        let voice = VoiceCapability {
            synthesize_audio: true,
            ..Default::default()
        };
        let sb = empty_sandbox().with_voice_capability(Some(voice));
        assert!(sb.check_can_synthesize_audio().is_ok());
    }

    #[test]
    fn voice_capability_isolation_between_sandboxes() {
        // Each sandbox has its own voice cap; flipping one doesn't
        // affect the other.
        let sb_a = empty_sandbox().with_voice_capability(Some(VoiceCapability {
            read_transcripts: true,
            ..Default::default()
        }));
        let sb_b = empty_sandbox(); // no voice
        assert!(sb_a.check_can_read_transcript("t").is_ok());
        assert!(sb_b.check_can_read_transcript("t").is_err());
    }
}
