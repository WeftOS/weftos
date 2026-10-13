//! FU mesh-kernel: O(1) route-loss detection, bidirectional/reconnecting
//! seed dials, Noise listener resilience, and forward-compatible envelopes.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;

use super::*;
use crate::error::KernelResult;
use crate::ipc::{KernelMessage, MessageTarget};
use crate::mesh::MeshTransport;
use crate::mesh_delivery::{LocalDelivery, PeerCtx};
use crate::mesh_ipc::{MeshIpcEnvelope, Scope};
use crate::mesh_noise::{NoiseChannel, NoiseConfig, NoisePattern};
use crate::mesh_noise::EncryptedChannel;
use ed25519_dalek::SigningKey;
use weftos_leaf_types::link::{LeafCertificate, PublishAck, SignedPublish, FRAME_MAGIC, ACK_MAGIC};

struct MemoryChannel {
    inbound: tokio::sync::Mutex<tokio::sync::mpsc::Receiver<Vec<u8>>>,
    outbound: tokio::sync::mpsc::Sender<Vec<u8>>,
}

#[async_trait]
impl EncryptedChannel for MemoryChannel {
    async fn send_encrypted(&mut self, bytes: &[u8]) -> Result<(), crate::mesh::MeshError> {
        self.outbound.send(bytes.to_vec()).await.map_err(|e| crate::mesh::MeshError::Transport(e.to_string()))
    }
    async fn recv_encrypted(&mut self) -> Result<Vec<u8>, crate::mesh::MeshError> {
        self.inbound.lock().await.recv().await.ok_or_else(|| crate::mesh::MeshError::Transport("closed".into()))
    }
    fn remote_static_key(&self) -> Option<&[u8]> { None }
    async fn close(&mut self) -> Result<(), crate::mesh::MeshError> { Ok(()) }
}

fn memory_leaf_connection(rt: Arc<MeshRuntime>) -> (tokio::sync::mpsc::Sender<Vec<u8>>, tokio::sync::mpsc::Receiver<Vec<u8>>, tokio::task::JoinHandle<()>) {
    let (client, inbound) = tokio::sync::mpsc::channel(8);
    let (outbound, reply) = tokio::sync::mpsc::channel(8);
    let channel = Box::new(MemoryChannel { inbound: tokio::sync::Mutex::new(inbound), outbound });
    let (route_tx, route_rx) = tokio::sync::mpsc::channel(8);
    let task = tokio::spawn(async move {
        pump(&rt, channel, ChannelKind::Passthrough, "memory-leaf", &crate::mesh_admit::AllowAll,
            Limits::default(), route_tx, route_rx, None, RouteTally::default(), false, true).await;
    });
    (client, reply, task)
}

#[tokio::test]
async fn certified_leaf_listener_rejects_unsigned_first_frame() {
    let (rt, rec) = runtime("machine");
    let (sender, mut reply, task) = memory_leaf_connection(rt);
    sender.send(frame("legacy", "mesh.leaf.legacy.input")).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), task).await.unwrap().unwrap();
    assert!(reply.try_recv().is_err());
    assert!(rec.0.lock().unwrap().is_empty());
}

#[derive(Default)]
struct Rec(Mutex<Vec<String>>, Mutex<Vec<(Option<Scope>, Option<Scope>)>>);

#[async_trait]
impl LocalDelivery for Rec {
    async fn deliver(&self, ctx: &PeerCtx, dest: Option<&Scope>, m: KernelMessage) -> KernelResult<()> {
        if let MessageTarget::Topic(t) = m.target {
            self.0.lock().unwrap().push(t);
            self.1.lock().unwrap().push((ctx.src_scope.clone(), dest.cloned()));
        }
        Ok(())
    }
}

/// Poll `f` until it holds (up to 10 s); no fixed sleeps in the tests.
async fn until(mut f: impl FnMut() -> bool) -> bool {
    for _ in 0..2000 {
        if f() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    false
}

async fn seen(rec: &Rec, topic: &str) -> bool {
    until(|| rec.0.lock().unwrap().iter().any(|t| t == topic)).await
}

/// A listener that counts accepted connections and keeps them open and
/// silent (`hold`) or closes them at once.
async fn counting_listener(hold: bool) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
    let mut l = crate::mesh_tcp::TcpTransport.listen("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    let n = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let n2 = Arc::clone(&n);
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = l.accept().await {
            n2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if hold {
                held.push(stream);
            }
        }
    });
    (addr, n)
}

fn count(n: &std::sync::atomic::AtomicUsize) -> usize {
    n.load(std::sync::atomic::Ordering::SeqCst)
}

fn runtime(id: &str) -> (Arc<MeshRuntime>, Arc<Rec>) {
    let rec = Arc::new(Rec::default());
    let mut rt = MeshRuntime::new(id.into());
    rt.set_local_delivery(rec.clone());
    (Arc::new(rt), rec)
}

fn frame(src: &str, topic: &str) -> Vec<u8> {
    let msg = KernelMessage::text(0, MessageTarget::Topic(topic.into()), "x");
    MeshIpcEnvelope::new(src.into(), "dst".into(), msg).to_bytes().unwrap()
}

async fn listen(
    rt: &Arc<MeshRuntime>,
    noise: Option<Arc<NoiseConfig>>,
) -> (String, tokio::task::JoinHandle<()>) {
    let l = crate::mesh_tcp::TcpTransport.listen("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    let task = tokio::spawn(serve_listener(
        Arc::clone(rt), l, noise, "tcp", "x", Arc::new(crate::mesh_admit::AllowAll),
    ));
    (addr, task)
}

