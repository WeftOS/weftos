//! MCP session capability tokens + client labels (ADR-075 G5 / WEFT-697).
//!
//! Aligns with ADR-071 spirit (capability tokens with scopes) without
//! requiring WASM panel machinery. Used by both stdio (implicit owner)
//! and HTTP/SSE remote serve (explicit bearer).
//!
//! # Scopes
//!
//! - **tools** — glob patterns over tool names (empty = profile surface)
//! - **workspace_roots** — optional path prefixes for FS tools
//! - **agent_spawn** — whether `agent_spawn` / spawn-class control tools run
//!
//! # Enterprise ceiling
//!
//! An optional ceiling intersects every token's scopes so multi-tenant
//! deployments can force reduced rights regardless of issued claims.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

// ── Client label ───────────────────────────────────────────────────────

/// Known MCP client harness labels for audit (ADR-075 §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ClientLabel {
    /// Grok Build CLI / TUI.
    Grok,
    /// Claude Code / Claude CLI.
    Claude,
    /// Unrecognized or omitted.
    #[default]
    Unknown,
}

impl ClientLabel {
    /// Parse from `clientInfo.name`, `X-MCP-Client` header, or free text.
    pub fn parse(s: &str) -> Self {
        let lower = s.trim().to_ascii_lowercase();
        if lower.is_empty() {
            return Self::Unknown;
        }
        if lower.contains("grok") || lower == "xai" {
            return Self::Grok;
        }
        if lower.contains("claude") || lower.contains("anthropic") {
            return Self::Claude;
        }
        Self::Unknown
    }

    /// Stable string for audit logs.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Grok => "grok",
            Self::Claude => "claude",
            Self::Unknown => "unknown",
        }
    }
}

impl std::fmt::Display for ClientLabel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ── Scopes ─────────────────────────────────────────────────────────────

/// Capability scopes bound to an MCP session (ADR-075 G5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionScopes {
    /// Glob patterns for tool names. `None` / empty = no extra restriction
    /// beyond the serve profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    /// Allowed workspace path prefixes. `None` = do not enforce here
    /// (workspace sandbox still applies via tool implementations).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_roots: Option<Vec<PathBuf>>,
    /// Whether agent lifecycle spawn is permitted.
    #[serde(default = "default_agent_spawn")]
    pub agent_spawn: bool,
}

fn default_agent_spawn() -> bool {
    true
}

impl Default for SessionScopes {
    fn default() -> Self {
        Self {
            tools: None,
            workspace_roots: None,
            agent_spawn: true,
        }
    }
}

impl SessionScopes {
    /// Full owner scopes (local stdio default).
    pub fn owner() -> Self {
        Self::default()
    }

    /// Enterprise reduced ceiling: no agent spawn, control+workspace tool
    /// globs only (no media / full dump).
    pub fn enterprise_reduced() -> Self {
        Self {
            tools: Some(vec![
                "status".into(),
                "agent_list".into(),
                "agent_get".into(),
                "agent_stop".into(),
                "skill_list".into(),
                "skill_get".into(),
                "read_file".into(),
                "write_file".into(),
                "list_dir".into(),
                "exec_shell".into(),
                "process_spawn".into(),
                "web_search".into(),
                "web_fetch".into(),
                "memory_*".into(),
                "window_*".into(),
            ]),
            workspace_roots: None,
            agent_spawn: false,
        }
    }

