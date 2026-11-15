//! The browse reads through a [`ManifestCache`] held by the caller (`&mut`, single owner). Split out
//! of `follower_cache` and re-exported there, so paths are unchanged. A cache shared by threads
//! goes through `follower_cache_shared` instead, which never holds its lock across a backend call.

use crate::follower_browse::{BrowseFile, BrowseListing, RegistryBrowse, clean_path, get_from, ls_from, summary};
use crate::follower_cache::{ManifestCache, Source};
use crate::overlay::{ObjectBackend, Result};
use crate::share::sha256_hex;
use crate::share_fetch::{DIGEST_KEY, Digest, REGISTRY_PREFIX, check_entry, list_shared_lenient};

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
/// tree object is always read live. Inside the TTL after a republish that is the object the cached
/// manifest names, so the previous version's bytes (see `follower_cache.rs`). If the read fails (a
/// schema-1 object overwritten or gone), the slot is dropped and the read is retried once cold.
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
/// drops manifest slots whose entry moved or left. The daemon does not use this (it lists: see
/// `follower_cache_shared`); it is kept for library callers.
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
