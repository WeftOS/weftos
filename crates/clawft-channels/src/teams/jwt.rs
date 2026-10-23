//! Bot Framework inbound-token verification for the Teams webhook.
//!
//! Implements the connector-to-bot authentication steps from Microsoft's
//! Bot Framework spec:
//!
//! 1. Fetch the OpenID metadata document
//!    (`https://login.botframework.com/v1/.well-known/openidconfiguration`)
//!    and read its `jwks_uri`.
//! 2. Fetch the JWKS and cache it: refreshed after a TTL, and re-fetched
//!    (rate limited) when a token names an unknown `kid`, to follow key
//!    rotation without letting a flood of bogus `kid`s hammer the endpoint.
//! 3. Accept only `RS256`; require `iss == https://api.botframework.com`,
//!    `aud == <configured app id>`, a valid signature, and `exp` / `nbf`
//!    within a five-minute clock skew. Everything else is rejected.
//!
//! The HTTP fetch sits behind [`JwksSource`] so tests run without network.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use tokio::sync::Mutex;

/// Public OpenID metadata document for Bot Framework channel tokens.
pub const BOT_FRAMEWORK_OPENID_URL: &str =
    "https://login.botframework.com/v1/.well-known/openidconfiguration";
/// Required `iss` claim on tokens sent by the Bot Connector service.
pub const BOT_FRAMEWORK_ISSUER: &str = "https://api.botframework.com";
/// Maximum accepted clock skew for `exp` / `nbf`.
pub const MAX_CLOCK_SKEW_SECS: u64 = 300;
/// How long a fetched JWKS is trusted before a refresh is forced.
pub const DEFAULT_JWKS_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// Minimum gap between refreshes triggered by an unknown `kid`.
pub const DEFAULT_MIN_REFRESH: Duration = Duration::from_secs(60);

/// Source of the OpenID metadata and JWKS documents.
#[async_trait]
pub trait JwksSource: Send + Sync {
    /// GET `url` and return the parsed JSON body.
    async fn get_json(&self, url: &str) -> Result<serde_json::Value, String>;
}

/// Production [`JwksSource`] backed by `reqwest`.
pub struct HttpJwksSource {
    client: reqwest::Client,
}

impl HttpJwksSource {
    pub fn new(client: reqwest::Client) -> Self {
        Self { client }
    }
}

#[async_trait]
impl JwksSource for HttpJwksSource {
    async fn get_json(&self, url: &str) -> Result<serde_json::Value, String> {
        let resp = self
            .client
            .get(url)
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|e| format!("GET {url}: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("GET {url}: HTTP {}", resp.status()));
        }
        resp.json().await.map_err(|e| format!("GET {url}: bad JSON: {e}"))
    }
}

#[derive(Default)]
struct KeyCache {
    keys: HashMap<String, DecodingKey>,
    fetched_at: Option<Instant>,
}

/// Verifies Bot Framework bearer tokens against the published JWKS.
pub struct TeamsJwtVerifier {
    source: Box<dyn JwksSource>,
    metadata_url: String,
    app_id: String,
    ttl: Duration,
    min_refresh: Duration,
    cache: Mutex<KeyCache>,
}

impl TeamsJwtVerifier {
    /// Build a verifier for `app_id`. Refuses an empty app id: with no
    /// audience to compare against, every token would be accepted.
    pub fn new(app_id: &str, source: Box<dyn JwksSource>) -> Result<Self, String> {
        if app_id.trim().is_empty() {
            return Err("teams JWT verifier requires a non-empty app_id (audience)".into());
        }
        Ok(Self {
            source,
            metadata_url: BOT_FRAMEWORK_OPENID_URL.into(),
            app_id: app_id.into(),
            ttl: DEFAULT_JWKS_TTL,
            min_refresh: DEFAULT_MIN_REFRESH,
            cache: Mutex::new(KeyCache::default()),
        })
    }

    /// Override cache timing (tests).
    #[cfg(test)]
    pub(crate) fn with_timing(mut self, ttl: Duration, min_refresh: Duration) -> Self {
        self.ttl = ttl;
        self.min_refresh = min_refresh;
        self
    }

