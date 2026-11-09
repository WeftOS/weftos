//! The RustFS backend (ADR-126 sections 3 and 7): the S3 data plane and the admin API behind
//! [`ObjectBackend`] and [`AdminBackend`]. Blocking HTTP, SigV4-signed with the root credential.
//! Call it from a blocking context (the daemon uses `spawn_blocking`), never from an async task.
//!
//! Behaviour checked against RustFS 1.0.4: the admin API takes plain JSON at
//! `/rustfs/admin/v3/*`, an empty-statement policy is refused (so "no grants" becomes an explicit
//! deny-all), creating an existing bucket succeeds for the root owner, a conditional put on an
//! existing key is a 412, and a disabled user's next request is a 403.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use reqwest::blocking::Client;
use serde_json::{Value, json};

use crate::backend::{AccessKey, AdminBackend, BackendError, BucketTags, DefaultRetention, LockConfig, LockMode, LifecycleRule, LockedPut, ObjectBackend, ObjectMeta, ObjectVersion, Part, PutOpts, Retention, check_bucket, check_key};
use crate::integrity;
use crate::sigv4::{self, Req};

/// Connection settings. `secrets_dir` receives one 0600 file per issued access key, in a 0700
/// directory.
///
/// `Debug` is written by hand and prints neither key: the root secret and access key must not
/// reach a log line, a panic message or a test failure through `{:?}`.
#[derive(Clone)]
pub struct RustfsConfig {
    /// `http://host:port` or `https://host:port`, no path.
    pub endpoint: String,
    pub region: String,
    pub root_access_key: String,
    pub root_secret_key: String,
    pub secrets_dir: PathBuf,
    pub timeout: Duration,
}

impl std::fmt::Debug for RustfsConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RustfsConfig")
            .field("endpoint", &self.endpoint)
            .field("region", &self.region)
            .field("root_access_key", &"<redacted>")
            .field("root_secret_key", &"<redacted>")
            .field("secrets_dir", &self.secrets_dir)
            .field("timeout", &self.timeout)
            .finish()
    }
}

/// Whether an endpoint authority (`host`, `host:port`, `[v6]:port`, with optional `user@`) names
/// this machine: `localhost`, a 127.0.0.0/8 address or `::1`.
fn is_loopback_host(auth: &str) -> bool {
    let auth = auth.rsplit_once('@').map_or(auth, |(_, h)| h);
    let host = match auth.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => auth.rsplit_once(':').map_or(auth, |(h, _)| h),
    };
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
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
    retry: Retry,
    /// Shared by every view of this backend (see [`RustfsBackend::with_timeout`]).
    breaker: Arc<Mutex<BreakerState>>,
    /// A per-request deadline shorter than the client's, set by `with_timeout`.
    call_timeout: Option<Duration>,
}

/// When to stop calling a RustFS that is not there: after `threshold` requests in a row fail
/// because it could not be reached (no connection, a timeout, 502/503/504), calls fail at once
/// with [`BackendError::Unavailable`] for `cooldown`, instead of each waiting out its own
/// timeout. After the cooldown one request is let through as a probe: if it gets any answer the
/// breaker closes, if not it opens for another cooldown. An answer of any kind (even 403 or 404, even
/// a 500) means RustFS is up and resets the count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Breaker {
    /// Consecutive failures that open it. 0 turns the breaker off.
    pub threshold: u32,
    pub cooldown: Duration,
}

impl Default for Breaker {
    fn default() -> Self {
        Self { threshold: 5, cooldown: Duration::from_secs(10) }
    }
}

/// The breaker's memory.
#[derive(Debug, Default)]
pub(crate) struct BreakerState {
    cfg: Breaker,
    fails: u32,
    open_until: Option<Instant>,
    /// A probe is out; everyone else keeps failing fast until it comes back.
    probing: bool,
    /// When the probe went out. One that never reports back (its thread died) is given up on
    /// after a cooldown and another is let through.
    probe_at: Option<Instant>,
}

impl BreakerState {
    /// May a request go out at `now`? `Err(wait)` is how long the breaker stays open.
    pub(crate) fn admit(&mut self, now: Instant) -> Result<(), Duration> {
        if self.cfg.threshold == 0 {
            return Ok(());
        }
        match self.open_until {
            None => Ok(()),
            Some(t) if now < t => Err(t - now),
            Some(_) if self.probing && self.probe_at.is_some_and(|p| now < p + self.cfg.cooldown) => Err(Duration::from_secs(1)),
            Some(_) => {
                self.probing = true;
                self.probe_at = Some(now);
                Ok(())
            }
        }
    }

    /// Record how a request ended. `down` is true when RustFS could not be reached.
    pub(crate) fn record(&mut self, down: bool, now: Instant) {
        if self.cfg.threshold == 0 {
            return;
        }
        if !down {
            *self = Self { cfg: self.cfg, ..Self::default() };
            return;
        }
        self.fails = self.fails.saturating_add(1);
        if self.probing || self.fails >= self.cfg.threshold {
            self.open_until = Some(now + self.cfg.cooldown);
            self.probing = false;
            self.probe_at = None;
        }
    }

    /// A request that was let through as the probe ended without saying anything about RustFS
    /// (a local fault): free the probe slot, leave the breaker as it was.
    pub(crate) fn release_probe(&mut self) {
        self.probing = false;
        self.probe_at = None;
    }

