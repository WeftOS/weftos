//! The loopback listener that keeps `127.0.0.1:<role port>` stable.
//!
//! Binding rules:
//! - Loopback addresses only. A non-loopback address is refused.
//! - A port something already answers on is never bound over. The caller
//!   chooses [`OccupiedPolicy::Refuse`] (an error) or
//!   [`OccupiedPolicy::Adopt`] (leave the existing server as the address;
//!   the card-18 adapter registers it as an `Adopted` instance).
//! - The socket is bound without `SO_REUSEADDR`, so a wildcard listener on
//!   the same port also makes the bind fail instead of being shadowed.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;

use super::http::{read_request_with, write_error, write_head};
use super::mesh_forward::forward_remote;
use super::svc::{SvcRoute, serve_svc};
use super::table::PlacementTable;
use super::types::{ProxyAudit, ProxyError, ProxyLimits, ResponseSink, Target};
use super::upstream::{PROBE_TIMEOUT, Upstream};

/// What to do when the port is already held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OccupiedPolicy {
    /// Fail with [`ProxyError::Occupied`].
    Refuse,
    /// Do not bind; the existing server stays the address.
    Adopt,
}

/// Result of [`InferProxy::start`].
pub enum Started {
    /// The proxy is listening.
    Running(InferProxy),
    /// Something else holds the port; nothing was bound.
    Adopted(SocketAddr),
}

/// Counters, readable while running.
#[derive(Debug, Default)]
pub struct ProxyStats {
    /// Requests accepted.
    pub requests: AtomicU64,
    /// Forwarded to a local instance.
    pub local: AtomicU64,
    /// Forwarded over the mesh.
    pub remote: AtomicU64,
    /// Refused or failed before any forwarding.
    pub rejected: AtomicU64,
}

/// A running listener for one role.
pub struct InferProxy {
    role: String,
    addr: SocketAddr,
    stats: Arc<ProxyStats>,
    task: JoinHandle<()>,
}

impl InferProxy {
    /// The bound address.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }
    /// The role this listener fronts.
    pub fn role(&self) -> &str {
        &self.role
    }
    /// Counters.
    pub fn stats(&self) -> &ProxyStats {
        &self.stats
    }
    /// Stop listening (in-flight requests finish on their own tasks).
    pub fn stop(&self) {
        self.task.abort();
    }
}

impl Drop for InferProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Open connections per client address.
type IpCounts = Arc<std::sync::Mutex<HashMap<IpAddr, usize>>>;

/// One of a client address's connections on an exposed listener.
struct IpSlot {
    counts: IpCounts,
    ip: IpAddr,
}

impl IpSlot {
    fn take(counts: &IpCounts, ip: IpAddr, cap: usize) -> Option<Self> {
        let mut g = counts.lock().unwrap_or_else(|e| e.into_inner());
        let n = g.entry(ip).or_insert(0);
        if *n >= cap {
            return None;
        }
        *n += 1;
        Some(Self { counts: counts.clone(), ip })
    }
}

impl Drop for IpSlot {
    fn drop(&mut self) {
        let mut g = self.counts.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = g.get_mut(&self.ip) {
            *n -= 1;
            if *n == 0 {
                g.remove(&self.ip);
            }
        }
    }
}

/// A bind address is loopback, or the caller holds a permit and a token.
pub fn check_bind(addr: SocketAddr, exposed: bool) -> Result<(), ProxyError> {
    if addr.ip().is_loopback() || exposed {
        Ok(())
    } else {
        Err(ProxyError::NotLoopback(addr.to_string()))
    }
}

async fn held(addr: SocketAddr) -> bool {
    matches!(
        tokio::time::timeout(PROBE_TIMEOUT, TcpStream::connect(addr)).await,
        Ok(Ok(_))
    )
}

/// The bearer credential of an exposed listener. At least 32 bytes; never
/// logged, chained or forwarded.
#[derive(Clone)]
pub struct ExposureAuth(Vec<u8>);

impl ExposureAuth {
    /// Shortest accepted token.
    pub const MIN_LEN: usize = 32;

