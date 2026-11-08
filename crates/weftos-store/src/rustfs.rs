//! The RustFS backend (ADR-126 sections 3 and 7): the S3 data plane and the admin API behind
//! [`ObjectBackend`] and [`AdminBackend`]. Blocking HTTP, SigV4-signed with the root credential.
//! Call it from a blocking context (the daemon uses `spawn_blocking`), never from an async task.
//!
//! Behaviour checked against RustFS 1.0.4: the admin API takes plain JSON at
//! `/rustfs/admin/v3/*`, an empty-statement policy is refused (so "no grants" becomes an explicit
//! deny-all), creating an existing bucket succeeds for the root owner, a conditional put on an
//! existing key is a 412, and a disabled user's next request is a 403.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::blocking::Client;
use serde_json::{Value, json};

use crate::backend::{AccessKey, AdminBackend, BackendError, BucketTags, LockedPut, ObjectBackend, ObjectMeta, PutOpts, check_key};
use crate::sigv4::{self, Req};

/// Connection settings. `secrets_dir` receives one 0600 file per issued access key.
#[derive(Debug, Clone)]
pub struct RustfsConfig {
    /// `http://host:port` or `https://host:port`, no path.
    pub endpoint: String,
    pub region: String,
    pub root_access_key: String,
    pub root_secret_key: String,
    pub secrets_dir: PathBuf,
    pub timeout: Duration,
}

impl RustfsConfig {
    /// Read `MESH_S3_ENDPOINT`, `MESH_S3_ACCESS_KEY`, `MESH_S3_SECRET_KEY` and optional
    /// `MESH_S3_REGION` from an env file (`KEY=value` lines, `#` comments).
    pub fn from_env_file(path: &Path, secrets_dir: PathBuf) -> Result<Self, BackendError> {
        let text = std::fs::read_to_string(path).map_err(|e| BackendError::Io(format!("{}: {e}", path.display())))?;
        let get = |k: &str| -> Option<String> {
            text.lines().filter_map(|l| l.trim().strip_prefix(k)?.strip_prefix('=')).next_back().map(|v| v.trim().trim_matches(['"', '\'']).to_owned())
        };
        let need = |k: &str| get(k).filter(|v| !v.is_empty()).ok_or_else(|| BackendError::Io(format!("{}: {k} missing", path.display())));
        Ok(Self {
            endpoint: need("MESH_S3_ENDPOINT")?,
            region: get("MESH_S3_REGION").filter(|v| !v.is_empty()).unwrap_or_else(|| "us-east-1".into()),
            root_access_key: need("MESH_S3_ACCESS_KEY")?,
            root_secret_key: need("MESH_S3_SECRET_KEY")?,
            secrets_dir,
            timeout: Duration::from_secs(30),
        })
    }
}

/// The RustFS backend.
pub struct RustfsBackend {
    cfg: RustfsConfig,
    base: String,
    host: String,
    http: Client,
}

struct Resp {
    status: u16,
    etag: Option<String>,
    len: Option<u64>,
    /// `x-amz-version-id`, set by versioned (Object Lock) buckets.
    version: Option<String>,
    body: Vec<u8>,
}

/// What a 404 means for the call that got it.
#[derive(Clone, Copy)]
enum Ctx<'a> {
    Bucket(&'a str),
    /// (bucket, key)
    Object(&'a str, &'a str),
    Other,
}

fn io(e: impl std::fmt::Display) -> BackendError {
    BackendError::Io(e.to_string())
}

/// The text between `<tag>` and `</tag>`, unescaped.
fn xml_tag(s: &str, tag: &str) -> Option<String> {
    let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
    let a = s.find(&open)? + open.len();
    let b = s[a..].find(&close)? + a;
    Some(unescape(&s[a..b]))
}

fn xml_blocks<'a>(s: &'a str, tag: &str) -> Vec<&'a str> {
    let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
    let mut v = Vec::new();
    let mut at = 0;
    while let Some(a) = s[at..].find(&open) {
        let a = at + a + open.len();
        let Some(b) = s[a..].find(&close) else { break };
        v.push(&s[a..a + b]);
        at = a + b + close.len();
    }
    v
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