    pub(crate) fn is_open(&self, now: Instant) -> bool {
        self.cfg.threshold != 0 && self.open_until.is_some_and(|t| now < t || self.probing)
    }
}

/// Whether a finished request means RustFS is unreachable.
fn counts_as_down(r: &Result<u16, &Xport>) -> bool {
    matches!(r, Err(_) | Ok(502..=504))
}

/// What a finished request tells the breaker.
#[derive(Debug, PartialEq, Eq)]
enum Feed {
    Down,
    Up,
    /// Nothing about RustFS (the caller's own short deadline ran out).
    Neutral,
}

fn breaker_feed(r: &Result<u16, &Xport>, short_view: bool) -> Feed {
    match r {
        Err(x) if x.timed_out && short_view => Feed::Neutral,
        _ if counts_as_down(r) => Feed::Down,
        _ => Feed::Up,
    }
}

/// How a request is retried. Only what is safe to repeat is: a connection that never opened (any
/// request), a 503/429/408 (the server says it did not act, any request), and for reads and deletes
/// also a 500/502/504 or a dropped connection. A timed-out or dropped write is NOT repeated: it may
/// have landed, and a repeat would add a version in a locked store or turn a conditional create
/// into a false "exists".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retry {
    /// Total tries, the first included. 1 turns retrying off.
    pub attempts: u32,
    /// Wait before the second try; each wait is four times the one before, capped at 5 s.
    pub base: Duration,
}

impl Default for Retry {
    fn default() -> Self {
        Self { attempts: 3, base: Duration::from_millis(100) }
    }
}

impl Retry {
    /// The wait after try number `done` (1-based) failed.
    pub fn wait_after(&self, done: u32) -> Duration {
        self.base.saturating_mul(4u32.saturating_pow(done.saturating_sub(1))).min(Duration::from_secs(5))
    }
}

/// A request that failed below HTTP.
struct Xport {
    msg: String,
    /// The connection was never made, so the server saw nothing.
    never_sent: bool,
    timed_out: bool,
}

/// Reads and deletes can be repeated without changing the outcome.
fn repeatable(method: &str, query: &[(String, String)]) -> bool {
    match method {
        "GET" | "HEAD" => true,
        // Sending the same part number again replaces it: safe to repeat.
        "PUT" if query.iter().any(|(k, _)| k == "partNumber") => true,
        // A plain delete is idempotent; deleting a named version of a locked object is too, but
        // leave that to the caller to reissue.
        "DELETE" => !query.iter().any(|(k, _)| k == "versionId"),
        _ => false,
    }
}

/// Whether to try again after this result.
fn should_retry(method: &str, query: &[(String, String)], r: &Result<u16, &Xport>) -> bool {
    match r {
        Ok(503 | 429 | 408) => true,
        Ok(500 | 502 | 504) => repeatable(method, query),
        Ok(_) => false,
        Err(x) => x.never_sent || (repeatable(method, query) && !x.timed_out),
    }
}

