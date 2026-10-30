//! Versioned Ed25519 + ML-DSA-65 dual signatures.
//!
//! A [`DualSignature`] carries a [`SigSuite`] tag:
//!
//! - [`SigSuite::LegacyPlaceholderV1`]: written by nodes running the
//!   `weftos-rvf-crypto` fork. The second half is an HMAC-SHAKE-256 value,
//!   not a signature. It is never checked and never counts as post-quantum;
//!   only the Ed25519 half decides validity.
//! - [`SigSuite::Ed25519MlDsa65V2`]: the second half, when present, is a
//!   real ML-DSA-65 signature over the same message under a FIPS 204
//!   context string.
//!
//! The serde form is the old kernel `DualSignature` (`ed25519`, `ml_dsa65`)
//! plus a `suite` field that defaults to the legacy suite, so values
//! written by old nodes deserialize as legacy and old nodes ignore the new
//! field.

use ed25519_dalek::Signer;
use serde::{Deserialize, Serialize};

use crate::mldsa65;
use crate::{Error, Result};

/// The ML-DSA-65 context for general-purpose dual signatures.
pub const CONTEXT_DUAL_V2: &[u8] = b"weftos/dual-sig/ed25519+ml-dsa-65/v2";

/// Length of the legacy placeholder's second half.
pub const LEGACY_PLACEHOLDER_LEN: usize = 3309;

const MAGIC: &[u8; 3] = b"WDS";

/// Which algorithms produced a dual signature.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SigSuite {
    /// Ed25519 + the HMAC placeholder from `weftos-rvf-crypto` (pre-ADR-129).
    #[default]
    #[serde(rename = "ed25519+hmac-placeholder/v1")]
    LegacyPlaceholderV1,
    /// Ed25519 + real ML-DSA-65 (ADR-129).
    #[serde(rename = "ed25519+ml-dsa-65/v2")]
    Ed25519MlDsa65V2,
}

impl SigSuite {
    /// One-byte tag for the binary encoding.
    pub const fn tag(self) -> u8 {
        match self {
            Self::LegacyPlaceholderV1 => 1,
            Self::Ed25519MlDsa65V2 => 2,
        }
    }

    /// Parse a binary tag.
    pub fn from_tag(tag: u8) -> Result<Self> {
        match tag {
            1 => Ok(Self::LegacyPlaceholderV1),
            2 => Ok(Self::Ed25519MlDsa65V2),
            _ => Err(Error::Malformed("dual signature suite tag")),
        }
    }

    /// The stable text name, as used in serde and in RVF checkpoints.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LegacyPlaceholderV1 => "ed25519+hmac-placeholder/v1",
            Self::Ed25519MlDsa65V2 => "ed25519+ml-dsa-65/v2",
        }
    }

    /// Parse the text name.
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "ed25519+hmac-placeholder/v1" => Ok(Self::LegacyPlaceholderV1),
            "ed25519+ml-dsa-65/v2" => Ok(Self::Ed25519MlDsa65V2),
            _ => Err(Error::Malformed("dual signature suite name")),
        }
    }
}

/// What happened to the post-quantum half of a valid signature.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PqStatus {
    /// A real ML-DSA-65 signature verified.
    Verified,
    /// The second half is the legacy HMAC placeholder; it was not checked
    /// and provides no post-quantum assurance.
    LegacyPlaceholder,
    /// A real ML-DSA-65 signature is present but no verifying key was given.
    NotChecked,
    /// The signer produced no post-quantum half.
    Absent,
}

/// Why a dual signature was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RejectReason {
    /// The Ed25519 half is malformed or did not verify.
    Ed25519,
    /// The ML-DSA-65 half is malformed or did not verify.
    MlDsa65,
    /// A legacy or Ed25519-only signature where the signer had already cut
    /// over to ML-DSA-65 (ADR-129 downgrade rule).
    Downgrade,
}

/// The outcome of verifying a dual signature.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Ed25519 verified; the status says what is known about the PQ half.
    Valid(PqStatus),
    /// The signature must not be accepted.
    Rejected(RejectReason),
}

impl Verdict {
    /// Ed25519 verified and no present half failed.
    pub fn is_valid(self) -> bool {
        matches!(self, Self::Valid(_))
    }

    /// A real ML-DSA-65 signature verified, in addition to Ed25519.
    pub fn is_post_quantum(self) -> bool {
        matches!(self, Self::Valid(PqStatus::Verified))
    }