impl RustfsBackend {
    pub fn new(cfg: RustfsConfig) -> Result<Self, BackendError> {
        let base = cfg.endpoint.trim_end_matches('/').to_owned();
        let (scheme, rest) = base.split_once("://").ok_or_else(|| io(format!("endpoint {base:?}: need http:// or https://")))?;
        let auth = rest.split('/').next().unwrap_or(rest);
        let host = match (scheme, auth.rsplit_once(':')) {
            ("http", Some((h, "80"))) | ("https", Some((h, "443"))) => h.to_owned(),
            _ => auth.to_owned(),
        };
        let http = Client::builder().timeout(cfg.timeout).build().map_err(io)?;
        std::fs::create_dir_all(&cfg.secrets_dir).map_err(io)?;
        Ok(Self { cfg, base: format!("{scheme}://{auth}"), host, http })
    }

    /// The access-key id issued for a principal label. Stable, so a restart finds the same key.
    pub fn key_id_for(principal: &str) -> String {
        format!("wk-{}", &blake3::hash(principal.as_bytes()).to_hex()[..16])
    }

    /// Where the secret of an issued access key is kept (0600).
    pub fn secret_path(&self, key_id: &str) -> PathBuf {
        self.cfg.secrets_dir.join(key_id)
    }

    fn send(&self, method: &str, path: &str, query: &[(String, String)], body: &[u8], extra: &[(&str, String)]) -> Result<Resp, BackendError> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_err(io)?.as_secs();
        let date = sigv4::amz_date(now);
        let sha = sigv4::sha256_hex(body);
        let extra_owned: Vec<(String, String)> = extra.iter().map(|(k, v)| ((*k).to_owned(), v.clone())).collect();
        let auth = sigv4::authorization(
            &Req { method, path, query, host: &self.host, payload_sha256: &sha, extra: &extra_owned },
            &self.cfg.root_access_key,
            &self.cfg.root_secret_key,
            &self.cfg.region,
            &date,
        );
        let mut url = format!("{}{}", self.base, sigv4::encode(path, true));
        if !query.is_empty() {
            url.push('?');
            url.push_str(&sigv4::canonical_query(query));
        }
        let m = reqwest::Method::from_bytes(method.as_bytes()).map_err(io)?;
        let mut rb = self.http.request(m, url).header("x-amz-date", date).header("x-amz-content-sha256", sha).header("authorization", auth);
        for (k, v) in extra {
            rb = rb.header(*k, v);
        }
        if !body.is_empty() {
            rb = rb.body(body.to_vec());
        }
        let r = rb.send().map_err(io)?;
        let status = r.status().as_u16();
        let etag = r.headers().get("etag").and_then(|v| v.to_str().ok()).map(|v| v.trim_matches('"').to_owned());
        let len = r.headers().get("content-length").and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok());
        let version = r.headers().get("x-amz-version-id").and_then(|v| v.to_str().ok()).map(str::to_owned);
        let body = r.bytes().map_err(io)?.to_vec();
        Ok(Resp { status, etag, len, version, body })
    }

    /// Turn a non-2xx response into the matching [`BackendError`].
    fn check(&self, r: Resp, ctx: Ctx) -> Result<Resp, BackendError> {
        if (200..300).contains(&r.status) {
            return Ok(r);
        }
        let text = String::from_utf8_lossy(&r.body);
        let code = xml_tag(&text, "Code").unwrap_or_default();
        let msg = xml_tag(&text, "Message").unwrap_or_default();
        Err(match (r.status, code.as_str(), ctx) {
            (403, _, _) => BackendError::Forbidden(format!("{code} {msg}").trim().to_owned()),
            (412, _, Ctx::Object(_, k)) => BackendError::Exists(k.to_owned()),
            (404, "NoSuchBucket", Ctx::Bucket(b) | Ctx::Object(b, _)) => BackendError::NoBucket(b.to_owned()),
            (404, _, Ctx::Object(_, k)) => BackendError::NoObject(k.to_owned()),
            (404, _, Ctx::Bucket(b)) => BackendError::NoBucket(b.to_owned()),
            (409, "BucketAlreadyOwnedByYou" | "BucketAlreadyExists", Ctx::Bucket(b)) => BackendError::BucketExists(b.to_owned()),
            _ => BackendError::Io(format!("{} {code}: {msg}", r.status)),
        })
    }

    fn q(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()
    }

    fn admin(&self, method: &str, verb: &str, query: &[(&str, &str)], body: &Value) -> Result<Resp, BackendError> {
        let body = if body.is_null() { Vec::new() } else { serde_json::to_vec(body).map_err(io)? };
        self.send(method, &format!("/rustfs/admin/v3/{verb}"), &Self::q(query), &body, &[])
    }

    fn user_status(&self, key_id: &str) -> Result<Option<String>, BackendError> {
        let r = self.admin("GET", "user-info", &[("accessKey", key_id)], &Value::Null)?;
        if r.status == 404 {
            return Ok(None);
        }
        let r = self.check(r, Ctx::Other)?;
        let v: Value = serde_json::from_slice(&r.body).map_err(io)?;
        Ok(Some(v["status"].as_str().unwrap_or("enabled").to_owned()))
    }

    fn new_secret() -> Result<String, BackendError> {
        use std::io::Read;
        let mut b = [0u8; 24];
        std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).map_err(io)?;
        Ok(sigv4::hex(&b))
    }

    fn write_secret(path: &Path, secret: &str) -> Result<(), BackendError> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let tmp = path.with_extension("tmp");
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp).map_err(io)?;
        f.write_all(secret.as_bytes()).map_err(io)?;
        f.sync_all().map_err(io)?;
        std::fs::rename(&tmp, path).map_err(io)
    }
}