#[derive(Debug)]
struct Resp {
    status: u16,
    etag: Option<String>,
    len: Option<u64>,
    /// `x-amz-version-id`, set by versioned (Object Lock) buckets.
    version: Option<String>,
    /// `x-amz-checksum-sha256` (base64), when the store returns one.
    sha256: Option<String>,
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

/// The ETag in a CopyObject answer, or the error an `<Error>` body of a `200` carries (the same
/// mapping as a non-2xx answer), or an error if the body is neither.
fn copy_result_etag(body: &[u8], src_bucket: &str, src_key: &str) -> Result<String, BackendError> {
    let text = String::from_utf8_lossy(body);
    if let Some(code) = xml_tag(&text, "Code").filter(|_| text.contains("<Error>")) {
        let msg = xml_tag(&text, "Message").unwrap_or_default();
        return Err(match code.as_str() {
            "NoSuchKey" => BackendError::NoObject(src_key.to_owned()),
            "NoSuchBucket" => BackendError::NoBucket(src_bucket.to_owned()),
            "AccessDenied" => BackendError::Forbidden(format!("{code} {msg}").trim().to_owned()),
            "InternalError" | "SlowDown" | "ServiceUnavailable" | "RequestTimeout" => BackendError::Unavailable(format!("{code} {msg}").trim().to_owned()),
            _ => BackendError::Io(format!("copy failed with a 200 answer: {code}: {msg}")),
        });
    }
    if !text.contains("<CopyObjectResult") {
        return Err(BackendError::Io(format!("copy answered 200 without a CopyObjectResult ({} bytes)", body.len())));
    }
    Ok(xml_tag(&text, "ETag").map(|e| unescape(&e).trim_matches('"').to_owned()).unwrap_or_default())
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

/// The `LifecycleConfiguration` body for `rules`.
pub fn lifecycle_xml(rules: &[LifecycleRule]) -> String {
    let mut x = String::from("<LifecycleConfiguration>");
    for r in rules {
        x.push_str(&format!("<Rule><ID>{}</ID><Filter><Prefix>{}</Prefix></Filter><Status>{}</Status>", escape(&r.id), escape(&r.prefix), if r.enabled { "Enabled" } else { "Disabled" }));
        if let Some(d) = r.expire_days {
            x.push_str(&format!("<Expiration><Days>{d}</Days></Expiration>"));
        }
        if let Some(d) = r.noncurrent_days {
            x.push_str(&format!("<NoncurrentVersionExpiration><NoncurrentDays>{d}</NoncurrentDays></NoncurrentVersionExpiration>"));
        }
        if let Some(d) = r.abort_multipart_days {
            x.push_str(&format!("<AbortIncompleteMultipartUpload><DaysAfterInitiation>{d}</DaysAfterInitiation></AbortIncompleteMultipartUpload>"));
        }
        x.push_str("</Rule>");
    }
    x.push_str("</LifecycleConfiguration>");
    x
}

/// Read the rules out of a `LifecycleConfiguration` document.
fn parse_lifecycle(x: &str) -> Vec<LifecycleRule> {
    xml_blocks(x, "Rule")
        .into_iter()
        .map(|b| {
            let n = |outer: &str, t: &str| xml_blocks(b, outer).first().and_then(|o| xml_tag(o, t)).and_then(|v| v.trim().parse::<u32>().ok());
            // The prefix is either `Filter/Prefix` or a bare `Prefix` (older documents).
            let prefix = xml_blocks(b, "Filter").first().and_then(|f| xml_tag(f, "Prefix")).or_else(|| xml_tag(b, "Prefix")).unwrap_or_default();
            LifecycleRule {
                id: xml_tag(b, "ID").unwrap_or_default(),
                prefix,
                enabled: xml_tag(b, "Status").as_deref() == Some("Enabled"),
                expire_days: n("Expiration", "Days"),
                noncurrent_days: n("NoncurrentVersionExpiration", "NoncurrentDays"),
                abort_multipart_days: n("AbortIncompleteMultipartUpload", "DaysAfterInitiation"),
            }
        })
        .collect()
}

/// A retain-until time must be in the future and writable as a four-digit-year ISO date.
fn check_until(until_unix_s: u64) -> Result<(), BackendError> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_err(io)?.as_secs();
    if until_unix_s <= now {
        return Err(BackendError::BadKey(format!("retain-until {until_unix_s} is not in the future")));
    }
    if until_unix_s > sigv4::MAX_ISO_SECS {
        return Err(BackendError::BadKey(format!("retain-until {until_unix_s} is past the year 9999")));
    }
    Ok(())
}

/// Parse an `ObjectLockConfiguration` document.
fn parse_lock_config(x: &str) -> Result<LockConfig, BackendError> {
    let enabled = xml_tag(x, "ObjectLockEnabled").as_deref() == Some("Enabled");
    let default = match xml_blocks(x, "DefaultRetention").first() {
        None => None,
        Some(d) => {
            let mode = match xml_tag(d, "Mode").as_deref() {
                Some("GOVERNANCE") => LockMode::Governance,
                Some("COMPLIANCE") => LockMode::Compliance,
                other => return Err(BackendError::Io(format!("object lock configuration: unknown mode {other:?}"))),
            };
            let n = |t: &str| xml_tag(d, t).and_then(|v| v.trim().parse::<u32>().ok());
            Some(DefaultRetention { mode, days: n("Days"), years: n("Years") })
        }
    };
    Ok(LockConfig { enabled, default })
}

/// The blocks of any of `tags`, in the order they appear (versions and delete markers interleave).
fn xml_blocks_in_order<'a>(s: &'a str, tags: &[&'a str]) -> Vec<(&'a str, &'a str)> {
    let mut v = Vec::new();
    let mut at = 0;
    while at < s.len() {
        let next = tags.iter().filter_map(|t| s[at..].find(&format!("<{t}>")).map(|i| (i, *t))).min_by_key(|(i, _)| *i);
        let Some((i, t)) = next else { break };
        let a = at + i + t.len() + 2;
        let Some(b) = s[a..].find(&format!("</{t}>")) else { break };
        v.push((t, &s[a..a + b]));
        at = a + b + t.len() + 3;
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
        // The root credential is signed into every request and the bodies are not encrypted:
        // plain http only to this machine.
        if scheme == "http" && !is_loopback_host(auth) {
            return Err(io(format!("endpoint {base:?}: plain http is only allowed to a loopback host (localhost, 127.0.0.0/8, ::1); use https")));
        }
        if scheme != "http" && scheme != "https" {
            return Err(io(format!("endpoint {base:?}: need http:// or https://")));
        }
        let host = match (scheme, auth.rsplit_once(':')) {
            ("http", Some((h, "80"))) | ("https", Some((h, "443"))) => h.to_owned(),
            _ => auth.to_owned(),
        };
        let http = Client::builder().timeout(cfg.timeout).build().map_err(io)?;
        Self::private_dir(&cfg.secrets_dir)?;
        Ok(Self { cfg, base: format!("{scheme}://{auth}"), host, http, retry: Retry::default(), breaker: Arc::new(Mutex::new(BreakerState::default())), call_timeout: None })
    }

    /// Change how requests are retried (tests use a short `base`; `attempts: 1` turns it off).
    pub fn with_retry(mut self, retry: Retry) -> Self {
        self.retry = retry;
        self
    }

    /// Set when the circuit breaker opens (see [`Breaker`]); `Breaker { threshold: 0, .. }` turns it off.
    pub fn with_breaker(self, b: Breaker) -> Self {
        *self.breaker.lock().unwrap_or_else(|e| e.into_inner()) = BreakerState { cfg: b, ..Default::default() };
        self
    }

    /// Whether calls are failing fast right now because RustFS could not be reached.
    pub fn breaker_open(&self) -> bool {
        self.breaker.lock().unwrap_or_else(|e| e.into_inner()).is_open(Instant::now())
    }

    /// A view of this backend whose requests give up after `d` instead of the client's default
    /// (30 s), for a caller that would rather fail than wait: `be.with_timeout(2s).get(..)`. It
    /// shares the circuit breaker with the original, so a dead RustFS found through one view is
    /// known to all. A timed-out write is not retried (it may have landed).
    pub fn with_timeout(&self, d: Duration) -> Self {
        Self { cfg: self.cfg.clone(), base: self.base.clone(), host: self.host.clone(), http: self.http.clone(), retry: self.retry, breaker: self.breaker.clone(), call_timeout: Some(d) }
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
        if let Err(wait) = self.breaker.lock().unwrap_or_else(|e| e.into_inner()).admit(Instant::now()) {
            return Err(BackendError::Unavailable(format!("{method} {path}: circuit open, RustFS was unreachable; next try in {}s", wait.as_secs().max(1))));
        }
        let mut done = 0;
        loop {
            done += 1;
            let r = self.send_once(method, path, query, body, extra);
            let seen = match &r {
                Ok(Ok(resp)) => Ok(resp.status),
                Ok(Err(x)) => Err(x),
                Err(e) => {
                    // A local fault (signing, clock) says nothing about RustFS; free a probe if this was one.
                    self.breaker.lock().unwrap_or_else(|x| x.into_inner()).release_probe();
                    return Err(e.clone());
                }
            };
            if done < self.retry.attempts && should_retry(method, query, &seen) {
                std::thread::sleep(self.retry.wait_after(done));
                continue;
            }
            // A timeout under a caller's own short deadline says the caller was impatient, not that
            // RustFS is down: it does not count toward opening the breaker for everyone.
            let short_view = self.call_timeout.is_some_and(|t| t < self.cfg.timeout);
            let mut b = self.breaker.lock().unwrap_or_else(|e| e.into_inner());
            match breaker_feed(&seen, short_view) {
                Feed::Down => b.record(true, Instant::now()),
                Feed::Up => b.record(false, Instant::now()),
                Feed::Neutral => b.release_probe(),
            }
            return match r {
                Ok(Ok(resp)) => Ok(resp),
                Ok(Err(x)) => Err(BackendError::Unavailable(format!("{method} {path}: {}", x.msg))),
                Err(e) => Err(e),
            };
        }
    }

    /// One try. The outer error is a local fault (signing, clock) that retrying cannot help.
    fn send_once(&self, method: &str, path: &str, query: &[(String, String)], body: &[u8], extra: &[(&str, String)]) -> Result<Result<Resp, Xport>, BackendError> {
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
        if let Some(t) = self.call_timeout {
            rb = rb.timeout(t);
        }
        let r = match rb.send() {
            Ok(r) => r,
            Err(e) => return Ok(Err(Xport { msg: e.to_string(), never_sent: e.is_connect(), timed_out: e.is_timeout() })),
        };
        let status = r.status().as_u16();
        let etag = r.headers().get("etag").and_then(|v| v.to_str().ok()).map(|v| v.trim_matches('"').to_owned());
        let len = r.headers().get("content-length").and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok());
        let version = r.headers().get("x-amz-version-id").and_then(|v| v.to_str().ok()).map(str::to_owned);
        let sha256 = r.headers().get("x-amz-checksum-sha256").and_then(|v| v.to_str().ok()).map(str::to_owned);
        let body = match r.bytes() {
            Ok(b) => b.to_vec(),
            Err(e) => return Ok(Err(Xport { msg: format!("reading the response: {e}"), never_sent: false, timed_out: e.is_timeout() })),
        };
        Ok(Ok(Resp { status, etag, len, version, sha256, body }))
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
            (401 | 403, _, _) => BackendError::Forbidden(format!("{code} {msg}").trim().to_owned()),
            (408 | 429 | 500 | 502 | 503 | 504, _, _) => BackendError::Unavailable(format!("{} {code} {msg}", r.status).trim().to_owned()),
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

    /// The secrets directory: made 0700, and an existing one is tightened to 0700, so no other
    /// user can list or read the per-key secret files.
    fn private_dir(dir: &Path) -> Result<(), BackendError> {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).map_err(io)?;
        let md = std::fs::metadata(dir).map_err(io)?;
        if md.permissions().mode() & 0o777 != 0o700 {
            // Only a directory of ours is tightened: a shared one (a temp dir, say) is refused
            // rather than having its permissions changed under other users. Our own uid is read
            // from a probe file made here (std only: `/proc/self` is Linux-only and macOS has no
            // `/proc`); a directory we cannot create in is not ours either.
            let probe = dir.join(".owner-probe");
            let _ = std::fs::remove_file(&probe);
            let mine = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&probe)
                .and_then(|f| f.metadata())
                .map(|m| m.uid());
            let _ = std::fs::remove_file(&probe);
            match mine {
                Ok(me) if md.uid() == me => {}
                _ => return Err(io(format!("{}: secrets directory belongs to another user and is not 0700", dir.display()))),
            }
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(io)?;
        }
        Ok(())
    }

    fn write_secret(path: &Path, secret: &str) -> Result<(), BackendError> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let tmp = path.with_extension("tmp");
        // `create_new` (O_EXCL): the mode applies only to a file made here, never to one that was
        // already there, and a pre-planted file or link at the path is not written through. A
        // leftover from an earlier crash is removed first.
        match std::fs::remove_file(&tmp) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io(e)),
        }
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp).map_err(io)?;
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
    /// GetObject with its integrity check (see [`integrity`]): the body is verified against
    /// `Content-Length` and the SHA-256 the store holds, or the single-part ETag, and a mismatch is an
    /// error, not data. The [`integrity::Check`] says what it was verified against; `Unverified`
    /// means only the length could be checked (an object stored without a checksum with a
    /// multipart ETag).
    pub fn get_checked(&self, bucket: &str, key: &str) -> Result<(Vec<u8>, integrity::Check), BackendError> {
        check_bucket(bucket)?;
        check_key(key)?;
        let r = self.send("GET", &format!("/{bucket}/{key}"), &[], &[], &[("x-amz-checksum-mode", "ENABLED".into())])?;
        let r = self.check(r, Ctx::Object(bucket, key))?;
        let how = integrity::verify_body(&r.body, r.len, r.sha256.as_deref(), r.etag.as_deref()).map_err(BackendError::Io)?;
        Ok((r.body, how))
    }

    pub fn put_bucket_notification(&self, bucket: &str, target: &str) -> Result<(), BackendError> {
        check_bucket(bucket)?;
        if target.is_empty() || !target.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
            return Err(BackendError::BadKey(format!("notification target {target:?}: letters, digits, '_' and '-' only")));
        }
        let r = self.send("PUT", &format!("/{bucket}"), &Self::q(&[("notification", "")]), notification_xml(target).as_bytes(), &[])?;
        self.check(r, Ctx::Bucket(bucket)).map(|_| ())
    }
}

