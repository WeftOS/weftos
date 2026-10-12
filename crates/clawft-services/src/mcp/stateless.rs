//! Stateless MCP (spec 2026-07-28) helpers shared by every transport.
//!
//! A *modern* request carries its protocol version in
//! `params._meta["io.modelcontextprotocol/protocolVersion"]` and needs no
//! `initialize` handshake. A *legacy* client opens with `initialize` and is
//! served by the 2025-06-18 path in [`super::server`]. The era is chosen per
//! message, so one shell can serve both and a modern request never reads or
//! writes the legacy `initialized` flag.

use serde_json::{Value, json};

/// Protocol revision implemented by the stateless path.
pub const MODERN_PROTOCOL_VERSION: &str = "2026-07-28";
/// Versions the stateless path accepts, newest first.
pub const MODERN_VERSIONS: &[&str] = &[MODERN_PROTOCOL_VERSION];

/// `_meta` key carrying the per-request protocol version.
pub const META_PROTOCOL_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
/// `_meta` key a server uses to identify itself in a result.
pub const META_SERVER_INFO: &str = "io.modelcontextprotocol/serverInfo";

/// Error code: request header does not match the body (spec: `HeaderMismatch`).
pub const HEADER_MISMATCH: i32 = -32020;
/// Error code: `UnsupportedProtocolVersion`.
pub const UNSUPPORTED_PROTOCOL_VERSION: i32 = -32022;

/// Freshness hint on cacheable results (tool lists change with hot-reload).
const TTL_MS: u64 = 0;

/// Protocol version a request declares in `_meta`, if any.
///
/// `Some` marks the request as modern.
pub fn declared_version(msg: &Value) -> Option<&str> {
    msg.pointer("/params/_meta")
        .and_then(|m| m.get(META_PROTOCOL_VERSION))
        .and_then(Value::as_str)
}

/// Whether `version` is served by the stateless path.
pub fn is_supported(version: &str) -> bool {
    MODERN_VERSIONS.contains(&version)
}

/// `UnsupportedProtocolVersionError` response body.
pub fn unsupported_version_error(id: Value, requested: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": UNSUPPORTED_PROTOCOL_VERSION,
            "message": "Unsupported protocol version",
            "data": { "supported": MODERN_VERSIONS, "requested": requested }
        }
    })
}

/// Whether a response is a modern protocol-level error that HTTP maps to 400.
pub fn is_modern_protocol_error(resp: &Value) -> bool {
    matches!(
        resp.pointer("/error/code").and_then(Value::as_i64),
        Some(c) if c == i64::from(UNSUPPORTED_PROTOCOL_VERSION) || c == i64::from(HEADER_MISMATCH)
    )
}

/// Server identity placed in a modern result's `_meta`.
pub fn server_info(name: &str, version: &str) -> Value {
    json!({ "name": name, "version": version })
}

/// Decorate a result for a modern request: `resultType`, `_meta.serverInfo`
/// and, for cacheable results, `ttlMs` / `cacheScope`.
pub fn decorate_result(result: &mut Value, info: Value, cacheable: bool) {
    let Some(obj) = result.as_object_mut() else {
        return;
    };
    obj.insert("resultType".into(), json!("complete"));
    if cacheable {
        obj.insert("ttlMs".into(), json!(TTL_MS));
        obj.insert("cacheScope".into(), json!("private"));
    }
    let meta = obj.entry("_meta").or_insert_with(|| json!({}));
    if let Some(m) = meta.as_object_mut() {
        m.insert(META_SERVER_INFO.into(), info);
    }
}

/// `server/discover` result.
pub fn discover_result(info: Value, instructions: Option<&str>) -> Value {
    let mut r = json!({
        "supportedVersions": MODERN_VERSIONS,
        "capabilities": { "tools": {} },
    });
    if let Some(i) = instructions {
        r["instructions"] = json!(i);
    }
    decorate_result(&mut r, info, true);
    r
}

/// Validate the Streamable HTTP mirror headers of a modern POST against its
/// body. `Err` carries the `HeaderMismatch` message (HTTP 400).
pub fn validate_http_headers(
    msg: &Value,
    protocol_version: Option<&str>,
    mcp_method: Option<&str>,
    mcp_name: Option<&str>,
) -> Result<(), String> {
    let declared = declared_version(msg).unwrap_or_default();
    match protocol_version {
        None => return Err("Header mismatch: MCP-Protocol-Version header missing".into()),
        Some(h) if h != declared => {
            return Err(format!(
                "Header mismatch: MCP-Protocol-Version header '{h}' does not match body '{declared}'"
            ));
        }
        Some(_) => {}
    }
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    match mcp_method {
        None => return Err("Header mismatch: Mcp-Method header missing".into()),
        Some(h) if h != method => {
            return Err(format!(
                "Header mismatch: Mcp-Method header '{h}' does not match body '{method}'"
            ));
        }
        Some(_) => {}
    }
    let body_name = match method {
        "tools/call" | "prompts/get" => msg.pointer("/params/name").and_then(Value::as_str),
        "resources/read" => msg.pointer("/params/uri").and_then(Value::as_str),
        _ => return Ok(()),
    };
    match (mcp_name, body_name) {
        (Some(h), Some(b)) if h == b => Ok(()),
        (Some(h), Some(b)) => Err(format!(
            "Header mismatch: Mcp-Name header '{h}' does not match body '{b}'"
        )),
        (None, _) => Err("Header mismatch: Mcp-Name header missing".into()),
        (Some(_), None) => Err("Header mismatch: request body has no name/uri".into()),
    }
}

/// `HeaderMismatch` JSON-RPC response body.
pub fn header_mismatch_error(id: Value, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": HEADER_MISMATCH, "message": message }
    })
}

/// Check a POSTed message's mirror headers when it is a modern request.
///
/// `header` looks a header up by lowercase name. Returns the `HeaderMismatch`
/// response body (send with HTTP 400) on failure; `None` for legacy messages
/// and for modern ones that validate.
pub fn check_http_request<'a>(
    msg: &Value,
    header: impl Fn(&str) -> Option<&'a str>,
) -> Option<Value> {
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    if method == "initialize" || declared_version(msg).is_none() {
        return None;
    }
    let id = msg.get("id").cloned().unwrap_or(Value::Null);
    validate_http_headers(
        msg,
        header("mcp-protocol-version"),
        header("mcp-method"),
        header("mcp-name"),
    )
    .err()
    .map(|m| header_mismatch_error(id, &m))
}
