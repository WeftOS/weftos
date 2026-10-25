//! `<runtime>/cog-ingest.json`: the ingest bridge's config and its boundary
//! validation (the shape is documented on [`super`]).

use std::net::{IpAddr, SocketAddr};
use std::path::Path;

use clawft_kernel::cog_ingest::valid_project_id;
use clawft_kernel::workload_pkg::codec::hex_decode_exact;
use serde::Deserialize;

/// Config file under the runtime dir.
pub const INGEST_FILE: &str = "cog-ingest.json";
const MAX_FILE: u64 = 64 * 1024;
const MAX_ROUTES: usize = 256;

/// Whole file.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IngestConfig {
    /// Bridge listener and budgets.
    #[serde(default)]
    pub bridge: BridgeSection,
    /// Where each placement's vectors go. Empty: the default route.
    #[serde(default)]
    pub routes: Vec<Route>,
    /// Serve the `cog-store` service to other nodes' bridges.
    #[serde(default)]
    pub store_owner: Option<OwnerSection>,
}

/// Bridge settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BridgeSection {
    /// Shared listener (loopback only), or `"off"`.
    #[serde(default = "default_bind")]
    pub bind: String,
    /// Loopback listener used when `bind` cannot be bound (for example an
    /// unprivileged daemon and port 80). Absent: the bridge is disabled then.
    #[serde(default)]
    pub fallback_bind: Option<String>,
    /// Requests per second per instance.
    #[serde(default = "default_requests")]
    pub requests_per_sec: u32,
    /// Vectors per second per instance.
    #[serde(default = "default_vectors")]
    pub vectors_per_sec: u32,
    /// Address to bind token-scoped listeners on for container relays (the
    /// engine or VM gateway). Absent: containers get no scoped listener.
    #[serde(default)]
    pub container_bind: Option<String>,
}

fn default_bind() -> String {
    "127.0.0.1:80".into()
}
fn default_requests() -> u32 {
    20
}
fn default_vectors() -> u32 {
    2048
}

impl Default for BridgeSection {
    fn default() -> Self {
        Self {
            bind: default_bind(),
            fallback_bind: None,
            requests_per_sec: default_requests(),
            vectors_per_sec: default_vectors(),
            container_bind: None,
        }
    }
}

/// One routing rule: `project` (that project's batches) or `controller`
/// (project-less batches placed by that node), never both.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    /// Project id.
    #[serde(default)]
    pub project: Option<String>,
    /// Placing controller node id.
    #[serde(default)]
    pub controller: Option<String>,
    /// Who owns the store.
    pub owner: Owner,
    /// Node ids besides this node allowed to place cogs for the project
    /// (project routes only; default: this node alone).
    #[serde(default)]
    pub controllers: Vec<String>,
}

/// A store owner: this node, or another.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Owner {
    /// `"local"`.
    Local(LocalMarker),
    /// A remote node's `cog-store`.
    Remote(RemoteOwner),
}

/// The literal string `local`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LocalMarker {
    /// This node.
    Local,
}

/// A remote owner, pinned by key.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteOwner {
    /// Its node id.
    pub node: String,
    /// Its Ed25519 key (64 hex); its answers must verify against it.
    pub key: String,
    /// `host:port` of its `cog-store` listener.
    pub addr: String,
    /// Noise XX over TCP (default on).
    #[serde(default = "yes")]
    pub noise: bool,
}

fn yes() -> bool {
    true
}

/// Serve the `cog-store` service.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnerSection {
    /// Bind address; never the mesh port or the workload-host port.
    pub listen: String,
    /// Noise XX (default on).
    #[serde(default = "yes")]
    pub noise: bool,
    /// Bridge node keys allowed to forward here.
    pub forwarders: Vec<ForwarderKey>,
    /// Projects whose stores this node owns.
    #[serde(default)]
    pub projects: Vec<String>,
    /// Also own the project-less (controller) store.
    #[serde(default)]
    pub fallback: bool,
}

/// One forwarder key and the projects it may forward for (all when absent).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForwarderKey {
    /// Ed25519 key, 64 hex.
    pub key: String,
    /// The projects it may forward for, or `"*"` for any (and for
    /// project-less batches). Required: there is no default.
    pub projects: ForwarderProjects,
}

