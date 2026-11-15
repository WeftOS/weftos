//! The hub pass key: a small mint-and-verify scheme.
//!
//! Why this exists: WeftOS's existing token machinery (`SessionTokenStore`,
//! `DaemonTokenValidator`, ADR-075/076 capability tokens) validates against
//! state inside a running daemon. A standalone hub that must not touch the
//! daemon cannot use it, so the hub verifies a self-contained signed token
//! against a per-minter secret instead.
//!
//! Wire form (sent as `Authorization: Bearer <token>`):
//!
//! ```text
//! wfhub1.<base64url(claims JSON)>.<base64url(HMAC-SHA256(key, "wfhub1." + base64url(claims JSON)))>
//! ```
//!
//! The claims name the minter (`mnt`). The hub looks the key up by that id,
//! checks the MAC in constant time, then checks version, lifetime, revocation,
//! and the requested role. `host_kind` is **never** read from the token: the
//! hub derives it from the minter's configured entry.

use std::collections::HashMap;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::config::Minter;

pub const PREFIX: &str = "wfhub1";
const CLAIMS_VERSION: u8 = 1;
/// Clock skew tolerated on `iat`.
const SKEW_SECS: u64 = 60;
/// Longest token accepted from the wire, bytes.
pub const MAX_TOKEN_BYTES: usize = 4096;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// May connect as a `tool_server` and register a machine.
    Server,
    /// May connect as a `harness`.
    Harness,
}

impl Role {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "server" | "tool_server" => Some(Self::Server),
            "harness" => Some(Self::Harness),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claims {
    pub v: u8,
    /// Minter id.
    pub mnt: String,
    /// Subject: becomes the hub `user_id`. `servers.list` is scoped by it.
    pub sub: String,
    pub roles: Vec<Role>,
    pub iat: u64,
    pub exp: u64,
    pub jti: String,
    /// When set, the token only serves this machine id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub srv: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PassKeyError {
    Malformed(&'static str),
    UnknownMinter,
    BadSignature,
    UnsupportedVersion,
    Expired,
    NotYetValid,
    TtlTooLong,
    Revoked,
    InvalidSubject,
}

impl std::fmt::Display for PassKeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(why) => write!(f, "malformed pass key: {why}"),
            Self::UnknownMinter => f.write_str("pass key names an unknown minter"),
            Self::BadSignature => f.write_str("pass key signature does not verify"),
            Self::UnsupportedVersion => f.write_str("unsupported pass key version"),
            Self::Expired => f.write_str("pass key expired"),
            Self::NotYetValid => f.write_str("pass key not yet valid"),
            Self::TtlTooLong => f.write_str("pass key lifetime exceeds the hub maximum"),
            Self::Revoked => f.write_str("pass key revoked"),
            Self::InvalidSubject => f.write_str("pass key subject is empty"),
        }
    }
}
impl std::error::Error for PassKeyError {}

fn mac_over(key: &[u8], signed: &str) -> HmacSha256 {
    // HMAC accepts any key length, so this cannot fail.
    let mut mac = HmacSha256::new_from_slice(key).expect("hmac accepts any key length");
    mac.update(signed.as_bytes());
    mac
}

/// Mint a token. Refuses an empty subject, no roles, or `exp <= iat`.
pub fn mint(
    minter: &Minter,
    sub: &str,
    roles: &[Role],
    ttl_secs: u64,
    srv: Option<&str>,
    now: u64,
) -> Result<String, PassKeyError> {
    if sub.is_empty() {
        return Err(PassKeyError::InvalidSubject);
    }
    if roles.is_empty() {
        return Err(PassKeyError::Malformed("at least one role is required"));
    }
    if ttl_secs == 0 {
        return Err(PassKeyError::TtlTooLong);
    }
    let claims = Claims {
        v: CLAIMS_VERSION,
        mnt: minter.id.clone(),
        sub: sub.to_owned(),
        roles: roles.to_vec(),
        iat: now,
        exp: now.saturating_add(ttl_secs),
        jti: uuid::Uuid::new_v4().to_string(),
        srv: srv.map(str::to_owned),
    };
    let json = serde_json::to_vec(&claims).map_err(|_| PassKeyError::Malformed("claims encode"))?;
    let signed = format!("{PREFIX}.{}", B64.encode(json));
    let tag = mac_over(&minter.key, &signed).finalize().into_bytes();
    Ok(format!("{signed}.{}", B64.encode(tag)))
}

/// Verify a token against the trusted minters. Returns the claims and the
/// minter that signed them.
pub fn verify<'a>(
    token: &str,
    minters: &'a HashMap<String, Minter>,
    revoked_jti: &[String],
    max_ttl_secs: u64,
    now: u64,
) -> Result<(Claims, &'a Minter), PassKeyError> {
    if token.len() > MAX_TOKEN_BYTES {
        return Err(PassKeyError::Malformed("too long"));
    }
    let mut parts = token.split('.');
    let (Some(prefix), Some(payload), Some(tag), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(PassKeyError::Malformed(
            "expected three dot-separated parts",
        ));
    };
    if prefix != PREFIX {
        return Err(PassKeyError::Malformed("wrong prefix"));
    }
    let json = B64
        .decode(payload)
        .map_err(|_| PassKeyError::Malformed("payload base64"))?;
    let tag = B64
        .decode(tag)
        .map_err(|_| PassKeyError::Malformed("signature base64"))?;
    // The claims are untrusted until the MAC checks; `mnt` is only a key lookup.
    let claims: Claims =
        serde_json::from_slice(&json).map_err(|_| PassKeyError::Malformed("claims json"))?;
    let minter = minters
        .get(&claims.mnt)
        .ok_or(PassKeyError::UnknownMinter)?;
    let signed = format!("{PREFIX}.{payload}");
    mac_over(&minter.key, &signed)
        .verify_slice(&tag)
        .map_err(|_| PassKeyError::BadSignature)?;
    if claims.v != CLAIMS_VERSION {
        return Err(PassKeyError::UnsupportedVersion);
    }
    if claims.sub.is_empty() {
        return Err(PassKeyError::InvalidSubject);
    }
    if now >= claims.exp {
        return Err(PassKeyError::Expired);
    }
    if claims.iat > now.saturating_add(SKEW_SECS) {
        return Err(PassKeyError::NotYetValid);
    }
    if claims.exp.saturating_sub(claims.iat) > max_ttl_secs {
        return Err(PassKeyError::TtlTooLong);
    }
    if revoked_jti.iter().any(|j| j == &claims.jti) {
        return Err(PassKeyError::Revoked);
    }
    Ok((claims, minter))
}