    /// Verify `token`; `Ok(())` only for a correctly signed, in-date
    /// RS256 token issued by Bot Framework for this app id.
    pub async fn verify(&self, token: &str) -> Result<(), String> {
        if token.is_empty() {
            return Err("missing bearer token".into());
        }
        let header = jsonwebtoken::decode_header(token)
            .map_err(|e| format!("teams: malformed JWT header: {e}"))?;
        if header.alg != Algorithm::RS256 {
            return Err(format!("teams: unsupported JWT alg {:?} (RS256 only)", header.alg));
        }
        let kid = header.kid.ok_or("teams: JWT header has no kid")?;
        let key = self.key_for(&kid).await?;

        let mut v = Validation::new(Algorithm::RS256);
        v.leeway = MAX_CLOCK_SKEW_SECS;
        v.validate_exp = true;
        v.validate_nbf = true;
        v.set_issuer(&[BOT_FRAMEWORK_ISSUER]);
        v.set_audience(&[self.app_id.as_str()]);
        v.set_required_spec_claims(&["exp", "iss", "aud"]);
        jsonwebtoken::decode::<serde_json::Value>(token, &key, &v)
            .map(|_| ())
            .map_err(|e| format!("teams: JWT rejected: {e}"))
    }

    async fn key_for(&self, kid: &str) -> Result<DecodingKey, String> {
        // The lock is held across the fetch so concurrent requests with an
        // unknown kid trigger one refresh, not a stampede.
        let mut cache = self.cache.lock().await;
        let age = cache.fetched_at.map(|t| t.elapsed());
        let fresh = age.is_some_and(|a| a < self.ttl);
        if fresh && let Some(k) = cache.keys.get(kid) {
            return Ok(k.clone());
        }
        // Stale cache, or fresh cache without this kid: refresh, but not
        // more often than `min_refresh` unless the cache has expired.
        let may_refresh = age.is_none_or(|a| a >= self.min_refresh || a >= self.ttl);
        if !may_refresh {
            return Err(format!("teams: unknown signing key id {kid:?}"));
        }
        let keys = self.fetch_keys().await?;
        cache.keys = keys;
        cache.fetched_at = Some(Instant::now());
        cache
            .keys
            .get(kid)
            .cloned()
            .ok_or_else(|| format!("teams: unknown signing key id {kid:?}"))
    }

    async fn fetch_keys(&self) -> Result<HashMap<String, DecodingKey>, String> {
        let meta = self.source.get_json(&self.metadata_url).await?;
        let jwks_uri = meta
            .get("jwks_uri")
            .and_then(|v| v.as_str())
            .ok_or("teams: OpenID metadata has no jwks_uri")?;
        if !jwks_uri.starts_with("https://") {
            return Err(format!("teams: refusing non-https jwks_uri {jwks_uri:?}"));
        }
        let jwks = self.source.get_json(jwks_uri).await?;
        parse_jwks(&jwks)
    }
}

#[derive(Deserialize)]
struct RsaJwk {
    #[serde(default)]
    kty: String,
    kid: Option<String>,
    n: Option<String>,
    e: Option<String>,
}

/// Parse an RSA JWKS into a `kid -> key` map; non-RSA keys and keys
/// without a `kid` are skipped. An empty result is an error, so a broken
/// document can never be mistaken for "no keys needed".
fn parse_jwks(doc: &serde_json::Value) -> Result<HashMap<String, DecodingKey>, String> {
    let list = doc
        .get("keys")
        .and_then(|k| k.as_array())
        .ok_or("teams: JWKS has no keys array")?;
    let mut out = HashMap::new();
    for item in list {
        let Ok(jwk) = serde_json::from_value::<RsaJwk>(item.clone()) else {
            continue;
        };
        if jwk.kty != "RSA" {
            continue;
        }
        if let (Some(kid), Some(n), Some(e)) = (jwk.kid, jwk.n, jwk.e)
            && let Ok(key) = DecodingKey::from_rsa_components(&n, &e)
        {
            out.insert(kid, key);
        }
    }
    if out.is_empty() {
        return Err("teams: JWKS contained no usable RSA keys".into());
    }
    Ok(out)
}
