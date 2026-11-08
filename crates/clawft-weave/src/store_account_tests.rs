use super::*;

const ULID: &str = "01J8ZQ4W2X9K3M5N7P0R2T4V6Y";
const OTHER: &str = "01J8ZQ4W2X9K3M5N7P0R2T4V6Z";
const NOW: u64 = 1_800_000_000;

fn mesh(b: u8) -> [u8; 32] {
    [b; 32]
}
fn name(m: [u8; 32], ulid: &str) -> String {
    format!("weftos://{}/projects/{ulid}/store", hex::encode(m))
}
fn client(id: &str) -> Principal {
    Principal::Client { account: "acme".into(), id: id.into() }
}
fn weftos() -> Principal {
    Principal::WeftosAccount { id: "weaver".into() }
}
fn gate() -> StoreGate {
    let mut g = StoreGate::new(Authority(mesh(1)), "acme");
    g.hold_project(ULID);
    g
}
fn key(b: u8) -> SigningKey {
    SigningKey::from_bytes(&[b; 32])
}
fn ok(g: &StoreGate, p: &Principal, s: &str, a: Access) -> bool {
    g.authorize(p, s, a, NOW).0.is_ok()
}

#[test]
fn spec_is_pinned_to_data1_account_store() {
    let s = ClientStoreSpec::standard("acme", 2102, 9100).unwrap();
    assert_eq!(s.root, PathBuf::from("/data1/acme/store"));
    let bad_root = |r: &str| ClientStoreSpec::new("acme", 2102, r.into(), "127.0.0.1:9100".parse().unwrap());
    for r in ["/data1/other/store", "/data1/acme/store/../../other/store", "/tmp/store", "/data1/acme", "/data1/acme/store/x"] {
        assert_eq!(bad_root(r), Err(SpecError::Root), "{r}");
    }
}

#[test]
fn spec_refuses_weftos_account_low_uid_bad_name_and_wildcard_listen() {
    let l = "127.0.0.1:9100".parse().unwrap();
    let root = |a: &str| PathBuf::from(format!("/data1/{a}/store"));
    assert_eq!(ClientStoreSpec::new("weftos", 2102, root("weftos"), l), Err(SpecError::NotClient));
    assert_eq!(ClientStoreSpec::new("acme", 1000, root("acme"), l), Err(SpecError::Uid(1000)));
    assert_eq!(ClientStoreSpec::new("Acme", 2102, root("Acme"), l), Err(SpecError::Account));
    assert_eq!(ClientStoreSpec::new("a/b", 2102, root("a/b"), l), Err(SpecError::Account));
    assert_eq!(ClientStoreSpec::new("acme", 2102, root("acme"), "0.0.0.0:9100".parse().unwrap()), Err(SpecError::Listen));
    assert_eq!(ClientStoreSpec::new("acme", 2102, root("acme"), "127.0.0.1:0".parse().unwrap()), Err(SpecError::Listen));
}

#[test]
fn unit_runs_as_client_user_and_hides_other_trees() {
    let u = ClientStoreSpec::standard("acme", 2102, 9100).unwrap().systemd_unit(Path::new("/opt/rustfs/rustfs")).unwrap();
    assert!(u.contains("User=acme\n"));
    assert!(u.contains("TemporaryFileSystem=/data1:ro"));
    assert!(u.contains("BindPaths=/data1/acme/store"));
    assert!(u.contains("IPAddressDeny=any"));
    assert!(u.contains("RUSTFS_AUDIT_ENABLE=true"));
    assert!(!u.contains("User=weftos"));
}

#[cfg(unix)]
#[test]
fn on_disk_check_needs_owner_and_0700() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let d = tempfile::tempdir().unwrap();
    let me = std::fs::metadata(d.path()).unwrap().uid();
    let mut s = ClientStoreSpec::standard("acme", 2102, 9100).unwrap();
    s.root = d.path().to_path_buf();
    s.uid = me;
    std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(s.verify_on_disk(), Err(SpecError::Ownership));
    std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(s.verify_on_disk(), Ok(()));
    s.uid = me.wrapping_add(1);
    assert_eq!(s.verify_on_disk(), Err(SpecError::Ownership));
    let link = d.path().join("l");
    std::os::unix::fs::symlink(d.path(), &link).unwrap();
    s.uid = me;
    s.root = link;
    assert_eq!(s.verify_on_disk(), Err(SpecError::Ownership), "a symlinked root is refused");
}

