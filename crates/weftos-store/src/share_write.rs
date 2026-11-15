//! `publish_tree`: the writing half of `share` (WO-069 lane D3 restructured it; the steps are in
//! `reports/wo069-content-addressed-keys-proposal-20261115.md` section 2.2). A child of `share`.
//!
//! 1. Guard on the handoff record; read the manifest (the base) with its etag; one listing.
//! 2. Upload each content not already in the pool, to a fresh key (`share_gc.rs` decides).
//! 3. Build the manifest with its retiring list; announce it in the handoff record (lane Y).
//! 4. Write it with `put_cas` against the base's etag: a publish that lost a race writes nothing.
//! 5. Post-check the handoff record (design note B1): merged meanwhile → restore the base manifest.
//! 6. Sweep what is past its grace.
//!
//! A schema-1 writer (the rollback lever) still writes `.wt/tree/<path>` and removes stale tree
//! objects at once, as before, but also writes conditionally, post-checks, and sweeps the pool.

use super::gc::{Chooser, Pool, PublishOptions, plan, references};
use super::hashcache::HashCacheRun;
use super::*;
use crate::overlay::ObjectMeta;

/// Publish (or refresh) the worktree tree into its bucket with the default [`PublishOptions`]
/// (schema 2, 24 h grace, sweep on).
pub fn publish_tree<B: ObjectBackend>(
    b: &B,
    bucket: &str,
    src: &TreeSource<'_>,
    limits: &Limits,
    now_ms: u64,
) -> Result<PublishReport> {
    Ok(publish_tree_opts(b, bucket, src, limits, now_ms, None, &PublishOptions::default())?.report)
}

/// `publish_tree` with the bucket listing supplied by the caller: the WHOLE bucket
/// (`list(bucket, "")`), taken before any PUT. `None` lists it here, one call.
pub fn publish_tree_listed<B: ObjectBackend>(
    b: &B,
    bucket: &str,
    src: &TreeSource<'_>,
    limits: &Limits,
    now_ms: u64,
    listing: Option<Vec<ObjectMeta>>,
) -> Result<Published> {
    publish_tree_opts(b, bucket, src, limits, now_ms, listing, &PublishOptions::default())
}

/// The full publish, with explicit options (the daemon passes its environment switches here).
pub fn publish_tree_opts<B: ObjectBackend>(
    b: &B,
    bucket: &str,
    src: &TreeSource<'_>,
    limits: &Limits,
    now_ms: u64,
    listing: Option<Vec<ObjectMeta>>,
    opts: &PublishOptions,
) -> Result<Published> {
    publish_tree_cached(b, bucket, src, limits, now_ms, listing, opts, None)
}

