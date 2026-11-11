//! What the RustFS backend puts on the wire for the execute attribute, against a loopback fake S3
//! that records each request's headers. The real round trip is in `live_exec_attribute_*` in
//! `rustfs_tests.rs` (needs a throwaway RustFS).

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::*;
use crate::backend::{ObjectAttrs, PutOpts};

const EXEC_HDR: &str = "x-amz-meta-weft-exec";

struct Seen {
    line: String,
    headers: Vec<(String, String)>,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

fn http(headers: &[(&str, &str)], body: &str) -> String {
    let mut r = String::from("HTTP/1.1 200 OK\r\n");
    for (k, v) in headers {
        r.push_str(&format!("{k}: {v}\r\n"));
    }
    r.push_str(&format!("Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()));
    r
}

/// A one-thread fake S3: records every request, answers with `respond(request line)`.
fn fake(respond: impl Fn(&str) -> String + Send + 'static) -> (RustfsBackend, Arc<Mutex<Vec<Seen>>>, tempfile::TempDir) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    std::thread::spawn(move || {
        for conn in l.incoming() {
            let Ok(mut c) = conn else { break };
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            let (head_end, want) = loop {
                let n = c.read(&mut chunk).unwrap_or(0);
                if n == 0 {
                    break (buf.len(), 0);
                }
                buf.extend_from_slice(&chunk[..n]);
                if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..i]).to_lowercase();
                    let len = head.lines().find_map(|l| l.strip_prefix("content-length:")).and_then(|v| v.trim().parse::<usize>().ok()).unwrap_or(0);
                    break (i + 4, len);
                }
            };
            while buf.len() < head_end + want {
                let n = c.read(&mut chunk).unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            let text = String::from_utf8_lossy(&buf[..head_end]).into_owned();
            let mut lines = text.split("\r\n");
            let line = lines.next().unwrap_or("").to_owned();
            let headers = lines.filter_map(|h| h.split_once(": ")).map(|(k, v)| (k.to_lowercase(), v.to_owned())).collect();
            let reply = respond(&line);
            log.lock().unwrap().push(Seen { line, headers });
            let _ = c.write_all(reply.as_bytes());
        }
    });
    let d = tempfile::tempdir().unwrap();
    let cfg = RustfsConfig {
        endpoint: format!("http://127.0.0.1:{port}"),
        region: "us-east-1".into(),
        root_access_key: "a".into(),
        root_secret_key: "b".into(),
        secrets_dir: d.path().join("s"),
        timeout: Duration::from_secs(5),
    };
    (RustfsBackend::new(cfg).unwrap().with_retry(Retry { attempts: 1, base: Duration::from_millis(1) }), seen, d)
}

fn ok_put() -> String {
    http(&[("ETag", "\"abc\""), ("x-amz-version-id", "v1")], "")
}

const EXEC: ObjectAttrs = ObjectAttrs { executable: true };

#[test]
fn a_locked_put_carries_the_execute_attribute_only_when_set() {
    let (be, seen, _d) = fake(|_| ok_put());
    let lock = PutOpts { retention: Some(Retention { mode: LockMode::Governance, until_unix_s: 4_000_000_000 }), ..Default::default() };
    be.put_locked("b-x", "bin/run", b"#!/bin/sh\n", &PutOpts { attrs: EXEC, ..lock }).unwrap();
    be.put_locked("b-x", "plain", b"data", &lock).unwrap();
    let s = seen.lock().unwrap();
    assert_eq!(s[0].header(EXEC_HDR), Some("1"), "{}", s[0].line);
    assert!(s[0].header("x-amz-object-lock-mode").is_some(), "the lock headers are still sent");
    assert_eq!(s[1].header(EXEC_HDR), None);
}

#[test]
fn a_streamed_object_that_fits_one_part_carries_the_execute_attribute() {
    let (be, seen, _d) = fake(|_| ok_put());
    be.put_stream("b-x", "bin/run", &mut std::io::Cursor::new(b"#!/bin/sh\n".to_vec()), crate::backend::MIN_PART, &PutOpts { attrs: EXEC, ..Default::default() }).unwrap();
    let s = seen.lock().unwrap();
    assert_eq!(s[0].header(EXEC_HDR), Some("1"), "{}", s[0].line);
}

#[test]
fn a_multipart_upload_sends_the_execute_attribute_when_it_is_created() {
    let (be, seen, _d) = fake(|line| {
        if line.contains("uploads") {
            http(&[], "<InitiateMultipartUploadResult><UploadId>u1</UploadId></InitiateMultipartUploadResult>")
        } else {
            ok_put()
        }
    });
    assert_eq!(be.create_multipart("b-x", "big/run", &PutOpts { attrs: EXEC, ..Default::default() }).unwrap(), "u1");
    assert_eq!(be.create_multipart("b-x", "big/data", &PutOpts::default()).unwrap(), "u1");
    let s = seen.lock().unwrap();
    assert!(s[0].line.starts_with("POST ") && s[0].line.contains("uploads"), "{}", s[0].line);
    assert_eq!(s[0].header(EXEC_HDR), Some("1"));
    assert_eq!(s[1].header(EXEC_HDR), None);
}

#[test]
fn a_listing_carries_each_objects_modification_stamp() {
    let xml = "<ListBucketResult><IsTruncated>false</IsTruncated>\
        <Contents><Key>p/a</Key><LastModified>2026-11-10T12:00:00.123Z</LastModified><ETag>\"e1\"</ETag><Size>3</Size></Contents>\
        <Contents><Key>p/b</Key><LastModified>2026-11-10T12:00:05.000Z</LastModified><ETag>\"e2\"</ETag><Size>4</Size></Contents>\
        </ListBucketResult>";
    let (be, _seen, _d) = fake(move |_| http(&[], xml));
    let l = be.list_stamped("b-x", "p/").unwrap();
    assert_eq!(l.iter().map(|(m, s)| (m.key.as_str(), m.size, s.as_str())).collect::<Vec<_>>(), [("p/a", 3, "2026-11-10T12:00:00.123Z"), ("p/b", 4, "2026-11-10T12:00:05.000Z")]);
    assert_eq!(be.list("b-x", "p/").unwrap().len(), 2, "list is the same listing without the stamps");
}
