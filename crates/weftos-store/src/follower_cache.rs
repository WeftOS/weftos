//! Short-lived manifest cache for the follower/browse path (WO-069 lane F, prototype; ADR-126 4a, Q14).
//!
//! Every uncached `browse_ls` / `browse_get` re-reads the registry entry, the bucket's
//! `.wt/meta.json` and the manifest (up to 32 MiB). A dashboard folder click does that per click.
//! [`ManifestCache`] keeps, per `(project bucket, slug)`, the parsed manifest with everything that
//! proves it is still the right one.
//!
//! What a cached slot holds: the registry entry and its etag, the manifest etag (from a `head`),
//! the parsed manifest, `inserted_ms` (restarted by a revalidation) and `loaded_ms` (the cold load,
//! never restarted). There is no clock in here: every call takes `now_ms`, which the daemon takes
//! from a monotonic clock (WO-069 X2).
//!
//! When a slot is served:
//! - **Within the TTL** it is served with no backend call at all. This also skips the
//!   `.wt/meta.json` cross-check. That is safe because the cross-check's inputs are the registry
//!   entry's `(slug, bucket)` and the bucket's `meta.json`, and a slot is only ever created by a
//!   load that ran the cross-check on exactly that entry. The slot is also dropped at once when an
//!   observation shows the entry's etag moved ([`ManifestCache::observe_registry`], fed by a
//!   listing or the digest) or when the daemon invalidates it after its own write.
//! - **After the TTL** one `head` of the registry entry and one `head` of the manifest decide (WO-069
//!   X2: before this the entry was not re-checked, so a claim, merge or orphan mark, which change the
//!   entry and not the manifest, was served with no bound). Both etags unchanged: the slot is kept and
//!   its TTL restarts. Either moved or missing: the slot is dropped and a full cold load follows,
//!   which runs the cross-check again. A caller that has just listed the registry passes the entry
//!   and its etag ([`ManifestCache::get_or_load_known`]), which replaces the entry `head`.
//! - **After `max_age`** (since the cold load, revalidations do not restart it) the slot is dropped
//!   whatever the etags say, so nothing is older than `max_age` even if every validator lies.
//! - **Cold, or after any invalidation** the load is: `head` + `get` of the registry entry, the
//!   mandatory `.wt/meta.json` cross-check ([`verify_entry`]), then `head` + `get` of the manifest.
//!   A failed load (missing entry, missing manifest, refused cross-check) stores nothing.
//!
//! The split API ([`ManifestCache::lookup`], [`load_slot`], [`ManifestCache::install`]) lets a caller
//! that shares the cache behind a lock do the backend calls with the lock released. An
//! invalidation bumps an epoch; a load that started before it is served to its caller but not
//! stored, so a slot never outlives a write this process made.
//!
//! Bounded: at most `max_entries` slots and `max_bytes` of manifest JSON; the oldest slot (by
//! `inserted_ms`, then insertion order) is evicted first. A manifest bigger than `max_bytes` alone
//! is served but not stored.

use crate::follower::verify_entry;
use crate::overlay::{ObjectBackend, Result, S2Error};
use crate::share::{MANIFEST_KEY, MAX_MANIFEST_BYTES, TreeManifest, parse_manifest};
use crate::share_fetch::{REGISTRY_PREFIX, RegistryEntry, check_entry};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

pub use crate::follower_cache_ops::{browse_get_cached, browse_ls_cached, browse_registry_cached};

/// Default freshness window.
pub const DEFAULT_TTL_MS: u64 = 5_000;
/// Default hard age limit of a slot, whatever its etags say.
pub const DEFAULT_MAX_AGE_MS: u64 = 60_000;
/// Default slot cap.
pub const DEFAULT_MAX_ENTRIES: usize = 64;
/// Default cap on manifest JSON bytes held (the parsed form is a small multiple of this).
pub const DEFAULT_MAX_BYTES: u64 = 32 << 20;

/// A loaded worktree: its registry entry and parsed manifest. Never changed after a load, so a
/// reader always sees one entry with the one manifest it was loaded with.
#[derive(Debug)]
pub struct Loaded {
    pub entry: RegistryEntry,
    pub manifest: TreeManifest,
}

/// Where a returned [`Loaded`] came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Full load (entry, cross-check, manifest).
    Cold,
    /// Served inside the TTL, no backend call.
    Fresh,
    /// The TTL had lapsed; the entry and manifest etags were unchanged.
    Revalidated,
}

