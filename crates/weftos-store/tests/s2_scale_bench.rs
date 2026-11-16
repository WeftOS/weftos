//! WO-069 lane H drill 4 (ADR-126 section 4a gate item 3): latency at client-scale tree sizes, SYNTHETIC trees against a
//! THROWAWAY RustFS (`WEFTOS_STORE_TEST_RUSTFS_ENV`, never PG). Prints `BENCH|<files>|<step>|...` lines.
//! Run via `scripts/store/wo069-drills.sh --scale` (adds the 50k tree). The tree sizes come from
//! `WEFTOS_SCALE_SIZES` (comma list, default `1000,10000`). The runner owns the server and deletes it, so the
//! buckets this test makes are not emptied object by object.
//!
//! Size mix: 90% of files 1-20 kB, 9% 20-200 kB, 1% 200 kB - 2 MiB, plus three big files (4, 8 and 16 MiB: the
//! default per-file cap). Content is pseudo-random (incompressible), laid out in `src/mNN/sNN/` (code) and
//! `docs/` (3% markdown). The run uses `Limits::default()` (100 000 files, 4 GiB since the 2026-11-15 owner decision), so the
//! 50 000-file tree is expected to publish, read back and republish with no raised limits.
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
use weftos_store::testing::{CountingBackend, Counts};
use weftos_store::{RustfsBackend, RustfsConfig};

type Be = CountingBackend<RustfsBackend>;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn fill(&mut self, n: usize, tag: u64) -> Vec<u8> {
        let mut v = Vec::with_capacity(n + 8);
        let mut x = self.next() ^ tag;
        while v.len() < n {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            v.extend_from_slice(&x.to_le_bytes());
        }
        v.truncate(n);
        v
    }
}

/// Write the synthetic tree; returns (paths, total bytes, largest).
fn make_tree(root: &std::path::Path, n: usize) -> (Vec<String>, u64, u64) {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let (mut paths, mut total, mut largest) = (Vec::with_capacity(n), 0u64, 0u64);
    for i in 0..n {
        let big = match i {
            0 => Some(4usize << 20),
            1 => Some(8 << 20),
            2 => Some(16 << 20),
            _ => None,
        };
        let doc = rng.below(100) < 3;
        let size = big.unwrap_or_else(|| match rng.below(100) {
            0..=89 => 1_000 + rng.below(19_000) as usize,
            90..=98 => 20_000 + rng.below(180_000) as usize,
            _ => 200_000 + rng.below(1_800_000) as usize,
        });
        let p = if big.is_some() {
            format!("assets/big{i}.bin")
        } else if doc {
            format!("docs/area{:02}/note{i}.md", rng.below(20))
        } else {
            format!("src/m{:02}/s{:02}/f{i}.rs", rng.below(40), rng.below(25))
        };
        let mut data = rng.fill(size, i as u64);
        if doc && big.is_none() {
            // a readable first line so the title index has something to read; the rest stays random bytes
            let head = format!("# Note {i}\n");
            data[..head.len().min(size)].copy_from_slice(&head.as_bytes()[..head.len().min(size)]);
        }
        let f = root.join(&p);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(&f, &data).unwrap();
        total += size as u64;
        largest = largest.max(size as u64);
        paths.push(p);
    }
    (paths, total, largest)
}

fn report(n: usize, step: &str, wall: std::time::Duration, c: Counts, extra: &str) {
    println!("BENCH|{n}|{step}|wall_ms={}|get={}|head={}|list={}|put={}|put_cas={}|delete={}|copy={}|bytes_read={}|{extra}", wall.as_millis(), c.get, c.head, c.list, c.put, c.put_cas, c.delete, c.copy, c.bytes_read);
}

fn measure<T>(be: &Be, n: usize, step: &str, extra: impl FnOnce(&T) -> String, f: impl FnOnce() -> T) -> T {
    be.reset();
    let t = Instant::now();
    let v = f();
    report(n, step, t.elapsed(), be.counts(), &extra(&v));
    v
}

fn pct(v: &mut [u128], p: f64) -> u128 {
    v.sort_unstable();
    v[(((v.len() - 1) as f64) * p).round() as usize]
}

