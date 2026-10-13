//! Mesh transport client — std::net port of the embassy-net client at
//! `clawft-edge-pad/src/mesh.rs`.
//!
//! Outbound control uses the ADR-103 certified leaf protocol:
//! - Framing: `[4-byte big-endian length][WLF1 || CBOR SignedPublish]`.
//! - Subscribe: a signed `MeshIpcEnvelope` with `target = Topic("mesh.subscribe")`
//!   and `payload = Json({"topic": "<our push topic>"})`.
//! - Discovery: a nonce-bound machine-signed UDP advertisement selects the
//!   parent endpoint. The publish remains in NVS until its WLA1 ACK verifies.
//! - Inbound leaf-push: a `MeshIpcEnvelope` carrying `{"type":"leaf_push",
//!   "cbor_b64":"<base64 CBOR>",...}`. We extract the base64, decode to
//!   CBOR, decode to `LeafPush`.
//!
//! Inbound display pushes still use the existing JSON mesh envelope.
//! Display sessions (`session.rs`) ride the same push carrier; their
//! replies and the touch / push-to-talk events go out as signed `.input`
//! publishes through the same durable journal.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, UdpSocket};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::Duration;

use log::{info, warn};

use weftos_leaf_types::LeafPush;
use crate::scene::LeafScene;
use ed25519_dalek::SigningKey;
use esp_idf_svc::nvs::EspDefaultNvs;
use weftos_leaf_types::push_topic;
use weftos_leaf_scene::session::{b64, envelope as wire, SessionFrame};
use weftos_leaf_scene::{InputEnvelope, InputEvent};
use weftos_leaf_types::link::{LeafCertificate, OfflineJournal, ParentAdvertisement, PublishAck, FRAME_MAGIC, ACK_MAGIC};

use crate::display::DpiDisplay;
use crate::session::PadSession;

// Per-device credentials come from the operator's provisioned files at
// build time. An image built without them boots the display but never
// connects to the mesh or emits unsigned traffic.
include!(concat!(env!("OUT_DIR"), "/leaf_provision.rs"));
const MAX_NVS_JOURNAL: usize = 4096;

const RX_BUF: usize = 8192;

fn input_envelope(id: &str, topic: &str, event: InputEvent) -> anyhow::Result<String> {
    let input = InputEnvelope::new(0, None, event);
    let cbor = weftos_leaf_scene::codec::encode(&input)
        .map_err(|e| anyhow::anyhow!("encode touch CBOR: {e:?}"))?;
    let inner = serde_json::json!({
        "type": "leaf_input", "cbor_b64": b64::encode(&cbor), "target_pubkey": id
    }).to_string();
    Ok(serde_json::json!({
        "source_node": id, "dest_node": "daemon",
        "message": {"id": "leaf-input", "from": 0,
            "target": {"Topic": "ipc.publish"},
            "payload": {"Json": {"topic": topic, "message": inner}},
            "timestamp": "2026-05-14T00:00:00Z"},
        "hop_count": 0, "envelope_id": "leaf-env-input"
    }).to_string())
}

/// Write a `[4-byte BE length][payload]` frame to the socket.
fn write_frame(sock: &mut TcpStream, payload: &[u8]) -> std::io::Result<()> {
    let len = (payload.len() as u32).to_be_bytes();
    sock.write_all(&len)?;
    sock.write_all(payload)?;
    Ok(())
}

/// Read exactly `buf.len()` bytes or fail.
fn read_exact(sock: &mut TcpStream, buf: &mut [u8]) -> std::io::Result<()> {
    sock.read_exact(buf)
}

/// Events parked because the journal was full.
#[derive(Default)]
struct Held {
    touch: Option<InputEvent>,
    ptt: Option<bool>,
}

/// A touch event as the session's `Input` frame: hit-tested against the
/// retained scene here, so the host gets the object, not the coordinates.
fn touch_frame(scene: &LeafScene, session: &PadSession, event: InputEvent) -> Option<SessionFrame> {
    let node = match event {
        InputEvent::PointerDown { x, y, .. } | InputEvent::PointerMove { x, y, .. } | InputEvent::PointerUp { x, y, .. } => {
            scene.hit_test(x, y)
        }
        _ => None,
    };
    session.input_frame(InputEnvelope::new(crate::scene::DISPLAY_ID, node, event))
}

struct LeafLink {
    cert: LeafCertificate,
    key: SigningKey,
    queue: OfflineJournal,
    nvs: EspDefaultNvs,
}

