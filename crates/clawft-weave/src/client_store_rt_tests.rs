use weftos_store::audit::MemSink;
use weftos_store::service::OpenGate;

use super::*;

const MESH: &str = "0101010101010101010101010101010101010101010101010101010101010101";

fn cfg(account: &str) -> ClientStoreConfig {
    ClientStoreConfig { account: account.into(), port: 9100 }
}
fn open(c: &ClientStoreConfig, euid: u32, mesh: &str) -> Result<ClientStore, String> {
    open_as(c, euid, mesh, Arc::new(OpenGate), Arc::new(MemSink::default()))
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
