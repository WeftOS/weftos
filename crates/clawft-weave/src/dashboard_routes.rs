//! `report.routes` (ADR-116 R3, contract §2): what this machine's tailnet
//! router serves, in every heartbeat while the router is on (the key is left
//! out when it is off):
//!
//! ```json
//! "routes": {"base_url": "https://machine.example.ts.net", "served": true,
//!            "items": [{"prefix", "project", "project_ulid", "port", "default",
//!                       "healthy", "restricted", "source"}],
//!            "refused": [{"project", "prefix", "port", "reason", "source"}]}
//! ```
//!
//! `base_url` comes from `tailscale status --json` (`Self.DNSName`, trailing
//! dot stripped) and is absent when Tailscale is unavailable; `served` is true
//! when `tailscale serve status --json` shows `:443` proxying `/` to the
//! router. Both run as an argument vector with a timeout and are cached. The
//! allow list itself is never reported, only `restricted`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{Map, Value, json};

use crate::router_action::RouterRef;
use crate::router_index::{join_all, probe_health};
use crate::router_serve;

/// Most routes reported.
pub const MAX_ITEMS: usize = 32;
/// Most refusals reported.
pub const MAX_REFUSED: usize = 16;
/// How long the Tailscale facts are reused between beats.
pub const TAILSCALE_CACHE: Duration = Duration::from_secs(120);

/// The two `tailscale` documents the report needs (overridable in tests).
pub trait TailscaleFacts: Send + Sync {
    /// `tailscale status --json`.
    fn status_json(&self) -> Result<String, String>;
    /// `tailscale serve status --json`.
    fn serve_status_json(&self) -> Result<String, String>;
}

/// The real binary, run without a shell and bounded in time (see [`router_serve::run`]).
pub struct TailscaleCli {
    pub bin: String,
}

impl TailscaleFacts for TailscaleCli {
    fn status_json(&self) -> Result<String, String> {
        router_serve::run(&self.bin, &["status".into(), "--json".into()])
    }
    fn serve_status_json(&self) -> Result<String, String> {
        router_serve::run(&self.bin, &["serve".into(), "status".into(), "--json".into()])
    }
}

/// `https://<Self.DNSName>` without the trailing dot, from `tailscale status --json`.
pub fn base_url(status_json: &str) -> Option<String> {
    let v: Value = serde_json::from_str(status_json).ok()?;
    let name = v.get("Self")?.get("DNSName")?.as_str()?.trim().trim_end_matches('.');
    let ok = !name.is_empty() && name.len() <= 253 && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.');
    ok.then(|| format!("https://{name}"))
}

/// Does Serve's `:443` proxy `/` to the router on `router_port`?
pub fn served(serve_json: &str, router_port: u16) -> bool {
    router_serve::parse_status(serve_json)
        .map(|s| s.mounts.iter().any(|m| m.path == "/" && m.is_proxy && router_serve::loopback_port(&m.target) == Some(router_port)))
        .unwrap_or(false)
}

/// Where `report.routes` comes from.
#[async_trait]
pub trait RouteReportSource: Send + Sync {
    /// The document, or `None` to leave the key out.
    async fn routes(&self) -> Option<Value>;
}

#[derive(Debug, Clone, Default)]
struct Facts {
    base_url: Option<String>,
    served: bool,
}

/// The daemon's router plus Tailscale facts.
pub struct RouterRoutes {
    router: RouterRef,
    tailscale: Arc<dyn TailscaleFacts>,
    cache: Mutex<Option<(Instant, Facts)>>,
}

impl RouterRoutes {
    pub fn new(router: RouterRef, tailscale: Arc<dyn TailscaleFacts>) -> Self {
        Self { router, tailscale, cache: Mutex::new(None) }
    }

    /// The daemon's own router and the `tailscale` binary on `PATH`.
    pub fn daemon() -> Self {
        Self::new(RouterRef::Global, Arc::new(TailscaleCli { bin: "tailscale".into() }))
    }

    async fn facts(&self, router_port: u16) -> Facts {
        if let Some((at, f)) = self.cache.lock().expect("cache lock poisoned").clone()
            && at.elapsed() < TAILSCALE_CACHE
        {
            return f;
        }
        let ts = self.tailscale.clone();
        let f = tokio::task::spawn_blocking(move || Facts {
            base_url: ts.status_json().ok().and_then(|j| base_url(&j)),
            served: ts.serve_status_json().ok().is_some_and(|j| served(&j, router_port)),
        })
        .await
        .unwrap_or_default();
        *self.cache.lock().expect("cache lock poisoned") = Some((Instant::now(), f.clone()));
        f
    }
}

#[async_trait]
impl RouteReportSource for RouterRoutes {
    async fn routes(&self) -> Option<Value> {
        let h = self.router.get()?;
        let table = h.table();
        let timeout = h.health_timeout();
        let facts = self.facts(h.bound.port()).await;
        let routes: Vec<_> = table.routes.iter().take(MAX_ITEMS).cloned().collect();
        let health = join_all(routes.iter().cloned().map(|r| async move { probe_health(&r, timeout).await })).await;
        let items: Vec<Value> = routes
            .iter()
            .zip(health)
            .map(|(r, hv)| {
                let healthy = match hv["state"].as_str() {
                    Some("ok") => json!(true),
                    Some("down") => json!(false),
                    _ => Value::Null,
                };
                json!({
                    "prefix": r.prefix,
                    "project": r.project,
                    "project_ulid": table.project(&r.project).and_then(|p| p.ulid.clone()),
                    "port": r.port,
                    "default": r.default,
                    "healthy": healthy,
                    "restricted": r.restricted(),
                    "source": r.source,
                })
            })
            .collect();
        let refused: Vec<Value> = table
            .refused
            .iter()
            .take(MAX_REFUSED)
            .map(|x| json!({ "project": x.project, "prefix": x.prefix, "port": x.port, "reason": x.reason, "source": x.source }))
            .collect();
        let mut doc = Map::new();
        if let Some(b) = facts.base_url {
            doc.insert("base_url".into(), json!(b));
        }
        doc.insert("served".into(), json!(facts.served));
        doc.insert("items".into(), json!(items));
        doc.insert("refused".into(), json!(refused));
        Some(Value::Object(doc))
    }
}

#[cfg(test)]
#[path = "dashboard_routes_tests.rs"]
mod tests;