impl ObjectBackend for RustfsBackend {
    fn put(&self, bucket: &str, key: &str, data: &[u8], if_none_match: bool) -> Result<ObjectMeta, BackendError> {
        check_bucket(bucket)?;
        check_key(key)?;
        let mut extra: Vec<(&str, String)> = vec![("x-amz-checksum-sha256", integrity::checksum_sha256(data))];
        if if_none_match {
            extra.push(("if-none-match", "*".into()));
        }
        let r = self.send("PUT", &format!("/{bucket}/{key}"), &[], data, &extra)?;
        let r = self.check(r, Ctx::Object(bucket, key))?;
        Ok(ObjectMeta { key: key.to_owned(), size: data.len() as u64, etag: r.etag.unwrap_or_default() })
    }

    fn put_if_match(&self, bucket: &str, key: &str, data: &[u8], etag: &str) -> Result<ObjectMeta, BackendError> {
        check_bucket(bucket)?;
        check_key(key)?;
        // An ETag header value is quoted; accept the bare value the caller got from `ObjectMeta`.
        let q = if etag.starts_with('"') { etag.to_owned() } else { format!("\"{etag}\"") };
        let r = self.send("PUT", &format!("/{bucket}/{key}"), &[], data, &[("if-match", q), ("x-amz-checksum-sha256", integrity::checksum_sha256(data))])?;
        let r = self.check(r, Ctx::Object(bucket, key)).map_err(|e| match e {
            BackendError::Exists(k) => BackendError::Stale(k),
            e => e,
        })?;
        Ok(ObjectMeta { key: key.to_owned(), size: data.len() as u64, etag: r.etag.unwrap_or_default() })
    }

