//! Forwarding to a loopback inference server.
//!
//! The destination is never taken from the request: it is a base URL the
//! placement table verified as `http://<loopback ip>:<port>`, joined with
//! an allowlisted origin-form path. The client keeps no proxy, follows no
//! redirect and reuses no connection.

use std::net::IpAddr;
use std::time::Duration;

use super::hosted::{HostedRoute, pin_model};
use super::types::{Method, ProxyError, ProxyLimits, ProxyRequest, ResponseSink};

/// `http://<loopback ip>:<port>` and nothing else (no path, userinfo or
/// name: `localhost` could be remapped, an address cannot).
pub fn parse_loopback_base(base: &str) -> Result<(IpAddr, u16), ProxyError> {
    let bad = || ProxyError::NotLoopback(base.to_string());
    let rest = base.strip_prefix("http://").ok_or_else(bad)?;
    let rest = rest.trim_end_matches('/');
    let sock: std::net::SocketAddr = rest.parse().map_err(|_| bad())?;
    if !sock.ip().is_loopback() || sock.port() == 0 {
        return Err(bad());
    }
    Ok((sock.ip(), sock.port()))
}

/// Streaming HTTP client for local servers.
pub struct Upstream {
    http: reqwest::Client,
    limits: ProxyLimits,
}

impl Upstream {
    /// Client with the limits' connect timeout.
    pub fn new(limits: ProxyLimits) -> Result<Self, ProxyError> {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .pool_max_idle_per_host(0)
            .connect_timeout(limits.connect_timeout)
            .build()
            .map_err(|e| ProxyError::Io(format!("http client: {e}")))?;
        Ok(Self { http, limits })
    }

    /// Limits in force.
    pub fn limits(&self) -> &ProxyLimits {
        &self.limits
    }

    /// Forward `req` to the verified loopback `base`, relaying the response
    /// into `sink` as it arrives. `send_auth` is true only for a request from
    /// a local client on this node's loopback listener: the client chose to
    /// send that credential to this address, and a server started with
    /// `--api-key` needs it. It is forwarded to the local instance only and
    /// never over the mesh; it is false for every request that arrived over
    /// the mesh (a peer's credentials are not ours to pass on).
    pub async fn forward(
        &self,
        base: &str,
        req: &ProxyRequest,
        send_auth: bool,
        sink: &mut dyn ResponseSink,
    ) -> Result<(), ProxyError> {
        self.forward_stall(base, req, send_auth, sink, self.limits.stall_timeout).await
    }

    /// [`forward`](Self::forward) with this request's own stall timeout (a
    /// role may set one longer than the default for a slow cold prefill).
    pub async fn forward_stall(
        &self,
        base: &str,
        req: &ProxyRequest,
        send_auth: bool,
        sink: &mut dyn ResponseSink,
        stall: Duration,
    ) -> Result<(), ProxyError> {
        let (ip, port) = parse_loopback_base(base)?;
        let authority = match ip {
            IpAddr::V4(a) => format!("{a}:{port}"),
            IpAddr::V6(a) => format!("[{a}]:{port}"),
        };
        let url = format!("http://{authority}{}", req.path);
        let parsed = reqwest::Url::parse(&url).map_err(|e| ProxyError::BadRequest(e.to_string()))?;
        // Defense in depth: whatever the path was, the host is unchanged.
        if parsed.port() != Some(port) || parsed.host_str().is_none() {
            return Err(ProxyError::Forbidden("destination changed".into()));
        }
        let mut b = match req.method {
            Method::Get => self.http.get(parsed),
            Method::Post => self.http.post(parsed).body(req.body.clone()),
        };
        if let Some(ct) = &req.content_type {
            b = b.header("content-type", ct);
        }
        if let Some(a) = &req.accept {
            b = b.header("accept", a);
        }
        if send_auth && let Some(a) = &req.authorization {
            b = b.header("authorization", a);
        }
        self.relay(b, sink, stall).await
    }

    /// Forward `req` to a hosted API role. The URL comes from the role's
    /// config, the client's `Authorization` is dropped, and the role's key
    /// (read from the environment by name, now) is sent as a bearer token.
    /// The key appears in no error: failures name the variable only.
    pub async fn forward_hosted(
        &self,
        route: &HostedRoute,
        req: &ProxyRequest,
        sink: &mut dyn ResponseSink,
        stall: Duration,
    ) -> Result<(), ProxyError> {
        let url = route.url_for(&req.path)?;
        let auth = route.bearer()?;
        let mut b = match req.method {
            Method::Get => self.http.get(url),
            Method::Post => self.http.post(url).body(pin_model(&req.body, &route.model)?),
        };
        if let Some(ct) = &req.content_type {
            b = b.header("content-type", ct);
        }
        if let Some(a) = &req.accept {
            b = b.header("accept", a);
        }
        self.relay(b.header("authorization", auth), sink, stall).await
    }

    /// Send, then relay the response into `sink` chunk by chunk.
    async fn relay(
        &self,
        b: reqwest::RequestBuilder,
        sink: &mut dyn ResponseSink,
        stall: Duration,
    ) -> Result<(), ProxyError> {
        let mut resp = tokio::time::timeout(stall, b.send())
            .await
            .map_err(|_| ProxyError::Timeout("upstream did not answer".into()))?
            .map_err(|e| ProxyError::Upstream(e.without_url().to_string()))?;
        let ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let code = resp.status().as_u16();
        if !(200..=599).contains(&code) {
            // 1xx (or 101 Upgrade) is not a final answer and must not reach
            // the client as one.
            return Err(ProxyError::Upstream(format!("unexpected status {code}")));
        }
        sink.head(code, ct.as_deref()).await?;
        let mut total: u64 = 0;
        loop {
            let next = tokio::time::timeout(stall, resp.chunk())
                .await
                .map_err(|_| ProxyError::Timeout("upstream stalled".into()))?
                .map_err(|e| ProxyError::Upstream(e.without_url().to_string()))?;
            let Some(c) = next else { break };
            total += c.len() as u64;
            if total > self.limits.max_response_body {
                return Err(ProxyError::TooLarge("response body".into()));
            }
            sink.chunk(&c).await?;
        }
        Ok(())
    }
}

/// Connect timeout used when probing whether a port is held.
pub const PROBE_TIMEOUT: Duration = Duration::from_millis(300);