    /// Valid, with a legacy placeholder second half.
    pub fn is_legacy(self) -> bool {
        matches!(self, Self::Valid(PqStatus::LegacyPlaceholder))
    }
}

/// An Ed25519 signature plus an optional post-quantum half.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DualSignature {
    /// Ed25519 signature (64 bytes).
    pub ed25519: Vec<u8>,
    /// Second half: real ML-DSA-65 for v2, the HMAC placeholder for v1.
    pub ml_dsa65: Option<Vec<u8>>,
    /// Which suite produced this signature. Missing means legacy.
    #[serde(default)]
    pub suite: SigSuite,
}

impl DualSignature {
    /// Sign `msg` with Ed25519 and, when `ml` is given, ML-DSA-65 under `ctx`.
    pub fn sign(
        msg: &[u8],
        ctx: &[u8],
        ed: &ed25519_dalek::SigningKey,
        ml: Option<&mldsa65::SigningKey>,
    ) -> Result<Self> {
        let ml_dsa65 = match ml {
            Some(k) => Some(k.sign(msg, ctx)?.as_bytes().to_vec()),
            None => None,
        };
        Ok(Self {
            ed25519: ed.sign(msg).to_bytes().to_vec(),
            ml_dsa65,
            suite: SigSuite::Ed25519MlDsa65V2,
        })
    }

    /// Verify against `msg`. Ed25519 is mandatory and strict. The ML-DSA-65
    /// half is checked only for the v2 suite and only when `ml` is given.
    pub fn verify(
        &self,
        msg: &[u8],
        ctx: &[u8],
        ed: &ed25519_dalek::VerifyingKey,
        ml: Option<&mldsa65::VerifyingKey>,
    ) -> Verdict {
        let Ok(ed_sig) = ed25519_dalek::Signature::from_slice(&self.ed25519) else {
            return Verdict::Rejected(RejectReason::Ed25519);
        };
        if ed.verify_strict(msg, &ed_sig).is_err() {
            return Verdict::Rejected(RejectReason::Ed25519);
        }
        let Some(pq) = &self.ml_dsa65 else {
            return Verdict::Valid(PqStatus::Absent);
        };
        match self.suite {
            SigSuite::LegacyPlaceholderV1 => Verdict::Valid(PqStatus::LegacyPlaceholder),
            SigSuite::Ed25519MlDsa65V2 => {
                let Ok(sig) = mldsa65::Signature::from_bytes(pq) else {
                    return Verdict::Rejected(RejectReason::MlDsa65);
                };
                match ml {
                    None => Verdict::Valid(PqStatus::NotChecked),
                    Some(vk) if vk.verify(msg, ctx, &sig).is_ok() => {
                        Verdict::Valid(PqStatus::Verified)
                    }
                    Some(_) => Verdict::Rejected(RejectReason::MlDsa65),
                }
            }
        }
    }

    /// Binary form: `"WDS" | suite tag | ed25519[64] | pq_len u16 LE | pq`.
    pub fn to_bytes(&self) -> Vec<u8> {
        let pq = self.ml_dsa65.as_deref().unwrap_or(&[]);
        let mut out = Vec::with_capacity(3 + 1 + 64 + 2 + pq.len());
        out.extend_from_slice(MAGIC);
        out.push(self.suite.tag());
        out.extend_from_slice(&self.ed25519);
        out.extend_from_slice(&(pq.len() as u16).to_le_bytes());
        out.extend_from_slice(pq);
        out
    }