#[tokio::test]
async fn certified_leaf_publish_replay_is_suppressed_after_committed_floor() {
    let dir = tempfile::tempdir().unwrap();
    #[cfg(unix)] { use std::os::unix::fs::PermissionsExt; std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap(); }
    let parent = SigningKey::from_bytes(&[31; 32]);
    let user_id = clawft_types::project::cert::key_id(&parent.verifying_key().to_bytes());
    let machine = SigningKey::from_bytes(&[32; 32]);
    let leaf = SigningKey::from_bytes(&[33; 32]);
    let cert = LeafCertificate::issue(&parent, format!("user:{user_id}"), machine.verifying_key().to_bytes(), leaf.verifying_key().to_bytes(), 1, 1, u64::MAX, vec!["input.publish".into()]).unwrap();
    let id = cert.leaf_id();
    let topic = format!("mesh.leaf.{id}.input");
    let msg = KernelMessage::new(0, MessageTarget::Topic("ipc.publish".into()),
        crate::ipc::MessagePayload::Json(serde_json::json!({"topic": topic, "message": "touch"})));
    let mut inner = MeshIpcEnvelope::new(id.clone(), "srv".into(), msg);
    inner.src_scope = Some(Scope { user_id: "a".repeat(32), project_id: None });
    inner.dest_scope = Some(Scope { user_id: "b".repeat(32), project_id: None });
    let envelope = inner.to_bytes().unwrap();
    let publish = SignedPublish::sign(cert.clone(), &leaf, 1, topic.clone(), envelope).unwrap();
    let public = serde_json::json!({"node_id":id,"cert":cert});
    let (rt, rec) = runtime("srv");
    let ingress = crate::mesh_leaf::LeafIngress::open(dir.path(), machine.clone()).unwrap();
    std::fs::write(dir.path().join("registry").join(format!("{id}.json")), public.to_string()).unwrap();
    rt.set_leaf_ingress(Arc::new(ingress)).ok().unwrap();
    let (client, mut replies, task) = memory_leaf_connection(Arc::clone(&rt));
    let mut wire = FRAME_MAGIC.to_vec();
    wire.extend_from_slice(&weftos_leaf_types::encode(&publish).unwrap());
    client.send(wire.clone()).await.unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(5), replies.recv()).await.unwrap().unwrap();
    assert!(reply.starts_with(ACK_MAGIC));
    let ack: PublishAck = weftos_leaf_types::decode(&reply[ACK_MAGIC.len()..]).unwrap();
    ack.verify(&machine.verifying_key().to_bytes(), &publish).unwrap();
    assert!(seen(&rec, &topic).await);
    assert_eq!(rec.0.lock().unwrap().len(), 1);
    assert_eq!(rec.1.lock().unwrap().as_slice(), &[(None, Some(Scope { user_id: user_id.clone(), project_id: None }))]);
    task.abort();
    drop(client);

    // New listener and gate process state: the exact replay gets its ACK,
    // but the local router never receives a second effect.
    let (rt2, rec2) = runtime("srv");
    rt2.set_leaf_ingress(Arc::new(crate::mesh_leaf::LeafIngress::open(dir.path(), machine).unwrap())).ok().unwrap();
    let (retry, mut replies, task) = memory_leaf_connection(Arc::clone(&rt2));
    retry.send(wire).await.unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(5), replies.recv()).await.unwrap().unwrap();
    assert!(reply.starts_with(ACK_MAGIC));
    assert!(rec2.0.lock().unwrap().is_empty());
    task.abort();
}

#[tokio::test]
async fn certified_leaf_duplicate_subscribe_restores_route_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    #[cfg(unix)] { use std::os::unix::fs::PermissionsExt; std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap(); }
    let parent = SigningKey::from_bytes(&[41; 32]);
    let user_id = clawft_types::project::cert::key_id(&parent.verifying_key().to_bytes());
    let machine = SigningKey::from_bytes(&[42; 32]);
    let leaf = SigningKey::from_bytes(&[43; 32]);
    let cert = LeafCertificate::issue(&parent, format!("user:{user_id}"), machine.verifying_key().to_bytes(), leaf.verifying_key().to_bytes(), 1, 1, u64::MAX, vec!["push.subscribe".into()]).unwrap();
    let id = cert.leaf_id();
    let topic = weftos_leaf_types::push_topic(&id);
    let msg = KernelMessage::new(0, MessageTarget::Topic("mesh.subscribe".into()),
        crate::ipc::MessagePayload::Json(serde_json::json!({"topic": topic})));
    let envelope = MeshIpcEnvelope::new(id.clone(), "srv".into(), msg).to_bytes().unwrap();
    let frame = SignedPublish::sign(cert.clone(), &leaf, 1, "mesh.subscribe".into(), envelope).unwrap();
    let mut wire = FRAME_MAGIC.to_vec();
    wire.extend_from_slice(&weftos_leaf_types::encode(&frame).unwrap());
    let (rt, _) = runtime("srv");
    let ingress = crate::mesh_leaf::LeafIngress::open(dir.path(), machine.clone()).unwrap();
    std::fs::write(dir.path().join("registry").join(format!("{id}.json")), serde_json::json!({"node_id":id,"cert":cert}).to_string()).unwrap();
    rt.set_leaf_ingress(Arc::new(ingress)).ok().unwrap();
    let (client, mut replies, task) = memory_leaf_connection(Arc::clone(&rt));
    client.send(wire.clone()).await.unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(5), replies.recv()).await.unwrap().unwrap();
    assert!(reply.starts_with(ACK_MAGIC));
    assert!(until(|| rt.peers_for_topic(&topic).contains(&id)).await);
    task.abort();
    drop(client);

    let (rt2, _) = runtime("srv");
    rt2.set_leaf_ingress(Arc::new(crate::mesh_leaf::LeafIngress::open(dir.path(), machine).unwrap())).ok().unwrap();
    let (retry, mut replies, task) = memory_leaf_connection(Arc::clone(&rt2));
    retry.send(wire).await.unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(5), replies.recv()).await.unwrap().unwrap();
    assert!(reply.starts_with(ACK_MAGIC));
    assert!(until(|| rt2.peers_for_topic(&topic).contains(&id)).await);
    // An idle subscribed leaf must lose its push route without sending
    // another frame after enrollment is removed.
    std::fs::remove_file(dir.path().join("registry").join(format!("{id}.json"))).unwrap();
    let push = MeshIpcEnvelope::new("srv".into(), id.clone(),
        KernelMessage::text(0, MessageTarget::Topic(topic.clone()), "revoked push"));
    let _ = rt2.send_to_peer(&id, push).await;
    assert!(until(|| !rt2.peers_for_topic(&topic).contains(&id)).await);
    tokio::time::timeout(Duration::from_secs(2), task).await.unwrap().unwrap();
    assert!(replies.try_recv().is_err(), "revoked leaf received a pushed frame");
    drop(retry);
}