/// [`publish_tree_opts`] with the local hash cache (`share_hashcache.rs`): a file whose size, mtime,
/// inode and ctime match its cache entry is not read. `None` publishes exactly as without a cache.
#[allow(clippy::too_many_arguments)]
pub fn publish_tree_cached<B: ObjectBackend>(
    b: &B,
    bucket: &str,
    src: &TreeSource<'_>,
    limits: &Limits,
    now_ms: u64,
    listing: Option<Vec<ObjectMeta>>,
    opts: &PublishOptions,
    hash_cache: Option<&HashCacheConfig>,
) -> Result<Published> {
    if opts.schema != SCHEMA_V1 && opts.schema != SCHEMA_V2 {
        return Err(S2Error::Invalid(format!("cannot write manifest schema {}; this build writes 1 or 2", opts.schema)));
    }
    let pooled = opts.schema == SCHEMA_V2;
    // Read with its etag: the announcement before the manifest PUT is a conditional write on this
    // version, so a record unchanged since needs no second read (WO-069 lane Y, A4).
    let existing_handoff = read_handoff_versioned(b, bucket)?;
    if let Some((h, _)) = &existing_handoff {
        if matches!(h.state, State::Merged | State::Archived) {
            return Err(S2Error::Invalid(format!("worktree is already {:?}; refusing to republish", h.state)));
        }
        guard_live_claim(h, &src.writer, now_ms)?;
    }
    if let Some(bp) = src.bundle {
        check_bundle_len(std::fs::metadata(bp)?.len(), limits)?;
    }
    let (prev, prev_bytes, prev_etag) = match read_manifest_versioned(b, bucket, limits)? {
        Some((m, d, e)) => (Some(m), Some(d), Some(e)),
        None => (None, None, None),
    };
    let pool = Pool::from_listing(match listing {
        Some(l) => l,
        None => b.list(bucket, "")?,
    });
    let mut cache = HashCacheRun::open(hash_cache, src.root, now_ms);
    // A cached entry whose bytes turn out to differ at upload time voids the cache: the whole pass
    // runs again with it ignored (objects the first attempt put stay in the pool, same keys).
    let (mut chooser, mut rep, files, docs) = loop {
        let mut chooser = Chooser::new(prev.as_ref(), &pool, opts, now_ms);
        let mut rep = PublishReport::default();
        let done = upload_files(b, bucket, src, limits, prev.as_ref(), &pool, pooled.then_some(&mut chooser), &mut rep, &mut cache)?;
        match done {
            Some((files, docs)) => break (chooser, rep, files, docs),
            None => cache.force_full("stale-entry"),
        }
    };
    rep.hash_cache = cache.finish();
    let (has_bundle, bundle_sha256, bundle_gen) = if pooled {
        match pooled_bundle(b, bucket, src, limits, prev.as_ref(), &pool, &mut chooser, &mut rep)? {
            Some((s, g)) => (true, Some(s), Some(g)),
            None => (false, None, None),
        }
    } else {
        let (h, s) = path_bundle(b, bucket, src, limits, prev.as_ref(), &pool, opts, &mut rep)?;
        (h, s, None)
    };
    // A set, not a scan of `files` per path: with 100 000 files and many uncommitted paths the scan
    // is quadratic.
    let uncommitted = {
        let published: std::collections::HashSet<&str> = files.iter().map(|f| f.path.as_str()).collect();
        src.uncommitted.iter().filter(|u| published.contains(u.as_str())).cloned().collect()
    };
    let mut manifest = TreeManifest {
        schema: opts.schema,
        generation: chooser.fresh,
        branch: src.branch.into(),
        base_commit: src.base_commit.into(),
        head_commit: src.head_commit.into(),
        files,
        uncommitted,
        changes: src.changes.to_vec(),
        changes_recorded: src.changes_recorded || !src.changes.is_empty(),
        docs,
        has_bundle,
        bundle_sha256,
        bundle_gen,
        retiring: Vec::new(),
        v1_retired_ms: None,
        writer: src.writer.clone(),
        published_ms: now_ms,
    };
    let refs = references(&manifest);
    let gc = plan(prev.as_ref(), &refs, pooled, &pool, opts, limits, now_ms)?;
    manifest.retiring = gc.retiring;
    manifest.v1_retired_ms = gc.v1_retired_ms;
    let bytes = serde_json::to_vec_pretty(&manifest)?;
    check_manifest_len(bytes.len() as u64, manifest.files.len())?;
    // Announce the manifest in the handoff record, conditionally, right before writing it: a fenced
    // merge that read the record before this write fails its own conditional write, one that reads
    // it after refuses until the manifest below is the one announced (lane Y, A4). This also creates
    // the record on a first publish, before the manifest, and refuses (nothing written) when the
    // worktree was merged or claimed live by someone else while the files uploaded.
    let mine = sha256_hex(&bytes);
    let announced = announce(
        b,
        bucket,
        &src.writer,
        now_ms,
        existing_handoff,
        prev_bytes.as_deref().map(sha256_hex),
        PublishIntent {
            manifest_sha256: mine.clone(),
            head: manifest.head_commit.clone(),
            tree: tree_digest(&manifest),
            ms: now_ms,
            writer: src.writer.clone(),
            held_ms: None,
            done: false,
        },
    )?;
    // Conditional on the base this publish read: another publish that landed since makes this one
    // write nothing (no lost update, and its sweep below never runs against a manifest it did not
    // write). Refused, not retried: the key choices, generation and retiring list all depend on the
    // base, so a retry is a whole new publish, which the caller (or the next commit) runs; this
    // publish's uploads stay in the pool for it to re-use.
    let written = b.put_cas(bucket, MANIFEST_KEY, &bytes, prev_etag.as_deref());
    if !matches!(written, Ok(true)) {
        let e = match written {
            Err(e) => e,
            _ => S2Error::Backend(
                "the manifest changed while this publish ran (another publish of this worktree landed first); \
                 nothing was written or swept; publish again"
                    .into(),
            ),
        };
        // Withdraw the announcement (lane Y, A5), else merges wait for it until it expires.
        return Err(match withdraw(b, bucket, &mine, announced.previous) {
            Ok(()) => e,
            Err(w) => S2Error::Backend(format!(
                "{e}; the publish announcement could not be withdrawn either ({w}), so merges of this \
                 worktree wait until it expires ({} min) or the next publish",
                CLAIM_TTL_MS / 60_000
            )),
        });
    }
    let settled = match settle(b, bucket, &mine)? {
        Settled::Kept(h) => h,
        Settled::Finalised(state) => {
            let restored = restore_base(b, bucket, &bytes, prev_bytes.as_deref())?;
            return Err(S2Error::Invalid(format!(
                "worktree became {state:?} while this publish ran; {}; nothing was swept",
                if restored { "the previous manifest was restored" } else { "the manifest could not be restored (it changed again, or there was none before)" }
            )));
        }
    };
    let waiting = manifest.retiring.len().saturating_sub(gc.doomed.len());
    if opts.sweep {
        let mut gone: Vec<String> = gc.doomed;
        gone.extend(gc.v1_doomed);
        if !pooled {
            // Schema 1, as before the pool: a tree object leaves as soon as the manifest stops naming it.
            let kept: std::collections::HashSet<String> = manifest.files.iter().map(|f| manifest.object_key(f)).collect();
            gone.extend(pool.tree.keys().filter(|k| !kept.contains(*k)).cloned());
        }
        for key in gone {
            // Never a key the manifest just written names, whatever the plan says.
            if parse_obj_key(&key).is_some_and(|k| refs.contains_key(&k)) {
                continue;
            }
            b.delete(bucket, &key)?;
            rep.removed += 1;
        }
    }
    rep.retired = gc.added;
    rep.deferred = if opts.sweep { waiting } else { manifest.retiring.len() };
    // The record was created, if none existed, by the announcement above (and its announcement
    // cleared by the post-check: `settled` is what is stored).
    let handoff_created = announced.created.and(settled.map(|h| *h));
    Ok(Published { report: rep, manifest, handoff_created })
}

