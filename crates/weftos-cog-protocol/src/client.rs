//! Synchronous Unix-socket client for `cog.check_run`.
//!
//! One JSON object per line, the same framing as the daemon. This module does
//! not depend on `clawft-rpc`: the cog crate must build without the kernel.
//! `proto` is always [`crate::CLIENT_PROTO`]. Auth is omitted. `cog.check_run`
//! is a read, and an absent token stays anonymous rather than becoming admin.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use crate::wire::{request_json, CallError};
use crate::{CheckRunParams, CheckRunResult, PROTOCOL, RefusalCode};

const MAX_LINE: usize = 1024 * 1024;

/// One check against a daemon socket.
#[derive(Debug, Clone)]
pub struct DaemonCheck {
    socket: PathBuf,
    timeout: Duration,
    id: String,
}

impl DaemonCheck {
    /// Check `socket` with [`crate::DEFAULT_TIMEOUT`] and id `cog-check-1`.
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
            timeout: crate::DEFAULT_TIMEOUT,
            id: "cog-check-1".into(),
        }
    }

    /// Bound the connect, write, and read.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Request id echoed by the daemon.
    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = id.into();
        self
    }

    /// The socket this check will open.
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Send `params` and read one reply line.
    pub fn check(&self, params: &CheckRunParams) -> Result<CheckRunResult, CallError> {
        let mut stream = UnixStream::connect(&self.socket).map_err(connect_err)?;
        stream.set_read_timeout(Some(self.timeout)).map_err(io_err)?;
        stream.set_write_timeout(Some(self.timeout)).map_err(io_err)?;
        let line = request_json(&self.id, params);
        stream.write_all(line.as_bytes()).map_err(io_err)?;
        stream.write_all(b"\n").map_err(io_err)?;
        stream.flush().map_err(io_err)?;
        let bytes = read_line(&mut stream)?;
        parse_reply(&self.id, params, &bytes)
    }
}

fn connect_err(e: std::io::Error) -> CallError {
    if e.kind() == std::io::ErrorKind::TimedOut {
        CallError::Timeout
    } else {
        CallError::DaemonUnavailable(e.to_string())
    }
}

fn io_err(e: std::io::Error) -> CallError {
    if e.kind() == std::io::ErrorKind::TimedOut || e.kind() == std::io::ErrorKind::WouldBlock {
        CallError::Timeout
    } else {
        CallError::DaemonUnavailable(e.to_string())
    }
}

fn read_line(stream: &mut UnixStream) -> Result<Vec<u8>, CallError> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(0) if buf.is_empty() => {
                return Err(CallError::DaemonUnavailable(
                    "daemon closed connection without response".into(),
                ));
            }
            Ok(0) => return Err(CallError::MalformedReply("reply has no newline".into())),
            Ok(_) if byte[0] == b'\n' => return Ok(buf),
            Ok(_) => {
                if buf.len() >= MAX_LINE {
                    return Err(CallError::MalformedReply("reply exceeds 1 MiB".into()));
                }
                buf.push(byte[0]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(io_err(e)),
        }
    }
}

#[derive(Deserialize)]
struct Envelope {
    ok: bool,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_kind: Option<String>,
    #[serde(default)]
    id: Option<String>,
}

fn parse_reply(id: &str, params: &CheckRunParams, bytes: &[u8]) -> Result<CheckRunResult, CallError> {
    let env: Envelope = serde_json::from_slice(bytes)
        .map_err(|e| CallError::MalformedReply(format!("reply is not json: {e}")))?;
    if env.id.as_deref().is_some_and(|got| got != id) {
        return Err(CallError::MalformedReply(format!(
            "reply id {} does not match {id}",
            env.id.unwrap_or_default()
        )));
    }
    if env.ok {
        let result = env.result.ok_or_else(|| CallError::MalformedReply("success has no result".into()))?;
        return parse_result(params, result);
    }
    let kind = env.error_kind.unwrap_or_default();
    let message = env.error.unwrap_or_else(|| kind.clone());
    match kind.as_str() {
        "proto_mismatch" | "version_mismatch" => Err(CallError::VersionMismatch(message)),
        "licence_not_here" => Err(CallError::Denial { code: RefusalCode::NotHolder, message }),
        "" => Err(CallError::MalformedReply("refusal has no error_kind".into())),
        other => match RefusalCode::parse(other) {
            Some(code) => Err(CallError::Denial { code, message }),
            None => Err(CallError::MalformedReply(format!("unknown error_kind {other}"))),
        },
    }
}

