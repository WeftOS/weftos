//! In-memory RustFS stand-in for tests and for hosts without a live store.
//! It checks what RustFS checks for this path: SigV4 (header and query),
//! the session token, expiry, the user's enabled flag, and the session
//! policy. It is the same code path as S0's measured behaviour, not a
//! shortcut around it: a request with a bad signature, a stale session, a
//! disabled user or an out-of-scope bucket is refused with 403.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use serde_json::Value;

use crate::broker::{AuditEvent, AuditSink, GrantCheck, StsError, StsMinter};
use crate::clock::{Clock, ManualClock};
use crate::creds::TempCredentials;
use crate::policy::Access;
use crate::sigv4::{self, Canonical, UNSIGNED_PAYLOAD};
use crate::wire::{HttpRequest, HttpResponse, ObjectTransport, TransportError};

struct Session {
    secret: String,
    token: String,
    expires_at: u64,
    user: String,
    policy: Value,
}

#[derive(Default)]
struct Inner {
    disabled: Vec<String>,
    sessions: HashMap<String, Session>,
    objects: BTreeMap<(String, String), Vec<u8>>,
    counter: u64,
    /// `(status, access_key, method, path)` per request seen.
    log: Vec<(u16, String, String, String)>,
}

#[derive(Clone)]
pub struct MockRustfs {
    inner: Arc<Mutex<Inner>>,
    clock: ManualClock,
    region: String,
}

impl MockRustfs {
    pub fn new(clock: ManualClock) -> Self {
        Self { inner: Arc::default(), clock, region: "us-east-1".into() }
    }

    pub fn object(&self, bucket: &str, key: &str) -> Option<Vec<u8>> {
        self.inner.lock().unwrap().objects.get(&(bucket.into(), key.into())).cloned()
    }

    pub fn object_count(&self) -> usize {
        self.inner.lock().unwrap().objects.len()
    }

    pub fn log(&self) -> Vec<(u16, String, String, String)> {
        self.inner.lock().unwrap().log.clone()
    }

    /// Sessions issued so far: proves the store only ever saw STS access keys.
    pub fn session_keys(&self) -> Vec<String> {
        self.inner.lock().unwrap().sessions.keys().cloned().collect()
    }
}

fn allows(policy: &Value, action: &str, arn: &str) -> bool {
    let list = |v: &Value| -> Vec<String> {
        match v {
            Value::String(s) => vec![s.clone()],
            Value::Array(a) => a.iter().filter_map(|x| x.as_str().map(String::from)).collect(),
            _ => vec![],
        }
    };
    let m = |pat: &str, s: &str| match pat.strip_suffix('*') {
        Some(prefix) => s.starts_with(prefix),
        None => pat == s,
    };
    policy["Statement"].as_array().is_some_and(|st| {
        st.iter().any(|s| {
            s["Effect"] == "Allow"
                && list(&s["Action"]).iter().any(|a| m(a, action))
                && list(&s["Resource"]).iter().any(|r| m(r, arn))
        })
    })
}

fn s3_action(method: &str, has_key: bool) -> &'static str {
    match (method, has_key) {
        ("PUT", true) => "s3:PutObject",
        ("GET", true) => "s3:GetObject",
        ("DELETE", true) => "s3:DeleteObject",
        ("GET", false) => "s3:ListBucket",
        _ => "s3:Unsupported",
    }
}

impl MockRustfs {
    /// Authenticate: returns the access key whose session signed the request.
    fn authenticate(&self, inner: &Inner, req: &HttpRequest) -> Result<String, &'static str> {
        let now = self.clock.now();
        let (access, token, sig_given, canon, stamp) = if let Some(sig) =
            req.query.iter().find(|(k, _)| k == "X-Amz-Signature").map(|(_, v)| v.clone())
        {
            let q = |n: &str| req.query.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone());
            let cred = q("X-Amz-Credential").ok_or("missing credential")?;
            let stamp = q("X-Amz-Date").ok_or("missing date")?;
            let expires: u64 = q("X-Amz-Expires").and_then(|e| e.parse().ok()).ok_or("bad expires")?;
            let t0 = sigv4::parse_stamp(&stamp).ok_or("bad date")?;
            if now >= t0 + expires {
                return Err("ExpiredToken: presigned url expired");
            }
            let rest: Vec<(String, String)> = req.query.iter().filter(|(k, _)| k != "X-Amz-Signature").cloned().collect();
            let headers = [("host".to_string(), req.host.clone())];
            let (canon, _) = sigv4::canonical_request(&Canonical {
                method: &req.method,
                path: &req.path,
                query: &rest,
                headers: &headers,
                payload_hash: UNSIGNED_PAYLOAD,
            });
            (cred.split('/').next().unwrap_or("").to_string(), q("X-Amz-Security-Token"), sig, canon, stamp)
        } else {
            let auth = req.header("authorization").ok_or("missing authorization")?;
            let rest = auth.strip_prefix("AWS4-HMAC-SHA256 ").ok_or("bad algorithm")?;
            let field = |n: &str| {
                rest.split(", ").find_map(|p| p.strip_prefix(&format!("{n}=")).map(String::from))
            };
            let cred = field("Credential").ok_or("missing credential")?;
            let signed = field("SignedHeaders").ok_or("missing signed headers")?;
            let sig = field("Signature").ok_or("missing signature")?;
            let stamp = req.header("x-amz-date").ok_or("missing date")?.to_string();
            let t0 = sigv4::parse_stamp(&stamp).ok_or("bad date")?;
            if t0.abs_diff(now) > 900 {
                return Err("RequestTimeTooSkewed");
            }
            let names: Vec<&str> = signed.split(';').collect();
            if !names.contains(&"x-amz-security-token") && req.header("x-amz-security-token").is_some() {
                return Err("security token not signed");
            }
            let headers: Vec<(String, String)> = names
                .iter()
                .map(|n| (n.to_string(), req.header(n).unwrap_or("").to_string()))
                .collect();
            let payload = req.header("x-amz-content-sha256").ok_or("missing payload hash")?;
            if payload != sigv4::sha256_hex(&req.body) {
                return Err("XAmzContentSHA256Mismatch");
            }
            let (canon, _) = sigv4::canonical_request(&Canonical {
                method: &req.method,
                path: &req.path,
                query: &req.query,
                headers: &headers,
                payload_hash: payload,
            });
            (
                cred.split('/').next().unwrap_or("").to_string(),
                req.header("x-amz-security-token").map(String::from),
                sig,
                canon,
                stamp,
            )
        };
        let s = inner.sessions.get(&access).ok_or("InvalidAccessKeyId")?;
        if token.as_deref() != Some(s.token.as_str()) {
            return Err("InvalidToken");
        }
        if now >= s.expires_at {
            return Err("ExpiredToken");
        }
        if inner.disabled.contains(&s.user) {
            return Err("AccessDenied: user disabled");
        }
        let expect = sigv4::sign(&s.secret, &stamp, &self.region, &canon);
        if !sigv4::sig_eq(&expect, &sig_given) {
            return Err("SignatureDoesNotMatch");
        }
        Ok(access)
    }
}

