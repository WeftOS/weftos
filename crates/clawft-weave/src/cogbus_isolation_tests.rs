//! Uid allocation, the shared-uid marking and the config file.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;

use super::*;

const GID: u32 = 4321;

fn cfg(range: Option<[u32; 2]>, require: bool) -> IsolationConfig {
    IsolationConfig { require_cog_uid: require, uid_range: range, group: "cogbus".into(), ..Default::default() }
}

#[test]
fn a_privileged_daemon_with_a_range_and_the_group_gives_each_cog_its_own_uid() {
    let iso = Isolator::new(cfg(Some([60000, 60002]), false), true, Some(GID));
    assert_eq!(iso.blocker(), None);
    let a = iso.plan();
    let b = iso.plan();
    let (ia, ib) = (a.identity.clone().unwrap(), b.identity.clone().unwrap());
    assert_eq!((ia.uid, ib.uid), (60000, 60001), "lowest free first, never shared");
    assert_eq!(ia.gid, GID);
    assert_eq!(ia.groups, vec![GID], "the cogbus group and nothing of the daemon's");
    assert_eq!(a.isolation, Isolation::PerCogUid);
    assert_eq!(a.note, None);
}

#[test]
fn every_reason_it_cannot_isolate_is_shared_and_says_why() {
    let cases = [
        (cfg(None, false), true, Some(GID), "no uid_range"),
        (cfg(Some([60000, 60009]), false), false, Some(GID), "unprivileged"),
        (cfg(Some([60000, 60009]), false), true, None, "does not exist"),
    ];
    for (c, privileged, gid, want) in cases {
        let p = Isolator::new(c, privileged, gid).plan();
        assert_eq!(p.isolation, Isolation::SharedUid);
        assert_eq!(p.identity, None);
        assert!(p.note.unwrap().contains(want), "{want}");
    }
}

#[test]
fn an_exhausted_range_is_shared_not_a_reused_uid() {
    let iso = Isolator::new(cfg(Some([60000, 60000]), false), true, Some(GID));
    assert!(iso.plan().identity.is_some());
    let p = iso.plan();
    assert_eq!(p.isolation, Isolation::SharedUid);
    assert!(p.note.unwrap().contains("exhausted"));
}

#[test]
fn a_uid_follows_its_instance_through_stop_and_is_freed_at_release() {
    let iso = Isolator::new(cfg(Some([60000, 60001]), false), true, Some(GID));
    let uid = iso.plan().identity.unwrap().uid;
    iso.bind("i1", "radar", Some(uid), Isolation::PerCogUid);
    assert_eq!(iso.report()["instances"][0]["isolation"], "per-cog-uid");

    // Stop: unlisted, but nobody else may take the uid its data dir is owned by.
    iso.unbind("i1");
    assert_eq!(iso.report()["instances"].as_array().unwrap().len(), 0);
    assert_eq!(iso.plan().identity.unwrap().uid, uid + 1);
    assert!(iso.held().contains(&uid));

    // Unload: freed.
    iso.release("i1");
    assert_eq!(iso.plan().identity.unwrap().uid, uid);
}

#[test]
fn a_placement_that_never_registered_gives_its_uid_back_but_a_bound_one_stays() {
    let iso = Isolator::new(cfg(Some([60000, 60005]), false), true, Some(GID));
    let pending = iso.plan().identity.unwrap().uid;
    iso.abandon(pending);
    assert!(iso.held().is_empty());

    let bound = iso.plan().identity.unwrap().uid;
    iso.bind("i2", "radar", Some(bound), Isolation::PerCogUid);
    iso.abandon(bound);
    assert!(iso.held().contains(&bound), "abandon only frees a pending allocation");
}

#[test]
fn a_shared_instance_is_marked_in_the_report() {
    let iso = Isolator::new(cfg(None, true), false, None);
    iso.bind("i3", "radar", Some(501), Isolation::SharedUid);
    let r = iso.report();
    assert_eq!(r["mode"], "shared-uid");
    assert_eq!(r["require_cog_uid"], true);
    assert_eq!(r["instances"][0]["isolation"], "shared-uid");
    assert_eq!(r["instances"][0]["uid"], 501);
    assert!(r["reason"].as_str().unwrap().contains("uid_range"));
}

#[test]
fn caps_need_both_setuid_and_setgid() {
    assert!(caps_allow_switch("00000000000000c0"));
    assert!(caps_allow_switch(" 000001ffffffffff\n"));
    assert!(!caps_allow_switch("0000000000000040"), "setgid only");
    assert!(!caps_allow_switch("0000000000000080"), "setuid only");
    assert!(!caps_allow_switch("0000000000000000"));
    assert!(!caps_allow_switch("not hex"));
}