fn check_bundle_len(len: u64, limits: &Limits) -> Result<()> {
    if len > limits.max_bundle {
        return Err(S2Error::Invalid(format!("bundle is {len} bytes, over the {} limit", limits.max_bundle)));
    }
    Ok(())
}

/// Read, check and upload every path; returns the file and doc lists. `chooser` is `Some` for a
/// schema-2 publish (objects in the pool), `None` for schema 1 (`.wt/tree/<path>`).
#[allow(clippy::too_many_arguments)]
fn upload_files<B: ObjectBackend>(
    b: &B,
    bucket: &str,
    src: &TreeSource<'_>,
    limits: &Limits,
    prev: Option<&TreeManifest>,
    pool: &Pool,
    mut chooser: Option<&mut Chooser<'_>>,
    rep: &mut PublishReport,
    cache: &mut HashCacheRun,
) -> Result<Option<(Vec<FileEntry>, Vec<DocEntry>)>> {
    cache.sample_check(src.root, src.paths, limits.max_file);
    let prev_sha: std::collections::HashMap<&str, &str> =
        prev.map(|m| m.files.iter().map(|f| (f.path.as_str(), f.sha256.as_str())).collect()).unwrap_or_default();
    let (mut files, mut docs, mut total) = (Vec::new(), Vec::new(), 0u64);
    let mut seen = std::collections::BTreeSet::new();
    for p in src.paths {
        if validate_key(p).is_err() || p.starts_with(".wt/") || !seen.insert(p.clone()) {
            rep.skipped.push((p.clone(), Skip::BadPath));
            continue;
        }
        if is_secret_path(p) {
            rep.skipped.push((p.clone(), Skip::Secret));
            continue;
        }
        if is_dangerous_path(p) {
            rep.skipped.push((p.clone(), Skip::BadPath));
            continue;
        }
        let Ok(full) = safe_join(src.root, p) else {
            rep.skipped.push((p.clone(), Skip::Symlink));
            continue;
        };
        let Ok(md) = std::fs::symlink_metadata(&full) else {
            rep.skipped.push((p.clone(), Skip::Missing));
            continue;
        };
        let skip = if md.file_type().is_symlink() {
            Some(Skip::Symlink)
        } else if !md.is_file() {
            Some(Skip::NotAFile)
        } else if md.len() > limits.max_file {
            Some(Skip::TooLarge)
        } else if files.len() >= limits.max_files || total + md.len() > limits.max_total {
            Some(Skip::OverBudget)
        } else {
            None
        };
        if let Some(s) = skip {
            rep.skipped.push((p.clone(), s));
            continue;
        }
        let stamp = HashCacheRun::stamp_of(&md);
        let hit = stamp.as_ref().and_then(|st| cache.lookup(p, st)).cloned();
        // Either the cache entry stands in for the read, or the file is read and checked as always.
        let (sha, len, title, mut data) = match hit {
            Some(e) => {
                if total + e.stamp.size > limits.max_total {
                    rep.skipped.push((p.clone(), Skip::OverBudget));
                    continue;
                }
                cache.record_served(p, &e);
                (e.sha256, e.stamp.size, e.title, None)
            }
            None => {
                let data = match read_regular(&full, limits.max_file) {
                    Ok(d) => d,
                    Err(Some(skip)) => {
                        rep.skipped.push((p.clone(), skip));
                        continue;
                    }
                    Err(None) => return Err(S2Error::Backend(format!("cannot read {p}"))),
                };
                cache.note_read(data.len() as u64);
                if total + data.len() as u64 > limits.max_total {
                    rep.skipped.push((p.clone(), Skip::OverBudget));
                    continue;
                }
                if looks_like_private_key(&data) {
                    rep.skipped.push((p.clone(), Skip::Secret));
                    continue;
                }
                let (sha, title) = (sha256_hex(&data), is_doc_path(p).then(|| md_title(p, &data)).flatten());
                if let Some(st) = stamp {
                    cache.record_hashed(p, st, &sha, title.clone());
                }
                (sha, data.len() as u64, title, Some(data))
            }
        };
        let (generation, put) = match chooser.as_deref_mut() {
            Some(c) => {
                let (g, put) = c.choose(&sha, len);
                (Some(g), put)
            }
            None => {
                let key = format!("{TREE_PREFIX}{p}");
                (None, !(prev_sha.get(p.as_str()) == Some(&sha.as_str()) && pool.tree.get(&key) == Some(&len)))
            }
        };
        if put {
            if data.is_none() {
                // A cached file the store lacks: read it now and require the bytes to be the cached ones.
                data = read_regular(&full, limits.max_file).ok().inspect(|d| cache.note_read(d.len() as u64)).filter(|d| sha256_hex(d) == sha);
            }
            let Some(d) = data else { return Ok(None) };
            let key = match generation {
                Some(g) => obj_key(&sha, g),
                None => format!("{TREE_PREFIX}{p}"),
            };
            b.put(bucket, &key, &d)?;
            rep.uploaded += 1;
        } else {
            rep.unchanged += 1;
        }
        total += len;
        if is_doc_path(p) {
            docs.push(DocEntry { path: p.clone(), title });
        }
        files.push(FileEntry { path: p.clone(), size: len, sha256: sha, generation });
    }
    // Files served from the cache must still be what the entry says now that the tree is processed;
    // one that moved is `Changed`, exactly as a swap between check and read is without the cache.
    let changed = cache.changed_since(src.root);
    if !changed.is_empty() {
        files.retain(|f| !changed.contains(&f.path));
        docs.retain(|d| !changed.contains(&d.path));
        rep.skipped.extend(changed.into_iter().map(|p| (p, Skip::Changed)));
    }
    Ok(Some((files, docs)))
}

