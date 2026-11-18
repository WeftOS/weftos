//! The running half of a child kernel: genesis, heartbeat, anchors, shutdown
//! (ADR-103 A6, Phase 2 package H). The bootstrap half is `project_boot`.
//!
//! **Parent loss.** The child keeps running. Anchors are not dropped: package
//! D's [`ParentAnchor`] writes each statement to a 0600 pending file before it
//! sends it and replays it with backoff, and this module logs the first
//! failure and the recovery. Shared embeddings and LLM calls fail closed in
//! package F's link. Tokens: a child validates only tokens its own authority
//! issued; nothing in Phase 2 asks the parent whether to trust one, so a
//! parent outage cannot make the child accept a token it cannot validate.
//! On reconnect the link heartbeats its old session and re-registers when
//! the parent no longer knows it (a parent restart or three missed beats),
//! which needs only the certified key and a proof of possession.
//!
//! **Genesis.** The first boot writes `project.genesis {cert, parent_head}`
//! into the child's own chain, naming the user-chain head the user daemon had
//! when it registered the project. Written once: later boots find the event
//! and write nothing.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use clawft_kernel::Kernel;
use clawft_kernel::chain::ChainManager;
use clawft_kernel::chain_anchor::{
    AnchorFrequencyPolicy, AnchoringController, ParentAnchor, ParentAnchorConfig,
};
use clawft_platform::NativePlatform;
use clawft_rpc::mesh_local::{
    Activity, Busy, HeartbeatRequest, METHOD_HEARTBEAT, METHOD_UNREGISTER, ParentHead,
    UnregisterRequest,
};
use clawft_types::project::ProjectCert;
use serde_json::json;
use tokio::sync::watch;
use tracing::{error, info, warn};

use crate::project_boot::{ChildBoot, LinkParams, PreBoot, RegError, register_once};
use crate::project_boot_link::{RpcParentTransport, call_async};

/// Kind of the first event of a project chain.
pub const KIND_GENESIS: &str = "project.genesis";
/// Source of the genesis event.
pub const SOURCE_GENESIS: &str = "project";
/// How often the anchor task looks at the chain head.
const ANCHOR_POLL: Duration = Duration::from_secs(30);

static LAST_ACTIVITY: AtomicU64 = AtomicU64::new(0);
type BusyProbe = Box<dyn Fn() -> Busy + Send + Sync>;
static BUSY_PROBE: OnceLock<BusyProbe> = OnceLock::new();
static RUNNING: OnceLock<Arc<Running>> = OnceLock::new();
static FATAL_STOP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
type FatalHook = Box<dyn Fn(&str) + Send + Sync>;
static FATAL_HOOK: OnceLock<FatalHook> = OnceLock::new();

/// Refusal kinds on re-register that mean the user daemon has withdrawn its
/// certification of this project: the child must stop, not run degraded.
pub const FATAL_KINDS: &[&str] = &[
    "key_revoked",
    "key_conflict",
    "key_reuse",
    "project_not_found",
    "project_revoked",
];

/// Refusal kinds that mean "the user daemon expects no spawn from you": its
/// spawn ledger no longer has this child's nonce (a restarted daemon starts
/// with an empty one) or the nonce expired. A child that booted degraded
/// holds its nonce until a registration succeeds, so on these it drops the
/// nonce and registers the way an adopted child does (same pid, certified
/// key, proof of possession) instead of being refused for ever.
pub const SPAWN_NONCE_KINDS: &[&str] = &["spawn_not_expected", "bad_spawn_nonce", "spawn_expired"];

/// Replace what "stop the kernel" does after a fatal refusal (tests). The
/// default raises SIGTERM on this process, which the daemon already turns
/// into an orderly shutdown. First call wins.
pub fn set_fatal_hook(f: impl Fn(&str) + Send + Sync + 'static) -> bool {
    FATAL_HOOK.set(Box::new(f)).is_ok()
}

