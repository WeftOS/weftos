//! One owner per device (`device_owner`): claim conflicts, a non-owner refused,
//! advisory marking and the privileged chown plan, as data; then the same
//! rules through the real placement path.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use clawft_types::placement::TrustTier;
use ed25519_dalek::SigningKey;

use super::device_owner::*;
use super::msg::method;
use super::test_support::*;
use super::transport::MeshConnector;
use crate::workload_runtime::cog_devices::{DeviceDecl, DeviceSel};
use crate::workload_runtime::{CogIdentity, RunMode};

/// A node with a camera, a mic and an MCU router, no filesystem involved.
struct FakeNode {
    nodes: Vec<&'static str>,
    usb: BTreeMap<(u16, u16), Vec<&'static str>>,
    owners: BTreeMap<&'static str, (u32, u32, u32)>,
    nets: Vec<&'static str>,
}

impl FakeNode {
    fn ventuno() -> Box<Self> {
        Box::new(Self {
            nodes: vec![
                "/dev/video0", "/dev/video1", "/dev/snd/controlC1", "/dev/snd/pcmC1D0c", "/dev/snd/pcmC1D0p",
                "/run/arduino-router.sock",
            ],
            usb: BTreeMap::from([((0x1bcf, 0x0b15), vec!["/dev/video0", "/dev/video1", "/dev/snd/pcmC1D0c"])]),
            nets: vec!["can0", "can1", "can2", "eth0", "wlan0"],
            owners: BTreeMap::from([
                ("/dev/video0", (0, 44, 0o660)),
                ("/dev/video1", (0, 44, 0o660)),
                ("/dev/snd/pcmC1D0c", (0, 29, 0o660)),
                ("/dev/snd/pcmC1D0p", (0, 29, 0o660)),
            ]),
        })
    }
}

impl DeviceResolver for FakeNode {
    fn expand(&self, pattern: &str) -> Vec<String> {
        self.nodes.iter().filter(|n| glob_match(pattern, n)).map(|n| n.to_string()).collect()
    }
    fn usb_nodes(&self, vid: u16, pid: u16) -> Vec<String> {
        self.usb.get(&(vid, pid)).into_iter().flatten().map(|n| n.to_string()).collect()
    }
    fn stat(&self, path: &str) -> Option<(u32, u32, u32)> {
        self.owners.get(path).copied()
    }
    fn netdevs(&self, pattern: &str) -> Vec<String> {
        self.nets.iter().filter(|n| glob_match(pattern, n)).map(|n| n.to_string()).collect()
    }
}

fn path(p: &str) -> DeviceDecl {
    DeviceDecl { sel: DeviceSel::Path(p.into()), write: true }
}

fn net(n: &str) -> DeviceDecl {
    DeviceDecl { sel: DeviceSel::Netdev(n.into()), write: true }
}

fn usb(vid: u16, pid: u16) -> DeviceDecl {
    DeviceDecl { sel: DeviceSel::Usb { vid, pid }, write: true }
}

fn ident(uid: u32) -> CogIdentity {
    CogIdentity { uid, gid: 900, groups: vec![900] }
}

fn ledger() -> Arc<DeviceLedger> {
    DeviceLedger::new(FakeNode::ventuno())
}

#[test]
fn globs_match_and_intersect() {
    assert!(glob_match("/dev/video*", "/dev/video0"));
    assert!(glob_match("pcmC?D?c", "pcmC1D0c"));
    assert!(!glob_match("pcmC?D?c", "pcmC1D0p"));
    assert!(globs_intersect("/dev/video*", "/dev/video0"));
    assert!(globs_intersect("/dev/video*", "/dev/*0"), "both can name video0");
    assert!(!globs_intersect("/dev/snd/pcmC*D*c", "/dev/snd/pcmC*D*p"), "capture and playback differ");
    assert!(!globs_intersect("/dev/video*", "/dev/snd/*"));
}

