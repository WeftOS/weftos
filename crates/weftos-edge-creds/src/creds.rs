//! Short-lived credentials (ADR-126 section 5).
//!
//! The edge never holds a long-lived key. [`TempCredentials`] is the only
//! credential shape the edge client accepts, and it refuses anything that
//! does not carry a session token or lives longer than [`MAX_LEASE_TTL_SECS`].

use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

/// STS temporary credentials live 15 minutes (S0 result).
pub const MAX_LEASE_TTL_SECS: u64 = 900;
/// Refresh when less than this remains.
pub const REFRESH_MARGIN_SECS: u64 = 60;
/// Tolerated clock difference between parent and edge when checking the cap.
pub const CLOCK_SKEW_SECS: u64 = 60;

/// An STS session: access key, secret, session token, absolute expiry.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TempCredentials {
    pub access_key: String,
    pub secret_key: String,
    pub session_token: String,
    /// Unix seconds.
    pub expires_at: u64,
}

impl std::fmt::Debug for TempCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TempCredentials")
            .field("access_key", &self.access_key)
            .field("secret_key", &"<redacted>")
            .field("session_token", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

impl Drop for TempCredentials {
    fn drop(&mut self) {
        self.secret_key.zeroize();
        self.session_token.zeroize();
    }
}

/// Why the edge refused a credential.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CredentialRefusal {
    #[error("credential has no session token: a long-lived key is not accepted on the edge")]
    NoSessionToken,
    #[error("credential already expired")]
    Expired,
    #[error("credential lifetime {0}s exceeds the {MAX_LEASE_TTL_SECS}s cap: not a short-lived credential")]
    TooLong(u64),
}

impl TempCredentials {
    /// Seconds left at `now`, 0 if expired.
    pub fn remaining(&self, now: u64) -> u64 {
        self.expires_at.saturating_sub(now)
    }

    /// True when it should be replaced before the next request.
    pub fn needs_refresh(&self, now: u64) -> bool {
        self.remaining(now) <= REFRESH_MARGIN_SECS
    }

    /// The edge-side admission check.
    pub fn admit(&self, now: u64) -> Result<(), CredentialRefusal> {
        if self.session_token.is_empty() {
            return Err(CredentialRefusal::NoSessionToken);
        }
        if self.expires_at <= now {
            return Err(CredentialRefusal::Expired);
        }
        let life = self.expires_at - now;
        if life > MAX_LEASE_TTL_SECS + CLOCK_SKEW_SECS {
            return Err(CredentialRefusal::TooLong(life));
        }
        Ok(())
    }
}
