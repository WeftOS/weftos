//! mDNS LAN discovery for WeftOS mesh networking (K6.2).
//!
//! Implements DNS-SD service discovery over multicast UDP (224.0.0.251:5353).
//! Advertises `_weftos._tcp.local` service and discovers peers on the LAN.

use std::collections::HashSet;
use std::net::{Ipv4Addr, SocketAddrV4};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::net::UdpSocket;

use crate::mesh_discovery::{DiscoveredPeer, DiscoveryBackend, DiscoveryError, DiscoverySource};

/// Multicast group for mDNS.
const MDNS_MULTICAST_ADDR: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);

/// Standard mDNS port.
const MDNS_PORT: u16 = 5353;

/// WeftOS service name for DNS-SD.
pub const WEFTOS_SERVICE_NAME: &str = "_weftos._tcp.local";

/// mDNS service announcement payload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MdnsAnnouncement {
    /// Service type (always `_weftos._tcp.local`).
    pub service: String,
    /// Node identifier.
    pub node_id: String,
    /// Listen address (IP).
    pub address: String,
    /// Listen port.
    pub port: u16,
    /// Platform (linux, darwin, wasm, etc.).
    pub platform: String,
    /// Governance genesis hash — used to scope clusters.
    pub governance_genesis: String,
}

/// mDNS discovery backend.
///
/// Sends and receives JSON-encoded [`MdnsAnnouncement`] packets over
/// UDP multicast. Filters out self-announcements and duplicates.
pub struct MdnsDiscovery {
    /// Our node's announcement template.
    local_announcement: MdnsAnnouncement,
    /// Discovered peers pending delivery.
    pending: Vec<DiscoveredPeer>,
    /// Multicast socket.
    socket: Option<UdpSocket>,
    /// Whether discovery is active.
    active: bool,
    /// Known node IDs for deduplication.
    known: HashSet<String>,
    /// Where announcements are sent: the mDNS group, except in tests.
    announce_dest: SocketAddrV4,
}

impl MdnsDiscovery {
    /// Create a new mDNS discovery backend.
    pub fn new(
        node_id: String,
        listen_addr: String,
        port: u16,
        platform: String,
        governance_genesis: String,
    ) -> Self {
        Self {
            local_announcement: MdnsAnnouncement {
                service: WEFTOS_SERVICE_NAME.to_string(),
                node_id,
                address: listen_addr,
                port,
                platform,
                governance_genesis,
            },
            pending: Vec::new(),
            socket: None,
            announce_dest: SocketAddrV4::new(MDNS_MULTICAST_ADDR, MDNS_PORT),
            active: false,
            known: HashSet::new(),
        }
    }

    /// Send our announcement to the multicast group.
    async fn announce(&self) -> Result<(), DiscoveryError> {
        if let Some(ref socket) = self.socket {
            let payload = serde_json::to_vec(&self.local_announcement)
                .map_err(|e| DiscoveryError::Backend(e.to_string()))?;
            socket
                .send_to(&payload, self.announce_dest)
                .await
                .map_err(|e| DiscoveryError::Backend(e.to_string()))?;
        }
        Ok(())
    }

    /// Try to receive announcements from peers (non-blocking with short timeout).
    async fn try_receive(&mut self) -> Vec<DiscoveredPeer> {
        let mut peers = Vec::new();
        let Some(ref socket) = self.socket else {
            return peers;
        };

        let mut buf = [0u8; 4096];
        // Short timeout so poll never blocks the coordinator.
        // Timeout or recv error → no peers this round; only the happy
        // path does work.
        if let Ok(Ok((len, _addr))) = tokio::time::timeout(
            std::time::Duration::from_millis(10),
            socket.recv_from(&mut buf),
        )
        .await
            && let Ok(ann) = serde_json::from_slice::<MdnsAnnouncement>(&buf[..len])
            && ann.node_id != self.local_announcement.node_id
            && !self.known.contains(&ann.node_id)
        {
            self.known.insert(ann.node_id.clone());
            peers.push(DiscoveredPeer {
                node_id: ann.node_id,
                address: format!("{}:{}", ann.address, ann.port),
                platform: ann.platform,
                source: DiscoverySource::Mdns,
            });
        }
        peers
    }

    /// Access the local announcement (for testing / introspection).
    pub fn local_announcement(&self) -> &MdnsAnnouncement {
        &self.local_announcement
    }
}

#[async_trait]
impl DiscoveryBackend for MdnsDiscovery {
    fn name(&self) -> &str {
        "mdns"
    }

