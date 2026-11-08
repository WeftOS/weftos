//! The hardening scenarios of `tests_hardening.rs` against a REAL RustFS. All ignored.
//!
//! Point `WEFTOS_STORE_TEST_RUSTFS_ENV` at the env file of a THROWAWAY loopback RustFS and run
//!   scripts/build.sh test weftos-store --features rustfs --ignored --filter live_hard
//! Never point it at PG. Each test uses its own bucket and removes it afterwards.

use std::path::Path;

use super::tests_crash::crash_mid_batch;
use super::tests_hardening::*;
use crate::backend::{AdminBackend, ObjectBackend};
use crate::rustfs::{RustfsBackend, RustfsConfig};

struct Live {
    be: RustfsBackend,
    bucket: String,
    _secrets: tempfile::TempDir,
}

impl Live {
    fn new(tag: &str) -> Self {
        let envf = std::env::var("WEFTOS_STORE_TEST_RUSTFS_ENV").expect("WEFTOS_STORE_TEST_RUSTFS_ENV");
        let secrets = tempfile::tempdir().unwrap();
        let cfg = RustfsConfig::from_env_file(Path::new(&envf), secrets.path().join("secrets")).unwrap();
        let be = RustfsBackend::new(cfg).unwrap();
        let bucket = format!("t-hard-{tag}-{}", &ulid::Ulid::new().to_string().to_lowercase()[16..]);
        be.create_bucket(&bucket, &[]).unwrap();
        Self { be, bucket, _secrets: secrets }
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        if let Ok(list) = self.be.list(&self.bucket, "") {
            for m in list {
                let _ = self.be.delete(&self.bucket, &m.key);
            }
        }
        let _ = self.be.delete_bucket(&self.bucket);
    }
}

#[test]
#[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
fn live_hard_race_sync_up_first() {
    let l = Live::new("up");
    race_sync_up_first(&l.be, &l.bucket);
}

#[test]
#[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
fn live_hard_race_sync_down_first() {
    let l = Live::new("dn");
    race_sync_down_first(&l.be, &l.bucket);
}

#[test]
#[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
fn live_hard_race_in_the_quiet_period() {
    let l = Live::new("qt");
    race_in_the_quiet_period(&l.be, &l.bucket);
}

#[test]
#[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
fn live_hard_tombstone_cycle() {
    let l = Live::new("tb");
    tombstone_cycle(&l.be, &l.bucket);
}

#[test]
#[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
fn live_hard_pull_that_lost_its_index_save() {
    let l = Live::new("ix");
    pull_that_lost_its_index_save(&l.be, &l.bucket);
}

#[test]
#[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
fn live_hard_killed_watch_loop() {
    let l = Live::new("kl");
    crash_mid_batch(&l.be, &l.bucket, "rustfs", 3);
}

#[test]
#[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
fn live_hard_killed_watch_loop_after_a_create() {
    let l = Live::new("k5");
    crash_mid_batch(&l.be, &l.bucket, "rustfs", 5);
}