    /// Wrap `token` (trimmed of surrounding whitespace).
    pub fn new(token: &str) -> Result<Self, ProxyError> {
        let t = token.trim();
        if t.len() < Self::MIN_LEN || !t.bytes().all(|b| (0x21..0x7f).contains(&b)) {
            return Err(ProxyError::Forbidden(format!(
                "an exposure token is {} or more visible ASCII characters",
                Self::MIN_LEN
            )));
        }
        Ok(Self(t.as_bytes().to_vec()))
    }
}

impl std::fmt::Debug for ExposureAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ExposureAuth(..)")
    }
}

/// Proof that the governance gate permitted binding beyond loopback.
/// It can only be made by [`ask`](Self::ask), which runs the gate check
/// itself (the gate chains its decisions), so holding one means the gate said
/// permit for this role just now.
#[derive(Debug)]
pub struct ExposurePermit(());

impl ExposurePermit {
    /// Ask `gate`, as `principal`, whether `role`'s proxy may listen beyond
    /// loopback: a `workload.start` of kind `inference` with `network: lan`
    /// and the package id `inference-expose:<role>`. A deny or a deferral is
    /// returned as the reason.
    pub fn ask(gate: &dyn crate::gate::GateBackend, principal: &str, role: &str) -> Result<Self, String> {
        let ctx = serde_json::json!({"workload": {
            "kind": "inference",
            "package_trust": "operator_attested",
            "node_tier": "pinned",
            "network": "lan",
            "secrets": false,
            "emulated": false,
            "resource_cost": 0.1,
            "package_id": format!("inference-expose:{role}"),
            "signer_keys": [],
            "artifact_hashes": [],
        }});
        let d = gate.check(principal, "workload.start", &ctx);
        Self::from_decision(&d).ok_or_else(|| match d {
            crate::gate::GateDecision::Deny { reason, .. } => {
                format!("governance denied listening beyond loopback: {reason}")
            }
            crate::gate::GateDecision::Defer { reason } => {
                format!("listening beyond loopback is deferred to a human: {reason}")
            }
            _ => "no governance permit".to_string(),
        })
    }

    /// `Some` only for a permit.
    pub(crate) fn from_decision(d: &crate::gate::GateDecision) -> Option<Self> {
        matches!(d, crate::gate::GateDecision::Permit { .. }).then_some(Self(()))
    }
}

struct Shared {
    audit: Option<Arc<dyn ProxyAudit>>,
    bearer: Option<Vec<u8>>,
    role: String,
    table: Arc<PlacementTable>,
    upstream: Upstream,
    limits: ProxyLimits,
    stats: Arc<ProxyStats>,
}

struct TcpSink<'a> {
    s: &'a mut TcpStream,
    sent_head: bool,
}

#[async_trait]
impl ResponseSink for TcpSink<'_> {
    async fn head(&mut self, status: u16, content_type: Option<&str>) -> Result<(), ProxyError> {
        write_head(self.s, status, content_type).await?;
        self.sent_head = true;
        Ok(())
    }
    async fn chunk(&mut self, data: &[u8]) -> Result<(), ProxyError> {
        self.s
            .write_all(data)
            .await
            .map_err(|e| ProxyError::Io(e.to_string()))
    }
}

