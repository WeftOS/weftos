//! Short-lived manifest cache for the follower/browse path (WO-069 lane F, prototype; ADR-126 4a, Q14).
//!
//! Every uncached `browse_ls` / `browse_get` re-reads the registry entry, the bucket's
//! `.wt/meta.json` and the manifest (up to 32 MiB). A dashboard folder click does that per click.
//! [`ManifestCache`] keeps, per `(project bucket, slug)`, the parsed manifest with everything that
//! proves it is still the right one.
//!
//! What a cached slot holds: the registry entry and its etag, the manifest etag (from a `head`),
//! the parsed manifest, and `inserted_ms`. There is no clock in here: every call takes `now_ms`.
//!
//! When a slot is served:
//! - **Within the TTL** it is served with no backend call at all. This also skips the
//!   `.wt/meta.json` cross-check. That is safe because the cross-check's inputs are the registry
//!   entry's `(slug, bucket)` and the bucket's `meta.json`, and a slot is only ever created by a
//!   load that ran the cross-check on exactly that entry. The slot is dropped when the entry's etag
//!   moves ([`ManifestCache::observe_registry`], fed by the digest or a listing) and, at the latest,
//!   when the TTL lapses and the manifest etag has moved. A slot can therefore only be wrong in the
//!   way a poll is wrong: by up to one TTL, for fields that are not authority anyway (state, head).
//! - **After the TTL** one `head` of the manifest key decides. Same etag: the slot is kept and its
//!   age restarts. Different etag or missing: the slot is dropped and a full cold load follows,
//!   which runs the cross-check again. A republish never serves the old manifest past the TTL.
//! - **Cold, or after any invalidation** the load is: `head` + `get` of the registry entry, the
//!   mandatory `.wt/meta.json` cross-check ([`verify_entry`]), then `head` + `get` of the manifest.
//!
//! `browse_get_cached` also fails closed on the tree object: if the object is missing or its
//! SHA-256 no longer matches the cached manifest, the slot is dropped and the read is retried once
//! from a cold load, so a republish inside the TTL gives the new bytes, never an error or old bytes.
//!
//! Bounded: at most `max_entries` slots and `max_bytes` of manifest JSON; the oldest slot (by
//! `inserted_ms`, then insertion order) is evicted first. A manifest bigger than `max_bytes` alone
//! is served but not stored.

use crate::follower::verify_entry;
use crate::follower_browse::{
    BrowseFile, BrowseListing, RegistryBrowse, clean_path, get_from, ls_from, slug_ok, summary,
};
use crate::overlay::{ObjectBackend, Result, S2Error};
use crate::share::{MANIFEST_KEY, MAX_MANIFEST_BYTES, TreeManifest, parse_manifest, sha256_hex};
use crate::share_fetch::{DIGEST_KEY, Digest, REGISTRY_PREFIX, RegistryEntry, check_entry, list_shared_lenient};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

/// Default freshness window.
pub const DEFAULT_TTL_MS: u64 = 5_000;
/// Default slot cap.
pub const DEFAULT_MAX_ENTRIES: usize = 64;
/// Default cap on manifest JSON bytes held (the parsed form is a small multiple of this).
pub const DEFAULT_MAX_BYTES: u64 = 32 << 20;

/// A loaded worktree: its registry entry and parsed manifest.
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
    /// The TTL had lapsed; one `head` showed the manifest unchanged.
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
}

struct Slot {
    loaded: Arc<Loaded>,
    entry_etag: String,
    manifest_etag: String,
    bytes: u64,
    inserted_ms: u64,
    seq: u64,
}

type Key = (String, String);

pub struct ManifestCache {
    ttl_ms: u64,
    max_entries: usize,
    max_bytes: u64,
    slots: HashMap<Key, Slot>,
    bytes: u64,
    seq: u64,
    /// Last seen digest content hash per project.
    digests: HashMap<String, String>,
    /// Registry entries by etag, for `browse_registry_cached`.
    regs: HashMap<Key, (String, RegistryEntry)>,
    pub stats: CacheStats,
}

impl Default for ManifestCache {
    fn default() -> Self {
        Self::new(DEFAULT_TTL_MS, DEFAULT_MAX_ENTRIES, DEFAULT_MAX_BYTES)
    }
}

