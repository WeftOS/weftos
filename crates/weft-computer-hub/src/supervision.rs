//! The definition weaver supervises `weft-hub` from, plus the checks that
//! keep it honest. The file itself is `deploy/weftapp.toml`.

/// Embedded so the binary can print the definition (`weft-hub print-app`)
/// and the tests check exactly what ships.
pub const APP_MANIFEST: &str = include_str!("../deploy/weftapp.toml");

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use std::collections::HashMap;

    // Mirrors of clawft-kernel's AppManifest / ServiceSpec / PortMapping /
    // AppCapabilities field names (this package cannot depend on the kernel).
    // `deny_unknown_fields` makes a renamed or invented key fail here.
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Manifest {
        name: String,
        version: String,
        description: String,
        license: String,
        capabilities: Caps,
        services: Vec<Service>,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Caps {
        network: bool,
        shell: bool,
        filesystem: Vec<String>,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Service {
        name: String,
        command: String,
        ports: Vec<Port>,
        env: HashMap<String, String>,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Port {
        host_port: u16,
        container_port: u16,
        protocol: String,
    }

    fn manifest() -> Manifest {
        toml::from_str(APP_MANIFEST).expect("deploy/weftapp.toml parses in the AppManifest shape")
    }

    #[test]
    fn definition_names_one_service_for_the_hub_binary() {
        let m = manifest();
        assert_eq!(
            (m.name.as_str(), m.license.as_str()),
            ("weft-hub", "Apache-2.0")
        );
        assert!(!m.version.is_empty() && !m.description.is_empty());
        assert_eq!(m.services.len(), 1);
        let s = &m.services[0];
        assert_eq!(s.name, "weft-hub");
        // argv-style command: the binary then `serve`, no shell metacharacters.
        let argv: Vec<&str> = s.command.split_whitespace().collect();
        assert_eq!(argv, ["weft-hub", "serve"]);
        assert!(!s.command.contains(['|', ';', '&', '$', '`']));
        assert_eq!(s.env.get("RUST_LOG").map(String::as_str), Some("info"));
    }

    #[test]
    fn definition_does_not_widen_the_hubs_reach() {
        let m = manifest();
        assert!(!m.capabilities.shell, "hub needs no shell");
        assert!(m.capabilities.network, "the listener needs sockets");
        assert_eq!(m.capabilities.filesystem, ["~/.weftos/hub"]);
        // The declared port is the hub's default listen port, over tcp.
        let p = &m.services[0].ports[0];
        assert_eq!(
            (p.host_port, p.container_port, p.protocol.as_str()),
            (9490, 9490, "tcp")
        );
        // The shipped example config listens on loopback at that same port.
        let example: toml::Table =
            toml::from_str(include_str!("../config/hub.example.toml")).unwrap();
        let addr = example["listen"]["addr"].as_str().unwrap();
        assert_eq!(
            crate::server::check_bind(addr, false).unwrap().port(),
            p.host_port
        );
    }
}
