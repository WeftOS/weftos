//! UniFFI facade for the glasses agent (`apps/android-glasses-node`). One object,
//! `GlassesCore`, wrapping [`GlassesNode`]. All time is passed in; nothing here blocks.

use crate::audio::Codec;
use crate::error::NodeError;
use crate::glasses::{GlassesNode, GlassesPairingState, GlassesStatus, Handled};
use crate::identity::{Identity, bytes_to_hex};
use crate::indicator::{IndicatorInput, IndicatorState};
use std::path::Path;
use std::sync::Mutex;

#[derive(uniffi::Object)]
pub struct GlassesCore {
    node_id: String,
    public_key_hex: String,
    node: Mutex<GlassesNode>,
}

#[uniffi::export]
impl GlassesCore {
    /// Load or create the glasses identity under `data_dir`.
    #[uniffi::constructor]
    pub fn new(data_dir: String) -> Result<Self, NodeError> {
        let id = Identity::load_or_create(Path::new(&data_dir))?;
        let node = GlassesNode::new(id);
        Ok(Self {
            node_id: node.node_id().to_string(),
            public_key_hex: bytes_to_hex(&node.public_key()),
            node: Mutex::new(node),
        })
    }

    pub fn node_id(&self) -> String {
        self.node_id.clone()
    }

    pub fn public_key_hex(&self) -> String {
        self.public_key_hex.clone()
    }

    pub fn pair_hello(&self) -> Result<Vec<u8>, NodeError> {
        self.node.lock().unwrap().pair_hello()
    }

    pub fn pairing_state(&self) -> GlassesPairingState {
        self.node.lock().unwrap().pairing_state()
    }

    /// Raw socket bytes in; frames to write, indicator actions and shell commands out.
    pub fn on_bytes(&self, bytes: Vec<u8>, now_ms: u64) -> Result<Handled, NodeError> {
        self.node.lock().unwrap().on_bytes(&bytes, now_ms)
    }

    /// Local indicator event: gate open/close, PTT, link lost, reply audio, tick.
    pub fn feed(&self, now_ms: u64, input: IndicatorInput) -> Result<Handled, NodeError> {
        self.node.lock().unwrap().feed(now_ms, input)
    }

    pub fn indicator_state(&self) -> IndicatorState {
        self.node.lock().unwrap().indicator_state()
    }

    pub fn turn_id(&self) -> u64 {
        self.node.lock().unwrap().turn_id()
    }

    /// Next time `IndicatorInput::Tick` is needed, if any.
    pub fn next_deadline(&self) -> Option<u64> {
        self.node.lock().unwrap().next_deadline()
    }

    pub fn status_frame(&self, status: GlassesStatus) -> Result<Vec<u8>, NodeError> {
        self.node.lock().unwrap().status_frame(&status)
    }

    pub fn mic_gate_frame(&self, open: bool, rms: u32, voiced: f32) -> Result<Vec<u8>, NodeError> {
        self.node.lock().unwrap().mic_gate_frame(open, rms, voiced)
    }

    pub fn link_up_frame(&self) -> Result<Vec<u8>, NodeError> {
        self.node.lock().unwrap().link_up_frame()
    }

    pub fn playback_done_frame(
        &self,
        utt_id: u32,
        spoken_through_seq: u32,
    ) -> Result<Vec<u8>, NodeError> {
        self.node
            .lock()
            .unwrap()
            .playback_done_frame(utt_id, spoken_through_seq)
    }

    /// One 20 ms uplink PCM16 frame (P0 codec; Opus is P1).
    pub fn audio_up_pcm_frame(
        &self,
        utt: u32,
        seq: u32,
        ts_ms: u32,
        pcm: Vec<u8>,
    ) -> Result<Vec<u8>, NodeError> {
        self.node
            .lock()
            .unwrap()
            .audio_up_frame(Codec::Pcm16, utt, seq, ts_ms, pcm)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn facade_creates_identity_and_frames_status() {
        let dir = tempfile::tempdir().unwrap();
        let g = GlassesCore::new(dir.path().to_string_lossy().into()).unwrap();
        assert_eq!(g.node_id().len(), 32);
        assert_eq!(g.pairing_state(), GlassesPairingState::Idle);
        let f = g.audio_up_pcm_frame(1, 0, 0, vec![0; 640]).unwrap();
        assert!(f.len() > 640);
        let h = g.feed(0, IndicatorInput::GateOpen).unwrap();
        assert!(!h.actions.is_empty() && !h.send.is_empty());
        assert_eq!(g.indicator_state(), IndicatorState::Listening);
    }
}
