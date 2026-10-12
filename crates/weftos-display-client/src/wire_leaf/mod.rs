//! A host-side emulation of the edge pad's wire behaviour.
//!
//! [`WireLeaf`] is a certified leaf in a thread: it discovers (or is told)
//! its parent, connects over TCP, replays a signed durable journal, subscribes
//! to its push topic, and speaks `weftos_leaf_scene::session` exactly as the
//! pad firmware does, by running the same `DisplayService` and the same
//! envelope builders. The scene lands on a [`LeafSim`], whose hit-test turns
//! injected touches into `Input` frames and whose button is the `ptt`
//! sensor. It exists so the daemon, the display client and the console can be
//! tested end to end without the pad.

mod link;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ed25519_dalek::SigningKey;
use weftos_leaf_scene::session::envelope::session_publish;
use weftos_leaf_scene::session::{Action, DisplayService, LeafInfo, ServiceConfig, SessionFrame};
use weftos_leaf_scene::{px, InputEnvelope, InputEvent};
use weftos_leaf_types::link::{LeafCertificate, OfflineJournal};

use crate::backend::sim::{LeafSim, SIM_DISPLAY};
use crate::caps::DisplaySize;

/// How the leaf finds its parent.
#[derive(Debug, Clone)]
pub enum Parent {
    /// Connect straight to this TCP address (no discovery).
    Direct(SocketAddr),
    /// Send the signed-advertisement discovery request to this UDP address
    /// (the parent's discovery socket, or the LAN broadcast on port 9490).
    Discover(SocketAddr),
}

pub struct WireLeafConfig {
    /// The 32-byte leaf seed `weaver leaf provision` wrote.
    pub seed: [u8; 32],
    /// The CBOR certificate it wrote (`leaf.cbor`).
    pub cert_cbor: Vec<u8>,
    pub parent: Parent,
    /// Where the durable journal lives, so a restart continues the sequence.
    /// `None` keeps it in memory (a fresh registry floor only).
    pub state_file: Option<PathBuf>,
    pub size: DisplaySize,
}

pub(crate) struct Shared {
    pub sim: LeafSim,
    pub service: DisplayService,
    pub journal: OfflineJournal,
    pub cert: LeafCertificate,
    pub key: SigningKey,
    pub state_file: Option<PathBuf>,
    pub started: Instant,
    pub subscribed: bool,
    pub connections: u32,
    pub log: Vec<String>,
}

impl Shared {
    pub fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    pub fn note(&mut self, line: impl Into<String>) {
        let line = line.into();
        eprintln!("[wire-leaf] {line}");
        self.log.push(line);
    }

    /// Sign and journal a session frame for the parent, durably.
    pub fn send(&mut self, frame: &SessionFrame) {
        let id = self.cert.leaf_id();
        let Ok((topic, envelope)) = session_publish(&id, frame) else { return };
        match self.journal.enqueue(&self.cert, &self.key, topic, envelope.into_bytes()) {
            Ok(_) => self.persist(),
            Err(e) => self.note(format!("journal refused a frame: {e:?}")),
        }
    }

    pub fn persist(&mut self) {
        let Some(path) = self.state_file.clone() else { return };
        let Ok(bytes) = weftos_leaf_types::encode(&self.journal) else { return };
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, bytes).and_then(|_| std::fs::rename(&tmp, &path)).is_err() {
            self.note("cannot write the journal state file");
        }
    }

    /// Carry out what the session service decided.
    pub fn run(&mut self, actions: Vec<Action>) {
        for action in actions {
            match action {
                Action::Send(frame) => self.send(&frame),
                Action::Apply { envelope, render } => {
                    if let Err(e) = self.sim.apply_wire(&envelope, render) {
                        self.note(format!("render failed: {e}"));
                    }
                }
                Action::SessionEnded => self.note("session ended"),
            }
        }
    }
}

/// A running emulated leaf. Dropping it stops the link thread.
pub struct WireLeaf {
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    sim: LeafSim,
}

