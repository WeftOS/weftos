//! `weftos.cog.v1` contracts between Cog Host and the local user or project daemon.
//!
//! This crate has no `clawft-kernel`, `clawft-types`, or `clawft-rpc` dependency.
//! [`client::DaemonCheck`] speaks newline-delimited JSON on the daemon's Unix socket.
//! Golden vectors under `testdata/` lock the request, verdict, refusal, and
//! host-event bytes.

mod wire;
pub use wire::*;

#[cfg(unix)]
mod client;
#[cfg(unix)]
pub use client::DaemonCheck;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Protocol name carried by these types.
pub const PROTOCOL: &str = "weftos.cog.v1";

/// Daemon method whose `params` are [`CheckRunParams`].
pub const CHECK_RUN_METHOD: &str = "cog.check_run";

/// A start-time licence check for the bytes that are about to run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckRunParams {
    pub cog_id: String,
    pub version: String,
    /// Lower-case hex sha256 of the binary.
    pub sha256: String,
    /// Lower-case hex BLAKE3 of the binary.
    pub blake3: String,
}

/// Why [`CheckRunParams`] was rejected before it was sent.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ParamsError {
    #[error("cog id is empty or longer than 128 bytes")]
    CogId,
    #[error("version is empty or longer than 64 bytes")]
    Version,
    #[error("sha256 must be 64 lower-case hex characters")]
    Sha256,
    #[error("blake3 must be 64 lower-case hex characters")]
    Blake3,
}

impl CheckRunParams {
    /// Build params from the four fields the kernel gate already requires.
    pub fn new(
        cog_id: impl Into<String>,
        version: impl Into<String>,
        sha256: impl Into<String>,
        blake3: impl Into<String>,
    ) -> Result<Self, ParamsError> {
        let cog_id = cog_id.into();
        let version = version.into();
        let sha256 = sha256.into();
        let blake3 = blake3.into();
        if cog_id.is_empty() || cog_id.len() > 128 || cog_id.chars().any(char::is_whitespace) {
            return Err(ParamsError::CogId);
        }
        if version.is_empty() || version.len() > 64 || version.chars().any(char::is_whitespace) {
            return Err(ParamsError::Version);
        }
        if !is_hex64(&sha256) {
            return Err(ParamsError::Sha256);
        }
        if !is_hex64(&blake3) {
            return Err(ParamsError::Blake3);
        }
        Ok(Self { cog_id, version, sha256, blake3 })
    }
}

fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// What the daemon returns when the check applies and passes, or does not apply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum CheckRunResult {
    /// The node holds no Seed binding. The gate does not apply.
    NotSeedBound,
    /// Grant and approval cover the binary.
    Permit {
        grant_id: String,
        approval_id: String,
        blake3: String,
    },
}

/// Stable refusal codes. These spellings match `RunRefusal::code` in the kernel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefusalCode {
    BindingInactive,
    NoGrant,
    GrantLapsed,
    NotInGrant,
    HashRevoked,
    NoApproval,
    NotHolder,
    /// The daemon socket did not answer. Cog Host fails closed.
    DaemonUnavailable,
}

impl std::fmt::Display for RefusalCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl RefusalCode {
    /// The wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BindingInactive => "binding_inactive",
            Self::NoGrant => "no_grant",
            Self::GrantLapsed => "grant_lapsed",
            Self::NotInGrant => "not_in_grant",
            Self::HashRevoked => "hash_revoked",
            Self::NoApproval => "no_approval",
            Self::NotHolder => "not_holder",
            Self::DaemonUnavailable => "daemon_unavailable",
        }
    }

    /// Parse a wire spelling. Unknown spellings are refused.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "binding_inactive" => Self::BindingInactive,
            "no_grant" => Self::NoGrant,
            "grant_lapsed" => Self::GrantLapsed,
            "not_in_grant" => Self::NotInGrant,
            "hash_revoked" => Self::HashRevoked,
            "no_approval" => Self::NoApproval,
            "not_holder" => Self::NotHolder,
            "daemon_unavailable" => Self::DaemonUnavailable,
            _ => return None,
        })
    }
}

/// One host event after a check. The daemon does not emit these yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum HostEvent {
    RunPermitted {
        cog_id: String,
        version: String,
        grant_id: String,
    },
    RunRefused {
        cog_id: String,
        version: String,
        code: RefusalCode,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> CheckRunParams {
        CheckRunParams::new(
            "ld2450-radar",
            "0.1.0",
            "ab".repeat(32),
            "cd".repeat(32),
        )
        .unwrap()
    }

    #[test]
    fn protocol_name_and_method_are_fixed() {
        assert_eq!(PROTOCOL, "weftos.cog.v1");
        assert_eq!(CHECK_RUN_METHOD, "cog.check_run");
    }

    #[test]
    fn params_reject_empty_ids_and_upper_case_hashes() {
        let ok = sample();
        assert!(CheckRunParams::new("", ok.version.clone(), ok.sha256.clone(), ok.blake3.clone()).is_err());
        assert!(CheckRunParams::new(ok.cog_id.clone(), " ", ok.sha256.clone(), ok.blake3.clone()).is_err());
        let upper = ok.sha256.to_ascii_uppercase();
        assert!(CheckRunParams::new(ok.cog_id.clone(), ok.version.clone(), upper, ok.blake3).is_err());
    }

    #[test]
    fn refusal_codes_round_trip_in_kernel_order() {
        let codes = [
            RefusalCode::BindingInactive,
            RefusalCode::NoGrant,
            RefusalCode::GrantLapsed,
            RefusalCode::NotInGrant,
            RefusalCode::HashRevoked,
            RefusalCode::NoApproval,
            RefusalCode::NotHolder,
            RefusalCode::DaemonUnavailable,
        ];
        let spellings = [
            "binding_inactive",
            "no_grant",
            "grant_lapsed",
            "not_in_grant",
            "hash_revoked",
            "no_approval",
            "not_holder",
            "daemon_unavailable",
        ];
        for (code, spelling) in codes.into_iter().zip(spellings) {
            assert_eq!(code.as_str(), spelling);
            assert_eq!(RefusalCode::parse(spelling), Some(code));
            let json = serde_json::to_string(&code).unwrap();
            assert_eq!(json, format!("\"{spelling}\""));
            assert_eq!(serde_json::from_str::<RefusalCode>(&json).unwrap(), code);
        }
        assert_eq!(RefusalCode::parse("no_valid_grant"), None);
    }

    #[test]
    fn permit_and_refusal_events_keep_their_tags() {
        let permit = CheckRunResult::Permit {
            grant_id: "g1".into(),
            approval_id: "a1".into(),
            blake3: "cd".repeat(32),
        };
        let json = serde_json::to_string(&permit).unwrap();
        assert!(json.contains("\"verdict\":\"permit\""));
        assert_eq!(serde_json::from_str::<CheckRunResult>(&json).unwrap(), permit);

        let event = HostEvent::RunRefused {
            cog_id: "ld2450-radar".into(),
            version: "0.1.0".into(),
            code: RefusalCode::HashRevoked,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"event\":\"run_refused\""));
        assert!(json.contains("\"code\":\"hash_revoked\""));
        assert_eq!(serde_json::from_str::<HostEvent>(&json).unwrap(), event);
    }
}
