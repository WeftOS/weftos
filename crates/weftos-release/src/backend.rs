//! The storage seam.
//!
//! ADR-126 §7 gives `weftos-store` an OpenDAL S3 client. S1 owns that client; this crate only
//! needs the three operations below, and the S3 mapping is exact:
//!
//! - `put_if_absent` is a PutObject with `If-None-Match: *`. RustFS answers 412 on an existing
//!   key (ADR-126 S0, "Conditional put"), which the adapter maps to [`PutOutcome::Exists`].
//! - `get` and `list` are GetObject and ListObjectsV2.
//!
//! There is deliberately **no unconditional put** in [`ReleaseBackend`]. A CI principal gets a
//! grant with PutObject and GetObject and no DeleteObject (ADR-126 §3, "CI writes once"), and the
//! trait gives the publisher no other way to write. [`DeletableBackend`] is a separate trait that
//! only the retention sweeper takes.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::task::{Context, Poll};

/// A backend failure (network, auth refused, server error). Never "key absent".
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("store backend: {0}")]
pub struct BackendError(pub String);

/// What a conditional put did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PutOutcome {
    /// The key did not exist and now holds the bytes.
    Created,
    /// The key already existed. Nothing was changed.
    Exists,
}

/// S3 Object Lock retention mode (ADR-126 S4, `docs/research/adr126-s4-release-bucket-immutability-*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockMode {
    /// Cannot be shortened or removed before the date except by a caller granted the bypass.
    Governance,
    /// Cannot be shortened or removed by anyone, root included.
    Compliance,
}

/// Keep a written version until `until` (Unix seconds).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lock {
    pub mode: LockMode,
    pub until: u64,
}

/// A bucket's Object Lock configuration, as read back from the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BucketLock {
    /// Object Lock is on (it can only be chosen when the bucket is made).
    pub enabled: bool,
    /// The default retention applied to a write that carries none: mode and days.
    pub default_retention: Option<(LockMode, u32)>,
}

/// Lifecycle rules a release bucket needs so deleted and replaced versions are eventually reclaimed.
/// The server skips versions still under retention, so these never shorten a lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LifecycleRules {
    /// Remove a noncurrent version this many days after it stopped being current. At least 1.
    pub noncurrent_days: u32,
    /// Remove a delete marker once no version remains behind it. S1's rule type cannot express this
    /// yet, so the S3 adapter refuses `true`.
    pub expire_delete_markers: bool,
    /// Abort a multipart upload not completed after this many days (a failed publish leaves parts).
    pub abort_multipart_days: Option<u32>,
}

/// One stored version of a key, or a delete marker, as a versioned bucket keeps them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyVersion {
    pub key: String,
    pub version_id: String,
    /// The newest entry for its key.
    pub is_latest: bool,
    /// A delete (the key reads as absent) rather than content.
    pub is_delete_marker: bool,
    /// Identifies the content: equal etags are equal bytes. Empty on a delete marker.
    pub etag: String,
}

/// Write-once access to one release store (one bucket).
pub trait ReleaseBackend: Send + Sync {
    /// Store `bytes` under `key` only if `key` does not exist. Must be atomic: of any number of
    /// concurrent callers for one key, exactly one gets `Created`.
    fn put_if_absent(
        &self,
        key: &str,
        bytes: &[u8],
    ) -> impl Future<Output = Result<PutOutcome, BackendError>> + Send;

    /// Like [`put_if_absent`](Self::put_if_absent), and the written version is locked until
    /// `lock.until`. Needs a bucket made with Object Lock. The default refuses, so a publisher asked
    /// to lock on a backend that cannot fails instead of writing unprotected objects.
    fn put_if_absent_locked(
        &self,
        _key: &str,
        _bytes: &[u8],
        _lock: &Lock,
    ) -> impl Future<Output = Result<PutOutcome, BackendError>> + Send {
        async { Err(BackendError("object lock is not supported by this backend".into())) }
    }

