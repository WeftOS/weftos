use std::sync::Arc;

use clawft_kernel::exposure::ExposureRegistry;
use serde_json::json;
use weftos_store::{FsBackend, StoreService};

use super::*;
use crate::store_rt::{ActivitySink, ChainAudit, RegistryGate};

const NODE: &str = "weftos://local/nodes/pg-1/store";

fn rig() -> (tempfile::TempDir, Arc<ExposureRegistry>, StoreService, Arc<std::sync::Mutex<Vec<String>>>) {
    let d = tempfile::tempdir().unwrap();
    let reg = Arc::new(ExposureRegistry::new("pg-1"));
    let be = Arc::new(FsBackend::open(d.path()).unwrap());
    let chain = Arc::new(std::sync::Mutex::new(Vec::new()));
    let c2 = chain.clone();
    let f: clawft_kernel::exposure::AuditFn = Arc::new(move |k, _| c2.lock().unwrap().push(k.to_owned()));
    let svc = StoreService::open(
        weftos_store::service::Config { node_id: "pg-1".into(), state_path: Some(d.path().join("state.json")), clock: clawft_kernel::exposure::registry::now_ms },
        be.clone(),
        be,
        Arc::new(RegistryGate(reg.clone())),
        Arc::new(ActivitySink(reg.clone())),
        Arc::new(ChainAudit(Some(f))),
    )
    .unwrap();
    (d, reg, svc, chain)
}

#[test]
fn daemon_creates_stores_a_cog_write_is_in_the_activity_log_and_a_disabled_grant_refuses() {
    let (_d, reg, svc, chain) = rig();
    run(&svc, "store.create", json!({"name": NODE})).unwrap();
    run(&svc, "store.create", json!({"name": "weftos://local/projects/01JBXK2M3N4P5Q6R7S8T9V0W1X/store", "read": ["member:ann"]})).unwrap();
    assert!(reg.get("store.n-pg-1").is_some(), "the store is an ADR-120 exposure");
    let ls = run(&svc, "store.ls", json!({})).unwrap();
    assert_eq!(ls["stores"].as_array().unwrap().len(), 2);

    run(&svc, "store.grant", json!({"name": NODE, "principal": "cog:c1", "access": "write"})).unwrap();
    let put = run(&svc, "store.put", json!({"name": NODE, "key": "agents/c1/a.txt", "principal": "cog:c1", "text": "hi"})).unwrap();
    assert_eq!(put["size"], 2);
    let log = reg.activity().query(Some("store.n-pg-1"), 50);
    let rec = log.iter().find(|r| r.caller_user.as_deref() == Some("PutObject agents/c1/a.txt")).expect("the write is in the activity log");
    assert_eq!((rec.caller_node.as_str(), rec.bytes_in, rec.outcome.as_str()), ("cog:c1", 2, "ok"));
    reg.activity().verify().unwrap();

    let got = run(&svc, "store.get", json!({"name": NODE, "key": "agents/c1/a.txt", "principal": "cog:c1"})).unwrap();
    assert_eq!(got["text"], "hi");
    let ls = run(&svc, "store.ls", json!({"name": NODE})).unwrap();
    assert_eq!(ls["objects"][0]["key"], "agents/c1/a.txt");

    run(&svc, "store.grant", json!({"name": NODE, "principal": "cog:c1", "enabled": false})).unwrap();
    let e = run(&svc, "store.put", json!({"name": NODE, "key": "k", "principal": "cog:c1", "text": "x"})).unwrap_err();
    assert!(e.contains("no enabled grant"), "{e}");
    let refused = reg.activity().query(Some("store.n-pg-1"), 50).into_iter().find(|r| r.outcome.starts_with("refused:")).expect("the refusal is logged");
    assert_eq!(refused.caller_node, "cog:c1");
    let kinds = chain.lock().unwrap().clone();
    for k in ["store.create", "store.grant", "store.grant.disable"] {
        assert!(kinds.iter().any(|x| x == k), "{k} not on the chain: {kinds:?}");
    }
}

