// Wires esp-idf-sys into Cargo's build graph: tells Cargo about the
// sysenv vars embuild/idf-sys sets (linker paths, sysroot, includes,
// CFG flags). Same one-liner as crates/clawft-edge-bench.
fn main() {
    embuild::espidf::sysenv::output();
    println!("cargo:rerun-if-env-changed=WEFTOS_LEAF_SEED_FILE");
    println!("cargo:rerun-if-env-changed=WEFTOS_LEAF_CERT_FILE");
    let seed = std::env::var("WEFTOS_LEAF_SEED_FILE").ok();
    let cert = std::env::var("WEFTOS_LEAF_CERT_FILE").ok();
    assert_eq!(seed.is_some(), cert.is_some(), "both leaf seed and certificate files must be supplied together");
    let source = match (seed, cert) {
        (Some(seed), Some(cert)) => {
            let seed = std::fs::canonicalize(seed).expect("leaf seed path");
            let cert = std::fs::canonicalize(cert).expect("leaf certificate path");
            assert_eq!(std::fs::metadata(&seed).expect("leaf seed file").len(), 32, "leaf seed must be 32 bytes");
            assert!(std::fs::metadata(&cert).expect("leaf certificate file").len() <= 4096, "leaf certificate too large");
            println!("cargo:rerun-if-changed={}", seed.display());
            println!("cargo:rerun-if-changed={}", cert.display());
            format!("const LEAF_SEED: &[u8] = include_bytes!({:?});\nconst LEAF_CERT: &[u8] = include_bytes!({:?});\n", seed.to_string_lossy(), cert.to_string_lossy())
        }
        _ => "const LEAF_SEED: &[u8] = &[];\nconst LEAF_CERT: &[u8] = &[];\n".to_string(),
    };
    // Optional unicast parent hint (`ip:port`) for networks that drop broadcast.
    println!("cargo:rerun-if-env-changed=WEFTOS_LEAF_PARENT_HINT");
    let hint = std::env::var("WEFTOS_LEAF_PARENT_HINT").unwrap_or_default();
    let hint = hint.trim();
    if !hint.is_empty() {
        let addr: std::net::SocketAddr = hint.parse().unwrap_or_else(|_| panic!("WEFTOS_LEAF_PARENT_HINT must be ip:port (example 192.168.10.100:9490)"));
        assert!(addr.port() != 0 && !addr.ip().is_unspecified() && !addr.ip().is_multicast(), "WEFTOS_LEAF_PARENT_HINT must be a unicast ip:port");
    }
    let source = format!("{source}const PARENT_HINT: &str = {hint:?};\n");
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    std::fs::write(out.join("leaf_provision.rs"), source).expect("write leaf provision source");
}
