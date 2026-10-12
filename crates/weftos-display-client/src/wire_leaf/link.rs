//! The emulated leaf's link thread: the same loop the pad firmware runs in
//! `clawft-edge-pad-idf/src/mesh.rs`, over `std::net`.
//!
//! discover -> connect -> replay the journal -> sign and send the subscribe ->
//! replay -> read pushes (500 ms poll) -> on any I/O error reconnect.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use weftos_leaf_scene::codec;
use weftos_leaf_scene::session::envelope::subscribe_envelope;
use weftos_leaf_scene::session::{b64, SessionFrame};
use weftos_leaf_types::link::{ParentAdvertisement, PublishAck, ACK_MAGIC, FRAME_MAGIC};
use weftos_leaf_types::push_topic;

use super::{Parent, Shared};

const RX_BUF: usize = 8192;

pub(super) fn run(shared: Arc<Mutex<Shared>>, stop: Arc<AtomicBool>, parent: Parent) {
    while !stop.load(Ordering::Relaxed) {
        if let Err(e) = session(&shared, &stop, &parent) {
            lock(&shared).note(format!("link down: {e}; retry in 1s"));
        }
        for _ in 0..10 {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

fn lock(shared: &Arc<Mutex<Shared>>) -> std::sync::MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

fn session(shared: &Arc<Mutex<Shared>>, stop: &AtomicBool, parent: &Parent) -> Result<(), String> {
    let addr = match parent {
        Parent::Direct(a) => *a,
        Parent::Discover(a) => discover(shared, *a)?,
    };
    let mut sock = TcpStream::connect_timeout(&addr, Duration::from_secs(5)).map_err(|e| format!("connect {addr}: {e}"))?;
    sock.set_nodelay(true).ok();
    sock.set_write_timeout(Some(Duration::from_secs(10))).ok();
    sock.set_read_timeout(Some(Duration::from_secs(10))).ok();
    {
        let mut g = lock(shared);
        g.connections += 1;
        g.subscribed = false;
        g.note(format!("connected to {addr}"));
    }
    replay(shared, &mut sock)?;
    {
        let mut g = lock(shared);
        let id = g.cert.leaf_id();
        let envelope = subscribe_envelope(&id, &push_topic(&id));
        let (cert, key) = (g.cert.clone(), g.key.clone());
        g.journal
            .enqueue(&cert, &key, "mesh.subscribe".into(), envelope.into_bytes())
            .map_err(|e| format!("subscribe: {e:?}"))?;
        g.persist();
    }
    replay(shared, &mut sock)?;
    lock(shared).subscribed = true;
    lock(shared).note("subscribed");
    sock.set_read_timeout(Some(Duration::from_millis(200))).ok();
    while !stop.load(Ordering::Relaxed) {
        {
            let mut g = lock(shared);
            let now = g.now_ms();
            let actions = g.service.tick(now);
            g.run(actions);
        }
        if !lock(shared).journal.pending.is_empty() {
            sock.set_read_timeout(Some(Duration::from_secs(10))).ok();
            replay(shared, &mut sock)?;
            sock.set_read_timeout(Some(Duration::from_millis(200))).ok();
        }
        let mut len = [0u8; 4];
        match sock.peek(&mut len) {
            Ok(0) => return Err("parent closed the connection".into()),
            Ok(n) if n < 4 => continue,
            Ok(_) => {}
            Err(e) if matches!(e.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock) => continue,
            Err(e) => return Err(format!("peek: {e}")),
        }
        sock.set_read_timeout(Some(Duration::from_secs(10))).ok();
        let frame = read_frame(&mut sock)?;
        sock.set_read_timeout(Some(Duration::from_millis(200))).ok();
        on_push(shared, &frame);
    }
    Ok(())
}

fn discover(shared: &Arc<Mutex<Shared>>, to: SocketAddr) -> Result<SocketAddr, String> {
    let (id, mesh_pubkey, scope) = {
        let g = lock(shared);
        (g.cert.leaf_id(), g.cert.mesh_pubkey, g.cert.parent_scope.clone())
    };
    let sock = UdpSocket::bind("0.0.0.0:0").map_err(|e| e.to_string())?;
    sock.set_broadcast(true).ok();
    sock.set_read_timeout(Some(Duration::from_secs(3))).ok();
    let nonce = random_nonce();
    let mut request = Vec::with_capacity(52);
    request.extend_from_slice(b"WLD1");
    request.extend_from_slice(id.as_bytes());
    request.extend_from_slice(&nonce);
    sock.send_to(&request, to).map_err(|e| format!("discovery send: {e}"))?;
    let mut buf = [0u8; 1024];
    loop {
        let (n, _) = sock.recv_from(&mut buf).map_err(|e| format!("no parent answered discovery at {to}: {e}"))?;
        let Ok(ad) = weftos_leaf_types::decode::<ParentAdvertisement>(&buf[..n]) else { continue };
        let wall = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        if ad.verify_for(&mesh_pubkey, &scope, wall, &nonce).is_ok() {
            return ad.endpoint.parse().map_err(|e| format!("advertised endpoint: {e}"));
        }
    }
}

/// 16 bytes from the OS-seeded hasher: unpredictable enough for a
/// discovery nonce, and no extra dependency.
fn random_nonce() -> [u8; 16] {
    use std::hash::{BuildHasher, Hasher};
    let state = std::collections::hash_map::RandomState::new();
    let mut out = [0u8; 16];
    for (i, chunk) in out.chunks_mut(8).enumerate() {
        let mut h = state.build_hasher();
        h.write_u8(i as u8);
        chunk.copy_from_slice(&h.finish().to_le_bytes());
    }
    out
}

fn write_frame(sock: &mut TcpStream, payload: &[u8]) -> Result<(), String> {
    sock.write_all(&(payload.len() as u32).to_be_bytes()).and_then(|_| sock.write_all(payload)).map_err(|e| format!("write: {e}"))
}

fn read_frame(sock: &mut TcpStream) -> Result<Vec<u8>, String> {
    let mut len = [0u8; 4];
    sock.read_exact(&mut len).map_err(|e| format!("read length: {e}"))?;
    let len = u32::from_be_bytes(len) as usize;
    if !(1..=RX_BUF).contains(&len) {
        return Err(format!("frame length {len} out of range (the pad reads at most {RX_BUF})"));
    }
    let mut body = vec![0u8; len];
    sock.read_exact(&mut body).map_err(|e| format!("read body: {e}"))?;
    Ok(body)
}

/// Send the oldest journalled frame, read until its signed ACK (handing
/// any push that arrives meanwhile to the session logic), repeat.
fn replay(shared: &Arc<Mutex<Shared>>, sock: &mut TcpStream) -> Result<(), String> {
    loop {
        let Some(frame) = lock(shared).journal.oldest().cloned() else { return Ok(()) };
        let mut wire = FRAME_MAGIC.to_vec();
        wire.extend_from_slice(&weftos_leaf_types::encode(&frame).map_err(|e| format!("encode frame: {e:?}"))?);
        if wire.len() > 32 * 1024 {
            return Err("signed frame exceeds the mesh bound".into());
        }
        write_frame(sock, &wire)?;
        let ack = loop {
            let reply = read_frame(sock)?;
            if reply.starts_with(ACK_MAGIC) {
                break weftos_leaf_types::decode::<PublishAck>(&reply[ACK_MAGIC.len()..]).map_err(|e| format!("decode ACK: {e:?}"))?;
            }
            on_push(shared, &reply);
        };
        let mut g = lock(shared);
        g.journal.acknowledge(&ack).map_err(|e| format!("ACK rejected: {e:?}"))?;
        g.persist();
    }
}

/// One frame pushed by the parent: a session frame, or a bare scene
/// envelope from `weaver leaf scene`.
fn on_push(shared: &Arc<Mutex<Shared>>, bytes: &[u8]) {
    let mut g = lock(shared);
    match SessionFrame::from_carrier(bytes) {
        Some(Ok(frame)) => {
            let now = g.now_ms();
            let actions = g.service.handle(frame, now);
            g.run(actions);
        }
        Some(Err(e)) => g.note(format!("undecodable session frame: {e}")),
        None => {
            let Some(cbor) = b64::extract_member(bytes, b"cbor_b64") else { return };
            if let Ok(env) = codec::decode_scene_envelope(&cbor) {
                let _ = g.sim.apply_wire(&env, true);
            }
        }
    }
}