fn fatal(run: &Running, why: &str) {
    error!(reason = %why, "the user daemon withdrew this project's certification; shutting the kernel down");
    FATAL_STOP.store(true, Ordering::SeqCst);
    run.stop.send_replace(true);
    match FATAL_HOOK.get() {
        Some(h) => h(why),
        None => {
            let _ = nix::sys::signal::raise(nix::sys::signal::Signal::SIGTERM);
        }
    }
}

fn session_proof(run: &Running, op: &str, session: &str, extra: &str) -> (u32, u64, String) {
    use ed25519_dalek::Signer;
    let pid = std::process::id();
    let at = crate::project_boot::now_unix();
    let sig = run
        .boot
        .key
        .sign(&clawft_rpc::mesh_local::session_signed_bytes(
            op, session, pid, at, extra,
        ));
    (
        pid,
        at,
        clawft_types::project::canon::hex_encode(&sig.to_bytes()),
    )
}

/// Record an RPC for the idle clock. Status, health, handshake and mesh
/// calls do not count (a supervisor polling a child must not keep it awake).
/// Cheap and safe to call from any request path; does nothing until the
/// child has booted.
pub fn note_activity(method: &str) {
    if !tracking() {
        return;
    }
    let quiet = method.starts_with("kernel.status")
        || method.starts_with("kernel.health")
        || method.starts_with("kernel.handshake")
        || method.starts_with("mesh.");
    if !quiet {
        LAST_ACTIVITY.store(crate::project_boot::now_unix(), Ordering::Relaxed);
    }
}

fn tracking() -> bool {
    RUNNING.get().is_some() || test_hooks::forced()
}

/// Lets unit tests of the request path observe the idle clock without
/// booting a whole project kernel.
#[cfg(test)]
pub(crate) mod test_hooks {
    use std::sync::atomic::{AtomicBool, Ordering};

    static FORCE: AtomicBool = AtomicBool::new(false);
    /// Tests that touch the idle clock hold this for their duration. A tokio
    /// mutex, so an async test may hold it across awaits.
    pub(crate) static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    pub(super) fn forced() -> bool {
        FORCE.load(Ordering::SeqCst)
    }

    /// Track activity as if a project kernel were running, from a zero clock.
    pub(crate) fn track(on: bool) {
        FORCE.store(on, Ordering::SeqCst);
        super::LAST_ACTIVITY.store(0, Ordering::SeqCst);
    }

    /// The idle clock.
    pub(crate) fn last_activity() -> u64 {
        super::LAST_ACTIVITY.load(Ordering::SeqCst)
    }
}

#[cfg(not(test))]
mod test_hooks {
    pub(super) fn forced() -> bool {
        false
    }
}

/// Supply what the kernel is busy with beyond running agents (workloads and
/// open streams are the supervisor's idle-stop inputs). First call wins.
pub fn set_busy_probe(probe: impl Fn() -> Busy + Send + Sync + 'static) -> bool {
    BUSY_PROBE.set(Box::new(probe)).is_ok()
}

fn activity(agents: &dyn Fn() -> u32) -> Activity {
    let mut busy = BUSY_PROBE.get().map_or_else(Busy::default, |p| p());
    busy.agents = busy.agents.max(agents());
    Activity {
        last_activity_unix: LAST_ACTIVITY.load(Ordering::Relaxed),
        busy,
    }
}

/// Link state for `kernel.status` style reporting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildStatus {
    /// True while the user daemon answers.
    pub parent_up: bool,
    /// Current session id, if registered.
    pub session: Option<String>,
    /// Last link problem.
    pub last_error: Option<String>,
    /// Anchor statement waiting to be accepted by the parent.
    pub anchor_pending: bool,
}

struct Link {
    session: Option<String>,
    parent_up: bool,
    last_error: Option<String>,
    cert: ProjectCert,
    spawn_nonce: Option<String>,
}

struct Running {
    boot: ChildBoot,
    params: LinkParams,
    chain: Arc<ChainManager>,
    anchor: Arc<ParentAnchor>,
    link: Mutex<Link>,
    agents: Box<dyn Fn() -> u32 + Send + Sync>,
    stop: watch::Sender<bool>,
}

