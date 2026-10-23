//! UniFFI facade for the phone's glasses link (`apps/android-phone-node`). One object,
//! `PhoneLink`, wrapping [`PhoneSession`]. All time is passed in; nothing here blocks.

use crate::error::NodeError;
use crate::identity::{Identity, bytes_to_hex, hex_to_bytes};
use crate::phone::{PhoneOut, PhoneSession, RelayResult};
use std::path::PathBuf;
use std::sync::Mutex;

#[derive(uniffi::Object)]
pub struct PhoneLink {
    node_id: String,
    public_key_hex: String,
    session: Mutex<PhoneSession>,
}

#[uniffi::export]
impl PhoneLink {
    /// Load or create the phone identity under `data_dir` (the same file `NodeCore` uses).
    /// `trusted_glasses_hex`: the glasses key the owner confirmed before, if any.
    #[uniffi::constructor]
    pub fn new(
        data_dir: String,
        trusted_glasses_hex: Option<String>,
        turn_base: u64,
    ) -> Result<Self, NodeError> {
        let dir = PathBuf::from(data_dir);
        let id = Identity::load_or_create(&dir)?;
        let trusted = match trusted_glasses_hex.filter(|h| !h.is_empty()) {
            Some(h) => Some(
                hex_to_bytes(&h)?
                    .try_into()
                    .map_err(|_| NodeError::Pairing("glasses key must be 32 bytes".into()))?,
            ),
            None => None,
        };
        let (node_id, public_key_hex) = (id.node_id(), bytes_to_hex(&id.public_key()));
        let make_id = move || {
            Identity::load_or_create(&dir).expect("identity file readable after first load")
        };
        Ok(Self {
            node_id,
            public_key_hex,
            session: Mutex::new(PhoneSession::new(make_id, trusted, turn_base)),
        })
    }

    pub fn node_id(&self) -> String {
        self.node_id.clone()
    }

    pub fn public_key_hex(&self) -> String {
        self.public_key_hex.clone()
    }

    /// Remember the glasses key the owner confirmed (hex); empty forgets it.
    pub fn set_trusted(&self, glasses_key_hex: String) -> Result<(), NodeError> {
        let key = if glasses_key_hex.is_empty() {
            None
        } else {
            Some(
                hex_to_bytes(&glasses_key_hex)?
                    .try_into()
                    .map_err(|_| NodeError::Pairing("glasses key must be 32 bytes".into()))?,
            )
        };
        self.session.lock().unwrap().set_trusted(key);
        Ok(())
    }

    /// A glasses socket was accepted. `resumed`: same address as an already paired peer.
    pub fn on_connect(&self, resumed: bool) {
        self.session.lock().unwrap().on_connect(resumed);
    }

    pub fn on_disconnect(&self) -> PhoneOut {
        self.session.lock().unwrap().on_disconnect()
    }

    pub fn is_authorized(&self) -> bool {
        self.session.lock().unwrap().is_authorized()
    }

    pub fn pairing_code(&self) -> Option<String> {
        self.session.lock().unwrap().pairing_code()
    }

    /// The owner compared the codes and they match. `cert_clock_ms`: epoch milliseconds.
    pub fn confirm_pairing(&self, cert_clock_ms: u64) -> Result<PhoneOut, NodeError> {
        self.session.lock().unwrap().confirm_pairing(cert_clock_ms)
    }

    pub fn reject_pairing(&self) -> Result<PhoneOut, NodeError> {
        self.session.lock().unwrap().reject_pairing()
    }

    /// Raw socket bytes in; frames to write and events out.
    pub fn on_bytes(
        &self,
        bytes: Vec<u8>,
        now_ms: u64,
        cert_clock_ms: u64,
    ) -> Result<PhoneOut, NodeError> {
        self.session
            .lock()
            .unwrap()
            .on_bytes(&bytes, now_ms, cert_clock_ms)
    }

    pub fn relay_result(&self, now_ms: u64, result: RelayResult) -> Result<PhoneOut, NodeError> {
        self.session.lock().unwrap().relay_result(now_ms, result)
    }

    pub fn tick(&self, now_ms: u64) -> Result<PhoneOut, NodeError> {
        self.session.lock().unwrap().tick(now_ms)
    }

    /// Next time `tick` is needed, if any.
    pub fn next_deadline(&self) -> Option<u64> {
        self.session.lock().unwrap().next_deadline()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn facade_loads_identity_and_starts_unauthorized() {
        let dir = tempfile::tempdir().unwrap();
        let p = PhoneLink::new(dir.path().to_string_lossy().into(), None, 1000).unwrap();
        assert_eq!(p.node_id().len(), 32);
        p.on_connect(false);
        assert!(!p.is_authorized());
        p.on_connect(true);
        assert!(p.is_authorized());
        assert!(p.next_deadline().is_none());
        assert!(PhoneLink::new(dir.path().to_string_lossy().into(), Some("zz".into()), 0).is_err());
    }
}
