//! WO-069 lane D3, the gate for the grace sweep (proposal section 7.5: "deleting a referenced object
//! is data loss, so that test is the gate"). Random publish sequences (edit, add, delete, rename,
//! duplicate content, revert, bundle moves) with publishes cut at a random call, a second publisher
//! run in the middle of another one at a random call, publisher clocks skewed and stepped backwards,
//! and long quiet periods past the grace. A hooking backend checks every delete as it is made.
//!
//! Invariants:
//! - D: no delete ever targets a key the current manifest names, or one named by a manifest
//!   replaced less than its grace ago (checked on the delete call itself);
//! - I1: after every publish, every object the current manifest names exists and hashes right;
//! - I2: every object named by a manifest replaced less than its grace ago exists and hashes right;
//! - I3: after a quiet period longer than the grace, the pool holds exactly what the current
//!   manifest names.
//!
//! "Less than its grace ago" is in true (test) time minus twice the largest publisher clock skew,
//! the bound the design assumes (`share_gc.rs`). Seeds are fixed; a failure prints seed and step.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashSet};
use weftos_store::overlay::{ObjectBackend, ObjectMeta, Result as R, S2Error};
use weftos_store::share::*;
use weftos_store::testing::MemBackend;

const WT: &str = "w-d3-prop";
const MIN: u64 = 60 * 1000;
const GRACE: u64 = 60 * MIN;
const BGRACE: u64 = 10 * MIN;
/// Largest publisher clock skew either way (half of it must stay under BGRACE / 2).
const SKEW: u64 = 2 * MIN;

fn opts() -> PublishOptions {
    PublishOptions { schema: SCHEMA_V2, grace_ms: GRACE, bundle_grace_ms: BGRACE, sweep: true }
}

/// splitmix64; no extra dependency.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    fn chance(&mut self, one_in: u64) -> bool {
        self.below(one_in) == 0
    }
}

type Hook = Box<dyn FnOnce(&Hooked)>;

/// MemBackend plus: a crash point (every call from index `dead_from` on fails), one hook run before
/// call `hook_at`, the manifest history, and the delete-path check.
struct Hooked {
    inner: MemBackend,
    calls: Cell<u64>,
    dead_from: Cell<Option<u64>>,
    hook_at: Cell<Option<u64>>,
    /// Run the hook just before the first delete instead (inside the sweep, after the manifest write).
    hook_on_delete: Cell<bool>,
    hook: RefCell<Option<Hook>>,
    /// True (test) time.
    now: Cell<u64>,
    /// Every manifest that stopped being current, with the true time it was replaced.
    history: RefCell<Vec<(TreeManifest, u64)>>,
    violations: RefCell<Vec<String>>,
    deletes: Cell<u64>,
}

fn named(m: &TreeManifest) -> Vec<(String, String, bool)> {
    let mut v: Vec<(String, String, bool)> = m.files.iter().map(|f| (m.object_key(f), f.sha256.clone(), false)).collect();
    if let (Some(k), Some(s)) = (m.bundle_object(), &m.bundle_sha256) {
        v.push((k, s.clone(), true));
    }
    v
}

