//! Minimal HTTP/1.1 for the loopback listener, and the request allowlist.
//!
//! One request per connection (`Connection: close`), so there is no
//! pipelining and no keep-alive state to get wrong. Only origin-form
//! targets are accepted: an absolute-form target (`GET http://host/...`)
//! is how a forward proxy becomes an open relay, so it is refused.

use std::net::IpAddr;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::types::{Method, ProxyError, ProxyLimits, ProxyRequest};

/// A parsed request head.
#[derive(Debug)]
pub struct Head {
    /// Method token.
    pub method: String,
    /// Request target.
    pub target: String,
    /// Header (lower-case name, value) pairs.
    pub headers: Vec<(String, String)>,
}

impl Head {
    /// First value of `name` (lower-case).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

fn io(e: std::io::Error) -> ProxyError {
    ProxyError::Io(e.to_string())
}

/// Paths the proxy forwards: the OpenAI-compatible surface, the servers'
/// health endpoints, and Ollama's inference and read-only endpoints. Model
/// management (`/api/pull`, `/api/delete`, `/api/create`, ...) is refused.
pub fn path_allowed(path: &str) -> bool {
    let route = path.split('?').next().unwrap_or("");
    if route.starts_with("/v1/") {
        return true;
    }
    matches!(
        route,
        "/health"
            | "/v1"
            | "/api/tags"
            | "/api/version"
            | "/api/ps"
            | "/api/show"
            | "/api/chat"
            | "/api/generate"
            | "/api/embed"
            | "/api/embeddings"
    )
}

/// Origin-form path with no way to change the authority, climb out of the
/// route, or smuggle control bytes.
pub fn validate_path(path: &str) -> Result<(), ProxyError> {
    let bad = |m: &str| Err(ProxyError::Forbidden(m.into()));
    if path.len() > 2048 || !path.starts_with('/') || path.starts_with("//") {
        return bad("path must be origin-form");
    }
    if path
        .bytes()
        .any(|b| b <= b' ' || b == 0x7f || b == b'\\' || b == b'#')
    {
        return bad("path has forbidden characters");
    }
    let route = path.split('?').next().unwrap_or("");
    let lower = route.to_ascii_lowercase();
    if route.split('/').any(|s| s == ".." || s == ".")
        || ["%2e", "%2f", "%5c", "%00"].iter().any(|p| lower.contains(p))
    {
        return bad("path traversal");
    }
    if !path_allowed(path) {
        return bad("path is not served by the inference proxy");
    }
    Ok(())
}

/// `host[:port]` is a loopback name or address (DNS-rebinding guard). The
/// port, when present, must be digits: `127.0.0.1:80.evil.example` is not
/// a loopback host.
pub fn loopback_host(value: &str) -> bool {
    let v = value.trim();
    let (host, port) = if let Some(rest) = v.strip_prefix('[') {
        match rest.split_once(']') {
            Some((h, tail)) => (h, tail.strip_prefix(':')),
            None => return false,
        }
    } else {
        match v.split_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (v, None),
        }
    };
    if port.is_some_and(|p| p.is_empty() || p.len() > 5 || !p.bytes().all(|b| b.is_ascii_digit())) {
        return false;
    }
    host.eq_ignore_ascii_case("localhost") || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// An `Origin` is acceptable when absent or itself loopback: a web page on
/// another origin must not drive a local model through the proxy.
pub fn origin_ok(origin: Option<&str>) -> bool {
    match origin {
        None => true,
        Some(o) => o
            .strip_prefix("http://")
            .is_some_and(|rest| loopback_host(rest.split('/').next().unwrap_or(""))),
    }
}

fn token_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

/// Parse a head (without the terminating blank line).
pub fn parse_head(raw: &[u8], limits: &ProxyLimits) -> Result<Head, ProxyError> {
    let bad = |m: &str| ProxyError::BadRequest(m.into());
    let text = std::str::from_utf8(raw).map_err(|_| bad("head is not ASCII"))?;
    let mut lines = text.split("\r\n");
    let line = lines.next().ok_or_else(|| bad("empty request"))?;
    let mut parts = line.split(' ');
    let (method, target, version) = (
        parts.next().unwrap_or(""),
        parts.next().unwrap_or(""),
        parts.next().unwrap_or(""),
    );
    if parts.next().is_some() || method.is_empty() || target.is_empty() {
        return Err(bad("malformed request line"));
    }
    if !method.bytes().all(token_char) {
        return Err(bad("bad method"));
    }
    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        return Err(bad("unsupported HTTP version"));
    }
    let mut headers = Vec::new();
    for l in lines {
        if headers.len() >= limits.max_headers {
            return Err(ProxyError::TooLarge("too many headers".into()));
        }
        if l.starts_with(' ') || l.starts_with('\t') {
            return Err(bad("folded header"));
        }
        let (n, v) = l.split_once(':').ok_or_else(|| bad("malformed header"))?;
        if n.is_empty() || !n.bytes().all(token_char) {
            return Err(bad("bad header name"));
        }
        let v = v.trim();
        if v.bytes().any(|b| b < b' ' && b != b'\t' || b == 0x7f) {
            return Err(bad("control byte in header"));
        }
        headers.push((n.to_ascii_lowercase(), v.to_string()));
    }
    Ok(Head {
        method: method.to_string(),
        target: target.to_string(),
        headers,
    })
}

