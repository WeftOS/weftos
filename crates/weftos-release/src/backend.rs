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

/// Write-once access to one release store (one bucket).
pub trait ReleaseBackend: Send + Sync {
    /// Store `bytes` under `key` only if `key` does not exist. Must be atomic: of any number of
    /// concurrent callers for one key, exactly one gets `Created`.
    fn put_if_absent(
        &self,
        key: &str,
        bytes: &[u8],
    ) -> impl Future<Output = Result<PutOutcome, BackendError>> + Send;

    /// The object's bytes, or `None` when the key does not exist.
    fn get(&self, key: &str) -> impl Future<Output = Result<Option<Vec<u8>>, BackendError>> + Send;

    /// Every key that starts with `prefix`, in any order. Must follow pagination to the end.
    fn list(&self, prefix: &str) -> impl Future<Output = Result<Vec<String>, BackendError>> + Send;
}

/// The extra grant retention needs. Not given to CI.
pub trait DeletableBackend: ReleaseBackend {
    /// Delete `key`. Deleting a key that does not exist succeeds.
    fn delete(&self, key: &str) -> impl Future<Output = Result<(), BackendError>> + Send;

    /// Copy `src` to `dst` inside the store, server side: the bytes never pass through this
    /// process. Overwrites `dst`. Fails if `src` does not exist.
    fn copy(&self, src: &str, dst: &str) -> impl Future<Output = Result<(), BackendError>> + Send;
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
        self.inner.lock().unwrap().objects.insert(key.to_string(), bytes.to_vec());
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
        Ok(PutOutcome::Created)
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
}

impl DeletableBackend for MemoryBackend {
    async fn copy(&self, src: &str, dst: &str) -> Result<(), BackendError> {
        self.pause().await;
        let mut g = self.inner.lock().unwrap();
        let Some(bytes) = g.objects.get(src).cloned() else {
            return Err(BackendError(format!("copy: {src} does not exist")));
        };
        g.objects.insert(dst.to_string(), bytes);
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), BackendError> {
        self.pause().await;
        self.fire(key, false);
        self.inner.lock().unwrap().objects.remove(key);
        self.fire(key, true);
        Ok(())
    }
}
