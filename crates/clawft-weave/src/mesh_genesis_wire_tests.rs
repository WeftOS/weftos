//! Where the mesh id travels: the pair action, the heartbeat, `project add`.
//! Temp dirs and a fake dashboard only.

use std::sync::Arc;

use clawft_kernel::chain::ChainManager;
use clawft_kernel::licence::LocalMeshId;
use serde_json::{Value, json};

use super::tests::{NOW, key, pk};
use super::*;
use crate::dashboard_test_support::*;
use crate::mesh_pair::{self, PairDeps};
use crate::weftos_uri::Authority;

fn node_id(seed: u8) -> String {
    clawft_kernel::node_id_from_pubkey(&pk(seed))
}

/// A `pair` payload for a member whose primary is the node with key seed `primary`.
fn payload(primary: u8, record: Option<&SignedMeshRecord>) -> Value {
    let node = node_id(primary);
    let mut p = json!({
        "op": "add", "request_id": "r-1", "peer_node": node, "fingerprint": &node[..16],
        "peer_ed25519": hex::encode(pk(primary)), "advertise": "100.64.0.9:9471",
        "role": "primary", "projects": [],
    });
    if let Some(r) = record {
        p["mesh_record"] = serde_json::to_value(r).unwrap();
    }
    p
}

fn deps(dir: &Path) -> PairDeps {
    PairDeps { runtime_dir: dir.to_path_buf(), chain: Some(Arc::new(ChainManager::new(0, 1000))), self_node: Some(node_id(9)) }
}

#[test]
fn a_member_learns_the_mesh_id_from_the_primarys_pair_payload() {
    let primary = tempfile::tempdir().unwrap();
    let rec = create(primary.path(), &key(1), &key(2), None, NOW).unwrap();
    let member = tempfile::tempdir().unwrap();
    let out = mesh_pair::apply(&deps(member.path()), "a1", &payload(1, Some(&rec))).unwrap();
    assert_eq!(out["peer_node"], node_id(1));
    let r = resolve(member.path(), None).unwrap().unwrap();
    assert_eq!(r.id.to_hex(), rec.record.mesh_id);
    assert_eq!(r.source, IdSource::Learned { node: node_id(1) });
    assert!(member.path().join(mesh_pair::PEERS_FILE).exists());
}

#[test]
fn a_tampered_or_foreign_signed_record_fails_the_pair_and_writes_nothing() {
    let primary = tempfile::tempdir().unwrap();
    let rec = create(primary.path(), &key(1), &key(2), None, NOW).unwrap();
    let member = tempfile::tempdir().unwrap();
    let mut tampered = rec.clone();
    tampered.record.created_at = "2031-01-01T00:00:00Z".into();
    let e = mesh_pair::apply(&deps(member.path()), "a1", &payload(1, Some(&tampered))).unwrap_err();
    assert!(e.contains("signature"), "{e}");
    // A genuine record, but the pairing pins another node.
    let e = mesh_pair::apply(&deps(member.path()), "a2", &payload(5, Some(&rec))).unwrap_err();
    assert!(e.contains("pins"), "{e}");
    assert!(!member.path().join(mesh_pair::PEERS_FILE).exists(), "no trust file on a refused pair");
    assert!(load(member.path()).unwrap().is_none());
    // A malformed record fails the payload parse.
    let mut p = payload(1, None);
    p["mesh_record"] = json!({ "record": 1 });
    assert!(mesh_pair::apply(&deps(member.path()), "a3", &p).is_err());
}

#[test]
fn a_pair_without_a_record_still_works_and_leaves_the_mesh_id_unset() {
    let member = tempfile::tempdir().unwrap();
    mesh_pair::apply(&deps(member.path()), "a1", &payload(1, None)).unwrap();
    assert_eq!(resolve(member.path(), None).unwrap(), None);
}