    /// Intersect `self` with a ceiling (enterprise forces reduced scopes).
    pub fn intersect(&self, ceiling: &SessionScopes) -> SessionScopes {
        let agent_spawn = self.agent_spawn && ceiling.agent_spawn;

        let tools = match (&self.tools, &ceiling.tools) {
            (None, None) => None,
            (Some(a), None) => Some(a.clone()),
            (None, Some(c)) => Some(c.clone()),
            (Some(a), Some(c)) => {
                // Keep patterns that match something in the other set, or
                // keep the tighter list (ceiling patterns when claim is broader).
                // Practical rule: require a name to match *both* lists.
                // Represent as: claim tools filtered by ceiling patterns, and
                // if claim is open use ceiling only.
                let mut out = Vec::new();
                for pat in a {
                    if c.iter().any(|cp| patterns_compatible(pat, cp)) {
                        out.push(pat.clone());
                    }
                }
                // Also include ceiling-only exact tools if claim is wildcard-ish.
                if a.iter().any(|p| p == "*" || p == "**") {
                    out = c.clone();
                }
                if out.is_empty() {
                    Some(c.clone())
                } else {
                    Some(out)
                }
            }
        };

        let workspace_roots = match (&self.workspace_roots, &ceiling.workspace_roots) {
            (None, None) => None,
            (Some(a), None) => Some(a.clone()),
            (None, Some(c)) => Some(c.clone()),
            (Some(a), Some(c)) => {
                // Keep roots that fall under a ceiling root.
                let mut out = Vec::new();
                for root in a {
                    if c.iter().any(|cr| path_under(root, cr) || path_under(cr, root)) {
                        // Prefer the more specific (longer) path when nested.
                        out.push(root.clone());
                    }
                }
                if out.is_empty() {
                    Some(c.clone())
                } else {
                    Some(out)
                }
            }
        };

        SessionScopes {
            tools,
            workspace_roots,
            agent_spawn,
        }
    }

    /// Whether a tool name is allowed by these scopes.
    pub fn allows_tool(&self, name: &str) -> bool {
        if is_agent_spawn_tool(name) && !self.agent_spawn {
            return false;
        }
        match &self.tools {
            None => true,
            Some(patterns) if patterns.is_empty() => true,
            Some(patterns) => patterns.iter().any(|p| glob_match(p, name)),
        }
    }

    /// Whether a filesystem path is inside allowed workspace roots.
    pub fn allows_workspace_path(&self, path: &Path) -> bool {
        match &self.workspace_roots {
            None => true,
            Some(roots) if roots.is_empty() => true,
            Some(roots) => roots.iter().any(|r| path_under(path, r)),
        }
    }

    /// Glob patterns suitable for [`super::middleware::PermissionFilter`].
    pub fn tool_patterns_for_filter(&self) -> Option<Vec<String>> {
        match &self.tools {
            None => None,
            Some(p) if p.is_empty() => None,
            Some(p) => {
                let mut pats = p.clone();
                if !self.agent_spawn {
                    // Ensure spawn tools stay out even if pattern is broad.
                    pats.retain(|pat| {
                        !pat.contains("agent_spawn") && pat.as_str() != "agent_*"
                    });
                }
                Some(pats)
            }
        }
    }
}

fn is_agent_spawn_tool(name: &str) -> bool {
    name == "agent_spawn" || name.ends_with("__agent_spawn")
}

fn path_under(path: &Path, root: &Path) -> bool {
    path.starts_with(root)
}

/// Two patterns are compatible if either matches the other as a name or
/// they share a common prefix shape (best-effort for intersect).
fn patterns_compatible(a: &str, b: &str) -> bool {
    a == b || glob_match(a, b) || glob_match(b, a)
}

