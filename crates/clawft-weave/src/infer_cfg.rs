//! `<runtime>/inference.json`: what the daemon places (card
//! mesh-placement-19 and -20; ADR-101 sections 5 to 8). Everything here is
//! read once at boot, bounded, validated at the boundary, and off or deny
//! when absent:
//!
//! ```json
//! { "roles": [
//!     { "role": "hermes", "mode": "managed", "flavor": "llamacpp",
//!       "model": "Hermes-4.3-36B", "memory_gb": 22, "instance_port": 18090,
//!       "proxy_port": 8090, "provider": "local", "autostart": false },
//!     { "role": "coder-daily", "mode": "managed", "roster_id": "coder-daily",
//!       "instance_port": 18081, "proxy_port": 8081,
//!       "expose": { "listen": "0.0.0.0", "port": 18082,
//!                   "token_file": "secrets/infer/coder-daily.token" } },
//!     { "role": "orpheus-tts", "mode": "managed", "flavor": "ollama",
//!       "model": "orpheus-tts", "memory_gb": 4, "instance_port": 11434 } ],
//!   "roster": { "file": "/path/to/queue.yaml",
//!               "overlay": { "excludes": { "planner": ["role:coder-daily"] } } },
//!   "serve_programs": { "llamacpp": "/path/to/serve-llamacpp", "mlx-lm": "/path/to/serve" },
//!   "budget_gb": 96,
//!   "mesh": { "expose": [], "serve_peers": {}, "remote_nodes": {} } }
//! ```
//!
//! - A role is `adopted` (default: a server somebody else runs is observed,
//!   never started or stopped) or `managed` (the adapter starts and stops
//!   it through the launcher named in `serve_programs`, governed by the
//!   daemon's workload gate; managed roles never autostart unless
//!   `autostart` says so).
//! - `roster` points at the model lab's `queue.yaml`, which is only read.
//!   `roster_id` takes memory, port, model name and flavor from that entry;
//!   explicit role fields win.
//! - `budget_gb` is the unified-memory budget and `excludes` (roster
//!   overlay or per role) the co-residency rule, enforced across every
//!   managed role.
//! - `expose` adds a second proxy listener beyond loopback (`listen:port`),
//!   with a bearer token and a chained governance permit, beside the
//!   token-free loopback listener on `proxy_port` that local consumers keep
//!   using; the model server itself always stays on loopback. The token
//!   travels in cleartext on the LAN (no TLS yet), so `network: lan` is for
//!   trusted segments.

use std::collections::BTreeMap;
use std::path::Path;

use clawft_kernel::workload_pkg::manifest::valid_token;
use clawft_kernel::workload_runtime::infer::{
    GB, ImportedRoster, InferFlavor, InferenceSpec, LatencyClass, RosterOverlay,
};
use serde::Deserialize;

pub use crate::infer_svc::SvcCfg;

/// Config file under the runtime dir.
pub const CONFIG_FILE: &str = "inference.json";
const MAX_FILE: u64 = 64 * 1024;
const MAX_ROLES: usize = 16;
/// Default seconds between local re-checks.
pub const DEFAULT_SYNC: u64 = 5;
/// Default seconds between mesh announcements.
pub const DEFAULT_ADVERT: u64 = 20;
const MAX_LIST: usize = 64;

