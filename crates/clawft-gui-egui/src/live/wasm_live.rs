//! Wasm transport — runs inside a VSCode / Cursor WebviewPanel.
//!
//! Unix sockets are unreachable from inside a sandboxed webview, so
//! instead we ride the `postMessage` bridge provided by the VSCode
//! extension host:
//!
//! ```text
//!   egui (wasm) ─ postMessage ──► VSCode extension ─ UDS ──► daemon
//!   egui (wasm) ◄ postMessage ──  VSCode extension ◄ UDS ──  daemon
//! ```
//!
//! The extension's webview-side script (`media/main.js`) relays every
//! RPC verb to/from the extension host via `acquireVsCodeApi()`. From
//! the wasm side all we see is a typed message channel on the
//! `window` object.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex as PLMutex;
use parking_lot::RwLock;
use serde_json::{Value, json};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;

use super::{Command, Connection, Live, Snapshot, now_ms};

/// Poll cadence (ms). Matches the native transport for predictable UX.
const POLL_INTERVAL_MS: i32 = 1000;
const LOG_TAIL: usize = 200;

/// Handle held by `Live` on wasm. Owns the pending-RPC registry and
/// the request sender.
pub(super) struct Bridge {
    /// Pending responses keyed by the numeric `id` we assigned.
    pending: Arc<PLMutex<HashMap<u64, ReplySlot>>>,
    /// Monotonic id allocator.
    next_id: Arc<PLMutex<u64>>,
}

enum ReplySlot {
    Internal(Box<dyn FnOnce(Result<Value, String>) + Send + 'static>),
    External(super::ReplyTx),
}

impl Bridge {
    pub(super) fn submit(&self, cmd: Command) -> bool {
        let Command::Raw {
            method,
            params,
            reply,
        } = cmd;
        let id = {
            let mut n = self.next_id.lock();
            let id = *n;
            *n = n.wrapping_add(1);
            id
        };
        if let Some(tx) = reply {
            self.pending.lock().insert(id, ReplySlot::External(tx));
        }
        post_rpc(id, &method, params)
    }
}

pub(super) fn spawn() -> Arc<Live> {
    console_error_panic_hook::set_once();

    let pending = Arc::new(PLMutex::new(HashMap::<u64, ReplySlot>::new()));
    let next_id = Arc::new(PLMutex::new(1u64));
    let live = Arc::new(Live {
        inner: RwLock::new(Snapshot::default()),
        bridge: Bridge {
            pending: Arc::clone(&pending),
            next_id: Arc::clone(&next_id),
        },
    });

    install_message_listener(Arc::clone(&live), Arc::clone(&pending));
    install_poll_timer(
        Arc::clone(&live),
        Arc::clone(&pending),
        Arc::clone(&next_id),
    );

    live
}

/// Install a `message` event listener on the window that routes
/// `{ type: "rpc-response", id, ok, result?, error?, variant-id? }`
/// shapes into the pending-RPC registry. Optional `variant-id` is
/// WEFT-283 pulse attribution (echoed by the extension host); plain
/// RPC without the field remains valid. `radar-return-ack` is accepted
/// and currently ignored at the Live layer (channel contract only —
/// full ECC/GEPA consumption lands with the M2 radar loop).
fn install_message_listener(live: Arc<Live>, pending: Arc<PLMutex<HashMap<u64, ReplySlot>>>) {
    let window = web_sys::window().expect("no global window");

    let closure =
        Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |ev: web_sys::MessageEvent| {
            let data = ev.data();
            let value = match js_value_to_json(&data) {
                Some(v) => v,
                None => return,
            };
            let kind = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
            match kind {
                "rpc-response" => {
                    let id = value.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
                    let ok = value.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
                    // variant-id may be present (string or number); ignored
                    // for snapshot routing but must not break parse.
                    let _variant_id = value.get("variant-id");
                    let result = if ok {
                        Ok(value.get("result").cloned().unwrap_or(Value::Null))
                    } else {
                        Err(value
                            .get("error")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown error")
                            .to_string())
                    };
                    let maybe = pending.lock().remove(&id);
                    if let Some(slot) = maybe {
                        match slot {
                            ReplySlot::External(tx) => {
                                let _ = tx.send(result.clone());
                            }
                            ReplySlot::Internal(cb) => cb(result.clone()),
                        }
                    }
                    if let Err(e) = &result {
                        live.write(|s| s.last_error = Some(e.clone()));
                    }
                }
                // WEFT-283: host ack for typed radar-return. No Live
                // state yet — drop so unknown-message path stays quiet.
                "radar-return-ack" => {}
                _ => { /* ignore unknown messages */ }
            }
        });

    window
        .add_event_listener_with_callback("message", closure.as_ref().unchecked_ref())
        .expect("add message listener");
    closure.forget();
}

