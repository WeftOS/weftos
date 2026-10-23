//! Service start-up and run loop (plan 2 S, section 7).
//!
//! Order matters and every step fails closed:
//!
//! 1. refuse `euid == 0` (no override; tests run as the current user);
//! 2. state directory: a real directory owned by us with mode 0700; socket
//!    directory: a real directory owned by us or root, not world-writable;
//! 3. the box key (`node.key`) under the state directory, then the journal
//!    (which takes the `mesh.lock` single-writer lock: a second service on the
//!    same state dir stops here, naming the holder);
//! 4. the mesh listener (a taken port aborts start, naming the address),
//!    then the mesh-local socket (a live service on it aborts; a stale
//!    socket we own is replaced), then `service.json`.
//!
//! The service constructs no `Kernel`, `ChainManager`, `GateBackend`, token
//! or secret type, and reads nothing under a home directory.

use std::net::SocketAddr;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use clawft_kernel::mesh_admit::{AdmissionGate, DialIdentity};
use clawft_kernel::mesh_noise::{NoiseConfig, NoisePattern};
use clawft_kernel::mesh_runtime::MeshRuntime;
use clawft_kernel::mesh_serve::{connect_seeds, serve_listener_with, transport_for};
use clawft_kernel::node_key::{load_or_generate_node_key, NODE_KEY_FILE};
use clawft_kernel::revocation::RevocationList;
use clawft_mesh_local::proto::{ServiceRecord, VersionRange, PROTO_MAX, PROTO_MIN};
use clawft_types::config::MeshAdmissionMode;
use rand::RngCore;
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::net::UnixListener;
use tokio::task::JoinHandle;

use crate::config::{ConfigError, MeshServiceConfig};
use crate::fsutil;
use crate::limits::LimitConfig;
use crate::local_server::{real_peer_source, serve_local, PeerSource};
use crate::state::{admission_str, ServiceState};
use crate::{Journal, JournalError};

/// Public record file name (state dir, and next to the socket for clients).
pub const SERVICE_JSON: &str = "service.json";

#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error("refusing to run as root: the mesh service must run as an unprivileged service account")]
    Root,
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("state directory: {0}")]
    State(String),
    #[error("socket directory: {0}")]
    SocketDir(String),
    #[error("box key {path}: {reason}")]
    Key { path: String, reason: String },
    #[error(transparent)]
    Journal(#[from] JournalError),
    #[error(
        "could not bind the mesh listener on {addr} ({reason}); another kernel or mesh service on this \
         machine probably holds it (port 9489 is the weave). Stop it or set `listen` in mesh.toml"
    )]
    Listen { addr: String, reason: String },
    #[error("mesh-local socket {path}: {reason}")]
    Socket { path: String, reason: String },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Spawned tasks, aborted if start-up fails after some were spawned.
#[derive(Default)]
struct Tasks(Vec<JoinHandle<()>>);

impl Tasks {
    fn push(&mut self, h: JoinHandle<()>) {
        self.0.push(h);
    }

    /// Hand the tasks to the running service (disarming the abort).
    fn into_vec(mut self) -> Vec<JoinHandle<()>> {
        std::mem::take(&mut self.0)
    }
}

impl Drop for Tasks {
    fn drop(&mut self) {
        for t in &self.0 {
            t.abort();
        }
    }
}

/// A started service. Dropping it does not stop it; call [`Self::shutdown`].
pub struct RunningService {
    pub state: Arc<ServiceState>,
    pub node_id: String,
    pub socket: PathBuf,
    /// Where the mesh listener bound (useful with port 0).
    pub mesh_addr: Option<SocketAddr>,
    pub health_addr: Option<SocketAddr>,
    runtime: Arc<MeshRuntime>,
    tasks: Vec<JoinHandle<()>>,
}

impl RunningService {
    /// The mesh runtime (peer table, for tests and status).
    pub fn runtime(&self) -> &Arc<MeshRuntime> {
        &self.runtime
    }

    /// Stop serving: abort every task, drop peers, remove the socket and the
    /// advertised record. State on disk is kept.
    pub async fn shutdown(self) {
        for t in &self.tasks {
            t.abort();
        }
        self.runtime.disconnect_all_peers();
        let _ = std::fs::remove_file(&self.socket);
        if let Some(dir) = self.socket.parent() {
            let _ = std::fs::remove_file(dir.join(SERVICE_JSON));
        }
        for t in self.tasks {
            let _ = t.await;
        }
    }
}

#[cfg(unix)]
fn require_unprivileged() -> Result<(), StartError> {
    if fsutil::euid() == 0 {
        return Err(StartError::Root);
    }
    Ok(())
}

