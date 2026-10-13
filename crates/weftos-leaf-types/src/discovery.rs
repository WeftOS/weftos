//! Leaf parent-discovery targets: an optional unicast hint, then broadcast.
//!
//! Some access points drop broadcast between clients, so a leaf can be built
//! with the parent's discovery endpoint (`ip:port`). The probe goes to the hint
//! first and only then falls back to broadcast. Whatever answers is still
//! verified (machine signature, nonce, scope) by the caller; the hint only
//! chooses where the probe is sent.

use alloc::vec::Vec;
use core::net::{IpAddr, Ipv4Addr, SocketAddr};

/// Discovery port the parent serves by default (main mesh port + 1).
pub const DISCOVERY_PORT: u16 = 9490;
/// Probes sent to the hint before the plan falls back to broadcast.
pub const HINT_ATTEMPTS: usize = 3;

/// Why a parent hint was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HintError {
    /// Not `ip:port` (a bare IP is refused so the port is never guessed).
    NotSocketAddr,
    /// Port 0.
    ZeroPort,
    /// Unspecified, multicast or broadcast address: not a parent's address.
    NotUnicast,
}

impl core::fmt::Display for HintError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            HintError::NotSocketAddr => "parent hint must be ip:port (example 192.168.10.100:9490)",
            HintError::ZeroPort => "parent hint port must not be 0",
            HintError::NotUnicast => "parent hint must be a unicast address",
        })
    }
}

/// Parse a parent hint. Empty (after trimming) means no hint, `Ok(None)`;
/// anything else that is not a usable `ip:port` is refused, never ignored.
pub fn parse_parent_hint(s: &str) -> Result<Option<SocketAddr>, HintError> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    let addr: SocketAddr = s.parse().map_err(|_| HintError::NotSocketAddr)?;
    if addr.port() == 0 {
        return Err(HintError::ZeroPort);
    }
    let bad = match addr.ip() {
        IpAddr::V4(ip) => ip.is_unspecified() || ip.is_multicast() || ip.is_broadcast(),
        IpAddr::V6(ip) => ip.is_unspecified() || ip.is_multicast(),
    };
    if bad {
        return Err(HintError::NotUnicast);
    }
    Ok(Some(addr))
}

/// The limited-broadcast discovery target.
pub const BROADCAST: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::BROADCAST), DISCOVERY_PORT);

/// Where successive probes go: the hint [`HINT_ATTEMPTS`] times when there is
/// one, then broadcast. Without a hint, broadcast alone.
pub fn probe_plan(hint: Option<SocketAddr>) -> Vec<SocketAddr> {
    let mut plan = Vec::new();
    if let Some(h) = hint {
        for _ in 0..HINT_ATTEMPTS {
            plan.push(h);
        }
    }
    plan.push(BROADCAST);
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_hint_is_no_hint_and_a_good_one_parses() {
        assert_eq!(parse_parent_hint(""), Ok(None));
        assert_eq!(parse_parent_hint("  \n"), Ok(None));
        assert_eq!(parse_parent_hint(" 192.168.10.100:9490\n"), Ok(Some("192.168.10.100:9490".parse().unwrap())));
        assert!(parse_parent_hint("[fd00::1]:9490").unwrap().is_some());
    }

    #[test]
    fn malformed_hints_refuse_rather_than_mean_no_hint() {
        assert_eq!(parse_parent_hint("192.168.10.100"), Err(HintError::NotSocketAddr));
        assert_eq!(parse_parent_hint("host.local:9490"), Err(HintError::NotSocketAddr));
        assert_eq!(parse_parent_hint("192.168.10.100:99999"), Err(HintError::NotSocketAddr));
        assert_eq!(parse_parent_hint("192.168.10.100:0"), Err(HintError::ZeroPort));
        assert_eq!(parse_parent_hint("0.0.0.0:9490"), Err(HintError::NotUnicast));
        assert_eq!(parse_parent_hint("255.255.255.255:9490"), Err(HintError::NotUnicast));
        assert_eq!(parse_parent_hint("224.0.0.1:9490"), Err(HintError::NotUnicast));
    }

    #[test]
    fn plan_tries_the_hint_before_broadcast() {
        let hint: SocketAddr = "192.168.10.100:9490".parse().unwrap();
        let plan = probe_plan(Some(hint));
        assert_eq!(plan.len(), HINT_ATTEMPTS + 1);
        assert!(plan[..HINT_ATTEMPTS].iter().all(|t| *t == hint));
        assert_eq!(plan[HINT_ATTEMPTS], BROADCAST);
        assert_eq!(probe_plan(None), alloc::vec![BROADCAST]);
    }
}
