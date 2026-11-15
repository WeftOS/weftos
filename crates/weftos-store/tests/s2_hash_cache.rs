//! WO-069 lane E1: the local publish hash cache (`share_hashcache.rs`). An unchanged file is not
//! re-read; every check `publish` makes on a file's content still holds; the manifest is the same
//! bytes with the cache off, cold or warm.
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use weftos_store::overlay::ObjectBackend;
use weftos_store::share::*;
use weftos_store::testing::MemBackend;

const WT: &str = "w-01abc-aaaaaaaaaaaa";
const HOUR: Duration = Duration::from_secs(3600);

fn t0() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64
}

fn set_mtime(p: &Path, t: SystemTime) {
    std::fs::OpenOptions::new().write(true).open(p).unwrap().set_modified(t).unwrap();
}

fn old() -> SystemTime {
    SystemTime::now() - HOUR
}

/// `n` files `src/f<i>.rs` with distinct contents and an old mtime, plus the repo-style path list.
fn tree(n: usize) -> (tempfile::TempDir, Vec<String>) {
    let d = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(d.path().join("src")).unwrap();
    let mut ps = Vec::new();
    for i in 0..n {
        let p = format!("src/f{i:04}.rs");
        std::fs::write(d.path().join(&p), format!("fn f{i:04}() {{}}\n")).unwrap();
        set_mtime(&d.path().join(&p), old());
        ps.push(p);
    }
    (d, ps)
}

struct Env {
    mem: MemBackend,
    cache: tempfile::TempDir,
}
impl Env {
    fn new() -> Self {
        Self { mem: MemBackend::new(), cache: tempfile::tempdir().unwrap() }
    }
    fn cfg(&self, every_ms: u64) -> HashCacheConfig {
        HashCacheConfig { path: self.cache.path().join("sub").join("wt.json"), full_rehash_every_ms: every_ms }
    }
}

fn src<'a>(root: &'a Path, ps: &'a [String]) -> TreeSource<'a> {
    TreeSource {
        root,
        paths: ps,
        uncommitted: &[],
        changes: &[],
        changes_recorded: false,
        branch: "f",
        base_commit: "a",
        head_commit: "b",
        bundle: None,
        writer: Actor { agent: "w".into(), host: "h".into() },
    }
}

fn publish(mem: &MemBackend, root: &Path, ps: &[String], now: u64, cfg: Option<&HashCacheConfig>) -> PublishReport {
    publish_tree_cached(mem, WT, &src(root, ps), &Limits::default(), now, None, &PublishOptions::default(), cfg).unwrap().report
}

fn manifest(mem: &MemBackend) -> TreeManifest {
    read_manifest(mem, WT).unwrap().unwrap()
}

fn sha_of(mem: &MemBackend, path: &str) -> String {
    manifest(mem).files.iter().find(|f| f.path == path).unwrap().sha256.clone()
}

fn cache_json(cfg: &HashCacheConfig) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(&cfg.path).unwrap()).unwrap()
}

#[test]
fn an_unchanged_tree_reads_only_the_integrity_sample() {
    let (d, ps) = tree(200);
    let env = Env::new();
    let cfg = env.cfg(DEFAULT_FULL_REHASH_MS);
    let now = t0();
    let cold = publish(&env.mem, d.path(), &ps, now, Some(&cfg));
    assert_eq!((cold.uploaded, cold.hash_cache.hashed, cold.hash_cache.from_cache), (200, 200, 0));
    assert!(cold.hash_cache.full_pass && cold.hash_cache.full_pass_reason.as_deref() == Some("missing"));
    let warm = publish(&env.mem, d.path(), &ps, now + 10_000, Some(&cfg));
    let h = &warm.hash_cache;
    assert_eq!((warm.uploaded, warm.unchanged), (0, 200));
    assert_eq!(h.from_cache, 200, "every file's hash came from the cache");
    assert_eq!((h.hashed, h.sampled), (16, 16), "the only file reads are the 16-file sample: {h:?}");
    assert!(!h.full_pass && !h.mismatch && h.write_error.is_none());
}

