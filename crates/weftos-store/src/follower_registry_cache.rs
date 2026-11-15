//! Registry-entry reuse on the listing path (WO-069 X2, groundwork note lane S2).
//!
//! Every reader of the project registry (`leftovers`, `search`, the browse list, the orphan plan)
//! lists `worktrees/` and then GETs every entry. The listing already returns each object's etag, so
//! a [`RegistryCache`] of `(project bucket, slug) -> (etag, parsed entry)` lets a read GET only the
//! entries whose etag moved since it last looked. The listing stays the authority: it is made on
//! every call, an entry missing from it is dropped from the cache, and an entry is reused only when
//! its listed etag equals the one it was read under. So the result is exactly what
//! [`crate::share_fetch::list_shared_lenient`] returns for the same listing, with no staleness
//! window of its own (the digest's lag does not apply: the digest is not read).
//!
//! What is never cached: a corrupt, unknown-schema or mislabelled entry (it is GET and reported on
//! every call, as today) and an entry that vanished between the listing and the GET (skipped, as
//! today). The etag stored is the one the listing gave, taken before the body: if the entry moves in
//! between, the next listing shows a different etag and the entry is read again. Conservative,
//! never stale.
//!
//! Signed registries are not served from here: the daemon bypasses the cache when a trusted-key
//! file is set (signature checks and the generation rollback guard read the registry as before).

use crate::overlay::{ObjectBackend, Result};
use crate::share_fetch::{DIGEST_KEY, REGISTRY_PREFIX, RegistryEntry, check_entry};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// Default cap on cached entries across all projects; over it the cache is cleared and refilled.
pub const DEFAULT_MAX_REGISTRY_ENTRIES: usize = 4096;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RegistryStats {
    /// Entries reused because their listed etag was unchanged.
    pub reused: u64,
    /// Entries read (absent from the cache or etag moved).
    pub read: u64,
    /// Entries dropped because the listing no longer had them.
    pub dropped: u64,
}

type Key = (String, String);

pub struct RegistryCache {
    entries: HashMap<Key, (String, RegistryEntry)>,
    max_entries: usize,
    pub stats: RegistryStats,
}

impl Default for RegistryCache {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_REGISTRY_ENTRIES)
    }
}

impl RegistryCache {
    pub fn new(max_entries: usize) -> Self {
        Self { entries: HashMap::new(), max_entries: max_entries.max(1), stats: RegistryStats::default() }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The cached entry for `slug` if it was read under exactly `etag`.
    pub fn get(&self, project_bucket: &str, slug: &str, etag: &str) -> Option<RegistryEntry> {
        let (e, entry) = self.entries.get(&(project_bucket.to_string(), slug.to_string()))?;
        (e == etag).then(|| entry.clone())
    }

    pub fn put(&mut self, project_bucket: &str, slug: &str, etag: &str, entry: &RegistryEntry) {
        if self.entries.len() >= self.max_entries {
            self.entries.clear();
        }
        self.entries.insert((project_bucket.to_string(), slug.to_string()), (etag.to_string(), entry.clone()));
    }

    /// Keep only the slugs of `project_bucket` the listing still has.
    pub fn retain_listed(&mut self, project_bucket: &str, listed: &BTreeSet<String>) {
        let before = self.entries.len();
        self.entries.retain(|k, _| k.0 != project_bucket || listed.contains(&k.1));
        self.stats.dropped += (before - self.entries.len()) as u64;
    }

    pub fn invalidate(&mut self, project_bucket: &str, slug: &str) {
        self.entries.remove(&(project_bucket.to_string(), slug.to_string()));
    }

    /// Drop every entry `keep` returns false for (by `(project bucket, slug)`).
    pub fn invalidate_where(&mut self, keep: impl Fn(&str, &str) -> bool) {
        self.entries.retain(|k, _| keep(&k.0, &k.1));
    }
}

/// One registry read: the readable entries (newest first, as `list_shared_lenient`) each with the
/// etag the listing gave, and the key slugs that could not be read.
#[derive(Debug, Default, Clone)]
pub struct Listed {
    pub entries: Vec<(RegistryEntry, String)>,
    pub unreadable: Vec<String>,
}

impl Listed {
    /// `slug -> entry etag` of the readable entries, for `ManifestCache::observe_registry`.
    pub fn etags(&self) -> BTreeMap<String, String> {
        self.entries.iter().map(|(e, t)| (e.slug.clone(), t.clone())).collect()
    }
    pub fn into_entries(self) -> Vec<RegistryEntry> {
        self.entries.into_iter().map(|(e, _)| e).collect()
    }
}

/// [`crate::share_fetch::list_shared_lenient`] with entry reuse: one listing, then a GET only for
/// the entries `reuse` cannot give for their listed etag; every readable entry read is handed to
/// `keep`, and `listed` gets the full set of slugs the listing had. The closures let a caller hold
/// its lock only while it touches the cache, never across a backend call.
pub fn list_shared_reusing<B: ObjectBackend>(
    b: &B,
    project_bucket: &str,
    reuse: impl Fn(&str, &str) -> Option<RegistryEntry>,
    keep: impl Fn(&str, &str, &RegistryEntry),
    listed: impl FnOnce(&BTreeSet<String>),
) -> Result<Listed> {
    let (mut v, mut bad) = (Vec::new(), Vec::new());
    let mut slugs = BTreeSet::new();
    for o in b.list(project_bucket, REGISTRY_PREFIX)?.into_iter().filter(|o| o.key != DIGEST_KEY) {
        // the same key-to-slug rule as `share_fetch::read_registry`
        let key_slug = o.key.trim_start_matches(REGISTRY_PREFIX).trim_end_matches(".json").to_string();
        slugs.insert(key_slug.clone());
        if let Some(e) = reuse(&key_slug, &o.etag) {
            v.push((e, o.etag));
            continue;
        }
        let Some(bytes) = b.get(project_bucket, &o.key)? else { continue };
        match check_entry(project_bucket, &key_slug, Some(&bytes)) {
            Some(e) => {
                keep(&key_slug, &o.etag, &e);
                v.push((e, o.etag));
            }
            None => bad.push(key_slug),
        }
    }
    listed(&slugs);
    v.sort_by(|(a, _), (c, _)| c.updated_ms.cmp(&a.updated_ms).then_with(|| a.slug.cmp(&c.slug)));
    bad.sort();
    Ok(Listed { entries: v, unreadable: bad })
}

/// [`list_shared_reusing`] on a cache the caller owns.
pub fn list_shared_cached<B: ObjectBackend>(b: &B, cache: &mut RegistryCache, project_bucket: &str) -> Result<Listed> {
    let cell = std::cell::RefCell::new(cache);
    list_shared_reusing(
        b,
        project_bucket,
        |slug, etag| {
            let mut c = cell.borrow_mut();
            let hit = c.get(project_bucket, slug, etag);
            if hit.is_some() { c.stats.reused += 1 } else { c.stats.read += 1 }
            hit
        },
        |slug, etag, e| cell.borrow_mut().put(project_bucket, slug, etag, e),
        |slugs| cell.borrow_mut().retain_listed(project_bucket, slugs),
    )
}
