//! WO-069 lane Q: ADR-126 section 4a gate item 6, follower drills (e) to (g). (a) to (d) are in `s2_drills_follower.rs`. Same layout: each
//! drill runs on the in-memory backend, the directory backend and, ignored, a THROWAWAY RustFS (`s2_drills_fcommon`).
//!
//! - 6e: `sync_docs` interrupted, then a doc deleted: damaged mirror index, a missing index, a tampered index, nested docs.
//! - 6f: `overlaps` on worktrees with different bases: the false-positive rate as numbers, a worktree with no changes, mixed manifests.
//! - 6g: `FollowState` files: corrupt and unreadable files, a run that refuses to start, interleaved runs and signature generations.
//!
//! Already covered in `s2_follower_drills.rs`: reads cut at 7 points then a doc deleted/edited/added (`interrupted_sync`), the two-run last-writer-wins
//! file and the torn-file thread test (`two_followers_sharing_a_state_file...`, `concurrent_saves_and_loads...`), one overlap case with different bases.
#[macro_use]
#[path = "s2_drills_fcommon/mod.rs"]
mod fc;
use fc::*;
use std::cell::Cell;
use std::path::Path;
use std::time::Instant;
use weftos_store::follower::*;
use weftos_store::overlay::{ObjectBackend as S2, ObjectMeta, Result as R, S2Error};
use weftos_store::registry_sign::{poll_verified, write_registry_entry_signed};
use weftos_store::share::*;
use weftos_store::share_fetch::RegistryEntry;

// =====================================================================================================
// 6e: interrupted sync_docs, then a doc is deleted
// =====================================================================================================

struct FailReads<'a, B: S2> {
    inner: &'a B,
    left: Cell<usize>,
}
impl<B: S2> S2 for FailReads<'_, B> {
    fn head(&self, b: &str, k: &str) -> R<Option<ObjectMeta>> {
        self.inner.head(b, k)
    }
    fn get(&self, b: &str, k: &str) -> R<Option<Vec<u8>>> {
        if self.left.get() == 0 {
            return Err(S2Error::Backend("injected: connection reset".into()));
        }
        self.left.set(self.left.get() - 1);
        self.inner.get(b, k)
    }
    fn put(&self, b: &str, k: &str, d: &[u8]) -> R<ObjectMeta> {
        self.inner.put(b, k, d)
    }
    fn delete(&self, b: &str, k: &str) -> R<()> {
        self.inner.delete(b, k)
    }
    fn list(&self, b: &str, p: &str) -> R<Vec<ObjectMeta>> {
        self.inner.list(b, p)
    }
    fn put_cas(&self, b: &str, k: &str, d: &[u8], e: Option<&str>) -> R<bool> {
        self.inner.put_cas(b, k, d, e)
    }
}