#[test]
fn the_sample_rotates_across_publishes() {
    let (d, ps) = tree(40);
    let env = Env::new();
    let cfg = env.cfg(DEFAULT_FULL_REHASH_MS);
    let now = t0();
    publish(&env.mem, d.path(), &ps, now, Some(&cfg));
    // 40 entries, 16 a publish: the third publish has covered every entry
    for i in 1..=3 {
        let r = publish(&env.mem, d.path(), &ps, now + 10_000 * i, Some(&cfg));
        assert_eq!(r.hash_cache.sampled, 16);
    }
    assert_eq!(cache_json(&cfg)["cursor"].as_u64().unwrap(), (16 * 3) % 40);
}

#[test]
fn an_edit_that_changes_the_size_is_published() {
    let (d, ps) = tree(50);
    let env = Env::new();
    let cfg = env.cfg(DEFAULT_FULL_REHASH_MS);
    let now = t0();
    publish(&env.mem, d.path(), &ps, now, Some(&cfg));
    std::fs::write(d.path().join("src/f0007.rs"), "fn changed_and_longer() {}\n").unwrap();
    let r = publish(&env.mem, d.path(), &ps, now + 10_000, Some(&cfg));
    assert_eq!(r.uploaded, 1);
    assert_eq!(sha_of(&env.mem, "src/f0007.rs"), sha256_hex(b"fn changed_and_longer() {}\n"));
    assert_eq!(r.hash_cache.from_cache, 49);
}

#[test]
fn an_edit_with_the_same_size_and_a_newer_mtime_is_published() {
    let (d, ps) = tree(50);
    let env = Env::new();
    let cfg = env.cfg(DEFAULT_FULL_REHASH_MS);
    let now = t0();
    publish(&env.mem, d.path(), &ps, now, Some(&cfg));
    let p = d.path().join("src/f0003.rs");
    let before = std::fs::read(&p).unwrap();
    let mut after = before.clone();
    after[4] = b'X';
    std::fs::write(&p, &after).unwrap();
    set_mtime(&p, SystemTime::now() - Duration::from_secs(60));
    let r = publish(&env.mem, d.path(), &ps, now + 10_000, Some(&cfg));
    assert_eq!(r.uploaded, 1);
    assert_eq!(sha_of(&env.mem, "src/f0003.rs"), sha256_hex(&after));
}

/// Same size and mtime put back: only the inode change time betrays it. Caught on unix (ctime is
/// validated); elsewhere it stays undetected until the next full pass or until the rotating sample
/// reaches the file (checked at the bottom of this test, on every platform, with the sample).
#[test]
fn an_edit_with_the_size_and_mtime_restored_is_caught_by_ctime_or_the_sample() {
    let (d, ps) = tree(10);
    let env = Env::new();
    let cfg = env.cfg(DEFAULT_FULL_REHASH_MS);
    let now = t0();
    publish(&env.mem, d.path(), &ps, now, Some(&cfg));
    let p = d.path().join("src/f0002.rs");
    let mut after = std::fs::read(&p).unwrap();
    let want = std::fs::metadata(&p).unwrap().modified().unwrap();
    std::thread::sleep(Duration::from_millis(30));
    after[4] = b'Q';
    std::fs::write(&p, &after).unwrap();
    set_mtime(&p, want);
    // 10 entries are all inside the 16-file sample, so the publish sees it by either route
    let r = publish(&env.mem, d.path(), &ps, now + 10_000, Some(&cfg));
    assert_eq!(sha_of(&env.mem, "src/f0002.rs"), sha256_hex(&after), "{:?}", r.hash_cache);
    if cfg!(unix) {
        assert!(!r.hash_cache.mismatch, "on unix the ctime validator catches it before the sample has to: {:?}", r.hash_cache);
        assert!(!r.hash_cache.full_pass);
    }
}

