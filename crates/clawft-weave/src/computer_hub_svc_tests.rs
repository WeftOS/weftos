//! `computer_hub_svc`: start-up from a runtime dir, status, minting, revocation
//! and route registration. Every test gets its own temp runtime dir; none reads
//! or writes the real `~/.weftos`.

use serde_json::{Value, json};
use weft_hub::hub::now_secs;
use weft_hub::passkey::Role;

use crate::capability::Capability;
use crate::computer_hub_svc::{CONFIG_DIR, CONFIG_FILE, ComputerHubService, MintParams, config_path, start};

const HUB_TOML: &str = r#"
[listen]
addr = "127.0.0.1:0"

[auth]
max_ttl_secs = 7200

[[auth.minters]]
id = "weftos-xpc"
host_kind = "desktop"
key_file = "keys/xpc.key"

[[auth.minters]]
id = "weftos-box"
host_kind = "container"
key_file = "keys/box.key"

[[machines]]
id = "xpc"
description = "aepod-xpc"
minter = "weftos-xpc"

[[machines]]
id = "box"
description = "a container"
minter = "weftos-box"
"#;

fn write_cfg(runtime: &std::path::Path, toml: &str) {
    let dir = runtime.join(CONFIG_DIR);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(CONFIG_FILE), toml).unwrap();
}

async fn running() -> (tempfile::TempDir, std::sync::Arc<ComputerHubService>) {
    let tmp = tempfile::tempdir().unwrap();
    write_cfg(tmp.path(), HUB_TOML);
    let svc = start(tmp.path()).await.unwrap().expect("configured hub starts");
    (tmp, svc)
}

#[tokio::test]
async fn no_config_file_means_no_hub_and_creates_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(start(tmp.path()).await.unwrap().is_none());
    assert!(!tmp.path().join(CONFIG_DIR).exists(), "an unconfigured node gets no directory");
}

#[tokio::test]
async fn a_broken_config_is_an_error_not_a_silent_skip() {
    let tmp = tempfile::tempdir().unwrap();
    write_cfg(tmp.path(), "[[machines]]\nid = \"x\"\nminter = \"nope\"\n");
    assert!(start(tmp.path()).await.is_err());
    write_cfg(tmp.path(), "bogus = 1\n");
    assert!(start(tmp.path()).await.is_err());
}

#[tokio::test]
async fn a_non_loopback_bind_needs_the_files_own_opt_in() {
    let tmp = tempfile::tempdir().unwrap();
    write_cfg(tmp.path(), &HUB_TOML.replace("127.0.0.1:0", "0.0.0.0:0"));
    let e = start(tmp.path()).await.err().expect("refused");
    assert!(e.contains("not loopback"), "{e}");
    write_cfg(
        tmp.path(),
        &HUB_TOML.replace("addr = \"127.0.0.1:0\"", "addr = \"127.0.0.1:0\"\nallow_non_loopback = true"),
    );
    assert!(start(tmp.path()).await.unwrap().is_some());
}

#[tokio::test]
async fn start_creates_private_minter_keys_and_status_lists_the_machines() {
    let (tmp, svc) = running().await;
    assert_eq!(config_path(tmp.path()), tmp.path().join(CONFIG_DIR).join(CONFIG_FILE));
    for k in ["xpc", "box"] {
        let key = tmp.path().join(CONFIG_DIR).join("keys").join(format!("{k}.key"));
        assert!(key.exists(), "{k} key created");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&key).unwrap().permissions().mode() & 0o077, 0);
        }
    }
    let st = svc.status();
    assert_eq!(st["running"], true);
    assert_ne!(st["addr"], "127.0.0.1:0", "the bound port is reported, not the requested one");
    let machines = st["machines"].as_array().unwrap();
    assert_eq!(machines.len(), 2);
    assert_eq!(machines[0]["id"], "box");
    assert_eq!(machines[0]["host_kind"], "container");
    assert_eq!(machines[1]["id"], "xpc");
    assert_eq!(machines[1]["host_kind"], "desktop");
    assert_eq!(machines[1]["connected"], false);
    assert!(!st.to_string().contains("key"), "status never mentions key material: {st}");
    svc.shutdown();
}

#[tokio::test]
async fn host_kind_comes_from_the_minter_of_the_machine() {
    let (_t, svc) = running().await;
    let now = now_secs();
    let xpc = svc
        .mint(&json!({"machine": "xpc", "sub": "mathew", "roles": ["server"]}), now)
        .unwrap();
    assert_eq!(xpc["host_kind"], "desktop");
    assert_eq!(xpc["minter"], "weftos-xpc");
    assert_eq!(xpc["expires_at"], now + 3600, "default ttl");
    let boxed = svc
        .mint(&json!({"machine": "box", "sub": "mathew", "roles": ["server"]}), now)
        .unwrap();
    assert_eq!(boxed["host_kind"], "container");
    // The hub verifies the token it minted, as the machine's server, and
    // reports the minter's host_kind, never anything from the request.
    let a = svc
        .hub()
        .authenticate(xpc["token"].as_str(), Role::Server, now)
        .unwrap();
    assert_eq!(a.minter_id, "weftos-xpc");
    assert_eq!(a.host_kind.as_str(), "desktop");
    assert_eq!(a.claims.srv.as_deref(), Some("xpc"));
    // A server key does not grant the harness role.
    assert_eq!(
        svc.hub().authenticate(xpc["token"].as_str(), Role::Harness, now).unwrap_err().status,
        403
    );
}

