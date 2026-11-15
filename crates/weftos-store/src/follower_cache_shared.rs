//! One manifest cache and one registry cache shared by every read verb of a long-lived process (the
//! daemon), WO-069 X2. Holds both behind short locks: a lock is taken to look up, released for the
//! backend calls, and taken again to store. Two threads that miss the same slot both load it; the
//! second store replaces the first. Nothing here reads a clock: every call takes `now_ms`, which the
//! daemon reads from a monotonic clock.
//!
//! Staleness, per read (the module docs of `follower_cache` and `follower_registry_cache` say why):
//! - a registry list (`browse` with no slug, `search`, `leftovers`, the orphan display plan) is as
//!   of its own listing: no window;
//! - a worktree's entry and manifest (`browse` ls/get, `search` hits, `overlaps`) are at most one TTL
//!   older than a write made by another process, never older than `max_age`, and current at once
//!   after a write this process made and reported with [`SharedCache::invalidate`];
//! - a `browse` get never returns bytes that do not match the manifest it lists, and a republish
//!   inside the TTL gives the new bytes (the slot is dropped and loaded again once).
//!
//! Not for anything that claims, promotes, retires, refuses or fences: those read the store.

use crate::follower::{DocHit, SearchResult};
use crate::follower_browse::{BrowseFile, BrowseListing, RegistryBrowse, clean_path, get_from, ls_from, slug_ok, summary};
use crate::follower_cache::{
    CacheStats, DEFAULT_MAX_AGE_MS, DEFAULT_MAX_BYTES, DEFAULT_MAX_ENTRIES, DEFAULT_TTL_MS, Loaded, Lookup, ManifestCache, Source,
    load_slot, still_current,
};
use crate::follower_registry_cache::{DEFAULT_MAX_REGISTRY_ENTRIES, Listed, RegistryCache, RegistryStats, list_shared_reusing};
use crate::follower_report::{OverlapReport, overlaps_from};
use crate::overlay::{ObjectBackend, Result, S2Error};
use crate::share::State;
use crate::share_fetch::{RegistryEntry, find_docs};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

/// Freshness window in ms (default [`DEFAULT_TTL_MS`]).
pub const ENV_TTL_MS: &str = "WEFTOS_WORKTREE_CACHE_TTL_MS";
/// Hard slot age in ms (default [`DEFAULT_MAX_AGE_MS`]).
pub const ENV_MAX_AGE_MS: &str = "WEFTOS_WORKTREE_CACHE_MAX_AGE_MS";
/// Manifest bytes held, in MiB (default 32); `0` turns the cache off.
pub const ENV_MB: &str = "WEFTOS_WORKTREE_CACHE_MB";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheCfg {
    pub ttl_ms: u64,
    pub max_age_ms: u64,
    pub max_bytes: u64,
    pub max_entries: usize,
}

impl Default for CacheCfg {
    fn default() -> Self {
        Self { ttl_ms: DEFAULT_TTL_MS, max_age_ms: DEFAULT_MAX_AGE_MS, max_bytes: DEFAULT_MAX_BYTES, max_entries: DEFAULT_MAX_ENTRIES }
    }
}

impl CacheCfg {
    /// The configuration from the three variables, or `None` for "no cache" (every verb then reads
    /// the store exactly as without one). `MB=0` is off. A value that is not a whole number is also
    /// off, so a typo can only cost round trips, never serve something stale for longer.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Option<Self> {
        let num = |var: &str, dflt: u64| -> Option<u64> {
            match get(var) {
                None => Some(dflt),
                Some(v) if v.trim().is_empty() => Some(dflt),
                Some(v) => v.trim().parse::<u64>().ok(),
            }
        };
        let d = Self::default();
        let mb = num(ENV_MB, d.max_bytes >> 20)?;
        if mb == 0 {
            return None;
        }
        let max_age_ms = num(ENV_MAX_AGE_MS, d.max_age_ms)?.max(1);
        let ttl_ms = num(ENV_TTL_MS, d.ttl_ms)?.min(max_age_ms);
        Some(Self { ttl_ms, max_age_ms, max_bytes: mb.saturating_mul(1 << 20), max_entries: d.max_entries })
    }

    pub fn from_env() -> Option<Self> {
        Self::from_lookup(|v| std::env::var(v).ok())
    }
}