#[test]
fn a_file_changed_within_the_racy_margin_is_not_trusted_until_it_is_older() {
    let (d, ps) = tree(200);
    let env = Env::new();
    let cfg = env.cfg(DEFAULT_FULL_REHASH_MS);
    let now = t0();
    // f0199's mtime is half a second before the publish clock: inside the 2 s margin
    set_mtime(&d.path().join("src/f0199.rs"), SystemTime::UNIX_EPOCH + Duration::from_millis(now - 500));
    publish(&env.mem, d.path(), &ps, now, Some(&cfg));
    let again = publish(&env.mem, d.path(), &ps, now + 10_000, Some(&cfg));
    assert_eq!(again.hash_cache.from_cache, 199, "the racy file was hashed again: {:?}", again.hash_cache);
    // that publish hashed it at now+10 s, its mtime is far older than that: trusted from here on
    let third = publish(&env.mem, d.path(), &ps, now + 20_000, Some(&cfg));
    assert_eq!(third.hash_cache.from_cache, 200);
    // and an entry hashed exactly RACY_MARGIN_MS after the mtime is still not trusted (strictly older)
    let boundary = tempfile::tempdir().unwrap();
    std::fs::write(boundary.path().join("a.txt"), "a").unwrap();
    set_mtime(&boundary.path().join("a.txt"), SystemTime::UNIX_EPOCH + Duration::from_millis(now));
    let e2 = Env::new();
    let c2 = e2.cfg(DEFAULT_FULL_REHASH_MS);
    let p2 = vec!["a.txt".to_string()];
    publish(&e2.mem, boundary.path(), &p2, now + RACY_MARGIN_MS, Some(&c2));
    assert_eq!(publish(&e2.mem, boundary.path(), &p2, now + 99_000, Some(&c2)).hash_cache.from_cache, 0, "mtime == hashed - margin is not strictly older");
    // hashed 1 ms later it is
    let e3 = Env::new();
    let c3 = e3.cfg(DEFAULT_FULL_REHASH_MS);
    publish(&e3.mem, boundary.path(), &p2, now + RACY_MARGIN_MS + 1, Some(&c3));
    assert_eq!(publish(&e3.mem, boundary.path(), &p2, now + 99_000, Some(&c3)).hash_cache.from_cache, 1);
}

#[test]
fn deleted_added_and_renamed_files() {
    let (d, mut ps) = tree(60);
    let env = Env::new();
    let cfg = env.cfg(DEFAULT_FULL_REHASH_MS);
    let now = t0();
    publish(&env.mem, d.path(), &ps, now, Some(&cfg));
    // delete one, add one, rename one
    std::fs::remove_file(d.path().join("src/f0001.rs")).unwrap();
    ps.retain(|p| p != "src/f0001.rs");
    std::fs::write(d.path().join("src/new.rs"), "fn new() {}\n").unwrap();
    set_mtime(&d.path().join("src/new.rs"), old());
    ps.push("src/new.rs".into());
    std::fs::rename(d.path().join("src/f0002.rs"), d.path().join("src/renamed.rs")).unwrap();
    ps.retain(|p| p != "src/f0002.rs");
    ps.push("src/renamed.rs".into());
    let r = publish(&env.mem, d.path(), &ps, now + 10_000, Some(&cfg));
    // 58 files kept their path; the added file and the renamed one (same bytes as before the rename,
    // new path, no entry) are hashed
    assert_eq!(r.hash_cache.from_cache, 58, "{:?}", r.hash_cache);
    let m = manifest(&env.mem);
    assert!(m.files.iter().all(|f| f.path != "src/f0001.rs" && f.path != "src/f0002.rs"));
    assert!(m.files.iter().any(|f| f.path == "src/renamed.rs" && f.sha256 == sha256_hex(b"fn f0002() {}\n")));
    let keys: Vec<String> = cache_json(&cfg)["entries"].as_object().unwrap().keys().cloned().collect();
    assert!(!keys.contains(&"src/f0001.rs".to_string()) && !keys.contains(&"src/f0002.rs".to_string()), "gone paths are pruned");
    assert!(keys.contains(&"src/renamed.rs".to_string()) && keys.contains(&"src/new.rs".to_string()));
}

