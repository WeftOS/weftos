//! The daemon side of pairing (ADR-108 P2b): the reporter's
//! [`PairSource`] (this node's mesh identity and its pending requests) and
//! the `mesh.pair.request|list|cancel` RPCs behind `weaver mesh pair`.
//!
//! The identity is read on every beat from the placement signer
//! ([`crate::workload_place_rpc::local_signing_pubkey`]), so a reporter that
//! starts before placement is up simply omits it until then. `advertise` is
//! `workload-host.json`'s `advertise` when set; else its bind address, or
//! the kernel mesh listen address, with a wildcard host replaced by the
//! address this machine would use to reach the tailnet (never loopback;
//! absent when there is none).

use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use clawft_rpc::Response;
use serde_json::{Value, json};

use crate::mesh_pair::{MeshIdentity, PairSource};
use crate::mesh_pair_requests::{self, PairRequest};
use crate::rpc_ext::{ExtCall, ExtFuture};

pub const REQUEST: &str = "mesh.pair.request";
pub const LIST: &str = "mesh.pair.list";
pub const CANCEL: &str = "mesh.pair.cancel";

static MESH_LISTEN: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

/// Record the kernel mesh listen address for the `mesh.pair.request` reply
/// (first call wins; the daemon sets it once at boot).
pub fn set_mesh_listen(listen: Option<String>) {
    let _ = MESH_LISTEN.set(listen);
}

/// The running daemon's source.
pub struct DaemonPairSource {
    /// Kernel mesh listen address when the mesh is on; `None` means the
    /// mesh is off and no identity is reported.
    pub mesh_listen: Option<String>,
}

fn runtime_dir() -> Option<PathBuf> {
    crate::workload_place_rpc::runtime_dir()
}

/// The address this host would use to reach `probe` (no packet is sent).
fn source_ip_towards(probe: &str) -> Option<IpAddr> {
    let target: SocketAddr = probe.parse().ok()?;
    let bind = if target.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" };
    let s = UdpSocket::bind(bind).ok()?;
    s.connect(target).ok()?;
    let ip = s.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
}

fn is_wildcard(host: &str) -> bool {
    matches!(host, "" | "0.0.0.0" | "::" | "[::]")
}

/// `listen` with a wildcard host replaced by a reachable address of this
/// machine (the tailnet route first, then the default route).
pub fn advertise_from(listen: &str) -> Option<String> {
    let (host, port) = listen.rsplit_once(':')?;
    if !is_wildcard(host) {
        return (!host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback()) && host != "localhost").then(|| listen.to_owned());
    }
    // 100.100.100.100 is the tailnet's own resolver address; a host on a
    // tailnet routes it over the tailnet interface.
    let ip = source_ip_towards("100.100.100.100:53").or_else(|| source_ip_towards("8.8.8.8:53"))?;
    Some(match ip {
        IpAddr::V4(v4) => format!("{v4}:{port}"),
        IpAddr::V6(v6) => format!("[{v6}]:{port}"),
    })
}

fn advertise_for(dir: &Path, mesh_listen: Option<&str>) -> Option<String> {
    match crate::workload_host_serve::load_host_config(dir) {
        Ok(Some(cfg)) => match cfg.advertise {
            Some(a) => Some(a),
            None => advertise_from(&cfg.listen),
        },
        _ => mesh_listen.and_then(advertise_from),
    }
}

#[async_trait]
impl PairSource for DaemonPairSource {
    async fn mesh_identity(&self) -> Option<MeshIdentity> {
        let listen = self.mesh_listen.clone()?;
        let pk = crate::workload_place_rpc::local_signing_pubkey()?;
        let dir = runtime_dir()?;
        let advertise = tokio::task::spawn_blocking(move || advertise_for(&dir, Some(&listen))).await.ok().flatten();
        Some(MeshIdentity::from_pubkey(&pk, advertise))
    }

