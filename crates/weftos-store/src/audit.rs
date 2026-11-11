//! What gets logged (ADR-126 section 6), and the parsers for the two RustFS sources.
//!
//! The daemon logs every governed request itself. RustFS has its own audit stream (successful
//! operations) and its own JSON log (`s3_authorization_denied` refusals); both are read here
//! into the same [`AuditEvent`]. **The field names the parsers accept are written from the S0
//! write-up, not from a captured RustFS line:** they look for several spellings and ignore the
//! rest, and need checking against a live RustFS before S1 is called done on PG.

use std::sync::Mutex;

use serde_json::Value;

/// How a request ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    /// Exposure, grant or policy refused it.
    Refused(String),
    /// Allowed, but the backend failed it.
    Error(String),
}

impl Outcome {
    /// `ok`, `refused:<why>` or `error:<why>` (the activity log's spelling).
    pub fn text(&self) -> String {
        match self {
            Self::Ok => "ok".into(),
            Self::Refused(w) => format!("refused:{w}"),
            Self::Error(w) => format!("error:{w}"),
        }
    }
}

/// Where the event came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The daemon's own governed path.
    Governed,
    /// RustFS's audit stream.
    RustfsAudit,
    /// RustFS's JSON log (refusals).
    RustfsLog,
    /// A bucket notification.
    Notification,
}

/// One store operation: who, which store, what, how big, how it ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEvent {
    pub ts_ms: u64,
    /// `cog:<id>` etc., or the raw access key if it maps to no principal.
    pub principal: String,
    /// The store's `weftos://` name, or the bucket if it maps to no store.
    pub store: String,
    pub bucket: String,
    /// S3 operation name (`PutObject`, `CreateBucket`, ...).
    pub op: String,
    pub key: Option<String>,
    pub bytes: u64,
    pub outcome: Outcome,
    pub source: Source,
}

/// Where events go. The daemon writes them to the activity log.
pub trait AuditSink: Send + Sync {
    fn record(&self, e: &AuditEvent);
}

/// Keeps events in memory (tests).
#[derive(Default)]
pub struct MemSink(pub Mutex<Vec<AuditEvent>>);

impl MemSink {
    pub fn events(&self) -> Vec<AuditEvent> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl AuditSink for MemSink {
    fn record(&self, e: &AuditEvent) {
        self.0.lock().unwrap_or_else(|x| x.into_inner()).push(e.clone());
    }
}

/// A RustFS record before the principal and store are resolved.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Raw {
    pub access_key: String,
    pub bucket: String,
    pub object: Option<String>,
    pub op: String,
    pub status: Option<u16>,
    pub bytes: u64,
    pub reason: Option<String>,
}

fn find<'a>(v: &'a Value, names: &[&str]) -> Option<&'a Value> {
    match v {
        Value::Object(m) => {
            for n in names {
                if let Some(x) = m.get(*n).filter(|x| !x.is_null() && !x.is_object() && !x.is_array()) {
                    return Some(x);
                }
            }
            m.values().find_map(|x| find(x, names))
        }
        Value::Array(a) => a.iter().find_map(|x| find(x, names)),
        _ => None,
    }
}

fn text(v: &Value, names: &[&str]) -> Option<String> {
    find(v, names).and_then(|x| match x {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    })
}

fn num(v: &Value, names: &[&str]) -> Option<u64> {
    find(v, names).and_then(|x| x.as_u64().or_else(|| x.as_str().and_then(|s| s.parse().ok())))
}