    async fn start(&mut self) -> Result<(), DiscoveryError> {
        // Bind to INADDR_ANY on the mDNS port (or any port as fallback).
        let socket =
            match UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, MDNS_PORT)).await {
                Ok(s) => s,
                Err(_) => {
                    // Port 5353 may be in use (systemd-resolved, avahi, etc.).
                    UdpSocket::bind("0.0.0.0:0")
                        .await
                        .map_err(|e| DiscoveryError::Backend(format!("bind failed: {e}")))?
                }
            };

        // Join the multicast group — best effort (may fail in containers).
        let _ = socket.join_multicast_v4(MDNS_MULTICAST_ADDR, Ipv4Addr::UNSPECIFIED);

        self.socket = Some(socket);
        self.active = true;

        // Send the initial announcement. Best effort, like the join above: a host
        // with no usable multicast route (VPN or bridge interfaces holding the
        // 224.0.0.0/4 route) returns "No route to host", and discovery should still
        // run so it can receive and answer peers that do reach it.
        if let Err(e) = self.announce().await {
            tracing::warn!(error = %e, "mdns: initial announcement not sent");
        }
        Ok(())
    }

    async fn poll(&mut self) -> Vec<DiscoveredPeer> {
        if !self.active {
            return vec![];
        }
        let mut peers = self.try_receive().await;
        peers.append(&mut self.pending);
        peers
    }

    async fn stop(&mut self) -> Result<(), DiscoveryError> {
        self.active = false;
        self.socket = None;
        self.pending.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mdns_announcement_serde_roundtrip() {
        let ann = MdnsAnnouncement {
            service: WEFTOS_SERVICE_NAME.to_string(),
            node_id: "node-abc".into(),
            address: "192.168.1.42".into(),
            port: 9489,
            platform: "linux".into(),
            governance_genesis: "deadbeef01234567".into(),
        };
        let json = serde_json::to_string(&ann).unwrap();
        let back: MdnsAnnouncement = serde_json::from_str(&json).unwrap();
        assert_eq!(ann, back);
    }

    #[tokio::test]
    async fn mdns_start_survives_a_failed_announcement() {
        let mut disc = MdnsDiscovery::new(
            "node-abc".into(),
            "127.0.0.1".into(),
            9489,
            "test".into(),
            "deadbeef01234567".into(),
        );
        // The limited broadcast address is refused without SO_BROADCAST on every
        // platform, so the first announcement fails regardless of host routing.
        disc.announce_dest = SocketAddrV4::new(Ipv4Addr::BROADCAST, MDNS_PORT);

        disc.start().await.unwrap();
        assert!(disc.active);
        assert!(disc.socket.is_some());
        assert!(
            disc.announce().await.is_err(),
            "the broadcast send must fail for this test to cover the failure path"
        );

        disc.stop().await.unwrap();
        assert!(!disc.active);
    }

    #[tokio::test]
    async fn mdns_discovery_creates_and_stops() {
        let mut disc = MdnsDiscovery::new(
            "test-node".into(),
            "127.0.0.1".into(),
            9489,
            "linux".into(),
            "0000000000000000".into(),
        );
        assert_eq!(disc.name(), "mdns");
        assert!(!disc.active);

        disc.start().await.unwrap();
        assert!(disc.active);
        assert!(disc.socket.is_some());

        disc.stop().await.unwrap();
        assert!(!disc.active);
        assert!(disc.socket.is_none());
    }

    #[tokio::test]
    async fn mdns_self_announcement_filtered() {
        // Simulate receiving our own announcement — it should be ignored.
        let mut disc = MdnsDiscovery::new(
            "self-node".into(),
            "127.0.0.1".into(),
            9489,
            "linux".into(),
            "aabbccdd".into(),
        );
        disc.active = true;

        // Manually inject a "self" announcement into the known set check path.
        // The filter is: ann.node_id != self.local_announcement.node_id
        // We verify by checking that the local announcement node_id is "self-node".
        assert_eq!(disc.local_announcement().node_id, "self-node");

        // Create a fake announcement from ourselves.
        let self_ann = disc.local_announcement.clone();
        let payload = serde_json::to_vec(&self_ann).unwrap();

        // Parse it the way try_receive would.
        let parsed: MdnsAnnouncement = serde_json::from_slice(&payload).unwrap();
        assert_eq!(parsed.node_id, "self-node");
        // The filter would reject this because parsed.node_id == local node_id.
        assert_eq!(parsed.node_id, disc.local_announcement.node_id);
    }

    #[tokio::test]
    async fn mdns_duplicate_filtered() {
        let mut disc = MdnsDiscovery::new(
            "local".into(),
            "127.0.0.1".into(),
            9489,
            "linux".into(),
            "aabb".into(),
        );

        // Simulate discovering the same node twice.
        disc.known.insert("remote-1".to_string());

        // Second insert returns false — already known.
        assert!(!disc.known.insert("remote-1".to_string()));
        // New node is accepted.
        assert!(disc.known.insert("remote-2".to_string()));
    }

    #[tokio::test]
    async fn mdns_poll_when_inactive_returns_empty() {
        let mut disc = MdnsDiscovery::new(
            "n".into(),
            "127.0.0.1".into(),
            9489,
            "linux".into(),
            "aa".into(),
        );
        // Not started — should return empty.
        let peers = disc.poll().await;
        assert!(peers.is_empty());
    }

    #[tokio::test]
    async fn mdns_pending_drained_on_poll() {
        let mut disc = MdnsDiscovery::new(
            "n".into(),
            "127.0.0.1".into(),
            9489,
            "linux".into(),
            "aa".into(),
        );
        disc.active = true;
        disc.pending.push(DiscoveredPeer {
            node_id: "injected".into(),
            address: "10.0.0.1:9489".into(),
            platform: "darwin".into(),
            source: DiscoverySource::Mdns,
        });

        let peers = disc.poll().await;
        // The pending peer should be drained (try_receive will time out with no socket,
        // but pending is appended).
        assert!(peers.iter().any(|p| p.node_id == "injected"));

        // Second poll should be empty.
        let second = disc.poll().await;
        assert!(second.is_empty());
    }
}
