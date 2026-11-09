//! The live [`StsMinter`]: RustFS STS `AssumeRole` and the admin
//! user-status call, over the host's HTTP client (card b76c74a7).
//!
//! - **Mint:** `POST /` form `Action=AssumeRole`, signed (SigV4, service
//!   `sts`) with the *node user's* own long-lived key. The inline `Policy`
//!   narrows the session; RustFS intersects it with the user's attached
//!   policy, so a session can never exceed what the node's user may do.
//!   S0 measured 15-minute sessions and a GET with them returning 200.
//! - **Disable / enable:** `PUT <admin prefix>/set-user-status` signed with an
//!   admin key. S0 measured the next request after `admin user disable`
//!   being refused about 0.22 s later, including a multipart upload in flight.
//!
//! Wire facts follow the MinIO API that RustFS mirrors and were checked against
//! a RustFS 1.0.4 container on loopback (2026-11-07): the AssumeRole form and
//! XML reply, `/rustfs/admin/v3/{add-canned-policy,add-user,
//! set-user-or-group-policy,set-user-status,remove-user,remove-canned-policy}`
//! with plain JSON bodies (the `/minio/admin/v3` prefix wants an encrypted
//! body), and bucket create. RustFS accepts `DurationSeconds` below 900.
//!
//! No sockets here: the host supplies the HTTP client as an
//! [`ObjectTransport`], same as the edge. Long-lived keys are held by the
//! parent only, behind [`NodeKeys`].

use zeroize::Zeroize;

use crate::broker::{StsError, StsMinter};
use crate::clock::Clock;
use crate::creds::{MAX_LEASE_TTL_SECS, TempCredentials};
use crate::policy::{Access, bucket_session_policy, node_bucket, principal_for};
use crate::sigv4::{self, HeaderSigner};
use crate::wire::{Endpoint, HttpRequest, ObjectTransport};

/// Default floor for DurationSeconds. MinIO and AWS refuse less; RustFS 1.0.4
/// accepted 60 s when measured (2026-11-07), so this is a config field.
pub const MIN_STS_DURATION_SECS: u64 = 900;
/// Inline session policy size limit.
pub const MAX_SESSION_POLICY_BYTES: usize = 2048;

/// A long-lived access key. Parent only; never serialized, redacted in Debug,
/// zeroed on drop.
#[derive(Clone)]
pub struct StaticKey {
    pub access_key: String,
    pub secret_key: String,
}

impl std::fmt::Debug for StaticKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "StaticKey({}, <redacted>)", self.access_key)
    }
}

impl Drop for StaticKey {
    fn drop(&mut self) {
        self.secret_key.zeroize();
    }
}

/// The parent's secret store: the RustFS user key behind each principal.
/// Created when a node is paired (ADR-126 section 3); the edge never sees it.
pub trait NodeKeys {
    fn key_for(&self, principal: &str) -> Option<StaticKey>;
}

#[derive(Debug, Clone)]
pub struct RustfsConfig {
    pub endpoint: Endpoint,
    pub region: String,
    /// Admin API prefix, e.g. `/rustfs/admin/v3`.
    pub admin_prefix: String,
    /// Shortest session to ask STS for. 900 is portable; 60 works on RustFS 1.0.4 and
    /// lets a presigned URL's session die with the URL.
    pub min_session_secs: u64,
}

impl RustfsConfig {
    pub fn new(endpoint: Endpoint) -> Self {
        Self { endpoint, region: "us-east-1".into(), admin_prefix: "/rustfs/admin/v3".into(), min_session_secs: MIN_STS_DURATION_SECS }
    }
}

pub struct RustfsMinter<H, K, C> {
    cfg: RustfsConfig,
    http: H,
    keys: K,
    admin: StaticKey,
    clock: C,
}

