use std::sync::Arc;

use ed25519_dalek::SigningKey;
use serde_json::json;
use weftos_store::FsBackend;
use weftos_store::audit::MemSink;
use weftos_store::service::OpenGate;

use super::*;
use crate::client_store::ClientStore;
use crate::store_account::ClientStoreSpec;
use crate::weftos_uri::Authority;

const ULID: &str = "01J8ZQ4W2X9K3M5N7P0R2T4V6Y";
const MS: u64 = 1_800_000_000_000;

fn clock() -> u64 {
    MS
}
fn key() -> SigningKey {
    SigningKey::from_bytes(&[1; 32])
}
fn client(d: &std::path::Path) -> ClientStore {
    let spec = ClientStoreSpec { account: "acme".into(), uid: 2102, root: d.to_path_buf(), listen: "127.0.0.1:9100".parse().unwrap() };
    let be = Arc::new(FsBackend::open(&d.join("data")).unwrap());
    ClientStore::open(spec, Authority([1; 32]), be.clone(), be, Arc::new(OpenGate), Arc::new(MemSink::default()), clock).unwrap()
}
fn name() -> String {
    format!("weftos://{}/projects/{ULID}/store", Authority([1; 32]))
}
fn local(p: &str) -> serde_json::Value {
    json!({"kind": "local", "principal": p})
}
fn weftos() -> serde_json::Value {
    json!({"kind": "weftos", "id": "weaver"})
}
fn foreign() -> serde_json::Value {
    json!({"kind": "foreign", "mesh": hex::encode([2u8; 32]), "id": "w"})
}

#[test]
fn status_hold_grant_put_get_ls_delete_as_a_local_principal() {
    let d = tempfile::tempdir().unwrap();
    let c = client(d.path());
    assert_eq!(run(&c, "store.client.status", json!({})).unwrap()["account"], "acme");
    assert_eq!(run(&c, "store.client.hold", json!({"ulid": ULID})).unwrap()["store"], name());
    run(&c, "store.client.grant", json!({"name": name(), "principal": "cog:c1", "access": "write"})).unwrap();
    let put = |k: &str| run(&c, "store.client.put", json!({"name": name(), "key": k, "as": local("cog:c1"), "text": "hello"}));
    put("a/b").unwrap();
    let g = run(&c, "store.client.get", json!({"name": name(), "key": "a/b", "as": local("cog:c1")})).unwrap();
    assert_eq!(g["text"], "hello");
    let l = run(&c, "store.client.ls", json!({"name": name(), "prefix": "a/", "as": local("cog:c1")})).unwrap();
    assert_eq!(l["objects"].as_array().unwrap().len(), 1);
    run(&c, "store.client.delete", json!({"name": name(), "key": "a/b", "as": local("cog:c1")})).unwrap();
    run(&c, "store.client.revoke", json!({"name": name(), "principal": "cog:c1"})).unwrap();
    assert!(put("c").is_err(), "revoked");
}

#[test]
fn the_weftos_account_is_refused_until_granted_over_rpc() {
    let d = tempfile::tempdir().unwrap();
    let c = client(d.path());
    run(&c, "store.client.hold", json!({"ulid": ULID})).unwrap();
    run(&c, "store.client.grant", json!({"name": name(), "principal": "cog:c1", "access": "write"})).unwrap();
    run(&c, "store.client.put", json!({"name": name(), "key": "k", "as": local("cog:c1"), "text": "v"})).unwrap();
    let get = || run(&c, "store.client.get", json!({"name": name(), "key": "k", "as": weftos()}));
    assert_eq!(get().unwrap_err(), "refused: refused");
    run(&c, "store.client.weftos_grant", json!({"name": name(), "id": "weaver", "access": "read"})).unwrap();
    assert_eq!(get().unwrap()["text"], "v");
    run(&c, "store.client.weftos_revoke", json!({"name": name(), "id": "weaver"})).unwrap();
    assert_eq!(get().unwrap_err(), "refused: refused");
    // A local principal cannot take the reserved name over RPC either.
    let r = run(&c, "store.client.grant", json!({"name": name(), "principal": "member:weftos-weaver", "access": "admin"}));
    assert!(r.is_err());
}

#[test]
fn cross_mesh_grant_over_rpc_and_a_forged_one_is_refused() {
    let d = tempfile::tempdir().unwrap();
    let c = client(d.path());
    run(&c, "store.client.hold", json!({"ulid": ULID})).unwrap();
    run(&c, "store.client.grant", json!({"name": name(), "principal": "cog:c1", "access": "write"})).unwrap();
    run(&c, "store.client.put", json!({"name": name(), "key": "k", "as": local("cog:c1"), "text": "v"})).unwrap();
    let get = || run(&c, "store.client.get", json!({"name": name(), "key": "k", "as": foreign()}));
    assert_eq!(get().unwrap_err(), "refused: refused");
    run(&c, "store.client.xmesh_trust", json!({"mesh": hex::encode([1u8; 32]), "key": hex::encode(key().verifying_key().to_bytes())})).unwrap();
    let g = CrossMeshGrant::sign(&key(), [1; 32], [2; 32], &name(), Access::Read, 1, MS / 1000 - 10, MS / 1000 + 100);
    run(&c, "store.client.xmesh_install", json!({"grant": g.to_json()})).unwrap();
    assert_eq!(get().unwrap()["text"], "v");
    let forged = CrossMeshGrant::sign(&SigningKey::from_bytes(&[9; 32]), [1; 32], [2; 32], &name(), Access::Admin, 2, MS / 1000 - 10, MS / 1000 + 100);
    assert_eq!(run(&c, "store.client.xmesh_install", json!({"grant": forged.to_json()})).unwrap_err(), "refused: refused");
    assert_eq!(run(&c, "store.client.sweep", json!({})).unwrap()["expired"], 0);
    run(&c, "store.client.xmesh_revoke", json!({"grant": g.to_json()})).unwrap();
    assert_eq!(get().unwrap_err(), "refused: refused");
}

#[test]
fn bad_input_is_an_error_not_a_panic() {
    let d = tempfile::tempdir().unwrap();
    let c = client(d.path());
    for (m, p) in [
        ("store.client.nope", json!({})),
        ("store.client.hold", json!({})),
        ("store.client.grant", json!({"name": name(), "principal": "cog:c1", "access": "root"})),
        ("store.client.get", json!({"name": name(), "key": "k"})),
        ("store.client.get", json!({"name": name(), "key": "k", "as": {"kind": "martian"}})),
        ("store.client.get", json!({"name": name(), "key": "k", "as": {"kind": "foreign", "mesh": "zz", "id": "x"}})),
        ("store.client.put", json!({"name": name(), "key": "k", "as": local("cog:c1")})),
        ("store.client.put", json!({"name": name(), "key": "k", "as": local("cog:c1"), "text": "a", "b64": "YQ=="})),
        ("store.client.xmesh_trust", json!({"mesh": "ab", "key": "cd"})),
        ("store.client.xmesh_install", json!({"grant": {"owner": "x"}})),
        ("store.client.status", json!({"surprise": 1})),
    ] {
        assert!(run(&c, m, p.clone()).is_err(), "{m} {p}");
    }
}

#[test]
fn a_grant_round_trips_through_json() {
    let g = CrossMeshGrant::sign(&key(), [1; 32], [2; 32], &name(), Access::Write, 7, 10, 20);
    assert_eq!(CrossMeshGrant::from_json(&g.to_json()).unwrap(), g);
}