impl LeafLink {
    fn load(nvs: EspDefaultNvs) -> anyhow::Result<Self> {
        anyhow::ensure!(LEAF_SEED.len() == 32 && !LEAF_CERT.is_empty(), "leaf image has no provisioned identity");
        let seed: [u8; 32] = LEAF_SEED.try_into()?;
        let key = SigningKey::from_bytes(&seed);
        let cert: LeafCertificate = weftos_leaf_types::decode(LEAF_CERT)
            .map_err(|e| anyhow::anyhow!("decode leaf certificate: {e:?}"))?;
        let wall = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        cert.verify(&cert.parent_pubkey, wall.max(cert.issued_at)).map_err(|e| anyhow::anyhow!("leaf certificate: {e:?}"))?;
        anyhow::ensure!(cert.leaf_pubkey == key.verifying_key().to_bytes(), "leaf key/certificate mismatch");
        let queue = match nvs.blob_len("journal")? {
            None => OfflineJournal::default(),
            Some(len) => {
                anyhow::ensure!(len <= MAX_NVS_JOURNAL, "NVS leaf journal exceeds bound");
                let mut buf = vec![0u8; len];
                let bytes = nvs.get_blob("journal", &mut buf)?.ok_or_else(|| anyhow::anyhow!("NVS leaf journal disappeared"))?;
                weftos_leaf_types::decode(bytes)
                    .map_err(|e| anyhow::anyhow!("decode NVS leaf journal: {e:?}"))?
            }
        };
        queue.validate(&cert).map_err(|e| anyhow::anyhow!("NVS leaf journal invalid: {e:?}"))?;
        Ok(Self { cert, key, queue, nvs })
    }

    fn save_queue(&mut self, next: OfflineJournal) -> anyhow::Result<()> {
        let bytes = weftos_leaf_types::encode(&next)
            .map_err(|e| anyhow::anyhow!("encode NVS leaf journal: {e:?}"))?;
        anyhow::ensure!(bytes.len() <= MAX_NVS_JOURNAL, "NVS leaf journal full");
        // EspNvs::set_blob commits before returning. Never send a frame
        // until this succeeds; never forget one until ACK persistence does.
        self.nvs.set_blob("journal", &bytes)?;
        self.queue = next;
        Ok(())
    }

    fn subscribe(&mut self) -> anyhow::Result<()> {
        let id = self.cert.leaf_id();
        let topic = push_topic(&id);
        let envelope = wire::subscribe_envelope(&id, &topic);
        let mut next = self.queue.clone();
        next.enqueue(&self.cert, &self.key, "mesh.subscribe".into(), envelope.into_bytes())
            .map_err(|e| anyhow::anyhow!("signed subscribe: {e:?}"))?;
        self.save_queue(next)
    }

    /// Journal one signed publish. False means the bounded journal is full:
    /// the caller retains the event and backpressures its producer until
    /// replay frees space.
    fn enqueue_payload(&mut self, topic: String, envelope: String) -> anyhow::Result<bool> {
        let mut next = self.queue.clone();
        match next.enqueue(&self.cert, &self.key, topic, envelope.into_bytes()) {
            Ok(_) => {},
            Err(weftos_leaf_types::link::LinkError::JournalFull) => return Ok(false),
            Err(e) => return Err(anyhow::anyhow!("signed input journal: {e:?}")),
        }
        let bytes = weftos_leaf_types::encode(&next)
            .map_err(|e| anyhow::anyhow!("encode NVS input journal: {e:?}"))?;
        if bytes.len() > MAX_NVS_JOURNAL { return Ok(false); }
        self.nvs.set_blob("journal", &bytes)?;
        self.queue = next;
        Ok(true)
    }

    /// A raw touch observation (no session holds the screen).
    fn enqueue_input(&mut self, event: InputEvent) -> anyhow::Result<bool> {
        let id = self.cert.leaf_id();
        let topic = format!("mesh.leaf.{id}.input");
        let envelope = input_envelope(&id, &topic, event)?;
        self.enqueue_payload(topic, envelope)
    }

    /// A display-session frame for the host.
    fn enqueue_frame(&mut self, frame: &SessionFrame) -> anyhow::Result<bool> {
        let (topic, envelope) = wire::session_publish(&self.cert.leaf_id(), frame)
            .map_err(|e| anyhow::anyhow!("encode session frame: {e}"))?;
        self.enqueue_payload(topic, envelope)
    }