/// The `NotificationConfiguration` body that sends a bucket's object-created and -removed
/// events to the webhook target `target` (the `<NAME>` of `RUSTFS_NOTIFY_WEBHOOK_*_<NAME>`).
pub fn notification_xml(target: &str) -> String {
    format!(
        "<NotificationConfiguration><QueueConfiguration><Id>weftos-activity</Id><Queue>arn:rustfs:sqs::{target}:webhook</Queue>\
<Event>s3:ObjectCreated:*</Event><Event>s3:ObjectRemoved:*</Event></QueueConfiguration></NotificationConfiguration>"
    )
}

impl RustfsBackend {
    /// `PutBucketNotification`: route the bucket's events to the webhook target `target`. The
    /// target must already exist (set from the environment when RustFS starts); without it
    /// RustFS answers 500. See docs/guides/rustfs-audit-deploy.md.
    pub fn put_bucket_notification(&self, bucket: &str, target: &str) -> Result<(), BackendError> {
        if target.is_empty() || !target.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
            return Err(BackendError::BadKey(format!("notification target {target:?}: letters, digits, '_' and '-' only")));
        }
        let r = self.send("PUT", &format!("/{bucket}"), &Self::q(&[("notification", "")]), notification_xml(target).as_bytes(), &[])?;
        self.check(r, Ctx::Bucket(bucket)).map(|_| ())
    }
}

impl ObjectBackend for RustfsBackend {
    fn put(&self, bucket: &str, key: &str, data: &[u8], if_none_match: bool) -> Result<ObjectMeta, BackendError> {
        check_key(key)?;
        let extra: Vec<(&str, String)> = if if_none_match { vec![("if-none-match", "*".into())] } else { Vec::new() };
        let r = self.send("PUT", &format!("/{bucket}/{key}"), &[], data, &extra)?;
        let r = self.check(r, Ctx::Object(bucket, key))?;
        Ok(ObjectMeta { key: key.to_owned(), size: data.len() as u64, etag: r.etag.unwrap_or_default() })
    }

