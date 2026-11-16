use std::sync::Arc;

use clawft_kernel::exposure::registry::ExposureRegistry;
use serde_json::json;
use weftos_store::{Access, FsBackend, Principal, StoreName, StoreService};

use super::*;
use crate::child_peer::PeerClass;
use crate::store_rt::{ActivitySink, ChainAudit, RegistryGate};

const PROJ: &str = "01JBXK2M3N4P5Q6R7S8T9V0W1X";
const OWN: u32 = 501;
const FOLLOWER_UID: u32 = 1001;
const WRITER_UID: u32 = 1002;

fn rig() -> (tempfile::TempDir, StoreService) {
    let d = tempfile::tempdir().unwrap();
    let reg = Arc::new(ExposureRegistry::new("pg-1"));
    let be = Arc::new(FsBackend::open(d.path()).unwrap());
    let svc = StoreService::open(
        weftos_store::service::Config { node_id: "pg-1".into(), state_path: None, clock: clawft_kernel::exposure::registry::now_ms },
        be.clone(),
        be,
        Arc::new(RegistryGate(reg.clone())),
        Arc::new(ActivitySink(reg)),
        Arc::new(ChainAudit(None)),
    )
    .unwrap();
    (d, svc)
}

fn p(s: &str) -> Principal {
    Principal::parse(s).unwrap()
}

/// A project store and a worktree store; `follower` reads the project, `writer` writes both.
fn stores(svc: &StoreService) -> (StoreName, StoreName) {
    let pr = StoreName::project("local", PROJ).unwrap();
    let wt = StoreName::worktree("local", PROJ, "feat-a").unwrap();
    for n in [&pr, &wt] {
        svc.create_store(n, "t", Default::default()).unwrap();
    }
    svc.grant(&pr, p("agent:follower"), Access::Read, "t").unwrap();
    svc.grant(&pr, p("agent:writer"), Access::Write, "t").unwrap();
    svc.grant(&wt, p("agent:writer"), Access::Write, "t").unwrap();
    (pr, wt)
}

fn map() -> PrincipalMap {
    PrincipalMap::parse(&format!("[uids]\n{FOLLOWER_UID} = \"agent:follower\"\n{WRITER_UID} = \"agent:writer\"\n"), OWN).unwrap()
}

fn caller(uid: u32) -> CallerCtx {
    let peer = if uid == OWN { PeerClass::Owner } else { PeerClass::OtherUid };
    CallerCtx::from_auth(None).with_peer(peer).with_peer_key(Some(crate::child_peer::peer_key(uid, None)))
}

fn who(uid: u32) -> StoreCaller {
    identify(Some(&map()), &caller(uid), OWN).unwrap()
}

fn wt(verb: &str, extra: serde_json::Value) -> (String, serde_json::Value) {
    let mut v = json!({"project": PROJ, "slug": "feat-a"});
    if let (Some(o), Some(e)) = (v.as_object_mut(), extra.as_object()) {
        o.extend(e.clone());
    }
    (format!("store.worktree.{verb}"), v)
}

#[test]
fn a_read_only_principal_is_refused_every_write_verb() {
    let (_d, svc) = rig();
    stores(&svc);
    let f = who(FOLLOWER_UID);
    assert_eq!(f, StoreCaller::Principal(p("agent:follower")));
    let pr = StoreName::project("local", PROJ).unwrap().to_string();
    let mut cases: Vec<(String, serde_json::Value)> = ["create", "publish", "claim", "promote", "orphan", "gc"].iter().map(|v| wt(v, json!({}))).collect();
    cases.push(("store.grant".into(), json!({"name": pr, "principal": "agent:follower", "access": "write"})));
    cases.push(("store.revoke".into(), json!({"name": pr, "principal": "agent:writer"})));
    for (m, params) in cases {
        let e = check(&svc, "local", &f, &m, &params).unwrap_err();
        assert!(!e.is_empty(), "{m} must be refused");
    }
    // It did not grant itself anything.
    assert!(!svc.holds_at_least(&p("agent:follower"), &StoreName::project("local", PROJ).unwrap(), Access::Write));
}

#[test]
fn a_read_only_principal_may_still_read() {
    let (_d, svc) = rig();
    stores(&svc);
    let f = who(FOLLOWER_UID);
    for v in ["follow", "browse", "search", "fetch", "leftovers", "sync-docs"] {
        let (m, params) = wt(v, json!({}));
        check(&svc, "local", &f, &m, &params).unwrap_or_else(|e| panic!("{v}: {e}"));
    }
}

#[test]
fn a_writer_is_held_to_its_grants_and_the_operator_is_not() {
    let (_d, svc) = rig();
    let (pr, _wt) = stores(&svc);
    let w = who(WRITER_UID);
    for v in ["create", "publish", "claim", "orphan", "promote"] {
        let (m, params) = wt(v, json!({}));
        check(&svc, "local", &w, &m, &params).unwrap_or_else(|e| panic!("{v}: {e}"));
    }
    // Write is not Admin: it may not grant, and `gc` is the operator's.
    let g = json!({"name": pr.to_string(), "principal": "agent:writer", "access": "admin"});
    assert!(check(&svc, "local", &w, "store.grant", &g).is_err());
    let (m, params) = wt("gc", json!({}));
    assert!(check(&svc, "local", &w, &m, &params).is_err());
    // The operator path is unchanged.
    let op = who(OWN);
    assert_eq!(op, StoreCaller::Operator);
    check(&svc, "local", &op, "store.grant", &g).unwrap();
    check(&svc, "local", &op, &m, &params).unwrap();
}

