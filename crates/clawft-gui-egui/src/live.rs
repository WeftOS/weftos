//! Live kernel state — two implementations behind one public surface:
//!
//! - **Native** (`cfg(not(target_arch = "wasm32"))`) — background
//!   `std::thread` hosts a single-threaded tokio runtime and pokes the
//!   daemon IPC socket through `clawft_rpc::DaemonClient`.
//! - **Wasm / webview** (`cfg(target_arch = "wasm32")`) — lives inside a
//!   VSCode / Cursor WebviewPanel. Cannot use Unix sockets; instead
//!   posts JSON-RPC-shaped messages through `acquireVsCodeApi()` and
//!   the extension host proxies them to the daemon.
//!
//! Both publish the same `Snapshot` through an `Arc<Live>` — the UI
//! layer is target-agnostic.

use std::sync::Arc;

use parking_lot::RwLock;
use serde_json::Value;

#[cfg(not(target_arch = "wasm32"))]
mod mic_discovery;
#[cfg(not(target_arch = "wasm32"))]
mod native_live;

#[cfg(target_arch = "wasm32")]
mod wasm_live;

/// Point-in-time view of everything the transport has learned.
#[derive(Clone, Default)]
pub struct Snapshot {
    pub connection: Connection,
    pub status: Option<Value>,
    pub processes: Option<Vec<Value>>,
    pub services: Option<Vec<Value>>,
    pub logs: Option<Vec<Value>>,
    /// M1.5.1b — raw `substrate/network/wifi` value (native path).
    /// Shape: `{"state": "connected"|"disconnected"|"absent", "iface"?}`.
    /// `None` on wasm / before first poll.
    pub network_wifi: Option<Value>,
    /// M1.5.1b — raw `substrate/network/ethernet` value.
    pub network_ethernet: Option<Value>,
    /// M1.5.1b — raw `substrate/network/battery` value.
    /// Shape: `{"present": bool, "percent"?: u8, "charging"?: bool}`.
    pub network_battery: Option<Value>,
    /// M1.5.1c — raw `substrate/bluetooth` value.
    /// Shape: `{"present": bool, "enabled": bool, "controller"?}`.
    pub bluetooth: Option<Value>,
    /// M1.5.1d — raw `substrate/mesh/status` value.
    /// Shape on success: `{total_nodes, healthy_nodes, ...}`.
    /// Shape on daemon unreachable: `{available: false, reason}`.
    pub mesh_status: Option<Value>,
    /// M1.5.1d — raw `substrate/chain/status` value.
    /// Shape on success: `{available: true, chain_id, sequence,
    /// event_count, ...}`. On missing `exochain` feature or daemon
    /// unreachable: `{available: false, reason}`.
    pub chain_status: Option<Value>,
    /// M1.5.2 — raw `substrate/sensor/mic` value from the
    /// [`MicrophoneAdapter`]. Shape on success:
    /// `{available: true, rms_db, peak_db, sample_rate,
    /// samples_in_window, characterization}`. On missing/truncated
    /// source: `{available: false, reason}`.
    pub audio_mic: Option<Value>,
    /// M1.5.3 — raw `substrate/sensor/tof` value from an
    /// 8×8 (or NxM) ToF depth sensor. Shape on success:
    /// `{available: true, width, height, depths_mm: [u16; w*h],
    /// min_mm?, max_mm?, frame_count?}`. Pixels that the sensor
    /// flagged as "no valid reading" are 65535 (0xFFFF) per
    /// VL53L5CX/L7CX convention.
    pub tof_depth: Option<Value>,
    /// Result of `cron.list` — array of `CronJobInfo` rows. `None` until
    /// the first poll lands. Drives the Scheduler app's table.
    pub cron_jobs: Option<Vec<Value>>,
    /// Result of `ecc.status` — RNN/vector backend stats. Shape:
    /// `{enabled, hnsw_entries, cognitive_tick?, causal_graph?, crossref_count}`.
    /// Surfaced in the Explorer header so the user can see RNN +
    /// vector-DB state at a glance.
    pub ecc_status: Option<Value>,
    pub last_error: Option<String>,
    /// Incremented every successful poll tick so the UI can detect freshness.
    pub tick: u64,
    /// Monotonic ms since app start for the most recent successful poll.
    /// `Instant` doesn't exist on wasm so we use `f64` (performance.now).
    pub last_tick_at_ms: Option<f64>,
    /// Round-trip duration of the previous successful poll in milliseconds.
    pub last_tick_dur_ms: Option<f64>,
}

