//! Which pool object a publish uses for each content, what it retires, and what its sweep may
//! delete (WO-069 lane D3). A child of `share`; pure functions over the manifest the publish read
//! (the base) and its one listing of the bucket. No backend calls.
//!
//! The rules, and why a delete can never hit a key a current manifest names:
//!
//! 1. **A PUT is only ever to a fresh key**, of generation `fresh = max(base generation, every
//!    generation listed or retiring) + 1`. A sweep deletes only keys in its own manifest's
//!    `retiring` list, which came from listings taken before that manifest's conditional write.
//!    Every such key has a generation at most that manifest's. A publish whose manifest write
//!    succeeds read a base at or after every manifest whose sweep can still be running (otherwise
//!    its conditional write fails), so its fresh generation is above every key such a sweep holds:
//!    a PUT never re-creates a key a sweep is deleting. (The proposal's plain sha keys fail exactly
//!    here: a publish built on manifest N re-uses or re-PUTs an object N's own sweep deletes next.)
//! 2. **Re-use without a PUT** is limited to: a key the base references and the listing has at the
//!    right size (no sweep of the base or of its ancestors can hold it); a key in the base's
//!    `retiring` list younger than half its grace (a sweep deletes only past the full grace, so the
//!    two are disjoint while publisher clocks agree within half a grace); and a stray (listed, not
//!    referenced, not retiring) of a generation above the base's, which can only have been put by a
//!    publish built on the base itself, after every listing any running sweep took (this is what
//!    makes a rerun after an interrupted publish upload nothing twice).
//! 3. **A sweep deletes** only a key its new manifest does not reference and whose retiring record
//!    is past its grace (`now - since_ms`, a future stamp counting as now). Strays are recorded on
//!    first sight (`since_ms = now`) and so wait a full grace too: the listing carries no usable
//!    modification time (`worktree_backend.rs` reports 0 for every S2 backend), so an object's age
//!    is only ever measured from when a publish first saw it unreferenced.
//!
//! Residuals (ADR-126 section 4): publisher clocks more than half a grace apart (or a publish that
//! stalls that long between taking `now` and reading the manifest); a manifest deleted by hand
//! (re-share) while a sweep of the old tree is still running.

use super::*;
use crate::overlay::ObjectMeta;
use std::collections::{BTreeMap, HashMap, HashSet};

/// How long an unreferenced object is kept (owner decision 2026-11-15: 24 h).
pub const DEFAULT_GRACE_MS: u64 = 24 * 60 * 60 * 1000;
/// Bundles beyond the newest [`KEPT_BUNDLES`] retiring ones are kept this long (1 h).
pub const DEFAULT_BUNDLE_GRACE_MS: u64 = 60 * 60 * 1000;
/// Retiring bundles kept for the full grace.
pub const KEPT_BUNDLES: usize = 2;

/// How a publish writes and sweeps. The daemon builds it once from its environment
/// (`WEFTOS_WORKTREE_MANIFEST_SCHEMA`, `WEFTOS_WORKTREE_SWEEP`); the library reads no environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublishOptions {
    /// [`SCHEMA_V2`] (default) or [`SCHEMA_V1`], the rollback lever: path-named objects an rc.1
    /// reader can read, with the pool still swept.
    pub schema: u32,
    pub grace_ms: u64,
    pub bundle_grace_ms: u64,
    /// `false` (the kill switch): retire objects in the manifest, delete nothing.
    pub sweep: bool,
}

impl Default for PublishOptions {
    fn default() -> Self {
        Self { schema: SCHEMA, grace_ms: DEFAULT_GRACE_MS, bundle_grace_ms: DEFAULT_BUNDLE_GRACE_MS, sweep: true }
    }
}

/// The bucket as one listing shows it.
pub(super) struct Pool {
    /// Pool objects: `(sha, generation) -> size`.
    pub objects: HashMap<(String, u64), u64>,
    /// Schema-1 objects under `.wt/tree/`: key -> size.
    pub tree: HashMap<String, u64>,
    /// The schema-1 bundle key exists.
    pub legacy_bundle: bool,
}