#[test]
fn weftos_account_is_refused_unless_granted_and_admin_elsewhere_does_not_count() {
    let mut g = gate();
    let s = name(mesh(1), ULID);
    for a in [Access::Read, Access::Write, Access::Admin] {
        assert!(!ok(&g, &weftos(), &s, a), "no default grant for the weftos account");
    }
    // Admin on another store, or a grant to a different weftos principal, does not leak.
    g.hold_project(OTHER);
    g.grant(StoreGrant { principal: weftos(), store: name(mesh(1), OTHER), access: Access::Admin });
    g.grant(StoreGrant { principal: Principal::WeftosAccount { id: "other".into() }, store: s.clone(), access: Access::Admin });
    assert!(!ok(&g, &weftos(), &s, Access::Read));
    // An explicit grant works, at its level only.
    g.grant(StoreGrant { principal: weftos(), store: s.clone(), access: Access::Read });
    assert!(ok(&g, &weftos(), &s, Access::Read));
    assert!(!ok(&g, &weftos(), &s, Access::Write));
    let (r, a) = g.authorize(&weftos(), &s, Access::Write, NOW);
    assert_eq!(r, Err(Refused));
    assert_eq!(a.reason, "weftos_not_granted");
    g.revoke(&weftos(), &s);
    assert!(!ok(&g, &weftos(), &s, Access::Read), "revoke refuses the next call");
}

#[test]
fn client_members_need_a_grant_and_other_accounts_never_pass() {
    let mut g = gate();
    let s = name(mesh(1), ULID);
    assert!(!ok(&g, &client("a"), &s, Access::Read));
    g.grant(StoreGrant { principal: client("a"), store: s.clone(), access: Access::Write });
    assert!(ok(&g, &client("a"), &s, Access::Read));
    assert!(ok(&g, &client("a"), &s, Access::Write));
    assert!(!ok(&g, &client("b"), &s, Access::Read));
    let other = Principal::Client { account: "rival".into(), id: "a".into() };
    g.grant(StoreGrant { principal: other.clone(), store: s.clone(), access: Access::Admin });
    assert!(!ok(&g, &other, &s, Access::Read), "a grant for another account's principal is dead");
}

#[test]
fn instance_never_serves_a_project_it_does_not_hold_or_a_non_store_name() {
    let mut g = gate();
    let held = name(mesh(1), ULID);
    let not_held = name(mesh(1), OTHER);
    g.grant(StoreGrant { principal: client("a"), store: not_held.clone(), access: Access::Admin });
    assert_eq!(g.authorize(&client("a"), &not_held, Access::Read, NOW).1.reason, "not_held_here");
    let sub = format!("weftos://{}/projects/{ULID}/store/x", hex::encode(mesh(1)));
    let node = format!("weftos://{}/nodes/n1/store", hex::encode(mesh(1)));
    for n in [sub.as_str(), node.as_str(), "nonsense", &format!("{held}?rev=sha256:{}", "0".repeat(64))] {
        assert!(!ok(&g, &client("a"), n, Access::Read), "{n}");
    }
}

#[test]
fn every_refusal_is_the_same_value() {
    let g = gate();
    let s = name(mesh(1), ULID);
    let errs: Vec<_> = [
        g.authorize(&weftos(), &s, Access::Read, NOW).0,
        g.authorize(&client("a"), &s, Access::Read, NOW).0,
        g.authorize(&client("a"), &name(mesh(1), OTHER), Access::Read, NOW).0,
        g.authorize(&Principal::Foreign { mesh: mesh(2), id: "x".into() }, &s, Access::Read, NOW).0,
        g.authorize(&client("a"), "junk", Access::Read, NOW).0,
    ]
    .into();
    assert!(errs.iter().all(|e| *e == Err(Refused)));
    assert_eq!(format!("{}", Refused), "refused");
}

fn xgrant(g: &mut StoreGate, owner: u8, recv: u8, store: &str, acc: Access, seq: u64, exp: u64) -> CrossMeshGrant {
    g.trust_mesh(mesh(owner), key(owner).verifying_key());
    CrossMeshGrant::sign(&key(owner), mesh(owner), mesh(recv), store, acc, seq, NOW - 10, exp)
}

