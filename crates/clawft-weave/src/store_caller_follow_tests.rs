//! Tests for card 5e5b5d23, the follow-up to the store caller identity work (card 6f0bc9c8): `gc`
//! for a project admin, `store.view.*` and `store.drive.*` under the same caller check, and the
//! Lows N1, N3 and N4 from review round 2.

use serde_json::{Value, json};
use weftos_store::promote::{WorktreeMeta, write_meta};
use weftos_store::{Access, StoreName};

use super::tests::{FOLLOWER_UID, PROJ, WRITER_UID, p, rig, stores, who, wt, wt_as};
use super::*;
use crate::store_worktree::Op;
use crate::store_worktree_paths::Guard;

fn pr_name() -> String {
    StoreName::project("local", PROJ).unwrap().to_string()
}

/// The call as `who` makes it: the store named, and the agent label the full verified principal.
fn call(who: &StoreCaller, method: &str, mut params: Value) -> (String, Value) {
    params["name"] = json!(pr_name());
    if let StoreCaller::Principal(pr) = who {
        params["agent"] = json!(pr.to_string());
    }
    (method.to_owned(), params)
}

// ---- (a) gc ------------------------------------------------------------------------------------

#[test]
fn gc_is_open_to_a_principal_with_admin_on_the_project_store_and_to_no_one_else() {
    let (_d, svc) = rig();
    let (pr, wtn) = stores(&svc);
    let admin = StoreCaller::Principal(p("agent:admin"));
    svc.grant(&pr, p("agent:admin"), Access::Admin, "t").unwrap();
    let (m, params) = wt_as(&admin, "gc", json!({"confirmed": true, "keep_days": 14}));
    check(&svc, "local", &admin, &m, &params).expect("admin on the project store may gc");
    let (m, dry) = wt_as(&admin, "gc", json!({}));
    check(&svc, "local", &admin, &m, &dry).expect("a dry run too");
    // Write is not Admin; read is not Admin; no grant is not Admin.
    for uid in [WRITER_UID, FOLLOWER_UID] {
        let c = who(uid);
        let (m, params) = wt_as(&c, "gc", json!({"confirmed": true}));
        assert!(check(&svc, "local", &c, &m, &params).is_err(), "uid {uid}");
    }
    let nobody = StoreCaller::Principal(p("agent:nobody"));
    let (m, params) = wt_as(&nobody, "gc", json!({}));
    assert!(check(&svc, "local", &nobody, &m, &params).is_err());
    // Admin on a worktree store is not admin on the project: gc sweeps the whole project.
    let wa = StoreCaller::Principal(p("agent:wtadmin"));
    svc.grant(&wtn, p("agent:wtadmin"), Access::Admin, "t").unwrap();
    svc.grant(&pr, p("agent:wtadmin"), Access::Write, "t").unwrap();
    let (m, params) = wt_as(&wa, "gc", json!({}));
    assert!(check(&svc, "local", &wa, &m, &params).is_err());
    // A disabled admin grant holds nothing.
    svc.set_grant_enabled(&pr, &p("agent:admin"), false, "t").unwrap();
    let (m, params) = wt_as(&admin, "gc", json!({}));
    assert!(check(&svc, "local", &admin, &m, &params).is_err(), "disabled grant");
    // The rules that apply to every worktree verb still apply to gc: the label and no host path.
    svc.set_grant_enabled(&pr, &p("agent:admin"), true, "t").unwrap();
    let (m, params) = wt("gc", json!({"agent": "agent:someone-else"}));
    assert!(check(&svc, "local", &admin, &m, &params).is_err(), "wrong label");
    let (m, params) = wt_as(&admin, "gc", json!({"root": "/Users/op"}));
    assert!(check(&svc, "local", &admin, &m, &params).is_err(), "host path");
}

