//! Per-test `cog.check_run` listener. One short socket under `/tmp`
//! (macOS `sun_path` is 104 bytes). Each request opens a fresh
//! [`HostLicence`] so a disk write or a clock change is visible.
//! The reply is the raw kernel verdict. The host under test applies overrides.

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use clawft_kernel::licence::{Clock, RunRequest, RunVerdict};
use weftos_cog_protocol::{refusal_json, success_json, CheckRunParams, CheckRunResult, CHECK_RUN_METHOD};

use super::HostLicence;

pub(super) struct Gate {
    socket: PathBuf,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Gate {
    pub(super) fn start(dir: PathBuf, clock: Clock) -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let socket = PathBuf::from(format!("/tmp/weft-cog-host-{}-{n}.sock", std::process::id()));
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).unwrap_or_else(|e| panic!("bind {}: {e}", socket.display()));
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            while !stop2.load(Ordering::SeqCst) {
                let Ok((mut stream, _)) = listener.accept() else { break };
                if stop2.load(Ordering::SeqCst) {
                    break;
                }
                serve(&mut stream, &dir, &clock);
            }
        });
        Self { socket, stop, thread: Some(thread) }
    }

    pub(super) fn socket(&self) -> &Path {
        &self.socket
    }
}

impl Drop for Gate {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = UnixStream::connect(&self.socket);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = std::fs::remove_file(&self.socket);
    }
}

fn serve(stream: &mut UnixStream, dir: &Path, clock: &Clock) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let Ok(line) = read_line(stream) else { return };
    let reply = reply_line(&line, dir, clock);
    let _ = stream.write_all(reply.as_bytes());
    let _ = stream.write_all(b"\n");
}

fn read_line(stream: &mut UnixStream) -> Result<String, ()> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(0) => return Err(()),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(()),
            Ok(_) if byte[0] == b'\n' => {
                return String::from_utf8(buf).map_err(|_| ());
            }
            Ok(_) => {
                if buf.len() >= 1024 * 1024 {
                    return Err(());
                }
                buf.push(byte[0]);
            }
        }
    }
}

fn reply_line(line: &str, dir: &Path, clock: &Clock) -> String {
    let value: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return refusal_json("cog-check-1", "malformed_reply", "request is not json"),
    };
    let id = value.get("id").and_then(|v| v.as_str()).unwrap_or("cog-check-1");
    if value.get("method").and_then(|v| v.as_str()) != Some(CHECK_RUN_METHOD) {
        return refusal_json(id, "malformed_reply", "method is not cog.check_run");
    }
    let Some(params) = value.get("params") else {
        return refusal_json(id, "malformed_reply", "request has no params");
    };
    let field = |name: &str| params.get(name).and_then(|v| v.as_str()).unwrap_or("");
    let Ok(parsed) = CheckRunParams::new(field("cog_id"), field("version"), field("sha256"), field("blake3")) else {
        return refusal_json(id, "malformed_reply", "params rejected");
    };
    let req = RunRequest {
        cog_id: &parsed.cog_id,
        version: &parsed.version,
        sha256: &parsed.sha256,
        blake3: &parsed.blake3,
    };
    let lic = HostLicence::with_clock(dir.to_path_buf(), Arc::clone(clock));
    match lic.evaluate_local(&req) {
        Ok(RunVerdict::NotSeedBound) => success_json(id, &CheckRunResult::NotSeedBound),
        Ok(RunVerdict::Permit(permit)) => success_json(
            id,
            &CheckRunResult::Permit {
                grant_id: permit.grant_id,
                approval_id: permit.approval_id,
                blake3: permit.blake3,
            },
        ),
        Err(err) => refusal_json(id, err.code(), &err.to_string()),
    }
}