fn noise_cfg() -> Arc<NoiseConfig> {
    let kp = snow::Builder::new("Noise_XX_25519_ChaChaPoly_SHA256".parse().unwrap())
        .generate_keypair()
        .unwrap();
    Arc::new(NoiseConfig {
        pattern: NoisePattern::XX,
        local_private_key: kp.private.try_into().unwrap(),
        remote_static_key: None,
    })
}

// ── route tally ──────────────────────────────────────────────────

#[test]
fn tally_follows_registration_replacement_and_removal() {
    let rt = MeshRuntime::new("local".into());
    let t = RouteTally::default();
    let (a, _ra) = tokio::sync::mpsc::channel(4);
    let (b, _rb) = tokio::sync::mpsc::channel(4);
    rt.add_peer_tallied("n".into(), a.clone(), &t);
    assert_eq!(t.live(), 1);
    // Same channel again: no double count.
    rt.add_peer_tallied("n".into(), a.clone(), &t);
    assert_eq!(t.live(), 1);
    // Replaced by another connection: this connection's count drops.
    rt.add_peer("n".into(), b);
    assert_eq!(t.live(), 0);
    // Removal path.
    rt.add_peer_tallied("m".into(), a, &t);
    assert_eq!(t.live(), 1);
    rt.disconnect_peer("m");
    assert_eq!(t.live(), 0);
}

#[test]
fn tally_ignores_every_route_it_does_not_own() {
    // The per-connection check reads only its own counter, so unrelated
    // peers (added, replaced or removed) never change it.
    let rt = MeshRuntime::new("local".into());
    let t = RouteTally::default();
    let mut keep = Vec::new();
    for i in 0..5_000 {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        rt.add_peer(format!("p{i}"), tx);
        keep.push(rx);
    }
    assert_eq!(t.live(), 0);
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    rt.add_peer_tallied("mine".into(), tx, &t);
    assert_eq!(t.live(), 1);
    for i in 0..5_000 {
        rt.disconnect_peer(&format!("p{i}"));
    }
    assert_eq!(t.live(), 1);
}

#[tokio::test]
async fn connection_closes_when_its_route_is_revoked() {
    let (rt, _) = runtime("srv");
    let (addr, _task) = listen(&rt, None).await;
    let mut c = crate::mesh_tcp::TcpTransport.connect(&addr).await.unwrap();
    c.send(&frame("n1", "t.a")).await.unwrap();
    assert!(until(|| !rt.peer_ids().is_empty()).await);
    assert_eq!(rt.peer_ids(), vec!["n1".to_string()]);
    rt.disconnect_peer("n1");
    assert!(matches!(tokio::time::timeout(Duration::from_secs(2), c.recv()).await, Ok(Err(_))),
        "revoked connection must close within the route-check interval");
}

#[tokio::test]
async fn old_connection_closes_when_a_new_one_replaces_its_route() {
    let (rt, _) = runtime("srv");
    let (addr, _task) = listen(&rt, None).await;
    let mut old = crate::mesh_tcp::TcpTransport.connect(&addr).await.unwrap();
    old.send(&frame("n1", "t.a")).await.unwrap();
    assert!(until(|| !rt.peer_ids().is_empty()).await);
    let mut new = crate::mesh_tcp::TcpTransport.connect(&addr).await.unwrap();
    new.send(&frame("n1", "t.b")).await.unwrap();
    assert!(matches!(tokio::time::timeout(Duration::from_secs(2), old.recv()).await, Ok(Err(_))),
        "replaced connection must close");
    // The replacement stays up and is the live route.
    assert_eq!(rt.peer_ids(), vec!["n1".to_string()]);
    let msg = KernelMessage::text(0, MessageTarget::Topic("t.back".into()), "x");
    rt.send_to_peer("n1", MeshIpcEnvelope::new("srv".into(), "n1".into(), msg)).await.unwrap();
    assert!(matches!(tokio::time::timeout(Duration::from_secs(5), new.recv()).await, Ok(Ok(_))));
}

// ── Noise listener ───────────────────────────────────────────────

#[tokio::test]
async fn noise_listener_serves_and_survives_a_failed_handshake() {
    let (rt, rec) = runtime("srv");
    let (addr, task) = listen(&rt, Some(noise_cfg())).await;

    // Garbage instead of a Noise handshake, then hang up.
    let mut bad = crate::mesh_tcp::TcpTransport.connect(&addr).await.unwrap();
    let _ = bad.send(&[0xde, 0xad, 0xbe, 0xef, 1, 2, 3]).await;
    drop(bad);

    // The listener is still alive and a real initiator gets through.
    let stream = crate::mesh_tcp::TcpTransport.connect(&addr).await.unwrap();
    let mut ch = NoiseChannel::initiate(stream, &noise_cfg()).await.unwrap();
    ch.send_encrypted(&frame("n1", "t.noise")).await.unwrap();
    assert!(seen(&rec, "t.noise").await);
    assert!(!task.is_finished());
}