/// M1 (review of 08eee8a0a): `gc` retires stores the caller holds nothing on and `archive_keep_days`
/// hard-deletes archives. A principal may run neither of the two knobs that make that possible.
#[test]
fn a_principals_gc_cannot_shorten_the_keep_period_or_expire_archives() {
    let (_d, svc) = rig();
    let (pr, _wtn) = stores(&svc);
    let admin = StoreCaller::Principal(p("agent:admin"));
    svc.grant(&pr, p("agent:admin"), Access::Admin, "t").unwrap();
    let ok = |extra: Value| {
        let (m, params) = wt_as(&admin, "gc", extra);
        check(&svc, "local", &admin, &m, &params)
    };
    // Archive expiry is the operator's, whatever the value and whether or not it is a dry run.
    for days in [json!(0), json!(1), json!(3650)] {
        for confirmed in [true, false] {
            let e = ok(json!({"archive_keep_days": days, "confirmed": confirmed})).expect_err(&format!("archive_keep_days {days}"));
            assert!(e.contains("archive_keep_days") && e.contains("operator"), "{e}");
        }
    }
    // The landed-worktree keep period has a floor: the default (14 days).
    for days in [json!(0), json!(1), json!(13)] {
        let e = ok(json!({"keep_days": days, "confirmed": true})).expect_err(&format!("keep_days {days}"));
        assert!(e.contains("keep_days"), "{e}");
    }
    assert!(ok(json!({"keep_days": "0", "confirmed": true})).is_err(), "not a number");
    assert!(ok(json!({"keep_days": -1, "confirmed": true})).is_err(), "negative");
    // The floor and above, and no value at all, are fine.
    for extra in [json!({"keep_days": 14, "confirmed": true}), json!({"keep_days": 400}), json!({"confirmed": true}), json!({"keep_days": null, "archive_keep_days": null})] {
        ok(extra.clone()).unwrap_or_else(|e| panic!("{extra}: {e}"));
    }
    // The operator keeps both.
    for extra in [json!({"keep_days": 0, "confirmed": true}), json!({"archive_keep_days": 0, "confirmed": true})] {
        let (m, params) = wt("gc", extra);
        check(&svc, "local", &StoreCaller::Operator, &m, &params).unwrap();
    }
}

// ---- (b) store.view.* and store.drive.* --------------------------------------------------------

const R: Access = Access::Read;
const W: Access = Access::Write;

/// Every verb with the params that name no path on the host, and what it needs on the store.
fn view_and_drive() -> Vec<(&'static str, Value, Access)> {
    vec![
        ("store.view.open", json!({"spec": {"includes": []}}), W),
        ("store.view.open", json!({"spec": {"includes": []}, "read_only": true}), R),
        ("store.view.status", json!({}), R),
        ("store.view.refresh", json!({}), R),
        ("store.view.publish", json!({}), W),
        ("store.view.resolve", json!({"path": "a", "keep": "local"}), W),
        ("store.view.renew", json!({}), W),
        ("store.view.close", json!({"discard": true}), W),
        ("store.drive.materialise", json!({}), R),
        ("store.drive.sync-down", json!({}), R),
        ("store.drive.sync-up", json!({}), W),
        ("store.drive.resolve", json!({"path": "a", "keep": "local"}), W),
        ("store.drive.watch", json!({}), R),
    ]
}

#[test]
fn view_and_drive_verbs_are_decided_by_the_principals_grants() {
    let (_d, svc) = rig();
    stores(&svc);
    let follower = who(FOLLOWER_UID);
    let writer = who(WRITER_UID);
    let nobody = StoreCaller::Principal(p("agent:nobody"));
    for (method, extra, need) in view_and_drive() {
        let (m, params) = call(&follower, method, extra.clone());
        let r = check(&svc, "local", &follower, &m, &params);
        if need == R {
            let e = r.expect_err(&format!("follower {m} {extra}"));
            assert!(e.contains("for the operator") && !e.contains("does not hold"), "{m}: {e}");
        } else {
            let e = r.expect_err(&format!("follower {m} writes"));
            assert!(e.contains("does not hold"), "{m}: {e}");
        }
        // A grant is necessary but no longer enough: the call names no directory, and the daemon would
        // open one as its own uid, so it is refused at the gate, before any handler records anything.
        let (m, params) = call(&writer, method, extra.clone());
        let e = check(&svc, "local", &writer, &m, &params).expect_err(&format!("writer {m} {extra}"));
        assert!(e.contains("for the operator") && !e.contains("does not hold"), "{m}: {e}");
        let (m, params) = call(&nobody, method, extra.clone());
        assert!(check(&svc, "local", &nobody, &m, &params).is_err(), "no grant: {m}");
        // The store must be named, or there is nothing to check the grant against.
        let (m, mut params) = call(&writer, method, extra.clone());
        params.as_object_mut().unwrap().remove("name");
        let e = check(&svc, "local", &writer, &m, &params).unwrap_err();
        assert!(e.contains("name"), "{m}: {e}");
        // The label is the verified principal in full.
        let (m, mut params) = call(&writer, method, extra);
        params["agent"] = json!("agent:follower");
        assert!(check(&svc, "local", &writer, &m, &params).is_err(), "{m}: another principal's label");
    }
    // A verb this table does not know stays the operator's.
    let (m, params) = call(&writer, "store.view.frobnicate", json!({}));
    assert!(check(&svc, "local", &writer, &m, &params).unwrap_err().contains("operator"));
    // The operator still has all of them, and needs no label or grant.
    for (method, extra, _) in view_and_drive() {
        check(&svc, "local", &StoreCaller::Operator, method, &extra).unwrap();
    }
}