/// Counters, for tests and the bench.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CacheStats {
    pub cold: u64,
    pub fresh: u64,
    pub revalidated: u64,
    pub evicted: u64,
    pub invalidated: u64,
    /// Slots dropped for reaching `max_age`.
    pub expired: u64,
    /// Loads not stored because an invalidation ran while they were in flight.
    pub discarded: u64,
}

struct Slot {
    loaded: Arc<Loaded>,
    entry_etag: String,
    manifest_etag: String,
    bytes: u64,
    inserted_ms: u64,
    loaded_ms: u64,
    seq: u64,
}

type Key = (String, String);

/// A cold load's result, ready for [`ManifestCache::install`].
pub struct Fill {
    pub loaded: Arc<Loaded>,
    entry_etag: String,
    manifest_etag: String,
    bytes: u64,
}

/// The invalidation epoch a lookup saw; [`ManifestCache::install`] stores only if it is unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ticket(u64);

/// A slot whose TTL lapsed: the etags to confirm before serving it.
pub struct Probe {
    pub loaded: Arc<Loaded>,
    pub entry_etag: String,
    pub manifest_etag: String,
    seq: u64,
}

/// What [`ManifestCache::lookup`] found.
pub enum Lookup {
    Fresh(Arc<Loaded>),
    Stale(Probe, Ticket),
    Miss(Ticket),
}

pub struct ManifestCache {
    ttl_ms: u64,
    max_age_ms: u64,
    max_entries: usize,
    max_bytes: u64,
    slots: HashMap<Key, Slot>,
    bytes: u64,
    seq: u64,
    epoch: u64,
    /// Last seen digest content hash per project.
    pub(crate) digests: HashMap<String, String>,
    /// Registry entries by etag, for `browse_registry_cached`.
    pub(crate) regs: HashMap<Key, (String, RegistryEntry)>,
    pub stats: CacheStats,
}

impl Default for ManifestCache {
    fn default() -> Self {
        Self::new(DEFAULT_TTL_MS, DEFAULT_MAX_ENTRIES, DEFAULT_MAX_BYTES)
    }
}

impl ManifestCache {
    /// `max_age` is [`DEFAULT_MAX_AGE_MS`], or `ttl_ms` when that is longer; see [`Self::with_max_age`].
    pub fn new(ttl_ms: u64, max_entries: usize, max_bytes: u64) -> Self {
        Self {
            ttl_ms,
            max_age_ms: DEFAULT_MAX_AGE_MS.max(ttl_ms),
            max_entries: max_entries.max(1),
            max_bytes,
            slots: HashMap::new(),
            bytes: 0,
            seq: 0,
            epoch: 0,
            digests: HashMap::new(),
            regs: HashMap::new(),
            stats: CacheStats::default(),
        }
    }

