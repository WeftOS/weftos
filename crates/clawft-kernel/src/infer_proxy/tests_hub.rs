//! The hub over two real [`MeshRuntime`]s joined by in-process channels.
//! Each side's inbound frames are fed to the runtime with the
//! [`PeerCtx`] the test chooses, standing in for the connection's
//! admission result.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use super::hub::InferHub;
use super::listener::{InferProxy, OccupiedPolicy, Started};
use super::mesh_forward::ServeGate;
use super::support::*;
use super::table::PlacementTable;
use super::types::*;
use super::upstream::Upstream;
use crate::mesh_admit::PeerClass;
use crate::mesh_delivery::PeerCtx;
use crate::mesh_runtime::{MeshRuntime, PeerControlSink};

struct Side {
    rt: Arc<MeshRuntime>,
    hub: Arc<InferHub>,
    table: Arc<PlacementTable>,
}

fn ctx(peer: &str, verified: bool, class: PeerClass) -> PeerCtx {
    PeerCtx {
        peer_id: peer.into(),
        node_verified: verified,
        class,
        remote_static: None,
        src_scope: None,
    }
}

fn side(node: &str, audit: Arc<Audit>) -> Side {
    let rt = Arc::new(MeshRuntime::new(node.into()));
    let up = Arc::new(Upstream::new(small_limits()).unwrap());
    let hub = InferHub::new(rt.clone(), up, Arc::new(ServeGate::new(4, 16)), Some(audit.clone()));
    let table = Arc::new(PlacementTable::new(node, Some(hub.clone() as Arc<dyn MeshDialer>), Some(audit)));
    hub.attach(table.clone());
    hub.install();
    Side { rt, hub, table }
}

/// Join `a` and `b`; frames from each arrive at the other with `ctx_of_a`
/// (how B sees A) and `ctx_of_b` (how A sees B).
fn join(a: &Side, b: &Side, ctx_of_a: PeerCtx, ctx_of_b: PeerCtx) {
    let (a2b_tx, mut a2b_rx) = mpsc::channel::<Vec<u8>>(256);
    let (b2a_tx, mut b2a_rx) = mpsc::channel::<Vec<u8>>(256);
    // Routes are registered the way the accept loop does for an admitted
    // peer, so each carries the verified flag the connection earned.
    let tally = crate::mesh_runtime::RouteTally::default();
    a.rt.register_authenticated(ctx_of_b.peer_id.clone(), a2b_tx.clone(), ctx_of_b.node_verified, &tally);
    b.rt.register_authenticated(ctx_of_a.peer_id.clone(), b2a_tx.clone(), ctx_of_a.node_verified, &tally);
    let (rb, ra) = (b.rt.clone(), a.rt.clone());
    let (b2a, a2b) = (b2a_tx, a2b_tx);
    tokio::spawn(async move {
        while let Some(bytes) = a2b_rx.recv().await {
            let _ = rb.handle_incoming_peer(&bytes, b2a.clone(), Some(&ctx_of_a)).await;
        }
    });
    tokio::spawn(async move {
        while let Some(bytes) = b2a_rx.recv().await {
            let _ = ra.handle_incoming_peer(&bytes, a2b.clone(), Some(&ctx_of_b)).await;
        }
    });
}

async fn proxy(t: &Arc<PlacementTable>) -> InferProxy {
    match InferProxy::start("hermes", "127.0.0.1:0".parse().unwrap(), OccupiedPolicy::Refuse, t.clone(), small_limits(), None)
        .await
        .unwrap()
    {
        Started::Running(p) => p,
        _ => panic!(),
    }
}

async fn meet(w: &World) {
    w.a.hub.announce(0).await;
    w.b.hub.announce(0).await;
    settle().await;
}

async fn settle() {
    tokio::time::sleep(Duration::from_millis(150)).await;
}

struct World {
    a: Side,
    b: Side,
    up: Fake,
    audit_a: Arc<Audit>,
    audit_b: Arc<Audit>,
}

