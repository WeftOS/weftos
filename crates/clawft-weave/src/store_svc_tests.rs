use std::sync::Arc;
use std::time::Duration;

use clawft_kernel::exposure::ExposureRegistry;
use serde_json::json;
use weftos_cogbus::{BusHooks, Event, InputSpec, InstanceSpec};
use weftos_store::service::Parties;
use weftos_store::{Access, Principal, StoreName};

use super::*;
use crate::store_rt::{mesh_token, open_service};

const P: &str = "01HZX3K9Q5M2N7P8R4T6V0WABC";

fn service() -> (Arc<StoreService>, tempfile::TempDir) {
    let d = tempfile::tempdir().unwrap();
    let svc = open_service("pg-1", d.path(), Arc::new(ExposureRegistry::new("pg-1")), None, None).unwrap();
    (svc, d)
}

fn project(svc: &StoreService) -> StoreName {
    let n = StoreName::project(&mesh_token(), P).unwrap();
    svc.ensure_store(&n, "t", Parties::defaults_for(&n)).unwrap();
    n
}

#[test]
fn a_cog_with_a_grant_uses_its_project_store_and_one_without_is_refused() {
    let (svc, _d) = service();
    let n = project(&svc);
    let cog = Principal::parse("cog:inst-1").unwrap();
    let put = |t: &str| call(&svc, "pg-1", "inst-1", Some(P), json!({"op": "put", "key": "a/b.txt", "text": t}));
    assert!(put("x").unwrap_err().to_lowercase().contains("grant") || put("x").is_err(), "no grant, no write");
    svc.grant(&n, cog, Access::Write, "t").unwrap();
    let m = put("hello").unwrap();
    assert_eq!(m["size"], 5);
    let g = call(&svc, "pg-1", "inst-1", Some(P), json!({"op": "get", "key": "a/b.txt"})).unwrap();
    assert_eq!(g["text"], "hello");
    let l = call(&svc, "pg-1", "inst-1", Some(P), json!({"op": "list", "prefix": "a/"})).unwrap();
    assert_eq!(l["objects"][0]["key"], "a/b.txt");
    assert_eq!(call(&svc, "pg-1", "inst-1", Some(P), json!({"op": "head", "key": "a/b.txt"})).unwrap()["size"], 5);
    // A second put with if_none_match is refused: the key exists.
    assert!(call(&svc, "pg-1", "inst-1", Some(P), json!({"op": "put", "key": "a/b.txt", "text": "y", "if_none_match": true})).is_err());
    call(&svc, "pg-1", "inst-1", Some(P), json!({"op": "delete", "key": "a/b.txt"})).unwrap();
    // A different cog has no grant on the same store.
    assert!(call(&svc, "pg-1", "inst-2", Some(P), json!({"op": "get", "key": "a/b.txt"})).is_err());
}

#[test]
fn a_cog_swaps_an_object_only_if_its_etag_still_matches() {
    let (svc, _d) = service();
    let n = project(&svc);
    svc.grant(&n, Principal::parse("cog:inst-1").unwrap(), Access::Write, "t").unwrap();
    let c = |b: serde_json::Value| call(&svc, "pg-1", "inst-1", Some(P), b);
    let v1 = c(json!({"op": "put", "key": "s.json", "text": "1"})).unwrap();
    let e1 = v1["etag"].as_str().unwrap().to_owned();
    let v2 = c(json!({"op": "put", "key": "s.json", "text": "2", "if_match": e1})).unwrap();
    // The first etag is stale now: a second writer holding it loses.
    let lost = c(json!({"op": "put", "key": "s.json", "text": "3", "if_match": e1})).unwrap_err();
    assert!(lost.contains("changed"), "{lost}");
    assert_eq!(c(json!({"op": "get", "key": "s.json"})).unwrap()["text"], "2");
    c(json!({"op": "put", "key": "s.json", "text": "3", "if_match": v2["etag"]})).unwrap();
    assert!(c(json!({"op": "put", "key": "s.json", "text": "x", "if_match": "a", "if_none_match": true})).is_err());
}

#[test]
fn bad_requests_are_errors_not_panics() {
    let (svc, _d) = service();
    let c = |b| call(&svc, "pg-1", "inst-1", None, b);
    assert!(c(json!({"op": "frob"})).unwrap_err().contains("op \"frob\""));
    assert!(c(json!({"op": "put", "key": "k"})).is_err(), "no payload");
    assert!(c(json!({"op": "put", "key": "k", "text": "a", "b64": "YQ=="})).is_err(), "two payloads");
    assert!(c(json!({"op": "get"})).is_err(), "no key");
    assert!(c(json!({"op": "get", "key": "k", "extra": 1})).is_err(), "unknown field");
    assert!(c(json!({"op": "get", "key": "k", "store": "not-a-store"})).is_err());
    assert!(call(&svc, "pg-1", "bad id!", None, json!({"op": "list"})).is_err(), "instance id is not a principal");
}

