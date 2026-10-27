//! Raw frames on a peer connection that are neither an IPC envelope nor an
//! assessment frame: the cogbus mesh topic frames (`TopicRequest` 0x12,
//! `TopicData` 0x13, ADR-122 section 14). They carry binary stream chunks, so
//! they are sent as `[len][type][payload]` on the connection, not wrapped in
//! a JSON envelope.
//!
//! A frame is handed to the sink registered for its type only when it came
//! from a peer whose node id admission verified; anything else is dropped
//! here. The sink decides the rest (whether the grant qualifies, what the
//! peer may read).

use std::sync::Arc;

use super::{MeshRuntime, PeerCtx};
use crate::mesh_framing::{FrameType, MeshFrame};

/// Receiver of one raw frame type.
pub trait PeerFrameSink: Send + Sync + 'static {
    /// One frame from `ctx.peer_id` (verified). Must not block.
    fn on_peer_frame(&self, ctx: &PeerCtx, frame: MeshFrame);
}

/// `[len][type][payload]` with a registered type, else `None`.
fn framed(data: &[u8]) -> Option<(u8, &[u8])> {
    if data.len() < 5 || data[0] == b'{' {
        return None;
    }
    let len = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
    (len + 4 == data.len()).then(|| (data[4], &data[5..]))
}

impl MeshRuntime {
    /// Receive raw frames of `frame_type` from verified peers in `sink`.
    pub fn set_frame_sink(&self, frame_type: FrameType, sink: Arc<dyn PeerFrameSink>) {
        self.frame_sinks.insert(frame_type.as_byte(), sink);
    }

    /// Hand `data` to its frame sink. True when it was a frame of a
    /// registered type (delivered, or dropped because the peer is not
    /// verified); false for anything else, which continues as an envelope.
    pub(super) fn try_frame_sink(&self, data: &[u8], peer: Option<&PeerCtx>) -> bool {
        let Some((ty, payload)) = framed(data) else { return false };
        let Some(sink) = self.frame_sinks.get(&ty).map(|s| s.clone()) else { return false };
        let Some(frame_type) = FrameType::from_byte(ty) else { return false };
        match peer.filter(|c| c.node_verified) {
            Some(ctx) => sink.on_peer_frame(ctx, MeshFrame { frame_type, payload: payload.to_vec() }),
            None => tracing::debug!(frame = ty, "raw frame from an unverified peer; dropped"),
        }
        true
    }

    /// Queue `frame` on `node_id`'s connection without waiting: false when
    /// the peer is not connected, the frame is too large, or the outbound
    /// channel is full (the caller counts it as dropped).
    pub fn try_send_frame(&self, node_id: &str, frame: &MeshFrame) -> bool {
        let Some(sender) = self.peers.get(node_id).map(|p| p.sender.clone()) else { return false };
        let Ok(bytes) = frame.encode() else { return false };
        sender.try_send(bytes).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct Seen(Mutex<Vec<(String, Vec<u8>)>>);

    impl PeerFrameSink for Seen {
        fn on_peer_frame(&self, ctx: &PeerCtx, frame: MeshFrame) {
            self.0.lock().unwrap().push((ctx.peer_id.clone(), frame.payload));
        }
    }

    fn ctx(id: &str, verified: bool) -> PeerCtx {
        let mut c = PeerCtx::unauthenticated(id.to_string());
        c.node_verified = verified;
        c
    }

    #[tokio::test]
    async fn topic_frames_reach_their_sink_only_from_verified_peers_and_go_out_raw() {
        let rt = MeshRuntime::new("me".into());
        let seen = Arc::new(Seen::default());
        rt.set_frame_sink(FrameType::TopicRequest, seen.clone());
        let f = MeshFrame { frame_type: FrameType::TopicRequest, payload: vec![1, 2, 3] }.encode().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        rt.handle_incoming_peer(&f, tx.clone(), Some(&ctx("peer-a", true))).await.unwrap();
        rt.handle_incoming_peer(&f, tx.clone(), Some(&ctx("peer-b", false))).await.unwrap();
        assert_eq!(*seen.0.lock().unwrap(), [("peer-a".to_string(), vec![1, 2, 3])]);
        // A type with no sink is not swallowed: it fails as an envelope would.
        let other = MeshFrame { frame_type: FrameType::TopicData, payload: vec![9] }.encode().unwrap();
        assert!(rt.handle_incoming_peer(&other, tx.clone(), Some(&ctx("peer-a", true))).await.is_err());
        // Sending: raw bytes on the peer's channel, or false when absent.
        rt.add_peer("peer-a".into(), tx);
        let out = MeshFrame { frame_type: FrameType::TopicData, payload: vec![7] };
        assert!(rt.try_send_frame("peer-a", &out));
        assert_eq!(rx.recv().await.unwrap(), out.encode().unwrap());
        assert!(!rt.try_send_frame("nobody", &out));
    }
}
