//! `weaver mesh import`: trust, tamper, conflict, idempotence. Temp dirs only.

use std::os::unix::fs::PermissionsExt;

use super::tests::{NOW, key, pk};
use super::*;
use crate::mesh_import::{Imported, import, parse, signer_trust};

fn write_0600(dir: &Path, name: &str, text: &str) {
    let p = dir.join(name);
    std::fs::write(&p, text).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
}

/// A record made on "the Mac": node key seed 1, user key seed 2.
fn mac_record() -> SignedMeshRecord {
    let d = tempfile::tempdir().unwrap();
    create(d.path(), &key(1), &key(2), None, NOW).unwrap()
}

fn trust_peer(dir: &Path, seed: u8, tier: &str) {
    write_0600(dir, "workload-peers.json", &format!(r#"[{{"key":"{}","tier":"{tier}"}}]"#, hex::encode(pk(seed))));
}

#[test]
fn export_import_round_trip_with_a_paired_signer() {
    let mac = tempfile::tempdir().unwrap();
    let rec = create(mac.path(), &key(1), &key(2), None, NOW).unwrap();
    let mut out = Vec::new();
    crate::commands::mesh_id_cmd::export_with(&mut out, mac.path(), None).unwrap();
    let text = String::from_utf8(out).unwrap();
    // No secrets in the export (node key seed 1, user key seed 2).
    assert!(!text.contains(&hex::encode([1u8; 32])) && !text.contains(&hex::encode([2u8; 32])));
    assert!(!text.contains("origin"));

    let pg = tempfile::tempdir().unwrap();
    trust_peer(pg.path(), 1, "paired");
    let got = import(pg.path(), &parse(&text).unwrap(), None).unwrap();
    assert_eq!(got, Imported::Written(MeshId::from_hex(&rec.record.mesh_id).unwrap()));
    let stored = load(pg.path()).unwrap().unwrap();
    assert_eq!(stored.signed, rec);
    assert_eq!(stored.origin, Origin::Imported { node: clawft_kernel::node_id_from_pubkey(&pk(1)) });
    assert_eq!(std::fs::metadata(record_path(pg.path())).unwrap().permissions().mode() & 0o777, 0o600);
    let r = resolve(pg.path(), None).unwrap().unwrap();
    assert!(r.source.describe().starts_with("imported from "), "{}", r.source.describe());
}

#[test]
fn export_without_a_record_refuses() {
    let d = tempfile::tempdir().unwrap();
    let mut out = Vec::new();
    assert!(crate::commands::mesh_id_cmd::export_with(&mut out, d.path(), None).is_err());
    assert!(out.is_empty());
}

#[test]
fn an_untrusted_signer_is_refused_naming_it_and_nothing_is_written() {
    let pg = tempfile::tempdir().unwrap();
    let e = import(pg.path(), &mac_record(), None).unwrap_err();
    assert!(e.contains(&hex::encode(pk(1))) && e.contains("workload-peers.json"), "{e}");
    assert!(!record_path(pg.path()).exists());
    // A discovered-tier entry is not trust.
    trust_peer(pg.path(), 1, "discovered");
    assert!(import(pg.path(), &mac_record(), None).is_err());
    // Trusting some other key does not help.
    trust_peer(pg.path(), 9, "pinned");
    assert!(import(pg.path(), &mac_record(), None).is_err());
}

#[test]
fn a_workload_host_controller_key_is_trusted() {
    let pg = tempfile::tempdir().unwrap();
    write_0600(
        pg.path(),
        "workload-host.json",
        &format!(r#"{{"listen":"127.0.0.1:9471","controllers":["{}"]}}"#, hex::encode(pk(1))),
    );
    assert_eq!(signer_trust(pg.path(), &pk(1)).unwrap(), "workload-host.json controllers");
    assert!(matches!(import(pg.path(), &mac_record(), None).unwrap(), Imported::Written(_)));
}

#[test]
fn a_pairing_node_is_trusted() {
    let pg = tempfile::tempdir().unwrap();
    let node = clawft_kernel::node_id_from_pubkey(&pk(1));
    crate::mesh_pairings::upsert(pg.path(), &node, "primary", &[], "100.64.0.9:9471").unwrap();
    assert!(matches!(import(pg.path(), &mac_record(), None).unwrap(), Imported::Written(_)));
}

#[test]
fn a_tampered_record_is_refused_even_from_a_trusted_signer() {
    let pg = tempfile::tempdir().unwrap();
    trust_peer(pg.path(), 1, "pinned");
    let mut r = mac_record();
    r.record.created_at = "2030-01-01T00:00:00Z".into();
    assert!(import(pg.path(), &r, None).unwrap_err().contains("does not verify"));
    let mut r = mac_record();
    r.record.mesh_id = "0".repeat(64);
    assert!(import(pg.path(), &r, None).unwrap_err().contains("not derived"));
    assert!(!record_path(pg.path()).exists());
}

#[test]
fn a_conflicting_record_or_licence_is_refused() {
    let pg = tempfile::tempdir().unwrap();
    trust_peer(pg.path(), 1, "paired");
    let other = create(tempfile::tempdir().unwrap().path(), &key(1), &key(2), None, NOW).unwrap();
    let first = mac_record();
    import(pg.path(), &first, None).unwrap();
    let e = import(pg.path(), &other, None).unwrap_err();
    assert!(e.contains(&first.record.mesh_id) && e.contains(&other.record.mesh_id), "{e}");
    assert_eq!(load(pg.path()).unwrap().unwrap().signed, first);

    let pg2 = tempfile::tempdir().unwrap();
    trust_peer(pg2.path(), 1, "paired");
    let licence = MeshId::derive(&[7; 32], &[8; 32]);
    let e = import(pg2.path(), &first, Some(licence)).unwrap_err();
    assert!(e.contains("Seed licence"), "{e}");
    assert!(!record_path(pg2.path()).exists());
}

#[test]
fn importing_the_same_record_twice_is_a_no_op_success() {
    let pg = tempfile::tempdir().unwrap();
    trust_peer(pg.path(), 1, "paired");
    let r = mac_record();
    assert!(matches!(import(pg.path(), &r, None).unwrap(), Imported::Written(_)));
    let before = std::fs::read_to_string(record_path(pg.path())).unwrap();
    assert!(matches!(import(pg.path(), &r, None).unwrap(), Imported::AlreadyHeld(_)));
    assert_eq!(std::fs::read_to_string(record_path(pg.path())).unwrap(), before);
}

#[test]
fn malformed_input_refuses() {
    assert!(parse("not json").is_err());
    assert!(parse(r#"{"record":{}}"#).is_err());
}