    fn put_if_match(&self, bucket: &str, key: &str, data: &[u8], etag: &str) -> Result<ObjectMeta, BackendError> {
        check_key(key)?;
        // An ETag header value is quoted; accept the bare value the caller got from `ObjectMeta`.
        let q = if etag.starts_with('"') { etag.to_owned() } else { format!("\"{etag}\"") };
        let r = self.send("PUT", &format!("/{bucket}/{key}"), &[], data, &[("if-match", q)])?;
        let r = self.check(r, Ctx::Object(bucket, key)).map_err(|e| match e {
            BackendError::Exists(k) => BackendError::Stale(k),
            e => e,
        })?;
        Ok(ObjectMeta { key: key.to_owned(), size: data.len() as u64, etag: r.etag.unwrap_or_default() })
    }

    fn put_locked(&self, bucket: &str, key: &str, data: &[u8], opts: &PutOpts) -> Result<LockedPut, BackendError> {
        check_key(key)?;
        let mut extra: Vec<(&str, String)> = Vec::new();
        if opts.if_none_match {
            extra.push(("if-none-match", "*".into()));
        }
        if let Some(r) = opts.retention {
            let now = SystemTime::now().duration_since(UNIX_EPOCH).map_err(io)?.as_secs();
            if r.until_unix_s <= now {
                return Err(BackendError::BadKey(format!("retain-until {} is not in the future", r.until_unix_s)));
            }
            extra.push(("x-amz-object-lock-mode", r.mode.as_str().into()));
            extra.push(("x-amz-object-lock-retain-until-date", sigv4::iso_date(r.until_unix_s)));
        }
        if opts.legal_hold {
            extra.push(("x-amz-object-lock-legal-hold", "ON".into()));
        }
        let r = self.send("PUT", &format!("/{bucket}/{key}"), &[], data, &extra)?;
        let r = self.check(r, Ctx::Object(bucket, key))?;
        Ok(LockedPut { meta: ObjectMeta { key: key.to_owned(), size: data.len() as u64, etag: r.etag.unwrap_or_default() }, version_id: r.version })
    }

    fn delete_version(&self, bucket: &str, key: &str, version_id: &str, bypass_governance: bool) -> Result<(), BackendError> {
        check_key(key)?;
        let extra: Vec<(&str, String)> = if bypass_governance { vec![("x-amz-bypass-governance-retention", "true".into())] } else { Vec::new() };
        let r = self.send("DELETE", &format!("/{bucket}/{key}"), &Self::q(&[("versionId", version_id)]), &[], &extra)?;
        self.check(r, Ctx::Object(bucket, key)).map(|_| ())
    }

    fn put_legal_hold(&self, bucket: &str, key: &str, version_id: &str, on: bool) -> Result<(), BackendError> {
        check_key(key)?;
        let body = format!("<LegalHold><Status>{}</Status></LegalHold>", if on { "ON" } else { "OFF" });
        let r = self.send("PUT", &format!("/{bucket}/{key}"), &Self::q(&[("legal-hold", ""), ("versionId", version_id)]), body.as_bytes(), &[])?;
        self.check(r, Ctx::Object(bucket, key)).map(|_| ())
    }

    fn get(&self, bucket: &str, key: &str) -> Result<Vec<u8>, BackendError> {
        check_key(key)?;
        let r = self.send("GET", &format!("/{bucket}/{key}"), &[], &[], &[])?;
        Ok(self.check(r, Ctx::Object(bucket, key))?.body)
    }

    fn head(&self, bucket: &str, key: &str) -> Result<ObjectMeta, BackendError> {
        check_key(key)?;
        let r = self.send("HEAD", &format!("/{bucket}/{key}"), &[], &[], &[])?;
        let r = self.check(r, Ctx::Object(bucket, key))?;
        Ok(ObjectMeta { key: key.to_owned(), size: r.len.unwrap_or(0), etag: r.etag.unwrap_or_default() })
    }

    fn delete(&self, bucket: &str, key: &str) -> Result<(), BackendError> {
        check_key(key)?;
        let r = self.send("DELETE", &format!("/{bucket}/{key}"), &[], &[], &[])?;
        self.check(r, Ctx::Object(bucket, key)).map(|_| ())
    }