fn glob_match(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let s: Vec<char> = name.chars().collect();
    let (mut pi, mut si, mut star, mut match_si) = (0usize, 0usize, None, 0usize);
    while si < s.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == s[si]) {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            match_si = si;
            pi += 1;
        } else if let Some(sp) = star {
            pi = sp + 1;
            match_si += 1;
            si = match_si;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

// ── Session capability ─────────────────────────────────────────────────

/// Issued session capability (metadata; secret token is the map key).
#[derive(Debug, Clone)]
pub struct SessionCapability {
    /// Opaque id for logs (not the secret).
    pub id: String,
    pub scopes: SessionScopes,
    pub client_label: ClientLabel,
    pub issued_at: Instant,
    pub expires_at: Option<Instant>,
}

impl SessionCapability {
    /// Whether the capability is still valid (not expired).
    pub fn is_valid(&self) -> bool {
        match self.expires_at {
            None => true,
            Some(exp) => Instant::now() < exp,
        }
    }
}

// ── Token store ────────────────────────────────────────────────────────

/// In-memory session token store for MCP remote auth (WEFT-696 / WEFT-697).
///
/// Supports:
/// - **Static bearer** secrets (shared env / CLI `--token`) → owner scopes
/// - **Issued capabilities** with custom scopes + client label
/// - **Enterprise ceiling** that intersects every validation
pub struct SessionTokenStore {
    /// secret → capability
    tokens: RwLock<HashMap<String, SessionCapability>>,
    /// Shared static secrets (CLI / env). Map to owner or static_scopes.
    static_secrets: RwLock<Vec<String>>,
    /// Scopes applied to static secrets.
    static_scopes: RwLock<SessionScopes>,
    /// Optional enterprise reduced ceiling.
    enterprise_ceiling: RwLock<Option<SessionScopes>>,
    /// Auth required? When false, unauthenticated sessions get owner scopes
    /// (stdio / explicit `--allow-unauthenticated` only).
    auth_required: bool,
}

impl SessionTokenStore {
    /// Create a store. `auth_required` should be true for remote listen.
    pub fn new(auth_required: bool) -> Self {
        Self {
            tokens: RwLock::new(HashMap::new()),
            static_secrets: RwLock::new(Vec::new()),
            static_scopes: RwLock::new(SessionScopes::owner()),
            enterprise_ceiling: RwLock::new(None),
            auth_required,
        }
    }

    /// Whether HTTP/SSE must present a bearer.
    pub fn auth_required(&self) -> bool {
        self.auth_required
    }

    /// Register a static shared secret (CLI `--token` / env).
    pub fn add_static_token(&self, secret: impl Into<String>) {
        if let Ok(mut v) = self.static_secrets.write() {
            let s = secret.into();
            if !s.is_empty() && !v.iter().any(|x| constant_time_eq(x.as_bytes(), s.as_bytes())) {
                v.push(s);
            }
        }
    }

    /// Override scopes used for static secrets (default: owner).
    pub fn set_static_scopes(&self, scopes: SessionScopes) {
        if let Ok(mut g) = self.static_scopes.write() {
            *g = scopes;
        }
    }

    /// Enable enterprise reduced ceiling (intersect on every validate).
    pub fn set_enterprise_ceiling(&self, ceiling: Option<SessionScopes>) {
        if let Ok(mut g) = self.enterprise_ceiling.write() {
            *g = ceiling;
        }
    }

    /// Force enterprise reduced scopes.
    pub fn enable_enterprise_reduced(&self) {
        self.set_enterprise_ceiling(Some(SessionScopes::enterprise_reduced()));
    }

    /// Issue a new opaque capability token. Returns the secret (show once).
    pub fn issue(
        &self,
        scopes: SessionScopes,
        client_label: ClientLabel,
        ttl: Option<Duration>,
    ) -> Option<String> {
        let secret = format!("wft_{}", uuid::Uuid::new_v4());
        let id = uuid::Uuid::new_v4().to_string();
        let now = Instant::now();
        let cap = SessionCapability {
            id,
            scopes: self.apply_ceiling(scopes),
            client_label,
            issued_at: now,
            expires_at: ttl.map(|d| now + d),
        };
        self.tokens.write().ok()?.insert(secret.clone(), cap);
        Some(secret)
    }

    /// Revoke an issued (or static) token. Static secrets are removed from
    /// the static list; issued tokens are dropped from the map.
    pub fn revoke(&self, secret: &str) -> bool {
        let mut removed = false;
        if let Ok(mut map) = self.tokens.write() {
            removed |= map.remove(secret).is_some();
        }
        if let Ok(mut statics) = self.static_secrets.write() {
            let before = statics.len();
            statics.retain(|s| !constant_time_eq(s.as_bytes(), secret.as_bytes()));
            removed |= statics.len() != before;
        }
        removed
    }

    /// Validate bearer secret → session capability.
    ///
    /// Returns `None` when auth is required and the token is missing/invalid.
    /// When auth is not required and token is absent, returns an implicit
    /// owner capability with `ClientLabel::Unknown`.
    pub fn validate(&self, bearer: Option<&str>) -> Option<SessionCapability> {
        match bearer {
            None | Some("") => {
                if self.auth_required {
                    None
                } else {
                    Some(SessionCapability {
                        id: "implicit-owner".into(),
                        scopes: self.apply_ceiling(SessionScopes::owner()),
                        client_label: ClientLabel::Unknown,
                        issued_at: Instant::now(),
                        expires_at: None,
                    })
                }
            }
            Some(tok) => {
                // Issued map first.
                if let Ok(map) = self.tokens.read()
                    && let Some(cap) = map.get(tok)
                {
                    if !cap.is_valid() {
                        return None;
                    }
                    let mut out = cap.clone();
                    out.scopes = self.apply_ceiling(out.scopes);
                    return Some(out);
                }
                // Static secrets.
                if let Ok(statics) = self.static_secrets.read() {
                    for s in statics.iter() {
                        if constant_time_eq(s.as_bytes(), tok.as_bytes()) {
                            let scopes = self
                                .static_scopes
                                .read()
                                .map(|g| g.clone())
                                .unwrap_or_default();
                            return Some(SessionCapability {
                                id: "static".into(),
                                scopes: self.apply_ceiling(scopes),
                                client_label: ClientLabel::Unknown,
                                issued_at: Instant::now(),
                                expires_at: None,
                            });
                        }
                    }
                }
                None
            }
        }
    }

    /// Validate and attach a client label override (header / clientInfo).
    pub fn validate_with_label(
        &self,
        bearer: Option<&str>,
        label_hint: Option<&str>,
    ) -> Option<SessionCapability> {
        let mut cap = self.validate(bearer)?;
        if let Some(hint) = label_hint {
            let parsed = ClientLabel::parse(hint);
            if parsed != ClientLabel::Unknown {
                cap.client_label = parsed;
            }
        }
        Some(cap)
    }

    fn apply_ceiling(&self, scopes: SessionScopes) -> SessionScopes {
        match self.enterprise_ceiling.read() {
            Ok(g) => match g.as_ref() {
                Some(c) => scopes.intersect(c),
                None => scopes,
            },
            Err(_) => scopes,
        }
    }
}

impl Default for SessionTokenStore {
    fn default() -> Self {
        Self::new(true)
    }
}

// ── Bind safety (WEFT-696) ─────────────────────────────────────────────

/// Classify a listen address for auth/bind policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindKind {
    /// 127.0.0.1 / ::1 only.
    Loopback,
    /// 0.0.0.0 / :: / non-loopback — public-facing.
    Public,
}

