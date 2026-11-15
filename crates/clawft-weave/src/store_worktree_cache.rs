//! The daemon's one manifest and registry cache for the `store.worktree.*` read verbs (WO-069 X2).
//! A child of `store_worktree`.
//!
//! One `SharedCache` per store service, found by the service's backend: the daemon has one service,
//! tests make many (each with its own backend and the same bucket names), and a cache must never
//! answer for a backend it did not read. The holder keeps a `Weak` of that backend, which also keeps
//! its address from being reused while the holder exists.
//!
//! The configuration (`WEFTOS_WORKTREE_CACHE_*`, see `weftos_store::follower_cache_shared`) is read
//! once, when a service's cache is first asked for; `WEFTOS_WORKTREE_CACHE_MB=0` gives no cache and
//! every verb then reads the store exactly as before. The clock is monotonic (`mono_ms`) plus a
//! per-service offset that only tests move.
//!
//! Writes: each write arm holds an [`Invalidate`] for the slug it changes; it fires when the arm
//! returns, success or error, so a later read never sees the state from before this daemon's write.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use weftos_store::follower_cache_shared::{CacheCfg, SharedCache};

use super::*;

struct Holder {
    backend: Weak<dyn S1>,
    cache: Option<Arc<SharedCache>>,
    offset_ms: Arc<AtomicU64>,
}

fn holders() -> &'static Mutex<Vec<Holder>> {
    static H: OnceLock<Mutex<Vec<Holder>>> = OnceLock::new();
    H.get_or_init(|| Mutex::new(Vec::new()))
}

fn same(w: &Weak<dyn S1>, a: &Arc<dyn S1>) -> bool {
    std::ptr::addr_eq(w.as_ptr(), Arc::as_ptr(a))
}

/// The holder of `svc`, made with `cfg` (or the environment's) when there is none yet.
fn with_holder<T>(svc: &StoreService, cfg: Option<Option<CacheCfg>>, f: impl FnOnce(&mut Holder) -> T) -> T {
    let be = svc.operator_objects();
    let mut hs = holders().lock().unwrap_or_else(|e| e.into_inner());
    hs.retain(|h| h.backend.strong_count() > 0);
    let i = match hs.iter().position(|h| same(&h.backend, &be)) {
        Some(i) => i,
        None => {
            let cfg = cfg.unwrap_or_else(CacheCfg::from_env);
            hs.push(Holder { backend: Arc::downgrade(&be), cache: cfg.map(|c| Arc::new(SharedCache::new(c))), offset_ms: Arc::default() });
            hs.len() - 1
        }
    };
    f(&mut hs[i])
}

/// `svc`'s cache, or `None` when caching is off.
pub(super) fn cache(svc: &StoreService) -> Option<Arc<SharedCache>> {
    with_holder(svc, None, |h| h.cache.clone())
}

/// The cache clock: monotonic milliseconds, never the caller's wall clock.
pub(super) fn now(svc: &StoreService) -> u64 {
    let off = with_holder(svc, None, |h| h.offset_ms.clone());
    mono_ms().saturating_add(off.load(Ordering::Relaxed))
}

/// Tests: give `svc` this cache configuration (`None` = off), replacing any cache it had.
#[cfg(test)]
pub(super) fn set_cfg(svc: &StoreService, cfg: Option<CacheCfg>) {
    with_holder(svc, Some(cfg), |h| h.cache = cfg.map(|c| Arc::new(SharedCache::new(c))));
}

/// Tests: move `svc`'s cache clock forward.
#[cfg(test)]
pub(super) fn advance(svc: &StoreService, ms: u64) {
    with_holder(svc, None, |h| h.offset_ms.fetch_add(ms, Ordering::Relaxed));
}

/// What an [`Invalidate`] drops.
enum Scope {
    Slug(String, String),
    Project(String),
    AnySlug(String),
}

/// Drops the cached state of what a write arm changes, when the arm ends (success or error).
pub(super) struct Invalidate(Option<Arc<SharedCache>>, Scope);

impl Drop for Invalidate {
    fn drop(&mut self) {
        let Some(c) = &self.0 else { return };
        match &self.1 {
            Scope::Slug(p, s) => c.invalidate(p, s),
            Scope::Project(p) => c.invalidate_project(p),
            Scope::AnySlug(s) => c.invalidate_slug(s),
        }
    }
}

/// One slug of the project whose registry is `reg`.
pub(super) fn on_exit(svc: &StoreService, reg: &str, slug: &str) -> Invalidate {
    Invalidate(cache(svc), Scope::Slug(reg.to_owned(), slug.to_owned()))
}

/// Every slug of the project whose registry is `reg`.
pub(super) fn on_exit_project(svc: &StoreService, reg: &str) -> Invalidate {
    Invalidate(cache(svc), Scope::Project(reg.to_owned()))
}

/// `slug` wherever its registry lives.
pub(super) fn on_exit_slug(svc: &StoreService, slug: &str) -> Invalidate {
    Invalidate(cache(svc), Scope::AnySlug(slug.to_owned()))
}