#[test]
fn foreign_mesh_principal_needs_a_signed_grant_for_the_exact_store() {
    let mut g = gate();
    let s = name(mesh(1), ULID);
    let p = Principal::Foreign { mesh: mesh(2), id: "weaver".into() };
    assert!(!ok(&g, &p, &s, Access::Read));
    let grant = xgrant(&mut g, 1, 2, &s, Access::Read, 1, NOW + 100);
    g.install_xmesh(grant.clone(), NOW).unwrap();
    assert!(ok(&g, &p, &s, Access::Read));
    assert!(!ok(&g, &p, &s, Access::Write), "read grant does not write");
    let q = Principal::Foreign { mesh: mesh(3), id: "weaver".into() };
    assert!(!ok(&g, &q, &s, Access::Read), "a third mesh gets nothing");
    // Not the sibling store: exact thing only.
    g.hold_project(OTHER);
    assert!(!ok(&g, &p, &name(mesh(1), OTHER), Access::Read));
    // Revocation stops the next call; so does expiry.
    assert!(g.authorize(&p, &s, Access::Read, NOW + 100).0.is_err());
    g.revoke_xmesh(grant.id());
    assert!(!ok(&g, &p, &s, Access::Read));
}

#[test]
fn installing_a_grant_checks_signature_owner_receiver_ttl_and_seq() {
    let mut g = gate();
    let owner_name = name(mesh(9), ULID);
    // Receiving side: mesh 9 owns the name and shares it with us (mesh 1).
    let good = xgrant(&mut g, 9, 1, &owner_name, Access::Write, 5, NOW + 1000);
    // tampered access
    let mut t = good.clone();
    t.access = Access::Admin;
    assert_eq!(g.install_xmesh(t, NOW), Err(Refused));
    // signed by the wrong key
    let forged = CrossMeshGrant::sign(&key(7), mesh(9), mesh(1), &owner_name, Access::Write, 6, NOW - 1, NOW + 1000);
    assert_eq!(g.install_xmesh(forged, NOW), Err(Refused));
    // for some other receiver
    let wrong_recv = xgrant(&mut g, 9, 4, &owner_name, Access::Write, 7, NOW + 1000);
    assert_eq!(g.install_xmesh(wrong_recv, NOW), Err(Refused));
    // owner does not match the name's authority
    let wrong_owner = xgrant(&mut g, 9, 1, &name(mesh(8), ULID), Access::Write, 8, NOW + 1000);
    assert_eq!(g.install_xmesh(wrong_owner, NOW), Err(Refused));
    // ttl too long, already expired
    let long = xgrant(&mut g, 9, 1, &owner_name, Access::Write, 9, NOW - 10 + MAX_GRANT_TTL_SECS + 1);
    assert_eq!(g.install_xmesh(long, NOW), Err(Refused));
    let old = xgrant(&mut g, 9, 1, &owner_name, Access::Write, 10, NOW - 1);
    assert_eq!(g.install_xmesh(old, NOW), Err(Refused));
    // unknown owner key
    let mut h = gate();
    let unk = CrossMeshGrant::sign(&key(9), mesh(9), mesh(1), &owner_name, Access::Write, 1, NOW - 1, NOW + 5);
    assert_eq!(h.install_xmesh(unk, NOW), Err(Refused));
    // good one installs; replay of the same seq does not
    assert_eq!(g.install_xmesh(good.clone(), NOW), Ok(()));
    assert_eq!(g.install_xmesh(good, NOW), Err(Refused));
}

#[test]
fn client_principal_reaches_an_owner_mesh_name_only_with_both_grants() {
    let mut g = gate();
    let s = name(mesh(9), ULID);
    let p = client("a");
    g.grant(StoreGrant { principal: p.clone(), store: s.clone(), access: Access::Read });
    assert_eq!(g.authorize(&p, &s, Access::Read, NOW).1.reason, "no_xmesh_grant", "local grant alone is not enough");
    let x = xgrant(&mut g, 9, 1, &s, Access::Read, 1, NOW + 100);
    g.install_xmesh(x.clone(), NOW).unwrap();
    assert!(ok(&g, &p, &s, Access::Read));
    assert!(!ok(&g, &client("b"), &s, Access::Read), "xmesh grant alone is not enough");
    assert!(!ok(&g, &weftos(), &s, Access::Read), "weftos account still needs its own grant");
    g.revoke_xmesh(x.id());
    assert!(!ok(&g, &p, &s, Access::Read));
}