fn write(dir: &Path, body: &str, mode: u32) {
    let p = dir.join(ISOLATION_FILE);
    std::fs::write(&p, body).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
}

#[test]
fn the_config_file_is_owner_only_validated_and_optional() {
    let tmp = tempfile::tempdir().unwrap();
    assert_eq!(load(tmp.path()).unwrap(), IsolationConfig::default());

    write(tmp.path(), r#"{"require_cog_uid": true, "uid_range": [60000, 60999]}"#, 0o600);
    let c = load(tmp.path()).unwrap();
    assert!(c.require_cog_uid);
    assert_eq!(c.uid_range, Some([60000, 60999]));
    assert_eq!(c.group, "cogbus");

    write(tmp.path(), r#"{"uid_range": [60000, 60999]}"#, 0o664);
    assert!(load(tmp.path()).unwrap_err().contains("writable"));
    for bad in [
        r#"{"uid_range": [0, 10]}"#,
        r#"{"uid_range": [70000, 60000]}"#,
        r#"{"uid_range": [1000, 90000]}"#,
        r#"{"group": "co gbus"}"#,
        r#"{"surprise": 1}"#,
    ] {
        write(tmp.path(), bad, 0o600);
        assert!(load(tmp.path()).is_err(), "{bad}");
    }
}

#[tokio::test]
async fn the_registrar_hands_out_identities_and_frees_them() {
    use clawft_kernel::workload_ctl::host_cogbus::CogbusRegistrar;
    let tmp = tempfile::tempdir().unwrap();
    let iso = Arc::new(Isolator::new(cfg(Some([60000, 60009]), true), true, Some(GID)));
    let rt = crate::cogbus_serve::start_with(crate::cogbus_serve::broker("n"), tmp.path(), iso, None)
        .await
        .unwrap();
    let reg = crate::cogbus_serve::Registrar::for_runtime(&rt);

    let a = reg.attach("native").unwrap();
    assert_eq!(a.isolation, Isolation::PerCogUid);
    assert_eq!(a.uid, Some(60000));
    assert_eq!(a.identity.unwrap().groups, vec![GID]);
    assert!(a.refuse_shared);
    assert!(reg.attach("container").is_none());

    reg.abandon(60000);
    assert_eq!(reg.attach("native").unwrap().uid, Some(60000), "an abandoned uid is reusable");
}

#[tokio::test]
async fn cogbus_list_shows_the_socket_and_marks_a_shared_uid_instance() {
    use clawft_kernel::workload_ctl::host_cogbus::{CogbusRegistrar, CogbusRegistration};
    let tmp = tempfile::tempdir().unwrap();
    let rt = crate::cogbus_serve::start(tmp.path(), "n").await.unwrap();
    let reg = crate::cogbus_serve::Registrar::for_runtime(&rt);
    let a = reg.attach("native").unwrap();
    assert_eq!(a.isolation, Isolation::SharedUid, "no range configured");
    reg.register(&CogbusRegistration {
        instance_id: "shared-1".into(),
        cog_id: "radar".into(),
        project_id: None,
        controller_node: "ctl".into(),
        token: clawft_types::secret::SecretString::new("t".repeat(40)),
        uid: a.uid,
        isolation: a.isolation,
        outputs: vec![],
        inputs: vec![],
    })
    .unwrap();

    let doc = crate::cogbus_serve::list_json(Some(&rt));
    assert_eq!(doc["socket_mode"], "0660");
    assert_eq!(doc["isolation"]["mode"], "shared-uid");
    assert_eq!(doc["isolation"]["instances"][0]["instance"], "shared-1");
    assert_eq!(doc["isolation"]["instances"][0]["isolation"], "shared-uid");

    reg.revoke("shared-1");
    let doc = crate::cogbus_serve::list_json(Some(&rt));
    assert_eq!(doc["isolation"]["instances"].as_array().unwrap().len(), 0);
}

fn registrar(iso: Isolator, route: crate::cogbus_serve::ContainerRoute, gid: Option<u32>) -> crate::cogbus_serve::Registrar {
    crate::cogbus_serve::Registrar::with_isolator(
        crate::cogbus_serve::broker("n"),
        Path::new("/run/weftos/node/cogbus.sock"),
        Arc::new(iso),
    )
    .with_container(route, gid)
}

#[test]
fn a_container_cog_gets_the_mounted_socket_the_share_uid_and_the_sockets_gid() {
    use clawft_kernel::workload_ctl::host_cogbus::CogbusRegistrar;
    use crate::cogbus_serve::ContainerRoute::Socket;
    // A user daemon: no per-cog uid, so every container cog shares uid 65534
    // and runs under the group that opens the socket.
    let r = registrar(Isolator::new(IsolationConfig::default(), false, None), Socket, Some(20));
    let a = r.attach("container").unwrap();
    assert_eq!(a.mount.as_deref(), Some("/run/weftos/node/cogbus.sock"));
    assert_eq!(a.socket, "/run/weftos/cogbus.sock");
    assert_eq!((a.uid, a.isolation), (Some(65534), Isolation::SharedUid));
    let id = a.identity.unwrap();
    assert_eq!((id.uid, id.gid, id.groups), (65534, 20, vec![]));
    assert!(a.note.unwrap().contains("share uid 65534"));
    // The native route is untouched.
    assert!(r.attach("native").unwrap().mount.is_none());
}

#[test]
fn a_privileged_daemon_gives_container_cogs_their_own_uid_too() {
    use clawft_kernel::workload_ctl::host_cogbus::CogbusRegistrar;
    use crate::cogbus_serve::ContainerRoute::Socket;
    let r = registrar(Isolator::new(cfg(Some([60000, 60009]), true), true, Some(GID)), Socket, Some(GID));
    let (a, b) = (r.attach("container").unwrap(), r.attach("container").unwrap());
    assert_eq!((a.uid, b.uid), (Some(60000), Some(60001)));
    assert_eq!(a.isolation, Isolation::PerCogUid);
    assert_eq!(a.identity.unwrap().gid, GID, "the group that owns the socket");
    assert!(a.refuse_shared, "require_cog_uid is carried");
}

#[test]
fn engines_that_cannot_share_a_host_socket_are_unsupported_and_say_why() {
    use clawft_kernel::workload_ctl::host_cogbus::CogbusRegistrar;
    use clawft_kernel::workload_runtime::{ContainerRuntimeConfig, container_cmd::Engine};
    use crate::cogbus_serve::{ContainerRoute, container_route};
    let mk = |e| ContainerRuntimeConfig::new(e, format!("debian@sha256:{}", "0".repeat(64)), std::path::PathBuf::from("/tmp/x"));
    assert!(matches!(container_route(None), ContainerRoute::Off(w) if w.contains("no container adapter")));
    assert!(matches!(container_route(Some(&mk(Engine::Apple))), ContainerRoute::Off(w) if w.contains("own VM")));
    let docker = container_route(Some(&mk(Engine::Docker)));
    if cfg!(target_os = "linux") {
        assert_eq!(docker, ContainerRoute::Socket);
    } else {
        assert!(matches!(docker, ContainerRoute::Off(w) if w.contains("VM")));
    }
    // Off, or no known socket group: no attach, and the report says so.
    let off = registrar(Isolator::shared(), ContainerRoute::Off("because".into()), Some(20));
    assert!(off.attach("container").is_none());
    let nogid = registrar(Isolator::shared(), ContainerRoute::Socket, None);
    assert!(nogid.attach("container").is_none());
    for r in ["vm", "seed", "nope"] {
        assert!(off.attach(r).is_none(), "{r}");
    }
}

#[test]
fn the_report_lists_how_each_route_reaches_the_broker() {
    use crate::cogbus_serve::ContainerRoute;
    let iso = Isolator::new(IsolationConfig::default(), false, None);
    let iso = Arc::new(iso);
    let _ = crate::cogbus_serve::Registrar::with_isolator(crate::cogbus_serve::broker("n"), Path::new("/s"), iso.clone())
        .with_container(ContainerRoute::Off("no container adapter on this node".into()), None);
    let routes = iso.report()["routes"].clone();
    assert!(routes["container"].as_str().unwrap().starts_with("unsupported: no container adapter"));
    assert!(routes["vm"].as_str().unwrap().contains("ingest HTTP only"));
    assert!(routes["seed"].as_str().unwrap().starts_with("unsupported"));
    let on = Arc::new(Isolator::new(IsolationConfig::default(), false, None));
    let _ = crate::cogbus_serve::Registrar::with_isolator(crate::cogbus_serve::broker("n"), Path::new("/s"), on.clone())
        .with_container(ContainerRoute::Socket, Some(20));
    assert!(on.report()["routes"]["container"].as_str().unwrap().contains("shared-uid 65534"));
}

#[test]
fn container_uid_is_validated() {
    for bad in [r#"{"container_uid": 0}"#, r#"{"uid_range": [60000, 60999], "container_uid": 60500}"#] {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), bad, 0o600);
        assert!(load(tmp.path()).is_err(), "{bad}");
    }
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), r#"{"container_uid": 100000}"#, 0o600);
    assert_eq!(load(tmp.path()).unwrap().container_uid, 100000);
}