impl ObjectTransport for MockRustfs {
    async fn send(&self, req: HttpRequest) -> Result<HttpResponse, TransportError> {
        let mut inner = self.inner.lock().unwrap();
        let mut parts = req.path.trim_start_matches('/').splitn(2, '/');
        let bucket = parts.next().unwrap_or("").to_string();
        let key = parts.next().unwrap_or("").to_string();
        let result = self.authenticate(&inner, &req).and_then(|ak| {
            let s = &inner.sessions[&ak];
            let arn = if key.is_empty() { format!("arn:aws:s3:::{bucket}") } else { format!("arn:aws:s3:::{bucket}/{key}") };
            if allows(&s.policy, s3_action(&req.method, !key.is_empty()), &arn) { Ok(ak) } else { Err("AccessDenied: policy") }
        });
        let (status, body, who) = match result {
            Err(_) => (403, vec![], String::new()),
            Ok(ak) => {
                let k = (bucket.clone(), key.clone());
                let (status, body) = match req.method.as_str() {
                    "PUT" if req.header("if-none-match") == Some("*") && inner.objects.contains_key(&k) => (412, vec![]),
                    "PUT" => {
                        inner.objects.insert(k, req.body.clone());
                        (200, vec![])
                    }
                    "GET" => match inner.objects.get(&k) {
                        Some(v) => (200, v.clone()),
                        None => (404, vec![]),
                    },
                    "DELETE" => {
                        inner.objects.remove(&k);
                        (204, vec![])
                    }
                    _ => (405, vec![]),
                };
                (status, body, ak)
            }
        };
        inner.log.push((status, who, req.method.clone(), req.path.clone()));
        Ok(HttpResponse { status, headers: vec![], body })
    }
}

impl StsMinter for MockRustfs {
    async fn assume_role(&self, principal: &str, policy: &str, ttl_secs: u64) -> Result<TempCredentials, StsError> {
        let mut inner = self.inner.lock().unwrap();
        if inner.disabled.iter().any(|u| u == principal) {
            return Err(StsError("user disabled".into()));
        }
        inner.counter += 1;
        let n = inner.counter;
        let access = format!("ASIA{n:016}");
        let secret = sigv4::sha256_hex(format!("secret-{n}").as_bytes());
        let token = format!("tok-{}", sigv4::sha256_hex(format!("token-{n}").as_bytes()));
        let expires_at = self.clock.now() + ttl_secs;
        inner.sessions.insert(
            access.clone(),
            Session {
                secret: secret.clone(),
                token: token.clone(),
                expires_at,
                user: principal.into(),
                policy: serde_json::from_str(policy).map_err(|e| StsError(e.to_string()))?,
            },
        );
        Ok(TempCredentials { access_key: access, secret_key: secret, session_token: token, expires_at })
    }

    async fn disable_principal(&self, principal: &str) -> Result<(), StsError> {
        let mut inner = self.inner.lock().unwrap();
        if !inner.disabled.iter().any(|u| u == principal) {
            inner.disabled.push(principal.into());
        }
        Ok(())
    }

    async fn enable_principal(&self, principal: &str) -> Result<(), StsError> {
        self.inner.lock().unwrap().disabled.retain(|u| u != principal);
        Ok(())
    }
}

/// Grants held in memory.
#[derive(Clone, Default)]
pub struct StaticGrants(Arc<Mutex<HashMap<String, Vec<Access>>>>);

impl StaticGrants {
    pub fn grant(&self, node: &str, access: &[Access]) {
        self.0.lock().unwrap().insert(node.into(), access.to_vec());
    }
}

impl GrantCheck for StaticGrants {
    fn allows(&self, node_id: &str, access: Access) -> bool {
        self.0.lock().unwrap().get(node_id).is_some_and(|a| a.contains(&access))
    }
}

/// Collects audit events.
#[derive(Clone, Default)]
pub struct VecAudit(Arc<Mutex<Vec<AuditEvent>>>);

impl VecAudit {
    pub fn events(&self) -> Vec<AuditEvent> {
        self.0.lock().unwrap().clone()
    }
}

impl AuditSink for VecAudit {
    fn record(&self, ev: AuditEvent) {
        self.0.lock().unwrap().push(ev);
    }
}