/// One line of RustFS's audit stream. RustFS 1.0.4 posts `{"EventName":..,"Key":..,"Records":[{..}]}`
/// with the operation in `Records[0].api` (`name` is `s3:PutObject`, `status_code` is the S3 result,
/// the object key is plain) and the caller in `Records[0].accessKey`. There is no size field, so
/// `bytes` is 0. A flat or differently nested record is still read by field name.
pub fn parse_audit(line: &str) -> Option<Raw> {
    let v: Value = serde_json::from_str(line.trim()).ok()?;
    let rec = v["Records"].as_array().and_then(|a| a.first()).unwrap_or(&v);
    let api = &rec["api"];
    if api.get("status_code").is_some()
        && let (Some(op), Some(bucket)) = (api["name"].as_str(), api["bucket"].as_str().filter(|b| !b.is_empty()))
    {
        return Some(Raw {
            access_key: rec["accessKey"].as_str().unwrap_or_default().to_owned(),
            bucket: bucket.to_owned(),
            object: api["object"].as_str().filter(|o| !o.is_empty()).map(str::to_owned),
            op: op.trim_start_matches("s3:").to_owned(),
            status: api["status_code"].as_u64().and_then(|n| u16::try_from(n).ok()),
            bytes: 0,
            reason: None,
        });
    }
    let bucket = text(&v, &["bucket", "bucketName"])?;
    let op = text(&v, &["apiName", "api", "name", "operation", "event", "eventName"])?;
    Some(Raw {
        access_key: text(&v, &["accessKey", "access_key", "accesskey", "principalId"]).unwrap_or_default(),
        bucket,
        object: text(&v, &["object", "objectName", "key"]),
        op: op.trim_start_matches("s3:").to_owned(),
        status: num(&v, &["statusCode", "status_code", "status"]).and_then(|n| u16::try_from(n).ok()),
        bytes: num(&v, &["contentLength", "size", "rx", "bytes"]).unwrap_or(0),
        reason: None,
    })
}