fn check_socket_dir(dir: &Path) -> Result<(), StartError> {
    let bad = |m: &str| Err(StartError::SocketDir(format!("{}: {m}", dir.display())));
    if std::fs::symlink_metadata(dir).is_err() {
        std::fs::create_dir_all(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755))?;
    }
    let md = std::fs::symlink_metadata(dir)?;
    if !md.file_type().is_dir() {
        return bad("not a real directory (symlink or file)");
    }
    if md.uid() != fsutil::euid() && md.uid() != 0 {
        return bad("owned by neither the service account nor root");
    }
    // Group-writable is as bad as world-writable here: any member could
    // replace the socket. (A root-owned directory must also be 0755 or tighter.)
    if md.mode() & 0o022 != 0 {
        return bad("group- or world-writable; another account could replace the socket");
    }
    Ok(())
}

/// Make `path` free for binding: a missing path is fine, a stale socket we
/// own is removed, a live service or anything else is an error.
fn prepare_socket(path: &Path) -> Result<(), StartError> {
    let err = |r: String| Err(StartError::Socket { path: path.display().to_string(), reason: r });
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => err(e.to_string()),
        Ok(md) if md.file_type().is_socket() => {
            if md.uid() != fsutil::euid() {
                return err("an existing socket belongs to another account; refusing to remove it".into());
            }
            if std::os::unix::net::UnixStream::connect(path).is_ok() {
                return err("another mesh service is already listening here".into());
            }
            std::fs::remove_file(path)?;
            Ok(())
        }
        Ok(_) => err("exists and is not a socket; refusing to remove it".into()),
    }
}

fn bind_socket(path: &Path) -> Result<UnixListener, StartError> {
    prepare_socket(path)?;
    let l = UnixListener::bind(path)
        .map_err(|e| StartError::Socket { path: path.display().to_string(), reason: e.to_string() })?;
    // D-7: world-connectable; authorisation is by peer credential, never by mode.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666))?;
    Ok(l)
}

fn load_key(state_dir: &Path) -> Result<(ed25519_dalek::SigningKey, bool), StartError> {
    let existed = std::fs::symlink_metadata(state_dir.join(NODE_KEY_FILE)).is_ok();
    let key = load_or_generate_node_key(state_dir).map_err(|e| StartError::Key {
        path: state_dir.join(NODE_KEY_FILE).display().to_string(),
        reason: e.to_string(),
    })?;
    Ok((key, existed))
}

fn noise_config(cfg: &MeshServiceConfig) -> Option<Arc<NoiseConfig>> {
    cfg.noise.then(|| {
        // The Noise static key is per boot: identity is bound by the box-key
        // signature over the handshake hash (admission), not by this key.
        let mut k = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut k);
        Arc::new(NoiseConfig { pattern: NoisePattern::XX, local_private_key: k, remote_static_key: None })
    })
}

/// Start with the real peer-credential reader and default limits.
pub async fn start(cfg: MeshServiceConfig) -> Result<RunningService, StartError> {
    start_inner(cfg, real_peer_source(), LimitConfig::default()).await
}

/// Start with an injected peer source and limits. Test seam: an injectable
/// credential source must not exist in release builds.
#[cfg(feature = "testing")]
pub async fn start_with(
    cfg: MeshServiceConfig,
    peers: PeerSource,
    limits: LimitConfig,
) -> Result<RunningService, StartError> {
    start_inner(cfg, peers, limits).await
}