#[test]
fn a_cache_for_another_root_is_a_full_pass() {
    let (a, pa) = tree(30);
    let (b, pb) = tree(30);
    let env = Env::new();
    let cfg = env.cfg(DEFAULT_FULL_REHASH_MS);
    let now = t0();
    publish(&env.mem, a.path(), &pa, now, Some(&cfg));
    let r = publish(&env.mem, b.path(), &pb, now + 10_000, Some(&cfg));
    assert_eq!(r.hash_cache.full_pass_reason.as_deref(), Some("other-root"));
    assert_eq!((r.hash_cache.hashed, r.hash_cache.from_cache), (30, 0));
}

#[test]
fn a_corrupt_unreadable_or_newer_format_cache_is_ignored_and_replaced() {
    let (d, ps) = tree(20);
    let env = Env::new();
    let cfg = env.cfg(DEFAULT_FULL_REHASH_MS);
    let now = t0();
    publish(&env.mem, d.path(), &ps, now, Some(&cfg));
    std::fs::write(&cfg.path, b"{ this is not json").unwrap();
    let r = publish(&env.mem, d.path(), &ps, now + 10_000, Some(&cfg));
    assert_eq!((r.hash_cache.full_pass_reason.as_deref(), r.hash_cache.hashed), (Some("corrupt"), 20));
    assert_eq!(cache_json(&cfg)["version"], CACHE_FORMAT, "replaced by a valid file");
    // another format version
    let mut j = cache_json(&cfg);
    j["version"] = serde_json::json!(CACHE_FORMAT + 1);
    std::fs::write(&cfg.path, serde_json::to_vec(&j).unwrap()).unwrap();
    let r = publish(&env.mem, d.path(), &ps, now + 20_000, Some(&cfg));
    assert_eq!(r.hash_cache.full_pass_reason.as_deref(), Some("format-version"));
    // a directory where the file should be: unreadable, and unwritable, and still a good publish
    std::fs::remove_file(&cfg.path).unwrap();
    std::fs::create_dir(&cfg.path).unwrap();
    let r = publish(&env.mem, d.path(), &ps, now + 30_000, Some(&cfg));
    assert_eq!(r.hash_cache.full_pass_reason.as_deref(), Some("unreadable"));
    assert!(r.hash_cache.write_error.is_some());
    assert_eq!(manifest(&env.mem).files.len(), 20);
}

#[test]
fn a_cache_that_cannot_be_written_never_fails_the_publish() {
    let (d, ps) = tree(5);
    let mem = MemBackend::new();
    let blocker = tempfile::NamedTempFile::new().unwrap();
    // the cache's parent "directory" is a regular file
    let cfg = HashCacheConfig { path: blocker.path().join("x").join("wt.json"), full_rehash_every_ms: DEFAULT_FULL_REHASH_MS };
    let r = publish(&mem, d.path(), &ps, t0(), Some(&cfg));
    assert_eq!(r.uploaded, 5);
    assert!(r.hash_cache.write_error.is_some());
}