/// H1 for these verbs: a directory the caller names is opened by the daemon as its own uid.
#[test]
fn view_and_drive_refuse_a_path_on_the_daemons_host() {
    let (_d, svc) = rig();
    stores(&svc);
    let writer = who(WRITER_UID);
    for (method, extra, _) in view_and_drive() {
        for field in HOST_PATH_FIELDS {
            let mut e = extra.clone();
            e[field] = json!("/Users/op/Library/LaunchAgents");
            let (m, params) = call(&writer, method, e);
            let err = check(&svc, "local", &writer, &m, &params).expect_err(&format!("{m} with {field}"));
            assert!(err.contains("host"), "{m} {field}: {err}");
        }
    }
}

/// The check lets a call through when it names no host path. These verbs then fail in the handler,
/// because each one needs the directory it works on (`root` or `dest`): so a principal reaches none
/// of them today. This pins that, so a verb that stops needing a path cannot slip through.
#[test]
fn every_view_and_drive_verb_needs_a_directory_it_would_open_as_the_daemon() {
    let (_d, svc) = rig();
    stores(&svc);
    for (method, extra, _) in view_and_drive() {
        let mut params = extra;
        params["name"] = json!(pr_name());
        let r = if method.starts_with("store.view.") {
            crate::store_view::run_with(&svc, method, params, &Guard::none())
        } else {
            crate::store_drive::run_with(&svc, method, params, &Guard::none())
        };
        let e = r.expect_err(method);
        assert!(e.contains("missing --root") || e.contains("missing --dest"), "{method}: {e}");
    }
}

// ---- (d) the Lows --------------------------------------------------------------------------------

/// N1: `create` of a worktree that already has a meta rewrites it; that needs write on the worktree.
#[test]
fn create_of_a_live_worktree_needs_write_on_it() {
    let (_d, svc) = rig();
    let (pr, _wtn) = stores(&svc);
    let pw = StoreCaller::Principal(p("agent:pw"));
    svc.grant(&pr, p("agent:pw"), Access::Write, "t").unwrap();
    // A slug nobody made yet: project write is enough.
    let (m, fresh) = wt_as(&pw, "create", json!({"slug": "feat-new", "branch": "b", "base": "0"}));
    check(&svc, "local", &pw, &m, &fresh).expect("a new worktree");
    // Another agent's live worktree: its meta exists, and pw holds nothing on it.
    let live = StoreName::worktree("local", PROJ, "feat-live").unwrap();
    svc.create_store(&live, "t", Default::default()).unwrap();
    write_meta(
        &Op(svc.operator_objects()),
        &live.bucket(),
        &WorktreeMeta { slug: "feat-live".into(), project: PROJ.into(), branch: "mine".into(), base_commit: "abc".into(), created_ms: 1 },
    )
    .unwrap();
    let (m, evil) = wt_as(&pw, "create", json!({"slug": "feat-live", "branch": "evil", "base": "0000"}));
    let e = check(&svc, "local", &pw, &m, &evil).expect_err("create over a live worktree");
    assert!(e.contains("does not hold"), "{e}");
    // With write on that worktree it is the caller's own, and idempotent.
    svc.grant(&live, p("agent:pw"), Access::Write, "t").unwrap();
    check(&svc, "local", &pw, &m, &evil).unwrap();
}

/// N4: a principal's params must be a JSON object, said outright and before anything else.
#[test]
fn a_principal_must_send_a_json_object() {
    let (_d, svc) = rig();
    stores(&svc);
    let w = who(WRITER_UID);
    for method in ["store.worktree.browse", "store.grant", "store.put", "store.view.status", "store.drive.watch"] {
        for bad in [json!([]), json!(["agent:writer", "local"]), json!("x"), json!(7), Value::Null] {
            let e = check(&svc, "local", &w, method, &bad).expect_err(&format!("{method} {bad}"));
            assert!(e.contains("JSON object"), "{method} {bad}: {e}");
        }
    }
    // The operator's params are not this check's business.
    check(&svc, "local", &StoreCaller::Operator, "store.grant", &json!([])).unwrap();
}

#[cfg(unix)]
fn good_file(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let f = dir.join(name);
    std::fs::write(&f, "[uids]\n40001 = \"agent:follower\"\n").unwrap();
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o600)).unwrap();
    f
}

#[cfg(unix)]
fn euid() -> u32 {
    nix::unistd::geteuid().as_raw()
}

