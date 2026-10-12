//! Real-leaf backend: a display session on a certified leaf, carried by
//! the daemon (`display.*` RPC, ADR-117 D1/D2).
//!
//! The app never speaks to the pad. It holds a Unix-socket connection to
//! the daemon, which owns the certified-leaf channel, the grant check and
//! the session on the leaf. Ops go out as one CBOR `SceneEnvelope` per
//! call; events come back by polling at a bounded rate. The wire between
//! daemon and pad is `weftos_leaf_scene::session`.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};
use weftos_leaf_renderer::CapabilityMask;
use weftos_leaf_scene::session::b64;
use weftos_leaf_scene::{codec, BitmapFormat, DisplayId, SceneEnvelope};

use super::{Backend, BackendEvent};
use crate::caps::{DisplayCapabilities, DisplaySize, InputKinds};
use crate::error::DisplayError;
use crate::event::SensorValue;
use crate::grant::DisplayGrant;
use crate::target::LeafEndpoint;

/// Wire protocol number the daemon expects on a request.
const PROTO: u32 = 1;
/// Least time between two event polls, so a 30 Hz frame loop does not
/// make 30 round trips a second for an idle pad.
const POLL_EVERY: Duration = Duration::from_millis(25);
const IO_TIMEOUT: Duration = Duration::from_secs(15);