#[test]
fn the_full_rehash_interval_boundary_uses_the_injected_clock() {
    let (d, ps) = tree(30);
    let env = Env::new();
    let every = 60_000;
    let cfg = env.cfg(every);
    let now = t0();
    assert!(publish(&env.mem, d.path(), &ps, now, Some(&cfg)).hash_cache.full_pass);
    let just_before = publish(&env.mem, d.path(), &ps, now + every - 1, Some(&cfg));
    assert!(!just_before.hash_cache.full_pass, "{:?}", just_before.hash_cache);
    let at = publish(&env.mem, d.path(), &ps, now + every, Some(&cfg));
    assert_eq!((at.hash_cache.full_pass_reason.as_deref(), at.hash_cache.hashed), (Some("interval"), 30));
    // the clock restarts from that pass
    assert!(!publish(&env.mem, d.path(), &ps, now + every + 1, Some(&cfg)).hash_cache.full_pass);
    // a clock that went backwards is not trusted
    assert_eq!(publish(&env.mem, d.path(), &ps, now, Some(&cfg)).hash_cache.full_pass_reason.as_deref(), Some("interval"));
    // 0 hours: every publish
    let zero = env.cfg(0);
    publish(&env.mem, d.path(), &ps, now + 1, Some(&zero));
    assert_eq!(publish(&env.mem, d.path(), &ps, now + 2, Some(&zero)).hash_cache.hashed, 30);
}

#[test]
fn a_sample_that_disagrees_discards_the_cache_and_does_a_full_pass_in_the_same_publish() {
    let (d, ps) = tree(10);
    let env = Env::new();
    let cfg = env.cfg(DEFAULT_FULL_REHASH_MS);
    let now = t0();
    publish(&env.mem, d.path(), &ps, now, Some(&cfg));
    // the cache claims other bytes for files whose validators still match: bit rot or tampering
    let mut j = cache_json(&cfg);
    for e in j["entries"].as_object_mut().unwrap().values_mut() {
        e["sha256"] = serde_json::json!("0".repeat(64));
    }
    std::fs::write(&cfg.path, serde_json::to_vec(&j).unwrap()).unwrap();
    let r = publish(&env.mem, d.path(), &ps, now + 10_000, Some(&cfg));
    let h = &r.hash_cache;
    assert!(h.mismatch && h.full_pass && h.full_pass_reason.as_deref() == Some("sample-mismatch"), "{h:?}");
    assert_eq!(h.from_cache, 0);
    assert_eq!(sha_of(&env.mem, "src/f0004.rs"), sha256_hex(b"fn f0004() {}\n"), "the manifest carries the real hashes");
    // and the rewritten cache is sound again
    let back = publish(&env.mem, d.path(), &ps, now + 20_000, Some(&cfg));
    assert!(!back.hash_cache.full_pass && back.hash_cache.from_cache == 10);
}

#[test]
fn a_cached_hash_that_is_wrong_for_an_object_the_store_lacks_is_caught_at_upload() {
    let (d, ps) = tree(300);
    let env = Env::new();
    let cfg = env.cfg(DEFAULT_FULL_REHASH_MS);
    let now = t0();
    publish(&env.mem, d.path(), &ps, now, Some(&cfg));
    // one entry outside the first sample (the last in path order) gets a hash the pool has no object for
    let mut j = cache_json(&cfg);
    j["entries"]["src/f0299.rs"]["sha256"] = serde_json::json!("f".repeat(64));
    std::fs::write(&cfg.path, serde_json::to_vec(&j).unwrap()).unwrap();
    let r = publish(&env.mem, d.path(), &ps, now + 10_000, Some(&cfg));
    let h = &r.hash_cache;
    assert!(h.mismatch && h.full_pass_reason.as_deref() == Some("stale-entry"), "{h:?}");
    assert_eq!(sha_of(&env.mem, "src/f0299.rs"), sha256_hex(b"fn f0299() {}\n"));
    assert!(r.skipped.is_empty());
}