    fn put_locked(&self, bucket: &str, key: &str, data: &[u8], opts: &PutOpts) -> Result<LockedPut, BackendError> {
        check_bucket(bucket)?;
        check_key(key)?;
        let mut extra = self.lock_headers(opts)?;
        extra.push(("x-amz-checksum-sha256", integrity::checksum_sha256(data)));
        if opts.if_none_match {
            extra.push(("if-none-match", "*".into()));
        }
        let r = self.send("PUT", &format!("/{bucket}/{key}"), &[], data, &extra)?;
        let r = self.check(r, Ctx::Object(bucket, key))?;
        Ok(LockedPut { meta: ObjectMeta { key: key.to_owned(), size: data.len() as u64, etag: r.etag.unwrap_or_default() }, version_id: r.version })
    }

    fn create_multipart(&self, bucket: &str, key: &str, opts: &PutOpts) -> Result<String, BackendError> {
        check_bucket(bucket)?;
        check_key(key)?;
        let extra = self.lock_headers(opts)?;
        let r = self.send("POST", &format!("/{bucket}/{key}"), &Self::q(&[("uploads", "")]), &[], &extra)?;
        let r = self.check(r, Ctx::Object(bucket, key))?;
        xml_tag(&String::from_utf8_lossy(&r.body), "UploadId").filter(|u| !u.is_empty()).ok_or_else(|| BackendError::Io("multipart: no UploadId in the answer".into()))
    }

    fn upload_part(&self, bucket: &str, key: &str, upload_id: &str, number: u32, data: &[u8]) -> Result<String, BackendError> {
        check_bucket(bucket)?;
        check_key(key)?;
        if !(1..=crate::backend::MAX_PARTS).contains(&number) {
            return Err(BackendError::BadKey(format!("part number {number} is outside 1..={}", crate::backend::MAX_PARTS)));
        }
        let n = number.to_string();
        let r = self.send("PUT", &format!("/{bucket}/{key}"), &Self::q(&[("partNumber", &n), ("uploadId", upload_id)]), data, &[])?;
        let r = self.check(r, Ctx::Object(bucket, key))?;
        r.etag.filter(|e| !e.is_empty()).ok_or_else(|| BackendError::Io(format!("multipart: part {number} came back without an etag")))
    }

