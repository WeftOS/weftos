//! What crosses the edge-to-parent link, and the HTTP shapes both sides use.
//! The link itself (cogbus, mesh-local, a WebSocket) is the host's; the edge
//! is authenticated by that link, and the request carries no identity.

use serde::{Deserialize, Serialize};

use crate::creds::TempCredentials;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    pub scheme: String,
    /// `host` or `host:port` as it goes in the `Host` header.
    pub host: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum LeaseRequest {
    /// STS credentials for the node's own bucket.
    Sts,
    /// A presigned PUT for one key. The edge then holds no credentials at all.
    PresignPut { key: String, ttl_secs: u64 },
    /// A presigned GET for one key.
    PresignGet { key: String, ttl_secs: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Presigned {
    pub method: String,
    pub endpoint: Endpoint,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub url: String,
    pub expires_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LeaseResponse {
    Sts { credentials: TempCredentials, bucket: String, region: String, endpoint: Endpoint },
    Presigned(Presigned),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefusalCode {
    BadRequest,
    NotGranted,
    Revoked,
    RateLimited,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("lease refused ({code:?}): {message}")]
pub struct LeaseRefused {
    pub code: RefusalCode,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: String,
    pub scheme: String,
    pub host: String,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

#[derive(Debug, Clone, Default)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
#[error("transport: {0}")]
pub struct TransportError(pub String);

/// Link to the parent daemon, supplied by the host.
pub trait ParentLink {
    async fn request_lease(&self, req: LeaseRequest) -> Result<LeaseResponse, LeaseRefused>;
}

/// The edge's HTTP client (fetch on the browser build), supplied by the host.
pub trait ObjectTransport {
    async fn send(&self, req: HttpRequest) -> Result<HttpResponse, TransportError>;
}