#[cfg(test)]
mod tests {
    use super::*;
    use xai_tool_protocol::HostKind;

    fn minter(id: &str, byte: u8) -> Minter {
        Minter {
            id: id.into(),
            host_kind: HostKind::Desktop,
            key: vec![byte; 32],
        }
    }
    fn table(ms: &[Minter]) -> HashMap<String, Minter> {
        ms.iter().map(|m| (m.id.clone(), m.clone())).collect()
    }
    const NOW: u64 = 1_000_000;

    #[test]
    fn round_trip() {
        let m = minter("a", 1);
        let t = mint(&m, "mathew", &[Role::Server], 600, Some("xpc"), NOW).unwrap();
        let tab = table(&[m]);
        let (c, signer) = verify(&t, &tab, &[], 3600, NOW + 1).unwrap();
        assert_eq!(
            (c.sub.as_str(), c.srv.as_deref(), signer.id.as_str()),
            ("mathew", Some("xpc"), "a")
        );
        assert_eq!(c.roles, vec![Role::Server]);
    }

    #[test]
    fn tampered_payload_and_wrong_key_are_refused() {
        let a = minter("a", 1);
        let t = mint(&a, "u", &[Role::Harness], 600, None, NOW).unwrap();
        let tab = table(std::slice::from_ref(&a));
        // Swap payload for a different subject, keep the old tag.
        let forged = mint(&a, "root", &[Role::Harness], 600, None, NOW).unwrap();
        let mut p: Vec<&str> = t.split('.').collect();
        let fp: Vec<&str> = forged.split('.').collect();
        p[1] = fp[1];
        assert_eq!(
            verify(&p.join("."), &tab, &[], 3600, NOW).unwrap_err(),
            PassKeyError::BadSignature
        );
        // Same minter id, different key.
        let evil = minter("a", 9);
        let t2 = mint(&evil, "u", &[Role::Harness], 600, None, NOW).unwrap();
        assert_eq!(
            verify(&t2, &tab, &[], 3600, NOW).unwrap_err(),
            PassKeyError::BadSignature
        );
    }

    #[test]
    fn unknown_minter_expiry_ttl_revocation_and_garbage_are_refused() {
        let a = minter("a", 1);
        let tab = table(std::slice::from_ref(&a));
        let other = mint(&minter("zzz", 1), "u", &[Role::Server], 60, None, NOW).unwrap();
        assert_eq!(
            verify(&other, &tab, &[], 3600, NOW).unwrap_err(),
            PassKeyError::UnknownMinter
        );
        let t = mint(&a, "u", &[Role::Server], 60, None, NOW).unwrap();
        assert_eq!(
            verify(&t, &tab, &[], 3600, NOW + 60).unwrap_err(),
            PassKeyError::Expired
        );
        assert_eq!(
            verify(&t, &tab, &[], 30, NOW).unwrap_err(),
            PassKeyError::TtlTooLong
        );
        assert_eq!(
            verify(&t, &tab, &[], 3600, NOW - 1000).unwrap_err(),
            PassKeyError::NotYetValid
        );
        let (c, _) = verify(&t, &tab, &[], 3600, NOW).unwrap();
        assert_eq!(
            verify(&t, &tab, &[c.jti], 3600, NOW).unwrap_err(),
            PassKeyError::Revoked
        );
        for bad in [
            "",
            "x",
            "a.b",
            "wfhub1.a.b.c",
            "wfhub1.!!.!!",
            "wfhub2.e30.AA",
        ] {
            assert!(
                matches!(
                    verify(bad, &tab, &[], 3600, NOW),
                    Err(PassKeyError::Malformed(_))
                ),
                "{bad}"
            );
        }
    }

    #[test]
    fn mint_refuses_empty_subject_roles_and_zero_ttl() {
        let a = minter("a", 1);
        assert_eq!(
            mint(&a, "", &[Role::Server], 60, None, NOW).unwrap_err(),
            PassKeyError::InvalidSubject
        );
        assert!(matches!(
            mint(&a, "u", &[], 60, None, NOW),
            Err(PassKeyError::Malformed(_))
        ));
        assert_eq!(
            mint(&a, "u", &[Role::Server], 0, None, NOW).unwrap_err(),
            PassKeyError::TtlTooLong
        );
    }

    #[test]
    fn role_parse_accepts_server_alias_and_refuses_others() {
        assert_eq!(Role::parse("server"), Some(Role::Server));
        assert_eq!(Role::parse("tool_server"), Some(Role::Server));
        assert_eq!(Role::parse("harness"), Some(Role::Harness));
        assert_eq!(Role::parse("bot_client"), None);
        assert_eq!(Role::parse(""), None);
    }
}