/// One served role.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleCfg {
    /// Stable role name.
    pub role: String,
    /// `adopted` (default) or `managed`.
    #[serde(default)]
    pub mode: Option<String>,
    /// `llamacpp`, `mlx-lm` or `ollama` (from the roster entry if absent).
    #[serde(default)]
    pub flavor: Option<String>,
    /// Entry of the roster this role takes its facts from.
    #[serde(default)]
    pub roster_id: Option<String>,
    /// Model registry name (managed); the roster's name if absent.
    #[serde(default)]
    pub model: Option<String>,
    /// Memory the role holds while resident, in GB.
    #[serde(default)]
    pub memory_gb: Option<f64>,
    /// Roles this one cannot be resident beside.
    #[serde(default)]
    pub excludes: Vec<String>,
    /// `interactive` or `batch`.
    #[serde(default)]
    pub latency_class: Option<LatencyClass>,
    /// Warm-KV stickiness (default true).
    #[serde(default)]
    pub sticky: Option<bool>,
    /// Context length (managed).
    #[serde(default)]
    pub ctx: Option<u32>,
    /// Port of the server (always on `127.0.0.1`); the roster's if absent.
    #[serde(default)]
    pub instance_port: Option<u16>,
    /// Stable port the proxy keeps for consumers.
    #[serde(default)]
    pub proxy_port: Option<u16>,
    /// `refuse` (default) or `adopt` when `proxy_port` is already held.
    #[serde(default)]
    pub on_occupied: Option<String>,
    /// Provider name that follows this role (`local`).
    #[serde(default)]
    pub provider: Option<String>,
    /// Start a managed role at boot (default false).
    #[serde(default)]
    pub autostart: bool,
    /// Let the proxy listen beyond loopback.
    #[serde(default)]
    pub expose: Option<ExposeCfg>,
    /// Serve the role from a cogbus service instead of an HTTP server
    /// (ADR-101 update 2026-10-28). Excludes `flavor`, `roster_id`, `model`,
    /// `instance_port`, `mode: managed` and the other server fields.
    #[serde(default)]
    pub svc: Option<SvcCfg>,
}

/// A proxy that listens beyond loopback.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExposeCfg {
    /// Address to listen on (`0.0.0.0`).
    pub listen: String,
    /// Port of the exposed listener. The role's `proxy_port` stays a
    /// loopback listener without a token for consumers on this machine.
    pub port: u16,
    /// Bearer token file under the runtime dir (mode 0600).
    pub token_file: String,
}

/// The model lab's roster.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RosterCfg {
    /// Path of `queue.yaml` (read only).
    pub file: String,
    /// What the roster does not carry.
    #[serde(default)]
    pub overlay: RosterOverlay,
}

/// Mesh settings; empty means nothing exposed, nothing allowed.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeshCfg {
    /// Roles this node exposes to the mesh.
    #[serde(default)]
    pub expose: Vec<String>,
    /// Per role, the peers this node serves.
    #[serde(default)]
    pub serve_peers: BTreeMap<String, Vec<String>>,
    /// Per role, the nodes this node may use.
    #[serde(default)]
    pub remote_nodes: BTreeMap<String, Vec<String>>,
}

/// The file.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileCfg {
    /// Roles.
    pub roles: Vec<RoleCfg>,
    /// Mesh settings.
    #[serde(default)]
    pub mesh: MeshCfg,
    /// The model lab's roster.
    #[serde(default)]
    pub roster: Option<RosterCfg>,
    /// Launcher per flavor (`llamacpp`, `mlx-lm`): absolute paths.
    #[serde(default)]
    pub serve_programs: BTreeMap<String, String>,
    /// Unified-memory budget in GB.
    #[serde(default)]
    pub budget_gb: Option<f64>,
    /// Seconds between local re-checks (default 5).
    #[serde(default)]
    pub sync_secs: Option<u64>,
    /// Seconds between mesh announcements (default 20).
    #[serde(default)]
    pub advert_secs: Option<u64>,
}

/// `llamacpp`, `mlx-lm`, `ollama`.
pub fn flavor_of(s: &str) -> Option<InferFlavor> {
    match s {
        "llamacpp" => Some(InferFlavor::LlamaCpp),
        "mlx-lm" => Some(InferFlavor::MlxLm),
        "ollama" => Some(InferFlavor::Ollama),
        _ => None,
    }
}

/// Config name of a flavor (the key of `serve_programs`).
pub fn flavor_key(f: InferFlavor) -> &'static str {
    match f {
        InferFlavor::LlamaCpp => "llamacpp",
        InferFlavor::MlxLm => "mlx-lm",
        InferFlavor::Ollama => "ollama",
    }
}