#[test]
fn an_object_over_the_reply_limit_is_an_error_not_a_truncation() {
    let (svc, _d) = service();
    let n = project(&svc);
    svc.grant(&n, Principal::parse("cog:i").unwrap(), Access::Write, "t").unwrap();
    svc.put(&weftos_store::service::Caller::local(Principal::parse("cog:i").unwrap()), &n, "big", &vec![7u8; 60_000], false).unwrap();
    let e = call(&svc, "pg-1", "i", Some(P), json!({"op": "get", "key": "big"})).unwrap_err();
    assert!(e.contains("over what a service reply can carry"), "{e}");
}

/// Through a real broker: a cog placed in the project calls `svc.store/<node>` and gets the object back.
#[tokio::test]
async fn a_cog_calls_the_store_through_the_broker() {
    let (svc, d) = service();
    let n = project(&svc);
    svc.grant(&n, Principal::parse("cog:caller-1").unwrap(), Access::Write, "t").unwrap();
    let b = crate::cogbus_serve::broker_in("pg-1", BusHooks::default(), d.path());
    attach_with(&b, "pg-1", svc.clone()).unwrap();
    let spec = InstanceSpec {
        instance_id: "caller-1".into(),
        cog_id: "caller".into(),
        project_id: Some(P.into()),
        uid: None,
        outputs: vec![],
        inputs: vec![InputSpec { topic: KIND.into(), schema: SCHEMA.into(), policy: Default::default() }],
    };
    let token = "cd".repeat(32);
    b.register(spec, &token).unwrap();
    let mut s = b.authenticate(&token, None).unwrap();
    let mut rx = s.take_pushed().unwrap();
    let topic = format!("{KIND}/{}", project_source(P));
    let mut ask = async |id: u64, body: serde_json::Value| {
        b.call_service(&s, id, &topic, SCHEMA, body, Some(5_000)).unwrap();
        match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.unwrap().unwrap() {
            Event::Ok { result, .. } => Ok(result["body"].clone()),
            Event::Error { message, .. } => Err(message),
            other => panic!("unexpected {other:?}"),
        }
    };
    let put = ask(1, json!({"op": "put", "key": "k", "text": "via the bus"})).await.unwrap();
    assert_eq!(put["size"], 11);
    assert_eq!(ask(2, json!({"op": "get", "key": "k"})).await.unwrap()["text"], "via the bus");
    // Disabling the grant refuses the next call, with the reason in the error.
    svc.set_grant_enabled(&n, &Principal::parse("cog:caller-1").unwrap(), false, "t").unwrap();
    assert!(ask(3, json!({"op": "get", "key": "k"})).await.is_err());
}

// ---- against a real RustFS ------------------------------------------------------------------
// WEFTOS_STORE_TEST_RUSTFS_ENV names the env file of a THROWAWAY RustFS; run with `--ignored`.
// Never point it at PG or a real mesh store.
#[test]
#[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
fn live_a_cog_uses_svc_store_on_rustfs_and_a_disabled_grant_is_refused() {
    let env = std::env::var_os("WEFTOS_STORE_TEST_RUSTFS_ENV").expect("WEFTOS_STORE_TEST_RUSTFS_ENV");
    let d = tempfile::tempdir().unwrap();
    // A fresh ULID per run, so reruns never collide on a bucket.
    let ulid = ulid::Ulid::new().to_string();
    let svc = open_service("pg-live", d.path(), Arc::new(ExposureRegistry::new("pg-live")), None, Some(env.into())).unwrap();
    let n = StoreName::project(&mesh_token(), &ulid).unwrap();
    svc.ensure_store(&n, "t", Parties::defaults_for(&n)).unwrap();
    let cog = Principal::parse("cog:live-1").unwrap();
    let c = |b: serde_json::Value| call(&svc, "pg-live", "live-1", Some(&ulid), b);
    assert!(c(json!({"op": "put", "key": "k.txt", "text": "no grant"})).is_err(), "no grant, no write");
    svc.grant(&n, cog.clone(), Access::Write, "t").unwrap();
    assert_eq!(c(json!({"op": "put", "key": "k.txt", "text": "hello"})).unwrap()["size"], 5);
    assert_eq!(c(json!({"op": "get", "key": "k.txt"})).unwrap()["text"], "hello");
    assert!(c(json!({"op": "put", "key": "k.txt", "text": "y", "if_none_match": true})).is_err());
    svc.set_grant_enabled(&n, &cog, false, "t").unwrap();
    assert!(c(json!({"op": "get", "key": "k.txt"})).is_err(), "a disabled grant refuses the next call");
    svc.set_grant_enabled(&n, &cog, true, "t").unwrap();
    c(json!({"op": "delete", "key": "k.txt"})).unwrap();
}