fn install_poll_timer(
    live: Arc<Live>,
    pending: Arc<PLMutex<HashMap<u64, ReplySlot>>>,
    next_id: Arc<PLMutex<u64>>,
) {
    let window = web_sys::window().expect("no global window");

    let tick = Closure::<dyn FnMut()>::new(move || {
        let live = Arc::clone(&live);
        let pending = Arc::clone(&pending);
        let next_id = Arc::clone(&next_id);

        let started_ms = now_ms();
        let partial = Arc::new(PLMutex::new(PartialPoll::default()));

        // Fire the polled RPCs in parallel via postMessage. When all
        // expected replies are in, update the snapshot. Note: cluster.*
        // / chain.* / cron.list / ecc.status may return error against
        // older daemons — `into_snapshot` treats those as "subsystem
        // unavailable" rather than rolling the whole connection back to
        // disconnected.
        let methods: &[(&str, Value, fn(&mut PartialPoll, Result<Value, String>))] = &[
            ("kernel.status", Value::Null, |p, r| p.status = Some(r)),
            ("kernel.ps", Value::Null, |p, r| p.ps = Some(r)),
            ("kernel.services", Value::Null, |p, r| p.services = Some(r)),
            ("kernel.logs", json!({ "count": LOG_TAIL }), |p, r| {
                p.logs = Some(r)
            }),
            ("cluster.status", Value::Null, |p, r| p.mesh = Some(r)),
            ("chain.status", Value::Null, |p, r| p.chain = Some(r)),
            ("cron.list", Value::Null, |p, r| p.cron = Some(r)),
            ("ecc.status", Value::Null, |p, r| p.ecc = Some(r)),
        ];

        for (method, params, setter) in methods {
            let id = {
                let mut n = next_id.lock();
                let id = *n;
                *n = n.wrapping_add(1);
                id
            };
            let partial_c = Arc::clone(&partial);
            let live_c = Arc::clone(&live);
            let setter = *setter;
            let cb: Box<dyn FnOnce(Result<Value, String>) + Send + 'static> =
                Box::new(move |result| {
                    let snap_opt = {
                        let mut p = partial_c.lock();
                        (setter)(&mut p, result);
                        if p.is_complete() {
                            let taken = std::mem::take(&mut *p);
                            let finished_ms = now_ms();
                            Some(taken.into_snapshot(started_ms, finished_ms))
                        } else {
                            None
                        }
                    };
                    if let Some(snap) = snap_opt {
                        live_c.write(move |s| {
                            let tick = s.tick.wrapping_add(1);
                            *s = snap.clone();
                            s.tick = tick;
                        });
                    }
                });
            pending.lock().insert(id, ReplySlot::Internal(cb));
            let _ = post_rpc(id, method, params.clone());
        }
    });

    window
        .set_interval_with_callback_and_timeout_and_arguments_0(
            tick.as_ref().unchecked_ref(),
            POLL_INTERVAL_MS,
        )
        .expect("set interval");
    tick.forget();
}

fn post_rpc(id: u64, method: &str, params: Value) -> bool {
    // Plain RPC (no variant-id) — backward compatible. Callers that
    // need pulse attribution use `post_rpc_with_variant`.
    post_rpc_with_variant(id, method, params, None)
}

/// WEFT-283: optional `variant-id` on rpc-request is echoed by the
/// extension host on the matching rpc-response (ADR-007 discipline).
fn post_rpc_with_variant(id: u64, method: &str, params: Value, variant_id: Option<Value>) -> bool {
    let mut payload = json!({
        "type": "rpc-request",
        "id": id,
        "method": method,
        "params": params,
    });
    if let Some(vid) = variant_id {
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("variant-id".into(), vid);
        }
    }
    let s = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".into());
    post_to_host(&s)
}

/// Emit a typed active-radar return observation (WEFT-283 / ADR-007).
/// Prefers `window.__weftPostRadarReturn` when the bootstrap installed
/// it; falls back to generic `__weftPostToHost`.
#[allow(dead_code)] // public surface for future egui return-signal path
pub(super) fn post_radar_return(observation: Value) -> bool {
    let Some(window) = web_sys::window() else {
        return false;
    };
    // Prefer the typed helper so camelCase → kebab-case normalization
    // happens in the bootstrap (variantId → variant-id, etc.).
    if let Ok(host_fn) = js_sys::Reflect::get(&window, &JsValue::from_str("__weftPostRadarReturn"))
    {
        if let Some(func) = host_fn.dyn_ref::<js_sys::Function>() {
            let s = serde_json::to_string(&observation).unwrap_or_else(|_| "{}".into());
            if let Ok(parsed) = js_sys::JSON::parse(&s) {
                return func
                    .call1(&JsValue::NULL, &parsed)
                    .ok()
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
            }
        }
    }
    let mut payload = observation;
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("type".into(), json!("radar-return"));
    }
    let s = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".into());
    post_to_host(&s)
}