async fn world(ctx_of_a: PeerCtx, ctx_of_b: PeerCtx) -> World {
    let (audit_a, audit_b) = (Arc::new(Audit::default()), Arc::new(Audit::default()));
    let (a, b) = (side("node-a", audit_a.clone()), side("node-b", audit_b.clone()));
    let up = fake(Reply::ok(r#"{"from":"b"}"#)).await;
    b.table.register_local("hermes", &up.base(), Some("m".into()), "openai-v1", "LlamaCpp").unwrap();
    b.table.expose_to_mesh("hermes", true);
    b.table.allow_mesh_peer("hermes", "node-a", true);
    a.table.allow_remote_node("hermes", "node-b", true);
    join(&a, &b, ctx_of_a, ctx_of_b);
    World { a, b, up, audit_a, audit_b }
}

fn node(id: &str) -> PeerCtx {
    ctx(id, true, PeerClass::Node)
}

#[tokio::test]
async fn an_advert_and_a_request_cross_two_runtimes() {
    let w = world(node("node-a"), node("node-b")).await;
    // Each side learns the other's standing, then B announces.
    meet(&w).await;
    w.b.hub.announce(10).await;
    settle().await;
    assert!(matches!(w.a.table.resolve("hermes"), Some(Target::Remote { ref node_id }) if node_id == "node-b"));
    // The request goes A's proxy -> hub -> mesh -> B's hub -> B's fake.
    let p = proxy(&w.a.table).await;
    let resp = raw(p.addr(), &post(p.addr(), "/v1/chat/completions", r#"{"messages":[],"adapters":"x"}"#)).await;
    assert_eq!(status(&resp), 200, "{resp}");
    assert_eq!(body(&resp), r#"{"from":"b"}"#);
    let sent: serde_json::Value = serde_json::from_slice(&w.up.last().body).unwrap();
    assert_eq!(sent["model"], "m");
    assert!(sent.get("adapters").is_none());
    assert_eq!(w.a.hub.qualifying_peers(), ["node-b"]);
}

#[tokio::test]
async fn a_large_streamed_response_survives_the_control_channel() {
    let (audit_a, audit_b) = (Arc::new(Audit::default()), Arc::new(Audit::default()));
    let (a, b) = (side("node-a", audit_a), side("node-b", audit_b));
    let mut r = Reply::ok("");
    let big: Vec<u8> = (0..150_000u32).map(|i| b'a' + (i % 26) as u8).collect();
    r.chunks = vec![big.clone()];
    let up = fake(r).await;
    b.table.register_local("hermes", &up.base(), None, "openai-v1", "LlamaCpp").unwrap();
    b.table.expose_to_mesh("hermes", true);
    b.table.allow_mesh_peer("hermes", "node-a", true);
    a.table.allow_remote_node("hermes", "node-b", true);
    join(&a, &b, node("node-a"), node("node-b"));
    a.hub.announce(0).await;
    b.hub.announce(0).await;
    settle().await;
    b.hub.announce(10).await;
    settle().await;
    let p = proxy(&a.table).await;
    let resp = raw(p.addr(), &get(p.addr(), "/v1/models")).await;
    assert_eq!(body(&resp).as_bytes(), big.as_slice());
}

#[tokio::test]
async fn adverts_are_only_sent_to_listed_qualifying_peers() {
    let w = world(node("node-a"), node("node-b")).await;
    meet(&w).await;
    w.b.table.allow_mesh_peer("hermes", "node-a", false);
    w.b.hub.announce(11).await;
    settle().await;
    assert_eq!(w.a.table.resolve("hermes"), None, "B does not announce to a peer it does not serve");
}

#[tokio::test]
async fn the_serving_hub_refuses_peers_that_do_not_qualify() {
    for (name, c, want) in [
        // No verified standing: dropped silently, the consumer's stall timeout fires.
        ("unverified", ctx("node-a", false, PeerClass::Node), 504),
        // Verified but not a full node: refused at once, so the consumer falls back.
        ("leaf", ctx("node-a", true, PeerClass::Leaf), 502),
        ("legacy", ctx("node-a", true, PeerClass::Legacy), 502),
    ] {
        let w = world(c, node("node-b")).await;
        // A (consumer) believes B qualifies and has an advert for it.
        meet(&w).await;
        let ad = w.b.table.advertisement("hermes", 10).unwrap();
        w.a.hub.attach(w.a.table.clone());
        w.a.table.ingest_advertisement("node-b", &ad);
        let p = proxy(&w.a.table).await;
        let resp = raw(p.addr(), &get(p.addr(), "/v1/models")).await;
        assert_eq!(status(&resp), want, "{name}: {resp}");
        assert_eq!(w.up.count(), 0, "{name}: the model server was reached");
        let _ = (&w.audit_a, &w.audit_b);
    }
}

#[tokio::test]
async fn an_advert_for_another_node_is_not_taken_from_a_peers_connection() {
    let w = world(node("node-a"), node("node-b")).await;
    w.b.table.expose_to_mesh("hermes", true);
    let mut ad = w.b.table.advertisement("hermes", 10).unwrap();
    ad.node_id = "node-x".into();
    w.a.table.allow_remote_node("hermes", "node-x", true);
    // B sends an advert whose payload names node-x; the sender is B.
    let m = crate::ipc::KernelMessage::new(
        0,
        crate::ipc::MessageTarget::Topic(crate::mesh_runtime::INFER_TOPIC.into()),
        crate::ipc::MessagePayload::Json(serde_json::json!({"t": "advert", "ad": ad})),
    );
    w.b.rt.route_to_remote("node-a", m).await.unwrap();
    settle().await;
    assert_eq!(w.a.table.resolve("hermes"), None);
    assert!(w.audit_a.kinds().contains(&"infer.advert.refused".to_string()));
}

#[tokio::test]
async fn a_response_is_accepted_only_from_the_peer_the_request_went_to() {
    let w = world(node("node-a"), node("node-b")).await;
    meet(&w).await;
    let d = w.a.hub.clone();
    let stream = d.dial_for_test("node-b").await;
    // A different peer answering id 1 reaches nothing.
    let forged = serde_json::json!({"t": "resp", "id": 1, "data": "0200"});
    d.on_peer_control(&node("node-evil"), 0, &forged);
    assert_eq!(stream.pending_len(), 1, "exchange untouched by a stranger");
    d.on_peer_control(&node("node-b"), 0, &forged);
    drop(stream);
}

async fn dial_refused(hub: &Arc<InferHub>, node: &str) -> bool {
    matches!(hub.dial(node).await, Err(ProxyError::Refused(_)))
}

#[tokio::test]
async fn a_standing_is_tied_to_the_connection_it_was_seen_on() {
    let w = world(node("node-a"), node("node-b")).await;
    meet(&w).await;
    assert!(w.a.hub.standing("node-b").is_some());
    // node-b reconnects (a new connection) and sends nothing on it: the old
    // grant describes a connection that is gone.
    w.a.rt.disconnect_peer("node-b");
    let (tx, _rx) = mpsc::channel::<Vec<u8>>(8);
    let tally = crate::mesh_runtime::RouteTally::default();
    w.a.rt.register_authenticated("node-b".into(), tx, true, &tally);
    assert!(w.a.hub.standing("node-b").is_none(), "grant from the old connection");
    assert!(dial_refused(&w.a.hub, "node-b").await);
    assert!(!w.a.hub.is_admitted("node-b"));
    // It then speaks as a Leaf: standing exists but does not qualify.
    w.a.hub.on_peer_control(&ctx("node-b", true, PeerClass::Leaf), w.a.rt.peer_route("node-b").unwrap().0, &serde_json::json!({"t": "hello"}));
    assert!(w.a.hub.standing("node-b").is_some());
    assert!(dial_refused(&w.a.hub, "node-b").await, "a Leaf is not served by dialing");
}

#[tokio::test]
async fn relaxed_admission_never_leaves_a_standing_behind() {
    let w = world(node("node-a"), node("node-b")).await;
    meet(&w).await;
    assert!(w.a.hub.standing("node-b").is_some());
    let conn = w.a.rt.peer_route("node-b").unwrap().0;
    // The same connection is heard again, now without a verified id (admission
    // relaxed or the peer is no longer a member): the grant is dropped.
    w.a.hub.on_peer_control(&ctx("node-b", false, PeerClass::Node), conn, &serde_json::json!({"t": "hello"}));
    assert!(w.a.hub.standing("node-b").is_none());
    assert!(dial_refused(&w.a.hub, "node-b").await);
    // A route that is not verified (the first-envelope path under observe)
    // has no standing even if a grant was somehow recorded for it.
    w.a.rt.disconnect_peer("node-b");
    let (tx, _rx) = mpsc::channel::<Vec<u8>>(8);
    let tally = crate::mesh_runtime::RouteTally::default();
    w.a.rt.register_authenticated("node-b".into(), tx, false, &tally);
    let conn = w.a.rt.peer_route("node-b").unwrap().0;
    w.a.hub.on_peer_control(&node("node-b"), conn, &serde_json::json!({"t": "hello"}));
    assert!(w.a.hub.standing("node-b").is_none(), "unverified route");
}

#[tokio::test]
async fn a_peer_leaving_clears_its_standing() {
    let w = world(node("node-a"), node("node-b")).await;
    meet(&w).await;
    assert_eq!(w.a.hub.grants_len(), 1);
    w.a.rt.disconnect_peer("node-b");
    settle().await;
    assert_eq!(w.a.hub.grants_len(), 0, "the Left event cleared it");
}

#[tokio::test]
async fn exchanges_in_flight_are_capped() {
    let w = world(node("node-a"), node("node-b")).await;
    meet(&w).await;
    w.a.hub.set_max_pending(2);
    let (a, b) = (w.a.hub.dial("node-b").await.ok().unwrap(), w.a.hub.dial("node-b").await.ok().unwrap());
    let third = w.a.hub.dial("node-b").await;
    assert!(matches!(third, Err(ProxyError::NoInstance(_))));
    drop(a);
    assert!(w.a.hub.dial("node-b").await.is_ok(), "a finished exchange frees a slot");
    drop(b);
}

fn req_msg(i: u64) -> serde_json::Value {
    serde_json::json!({"t": "req", "id": i, "data": "00"})
}

#[tokio::test]
async fn a_flood_of_refused_requests_spawns_nothing() {
    let audit = Arc::new(Audit::default());
    let b = side("node-b", audit.clone());
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(1024);
    b.rt.add_peer("node-a".into(), tx);
    let drained = |rx: &mut mpsc::Receiver<Vec<u8>>| {
        let mut n = 0;
        while rx.try_recv().is_ok() {
            n += 1;
        }
        n
    };
    // An unverified peer (no grant): no reply, no audit line, nothing.
    let c = ctx("node-a", false, PeerClass::Node);
    for i in 0..200 {
        b.hub.on_peer_control(&c, 1, &req_msg(i));
    }
    settle().await;
    assert_eq!(drained(&mut rx), 0, "replied to an unverified peer");
    assert!(audit.kinds().is_empty(), "{:?}", audit.kinds());
    // A verified peer that does not qualify (a leaf) is refused, rate-limited,
    // so its consumer falls back at once.
    let leaf = ctx("node-a", true, PeerClass::Leaf);
    for i in 0..200 {
        b.hub.on_peer_control(&leaf, 1, &req_msg(i));
    }
    settle().await;
    let n = drained(&mut rx);
    assert!((1..=8).contains(&n), "{n} refusals to a leaf");
    assert!(audit.kinds().len() <= 8);
    // A qualifying peer that is on no allowlist: refused a few times, then silence.
    let good = node("node-a");
    for i in 0..200 {
        b.hub.on_peer_control(&good, 1, &req_msg(i));
    }
    settle().await;
    let n = drained(&mut rx);
    assert!((0..=8).contains(&n), "{n} refusals sent");
    assert!(audit.kinds().len() <= 16);
}

#[tokio::test]
async fn switching_an_exposure_off_withdraws_the_advert_at_the_peer_at_once() {
    use crate::exposure::ExposureState;
    let w = world(node("node-a"), node("node-b")).await;
    meet(&w).await;
    w.b.hub.announce(10).await;
    settle().await;
    assert!(w.a.table.resolve("hermes").is_some());
    w.b.table.exposures().set_state("infer.hermes", ExposureState::Off, "test").unwrap();
    assert_eq!(w.b.hub.flush_withdrawals().await, 1);
    settle().await;
    assert!(w.a.table.resolve("hermes").is_none(), "no waiting out the advert TTL");
    // Nothing left to withdraw, and the next announce does not re-advertise.
    assert_eq!(w.b.hub.flush_withdrawals().await, 0);
    w.b.hub.announce(20).await;
    settle().await;
    assert!(w.a.table.resolve("hermes").is_none());
    // Switched back on, the next announce restores it.
    w.b.table.exposures().set_state("infer.hermes", ExposureState::On, "test").unwrap();
    w.b.hub.announce(30).await;
    settle().await;
    assert!(w.a.table.resolve("hermes").is_some());
}

#[tokio::test]
async fn a_peer_request_appears_in_the_serving_nodes_activity_log() {
    let w = world(node("node-a"), node("node-b")).await;
    meet(&w).await;
    w.b.hub.announce(10).await;
    settle().await;
    let p = proxy(&w.a.table).await;
    let resp = raw(p.addr(), &post(p.addr(), "/v1/chat/completions", r#"{"messages":[]}"#)).await;
    assert_eq!(status(&resp), 200, "{resp}");
    let recs = w.b.table.exposures().activity().query(Some("infer.hermes"), 10);
    assert_eq!(recs.len(), 1, "{recs:?}");
    assert_eq!((recs[0].caller_node.as_str(), recs[0].outcome.as_str()), ("node-a", "ok"));
    assert!(w.b.table.exposures().activity().verify().is_ok());
}