#[tokio::test]
async fn caller_cannot_pick_a_minter_that_does_not_serve_the_machine() {
    let (_t, svc) = running().await;
    let now = now_secs();
    let e = svc
        .mint(
            &json!({"machine": "xpc", "minter": "weftos-box", "sub": "u", "roles": ["server"]}),
            now,
        )
        .unwrap_err();
    assert!(e.contains("is served by minter"), "{e}");
}

#[tokio::test]
async fn harness_keys_name_a_minter_and_server_keys_name_a_machine() {
    let (_t, svc) = running().await;
    let now = now_secs();
    let h = svc
        .mint(&json!({"minter": "weftos-xpc", "sub": "tapestry", "roles": ["harness"], "ttl_secs": 600}), now)
        .unwrap();
    assert_eq!(h["machine"], Value::Null);
    assert!(svc.hub().authenticate(h["token"].as_str(), Role::Harness, now).is_ok());
    for bad in [
        json!({"sub": "u", "roles": ["harness"]}),
        json!({"minter": "weftos-xpc", "sub": "u", "roles": ["server"]}),
        json!({"minter": "nope", "sub": "u", "roles": ["harness"]}),
        json!({"machine": "nope", "sub": "u", "roles": ["server"]}),
    ] {
        assert!(svc.mint(&bad, now).is_err(), "{bad}");
    }
}

#[tokio::test]
async fn mint_refuses_malformed_and_over_long_requests() {
    let (_t, svc) = running().await;
    let now = now_secs();
    for bad in [
        json!({"machine": "xpc", "roles": ["server"]}),                       // no sub
        json!({"machine": "xpc", "sub": "", "roles": ["server"]}),            // empty sub
        json!({"machine": "xpc", "sub": "u"}),                                // no roles
        json!({"machine": "xpc", "sub": "u", "roles": []}),                   // empty roles
        json!({"machine": "xpc", "sub": "u", "roles": ["root"]}),             // unknown role
        json!({"machine": "xpc", "sub": "u", "roles": ["server"], "ttl_secs": 0}),
        json!({"machine": "xpc", "sub": "u", "roles": ["server"], "ttl_secs": "9"}),
        json!({"machine": "xpc", "sub": "u", "roles": ["server"], "ttl_secs": 7201}), // > max_ttl
        json!({"machine": "xpc", "sub": "u", "roles": ["server"], "host_kind": "desktop"}), // unknown field
        json!({"machine": "xpc", "sub": "x".repeat(129), "roles": ["server"]}),
    ] {
        assert!(svc.mint(&bad, now).is_err(), "{bad}");
    }
    assert!(svc.mint(&json!([]), now).is_err());
}

#[tokio::test]
async fn a_revoked_key_is_refused_by_the_hub() {
    let (_t, svc) = running().await;
    let now = now_secs();
    let m = svc
        .mint(&json!({"machine": "xpc", "sub": "u", "roles": ["server"]}), now)
        .unwrap();
    let jti = m["jti"].as_str().unwrap();
    assert!(!jti.is_empty());
    let r = svc.revoke(&json!({"jti": jti})).unwrap();
    assert_eq!(r["newly_revoked"], true);
    assert_eq!(svc.revoke(&json!({"jti": jti})).unwrap()["newly_revoked"], false);
    let e = svc.hub().authenticate(m["token"].as_str(), Role::Server, now).unwrap_err();
    assert_eq!((e.status, e.reason.contains("revoked")), (401, true));
    assert!(svc.revoke(&json!({})).is_err());
    assert!(svc.revoke(&json!({"jti": "a", "x": 1})).is_err());
}

#[test]
fn mint_params_parse_defaults() {
    let p = MintParams::parse(&json!({"machine": "xpc", "sub": "u", "roles": ["server", "harness"]})).unwrap();
    assert_eq!(p.roles, vec![Role::Server, Role::Harness]);
    assert_eq!(p.ttl_secs, 3600);
}

#[test]
fn routes_are_registered_with_the_right_capabilities() {
    let routes = crate::rpc_ext::builtin_route_names();
    let cap = |n: &str| routes.iter().find(|(r, _)| *r == n).map(|(_, c)| *c);
    assert_eq!(cap("computer.hub.status"), Some(Capability::Read));
    assert_eq!(cap("computer.hub.mint"), Some(Capability::Admin));
    assert_eq!(cap("computer.hub.revoke"), Some(Capability::Admin));
}