impl WireLeaf {
    pub fn start(cfg: WireLeafConfig) -> Result<Self, String> {
        let key = SigningKey::from_bytes(&cfg.seed);
        let cert: LeafCertificate =
            weftos_leaf_types::decode(&cfg.cert_cbor).map_err(|e| format!("leaf certificate: {e:?}"))?;
        if cert.leaf_pubkey != key.verifying_key().to_bytes() {
            return Err("leaf key does not match the certificate".into());
        }
        let journal = load_journal(cfg.state_file.as_deref(), &cert)?;
        let sim = LeafSim::new(cfg.size.width, cfg.size.height);
        let service = DisplayService::new(ServiceConfig {
            display: SIM_DISPLAY,
            info: LeafInfo {
                width: cfg.size.width as u16,
                height: cfg.size.height as u16,
                render_caps: sim.lock().render_caps(),
                touch: true,
                ptt: true,
            },
            max_nodes: 4096,
            max_ops_per_sec: 1000,
            image_budget_bytes: 2 * 1024 * 1024,
            hide_on_open: Vec::new(),
        });
        let shared = Arc::new(Mutex::new(Shared {
            sim: sim.clone(),
            service,
            journal,
            cert,
            key,
            state_file: cfg.state_file,
            started: Instant::now(),
            subscribed: false,
            connections: 0,
            log: Vec::new(),
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (shared, stop) = (Arc::clone(&shared), Arc::clone(&stop));
            let parent = cfg.parent;
            std::thread::Builder::new()
                .name("wire-leaf".into())
                .spawn(move || link::run(shared, stop, parent))
                .map_err(|e| e.to_string())?
        };
        Ok(Self { shared, stop, thread: Some(thread), sim })
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The leaf's scene and pixels, for assertions.
    pub fn sim(&self) -> LeafSim {
        self.sim.clone()
    }

    pub fn leaf_id(&self) -> String {
        self.lock().cert.leaf_id()
    }

    /// True once the leaf has connected and its subscribe was acknowledged.
    pub fn is_subscribed(&self) -> bool {
        self.lock().subscribed
    }

    /// The session currently holding the screen, if any.
    pub fn session_active(&self) -> bool {
        self.lock().service.active().is_some()
    }

    pub fn session_subject(&self) -> Option<String> {
        self.lock().service.subject().map(str::to_owned)
    }

    pub fn log(&self) -> Vec<String> {
        self.lock().log.clone()
    }

    /// Frames still waiting for the parent's acknowledgement.
    pub fn pending_frames(&self) -> usize {
        self.lock().journal.pending.len()
    }

    pub fn wait_for(&self, timeout: Duration, mut cond: impl FnMut(&WireLeaf) -> bool) -> bool {
        let end = Instant::now() + timeout;
        while Instant::now() < end {
            if cond(self) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        cond(self)
    }

    // ── Input injection (what the GT911 and the button would do) ─────

    pub fn touch_down(&self, x: i32, y: i32) {
        self.pointer(x, y, |x, y| InputEvent::PointerDown { pointer_id: 0, x, y, pressure_q8: 0xFF00 });
    }

    pub fn touch_move(&self, x: i32, y: i32) {
        self.pointer(x, y, |x, y| InputEvent::PointerMove { pointer_id: 0, x, y, pressure_q8: 0xFF00 });
    }

    pub fn touch_up(&self, x: i32, y: i32) {
        self.pointer(x, y, |x, y| InputEvent::PointerUp { pointer_id: 0, x, y });
    }

    pub fn tap(&self, x: i32, y: i32) {
        self.touch_down(x, y);
        self.touch_up(x, y);
    }

    /// The push-to-talk button, pressed or released.
    pub fn ptt(&self, pressed: bool) {
        let name = if pressed { "ptt.down" } else { "ptt.up" };
        let mut g = self.lock();
        if let Some(frame) = g.service.sensor(name, pressed) {
            g.send(&frame);
        }
    }

    fn pointer(&self, x: i32, y: i32, make: impl FnOnce(i32, i32) -> InputEvent) {
        let (xq, yq) = (px(x), px(y));
        let node = self.sim.hit_test(x, y);
        let env = InputEnvelope::new(SIM_DISPLAY, node, make(xq, yq));
        let mut g = self.lock();
        if let Some(frame) = g.service.input(env) {
            g.send(&frame);
        }
    }
}

impl Drop for WireLeaf {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn load_journal(path: Option<&std::path::Path>, cert: &LeafCertificate) -> Result<OfflineJournal, String> {
    let Some(path) = path else { return Ok(OfflineJournal::default()) };
    match std::fs::read(path) {
        Ok(bytes) => {
            let j: OfflineJournal = weftos_leaf_types::decode(&bytes).map_err(|e| format!("journal state {}: {e:?}", path.display()))?;
            j.validate(cert).map_err(|e| format!("journal state {} is not this leaf's: {e:?}", path.display()))?;
            Ok(j)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(OfflineJournal::default()),
        Err(e) => Err(format!("journal state {}: {e}", path.display())),
    }
}
