//! This daemon's `workload-host` (ADR-099 section 7, card
//! mesh-placement-12): the target side of placement, served to other
//! nodes' control planes over mesh TCP (Noise XX by default) and
//! advertised through its `ServiceAdvertisement` (returned by every signed
//! `describe`, merged into the controller's service registry).
//!
//! Serving is off unless the operator writes `<runtime>/workload-host.json`:
//!
//! ```json
//! { "listen": "0.0.0.0:9471", "noise": true,
//!   "controllers": ["<64-hex Ed25519 public key>", "..."],
//!   "advertise": "pi5.local:9471" }
//! ```
//!
//! `controllers` are the node keys whose signed requests are served (this
//! node's own key always is). What may run is still decided here, by this
//! node's `workload-permits.json` (default deny) and `workload-trust.json`
//! (package signers), and by the adapter's admission self-check.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use clawft_kernel::chain::ChainManager;
use clawft_kernel::mesh_artifact::ArtifactExchange;
use clawft_kernel::workload_ctl::{FactsSource, WorkloadHostService, listen_tcp, serve_listener};
use clawft_kernel::workload_governance::{NodeTrustTier, WorkloadGate};
use clawft_kernel::workload_pkg::TrustAnchors;
use clawft_kernel::workload_pkg::codec::hex_decode_exact;
use clawft_kernel::workload_runtime::{
    ContainerRuntimeConfig, NativeConfig, NativeRuntime, WorkloadHost,
};

use crate::workload_place_policy::container_host;
use ed25519_dalek::SigningKey;
use serde::Deserialize;

/// Serving config under the runtime dir.
pub const HOST_FILE: &str = "workload-host.json";
const MAX_HOST_FILE: u64 = 64 * 1024;
const MAX_CONTROLLERS: usize = 64;

/// Operator config for serving `workload-host` to other nodes.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostConfig {
    /// Bind address (`0.0.0.0:9471`); never the kernel mesh port.
    pub listen: String,
    /// Noise XX over TCP (default on).
    #[serde(default = "yes")]
    pub noise: bool,
    /// Controller node keys (64 hex each) whose requests are served.
    pub controllers: Vec<String>,
    /// Address advertised in the `workload-host` advertisement.
    #[serde(default)]
    pub advertise: Option<String>,
    /// Opt-in lease, in seconds: continuous workloads on this node stop
    /// once no remote controller has been heard from for this long, so a
    /// partitioned node does not keep running a copy the controller is
    /// about to replace. Set it at or below the controller's `dead_after`
    /// (default 30 s). At least 30 (three controller ticks of 10 s): a
    /// shorter lease would stop work for ordinary jitter. Absent: off.
    #[serde(default)]
    pub lease_secs: Option<u64>,
}

fn yes() -> bool {
    true
}

impl HostConfig {
    /// Boundary validation; returns the controller keys.
    pub fn validate(&self) -> Result<Vec<[u8; 32]>, String> {
        self.listen
            .parse::<SocketAddr>()
            .map_err(|e| format!("{HOST_FILE}: listen {:?}: {e}", self.listen))?;
        if self.controllers.is_empty() || self.controllers.len() > MAX_CONTROLLERS {
            return Err(format!(
                "{HOST_FILE}: controllers must list 1..={MAX_CONTROLLERS} keys"
            ));
        }
        if let Some(l) = self.lease_secs
            && !(30..=86_400).contains(&l)
        {
            return Err(format!("{HOST_FILE}: lease_secs must be 30..=86400 (at least three controller ticks)"));
        }
        if let Some(a) = &self.advertise
            && (a.is_empty()
                || a.len() > 253
                || !a
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || ".-:[]_".contains(c)))
        {
            return Err(format!("{HOST_FILE}: advertise {a:?} is not host:port"));
        }
        self.controllers
            .iter()
            .map(|h| {
                hex_decode_exact::<32>(h)
                    .ok_or_else(|| format!("{HOST_FILE}: controller {h:?} is not 64 hex"))
            })
            .collect()
    }

    /// The advertised address (explicit, else the bind address).
    pub fn advertised(&self) -> String {
        self.advertise.clone().unwrap_or_else(|| self.listen.clone())
    }
}

/// The host's instance journal under the runtime dir: what was placed here
/// and whether it was running, so a daemon restart puts it back.
pub const JOURNAL_FILE: &str = "workload-host-journal.json";

