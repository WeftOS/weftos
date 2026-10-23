//! UniFFI facade. Kotlin gets one object, `NodeCore`, holding identity, the parent side
//! of pairing and the indicator machine. All time is passed in (`now_ms`); nothing here
//! blocks or does I/O beyond reading/writing the identity file.

use crate::error::NodeError;
use crate::identity::{Identity, bytes_to_hex, hex_to_bytes};
use crate::indicator::{Indicator, IndicatorAction, IndicatorInput, IndicatorState};
use crate::pairing::{CAP_LED, CAP_MIC, CAP_SPEAKER, PairMsg, PairingState, ParentPairing};
use std::path::Path;
use std::sync::Mutex;

#[derive(uniffi::Object)]
pub struct NodeCore {
    data_dir: String,
    node_id: String,
    public_key_hex: String,
    pairing: Mutex<Option<ParentPairing>>,
    indicator: Mutex<Indicator>,
}

#[uniffi::export]
impl NodeCore {
    /// Load or create the identity under `data_dir` (same file as `clawft-android-edge`).
    #[uniffi::constructor]
    pub fn new(data_dir: String) -> Result<Self, NodeError> {
        let id = Identity::load_or_create(Path::new(&data_dir))?;
        Ok(Self {
            node_id: id.node_id(),
            public_key_hex: bytes_to_hex(&id.public_key()),
            data_dir,
            pairing: Mutex::new(None),
            indicator: Mutex::new(Indicator::new()),
        })
    }

    pub fn node_id(&self) -> String {
        self.node_id.clone()
    }

    pub fn public_key_hex(&self) -> String {
        self.public_key_hex.clone()
    }

    /// Begin parent-side pairing. `expected_peer_hex` is the glasses key from the beacon, if known.
    pub fn begin_pairing(&self, expected_peer_hex: Option<String>) -> Result<(), NodeError> {
        let expected = match expected_peer_hex {
            Some(h) => Some(
                hex_to_bytes(&h)?
                    .try_into()
                    .map_err(|_| NodeError::Pairing("peer key must be 32 bytes".into()))?,
            ),
            None => None,
        };
        let id = Identity::load_or_create(Path::new(&self.data_dir))?;
        *self.pairing.lock().unwrap() = Some(ParentPairing::new(
            id,
            expected,
            CAP_MIC | CAP_SPEAKER | CAP_LED,
        ));
        Ok(())
    }

    pub fn pairing_state(&self) -> PairingState {
        self.pairing
            .lock()
            .unwrap()
            .as_ref()
            .map_or(PairingState::Idle, |p| p.state().clone())
    }

    /// Feed pairing-lane bytes from the glasses; returns pairing-lane bytes to send back.
    pub fn pairing_on_bytes(&self, bytes: Vec<u8>) -> Result<Vec<Vec<u8>>, NodeError> {
        let mut g = self.pairing.lock().unwrap();
        let p = g
            .as_mut()
            .ok_or_else(|| NodeError::Pairing("pairing not started".into()))?;
        p.on_message(PairMsg::from_bytes(&bytes)?)
            .iter()
            .map(PairMsg::to_bytes)
            .collect()
    }

    /// The user entered the code the glasses spoke.
    pub fn pairing_confirm(&self, code: String, now_ms: u64) -> Result<Vec<Vec<u8>>, NodeError> {
        let mut g = self.pairing.lock().unwrap();
        let p = g
            .as_mut()
            .ok_or_else(|| NodeError::Pairing("pairing not started".into()))?;
        p.confirm_sas(&code, now_ms)
            .iter()
            .map(PairMsg::to_bytes)
            .collect()
    }

    pub fn indicator_state(&self) -> IndicatorState {
        self.indicator.lock().unwrap().state()
    }

    /// Next time `IndicatorInput::Tick` is needed, if any.
    pub fn indicator_next_deadline(&self) -> Option<u64> {
        self.indicator.lock().unwrap().next_deadline()
    }

    pub fn indicator_feed(&self, now_ms: u64, input: IndicatorInput) -> Vec<IndicatorAction> {
        self.indicator.lock().unwrap().handle(now_ms, input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn facade_reports_identity_and_drives_indicator() {
        let dir = tempfile::tempdir().unwrap();
        let core = NodeCore::new(dir.path().to_string_lossy().into()).unwrap();
        assert_eq!(core.node_id().len(), 32);
        assert_eq!(core.pairing_state(), PairingState::Idle);
        core.begin_pairing(None).unwrap();
        assert_eq!(core.pairing_state(), PairingState::AwaitingPeer);
        let a = core.indicator_feed(0, IndicatorInput::GateOpen);
        assert!(!a.is_empty());
        assert_eq!(core.indicator_state(), IndicatorState::Listening);
        assert!(core.pairing_on_bytes(vec![1, 2, 3]).is_err());
    }
}