/// Query, extra headers and body of a request to sign.
#[derive(Default)]
struct Parts {
    query: Vec<(String, String)>,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl<H: ObjectTransport, K: NodeKeys, C: Clock> RustfsMinter<H, K, C> {
    pub fn new(cfg: RustfsConfig, http: H, keys: K, admin: StaticKey, clock: C) -> Self {
        Self { cfg, http, keys, admin, clock }
    }

    fn request(&self, key: &StaticKey, service: &str, method: &str, path: &str, parts: Parts) -> HttpRequest {
        let Parts { query, mut headers, body } = parts;
        headers.push(("host".into(), self.cfg.endpoint.host.clone()));
        let signer = HeaderSigner {
            access_key: &key.access_key,
            secret_key: &key.secret_key,
            session_token: None,
            region: &self.cfg.region,
            now: self.clock.now(),
        };
        let headers = signer.sign_service(service, method, path, &query, &headers, &body);
        HttpRequest {
            method: method.into(),
            scheme: self.cfg.endpoint.scheme.clone(),
            host: self.cfg.endpoint.host.clone(),
            path: path.into(),
            query,
            headers,
            body,
        }
    }

    /// Admin-signed call; `Ok(body)` on 2xx.
    async fn admin(&self, method: &str, op: &str, query: &[(&str, &str)], body: Vec<u8>) -> Result<String, StsError> {
        let path = format!("{}/{op}", self.cfg.admin_prefix);
        let query = query.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let headers = if body.is_empty() { vec![] } else { vec![("content-type".to_string(), "application/json".to_string())] };
        let req = self.request(&self.admin, sigv4::SERVICE, method, &path, Parts { query, headers, body });
        let r = self.http.send(req).await.map_err(|e| StsError(e.to_string()))?;
        if (200..300).contains(&r.status) {
            Ok(String::from_utf8_lossy(&r.body).into_owned())
        } else {
            Err(StsError(format!("{op}: HTTP {}", r.status)))
        }
    }

    /// Create the node's bucket (`n-<id>`) with the admin key. Idempotent.
    pub async fn create_bucket(&self, node_id: &str) -> Result<(), StsError> {
        let bucket = node_bucket(node_id).map_err(|e| StsError(e.to_string()))?;
        let req = self.request(&self.admin, sigv4::SERVICE, "PUT", &format!("/{bucket}"), Parts::default());
        let r = self.http.send(req).await.map_err(|e| StsError(e.to_string()))?;
        // 409 BucketAlreadyOwnedByYou is fine.
        if (200..300).contains(&r.status) || r.status == 409 { Ok(()) } else { Err(StsError(format!("create bucket: HTTP {}", r.status))) }
    }

    /// Pairing (ADR-126 section 3): the node's own policy (its bucket only),
    /// its RustFS user with `secret`, the policy attached, and the bucket.
    /// The secret goes in the parent's [`NodeKeys`] store, never to the edge.
    pub async fn provision_node(&self, node_id: &str, secret: &str) -> Result<(), StsError> {
        let bucket = node_bucket(node_id).map_err(|e| StsError(e.to_string()))?;
        let principal = principal_for(node_id);
        let policy = bucket_session_policy(&bucket, Access::Write);
        self.admin("PUT", "add-canned-policy", &[("name", &principal)], policy.into_bytes()).await?;
        let user = serde_json::json!({"secretKey": secret, "status": "enabled"}).to_string();
        self.admin("PUT", "add-user", &[("accessKey", &principal)], user.into_bytes()).await?;
        self.admin("PUT", "set-user-or-group-policy", &[("policyName", &principal), ("userOrGroup", &principal), ("isGroup", "false")], vec![]).await?;
        self.create_bucket(node_id).await
    }

    /// Unpairing: remove the user and its policy. The bucket is archived by
    /// the daemon before any delete (ADR-126 section 3), not here.
    pub async fn deprovision_node(&self, node_id: &str) -> Result<(), StsError> {
        let principal = principal_for(node_id);
        node_bucket(node_id).map_err(|e| StsError(e.to_string()))?;
        self.admin("DELETE", "remove-user", &[("accessKey", &principal)], vec![]).await?;
        self.admin("DELETE", "remove-canned-policy", &[("name", &principal)], vec![]).await?;
        Ok(())
    }

    async fn set_status(&self, principal: &str, status: &str) -> Result<(), StsError> {
        // The admin key signs; the target user is named by its principal, so no
        // lookup in the key store can make a disable fail open.
        let path = format!("{}/set-user-status", self.cfg.admin_prefix);
        let query = vec![("accessKey".to_string(), principal.to_string()), ("status".to_string(), status.to_string())];
        let req = self.request(&self.admin, sigv4::SERVICE, "PUT", &path, Parts { query, ..Default::default() });
        let r = self.http.send(req).await.map_err(|e| StsError(e.to_string()))?;
        if (200..300).contains(&r.status) { Ok(()) } else { Err(StsError(format!("set-user-status {status}: HTTP {}", r.status))) }
    }
}

/// First `<tag>text</tag>` in `xml`.
fn tag<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&format!("</{name}>"))? + start;
    Some(&xml[start..end])
}

impl<H: ObjectTransport, K: NodeKeys, C: Clock> StsMinter for RustfsMinter<H, K, C> {
    async fn assume_role(&self, principal: &str, policy: &str, ttl_secs: u64) -> Result<TempCredentials, StsError> {
        if policy.len() > MAX_SESSION_POLICY_BYTES {
            return Err(StsError(format!("session policy is {} bytes, limit {MAX_SESSION_POLICY_BYTES}", policy.len())));
        }
        let user = self.keys.key_for(principal).ok_or_else(|| StsError(format!("no RustFS user for {principal}")))?;
        let dur = ttl_secs.clamp(self.cfg.min_session_secs.min(MAX_LEASE_TTL_SECS), MAX_LEASE_TTL_SECS);
        let form = [("Action", "AssumeRole"), ("DurationSeconds", &dur.to_string()), ("Policy", policy), ("Version", "2011-06-15")]
            .iter()
            .map(|(k, v)| format!("{}={}", sigv4::uri_encode(k, false), sigv4::uri_encode(v, false)))
            .collect::<Vec<_>>()
            .join("&");
        let ct = vec![("content-type".to_string(), "application/x-www-form-urlencoded".to_string())];
        let req = self.request(&user, "sts", "POST", "/", Parts { headers: ct, body: form.into_bytes(), ..Default::default() });
        let r = self.http.send(req).await.map_err(|e| StsError(e.to_string()))?;
        let body = String::from_utf8_lossy(&r.body);
        if r.status != 200 {
            let code = tag(&body, "Code").unwrap_or("");
            return Err(StsError(format!("AssumeRole: HTTP {} {code}", r.status)));
        }
        let field = |n: &str| tag(&body, n).map(str::to_string).ok_or_else(|| StsError(format!("AssumeRole reply has no {n}")));
        let expires_at = sigv4::parse_rfc3339(&field("Expiration")?).ok_or_else(|| StsError("bad Expiration".into()))?;
        Ok(TempCredentials {
            access_key: field("AccessKeyId")?,
            secret_key: field("SecretAccessKey")?,
            session_token: field("SessionToken")?,
            expires_at,
        })
    }

    async fn disable_principal(&self, principal: &str) -> Result<(), StsError> {
        self.set_status(principal, "disabled").await
    }

    async fn enable_principal(&self, principal: &str) -> Result<(), StsError> {
        self.set_status(principal, "enabled").await
    }
}
