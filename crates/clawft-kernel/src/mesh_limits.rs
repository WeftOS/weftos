//! Connection limits for the mesh listener (P3-K1): caps and timeouts. The
//! per-IP cap and first-frame timeout apply in every admission mode; the idle
//! timeout only under a strict gate.

use std::sync::Arc;

/// Most concurrent inbound connections served at once.
pub const MAX_CONNECTIONS: usize = 1024;
/// Time a peer gets to finish the Noise handshake.
pub const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// How often a connection checks that its route still exists and its peer is
/// not revoked (review S1: a revoked or disconnected peer is closed within this).
pub const ROUTE_CHECK: std::time::Duration = std::time::Duration::from_millis(250);

/// Default per-IP concurrent connection cap (every admission mode;
/// `kernel.mesh.max_connections_per_ip`).
pub const MAX_CONNECTIONS_PER_IP: usize = 64;
/// Default time a peer gets to send its first frame (every admission mode;
/// `kernel.mesh.first_frame_timeout_secs`).
pub const FIRST_FRAME_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// Close an admitted connection idle this long (strict gates only).
pub const IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(900);

/// Longest one outbound frame may take to write. A peer that stops reading
/// fills the socket, then the connection's outbound channel; past this the
/// connection is closed so senders fail fast instead of queueing behind it.
pub const WRITE_STALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Releases a per-IP slot on drop.
pub(crate) struct IpSlot {
    ip: std::net::IpAddr,
    map: Arc<std::sync::Mutex<std::collections::HashMap<std::net::IpAddr, usize>>>,
}

impl IpSlot {
    /// Slot key for a source address: IPv4-mapped IPv6 is folded onto its
    /// IPv4 form and IPv6 is bucketed by /64, so one host cannot multiply
    /// its quota by rotating addresses.
    fn key(ip: std::net::IpAddr) -> std::net::IpAddr {
        match ip.to_canonical() {
            std::net::IpAddr::V6(v6) => {
                let mut o = v6.octets();
                o[8..].fill(0);
                std::net::IpAddr::V6(o.into())
            }
            v4 => v4,
        }
    }

    pub(crate) fn acquire(
        map: &Arc<std::sync::Mutex<std::collections::HashMap<std::net::IpAddr, usize>>>,
        ip: std::net::IpAddr,
        max: usize,
    ) -> Option<Self> {
        let ip = Self::key(ip);
        let mut m = map.lock().unwrap();
        let n = m.entry(ip).or_insert(0);
        if *n >= max {
            return None;
        }
        *n += 1;
        Some(Self { ip, map: Arc::clone(map) })
    }
}

impl Drop for IpSlot {
    fn drop(&mut self) {
        let mut m = self.map.lock().unwrap();
        if let Some(n) = m.get_mut(&self.ip) {
            *n -= 1;
            if *n == 0 {
                m.remove(&self.ip);
            }
        }
    }
}


/// Connection limits. `first_frame` and `per_ip` apply under every gate;
/// `idle` only under a strict one (see [`AdmissionGate::strict`]) and on
/// dialled connections (inbound silence).
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Silence allowed before the first frame.
    pub first_frame: std::time::Duration,
    /// Silence allowed on an admitted connection (strict gates; dialled
    /// connections count inbound silence only).
    pub idle: std::time::Duration,
    /// Concurrent connections per source IP.
    pub per_ip: usize,
    /// Longest one outbound frame may take to write before the connection
    /// is closed as stalled (every gate).
    pub write_stall: std::time::Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            first_frame: FIRST_FRAME_TIMEOUT,
            idle: IDLE_TIMEOUT,
            per_ip: MAX_CONNECTIONS_PER_IP,
            write_stall: WRITE_STALL_TIMEOUT,
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;

    #[test]
    fn mapped_and_prefix_addresses_share_a_slot() {
        let map = Arc::new(std::sync::Mutex::new(Default::default()));
        let v4: IpAddr = "192.0.2.1".parse().unwrap();
        let mapped: IpAddr = "::ffff:192.0.2.1".parse().unwrap();
        let _a = IpSlot::acquire(&map, v4, 1).unwrap();
        assert!(IpSlot::acquire(&map, mapped, 1).is_none());

        let p1: IpAddr = "2001:db8:1:2::1".parse().unwrap();
        let p2: IpAddr = "2001:db8:1:2:ffff::9".parse().unwrap();
        let other: IpAddr = "2001:db8:1:3::1".parse().unwrap();
        let _b = IpSlot::acquire(&map, p1, 1).unwrap();
        assert!(IpSlot::acquire(&map, p2, 1).is_none(), "same /64");
        assert!(IpSlot::acquire(&map, other, 1).is_some(), "different /64");
    }
}