/// The link state, or `None` outside a project kernel.
pub fn status() -> Option<ChildStatus> {
    let r = RUNNING.get()?;
    let l = r.link.lock().expect("boot run link lock poisoned");
    Some(ChildStatus {
        parent_up: l.parent_up,
        session: l.session.clone(),
        last_error: l.last_error.clone(),
        anchor_pending: r.anchor.pending().is_some(),
    })
}

/// Write `project.genesis` unless the chain already has one. Returns whether
/// it wrote.
pub fn ensure_genesis(chain: &ChainManager, cert: &ProjectCert, head: &ParentHead) -> bool {
    if chain.tail(0).iter().any(|e| e.kind == KIND_GENESIS) {
        return false;
    }
    chain.append(
        SOURCE_GENESIS,
        KIND_GENESIS,
        Some(json!({
            "project_id": cert.project_id,
            "cert": cert,
            "parent_head": head,
        })),
    );
    true
}

/// Body of `project_hooks::post_boot`: genesis, heartbeat task, anchor task.
/// A no-op outside the `project` profile.
pub fn post_boot(kernel: &Kernel<NativePlatform>, pre: &PreBoot) -> anyhow::Result<()> {
    let Some(boot) = pre.child.clone() else {
        return Ok(());
    };
    let chain = kernel.chain_manager().cloned().ok_or_else(|| {
        anyhow::anyhow!("project kernel has no chain (the exochain feature is required)")
    })?;
    // The chain must be signed by the certified key: one project key.
    let chain_pk = chain.verifying_key().map(|k| k.to_bytes());
    if chain_pk != Some(boot.key.verifying_key().to_bytes()) {
        anyhow::bail!(
            "the project chain is not signed by the certified project key; refusing to run"
        );
    }
    if let Some(head) = &boot.parent_head
        && ensure_genesis(&chain, &boot.cert, head)
    {
        info!(user_seq = head.user_seq, "wrote project.genesis");
    }
    let params = LinkParams {
        socket: boot.parent_socket.clone(),
        project_id: boot.cert.project_id.clone(),
        user_pubkey: boot.user_pubkey,
        user_key_id: boot.user_key_id.clone(),
        own_socket: boot.paths.socket(),
        host_socket: boot.container.as_ref().map(|c| c.host_socket.clone().into()),
        container_id: boot.container.as_ref().map(|c| c.container_id.clone()),
        container_engine: boot.container.as_ref().map(|c| c.engine.clone()),
        root: boot.host_root.clone(),
        timeout: crate::project_boot::CALL_TIMEOUT,
    };
    let transport = Arc::new(RpcParentTransport::new(
        boot.parent_socket.clone(),
        boot.cert.project_id.clone(),
        Duration::from_secs(8),
    ));
    let anchor = Arc::new(ParentAnchor::new(
        Arc::clone(&chain),
        boot.key.clone(),
        boot.cert.project_id.clone(),
        boot.cert.serial,
        transport,
        boot.paths.chain_dir().join("anchor-pending.json"),
        ParentAnchorConfig::default(),
    ));
    let table = Arc::clone(kernel.process_table());
    let agents: Box<dyn Fn() -> u32 + Send + Sync> = Box::new(move || {
        table
            .list()
            .iter()
            .filter(|e| e.pid != 0 && e.state == clawft_kernel::process::ProcessState::Running)
            .count() as u32
    });
    let (stop, stop_rx) = watch::channel(false);
    LAST_ACTIVITY.store(crate::project_boot::now_unix(), Ordering::Relaxed);
    let running = Arc::new(Running {
        link: Mutex::new(Link {
            session: boot.session.clone(),
            parent_up: boot.session.is_some(),
            last_error: boot.degraded.clone(),
            cert: boot.cert.clone(),
            spawn_nonce: boot.spawn_nonce.clone(),
        }),
        boot,
        params,
        chain,
        anchor,
        agents,
        stop,
    });
    if RUNNING.set(Arc::clone(&running)).is_err() {
        warn!("project link already running; keeping the first");
        return Ok(());
    }
    tokio::spawn(link_loop(Arc::clone(&running), stop_rx.clone()));
    tokio::spawn(anchor_loop(running, stop_rx));
    Ok(())
}