impl ManifestCache {
    pub fn new(ttl_ms: u64, max_entries: usize, max_bytes: u64) -> Self {
        Self {
            ttl_ms,
            max_entries: max_entries.max(1),
            max_bytes,
            slots: HashMap::new(),
            bytes: 0,
            seq: 0,
            digests: HashMap::new(),
            regs: HashMap::new(),
            stats: CacheStats::default(),
        }
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

    /// Drop one slug's slot. Returns whether there was one.
    pub fn invalidate(&mut self, project_bucket: &str, slug: &str) -> bool {
        let gone = self.remove(&(project_bucket.to_string(), slug.to_string()));
        self.stats.invalidated += u64::from(gone);
        gone
    }

    /// The project's `worktrees/_digest.json` has etag (or content hash) `digest_etag`. If it differs
    /// from the last one seen, every manifest slot of that project is dropped and `true`
    /// is returned. The first observation only records it.
    pub fn invalidate_if_digest_changed(&mut self, project_bucket: &str, digest_etag: &str) -> bool {
        let prev = self.digests.insert(project_bucket.to_string(), digest_etag.to_string());
        if prev.as_deref().is_none_or(|p| p == digest_etag) {
            return false;
        }
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
        for k in &stale {
            self.remove(k);
        }
        self.stats.invalidated += stale.len() as u64;
        self.regs.retain(|k, (e, _)| k.0 != project_bucket || current.get(&k.1) == Some(e));
        stale.len()
    }

    /// The entry and manifest for `slug`: from the slot when fresh or still current, else loaded
    /// (with the `.wt/meta.json` cross-check). The module docs say exactly what each path reads.
    pub fn get_or_load<B: ObjectBackend>(
        &mut self,
        b: &B,
        project_bucket: &str,
        slug: &str,
        now_ms: u64,
    ) -> Result<(Arc<Loaded>, Source)> {
        if !slug_ok(slug) {
            return Err(S2Error::Invalid(format!("bad worktree slug {slug:?}")));
        }
        let key: Key = (project_bucket.to_string(), slug.to_string());
        if let Some(s) = self.slots.get(&key) {
            if now_ms.saturating_sub(s.inserted_ms) < self.ttl_ms {
                self.stats.fresh += 1;
                return Ok((s.loaded.clone(), Source::Fresh));
            }
            // One head of the manifest decides; an error is an error, never "serve the old one".
            let head = b.head(&s.loaded.entry.bucket, MANIFEST_KEY)?;
            if head.as_ref().is_some_and(|h| h.etag == s.manifest_etag) {
                let s = self.slots.get_mut(&key).expect("slot present");
                s.inserted_ms = now_ms;
                self.stats.revalidated += 1;
                return Ok((s.loaded.clone(), Source::Revalidated));
            }
            self.remove(&key);
            self.stats.invalidated += 1;
        }
        let slot = self.load(b, project_bucket, slug, now_ms)?;
        let loaded = slot.loaded.clone();
        self.stats.cold += 1;
        self.insert(key, slot);
        Ok((loaded, Source::Cold))
    }

    fn load<B: ObjectBackend>(&mut self, b: &B, project_bucket: &str, slug: &str, now_ms: u64) -> Result<Slot> {
        let no_entry = || S2Error::Invalid(format!("no readable registry entry for worktree {slug}"));
        let ekey = format!("{REGISTRY_PREFIX}{slug}.json");
        // The etag is read before the body: if the entry moves in between, the stored etag is the
        // older one and the next observation drops the slot. Conservative, never stale.
        let entry_etag = b.head(project_bucket, &ekey)?.ok_or_else(no_entry)?.etag;
        let raw = b.get(project_bucket, &ekey)?;
        let entry = check_entry(project_bucket, slug, raw.as_deref()).ok_or_else(no_entry)?;
        verify_entry(b, &entry)?; // mandatory on every cold load
        let no_tree = || S2Error::Invalid(format!("worktree {slug} has no shared tree"));
        let h = b.head(&entry.bucket, MANIFEST_KEY)?.ok_or_else(no_tree)?;
        if h.size > MAX_MANIFEST_BYTES {
            return Err(S2Error::Invalid(format!("manifest is {} bytes, over the {MAX_MANIFEST_BYTES} limit", h.size)));
        }
        let data = b.get(&entry.bucket, MANIFEST_KEY)?.ok_or_else(no_tree)?;
        let manifest = parse_manifest(&data)?;
        self.seq += 1;
        Ok(Slot {
            loaded: Arc::new(Loaded { entry, manifest }),
            entry_etag,
            manifest_etag: h.etag,
            bytes: data.len() as u64,
            inserted_ms: now_ms,
            seq: self.seq,
        })
    }

    fn insert(&mut self, key: Key, slot: Slot) {
        if slot.bytes > self.max_bytes {
            return; // served, not stored
        }
        while !self.slots.is_empty()
            && (self.slots.len() >= self.max_entries || self.bytes + slot.bytes > self.max_bytes)
        {
            let oldest = self.slots.iter().min_by_key(|(_, s)| (s.inserted_ms, s.seq)).map(|(k, _)| k.clone());
            let Some(k) = oldest else { break };
            self.remove(&k);
            self.stats.evicted += 1;
        }
        self.bytes += slot.bytes;
        self.slots.insert(key, slot);
    }
}

/// [`crate::follower_browse::browse_ls`] through the cache.
pub fn browse_ls_cached<B: ObjectBackend>(
    b: &B,
    cache: &mut ManifestCache,
    now_ms: u64,
    project_bucket: &str,
    slug: &str,
    prefix: &str,
) -> Result<BrowseListing> {
    let dir = clean_path(prefix, true)?;
    let (l, _) = cache.get_or_load(b, project_bucket, slug, now_ms)?;
    ls_from(&l.entry, &l.manifest, dir)
}

/// [`crate::follower_browse::browse_get`] through the cache. The manifest comes from the cache; the
/// tree object is always read live. If a cached manifest no longer matches the object (a republish
/// inside the TTL), the slot is dropped and the read is retried once from a cold load.
pub fn browse_get_cached<B: ObjectBackend>(
    b: &B,
    cache: &mut ManifestCache,
    now_ms: u64,
    project_bucket: &str,
    slug: &str,
    path: &str,
    max_bytes: u64,
) -> Result<BrowseFile> {
    let path = clean_path(path, false)?;
    let (l, src) = cache.get_or_load(b, project_bucket, slug, now_ms)?;
    match get_from(b, &l.entry, &l.manifest, slug, &path, max_bytes) {
        Err(_) if src != Source::Cold => {
            cache.invalidate(project_bucket, slug);
            let (l, _) = cache.get_or_load(b, project_bucket, slug, now_ms)?;
            get_from(b, &l.entry, &l.manifest, slug, &path, max_bytes)
        }
        r => r,
    }
}

/// [`crate::follower_browse::browse_registry`] from the digest: one `get` of `worktrees/_digest.json`,
/// plus one `get` per entry whose etag differs from the cached one. With no usable digest it falls
/// back to the plain listing. The digest is a hint written by the daemon after each registry change,
/// so a list built from it can lag a lost digest write (the same property `poll_fast` has). It also
/// drops manifest slots whose entry moved or left.
pub fn browse_registry_cached<B: ObjectBackend>(
    b: &B,
    cache: &mut ManifestCache,
    project_bucket: &str,
) -> Result<RegistryBrowse> {
    let raw = b.get(project_bucket, DIGEST_KEY)?;
    let digest = raw
        .as_deref()
        .and_then(|d| serde_json::from_slice::<Digest>(d).ok())
        .filter(|d| d.schema == 1);
    let (Some(d), Some(raw)) = (digest, raw) else {
        let (entries, bad) = list_shared_lenient(b, project_bucket)?;
        return Ok(RegistryBrowse { worktrees: entries.iter().map(summary).collect(), unreadable: bad });
    };
    // Per-slug etags are precise, so this does not use the coarse whole-project digest check; the
    // digest's own hash is only recorded so a caller can ask `invalidate_if_digest_changed` too.
    cache.digests.insert(project_bucket.to_string(), sha256_hex(&raw));
    cache.observe_registry(project_bucket, &d.entries);
    let (mut ok, mut bad) = (Vec::new(), Vec::new());
    for (slug, etag) in &d.entries {
        let k = (project_bucket.to_string(), slug.clone());
        if let Some((_, entry)) = cache.regs.get(&k).filter(|(e, _)| e == etag) {
            ok.push(entry.clone());
            continue;
        }
        let body = b.get(project_bucket, &format!("{REGISTRY_PREFIX}{slug}.json"))?;
        match check_entry(project_bucket, slug, body.as_deref()) {
            Some(entry) => {
                cache.regs.insert(k, (etag.clone(), entry.clone()));
                ok.push(entry);
            }
            None => bad.push(slug.clone()),
        }
    }
    cache.regs.retain(|k, _| k.0 != project_bucket || d.entries.contains_key(&k.1));
    ok.sort_by(|a, c| c.updated_ms.cmp(&a.updated_ms).then_with(|| a.slug.cmp(&c.slug)));
    bad.sort();
    Ok(RegistryBrowse { worktrees: ok.iter().map(summary).collect(), unreadable: bad })
}
