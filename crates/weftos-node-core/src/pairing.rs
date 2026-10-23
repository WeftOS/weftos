//! Leaf-parent pairing state machines (design section 4.3), sans-IO.
//!
//! P0 scope: the *shape* of the flow over a fake transport. The BLE Noise XX channel the
//! messages will ride in is P1 (stubbed: the messages here are plain CBOR on the pairing
//! lane). What is real and tested: mutual identity proofs, the short authentication
//! string (SAS) the glasses speak and the user confirms on the phone, the leaf
//! certificate the phone issues, and refusal of a wrong peer.
//!
//! ```text
//! glasses                                phone (parent)
//!   Hello{pk, nonce_g, sig}  ------------>   verify sig; expected peer? else Refuse
//!   <-------------  ParentHello{pk, nonce_p, sig}
//!   both compute SAS; glasses speak it; user types it on the phone
//!                                          confirm_sas(code) == SAS ? else Refuse
//!   <-------------  Cert{leaf certificate signed by the parent}
//!   verify cert -> Paired
//! ```

use crate::error::NodeError;
use crate::identity::{Identity, verify};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const CAP_MIC: u32 = 1;
pub const CAP_SPEAKER: u32 = 2;
pub const CAP_LED: u32 = 4;
pub const CAP_CAMERA: u32 = 8;
/// Default certificate lifetime: 7 days (short expiry, renewed on every connection).
pub const CERT_TTL_MS: u64 = 7 * 24 * 3600 * 1000;

const CTX_HELLO: &[u8] = b"weft.glasses.hello.v1";
const CTX_PARENT: &[u8] = b"weft.parent.hello.v1";
const CTX_SAS: &[u8] = b"weft.pair.sas.v1";
const CTX_CERT: &[u8] = b"weft.leaf.cert.v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeafCert {
    pub glasses_pk: Vec<u8>,
    pub parent_pk: Vec<u8>,
    pub caps: u32,
    pub issued_ms: u64,
    pub expires_ms: u64,
    pub sig: Vec<u8>,
}

impl LeafCert {
    fn signed_bytes(
        glasses_pk: &[u8],
        parent_pk: &[u8],
        caps: u32,
        issued: u64,
        expires: u64,
    ) -> Vec<u8> {
        let mut v = CTX_CERT.to_vec();
        v.extend_from_slice(glasses_pk);
        v.extend_from_slice(parent_pk);
        v.extend_from_slice(&caps.to_be_bytes());
        v.extend_from_slice(&issued.to_be_bytes());
        v.extend_from_slice(&expires.to_be_bytes());
        v
    }

    /// Verify against the pinned parent key, our own key and the clock.
    pub fn verify(
        &self,
        parent_pk: &[u8; 32],
        glasses_pk: &[u8; 32],
        now_ms: u64,
    ) -> Result<(), NodeError> {
        if self.parent_pk != parent_pk || self.glasses_pk != glasses_pk {
            return Err(NodeError::Pairing(
                "certificate names different keys".into(),
            ));
        }
        if now_ms >= self.expires_ms {
            return Err(NodeError::Pairing("certificate expired".into()));
        }
        let msg = Self::signed_bytes(
            &self.glasses_pk,
            &self.parent_pk,
            self.caps,
            self.issued_ms,
            self.expires_ms,
        );
        if !verify(parent_pk, &msg, &self.sig) {
            return Err(NodeError::Pairing("certificate signature invalid".into()));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "m", rename_all = "snake_case")]
pub enum PairMsg {
    Hello {
        pk: Vec<u8>,
        nonce: Vec<u8>,
        sig: Vec<u8>,
    },
    ParentHello {
        pk: Vec<u8>,
        nonce: Vec<u8>,
        sig: Vec<u8>,
    },
    Cert {
        cert: LeafCert,
    },
    Refuse {
        reason: String,
    },
}

impl PairMsg {
    pub fn to_bytes(&self) -> Result<Vec<u8>, NodeError> {
        let mut out = Vec::new();
        ciborium::into_writer(self, &mut out).map_err(|e| NodeError::Codec(e.to_string()))?;
        Ok(out)
    }