    fn complete_multipart(&self, bucket: &str, key: &str, upload_id: &str, parts: &[Part], opts: &PutOpts) -> Result<LockedPut, BackendError> {
        check_bucket(bucket)?;
        check_key(key)?;
        if parts.is_empty() || parts.windows(2).any(|w| w[0].number >= w[1].number) {
            return Err(BackendError::BadKey("multipart: parts must be given in increasing order, at least one".into()));
        }
        let body = format!(
            "<CompleteMultipartUpload>{}</CompleteMultipartUpload>",
            parts.iter().map(|p| format!("<Part><PartNumber>{}</PartNumber><ETag>\"{}\"</ETag></Part>", p.number, p.etag)).collect::<String>()
        );
        let extra: Vec<(&str, String)> = if opts.if_none_match { vec![("if-none-match", "*".into())] } else { Vec::new() };
        let r = self.send("POST", &format!("/{bucket}/{key}"), &Self::q(&[("uploadId", upload_id)]), body.as_bytes(), &extra)?;
        let r = self.check(r, Ctx::Object(bucket, key))?;
        // S3 may answer 200 and put the failure in the body.
        let text = String::from_utf8_lossy(&r.body).into_owned();
        if text.contains("<Error>") {
            return Err(BackendError::Io(format!("multipart complete: {} {}", xml_tag(&text, "Code").unwrap_or_default(), xml_tag(&text, "Message").unwrap_or_default())));
        }
        let etag = xml_tag(&text, "ETag").map(|e| e.trim_matches('"').to_owned()).or(r.etag).unwrap_or_default();
        Ok(LockedPut { meta: ObjectMeta { key: key.to_owned(), size: 0, etag }, version_id: r.version })
    }

    fn abort_multipart(&self, bucket: &str, key: &str, upload_id: &str) -> Result<(), BackendError> {
        check_bucket(bucket)?;
        check_key(key)?;
        let r = self.send("DELETE", &format!("/{bucket}/{key}"), &Self::q(&[("uploadId", upload_id)]), &[], &[])?;
        // Aborting an upload that is already gone is fine.
        if r.status == 404 && String::from_utf8_lossy(&r.body).contains("NoSuchUpload") {
            return Ok(());
        }
        self.check(r, Ctx::Object(bucket, key)).map(|_| ())
    }

    fn delete_version(&self, bucket: &str, key: &str, version_id: &str, bypass_governance: bool) -> Result<(), BackendError> {
        check_bucket(bucket)?;
        check_key(key)?;
        let extra: Vec<(&str, String)> = if bypass_governance { vec![("x-amz-bypass-governance-retention", "true".into())] } else { Vec::new() };
        let r = self.send("DELETE", &format!("/{bucket}/{key}"), &Self::q(&[("versionId", version_id)]), &[], &extra)?;
        self.check(r, Ctx::Object(bucket, key)).map(|_| ())
    }

    fn get_retention(&self, bucket: &str, key: &str, version_id: &str) -> Result<Option<Retention>, BackendError> {
        check_bucket(bucket)?;
        check_key(key)?;
        let r = self.send("GET", &format!("/{bucket}/{key}"), &Self::q(&[("retention", ""), ("versionId", version_id)]), &[], &[])?;
        // An object with no retention answers 404 NoSuchObjectLockConfiguration.
        if r.status == 404 && String::from_utf8_lossy(&r.body).contains("NoSuchObjectLockConfiguration") {
            return Ok(None);
        }
        let r = self.check(r, Ctx::Object(bucket, key))?;
        let x = String::from_utf8_lossy(&r.body);
        let mode = match xml_tag(&x, "Mode").as_deref() {
            Some("GOVERNANCE") => LockMode::Governance,
            Some("COMPLIANCE") => LockMode::Compliance,
            None => return Ok(None),
            Some(o) => return Err(BackendError::Io(format!("retention: unknown mode {o:?}"))),
        };
        let until = xml_tag(&x, "RetainUntilDate").and_then(|d| sigv4::parse_iso(&d)).ok_or_else(|| BackendError::Io("retention: no readable RetainUntilDate".into()))?;
        Ok(Some(Retention { mode, until_unix_s: until }))
    }

    fn put_retention(&self, bucket: &str, key: &str, version_id: &str, r: &Retention, bypass_governance: bool) -> Result<(), BackendError> {
        check_bucket(bucket)?;
        check_key(key)?;
        check_until(r.until_unix_s)?;
        let body = format!("<Retention><Mode>{}</Mode><RetainUntilDate>{}</RetainUntilDate></Retention>", r.mode.as_str(), sigv4::iso_date(r.until_unix_s));
        let extra: Vec<(&str, String)> = if bypass_governance { vec![("x-amz-bypass-governance-retention", "true".into())] } else { Vec::new() };
        let resp = self.send("PUT", &format!("/{bucket}/{key}"), &Self::q(&[("retention", ""), ("versionId", version_id)]), body.as_bytes(), &extra)?;
        self.check(resp, Ctx::Object(bucket, key)).map(|_| ())
    }

