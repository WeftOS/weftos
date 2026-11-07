use std::sync::Arc;

use clawft_kernel::exposure::ExposureRegistry;

use super::*;

#[test]
fn node_ids_become_store_tokens() {
    assert_eq!(node_token("PG_1.local"), "pg-1-local");
    assert_eq!(node_token(""), "node");
    assert_eq!(node_token(&"a".repeat(100)).len(), 61);
    assert!(StoreName::node("m", &node_token("Weird Node/ID")).is_ok());
}

#[test]
fn start_creates_the_node_store_once_and_it_is_an_exposure() {
    let d = tempfile::tempdir().unwrap();
    let reg = Arc::new(ExposureRegistry::new("pg-1"));
    let svc = start("pg-1", d.path(), reg.clone(), None).unwrap();
    let stores = svc.list_stores();
    assert_eq!(stores.len(), 1);
    assert_eq!(stores[0].kind, "node");
    assert!(stores[0].name.ends_with("/nodes/pg-1/store"), "{}", stores[0].name);
    assert!(reg.get(&format!("store.{}", stores[0].bucket)).is_some());
    assert!(global().is_some());
    assert!(Arc::ptr_eq(&svc, &start("pg-1", d.path(), reg, None).unwrap()), "the first start wins");
}