    /// Journal everything waiting to go out: the session's replies first,
    /// then button edges, then touch. A full journal parks the one event
    /// in `held` and stops; replay frees space and the next pass resumes.
    fn stage_all(
        &mut self,
        touch_rx: &Receiver<InputEvent>,
        ptt_rx: &Receiver<bool>,
        held: &mut Held,
        scene: &LeafScene,
        session: &mut PadSession,
    ) -> anyhow::Result<()> {
        while let Some(frame) = session.outbox_front() {
            if !self.enqueue_frame(frame)? {
                warn!("[mesh] NVS leaf journal full; session replies wait for replay");
                return Ok(());
            }
            session.outbox_pop();
        }
        loop {
            let pressed = match held.ptt.take() {
                Some(p) => p,
                None => match ptt_rx.try_recv() {
                    Ok(p) => p,
                    Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
                },
            };
            match session.ptt_frame(pressed) {
                Some(frame) => {
                    if !self.enqueue_frame(&frame)? {
                        held.ptt = Some(pressed);
                        warn!("[mesh] NVS leaf journal full; button edge waits for replay");
                        return Ok(());
                    }
                }
                None => info!("[mesh] ptt {} dropped: no session holds the screen", if pressed { "down" } else { "up" }),
            }
        }
        loop {
            let event = match held.touch.take() {
                Some(event) => event,
                None => match touch_rx.try_recv() {
                    Ok(event) => event,
                    Err(TryRecvError::Empty | TryRecvError::Disconnected) => return Ok(()),
                },
            };
            let stored = match touch_frame(scene, session, event) {
                Some(frame) => self.enqueue_frame(&frame)?,
                None => self.enqueue_input(event)?,
            };
            if !stored {
                held.touch = Some(event);
                warn!("[mesh] NVS leaf journal full; touch producer backpressured until replay");
                return Ok(());
            }
        }
    }

    /// Probe the parent: the build-time hint first (unicast, retried), then
    /// broadcast. Only a machine-signed, nonce-bound advertisement is accepted,
    /// and its advertised endpoint is what the caller connects to.
    fn discover(&self) -> anyhow::Result<SocketAddr> {
        let hint = match weftos_leaf_types::discovery::parse_parent_hint(PARENT_HINT) {
            Ok(h) => h,
            Err(e) => {
                warn!("[mesh] ignoring parent hint {PARENT_HINT:?}: {e}; broadcast only");
                None
            }
        };
        let socket = UdpSocket::bind("0.0.0.0:0")?;
        socket.set_broadcast(true)?;
        socket.set_read_timeout(Some(Duration::from_secs(3)))?;
        let mut last = anyhow::anyhow!("no discovery target");
        for target in weftos_leaf_types::discovery::probe_plan(hint) {
            let mut nonce = [0u8; 16];
            unsafe { esp_idf_svc::sys::esp_fill_random(nonce.as_mut_ptr().cast(), nonce.len()) };
            let mut request = Vec::with_capacity(52);
            request.extend_from_slice(b"WLD1");
            request.extend_from_slice(self.cert.leaf_id().as_bytes());
            request.extend_from_slice(&nonce);
            info!("[mesh] discovery probe -> {target}");
            if let Err(e) = socket.send_to(&request, target) {
                last = e.into();
                continue;
            }
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            let mut response = [0u8; 1024];
            while std::time::Instant::now() < deadline {
                let n = match socket.recv_from(&mut response) {
                    Ok((n, _)) => n,
                    Err(e) => { last = e.into(); break; }
                };
                let Ok(ad) = weftos_leaf_types::decode::<ParentAdvertisement>(&response[..n]) else { continue };
                // The fresh random nonce prevents replay even before this
                // device's wall clock has been synchronized.
                let wall = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
                if ad.verify_for(&self.cert.mesh_pubkey, &self.cert.parent_scope, wall, &nonce).is_ok() {
                    return ad.endpoint.parse().map_err(Into::into);
                }
            }
        }
        Err(last)
    }