#[test]
fn a_cached_file_the_store_lacks_is_read_and_uploaded() {
    let (d, ps) = tree(40);
    let env = Env::new();
    let cfg = env.cfg(DEFAULT_FULL_REHASH_MS);
    let now = t0();
    publish(&env.mem, d.path(), &ps, now, Some(&cfg));
    // a different, empty store: the cached hashes stand in for the reads, the uploads still need the bytes
    let fresh = MemBackend::new();
    let r = publish(&fresh, d.path(), &ps, now + 10_000, Some(&cfg));
    assert_eq!(r.uploaded, 40);
    assert_eq!(r.hash_cache.from_cache, 40);
    assert_eq!(r.hash_cache.hashed, 16 + 40, "the 16-file sample, then one read per upload");
}

#[test]
fn the_manifest_bytes_are_the_same_off_cold_and_warm() {
    let (d, mut ps) = tree(120);
    // every refusal the publish makes
    std::fs::write(d.path().join(".env"), "TOKEN=1\n").unwrap();
    std::fs::write(d.path().join("src/key.txt"), "-----BEGIN OPENSSH PRIVATE KEY-----\nabc\n").unwrap();
    std::fs::write(d.path().join("docs.md"), "# A title\nbody\n").unwrap();
    set_mtime(&d.path().join("docs.md"), old());
    std::fs::write(d.path().join("src/big.bin"), vec![7u8; 4096]).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("f0000.rs", d.path().join("src/link.rs")).unwrap();
    ps.extend([".env", "src/key.txt", "docs.md", "src/big.bin", "src/link.rs", "src/missing.rs", "src/../escape"].map(String::from));
    let env = Env::new();
    let cfg = env.cfg(DEFAULT_FULL_REHASH_MS);
    let now = t0();
    let run = |c: Option<&HashCacheConfig>, at: u64| {
        let mem = MemBackend::new();
        let r = publish(&mem, d.path(), &ps, at, c);
        (mem.get(WT, MANIFEST_KEY).unwrap().unwrap(), r.skipped)
    };
    let small = Limits { max_file: 1000, ..Limits::default() };
    let with_small = |c: Option<&HashCacheConfig>| {
        let mem = MemBackend::new();
        let r = publish_tree_cached(&mem, WT, &src(d.path(), &ps), &small, now + 20_000, None, &PublishOptions::default(), c).unwrap().report;
        (mem.get(WT, MANIFEST_KEY).unwrap().unwrap(), r.skipped)
    };
    let (off, off_skips) = run(None, now + 10_000);
    let (cold, cold_skips) = run(Some(&cfg), now + 10_000);
    let (warm, warm_skips) = run(Some(&cfg), now + 10_000);
    assert_eq!(off, cold, "cache off and cold: same manifest bytes");
    assert_eq!(off, warm, "cache warm: same manifest bytes");
    assert_eq!((&off_skips, &off_skips), (&cold_skips, &warm_skips), "same skips and refusals");
    assert!(off_skips.iter().any(|(p, s)| p == ".env" && *s == Skip::Secret));
    assert!(off_skips.iter().any(|(p, s)| p == "src/key.txt" && *s == Skip::Secret));
    assert!(String::from_utf8_lossy(&off).contains("A title"));
    // a size cap makes the same refusal with a warm cache
    let (a, a_skips) = with_small(None);
    let (b, b_skips) = with_small(Some(&cfg));
    assert_eq!((a, a_skips.clone()), (b, b_skips));
    assert!(a_skips.iter().any(|(p, s)| p == "src/big.bin" && *s == Skip::TooLarge));
}

