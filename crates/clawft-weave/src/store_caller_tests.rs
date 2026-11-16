use std::sync::Arc;

use clawft_kernel::exposure::registry::ExposureRegistry;
use serde_json::json;
use weftos_store::{Access, FsBackend, Principal, StoreName, StoreService};

use super::*;
use crate::child_peer::PeerClass;
use crate::store_rt::{ActivitySink, ChainAudit, RegistryGate};

pub(super) const PROJ: &str = "01JBXK2M3N4P5Q6R7S8T9V0W1X";
pub(super) const OWN: u32 = 501;
pub(super) const FOLLOWER_UID: u32 = 1001;
pub(super) const WRITER_UID: u32 = 1002;

pub(super) fn rig() -> (tempfile::TempDir, StoreService) {
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

pub(super) fn p(s: &str) -> Principal {
    Principal::parse(s).unwrap()
}

/// A project store and a worktree store; `follower` reads the project, `writer` writes both.
pub(super) fn stores(svc: &StoreService) -> (StoreName, StoreName) {
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

pub(super) fn map() -> PrincipalMap {
    PrincipalMap::parse(&format!("[uids]\n{FOLLOWER_UID} = \"agent:follower\"\n{WRITER_UID} = \"agent:writer\"\n"), OWN).unwrap()
}

pub(super) fn caller(uid: u32) -> CallerCtx {
    let peer = if uid == OWN { PeerClass::Owner } else { PeerClass::OtherUid };
    CallerCtx::from_auth(None).with_peer(peer).with_peer_key(Some(crate::child_peer::peer_key(uid, None)))
}

pub(super) fn who(uid: u32) -> StoreCaller {
    identify(Some(&map()), &caller(uid), OWN).unwrap()
}

/// A worktree call as `who` makes it: the agent label is the full verified principal (card 6f0bc9c8, M1).
pub(super) fn wt_as(who: &StoreCaller, verb: &str, extra: serde_json::Value) -> (String, serde_json::Value) {
    let (m, mut v) = wt(verb, extra);
    if let StoreCaller::Principal(p) = who
        && v.get("agent").is_none()
    {
        v["agent"] = json!(p.to_string());
    }
    (m, v)
}

pub(super) fn wt(verb: &str, extra: serde_json::Value) -> (String, serde_json::Value) {
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
    let mut cases: Vec<(String, serde_json::Value)> = ["create", "publish", "claim", "promote", "orphan", "gc"].iter().map(|v| wt_as(&f, v, json!({}))).collect();
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
fn a_read_only_principal_may_still_read_what_does_not_touch_the_host() {
    let (_d, svc) = rig();
    stores(&svc);
    let f = who(FOLLOWER_UID);
    for v in ["browse", "search", "leftovers", "buckets", "usage", "overlaps"] {
        let (m, params) = wt_as(&f, v, json!({}));
        check(&svc, "local", &f, &m, &params).unwrap_or_else(|e| panic!("{v}: {e}"));
    }
}

/// H1 (confused deputy): these verbs open a path the CALLER names, as the daemon's uid. A mapped
/// principal must never reach them, whatever it holds on the store.
#[test]
fn a_principal_is_refused_every_verb_that_takes_a_host_path() {
    let (_d, svc) = rig();
    stores(&svc);
    let cases = [
        ("fetch", json!({"dest": "/Users/op/Library/LaunchAgents", "force": true})),
        ("fetch", json!({"dest": "/x", "bundle_out": "/Users/op/.zshrc"})),
        ("follow", json!({"state": "/Users/op/anywhere.json"})),
        ("sync-docs", json!({"cache": "/Users/op/docs"})),
        ("publish", json!({"root": "/tmp/a", "paths_z": "/tmp/a.z", "bundle": "/Users/op/.ssh/id_ed25519"})),
    ];
    for uid in [FOLLOWER_UID, WRITER_UID] {
        let who = who(uid);
        for (verb, extra) in &cases {
            let (m, params) = wt_as(&who, verb, extra.clone());
            let e = check(&svc, "local", &who, &m, &params).expect_err(&format!("uid {uid}: {m} {extra}"));
            assert!(e.contains("operator"), "{m}: {e}");
        }
        // The same verbs with no path are refused too: the verb itself is the operator's.
        for verb in ["fetch", "follow", "sync-docs", "publish"] {
            let (m, params) = wt_as(&who, verb, json!({}));
            assert!(check(&svc, "local", &who, &m, &params).is_err(), "uid {uid}: bare {m}");
        }
        // A path field smuggled onto a verb that does not take one is refused as well.
        for field in ["root", "dest", "cache", "state", "bundle", "bundle_out", "paths_z", "uncommitted_z", "changes_z"] {
            let (m, params) = wt_as(&who, "browse", json!({ field: "/Users/op/x" }));
            assert!(check(&svc, "local", &who, &m, &params).is_err(), "uid {uid}: browse with {field}");
        }
    }
    // The operator still has every one of them.
    let op = who(OWN);
    for (verb, extra) in &cases {
        let (m, params) = wt_as(&op, verb, extra.clone());
        check(&svc, "local", &op, &m, &params).unwrap();
    }
}

#[test]
fn a_writer_is_held_to_its_grants_and_the_operator_is_not() {
    let (_d, svc) = rig();
    let (pr, _wt) = stores(&svc);
    let w = who(WRITER_UID);
    for v in ["create", "claim", "orphan", "promote"] {
        let (m, params) = wt_as(&w, v, json!({}));
        check(&svc, "local", &w, &m, &params).unwrap_or_else(|e| panic!("{v}: {e}"));
    }
    // Write is not Admin: it may not grant, and `gc` is the operator's.
    let g = json!({"name": pr.to_string(), "principal": "agent:writer", "access": "admin"});
    assert!(check(&svc, "local", &w, "store.grant", &g).is_err());
    let (m, params) = wt_as(&w, "gc", json!({}));
    assert!(check(&svc, "local", &w, &m, &params).is_err());
    // The operator path is unchanged.
    let op = who(OWN);
    assert_eq!(op, StoreCaller::Operator);
    check(&svc, "local", &op, "store.grant", &g).unwrap();
    check(&svc, "local", &op, &m, &params).unwrap();
}

/// L2: `promote` rewrites the worktree's handoff and landed state, so it needs write on the worktree.
#[test]
fn promote_needs_write_on_the_worktree_as_well_as_the_project() {
    let (_d, svc) = rig();
    let (pr, wtn) = stores(&svc);
    let pw = StoreCaller::Principal(p("agent:pw"));
    svc.grant(&pr, p("agent:pw"), Access::Write, "t").unwrap();
    svc.grant(&wtn, p("agent:pw"), Access::Read, "t").unwrap();
    let (m, params) = wt_as(&pw, "promote", json!({}));
    assert!(check(&svc, "local", &pw, &m, &params).is_err(), "project write + worktree read is not enough");
    svc.grant(&wtn, p("agent:pw"), Access::Write, "t").unwrap();
    check(&svc, "local", &pw, &m, &params).unwrap();
}

#[test]
fn a_disabled_or_missing_grant_or_a_suspended_principal_holds_nothing() {
    let (_d, svc) = rig();
    let (pr, _) = stores(&svc);
    let w = who(WRITER_UID);
    let (m, params) = wt_as(&w, "create", json!({}));
    svc.set_grant_enabled(&pr, &p("agent:writer"), false, "t").unwrap();
    assert!(check(&svc, "local", &w, &m, &params).is_err(), "disabled grant");
    svc.set_grant_enabled(&pr, &p("agent:writer"), true, "t").unwrap();
    check(&svc, "local", &w, &m, &params).unwrap();
    svc.suspend_principal(&p("agent:writer"), "t").unwrap();
    assert!(check(&svc, "local", &w, &m, &params).is_err(), "suspended");
}

#[test]
fn the_agent_label_must_be_the_full_verified_principal() {
    let (_d, svc) = rig();
    stores(&svc);
    let f = who(FOLLOWER_UID);
    // The follower claims to be the writer.
    let (m, params) = wt("browse", json!({"agent": "agent:writer"}));
    let e = check(&svc, "local", &f, &m, &params).unwrap_err();
    assert!(e.contains("verified principal"), "{e}");
    // M1: an absent or empty label is refused (it would be recorded as the literal "agent"), and so
    // are the bare id and the same id under another kind.
    for bad in [json!({}), json!({"agent": ""}), json!({"agent": null}), json!({"agent": "follower"}), json!({"agent": "cog:follower"}), json!({"agent": "member:follower"})] {
        let (m, params) = wt("browse", bad.clone());
        assert!(check(&svc, "local", &f, &m, &params).is_err(), "{bad}");
    }
    let (m, params) = wt("browse", json!({"agent": "agent:follower"}));
    check(&svc, "local", &f, &m, &params).unwrap();
    // The operator needs no label.
    let (m, params) = wt("browse", json!({}));
    check(&svc, "local", &StoreCaller::Operator, &m, &params).unwrap();
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
}

#[cfg(unix)]
fn euid() -> u32 {
    nix::unistd::geteuid().as_raw()
}

#[cfg(unix)]
fn map_file(mode: u32) -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let d = tempfile::tempdir().unwrap();
    let f = d.path().join(MAP_FILE);
    std::fs::write(&f, "[uids]\n40001 = \"agent:follower\"\n").unwrap();
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(mode)).unwrap();
    d
}

/// M2: the file is trusted only when the daemon's uid owns it and nobody else can write it.
#[cfg(unix)]
#[test]
fn the_map_file_must_be_owned_by_the_daemon_and_not_writable_by_others() {
    let d = tempfile::tempdir().unwrap();
    assert!(PrincipalMap::load(d.path(), euid()).unwrap().is_none(), "absent is Ok(None) here; the enable switch decides what absent means");
    for ok in [0o600, 0o640, 0o644] {
        let d = map_file(ok);
        assert_eq!(PrincipalMap::load(d.path(), euid()).unwrap().unwrap().get(40001), Some(&p("agent:follower")), "{ok:o}");
    }
    for bad in [0o620, 0o660, 0o602, 0o666, 0o666 | 0o1000] {
        let d = map_file(bad);
        let e = PrincipalMap::load(d.path(), euid()).unwrap_err();
        assert!(e.contains("writable"), "{bad:o}: {e}");
    }
    // Owned by someone other than the daemon's uid.
    let d = map_file(0o600);
    let e = PrincipalMap::load(d.path(), euid() + 1).unwrap_err();
    assert!(e.contains("owned"), "{e}");
    // A directory or a symlink to nowhere is not a map file.
    let d = tempfile::tempdir().unwrap();
    std::fs::create_dir(d.path().join(MAP_FILE)).unwrap();
    assert!(PrincipalMap::load(d.path(), euid()).is_err());
}

/// M2: the mode is on only when the operator switches it on; once on, a file that is missing or
/// unusable stops the store and leaves the deny-everyone-but-the-operator map installed.
#[cfg(unix)]
#[test]
fn principal_mode_needs_the_switch_and_fails_closed_once_on() {
    let _g = TEST_MAP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let f = caller(FOLLOWER_UID);
    let gate_refuses = || gate(&f, "store.ls", &json!({})).is_some_and(|r| r.is_err());
    // A file alone does not turn the mode on.
    let d = map_file(0o600);
    configure(d.path(), euid(), false).unwrap();
    assert!(gate(&f, "store.ls", &json!({})).is_none(), "switch off: the ordinary checks decide");
    // On, with a usable file: installed.
    configure(d.path(), euid(), true).unwrap();
    assert!(installed().is_some_and(|m| m.get(40001).is_some()));
    // On, but the file is gone: an error, and the gate refuses rather than opening up.
    install(None);
    let empty = tempfile::tempdir().unwrap();
    let e = configure(empty.path(), euid(), true).unwrap_err();
    assert!(e.contains(MAP_FILE), "{e}");
    assert!(gate_refuses(), "a missing map must not mean open mode");
    // On, but the file is writable by others: same.
    install(None);
    let bad = map_file(0o666);
    assert!(configure(bad.path(), euid(), true).is_err());
    assert!(gate_refuses());
    // On, but it does not parse: same.
    install(None);
    let junk = tempfile::tempdir().unwrap();
    std::fs::write(junk.path().join(MAP_FILE), "[uids\n").unwrap();
    assert!(configure(junk.path(), euid(), true).is_err());
    assert!(gate_refuses());
    install(None);
}

#[test]
fn the_switch_reads_the_environment_value() {
    for on in ["1", "true", "TRUE", "on", "yes"] {
        assert!(enabled_value(Some(on)), "{on}");
    }
    for off in ["", "0", "false", "off", "no", "banana"] {
        assert!(!enabled_value(Some(off)), "{off}");
    }
    assert!(!enabled_value(None));
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
