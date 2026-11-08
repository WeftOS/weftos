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
fn live_hard_dying_watch_loop() {
    let l = Live::new("kl");
    crash_mid_batch(&l.be, &l.bucket, 3);
}

#[test]
#[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
fn live_hard_dying_watch_loop_after_a_create() {
    let l = Live::new("k5");
    crash_mid_batch(&l.be, &l.bucket, 5);
}

#[test]
#[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
fn live_hard_tombstone_collection_cycle() {
    use super::{Options, collect_tombstones, materialise, sync_up, sync_up_with};
    let l = Live::new("gc");
    let (be, bucket) = (&l.be, l.bucket.as_str());
    for (k, v) in [("p/a.txt", "alpha"), ("p/b.txt", "beta")] {
        be.put(bucket, k, v.as_bytes(), false).unwrap();
    }
    let d = tempfile::tempdir().unwrap();
    let dest = d.path().join("t");
    materialise(be, bucket, "p", &dest).unwrap();
    std::fs::remove_file(dest.join("a.txt")).unwrap();
    sync_up(be, &dest).unwrap();
    assert!(be.head(bucket, "p/.drive-tombstones/a.txt").is_ok());
    // Switch off: the marker stays however old the clock says it is.
    let off = Options { tombstone_retention: std::time::Duration::from_millis(1), ..Options::default() };
    let far = super::now_ms() + 3_600_000;
    assert_eq!(collect_tombstones(be, bucket, "p", &off, far).unwrap().examined, 0);
    assert!(be.head(bucket, "p/.drive-tombstones/a.txt").is_ok());
    // Age it past R by shrinking R; a second, younger marker survives a window that excludes it.
    std::thread::sleep(std::time::Duration::from_millis(1500));
    std::fs::remove_file(dest.join("b.txt")).unwrap();
    sync_up(be, &dest).unwrap();
    let on = Options { collect_tombstones: true, tombstone_retention: std::time::Duration::from_millis(1000), ..Options::default() };
    let rep = collect_tombstones(be, bucket, "p", &on, super::now_ms()).unwrap();
    assert_eq!((rep.collected.clone(), rep.kept_young), (vec!["a.txt".to_string()], 1), "{rep:?}");
    assert!(be.head(bucket, "p/.drive-tombstones/a.txt").is_err());
    assert!(be.head(bucket, "p/.drive-tombstones/b.txt").is_ok());
    // Through sync_up: a window that covers b as well collects it after the push.
    let all = Options { tombstone_retention: std::time::Duration::from_millis(1), ..on };
    std::thread::sleep(std::time::Duration::from_millis(20));
    let up = sync_up_with(be, &dest, &all).unwrap();
    assert_eq!(up.gc.unwrap().collected, ["b.txt"]);
    assert!(be.head(bucket, "p/.drive-tombstones/b.txt").is_err());
}