    pub fn from_bytes(b: &[u8]) -> Result<Self, NodeError> {
        ciborium::from_reader(b).map_err(|e| NodeError::Codec(e.to_string()))
    }
}

/// Six decimal digits, the same on both ends iff both saw the same transcript.
pub fn sas(glasses_pk: &[u8], parent_pk: &[u8], nonce_g: &[u8], nonce_p: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(CTX_SAS);
    for p in [glasses_pk, parent_pk, nonce_g, nonce_p] {
        h.update((p.len() as u32).to_be_bytes());
        h.update(p);
    }
    let d = h.finalize();
    let n = u32::from_be_bytes([d[0], d[1], d[2], d[3]]) % 1_000_000;
    format!("{n:06}")
}

fn key32(b: &[u8]) -> Result<[u8; 32], NodeError> {
    b.try_into()
        .map_err(|_| NodeError::Pairing("key must be 32 bytes".into()))
}

fn hello_bytes(ctx: &[u8], pk: &[u8], nonce: &[u8], other_nonce: &[u8]) -> Vec<u8> {
    let mut v = ctx.to_vec();
    v.extend_from_slice(pk);
    v.extend_from_slice(nonce);
    v.extend_from_slice(other_nonce);
    v
}

fn fresh_nonce() -> Vec<u8> {
    use rand::RngCore;
    let mut n = vec![0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut n);
    n
}

// ---- phone (parent) side ------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum PairingState {
    Idle,
    /// Waiting for the glasses' hello.
    AwaitingPeer,
    /// Identity proven; waiting for the user to enter the code the glasses speak.
    AwaitingConfirm {
        sas: String,
    },
    Paired,
    Refused {
        reason: String,
    },
}

pub struct ParentPairing {
    id: Identity,
    expected_peer: Option<[u8; 32]>,
    caps: u32,
    state: PairingState,
    peer: Option<[u8; 32]>,
    sas: Option<String>,
    cert: Option<LeafCert>,
}

impl ParentPairing {
    /// `expected_peer`: the glasses key from the pairing beacon / USB provisioning, if known.
    pub fn new(id: Identity, expected_peer: Option<[u8; 32]>, caps: u32) -> Self {
        Self {
            id,
            expected_peer,
            caps,
            state: PairingState::AwaitingPeer,
            peer: None,
            sas: None,
            cert: None,
        }
    }

    pub fn state(&self) -> &PairingState {
        &self.state
    }

    pub fn certificate(&self) -> Option<&LeafCert> {
        self.cert.as_ref()
    }

    fn refuse(&mut self, reason: &str) -> Vec<PairMsg> {
        self.state = PairingState::Refused {
            reason: reason.into(),
        };
        vec![PairMsg::Refuse {
            reason: reason.into(),
        }]
    }

    pub fn on_message(&mut self, msg: PairMsg) -> Vec<PairMsg> {
        match (&self.state, msg) {
            (PairingState::AwaitingPeer, PairMsg::Hello { pk, nonce, sig }) => {
                let Ok(pk32) = key32(&pk) else {
                    return self.refuse("bad key");
                };
                if !verify(&pk32, &hello_bytes(CTX_HELLO, &pk, &nonce, &[]), &sig) {
                    return self.refuse("hello signature invalid");
                }
                if self.expected_peer.is_some_and(|e| e != pk32) {
                    return self.refuse("wrong peer");
                }
                let nonce_p = fresh_nonce();
                let me = self.id.public_key();
                let sig_p = self
                    .id
                    .sign(&hello_bytes(CTX_PARENT, &me, &nonce_p, &nonce));
                let code = sas(&pk, &me, &nonce, &nonce_p);
                self.peer = Some(pk32);
                self.sas = Some(code.clone());
                self.state = PairingState::AwaitingConfirm { sas: code };
                vec![PairMsg::ParentHello {
                    pk: me.to_vec(),
                    nonce: nonce_p,
                    sig: sig_p.to_vec(),
                }]
            }
            // Anything else in any other state is a protocol violation.
            (PairingState::Paired | PairingState::Refused { .. }, _) => vec![],
            (_, _) => self.refuse("unexpected message"),
        }
    }

    /// The user typed the code the glasses spoke. Issues the certificate on a match.
    pub fn confirm_sas(&mut self, entered: &str, now_ms: u64) -> Vec<PairMsg> {
        let (PairingState::AwaitingConfirm { .. }, Some(sas), Some(peer)) =
            (&self.state, &self.sas, self.peer)
        else {
            return vec![];
        };
        if entered.trim() != sas {
            return self.refuse("code mismatch");
        }
        let me = self.id.public_key();
        let expires = now_ms + CERT_TTL_MS;
        let msg = LeafCert::signed_bytes(&peer, &me, self.caps, now_ms, expires);
        let cert = LeafCert {
            glasses_pk: peer.to_vec(),
            parent_pk: me.to_vec(),
            caps: self.caps,
            issued_ms: now_ms,
            expires_ms: expires,
            sig: self.id.sign(&msg).to_vec(),
        };
        self.cert = Some(cert.clone());
        self.state = PairingState::Paired;
        vec![PairMsg::Cert { cert }]
    }
}