/// Schema 2: the bundle object the new manifest names. The base's, when the head did not move and
/// the pool still has it; else the supplied bundle file, in the pool (`Chooser`); else, moving a
/// schema-1 tree into the pool without a bundle file, a server-side copy of the old bundle key to
/// a fresh key.
#[allow(clippy::too_many_arguments)]
fn pooled_bundle<B: ObjectBackend>(
    b: &B,
    bucket: &str,
    src: &TreeSource<'_>,
    limits: &Limits,
    prev: Option<&TreeManifest>,
    pool: &Pool,
    chooser: &mut Chooser<'_>,
    rep: &mut PublishReport,
) -> Result<Option<(String, u64)>> {
    let carry = prev.filter(|m| m.has_bundle && m.head_commit == src.head_commit);
    if let Some(m) = carry
        && m.is_pooled()
        && let (Some(s), Some(g)) = (&m.bundle_sha256, m.bundle_gen)
        && pool.objects.contains_key(&(s.clone(), g))
    {
        return Ok(Some((s.clone(), g)));
    }
    if let Some(bp) = src.bundle {
        check_bundle_len(std::fs::metadata(bp)?.len(), limits)?;
        let bytes = std::fs::read(bp)?;
        check_bundle_len(bytes.len() as u64, limits)?;
        let sha = sha256_hex(&bytes);
        let (g, put) = chooser.choose(&sha, bytes.len() as u64);
        if put {
            b.put(bucket, &obj_key(&sha, g), &bytes)?;
            rep.bundle_uploaded = true;
        }
        return Ok(Some((sha, g)));
    }
    if let Some(m) = carry
        && !m.is_pooled()
        && pool.legacy_bundle
        && let Some(s) = m.bundle_sha256.as_deref().filter(|s| is_sha_hex(s))
    {
        b.copy(bucket, BUNDLE_KEY, bucket, &obj_key(s, chooser.fresh))?;
        rep.bundle_uploaded = true;
        return Ok(Some((s.to_string(), chooser.fresh)));
    }
    Ok(None)
}