    /// The bucket's Object Lock configuration, or `None` when this backend cannot report it. A publisher
    /// that was asked to lock reads this first, so an unlocked bucket is one clear error.
    fn lock_config(&self) -> impl Future<Output = Result<Option<BucketLock>, BackendError>> + Send {
        async { Ok(None) }
    }

    /// The retention on the current version of `key`, or `None` when it has none or the backend cannot say.
    fn retention_of(&self, _key: &str) -> impl Future<Output = Result<Option<Lock>, BackendError>> + Send {
        async { Ok(None) }
    }

    /// True when this backend cannot read a version's retention but its `extend_lock` still only ever
    /// lengthens (the service route: S1 refuses a date that does not extend). The publisher then asks for
    /// the new date without reading the old one, and a "does not extend" answer counts as done.
    fn extends_blind(&self) -> bool {
        false
    }

    /// Lengthen (never shorten, never weaken) the retention of the current version of `key`. The default
    /// refuses; the publisher only calls it after `retention_of` returned a lock.
    fn extend_lock(&self, _key: &str, _lock: &Lock) -> impl Future<Output = Result<(), BackendError>> + Send {
        async { Err(BackendError("extending a lock is not supported by this backend".into())) }
    }

    /// Store the file at `path` (`size` bytes) under `key` only if the key does not exist, locked when
    /// `lock` is given. The default reads the whole file; a backend with multipart upload overrides it
    /// so the file is sent in parts and never held in memory.
    fn put_file_if_absent(
        &self,
        key: &str,
        path: &std::path::Path,
        size: u64,
        lock: Option<&Lock>,
    ) -> impl Future<Output = Result<PutOutcome, BackendError>> + Send {
        async move {
            let bytes = std::fs::read(path).map_err(|e| BackendError(format!("{}: {e}", path.display())))?;
            if bytes.len() as u64 != size {
                return Err(BackendError(format!("{}: changed while publishing", path.display())));
            }
            match lock {
                Some(l) => self.put_if_absent_locked(key, &bytes, l).await,
                None => self.put_if_absent(key, &bytes).await,
            }
        }
    }

    /// BLAKE3 and length of the stored object, or `None` when absent. The default downloads it; a backend
    /// that can hash server-side or in a stream overrides it.
    fn blob_digest(&self, key: &str) -> impl Future<Output = Result<Option<(String, u64)>, BackendError>> + Send {
        async move {
            Ok(self.get(key).await?.map(|b| (crate::manifest::hash_bytes(&b), b.len() as u64)))
        }
    }

    /// The object's bytes, or `None` when the key does not exist.
    fn get(&self, key: &str) -> impl Future<Output = Result<Option<Vec<u8>>, BackendError>> + Send;

    /// Every key that starts with `prefix`, in any order. Must follow pagination to the end.
    fn list(&self, prefix: &str) -> impl Future<Output = Result<Vec<String>, BackendError>> + Send;

    /// Every stored version and delete marker under `prefix`, or `None` when this store keeps no
    /// history (a bucket made without Object Lock). Needs a grant that lists versions.
    /// [`ReleaseReader::history`](crate::ReleaseReader::history) uses it to find a replaced manifest.
    fn versions(&self, _prefix: &str) -> impl Future<Output = Result<Option<Vec<KeyVersion>>, BackendError>> + Send {
        async { Ok(None) }
    }
}

/// The extra grant retention needs. Not given to CI.
pub trait DeletableBackend: ReleaseBackend {
    /// Delete `key`. Deleting a key that does not exist succeeds.
    fn delete(&self, key: &str) -> impl Future<Output = Result<(), BackendError>> + Send;

    /// Copy `src` to `dst` inside the store, server side: the bytes never pass through this
    /// process. Overwrites `dst`. Fails if `src` does not exist.
    fn copy(&self, src: &str, dst: &str) -> impl Future<Output = Result<(), BackendError>> + Send;
}

/// Lifecycle provisioning for a release bucket (an admin grant, never CI's).
pub trait LifecycleBackend: Send + Sync {
    fn put_lifecycle(&self, rules: &LifecycleRules) -> impl Future<Output = Result<(), BackendError>> + Send;
    /// The rules the server holds, or `None` when it has none.
    fn get_lifecycle(&self) -> impl Future<Output = Result<Option<LifecycleRules>, BackendError>> + Send;
}