#[tokio::test]
async fn the_heartbeat_reports_mesh_id_only_when_set_and_the_record_that_backs_it() {
    let (_t, path) = token_dir(&token('a'));
    let dash = dashboard(config("http://127.0.0.1:1", &path));
    let local = LocalMeshId::unset();
    assert!(dash.set_mesh_id_source(local.clone()));
    let body = dash.build_body().await;
    assert!(body["report"].get("mesh_id").is_none(), "omitted when unset");
    assert!(body["report"].get("mesh_record").is_none());

    let id = MeshId::derive(&[7; 32], &[8; 32]);
    local.set(Some(id));
    let body = dash.build_body().await;
    let got = body["report"]["mesh_id"].as_str().unwrap();
    assert_eq!(got, id.to_hex());
    assert!(got.len() == 64 && got.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')));
    assert!(body["report"].get("mesh_record").is_none(), "a licence-only id has no record");
}

#[test]
fn attach_includes_the_record_only_when_it_names_the_reported_id() {
    let d = tempfile::tempdir().unwrap();
    let rec = create(d.path(), &key(1), &key(2), None, NOW).unwrap();
    let id = MeshId::from_hex(&rec.record.mesh_id).unwrap();
    let mut r = json!({});
    attach(&mut r, Some(id), Some(d.path()));
    assert_eq!(r["mesh_id"], rec.record.mesh_id);
    assert_eq!(serde_json::from_value::<SignedMeshRecord>(r["mesh_record"].clone()).unwrap(), rec);
    // The id in use is a licence id that differs from the record: no record.
    let mut r = json!({});
    attach(&mut r, Some(MeshId::derive(&[7; 32], &[8; 32])), Some(d.path()));
    assert!(r.get("mesh_id").is_some() && r.get("mesh_record").is_none());
    let mut r = json!({});
    attach(&mut r, None, Some(d.path()));
    assert_eq!(r, json!({}));
}

#[test]
fn project_add_authority_must_equal_the_local_mesh_id() {
    let mine = MeshId::derive(&[1; 32], &[2; 32]);
    let theirs = MeshId::derive(&[3; 32], &[4; 32]);
    let auth = |id: MeshId| Authority(hex::decode(id.to_hex()).unwrap().try_into().unwrap());
    assert!(check_authority(Some(mine), &auth(mine)).is_ok());
    let e = check_authority(Some(mine), &auth(theirs)).unwrap_err();
    assert!(e.contains(&mine.to_hex()) && e.contains(&theirs.to_hex()), "{e}");
    let e = check_authority(None, &auth(theirs)).unwrap_err();
    assert!(e.contains("no MeshId") && e.contains("weaver mesh create"), "{e}");
}

fn boot(dir: &Path, mesh: Option<&clawft_types::config::MeshConfig>) -> crate::licence_boot::LicenceRuntime {
    use clawft_kernel::workload_pkg::TrustAnchors;
    crate::licence_boot::build(crate::licence_boot::InitArgs {
        dir,
        anchors: TrustAnchors::default(),
        revocations: Arc::new(clawft_kernel::revocation::RevocationList::new(dir.join("revoked.json"))),
        chain: Arc::new(ChainManager::new(0, 1000)),
        mesh,
        steward_node_id: "node-steward".into(),
        steward_pubkey: hex::encode(pk(21)),
    })
}

#[test]
fn the_daemon_boots_with_the_record_id_the_licence_id_or_none() {
    let d = tempfile::tempdir().unwrap();
    let rt = boot(d.path(), None);
    assert_eq!((rt.local.get(), rt.mesh_source.clone(), rt.record_issue.clone()), (None, None, None));

    let rec = create(d.path(), &key(1), &key(2), None, NOW).unwrap();
    let rt = boot(d.path(), None);
    assert_eq!(rt.local.get().map(|i| i.to_hex()), Some(rec.record.mesh_id.clone()));
    assert_eq!(rt.mesh_source, Some(IdSource::Genesis));

    // A licence id outranks the record; the disagreement is reported.
    let cfg = clawft_types::config::MeshConfig {
        genesis_hash: Some("aa".repeat(32)),
        mesh_nonce: Some("bb".repeat(32)),
        ..Default::default()
    };
    let rt = boot(d.path(), Some(&cfg));
    assert_eq!(rt.mesh_source, Some(IdSource::Licence));
    assert_ne!(rt.local.get().map(|i| i.to_hex()), Some(rec.record.mesh_id));
    assert!(rt.record_issue.as_deref().unwrap().contains("licence id is in use"));
}

#[test]
fn a_broken_record_boots_with_no_mesh_id_and_says_why() {
    let d = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(d.path().join(RECORD_DIR)).unwrap();
    std::fs::write(record_path(d.path()), "garbage").unwrap();
    let rt = boot(d.path(), None);
    assert_eq!(rt.local.get(), None);
    assert!(rt.record_issue.is_some());
}