#[test]
fn a_cached_file_that_becomes_secret_is_still_refused() {
    let (d, mut ps) = tree(30);
    let env = Env::new();
    let cfg = env.cfg(DEFAULT_FULL_REHASH_MS);
    let now = t0();
    publish(&env.mem, d.path(), &ps, now, Some(&cfg));
    // a private-key marker where the cache has a clean hash: only a re-read can see it
    let p = d.path().join("src/f0005.rs");
    std::fs::write(&p, b"-----BEGIN PRIVATE KEY-----\nxxxxxxxx\n").unwrap();
    let r = publish(&env.mem, d.path(), &ps, now + 10_000, Some(&cfg));
    assert!(r.skipped.contains(&("src/f0005.rs".into(), Skip::Secret)), "{:?}", r.skipped);
    assert!(manifest(&env.mem).files.iter().all(|f| f.path != "src/f0005.rs"));
    assert!(!cache_json(&cfg)["entries"].as_object().unwrap().contains_key("src/f0005.rs"), "a refused file is not cached");
    // a cached path that turns into a secret-denylist name is refused on its path, before any cache
    ps.push(".env".into());
    std::fs::write(d.path().join(".env"), "A=1\n").unwrap();
    let r = publish(&env.mem, d.path(), &ps, now + 20_000, Some(&cfg));
    assert!(r.skipped.contains(&(".env".into(), Skip::Secret)));
}

#[cfg(unix)]
#[test]
fn a_cached_file_replaced_by_a_symlink_is_refused() {
    let (d, ps) = tree(30);
    let env = Env::new();
    let cfg = env.cfg(DEFAULT_FULL_REHASH_MS);
    let now = t0();
    publish(&env.mem, d.path(), &ps, now, Some(&cfg));
    std::fs::remove_file(d.path().join("src/f0006.rs")).unwrap();
    std::os::unix::fs::symlink("f0007.rs", d.path().join("src/f0006.rs")).unwrap();
    let r = publish(&env.mem, d.path(), &ps, now + 10_000, Some(&cfg));
    assert!(r.skipped.contains(&("src/f0006.rs".into(), Skip::Symlink)));
}

#[test]
fn with_no_cache_the_report_says_so_and_counts_the_reads() {
    let (d, ps) = tree(25);
    let mem = MemBackend::new();
    let r = publish(&mem, d.path(), &ps, t0(), None);
    let h = &r.hash_cache;
    assert!(!h.enabled && !h.full_pass && h.from_cache == 0 && h.sampled == 0);
    assert_eq!(h.hashed, 25);
}

/// 10 000 synthetic files, no-change republish uncached vs cached (in-memory store). Run with
/// `--ignored --nocapture`; the read counts are the reliable numbers, the wall times depend on load.
#[test]
#[ignore = "timing run, prints numbers"]
fn ten_thousand_files_no_change_republish_with_and_without_the_cache() {
    let d = tempfile::tempdir().unwrap();
    let mut ps = Vec::new();
    for i in 0..10_000 {
        let dir = format!("d{:03}", i / 100);
        std::fs::create_dir_all(d.path().join(&dir)).unwrap();
        let p = format!("{dir}/f{i:05}.txt");
        std::fs::write(d.path().join(&p), format!("{i}\n").repeat(600)).unwrap();
        set_mtime(&d.path().join(&p), old());
        ps.push(p);
    }
    let env = Env::new();
    let cfg = env.cfg(DEFAULT_FULL_REHASH_MS);
    let now = t0();
    let time = |label: &str, c: Option<&HashCacheConfig>, at: u64| -> PublishReport {
        let t = std::time::Instant::now();
        let r = publish(&env.mem, d.path(), &ps, at, c);
        let h = &r.hash_cache;
        println!(
            "E1|{label}|wall_ms={}|files_read={}|bytes_read={}|from_cache={}|sampled={}|full_pass={:?}|uploaded={}",
            t.elapsed().as_millis(), h.hashed, h.hashed_bytes, h.from_cache, h.sampled, h.full_pass_reason, r.uploaded
        );
        r
    };
    time("first_publish_cache_off", None, now);
    time("nochange_cache_off", None, now + 1_000);
    time("nochange_cache_cold", Some(&cfg), now + 2_000);
    let warm = time("nochange_cache_warm", Some(&cfg), now + 12_000);
    assert_eq!(warm.hash_cache.hashed, 100, "1 % of 10 000 entries");
    let path: PathBuf = cfg.path.clone();
    println!("E1|cache_file_bytes={}", std::fs::metadata(path).unwrap().len());
}