/// The serving config, if the operator enabled serving.
pub fn load_host_config(dir: &Path) -> Result<Option<HostConfig>, String> {
    let path = dir.join(HOST_FILE);
    match std::fs::metadata(&path) {
        Err(_) => Ok(None),
        Ok(m) if m.len() > MAX_HOST_FILE => Err(format!("{HOST_FILE} is too large")),
        Ok(_) => {
            let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
            let cfg: HostConfig =
                serde_json::from_str(&text).map_err(|e| format!("{HOST_FILE}: {e}"))?;
            cfg.validate()?;
            Ok(Some(cfg))
        }
    }
}

/// What the daemon's host is built from.
pub struct HostParts<'a> {
    /// Node key (signs responses; its own requests are always served).
    pub key: &'a SigningKey,
    /// Runtime dir (instances live under `workload-instances/`).
    pub dir: &'a Path,
    /// Kernel chain.
    pub chain: &'a Arc<ChainManager>,
    /// This node's governance.
    pub gate: Arc<WorkloadGate>,
    /// Piece exchange (payload fetch before load).
    pub exchange: Arc<ArtifactExchange>,
    /// Package signers this node trusts.
    pub anchors: TrustAnchors,
    /// Fresh signed facts for `describe`.
    pub facts: FactsSource,
    /// Serving config (adds remote controllers and the advertised address).
    pub serving: Option<&'a HostConfig>,
    /// Container adapter (`workload-container.json`); none serves native only.
    pub container: Option<ContainerRuntimeConfig>,
    /// Cog ingest bridge wiring (`cog-ingest.json`); none places cogs
    /// without ingest.
    pub ingest: Option<clawft_kernel::cog_ingest::IngestHooks>,
    /// The node's revocation list: a `place` that races a revocation is
    /// caught and a revoked start is rolled back by the revocation.
    pub revocations: Arc<clawft_kernel::revocation::RevocationList>,
}

/// This node's `workload-host`: the native adapter (and the container
/// adapter when the operator configured one) under this node's governance,
/// answering this node and the configured controllers. Its advertisement
/// lists exactly these routes, so controllers never offer it a runtime it
/// has no adapter for.
pub fn local_host(p: HostParts<'_>) -> Result<WorkloadHostService, String> {
    let pk = p.key.verifying_key().to_bytes();
    let id = clawft_kernel::node_id_from_pubkey(&pk);
    let native = NativeRuntime::new(NativeConfig {
        root: p.dir.join("workload-instances"),
        run_as: None,
        allow_interpreted: false,
    });
    let host = WorkloadHost::new(
        Arc::new(native),
        p.gate.clone(),
        id.clone(),
        NodeTrustTier::Pinned,
    )
    .with_chain(p.chain.clone());
    let container = p
        .container
        .map(|cfg| container_host(cfg, p.gate.clone(), &id, p.chain));
    let mut controllers = vec![pk];
    if let Some(cfg) = p.serving {
        controllers.extend(cfg.validate()?);
    }
    // ADR-108 P3b: a paired peer with a fetch grant may also `describe` this
    // node and call `project.fetch`; nothing else.
    let policy = crate::project_fetch_policy::FetchPeerPolicy::new(controllers, p.dir.to_path_buf());
    let mut svc = WorkloadHostService::new(p.key.clone(), p.exchange, p.anchors, p.gate)
        .with_route("native", Arc::new(host))
        .with_controllers(policy)
        .with_chain(p.chain.clone())
        .with_facts_source(p.facts)
        // Placed instances survive a daemon restart (restored in workload_place_rpc).
        .with_journal(&p.dir.join(JOURNAL_FILE));
    svc.set_revocations(p.revocations);
    if let Some(h) = p.ingest {
        svc = svc.with_ingest(h);
    }
    if let Some((h, routes)) = container {
        for r in routes {
            svc = svc.with_route(r, h.clone());
        }
    }
    if let Some(cfg) = p.serving {
        svc = svc.with_address(cfg.advertised());
        if let Some(l) = cfg.lease_secs {
            svc = svc.with_lease(std::time::Duration::from_secs(l));
        }
    }
    Ok(svc)
}

/// Bind `cfg.listen` and serve `svc` in the background. Returns the bound
/// address.
pub async fn serve(cfg: &HostConfig, svc: Arc<WorkloadHostService>) -> Result<SocketAddr, String> {
    let listener = listen_tcp(&cfg.listen)
        .await
        .map_err(|e| format!("workload-host listen {}: {e}", cfg.listen))?;
    let bound = listener.local_addr().map_err(|e| e.to_string())?;
    let noise = cfg.noise;
    tokio::spawn(async move {
        if let Err(e) = serve_listener(listener, svc, noise).await {
            tracing::warn!(error = %e, "workload-host listener stopped");
        }
    });
    Ok(bound)
}

#[cfg(test)]
#[path = "workload_host_serve_tests.rs"]
mod tests;