    fn copy(&self, src_bucket: &str, src_key: &str, dst_bucket: &str, dst_key: &str) -> Result<ObjectMeta, BackendError> {
        check_key(src_key)?;
        check_key(dst_key)?;
        let src = format!("/{src_bucket}/{}", sigv4::encode(src_key, true));
        let r = self.send("PUT", &format!("/{dst_bucket}/{dst_key}"), &[], &[], &[("x-amz-copy-source", src)])?;
        self.check(r, Ctx::Object(src_bucket, src_key))?;
        self.head(dst_bucket, dst_key)
    }

    fn list(&self, bucket: &str, prefix: &str) -> Result<Vec<ObjectMeta>, BackendError> {
        let mut out = Vec::new();
        let mut token = String::new();
        loop {
            let mut q = vec![("list-type", "2"), ("prefix", prefix)];
            if !token.is_empty() {
                q.push(("continuation-token", &token));
            }
            let r = self.send("GET", &format!("/{bucket}"), &Self::q(&q), &[], &[])?;
            let r = self.check(r, Ctx::Bucket(bucket))?;
            let text = String::from_utf8_lossy(&r.body).into_owned();
            for c in xml_blocks(&text, "Contents") {
                out.push(ObjectMeta {
                    key: xml_tag(c, "Key").unwrap_or_default(),
                    size: xml_tag(c, "Size").and_then(|s| s.parse().ok()).unwrap_or(0),
                    etag: xml_tag(c, "ETag").unwrap_or_default().trim_matches('"').to_owned(),
                });
            }
            match (xml_tag(&text, "IsTruncated").as_deref(), xml_tag(&text, "NextContinuationToken")) {
                (Some("true"), Some(t)) if !t.is_empty() => token = t,
                _ => break,
            }
        }
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
    }
}

impl RustfsBackend {
    fn create_bucket_with(&self, bucket: &str, tags: &[(String, String)], locked: bool) -> Result<(), BackendError> {
        // RustFS answers 200 for a bucket the root already owns; an explicit check keeps the
        // contract ("already exists" is an error) the same as the other backends.
        if self.bucket_exists(bucket)? {
            return Err(BackendError::BucketExists(bucket.to_owned()));
        }
        let lock_hdr: &[(&str, String)] = if locked { &[("x-amz-bucket-object-lock-enabled", "true".into())] } else { &[] };
        let r = self.send("PUT", &format!("/{bucket}"), &[], &[], lock_hdr)?;
        self.check(r, Ctx::Bucket(bucket))?;
        if !tags.is_empty() {
            let set: String = tags.iter().map(|(k, v)| format!("<Tag><Key>{}</Key><Value>{}</Value></Tag>", escape(k), escape(v))).collect();
            let body = format!("<Tagging><TagSet>{set}</TagSet></Tagging>");
            let r = self.send("PUT", &format!("/{bucket}"), &Self::q(&[("tagging", "")]), body.as_bytes(), &[]);
            if let Err(e) = r.and_then(|r| self.check(r, Ctx::Bucket(bucket))) {
                let _ = self.delete_bucket(bucket);
                return Err(e);
            }
        }
        Ok(())
    }
}

impl AdminBackend for RustfsBackend {
    fn create_locked_bucket(&self, bucket: &str, tags: &[(String, String)]) -> Result<(), BackendError> {
        self.create_bucket_with(bucket, tags, true)
    }

    fn create_bucket(&self, bucket: &str, tags: &[(String, String)]) -> Result<(), BackendError> {
        self.create_bucket_with(bucket, tags, false)
    }

    fn delete_bucket(&self, bucket: &str) -> Result<(), BackendError> {
        let r = self.send("DELETE", &format!("/{bucket}"), &[], &[], &[])?;
        self.check(r, Ctx::Bucket(bucket)).map(|_| ())
    }