    fn get_lock_config(&self, bucket: &str) -> Result<LockConfig, BackendError> {
        check_bucket(bucket)?;
        let r = self.send("GET", &format!("/{bucket}"), &Self::q(&[("object-lock", "")]), &[], &[])?;
        // A bucket made without lock answers 404 ObjectLockConfigurationNotFoundError.
        if r.status == 404 && String::from_utf8_lossy(&r.body).contains("ObjectLockConfigurationNotFound") {
            return Ok(LockConfig { enabled: false, default: None });
        }
        let r = self.check(r, Ctx::Bucket(bucket))?;
        parse_lock_config(&String::from_utf8_lossy(&r.body))
    }

    fn put_default_retention(&self, bucket: &str, d: &DefaultRetention) -> Result<(), BackendError> {
        check_bucket(bucket)?;
        let span = match (d.days, d.years) {
            (Some(n), None) if n > 0 => format!("<Days>{n}</Days>"),
            (None, Some(n)) if n > 0 => format!("<Years>{n}</Years>"),
            _ => return Err(BackendError::BadKey("default retention needs exactly one of days or years, above zero".into())),
        };
        let body = format!(
            "<ObjectLockConfiguration><ObjectLockEnabled>Enabled</ObjectLockEnabled><Rule><DefaultRetention><Mode>{}</Mode>{span}</DefaultRetention></Rule></ObjectLockConfiguration>",
            d.mode.as_str()
        );
        let r = self.send("PUT", &format!("/{bucket}"), &Self::q(&[("object-lock", "")]), body.as_bytes(), &[])?;
        self.check(r, Ctx::Bucket(bucket)).map(|_| ())
    }

    fn put_legal_hold(&self, bucket: &str, key: &str, version_id: &str, on: bool) -> Result<(), BackendError> {
        check_bucket(bucket)?;
        check_key(key)?;
        let body = format!("<LegalHold><Status>{}</Status></LegalHold>", if on { "ON" } else { "OFF" });
        let r = self.send("PUT", &format!("/{bucket}/{key}"), &Self::q(&[("legal-hold", ""), ("versionId", version_id)]), body.as_bytes(), &[])?;
        self.check(r, Ctx::Object(bucket, key)).map(|_| ())
    }

    fn list_versions(&self, bucket: &str, prefix: &str) -> Result<Vec<ObjectVersion>, BackendError> {
        check_bucket(bucket)?;
        self.list_versions_paged(bucket, prefix, 1000)
    }

    fn get(&self, bucket: &str, key: &str) -> Result<Vec<u8>, BackendError> {
        self.get_checked(bucket, key).map(|(b, _)| b)
    }

    fn head(&self, bucket: &str, key: &str) -> Result<ObjectMeta, BackendError> {
        check_bucket(bucket)?;
        check_key(key)?;
        let r = self.send("HEAD", &format!("/{bucket}/{key}"), &[], &[], &[])?;
        let r = self.check(r, Ctx::Object(bucket, key))?;
        Ok(ObjectMeta { key: key.to_owned(), size: r.len.unwrap_or(0), etag: r.etag.unwrap_or_default() })
    }

    fn delete(&self, bucket: &str, key: &str) -> Result<(), BackendError> {
        check_bucket(bucket)?;
        check_key(key)?;
        let r = self.send("DELETE", &format!("/{bucket}/{key}"), &[], &[], &[])?;
        self.check(r, Ctx::Object(bucket, key)).map(|_| ())
    }

    fn copy(&self, src_bucket: &str, src_key: &str, dst_bucket: &str, dst_key: &str) -> Result<ObjectMeta, BackendError> {
        check_bucket(src_bucket)?;
        check_bucket(dst_bucket)?;
        check_key(src_key)?;
        check_key(dst_key)?;
        let src = format!("/{src_bucket}/{}", sigv4::encode(src_key, true));
        let r = self.send("PUT", &format!("/{dst_bucket}/{dst_key}"), &[], &[], &[("x-amz-copy-source", src)])?;
        let r = self.check(r, Ctx::Object(src_bucket, src_key))?;
        // S3 answers a copy that failed part-way with 200 and an <Error> body: the status is not
        // the verdict. The body has to be a CopyObjectResult, and its ETag is the copy's.
        let etag = copy_result_etag(&r.body, src_bucket, src_key)?;
        let mut meta = self.head(dst_bucket, dst_key)?;
        if !etag.is_empty() && !meta.etag.is_empty() && etag != meta.etag {
            return Err(BackendError::Io(format!("copy {src_key} -> {dst_key}: the copy reports ETag {etag} but the destination now has {}", meta.etag)));
        }
        if meta.etag.is_empty() {
            meta.etag = etag;
        }
        Ok(meta)
    }