#[test]
fn a_disabled_or_missing_grant_or_a_suspended_principal_holds_nothing() {
    let (_d, svc) = rig();
    let (pr, _) = stores(&svc);
    let w = who(WRITER_UID);
    let (m, params) = wt("create", json!({}));
    svc.set_grant_enabled(&pr, &p("agent:writer"), false, "t").unwrap();
    assert!(check(&svc, "local", &w, &m, &params).is_err(), "disabled grant");
    svc.set_grant_enabled(&pr, &p("agent:writer"), true, "t").unwrap();
    check(&svc, "local", &w, &m, &params).unwrap();
    svc.suspend_principal(&p("agent:writer"), "t").unwrap();
    assert!(check(&svc, "local", &w, &m, &params).is_err(), "suspended");
}

#[test]
fn the_agent_label_must_be_the_verified_principal() {
    let (_d, svc) = rig();
    stores(&svc);
    let f = who(FOLLOWER_UID);
    // The follower claims to be the writer.
    let (m, params) = wt("follow", json!({"agent": "writer"}));
    let e = check(&svc, "local", &f, &m, &params).unwrap_err();
    assert!(e.contains("verified principal"), "{e}");
    let (m, params) = wt("follow", json!({"agent": "follower"}));
    check(&svc, "local", &f, &m, &params).unwrap();
    // store put as another principal is refused before the store's own check.
    let put = json!({"name": StoreName::project("local", PROJ).unwrap().to_string(), "key": "k", "principal": "agent:writer", "text": "x"});
    assert!(check(&svc, "local", &f, "store.put", &put).is_err());
}

#[test]
fn every_other_store_verb_is_the_operators() {
    let (_d, svc) = rig();
    stores(&svc);
    let w = who(WRITER_UID);
    for m in ["store.ls", "store.create", "store.promote", "store.archive", "store.archive.gc", "store.node.retire", "store.drive.sync", "store.client.get"] {
        assert!(check(&svc, "local", &w, m, &json!({})).is_err(), "{m}");
    }
}

#[test]
fn identify_fails_closed() {
    let m = map();
    // No map: everyone who got this far is the operator (unchanged behaviour).
    assert_eq!(identify(None, &caller(FOLLOWER_UID), OWN).unwrap(), StoreCaller::Operator);
    // An unmapped uid, a supervised child and a peer without a uid are refused.
    assert!(identify(Some(&m), &caller(4242), OWN).is_err());
    let child = CallerCtx::from_auth(None).with_peer(PeerClass::Child).with_peer_key(Some(crate::child_peer::peer_key(OWN, Some(7))));
    assert!(identify(Some(&m), &child, OWN).is_err());
    assert!(identify(Some(&m), &CallerCtx::from_auth(None), OWN).is_err());
    // A request cannot name its own uid: the label in the auth field is not read.
    let spoof = CallerCtx::from_auth(Some("uid:501".into())).with_peer(PeerClass::OtherUid).with_peer_key(Some(crate::child_peer::peer_key(FOLLOWER_UID, None)));
    assert_eq!(identify(Some(&m), &spoof, OWN).unwrap(), StoreCaller::Principal(p("agent:follower")));
}

#[test]
fn the_map_file_is_checked() {
    assert!(PrincipalMap::parse("[uids]\n501 = \"agent:x\"\n", OWN).is_err(), "the daemon's own uid is the operator");
    assert!(PrincipalMap::parse("[uids]\nbob = \"agent:x\"\n", OWN).is_err());
    assert!(PrincipalMap::parse("[uids]\n1 = \"nobody\"\n", OWN).is_err());
    assert!(PrincipalMap::parse("[uids]\n1 = 7\n", OWN).is_err());
    assert_eq!(PrincipalMap::parse("", OWN).unwrap(), PrincipalMap::default());
    let d = tempfile::tempdir().unwrap();
    assert!(PrincipalMap::load(d.path(), OWN).unwrap().is_none());
    std::fs::write(d.path().join(MAP_FILE), "[uids]\n1001 = \"agent:follower\"\n").unwrap();
    assert_eq!(PrincipalMap::load(d.path(), OWN).unwrap().unwrap().get(1001), Some(&p("agent:follower")));
}

#[test]
fn gate_is_inert_without_a_map_and_decides_with_one() {
    let _g = TEST_MAP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    install(None);
    let f = caller(FOLLOWER_UID);
    assert!(gate(&f, "store.grant", &json!({})).is_none(), "no map: the ordinary checks decide");
    install(Some(map()));
    assert!(gate(&f, "kernel.status", &json!({})).is_none(), "not a store verb");
    // With a map and no running store the gate refuses rather than guessing.
    let r = gate(&f, "store.grant", &json!({"name": "x"})).expect("decided");
    assert!(r.is_err());
    assert!(gate(&caller(4242), "store.ls", &json!({})).expect("decided").is_err(), "unmapped uid");
    install(None);
}