/// Notification object keys are percent-encoded (`a%2Fb.txt`); undo that. Invalid escapes stay as written.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        if b[i] == b'%' && i + 2 < b.len() && hex(b[i + 1]).is_some() && hex(b[i + 2]).is_some() {
            out.push((hex(b[i + 1]).unwrap_or(0) * 16 + hex(b[i + 2]).unwrap_or(0)) as u8);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `S3Action(GetObjectAction)` (RustFS 1.0.4's log) or `s3:GetObject` to `GetObject`.
fn action_name(a: &str) -> String {
    let a = a.trim();
    match a.strip_prefix("S3Action(").and_then(|x| x.strip_suffix(')')) {
        Some(x) => x.strip_suffix("Action").unwrap_or(x).to_owned(),
        None => a.trim_start_matches("s3:").to_owned(),
    }
}

/// A request RustFS refused before it knew who sent it: a bad signature or an unknown access key.
/// RustFS 1.0.4 logs these as `{"level":"ERROR","error":"S3Error { code: SignatureDoesNotMatch,
/// message: ... }","target":"s3s::ops",...}` with **no account, bucket or operation**, so the
/// record is unattributed (`access_key` is empty) and `reason` is the S3 error code.
pub fn parse_auth_failure(line: &str) -> Option<Raw> {
    if !line.contains("SignatureDoesNotMatch") && !line.contains("InvalidAccessKeyId") {
        return None;
    }
    let v: Value = serde_json::from_str(line.trim()).ok()?;
    let msg = ["error", "err", "message"].iter().find_map(|k| v[*k].as_str().filter(|m| m.contains("code: ")))?;
    let code = ["SignatureDoesNotMatch", "InvalidAccessKeyId"].into_iter().find(|c| msg.contains(&format!("code: {c}")))?;
    Some(Raw { access_key: String::new(), bucket: String::new(), object: None, op: String::new(), status: Some(403), bytes: 0, reason: Some(code.to_owned()) })
}

/// One `s3_authorization_denied` line of RustFS's JSON log.
pub fn parse_denial(line: &str) -> Option<Raw> {
    if !line.contains("s3_authorization_denied") {
        return None;
    }
    let v: Value = serde_json::from_str(line.trim()).ok()?;
    Some(Raw {
        access_key: text(&v, &["account", "accessKey", "access_key"]).unwrap_or_default(),
        bucket: text(&v, &["bucket"]).unwrap_or_default(),
        object: text(&v, &["object", "key"]).filter(|o| o != "None" && !o.is_empty()),
        op: text(&v, &["action"]).map(|a| action_name(&a)).unwrap_or_default(),
        status: Some(403),
        bytes: 0,
        reason: text(&v, &["reason"]),
    })
}

/// The records of an S3 event notification (`{"Records": [...]}`, RustFS 1.0.4 adds `EventName` and `Key`
/// beside it): event name, bucket, percent-decoded key, size, identity.
pub fn parse_notification(body: &str) -> Vec<Raw> {
    let Ok(v) = serde_json::from_str::<Value>(body.trim()) else { return Vec::new() };
    let recs = v["Records"].as_array().cloned().unwrap_or_else(|| if v.is_object() { vec![v.clone()] } else { Vec::new() });
    recs.iter()
        .filter_map(|r| {
            let bucket = r["s3"]["bucket"]["name"].as_str()?.to_owned();
            let op = r["eventName"].as_str()?.trim_start_matches("s3:").to_owned();
            Some(Raw {
                access_key: r["userIdentity"]["principalId"].as_str().unwrap_or_default().to_owned(),
                bucket,
                object: r["s3"]["object"]["key"].as_str().map(percent_decode),
                op,
                status: Some(200),
                bytes: r["s3"]["object"]["size"].as_u64().unwrap_or(0),
                reason: None,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_line_is_read_whatever_the_nesting() {
        let r = parse_audit(r#"{"time":"t","api":{"name":"PutObject","bucket":"n-one","object":"a/b","statusCode":200,"rx":42},"accessKey":"AK1"}"#).unwrap();
        assert_eq!((r.op.as_str(), r.bucket.as_str(), r.object.as_deref(), r.status, r.bytes, r.access_key.as_str()), ("PutObject", "n-one", Some("a/b"), Some(200), 42, "AK1"));
        assert!(parse_audit("not json").is_none());
        assert!(parse_audit(r#"{"api":"PutObject"}"#).is_none(), "no bucket, nothing to attribute");
    }

    #[test]
    fn denial_line_needs_the_marker_and_gives_a_403() {
        let r = parse_denial(r#"{"message":"s3_authorization_denied","account":"AK1","bucket":"p-x","object":"o","action":"s3:PutObject","reason":"no policy"}"#).unwrap();
        assert_eq!((r.op.as_str(), r.status, r.reason.as_deref()), ("PutObject", Some(403), Some("no policy")));
        assert!(parse_denial(r#"{"message":"something else"}"#).is_none());
        assert!(parse_denial("s3_authorization_denied but not json").is_none());
    }

    #[test]
    fn a_denial_line_captured_from_rustfs_1_0_4_is_read() {
        let line = r#"{"timestamp":"2026-11-07T06:36:23.965138209-05:00","level":"WARN","message":"request denied by authorization layer","event":"s3_authorization_denied","reason":"no_policy_allows_action","action":"S3Action(GetObjectAction)","bucket":"denybkt","object":"x","version_id":"None","account":"wk-denytest","is_owner":false,"target":"rustfs::storage::access"}"#;
        let r = parse_denial(line).unwrap();
        assert_eq!((r.access_key.as_str(), r.bucket.as_str(), r.object.as_deref(), r.op.as_str()), ("wk-denytest", "denybkt", Some("x"), "GetObject"));
        assert_eq!(r.reason.as_deref(), Some("no_policy_allows_action"));
        assert_eq!(parse_denial(&line.replace("\"object\":\"x\"", "\"object\":\"None\"")).unwrap().object, None);
    }

    #[test]
    fn notification_records_are_standard_s3_events() {
        let b = r#"{"Records":[{"eventName":"s3:ObjectCreated:Put","userIdentity":{"principalId":"AK1"},"s3":{"bucket":{"name":"n-one"},"object":{"key":"k","size":7}}},{"eventName":"s3:ObjectRemoved:Delete","s3":{"bucket":{"name":"n-one"},"object":{"key":"j"}}}]}"#;
        let v = parse_notification(b);
        assert_eq!(v.len(), 2);
        assert_eq!((v[0].op.as_str(), v[0].bytes, v[0].object.as_deref()), ("ObjectCreated:Put", 7, Some("k")));
        assert!(parse_notification("garbage").is_empty());
    }

    #[test]
    fn audit_records_captured_from_rustfs_1_0_4_are_read() {
        let put = r#"{"EventName":"s3:ObjectCreated:Put","Key":"auditbkt/s3:PutObject","Records":[{"version":"1.0","time":1794117164509,"event":"s3:ObjectCreated:Put","trigger":"s3:PutObject","api":{"name":"s3:PutObject","bucket":"auditbkt","object":"k/obj.txt","status":"success","status_code":200,"time_to_response":"93.66ms","timeToResponseInNS":"93659887"},"remotehost":"127.0.0.1","requestID":"r1","userAgent":"curl/8.18.0","requestPath":"/auditbkt/k/obj.txt","requestHost":"127.0.0.1:19400","requestQuery":{"authorization":"***redacted***"},"responseHeader":{},"accessKey":"AK1"}]}"#;
        let r = parse_audit(put).unwrap();
        assert_eq!((r.op.as_str(), r.bucket.as_str(), r.object.as_deref(), r.status, r.access_key.as_str()), ("PutObject", "auditbkt", Some("k/obj.txt"), Some(200), "AK1"));
        let mk = r#"{"EventName":"s3:BucketCreated:*","Key":"auditbkt/s3:CreateBucket","Records":[{"event":"s3:BucketCreated:*","trigger":"s3:CreateBucket","api":{"name":"s3:CreateBucket","bucket":"auditbkt","status":"success","status_code":200},"accessKey":"AK1"}]}"#;
        let r = parse_audit(mk).unwrap();
        assert_eq!((r.op.as_str(), r.object, r.status), ("CreateBucket", None, Some(200)));
    }

    #[test]
    fn a_notification_captured_from_rustfs_1_0_4_is_read_and_its_key_decoded() {
        let b = r#"{"EventName":"s3:ObjectCreated:Put","Key":"auditbkt/c/obj.txt","Records":[{"eventVersion":"2.1","eventSource":"rustfs:s3","eventTime":"2026-11-08T05:48:46.452Z","eventName":"s3:ObjectCreated:Put","userIdentity":{"principalId":"AK1"},"s3":{"s3SchemaVersion":"1.0","configurationId":"Config","bucket":{"name":"auditbkt","ownerIdentity":{"principalId":"AK1"},"arn":"arn:aws:s3:::auditbkt"},"object":{"key":"c%2Fobj%20x.txt","size":6,"eTag":"b1946ac92492d2347c6235b4d2611184","sequencer":"18E5FBD6C446E271"}}}]}"#;
        let v = parse_notification(b);
        assert_eq!(v.len(), 1);
        assert_eq!((v[0].op.as_str(), v[0].bucket.as_str(), v[0].object.as_deref(), v[0].bytes, v[0].access_key.as_str()), ("ObjectCreated:Put", "auditbkt", Some("c/obj x.txt"), 6, "AK1"));
        assert_eq!(percent_decode("100%"), "100%", "a bare percent stays");
    }
    /// Two records exactly as a throwaway RustFS 1.0.4 container posted them to the audit webhook
    /// (2026-11-10: CreateBucket, then PutObject of `k/obj.txt`, root credential). The access key is
    /// the throwaway's; the request headers arrive redacted. See `testdata/`.
    #[test]
    fn a_fixture_of_real_rustfs_1_0_4_audit_records_is_read() {
        let lines: Vec<&str> = include_str!("../testdata/rustfs-1.0.4-audit.ndjson").lines().collect();
        assert_eq!(lines.len(), 2);
        let mk = parse_audit(lines[0]).unwrap();
        assert_eq!((mk.op.as_str(), mk.bucket.as_str(), mk.object.as_deref(), mk.status), ("CreateBucket", "auditbkt", None, Some(200)));
        let put = parse_audit(lines[1]).unwrap();
        assert_eq!((put.op.as_str(), put.bucket.as_str(), put.object.as_deref(), put.status), ("PutObject", "auditbkt", Some("k/obj.txt"), Some(200)));
        assert!(put.access_key.starts_with("w2proot"), "the record names the caller's key: {}", put.access_key);
        for l in &lines {
            assert!(!l.contains("Credential=") && !l.contains("Signature="), "no signed header text in the fixture");
        }
    }
}