/// Set the lifecycle rules and read them back; a server that stored something else is an error.
pub async fn provision_lifecycle<B: LifecycleBackend>(b: &B, rules: &LifecycleRules) -> Result<(), BackendError> {
    if rules.noncurrent_days == 0 {
        return Err(BackendError("noncurrent_days must be at least 1".into()));
    }
    b.put_lifecycle(rules).await?;
    match b.get_lifecycle().await? {
        Some(held) if held == *rules => Ok(()),
        Some(held) => Err(BackendError(format!("server holds {held:?}, not the rules just set"))),
        None => Err(BackendError("server reports no lifecycle rules right after setting them".into())),
    }
}

/// An in-memory backend with the same conditional-put semantics, for tests and local dry runs.
#[derive(Default)]
pub struct MemoryBackend {
    inner: Mutex<MemInner>,
    hooks: Mutex<Vec<DeleteHook>>,
    yielding: AtomicBool,
}

/// Returns `Pending` once, so concurrent tasks interleave between backend calls.
struct YieldNow(bool);

impl Future for YieldNow {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.0 {
            return Poll::Ready(());
        }
        self.0 = true;
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

type HookFn = Box<dyn FnOnce(&MemoryBackend) + Send>;

struct DeleteHook {
    prefix: String,
    after: bool,
    run: HookFn,
}

#[derive(Default)]
struct MemInner {
    objects: BTreeMap<String, Vec<u8>>,
    /// Puts left before every further put fails (simulates a crash or an outage mid-publish).
    puts_left: Option<usize>,
    puts_total: usize,
    locks: BTreeMap<String, Lock>,
    lock_enabled: bool,
    /// Per key, oldest first: `Some(etag)` is content, `None` a delete marker. Kept once versioning
    /// is on (Object Lock turns it on).
    history: BTreeMap<String, Vec<Option<String>>>,
    versioned: bool,
    lifecycle: Option<LifecycleRules>,
    default_retention: Option<(LockMode, u32)>,
    /// Largest single read a file put made (the mock of multipart part size).
    max_part: usize,
    file_puts: usize,
}

impl MemInner {
    fn record(&mut self, key: &str, bytes: Option<&[u8]>) {
        if self.versioned {
            self.history.entry(key.to_string()).or_default().push(bytes.map(crate::manifest::hash_bytes));
        }
    }
}

impl MemoryBackend {
    pub fn new() -> Self {
        Self::default()
    }

    /// After `n` more successful puts, every put returns a backend error.
    pub fn fail_puts_after(&self, n: usize) {
        self.inner.lock().unwrap().puts_left = Some(n);
    }

    /// Lift [`fail_puts_after`](Self::fail_puts_after).
    pub fn heal(&self) {
        self.inner.lock().unwrap().puts_left = None;
    }

    /// Number of stored objects.
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().objects.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Number of put calls that reached the store (created or not).
    pub fn put_calls(&self) -> usize {
        self.inner.lock().unwrap().puts_total
    }

    /// Behave like a bucket made with Object Lock: `put_if_absent_locked` works.
    pub fn enable_lock(&self) {
        let mut g = self.inner.lock().unwrap();
        g.lock_enabled = true;
        g.versioned = true;
    }

    /// The default retention the bucket reports (mock of a bucket default).
    pub fn set_default_retention(&self, d: Option<(LockMode, u32)>) {
        self.inner.lock().unwrap().default_retention = d;
    }

    /// Largest chunk a file put read at once, and how many file puts were made.
    pub fn file_put_stats(&self) -> (usize, usize) {
        let g = self.inner.lock().unwrap();
        (g.max_part, g.file_puts)
    }

    /// Keep every version, as a versioned bucket does, without Object Lock.
    pub fn enable_versioning(&self) {
        self.inner.lock().unwrap().versioned = true;
    }