/// Schema 1 (the rollback writer): the bundle at `.wt/git/branch.bundle`, as before the pool.
#[allow(clippy::too_many_arguments)]
fn path_bundle<B: ObjectBackend>(
    b: &B,
    bucket: &str,
    src: &TreeSource<'_>,
    limits: &Limits,
    prev: Option<&TreeManifest>,
    pool: &Pool,
    opts: &PublishOptions,
    rep: &mut PublishReport,
) -> Result<(bool, Option<String>)> {
    let mut has_bundle = prev.is_some_and(|m| !m.is_pooled() && m.has_bundle && m.head_commit == src.head_commit) && pool.legacy_bundle;
    let mut sha = if has_bundle { prev.and_then(|m| m.bundle_sha256.clone()) } else { None };
    if let Some(bp) = src.bundle {
        if !has_bundle {
            check_bundle_len(std::fs::metadata(bp)?.len(), limits)?;
            let bytes = std::fs::read(bp)?;
            sha = Some(sha256_hex(&bytes));
            b.put(bucket, BUNDLE_KEY, &bytes)?;
            rep.bundle_uploaded = true;
            has_bundle = true;
        }
    } else if !has_bundle && pool.legacy_bundle && opts.sweep {
        // Only when the listing shows a bundle object: no blind DELETE.
        b.delete(bucket, BUNDLE_KEY)?;
    }
    Ok((has_bundle, sha))
}