#[tokio::test]
async fn silent_noise_handshake_is_dropped_and_listener_lives() {
    let (rt, _) = runtime("srv");
    let (addr, task) = listen(&rt, Some(noise_cfg())).await;
    // Connect and say nothing; the cap on handshake time frees the slot
    // (HANDSHAKE_TIMEOUT is 10 s; assert only that nothing is wedged).
    let _idle = crate::mesh_tcp::TcpTransport.connect(&addr).await.unwrap();
    let stream = crate::mesh_tcp::TcpTransport.connect(&addr).await.unwrap();
    assert!(NoiseChannel::initiate(stream, &noise_cfg()).await.is_ok(),
        "one stalled handshake must not block others");
    assert!(!task.is_finished());
}

// ── seed dials ───────────────────────────────────────────────────

fn quick() -> SeedTiming {
    SeedTiming {
        base: Duration::from_millis(20),
        max: Duration::from_millis(100),
        stable: Duration::from_secs(60),
        idle: Duration::from_secs(60),
    }
}

async fn push(from: &Arc<MeshRuntime>, to_addr: &str, src: &str, topic: &str) -> bool {
    let msg = KernelMessage::text(0, MessageTarget::Topic(topic.into()), "x");
    let env = MeshIpcEnvelope::new(src.into(), "srv".into(), msg);
    from.send_to_peer(to_addr, env).await.is_ok()
}

