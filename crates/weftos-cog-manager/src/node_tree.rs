//! The node the console is on, the nodes under it, and the things attached to each.
//!
//! The root is the cog-host Weave Manager connected to. Tailnet peers (and mesh cog-hosts) are
//! the nodes under it. Cogs reported on a node, and edge devices that checked in to this host,
//! hang under that node. A peer is offered as a connect target only when its address is on the
//! tailnet; a LAN address stays visible and is not a mesh hop.

use crate::address_book::{self, Reach};
use crate::client::{FleetNode, HostCog, MeshNodeRaw, Net};
use crate::sensor_install::peer_url;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Root,
    Peer,
    Cog,
    Edge,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Twig {
    pub key: String,
    pub label: String,
    pub role: Role,
    pub reach: Reach,
    /// Set for a tailnet peer the console can attach to. Empty for cogs, edges, and LAN peers.
    pub connect_url: Option<String>,
    pub note: String,
    pub children: Vec<Twig>,
}

pub fn tree(host_url: &str, this_name: &str, this_cogs: &[HostCog], net: Option<&Net>, mesh: &[MeshNodeRaw]) -> Twig {
    let root_label = {
        let named = net.map(|n| n.node.trim()).filter(|s| !s.is_empty()).unwrap_or("");
        if !named.is_empty() {
            named.to_string()
        } else if this_name.trim().is_empty() {
            "this node".into()
        } else {
            this_name.trim().to_string()
        }
    };
    let self_cogs = mesh.iter().find(|n| n.is_self).map(|n| n.cogs.as_slice()).unwrap_or(this_cogs);
    let mut children = Vec::new();
    children.extend(peer_twigs(host_url, net, mesh));
    children.extend(self_cogs.iter().map(cog_twig));
    if let Some(net) = net {
        children.extend(net.fleet.iter().map(edge_twig));
    }
    Twig {
        key: "self".into(),
        label: root_label,
        role: Role::Root,
        reach: address_book::classify(host_url),
        connect_url: None,
        note: host_url.trim().trim_end_matches('/').to_string(),
        children,
    }
}

fn peer_twigs(host_url: &str, net: Option<&Net>, mesh: &[MeshNodeRaw]) -> Vec<Twig> {
    let mut ips: Vec<String> = Vec::new();
    let mut push_ip = |ip: &str| {
        let ip = ip.trim();
        if ip.is_empty() || ips.iter().any(|e| e == ip) {
            return;
        }
        ips.push(ip.to_string());
    };
    for n in mesh.iter().filter(|n| !n.is_self) {
        push_ip(&n.ip);
    }
    if let Some(net) = net {
        for p in net.tailscale.peers.iter().filter(|p| !p.is_self) {
            push_ip(&p.ip);
        }
    }
    ips.into_iter().map(|ip| {
        let mesh_node = mesh.iter().find(|n| !n.is_self && n.ip == ip);
        let ts = net.and_then(|n| n.tailscale.peers.iter().find(|p| !p.is_self && p.ip == ip));
        let label = mesh_node.map(|n| n.node.trim()).filter(|s| !s.is_empty()).map(str::to_string)
            .or_else(|| ts.map(|p| p.name.trim()).filter(|s| !s.is_empty()).map(str::to_string))
            .unwrap_or_else(|| ip.clone());
        let connect_url = peer_url(host_url, &ip);
        let reach = if connect_url.is_some() { Reach::Tailnet } else { address_book::classify(&format!("http://{ip}:9480")) };
        let mut note = if connect_url.is_some() { "tailnet".to_string() } else { "not a tailnet address".to_string() };
        if let Some(n) = mesh_node {
            if !n.reachable {
                let why = n.error.clone().unwrap_or_else(|| "unreachable".into());
                note = format!("{note}, {why}");
            }
        } else if ts.is_some_and(|p| !p.online) {
            note = format!("{note}, offline");
        }
        let children = mesh_node.map(|n| n.cogs.iter().map(cog_twig).collect()).unwrap_or_default();
        Twig { key: ip.clone(), label, role: Role::Peer, reach, connect_url, note, children }
    }).collect()
}

fn cog_twig(c: &HostCog) -> Twig {
    Twig {
        key: format!("cog:{}", c.id),
        label: c.id.clone(),
        role: Role::Cog,
        reach: Reach::Other,
        connect_url: None,
        note: if c.running { "running".into() } else { "stopped".into() },
        children: Vec::new(),
    }
}

fn edge_twig(f: &FleetNode) -> Twig {
    let label = if f.sensor.is_empty() { f.id.clone() } else { format!("{} · {}", f.id, f.sensor) };
    let note = if f.ip.is_empty() { "checked in here".into() } else { format!("checked in here, {}", f.ip) };
    Twig {
        key: format!("edge:{}", f.id),
        label,
        role: Role::Edge,
        reach: Reach::Other,
        connect_url: None,
        note,
        children: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cog(id: &str, running: bool) -> HostCog {
        HostCog { id: id.into(), running, ..HostCog::default() }
    }

    #[test]
    fn root_holds_tailnet_peers_then_its_own_cogs_and_edges() {
        let mesh = vec![
            MeshNodeRaw { node: "example-host".into(), ip: "100.64.0.10".into(), is_self: true, reachable: true, cogs: vec![cog("catalog", true)], ..MeshNodeRaw::default() },
            MeshNodeRaw { node: "cog0".into(), ip: "100.64.0.20".into(), is_self: false, reachable: true, cogs: vec![cog("ld1040c-motion", false)], ..MeshNodeRaw::default() },
        ];
        let net = Net {
            node: "example-host".into(),
            tailscale: crate::client::Tailscale {
                available: true,
                peers: vec![
                    crate::client::NetPeer { name: "example-host".into(), ip: "100.64.0.10".into(), online: true, is_self: true, ..Default::default() },
                    crate::client::NetPeer { name: "example-cog".into(), ip: "100.64.0.20".into(), online: true, is_self: false, ..Default::default() },
                    crate::client::NetPeer { name: "bench".into(), ip: "192.168.1.50".into(), online: true, is_self: false, ..Default::default() },
                ],
            },
            fleet: vec![FleetNode { id: "esp-1".into(), sensor: "csi".into(), ip: "192.168.1.40".into(), online: true, ..FleetNode::default() }],
            ..Net::default()
        };
        let t = tree("http://127.0.0.1:9480", "", &[], Some(&net), &mesh);
        assert_eq!(t.label, "example-host");
        assert_eq!(t.reach, Reach::ThisMachine);
        let peer = t.children.iter().find(|c| c.role == Role::Peer && c.key == "100.64.0.20").unwrap();
        assert_eq!(peer.connect_url.as_deref(), Some("http://100.64.0.20:9480"));
        assert_eq!(peer.children[0].label, "ld1040c-motion");
        let lan = t.children.iter().find(|c| c.key == "192.168.1.50").unwrap();
        assert!(lan.connect_url.is_none());
        assert!(lan.note.contains("not a tailnet"));
        assert!(t.children.iter().any(|c| c.role == Role::Cog && c.label == "catalog"));
        assert!(t.children.iter().any(|c| c.role == Role::Edge && c.label.contains("esp-1")));
        assert!(!t.children.iter().any(|c| c.key == "100.64.0.10"), "the connected node is the root, not a child of itself");
    }
}