    fn list(&self, bucket: &str, prefix: &str) -> Result<Vec<ObjectMeta>, BackendError> {
        check_bucket(bucket)?;
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
    /// The Object Lock headers for `opts` (retention and legal hold; not the conditional).
    fn lock_headers(&self, opts: &PutOpts) -> Result<Vec<(&'static str, String)>, BackendError> {
        let mut extra = Vec::new();
        if let Some(r) = opts.retention {
            check_until(r.until_unix_s)?;
            extra.push(("x-amz-object-lock-mode", r.mode.as_str().into()));
            extra.push(("x-amz-object-lock-retain-until-date", sigv4::iso_date(r.until_unix_s)));
        }
        if opts.legal_hold {
            extra.push(("x-amz-object-lock-legal-hold", "ON".into()));
        }
        Ok(extra)
    }

    pub(crate) fn list_versions_paged(&self, bucket: &str, prefix: &str, page: usize) -> Result<Vec<ObjectVersion>, BackendError> {
        check_bucket(bucket)?;
        let page_s = page.to_string();
        let mut out = Vec::new();
        let (mut key_marker, mut ver_marker) = (String::new(), String::new());
        loop {
            let mut q = vec![("versions", ""), ("prefix", prefix), ("max-keys", &page_s)];
            if !key_marker.is_empty() {
                q.push(("key-marker", &key_marker));
            }
            if !ver_marker.is_empty() {
                q.push(("version-id-marker", &ver_marker));
            }
            let r = self.send("GET", &format!("/{bucket}"), &Self::q(&q), &[], &[])?;
            let r = self.check(r, Ctx::Bucket(bucket))?;
            let text = String::from_utf8_lossy(&r.body).into_owned();
            for (marker, c) in xml_blocks_in_order(&text, &["Version", "DeleteMarker"]) {
                out.push(ObjectVersion {
                    key: xml_tag(c, "Key").unwrap_or_default(),
                    version_id: xml_tag(c, "VersionId").unwrap_or_else(|| "null".into()),
                    is_latest: xml_tag(c, "IsLatest").as_deref() == Some("true"),
                    is_delete_marker: marker == "DeleteMarker",
                    size: xml_tag(c, "Size").and_then(|s| s.parse().ok()).unwrap_or(0),
                    etag: xml_tag(c, "ETag").unwrap_or_default().trim_matches('"').to_owned(),
                    last_modified: xml_tag(c, "LastModified").unwrap_or_default(),
                });
            }
            match (xml_tag(&text, "IsTruncated").as_deref(), xml_tag(&text, "NextKeyMarker"), xml_tag(&text, "NextVersionIdMarker")) {
                (Some("true"), Some(k), v) if !k.is_empty() => {
                    key_marker = k;
                    ver_marker = v.unwrap_or_default();
                }
                _ => break,
            }
        }
        // S3 lists by key and newest first within a key; keep that order, and make the sort
        // stable so a backend that ties on time keeps what it sent.
        out.sort_by(|a, b| a.key.cmp(&b.key).then(b.is_latest.cmp(&a.is_latest)));
        Ok(out)
    }

    fn create_bucket_with(&self, bucket: &str, tags: &[(String, String)], locked: bool) -> Result<(), BackendError> {
        check_bucket(bucket)?;
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
    fn put_lifecycle(&self, bucket: &str, rules: &[LifecycleRule]) -> Result<(), BackendError> {
        check_bucket(bucket)?;
        if rules.is_empty() {
            let r = self.send("DELETE", &format!("/{bucket}"), &Self::q(&[("lifecycle", "")]), &[], &[])?;
            // Removing rules from a bucket that has none is fine.
            if r.status == 404 && String::from_utf8_lossy(&r.body).contains("NoSuchLifecycleConfiguration") {
                return Ok(());
            }
            return self.check(r, Ctx::Bucket(bucket)).map(|_| ());
        }
        let mut seen = std::collections::BTreeSet::new();
        for r in rules {
            r.validate()?;
            if !seen.insert(&r.id) {
                return Err(BackendError::BadKey(format!("lifecycle rule id {:?} is used twice", r.id)));
            }
        }
        let body = lifecycle_xml(rules);
        let r = self.send("PUT", &format!("/{bucket}"), &Self::q(&[("lifecycle", "")]), body.as_bytes(), &[])?;
        self.check(r, Ctx::Bucket(bucket)).map(|_| ())
    }

    fn get_lifecycle(&self, bucket: &str) -> Result<Vec<LifecycleRule>, BackendError> {
        check_bucket(bucket)?;
        let r = self.send("GET", &format!("/{bucket}"), &Self::q(&[("lifecycle", "")]), &[], &[])?;
        if r.status == 404 && String::from_utf8_lossy(&r.body).contains("NoSuchLifecycleConfiguration") {
            return Ok(Vec::new());
        }
        let r = self.check(r, Ctx::Bucket(bucket))?;
        Ok(parse_lifecycle(&String::from_utf8_lossy(&r.body)))
    }

    fn create_locked_bucket(&self, bucket: &str, tags: &[(String, String)]) -> Result<(), BackendError> {
        check_bucket(bucket)?;
        self.create_bucket_with(bucket, tags, true)
    }

    fn create_bucket(&self, bucket: &str, tags: &[(String, String)]) -> Result<(), BackendError> {
        check_bucket(bucket)?;
        self.create_bucket_with(bucket, tags, false)
    }

    fn delete_bucket(&self, bucket: &str) -> Result<(), BackendError> {
        check_bucket(bucket)?;
        let r = self.send("DELETE", &format!("/{bucket}"), &[], &[], &[])?;
        self.check(r, Ctx::Bucket(bucket)).map(|_| ())
    }

    fn bucket_exists(&self, bucket: &str) -> Result<bool, BackendError> {
        check_bucket(bucket)?;
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

#[cfg(test)]
#[path = "rustfs_prop_tests.rs"]
mod prop_tests;