    async fn pair_requests(&self) -> Vec<PairRequest> {
        let Some(dir) = runtime_dir() else { return Vec::new() };
        tokio::task::spawn_blocking(move || match mesh_pair_requests::list(&dir) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "pending pair requests not reported");
                Vec::new()
            }
        })
        .await
        .unwrap_or_default()
    }
}

fn str_param<'a>(params: &'a Value, name: &str) -> Result<&'a str, String> {
    match params.get(name) {
        Some(Value::String(s)) if !s.is_empty() && s.len() <= 128 => Ok(s),
        _ => Err(format!("`{name}` must be a non-empty string")),
    }
}

fn projects_param(params: &Value) -> Result<Vec<String>, String> {
    match params.get("projects") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(a)) => a
            .iter()
            .map(|p| p.as_str().map(str::to_owned).ok_or_else(|| "`projects` must be an array of ULID strings".to_owned()))
            .collect(),
        Some(_) => Err("`projects` must be an array of ULID strings".into()),
    }
}

async fn handle_inner(method: &str, params: Value) -> Result<Value, String> {
    let dir = runtime_dir().ok_or("pairing needs the placement control plane (not initialised on this node)")?;
    match method {
        REQUEST => {
            let with = str_param(&params, "with_node")?.to_owned();
            let projects = projects_param(&params)?;
            let dir2 = dir.clone();
            let r = tokio::task::spawn_blocking(move || mesh_pair_requests::record(&dir, &with, &projects))
                .await
                .map_err(|e| e.to_string())??;
            let listen = MESH_LISTEN.get().cloned().flatten();
            let advertise = tokio::task::spawn_blocking(move || advertise_for(&dir2, listen.as_deref())).await.map_err(|e| e.to_string())?;
            Ok(json!({ "request": r, "advertise": advertise, "note": "reported to the dashboard on the next heartbeat; a member approves it there" }))
        }
        LIST => {
            let r = tokio::task::spawn_blocking(move || mesh_pair_requests::list(&dir)).await.map_err(|e| e.to_string())??;
            Ok(json!({ "requests": r }))
        }
        CANCEL => {
            let id = str_param(&params, "request_id")?.to_owned();
            let done = tokio::task::spawn_blocking(move || mesh_pair_requests::cancel(&dir, &id)).await.map_err(|e| e.to_string())??;
            Ok(json!({ "cancelled": done }))
        }
        other => Err(format!("{other} is not a mesh.pair method")),
    }
}

/// Route handler for `mesh.pair.request`, `mesh.pair.list`, `mesh.pair.cancel`.
pub fn handle(call: ExtCall) -> ExtFuture {
    Box::pin(async move {
        match handle_inner(&call.method, call.params).await {
            Ok(v) => Response::success(v),
            Err(e) => Response::error(e),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_explicit_host_is_kept_and_loopback_or_bad_input_is_dropped() {
        assert_eq!(advertise_from("pi5.local:9471").as_deref(), Some("pi5.local:9471"));
        assert_eq!(advertise_from("100.64.0.1:9471").as_deref(), Some("100.64.0.1:9471"));
        assert_eq!(advertise_from("127.0.0.1:9471"), None);
        assert_eq!(advertise_from("localhost:9471"), None);
        assert_eq!(advertise_from("nonsense"), None);
        if let Some(a) = advertise_from("0.0.0.0:9470") {
            assert!(a.ends_with(":9470"), "{a}");
            assert!(!a.starts_with("127.") && !a.starts_with("0.0.0.0"), "{a}");
        }
    }

    #[tokio::test]
    async fn params_are_validated_before_anything_is_read() {
        assert!(projects_param(&json!({ "projects": "x" })).is_err());
        assert!(projects_param(&json!({ "projects": [1] })).is_err());
        assert_eq!(projects_param(&json!({})).unwrap(), Vec::<String>::new());
        assert!(str_param(&json!({ "with_node": "" }), "with_node").is_err());
        assert!(str_param(&json!({}), "request_id").is_err());
    }
}