    fn bucket_exists(&self, bucket: &str) -> Result<bool, BackendError> {
        let r = self.send("HEAD", &format!("/{bucket}"), &[], &[], &[])?;
        match r.status {
            404 => Ok(false),
            _ => self.check(r, Ctx::Bucket(bucket)).map(|_| true),
        }
    }

    fn buckets(&self) -> Result<Vec<BucketTags>, BackendError> {
        let r = self.send("GET", "/", &[], &[], &[])?;
        let r = self.check(r, Ctx::Other)?;
        let text = String::from_utf8_lossy(&r.body).into_owned();
        let mut out = Vec::new();
        for b in xml_blocks(&text, "Bucket") {
            let Some(name) = xml_tag(b, "Name") else { continue };
            let t = self.send("GET", &format!("/{name}"), &Self::q(&[("tagging", "")]), &[], &[])?;
            let tags = if t.status == 404 {
                Vec::new()
            } else {
                let t = self.check(t, Ctx::Bucket(&name))?;
                let tt = String::from_utf8_lossy(&t.body).into_owned();
                xml_blocks(&tt, "Tag").into_iter().filter_map(|x| Some((xml_tag(x, "Key")?, xml_tag(x, "Value").unwrap_or_default()))).collect()
            };
            out.push((name, tags));
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    fn create_access_key(&self, principal: &str) -> Result<AccessKey, BackendError> {
        let id = Self::key_id_for(principal);
        let path = self.secret_path(&id);
        let stored = std::fs::read_to_string(&path).ok().filter(|s| !s.is_empty());
        let status = self.user_status(&id)?;
        if status.is_some() && stored.is_some() {
            return Ok(AccessKey { id });
        }
        // Missing on the server, or the secret was lost: issue a fresh secret and keep the
        // key's enabled/disabled state, so re-creating a suspended principal's key never
        // re-enables it.
        let secret = match stored {
            Some(s) => s,
            None => {
                let s = Self::new_secret()?;
                Self::write_secret(&path, &s)?;
                s
            }
        };
        let st = status.unwrap_or_else(|| "enabled".into());
        let r = self.admin("PUT", "add-user", &[("accessKey", &id)], &json!({"secretKey": secret, "status": st}))?;
        self.check(r, Ctx::Other)?;
        Ok(AccessKey { id })
    }

    fn set_key_enabled(&self, key_id: &str, enabled: bool) -> Result<(), BackendError> {
        let st = if enabled { "enabled" } else { "disabled" };
        let r = self.admin("PUT", "set-user-status", &[("accessKey", key_id), ("status", st)], &Value::Null)?;
        self.check(r, Ctx::Other).map(|_| ())
    }

    fn put_policy(&self, key_id: &str, policy: &Value) -> Result<(), BackendError> {
        let name = format!("p-{key_id}");
        let empty = policy["Statement"].as_array().is_none_or(Vec::is_empty);
        let body = if empty {
            json!({"Version": "2012-10-17", "Statement": [{"Effect": "Deny", "Action": ["s3:*"], "Resource": ["arn:aws:s3:::*"]}]})
        } else {
            policy.clone()
        };
        let r = self.admin("PUT", "add-canned-policy", &[("name", &name)], &body)?;
        self.check(r, Ctx::Other)?;
        let r = self.admin("PUT", "set-user-or-group-policy", &[("policyName", &name), ("userOrGroup", key_id), ("isGroup", "false")], &Value::Null)?;
        self.check(r, Ctx::Other).map(|_| ())
    }

    fn delete_access_key(&self, key_id: &str) -> Result<(), BackendError> {
        for (verb, k, v) in [("remove-user", "accessKey", key_id.to_owned()), ("remove-canned-policy", "name", format!("p-{key_id}"))] {
            let r = self.admin("DELETE", verb, &[(k, &v)], &Value::Null)?;
            if r.status != 404 {
                self.check(r, Ctx::Other)?;
            }
        }
        let _ = std::fs::remove_file(self.secret_path(key_id));
        Ok(())
    }
}

#[cfg(test)]
#[path = "rustfs_tests.rs"]
mod tests;
