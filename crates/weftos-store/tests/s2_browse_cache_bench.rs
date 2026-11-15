//! WO-069 lane F bench: browse, registry list and follower poll cost with and without the manifest
//! cache, against a THROWAWAY RustFS (`WEFTOS_STORE_TEST_RUSTFS_ENV`, never PG). Prints `BENCH|` lines.
//! Run via `scripts/store/wo069-live-test.sh --bench`.
#![cfg(feature = "rustfs")]

use std::time::Instant;
use weftos_store::backend::AdminBackend;
use weftos_store::follower::*;
use weftos_store::follower_browse::{browse_get, browse_ls, browse_registry};
use weftos_store::follower_cache::*;
use weftos_store::overlay::ObjectBackend;
use weftos_store::promote::{WorktreeMeta, write_meta};
use weftos_store::share::*;
use weftos_store::share_fetch::*;
use weftos_store::testing::{Counts, CountingBackend};
use weftos_store::{RustfsBackend, RustfsConfig};

const WORKTREES: usize = 5;
const FILES: usize = 200;
const OPS: usize = 50;
const POLLS: usize = 20;
/// Simulated time between two dashboard operations.
const STEP_MS: u64 = 100;

type Be = CountingBackend<RustfsBackend>;

fn publish(be: &Be, wb: &str, pb: &str, slug: &str) {
    let d = tempfile::tempdir().unwrap();
    let mut paths = vec![];
    for i in 0..FILES {
        let p = format!("d{}/f{}.md", i % 10, i);
        let f = d.path().join(&p);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, format!("# {slug} file {i}\n{}\n", "lorem ipsum ".repeat(40 + i % 30))).unwrap();
        paths.push(p);
    }
    let src = TreeSource {
        root: d.path(), paths: &paths, uncommitted: &[], changes: &[], branch: slug, base_commit: "base",
        head_commit: "head1", bundle: None, writer: Actor { agent: "agent-a".into(), host: "machine-a".into() },
    };
    let meta = WorktreeMeta { slug: slug.into(), project: "01abc".into(), branch: slug.into(), base_commit: "base".into(), created_ms: 1 };
    write_meta(be, wb, &meta).unwrap();
    publish_tree(be, wb, &src, &Limits::default(), 1_000).unwrap();
    let e = summarize(be, slug, wb).unwrap().unwrap();
    write_registry_entry(be, pb, &e).unwrap();
}

fn report(workload: &str, mode: &str, wall: std::time::Duration, c: Counts) {
    println!(
        "BENCH|{workload}|{mode}|wall_ms={}|get={}|head={}|list={}|put={}|put_cas={}|delete={}|copy={}|bytes={}",
        wall.as_millis(), c.get, c.head, c.list, c.put, c.put_cas, c.delete, c.copy, c.bytes_read
    );
}

fn measure(be: &Be, workload: &str, mode: &str, f: impl FnOnce()) {
    be.reset();
    let t = Instant::now();
    f();
    report(workload, mode, t.elapsed(), be.counts());
}

#[test]
#[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
fn manifest_cache_bench() {
    let f = std::env::var("WEFTOS_STORE_TEST_RUSTFS_ENV").expect("WEFTOS_STORE_TEST_RUSTFS_ENV");
    let d = tempfile::tempdir().unwrap();
    let cfg = RustfsConfig::from_env_file(std::path::Path::new(&f), d.path().join("secrets")).unwrap();
    let be: Be = CountingBackend::new(RustfsBackend::new(cfg).unwrap());
    let u = ulid::Ulid::new().to_string().to_lowercase();
    let u = &u[10..];
    let pb = format!("t-p-{u}");
    AdminBackend::create_bucket(&be.inner, &pb, &[]).unwrap();
    let slugs: Vec<String> = (0..WORKTREES).map(|i| format!("wt{i}")).collect();
    let wbs: Vec<String> = (0..WORKTREES).map(|i| format!("t-w{i}-{u}")).collect();
    for (s, wb) in slugs.iter().zip(&wbs) {
        AdminBackend::create_bucket(&be.inner, wb, &[]).unwrap();
        publish(&be, wb, &pb, s);
    }
    println!("BENCH|setup|{WORKTREES} worktrees x {FILES} files, {OPS} ls + {OPS} get, {POLLS} polls, {STEP_MS} ms per op");
    let manifest_bytes = be.inner.get(&wbs[0], MANIFEST_KEY).unwrap().unwrap().len();
    println!("BENCH|setup|manifest_bytes={manifest_bytes}");

    // dashboard browsing: round-robin over the worktrees, folders and files
    let dir = |i: usize| format!("d{}", i % 10);
    let file = |i: usize| format!("d{}/f{}.md", (i * 7 % FILES) % 10, i * 7 % FILES);
    measure(&be, "browse", "uncached", || {
        for i in 0..OPS {
            browse_ls(&be, &pb, &slugs[i % WORKTREES], &dir(i)).unwrap();
            browse_get(&be, &pb, &slugs[i % WORKTREES], &file(i), 1 << 20).unwrap();
        }
    });
    let mut cache = ManifestCache::default();
    measure(&be, "browse", "cached", || {
        for i in 0..OPS {
            let t = (2 * i as u64) * STEP_MS;
            browse_ls_cached(&be, &mut cache, t, &pb, &slugs[i % WORKTREES], &dir(i)).unwrap();
            browse_get_cached(&be, &mut cache, t + STEP_MS, &pb, &slugs[i % WORKTREES], &file(i), 1 << 20).unwrap();
        }
    });
    println!("BENCH|browse|cache_stats|{:?}", cache.stats);

    // the project list
    measure(&be, "registry", "uncached", || {
        for _ in 0..POLLS {
            assert_eq!(browse_registry(&be, &pb).unwrap().worktrees.len(), WORKTREES);
        }
    });
    let mut cache = ManifestCache::default();
    measure(&be, "registry", "cached", || {
        for _ in 0..POLLS {
            assert_eq!(browse_registry_cached(&be, &mut cache, &pb).unwrap().worktrees.len(), WORKTREES);
        }
    });

    // follower polls (poll_fast over the digest). The first poll reports every worktree as new.
    let mut st = FollowState::default();
    measure(&be, "poll", "uncached", || {
        for _ in 0..POLLS {
            poll_fast(&be, &pb, &mut st, false, true, 4).unwrap();
        }
    });
    let (mut st, mut cache) = (FollowState::default(), ManifestCache::default());
    measure(&be, "poll", "cached", || {
        for _ in 0..POLLS {
            poll_fast_cached(&be, &pb, &mut st, false, true, 4, Some(&mut cache)).unwrap();
        }
    });

    for b in wbs.iter().chain(std::iter::once(&pb)) {
        for m in ObjectBackend::list(&be.inner, b, "").unwrap() {
            ObjectBackend::delete(&be.inner, b, &m.key).unwrap();
        }
    }
}