#[test]
fn a_second_claim_on_the_same_device_is_refused() {
    let l = ledger();
    let cam = l.admit("camera", &[path("/dev/video*")], &[], None).unwrap().unwrap();
    cam.bind("i-cam");

    for other in [path("/dev/video0"), path("/dev/video*"), usb(0x1bcf, 0x0b15), path("/dev/vid?o1")] {
        let e = l.admit("intruder", &[other.clone()], &[], None).err().unwrap_or_else(|| panic!("{other:?} was admitted"));
        assert!(e.0.contains("owned by camera (i-cam)"), "{e}");
    }
    // Disjoint devices are fine, and the playback node does not clash with capture.
    let rec = l.admit("audio-in", &[path("/dev/snd/pcmC*D*c")], &[], None).unwrap().unwrap();
    rec.bind("i-in");
    l.admit("audio-out", &[path("/dev/snd/pcmC*D*p")], &[], None).unwrap().unwrap().bind("i-out");
    // The USB camera also carries the capture node: claiming it by id clashes with audio-in too.
    assert!(l.admit("x", &[usb(0x1bcf, 0x0b15)], &[], None).is_err());
}

#[test]
fn a_claim_in_flight_blocks_a_concurrent_one_and_a_dropped_one_frees_the_device() {
    let l = ledger();
    let first = l.admit("a", &[path("/dev/video0")], &[], None).unwrap().unwrap();
    let e = l.admit("b", &[path("/dev/video0")], &[], None).err().unwrap();
    assert!(e.0.contains("being placed"), "{e}");
    drop(first);
    l.admit("b", &[path("/dev/video0")], &[], None).unwrap().unwrap().bind("i-b");
}

#[test]
fn release_frees_the_device_for_the_next_owner() {
    let l = ledger();
    l.admit("a", &[path("/dev/video0")], &[], None).unwrap().unwrap().bind("i-a");
    assert!(l.admit("b", &[path("/dev/video0")], &[], None).is_err());
    assert!(l.release("i-a").is_empty(), "shared uid: nothing to restore");
    l.admit("b", &[path("/dev/video0")], &[], None).unwrap().unwrap().bind("i-b");
}

#[test]
fn a_non_owner_is_refused_a_device_path_in_its_arguments() {
    let l = ledger();
    let cam = l.admit("camera", &[path("/dev/video*")], &[], None).unwrap().unwrap();
    cam.bind("i-cam");
    for args in [["--device", "/dev/video0"], ["--src=/dev/video1", "--once"]] {
        let a: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let e = l.admit("detector", &[], &a, None).err().expect("refused");
        assert!(e.0.contains("only its owner may open it"), "{e}");
    }
    // The owner may name its own device; anyone may name something unowned.
    let own = vec!["--device".to_string(), "/dev/video0".to_string()];
    assert!(l.admit("camera", &[path("/dev/video*")], &own, None).is_err(), "a second camera instance still clashes");
    let free = vec!["--out".to_string(), "/data/frames".to_string()];
    assert!(l.admit("detector", &[], &free, None).unwrap().is_none());
}

#[test]
fn the_owner_is_handed_exactly_its_devices() {
    let l = ledger();
    let r = l.admit("mcu-bridge", &[DeviceDecl { sel: DeviceSel::Socket("/run/arduino-router.sock".into()), write: true }], &[], None).unwrap().unwrap();
    assert_eq!(r.paths(), ["/run/arduino-router.sock"]);
    let c = l.admit("camera", &[path("/dev/video*")], &[], None).unwrap().unwrap();
    assert_eq!(c.paths(), ["/dev/video0", "/dev/video1"], "the nodes the glob resolved to");
    assert!(l.admit("plain", &[], &[], None).unwrap().is_none(), "a cog with no devices gets none");
}

#[test]
fn shared_uid_nodes_report_advisory_and_own_uid_nodes_enforced() {
    let l = ledger();
    l.admit("camera", &[path("/dev/video*")], &[], None).unwrap().unwrap().bind("i-cam");
    l.admit("audio-in", &[path("/dev/snd/pcmC*D*c")], &[], Some(&ident(60001))).unwrap().unwrap().bind("i-in");
    assert_eq!(l.claim_of("i-cam").unwrap().1, DeviceIsolation::Advisory);
    assert_eq!(l.claim_of("i-in").unwrap().1, DeviceIsolation::Enforced);
    let rep = l.report();
    let rows = rep["owners"].as_array().unwrap();
    let iso = |inst: &str| rows.iter().find(|r| r["instance"] == inst).unwrap()["device_isolation"].clone();
    assert_eq!((iso("i-cam"), iso("i-in")), ("advisory".into(), "enforced".into()));
}

