//! Tests for [`super`]: pure classification plus a real process group.

use super::*;

#[test]
fn only_a_same_uid_peer_outside_every_child_group_is_the_owner() {
    let pgids = |pid: i32| Some(if pid == 100 { 4242 } else { pid });
    // Inside child 4242's group (the child itself, or anything it spawned).
    assert_eq!(classify(501, Some(100), 501, &[4242, 7], pgids), PeerClass::Child);
    assert_eq!(classify(501, Some(4242), 501, &[4242], pgids), PeerClass::Child);
    // The owner's CLI.
    assert_eq!(classify(501, Some(555), 501, &[4242], pgids), PeerClass::Owner);
    // No supervised children and a readable group: the owner.
    assert_eq!(classify(501, Some(100), 501, &[], pgids), PeerClass::Owner);
    // Fail closed: unknown pid or unreadable group (the sender exited and was reaped).
    assert_eq!(classify(501, None, 501, &[4242], pgids), PeerClass::Child);
    assert_eq!(classify(501, None, 501, &[], pgids), PeerClass::Child);
    assert_eq!(classify(501, Some(100), 501, &[4242], |_| None), PeerClass::Child);
    // Another uid is never the owner, children or not.
    assert_eq!(classify(0, Some(555), 501, &[], pgids), PeerClass::OtherUid);
    assert!(PeerClass::Owner.is_owner() && !PeerClass::Child.is_owner() && !PeerClass::OtherUid.is_owner());
}

#[cfg(unix)]
#[tokio::test]
async fn a_process_in_a_childs_own_group_is_classified_from_real_credentials() {
    use std::os::unix::process::CommandExt;
    // A "child kernel": its own process group, so pgid == pid.
    let mut child = std::process::Command::new("sleep").arg("30").process_group(0).spawn().unwrap();
    let child_pid = child.id();
    let (a, _b) = tokio::net::UnixStream::pair().unwrap();
    // This test process is the peer. It is not in the child's group...
    let cred = a.peer_cred();
    assert_eq!(classify_peer_with(cred, &[child_pid]), PeerClass::Owner);
    // ...but it is once the supervised set names its own group.
    let my_group = nix::unistd::getpgrp().as_raw() as u32;
    let cred = a.peer_cred();
    assert_eq!(classify_peer_with(cred, &[my_group]), PeerClass::Child);
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(unix)]
fn classify_peer_with(cred: std::io::Result<tokio::net::unix::UCred>, children: &[u32]) -> PeerClass {
    let c = cred.unwrap();
    classify(c.uid(), c.pid(), nix::unistd::geteuid().as_raw(), children, |pid| {
        nix::unistd::getpgid(Some(nix::unistd::Pid::from_raw(pid))).ok().map(nix::unistd::Pid::as_raw)
    })
}

/// The leader exits (and is reaped) but a grandchild keeps the group alive:
/// the group stays supervised until it is gone (review S9).
#[cfg(all(unix, feature = "exochain", feature = "placement"))]
#[test]
fn an_orphaned_grandchild_keeps_its_group_supervised_until_the_group_is_gone() {
    use crate::project_supervisor::child::{note_group, supervised_groups};
    use std::os::unix::process::CommandExt;
    let mut leader = std::process::Command::new("sh")
        .args(["-c", "sleep 30 & exit 0"])
        .process_group(0)
        .spawn()
        .unwrap();
    let pgid = leader.id();
    note_group(pgid);
    leader.wait().unwrap(); // the leader is gone and reaped
    assert!(supervised_groups().contains(&pgid), "orphans keep the group");
    nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pgid as i32), nix::sys::signal::Signal::SIGKILL).unwrap();
    for _ in 0..100 {
        if !supervised_groups().contains(&pgid) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("group never left the set after it died");
}

/// When the leader exits and its group is empty, the number leaves the set at
/// once (not at the next accept), so a recycled pgid is never a stale child.
#[cfg(all(unix, feature = "exochain", feature = "placement"))]
#[test]
fn an_empty_group_is_pruned_when_the_leader_exit_is_seen() {
    use crate::project_supervisor::child::{group_noted, note_group, prune_if_gone};
    use std::os::unix::process::CommandExt;
    let mut leader = std::process::Command::new("sleep").arg("30").process_group(0).spawn().unwrap();
    let pgid = leader.id();
    note_group(pgid);
    prune_if_gone(pgid);
    assert!(group_noted(pgid), "a live group is kept");
    leader.kill().unwrap();
    leader.wait().unwrap();
    prune_if_gone(pgid);
    assert!(!group_noted(pgid), "an empty group is dropped without waiting for an accept");
}

#[test]
fn peer_key_is_uid_only_so_a_new_pid_is_not_a_new_budget() {
    assert_eq!(super::peer_key(1000, Some(7)), super::peer_key(1000, Some(8)));
    assert_eq!(super::peer_key(1000, Some(7)), super::peer_key(1000, None));
    assert_ne!(super::peer_key(1000, Some(7)), super::peer_key(1001, Some(7)));
}