impl FileCfg {
    /// Boundary validation (no file access).
    pub fn validate(&self) -> Result<(), String> {
        let bad = |m: String| Err(format!("{CONFIG_FILE}: {m}"));
        if self.roles.is_empty() || self.roles.len() > MAX_ROLES {
            return bad(format!("roles must list 1..={MAX_ROLES} entries"));
        }
        let mut seen = std::collections::HashSet::new();
        for r in &self.roles {
            if !valid_token(&r.role, 64) || r.role.contains('/') || !seen.insert(r.role.clone()) {
                return bad(format!("role {:?} is invalid or repeated", r.role));
            }
            if let Some(s) = &r.svc {
                let server_fields = r.flavor.is_some()
                    || r.roster_id.is_some()
                    || r.model.is_some()
                    || r.instance_port.is_some()
                    || r.mode.as_deref() == Some("managed")
                    || r.memory_gb.is_some()
                    || r.ctx.is_some()
                    || r.autostart;
                if server_fields {
                    return bad(format!("role {}: a svc role has no flavor, roster_id, model, instance_port, ctx, memory_gb or managed mode (the service owns the model)", r.role));
                }
                s.validate(&r.role).map_err(|m| format!("{CONFIG_FILE}: {m}"))?;
            }
            let is_svc = r.svc.is_some();
            if r.flavor.as_deref().is_some_and(|f| flavor_of(f).is_none()) {
                return bad(format!("role {}: flavor is llamacpp, mlx-lm or ollama", r.role));
            }
            if !matches!(r.mode.as_deref(), None | Some("adopted") | Some("managed")) {
                return bad(format!("role {}: mode is adopted or managed", r.role));
            }
            if !is_svc && r.flavor.is_none() && r.roster_id.is_none() {
                return bad(format!("role {}: name a flavor or a roster_id", r.role));
            }
            if r.roster_id.is_some() && self.roster.is_none() {
                return bad(format!("role {}: roster_id needs a roster", r.role));
            }
            if r.instance_port == Some(0) || r.proxy_port == Some(0) || (r.proxy_port.is_some() && r.proxy_port == r.instance_port) {
                return bad(format!("role {}: ports must be nonzero and different", r.role));
            }
            if !matches!(r.on_occupied.as_deref(), None | Some("refuse") | Some("adopt")) {
                return bad(format!("role {}: on_occupied is refuse or adopt", r.role));
            }
            if r.provider.as_deref().is_some_and(|p| !valid_token(p, 32)) {
                return bad(format!("role {}: bad provider", r.role));
            }
            if r.memory_gb.is_some_and(|g| !g.is_finite() || !(0.0..=4096.0).contains(&g)) {
                return bad(format!("role {}: memory_gb must be 0..=4096", r.role));
            }
            if r.excludes.len() > MAX_LIST || r.excludes.iter().any(|x| !valid_token(x, 64)) {
                return bad(format!("role {}: bad excludes", r.role));
            }
            if let Some(e) = &r.expose {
                if r.proxy_port.is_none() {
                    return bad(format!("role {}: expose needs a proxy_port", r.role));
                }
                if e.port == 0 || Some(e.port) == r.proxy_port || Some(e.port) == r.instance_port {
                    return bad(format!("role {}: expose.port must be nonzero and differ from proxy_port and instance_port", r.role));
                }
                if e.listen.parse::<std::net::IpAddr>().is_err() {
                    return bad(format!("role {}: expose.listen is not an IP address", r.role));
                }
                let p = Path::new(&e.token_file);
                if e.token_file.is_empty()
                    || p.is_absolute()
                    || p.components().any(|c| !matches!(c, std::path::Component::Normal(_)))
                {
                    return bad(format!("role {}: expose.token_file is relative to the runtime dir, no '..'", r.role));
                }
            }
        }
        // Listener ports (a role's proxy and exposed ports) are unique across
        // all roles and never a server's port. Servers may share a port only
        // when both are Ollama roles (one Ollama serves several models).
        let mut listeners = std::collections::HashSet::new();
        for r in &self.roles {
            for p in r.proxy_port.into_iter().chain(r.expose.as_ref().map(|e| e.port)) {
                if !listeners.insert(p) {
                    return bad(format!("port {p} is used by more than one proxy or exposed listener"));
                }
            }
        }
        let mut servers: BTreeMap<u16, bool> = BTreeMap::new();
        for r in &self.roles {
            if let Some(p) = r.instance_port {
                if listeners.contains(&p) {
                    return bad(format!("port {p} is both a server port and a proxy or exposed listener port"));
                }
                let ollama = r.flavor.as_deref() == Some("ollama");
                if let Some(prev_ollama) = servers.insert(p, ollama)
                    && !(prev_ollama && ollama)
                {
                    return bad(format!("instance_port {p} is used by more than one role (only Ollama roles share a server)"));
                }
            }
        }
        if self.budget_gb.is_some_and(|g| !g.is_finite() || g <= 0.0 || g > 4096.0) {
            return bad("budget_gb must be 0..=4096".into());
        }
        for (k, v) in &self.serve_programs {
            if flavor_of(k).is_none() || k == "ollama" || !Path::new(v).is_absolute() {
                return bad(format!("serve_programs: {k:?} must be llamacpp or mlx-lm with an absolute path"));
            }
        }
        // An advert must be repeated well inside the receivers' TTL.
        let max_advert = clawft_kernel::infer_proxy::DEFAULT_ADVERT_TTL.as_secs() / 2;
        if self.advert_secs.is_some_and(|a| a == 0 || a > max_advert) {
            return bad(format!("advert_secs must be 1..={max_advert} (half the advert TTL)"));
        }
        if self.sync_secs.is_some_and(|s| s == 0 || s > 60) {
            return bad("sync_secs must be 1..=60".into());
        }
        // The announcement runs on the sync tick, so its real period is
        // the larger of the two.
        let (sync, advert) = (self.sync_secs.unwrap_or(DEFAULT_SYNC), self.advert_secs.unwrap_or(DEFAULT_ADVERT));
        if sync > advert {
            return bad(format!("sync_secs ({sync}) must not exceed advert_secs ({advert})"));
        }
        // The announcement fires on the first tick at or after advert_secs,
        // so its real period is ceil(advert / sync) * sync, and that must
        // still sit inside the half-TTL bound.
        let real = advert.div_ceil(sync) * sync;
        if real > max_advert {
            return bad(format!(
                "advert_secs ({advert}) on a {sync} s sync tick announces every {real} s; keep it at most {max_advert} s"
            ));
        }
        let known = |role: &String| self.roles.iter().any(|r| &r.role == role);
        let nodes_ok = |v: &Vec<String>| v.len() <= MAX_LIST && v.iter().all(|n| valid_token(n, 128));
        if self.mesh.expose.iter().any(|r| !known(r))
            || self.mesh.serve_peers.keys().chain(self.mesh.remote_nodes.keys()).any(|r| !known(r))
        {
            return bad("mesh settings name a role that is not in roles".into());
        }
        if !self.mesh.serve_peers.values().all(nodes_ok) || !self.mesh.remote_nodes.values().all(nodes_ok) {
            return bad("bad node id in the mesh allowlists".into());
        }
        Ok(())
    }
}

