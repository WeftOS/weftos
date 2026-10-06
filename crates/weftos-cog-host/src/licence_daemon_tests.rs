//! Host start codes when the daemon is missing, slow, or wrong.
//! Policy allow and deny stay in `licence_tests.rs` against the test listener.

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use clawft_kernel::licence::MeshId;
use ed25519_dalek::SigningKey;
use weftos_cog_protocol::{proto_mismatch_json, refusal_json, success_json, CheckRunResult};

use super::*;

fn ready(socket: &Path, timeout: Duration) -> (tempfile::TempDir, HostLicence) {
    let dir = tempfile::tempdir().unwrap();
    let lic_dir = dir.path().join(".licence");
    std::fs::create_dir_all(&lic_dir).unwrap();
    let mesh = MeshId::derive(&[9; 32], &[7; 32]);
    std::fs::write(lic_dir.join(CONFIG_FILE), serde_json::json!({"mesh_id": mesh.to_hex()}).to_string()).unwrap();
    let key = SigningKey::from_bytes(&[1; 32]);
    let public_key: String = key.verifying_key().to_bytes().iter().map(|b| format!("{b:02x}")).collect();
    let trust = serde_json::json!({
        "schema": "weftos.workload-trust.v1",
        "operator_keys": [{"key_id": "operator-1", "public_key": public_key}],
    });
    std::fs::write(lic_dir.join(TRUST_FILE), trust.to_string()).unwrap();
    let lic = HostLicence::with_clock(lic_dir, Arc::new(|| 1_800_000_000))
        .with_daemon_socket(socket)
        .with_daemon_timeout(timeout);
    (dir, lic)
}

fn record() -> CogRecord {
    CogRecord {
        id: "fall-detect".into(),
        version: "1.2.0".into(),
        source: Source::Cognitum,
        enabled: false,
        binary: "cog-fall-detect-arm".into(),
        args: vec![],
        signed: false,
    }
}

enum Script {
    Write(Vec<u8>),
    Hold,
    Close,
}

struct Server {
    socket: PathBuf,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Server {
    fn start(script: Script) -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let socket = PathBuf::from(format!("/tmp/weft-cog-daemon-{}-{n}.sock", std::process::id()));
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else { return };
            if stop2.load(Ordering::SeqCst) {
                return;
            }
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            match script {
                Script::Close => drop(stream),
                Script::Write(bytes) => {
                    let _ = read_request(&mut stream);
                    let _ = stream.write_all(&bytes);
                }
                Script::Hold => {
                    let _ = read_request(&mut stream);
                    while !stop2.load(Ordering::SeqCst) {
                        thread::sleep(Duration::from_millis(20));
                    }
                }
            }
        });
        Self { socket, stop, thread: Some(thread) }
    }

    fn socket(&self) -> &Path {
        &self.socket
    }
}

fn read_request(stream: &mut UnixStream) {
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(0) => break,
            Ok(_) if byte[0] == b'\n' => break,
            Ok(_) => {}
            Err(_) => break,
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = UnixStream::connect(&self.socket);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
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

fn refused(lic: &HostLicence) -> StartRefused {
    check_start(lic, &record(), b"cog binary bytes").unwrap_err()
}

#[test]
fn unavailable_malformed_timeout_denial_and_version_mismatch() {
    let missing = PathBuf::from(format!("/tmp/weft-cog-host-missing-{}-7.sock", std::process::id()));
    let _ = std::fs::remove_file(&missing);
    let (_dir, lic) = ready(&missing, Duration::from_secs(2));
    let err = refused(&lic);
    assert_eq!(err.code, "daemon_unavailable");
    assert!(err.reason.contains("start the local WeftOS daemon"), "{}", err.reason);

    let closed = Server::start(Script::Close);
    wait_bound(closed.socket());
    let (_dir, lic) = ready(closed.socket(), Duration::from_secs(2));
    assert_eq!(refused(&lic).code, "daemon_unavailable");

    let bad = Server::start(Script::Write(b"not-json\n".to_vec()));
    wait_bound(bad.socket());
    let (_dir, lic) = ready(bad.socket(), Duration::from_secs(2));
    assert_eq!(refused(&lic).code, "malformed_reply");

    let hold = Server::start(Script::Hold);
    wait_bound(hold.socket());
    let (_dir, lic) = ready(hold.socket(), Duration::from_millis(150));
    assert_eq!(refused(&lic).code, "timeout");

    let denial = format!("{}\n", refusal_json("cog-check-1", "hash_revoked", "the binary's hash is revoked"));
    let denied = Server::start(Script::Write(denial.into_bytes()));
    wait_bound(denied.socket());
    let (_dir, lic) = ready(denied.socket(), Duration::from_secs(2));
    let err = refused(&lic);
    assert_eq!(err.code, "hash_revoked");
    assert!(err.reason.contains("the binary's hash is revoked"), "{}", err.reason);

    let mismatch = format!("{}\n", proto_mismatch_json("cog-check-1"));
    let skewed = Server::start(Script::Write(mismatch.into_bytes()));
    wait_bound(skewed.socket());
    let (_dir, lic) = ready(skewed.socket(), Duration::from_secs(2));
    assert_eq!(refused(&lic).code, "version_mismatch");
}

#[test]
fn an_explicit_socket_wins_over_the_runtime_dir() {
    let body = format!("{}\n", success_json("cog-check-1", &CheckRunResult::NotSeedBound));
    let server = Server::start(Script::Write(body.into_bytes()));
    wait_bound(server.socket());
    // The isolated test runtime sets WEFTOS_RUNTIME_DIR to a directory with
    // no kernel.sock. A Ready host that ignored the explicit socket would
    // report daemon_unavailable. This assignment happens before the check,
    // on this test process only.
    unsafe { std::env::set_var("WEFTOS_RUNTIME_DIR", "/tmp/weft-cog-absent-runtime") };
    let (_dir, lic) = ready(server.socket(), Duration::from_secs(2));
    let err = refused(&lic);
    assert_eq!(err.code, "binding_inactive");
    assert!(err.reason.contains("no binding yet"), "{}", err.reason);
}