    fn replay(&mut self, sock: &mut TcpStream, mut on_push: impl FnMut(&[u8])) -> anyhow::Result<()> {
        while let Some(frame) = self.queue.oldest().cloned() {
            let mut wire = FRAME_MAGIC.to_vec();
            wire.extend_from_slice(&weftos_leaf_types::encode(&frame)
                .map_err(|e| anyhow::anyhow!("encode signed leaf frame: {e:?}"))?);
            anyhow::ensure!(wire.len() <= 32 * 1024, "signed leaf frame exceeds mesh bound");
            write_frame(sock, &wire)?;
            let ack = loop {
                let mut len_bytes = [0u8; 4];
                read_exact(sock, &mut len_bytes)?;
                let len = u32::from_be_bytes(len_bytes) as usize;
                anyhow::ensure!((1..=RX_BUF).contains(&len), "invalid mesh reply length");
                let mut reply = vec![0u8; len];
                read_exact(sock, &mut reply)?;
                if reply.starts_with(ACK_MAGIC) {
                    let ack: PublishAck = weftos_leaf_types::decode(&reply[ACK_MAGIC.len()..])
                        .map_err(|e| anyhow::anyhow!("decode signed ACK: {e:?}"))?;
                    break ack;
                }
                on_push(&reply);
            };
            let mut next = self.queue.clone();
            next.acknowledge(&ack).map_err(|e| anyhow::anyhow!("signed ACK rejected: {e:?}"))?;
            self.save_queue(next)?;
        }
        Ok(())
    }
}