#[test]
fn iam_policy_is_one_bucket_and_refuses_odd_names() {
    let p = iam_policy("p-01j8zq4w2x9k3m5n7p0r2t4v6y", Access::Read).unwrap();
    assert!(p.contains("s3:GetObject") && !p.contains("PutObject") && !p.contains("s3:*"));
    assert_eq!(p.matches("arn:aws:s3:::p-01j8").count(), 2);
    assert!(iam_policy("p-x", Access::Admin).unwrap().contains("s3:*"));
    for b in ["", "A", "a\",\"x", "a/b", "*", &"a".repeat(64)] {
        assert!(iam_policy(b, Access::Read).is_err(), "{b:?}");
    }
    serde_json::from_str::<serde_json::Value>(&p).expect("valid JSON");
}

#[test]
fn a_newer_grant_replaces_the_older_so_a_downgrade_sticks() {
    let mut g = gate();
    let s = name(mesh(1), ULID);
    let p = Principal::Foreign { mesh: mesh(2), id: "w".into() };
    let w = xgrant(&mut g, 1, 2, &s, Access::Write, 1, NOW + 100);
    g.install_xmesh(w, NOW).unwrap();
    assert!(ok(&g, &p, &s, Access::Write));
    let r = xgrant(&mut g, 1, 2, &s, Access::Read, 2, NOW + 100);
    g.install_xmesh(r, NOW).unwrap();
    assert!(ok(&g, &p, &s, Access::Read));
    assert!(!ok(&g, &p, &s, Access::Write));
}

#[test]
fn listen_must_be_loopback_or_tailnet_in_every_spelling() {
    let root = PathBuf::from("/data1/acme/store");
    let spec = |a: &str| ClientStoreSpec::new("acme", 2102, root.clone(), a.parse().unwrap());
    for ok in ["127.0.0.1:9100", "[::1]:9100", "100.101.102.103:9100", "[fd7a:115c:a1e0::1]:9100", "[::ffff:127.0.0.1]:9100"] {
        assert!(spec(ok).is_ok(), "{ok}");
    }
    for bad in ["0.0.0.0:9100", "[::]:9100", "[::ffff:0.0.0.0]:9100", "203.0.113.5:9100", "192.168.1.5:9100", "100.128.0.1:9100", "[2001:db8::1]:9100"] {
        assert_eq!(spec(bad), Err(SpecError::Listen), "{bad}");
    }
}

#[test]
fn system_user_names_and_out_of_range_uids_are_not_client_accounts() {
    let l: SocketAddr = "127.0.0.1:9100".parse().unwrap();
    for a in ["root", "postgres", "nobody", "sshd"] {
        let r = PathBuf::from(format!("/data1/{a}/store"));
        assert_eq!(ClientStoreSpec::new(a, 2102, r, l), Err(SpecError::NotClient), "{a}");
    }
    let r = PathBuf::from("/data1/acme/store");
    assert_eq!(ClientStoreSpec::new("acme", 65534, r.clone(), l), Err(SpecError::UidRange(65534)));
    assert!(ClientStoreSpec::new("acme", 59_999, r, l).is_ok());
}

#[test]
fn unit_refuses_a_binary_path_that_could_inject_lines_and_allows_the_tailnet_listener() {
    let s = ClientStoreSpec::standard("acme", 2102, 9100).unwrap();
    for bad in ["/opt/x\nExecStartPre=/bin/sh", "rustfs", "/opt/a b/rustfs", "/opt/%n/rustfs", "/opt/$X/rustfs"] {
        assert_eq!(s.systemd_unit(Path::new(bad)), Err(SpecError::Binary), "{bad:?}");
    }
    let t = ClientStoreSpec::new("acme", 2102, PathBuf::from("/data1/acme/store"), "100.64.1.2:9100".parse().unwrap()).unwrap();
    assert!(t.systemd_unit(Path::new("/opt/rustfs")).unwrap().contains("IPAddressAllow=localhost 100.64.0.0/10"));
}

