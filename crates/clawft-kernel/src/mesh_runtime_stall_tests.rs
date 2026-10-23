//! A stalled peer must not wedge the runtime (card 1a919ae6).
//!
//! The 2026-10-15 hang: a peer whose connection stopped draining filled its
//! outbound channel. `send_to_peer` awaited channel capacity while it still
//! held the `peers` shard read guard, so every write to that shard (a peer
//! registering or reconnecting, a connection closing) blocked its runtime
//! thread on a futex. Those threads never came back; the facts probe timed
//! out every two minutes until the runtime stopped scheduling at all.
//!
//! Each test runs the stall and the competing call on their own OS threads,
//! so on the old code the assertion fails (instead of the test hanging).

use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use super::*;
use crate::ipc::{KernelMessage, MessagePayload, MessageTarget};

/// How long a call that must not block may take before the test fails.
const MUST_RETURN_WITHIN: Duration = Duration::from_secs(10);

fn msg() -> KernelMessage {
    KernelMessage::new(
        0,
        MessageTarget::Topic("mesh.node_facts".into()),
        MessagePayload::Json(serde_json::json!({"probe": true})),
    )
}

/// A peer whose outbound channel is full and is never drained: what a peer
/// that stalls its handshake or stops reading looks like to the runtime.
fn stalled_peer(rt: &MeshRuntime, id: &str) -> tokio::sync::mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    tx.try_send(b"backlog".to_vec()).unwrap();
    rt.add_peer(id.into(), tx);
    rx
}

/// Run `f` on a fresh OS thread with its own current-thread runtime; the
/// receiver yields its result.
fn on_thread<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> std_mpsc::Receiver<T> {
    let (done, rx) = std_mpsc::channel();
    std::thread::spawn(move || {
        let _ = done.send(f());
    });
    rx
}

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(f)
}

#[test]
fn a_send_to_a_stalled_peer_does_not_block_that_peer_reconnecting() {
    let rt = Arc::new(MeshRuntime::new("local".into()));
    let _stalled = stalled_peer(&rt, "stalled");

    // The facts broadcast (or any send) to the stalled peer, in flight.
    let sender = rt.clone();
    let send = on_thread(move || block_on(sender.route_to_remote("stalled", msg())));
    std::thread::sleep(Duration::from_millis(200));

    // The same peer reconnects: a write to the same `peers` shard.
    let registrar = rt.clone();
    let reconnect = on_thread(move || {
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        registrar.add_peer("stalled".into(), tx);
    });
    assert!(
        reconnect.recv_timeout(MUST_RETURN_WITHIN).is_ok(),
        "registering a peer blocked behind a send to a stalled peer"
    );
    // And the send itself gives up rather than wait forever.
    let sent = send
        .recv_timeout(MUST_RETURN_WITHIN + PEER_SEND_TIMEOUT)
        .expect("a send to a stalled peer never returned");
    assert!(sent.is_err(), "a send to a full, undrained channel must fail, got {sent:?}");
}

#[test]
fn a_send_to_a_stalled_peer_fails_within_the_send_timeout() {
    let rt = Arc::new(MeshRuntime::new("local".into()));
    let _stalled = stalled_peer(&rt, "stalled");
    let started = std::time::Instant::now();
    let sender = rt.clone();
    let send = on_thread(move || block_on(sender.route_to_remote("stalled", msg())));
    let r = send
        .recv_timeout(PEER_SEND_TIMEOUT + MUST_RETURN_WITHIN)
        .expect("a send to a stalled peer never returned");
    let err = r.expect_err("a full channel is an error");
    assert!(err.to_string().contains("outbound channel full"), "{err}");
    assert!(started.elapsed() >= PEER_SEND_TIMEOUT, "gave the peer its full window");
}

#[test]
fn a_broadcast_with_one_stalled_peer_still_reaches_the_others_and_lets_peers_leave() {
    let rt = Arc::new(MeshRuntime::new("local".into()));
    let _stalled = stalled_peer(&rt, "stalled");
    let (ok_tx, mut ok_rx) = tokio::sync::mpsc::channel(8);
    rt.add_peer("healthy".into(), ok_tx);

    let caster = rt.clone();
    let cast = on_thread(move || block_on(caster.broadcast_raw(b"gossip")));
    std::thread::sleep(Duration::from_millis(200));

    // The stalled peer's connection closes while the broadcast is waiting on it.
    let leaver = rt.clone();
    let leave = on_thread(move || leaver.disconnect_peer("stalled"));
    assert!(
        leave.recv_timeout(MUST_RETURN_WITHIN).is_ok(),
        "a peer leaving blocked behind a broadcast to a stalled peer"
    );
    let sent = cast
        .recv_timeout(PEER_SEND_TIMEOUT + MUST_RETURN_WITHIN)
        .expect("broadcast never returned");
    assert_eq!(sent, 1, "the healthy peer got the frame, the stalled one did not");
    assert_eq!(ok_rx.try_recv().unwrap(), b"gossip".to_vec());
}

#[test]
fn many_peers_register_while_a_facts_send_is_stuck_on_a_stalled_peer() {
    // The incident pattern: a probe-driven send waits on one stalled peer
    // while other peers keep connecting (each one a shard write).
    let rt = Arc::new(MeshRuntime::new("local".into()));
    let _stalled = stalled_peer(&rt, "stalled");
    let sender = rt.clone();
    let _send = on_thread(move || block_on(sender.route_to_remote("stalled", msg())));
    std::thread::sleep(Duration::from_millis(200));

    let registrar = rt.clone();
    let joins = on_thread(move || {
        let mut keep = Vec::new();
        for i in 0..256 {
            let (tx, rx) = tokio::sync::mpsc::channel(8);
            registrar.add_peer(format!("peer-{i}"), tx);
            keep.push(rx);
        }
        // Re-register the stalled id too (a reconnect under the same claim).
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        registrar.add_peer("stalled".into(), tx);
        keep.push(rx);
        keep.len()
    });
    assert_eq!(
        joins.recv_timeout(MUST_RETURN_WITHIN).ok(),
        Some(257),
        "peer registrations wedged behind a send to a stalled peer"
    );
}