/// N3: the map file is opened without following a symlink and without blocking on a FIFO.
#[cfg(unix)]
#[test]
fn the_map_file_is_not_followed_through_a_symlink() {
    let d = tempfile::tempdir().unwrap();
    let real = good_file(d.path(), "real.toml");
    std::os::unix::fs::symlink(&real, d.path().join(MAP_FILE)).unwrap();
    let e = PrincipalMap::load(d.path(), euid()).expect_err("a symlink is not the map file");
    assert!(e.contains(MAP_FILE), "{e}");
    // A plain file at the same place is fine, so the refusal is the link.
    let ok = tempfile::tempdir().unwrap();
    good_file(ok.path(), MAP_FILE);
    assert!(PrincipalMap::load(ok.path(), euid()).unwrap().is_some());
}

#[cfg(unix)]
#[test]
fn the_map_file_being_a_fifo_does_not_block_startup() {
    let d = tempfile::tempdir().unwrap();
    nix::unistd::mkfifo(&d.path().join(MAP_FILE), nix::sys::stat::Mode::from_bits_truncate(0o600)).expect("mkfifo");
    let dir = d.path().to_path_buf();
    let (tx, rx) = std::sync::mpsc::channel();
    // Own thread: an open that blocks would otherwise hang the whole test run.
    std::thread::spawn(move || {
        let _ = tx.send(PrincipalMap::load(&dir, euid()));
    });
    let r = rx.recv_timeout(std::time::Duration::from_secs(5)).expect("load must return, not block on a FIFO");
    let e = r.expect_err("a FIFO is not a regular file");
    assert!(e.contains("regular"), "{e}");
}

/// L-a (review of 08eee8a0a): a worktree store that exists but has no meta yet (made by `store create`,
/// or its first meta write failed) is somebody's: `create` of that slug needs write on it.
#[test]
fn create_of_an_existing_worktree_store_without_a_meta_needs_write_on_it() {
    let (_d, svc) = rig();
    let (pr, _wtn) = stores(&svc);
    let pw = StoreCaller::Principal(p("agent:pw"));
    svc.grant(&pr, p("agent:pw"), Access::Write, "t").unwrap();
    let theirs = StoreName::worktree("local", PROJ, "feat-b").unwrap();
    svc.create_store(&theirs, "t", Default::default()).unwrap();
    svc.grant(&theirs, p("agent:b"), Access::Admin, "t").unwrap();
    assert!(weftos_store::promote::read_meta(&Op(svc.operator_objects()), &theirs.bucket()).unwrap().is_none(), "no meta yet");
    let (m, params) = wt_as(&pw, "create", json!({"slug": "feat-b", "branch": "evil", "base": "0000"}));
    let e = check(&svc, "local", &pw, &m, &params).expect_err("a store with grants for someone else");
    assert!(e.contains("does not hold"), "{e}");
    // Its owner may.
    let b = StoreCaller::Principal(p("agent:b"));
    svc.grant(&pr, p("agent:b"), Access::Write, "t").unwrap();
    let (m, params) = wt_as(&b, "create", json!({"slug": "feat-b", "branch": "b", "base": "1"}));
    check(&svc, "local", &b, &m, &params).unwrap();
}

/// L-c (review of 08eee8a0a): the daemon records a mapped principal's store verbs under the principal
/// (N2). The daemon asks `principal_route`; this pins its answers and that `daemon.rs` asks it.
#[test]
fn the_daemon_routes_a_mapped_principals_store_verbs_to_the_principal() {
    let _g = TEST_MAP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let own = nix::unistd::geteuid().as_raw();
    let mapped = PrincipalMap::parse(&format!("[uids]\n{FOLLOWER_UID} = \"agent:follower\"\n{WRITER_UID} = \"agent:writer\"\n"), own).unwrap();
    let routed = ["store.grant", "store.revoke", "store.put", "store.get", "store.worktree.gc"];
    install(Some(mapped));
    for m in routed {
        assert_eq!(principal_route(&super::tests::caller(WRITER_UID), m).as_deref(), Some("agent:writer"), "{m}");
        assert_eq!(principal_route(&super::tests::caller(FOLLOWER_UID), m).as_deref(), Some("agent:follower"), "{m}");
        assert_eq!(principal_route(&super::tests::caller(own), m), None, "{m}: the operator is recorded as cli");
    }
    // Verbs that are not routed, and calls that are not store verbs, are not.
    for m in ["store.ls", "store.create", "store.worktree.create", "store.worktree.promote", "kernel.status"] {
        assert_eq!(principal_route(&super::tests::caller(WRITER_UID), m), None, "{m}");
    }
    install(None);
    for m in routed {
        assert_eq!(principal_route(&super::tests::caller(WRITER_UID), m), None, "{m}: no map, no principal");
    }
    // daemon.rs must ask: removing the hunk would silently record everyone as the operator.
    let daemon = include_str!("daemon.rs");
    assert!(daemon.contains("crate::store_caller::principal_route(caller, &method)"), "dispatch_after_auth no longer routes through principal_route");
}
