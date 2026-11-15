//! The publish hash cache (WO-069 lane E1; owner decision 2026-11-15): a local, per-machine file
//! that lets an unchanged worktree file skip the read-and-SHA-256 `publish` otherwise does for
//! every file on every publish. A child of `share`; see ADR-126 section 4.
//!
//! Key: the path relative to the worktree root. Stored per entry: size, mtime (ns), and on unix the
//! inode and ctime (ns) as extra validators; the SHA-256; the time it was hashed; and every other
//! fact `publish` derives from the file CONTENT (today only the markdown title; the private-key
//! scan is never cached because a file that fails it is not cached at all). An entry is served only
//! when all validators match exactly AND its mtime is older than its hash time by [`RACY_MARGIN_MS`]
//! (the racy-timestamp rule, as git does it). The library reads no environment and has no default
//! location: the caller passes a [`HashCacheConfig`].
//!
//! A full pass ignores the cache entirely, re-reads everything and rewrites it: when the file is
//! missing, unreadable, corrupt, of another format version or for another root; when the last full
//! pass is `full_rehash_every_ms` old; and when the rotating sample (1 % of the entries, at least
//! 16, re-hashed on every cached publish) finds a file whose validators match but whose bytes do not.

use super::*;
use std::collections::{BTreeMap, HashSet};
use std::time::UNIX_EPOCH;

/// Bump when the file layout changes; an older or newer file is a cold start.
pub const CACHE_FORMAT: u32 = 1;
/// An entry is trusted only if its mtime is at least this much older than when it was hashed.
pub const RACY_MARGIN_MS: u64 = 2_000;
/// Default interval between full passes: 24 hours.
pub const DEFAULT_FULL_REHASH_MS: u64 = 24 * 60 * 60 * 1000;
/// A cache file larger than this is treated as corrupt rather than read.
const MAX_CACHE_BYTES: u64 = 128 << 20;
const SAMPLE_MIN: usize = 16;

/// Where the cache lives and how often it is bypassed. `None` for a publish means no cache at all.
#[derive(Debug, Clone)]
pub struct HashCacheConfig {
    /// A file on the publishing machine's local disk (never in the store, never in the worktree).
    pub path: PathBuf,
    /// A publish at least this long after the last full pass is a full pass; 0 means every publish.
    pub full_rehash_every_ms: u64,
}

/// What the cache did in one publish (additive; all zero/false when the cache is off, except
/// `hashed`/`hashed_bytes`, which count the file reads either way).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HashCacheReport {
    pub enabled: bool,
    /// Files read and hashed in this publish (a cache-off publish: every published file).
    pub hashed: usize,
    pub hashed_bytes: u64,
    /// Files whose hash, size and title came from the cache without a read.
    pub from_cache: usize,
    /// Of `hashed`, files re-hashed only as the rotating integrity sample.
    pub sampled: usize,
    pub full_pass: bool,
    /// Why this was a full pass: `missing`, `unreadable`, `corrupt`, `format-version`, `other-root`,
    /// `interval`, `sample-mismatch` or `stale-entry`.
    pub full_pass_reason: Option<String>,
    /// A sampled (or put-time) re-hash disagreed with a cache entry whose validators matched: the
    /// cache was discarded and this publish did a full pass.
    pub mismatch: bool,
    /// The cache file could not be written (the publish went on).
    pub write_error: Option<String>,
}

/// The facts a file's metadata is compared on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Stamp {
    pub(super) size: u64,
    mtime_ns: i64,
    ctime_ns: i64,
    ino: u64,
}

