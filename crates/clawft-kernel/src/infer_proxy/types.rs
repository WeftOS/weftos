//! Shared types of the stable inference address (card mesh-placement-19).

use std::time::Duration;

use async_trait::async_trait;

use crate::mesh::MeshStream;
use crate::mesh_admit::{Grant, PeerClass};

/// Hard limits. Every one bounds something a client or a peer controls.
#[derive(Debug, Clone)]
pub struct ProxyLimits {
    /// Request line plus headers.
    pub max_head_bytes: usize,
    /// Header lines.
    pub max_headers: usize,
    /// Request body.
    pub max_request_body: usize,
    /// Request body over the mesh (sent as hex inside a control message).
    pub max_mesh_request_body: usize,
    /// Response body, summed over all chunks.
    pub max_response_body: u64,
    /// Time to read a request head.
    pub head_timeout: Duration,
    /// Time to read a request body.
    pub body_timeout: Duration,
    /// Time to connect to an upstream.
    pub connect_timeout: Duration,
    /// Longest silence between upstream (or mesh) chunks.
    pub stall_timeout: Duration,
    /// Wall-clock cap on one forwarded request.
    pub request_timeout: Duration,
    /// Concurrent connections per listener.
    pub max_connections: usize,
    /// Concurrent connections per client address on an exposed listener.
    pub max_connections_per_ip: usize,
}

impl Default for ProxyLimits {
    fn default() -> Self {
        Self {
            max_head_bytes: 16 * 1024,
            max_headers: 64,
            max_request_body: 8 * 1024 * 1024,
            max_mesh_request_body: 2 * 1024 * 1024,
            max_response_body: 64 * 1024 * 1024,
            head_timeout: Duration::from_secs(10),
            body_timeout: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(5),
            stall_timeout: Duration::from_secs(120),
            request_timeout: Duration::from_secs(900),
            max_connections: 32,
            max_connections_per_ip: 8,
        }
    }
}

/// Methods the proxy forwards. Anything else is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// GET.
    Get,
    /// POST.
    Post,
}

impl Method {
    /// Wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
        }
    }
}

/// A request that passed validation: nothing here can name a host.
#[derive(Debug, Clone)]
pub struct ProxyRequest {
    /// Inference role the request is for.
    pub role: String,
    /// Method.
    pub method: Method,
    /// Origin-form path and query, allowlisted.
    pub path: String,
    /// `Content-Type`.
    pub content_type: Option<String>,
    /// `Accept`.
    pub accept: Option<String>,
    /// `Authorization`: forwarded to a local server only, never over the mesh.
    pub authorization: Option<String>,
    /// Body.
    pub body: Vec<u8>,
}

/// Where a role is served right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A loopback instance on this node (`http://127.0.0.1:PORT`).
    Local {
        /// Base URL, verified loopback.
        base: String,
    },
    /// An instance on an admitted peer, reached over the mesh.
    Remote {
        /// Peer node id.
        node_id: String,
    },
    /// A provider of a cogbus service on this node (the Venq's NPU cog), called
    /// through the table's [`SvcBackend`](super::svc::SvcBackend).
    Svc {
        /// `svc.infer/<source_id>`.
        topic: String,
        /// Model id the role uses.
        model: String,
    },
}

