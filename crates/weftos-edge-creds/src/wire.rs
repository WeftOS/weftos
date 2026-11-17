//! What crosses the edge-to-parent link, and the HTTP shapes both sides use.
//! The link itself (cogbus, mesh-local, a WebSocket) is the host's; the edge
//! is authenticated by that link, and the request carries no identity.

use serde::{Deserialize, Serialize};

use crate::creds::TempCredentials;

/// Scheme and host of the object store, as the edge addresses it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    /// URL scheme, such as `http` or `https`.
    pub scheme: String,
    /// `host` or `host:port` as it goes in the `Host` header.
    pub host: String,
}

/// A lease the edge asks the parent for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum LeaseRequest {
    /// STS credentials for the node's own bucket.
    Sts,
    /// A presigned PUT for one key. The edge then holds no credentials at all.
    PresignPut {
        /// Object key to upload.
        key: String,
        /// Requested URL lifetime in seconds.
        ttl_secs: u64,
    },
    /// A presigned GET for one key.
    PresignGet {
        /// Object key to download.
        key: String,
        /// Requested URL lifetime in seconds.
        ttl_secs: u64,
    },
}

/// A presigned URL for one object, with the parts needed to use it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Presigned {
    /// HTTP method the URL is signed for.
    pub method: String,
    /// Endpoint the URL points at.
    pub endpoint: Endpoint,
    /// Request path of the URL.
    pub path: String,
    /// Query parameters of the URL.
    pub query: Vec<(String, String)>,
    /// The complete presigned URL.
    pub url: String,
    /// Expiry of the URL, unix seconds.
    pub expires_at: u64,
}

/// The parent's answer to a [`LeaseRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LeaseResponse {
    /// An STS session scoped to the node's bucket.
    Sts {
        /// The temporary credentials.
        credentials: TempCredentials,
        /// Bucket the session is scoped to.
        bucket: String,
        /// Region to sign for.
        region: String,
        /// Object store endpoint to send requests to.
        endpoint: Endpoint,
    },
    /// A presigned URL for a single object.
    Presigned(Presigned),
}

/// Machine-readable reason a lease was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefusalCode {
    /// The node id or object key was invalid.
    BadRequest,
    /// The node has no grant for the store.
    NotGranted,
    /// The node has been revoked.
    Revoked,
    /// The node exceeded its lease rate limit.
    RateLimited,
    /// The credential service (STS minter) failed or misbehaved.
    Unavailable,
}

/// A lease refusal: a code and a human-readable message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("lease refused ({code:?}): {message}")]
pub struct LeaseRefused {
    /// Why the lease was refused.
    pub code: RefusalCode,
    /// Human-readable detail.
    pub message: String,
}

/// An HTTP request handed to the host's transport.
#[derive(Clone)]
pub struct HttpRequest {
    /// HTTP method, such as `GET`.
    pub method: String,
    /// URL scheme.
    pub scheme: String,
    /// Host (and port) for the `Host` header.
    pub host: String,
    /// Request path.
    pub path: String,
    /// Query parameters.
    pub query: Vec<(String, String)>,
    /// Request headers as name and value pairs.
    pub headers: Vec<(String, String)>,
    /// Request body bytes.
    pub body: Vec<u8>,
}

impl HttpRequest {
    /// First header value whose name matches `name`, ignoring case.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

/// An HTTP response returned by the host's transport.
#[derive(Clone, Default)]
pub struct HttpResponse {
    /// HTTP status code.
    pub status: u16,
    /// Response headers as name and value pairs.
    pub headers: Vec<(String, String)>,
    /// Response body bytes.
    pub body: Vec<u8>,
}

/// A failure in the host's transport, with its message.
#[derive(Debug, thiserror::Error)]
#[error("transport: {0}")]
pub struct TransportError(pub String);

/// Link to the parent daemon, supplied by the host.
pub trait ParentLink {
    /// Ask the parent daemon for a lease of the given kind.
    async fn request_lease(&self, req: LeaseRequest) -> Result<LeaseResponse, LeaseRefused>;
}

/// The edge's HTTP client (fetch on the browser build), supplied by the host.
pub trait ObjectTransport {
    /// Send one HTTP request and return the response.
    async fn send(&self, req: HttpRequest) -> Result<HttpResponse, TransportError>;
}

// Requests and responses carry session tokens, signatures and, on the admin
// path, long-lived secrets, so Debug prints shape only.
impl std::fmt::Debug for Presigned {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Presigned({} {}, <redacted>)", self.method, self.path)
    }
}

impl std::fmt::Debug for HttpRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HttpRequest({} {}{}, {} body bytes, <redacted>)", self.method, self.host, self.path, self.body.len())
    }
}

impl std::fmt::Debug for HttpResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HttpResponse({}, {} body bytes, <redacted>)", self.status, self.body.len())
    }
}