async fn link_loop(run: Arc<Running>, mut stop: watch::Receiver<bool>) {
    let mut backoff = Duration::from_secs(1);
    loop {
        let wait = step(&run, &mut backoff).await;
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = stop.changed() => {
                if *stop.borrow() {
                    return;
                }
            }
        }
    }
}

fn set_link(run: &Running, f: impl FnOnce(&mut Link)) {
    f(&mut run.link.lock().expect("boot run link lock poisoned"));
}

fn mark_down(run: &Running, why: &str) {
    set_link(run, |l| {
        if l.parent_up {
            warn!(reason = %why, "user daemon lost; running degraded (anchors queue on disk, shared services fail closed)");
        }
        l.parent_up = false;
        l.last_error = Some(why.to_owned());
    });
}

/// One link iteration; returns how long to wait before the next.
async fn step(run: &Running, backoff: &mut Duration) -> Duration {
    let hb = Duration::from_secs(run.boot.heartbeat_secs.max(1));
    let session = run
        .link
        .lock()
        .expect("boot run link lock poisoned")
        .session
        .clone();
    let id = run.params.project_id.as_str();
    if let Some(session) = session {
        let act = activity(&*run.agents);
        let (pid, at_unix, sig) = session_proof(
            run,
            "heartbeat",
            &session,
            &clawft_rpc::mesh_local::activity_digest(&act),
        );
        let req = HeartbeatRequest {
            session,
            pid,
            at_unix,
            sig,
            activity: act,
        };
        let res = call_async(
            &run.params.socket,
            METHOD_HEARTBEAT,
            serde_json::to_value(req).unwrap_or_default(),
            Some(id),
            run.params.timeout,
        )
        .await;
        return match res {
            Ok(_) => {
                *backoff = Duration::from_secs(1);
                set_link(run, |l| {
                    if !l.parent_up {
                        info!("user daemon is back");
                    }
                    l.parent_up = true;
                    l.last_error = None;
                });
                hb
            }
            Err(crate::project_boot_link::LinkError::Refused { kind, .. })
                if kind == "unknown_session" || kind == "session_expired" =>
            {
                info!(%kind, "the user daemon no longer knows this session; registering again");
                set_link(run, |l| l.session = None);
                Duration::ZERO
            }
            Err(e) => {
                mark_down(run, &e.to_string());
                let w = *backoff;
                *backoff = (*backoff * 2).min(Duration::from_secs(30));
                w
            }
        };
    }
    let (cert, nonce) = {
        let l = run.link.lock().expect("boot run link lock poisoned");
        (l.cert.clone(), l.spawn_nonce.clone())
    };
    match register_once(
        &run.params,
        &run.boot.key,
        Some(&cert),
        nonce.as_deref(),
        crate::project_boot::now_unix(),
    )
    .await
    {
        Ok(reg) => {
            *backoff = Duration::from_secs(1);
            if reg.cert != cert {
                persist_cert(run, &reg.cert);
                run.anchor.set_cert_serial(reg.cert.serial);
            }
            if let Some(head) = &reg.parent_head
                && ensure_genesis(&run.chain, &reg.cert, head)
            {
                info!(user_seq = head.user_seq, "wrote project.genesis");
            }
            set_link(run, |l| {
                l.session = Some(reg.session.clone());
                l.cert = reg.cert.clone();
                l.spawn_nonce = None;
                l.parent_up = true;
                l.last_error = None;
            });
            info!(session = %reg.session, "registered with the user daemon");
            Duration::from_secs(reg.heartbeat_secs.max(1))
        }
        Err(RegError::Unavailable(why)) => {
            mark_down(run, &why);
            let w = *backoff;
            *backoff = (*backoff * 2).min(Duration::from_secs(30));
            w
        }
        Err(RegError::Refused { kind, .. }) if nonce.is_some() && SPAWN_NONCE_KINDS.contains(&kind.as_str()) => {
            info!(%kind, "the user daemon expects no spawn from this kernel; registering again without the spawn nonce");
            set_link(run, |l| l.spawn_nonce = None);
            Duration::ZERO
        }
        Err(RegError::Refused { kind, message }) if FATAL_KINDS.contains(&kind.as_str()) => {
            fatal(run, &format!("{kind}: {message}"));
            Duration::from_secs(3600)
        }
        Err(e) => {
            // Refused or untrusted: the parent answered. Keep running and
            // keep telling the operator; do not hammer it.
            let why = match &e {
                RegError::Refused { kind, message } => {
                    format!("registration refused ({kind}): {message}")
                }
                RegError::Untrusted(m) => format!("registration answer not trusted: {m}"),
                RegError::Unavailable(m) => m.clone(),
            };
            error!(reason = %why, "cannot register with the user daemon; running degraded");
            set_link(run, |l| {
                l.parent_up = false;
                l.last_error = Some(why.clone());
            });
            Duration::from_secs(30)
        }
    }
}

