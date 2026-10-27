//! The router's live state inside the user daemon: the admitted route table,
//! the source it came from, and the poller that re-reads it when a project's
//! `compose/ports.yaml` changes (ADR-116 §2). `weaver route reload` goes
//! through [`RouterHandle::reload`] too.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Duration;

use crate::router_cfg::RouterConfig;
use crate::router_routes::RouteTable;
use crate::router_sources::{RouteSource, Stamp, fingerprint, load};

/// One router per daemon.
pub struct RouterHandle {
    pub cfg: RouterConfig,
    /// The address actually bound (the config's, or an ephemeral one in tests).
    pub bound: SocketAddr,
    source: Box<dyn RouteSource>,
    table: RwLock<Arc<RouteTable>>,
    stamps: Mutex<Vec<Stamp>>,
    reloaded_at: Mutex<Option<String>>,
    generation: AtomicU64,
    /// The accept loop, so shutdown can close the listener early.
    accept: Mutex<Option<tokio::task::AbortHandle>>,
}

impl RouterHandle {
    /// Build the handle and load the table once.
    pub fn new(cfg: RouterConfig, source: Box<dyn RouteSource>, bound: SocketAddr) -> Self {
        let table = load(source.as_ref());
        let stamps = fingerprint(source.as_ref());
        Self {
            cfg,
            bound,
            source,
            table: RwLock::new(Arc::new(table)),
            stamps: Mutex::new(stamps),
            reloaded_at: Mutex::new(Some(now())),
            generation: AtomicU64::new(1),
            accept: Mutex::new(None),
        }
    }

    /// The current table.
    pub fn table(&self) -> Arc<RouteTable> {
        self.table.read().map(|t| t.clone()).unwrap_or_default()
    }

    /// Re-read every source now.
    pub fn reload(&self) -> Arc<RouteTable> {
        let fresh = Arc::new(load(self.source.as_ref()));
        let stamps = fingerprint(self.source.as_ref());
        if let Ok(mut s) = self.stamps.lock() {
            *s = stamps;
        }
        if let Ok(mut t) = self.table.write() {
            *t = fresh.clone();
        }
        if let Ok(mut r) = self.reloaded_at.lock() {
            *r = Some(now());
        }
        self.generation.fetch_add(1, Ordering::SeqCst);
        tracing::info!(routes = fresh.routes.len(), refused = fresh.refused.len(), "router routes reloaded");
        fresh
    }

    /// Reload only when a watched file's mtime or length changed. Returns
    /// whether a reload happened.
    pub fn reload_if_changed(&self) -> bool {
        let fresh = fingerprint(self.source.as_ref());
        let changed = self.stamps.lock().map(|s| *s != fresh).unwrap_or(true);
        if changed {
            self.reload();
        }
        changed
    }

    /// Increments on every reload (1 after construction).
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// RFC 3339 time of the last (re)load.
    pub fn reloaded_at(&self) -> Option<String> {
        self.reloaded_at.lock().ok().and_then(|r| r.clone())
    }

    /// Stop accepting and close the listener (daemon shutdown). Connections
    /// already being served finish on their own tasks.
    pub fn stop_accepting(&self) {
        if let Some(h) = self.accept.lock().ok().and_then(|mut a| a.take()) {
            h.abort();
        }
    }

    /// Per-probe deadline from the config.
    pub fn health_timeout(&self) -> Duration {
        Duration::from_millis(self.cfg.health_timeout_ms)
    }
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

static GLOBAL: OnceLock<Arc<RouterHandle>> = OnceLock::new();

/// The daemon's router, once started.
pub fn global() -> Option<Arc<RouterHandle>> {
    GLOBAL.get().cloned()
}

/// Start the router from `[router]` in the user's `weave.toml`. A disabled
/// section does nothing; an invalid one logs why and leaves the daemon running.
pub async fn start(home: &Path) {
    let cfg = match crate::router_cfg::load(home).await {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "tailnet router not started: invalid [router] section");
            return;
        }
    };
    if !cfg.enabled {
        return;
    }
    let poll = Duration::from_secs(cfg.poll_secs);
    let source = crate::router_sources::ManifestSource {
        manifests_dir: crate::user_daemon::manifests_dir(home),
        overlays_dir: crate::router_overlay::overlays_dir(home),
    };
    match start_with(cfg, Box::new(source), poll).await {
        Ok(h) => {
            let t = h.table();
            tracing::info!(listen = %h.bound, routes = t.routes.len(), refused = t.refused.len(), "tailnet router started");
            if GLOBAL.set(h).is_err() {
                tracing::warn!("tailnet router already installed; second start ignored");
            }
        }
        Err(e) => tracing::error!(error = %e, "tailnet router not started"),
    }
}

/// Bind the configured address, load the table, and spawn the accept loop and
/// the mtime poller. Does not install the global (see [`start`]).
pub async fn start_with(cfg: RouterConfig, source: Box<dyn RouteSource>, poll: Duration) -> Result<Arc<RouterHandle>, String> {
    let addr = cfg.listen_addr()?;
    let listener = tokio::net::TcpListener::bind(addr).await.map_err(|e| format!("bind {addr}: {e}"))?;
    let bound = listener.local_addr().map_err(|e| format!("local_addr: {e}"))?;
    let handle = Arc::new(RouterHandle::new(cfg, source, bound));
    let accept = tokio::spawn(crate::router_proxy::accept_loop(listener, handle.clone()));
    if let Ok(mut a) = handle.accept.lock() {
        *a = Some(accept.abort_handle());
    }
    let poller = Arc::downgrade(&handle);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(poll).await;
            let Some(h) = poller.upgrade() else { break };
            let _ = tokio::task::spawn_blocking(move || h.reload_if_changed()).await;
        }
    });
    Ok(handle)
}