#[test]
fn a_grant_dated_in_the_future_is_not_live() {
    let mut g = gate();
    g.trust_mesh(mesh(2), key(2).verifying_key());
    let s = name(mesh(2), ULID);
    let far = CrossMeshGrant::sign(&key(2), mesh(2), mesh(1), &s, Access::Read, 1, NOW + 10 * 365 * 24 * 3600, NOW + 11 * 365 * 24 * 3600);
    assert!(g.install_xmesh(far, NOW).is_err(), "issued ten years ahead");
    let near = CrossMeshGrant::sign(&key(2), mesh(2), mesh(1), &s, Access::Read, 2, NOW + 60, NOW + 3600);
    assert!(g.install_xmesh(near, NOW).is_ok(), "a minute of skew is fine");
}

#[test]
fn a_local_downgrade_takes_effect_in_the_gate_alone() {
    let mut g = gate();
    let s = name(mesh(1), ULID);
    g.grant(StoreGrant { principal: client("c1"), store: s.clone(), access: Access::Write });
    g.grant(StoreGrant { principal: client("c1"), store: s.clone(), access: Access::Read });
    assert!(ok(&g, &client("c1"), &s, Access::Read));
    assert!(!ok(&g, &client("c1"), &s, Access::Write));
}

#[test]
fn gate_state_round_trips_and_drops_a_grant_whose_key_is_gone() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("gate.json");
    let mut g = gate();
    let s = name(mesh(1), ULID);
    g.grant(StoreGrant { principal: weftos(), store: s.clone(), access: Access::Read });
    g.trust_mesh(mesh(2), key(2).verifying_key());
    let x = CrossMeshGrant::sign(&key(2), mesh(2), mesh(1), &name(mesh(2), ULID), Access::Read, 1, NOW - 10, NOW + 100);
    g.install_xmesh(x.clone(), NOW).unwrap();
    g.revoke_xmesh(x.id());
    g.save(&path).unwrap();
    let mut back = StoreGate::load(Authority(mesh(1)), "acme", &path).unwrap();
    assert!(ok(&back, &weftos(), &s, Access::Read));
    assert!(back.install_xmesh(x, NOW).is_err(), "seq and revocation came back");
    // A file edited to carry a grant signed by someone else is not trusted on load.
    let text = std::fs::read_to_string(&path).unwrap().replace(&hex::encode(key(2).verifying_key().to_bytes()), &hex::encode(key(3).verifying_key().to_bytes()));
    std::fs::write(&path, text).unwrap();
    let tampered = StoreGate::load(Authority(mesh(1)), "acme", &path).unwrap();
    assert!(tampered.xmesh.is_empty());
}

#[test]
fn every_gate_event_round_trips_through_json_and_replays() {
    let s = name(mesh(1), ULID);
    let x = CrossMeshGrant::sign(&key(2), mesh(2), mesh(1), &name(mesh(2), ULID), Access::Read, 1, NOW - 10, NOW + 100);
    let evs = [
        GateEvent::Hold { ulid: ULID.into() },
        GateEvent::Grant(StoreGrant { principal: weftos(), store: s.clone(), access: Access::Write }),
        GateEvent::Grant(StoreGrant { principal: client("c1"), store: s.clone(), access: Access::Read }),
        GateEvent::Revoke { principal: Principal::Foreign { mesh: mesh(3), id: "z".into() }, store: s.clone() },
        GateEvent::Trust { mesh: mesh(2), key: key(2).verifying_key().to_bytes() },
        GateEvent::XmeshInstall(x.clone()),
        GateEvent::XmeshRevoke { id: x.id() },
    ];
    let mut g = StoreGate::new(Authority(mesh(1)), "acme");
    for e in &evs {
        let back = GateEvent::from_json(e.kind(), &e.to_json()).unwrap();
        assert_eq!(&back, e, "{}", e.kind());
        g.apply(&back, NOW).unwrap();
    }
    assert!(ok(&g, &client("c1"), &s, Access::Read));
    assert!(GateEvent::from_json("nope", &serde_json::json!({})).is_err());
    assert!(GateEvent::from_json("grant", &serde_json::json!({"store": "x"})).is_err());
}
