use weftos_store::audit::MemSink;
use weftos_store::service::OpenGate;

use super::*;

const MESH: &str = "0101010101010101010101010101010101010101010101010101010101010101";

fn cfg(account: &str) -> ClientStoreConfig {
    ClientStoreConfig { account: account.into(), port: 9100 }
}
fn open(c: &ClientStoreConfig, euid: u32, mesh: &str) -> Result<ClientStore, String> {
    open_as(c, euid, mesh, Arc::new(OpenGate), Arc::new(MemSink::default()), Arc::new(weftos_store::service::NoChain), None, None)
}

#[test]
fn config_has_a_default_port_and_refuses_unknown_fields() {
    assert_eq!(parse_config(r#"{"account":"acme"}"#).unwrap(), cfg("acme"));
    assert_eq!(parse_config(r#"{"account":"acme","port":9200}"#).unwrap().port, 9200);
    assert!(parse_config(r#"{"account":"acme","root":"/tmp/x"}"#).is_err(), "the root is not configurable");
    assert!(parse_config("not json").is_err());
}

#[test]
fn a_daemon_below_the_client_uid_range_never_opens_a_client_store() {
    // The weftos daemon's own uid is below 2102, so it cannot serve a client account's store.
    let e = open(&cfg("acme"), 1000, MESH).err().unwrap();
    assert!(e.contains("below the client range"), "{e}");
}

#[test]
fn the_weftos_account_and_system_names_are_not_served() {
    assert!(open(&cfg("weftos"), 2102, MESH).is_err());
    assert!(open(&cfg("postgres"), 2102, MESH).is_err());
}

#[test]
fn no_mesh_id_yet_waits_instead_of_opening_under_local() {
    let e = open(&cfg("acme"), 2102, "local").err().unwrap();
    assert!(e.contains("no mesh id"), "{e}");
}

#[test]
fn a_root_the_process_does_not_own_is_refused() {
    // /data1/<account>/store does not exist here (or is not ours); either way verify_on_disk refuses.
    let e = open(&cfg("no-such-client-account"), 2102, MESH).err().unwrap();
    assert!(e.contains("owned by the client user"), "{e}");
}

#[cfg(feature = "exochain")]
mod chain_keys {
    use ed25519_dalek::SigningKey;

    use super::*;
    use crate::client_store_snapshot::{Ed25519Key, Rotation, SnapshotKey};
    use crate::client_store_state::LoggedEvent;

    fn k(n: u8) -> SigningKey {
        SigningKey::from_bytes(&[n; 32])
    }
    fn ev(seq: u64, source: &str, p: &clawft_kernel::key_rotation::KeyRotationPayload) -> LoggedEvent {
        LoggedEvent { seq, source: source.into(), kind: clawft_kernel::key_rotation::KIND_KEY_ROTATION.into(), payload: serde_json::to_value(p).ok() }
    }

    #[test]
    fn rotations_come_from_dual_signed_identity_events_only() {
        use clawft_kernel::key_rotation::{SOURCE_IDENTITY, build_key_rotation_payload};
        let good = build_key_rotation_payload(&k(1), &k(2), 3600, 1000);
        let mut forged = build_key_rotation_payload(&k(2), &k(3), 3600, 2000);
        forged.new_signature_hex = good.new_signature_hex.clone();
        let events = [ev(5, SOURCE_IDENTITY, &good), ev(9, SOURCE_IDENTITY, &forged), ev(7, "other", &good)];
        assert_eq!(
            rotations_in(&events),
            vec![Rotation { old: k(1).verifying_key().to_bytes(), new: k(2).verifying_key().to_bytes(), seq: 5 }]
        );
    }

    #[test]
    fn the_chain_key_signs_with_the_current_key_and_honours_the_rotation() {
        use clawft_kernel::chain::ChainManager;
        let mut chain = ChainManager::new(0, 100).with_signing_key(k(11));
        let (rotation, new_key) = clawft_kernel::key_rotation::rotate_chain_signing_key_now(&mut chain, 3600).unwrap();
        let ck = ChainKey(Arc::new(chain));
        let (old_id, old_sig) = Ed25519Key(k(11)).sign(b"msg").unwrap();
        assert!(ck.verify(&old_id, b"msg", &old_sig, rotation.sequence.saturating_sub(1)), "written before the rotation");
        assert!(!ck.verify(&old_id, b"msg", &old_sig, rotation.sequence + 1), "written after it");
        let (id, sig) = ck.sign(b"msg").unwrap();
        assert_eq!(id, hex::encode(new_key.verifying_key().to_bytes()));
        assert!(ck.verify(&id, b"msg", &sig, rotation.sequence + 5));
    }
}