/// Read and validate the config; `None` when the operator did not enable it.
pub fn load_config(dir: &Path) -> Result<Option<FileCfg>, String> {
    let path = dir.join(CONFIG_FILE);
    match std::fs::metadata(&path) {
        Err(_) => Ok(None),
        Ok(m) if m.len() > MAX_FILE => Err(format!("{CONFIG_FILE} is too large")),
        Ok(_) => {
            let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
            let cfg: FileCfg = serde_json::from_str(&text).map_err(|e| format!("{CONFIG_FILE}: {e}"))?;
            cfg.validate()?;
            Ok(Some(cfg))
        }
    }
}

/// A role with everything resolved: roster facts merged under the role's
/// own fields, and the spec the adapter will see.
#[derive(Debug, Clone)]
pub struct Resolved {
    /// The role's config.
    pub cfg: RoleCfg,
    /// Managed (else adopted).
    pub managed: bool,
    /// The inference spec.
    pub spec: InferenceSpec,
}

/// Merge roster facts (when `imported` is given) under each role's fields
/// and build its spec. Explicit role fields win over the roster.
pub fn resolve_roles(cfg: &FileCfg, imported: Option<&ImportedRoster>) -> Result<Vec<Resolved>, String> {
    let mut out: Vec<Resolved> = Vec::new();
    for r in &cfg.roles {
        let fail = |m: String| Err(format!("{CONFIG_FILE}: role {}: {m}", r.role));
        if let Some(s) = &r.svc {
            // The service owns the model and the server: nothing to place, so
            // the spec is only a label for status.
            let mut spec = InferenceSpec::new(&r.role, InferFlavor::LlamaCpp);
            spec.model = Some(s.model.clone());
            out.push(Resolved { cfg: r.clone(), managed: false, spec });
            continue;
        }
        let from_roster = match (&r.roster_id, imported) {
            (Some(id), Some(roster)) => match roster.get(id) {
                Some(s) => Some(s.spec.clone()),
                None => {
                    let why = roster.skipped.iter().find(|(i, _)| i == id).map(|(_, w)| w.as_str()).unwrap_or("not in the roster");
                    return fail(format!("roster_id {id}: {why}"));
                }
            },
            _ => None,
        };
        let flavor = match (&r.flavor, &from_roster) {
            (Some(f), _) => flavor_of(f).ok_or("flavor")?,
            (None, Some(s)) => s.runtime,
            (None, None) => return fail("no flavor".into()),
        };
        let mut spec = from_roster.unwrap_or_else(|| InferenceSpec::new(&r.role, flavor));
        spec.role = r.role.clone();
        spec.runtime = flavor;
        if let Some(p) = r.instance_port {
            spec.serve.port = Some(p);
        }
        if spec.serve.port.is_none() {
            return fail("no instance_port (and the roster has none)".into());
        }
        if r.proxy_port.is_some() && r.proxy_port == spec.serve.port {
            return fail("proxy_port equals the server's port: give the server its own instance_port".into());
        }
        if let Some(m) = &r.model {
            spec.model = Some(m.clone());
        }
        if let Some(g) = r.memory_gb {
            spec.memory.weights_bytes = (g * GB as f64) as u64;
        }
        if !r.excludes.is_empty() {
            spec.excludes = r.excludes.clone();
        }
        if let Some(l) = r.latency_class {
            spec.latency_class = l;
        }
        if let Some(s) = r.sticky {
            spec.sticky = s;
        }
        if let Some(c) = r.ctx {
            spec.serve.ctx = Some(c);
        }
        let managed = r.mode.as_deref() == Some("managed");
        if managed && spec.model.is_none() {
            return fail("a managed role needs a model (registry name)".into());
        }
        if managed && cfg.budget_gb.is_some() && spec.memory.weights_bytes == 0 {
            return fail("a managed role needs its memory (memory_gb or the roster's ram_gb) when budget_gb is set".into());
        }
        spec.validate().map_err(|e| format!("{CONFIG_FILE}: role {}: {e}", r.role))?;
        // Ports are known now (a roster may have supplied them): no two
        // roles share a server port unless both are Ollama, and a server
        // never sits on a listener's port.
        let port = spec.serve.port;
        let listener = |p: Option<u16>| p.is_some() && p == port;
        for other in &out {
            if other.spec.serve.port == port
                && !(other.spec.runtime == InferFlavor::Ollama && spec.runtime == InferFlavor::Ollama)
            {
                return fail(format!("port {:?} is also role {}'s server port (only Ollama roles share a server)", port, other.cfg.role));
            }
            if listener(other.cfg.proxy_port) || listener(other.cfg.expose.as_ref().map(|e| e.port)) {
                return fail(format!("server port {port:?} is role {}'s listener port", other.cfg.role));
            }
        }
        if cfg.roles.iter().any(|o| o.role != r.role && (listener(o.proxy_port) || listener(o.expose.as_ref().map(|e| e.port)))) {
            return fail(format!("server port {port:?} is another role's listener port"));
        }
        out.push(Resolved { cfg: r.clone(), managed, spec });
    }
    Ok(out)
}