/// Where the daemon's socket is when the endpoint does not say:
/// `$WEFTOS_RUNTIME_DIR/kernel.sock`, else `~/.clawft/kernel.sock`.
pub fn default_socket() -> PathBuf {
    if let Some(dir) = std::env::var_os("WEFTOS_RUNTIME_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir).join("kernel.sock");
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    home.join(".clawft").join("kernel.sock")
}

/// One JSON-lines connection to the daemon.
struct Rpc {
    reader: BufReader<UnixStream>,
    next_id: u64,
}

impl Rpc {
    fn connect(path: &PathBuf) -> Result<Self, DisplayError> {
        let stream = UnixStream::connect(path)
            .map_err(|e| DisplayError::Backend(format!("cannot reach the daemon at {}: {e}", path.display())))?;
        let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
        let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
        Ok(Self { reader: BufReader::new(stream), next_id: 1 })
    }

    /// One request, one reply. `Err` carries the daemon's `error_kind`
    /// and message; a dead socket is `Closed`.
    fn call(&mut self, method: &str, params: Value) -> Result<Value, DisplayError> {
        let id = self.next_id.to_string();
        self.next_id += 1;
        // Same implicit scope as the CLI's daemon client: the daemon honours
        // literal scopes only from a same-uid Unix peer, and `display.*` is
        // a user-level Admin act outside a project.
        let mut line = json!({"method": method, "params": params, "id": id, "auth": "admin", "proto": PROTO}).to_string();
        line.push('\n');
        self.reader.get_mut().write_all(line.as_bytes()).map_err(|_| DisplayError::Closed)?;
        let mut reply = String::new();
        let n = self.reader.read_line(&mut reply).map_err(|_| DisplayError::Closed)?;
        if n == 0 {
            return Err(DisplayError::Closed);
        }
        let v: Value = serde_json::from_str(reply.trim()).map_err(|e| DisplayError::Backend(format!("bad reply from the daemon: {e}")))?;
        if v.get("ok").and_then(Value::as_bool) == Some(true) {
            return Ok(v.get("result").cloned().unwrap_or(Value::Null));
        }
        let kind = v.get("error_kind").and_then(Value::as_str).unwrap_or("");
        let msg = v.get("error").and_then(Value::as_str).unwrap_or("the daemon refused the call").to_owned();
        Err(match kind {
            "display_denied" => DisplayError::Denied(leak(msg)),
            "display_no_session" | "display_ended" => DisplayError::Closed,
            _ => DisplayError::Backend(format!("{method}: {msg}")),
        })
    }
}

/// `DisplayError::Denied` carries a `&'static str`; a denial is rare and
/// small, so leaking its text is the honest way to keep the daemon's words.
fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

#[derive(Deserialize)]
struct OpenReply {
    session: u64,
    width: u32,
    height: u32,
    render_caps: u32,
    touch: bool,
    ptt: bool,
}

#[derive(Deserialize)]
struct PollReply {
    #[serde(default)]
    events: Vec<Value>,
    #[serde(default)]
    rejected: Vec<String>,
    #[serde(default)]
    ended: Option<String>,
}

pub(crate) struct LeafBackend {
    rpc: Rpc,
    session: u64,
    display: DisplayId,
    size: DisplaySize,
    caps: DisplayCapabilities,
    queue: VecDeque<BackendEvent>,
    last_poll: Instant,
    /// The leaf refused a batch; the next `apply` reports it.
    rejection: Option<String>,
    closed: bool,
}

/// Entry point the session calls for [`crate::DisplayTarget::Leaf`].
pub(crate) fn open(endpoint: &LeafEndpoint, grant: &DisplayGrant) -> Result<Box<dyn Backend>, DisplayError> {
    let path = endpoint.daemon.clone().unwrap_or_else(default_socket);
    let mut rpc = Rpc::connect(&path)?;
    let resource = if endpoint.name.is_empty() { grant.resource.clone() } else { endpoint.name.clone() };
    let reply = rpc.call(
        "display.open",
        json!({
            "leaf_id": endpoint.leaf_id,
            "subject": grant.subject,
            "resource": resource,
            "proof_hex": grant.proof.iter().map(|b| format!("{b:02x}")).collect::<String>(),
            "display": endpoint.display,
        }),
    )?;
    let r: OpenReply = serde_json::from_value(reply).map_err(|e| DisplayError::Backend(format!("bad display.open reply: {e}")))?;
    let caps = DisplayCapabilities {
        render: CapabilityMask::from_bits_truncate(r.render_caps),
        image_formats: vec![BitmapFormat::Raw565],
        video: None,
        input: InputKinds { touch: r.touch, buttons: if r.ptt { vec!["ptt".to_string()] } else { Vec::new() } },
    };
    Ok(Box::new(LeafBackend {
        rpc,
        session: r.session,
        display: endpoint.display,
        size: DisplaySize { width: r.width, height: r.height },
        caps,
        queue: VecDeque::new(),
        last_poll: Instant::now(),
        rejection: None,
        closed: false,
    }))
}

impl LeafBackend {
    fn pull(&mut self) {
        self.last_poll = Instant::now();
        let reply = match self.rpc.call("display.poll", json!({"session": self.session, "max": 256})) {
            Ok(v) => v,
            Err(_) => return self.hang_up(),
        };
        let Ok(p) = serde_json::from_value::<PollReply>(reply) else { return self.hang_up() };
        for ev in p.events {
            if let Some(e) = decode_event(&ev) {
                self.queue.push_back(e);
            }
        }
        if let Some(r) = p.rejected.into_iter().last() {
            self.rejection = Some(r);
        }
        if p.ended.is_some() {
            self.hang_up();
        }
    }

    fn hang_up(&mut self) {
        if !self.closed {
            self.closed = true;
            self.queue.push_back(BackendEvent::Closed);
        }
    }
}

fn decode_event(ev: &Value) -> Option<BackendEvent> {
    match ev.get("kind")?.as_str()? {
        "input" => {
            let cbor = b64::decode(ev.get("cbor_b64")?.as_str()?.as_bytes())?;
            codec::decode_input_envelope(&cbor).ok().map(BackendEvent::Input)
        }
        "sensor" => {
            let name = ev.get("name")?.as_str()?.to_owned();
            let value = if let Some(b) = ev.get("pressed").and_then(Value::as_bool) {
                SensorValue::Bool(b)
            } else {
                SensorValue::Float(ev.get("value").and_then(Value::as_f64)?)
            };
            Some(BackendEvent::Sensor { name, value })
        }
        _ => None,
    }
}

impl Backend for LeafBackend {
    fn display_id(&self) -> DisplayId {
        self.display
    }

    fn size(&self) -> DisplaySize {
        self.size
    }

    fn capabilities(&self) -> DisplayCapabilities {
        self.caps.clone()
    }

    fn apply(&mut self, env: &SceneEnvelope) -> Result<(), DisplayError> {
        if self.closed {
            return Err(DisplayError::Closed);
        }
        if let Some(why) = self.rejection.take() {
            return Err(DisplayError::Backend(format!("the leaf refused an earlier batch: {why}")));
        }
        let cbor = codec::encode(env).map_err(|e| DisplayError::Backend(format!("cannot encode ops: {e}")))?;
        match self.rpc.call("display.ops", json!({"session": self.session, "cbor_b64": b64::encode(&cbor)})) {
            Ok(_) => Ok(()),
            Err(DisplayError::Closed) => {
                self.hang_up();
                Err(DisplayError::Closed)
            }
            Err(e) => Err(e),
        }
    }

    fn poll(&mut self) -> Option<BackendEvent> {
        if self.queue.is_empty() && !self.closed && self.last_poll.elapsed() >= POLL_EVERY {
            self.pull();
        }
        self.queue.pop_front()
    }

    fn close(&mut self) {
        if !self.closed || self.session != 0 {
            let _ = self.rpc.call("display.close", json!({"session": self.session}));
        }
        self.closed = true;
        self.session = 0;
        self.queue.clear();
    }
}