impl Stamp {
    fn of(md: &std::fs::Metadata) -> Option<Stamp> {
        let m = md.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
        let mtime_ns = i64::try_from(m.as_nanos()).ok()?;
        #[cfg(unix)]
        let (ino, ctime_ns) = {
            use std::os::unix::fs::MetadataExt;
            (md.ino(), md.ctime().saturating_mul(1_000_000_000).saturating_add(md.ctime_nsec()))
        };
        #[cfg(not(unix))]
        let (ino, ctime_ns) = (0, 0);
        Some(Stamp { size: md.len(), mtime_ns, ctime_ns, ino })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Entry {
    #[serde(flatten)]
    pub(super) stamp: Stamp,
    pub(super) sha256: String,
    /// When (the publish clock, ms) this entry was hashed: the reference of the racy rule.
    hashed_ms: u64,
    pub(super) title: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct CacheFile {
    version: u32,
    root: String,
    last_full_ms: u64,
    cursor: usize,
    entries: BTreeMap<String, Entry>,
}

enum Load {
    Ok(CacheFile),
    Cold(&'static str),
}

fn load(path: &Path, root_key: &str) -> Load {
    let bytes = match std::fs::metadata(path) {
        Ok(m) if m.len() > MAX_CACHE_BYTES => return Load::Cold("corrupt"),
        Ok(_) => match std::fs::read(path) {
            Ok(b) => b,
            Err(_) => return Load::Cold("unreadable"),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Load::Cold("missing"),
        Err(_) => return Load::Cold("unreadable"),
    };
    #[derive(Deserialize)]
    struct Peek {
        version: u32,
    }
    match serde_json::from_slice::<Peek>(&bytes) {
        Ok(p) if p.version != CACHE_FORMAT => return Load::Cold("format-version"),
        Ok(_) => {}
        Err(_) => return Load::Cold("corrupt"),
    }
    match serde_json::from_slice::<CacheFile>(&bytes) {
        Ok(c) if c.root != root_key => Load::Cold("other-root"),
        Ok(c) => Load::Ok(c),
        Err(_) => Load::Cold("corrupt"),
    }
}

/// One publish's use of the cache.
pub(super) struct HashCacheRun {
    cfg: Option<HashCacheConfig>,
    root_key: String,
    now_ms: u64,
    old: BTreeMap<String, Entry>,
    cursor: usize,
    last_full_ms: u64,
    new: BTreeMap<String, Entry>,
    /// Files served from the cache, to be re-stat'ed once the whole tree is processed.
    served: Vec<(String, Stamp)>,
    rep: HashCacheReport,
}

impl HashCacheRun {
    pub fn open(cfg: Option<&HashCacheConfig>, root: &Path, now_ms: u64) -> Self {
        let root_key = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf()).to_string_lossy().into_owned();
        let mut run = HashCacheRun {
            cfg: cfg.cloned(),
            root_key,
            now_ms,
            old: BTreeMap::new(),
            cursor: 0,
            last_full_ms: 0,
            new: BTreeMap::new(),
            served: Vec::new(),
            rep: HashCacheReport { enabled: cfg.is_some(), ..Default::default() },
        };
        let Some(c) = cfg else { return run };
        match load(&c.path, &run.root_key) {
            Load::Cold(why) => run.force_full(why),
            Load::Ok(f) if now_ms < f.last_full_ms || c.full_rehash_every_ms == 0 || now_ms - f.last_full_ms >= c.full_rehash_every_ms => {
                run.force_full("interval")
            }
            Load::Ok(f) => {
                run.old = f.entries;
                run.cursor = f.cursor;
                run.last_full_ms = f.last_full_ms;
            }
        }
        run
    }

    /// Ignore every cached entry for the rest of this publish.
    pub fn force_full(&mut self, why: &str) {
        self.old.clear();
        self.rep.full_pass = true;
        self.rep.full_pass_reason = Some(why.into());
        if why == "sample-mismatch" || why == "stale-entry" {
            self.rep.mismatch = true;
        }
    }

    /// The entry for `path` if every validator matches and the racy rule trusts it.
    pub fn lookup(&self, path: &str, st: &Stamp) -> Option<&Entry> {
        self.old.get(path).filter(|e| Self::trusts(e, st))
    }

    fn trusts(e: &Entry, st: &Stamp) -> bool {
        let trusted_before_ns = i128::from(e.hashed_ms.saturating_sub(RACY_MARGIN_MS)) * 1_000_000;
        e.stamp == *st && i128::from(st.mtime_ns) < trusted_before_ns
    }

    pub fn stamp_of(md: &std::fs::Metadata) -> Option<Stamp> {
        Stamp::of(md)
    }

    /// Re-hash a rotating sample of the entries that would be served; any disagreement discards the cache.
    pub fn sample_check(&mut self, root: &Path, paths: &[String], max_file: u64) {
        if self.old.is_empty() || self.cfg.is_none() {
            return;
        }
        let listed: HashSet<&str> = paths.iter().map(String::as_str).collect();
        let old = std::mem::take(&mut self.old);
        let total = old.len();
        let want = (total / 100).max(SAMPLE_MIN);
        let start = self.cursor % total;
        let (mut checked, mut examined, mut mismatch) = (0, 0, false);
        for (path, e) in old.iter().cycle().skip(start).take(total) {
            if checked >= want {
                break;
            }
            examined += 1;
            let Some(full) = listed.contains(path.as_str()).then(|| safe_join(root, path).ok()).flatten() else { continue };
            let Some(st) = std::fs::symlink_metadata(&full).ok().and_then(|m| Stamp::of(&m)) else { continue };
            if !Self::trusts(e, &st) {
                continue;
            }
            let Ok(data) = read_regular(&full, max_file) else { continue };
            checked += 1;
            self.rep.hashed += 1;
            self.rep.hashed_bytes += data.len() as u64;
            self.rep.sampled += 1;
            if sha256_hex(&data) != e.sha256 {
                mismatch = true;
                break;
            }
        }
        self.cursor = (start + examined) % total;
        if mismatch {
            self.force_full("sample-mismatch");
        } else {
            self.old = old;
        }
    }

    pub fn note_read(&mut self, bytes: u64) {
        self.rep.hashed += 1;
        self.rep.hashed_bytes += bytes;
    }

    pub fn record_hashed(&mut self, path: &str, st: Stamp, sha: &str, title: Option<String>) {
        if self.cfg.is_some() {
            self.new.insert(path.into(), Entry { stamp: st, sha256: sha.into(), hashed_ms: self.now_ms, title });
        }
    }

    pub fn record_served(&mut self, path: &str, e: &Entry) {
        self.rep.from_cache += 1;
        self.served.push((path.into(), e.stamp));
        self.new.insert(path.into(), e.clone());
    }

    /// Paths served from the cache whose file no longer matches the entry: changed since the check.
    pub fn changed_since(&mut self, root: &Path) -> Vec<String> {
        let mut gone = Vec::new();
        for (p, st) in std::mem::take(&mut self.served) {
            let now = safe_join(root, &p).ok().and_then(|f| std::fs::symlink_metadata(f).ok()).and_then(|m| Stamp::of(&m));
            if now != Some(st) {
                self.new.remove(&p);
                self.rep.from_cache -= 1;
                gone.push(p);
            }
        }
        gone
    }

    /// Write the cache (atomically) and hand back the report. A failed write is reported, not raised.
    pub fn finish(mut self) -> HashCacheReport {
        let Some(c) = self.cfg.clone() else { return self.rep };
        let file = CacheFile {
            version: CACHE_FORMAT,
            root: self.root_key.clone(),
            last_full_ms: if self.rep.full_pass { self.now_ms } else { self.last_full_ms },
            cursor: self.cursor,
            entries: std::mem::take(&mut self.new),
        };
        if let Err(e) = write_atomic(&c.path, &file, self.now_ms) {
            self.rep.write_error = Some(e);
        }
        self.rep
    }
}

fn write_atomic(path: &Path, file: &CacheFile, now_ms: u64) -> std::result::Result<(), String> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let bytes = serde_json::to_vec(file).map_err(|e| e.to_string())?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".tmp{now_ms}-{}", N.fetch_add(1, Ordering::Relaxed)));
    let tmp = PathBuf::from(tmp);
    let done = std::fs::write(&tmp, &bytes).and_then(|()| std::fs::rename(&tmp, path));
    if done.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    done.map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn served_entry(root: &Path, rel: &str) -> Entry {
        let st = Stamp::of(&std::fs::symlink_metadata(root.join(rel)).unwrap()).unwrap();
        Entry { stamp: st, sha256: "0".repeat(64), hashed_ms: 1, title: None }
    }

    #[test]
    fn a_served_file_that_moved_after_the_check_is_reported_changed() {
        let d = tempfile::tempdir().unwrap();
        for f in ["a.txt", "b.txt"] {
            std::fs::write(d.path().join(f), "one").unwrap();
        }
        let cfg = HashCacheConfig { path: d.path().join("..").join("unused.json"), full_rehash_every_ms: 1 };
        let mut run = HashCacheRun::open(Some(&cfg), d.path(), 5);
        for f in ["a.txt", "b.txt"] {
            let e = served_entry(d.path(), f);
            run.record_served(f, &e);
        }
        assert!(run.changed_since(d.path()).is_empty(), "nothing moved yet");
        for f in ["a.txt", "b.txt"] {
            let e = served_entry(d.path(), f);
            run.record_served(f, &e);
        }
        std::fs::write(d.path().join("b.txt"), "two, longer").unwrap();
        assert_eq!(run.changed_since(d.path()), vec!["b.txt".to_string()]);
        assert_eq!(run.rep.from_cache, 3, "the moved file no longer counts as served");
        assert!(!run.new.contains_key("b.txt") && run.new.contains_key("a.txt"));
        std::fs::remove_file(d.path().join("a.txt")).unwrap();
        let e = served_entry(d.path(), "b.txt");
        run.record_served("b.txt", &e);
        run.served.push(("a.txt".into(), e.stamp));
        assert!(run.changed_since(d.path()).contains(&"a.txt".to_string()), "a deleted file is changed too");
    }
}
