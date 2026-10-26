//! The socket's mode and group (ADR-122 gap 3). Whether the cogbus group
//! exists is the caller's lookup; here the group is the test process's own
//! gid (exists, may be assigned) or one it is not in (exists, may not be
//! assigned without root).

#![cfg(unix)]

use std::sync::Arc;

use weftos_cogbus::server::{ServeOptions, serve_with};
use weftos_cogbus::{Broker, BrokerConfig};

fn broker() -> Arc<Broker> {
    Arc::new(Broker::new(BrokerConfig::new("n")))
}

#[tokio::test]
async fn with_a_group_it_can_assign_the_socket_is_that_group_and_0660() {
    let tmp = tempfile::tempdir().unwrap();
    let gid = nix_free_egid();
    let h = serve_with(broker(), &tmp.path().join("cogbus.sock"), ServeOptions { group: Some(gid) })
        .await
        .unwrap();
    let r = h.report();
    assert_eq!(r.mode, 0o660, "never wider than owner and group");
    assert_eq!(r.gid, gid);
    assert!(r.group_applied);
    assert_eq!(r.note, None);
}

#[tokio::test]
async fn with_no_group_the_socket_keeps_the_daemons_group_and_says_so() {
    let tmp = tempfile::tempdir().unwrap();
    let h = serve_with(broker(), &tmp.path().join("cogbus.sock"), ServeOptions::default())
        .await
        .unwrap();
    let r = h.report();
    assert_eq!(r.mode, 0o660);
    assert!(!r.group_applied);
    assert!(r.note.as_deref().unwrap().contains("no cogbus group"));
}

#[tokio::test]
async fn a_group_the_daemon_may_not_assign_is_reported_and_the_mode_stays_0660() {
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipped: root can assign any group, so the refusal cannot be reached");
        return;
    }
    let own = nix_free_egid();
    // A gid the process is neither primary nor supplementary member of.
    let foreign = (1..65000u32)
        .find(|g| *g != own && !in_groups(*g))
        .expect("some gid the test process is not in");
    let tmp = tempfile::tempdir().unwrap();
    let h = serve_with(broker(), &tmp.path().join("cogbus.sock"), ServeOptions { group: Some(foreign) })
        .await
        .unwrap();
    let r = h.report();
    assert_eq!(r.mode, 0o660);
    assert!(!r.group_applied);
    assert_eq!(r.gid, own, "stays the daemon's group");
    assert!(r.note.as_deref().unwrap().contains("cannot give the socket"));
}

fn nix_free_egid() -> u32 {
    // SAFETY: getegid cannot fail.
    unsafe { libc::getegid() }
}

fn in_groups(gid: u32) -> bool {
    let n = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
    let mut v = vec![0 as libc::gid_t; n.max(0) as usize];
    let n = unsafe { libc::getgroups(v.len() as i32, v.as_mut_ptr()) };
    v.truncate(n.max(0) as usize);
    v.contains(&gid)
}