impl Hooked {
    fn new() -> Self {
        Hooked {
            inner: MemBackend::new(),
            calls: Cell::new(0),
            dead_from: Cell::new(None),
            hook_at: Cell::new(None),
            hook_on_delete: Cell::new(false),
            hook: RefCell::new(None),
            now: Cell::new(1_000 * GRACE),
            history: RefCell::new(Vec::new()),
            violations: RefCell::new(Vec::new()),
            deletes: Cell::new(0),
        }
    }
    fn step(&self) -> R<()> {
        let i = self.calls.get();
        self.calls.set(i + 1);
        if self.hook_at.get() == Some(i) {
            self.hook_at.set(None);
            if let Some(h) = self.hook.borrow_mut().take() {
                h(self);
            }
        }
        if self.dead_from.get().is_some_and(|d| i >= d) {
            return Err(S2Error::Backend(format!("drill: process killed at call {i}")));
        }
        Ok(())
    }
    fn current(&self) -> Option<TreeManifest> {
        self.inner.get(WT, MANIFEST_KEY).unwrap().map(|d| serde_json::from_slice(&d).unwrap())
    }
    /// Manifests replaced recently enough that their objects must still be there.
    fn protected(&self) -> Vec<TreeManifest> {
        let now = self.now.get();
        let mut v: Vec<TreeManifest> = self
            .history
            .borrow()
            .iter()
            .filter(|(_, t)| now.saturating_sub(*t) + 2 * SKEW < BGRACE)
            .map(|(m, _)| m.clone())
            .collect();
        v.extend(self.current());
        v
    }
    fn protected_files(&self) -> Vec<TreeManifest> {
        let now = self.now.get();
        let mut v: Vec<TreeManifest> = self
            .history
            .borrow()
            .iter()
            .filter(|(_, t)| now.saturating_sub(*t) + 2 * SKEW < GRACE)
            .map(|(m, _)| m.clone())
            .collect();
        v.extend(self.current());
        v
    }
    /// Why `k` must exist now, if it must: named as a file by a manifest replaced less than the
    /// grace ago, or named at all by one replaced less than the bundle grace ago (or current).
    fn guarded(&self, k: &str) -> Option<String> {
        for m in self.protected_files() {
            if named(&m).iter().any(|(key, _, bundle)| key == k && !bundle) {
                return Some(format!("a file of manifest generation {}", m.generation));
            }
        }
        for m in self.protected() {
            if named(&m).iter().any(|(key, _, _)| key == k) {
                return Some(format!("named by manifest generation {}", m.generation));
            }
        }
        None
    }
    fn note_manifest_write(&self, old: Option<Vec<u8>>, wrote: bool) {
        if let (true, Some(old)) = (wrote, old) {
            self.history.borrow_mut().push((serde_json::from_slice(&old).unwrap(), self.now.get()));
        }
    }
}

impl ObjectBackend for Hooked {
    fn head(&self, b: &str, k: &str) -> R<Option<ObjectMeta>> {
        self.step()?;
        self.inner.head(b, k)
    }
    fn get(&self, b: &str, k: &str) -> R<Option<Vec<u8>>> {
        self.step()?;
        self.inner.get(b, k)
    }
    fn put(&self, b: &str, k: &str, d: &[u8]) -> R<ObjectMeta> {
        self.step()?;
        let old = (k == MANIFEST_KEY).then(|| self.inner.get(b, k).unwrap()).flatten();
        let r = self.inner.put(b, k, d)?;
        self.note_manifest_write(old, k == MANIFEST_KEY);
        Ok(r)
    }
    fn put_cas(&self, b: &str, k: &str, d: &[u8], e: Option<&str>) -> R<bool> {
        self.step()?;
        let old = (k == MANIFEST_KEY).then(|| self.inner.get(b, k).unwrap()).flatten();
        let r = self.inner.put_cas(b, k, d, e)?;
        self.note_manifest_write(old, r && k == MANIFEST_KEY);
        Ok(r)
    }
    fn delete(&self, b: &str, k: &str) -> R<()> {
        if self.hook_on_delete.replace(false)
            && let Some(h) = self.hook.borrow_mut().take()
        {
            h(self);
        }
        self.step()?;
        self.deletes.set(self.deletes.get() + 1);
        // The delete path itself: never a key the current manifest, or a protected one, names.
        if let Some(why) = self.guarded(k) {
            self.violations.borrow_mut().push(format!("delete of {k}: {why}"));
        }
        self.inner.delete(b, k)
    }
    fn list(&self, b: &str, p: &str) -> R<Vec<ObjectMeta>> {
        self.step()?;
        self.inner.list(b, p)
    }
}

/// One source tree on disk.
struct Tree {
    dir: tempfile::TempDir,
    files: BTreeMap<String, Vec<u8>>,
    head: String,
    bundle: Option<Vec<u8>>,
    disk: RefCell<BTreeMap<String, Vec<u8>>>,
}