/// Classify `host` from `--listen host:port`.
///
/// Loopback means the name `localhost` or an IP address that is itself
/// loopback: all of `127.0.0.0/8`, `::1`, and an IPv4-mapped `::ffff:` form
/// of those. Brackets and an IPv6 zone-less literal are accepted. Anything
/// else, including a hostname that merely resolves to loopback, is public.
pub fn classify_bind_host(host: &str) -> BindKind {
    let h = host.trim().trim_start_matches('[').trim_end_matches(']');
    if h.eq_ignore_ascii_case("localhost") {
        return BindKind::Loopback;
    }
    let loopback = match h.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(v4)) => v4.is_loopback(),
        Ok(std::net::IpAddr::V6(v6)) => {
            v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
        Err(_) => false,
    };
    if loopback {
        BindKind::Loopback
    } else {
        BindKind::Public
    }
}

/// Validate listen + auth flags before binding (ADR-075 G4).
///
/// Rules:
/// 1. Auth is required by default (`auth_required`).
/// 2. Non-loopback bind requires `--dangerously-bind-public`.
/// 3. Unauthenticated public bind is refused even with the dangerous flag
///    (open public MCP is never OK).
/// 4. Unauthenticated loopback requires `--allow-unauthenticated`.
pub fn validate_listen_policy(
    host: &str,
    auth_required: bool,
    allow_unauthenticated: bool,
    dangerously_bind_public: bool,
) -> Result<BindKind, String> {
    let kind = classify_bind_host(host);

    if allow_unauthenticated && auth_required {
        // Caller should set auth_required = !allow_unauthenticated.
        return Err(
            "internal: allow_unauthenticated contradicts auth_required".into(),
        );
    }

    match kind {
        BindKind::Loopback => {
            if !auth_required && !allow_unauthenticated {
                return Err(
                    "unauthenticated listen requires --allow-unauthenticated \
                     (loopback only; never expose MCP without auth on a public interface)"
                        .into(),
                );
            }
            Ok(kind)
        }
        BindKind::Public => {
            if !dangerously_bind_public {
                return Err(format!(
                    "refusing to bind non-loopback address '{host}' without \
                     --dangerously-bind-public (MCP remote surface expands attack surface)"
                ));
            }
            if !auth_required {
                return Err(
                    "refusing open (unauthenticated) public MCP bind; \
                     provide --token / --token-env (or WEFT_MCP_TOKEN) and omit \
                     --allow-unauthenticated"
                        .into(),
                );
            }
            Ok(kind)
        }
    }
}