fn mirror(root: &Path) -> Vec<(String, Vec<u8>)> {
    fn walk(d: &Path, root: &Path, out: &mut Vec<(String, Vec<u8>)>) {
        for e in std::fs::read_dir(d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                walk(&p, root, out);
            } else if !p.file_name().unwrap().to_string_lossy().starts_with(".wt-") {
                out.push((p.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/"), std::fs::read(&p).unwrap()));
            }
        }
    }
    let mut v = vec![];
    walk(root, root, &mut v);
    v.sort();
    v
}

const V1: [(&str, &str); 5] = [("docs/d1.md", "# D1\n"), ("docs/d2.md", "# D2\n"), ("docs/sub/d3.md", "# D3\n"), ("docs/sub/deep/d4.md", "# D4\n"), ("docs/d5.md", "# D5\n")];
const V2: [(&str, &str); 3] = [("docs/d1.md", "# D1\n"), ("docs/d5.md", "# D5 v2\n"), ("docs/d6.md", "# D6\n")]; // d2, d3, d4 deleted

/// Sync v1 under a damage `hurt(index path)`, publish v2 (three docs gone, one edited, one added), sync again.
/// Returns (missing, wrong, stale): docs that should be there and are not, wrong bytes, files left that v2 does not have.
fn damaged_index_run<B: S2>(env: &Env<'_, B>, tag: &str, cut_at: Option<usize>, hurt: &dyn Fn(&Path)) -> (usize, usize, Vec<String>) {
    let (be, pr) = (env.be, env.pr.as_str());
    let wb = env.wt(tag);
    init_wt(be, &wb, tag, "base");
    push(be, &wb, &tree(&V1), &Pub::default()).unwrap();
    let e1 = register(be, pr, tag, &wb);
    let cache = tempfile::tempdir().unwrap();
    match cut_at {
        Some(k) => assert!(sync_docs(&FailReads { inner: be, left: Cell::new(k) }, &e1, cache.path()).is_err(), "{tag}: interruption bites at k={k}"),
        None => {
            sync_docs(be, &e1, cache.path()).unwrap();
        }
    }
    hurt(&cache.path().join(tag).join(".wt-index.json"));
    push(be, &wb, &tree(&V2), &Pub { head: "h2", now: T0 + 9, ..Pub::default() }).unwrap();
    let e2 = register(be, pr, tag, &wb);
    sync_docs(be, &e2, cache.path()).unwrap();
    let have = mirror(&cache.path().join(tag));
    let want: Vec<(String, Vec<u8>)> = V2.iter().map(|(p, c)| (p.to_string(), c.as_bytes().to_vec())).collect();
    let missing = want.iter().filter(|(p, _)| !have.iter().any(|(h, _)| h == p)).count();
    let wrong = want.iter().filter(|(p, c)| have.iter().any(|(h, hc)| h == p && hc != c)).count();
    let stale = have.iter().filter(|(p, _)| !want.iter().any(|(w, _)| w == p)).map(|(p, _)| p.clone()).collect();
    (missing, wrong, stale)
}

fn d6e_interrupted_sync_then_delete<B: S2>(env: &Env<'_, B>) {
    let none: &dyn Fn(&Path) = &|_| {};
    // nested docs, interrupted at several reads, no damage: the mirror is exactly v2 (flat docs are covered elsewhere)
    let t = Instant::now();
    // reads of one sync: meta, manifest (once since WO-069 X1), then one per doc: 0..=6 interrupts each of them
    for (i, k) in [0usize, 1, 2, 4, 5, 6].into_iter().enumerate() {
        assert_eq!(damaged_index_run(env, &format!("n{i}"), Some(k), none), (0, 0, vec![]), "k={k}");
    }
    line("6e", "nested docs, interrupted at 6 read counts (0 to 6), then 3 deleted + 1 edited + 1 added", t, "mirror == v2 each time");

    // the index (the mirror's memory of what it holds) was damaged by the interruption itself: it is written with a plain write, not rename
    let t = Instant::now();
    let trunc: &dyn Fn(&Path) = &|p| std::fs::write(p, b"[\"docs/d1.md\",\"docs/d2.m").unwrap();
    let gone: &dyn Fn(&Path) = &|p| std::fs::remove_file(p).unwrap();
    let zero: &dyn Fn(&Path) = &|p| std::fs::write(p, b"").unwrap();
    let mut res = vec![];
    for (label, hurt) in [("truncated index", trunc), ("missing index", gone), ("zero-byte index", zero)] {
        let (missing, wrong, stale) = damaged_index_run(env, &format!("x{}", res.len()), None, hurt);
        assert_eq!((missing, wrong), (0, 0), "{label}: what v2 has is always there and correct");
        res.push((label, stale));
    }
    for (label, stale) in &res {
        println!("DETAIL|6e|{label}: {} stale file(s) left after the deleted docs: {stale:?}", stale.len());
    }
    for (label, stale) in &res {
        assert!(stale.is_empty(), "{label}: the index is rebuilt from the mirror and the deleted docs are pruned: {stale:?}");
    }
    fixed("6e", "a truncated, empty or missing `.wt-index.json` is rebuilt from the mirror directory (read-only, doc-shaped, regular files, symlinks never followed) and the deleted docs are pruned");
    line("6e", "index truncated / zero bytes / missing, then docs deleted upstream (FIXED, was: left stale)", t, res.iter().map(|(l, s)| format!("{l}: {} stale", s.len())).collect::<Vec<_>>().join(", "));

    // a tampered index naming files outside the mirror: pruning goes through safe_join, so nothing outside is touched
    let t = Instant::now();
    let outside = tempfile::tempdir().unwrap();
    let victim = outside.path().join("victim.txt");
    std::fs::write(&victim, b"mine").unwrap();
    let evil = format!("[\"../../{}\",\"/etc/hostname\",\"docs/d2.md\"]", victim.display());
    let hurt: &dyn Fn(&Path) = &|p| std::fs::write(p, &evil).unwrap();
    let (missing, wrong, stale) = damaged_index_run(env, "tamper", None, hurt);
    assert_eq!((missing, wrong), (0, 0));
    assert_eq!(std::fs::read(&victim).unwrap(), b"mine", "a tampered index cannot make the follower delete outside its mirror");
    line("6e", "tampered index naming ../ and absolute paths", t, format!("outside file untouched; {} stale (the real index was replaced, as above)", stale.len()));
}
drill!(interrupted_sync_more, d6e_interrupted_sync_then_delete);

// =====================================================================================================
// 6f: overlaps with different bases, false positives as numbers
// =====================================================================================================

const N: usize = 100;

fn base_files(drift: usize) -> Vec<(String, Vec<u8>)> {
    (0..N).map(|i| (format!("src/f{i:03}.rs"), if i < drift { format!("main {i}") } else { format!("base {i}") }.into_bytes())).collect()
}

fn edited(mut f: Vec<(String, Vec<u8>)>, who: &str, idx: std::ops::Range<usize>) -> (Vec<(String, Vec<u8>)>, Vec<PathChange>) {
    let mut c = vec![];
    for i in idx {
        f[i].1 = format!("{who} {i}").into_bytes();
        c.push(ch(&f[i].0, ChangeKind::Modified));
    }
    (f, c)
}

/// Publish two worktrees (A on base c1, B on `base_b`), with or without change lists, and return what `overlaps` says.
fn report<B: S2>(env: &Env<'_, B>, tag: &str, fa: &[(String, Vec<u8>)], ca: &[PathChange], fb: &[(String, Vec<u8>)], cb: &[PathChange], base_b: &str) -> OverlapReport {
    report_with(env, tag, fa, ca, fb, cb, base_b, false)
}

/// [`report`], with B's publish saying whether its change list was computed (`changes_recorded`).
#[allow(clippy::too_many_arguments)]
fn report_with<B: S2>(env: &Env<'_, B>, tag: &str, fa: &[(String, Vec<u8>)], ca: &[PathChange], fb: &[(String, Vec<u8>)], cb: &[PathChange], base_b: &str, recorded: bool) -> OverlapReport {
    let (be, pr) = (env.be, env.pr.as_str());
    let (sa, sb) = (format!("{tag}-a"), format!("{tag}-b"));
    let (wa, wb) = (env.wt(&sa), env.wt(&sb));
    init_wt(be, &wa, &sa, "c1");
    init_wt(be, &wb, &sb, base_b);
    push(be, &wa, &tree_bytes(fa), &Pub { base: "c1", changes: ca, ..Pub::default() }).unwrap();
    push(be, &wb, &tree_bytes(fb), &Pub { base: base_b, changes: cb, recorded, ..Pub::default() }).unwrap();
    let (ea, eb) = (register(be, pr, &sa, &wa), register(be, pr, &sb, &wb));
    overlaps(be, &ea, &eb).unwrap()
}

fn paths(r: &OverlapReport) -> Vec<String> {
    r.overlaps.iter().map(|o| o.path.clone()).collect()
}

fn d6f_overlap_false_positives<B: S2>(env: &Env<'_, B>) {
    // A edits f020..f030, B edits f025..f035: the five files f025..f029 are the true conflicts
    let truth: Vec<String> = (25..30).map(|i| format!("src/f{i:03}.rs")).collect();
    let (mut rows, mut k) = (vec![], 0);
    for drift in [0usize, 10, 25, 50] {
        let t = Instant::now();
        let (fa, ca) = edited(base_files(0), "a", 20..30);
        let (fb, cb) = edited(base_files(drift), "b", 25..35);
        let base_b = if drift == 0 { "c1" } else { "c2" };
        let exact = report(env, &format!("e{k}"), &fa, &ca, &fb, &cb, base_b);
        let loose = report(env, &format!("l{k}"), &fa, &[], &fb, &[], base_b);
        k += 1;
        assert_eq!(exact.same_base, drift == 0);
        assert_eq!(loose.same_base, drift == 0);
        let (pe, pl) = (paths(&exact), paths(&loose));
        assert_eq!(pe, truth, "drift {drift}: with change lists only the real conflicts are reported");
        assert!(truth.iter().all(|p| pl.contains(p)), "without them nothing real is missed (a superset)");
        let (fp_e, fp_l) = (pe.iter().filter(|p| !truth.contains(p)).count(), pl.iter().filter(|p| !truth.contains(p)).count());
        let rate = 100.0 * fp_l as f64 / pl.len() as f64;
        rows.push((drift, pe.len(), pl.len(), fp_l, rate));
        println!("DRILL|6f|drift {drift:>2}% of files (bases differ: {})|PASS|{} ms|true conflicts {}; with change lists reported {} (false positives {fp_e}); without {} (false positives {fp_l} = {rate:.0}% of the report)", drift != 0, t.elapsed().as_millis(), truth.len(), pe.len(), pl.len());
    }
    assert_eq!(rows.iter().map(|r| r.2).collect::<Vec<_>>(), [15, 25, 35, 50], "fallback report size is exactly the union of both edit ranges and the drifted files (documented superset)");

    // a worktree that changed nothing carries an EMPTY change list, which `overlaps` cannot tell from a manifest written before change lists existed
    let t = Instant::now();
    let (fa, ca) = edited(base_files(0), "a", 20..30);
    let idle = report(env, "idle", &fa, &ca, &base_files(0), &[], "c1");
    let idle_diff = report(env, "idled", &fa, &ca, &base_files(25), &[], "c2");
    println!("DETAIL|6f|B changed nothing: truth 0 conflicts; same base reports {}, different base reports {}", idle.overlaps.len(), idle_diff.overlaps.len());
    // with the signal (a writer that computed its change list and found it empty) there is nothing to report
    let idle_rec = report_with(env, "idler", &fa, &ca, &base_files(0), &[], "c1", true);
    assert!(idle_rec.overlaps.is_empty(), "a recorded empty change list means no changes: {:?}", paths(&idle_rec));
    fixed("6f", "a worktree that recorded an empty change list (`changes_recorded`) has no overlaps");
    if !idle.overlaps.is_empty() {
        finding("6f", format!("WITHOUT the `changes_recorded` signal (a manifest from an older writer): a worktree that has changed nothing has an empty `changes` list, and `overlaps` treats an empty list as 'manifest from before the field existed' and falls back to comparing content, so every file the OTHER worktree edited is reported as a conflict: {} false positives on the same base and {} on different bases, against a truth of 0. The report is marked same_base=true so it reads as exact. Proposed: record `changes_recorded: true` in the manifest (or write the list as null for old manifests) so an empty list can mean 'no changes'", idle.overlaps.len(), idle_diff.overlaps.len()));
    }
    // mixed: only one side has a list: still the superset
    let mixed = report(env, "mixed", &fa, &ca, &edited(base_files(0), "b", 25..35).0, &[], "c1");
    assert!(truth.iter().all(|p| paths(&mixed).contains(p)) && paths(&mixed).len() == 15, "one list is not enough: {}", paths(&mixed).len());
    line("6f", "a worktree with no changes, and one with a list against one without", t, format!("idle: {} / {} reported (truth 0); mixed lists: {} (truth 5)", idle.overlaps.len(), idle_diff.overlaps.len(), paths(&mixed).len()));

    // modify/delete and a rename (delete + add) across different bases
    let t = Instant::now();
    let (mut fa, mut ca) = edited(base_files(0), "a", 20..21);
    fa.retain(|(p, _)| p != "src/f050.rs" && p != "src/f060.rs");
    ca.push(ch("src/f050.rs", ChangeKind::Deleted));
    ca.push(ch("src/f060.rs", ChangeKind::Deleted)); // renamed ...
    fa.push(("src/g060.rs".into(), b"a moved 60".to_vec()));
    ca.push(ch("src/g060.rs", ChangeKind::Added)); // ... to g060
    let (mut fb, mut cb) = edited(base_files(25), "b", 20..21);
    for i in [50usize, 60] {
        fb[i].1 = format!("b {i}").into_bytes();
        cb.push(ch(&fb[i].0, ChangeKind::Modified));
    }
    fb.push(("src/g060.rs".into(), b"b added g060".to_vec()));
    cb.push(ch("src/g060.rs", ChangeKind::Added));
    let r = report(env, "md", &fa, &ca, &fb, &cb, "c2");
    let mut got: Vec<String> = r.overlaps.iter().map(|o| format!("{}:{}", o.path, if matches!(o.kind, OverlapKind::BothChanged) { "both" } else { "modify-delete" })).collect();
    got.sort();
    assert_eq!(got, ["src/f020.rs:both", "src/f050.rs:modify-delete", "src/f060.rs:modify-delete", "src/g060.rs:both"], "different bases: edit/edit, edit-vs-delete, rename-vs-edit, add/add, and nothing from the 25 drifted files");
    line("6f", "different bases: edit/edit, modify/delete, rename vs edit, add/add", t, format!("{got:?}"));
}
drill!(overlap_false_positives, d6f_overlap_false_positives);

// =====================================================================================================
// 6g: FollowState files: corrupt, unwritable, interleaved
// =====================================================================================================

/// What `weaver store follow` does with its state file: load (a corrupt file stops the run), poll, save.
fn run_follow<B: S2>(be: &B, pr: &str, path: &Path) -> R<Vec<String>> {
    let mut st = FollowState::load(path)?;
    let c = poll(be, pr, &mut st)?;
    st.save(path)?;
    Ok(kinds(&c))
}

fn files_in(d: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(d).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    v.sort();
    v
}

fn d6g_corrupt_state_files<B: S2>(env: &Env<'_, B>) {
    let (be, pr) = (env.be, env.pr.as_str());
    for s in ["a", "b"] {
        up(env, s, &[("docs/x.md", "# X\n")], &Pub::default());
    }
    let t = Instant::now();
    let cases: [(&str, &[u8]); 6] = [
        ("truncated", b"{\"seen\":{\"a\":{\"bucket\":\"w"), ("zero bytes", b""), ("binary", &[0xde, 0xad, 0x00, 0xff]), ("wrong shape", b"[]"),
        ("wrong field type", br#"{"seen":{"a":1}}"#), ("unknown state", br#"{"seen":{"a":{"bucket":"b","head_commit":"h","state":"reviewing"}}}"#),
    ];
    for (label, bytes) in cases {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("follow.json");
        std::fs::write(&path, bytes).unwrap();
        let e = FollowState::load(&path).expect_err(label);
        assert!(e.to_string().contains("follow.json"), "{label}: the error names the file: {e}");
        assert!(run_follow(be, pr, &path).is_err(), "{label}: the run refuses to start");
        assert_eq!(std::fs::read(&path).unwrap(), bytes, "{label}: the bad file is left as found for the operator");
        assert_eq!(files_in(d.path()), ["follow.json"], "{label}: no temp file");
        // the operator removes it: everything is New once, then quiet
        std::fs::remove_file(&path).unwrap();
        assert_eq!(run_follow(be, pr, &path).unwrap(), ["new:a", "new:b"], "{label}");
        assert!(run_follow(be, pr, &path).unwrap().is_empty());
        // or a save over the bad file replaces it
        std::fs::write(&path, bytes).unwrap();
        FollowState::default().save(&path).unwrap();
        assert!(FollowState::load(&path).is_ok(), "{label}: save recovers by replacing");
    }
    // a directory where the file should be
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("follow.json");
    std::fs::create_dir(&path).unwrap();
    assert!(FollowState::load(&path).is_err() && FollowState::default().save(&path).is_err());
    assert_eq!(files_in(d.path()), ["follow.json"], "a failed rename leaves no temp file");
    // a newer follower's file with extra fields loads, and a save by this version drops them
    let path = d.path().join("newer.json");
    std::fs::write(&path, br#"{"seen":{},"generations":{},"added_later":{"x":1}}"#).unwrap();
    let st = FollowState::load(&path).unwrap();
    st.save(&path).unwrap();
    assert!(!std::fs::read_to_string(&path).unwrap().contains("added_later"));
    line("6g", "corrupt state file (6 kinds): load errors naming the file, the run stops, file kept; remove or overwrite recovers; dir at the path; newer fields dropped", t, "6 kinds x (refuse, keep, recover)");
}
drill!(corrupt_state_files, d6g_corrupt_state_files);

/// Two runs loaded the same file; the slower one saves last with an older view. Besides the re-report the docs promise, the loser's
/// state carries an older signature generation, so a rollback that a run holding the newer state would refuse used to go unnoticed.
/// Fixed (WO-069 W1 6g): `save` merges the generations already on disk, so the file keeps the higher one and the replay is refused.
fn d6g_interleaved_runs_and_generations<B: S2>(env: &Env<'_, B>) {
    let (be, pr) = (env.be, env.pr.as_str());
    let t = Instant::now();
    let mk = |state: State, updated: u64| RegistryEntry {
        slug: "wt".into(), bucket: format!("w-{}-wt", env.u), branch: "b".into(), head_commit: "h".into(), state, writer: actor("w"), merger: None,
        doc_count: 0, file_count: 0, updated_ms: updated, orphan: None, writer_check: None,
    };
    write_registry_entry_signed(be, pr, &mk(State::Claimed, 100), &Toy).unwrap();
    let (v1, s1) = (S2::get(be, pr, "worktrees/wt.json").unwrap().unwrap(), S2::get(be, pr, "worktrees.sig/wt.json").unwrap().unwrap());
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("follow.json");
    let step = |who: &str| -> (Vec<String>, FollowState) {
        let mut st = FollowState::load(&path).unwrap();
        let c = poll_verified(be, pr, &mut st, &Toy, true, false).unwrap();
        println!("DETAIL|6g|{who} reports {:?}", kinds(&c));
        (kinds(&c), st)
    };
    step("first run").1.save(&path).unwrap();
    // X and Y both load the saved file; Y polls (registry still at v1) and is slow to save; the daemon moves on; X polls and saves
    let y_state = step("Y (older view)").1;
    write_registry_entry_signed(be, pr, &mk(State::Merged, 200), &Toy).unwrap();
    let (cx, x_state) = step("X (newer view)");
    assert_eq!(cx, ["state:wt"]);
    let g_new = x_state.generations["wt"];
    x_state.save(&path).unwrap();
    y_state.save(&path).unwrap(); // the loser saves last
    let g_after = FollowState::load(&path).unwrap().generations["wt"];
    // fixed (WO-069 W1 6g): save merges the generations on disk, so the loser's save keeps the newer one
    assert_eq!(g_after, g_new, "the older view saved last, but the file keeps the higher generation ({g_after} vs {g_new})");
    // a rollback: the genuinely signed v1 is put back
    S2::put(be, pr, "worktrees/wt.json", &v1).unwrap();
    S2::put(be, pr, "worktrees.sig/wt.json", &s1).unwrap();
    let mut with_newer = x_state.clone();
    let refused = kinds(&poll_verified(be, pr, &mut with_newer, &Toy, true, false).unwrap());
    let mut from_file = FollowState::load(&path).unwrap();
    let silent = kinds(&poll_verified(be, pr, &mut from_file, &Toy, true, false).unwrap());
    println!("DETAIL|6g|rollback replay: a run holding the newer generation reports {refused:?}; a run loading the clobbered file reports {silent:?}");
    assert_eq!(refused, ["bad:wt"]);
    assert_eq!(silent, ["bad:wt"], "a run loading the file after the older view saved last refuses the replay too");
    if silent.is_empty() {
        finding("6g", "FollowState::save is last-writer-wins, and the state carries the signature generations that refuse a rollback. Two overlapping `follow` runs on one file can leave the file with the OLDER generation and the OLDER seen state; a genuinely signed old entry replayed afterwards is then accepted silently (the next run reports nothing at all, and believes the worktree is still in the old state), where a run that kept the newer generation reports it as unreadable. The documented 'loser re-reports, never skips' holds for changes but not for rollback protection. Proposed: make save merge, taking the per-slug maximum of `generations` with what is on disk, or give each run its own file");
    }
    line("6g", "two runs, the older view saves last: the file keeps the higher generation, the replay is refused", t, format!("generation {g_new} -> {g_after}; replay: newer-state run {refused:?}, clobbered-file run {silent:?}"));
}
drill!(interleaved_generations, d6g_interleaved_runs_and_generations);

#[test]
#[cfg(unix)]
fn six_g_unwritable_state_directory_keeps_the_file_and_leaves_no_temp() {
    use std::os::unix::fs::PermissionsExt;
    let t = Instant::now();
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("follow.json");
    let mut st = FollowState::default();
    st.save(&path).unwrap();
    let before = std::fs::read(&path).unwrap();
    st.seen.insert("a".into(), Seen { etag: None, bucket: "b".into(), head_commit: "h".into(), state: State::Open });
    std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
    if std::fs::write(d.path().join("probe"), b"").is_ok() {
        std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        println!("DETAIL|6g|running as a user that ignores directory permissions: skipped");
        return;
    }
    let r = st.save(&path);
    std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(r.is_err(), "a save into a read-only directory is an error, not a silent skip");
    assert_eq!(std::fs::read(&path).unwrap(), before, "the previous state file is intact");
    assert_eq!(files_in(d.path()), ["follow.json"]);
    // a parent that is a file
    let blocked = d.path().join("follow.json").join("x.json");
    assert!(st.save(&blocked).is_err());
    line("6g", "state directory not writable, or its parent a file: save errors, old file intact, no temp left", t, "ok");
}

#[test]
fn six_g_overlapping_runs_under_churn_leave_a_valid_file_and_converge() {
    let t = Instant::now();
    let d = tempfile::tempdir().unwrap();
    let be = weftos_store::FsBackend::open(&d.path().join("store")).unwrap();
    let env = Env::new(&be, Some(&be));
    let pr = env.pr.clone();
    for s in ["one", "two", "three"] {
        up(&env, s, &[("docs/x.md", "# X\n")], &Pub::default());
    }
    let path = d.path().join("state").join("follow.json");
    let (stop, heads) = (std::sync::atomic::AtomicBool::new(false), std::sync::Mutex::new(vec!["h1".to_string()]));
    let reported = std::sync::Mutex::new(Vec::<(String, String)>::new());
    std::thread::scope(|s| {
        for _ in 0..2 {
            s.spawn(|| {
                while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                    let mut st = FollowState::load(&path).unwrap_or_else(|e| panic!("load saw a torn file: {e}"));
                    let c = poll(&be, &pr, &mut st).unwrap();
                    for x in c {
                        if let Change::HeadMoved { entry, from } = x {
                            reported.lock().unwrap().push((from, entry.head_commit));
                        }
                    }
                    st.save(&path).unwrap_or_else(|e| panic!("save failed under overlap: {e}"));
                }
            });
        }
        let wb = format!("w-{}-one", env.u);
        for i in 2..=25 {
            let h = format!("h{i}");
            push(&be, &wb, &tree(&[("docs/x.md", "# X\n")]), &Pub { head: &h, now: T0 + i as u64, ..Pub::default() }).unwrap();
            register(&be, &pr, "one", &wb);
            heads.lock().unwrap().push(h);
            std::thread::sleep(std::time::Duration::from_millis(3));
        }
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    let known = heads.into_inner().unwrap();
    let reported = reported.into_inner().unwrap();
    assert!(reported.iter().all(|(f, t)| known.contains(f) && known.contains(t)), "no phantom heads in any report");
    assert!(reported.iter().all(|(f, t)| f != t));
    let st = FollowState::load(&path).unwrap();
    assert!(st.seen.len() == 3, "{}", st.seen.len());
    let c = run_follow(&be, &pr, &path).unwrap();
    let after = FollowState::load(&path).unwrap();
    assert_eq!(after.seen["one"].head_commit, "h25", "one more run converges on the final head (it reported {c:?})");
    assert!(run_follow(&be, &pr, &path).unwrap().is_empty());
    assert_eq!(files_in(path.parent().unwrap()), ["follow.json"]);
    line("6g", "two overlapping follow loops on one file while a worktree moves 24 times", t, format!("{} head moves reported across both, file valid, converges on h25", reported.len()));
}