// ---- glasses (child) side -----------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GlassesState {
    Idle,
    /// Hello sent.
    AwaitingParent,
    /// Parent proven; speak this code and wait for the certificate.
    AwaitingCert {
        sas: String,
    },
    Paired {
        parent_pk: [u8; 32],
    },
    Refused {
        reason: String,
    },
}

pub struct GlassesPairing {
    id: Identity,
    nonce: Vec<u8>,
    parent: Option<[u8; 32]>,
    state: GlassesState,
    cert: Option<LeafCert>,
}

impl GlassesPairing {
    pub fn new(id: Identity) -> Self {
        Self {
            id,
            nonce: Vec::new(),
            parent: None,
            state: GlassesState::Idle,
            cert: None,
        }
    }

    pub fn state(&self) -> &GlassesState {
        &self.state
    }

    pub fn certificate(&self) -> Option<&LeafCert> {
        self.cert.as_ref()
    }

    pub fn start(&mut self) -> PairMsg {
        self.nonce = fresh_nonce();
        let pk = self.id.public_key();
        let sig = self.id.sign(&hello_bytes(CTX_HELLO, &pk, &self.nonce, &[]));
        self.state = GlassesState::AwaitingParent;
        PairMsg::Hello {
            pk: pk.to_vec(),
            nonce: self.nonce.clone(),
            sig: sig.to_vec(),
        }
    }

    fn refuse(&mut self, reason: &str) {
        self.state = GlassesState::Refused {
            reason: reason.into(),
        };
    }