/// Why a request was not served.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProxyError {
    /// Malformed request.
    #[error("bad request: {0}")]
    BadRequest(String),
    /// Missing or wrong bearer token on an exposed listener.
    #[error("unauthorized")]
    Unauthorized,
    /// The request is not one the proxy forwards (path, host, origin).
    #[error("forbidden: {0}")]
    Forbidden(String),
    /// Method other than GET or POST.
    #[error("method not allowed")]
    MethodNotAllowed,
    /// Request body or head over the limit.
    #[error("too large: {0}")]
    TooLarge(String),
    /// A framing the proxy does not implement (chunked request bodies).
    #[error("not implemented: {0}")]
    Unsupported(String),
    /// Nothing serves the role.
    #[error("no instance serves role '{0}'")]
    NoInstance(String),
    /// A local limit is full (content store quota, open uploads, concurrent
    /// image calls): retry later. 429 with `Retry-After`.
    #[error("busy: {0}")]
    Busy(String),
    /// The upstream failed or answered garbage.
    #[error("upstream: {0}")]
    Upstream(String),
    /// The mesh path failed.
    #[error("mesh: {0}")]
    Mesh(String),
    /// A timeout elapsed.
    #[error("timeout: {0}")]
    Timeout(String),
    /// The peer is not an admitted, verified peer.
    #[error("refused: {0}")]
    Refused(String),
    /// A bind address or endpoint that is not loopback.
    #[error("not loopback: {0}")]
    NotLoopback(String),
    /// The port is held by something else.
    #[error("port {0} is in use")]
    Occupied(u16),
    /// Local I/O.
    #[error("io: {0}")]
    Io(String),
}

impl ProxyError {
    /// HTTP status for a failure before any response byte was sent.
    pub fn status(&self) -> u16 {
        match self {
            Self::BadRequest(_) => 400,
            Self::Unauthorized => 401,
            Self::Forbidden(_) | Self::Refused(_) => 403,
            Self::MethodNotAllowed => 405,
            Self::TooLarge(_) => 413,
            Self::Unsupported(_) => 501,
            Self::NoInstance(_) => 503,
            Self::Busy(_) => 429,
            Self::Upstream(_) | Self::Mesh(_) => 502,
            Self::Timeout(_) => 504,
            Self::NotLoopback(_) | Self::Occupied(_) | Self::Io(_) => 500,
        }
    }
}

/// Receives a response as it is produced (so a streamed completion is
/// relayed as it arrives, not buffered).
#[async_trait]
pub trait ResponseSink: Send {
    /// The status line and content type. Called once, first.
    async fn head(&mut self, status: u16, content_type: Option<&str>) -> Result<(), ProxyError>;
    /// A piece of the body.
    async fn chunk(&mut self, data: &[u8]) -> Result<(), ProxyError>;
}

/// A peer's standing is good enough to serve inference: admission was
/// enforced (not observe or off), the peer is a full node (not a leaf or
/// legacy device), and its scope is trusted.
pub fn qualifies(g: &Grant) -> bool {
    g.admitted && g.class == PeerClass::Node && g.trust_scope && g.observed.is_none()
}

/// The mesh side of the proxy. The implementation is the only authority on
/// who is admitted: `dial` must return a stream only to a peer that passed
/// admission and whose id was verified against the connection (an
/// authenticated Noise channel), never to an address a client named.
#[async_trait]
pub trait MeshDialer: Send + Sync + 'static {
    /// The admission grant of the connected peer `node_id`, if any.
    fn standing(&self, node_id: &str) -> Option<Grant>;
    /// Whether `node_id` is an enforced-admission full node with a trusted
    /// scope right now.
    fn is_admitted(&self, node_id: &str) -> bool {
        self.standing(node_id).is_some_and(|g| qualifies(&g))
    }
    /// An authenticated stream to the peer.
    async fn dial(&self, node_id: &str) -> Result<Box<dyn MeshStream>, ProxyError>;
}

/// Where security-relevant proxy decisions are recorded (the chain, in
/// production). Per-request traffic is not recorded, only binds, refusals
/// and exposure changes.
pub trait ProxyAudit: Send + Sync + 'static {
    /// Record `kind` with `payload`.
    fn record(&self, kind: &str, payload: serde_json::Value);
}

/// Audit into the ExoChain.
#[cfg(feature = "exochain")]
pub struct ChainAudit(pub std::sync::Arc<crate::chain::ChainManager>);

#[cfg(feature = "exochain")]
impl ProxyAudit for ChainAudit {
    fn record(&self, kind: &str, payload: serde_json::Value) {
        self.0.append("infer_proxy", kind, Some(payload));
    }
}
