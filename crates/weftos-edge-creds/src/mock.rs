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
    /// The user's attached policy; the effective permission is its
    /// intersection with `policy` (the inline session policy), as in STS.
    base: Option<Value>,
    expires_at: u64,
    user: String,
    policy: Value,
}

struct StaticUser {
    secret: String,
    policy: Value,
}

#[derive(Default)]
struct Inner {
    /// Long-lived users (the parent's per-node users and the admin).
    users: HashMap<String, StaticUser>,
    admins: Vec<String>,
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

    /// A long-lived user with an attached policy, as the parent provisions at pairing.
    pub fn add_user(&self, access: &str, secret: &str, policy_json: &str) {
        let policy = serde_json::from_str(policy_json).expect("policy json");
        self.inner.lock().unwrap().users.insert(access.into(), StaticUser { secret: secret.into(), policy });
    }

    pub fn add_admin(&self, access: &str, secret: &str) {
        self.add_user(access, secret, "{}");
        self.inner.lock().unwrap().admins.push(access.into());
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
        let (access, token, sig_given, canon, stamp, service) = if let Some(sig) =
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
            let service = cred.split('/').nth(3).unwrap_or("s3").to_string();
            (cred.split('/').next().unwrap_or("").to_string(), q("X-Amz-Security-Token"), sig, canon, stamp, service)
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
                cred.split('/').nth(3).unwrap_or("s3").to_string(),
            )
        };
        let (secret, want_token, expires_at, user) = if let Some(s) = inner.sessions.get(&access) {
            (s.secret.as_str(), Some(s.token.as_str()), s.expires_at, s.user.as_str())
        } else if let Some(u) = inner.users.get(&access) {
            (u.secret.as_str(), None, u64::MAX, access.as_str())
        } else {
            return Err("InvalidAccessKeyId");
        };
        if token.as_deref() != want_token {
            return Err("InvalidToken");
        }
        if now >= expires_at {
            return Err("ExpiredToken");
        }
        if inner.disabled.iter().any(|u| u == user) {
            return Err("AccessDenied: user disabled");
        }
        let expect = sigv4::sign_for(secret, &stamp, &self.region, &service, &canon);
        if !sigv4::sig_eq(&expect, &sig_given) {
            return Err("SignatureDoesNotMatch");
        }
        Ok(access)
    }
}

fn xml_error(status: u16, code: &str) -> HttpResponse {
    HttpResponse { status, headers: vec![], body: format!("<ErrorResponse><Error><Code>{code}</Code></Error></ErrorResponse>").into_bytes() }
}

fn rfc3339(unix: u64) -> String {
    let (d, s) = sigv4::amz_dates(unix);
    format!("{}-{}-{}T{}:{}:{}Z", &d[0..4], &d[4..6], &d[6..8], &s[9..11], &s[11..13], &s[13..15])
}

impl MockRustfs {
    /// STS `AssumeRole` as MinIO/RustFS serve it: caller is a long-lived user,
    /// duration at least 900 s, inline policy at most 2048 bytes.
    fn sts_endpoint(&self, inner: &mut Inner, req: &HttpRequest) -> HttpResponse {
        let caller = match self.authenticate(inner, req) {
            Ok(a) if inner.users.contains_key(&a) => a,
            _ => {
                inner.log.push((403, String::new(), req.method.clone(), "sts".into()));
                return xml_error(403, "SignatureDoesNotMatch");
            }
        };
        let form: HashMap<String, String> = String::from_utf8_lossy(&req.body)
            .split('&')
            .filter_map(|kv| kv.split_once('='))
            .map(|(k, v)| (pct_decode(k), pct_decode(v)))
            .collect();
        let dur: u64 = form.get("DurationSeconds").and_then(|d| d.parse().ok()).unwrap_or(3600);
        let policy = form.get("Policy").cloned().unwrap_or_default();
        let bad = form.get("Action").map(String::as_str) != Some("AssumeRole") || dur < 900 || policy.len() > 2048;
        let parsed = serde_json::from_str::<Value>(&policy);
        if bad || parsed.is_err() {
            inner.log.push((400, caller, "POST".into(), "sts".into()));
            return xml_error(400, "InvalidParameterValue");
        }
        inner.counter += 1;
        let n = inner.counter;
        let (access, secret) = (format!("ASIA{n:016}"), sigv4::sha256_hex(format!("secret-{n}").as_bytes()));
        let token = format!("tok-{}", sigv4::sha256_hex(format!("token-{n}").as_bytes()));
        let expires_at = self.clock.now() + dur;
        let base = inner.users[&caller].policy.clone();
        inner.sessions.insert(access.clone(), Session { secret: secret.clone(), token: token.clone(), base: Some(base), expires_at, user: caller.clone(), policy: parsed.unwrap() });
        inner.log.push((200, caller, "POST".into(), "sts".into()));
        let body = format!(
            "<AssumeRoleResponse><AssumeRoleResult><Credentials><AccessKeyId>{access}</AccessKeyId><SecretAccessKey>{secret}</SecretAccessKey><SessionToken>{token}</SessionToken><Expiration>{}</Expiration></Credentials></AssumeRoleResult></AssumeRoleResponse>",
            rfc3339(expires_at)
        );
        HttpResponse { status: 200, headers: vec![], body: body.into_bytes() }
    }

    fn admin_endpoint(&self, inner: &mut Inner, req: &HttpRequest) -> HttpResponse {
        let ok = self.authenticate(inner, req).ok().filter(|a| inner.admins.contains(a));
        let q = |n: &str| req.query.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone());
        let (Some(admin), "/rustfs/admin/v3/set-user-status", Some(user), Some(status)) = (ok, req.path.as_str(), q("accessKey"), q("status")) else {
            inner.log.push((403, String::new(), req.method.clone(), req.path.clone()));
            return xml_error(403, "AccessDenied");
        };
        if !inner.users.contains_key(&user) {
            return xml_error(404, "NoSuchUser");
        }
        match status.as_str() {
            "disabled" => {
                if !inner.disabled.contains(&user) {
                    inner.disabled.push(user);
                }
            }
            _ => inner.disabled.retain(|u| *u != user),
        }
        inner.log.push((200, admin, req.method.clone(), req.path.clone()));
        HttpResponse { status: 200, ..Default::default() }
    }
}

fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

impl ObjectTransport for MockRustfs {
    async fn send(&self, req: HttpRequest) -> Result<HttpResponse, TransportError> {
        let mut inner = self.inner.lock().unwrap();
        if req.path == "/" && req.method == "POST" {
            return Ok(self.sts_endpoint(&mut inner, &req));
        }
        if req.path.starts_with("/rustfs/admin/v3/") {
            return Ok(self.admin_endpoint(&mut inner, &req));
        }
        let mut parts = req.path.trim_start_matches('/').splitn(2, '/');
        let bucket = parts.next().unwrap_or("").to_string();
        let key = parts.next().unwrap_or("").to_string();
        let result = self.authenticate(&inner, &req).and_then(|ak| {
            let s = inner.sessions.get(&ak).ok_or("AccessDenied: static key not accepted for objects here")?;
            let arn = if key.is_empty() { format!("arn:aws:s3:::{bucket}") } else { format!("arn:aws:s3:::{bucket}/{key}") };
            let act = s3_action(&req.method, !key.is_empty());
            if allows(&s.policy, act, &arn) && s.base.as_ref().is_none_or(|b| allows(b, act, &arn)) { Ok(ak) } else { Err("AccessDenied: policy") }
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
                base: None,
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