impl Pool {
    pub fn from_listing(listing: Vec<ObjectMeta>) -> Self {
        let mut p = Pool { objects: HashMap::new(), tree: HashMap::new(), legacy_bundle: false };
        for o in listing {
            if let Some(k) = parse_obj_key(&o.key) {
                p.objects.insert(k, o.size);
            } else if o.key.starts_with(TREE_PREFIX) {
                p.tree.insert(o.key, o.size);
            } else if o.key == BUNDLE_KEY {
                p.legacy_bundle = true;
            }
        }
        p
    }
    fn has(&self, sha: &str, g: u64, size: u64) -> bool {
        self.objects.get(&(sha.to_string(), g)) == Some(&size)
    }
}

/// `now - since`, a stamp from the future counting as now (never as old).
pub(super) fn age(now_ms: u64, since_ms: u64) -> u64 {
    now_ms.saturating_sub(since_ms)
}

/// Every pool object `m` references, with whether it is the bundle.
pub(super) fn references(m: &TreeManifest) -> HashMap<(String, u64), bool> {
    let mut r = HashMap::new();
    if !m.is_pooled() {
        return r;
    }
    // The bundle first: a key that is also some file's object is a file (the longer grace).
    if let (true, Some(s), Some(g)) = (m.has_bundle, &m.bundle_sha256, m.bundle_gen) {
        r.insert((s.clone(), g), true);
    }
    for f in &m.files {
        if let Some(g) = f.generation {
            r.insert((f.sha256.clone(), g), false);
        }
    }
    r
}

/// Picks the pool object for each content of one publish (rules 1 and 2 above).
pub(super) struct Chooser<'a> {
    pool: &'a Pool,
    /// sha -> generations the base references.
    base_refs: HashMap<String, Vec<u64>>,
    /// sha -> generations retiring in the base and young enough to revive.
    young: HashMap<String, Vec<u64>>,
    /// Every key the base references or retires (never a stray).
    known: HashSet<(String, u64)>,
    /// Strays above this generation may be re-used; `None`: none may (no base generation).
    stray_floor: Option<u64>,
    pub fresh: u64,
    memo: HashMap<String, (u64, bool)>,
}

impl<'a> Chooser<'a> {
    pub fn new(base: Option<&TreeManifest>, pool: &'a Pool, opts: &PublishOptions, now_ms: u64) -> Self {
        let mut c = Chooser {
            pool,
            base_refs: HashMap::new(),
            young: HashMap::new(),
            known: HashSet::new(),
            stray_floor: base.map(|m| m.generation).filter(|g| *g > 0),
            fresh: 0,
            memo: HashMap::new(),
        };
        let mut top = pool.objects.keys().map(|(_, g)| *g).max().unwrap_or(0);
        if let Some(m) = base {
            top = top.max(m.generation);
            for (k, _) in references(m) {
                c.base_refs.entry(k.0.clone()).or_default().push(k.1);
                c.known.insert(k);
            }
            for r in &m.retiring {
                top = top.max(r.generation);
                c.known.insert((r.sha256.clone(), r.generation));
                let grace = if r.bundle { opts.bundle_grace_ms } else { opts.grace_ms };
                if age(now_ms, r.since_ms) < grace / 2 {
                    c.young.entry(r.sha256.clone()).or_default().push(r.generation);
                }
            }
        }
        for v in c.base_refs.values_mut().chain(c.young.values_mut()) {
            v.sort_unstable();
        }
        c.fresh = top + 1;
        c
    }

    /// The generation holding content `sha` of `size` bytes, and whether this publish must PUT it.
    /// A content seen earlier in this publish is never PUT twice.
    pub fn choose(&mut self, sha: &str, size: u64) -> (u64, bool) {
        if let Some(&(g, _)) = self.memo.get(sha) {
            return (g, false);
        }
        let pool = self.pool;
        let listed = |gens: Option<&Vec<u64>>| gens.and_then(|v| v.iter().copied().find(|g| pool.has(sha, *g, size)));
        let stray = || {
            let floor = self.stray_floor?;
            let mut gens: Vec<u64> = pool
                .objects
                .iter()
                .filter(|((s, g), sz)| s == sha && *g > floor && **sz == size && !self.known.contains(&(s.clone(), *g)))
                .map(|((_, g), _)| *g)
                .collect();
            gens.sort_unstable();
            gens.first().copied()
        };
        let pick = listed(self.base_refs.get(sha))
            .or_else(|| listed(self.young.get(sha)))
            .or_else(stray)
            .map(|g| (g, false))
            .unwrap_or((self.fresh, true));
        self.memo.insert(sha.to_string(), pick);
        pick
    }
}

