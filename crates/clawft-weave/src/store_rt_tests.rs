use std::sync::Arc;

use clawft_kernel::exposure::ExposureRegistry;

use super::*;

#[test]
fn node_ids_become_topic_tokens_and_only_a_clean_id_becomes_a_store_token() {
    // The topic token is lossy on purpose.
    assert_eq!(topic_token("PG_1.local"), "pg-1-local");
    assert_eq!(topic_token(""), "node");
    assert_eq!(topic_token(&"a".repeat(100)).len(), 61);
    assert!(StoreName::node("m", &topic_token("Weird Node/ID")).is_ok());
    // The store token is the id itself or nothing: nothing cleaned up can share a bucket.
    assert_eq!(node_token("pg-1").unwrap(), "pg-1");
    assert_eq!(node_token(&"a".repeat(61)).unwrap().len(), 61);
    for lossy in ["pg.1", "PG-1", "pg_1", "pg 1", "pg/1", "", &"a".repeat(62), "pg-1\n"] {
        let e = node_token(lossy).unwrap_err();
        assert!(e.contains("collid"), "{lossy:?}: {e}");
    }
    // The ids that used to collide are told apart: one is a store token, the other is refused.
    assert_eq!(topic_token("pg.1"), topic_token("pg-1"));
    assert!(node_token("pg-1").is_ok() && node_token("pg.1").is_err());
}

fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
    move |k| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| (*v).to_owned())
}

#[test]
fn the_archive_schedule_is_off_unless_an_interval_is_set_and_has_no_built_in_days() {
    assert_eq!(archive_schedule_from(env(&[])).unwrap(), None);
    // Day numbers alone do not turn it on.
    assert_eq!(archive_schedule_from(env(&[("WEFTOS_STORE_ARCHIVE_NODE_DAYS", "30")])).unwrap(), None);
    let s = archive_schedule_from(env(&[("WEFTOS_STORE_ARCHIVE_GC_EVERY_HOURS", "24")])).unwrap().unwrap();
    assert_eq!((s.every_hours, s.keep, s.confirmed), (24, weftos_store::service::ArchiveKeep::default(), false));
}

#[test]
fn the_archive_schedule_reads_days_and_confirm_and_refuses_bad_values() {
    let s = archive_schedule_from(env(&[
        ("WEFTOS_STORE_ARCHIVE_GC_EVERY_HOURS", " 6 "),
        ("WEFTOS_STORE_ARCHIVE_NODE_DAYS", "30"),
        ("WEFTOS_STORE_ARCHIVE_RELEASE_DAYS", "365"),
        ("WEFTOS_STORE_ARCHIVE_GC_CONFIRM", "true"),
    ]))
    .unwrap()
    .unwrap();
    assert_eq!((s.every_hours, s.keep.node_days, s.keep.project_days, s.keep.release_days, s.confirmed), (6, Some(30), None, Some(365), true));
    for bad in [
        &[("WEFTOS_STORE_ARCHIVE_GC_EVERY_HOURS", "daily")][..],
        &[("WEFTOS_STORE_ARCHIVE_GC_EVERY_HOURS", "0")][..],
        &[("WEFTOS_STORE_ARCHIVE_GC_EVERY_HOURS", "1"), ("WEFTOS_STORE_ARCHIVE_NODE_DAYS", "0")][..],
        &[("WEFTOS_STORE_ARCHIVE_GC_EVERY_HOURS", "1"), ("WEFTOS_STORE_ARCHIVE_PROJECT_DAYS", "-3")][..],
    ] {
        let bad: &'static [(&str, &str)] = Box::leak(bad.to_vec().into_boxed_slice());
        assert!(archive_schedule_from(env(bad)).is_err(), "{bad:?}");
    }
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

/// The daemon's wiring over a real RustFS: node store created, exposure registered, a governed
/// write lands as an object. Needs a THROWAWAY RustFS (never PG): set WEFTOS_STORE_TEST_RUSTFS_ENV
/// to an env file with MESH_S3_ENDPOINT / MESH_S3_ACCESS_KEY / MESH_S3_SECRET_KEY.
#[test]
#[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
fn live_rustfs_backs_the_daemon_service() {
    use weftos_store::{Access, Principal, service::Caller};
    let env = std::path::PathBuf::from(std::env::var("WEFTOS_STORE_TEST_RUSTFS_ENV").expect("WEFTOS_STORE_TEST_RUSTFS_ENV"));
    let d = tempfile::tempdir().unwrap();
    let reg = Arc::new(ExposureRegistry::new("live-1"));
    let node_id = format!("live-{}", &ulid::Ulid::new().to_string().to_lowercase()[10..]);
    let svc = open_service(&node_id, d.path(), reg.clone(), None, Some(env)).unwrap();
    let node = svc.list_stores().into_iter().next().expect("node store");
    assert!(reg.get(&format!("store.{}", node.bucket)).is_some());
    let name = StoreName::parse(&node.name).unwrap();
    let cog = Principal::parse("cog:live").unwrap();
    svc.grant(&name, cog.clone(), Access::Write, "test").unwrap();
    let who = Caller::local(cog.clone());
    svc.put(&who, &name, "agents/live/out.txt", b"hi", false).unwrap();
    assert_eq!(svc.get(&who, &name, "agents/live/out.txt").unwrap(), b"hi");
    assert!(d.path().join("store-secrets").read_dir().unwrap().count() >= 1, "the cog's key secret is kept 0600");
    svc.set_grant_enabled(&name, &cog, false, "test").unwrap();
    assert!(svc.get(&who, &name, "agents/live/out.txt").is_err());
}

#[test]
fn project_added_makes_the_project_store_once() {
    let d = tempfile::tempdir().unwrap();
    let svc = open_service("pg-1", d.path(), Arc::new(ExposureRegistry::new("pg-1")), None, None).unwrap();
    let ulid = "01HZX3K9Q5M2N7P8R4T6V0WABC";
    let a = ensure_project_store(&svc, ulid).unwrap();
    assert!(a.ends_with(&format!("/projects/{ulid}/store")), "{a}");
    assert_eq!(ensure_project_store(&svc, ulid).unwrap(), a, "idempotent");
    assert!(svc.has_store(&StoreName::project(&mesh_token(), ulid).unwrap()));
    assert!(ensure_project_store(&svc, "not-a-ulid").is_err());
}