fn persist_cert(run: &Running, cert: &ProjectCert) {
    let Some(path) = run.boot.paths.project_cert() else {
        return;
    };
    match serde_json::to_vec_pretty(cert) {
        Ok(text) => {
            if let Err(e) =
                clawft_kernel::project_identity::write_private_atomic(&path, &text, false)
            {
                warn!(error = %e, "could not persist the new certificate");
            }
        }
        Err(e) => warn!(error = %e, "could not encode the new certificate"),
    }
}

async fn anchor_loop(run: Arc<Running>, mut stop: watch::Receiver<bool>) {
    let controller = Arc::new(AnchoringController::new(
        run.anchor.clone(),
        AnchorFrequencyPolicy {
            min_interval: Duration::from_secs(300),
            min_events_between: 100,
        },
    ));
    let mut last_err: Option<String> = None;
    loop {
        let (c, a, chain) = (
            Arc::clone(&controller),
            Arc::clone(&run.anchor),
            Arc::clone(&run.chain),
        );
        let res = tokio::task::spawn_blocking(move || {
            let pending = a.retry_pending().map(|_| ());
            let head = c
                .try_anchor(&chain.head_hash(), chain.sequence())
                .map(|_| ());
            pending.and(head)
        })
        .await;
        let outcome = match res {
            Ok(r) => r,
            Err(e) => Err(format!("anchor task failed: {e}")),
        };
        match (&outcome, &last_err) {
            (Err(e), prev) if prev.as_deref() != Some(e.as_str()) => {
                warn!(error = %e, "anchor to the user daemon failed; the statement stays queued on disk and is retried");
                last_err = Some(e.clone());
            }
            (Ok(()), Some(_)) => {
                info!("anchoring to the user daemon recovered");
                last_err = None;
            }
            _ => {}
        }
        tokio::select! {
            _ = tokio::time::sleep(ANCHOR_POLL) => {}
            _ = stop.changed() => {
                if *stop.borrow() {
                    return;
                }
            }
        }
    }
}