#[test]
#[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
fn scale_bench() {
    let f = std::env::var("WEFTOS_STORE_TEST_RUSTFS_ENV").expect("WEFTOS_STORE_TEST_RUSTFS_ENV");
    let sizes: Vec<usize> = std::env::var("WEFTOS_SCALE_SIZES").unwrap_or_else(|_| "1000,10000".into()).split(',').map(|s| s.trim().parse().unwrap()).collect();
    let d = tempfile::tempdir().unwrap();
    let cfg = RustfsConfig::from_env_file(std::path::Path::new(&f), d.path().join("secrets")).unwrap();
    let be: Be = CountingBackend::new(RustfsBackend::new(cfg).unwrap());
    println!("BENCH|setup|sizes={sizes:?}|profile={}|limits=default (100000 files / 4 GiB)", if cfg!(debug_assertions) { "debug" } else { "release" });
    let writer = Actor { agent: "agent-a".into(), host: "machine-a".into() };
    for &n in &sizes {
        let u = ulid::Ulid::new().to_string().to_lowercase();
        let u = &u[10..];
        let (pb, wb, slug) = (format!("t-p-{u}"), format!("t-w-{u}"), format!("scale{n}"));
        for b in [&pb, &wb] {
            // a just-started or loaded loopback server drops the first connection now and then: retry the setup call
            for attempt in 1..=5 {
                match AdminBackend::create_bucket(&be.inner, b, &[]) {
                    Ok(_) => break,
                    Err(e) if attempt < 5 => {
                        println!("BENCH|{n}|setup_retry|create_bucket {b} attempt {attempt}: {e}");
                        std::thread::sleep(std::time::Duration::from_secs(2));
                    }
                    Err(e) => panic!("create_bucket {b}: {e}"),
                }
            }
        }
        let tmp = tempfile::tempdir_in(std::env::var("WEFTOS_SCALE_TMP").unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().into())).unwrap();
        let t = Instant::now();
        let (mut paths, total, largest) = make_tree(tmp.path(), n);
        println!("BENCH|{n}|generate|wall_ms={}|files={n}|bytes={total}|largest={largest}", t.elapsed().as_millis());
        let lim = Limits::default();
        write_meta(&be, &wb, &WorktreeMeta { slug: slug.clone(), project: "01abc".into(), branch: slug.clone(), base_commit: "base".into(), created_ms: 1 }).unwrap();
        let publish = |paths: &[String], head: &str, now: u64| {
            let src = TreeSource { root: tmp.path(), paths, uncommitted: &[], changes: &[], changes_recorded: false, branch: &slug, base_commit: "base", head_commit: head, bundle: None, writer: writer.clone() };
            publish_tree(&be, &wb, &src, &lim, now).unwrap()
        };

        // 1-3: publish
        let done = std::sync::atomic::AtomicBool::new(false);
        let rep = std::thread::scope(|sc| {
            // heartbeat: a stalled server shows up as a flat put count
            sc.spawn(|| {
                let t = Instant::now();
                while !done.load(std::sync::atomic::Ordering::Relaxed) {
                    std::thread::sleep(std::time::Duration::from_secs(10));
                    println!("BENCH|{n}|progress|t_s={}|put={}", t.elapsed().as_secs(), be.counts().put);
                }
            });
            let r = measure(&be, n, "publish_first", |r: &PublishReport| format!("uploaded={} skipped={} bytes={total} ({:.1} MiB)", r.uploaded, r.skipped.len(), total as f64 / 1048576.0), || publish(&paths, "head-1", 1_000));
            done.store(true, std::sync::atomic::Ordering::Relaxed);
            r
        });
        assert_eq!(rep.uploaded, n, "every file uploaded (skips: {:?})", &rep.skipped[..rep.skipped.len().min(3)]);
        let man = be.inner.get(&wb, MANIFEST_KEY).unwrap().unwrap();
        println!("BENCH|{n}|manifest|bytes={}|per_file={}", man.len(), man.len() / n);
        // Every reader (and the next publish, which reads the previous manifest) takes the same default file cap as the writer.
        let read = read_manifest(&be, &wb).unwrap().expect("manifest");
        assert_eq!(read.files.len(), n, "the tree just published is readable with default limits");
        let again = publish(&paths, "head-1", 1_500);
        assert_eq!((again.uploaded, again.unchanged), (0, n), "and republishable");
        write_registry_entry(&be, &pb, &summarize(&be, &slug, &wb).unwrap().unwrap()).unwrap();
        // three repetitions each: the first numbers after a long upload are disturbed by the server's I/O backlog
        for rep in 1..=3u64 {
            measure(&be, n, &format!("publish_nochange#{rep}"), |r: &PublishReport| format!("uploaded={} unchanged={} hashed={} hashed_bytes={}", r.uploaded, r.unchanged, r.hash_cache.hashed, r.hash_cache.hashed_bytes), || publish(&paths, "head-1", 2_000 + rep));
        }
        for rep in 1..=3u64 {
            let edit = tmp.path().join(&paths[n / 2]);
            let mut data = std::fs::read(&edit).unwrap();
            data.extend_from_slice(b"one more line\n");
            std::fs::write(&edit, &data).unwrap();
            measure(&be, n, &format!("publish_one_file#{rep}"), |r: &PublishReport| format!("uploaded={} unchanged={}", r.uploaded, r.unchanged), || publish(&paths, "head-1", 3_000 + rep));
        }
        // 3b (WO-069 E1): the same no-change republish with the local publish hash cache. Real clock, since the
        // cache's racy-timestamp rule compares file mtimes with the publish time; one cold pass fills the cache.
        let cache_cfg = HashCacheConfig { path: tmp.path().parent().unwrap().join(format!("hashcache-{slug}.json")), full_rehash_every_ms: DEFAULT_FULL_REHASH_MS };
        let wall = || std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
        let publish_cached = |paths: &[String], head: &str| {
            let src = TreeSource { root: tmp.path(), paths, uncommitted: &[], changes: &[], changes_recorded: false, branch: &slug, base_commit: "base", head_commit: head, bundle: None, writer: writer.clone() };
            publish_tree_cached(&be, &wb, &src, &lim, wall(), None, &PublishOptions::default(), Some(&cache_cfg)).unwrap().report
        };
        let hc = |r: &PublishReport| format!("uploaded={} unchanged={} hashed={} hashed_bytes={} from_cache={} sampled={} full_pass={:?}", r.uploaded, r.unchanged, r.hash_cache.hashed, r.hash_cache.hashed_bytes, r.hash_cache.from_cache, r.hash_cache.sampled, r.hash_cache.full_pass_reason);
        measure(&be, n, "publish_nochange_cache_cold", hc, || publish_cached(&paths, "head-1"));
        std::thread::sleep(std::time::Duration::from_millis(2_500)); // past the racy margin for the files just hashed
        measure(&be, n, "publish_nochange_cache_warm_fill", hc, || publish_cached(&paths, "head-1"));
        for rep in 1..=3u64 {
            measure(&be, n, &format!("publish_nochange_cached#{rep}"), hc, || publish_cached(&paths, "head-1"));
        }
        write_registry_entry(&be, &pb, &summarize(&be, &slug, &wb).unwrap().unwrap()).unwrap();

        // 4: list of the tree prefix, head latency
        // lane D3: the tree objects are the content-named pool (`.wt/obj/`), one per distinct content
        measure(&be, n, "list_tree_prefix", |v: &usize| format!("objects={v} pages_of_1000={}", v.div_ceil(1000)), || ObjectBackend::list(&be, &wb, OBJ_PREFIX).unwrap().len());
        let man = read_manifest(&be, &wb).unwrap().unwrap();
        let obj_of: std::collections::HashMap<&str, String> = man.files.iter().map(|f| (f.path.as_str(), man.object_key(f))).collect();
        let mut rng = Rng(42);
        let mut lat = Vec::new();
        be.reset();
        for _ in 0..300 {
            let p = &paths[rng.below(n as u64) as usize];
            let t = Instant::now();
            assert!(be.head(&wb, &obj_of[p.as_str()]).unwrap().is_some());
            lat.push(t.elapsed().as_micros());
        }
        println!("BENCH|{n}|head_latency|samples=300|p50_us={}|p95_us={}|max_us={}", pct(&mut lat.clone(), 0.5), pct(&mut lat.clone(), 0.95), pct(&mut lat, 1.0));

        // 5: follower poll cost
        let mut st = FollowState::default();
        measure(&be, n, "poll_first(new)", |c: &Vec<Change>| format!("changes={}", c.len()), || poll_fast(&be, &pb, &mut st, false, true, 4).unwrap());
        measure(&be, n, "poll_digest_hit", |c: &Vec<Change>| format!("changes={}", c.len()), || poll_fast(&be, &pb, &mut st, false, true, 4).unwrap());
        measure(&be, n, "poll_full(--full)", |c: &Vec<Change>| format!("changes={}", c.len()), || poll_fast(&be, &pb, &mut st, false, false, 4).unwrap());
        measure(&be, n, "poll_slow(list_shared)", |c: &Vec<Change>| format!("changes={}", c.len()), || poll(&be, &pb, &mut st).unwrap());
        measure(&be, n, "registry_list", |r: &weftos_store::follower_browse::RegistryBrowse| format!("worktrees={}", r.worktrees.len()), || browse_registry(&be, &pb).unwrap());

        // 6: browse, uncached then cached
        let dir = paths[3].rsplit_once('/').map(|x| x.0.to_string()).unwrap();
        let small = paths.iter().find(|p| p.starts_with("src/")).unwrap().clone();
        measure(&be, n, "browse_ls_root(uncached)", |l: &weftos_store::follower_browse::BrowseListing| format!("entries={}", l.entries.len()), || browse_ls(&be, &pb, &slug, "").unwrap());
        measure(&be, n, "browse_ls_dir(uncached)", |l: &weftos_store::follower_browse::BrowseListing| format!("entries={}", l.entries.len()), || browse_ls(&be, &pb, &slug, &dir).unwrap());
        measure(&be, n, "browse_get_small(uncached)", |r: &weftos_store::follower_browse::BrowseFile| format!("size={}", r.size), || browse_get(&be, &pb, &slug, &small, 1 << 20).unwrap());
        let mut cache = ManifestCache::default();
        let mut t_ms = 0u64;
        measure(&be, n, "browse_ls_root(cached,cold)", |l: &weftos_store::follower_browse::BrowseListing| format!("entries={}", l.entries.len()), || browse_ls_cached(&be, &mut cache, t_ms, &pb, &slug, "").unwrap());
        t_ms += 100;
        measure(&be, n, "browse_ls_dir(cached,fresh)", |l: &weftos_store::follower_browse::BrowseListing| format!("entries={}", l.entries.len()), || browse_ls_cached(&be, &mut cache, t_ms, &pb, &slug, &dir).unwrap());
        measure(&be, n, "browse_get_small(cached,fresh)", |r: &weftos_store::follower_browse::BrowseFile| format!("size={}", r.size), || browse_get_cached(&be, &mut cache, t_ms, &pb, &slug, &small, 1 << 20).unwrap());
        t_ms += 6_000; // past the 5 s TTL: one head to revalidate
        measure(&be, n, "browse_ls_dir(cached,revalidate)", |l: &weftos_store::follower_browse::BrowseListing| format!("entries={}", l.entries.len()), || browse_ls_cached(&be, &mut cache, t_ms, &pb, &slug, &dir).unwrap());
        println!("BENCH|{n}|cache_stats|{:?}", cache.stats);

        // 7: docs mirror and the whole tree
        let e = summarize(&be, &slug, &wb).unwrap().unwrap();
        let dd = tempfile::tempdir_in(tmp.path().parent().unwrap()).unwrap();
        measure(&be, n, "sync_docs", |r: &FetchReport| format!("written={}", r.written.len()), || sync_docs(&be, &e, dd.path()).unwrap());
        let dest = tempfile::tempdir_in(tmp.path().parent().unwrap()).unwrap();
        let fr = measure(&be, n, "fetch_tree(all)", |r: &FetchReport| format!("written={} missing={} refused={} conflicts={}", r.written.len(), r.missing.len(), r.refused.len(), r.conflicts.len()), || fetch_tree_with(&be, &wb, dest.path(), Select::All, false, &lim).unwrap());
        assert_eq!(fr.written.len(), n, "the whole tree arrived");
        measure(&be, n, "fetch_tree(again,unchanged)", |r: &FetchReport| format!("written={} unchanged={}", r.written.len(), r.unchanged.len()), || fetch_tree_with(&be, &wb, dest.path(), Select::All, false, &lim).unwrap());
        paths.clear();
    }
}