fn parse_result(params: &CheckRunParams, result: Value) -> Result<CheckRunResult, CallError> {
    let protocol = result.get("protocol").and_then(Value::as_str);
    match protocol {
        Some(p) if p == PROTOCOL => {}
        Some(p) => {
            return Err(CallError::VersionMismatch(format!("result protocol is {p}")));
        }
        None => return Err(CallError::MalformedReply("result has no protocol".into())),
    }
    let parsed: CheckRunResult = serde_json::from_value(result)
        .map_err(|e| CallError::MalformedReply(format!("result verdict: {e}")))?;
    if let CheckRunResult::Permit { blake3, .. } = &parsed
        && blake3 != &params.blake3
    {
        return Err(CallError::MalformedReply("permit blake3 does not match the request".into()));
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{proto_mismatch_json, success_json};
    use crate::CheckRunResult;
    use std::io::{Read, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    fn sample() -> CheckRunParams {
        CheckRunParams::new("ld2450-radar", "0.1.0", "ab".repeat(32), "cd".repeat(32)).unwrap()
    }

    enum Script {
        /// Read the request, then write these bytes.
        Write(Vec<u8>),
        /// Read the request, then hold the connection until stopped.
        Hold,
        /// Accept and close with no bytes.
        Close,
    }

    struct Server {
        socket: PathBuf,
        stop: Arc<AtomicBool>,
        seen: Arc<Mutex<String>>,
        thread: Option<thread::JoinHandle<()>>,
    }

    impl Server {
        fn start(script: Script) -> Self {
            static N: AtomicU64 = AtomicU64::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let socket = PathBuf::from(format!("/tmp/weft-cog-client-{}-{n}.sock", std::process::id()));
            let _ = std::fs::remove_file(&socket);
            let listener = UnixListener::bind(&socket).unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let seen = Arc::new(Mutex::new(String::new()));
            let stop2 = Arc::clone(&stop);
            let seen2 = Arc::clone(&seen);
            let thread = thread::spawn(move || {
                let Ok((mut stream, _)) = listener.accept() else { return };
                if stop2.load(Ordering::SeqCst) {
                    return;
                }
                match script {
                    Script::Close => drop(stream),
                    Script::Write(bytes) => {
                        let _ = read_request(&mut stream, &seen2);
                        let _ = stream.write_all(&bytes);
                    }
                    Script::Hold => {
                        let _ = read_request(&mut stream, &seen2);
                        while !stop2.load(Ordering::SeqCst) {
                            thread::sleep(Duration::from_millis(20));
                        }
                    }
                }
            });
            Self { socket, stop, seen, thread: Some(thread) }
        }

        fn socket(&self) -> &Path {
            &self.socket
        }
    }

    fn read_request(stream: &mut UnixStream, seen: &Mutex<String>) {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            match stream.read(&mut byte) {
                Ok(0) => break,
                Ok(_) if byte[0] == b'\n' => break,
                Ok(_) => buf.push(byte[0]),
                Err(_) => break,
            }
        }
        *seen.lock().unwrap() = String::from_utf8_lossy(&buf).into_owned();
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            let _ = UnixStream::connect(&self.socket);
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
            let _ = std::fs::remove_file(&self.socket);
        }
    }

    fn wait_bound(path: &Path) {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !path.exists() {
            assert!(std::time::Instant::now() < deadline, "socket {}", path.display());
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn unavailable_daemon_malformed_reply_timeout_denial_and_version_mismatch() {
        let params = sample();
        let missing = PathBuf::from(format!("/tmp/weft-cog-missing-{}-{}.sock", std::process::id(), 9));
        let _ = std::fs::remove_file(&missing);
        let err = DaemonCheck::new(&missing).check(&params).unwrap_err();
        assert_eq!(err.code(), "daemon_unavailable");

        let closed = Server::start(Script::Close);
        wait_bound(closed.socket());
        let err = DaemonCheck::new(closed.socket()).check(&params).unwrap_err();
        assert_eq!(err.code(), "daemon_unavailable");

        let bad = Server::start(Script::Write(b"not-json\n".to_vec()));
        wait_bound(bad.socket());
        let err = DaemonCheck::new(bad.socket()).check(&params).unwrap_err();
        assert_eq!(err.code(), "malformed_reply");
        assert_eq!(bad.seen.lock().unwrap().as_str(), request_json("cog-check-1", &params));

        let hold = Server::start(Script::Hold);
        wait_bound(hold.socket());
        let err = DaemonCheck::new(hold.socket())
            .with_timeout(Duration::from_millis(150))
            .check(&params)
            .unwrap_err();
        assert_eq!(err.code(), "timeout");

        let denial = format!(
            "{}\n",
            crate::refusal_json("cog-check-1", "hash_revoked", "the binary's hash is revoked")
        );
        let denied = Server::start(Script::Write(denial.into_bytes()));
        wait_bound(denied.socket());
        let err = DaemonCheck::new(denied.socket()).check(&params).unwrap_err();
        assert_eq!(err.code(), "hash_revoked");

        let mismatch = format!("{}\n", proto_mismatch_json("cog-check-1"));
        let skewed = Server::start(Script::Write(mismatch.into_bytes()));
        wait_bound(skewed.socket());
        let err = DaemonCheck::new(skewed.socket()).check(&params).unwrap_err();
        assert_eq!(err.code(), "version_mismatch");

        let wrong = b"{\"ok\":true,\"result\":{\"protocol\":\"weftos.cog.v0\",\"verdict\":\"not_seed_bound\"},\"id\":\"cog-check-1\"}\n";
        let old = Server::start(Script::Write(wrong.to_vec()));
        wait_bound(old.socket());
        let err = DaemonCheck::new(old.socket()).check(&params).unwrap_err();
        assert_eq!(err.code(), "version_mismatch");
    }

    #[test]
    fn permit_and_not_seed_bound_round_trip_and_a_mismatched_blake3_is_malformed() {
        let params = sample();
        let ok = format!("{}\n", success_json("cog-check-1", &CheckRunResult::NotSeedBound));
        let server = Server::start(Script::Write(ok.into_bytes()));
        wait_bound(server.socket());
        assert_eq!(
            DaemonCheck::new(server.socket()).check(&params).unwrap(),
            CheckRunResult::NotSeedBound
        );

        let permit = CheckRunResult::Permit {
            grant_id: "g1".into(),
            approval_id: "a1".into(),
            blake3: params.blake3.clone(),
        };
        let ok = format!("{}\n", success_json("cog-check-1", &permit));
        let server = Server::start(Script::Write(ok.into_bytes()));
        wait_bound(server.socket());
        assert_eq!(DaemonCheck::new(server.socket()).check(&params).unwrap(), permit);

        let forged = CheckRunResult::Permit {
            grant_id: "g1".into(),
            approval_id: "a1".into(),
            blake3: "ab".repeat(32),
        };
        let ok = format!("{}\n", success_json("cog-check-1", &forged));
        let server = Server::start(Script::Write(ok.into_bytes()));
        wait_bound(server.socket());
        let err = DaemonCheck::new(server.socket()).check(&params).unwrap_err();
        assert_eq!(err.code(), "malformed_reply");
    }
}