async fn serve_conn(sh: Arc<Shared>, mut s: TcpStream, peer: SocketAddr) {
    sh.stats.requests.fetch_add(1, Ordering::Relaxed);
    let req = match read_request_with(&mut s, &sh.role, &sh.limits, sh.bearer.as_deref()).await {
        Ok(r) => r,
        Err(e) => {
            sh.stats.rejected.fetch_add(1, Ordering::Relaxed);
            write_error(&mut s, &e).await;
            return;
        }
    };
    let target = sh.table.resolve(&req.role);
    let mut sink = TcpSink {
        s: &mut s,
        sent_head: false,
    };
    let fwd = async {
        match &target {
            Some(Target::Local { base }) => {
                sh.stats.local.fetch_add(1, Ordering::Relaxed);
                sh.upstream.forward(base, &req, true, &mut sink).await
            }
            Some(Target::Svc { topic, model }) => {
                let Some(b) = sh.table.svc_backend() else {
                    return Err(ProxyError::NoInstance(req.role.clone()));
                };
                sh.stats.local.fetch_add(1, Ordering::Relaxed);
                let route = SvcRoute { topic: topic.clone(), model: model.clone() };
                // The IP only: a source port is new per connection, so a deny or
                // the per-caller image cap keyed on it would never hold.
                let caller = format!("local:{}", peer.ip());
                serve_svc(b.as_ref(), &route, &req, &caller, &mut sink, sh.audit.as_deref()).await
            }
            Some(Target::Remote { node_id }) => {
                let Some(d) = sh.table.dialer() else {
                    return Err(ProxyError::NoInstance(req.role.clone()));
                };
                sh.stats.remote.fetch_add(1, Ordering::Relaxed);
                forward_remote(d.as_ref(), node_id, &req, &mut sink, &sh.limits).await
            }
            None => Err(ProxyError::NoInstance(req.role.clone())),
        }
    };
    let res = tokio::time::timeout(sh.limits.request_timeout, fwd)
        .await
        .unwrap_or_else(|_| Err(ProxyError::Timeout("request".into())));
    let sent_head = sink.sent_head;
    match res {
        Ok(()) => {
            let _ = s.shutdown().await;
        }
        Err(e) if !sent_head => {
            sh.stats.rejected.fetch_add(1, Ordering::Relaxed);
            write_error(&mut s, &e).await;
        }
        // Mid-body failure: the status is already out, so cut the stream.
        Err(_) => {
            let _ = s.shutdown().await;
        }
    }
}

impl InferProxy {
    /// Start fronting `role` on `addr` (must be loopback).
    pub async fn start(
        role: &str,
        addr: SocketAddr,
        policy: OccupiedPolicy,
        table: Arc<PlacementTable>,
        limits: ProxyLimits,
        audit: Option<Arc<dyn ProxyAudit>>,
    ) -> Result<Started, ProxyError> {
        Self::start_inner(role, addr, policy, table, limits, audit, None).await
    }

    /// Start fronting `role` on `addr`, which may be beyond loopback
    /// (`0.0.0.0` for clients such as containers on this machine). Needs a
    /// governance [`ExposurePermit`] and a bearer token: every request must
    /// carry `Authorization: Bearer <token>` (checked before any body is
    /// read), the token is never forwarded, and the `Host` need not be
    /// loopback. The default, [`start`](Self::start), stays loopback only.
    ///
    /// The token travels in cleartext (HTTP, no TLS): `network: lan` is for
    /// trusted segments until TLS lands. An exposed listener is *in
    /// addition to* a loopback one: it does not register as the proxy port
    /// of the role (local consumers keep using the token-free loopback
    /// listener), and it has its own connection pool with a per-client cap.
    #[allow(clippy::too_many_arguments)]
    pub async fn start_exposed(
        role: &str,
        addr: SocketAddr,
        policy: OccupiedPolicy,
        table: Arc<PlacementTable>,
        limits: ProxyLimits,
        audit: Option<Arc<dyn ProxyAudit>>,
        _permit: ExposurePermit,
        auth: ExposureAuth,
    ) -> Result<Started, ProxyError> {
        Self::start_inner(role, addr, policy, table, limits, audit, Some(auth)).await
    }

