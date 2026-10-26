//! Test fixtures. Everything is a fake: HTTP servers on random loopback
//! ports (wiremock), launcher scripts in temp dirs, and model directories
//! of a few bytes. Nothing here may touch a real server, `~/llm`, an HF
//! cache or Ollama data, and no test uses a default port.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::config::{InferConfig, ManagedConfig, RestartPolicy};
use super::runtime::InferRuntime;
use super::spec::{InferFlavor, InferenceSpec};
use crate::model_manifest::{
    AdoptInput, ModelFormat, ModelRegistry, ModelSource, scan_dir, scan_file,
};
use crate::workload_pkg::codec::hex_encode;
use crate::workload_pkg::{KeyOrigin, TrustAnchors, key_id_for};
use crate::workload_runtime::host_contract::HostContract;
use crate::workload_runtime::types::{
    InstanceHandle, RunMode, VerifiedWorkload, WorkloadConfig, WorkloadRuntime,
};

/// A port nothing listens on, that no parallel test will be handed.
///
/// Asking the OS for port 0 and dropping the listener races: the OS may
/// give the same ephemeral port to another test (each test is its own
/// process under nextest) before this one binds it. Ports here come from
/// below the OS ephemeral range, walking up from a start owned by this
/// process (`pid % 1000`, 16 ports apart) and are checked to be bindable,
/// so two processes only share a port if their pids collide mod 1000, and
/// then the bind check skips what the other already holds. Within one
/// process (plain `cargo test` runs every test as a thread of it) a port is
/// handed out once and not again until the walk has covered the whole
/// 20 000-port range: a test that has not yet started its fake server on the
/// port it was given must not see the same port given to another test.
pub fn free_port() -> u16 {
    use std::sync::atomic::{AtomicU32, Ordering};
    const LOW: u32 = 10_000;
    const SPAN: u32 = 20_000;
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let start = (std::process::id() % 1000) * 16;
    loop {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let port = (LOW + (start + n) % SPAN) as u16;
        if TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
        if n > SPAN {
            // The whole range is held: fall back to the OS.
            return TcpListener::bind(("127.0.0.1", 0)).unwrap().local_addr().unwrap().port();
        }
    }
}

/// A fake server bound to a chosen free port (so the adapter's "port is
/// free" guard can be satisfied first and the server brought up later).
pub async fn server_on(port: u16) -> MockServer {
    let l = TcpListener::bind(("127.0.0.1", port)).unwrap();
    MockServer::builder().listener(l).start().await
}

pub fn port_of(s: &MockServer) -> u16 {
    s.address().port()
}

/// llama-server: `/health` with `status`, `/v1/models` listing `model`.
pub async fn mount_llama(s: &MockServer, status: u16, model: &str) {
    Mock::given(method("GET"))
        .and(path("/health"))
        .respond_with(ResponseTemplate::new(status).set_body_json(json!({"status": "ok"})))
        .mount(s)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [{"id": model}]})))
        .mount(s)
        .await;
}

/// Ollama: version, installed tags and resident models.
pub async fn mount_ollama(s: &MockServer, tags: &[&str], resident: &[&str]) {
    let list =
        |v: &[&str]| json!({"models": v.iter().map(|n| json!({"name": n})).collect::<Vec<_>>()});
    Mock::given(method("GET"))
        .and(path("/api/version"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"version": "0.0.0-test"})))
        .mount(s)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(200).set_body_json(list(tags)))
        .mount(s)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/ps"))
        .respond_with(ResponseTemplate::new(200).set_body_json(list(resident)))
        .mount(s)
        .await;
}