pub struct SharedCache {
    m: Mutex<ManifestCache>,
    r: Mutex<RegistryCache>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // every critical section here is plain map work; a panic inside one leaves consistent maps
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl SharedCache {
    pub fn new(cfg: CacheCfg) -> Self {
        let m = ManifestCache::new(cfg.ttl_ms, cfg.max_entries, cfg.max_bytes).with_max_age(cfg.max_age_ms);
        Self { m: Mutex::new(m), r: Mutex::new(RegistryCache::new(DEFAULT_MAX_REGISTRY_ENTRIES)) }
    }

    pub fn stats(&self) -> (CacheStats, RegistryStats) {
        (lock(&self.m).stats, lock(&self.r).stats)
    }

    /// Manifest slots held and their JSON bytes.
    pub fn footprint(&self) -> (usize, u64) {
        let m = lock(&self.m);
        (m.len(), m.bytes())
    }

    /// This process changed `slug`'s registry entry, handoff, manifest or bucket.
    pub fn invalidate(&self, project_bucket: &str, slug: &str) {
        lock(&self.r).invalidate(project_bucket, slug);
        lock(&self.m).invalidate(project_bucket, slug);
    }

    /// Every slug of one project (a gc that may retire several).
    pub fn invalidate_project(&self, project_bucket: &str) {
        lock(&self.r).invalidate_where(|p, _| p != project_bucket);
        lock(&self.m).invalidate_where(|p, _| p != project_bucket);
    }

    /// `slug` in every project (a store archive knows the slug, not where its registry lives).
    pub fn invalidate_slug(&self, slug: &str) {
        lock(&self.r).invalidate_where(|_, s| s != slug);
        lock(&self.m).invalidate_where(|_, s| s != slug);
    }

    /// Registry `slug -> entry etag` just seen by some other read (a follow poll).
    pub fn observe(&self, project_bucket: &str, current: &BTreeMap<String, String>) {
        lock(&self.m).observe_registry(project_bucket, current);
    }

    /// One registry listing; only entries whose listed etag moved are read. Also drops the manifest
    /// slots whose entry moved or left.
    pub fn registry<B: ObjectBackend>(&self, b: &B, project_bucket: &str) -> Result<Listed> {
        let l = list_shared_reusing(
            b,
            project_bucket,
            |slug, etag| {
                let mut r = lock(&self.r);
                let hit = r.get(project_bucket, slug, etag);
                if hit.is_some() { r.stats.reused += 1 } else { r.stats.read += 1 }
                hit
            },
            |slug, etag, e| lock(&self.r).put(project_bucket, slug, etag, e),
            |slugs| lock(&self.r).retain_listed(project_bucket, slugs),
        )?;
        lock(&self.m).observe_registry(project_bucket, &l.etags());
        Ok(l)
    }

    /// The entry and manifest of `slug`. `known` is the entry and its etag from a listing this call
    /// just made (it replaces the entry `head` and `get`). No lock is held across a backend call.
    pub fn loaded<B: ObjectBackend>(
        &self,
        b: &B,
        project_bucket: &str,
        slug: &str,
        known: Option<(&RegistryEntry, &str)>,
        now_ms: u64,
    ) -> Result<(Arc<Loaded>, Source)> {
        if !slug_ok(slug) {
            return Err(S2Error::Invalid(format!("bad worktree slug {slug:?}")));
        }
        let look = lock(&self.m).lookup(project_bucket, slug, now_ms);
        let t = match look {
            Lookup::Fresh(l) => return Ok((l, Source::Fresh)),
            Lookup::Miss(t) => t,
            Lookup::Stale(p, _) => {
                if still_current(b, project_bucket, slug, &p, known.map(|k| k.1))? {
                    lock(&self.m).confirm(project_bucket, slug, &p, now_ms);
                    return Ok((p.loaded, Source::Revalidated));
                }
                let mut m = lock(&self.m);
                m.reject(project_bucket, slug, &p);
                m.ticket()
            }
        };
        let f = load_slot(b, project_bucket, slug, known)?;
        lock(&self.m).install(project_bucket, slug, &f, t, now_ms);
        Ok((f.loaded, Source::Cold))
    }

    /// `browse` with no slug.
    pub fn browse_registry<B: ObjectBackend>(&self, b: &B, project_bucket: &str) -> Result<RegistryBrowse> {
        let l = self.registry(b, project_bucket)?;
        Ok(RegistryBrowse { worktrees: l.entries.iter().map(|(e, _)| summary(e)).collect(), unreadable: l.unreadable })
    }

    /// `browse <slug> [folder/]`.
    pub fn browse_ls<B: ObjectBackend>(&self, b: &B, project_bucket: &str, slug: &str, prefix: &str, now_ms: u64) -> Result<BrowseListing> {
        let dir = clean_path(prefix, true)?;
        let (l, _) = self.loaded(b, project_bucket, slug, None, now_ms)?;
        ls_from(&l.entry, &l.manifest, dir)
    }

    /// `browse <slug> <file>`: the manifest from the cache, the object always live. A cached
    /// manifest the object no longer matches (a republish inside the TTL) is dropped and the read
    /// retried once from a cold load.
    pub fn browse_get<B: ObjectBackend>(
        &self,
        b: &B,
        project_bucket: &str,
        slug: &str,
        path: &str,
        max_bytes: u64,
        now_ms: u64,
    ) -> Result<BrowseFile> {
        let path = clean_path(path, false)?;
        let (l, src) = self.loaded(b, project_bucket, slug, None, now_ms)?;
        match get_from(b, &l.entry, &l.manifest, slug, &path, max_bytes) {
            Err(_) if src != Source::Cold => {
                lock(&self.m).invalidate(project_bucket, slug);
                let (l, _) = self.loaded(b, project_bucket, slug, None, now_ms)?;
                get_from(b, &l.entry, &l.manifest, slug, &path, max_bytes)
            }
            r => r,
        }
    }

    /// `search`: [`crate::follower::search_docs`] over the cached registry and manifests. Returns the
    /// result and the registry entries that could not be read.
    pub fn search<B: ObjectBackend>(&self, b: &B, project_bucket: &str, query: &str, now_ms: u64) -> Result<(SearchResult, Vec<String>)> {
        let l = self.registry(b, project_bucket)?;
        let mut res = SearchResult::default();
        for (e, etag) in l.entries.iter().filter(|(e, _)| !matches!(e.state, State::Merged | State::Archived)) {
            match self.loaded(b, project_bucket, &e.slug, Some((e, etag)), now_ms) {
                Ok((ld, _)) => res.hits.extend(find_docs(&ld.manifest, query).into_iter().map(|d| DocHit {
                    slug: e.slug.clone(),
                    path: d.path.clone(),
                    title: d.title.clone(),
                    state: e.state,
                })),
                Err(_) => res.unreadable.push(e.slug.clone()),
            }
        }
        res.hits.sort_by(|a, c| a.slug.cmp(&c.slug).then_with(|| a.path.cmp(&c.path)));
        Ok((res, l.unreadable))
    }

    /// `overlaps a b` from the two cached manifests.
    pub fn overlaps<B: ObjectBackend>(&self, b: &B, project_bucket: &str, a: &str, c: &str, now_ms: u64) -> Result<OverlapReport> {
        let (la, _) = self.loaded(b, project_bucket, a, None, now_ms)?;
        let (lc, _) = self.loaded(b, project_bucket, c, None, now_ms)?;
        if la.entry.bucket == lc.entry.bucket {
            return Err(S2Error::Invalid("both entries point at the same bucket".into()));
        }
        Ok(overlaps_from(a, &la.manifest, c, &lc.manifest))
    }
}