    pub fn on_message(&mut self, msg: PairMsg, now_ms: u64) {
        match (&self.state, msg) {
            (GlassesState::AwaitingParent, PairMsg::ParentHello { pk, nonce, sig }) => {
                let Ok(pk32) = key32(&pk) else {
                    return self.refuse("bad parent key");
                };
                if !verify(
                    &pk32,
                    &hello_bytes(CTX_PARENT, &pk, &nonce, &self.nonce),
                    &sig,
                ) {
                    return self.refuse("parent signature invalid");
                }
                let code = sas(&self.id.public_key(), &pk, &self.nonce, &nonce);
                self.parent = Some(pk32);
                self.state = GlassesState::AwaitingCert { sas: code };
            }
            (GlassesState::AwaitingCert { .. }, PairMsg::Cert { cert }) => {
                let Some(parent) = self.parent else {
                    return self.refuse("no parent");
                };
                match cert.verify(&parent, &self.id.public_key(), now_ms) {
                    Ok(()) => {
                        self.cert = Some(cert);
                        self.state = GlassesState::Paired { parent_pk: parent };
                    }
                    Err(e) => self.refuse(&e.to_string()),
                }
            }
            (_, PairMsg::Refuse { reason }) => self.refuse(&format!("parent refused: {reason}")),
            (GlassesState::Paired { .. } | GlassesState::Refused { .. }, _) => {}
            (_, _) => self.refuse("unexpected message"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{FakeTransport, Inbound, Link};

    fn id(n: u8) -> Identity {
        Identity::from_seed([n; 32])
    }

    /// Run the whole exchange through a fake transport; returns the two machines.
    fn run(
        expected_peer: Option<[u8; 32]>,
        glasses: Identity,
        enter: impl Fn(&str) -> String,
    ) -> (ParentPairing, GlassesPairing) {
        let (gt, pt) = FakeTransport::pair();
        let (mut gl, mut pl) = (Link::new(gt), Link::new(pt));
        let mut g = GlassesPairing::new(glasses);
        let mut p = ParentPairing::new(id(1), expected_peer, CAP_MIC | CAP_SPEAKER | CAP_LED);
        let pump = |l: &mut Link<FakeTransport>| -> Vec<PairMsg> {
            let mut v = vec![];
            while let Some(Inbound::Pair(b)) = l.recv().unwrap() {
                v.push(PairMsg::from_bytes(&b).unwrap());
            }
            v
        };
        gl.send_pair(&g.start().to_bytes().unwrap()).unwrap();
        for m in pump(&mut pl) {
            for out in p.on_message(m) {
                pl.send_pair(&out.to_bytes().unwrap()).unwrap();
            }
        }
        for m in pump(&mut gl) {
            g.on_message(m, 1_000);
        }
        if let GlassesState::AwaitingCert { sas } = g.state().clone() {
            for out in p.confirm_sas(&enter(&sas), 1_000) {
                pl.send_pair(&out.to_bytes().unwrap()).unwrap();
            }
            for m in pump(&mut gl) {
                g.on_message(m, 1_000);
            }
        } else if let PairingState::Refused { .. } = p.state() {
            for m in pump(&mut gl) {
                g.on_message(m, 1_000);
            }
        }
        (p, g)
    }

    #[test]
    fn happy_path_pairs_and_issues_certificate() {
        let glasses = id(2);
        let gpk = glasses.public_key();
        let (p, g) = run(Some(gpk), glasses, |s| s.to_string());
        assert_eq!(*p.state(), PairingState::Paired);
        assert!(
            matches!(g.state(), GlassesState::Paired { parent_pk } if *parent_pk == id(1).public_key())
        );
        let cert = g.certificate().unwrap();
        assert_eq!(cert.caps, CAP_MIC | CAP_SPEAKER | CAP_LED);
        assert!(cert.verify(&id(1).public_key(), &gpk, 2_000).is_ok());
        // short expiry: stale after the TTL
        assert!(
            cert.verify(&id(1).public_key(), &gpk, 1_000 + CERT_TTL_MS)
                .is_err()
        );
    }

    #[test]
    fn happy_path_without_a_known_peer_still_needs_the_code() {
        let (p, g) = run(None, id(2), |s| s.to_string());
        assert_eq!(*p.state(), PairingState::Paired);
        assert!(matches!(g.state(), GlassesState::Paired { .. }));
    }

    #[test]
    fn wrong_peer_is_refused_and_gets_no_certificate() {
        // The beacon said glasses key 2; a different device (key 9) answers.
        let expected = id(2).public_key();
        let (p, g) = run(Some(expected), id(9), |s| s.to_string());
        assert_eq!(
            *p.state(),
            PairingState::Refused {
                reason: "wrong peer".into()
            }
        );
        assert!(p.certificate().is_none());
        assert!(matches!(g.state(), GlassesState::Refused { .. }));
        assert!(g.certificate().is_none());
    }

    #[test]
    fn wrong_code_is_refused() {
        let (p, g) = run(None, id(2), |_| "000000x".into());
        assert_eq!(
            *p.state(),
            PairingState::Refused {
                reason: "code mismatch".into()
            }
        );
        assert!(g.certificate().is_none());
    }

    #[test]
    fn forged_hello_signature_is_refused() {
        let mut g = GlassesPairing::new(id(2));
        let PairMsg::Hello { pk, nonce, mut sig } = g.start() else {
            unreachable!()
        };
        sig[0] ^= 1;
        let mut p = ParentPairing::new(id(1), None, CAP_MIC);
        let out = p.on_message(PairMsg::Hello { pk, nonce, sig });
        assert!(matches!(out[0], PairMsg::Refuse { .. }));
        assert!(matches!(p.state(), PairingState::Refused { .. }));
    }

    #[test]
    fn certificate_from_a_different_parent_is_refused_by_glasses() {
        let mut g = GlassesPairing::new(id(2));
        let mut p = ParentPairing::new(id(1), None, CAP_MIC);
        let hello = g.start();
        let ph = p.on_message(hello).remove(0);
        g.on_message(ph, 1);
        // an impostor with another key signs a certificate for these glasses
        let mut evil = ParentPairing::new(id(7), None, CAP_MIC);
        evil.on_message(PairMsg::Hello {
            pk: id(2).public_key().to_vec(),
            nonce: vec![0; 16],
            sig: id(2)
                .sign(&hello_bytes(CTX_HELLO, &id(2).public_key(), &[0; 16], &[]))
                .to_vec(),
        });
        let sas = match evil.state() {
            PairingState::AwaitingConfirm { sas } => sas.clone(),
            _ => panic!(),
        };
        let PairMsg::Cert { cert } = evil.confirm_sas(&sas, 1).remove(0) else {
            panic!()
        };
        g.on_message(PairMsg::Cert { cert }, 1);
        assert!(matches!(g.state(), GlassesState::Refused { .. }));
        assert!(g.certificate().is_none());
    }

    #[test]
    fn out_of_order_message_is_a_violation() {
        let mut p = ParentPairing::new(id(1), None, CAP_MIC);
        let out = p.on_message(PairMsg::Refuse { reason: "x".into() });
        assert!(matches!(out[0], PairMsg::Refuse { .. }));
    }
}