/// The retiring list a publish writes and the keys its sweep may delete.
#[derive(Debug, Default)]
pub(super) struct Plan {
    pub retiring: Vec<Retiring>,
    /// Pool keys past their grace (still listed in `retiring` until a listing shows them gone).
    pub doomed: Vec<String>,
    /// Entries this publish added (newly unreferenced, and strays seen for the first time).
    pub added: usize,
    pub v1_retired_ms: Option<u64>,
    /// Schema-1 keys (`.wt/tree/*`, the old bundle key) past the grace after `v1_retired_ms`.
    pub v1_doomed: Vec<String>,
}

/// Rule 3 above. `new_refs` are the pool objects the new manifest references (empty for a
/// schema-1 manifest), `pooled` whether the new manifest is schema 2.
pub(super) fn plan(
    base: Option<&TreeManifest>,
    new_refs: &HashMap<(String, u64), bool>,
    pooled: bool,
    pool: &Pool,
    opts: &PublishOptions,
    limits: &Limits,
    now_ms: u64,
) -> Result<Plan> {
    let mut out: BTreeMap<(String, u64), Retiring> = BTreeMap::new();
    let mut p = Plan::default();
    if let Some(m) = base {
        // Carried forward unchanged (its stamp is never reset), unless revived or gone.
        for r in &m.retiring {
            let k = (r.sha256.clone(), r.generation);
            if !new_refs.contains_key(&k) && pool.objects.contains_key(&k) {
                out.entry(k).or_insert_with(|| r.clone());
            }
        }
        for (k, bundle) in references(m) {
            if !new_refs.contains_key(&k) && !out.contains_key(&k) && let Some(&size) = pool.objects.get(&k) {
                out.insert(k.clone(), Retiring { sha256: k.0, generation: k.1, size, since_ms: now_ms, bundle });
                p.added += 1;
            }
        }
    }
    // Strays: never named by the base. Aged from now, never by a server clock (module doc).
    for (k, &size) in &pool.objects {
        if !new_refs.contains_key(k) && !out.contains_key(k) {
            out.insert(k.clone(), Retiring { sha256: k.0.clone(), generation: k.1, size, since_ms: now_ms, bundle: false });
            p.added += 1;
        }
    }
    let mut bundles: Vec<&Retiring> = out.values().filter(|r| r.bundle).collect();
    bundles.sort_by(|a, b| b.since_ms.cmp(&a.since_ms).then(b.generation.cmp(&a.generation)));
    let kept: HashSet<(String, u64)> =
        bundles.iter().take(KEPT_BUNDLES).map(|r| (r.sha256.clone(), r.generation)).collect();
    let mut live = 0usize;
    for (k, r) in &out {
        let grace = if r.bundle && !kept.contains(k) { opts.bundle_grace_ms } else { opts.grace_ms };
        if age(now_ms, r.since_ms) >= grace {
            p.doomed.push(r.key());
        } else {
            live += 1;
        }
    }
    let held = if opts.sweep { live } else { out.len() };
    if held > max_retiring(limits) {
        return Err(S2Error::Invalid(format!(
            "{held} unreferenced objects are inside the grace period, over the {} limit; nothing was written \
             (wait for the grace period to pass, or check that the sweep is not switched off)",
            max_retiring(limits)
        )));
    }
    p.retiring = out.into_values().collect();
    // Schema-1 objects left by a schema-1 tree: kept for a grace after the first schema-2 publish
    // replaced it. Only a schema-2 writer retires them (a schema-1 writer owns `.wt/tree/`).
    if pooled && (!pool.tree.is_empty() || pool.legacy_bundle) {
        let since = base.filter(|m| m.is_pooled()).and_then(|m| m.v1_retired_ms).unwrap_or(now_ms);
        p.v1_retired_ms = Some(since);
        if age(now_ms, since) >= opts.grace_ms {
            p.v1_doomed = pool.tree.keys().cloned().collect();
            p.v1_doomed.sort();
            if pool.legacy_bundle {
                p.v1_doomed.push(BUNDLE_KEY.to_string());
            }
        }
    }
    Ok(p)
}