#[test]
fn a_privileged_node_plans_a_chown_to_the_owner_and_the_way_back() {
    let l = ledger();
    let r = l.admit("camera", &[path("/dev/video*")], &[], Some(&ident(60002))).unwrap().unwrap();
    assert_eq!(
        r.plan().apply,
        vec![
            GrantOp::Chown { path: "/dev/video0".into(), uid: 60002, gid: 44, mode: 0o600 },
            GrantOp::Chown { path: "/dev/video1".into(), uid: 60002, gid: 44, mode: 0o600 },
        ],
        "the owner's uid, the node's own group, owner-only mode"
    );
    assert_eq!(r.plan().restore[0], GrantOp::Chown { path: "/dev/video0".into(), uid: 0, gid: 44, mode: 0o660 });
    r.bind("i-cam");
    assert_eq!(l.release("i-cam"), vec![
        GrantOp::Chown { path: "/dev/video0".into(), uid: 0, gid: 44, mode: 0o660 },
        GrantOp::Chown { path: "/dev/video1".into(), uid: 0, gid: 44, mode: 0o660 },
    ], "unload hands the original owners back");
}

#[test]
fn a_read_only_claim_is_granted_read_only_and_missing_nodes_are_skipped() {
    let l = ledger();
    let ro = DeviceDecl { sel: DeviceSel::Path("/dev/video0".into()), write: false };
    let r = l.admit("viewer", &[ro, path("/dev/snd/controlC*")], &[], Some(&ident(60003))).unwrap().unwrap();
    // controlC1 has no recorded owner in the fake, so it is skipped, not guessed.
    assert_eq!(r.plan().apply, vec![GrantOp::Chown { path: "/dev/video0".into(), uid: 60003, gid: 44, mode: 0o400 }]);
}

#[test]
fn network_interfaces_are_owned_by_name_in_their_own_namespace() {
    let l = ledger();
    let r = l.admit("can-fd", &[net("can*")], &[], None).unwrap().unwrap();
    assert_eq!(r.paths(), ["can0", "can1", "can2"], "the owner is handed interface names, not paths");
    r.bind("i-can");
    for other in [net("can1"), net("can*"), net("c?n2")] {
        let e = l.admit("rival", &[other.clone()], &[], None).err().unwrap_or_else(|| panic!("{other:?} was admitted"));
        assert!(e.0.contains("owned by can-fd (i-can)"), "{e}");
    }
    // Other interfaces, and paths that happen to look alike, do not clash.
    l.admit("wifi", &[net("wlan0")], &[], None).unwrap().unwrap().bind("i-wifi");
    l.admit("path", &[path("/dev/can0")], &[], None).unwrap().unwrap().bind("i-path");
    assert_eq!(l.claim_of("i-can").unwrap().0, ["net:can*"]);
}

// ---- through the real placement path ----

#[derive(Default)]
struct Recorder(Mutex<Vec<Vec<GrantOp>>>);

impl DeviceApplier for Recorder {
    fn apply(&self, ops: &[GrantOp]) -> Result<(), String> {
        self.0.lock().unwrap().push(ops.to_vec());
        Ok(())
    }
}

const COG: &str = "#!/bin/sh\necho \"$WEFTOS_DEVICES\" > \"$COGNITUM_COG_DATA_DIR/dev\"\nexec sleep 30\n";

async fn host_with_plane(seed: u8) -> (HostNode, super::plane::PlacementControlPlane, tempfile::TempDir) {
    let ctl = SigningKey::from_bytes(&[seed + 1; 32]);
    let host = host_node_native_only(seed, board_caps("pi5"), true, &ctl);
    let tmp = tempfile::tempdir().unwrap();
    let conn = Arc::new(MeshConnector::new(false));
    let addr = conn.register_local("host", host.svc.clone());
    let (plane, _chain) = controller(&ctl, conn);
    plane.add_target(&addr, TrustTier::Paired).await.unwrap();
    (host, plane, tmp)
}