/// A launcher that records its argv and pid beside itself and then idles.
pub fn write_script(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join("fake-serve");
    std::fs::write(
        &p,
        "#!/bin/sh\nd=$(dirname \"$0\")\nprintf '%s\\n' \"$@\" > \"$d/argv.txt\"\nprintf '%s' \"$PATH\" > \"$d/path.txt\"\necho $$ > \"$d/pid.txt\"\nexec sleep 600\n",
    )
    .unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

/// Replace the launcher with one that does not exec: it forks a grandchild
/// (`gpid.txt`) and waits, so the leader and the grandchild are separate
/// processes in one group.
pub fn write_forking_script(script: &Path) {
    std::fs::write(
        script,
        "#!/bin/sh\nd=$(dirname \"$0\")\nsleep 600 &\necho $! > \"$d/gpid.txt\"\necho $$ > \"$d/pid.txt\"\nwait\n",
    )
    .unwrap();
}

/// Wait for the script's pid file and return the pid.
pub async fn script_pid(dir: &Path) -> i32 {
    let f = dir.join("pid.txt");
    // Up to 20 s: a loaded test machine can be slow to start a shell.
    for _ in 0..800 {
        if let Ok(s) = std::fs::read_to_string(&f)
            && let Ok(p) = s.trim().parse()
        {
            return p;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("script never wrote its pid");
}

pub fn argv(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("argv.txt"))
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect()
}

pub fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks existence of a process this test started.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Wait (up to 20 s, polling) until none of `pids` exists. Death is not
/// instant on a loaded machine: an orphan is a zombie until init reaps it.
pub async fn wait_dead(pids: &[i32]) -> bool {
    for _ in 0..400 {
        if pids.iter().all(|p| !alive(*p)) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

/// Kill a process this test's adapter started (simulates a crash).
pub fn crash(pid: i32) {
    // SAFETY: the pid is the fake launcher the adapter under test spawned.
    unsafe { libc::kill(pid, libc::SIGKILL) };
}

pub fn operator() -> (ed25519_dalek::SigningKey, String, TrustAnchors) {
    let key = ed25519_dalek::SigningKey::from_bytes(&[9u8; 32]);
    let pk = key.verifying_key().to_bytes();
    let id = key_id_for(&pk);
    let mut anchors = TrustAnchors::default();
    anchors
        .push_signer(&id, &hex_encode(&pk), KeyOrigin::Operator)
        .unwrap();
    (key, id, anchors)
}

/// Adopt a fake model of `format` into `reg`. Returns its directory.
pub fn adopt_model(reg: &ModelRegistry, root: &Path, name: &str, format: ModelFormat) -> PathBuf {
    let (key, id, anchors) = operator();
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    let mut source = ModelSource {
        hf_repo: Some("example-org/example-model".into()),
        ..ModelSource::default()
    };
    let scanned = match format {
        ModelFormat::Gguf | ModelFormat::Ollama => {
            let f = dir.join(format!("{name}.gguf"));
            std::fs::write(&f, vec![3u8; 2048]).unwrap();
            if format == ModelFormat::Ollama {
                source = ModelSource {
                    ollama_tag: Some(format!("{name}:latest")),
                    ..ModelSource::default()
                };
            }
            scan_file(
                &f,
                AdoptInput {
                    name: name.into(),
                    format,
                    source,
                    redistributable: false,
                },
            )
            .unwrap()
        }
        _ => {
            std::fs::write(dir.join("model.safetensors"), vec![4u8; 2048]).unwrap();
            std::fs::write(dir.join("tokenizer.json"), b"{}").unwrap();
            scan_dir(
                &dir,
                AdoptInput {
                    name: name.into(),
                    format,
                    source,
                    redistributable: false,
                },
            )
            .unwrap()
        }
    };
    reg.adopt(scanned, &key, &id, &anchors, false).unwrap();
    dir
}

pub fn wl_cfg() -> WorkloadConfig {
    WorkloadConfig {
        mode: RunMode::Listener,
        args: vec![],
        host: HostContract::default_feed(),
        node_id: "n1".into(),
    }
}

pub fn spec(role: &str, flavor: InferFlavor, port: u16) -> InferenceSpec {
    let mut s = InferenceSpec::new(role, flavor);
    s.serve.port = Some(port);
    s
}

pub fn workload(s: InferenceSpec) -> VerifiedWorkload {
    VerifiedWorkload::inference(s).unwrap()
}

pub fn adopted(flavor: InferFlavor) -> InferRuntime {
    InferRuntime::new(InferConfig::adopted(flavor))
}

/// A managed adapter over a fresh registry in `tmp`, launching `script`.
pub struct Managed {
    pub rt: Arc<InferRuntime>,
    pub reg: Arc<ModelRegistry>,
    pub tmp: tempfile::TempDir,
    pub script_dir: PathBuf,
    pub data: PathBuf,
}

pub fn managed(flavor: InferFlavor, restart: RestartPolicy) -> Managed {
    managed_with(flavor, restart, |_| {})
}

pub fn managed_with(
    flavor: InferFlavor,
    restart: RestartPolicy,
    tweak: impl FnOnce(&mut ManagedConfig),
) -> Managed {
    let tmp = tempfile::tempdir().unwrap();
    let script_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&script_dir).unwrap();
    let script = write_script(&script_dir);
    let data = tmp.path().join("data");
    let reg = Arc::new(
        ModelRegistry::in_memory().with_trust(crate::model_manifest::ModelTrust::new(operator().2)),
    );
    let mut cfg = ManagedConfig::new(reg.clone(), data.clone()).with_serve_program(script);
    cfg.restart = restart;
    cfg.env_passthrough = vec![];
    tweak(&mut cfg);
    let rt = Arc::new(InferRuntime::new(InferConfig::managed(flavor, cfg)));
    Managed {
        rt,
        reg,
        tmp,
        script_dir,
        data,
    }
}

pub async fn load(rt: &InferRuntime, s: InferenceSpec) -> InstanceHandle {
    rt.load(&workload(s), &wl_cfg()).await.unwrap()
}

/// A permit for inference workloads. Model weights are operator-attested
/// (not signed by a pinned signer), so the rule must say so explicitly.
pub fn infer_permit() -> crate::workload_governance::WorkloadPermitRule {
    let mut r = crate::workload_governance::WorkloadPermitRule::new(
        "permit-infer",
        ["workload.*"],
        ["inference"],
    );
    r.min_package_trust = crate::workload_governance::PackageTrust::OperatorAttested;
    r
}

/// A permit that admits adopted (observed, unverified) servers only.
pub fn adopted_permit() -> crate::workload_governance::WorkloadPermitRule {
    let mut r = infer_permit();
    r.id = "permit-adopted".into();
    r.min_package_trust = crate::workload_governance::PackageTrust::AdoptedUnverified;
    r
}