    async fn start_inner(
        role: &str,
        addr: SocketAddr,
        policy: OccupiedPolicy,
        table: Arc<PlacementTable>,
        limits: ProxyLimits,
        audit: Option<Arc<dyn ProxyAudit>>,
        auth: Option<ExposureAuth>,
    ) -> Result<Started, ProxyError> {
        let note = |kind: &str, extra: serde_json::Value| {
            if let Some(a) = &audit {
                a.record(
                    kind,
                    serde_json::json!({"role": role, "addr": addr.to_string(), "detail": extra}),
                );
            }
        };
        if let Err(e) = check_bind(addr, auth.is_some()) {
            note("infer.proxy.refused", serde_json::json!("not loopback"));
            return Err(e);
        }
        // Probe the same port on every loopback name a client might use, v4
        // and v6, so a server bound to the wildcard or to the other family
        // is seen as well.
        let probes = [
            addr,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), addr.port()),
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), addr.port()),
        ];
        for p in probes {
            if addr.port() != 0 && held(p).await {
                note("infer.proxy.occupied", serde_json::json!(format!("{policy:?}")));
                return match policy {
                    OccupiedPolicy::Refuse => Err(ProxyError::Occupied(addr.port())),
                    OccupiedPolicy::Adopt => Ok(Started::Adopted(addr)),
                };
            }
        }
        let socket = match addr {
            SocketAddr::V4(_) => TcpSocket::new_v4(),
            SocketAddr::V6(_) => TcpSocket::new_v6(),
        }
        .map_err(|e| ProxyError::Io(e.to_string()))?;
        socket
            .set_reuseaddr(false)
            .map_err(|e| ProxyError::Io(e.to_string()))?;
        if socket.bind(addr).is_err() {
            note("infer.proxy.occupied", serde_json::json!("bind failed"));
            return match policy {
                OccupiedPolicy::Refuse => Err(ProxyError::Occupied(addr.port())),
                OccupiedPolicy::Adopt => Ok(Started::Adopted(addr)),
            };
        }
        let listener: TcpListener = socket
            .listen(128)
            .map_err(|e| ProxyError::Io(e.to_string()))?;
        let bound = listener
            .local_addr()
            .map_err(|e| ProxyError::Io(e.to_string()))?;
        // Only the loopback listener is the address in-process consumers use
        // for a remote role; an exposed listener demands a token they lack.
        if auth.is_none() {
            table.set_proxy_port(role, bound.port());
        }
        note(
            "infer.proxy.bind",
            serde_json::json!({"listening": bound.to_string(), "exposed": auth.is_some()}),
        );

        let stats = Arc::new(ProxyStats::default());
        let sh = Arc::new(Shared {
            audit: audit.clone(),
            bearer: auth.map(|a| a.0),
            role: role.to_string(),
            table,
            upstream: Upstream::new(limits.clone())?,
            limits: limits.clone(),
            stats: stats.clone(),
        });
        // Each listener has its own connection pool, so clients of the
        // exposed one (slow or many) cannot starve the loopback one. The
        // exposed pool also caps connections per client address.
        let permits = Arc::new(Semaphore::new(limits.max_connections));
        let exposed = sh.bearer.is_some();
        let per_ip: Option<(usize, IpCounts)> =
            exposed.then(|| (limits.max_connections_per_ip.max(1), Arc::default()));
        let task = tokio::spawn(async move {
            let mut backoff = Duration::from_millis(10);
            loop {
                let (s, peer) = match listener.accept().await {
                    Ok(x) => {
                        backoff = Duration::from_millis(10);
                        x
                    }
                    Err(_) => {
                        // EMFILE and friends: do not spin.
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(Duration::from_secs(1));
                        continue;
                    }
                };
                // Loopback listener: a non-loopback peer cannot arrive, but
                // the check costs nothing.
                if !exposed && !peer.ip().is_loopback() {
                    continue;
                }
                let Ok(permit) = permits.clone().try_acquire_owned() else {
                    // Over the connection cap: shed load.
                    drop(s);
                    continue;
                };
                let slot = match &per_ip {
                    Some((cap, counts)) => match IpSlot::take(counts, peer.ip(), *cap) {
                        Some(slot) => Some(slot),
                        None => {
                            drop(s);
                            continue;
                        }
                    },
                    None => None,
                };
                let sh = sh.clone();
                tokio::spawn(async move {
                    serve_conn(sh, s, peer).await;
                    drop(permit);
                    drop(slot);
                });
            }
        });
        Ok(Started::Running(Self {
            role: role.to_string(),
            addr: bound,
            stats,
            task,
        }))
    }
}