    /// The lock stamped on the current version of `key`, if any.
    pub fn lock_of(&self, key: &str) -> Option<Lock> {
        self.inner.lock().unwrap().locks.get(key).copied()
    }

    /// Every key, sorted.
    pub fn keys(&self) -> Vec<String> {
        self.inner.lock().unwrap().objects.keys().cloned().collect()
    }

    /// Make every backend call yield to the scheduler first, so concurrent tasks interleave.
    #[doc(hidden)]
    pub fn yield_between_calls(&self, on: bool) {
        self.yielding.store(on, Ordering::Relaxed);
    }

    async fn pause(&self) {
        if self.yielding.load(Ordering::Relaxed) {
            YieldNow(false).await;
        }
    }

    /// Run `f` once, just before the next delete of a key starting with `prefix`, with the backend
    /// unlocked. Lets a test land a publish exactly between a sweep's last check and its delete.
    #[doc(hidden)]
    pub fn hook_before_delete(&self, prefix: &str, f: impl FnOnce(&MemoryBackend) + Send + 'static) {
        self.hooks.lock().unwrap().push(DeleteHook { prefix: prefix.into(), after: false, run: Box::new(f) });
    }

    /// Like [`hook_before_delete`](Self::hook_before_delete), but right after the delete.
    #[doc(hidden)]
    pub fn hook_after_delete(&self, prefix: &str, f: impl FnOnce(&MemoryBackend) + Send + 'static) {
        self.hooks.lock().unwrap().push(DeleteHook { prefix: prefix.into(), after: true, run: Box::new(f) });
    }

    fn fire(&self, key: &str, after: bool) {
        let due: Vec<DeleteHook> = {
            let mut g = self.hooks.lock().unwrap();
            let (due, keep): (Vec<_>, Vec<_>) =
                g.drain(..).partition(|h| h.after == after && key.starts_with(&h.prefix));
            *g = keep;
            due
        };
        for h in due {
            (h.run)(self);
        }
    }

    /// Overwrite or insert behind the trait's back, as an operator with admin rights or a
    /// damaged disk would. For adversarial tests only.
    #[doc(hidden)]
    pub fn tamper(&self, key: &str, bytes: &[u8]) {
        let mut g = self.inner.lock().unwrap();
        g.objects.insert(key.to_string(), bytes.to_vec());
        g.record(key, Some(bytes));
    }
}

impl ReleaseBackend for MemoryBackend {
    async fn put_if_absent(&self, key: &str, bytes: &[u8]) -> Result<PutOutcome, BackendError> {
        self.pause().await;
        let mut g = self.inner.lock().unwrap();
        g.puts_total += 1;
        if let Some(left) = g.puts_left.as_mut() {
            if *left == 0 {
                return Err(BackendError("injected put failure".into()));
            }
            *left -= 1;
        }
        if g.objects.contains_key(key) {
            return Ok(PutOutcome::Exists);
        }
        g.objects.insert(key.to_string(), bytes.to_vec());
        g.record(key, Some(bytes));
        Ok(PutOutcome::Created)
    }

    async fn put_if_absent_locked(&self, key: &str, bytes: &[u8], lock: &Lock) -> Result<PutOutcome, BackendError> {
        if !self.inner.lock().unwrap().lock_enabled {
            return Err(BackendError("bucket was not made with object lock".into()));
        }
        let out = self.put_if_absent(key, bytes).await?;
        if out == PutOutcome::Created {
            self.inner.lock().unwrap().locks.insert(key.to_string(), *lock);
        }
        Ok(out)
    }

    async fn lock_config(&self) -> Result<Option<BucketLock>, BackendError> {
        let g = self.inner.lock().unwrap();
        Ok(Some(BucketLock { enabled: g.lock_enabled, default_retention: g.default_retention }))
    }

    async fn retention_of(&self, key: &str) -> Result<Option<Lock>, BackendError> {
        Ok(self.inner.lock().unwrap().locks.get(key).copied())
    }