/// Before the kernel shuts down: stop the tasks, anchor the final head, tell
/// the user daemon. Every step is bounded; a failure is logged, never fatal
/// (an anchor that cannot be sent stays in the pending file for the next
/// boot). A no-op outside a project kernel.
pub async fn pre_shutdown() {
    let Some(run) = RUNNING.get().cloned() else {
        return;
    };
    run.stop.send_replace(true);
    if FATAL_STOP.load(Ordering::SeqCst) {
        // Certification withdrawn: no more anchors, no unregister.
        return;
    }
    let a = Arc::clone(&run.anchor);
    match tokio::task::spawn_blocking(move || a.anchor_head()).await {
        Ok(Ok(r)) => info!(tx = %r.tx_id, "final head anchored to the user daemon"),
        Ok(Err(e)) => {
            warn!(error = %e, "final anchor not accepted; it stays queued for the next boot")
        }
        Err(e) => warn!(error = %e, "final anchor task failed"),
    }
    let session = run
        .link
        .lock()
        .expect("boot run link lock poisoned")
        .session
        .clone();
    if let Some(session) = session {
        let (pid, at_unix, sig) = session_proof(&run, "unregister", &session, "");
        let req = UnregisterRequest {
            session,
            pid,
            at_unix,
            sig,
            reason: "shutdown".into(),
        };
        let _ = call_async(
            &run.params.socket,
            METHOD_UNREGISTER,
            serde_json::to_value(req).unwrap_or_default(),
            Some(&run.params.project_id),
            Duration::from_secs(3),
        )
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clawft_types::project::cert::CertRequest;
    use ed25519_dalek::SigningKey;

    fn cert(serial: u64) -> ProjectCert {
        let user = SigningKey::from_bytes(&[1u8; 32]);
        ProjectCert::sign(
            &user,
            &CertRequest {
                project_id: "01JB8Z3Q0V6X9KQ4M2N7T5R1WD".into(),
                project_pubkey: SigningKey::from_bytes(&[2u8; 32])
                    .verifying_key()
                    .to_bytes(),
                serial,
                issued_at: chrono::Utc::now(),
                expires_at: None,
            },
        )
    }

    #[test]
    fn genesis_names_the_parent_head_and_is_written_once() {
        let chain = ChainManager::new(0, 1000);
        chain.append("kernel", "boot.init", None);
        let head = ParentHead {
            user_seq: 7,
            user_event_hash: "ab".repeat(32),
        };
        assert!(ensure_genesis(&chain, &cert(1), &head));
        assert!(!ensure_genesis(
            &chain,
            &cert(1),
            &ParentHead {
                user_seq: 9,
                user_event_hash: "cd".repeat(32)
            }
        ));
        let g: Vec<_> = chain
            .tail(0)
            .into_iter()
            .filter(|e| e.kind == KIND_GENESIS)
            .collect();
        assert_eq!(g.len(), 1);
        let p = g[0].payload.as_ref().unwrap();
        assert_eq!(p["parent_head"]["user_seq"], 7);
        assert_eq!(p["cert"]["serial"], 1);
        assert!(chain.verify_integrity().valid);
    }

    fn fake_parent(path: &std::path::Path, kind: &'static str) -> std::thread::JoinHandle<()> {
        use std::io::{BufRead, BufReader, Write};
        let l = std::os::unix::net::UnixListener::bind(path).unwrap();
        std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            let mut line = String::new();
            BufReader::new(&s).read_line(&mut line).unwrap();
            let body = format!("{{\"ok\":false,\"error\":\"x\",\"error_kind\":\"{kind}\"}}\n");
            (&s).write_all(body.as_bytes()).unwrap();
        })
    }

    fn running(dir: &std::path::Path, socket: std::path::PathBuf) -> Running {
        use crate::project_boot::LinkParams;
        let key = SigningKey::from_bytes(&[2u8; 32]);
        let c = cert(1);
        let id = c.project_id.clone();
        let paths = clawft_types::runtime_paths::RuntimePaths::child_at(
            dir.join("run"),
            &id,
            dir.join("proj"),
        )
        .unwrap();
        let params = LinkParams {
            socket: socket.clone(),
            project_id: id.clone(),
            user_pubkey: [0u8; 32],
            user_key_id: String::new(),
            own_socket: paths.socket(),
            host_socket: None,
            container_id: None,
            container_engine: None,
            root: dir.join("proj"),
            timeout: Duration::from_secs(2),
        };
        let boot = ChildBoot {
            paths,
            host_root: dir.join("proj"),
            container: None,
            key: key.clone(),
            cert: c.clone(),
            parent_socket: socket.clone(),
            user_pubkey: [0u8; 32],
            user_key_id: String::new(),
            session: None,
            parent_head: None,
            heartbeat_secs: 1,
            degraded: None,
            spawn_nonce: None,
            project_token: None,
        };
        let chain = Arc::new(ChainManager::new(0, 1000));
        let anchor = Arc::new(ParentAnchor::new(
            Arc::clone(&chain),
            key,
            id.clone(),
            1,
            Arc::new(RpcParentTransport::new(socket, id, Duration::from_secs(1))),
            dir.join("pending.json"),
            ParentAnchorConfig::default(),
        ));
        Running {
            link: Mutex::new(Link {
                session: None,
                parent_up: false,
                last_error: None,
                cert: c,
                spawn_nonce: None,
            }),
            boot,
            params,
            chain,
            anchor,
            agents: Box::new(|| 0),
            stop: watch::channel(false).0,
        }
    }

    #[tokio::test]
    async fn a_degraded_child_drops_a_spawn_nonce_the_parent_no_longer_expects() {
        let t = tempfile::tempdir().unwrap();
        for kind in SPAWN_NONCE_KINDS {
            let sock = t.path().join(format!("{kind}.sock"));
            let h = fake_parent(&sock, kind);
            let run = running(t.path(), sock);
            set_link(&run, |l| l.spawn_nonce = Some("ab".repeat(32)));
            let mut backoff = Duration::from_secs(1);
            // First try: refused with the nonce -> the nonce goes, retry now.
            assert_eq!(step(&run, &mut backoff).await, Duration::ZERO, "{kind}");
            h.join().unwrap();
            assert!(run.link.lock().unwrap().spawn_nonce.is_none(), "{kind}: nonce dropped");
            assert!(!*run.stop.borrow(), "{kind} is not fatal");
            // Second try, no nonce, refused the same way: back to the slow
            // retry (no hammering the parent).
            let sock2 = t.path().join(format!("{kind}-2.sock"));
            let h2 = fake_parent(&sock2, kind);
            let run2 = running(t.path(), sock2);
            assert_eq!(step(&run2, &mut backoff).await, Duration::from_secs(30), "{kind} without a nonce");
            h2.join().unwrap();
        }
    }

    static FATALS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    #[tokio::test]
    async fn a_withdrawn_certification_is_fatal_and_a_transient_refusal_is_not() {
        set_fatal_hook(|_| {
            FATALS.fetch_add(1, Ordering::SeqCst);
        });
        let t = tempfile::tempdir().unwrap();
        let before = FATALS.load(Ordering::SeqCst);
        // Not fatal: the parent is merely busy.
        let sock = t.path().join("a.sock");
        let h = fake_parent(&sock, "cert_unavailable");
        let run = running(t.path(), sock);
        let mut backoff = Duration::from_secs(1);
        let wait = step(&run, &mut backoff).await;
        h.join().unwrap();
        assert_eq!(wait, Duration::from_secs(30));
        assert_eq!(FATALS.load(Ordering::SeqCst), before);
        assert!(!*run.stop.borrow());
        // Fatal: revoked. No more heartbeats or anchors, orderly shutdown.
        for k in FATAL_KINDS {
            let sock = t.path().join(format!("{k}.sock"));
            let h = fake_parent(&sock, k);
            let run = running(t.path(), sock);
            step(&run, &mut backoff).await;
            h.join().unwrap();
            assert!(*run.stop.borrow(), "{k}: tasks stopped");
        }
        assert_eq!(FATALS.load(Ordering::SeqCst), before + FATAL_KINDS.len());
        assert!(
            FATAL_STOP.load(Ordering::SeqCst),
            "pre_shutdown will skip the final anchor"
        );
    }

    #[test]
    fn quiet_methods_do_not_move_the_idle_clock() {
        let _serial = test_hooks::SERIAL.blocking_lock();
        test_hooks::track(false);
        // No child running in this process: nothing is recorded at all.
        LAST_ACTIVITY.store(0, Ordering::Relaxed);
        note_activity("agent.chat");
        assert_eq!(LAST_ACTIVITY.load(Ordering::Relaxed), 0);
    }
}