impl Tree {
    fn new() -> Self {
        Tree { dir: tempfile::tempdir().unwrap(), files: BTreeMap::new(), head: "h0".into(), bundle: None, disk: RefCell::new(BTreeMap::new()) }
    }
    /// Make the directory match `files` (incremental: only changed files are written).
    fn sync(&self) {
        let root = self.dir.path().join("t");
        let mut disk = self.disk.borrow_mut();
        disk.retain(|p, _| {
            let keep = self.files.contains_key(p);
            if !keep {
                let _ = std::fs::remove_file(root.join(p));
            }
            keep
        });
        for (p, d) in &self.files {
            if disk.get(p) != Some(d) {
                let f = root.join(p);
                std::fs::create_dir_all(f.parent().unwrap()).unwrap();
                std::fs::write(f, d).unwrap();
                disk.insert(p.clone(), d.clone());
            }
        }
        std::fs::create_dir_all(&root).unwrap();
        if let Some(b) = &self.bundle {
            std::fs::write(self.dir.path().join("b.bundle"), b).unwrap();
        }
    }
    fn mutate(&mut self, r: &mut Rng, seen: &mut Vec<Vec<u8>>) {
        let paths: Vec<String> = self.files.keys().cloned().collect();
        let pick = |r: &mut Rng| paths.get(r.below(paths.len() as u64) as usize).cloned();
        match r.below(9) {
            0 | 1 => {
                if let Some(p) = pick(r) {
                    self.files.insert(p, format!("edit {}", r.below(1000)).into_bytes());
                }
            }
            2 => {
                self.files.insert(format!("docs/n{}.md", r.below(40)), format!("new {}", r.below(50)).into_bytes());
            }
            3 => {
                if let Some(p) = pick(r) {
                    self.files.remove(&p);
                }
            }
            4 => {
                if let Some(p) = pick(r) {
                    let d = self.files.remove(&p).unwrap();
                    self.files.insert(format!("src/r{}.rs", r.below(40)), d);
                }
            }
            5 => {
                if let Some(p) = pick(r) {
                    let d = self.files[&p].clone();
                    self.files.insert(format!("src/dup{}.rs", r.below(10)), d);
                }
            }
            6 => {
                // revert to some content seen before (possibly retiring, possibly long gone)
                if let (Some(p), false) = (pick(r), seen.is_empty()) {
                    let d = seen[r.below(seen.len() as u64) as usize].clone();
                    self.files.insert(p, d);
                }
            }
            7 => {
                self.head = format!("h{}", r.below(1000));
                self.bundle = if r.chance(4) { None } else { Some(format!("bundle {}", r.below(20)).into_bytes()) };
            }
            _ => {}
        }
        seen.extend(self.files.values().cloned());
        if let Some(b) = &self.bundle {
            seen.push(b.clone());
        }
        if seen.len() > 200 {
            seen.drain(..100);
        }
    }
    fn publish(&self, b: &Hooked, now: u64) -> R<Published> {
        self.sync();
        let paths: Vec<String> = self.files.keys().cloned().collect();
        let bp = self.dir.path().join("b.bundle");
        let src = TreeSource {
            root: &self.dir.path().join("t"),
            paths: &paths,
            uncommitted: &[],
            changes: &[],
            branch: "feat/prop",
            base_commit: "base",
            head_commit: &self.head,
            bundle: self.bundle.as_ref().map(|_| bp.as_path()),
            writer: Actor { agent: "w".into(), host: "h".into() },
        };
        publish_tree_opts(b, WT, &src, &Limits::default(), now, None, &opts())
    }
}

fn check(b: &Hooked, ctx: &str) {
    let v = b.violations.borrow();
    assert!(v.is_empty(), "{ctx}: delete-path violations: {v:?}");
    drop(v);
    // I1 / I2
    let mut must: Vec<(String, String, u64)> = Vec::new();
    for m in b.protected_files() {
        must.extend(named(&m).into_iter().filter(|(_, _, bundle)| !bundle).map(|(k, s, _)| (k, s, m.generation)));
    }
    for m in b.protected() {
        must.extend(named(&m).into_iter().map(|(k, s, _)| (k, s, m.generation)));
    }
    for (key, sha, g) in must {
        let d = b.inner.get(WT, &key).unwrap();
        assert!(d.is_some(), "{ctx}: {key} (named by manifest generation {g}) is missing");
        assert_eq!(sha256_hex(&d.unwrap()), sha, "{ctx}: {key} has the wrong bytes");
    }
}

fn skewed(r: &mut Rng, t: u64) -> u64 {
    // within +-SKEW of true time; a later publish can be up to 2 * SKEW behind an earlier one
    t + r.below(2 * SKEW + 1) - SKEW
}