#[test]
fn bad_input_is_an_error_not_a_panic() {
    let (_d, _r, svc, _c) = rig();
    for (m, p) in [
        ("store.create", json!({})),
        ("store.create", json!({"name": "http://x"})),
        ("store.grant", json!({"name": NODE, "principal": "cog:c1", "access": "write"})),
        ("store.put", json!({"name": NODE, "key": "k", "principal": "cog:c1"})),
        ("store.promote", json!({"name": NODE, "prefixes": []})),
        ("store.ls", json!({"bogus": 1})),
        ("store.nope", json!({})),
    ] {
        assert!(run(&svc, m, p.clone()).is_err(), "{m} {p}");
    }
    run(&svc, "store.create", json!({"name": NODE})).unwrap();
    assert!(run(&svc, "store.grant", json!({"name": NODE, "principal": "cog:c1"})).is_err(), "needs access or enabled");
    assert!(run(&svc, "store.grant", json!({"name": NODE, "principal": "bad", "access": "read"})).is_err());
    assert!(run(&svc, "store.grant", json!({"name": NODE, "principal": "cog:c1", "access": "owner"})).is_err());
    assert!(run(&svc, "store.put", json!({"name": NODE, "key": "k", "principal": "cog:c1", "text": "a", "b64": "YQ=="})).is_err());
}

#[test]
fn archive_and_revoke_verbs() {
    let (_d, _r, svc, _c) = rig();
    run(&svc, "store.create", json!({"name": NODE, "write": ["cog:c1"]})).unwrap();
    run(&svc, "store.put", json!({"name": NODE, "key": "k", "principal": "cog:c1", "b64": "YQ=="})).unwrap();
    run(&svc, "store.revoke", json!({"name": NODE, "principal": "cog:c1"})).unwrap();
    assert!(run(&svc, "store.get", json!({"name": NODE, "key": "k", "principal": "cog:c1"})).is_err());
    let a = run(&svc, "store.archive", json!({"name": NODE, "delete": true})).unwrap();
    assert_eq!((a["archived_objects"].as_u64(), a["deleted"].as_bool()), (Some(1), Some(true)));
    assert!(run(&svc, "store.ls", json!({})).unwrap()["stores"].as_array().unwrap().is_empty());
}

#[test]
fn archive_gc_is_a_dry_run_by_default_and_expires_nothing_unless_days_are_given() {
    let (_d, _r, svc, _c) = rig();
    run(&svc, "store.create", json!({"name": NODE, "write": ["cog:c1"]})).unwrap();
    run(&svc, "store.put", json!({"name": NODE, "key": "k", "principal": "cog:c1", "b64": "YQ=="})).unwrap();
    run(&svc, "store.archive", json!({"name": NODE, "delete": true})).unwrap();
    // No days: nothing is old enough, whatever the flags.
    let none = run(&svc, "store.archive.gc", json!({"confirmed": true})).unwrap();
    assert_eq!((none["expired"].as_array().unwrap().len(), none["kept"].as_u64(), none["deleted_objects"].as_u64()), (0, Some(1), Some(0)));
    // Days given but the archive is minutes old: kept.
    let young = run(&svc, "store.archive.gc", json!({"node_days": 30, "confirmed": true})).unwrap();
    assert_eq!(young["kept"].as_u64(), Some(1));
    assert!(run(&svc, "store.archive.gc", json!({"node_days": 0})).is_err(), "zero days is refused");
    assert!(run(&svc, "store.archive.gc", json!({"bogus": 1})).is_err(), "unknown params are refused");
    let rep = run(&svc, "store.archive.gc", json!({})).unwrap();
    assert_eq!(rep["dry_run"], true);
}

#[test]
fn archive_keep_sets_and_clears_a_per_store_override_and_refuses_bad_ones() {
    let (_d, _r, svc, _c) = rig();
    run(&svc, "store.create", json!({"name": NODE})).unwrap();
    let set = run(&svc, "store.archive.keep", json!({"name": NODE, "days": 9})).unwrap();
    assert_eq!(set["keep"], json!({"days": 9}));
    assert_eq!(run(&svc, "store.archive.keep", json!({"name": NODE, "forever": true})).unwrap()["keep"], json!("forever"));
    assert!(run(&svc, "store.archive.keep", json!({"name": NODE, "days": 9, "forever": true})).is_err(), "both is refused");
    assert!(run(&svc, "store.archive.keep", json!({"name": NODE, "days": 0})).is_err(), "zero days is refused");
    assert!(run(&svc, "store.archive.keep", json!({"days": 3})).is_err(), "needs a store");
    assert_eq!(run(&svc, "store.archive.keep", json!({"name": NODE})).unwrap()["keep"], json!(null));
}