    /// Set the hard age limit; the TTL is clamped to it.
    pub fn with_max_age(mut self, max_age_ms: u64) -> Self {
        self.max_age_ms = max_age_ms.max(1);
        self.ttl_ms = self.ttl_ms.min(self.max_age_ms);
        self
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }
    /// Manifest JSON bytes currently held.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    fn remove(&mut self, k: &Key) -> bool {
        match self.slots.remove(k) {
            Some(s) => {
                self.bytes -= s.bytes;
                true
            }
            None => false,
        }
    }

    /// Drop one slug's slot. Returns whether there was one. Loads in flight are not stored.
    pub fn invalidate(&mut self, project_bucket: &str, slug: &str) -> bool {
        self.epoch += 1;
        self.regs.remove(&(project_bucket.to_string(), slug.to_string()));
        let gone = self.remove(&(project_bucket.to_string(), slug.to_string()));
        self.stats.invalidated += u64::from(gone);
        gone
    }

    /// Drop every slot `keep` returns false for (by `(project bucket, slug)`). Returns how many.
    pub fn invalidate_where(&mut self, keep: impl Fn(&str, &str) -> bool) -> usize {
        self.epoch += 1;
        self.regs.retain(|k, _| keep(&k.0, &k.1));
        let keys: Vec<Key> = self.slots.keys().filter(|k| !keep(&k.0, &k.1)).cloned().collect();
        for k in &keys {
            self.remove(k);
        }
        self.stats.invalidated += keys.len() as u64;
        keys.len()
    }

    /// The project's `worktrees/_digest.json` has etag (or content hash) `digest_etag`. If it differs
    /// from the last one seen, every manifest slot of that project is dropped and `true`
    /// is returned. The first observation only records it.
    pub fn invalidate_if_digest_changed(&mut self, project_bucket: &str, digest_etag: &str) -> bool {
        let prev = self.digests.insert(project_bucket.to_string(), digest_etag.to_string());
        if prev.as_deref().is_none_or(|p| p == digest_etag) {
            return false;
        }
        self.epoch += 1;
        let keys: Vec<Key> = self.slots.keys().filter(|k| k.0 == project_bucket).cloned().collect();
        for k in &keys {
            self.remove(k);
        }
        self.stats.invalidated += keys.len() as u64;
        true
    }

    /// `current` is the project's `slug -> registry entry etag` (the digest, or a listing). Drops the
    /// slot of every slug that left the registry or whose entry etag moved. Returns how many.
    pub fn observe_registry(&mut self, project_bucket: &str, current: &BTreeMap<String, String>) -> usize {
        let stale: Vec<Key> = self
            .slots
            .iter()
            .filter(|(k, s)| k.0 == project_bucket && current.get(&k.1) != Some(&s.entry_etag))
            .map(|(k, _)| k.clone())
            .collect();
        if !stale.is_empty() {
            self.epoch += 1;
        }
        for k in &stale {
            self.remove(k);
        }
        self.stats.invalidated += stale.len() as u64;
        self.regs.retain(|k, (e, _)| k.0 != project_bucket || current.get(&k.1) == Some(e));
        stale.len()
    }

    /// The current invalidation epoch, for a load that did not come from [`Self::lookup`].
    pub fn ticket(&self) -> Ticket {
        Ticket(self.epoch)
    }

    /// The first half of a read: a fresh slot, a slot to revalidate, or nothing. A slot past
    /// `max_age` is dropped here and reported as a miss.
    pub fn lookup(&mut self, project_bucket: &str, slug: &str, now_ms: u64) -> Lookup {
        let key: Key = (project_bucket.to_string(), slug.to_string());
        let Some(s) = self.slots.get(&key) else { return Lookup::Miss(Ticket(self.epoch)) };
        if now_ms.saturating_sub(s.loaded_ms) >= self.max_age_ms {
            self.remove(&key);
            self.stats.expired += 1;
            return Lookup::Miss(Ticket(self.epoch));
        }
        if now_ms.saturating_sub(s.inserted_ms) < self.ttl_ms {
            self.stats.fresh += 1;
            return Lookup::Fresh(s.loaded.clone());
        }
        let p = Probe { loaded: s.loaded.clone(), entry_etag: s.entry_etag.clone(), manifest_etag: s.manifest_etag.clone(), seq: s.seq };
        Lookup::Stale(p, Ticket(self.epoch))
    }

    /// The probed slot's etags were confirmed: restart its TTL (if it is still the same slot).
    pub fn confirm(&mut self, project_bucket: &str, slug: &str, p: &Probe, now_ms: u64) {
        self.stats.revalidated += 1;
        if let Some(s) = self.slots.get_mut(&(project_bucket.to_string(), slug.to_string())).filter(|s| s.seq == p.seq) {
            s.inserted_ms = now_ms;
        }
    }

    /// The probed slot is out of date: drop it (if it is still the same slot).
    pub fn reject(&mut self, project_bucket: &str, slug: &str, p: &Probe) {
        let key: Key = (project_bucket.to_string(), slug.to_string());
        if self.slots.get(&key).is_some_and(|s| s.seq == p.seq) {
            self.epoch += 1;
            self.remove(&key);
            self.stats.invalidated += 1;
        }
    }

    /// Store a cold load, unless an invalidation ran since `t` was taken. Counts the load as cold.
    pub fn install(&mut self, project_bucket: &str, slug: &str, f: &Fill, t: Ticket, now_ms: u64) {
        self.stats.cold += 1;
        if t.0 != self.epoch {
            self.stats.discarded += 1;
            return;
        }
        if f.bytes > self.max_bytes {
            return; // served, not stored
        }
        let key: Key = (project_bucket.to_string(), slug.to_string());
        if self.slots.contains_key(&key) {
            self.remove(&key);
        }
        while !self.slots.is_empty() && (self.slots.len() >= self.max_entries || self.bytes + f.bytes > self.max_bytes) {
            let oldest = self.slots.iter().min_by_key(|(_, s)| (s.inserted_ms, s.seq)).map(|(k, _)| k.clone());
            let Some(k) = oldest else { break };
            self.remove(&k);
            self.stats.evicted += 1;
        }
        self.seq += 1;
        self.bytes += f.bytes;
        let slot = Slot {
            loaded: f.loaded.clone(),
            entry_etag: f.entry_etag.clone(),
            manifest_etag: f.manifest_etag.clone(),
            bytes: f.bytes,
            inserted_ms: now_ms,
            loaded_ms: now_ms,
            seq: self.seq,
        };
        self.slots.insert(key, slot);
    }

    /// The entry and manifest for `slug`: from the slot when fresh or still current, else loaded
    /// (with the `.wt/meta.json` cross-check). The module docs say exactly what each path reads.
    pub fn get_or_load<B: ObjectBackend>(&mut self, b: &B, project_bucket: &str, slug: &str, now_ms: u64) -> Result<(Arc<Loaded>, Source)> {
        self.get_or_load_known(b, project_bucket, slug, None, now_ms)
    }

    /// [`Self::get_or_load`] for a caller that has just listed the registry: `known` is the entry it
    /// read and the etag the listing gave, which replaces the entry `head` and `get`.
    pub fn get_or_load_known<B: ObjectBackend>(
        &mut self,
        b: &B,
        project_bucket: &str,
        slug: &str,
        known: Option<(&RegistryEntry, &str)>,
        now_ms: u64,
    ) -> Result<(Arc<Loaded>, Source)> {
        if !crate::follower_browse::slug_ok(slug) {
            return Err(S2Error::Invalid(format!("bad worktree slug {slug:?}")));
        }
        let t = match self.lookup(project_bucket, slug, now_ms) {
            Lookup::Fresh(l) => return Ok((l, Source::Fresh)),
            Lookup::Miss(t) => t,
            Lookup::Stale(p, _) => {
                if still_current(b, project_bucket, slug, &p, known.map(|k| k.1))? {
                    self.confirm(project_bucket, slug, &p, now_ms);
                    return Ok((p.loaded, Source::Revalidated));
                }
                self.reject(project_bucket, slug, &p);
                Ticket(self.epoch)
            }
        };
        let f = load_slot(b, project_bucket, slug, known)?;
        self.install(project_bucket, slug, &f, t, now_ms);
        Ok((f.loaded, Source::Cold))
    }
}