#[tokio::test]
async fn seed_connection_reads_inbound_frames_from_the_seed() {
    let (srv, _) = runtime("srv");
    let (addr, _task) = listen(&srv, None).await;
    let (dialer, drec) = runtime("dialer");
    let handles = connect_seeds_with(&dialer, std::slice::from_ref(&addr), "tcp", None, None, quick());

    // Dialer introduces itself so the seed can route back.
    let mut sent = false;
    for _ in 0..2000 {
        if push(&dialer, &addr, "dialer", "t.up").await {
            sent = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(sent);
    assert!(until(|| srv.peer_ids().contains(&"dialer".to_string())).await);
    // Seed pushes to the dialer over the same connection.
    let msg = KernelMessage::text(0, MessageTarget::Topic("t.down".into()), "x");
    srv.send_to_peer("dialer", MeshIpcEnvelope::new("srv".into(), "dialer".into(), msg))
        .await
        .unwrap();
    assert!(seen(&drec, "t.down").await, "dialled connection must read the seed's frames");
    for h in handles {
        h.abort();
    }
}

#[tokio::test]
async fn seed_connection_redials_after_the_seed_drops_it() {
    let (srv, srec) = runtime("srv");
    let (addr, _task) = listen(&srv, None).await;
    let (dialer, _) = runtime("dialer");
    let handles = connect_seeds_with(&dialer, std::slice::from_ref(&addr), "tcp", None, None, quick());

    let mut sent = false;
    for _ in 0..2000 {
        if push(&dialer, &addr, "dialer", "t.one").await {
            sent = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(sent);
    assert!(seen(&srec, "t.one").await);
    // Seed cuts the connection (route check closes it).
    srv.disconnect_peer("dialer");
    // The dialer redials and traffic flows again.
    let mut n = 0;
    let mut ok = false;
    while n < 2000 && !ok {
        n += 1;
        if push(&dialer, &addr, "dialer", "t.two").await {
            ok = seen_for(&srec, "t.two").await;
        } else {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
    assert!(ok, "dialer never reconnected");
    for h in handles {
        h.abort();
    }
}

/// Bounded check that a send which went into a dying connection did not
/// arrive (the dialer resends on the next loop turn).
async fn seen_for(rec: &Rec, topic: &str) -> bool {
    for _ in 0..20 {
        if rec.0.lock().unwrap().iter().any(|t| t == topic) {
            return true;
        }
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    false
}

#[tokio::test]
async fn seed_dial_keeps_redialling_after_failures_until_aborted() {
    // Every connection is closed at once, so each dial "fails"; the loop
    // must come back repeatedly.
    let (addr, n) = counting_listener(false).await;
    let (dialer, _) = runtime("dialer");
    let handles = connect_seeds_with(&dialer, std::slice::from_ref(&addr), "tcp", None, None, quick());
    assert!(until(|| count(&n) >= 3).await, "dial loop gave up");
    for h in &handles {
        h.abort();
    }
}

#[tokio::test]
async fn silent_seed_is_dropped_and_redialled() {
    // The seed accepts and never speaks: a half-open connection.
    let (addr, n) = counting_listener(true).await;
    let (dialer, _) = runtime("dialer");
    let timing = SeedTiming { idle: Duration::from_millis(100), ..quick() };
    let handles = connect_seeds_with(&dialer, std::slice::from_ref(&addr), "tcp", None, None, timing);
    assert!(until(|| count(&n) >= 2).await, "silent seed was never redialled");
    for h in &handles {
        h.abort();
    }
}

#[tokio::test]
async fn duplicate_seed_addresses_are_dialled_once() {
    let (addr, n) = counting_listener(true).await;
    let (dialer, _) = runtime("dialer");
    let seeds = vec![addr.clone(), addr.clone(), addr];
    let handles = connect_seeds_with(&dialer, &seeds, "tcp", None, None, quick());
    assert_eq!(handles.len(), 1);
    assert!(until(|| count(&n) >= 1).await);
    handles[0].abort();
}

#[tokio::test]
async fn aborting_a_dial_removes_its_route_and_emits_left() {
    let (srv, _) = runtime("srv");
    let (addr, _task) = listen(&srv, None).await;
    let (dialer, _) = runtime("dialer");
    let mut events = dialer.subscribe_peer_events();
    let handles = connect_seeds_with(&dialer, std::slice::from_ref(&addr), "tcp", None, None, quick());
    assert!(until(|| dialer.peer_ids() == vec![addr.clone()]).await);
    for h in &handles {
        h.abort();
    }
    assert!(until(|| dialer.peer_ids().is_empty()).await, "dead route left after abort");
    let mut left = false;
    while let Ok(ev) = events.try_recv() {
        left |= matches!(ev, crate::mesh_discovery::MeshPeerEvent::Left { .. });
    }
    assert!(left, "no Left event after abort");
}

#[tokio::test]
async fn dialled_connection_binds_to_the_first_id_and_cannot_claim_a_routed_one() {
    use crate::mesh_noise::{EncryptedChannel, PassthroughChannel};
    let mut l = crate::mesh_tcp::TcpTransport.listen("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    let (dialer, drec) = runtime("dialer");
    // An id already routed through another connection.
    let (victim_tx, _victim_rx) = tokio::sync::mpsc::channel(4);
    dialer.add_peer("victim".into(), victim_tx.clone());
    let handles = connect_seeds_with(&dialer, std::slice::from_ref(&addr), "tcp", None, None, quick());
    let (stream, _) = l.accept().await.unwrap();
    let mut seed = PassthroughChannel::new(stream);
    for (src, topic) in [("victim", "t.1"), ("a", "t.2"), ("b", "t.3"), ("a", "t.4")] {
        seed.send_encrypted(&frame(src, topic)).await.unwrap();
    }
    assert!(seen(&drec, "t.4").await);
    let got = drec.0.lock().unwrap().clone();
    assert_eq!(got, vec!["t.2".to_string(), "t.4".to_string()], "binding must drop 'victim' and 'b'");
    // The victim's route is untouched.
    assert!(!dialer.route_is_foreign("victim", &victim_tx));
    for h in &handles {
        h.abort();
    }
}

fn assessment_frame(src: &str) -> Vec<u8> {
    use crate::mesh_assess::AssessmentEnvelope;
    use crate::assessment::mesh::AssessmentMessage;
    AssessmentEnvelope {
        source_node: src.into(),
        sequence: 1,
        message: AssessmentMessage::RequestReport { requesting_node: src.into() },
    }
    .to_frame()
    .unwrap()
    .encode()
    .unwrap()
}

#[test]
fn a_frame_is_attributed_to_the_id_it_names_whatever_its_kind() {
    assert_eq!(claimed_source(&frame("env-id", "t")).as_deref(), Some("env-id"));
    assert_eq!(claimed_source(&assessment_frame("assess-id")).as_deref(), Some("assess-id"));
    assert_eq!(claimed_source(b"\x00\x01 not a frame"), None);
    assert_eq!(claimed_source(b""), None);
}

#[tokio::test]
async fn a_dialled_connection_binds_on_an_assessment_frame_and_drops_unattributable_ones() {
    use crate::mesh_noise::{EncryptedChannel, PassthroughChannel};
    let mut l = crate::mesh_tcp::TcpTransport.listen("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    let (dialer, drec) = runtime("dialer");
    let handles = connect_seeds_with(&dialer, std::slice::from_ref(&addr), "tcp", None, None, quick());
    let (stream, _) = l.accept().await.unwrap();
    let mut seed = PassthroughChannel::new(stream);
    // Garbage first (names no id), then an assessment frame naming "a"
    // (binds), then an envelope for "b" (not the bound id), then one for "a".
    seed.send_encrypted(b"\x00\x01 not a frame").await.unwrap();
    seed.send_encrypted(&assessment_frame("a")).await.unwrap();
    seed.send_encrypted(&frame("b", "t.b")).await.unwrap();
    seed.send_encrypted(&frame("a", "t.a")).await.unwrap();
    assert!(seen(&drec, "t.a").await);
    assert_eq!(drec.0.lock().unwrap().clone(), vec!["t.a".to_string()], "'b' must not ride a connection bound to 'a'");
    for h in &handles {
        h.abort();
    }
}

#[test]
fn seed_entries_parse_with_an_optional_pinned_id() {
    assert_eq!(
        parse_seed("10.0.0.2:9489").unwrap(),
        SeedSpec { addr: "10.0.0.2:9489".into(), node_id: None }
    );
    assert_eq!(
        parse_seed(" quic://10.0.0.2:9489#n-abc123 ").unwrap(),
        SeedSpec { addr: "quic://10.0.0.2:9489".into(), node_id: Some("n-abc123".into()) }
    );
    for bad in ["", "#n-1", "host:1#", "host:1#has space", "host:1#a#b", "host :1", &format!("host:1#{}", "x".repeat(129))] {
        assert!(parse_seed(bad).is_err(), "{bad:?}");
    }
    // One entry per address: the first wins, a repeat with another pin is not honoured.
    let specs = seed_specs(&["a:1#n-1".into(), "b:2".into(), "a:1#n-2".into(), "a:1#n-1".into(), "bad#".into()]);
    assert_eq!(specs.len(), 2);
    assert_eq!(specs[0].node_id.as_deref(), Some("n-1"));
}

#[tokio::test]
async fn a_pinned_seed_may_only_speak_for_its_own_id() {
    use crate::mesh_noise::{EncryptedChannel, PassthroughChannel};
    let mut l = crate::mesh_tcp::TcpTransport.listen("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    let (dialer, drec) = runtime("dialer");
    let entry = format!("{addr}#seed-id");
    let handles = connect_seeds_with(&dialer, &[entry], "tcp", None, None, quick());
    let (stream, _) = l.accept().await.unwrap();
    let mut seed = PassthroughChannel::new(stream);
    // The very first frame is for an offline id the seed wants to impersonate.
    for (src, topic) in [("offline-victim", "t.1"), ("seed-id", "t.2"), ("other", "t.3"), ("seed-id", "t.4")] {
        seed.send_encrypted(&frame(src, topic)).await.unwrap();
    }
    assert!(seen(&drec, "t.4").await);
    assert_eq!(drec.0.lock().unwrap().clone(), vec!["t.2".to_string(), "t.4".to_string()]);
    assert!(!dialer.peer_ids().contains(&"offline-victim".to_string()));
    for h in &handles {
        h.abort();
    }
}

#[tokio::test]
async fn a_pinned_seed_that_keeps_speaking_for_other_ids_is_cut_off() {
    use crate::mesh_noise::{EncryptedChannel, PassthroughChannel};
    let mut l = crate::mesh_tcp::TcpTransport.listen("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    let (dialer, _drec) = runtime("dialer");
    let handles = connect_seeds_with(&dialer, &[format!("{addr}#seed-id")], "tcp", None, None, quick());
    let (stream, _) = l.accept().await.unwrap();
    let mut seed = PassthroughChannel::new(stream);
    for i in 0..MAX_ID_MISMATCHES {
        seed.send_encrypted(&frame("someone-else", &format!("t.{i}"))).await.unwrap();
    }
    // The dialler closes the connection (our side reads EOF or an error).
    let closed = tokio::time::timeout(Duration::from_secs(5), seed.recv_encrypted()).await;
    assert!(matches!(closed, Ok(Err(_))), "connection must be closed: {closed:?}");
    for h in &handles {
        h.abort();
    }
}

#[tokio::test]
async fn a_pinned_id_routed_through_another_connection_is_not_dialled_for() {
    let mut l = crate::mesh_tcp::TcpTransport.listen("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    let (dialer, _drec) = runtime("dialer");
    let (held_tx, _held_rx) = tokio::sync::mpsc::channel(4);
    dialer.add_peer("seed-id".into(), held_tx.clone());
    let handles = connect_seeds_with(&dialer, &[format!("{addr}#seed-id")], "tcp", None, None, quick());
    // The dial is made (the TCP connect happens first) but dropped before
    // any route is registered for it; the existing route is untouched.
    let _ = tokio::time::timeout(Duration::from_millis(500), l.accept()).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!dialer.route_is_foreign("seed-id", &held_tx));
    assert!(!dialer.peer_ids().iter().any(|p| p == &addr), "no route under the seed address");
    for h in &handles {
        h.abort();
    }
}

#[test]
fn backoff_grows_is_capped_and_jittered() {
    let (base, max) = (Duration::from_secs(1), Duration::from_secs(60));
    for f in 0..40u32 {
        let d = seed_backoff(f, base, max);
        let nominal = base.saturating_mul(1u32 << f.min(16)).min(max);
        assert!(d >= nominal / 2 && d <= nominal, "failure {f}: {d:?} vs {nominal:?}");
    }
    let samples: std::collections::HashSet<_> =
        (0..20).map(|_| seed_backoff(5, base, max).as_nanos()).collect();
    assert!(samples.len() > 1, "no jitter");
}

// ── forward compatibility ────────────────────────────────────────

#[tokio::test]
async fn envelope_from_a_newer_peer_with_an_unknown_field_is_delivered() {
    let (rt, rec) = runtime("srv");
    let (addr, _task) = listen(&rt, None).await;
    let mut v: serde_json::Value = serde_json::from_slice(&frame("n1", "t.future")).unwrap();
    v["from_the_future"] = serde_json::json!({"x": 1});
    let mut c = crate::mesh_tcp::TcpTransport.connect(&addr).await.unwrap();
    c.send(&serde_json::to_vec(&v).unwrap()).await.unwrap();
    assert!(seen(&rec, "t.future").await, "unknown fields must be ignored, not rejected");
}

#[test]
fn an_older_relay_reserialising_strips_scope_fields() {
    // Documents ADR-103 A10: a peer whose envelope type predates the scope
    // fields drops them when it deserialises and re-serialises.
    #[derive(serde::Serialize, serde::Deserialize)]
    struct OldEnvelope {
        source_node: String,
        dest_node: String,
        message: KernelMessage,
        hop_count: u8,
        envelope_id: String,
    }
    let msg = KernelMessage::text(0, MessageTarget::Topic("t".into()), "x");
    let mut env = MeshIpcEnvelope::new("a".into(), "b".into(), msg);
    env.dest_scope = Some(Scope { user_id: "u".repeat(32), project_id: None });
    let old: OldEnvelope = serde_json::from_slice(&env.to_bytes().unwrap()).unwrap();
    let relayed = serde_json::to_vec(&old).unwrap();
    let back = MeshIpcEnvelope::from_bytes(&relayed).unwrap();
    assert!(back.dest_scope.is_none());
}

#[tokio::test]
async fn leaf_listeners_bind_where_leaf_listen_addr_says_not_on_the_main_ip() {
    let dir = tempfile::tempdir().unwrap();
    #[cfg(unix)] { use std::os::unix::fs::PermissionsExt; std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap(); }
    let tcp_port = {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        probe.local_addr().unwrap().port()
    };
    let (rt, _) = runtime("srv");
    let machine = SigningKey::from_bytes(&[61; 32]);
    rt.set_leaf_ingress(Arc::new(crate::mesh_leaf::LeafIngress::open(dir.path(), machine).unwrap())).ok().unwrap();
    let spec = crate::mesh_leaf_bind::LeafBindSpec::parse(&format!("127.0.0.1:{tcp_port}"), false).unwrap();
    assert!(rt.set_leaf_bind_spec(spec));
    let (main, task) = listen(&rt, None).await;
    let bound = loop {
        if let Some(b) = rt.leaf_bound() { break b; }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(bound.tcp.to_string(), format!("127.0.0.1:{tcp_port}"));
    assert_eq!(bound.discovery.to_string(), format!("127.0.0.1:{}", tcp_port - 1));
    let main_port: u16 = main.rsplit(':').next().unwrap().parse().unwrap();
    assert_ne!(bound.tcp.port(), main_port + 2, "explicit port replaces the derived one");
    assert!(tokio::net::TcpStream::connect(bound.tcp).await.is_ok());
    // The discovery socket is held by the listener, so a UDP rebind fails.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(std::net::UdpSocket::bind(bound.discovery).is_err());
    task.abort();
}

#[tokio::test]
async fn leaf_listeners_without_a_spec_follow_the_main_listener() {
    let dir = tempfile::tempdir().unwrap();
    #[cfg(unix)] { use std::os::unix::fs::PermissionsExt; std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap(); }
    let (rt, _) = runtime("srv");
    let machine = SigningKey::from_bytes(&[62; 32]);
    rt.set_leaf_ingress(Arc::new(crate::mesh_leaf::LeafIngress::open(dir.path(), machine).unwrap())).ok().unwrap();
    let (main, task) = listen(&rt, None).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let main_port: u16 = main.rsplit(':').next().unwrap().parse().unwrap();
    // main+2 can be taken on a busy machine; when it bound, it is the old derivation exactly.
    if let Some(b) = rt.leaf_bound() {
        assert_eq!(b.tcp.to_string(), format!("127.0.0.1:{}", main_port + 2));
        assert_eq!(b.discovery.to_string(), format!("127.0.0.1:{}", main_port + 1));
    }
    task.abort();
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn leaf_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    #[cfg(unix)] { use std::os::unix::fs::PermissionsExt; std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap(); }
    dir
}

async fn wait_bound(rt: &Arc<MeshRuntime>, n: usize) -> Vec<crate::mesh_leaf_bind::LeafBind> {
    assert!(until(|| rt.leaf_bounds().len() >= n).await, "expected {n} leaf listeners, got {:?}", rt.leaf_bounds());
    rt.leaf_bounds()
}

#[tokio::test]
async fn a_list_binds_one_leaf_listener_per_address() {
    let dir = leaf_dir();
    let (p1, p2) = (free_port(), free_port());
    let (rt, _) = runtime("srv");
    rt.set_leaf_ingress(Arc::new(crate::mesh_leaf::LeafIngress::open(dir.path(), SigningKey::from_bytes(&[71; 32])).unwrap())).ok().unwrap();
    let specs = crate::mesh_leaf_bind::LeafBindSpec::parse_all(
        &[&format!("127.0.0.1:{p1}"), &format!("127.0.0.1:{p2}")], false).unwrap();
    assert!(rt.set_leaf_bind_specs(specs));
    let (_main, task) = listen(&rt, None).await;
    let bound = wait_bound(&rt, 2).await;
    let mut tcps: Vec<_> = bound.iter().map(|b| b.tcp.port()).collect();
    tcps.sort();
    let mut want = vec![p1, p2];
    want.sort();
    assert_eq!(tcps, want);
    for b in &bound {
        assert!(tokio::net::TcpStream::connect(b.tcp).await.is_ok());
    }
    task.abort();
}

#[tokio::test]
async fn one_bind_failing_keeps_the_others_and_is_retried() {
    let dir = leaf_dir();
    let (p_ok, p_busy) = (free_port(), free_port());
    let hold = std::net::TcpListener::bind(("127.0.0.1", p_busy)).unwrap();
    let (rt, _) = runtime("srv");
    rt.set_leaf_bind_retry(Duration::from_millis(50));
    rt.set_leaf_ingress(Arc::new(crate::mesh_leaf::LeafIngress::open(dir.path(), SigningKey::from_bytes(&[72; 32])).unwrap())).ok().unwrap();
    let specs = crate::mesh_leaf_bind::LeafBindSpec::parse_all(
        &[&format!("127.0.0.1:{p_busy}"), &format!("127.0.0.1:{p_ok}")], false).unwrap();
    rt.set_leaf_bind_specs(specs);
    let (_main, task) = listen(&rt, None).await;
    let bound = wait_bound(&rt, 1).await;
    assert_eq!(bound.len(), 1);
    assert_eq!(bound[0].tcp.port(), p_ok, "the free address bound despite the busy one");
    // The interface comes up later: the missing address binds without a restart.
    drop(hold);
    let bound = wait_bound(&rt, 2).await;
    assert!(bound.iter().any(|b| b.tcp.port() == p_busy));
    task.abort();
}

#[tokio::test]
async fn a_leaf_admitted_over_the_second_address_is_the_same_identity() {
    let dir = leaf_dir();
    let (p1, p2) = (free_port(), free_port());
    let parent = SigningKey::from_bytes(&[73; 32]);
    let user_id = clawft_types::project::cert::key_id(&parent.verifying_key().to_bytes());
    let machine = SigningKey::from_bytes(&[74; 32]);
    let leaf = SigningKey::from_bytes(&[75; 32]);
    let cert = LeafCertificate::issue(&parent, format!("user:{user_id}"), machine.verifying_key().to_bytes(), leaf.verifying_key().to_bytes(), 1, 1, u64::MAX, vec!["input.publish".into()]).unwrap();
    let id = cert.leaf_id();
    let topic = format!("mesh.leaf.{id}.input");
    let msg = KernelMessage::new(0, MessageTarget::Topic("ipc.publish".into()),
        crate::ipc::MessagePayload::Json(serde_json::json!({"topic": topic, "message": "touch"})));
    let mut inner = MeshIpcEnvelope::new(id.clone(), "srv".into(), msg);
    inner.src_scope = Some(Scope { user_id: "a".repeat(32), project_id: None });
    inner.dest_scope = Some(Scope { user_id: "b".repeat(32), project_id: None });
    let publish = SignedPublish::sign(cert.clone(), &leaf, 1, topic.clone(), inner.to_bytes().unwrap()).unwrap();
    let ingress = crate::mesh_leaf::LeafIngress::open(dir.path(), machine.clone()).unwrap();
    std::fs::write(dir.path().join("registry").join(format!("{id}.json")), serde_json::json!({"node_id":id,"cert":cert}).to_string()).unwrap();
    let (rt, rec) = runtime("srv");
    rt.set_leaf_ingress(Arc::new(ingress)).ok().unwrap();
    rt.set_leaf_bind_specs(crate::mesh_leaf_bind::LeafBindSpec::parse_all(
        &[&format!("127.0.0.1:{p1}"), &format!("127.0.0.1:{p2}")], false).unwrap());
    let (_main, task) = listen(&rt, None).await;
    wait_bound(&rt, 2).await;

    let mut wire = FRAME_MAGIC.to_vec();
    wire.extend_from_slice(&weftos_leaf_types::encode(&publish).unwrap());
    let send_over = |port: u16| {
        let wire = wire.clone();
        async move {
            let mut s = crate::mesh_tcp::TcpTransport.connect(&format!("127.0.0.1:{port}")).await.unwrap();
            s.send(&wire).await.unwrap();
            tokio::time::timeout(Duration::from_secs(5), s.recv()).await.unwrap().unwrap()
        }
    };
    // First over the second address: admitted, ACK signed by the one machine key.
    let reply = send_over(p2).await;
    assert!(reply.starts_with(ACK_MAGIC));
    let ack: PublishAck = weftos_leaf_types::decode(&reply[ACK_MAGIC.len()..]).unwrap();
    ack.verify(&machine.verifying_key().to_bytes(), &publish).unwrap();
    assert!(seen(&rec, &topic).await);
    // The exact replay over the first address gets its ACK but no second
    // effect: both listeners share one registry and replay floor.
    let reply = send_over(p1).await;
    assert!(reply.starts_with(ACK_MAGIC));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(rec.0.lock().unwrap().len(), 1);
    task.abort();
}

#[tokio::test]
async fn wildcard_discovery_answers_a_unicast_probe_and_advertises_the_specific_tcp_address() {
    let dir = leaf_dir();
    let tcp_port = loop {
        let p = free_port();
        if p > 2 && std::net::UdpSocket::bind(("0.0.0.0", p - 1)).is_ok() { break p; }
    };
    let parent = SigningKey::from_bytes(&[81; 32]);
    let user_id = clawft_types::project::cert::key_id(&parent.verifying_key().to_bytes());
    let machine = SigningKey::from_bytes(&[82; 32]);
    let leaf = SigningKey::from_bytes(&[83; 32]);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let cert = LeafCertificate::issue(&parent, format!("user:{user_id}"), machine.verifying_key().to_bytes(), leaf.verifying_key().to_bytes(), 1, now - 1, now + 600, vec!["input.publish".into()]).unwrap();
    let id = cert.leaf_id();
    let ingress = crate::mesh_leaf::LeafIngress::open(dir.path(), machine.clone()).unwrap();
    std::fs::write(dir.path().join("registry").join(format!("{id}.json")), serde_json::json!({"node_id":id,"cert":cert}).to_string()).unwrap();
    let (rt, _) = runtime("srv");
    rt.set_leaf_ingress(Arc::new(ingress)).ok().unwrap();
    rt.set_leaf_discovery_any(true);
    rt.set_leaf_bind_specs(crate::mesh_leaf_bind::LeafBindSpec::parse_all(&[&format!("127.0.0.1:{tcp_port}")], false).unwrap());
    let (_main, task) = listen(&rt, None).await;
    wait_bound(&rt, 1).await;

    let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let nonce = [5u8; 16];
    let mut probe = b"WLD1".to_vec();
    probe.extend_from_slice(id.as_bytes());
    probe.extend_from_slice(&nonce);
    let target = format!("127.0.0.1:{}", tcp_port - 1);
    let mut buf = [0u8; 1024];
    let mut reply = None;
    // The wildcard socket binds asynchronously; probe until it answers.
    for _ in 0..50 {
        sock.send_to(&probe, &target).await.unwrap();
        if let Ok(Ok((n, _))) = tokio::time::timeout(Duration::from_millis(100), sock.recv_from(&mut buf)).await { reply = Some(n); break; }
    }
    let n = reply.expect("wildcard discovery answered");
    let ad: weftos_leaf_types::link::ParentAdvertisement = weftos_leaf_types::decode(&buf[..n]).unwrap();
    ad.verify_for(&machine.verifying_key().to_bytes(), &cert.parent_scope, now, &nonce).unwrap();
    assert_eq!(ad.endpoint, format!("127.0.0.1:{tcp_port}"));
    // An unenrolled leaf still gets silence.
    let mut other = b"WLD1".to_vec();
    other.extend_from_slice("f".repeat(32).as_bytes());
    other.extend_from_slice(&nonce);
    sock.send_to(&other, &target).await.unwrap();
    assert!(tokio::time::timeout(Duration::from_millis(300), sock.recv_from(&mut buf)).await.is_err());
    task.abort();
}