/// A forwarder's scope.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ForwarderProjects {
    /// `"*"`.
    Any(AnyMarker),
    /// Listed project ids.
    List(Vec<String>),
}

/// The literal string `*`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub enum AnyMarker {
    /// Any project.
    #[serde(rename = "*")]
    Any,
}

impl IngestConfig {
    /// Boundary validation.
    pub fn validate(&self) -> Result<(), String> {
        let bad = |m: String| Err(format!("{INGEST_FILE}: {m}"));
        let b = &self.bridge;
        let off = b.bind == crate::cogbus_ingest::BIND_OFF;
        if off && b.fallback_bind.is_some() {
            return bad("bridge.fallback_bind needs bridge.bind on".into());
        }
        for (name, v) in [("bind", Some(&b.bind).filter(|_| !off)), ("fallback_bind", b.fallback_bind.as_ref())] {
            let Some(v) = v else { continue };
            let addr: SocketAddr = v
                .parse()
                .map_err(|e| format!("{INGEST_FILE}: bridge.{name} {v:?}: {e}"))?;
            if !addr.ip().is_loopback() {
                return bad(format!("bridge.{name} {addr} must be loopback"));
            }
        }
        if b.requests_per_sec == 0 || b.vectors_per_sec == 0 {
            return bad("bridge budgets must be above zero".into());
        }
        if let Some(c) = &b.container_bind {
            let ip = c
                .parse::<IpAddr>()
                .map_err(|e| format!("{INGEST_FILE}: bridge.container_bind {c:?}: {e}"))?;
            if ip.is_unspecified() || ip.is_multicast() {
                return bad(format!(
                    "bridge.container_bind {ip} must be one gateway address, not unspecified or multicast"
                ));
            }
        }
        if self.routes.len() > MAX_ROUTES {
            return bad(format!("at most {MAX_ROUTES} routes"));
        }
        for r in &self.routes {
            if r.project.is_none() && !r.controllers.is_empty() {
                return bad("`controllers` belongs on a `project` route".into());
            }
            if r.controllers.iter().any(|c| c.is_empty() || c.len() > 128) {
                return bad("controllers must be node ids".into());
            }
            match (&r.project, &r.controller) {
                (Some(p), None) if valid_project_id(p) => {}
                (Some(p), None) => return bad(format!("route project {p:?} is not a project id")),
                (None, Some(c)) if !c.is_empty() && c.len() <= 128 => {}
                _ => return bad("a route names exactly one of `project` or `controller`".into()),
            }
            if let Owner::Remote(o) = &r.owner {
                if hex_decode_exact::<32>(&o.key).is_none() {
                    return bad(format!("owner key for {} is not 64 hex", o.node));
                }
                if o.node.is_empty() || o.addr.is_empty() || o.addr.len() > 253 {
                    return bad("owner needs node and addr".into());
                }
            }
        }
        if let Some(s) = &self.store_owner {
            s.listen
                .parse::<SocketAddr>()
                .map_err(|e| format!("{INGEST_FILE}: store_owner.listen {:?}: {e}", s.listen))?;
            if s.forwarders.is_empty() {
                return bad("store_owner.forwarders must list at least one key".into());
            }
            for f in &s.forwarders {
                if hex_decode_exact::<32>(&f.key).is_none() {
                    return bad(format!("forwarder key {:?} is not 64 hex", f.key));
                }
                if let ForwarderProjects::List(ps) = &f.projects
                    && ps.iter().any(|p| !valid_project_id(p))
                {
                    return bad("forwarder projects must be project ids".into());
                }
            }
            if s.projects.iter().any(|p| !valid_project_id(p)) {
                return bad("store_owner.projects must be project ids".into());
            }
        }
        Ok(())
    }
}

/// The config, or the defaults when the file is absent.
pub fn load_config(dir: &Path) -> Result<IngestConfig, String> {
    let path = dir.join(INGEST_FILE);
    match std::fs::metadata(&path) {
        Err(_) => Ok(IngestConfig::default()),
        Ok(m) if m.len() > MAX_FILE => Err(format!("{INGEST_FILE} is too large")),
        Ok(_) => {
            let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
            let cfg: IngestConfig =
                serde_json::from_str(&text).map_err(|e| format!("{INGEST_FILE}: {e}"))?;
            cfg.validate()?;
            Ok(cfg)
        }
    }
}