#[derive(Copy, Clone, Default, PartialEq, Eq)]
pub enum Connection {
    #[default]
    Connecting,
    Connected,
    Disconnected,
}

/// Commands the UI pushes to the transport (e.g. from the Terminal block).
#[derive(Debug)]
pub enum Command {
    /// Fire a raw RPC call. Response is delivered via the oneshot reply
    /// if present, otherwise dropped.
    Raw {
        method: String,
        params: Value,
        reply: Option<ReplyTx>,
    },
}

/// Reply channel for commands. `tokio::sync::oneshot` on native, a tiny
/// futures-channel on wasm where tokio isn't available.
#[cfg(not(target_arch = "wasm32"))]
pub type ReplyTx = tokio::sync::oneshot::Sender<Result<Value, String>>;
#[cfg(target_arch = "wasm32")]
pub type ReplyTx = futures::channel::oneshot::Sender<Result<Value, String>>;

#[cfg(not(target_arch = "wasm32"))]
pub type ReplyRx = tokio::sync::oneshot::Receiver<Result<Value, String>>;
#[cfg(target_arch = "wasm32")]
pub type ReplyRx = futures::channel::oneshot::Receiver<Result<Value, String>>;

/// Create a new oneshot reply channel (target-agnostic).
pub fn reply_channel() -> (ReplyTx, ReplyRx) {
    #[cfg(not(target_arch = "wasm32"))]
    {
        tokio::sync::oneshot::channel()
    }
    #[cfg(target_arch = "wasm32")]
    {
        futures::channel::oneshot::channel()
    }
}

/// Non-blocking receive attempt on a [`ReplyRx`]. Unifies the slightly
/// different APIs tokio and futures provide.
pub enum TryReply<T> {
    Empty,
    Closed,
    Done(T),
}

pub fn try_recv_reply(rx: &mut ReplyRx) -> TryReply<Result<Value, String>> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        match rx.try_recv() {
            Ok(v) => TryReply::Done(v),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => TryReply::Empty,
            Err(tokio::sync::oneshot::error::TryRecvError::Closed) => TryReply::Closed,
        }
    }
    #[cfg(target_arch = "wasm32")]
    {
        use futures::channel::oneshot::Canceled;
        match rx.try_recv() {
            Ok(Some(v)) => TryReply::Done(v),
            Ok(None) => TryReply::Empty,
            Err(Canceled) => TryReply::Closed,
        }
    }
}

/// The public handle.
///
/// On native, `spawn()` starts a std::thread hosting tokio. On wasm,
/// `spawn()` registers a `message` event listener on the window and
/// wires `postMessage` to the VSCode extension host.
pub struct Live {
    inner: RwLock<Snapshot>,
    #[cfg(not(target_arch = "wasm32"))]
    cmd_tx: tokio::sync::mpsc::Sender<Command>,
    /// Shared substrate — populated by the native driver so
    /// [`Live::drop`] can tombstone its kernel-adapter subscriptions on
    /// shutdown. `None` until the driver has finished subscribing.
    #[cfg(not(target_arch = "wasm32"))]
    substrate: parking_lot::Mutex<Option<Arc<clawft_substrate::Substrate>>>,
    #[cfg(target_arch = "wasm32")]
    bridge: wasm_live::Bridge,
}

#[cfg(not(target_arch = "wasm32"))]
impl Drop for Live {
    /// Best-effort shutdown: ask the substrate to tombstone every
    /// kernel-adapter subscription, then abort the remaining drain
    /// tasks. We can't reliably await async work inside `Drop` because
    /// the owning tokio runtime may already be gone — we therefore
    /// prefer `Handle::try_current() + spawn` for the graceful case and
    /// fall back to dropping the `Arc<Substrate>` (whose own `Drop`
    /// synchronously aborts outstanding join handles). Documented
    /// tradeoff: callers that want a clean tombstone must drop the
    /// `Arc<Live>` while still inside the tokio runtime thread.
    fn drop(&mut self) {
        let Some(substrate) = self.substrate.lock().take() else {
            return;
        };
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            // Fire-and-forget close_all on the existing runtime.
            handle.spawn(async move {
                substrate.close_all().await;
            });
        }
        // If no runtime is current, the substrate's own `Drop` will
        // abort outstanding join handles synchronously when this scope
        // ends and the last `Arc` is released.
    }
}