    /// Parse the binary form, checking magic, tag and every length.
    pub fn from_bytes(b: &[u8]) -> Result<Self> {
        if b.len() < 3 + 1 + 64 + 2 || &b[..3] != MAGIC {
            return Err(Error::Malformed("dual signature"));
        }
        let suite = SigSuite::from_tag(b[3])?;
        let ed25519 = b[4..68].to_vec();
        let pq_len = u16::from_le_bytes([b[68], b[69]]) as usize;
        let rest = &b[70..];
        if rest.len() != pq_len {
            return Err(Error::Malformed("dual signature length"));
        }
        let ml_dsa65 = match pq_len {
            0 => None,
            n if n == mldsa65::SIGNATURE_LEN || n == LEGACY_PLACEHOLDER_LEN => Some(rest.to_vec()),
            _ => return Err(Error::Malformed("dual signature PQ half length")),
        };
        Ok(Self { ed25519, ml_dsa65, suite })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::OsRng;

    fn keys() -> (ed25519_dalek::SigningKey, mldsa65::SigningKey) {
        (ed25519_dalek::SigningKey::generate(&mut OsRng), mldsa65::SigningKey::generate())
    }

    #[test]
    fn v2_round_trip_is_post_quantum() {
        let (ed, ml) = keys();
        let sig = DualSignature::sign(b"m", CONTEXT_DUAL_V2, &ed, Some(&ml)).unwrap();
        assert_eq!(sig.suite, SigSuite::Ed25519MlDsa65V2);
        let v = sig.verify(b"m", CONTEXT_DUAL_V2, &ed.verifying_key(), Some(ml.verifying_key()));
        assert!(v.is_post_quantum());
        let none = sig.verify(b"m", CONTEXT_DUAL_V2, &ed.verifying_key(), None);
        assert_eq!(none, Verdict::Valid(PqStatus::NotChecked));
    }

    #[test]
    fn tamper_rejects_each_half() {
        let (ed, ml) = keys();
        let (edv, mlv) = (ed.verifying_key(), ml.verifying_key().clone());
        let sig = DualSignature::sign(b"m", CONTEXT_DUAL_V2, &ed, Some(&ml)).unwrap();
        assert_eq!(
            sig.verify(b"x", CONTEXT_DUAL_V2, &edv, Some(&mlv)),
            Verdict::Rejected(RejectReason::Ed25519)
        );
        let mut bad = sig.clone();
        bad.ml_dsa65.as_mut().unwrap()[7] ^= 0x80;
        assert_eq!(
            bad.verify(b"m", CONTEXT_DUAL_V2, &edv, Some(&mlv)),
            Verdict::Rejected(RejectReason::MlDsa65)
        );
        assert_eq!(
            sig.verify(b"m", b"other-context", &edv, Some(&mlv)),
            Verdict::Rejected(RejectReason::MlDsa65)
        );
    }

    #[test]
    fn legacy_is_valid_on_ed25519_but_never_post_quantum() {
        let (ed, ml) = keys();
        let legacy = DualSignature {
            ed25519: ed.sign(b"m").to_bytes().to_vec(),
            ml_dsa65: Some(vec![0xAB; LEGACY_PLACEHOLDER_LEN]),
            suite: SigSuite::LegacyPlaceholderV1,
        };
        let v = legacy.verify(b"m", CONTEXT_DUAL_V2, &ed.verifying_key(), Some(ml.verifying_key()));
        assert_eq!(v, Verdict::Valid(PqStatus::LegacyPlaceholder));
        assert!(v.is_valid() && v.is_legacy() && !v.is_post_quantum());
        // Ed25519 is still strict for legacy signatures.
        let v = legacy.verify(b"x", CONTEXT_DUAL_V2, &ed.verifying_key(), None);
        assert_eq!(v, Verdict::Rejected(RejectReason::Ed25519));
    }

    #[test]
    fn old_serde_shape_deserializes_as_legacy() {
        let old = r#"{"ed25519":[1,2],"ml_dsa65":null}"#;
        let s: DualSignature = serde_json::from_str(old).unwrap();
        assert_eq!(s.suite, SigSuite::LegacyPlaceholderV1);
        let new = serde_json::to_value(DualSignature {
            ed25519: vec![],
            ml_dsa65: None,
            suite: SigSuite::Ed25519MlDsa65V2,
        })
        .unwrap();
        assert_eq!(new["suite"], "ed25519+ml-dsa-65/v2");
    }

    #[test]
    fn binary_round_trip_and_rejections() {
        let (ed, ml) = keys();
        let sig = DualSignature::sign(b"m", CONTEXT_DUAL_V2, &ed, Some(&ml)).unwrap();
        let b = sig.to_bytes();
        assert_eq!(DualSignature::from_bytes(&b).unwrap(), sig);
        let mut bad_tag = b.clone();
        bad_tag[3] = 9;
        assert!(DualSignature::from_bytes(&bad_tag).is_err());
        assert!(DualSignature::from_bytes(&b[..b.len() - 1]).is_err());
        assert!(DualSignature::from_bytes(b"WDX").is_err());
    }

    #[test]
    fn suite_names_round_trip() {
        for s in [SigSuite::LegacyPlaceholderV1, SigSuite::Ed25519MlDsa65V2] {
            assert_eq!(SigSuite::parse(s.as_str()).unwrap(), s);
            assert_eq!(SigSuite::from_tag(s.tag()).unwrap(), s);
        }
    }
}