/// Run the mesh client forever on the calling thread. Owns the display
/// stack (surface + retained scene). The bare-metal port spawned this as
/// an embassy task; the IDF port runs it on a FreeRTOS-backed std
/// thread the caller dedicates.
pub fn run(
    mut surface: DpiDisplay,
    mut compositor: LeafScene,
    nvs: EspDefaultNvs,
    touch_rx: Receiver<InputEvent>,
    ptt_rx: Receiver<bool>,
) -> ! {
    let mut leaf = match LeafLink::load(nvs) {
        Ok(leaf) => leaf,
        Err(e) => {
            warn!("[mesh] certified identity/journal unavailable: {e}; mesh disabled");
            // Keep draining the touch channel: it is a 64-slot bounded
            // channel and the GT911 thread blocks on `send` once it
            // fills, so an unprovisioned image used to wedge touch
            // after ~0.6 s of dragging. Drive the bench touch-target
            // screen from the events and log each one, so touch can be
            // verified end to end without a parent.
            let mut targets = crate::selftest::TouchTargets::new();
            // FONT_10X20 from x=120 leaves room for 68 characters.
            let header = "clawft-edge-pad-idf :: unprovisioned leaf (no identity)";
            if let Err(e) = targets.draw(&mut surface, header) { warn!("[mesh] touch screen draw failed: {e:?}"); }
            loop {
                let mut dirty = false;
                while ptt_rx.try_recv().is_ok() {}
                while let Ok(event) = touch_rx.try_recv() {
                    info!("[mesh] touch (offline): {event:?}");
                    dirty |= targets.feed(event);
                }
                if dirty {
                    if let Err(e) = targets.draw(&mut surface, header) { warn!("[mesh] touch screen draw failed: {e:?}"); }
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    };
    let id = leaf.cert.leaf_id();
    let topic = push_topic(&id);
    info!("[mesh] certified leaf id '{}', push topic '{}'", id, topic);

    boot_screen(&mut compositor, &mut surface, "connecting to mesh...");
    let mut shown_waiting = false;
    let mut held = Held::default();
    let mut session = PadSession::new();

    loop {
        if let Err(e) = leaf.stage_all(&touch_rx, &ptt_rx, &mut held, &compositor, &mut session) {
            warn!("[mesh] cannot persist touch observation: {e}; mesh disabled");
            boot_screen(&mut compositor, &mut surface, "leaf journal unavailable");
            loop { std::thread::sleep(Duration::from_secs(30)); }
        }
        info!("[mesh] starting parent discovery (hint {})", if PARENT_HINT.trim().is_empty() { "none" } else { PARENT_HINT });
        let addr = match leaf.discover() {
            Ok(addr) => addr,
            Err(e) => {
                warn!("[mesh] certified parent discovery failed: {e}; retry 3s");
                std::thread::sleep(Duration::from_secs(3));
                continue;
            }
        };
        info!("[mesh] connecting to {addr}");
        let mut sock = match TcpStream::connect_timeout(&addr, Duration::from_secs(5)) {
            Ok(s) => s,
            Err(e) => {
                warn!("[mesh] connect failed: {e:?} — retry 3s");
                std::thread::sleep(Duration::from_secs(3));
                continue;
            }
        };
        // Give signed ACKs time for parent fsync. The live display loop
        // switches to a short poll after backlog and subscribe replay.
        sock.set_read_timeout(Some(Duration::from_secs(10))).ok();
        sock.set_write_timeout(Some(Duration::from_secs(10))).ok();
        sock.set_nodelay(true).ok();

        info!("[mesh] connected — replaying durable backlog before new subscribe to '{topic}'");
        if let Err(e) = leaf.replay(&mut sock, |bytes| apply_push(bytes, &mut compositor, &mut surface, &mut session)) {
            warn!("[mesh] signed replay failed: {e}; retry 3s");
            std::thread::sleep(Duration::from_secs(3));
            continue;
        }
        if let Err(e) = leaf.subscribe().and_then(|_| leaf.replay(&mut sock, |bytes| apply_push(bytes, &mut compositor, &mut surface, &mut session))) {
            warn!("[mesh] signed subscribe failed: {e}; retry 3s");
            std::thread::sleep(Duration::from_secs(3));
            continue;
        }
        sock.set_read_timeout(Some(Duration::from_millis(500))).ok();
        if !shown_waiting {
            boot_screen(
                &mut compositor,
                &mut surface,
                "subscribed -- waiting for pushes",
            );
            shown_waiting = true;
        }

        let mut frame = [0u8; RX_BUF];
        loop {
            session.tick(&mut compositor, &mut surface);
            if let Err(e) = leaf.stage_all(&touch_rx, &ptt_rx, &mut held, &compositor, &mut session) {
                warn!("[mesh] cannot persist touch observation: {e}; mesh disabled");
                boot_screen(&mut compositor, &mut surface, "leaf journal unavailable");
                loop { std::thread::sleep(Duration::from_secs(30)); }
            }
            if !leaf.queue.pending.is_empty() { sock.set_read_timeout(Some(Duration::from_secs(10))).ok(); }
            if let Err(e) = leaf.replay(&mut sock, |bytes| apply_push(bytes, &mut compositor, &mut surface, &mut session)) {
                warn!("[mesh] signed touch replay failed: {e}; reconnecting");
                break;
            }
            // A short poll keeps touch and button edges under ~50 ms from
            // the journal to the wire (they leave at the top of the loop).
            sock.set_read_timeout(Some(Duration::from_millis(40))).ok();
            let mut len_buf = [0u8; 4];
            match sock.peek(&mut len_buf) {
                Ok(0) => break,
                Ok(n) if n < len_buf.len() => continue,
                Ok(_) => {},
                Err(e) if matches!(e.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock) => continue,
                Err(e) => { warn!("[mesh] peek failed: {e:?} — reconnecting"); break; }
            }
            if let Err(e) = read_exact(&mut sock, &mut len_buf) {
                warn!("[mesh] read len failed: {e:?} — reconnecting");
                break;
            }
            let len = u32::from_be_bytes(len_buf) as usize;
            if len == 0 || len > frame.len() {
                warn!("[mesh] frame len {len} out of range — reconnecting");
                break;
            }
            if let Err(e) = read_exact(&mut sock, &mut frame[..len]) {
                warn!("[mesh] read body failed: {e:?} — reconnecting");
                break;
            }
            apply_push(&frame[..len], &mut compositor, &mut surface, &mut session);
        }

        // Connection dropped — back off and reconnect.
        drop(sock);
        std::thread::sleep(Duration::from_secs(2));
    }
}

fn apply_push(bytes: &[u8], scene: &mut LeafScene, surface: &mut DpiDisplay, session: &mut PadSession) {
    // Display-session frames first (`session.rs`).
    match SessionFrame::from_carrier(bytes) {
        Some(Ok(frame)) => return session.on_frame(frame, scene, surface),
        Some(Err(e)) => return warn!("[mesh] undecodable display-session frame: {e}"),
        None => {}
    }
    let Some(cbor) = b64::extract_member(bytes, b"cbor_b64") else { return };
    // Vector scene envelopes (`weaver leaf scene …`); the older raster
    // `LeafPush` (`weaver leaf push …`) goes through the shim so
    // text/clear/image pushes keep rendering as they did.
    match weftos_leaf_scene::codec::decode_scene_envelope(&cbor) {
        Ok(env) => scene.apply_envelope(surface, &env),
        Err(weftos_leaf_scene::codec::CodecError::VersionMismatch { found, expected }) => {
            warn!("[mesh] scene wire version mismatch (found {found}, expected {expected}) — dropped");
        }
        Err(_) => match weftos_leaf_types::decode::<LeafPush>(&cbor) {
            Ok(push) => scene.apply_raster(surface, push),
            Err(e) => warn!("[mesh] CBOR is neither SceneEnvelope nor LeafPush: {e}"),
        },
    }
}

/// Two-line status screen — used for the boot screen and connection-state
/// messages. `main` also calls it before WiFi so the panel proves itself
/// offline.
pub(crate) fn boot_screen(scene: &mut LeafScene, surface: &mut DpiDisplay, msg: &str) {
    scene.boot_screen(surface, msg);
}