/// Send a message up to the extension host via `acquireVsCodeApi().postMessage(...)`.
/// The first call stashes the vscode api handle on `window` so we don't
/// acquire twice (which VSCode forbids).
fn post_to_host(payload_json: &str) -> bool {
    let Some(window) = web_sys::window() else {
        return false;
    };
    let host_fn = js_sys::Reflect::get(&window, &JsValue::from_str("__weftPostToHost")).ok();
    let func = match host_fn
        .as_ref()
        .and_then(|v| v.dyn_ref::<js_sys::Function>())
    {
        Some(f) => f,
        None => return false,
    };
    let parsed = js_sys::JSON::parse(payload_json).unwrap_or(JsValue::NULL);
    func.call1(&JsValue::NULL, &parsed).is_ok()
}

fn js_value_to_json(v: &JsValue) -> Option<Value> {
    let s = js_sys::JSON::stringify(v).ok()?;
    let s = s.as_string()?;
    serde_json::from_str(&s).ok()
}

#[derive(Default)]
struct PartialPoll {
    status: Option<Result<Value, String>>,
    ps: Option<Result<Value, String>>,
    services: Option<Result<Value, String>>,
    logs: Option<Result<Value, String>>,
    mesh: Option<Result<Value, String>>,
    chain: Option<Result<Value, String>>,
    cron: Option<Result<Value, String>>,
    ecc: Option<Result<Value, String>>,
}

impl PartialPoll {
    fn is_complete(&self) -> bool {
        self.status.is_some()
            && self.ps.is_some()
            && self.services.is_some()
            && self.logs.is_some()
            && self.mesh.is_some()
            && self.chain.is_some()
            && self.cron.is_some()
            && self.ecc.is_some()
    }

    fn into_snapshot(self, started_ms: f64, finished_ms: f64) -> Snapshot {
        let as_array = |r: &Result<Value, String>| -> Option<Vec<Value>> {
            r.as_ref().ok().and_then(|v| v.as_array().cloned())
        };
        let status = self.status.as_ref().and_then(|r| r.as_ref().ok().cloned());
        // Connection state is driven by the *core* kernel RPCs; the
        // optional ones (cluster/chain/cron/ecc) may return error
        // against older daemons or in feature-disabled builds without
        // implying the whole link is down.
        let err = [&self.status, &self.ps, &self.services, &self.logs]
            .iter()
            .find_map(|r| r.as_ref().and_then(|rr| rr.as_ref().err().cloned()));

        // mesh — pass through the cluster.status object on success so
        // the Monitor mesh tile / tray chip can render counts. On
        // error, we synthesize an `available: false` envelope so the
        // tile shows the reason rather than a stale "no data" hint.
        let mesh_status = match self.mesh.as_ref() {
            Some(Ok(v)) => {
                let mut v = v.clone();
                if let Value::Object(ref mut obj) = v {
                    obj.entry("available").or_insert(Value::Bool(true));
                }
                Some(v)
            }
            Some(Err(e)) => Some(json!({
                "available": false,
                "reason": e,
            })),
            None => None,
        };
        // chain — the daemon's chain.status returns the bare
        // ChainStatusResult on success; we inject `available: true` to
        // match the native ChainAdapter's contract so monitor.rs and
        // the witness-chain chip composer can rely on a single shape.
        let chain_status = match self.chain.as_ref() {
            Some(Ok(v)) => {
                let mut v = v.clone();
                if let Value::Object(ref mut obj) = v {
                    obj.insert("available".into(), Value::Bool(true));
                }
                Some(v)
            }
            Some(Err(e)) => Some(json!({
                "available": false,
                "reason": e,
            })),
            None => None,
        };
        let cron_jobs = as_array(self.cron.as_ref().unwrap_or(&Err(String::new())));
        let ecc_status = self.ecc.as_ref().and_then(|r| r.as_ref().ok().cloned());

        Snapshot {
            connection: if err.is_some() {
                Connection::Disconnected
            } else {
                Connection::Connected
            },
            status,
            processes: as_array(self.ps.as_ref().unwrap_or(&Err(String::new()))),
            services: as_array(self.services.as_ref().unwrap_or(&Err(String::new()))),
            logs: as_array(self.logs.as_ref().unwrap_or(&Err(String::new()))),
            // M1.5.1b — NetworkAdapter is native-only; wasm path
            // reports None so the tray shows grey chips until the
            // substrate-over-postMessage bridge lands (M1.6+).
            network_wifi: None,
            network_ethernet: None,
            network_battery: None,
            // M1.5.1c — BluetoothAdapter is native-only for the same
            // reason as the network adapter.
            bluetooth: None,
            // mesh/chain are now polled directly via cluster.status /
            // chain.status RPC — both are in the extension allowlist
            // (added M1.5.1d). Audio + ToF still need the substrate-
            // over-postMessage bridge (tracked M1.6+).
            mesh_status,
            chain_status,
            audio_mic: None,
            tof_depth: None,
            cron_jobs,
            ecc_status,
            last_error: err,
            tick: 0,
            last_tick_at_ms: Some(finished_ms),
            last_tick_dur_ms: Some(finished_ms - started_ms),
        }
    }
}