/// Whether a probed slot is still current: the entry etag (from `listed` when the caller has just
/// listed the registry, else one `head`) and one `head` of the manifest. An error is an error,
/// never "serve the old one".
pub fn still_current<B: ObjectBackend>(b: &B, project_bucket: &str, slug: &str, p: &Probe, listed: Option<&str>) -> Result<bool> {
    let entry_etag = match listed {
        Some(e) => Some(e.to_string()),
        None => b.head(project_bucket, &format!("{REGISTRY_PREFIX}{slug}.json"))?.map(|h| h.etag),
    };
    if entry_etag.as_deref() != Some(p.entry_etag.as_str()) {
        return Ok(false);
    }
    let head = b.head(&p.loaded.entry.bucket, MANIFEST_KEY)?;
    Ok(head.is_some_and(|h| h.etag == p.manifest_etag))
}

/// A full cold load of one worktree: entry `head` + `get` (or `known`, from a listing just made),
/// the mandatory `.wt/meta.json` cross-check, manifest `head` + `get`. No lock, no cache state.
pub fn load_slot<B: ObjectBackend>(b: &B, project_bucket: &str, slug: &str, known: Option<(&RegistryEntry, &str)>) -> Result<Fill> {
    let no_entry = || S2Error::Invalid(format!("no readable registry entry for worktree {slug}"));
    let (entry, entry_etag) = match known {
        Some((e, etag)) if e.slug == slug => (e.clone(), etag.to_string()),
        Some(_) => return Err(no_entry()),
        None => {
            let ekey = format!("{REGISTRY_PREFIX}{slug}.json");
            // The etag is read before the body: if the entry moves in between, the stored etag is
            // the older one and the next observation drops the slot. Conservative, never stale.
            let entry_etag = b.head(project_bucket, &ekey)?.ok_or_else(no_entry)?.etag;
            let raw = b.get(project_bucket, &ekey)?;
            (check_entry(project_bucket, slug, raw.as_deref()).ok_or_else(no_entry)?, entry_etag)
        }
    };
    verify_entry(b, &entry)?; // mandatory on every cold load
    let no_tree = || S2Error::Invalid(format!("worktree {slug} has no shared tree"));
    let h = b.head(&entry.bucket, MANIFEST_KEY)?.ok_or_else(no_tree)?;
    if h.size > MAX_MANIFEST_BYTES {
        return Err(S2Error::Invalid(format!("manifest is {} bytes, over the {MAX_MANIFEST_BYTES} limit", h.size)));
    }
    let data = b.get(&entry.bucket, MANIFEST_KEY)?.ok_or_else(no_tree)?;
    let manifest = parse_manifest(&data)?;
    Ok(Fill { loaded: Arc::new(Loaded { entry, manifest }), entry_etag, manifest_etag: h.etag, bytes: data.len() as u64 })
}