/// A `\n` not preceded by `\r`: lenient parsers elsewhere accept it, which
/// is the root of request-smuggling disagreements, so it is refused.
fn has_bare_lf(buf: &[u8]) -> bool {
    buf.iter()
        .enumerate()
        .any(|(i, b)| *b == b'\n' && (i == 0 || buf[i - 1] != b'\r'))
}

fn find_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Read one request: head, then exactly `Content-Length` body bytes.
pub async fn read_request<S>(
    s: &mut S,
    role: &str,
    limits: &ProxyLimits,
) -> Result<ProxyRequest, ProxyError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    read_request_with(s, role, limits, None).await
}

/// Constant-time equality of two byte strings of any length.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        diff |= usize::from(a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0));
    }
    diff == 0
}

/// [`read_request`] for a listener with a bearer token (an exposed
/// listener, see `InferProxy::start_exposed`). The token is checked right
/// after the head, before any body is read, and then removed from the
/// request: it is this listener's credential, not the model server's. With
/// a token the `Host` need not be loopback (the credential, not the
/// address, is the guard); without one it must be.
pub async fn read_request_with<S>(
    s: &mut S,
    role: &str,
    limits: &ProxyLimits,
    bearer: Option<&[u8]>,
) -> Result<ProxyRequest, ProxyError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let timeout = |what: &str| ProxyError::Timeout(format!("reading request {what}"));
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0u8; 4096];
    let end = tokio::time::timeout(limits.head_timeout, async {
        loop {
            if let Some(i) = find_end(&buf) {
                return Ok(i);
            }
            if buf.len() > limits.max_head_bytes {
                return Err(ProxyError::TooLarge("request head".into()));
            }
            if has_bare_lf(&buf) {
                return Err(ProxyError::BadRequest("bare LF line ending".into()));
            }
            let n = s.read(&mut chunk).await.map_err(io)?;
            if n == 0 {
                return Err(ProxyError::BadRequest("connection closed in head".into()));
            }
            buf.extend_from_slice(&chunk[..n]);
        }
    })
    .await
    .map_err(|_| timeout("head"))??;
    if end > limits.max_head_bytes {
        return Err(ProxyError::TooLarge("request head".into()));
    }
    if has_bare_lf(&buf[..end]) {
        return Err(ProxyError::BadRequest("bare LF line ending".into()));
    }
    let head = parse_head(&buf[..end], limits)?;
    let mut body = buf[end + 4..].to_vec();

    let method = match head.method.as_str() {
        "GET" => Method::Get,
        "POST" => Method::Post,
        _ => return Err(ProxyError::MethodNotAllowed),
    };
    validate_path(&head.target)?;
    match bearer {
        Some(token) => {
            let sent = head
                .header("authorization")
                .and_then(|a| a.strip_prefix("Bearer "))
                .unwrap_or("");
            if !ct_eq(sent.as_bytes(), token) {
                return Err(ProxyError::Unauthorized);
            }
        }
        None => match head.header("host") {
            Some(h) if loopback_host(h) => {}
            _ => return Err(ProxyError::Forbidden("Host is not loopback".into())),
        },
    }
    if !origin_ok(head.header("origin")) {
        return Err(ProxyError::Forbidden("cross-origin request".into()));
    }
    if head.header("transfer-encoding").is_some() {
        return Err(ProxyError::Unsupported(
            "chunked request bodies: send Content-Length".into(),
        ));
    }
    let lens: Vec<&str> = head
        .headers
        .iter()
        .filter(|(n, _)| n == "content-length")
        .map(|(_, v)| v.as_str())
        .collect();
    let want: usize = match lens.as_slice() {
        [] => 0,
        [one] if !one.is_empty() && one.bytes().all(|b| b.is_ascii_digit()) => one
            .parse()
            .map_err(|_| ProxyError::TooLarge("Content-Length".into()))?,
        _ => return Err(ProxyError::BadRequest("bad Content-Length".into())),
    };
    if want > limits.max_request_body {
        return Err(ProxyError::TooLarge(format!(
            "request body over {} bytes",
            limits.max_request_body
        )));
    }
    if want > body.len() {
        if head
            .header("expect")
            .is_some_and(|e| e.eq_ignore_ascii_case("100-continue"))
        {
            s.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")
                .await
                .map_err(io)?;
        }
        tokio::time::timeout(limits.body_timeout, async {
            while body.len() < want {
                let n = s.read(&mut chunk).await.map_err(io)?;
                if n == 0 {
                    return Err(ProxyError::BadRequest("connection closed in body".into()));
                }
                body.extend_from_slice(&chunk[..n]);
            }
            Ok(())
        })
        .await
        .map_err(|_| timeout("body"))??;
    }
    body.truncate(want);

    Ok(ProxyRequest {
        role: role.to_string(),
        method,
        path: head.target.clone(),
        content_type: head.header("content-type").map(str::to_string),
        accept: head.header("accept").map(str::to_string),
        // With a bearer token the Authorization header was this listener's
        // credential: never pass it on.
        authorization: if bearer.is_some() {
            None
        } else {
            head.header("authorization").map(str::to_string)
        },
        body,
    })
}

