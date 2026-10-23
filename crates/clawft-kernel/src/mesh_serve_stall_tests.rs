//! A connection whose peer stops reading is closed, not left holding its
//! route and outbound channel (card 1a919ae6).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use super::*;
use crate::mesh_noise::EncryptedChannel;

/// A peer that completed its handshake and then went deaf: writes never
/// finish (its receive window is full) and nothing ever arrives.
struct DeafChannel;

#[async_trait]
impl EncryptedChannel for DeafChannel {
    async fn send_encrypted(&mut self, _: &[u8]) -> Result<(), crate::mesh::MeshError> {
        std::future::pending().await
    }
    async fn recv_encrypted(&mut self) -> Result<Vec<u8>, crate::mesh::MeshError> {
        std::future::pending().await
    }
    fn remote_static_key(&self) -> Option<&[u8]> {
        None
    }
    async fn close(&mut self) -> Result<(), crate::mesh::MeshError> {
        Ok(())
    }
}

#[tokio::test]
async fn a_peer_that_stops_reading_is_closed_and_its_route_removed() {
    let rt = Arc::new(MeshRuntime::new("local".into()));
    let (out_tx, out_rx) = tokio::sync::mpsc::channel(4);
    let tally = RouteTally::default();
    rt.add_peer_tallied("deaf".into(), out_tx.clone(), &tally);
    let limits = Limits {
        first_frame: Duration::from_secs(600),
        write_stall: Duration::from_millis(100),
        ..Limits::default()
    };
    let (prt, ptx) = (Arc::clone(&rt), out_tx.clone());
    let task = tokio::spawn(async move {
        pump(&prt, Box::new(DeafChannel), ChannelKind::Passthrough, "deaf-peer", &crate::mesh_admit::AllowAll,
            limits, ptx, out_rx, None, tally, false, false).await;
    });
    rt.send_to_peer("deaf", crate::mesh_ipc::MeshIpcEnvelope::new(
        "local".into(), "deaf".into(),
        crate::ipc::KernelMessage::text(0, crate::ipc::MessageTarget::Topic("t".into()), "x"),
    )).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("the pump kept a connection whose peer stopped reading")
        .unwrap();
    assert_eq!(rt.peer_count(), 0, "the stalled connection's route went with it");
    // Later sends fail at once instead of queueing behind the dead peer.
    assert!(out_tx.try_send(b"x".to_vec()).is_err());
}