async fn place_cog(
    plane: &super::plane::PlacementControlPlane,
    tmp: &std::path::Path,
    id: &str,
    devices: &str,
    args: &[&str],
    port: u16,
) -> super::plane_place::PlaceReport {
    let pkg = package_toml(&tmp.join(format!("{id}-{port}")), id, &[(arch(), COG)], None, devices);
    let mut order = mode_order(&pkg, RunMode::Listener, None);
    order.start = false;
    order.config.csi_port = port;
    order.config.args = args.iter().map(|s| s.to_string()).collect();
    plane.place(&order).await.unwrap()
}

#[tokio::test]
async fn placement_refuses_a_second_owner_and_a_foreign_device_argument() {
    let (host, plane, tmp) = host_with_plane(81).await;
    let rec = Arc::new(Recorder::default());
    assert!(host.svc.set_device_applier(rec.clone()));

    let dev = "\n[[devices]]\npath = \"/dev/null\"\n";
    let first = place_cog(&plane, tmp.path(), "owner-a", dev, &[], 15081).await;
    let iid = first.placed.expect("the first owner is placed").instance_id;
    let row = host.svc.devices().claim_of(&iid).expect("recorded");
    assert_eq!((row.0, row.1), (vec!["/dev/null".to_string()], DeviceIsolation::Advisory));

    let second = place_cog(&plane, tmp.path(), "owner-b", dev, &[], 15082).await;
    assert!(second.placed.is_none(), "{}", second.explain);
    let a = &second.attempts[0];
    assert_eq!(a.code.as_deref(), Some("admission"));
    assert!(a.reason.as_deref().unwrap().contains("one owner per device"), "{a:?}");

    let third = place_cog(&plane, tmp.path(), "reader", "", &["--device", "/dev/null"], 15083).await;
    assert!(third.placed.is_none(), "{}", third.explain);
    assert!(third.attempts[0].reason.as_deref().unwrap().contains("only its owner"), "{:?}", third.attempts);
    assert!(rec.0.lock().unwrap().is_empty(), "a shared uid re-owns nothing");

    plane.instance(method::UNLOAD, &iid).await.unwrap();
    let again = place_cog(&plane, tmp.path(), "owner-b", dev, &[], 15084).await;
    assert!(again.placed.is_some(), "unload released the device: {}", again.explain);
}

#[tokio::test]
async fn the_owner_is_started_with_its_device_list_and_nobody_else_is() {
    let (host, plane, tmp) = host_with_plane(83).await;
    let pkg = package_toml(&tmp.path().join("own"), "own", &[(arch(), COG)], None, "\n[[devices]]\npath = \"/dev/null\"\n");
    let mut order = mode_order(&pkg, RunMode::Listener, None);
    order.config.csi_port = 15083;
    let iid = plane.place(&order).await.unwrap().placed.expect("placed").instance_id;
    let f = host._tmp.path().join("instances").join(&iid).join("data/dev");
    let mut got = String::new();
    for _ in 0..600 {
        if let Ok(s) = std::fs::read_to_string(&f)
            && s.ends_with('\n')
        {
            got = s.trim().to_string();
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(got, "/dev/null");

    let pkg = package_toml(&tmp.path().join("plain"), "plain", &[(arch(), COG)], None, "");
    let mut order = mode_order(&pkg, RunMode::Listener, None);
    order.config.csi_port = 15084;
    let other = plane.place(&order).await.unwrap().placed.expect("placed").instance_id;
    let f = host._tmp.path().join("instances").join(&other).join("data/dev");
    for _ in 0..600 {
        if std::fs::read_to_string(&f).is_ok_and(|s| s.ends_with('\n')) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(std::fs::read_to_string(&f).unwrap().trim(), "", "no WEFTOS_DEVICES for a cog that owns nothing");
    plane.instance(method::STOP, &iid).await.unwrap();
    plane.instance(method::STOP, &other).await.unwrap();
}