/// Visible ASCII only, bounded: a value that came from a server or a peer
/// must not be able to end the header block.
pub fn safe_header_value(v: &str) -> Option<&str> {
    (!v.is_empty() && v.len() <= 200 && v.bytes().all(|b| (b' '..0x7f).contains(&b))).then_some(v)
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        413 => "Payload Too Large",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Status",
    }
}

/// Write a status line and headers; the body that follows is delimited by
/// closing the connection.
pub async fn write_head<S: AsyncWrite + Unpin>(
    s: &mut S,
    status: u16,
    content_type: Option<&str>,
) -> Result<(), ProxyError> {
    let ct = content_type
        .and_then(safe_header_value)
        .unwrap_or("application/octet-stream");
    let head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: {ct}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        reason(status)
    );
    s.write_all(head.as_bytes()).await.map_err(io)
}

/// A complete JSON error response.
pub async fn write_error<S: AsyncWrite + Unpin>(s: &mut S, e: &ProxyError) {
    let body = serde_json::json!({
        "error": {"message": e.to_string(), "type": "weftos_inference_proxy"}
    })
    .to_string();
    let challenge = if matches!(e, ProxyError::Unauthorized) {
        "WWW-Authenticate: Bearer\r\n"
    } else if matches!(e, ProxyError::Busy(_)) {
        "Retry-After: 5\r\n"
    } else {
        ""
    };
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\n{challenge}Content-Length: {}\r\nConnection: close\r\n\r\n",
        e.status(),
        reason(e.status()),
        body.len()
    );
    let _ = s.write_all(head.as_bytes()).await;
    let _ = s.write_all(body.as_bytes()).await;
    let _ = s.shutdown().await;
}