/// Monotonic milliseconds since app start (cross-target).
pub fn now_ms() -> f64 {
    #[cfg(not(target_arch = "wasm32"))]
    {
        use std::sync::OnceLock;
        use std::time::Instant;
        static APP_START: OnceLock<Instant> = OnceLock::new();
        let t0 = *APP_START.get_or_init(Instant::now);
        Instant::now().duration_since(t0).as_secs_f64() * 1000.0
    }
    #[cfg(target_arch = "wasm32")]
    {
        web_sys::window()
            .and_then(|w| w.performance())
            .map(|p| p.now())
            .unwrap_or(0.0)
    }
}

impl Live {
    pub fn spawn() -> Arc<Self> {
        #[cfg(not(target_arch = "wasm32"))]
        {
            native_live::spawn()
        }
        #[cfg(target_arch = "wasm32")]
        {
            wasm_live::spawn()
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        self.inner.read().clone()
    }

    /// Snapshot the substrate state tree as an
    /// [`clawft_substrate::OntologySnapshot`]. This is the entry point
    /// for ADR-016 surface composers: they read bindings against the
    /// returned snapshot and drive canon primitives accordingly.
    ///
    /// On native: returns the live substrate state (empty until the
    /// first adapter tick lands). On wasm: the substrate is not yet
    /// wired through the webview bridge (M1.6+), so we return a
    /// best-effort snapshot assembled from the legacy `Snapshot`
    /// fields so the composer has *something* to render.
    pub fn substrate_snapshot(&self) -> clawft_substrate::OntologySnapshot {
        #[cfg(not(target_arch = "wasm32"))]
        {
            if let Some(sub) = self.substrate.lock().as_ref() {
                return sub.snapshot();
            }
            clawft_substrate::OntologySnapshot::default()
        }
        #[cfg(target_arch = "wasm32")]
        {
            // Fallback path: derive substrate paths from the legacy
            // Snapshot so the composer has data to render in the
            // webview until the real adapter bridge lands.
            //
            // M1.5.1a — mirror the native `KernelAdapter`'s projection
            // so the same admin surface bindings resolve under both
            // transports:
            // - process rows get `name` + `cpu` aliases
            // - services get per-name sub-paths
            //   (`substrate/kernel/services/<name>/status`, etc.)
            let snap = self.inner.read();
            let mut out = clawft_substrate::OntologySnapshot::default();
            if let Some(v) = &snap.status {
                out = out.with("substrate/kernel/status", v.clone());
            }
            if let Some(v) = &snap.processes {
                let projected = clawft_substrate::projection::project_process_rows(
                    serde_json::Value::Array(v.clone()),
                );
                out = out.with("substrate/kernel/processes", projected);
            }
            if let Some(v) = &snap.services {
                let raw = serde_json::Value::Array(v.clone());
                let projected = clawft_substrate::projection::project_service_rows(raw.clone());
                out = out.with("substrate/kernel/services", projected);
                for (path, value) in clawft_substrate::projection::explode_services_by_name(v) {
                    out = out.with(path, value);
                }
            }
            if let Some(v) = &snap.logs {
                out = out.with("substrate/kernel/logs", serde_json::Value::Array(v.clone()));
            }
            out
        }
    }

    pub fn submit(&self, cmd: Command) -> bool {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.cmd_tx.try_send(cmd).is_ok()
        }
        #[cfg(target_arch = "wasm32")]
        {
            self.bridge.submit(cmd)
        }
    }

    #[allow(dead_code)]
    pub(crate) fn write(&self, mut f: impl FnMut(&mut Snapshot)) {
        f(&mut self.inner.write());
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
impl Live {
    /// Offline `Live` for render tests: no transport, no daemon, no network.
    /// Commands submitted by the UI are accepted and discarded.
    pub(crate) fn test_stub(snap: Snapshot) -> Arc<Self> {
        let (cmd_tx, rx) = tokio::sync::mpsc::channel(4096);
        std::mem::forget(rx);
        Arc::new(Self {
            inner: RwLock::new(snap),
            cmd_tx,
            substrate: parking_lot::Mutex::new(None),
        })
    }
}