fn run(seed: u64, steps: usize) -> (u64, u64, u64) {
    let mut r = Rng(seed);
    let b = Hooked::new();
    let mut a = Tree::new();
    for i in 0..6 {
        a.files.insert(format!("src/f{i}.rs"), format!("c{}", i % 4).into_bytes());
    }
    let mut seen = Vec::new();
    let (mut cuts, mut races) = (0u64, 0u64);
    for step in 0..steps {
        let ctx = format!("seed {seed} step {step}");
        a.mutate(&mut r, &mut seen);
        // time passes: mostly short, sometimes past a bundle grace or a full grace
        let dt = match r.below(10) {
            0..=4 => r.below(5 * MIN),
            5 | 6 => BGRACE + r.below(BGRACE),
            7 => GRACE / 2,
            _ => GRACE + r.below(GRACE),
        };
        b.now.set(b.now.get() + dt);
        let now = skewed(&mut r, b.now.get());
        b.calls.set(0);
        match r.below(4) {
            0 => {
                // a publish killed at a random call
                cuts += 1;
                b.dead_from.set(Some(r.below(40)));
                let _ = a.publish(&b, now);
                b.dead_from.set(None);
            }
            1 => {
                // a second publisher, with its own tree and clock, runs inside this one
                races += 1;
                let mut other = Tree::new();
                other.files = a.files.clone();
                other.head = a.head.clone();
                other.bundle = a.bundle.clone();
                other.mutate(&mut r, &mut seen);
                // Often bring back content the current manifest is retiring: the case where a sweep and a
                // concurrent publish disagree about one key.
                if let Some(m) = b.current().filter(|m| !m.retiring.is_empty() && r.chance(2)) {
                    let x = &m.retiring[r.below(m.retiring.len() as u64) as usize];
                    if let (Some(d), Some(p)) = (b.inner.get(WT, &x.key()).unwrap(), other.files.keys().next().cloned()) {
                        other.files.insert(p, d);
                    }
                }
                let other_now = skewed(&mut r, b.now.get());
                if r.chance(2) {
                    b.hook_on_delete.set(true);
                } else {
                    b.hook_at.set(Some(r.below(40)));
                }
                *b.hook.borrow_mut() = Some(Box::new(move |hb: &Hooked| {
                    let _ = other.publish(hb, other_now);
                }));
                let _ = a.publish(&b, now);
                b.hook_at.set(None);
                b.hook_on_delete.set(false);
                b.hook.borrow_mut().take();
            }
            _ => {
                a.publish(&b, now).unwrap_or_else(|e| panic!("{ctx}: {e}"));
            }
        }
        check(&b, &ctx);
    }
    // I3: quiet past the grace, twice (strays seen for the first time wait a full grace too).
    for _ in 0..2 {
        b.now.set(b.now.get() + GRACE + 2 * SKEW + 1);
        a.publish(&b, b.now.get()).unwrap();
        check(&b, &format!("seed {seed} quiescence"));
    }
    a.publish(&b, b.now.get()).unwrap(); // drops the entries the last sweep deleted
    let m = b.current().unwrap();
    let want: HashSet<String> = named(&m).into_iter().map(|(k, _, _)| k).collect();
    let have: HashSet<String> = b.inner.list(WT, ".wt/obj/").unwrap().into_iter().map(|o| o.key).collect();
    assert_eq!(have, want, "seed {seed}: after quiescence the pool must be exactly the referenced set");
    assert!(m.retiring.is_empty(), "seed {seed}: {:?}", m.retiring);
    (cuts, races, b.deletes.get())
}

#[test]
fn the_sweep_never_deletes_a_referenced_or_protected_object_and_leaves_no_leak() {
    let (mut cuts, mut races, mut deletes) = (0, 0, 0);
    let seeds = 400u64;
    for seed in 0..seeds {
        let (c, r, d) = run(0xD3_0000 + seed, 30);
        cuts += c;
        races += r;
        deletes += d;
    }
    println!("D3-PROP|{seeds} sequences x 30 publishes|{cuts} cut|{races} raced|{deletes} deletes checked|PASS");
    assert!(cuts > 1000 && races > 1000 && deletes > 1000, "the generator must exercise every path: {cuts} {races} {deletes}");
}
