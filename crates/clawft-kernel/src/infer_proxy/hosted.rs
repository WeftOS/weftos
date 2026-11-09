//! A role served by a hosted, OpenAI-compatible API (`mode: "remote"`).
//!
//! The destination is the role's configured `base_url`, never anything a
//! request names; the request path is allowlisted and joined onto it. The
//! credential is held as an environment variable *name* only: the value is
//! read when a request is forwarded, put in one `Authorization` header
//! marked sensitive, and appears in no error, log, audit record or `Debug`.

use std::time::Duration;

use reqwest::header::HeaderValue;

use super::types::ProxyError;

/// Where a role's hosted API lives and how it is called.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostedRoute {
    /// API root including the version (`https://api.x.ai/v1`); request
    /// `/v1/<rest>` goes to `<base_url>/<rest>`.
    pub base_url: String,
    /// Name of the environment variable that holds the API key.
    pub api_key_env: String,
    /// Model every request is pinned to.
    pub model: String,
}

/// Per-role behavior the listener applies to a forwarded request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RolePolicy {
    /// Longest silence from the upstream, replacing the listener default.
    pub stall: Option<Duration>,
    /// Rewrite the body's top-level `model` to this before forwarding.
    pub pin_model: Option<String>,
}

/// Paths a hosted role forwards: chat and the model list, nothing else.
fn route_of(path: &str) -> Option<&'static str> {
    match path.split('?').next().unwrap_or("") {
        "/v1/chat/completions" => Some("/chat/completions"),
        "/v1/models" => Some("/models"),
        _ => None,
    }
}

/// `https://host[:port][/path]`, or `http://` with a loopback IP literal
/// (tests, a local relay). No userinfo, query or fragment. Returns the base
/// without a trailing slash.
pub fn parse_hosted_base(base: &str) -> Result<reqwest::Url, String> {
    let url = reqwest::Url::parse(base).map_err(|_| "base_url is not a URL".to_string())?;
    if !url.username().is_empty() || url.password().is_some() || url.query().is_some() || url.fragment().is_some() {
        return Err("base_url has no credentials, query or fragment".into());
    }
    match url.scheme() {
        "https" if url.host_str().is_some() => Ok(url),
        "http" if loopback_literal(url.host_str().unwrap_or("")) => Ok(url),
        _ => Err("base_url must be https (http only for 127.0.0.1 or [::1])".into()),
    }
}

/// `127.0.0.1` or `[::1]` style literal (a name could be remapped).
fn loopback_literal(host: &str) -> bool {
    host.trim_start_matches('[').trim_end_matches(']').parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// Environment variable names a config may reference.
pub fn valid_env_name(name: &str) -> bool {
    let mut c = name.chars();
    name.len() <= 128
        && c.next().is_some_and(|f| f.is_ascii_alphabetic() || f == '_')
        && c.all(|x| x.is_ascii_alphanumeric() || x == '_')
}

impl HostedRoute {
    /// The URL a request path maps to: the configured base plus the
    /// allowlisted route. The query is dropped.
    pub fn url_for(&self, path: &str) -> Result<reqwest::Url, ProxyError> {
        let rest = route_of(path).ok_or_else(|| ProxyError::Forbidden("path is not served by a remote role".into()))?;
        let base = parse_hosted_base(&self.base_url).map_err(ProxyError::Forbidden)?;
        let url = reqwest::Url::parse(&format!("{}{rest}", base.as_str().trim_end_matches('/')))
            .map_err(|e| ProxyError::BadRequest(e.to_string()))?;
        if url.host_str() != base.host_str() || url.port() != base.port() || url.scheme() != base.scheme() {
            return Err(ProxyError::Forbidden("destination changed".into()));
        }
        Ok(url)
    }

    /// `Bearer <key>`, read from the environment now. A missing, empty or
    /// unusable value fails naming the variable only.
    pub fn bearer(&self) -> Result<HeaderValue, ProxyError> {
        let name = &self.api_key_env;
        let key = std::env::var(name).unwrap_or_default();
        let key = key.trim();
        if key.is_empty() {
            return Err(ProxyError::Upstream(format!("remote role key: environment variable {name} is not set or empty")));
        }
        let mut v = HeaderValue::from_str(&format!("Bearer {key}"))
            .map_err(|_| ProxyError::Upstream(format!("remote role key: environment variable {name} does not hold a valid key")))?;
        v.set_sensitive(true);
        Ok(v)
    }

    /// Whether the key variable currently holds something (status).
    pub fn key_present(&self) -> bool {
        std::env::var(&self.api_key_env).is_ok_and(|v| !v.trim().is_empty())
    }
}

/// Set the top-level `model` of a JSON object body to `model`. An empty body
/// stays empty; anything else that is not a JSON object is refused.
pub fn pin_model(body: &[u8], model: &str) -> Result<Vec<u8>, ProxyError> {
    if body.is_empty() {
        return Ok(Vec::new());
    }
    let bad = || ProxyError::BadRequest("request body must be a JSON object".into());
    let mut v: serde_json::Value = serde_json::from_slice(body).map_err(|_| bad())?;
    v.as_object_mut().ok_or_else(bad)?.insert("model".into(), serde_json::Value::String(model.to_string()));
    serde_json::to_vec(&v).map_err(|_| bad())
}
