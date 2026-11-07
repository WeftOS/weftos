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

/// One line of RustFS's audit stream.
pub fn parse_audit(line: &str) -> Option<Raw> {
    let v: Value = serde_json::from_str(line.trim()).ok()?;
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

/// One `s3_authorization_denied` line of RustFS's JSON log.
pub fn parse_denial(line: &str) -> Option<Raw> {
    if !line.contains("s3_authorization_denied") {
        return None;
    }
    let v: Value = serde_json::from_str(line.trim()).ok()?;
    Some(Raw {
        access_key: text(&v, &["account", "accessKey", "access_key"]).unwrap_or_default(),
        bucket: text(&v, &["bucket"]).unwrap_or_default(),
        object: text(&v, &["object", "key"]),
        op: text(&v, &["action"]).map(|a| a.trim_start_matches("s3:").to_owned()).unwrap_or_default(),
        status: Some(403),
        bytes: 0,
        reason: text(&v, &["reason"]),
    })
}

/// The records of an S3 event notification (`{"Records": [...]}`): event name, bucket, key, size, identity.
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
                object: r["s3"]["object"]["key"].as_str().map(str::to_owned),
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
    fn notification_records_are_standard_s3_events() {
        let b = r#"{"Records":[{"eventName":"s3:ObjectCreated:Put","userIdentity":{"principalId":"AK1"},"s3":{"bucket":{"name":"n-one"},"object":{"key":"k","size":7}}},{"eventName":"s3:ObjectRemoved:Delete","s3":{"bucket":{"name":"n-one"},"object":{"key":"j"}}}]}"#;
        let v = parse_notification(b);
        assert_eq!(v.len(), 2);
        assert_eq!((v[0].op.as_str(), v[0].bytes, v[0].object.as_deref()), ("ObjectCreated:Put", 7, Some("k")));
        assert!(parse_notification("garbage").is_empty());
    }
}