async fn start_inner(
    cfg: MeshServiceConfig,
    peers: PeerSource,
    limits: LimitConfig,
) -> Result<RunningService, StartError> {
    require_unprivileged()?;
    cfg.validate()?;
    fsutil::ensure_state_dir(&cfg.state_dir).map_err(|e| StartError::State(e.to_string()))?;
    let socket_dir = cfg.socket.parent().expect("validated").to_path_buf();
    check_socket_dir(&socket_dir)?;

    let (key, key_existed) = load_key(&cfg.state_dir)?;
    let mut journal = Journal::open(&cfg.state_dir, key.clone())?;
    let node_id = clawft_mesh_local::node_id_from_pubkey(&key.verifying_key().to_bytes());
    let pubkey = key.verifying_key().to_bytes();
    if journal.is_empty() {
        journal.append(
            "machine.init",
            json!({
                "node_id": node_id, "machine_pubkey": clawft_mesh_local::hexser::encode(&pubkey),
                "key_origin": if key_existed { "adopted" } else { "generated" },
                "build_sha": cfg.build_sha,
            }),
        )?;
    }
    let revocations = Arc::new(RevocationList::load(cfg.state_dir.join("revoked.json")));
    let state = ServiceState::build(cfg.clone(), key.clone(), journal, revocations, limits)
        .map_err(|e| StartError::Config(ConfigError::Invalid(e)))?;
    state.note(
        "service.start",
        json!({
            "build_sha": cfg.build_sha, "proto": {"min": PROTO_MIN, "max": PROTO_MAX},
            "pid": std::process::id(), "listen": cfg.listen,
            "admission": admission_str(state.policy.admission()), "bind_policy": cfg.bind_policy.as_str(),
        }),
    );

    // Facts: probe off the async threads, then sign and publish.
    let facts = Arc::clone(&state.facts);
    let _ = tokio::task::spawn_blocking(move || facts.reprobe()).await;
    state.refresh_facts();

    // Mesh runtime with the tenant router as its local side.
    let mut rt = if cfg.discovery {
        let kad: [u8; 32] = Sha256::digest(node_id.as_bytes()).into();
        MeshRuntime::with_discovery(node_id.clone(), kad)
    } else {
        MeshRuntime::new(node_id.clone())
    };
    rt.set_local_delivery(state.router.clone());
    let rt = Arc::new(rt);
    let leaf_root = cfg.state_dir.join("leaf");
    if leaf_root.join("registry").is_dir() {
        let tenant_registry = Arc::clone(&state.registry);
        let authority_state = Arc::clone(&state);
        let ingress = clawft_kernel::mesh_leaf::LeafIngress::open(&leaf_root, key.clone())
            .map(|ingress| ingress.with_scope_authority(Arc::new(move |user_id, user_key, project_cert| {
                let Ok(registered) = tenant_registry.lookup_scope(user_id, None) else { return false };
                if registered.user_pubkey != *user_key || authority_state.force_revoked.contains(&registered.principal)
                    || authority_state.core.lock().unwrap_or_else(|e| e.into_inner()).bindings.key_of(&registered.principal) != Some(*user_key)
                { return false; }
                project_cert.is_none_or(|cert| {
                    clawft_types::project::canon::hex_decode::<32>(&cert.project_pubkey)
                        .is_some_and(|key| tenant_registry.current_project_key(user_id, &cert.project_id) == Some(key))
                })
            })))
            .map_err(|e| StartError::Config(ConfigError::Invalid(format!("leaf ingress: {e}"))))?;
        rt.set_leaf_ingress(Arc::new(ingress))
            .map_err(|_| StartError::Config(ConfigError::Invalid("leaf ingress already installed".into())))?;
    }
    rt.set_enforcing(state.policy.admission() == MeshAdmissionMode::Enforce);
    state.router.set_runtime(&rt);
    // ADR-106: licence records from licensed peers go to the owner's daemon.
    crate::licence_forward::install(&rt, &state.router);

    if !cfg.listen.parse::<SocketAddr>().is_ok_and(|a| a.ip().is_loopback()) {
        tracing::warn!(
            listen = %cfg.listen,
            "the mesh listener is exposed beyond loopback (explicit `listen` choice); \
             every host that can reach this port can attempt to join"
        );
    }
    let noise = noise_config(&cfg);
    let transport = transport_for(&cfg.transport, None);
    let listener = transport
        .listen(&cfg.listen)
        .await
        .map_err(|e| StartError::Listen { addr: cfg.listen.clone(), reason: e.to_string() })?;
    let mesh_addr = listener.local_addr().ok();
    *state.listen_addr.lock().expect("listen lock") =
        Some(mesh_addr.map_or_else(|| cfg.listen.clone(), |a| a.to_string()));

    let mut tasks = Tasks::default();
    {
        let (rt, state, noise) = (Arc::clone(&rt), Arc::clone(&state), noise.clone());
        let tname = transport.name().to_string();
        let listen = cfg.listen.clone();
        let limits = cfg.mesh_limits();
        tasks.push(tokio::spawn(async move {
            let gate: Arc<dyn AdmissionGate> = state.gate.clone();
            serve_listener_with(rt, listener, noise, &tname, &listen, gate, limits).await;
        }));
    }
    let dial = match (state.policy.admission(), cfg.genesis_hash) {
        (MeshAdmissionMode::Off, _) | (_, None) => None,
        (_, Some(genesis)) => Some(Arc::new(DialIdentity {
            key: key.clone(),
            genesis,
            platform: std::env::consts::OS.to_owned(),
            capabilities: vec![],
        })),
    };
    connect_seeds(&rt, &cfg.seed_peers, &cfg.transport, noise, dial);

    // Stage the record clients pin, bind the socket, and only then publish
    // the record: a service that loses a bind race must never replace the
    // winner's record. A client that sees the socket a moment before the
    // rename waits for the record (RECORD_WAIT in the client).
    prepare_socket(&cfg.socket)?;
    let staged = stage_service_json(&cfg, &state, &socket_dir)?;
    let sock = match bind_socket(&cfg.socket) {
        Ok(s) => s,
        Err(e) => {
            staged.discard();
            return Err(e);
        }
    };
    staged.publish()?;
    tasks.push(tokio::spawn(serve_local(Arc::clone(&state), sock, peers)));

    let health_addr = match &cfg.health_listen {
        Some(addr) => {
            let (a, h) = crate::health::spawn(Arc::clone(&state), addr).await?;
            tasks.push(h);
            Some(a)
        }
        None => None,
    };
    tasks.push(spawn_facts_refresh(Arc::clone(&state)));
    tracing::info!(node_id = %node_id, socket = %cfg.socket.display(), mesh = ?mesh_addr, "mesh service started");
    Ok(RunningService { node_id, socket: cfg.socket.clone(), mesh_addr, health_addr, runtime: rt, tasks: tasks.into_vec(), state })
}