    async fn extend_lock(&self, key: &str, lock: &Lock) -> Result<(), BackendError> {
        let mut g = self.inner.lock().unwrap();
        let Some(have) = g.locks.get(key).copied() else {
            return Err(BackendError(format!("{key} has no retention to extend")));
        };
        if lock.until < have.until || lock.mode != have.mode {
            return Err(BackendError("a lock can be lengthened, never shortened, and its mode is not changed".into()));
        }
        g.locks.insert(key.to_string(), *lock);
        Ok(())
    }

    async fn put_file_if_absent(
        &self,
        key: &str,
        path: &std::path::Path,
        size: u64,
        lock: Option<&Lock>,
    ) -> Result<PutOutcome, BackendError> {
        use std::io::Read;
        // Read in parts, as a multipart upload would, and remember the biggest part held at once.
        const PART: usize = 1 << 20;
        let mut f = std::fs::File::open(path).map_err(|e| BackendError(format!("{}: {e}", path.display())))?;
        let mut all = Vec::new();
        let mut buf = vec![0u8; PART];
        let mut biggest = 0;
        loop {
            let n = f.read(&mut buf).map_err(|e| BackendError(e.to_string()))?;
            if n == 0 {
                break;
            }
            biggest = biggest.max(n);
            all.extend_from_slice(&buf[..n]);
        }
        if all.len() as u64 != size {
            return Err(BackendError(format!("{}: changed while publishing", path.display())));
        }
        {
            let mut g = self.inner.lock().unwrap();
            g.max_part = g.max_part.max(biggest);
            g.file_puts += 1;
        }
        match lock {
            Some(l) => self.put_if_absent_locked(key, &all, l).await,
            None => self.put_if_absent(key, &all).await,
        }
    }

    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, BackendError> {
        self.pause().await;
        Ok(self.inner.lock().unwrap().objects.get(key).cloned())
    }

    async fn list(&self, prefix: &str) -> Result<Vec<String>, BackendError> {
        self.pause().await;
        let g = self.inner.lock().unwrap();
        Ok(g.objects.keys().filter(|k| k.starts_with(prefix)).cloned().collect())
    }

    async fn versions(&self, prefix: &str) -> Result<Option<Vec<KeyVersion>>, BackendError> {
        self.pause().await;
        let g = self.inner.lock().unwrap();
        if !g.versioned {
            return Ok(None);
        }
        let mut out = Vec::new();
        for (key, hist) in g.history.iter().filter(|(k, _)| k.starts_with(prefix)) {
            // Newest first, as S3 lists them.
            for (i, v) in hist.iter().enumerate().rev() {
                out.push(KeyVersion {
                    key: key.clone(),
                    version_id: format!("v{i}"),
                    is_latest: i + 1 == hist.len(),
                    is_delete_marker: v.is_none(),
                    etag: v.clone().unwrap_or_default(),
                });
            }
        }
        Ok(Some(out))
    }
}

impl LifecycleBackend for MemoryBackend {
    async fn put_lifecycle(&self, rules: &LifecycleRules) -> Result<(), BackendError> {
        self.inner.lock().unwrap().lifecycle = Some(*rules);
        Ok(())
    }
    async fn get_lifecycle(&self) -> Result<Option<LifecycleRules>, BackendError> {
        Ok(self.inner.lock().unwrap().lifecycle)
    }
}

impl DeletableBackend for MemoryBackend {
    async fn copy(&self, src: &str, dst: &str) -> Result<(), BackendError> {
        self.pause().await;
        let mut g = self.inner.lock().unwrap();
        let Some(bytes) = g.objects.get(src).cloned() else {
            return Err(BackendError(format!("copy: {src} does not exist")));
        };
        g.record(dst, Some(&bytes));
        g.objects.insert(dst.to_string(), bytes);
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), BackendError> {
        self.pause().await;
        self.fire(key, false);
        {
            let mut g = self.inner.lock().unwrap();
            if g.objects.remove(key).is_some() {
                g.record(key, None);
            }
            g.locks.remove(key);
        }
        self.fire(key, true);
        Ok(())
    }
}