/// Extract bearer from `Authorization: Bearer <tok>` header value.
pub fn extract_bearer(authorization: Option<&str>) -> Option<&str> {
    let h = authorization?.trim();
    h.strip_prefix("Bearer ")
        .or_else(|| h.strip_prefix("bearer "))
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Constant-time equality for secrets.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_label_parse() {
        assert_eq!(ClientLabel::parse("grok"), ClientLabel::Grok);
        assert_eq!(ClientLabel::parse("Grok Build"), ClientLabel::Grok);
        assert_eq!(ClientLabel::parse("claude-code"), ClientLabel::Claude);
        assert_eq!(ClientLabel::parse("Claude"), ClientLabel::Claude);
        assert_eq!(ClientLabel::parse("cursor"), ClientLabel::Unknown);
        assert_eq!(ClientLabel::parse(""), ClientLabel::Unknown);
    }

    #[test]
    fn owner_allows_agent_spawn() {
        let s = SessionScopes::owner();
        assert!(s.allows_tool("agent_spawn"));
        assert!(s.allows_tool("read_file"));
    }

    #[test]
    fn enterprise_blocks_agent_spawn() {
        let s = SessionScopes::enterprise_reduced();
        assert!(!s.allows_tool("agent_spawn"));
        assert!(s.allows_tool("read_file"));
        assert!(s.allows_tool("memory_search"));
    }

    #[test]
    fn intersect_forces_no_spawn() {
        let claim = SessionScopes::owner();
        let ceiling = SessionScopes::enterprise_reduced();
        let out = claim.intersect(&ceiling);
        assert!(!out.agent_spawn);
        assert!(!out.allows_tool("agent_spawn"));
    }

    #[test]
    fn static_token_validate() {
        let store = SessionTokenStore::new(true);
        store.add_static_token("secret-abc");
        assert!(store.validate(None).is_none());
        assert!(store.validate(Some("wrong")).is_none());
        let cap = store.validate(Some("secret-abc")).unwrap();
        assert!(cap.scopes.agent_spawn);
        assert_eq!(cap.id, "static");
    }

    #[test]
    fn issue_and_validate_with_scopes() {
        let store = SessionTokenStore::new(true);
        let scopes = SessionScopes {
            tools: Some(vec!["read_file".into(), "skill_*".into()]),
            workspace_roots: None,
            agent_spawn: false,
        };
        let tok = store
            .issue(scopes, ClientLabel::Grok, Some(Duration::from_secs(60)))
            .unwrap();
        assert!(tok.starts_with("wft_"));
        let cap = store.validate(Some(&tok)).unwrap();
        assert_eq!(cap.client_label, ClientLabel::Grok);
        assert!(cap.scopes.allows_tool("read_file"));
        assert!(cap.scopes.allows_tool("skill_list"));
        assert!(!cap.scopes.allows_tool("agent_spawn"));
        assert!(!cap.scopes.allows_tool("exec_shell"));
    }

    #[test]
    fn enterprise_ceiling_on_static() {
        let store = SessionTokenStore::new(true);
        store.add_static_token("t");
        store.enable_enterprise_reduced();
        let cap = store.validate(Some("t")).unwrap();
        assert!(!cap.scopes.agent_spawn);
    }

    #[test]
    fn unauth_allowed_when_not_required() {
        let store = SessionTokenStore::new(false);
        let cap = store.validate(None).unwrap();
        assert_eq!(cap.id, "implicit-owner");
    }

    #[test]
    fn label_hint_overrides_unknown() {
        let store = SessionTokenStore::new(true);
        store.add_static_token("t");
        let cap = store
            .validate_with_label(Some("t"), Some("grok-build"))
            .unwrap();
        assert_eq!(cap.client_label, ClientLabel::Grok);
    }

    #[test]
    fn classify_uses_ip_semantics() {
        for h in [
            "127.0.0.1", "127.0.0.2", "127.255.255.254", "localhost", "LOCALHOST", "::1", "[::1]",
            "0:0:0:0:0:0:0:1", "::ffff:127.0.0.1", "[::ffff:127.0.0.1]",
        ] {
            assert_eq!(classify_bind_host(h), BindKind::Loopback, "{h}");
        }
        for h in [
            "0.0.0.0", "::", "[::]", "192.0.2.1", "::ffff:192.0.2.1", "::ffff:0.0.0.0",
            "128.0.0.1", "localhost.example", "127.0.0.1.example", "",
        ] {
            assert_eq!(classify_bind_host(h), BindKind::Public, "{h}");
        }
    }

    #[test]
    fn bind_policy_loopback_auth() {
        assert!(validate_listen_policy("127.0.0.1", true, false, false).is_ok());
        assert!(validate_listen_policy("localhost", true, false, false).is_ok());
        assert!(validate_listen_policy("127.0.0.1", false, true, false).is_ok());
        assert!(validate_listen_policy("127.0.0.1", false, false, false).is_err());
    }

    #[test]
    fn bind_policy_public_requires_dangerous_and_auth() {
        assert!(validate_listen_policy("0.0.0.0", true, false, false).is_err());
        assert!(validate_listen_policy("0.0.0.0", true, false, true).is_ok());
        assert!(validate_listen_policy("0.0.0.0", false, true, true).is_err());
        assert!(validate_listen_policy("192.168.1.5", true, false, true).is_ok());
    }

    #[test]
    fn extract_bearer_header() {
        assert_eq!(extract_bearer(Some("Bearer abc")), Some("abc"));
        assert_eq!(extract_bearer(Some("bearer xyz")), Some("xyz"));
        assert_eq!(extract_bearer(Some("Basic x")), None);
        assert_eq!(extract_bearer(None), None);
    }

    #[test]
    fn workspace_roots_enforced() {
        let s = SessionScopes {
            tools: None,
            workspace_roots: Some(vec![PathBuf::from("/work/a")]),
            agent_spawn: true,
        };
        assert!(s.allows_workspace_path(Path::new("/work/a/file.rs")));
        assert!(!s.allows_workspace_path(Path::new("/etc/passwd")));
    }

    #[test]
    fn revoke_static() {
        let store = SessionTokenStore::new(true);
        store.add_static_token("gone");
        assert!(store.revoke("gone"));
        assert!(store.validate(Some("gone")).is_none());
    }
}