/// The `service.json` copies written beside temp names, not yet visible.
struct StagedRecord(Vec<(PathBuf, PathBuf)>);

impl StagedRecord {
    fn publish(mut self) -> Result<(), StartError> {
        let items = std::mem::take(&mut self.0);
        for (i, (tmp, dst)) in items.iter().enumerate() {
            if let Err(e) = std::fs::rename(tmp, dst) {
                for (rest, _) in &items[i..] {
                    let _ = std::fs::remove_file(rest);
                }
                return Err(StartError::from(e));
            }
        }
        Ok(())
    }

    fn discard(mut self) {
        for (tmp, _) in std::mem::take(&mut self.0) {
            let _ = std::fs::remove_file(tmp);
        }
    }
}

impl Drop for StagedRecord {
    fn drop(&mut self) {
        for (tmp, _) in self.0.drain(..) {
            let _ = std::fs::remove_file(tmp);
        }
    }
}

fn stage_service_json(cfg: &MeshServiceConfig, st: &ServiceState, socket_dir: &Path) -> Result<StagedRecord, StartError> {
    let rec = ServiceRecord {
        node_id: st.node_id.clone(),
        machine_pubkey: st.machine_pubkey,
        service_uid: fsutil::euid(),
        proto: VersionRange { min: PROTO_MIN, max: PROTO_MAX, sha: Some(cfg.build_sha.clone()) },
        build_sha: cfg.build_sha.clone(),
        started_at: st.started_at,
    };
    let bytes = serde_json::to_vec_pretty(&rec).map_err(std::io::Error::other)?;
    // Plan 1.1 puts the record in the state dir; that dir is 0700, so clients
    // of other uids cannot read it there. A copy beside the socket is the one
    // they pin from.
    let mut staged = StagedRecord(Vec::new());
    for dir in [cfg.state_dir.as_path(), socket_dir] {
        remove_stale_staging(dir);
        let tmp = dir.join(format!("{SERVICE_JSON}.{}.tmp", std::process::id()));
        write_staged(&tmp, &bytes, 0o644)?;
        staged.0.push((tmp, dir.join(SERVICE_JSON)));
    }
    Ok(staged)
}

/// Remove `service.json.<pid>.tmp` files this uid left behind (a crash between
/// staging and publishing), keeping those of a still-running process.
fn remove_stale_staging(dir: &Path) {
    use std::os::unix::fs::MetadataExt;
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(pid) = name
            .strip_prefix(&format!("{SERVICE_JSON}."))
            .and_then(|r| r.strip_suffix(".tmp"))
            .and_then(|p| p.parse::<i32>().ok())
        else {
            continue;
        };
        let ours = e.metadata().is_ok_and(|m| m.uid() == fsutil::euid());
        // SAFETY: signal 0 only probes for the process.
        let alive = pid as u32 == std::process::id() || unsafe { libc::kill(pid, 0) } == 0;
        if ours && !alive {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

fn write_staged(tmp: &Path, bytes: &[u8], mode: u32) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let _ = std::fs::remove_file(tmp);
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(mode).open(tmp)?;
    f.write_all(bytes)?;
    f.sync_all()
}

/// Re-probe and re-sign facts at half their lifetime so they never expire.
fn spawn_facts_refresh(st: Arc<ServiceState>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let period = Duration::from_secs((st.cfg.facts_ttl_s / 2).max(30));
        loop {
            tokio::time::sleep(period).await;
            let facts = Arc::clone(&st.facts);
            let _ = tokio::task::spawn_blocking(move || facts.reprobe()).await;
            st.refresh_facts();
        }
    })
}

/// Run until interrupted (`SIGINT` or `SIGTERM`), then shut down cleanly.
pub async fn run(cfg: MeshServiceConfig) -> Result<(), StartError> {
    let svc = start(cfg).await?;
    wait_for_signal().await;
    tracing::info!("mesh service stopping");
    svc.shutdown().await;
    Ok(())
}

/// Wait for `SIGINT` or `SIGTERM` (what [`run`] stops on).
pub async fn wait_for_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate()).ok();
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = async {
            match term.as_mut() {
                Some(t) => { t.recv().await; }
                None => std::future::pending::<()>().await,
            }
        } => {}
    }
}
