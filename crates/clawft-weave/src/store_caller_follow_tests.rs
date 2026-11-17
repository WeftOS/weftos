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
    let (m, params) = wt_as(&admin, "gc", json!({"confirmed": true, "keep_days": 7}));
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
            r.unwrap_or_else(|e| panic!("follower {m} {extra}: {e}"));
        } else {
            let e = r.expect_err(&format!("follower {m} writes"));
            assert!(e.contains("does not hold"), "{m}: {e}");
        }
        let (m, params) = call(&writer, method, extra.clone());
        check(&svc, "local", &writer, &m, &params).unwrap_or_else(|e| panic!("writer {m} {extra}: {e}"));
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
