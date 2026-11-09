//! ADR-126 section 4a gate drills (items 4 and 6) against a real RustFS, with injected clocks.
//!
//! Needs a THROWAWAY RustFS (never PG): WEFTOS_STORE_TEST_RUSTFS_ENV names its env file.
//!   scripts/build.sh test weftos-store --features rustfs --ignored --filter drill
//!
//! The registry entries, manifests and handoff records are written to the real server by the real
//! S2 code. Only the `now_ms` argument is chosen by the test, which is what lets a drill place a
//! writer a day ahead or a claim past its TTL without waiting. The follower's own clock is the
//! real wall clock.
#![cfg(feature = "rustfs")]

use weftos_store::backend::{AdminBackend, ObjectBackend as S1};
use weftos_store::follower::*;
use weftos_store::overlay::S2Error;
use weftos_store::promote::{WorktreeMeta, write_meta};
use weftos_store::share::*;
use weftos_store::share_fetch::*;
use weftos_store::{RustfsBackend, RustfsConfig};

const MIN: u64 = 60_000;
const HOUR: u64 = 60 * MIN;
const DAY: u64 = 24 * HOUR;

struct Rig {
    be: RustfsBackend,
    pr: String,
    u: String,
    _secrets: tempfile::TempDir,
}

fn rig() -> Rig {
    let f = std::env::var("WEFTOS_STORE_TEST_RUSTFS_ENV").expect("WEFTOS_STORE_TEST_RUSTFS_ENV");
    let d = tempfile::tempdir().unwrap();
    let cfg = RustfsConfig::from_env_file(std::path::Path::new(&f), d.path().join("secrets")).unwrap();
    let be = RustfsBackend::new(cfg).unwrap();
    let u = ulid::Ulid::new().to_string().to_lowercase();
    let u = u[10..].to_string();
    let pr = format!("t-dp-{u}");
    AdminBackend::create_bucket(&be, &pr, &[]).unwrap();
    Rig { be, pr, u, _secrets: d }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64
}

fn actor(n: &str) -> Actor {
    Actor { agent: n.into(), host: format!("host-{n}") }
}

impl Rig {
    /// Create a worktree bucket, publish `files` at `at`, write its registry entry. Returns the entry.
    fn publish(&self, slug: &str, base: &str, head: &str, files: &[(&str, &str)], changes: &[PathChange], at: u64) -> RegistryEntry {
        let bucket = format!("t-dw-{slug}-{}", self.u);
        if !AdminBackend::bucket_exists(&self.be, &bucket).unwrap() {
            AdminBackend::create_bucket(&self.be, &bucket, &[]).unwrap();
            write_meta(
                &self.be,
                &bucket,
                &WorktreeMeta { slug: slug.into(), project: self.u.clone(), branch: slug.into(), base_commit: base.into(), created_ms: at },
            )
            .unwrap();
        }
        let d = tempfile::tempdir().unwrap();
        let mut ps = vec![];
        for (p, c) in files {
            let f = d.path().join(p);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, c).unwrap();
            ps.push(p.to_string());
        }
        let src = TreeSource {
            root: d.path(),
            paths: &ps,
            uncommitted: &[],
            changes,
            branch: slug,
            base_commit: base,
            head_commit: head,
            bundle: None,
            writer: actor("w"),
        };
        publish_tree(&self.be, &bucket, &src, &Limits::default(), at).unwrap();
        self.register(slug, &bucket)
    }

    fn register(&self, slug: &str, bucket: &str) -> RegistryEntry {
        let e = summarize(&self.be, slug, bucket).unwrap().unwrap();
        write_registry_entry(&self.be, &self.pr, &e).unwrap();
        e
    }

    fn bucket(&self, slug: &str) -> String {
        format!("t-dw-{slug}-{}", self.u)
    }
}

/// Item 4 (leftover thresholds) and item 5 (a stale claim is recoverable), with real stored records.
#[test]
#[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
fn drill_leftovers_report_each_kind_and_a_stale_claim_is_taken_over() {
    let r = rig();
    let now = now_ms();
    let doc = [("docs/plan.md", "# Plan\n")];

    r.publish("forgotten", "b", "h", &doc, &[], now - 4 * DAY);
    r.publish("unclaimed", "b", "h", &doc, &[], now - 2 * DAY);
    mark_handoff(&r.be, &r.bucket("unclaimed"), &actor("w"), "please merge", now - 2 * DAY).unwrap();
    r.register("unclaimed", &r.bucket("unclaimed"));

    r.publish("abandoned", "b", "h", &doc, &[], now - 2 * HOUR);
    let ab = r.bucket("abandoned");
    mark_handoff(&r.be, &ab, &actor("w"), "go", now - 2 * HOUR).unwrap();
    claim_merge(&r.be, &ab, &actor("m1"), now - 31 * MIN).unwrap();
    r.register("abandoned", &ab);

    r.publish("stuck", "b", "h", &doc, &[], now - 3 * DAY);
    let st = r.bucket("stuck");
    mark_handoff(&r.be, &st, &actor("w"), "go", now - 3 * DAY).unwrap();
    claim_merge(&r.be, &st, &actor("m1"), now - 3 * DAY).unwrap();
    mark_merged(&r.be, &st, &actor("m1"), "landed", now - 2 * DAY).unwrap();
    r.register("stuck", &st);

    r.publish("skewed", "b", "h", &doc, &[], now + DAY); // writer clock a day ahead
    r.publish("fresh", "b", "h", &doc, &[], now - MIN);

    let (entries, bad) = list_shared_lenient(&r.be, &r.pr).unwrap();
    assert!(bad.is_empty());
    let got: std::collections::BTreeMap<_, _> =
        leftovers(&entries, now, &LeftoverPolicy::default()).into_iter().map(|l| (l.entry.slug.clone(), l.why)).collect();
    let want: std::collections::BTreeMap<_, _> = [
        ("forgotten", Why::ForgottenOpen),
        ("unclaimed", Why::UnclaimedHandoff),
        ("abandoned", Why::AbandonedClaim),
        ("stuck", Why::StuckCleanup),
        ("skewed", Why::FutureTimestamp),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    assert_eq!(got, want, "fresh must not be reported");
    // a skewed writer's idle time is not trusted: reported as 0, not as a huge or negative age
    assert_eq!(leftovers(&entries, now, &LeftoverPolicy::default()).iter().find(|l| l.entry.slug == "skewed").unwrap().idle_ms, 0);

    // item 5: the abandoned claim (31 min old) can be taken over; a live one (just made) cannot
    let taken = claim_merge(&r.be, &ab, &actor("m2"), now).unwrap();
    assert_eq!(taken.merger, Some(actor("m2")));
    match claim_merge(&r.be, &ab, &actor("m3"), now + MIN) {
        Err(S2Error::Claimed(who)) => assert_eq!(who, "m2@host-m2"),
        o => panic!("a live claim must refuse a third merger: {o:?}"),
    }
    // the writer cannot republish over a live claim, and can once the claim is stale
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("a.md"), "# a\n").unwrap();
    let ps = vec!["a.md".to_string()];
    let src = TreeSource { root: d.path(), paths: &ps, uncommitted: &[], changes: &[], branch: "abandoned", base_commit: "b", head_commit: "h2", bundle: None, writer: actor("w") };
    assert!(matches!(publish_tree(&r.be, &ab, &src, &Limits::default(), now + 2 * MIN), Err(S2Error::Claimed(_))));
    publish_tree(&r.be, &ab, &src, &Limits::default(), now + 31 * MIN).unwrap();
}

/// Item 6: a corrupt and an unknown-schema registry entry, read from a real registry.
#[test]
#[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
fn drill_corrupt_and_unknown_schema_registry_entries_are_reported_not_fatal() {
    let r = rig();
    let now = now_ms();
    r.publish("good", "b", "h1", &[("docs/a.md", "# A\n")], &[], now);
    S1::put(&r.be, &r.pr, "worktrees/corrupt.json", b"{ this is not json", false).unwrap();
    S1::put(&r.be, &r.pr, "worktrees/future.json", br#"{"schema":99,"slug":"future","kind":"signed-head","payload":{}}"#, false).unwrap();

    assert!(list_shared(&r.be, &r.pr).is_err(), "the strict listing refuses a corrupt registry");
    let (ok, bad) = list_shared_lenient(&r.be, &r.pr).unwrap();
    assert_eq!(ok.iter().map(|e| e.slug.as_str()).collect::<Vec<_>>(), ["good"]);
    assert_eq!(bad, ["corrupt", "future"]);

    let mut st = FollowState::default();
    let c1 = poll(&r.be, &r.pr, &mut st).unwrap();
    assert!(c1.iter().any(|c| matches!(c, Change::New(e) if e.slug == "good")));
    assert!(c1.iter().any(|c| matches!(c, Change::Unreadable { slug } if slug == "corrupt")));
    assert!(c1.iter().any(|c| matches!(c, Change::Unreadable { slug } if slug == "future")));
    // reported every poll until fixed, and the good worktree is not re-reported
    let c2 = poll(&r.be, &r.pr, &mut st).unwrap();
    assert_eq!(c2.len(), 2);
    assert!(c2.iter().all(|c| matches!(c, Change::Unreadable { .. })));
    // fixing the corrupt entry (the daemon rewrites it) clears the report
    S1::delete(&r.be, &r.pr, "worktrees/corrupt.json").unwrap();
    S1::delete(&r.be, &r.pr, "worktrees/future.json").unwrap();
    assert!(poll(&r.be, &r.pr, &mut st).unwrap().is_empty());
}

/// Item 6: an empty or wrong project bucket is an error, not "every worktree is gone".
#[test]
#[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
fn drill_empty_or_wrong_project_bucket_is_an_error_not_mass_gone() {
    let r = rig();
    r.publish("a", "b", "h", &[("docs/a.md", "# A\n")], &[], now_ms());
    r.publish("b", "b", "h", &[("docs/b.md", "# B\n")], &[], now_ms());
    let mut st = FollowState::default();
    assert_eq!(poll(&r.be, &r.pr, &mut st).unwrap().len(), 2);

    // a project bucket that exists but is empty (the wrong project, or a wiped registry)
    let empty = format!("t-de-{}", r.u);
    AdminBackend::create_bucket(&r.be, &empty, &[]).unwrap();
    let before = st.clone();
    assert!(poll(&r.be, &empty, &mut st).is_err(), "empty registry with known worktrees must not read as all gone");
    assert_eq!(st, before, "state untouched by the refused poll");
    // a bucket that does not exist at all
    assert!(poll(&r.be, &format!("t-dnone-{}", r.u), &mut st).is_err());
    assert_eq!(st, before);
    // the operator can confirm the registry really is empty
    let gone = poll_confirmed(&r.be, &empty, &mut st).unwrap();
    assert_eq!(gone.len(), 2);
    assert!(gone.iter().all(|c| matches!(c, Change::Gone { .. })));
}

/// Item 6: two follow runs sharing one state file. The file is written atomically, so it never
/// parses as garbage, but the runs do not lock: each may report the same change.
#[test]
#[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
fn drill_two_follow_runs_sharing_a_state_file_never_corrupt_it() {
    let r = rig();
    r.publish("a", "b", "h1", &[("docs/a.md", "# A\n")], &[], now_ms());
    let dir = tempfile::tempdir().unwrap();
    let sf = dir.path().join("follow.json");
    let reported: std::sync::Mutex<Vec<String>> = Default::default();
    std::thread::scope(|s| {
        for who in ["run1", "run2"] {
            let (be, pr, sf, reported) = (&r.be, &r.pr, &sf, &reported);
            s.spawn(move || {
                for _ in 0..12 {
                    let mut st = FollowState::load(sf).expect("the shared state file must always parse");
                    for c in poll(be, pr, &mut st).unwrap() {
                        if let Change::New(e) = c {
                            reported.lock().unwrap().push(format!("{who}:{}", e.slug));
                        }
                    }
                    st.save(sf).unwrap();
                }
            });
        }
    });
    let st = FollowState::load(&sf).unwrap();
    assert!(st.seen.contains_key("a"));
    let n = reported.lock().unwrap().len();
    // Not asserted to be 1: without a lock both runs may report it. Print the count as evidence.
    eprintln!("EVIDENCE two-follow-runs: slug 'a' reported as New {n} time(s) across 24 polls");
    assert!((1..=2).contains(&n));
}

/// Item 6: `overlaps` on two real worktrees with different bases. Measures how many reported paths
/// are not conflicts: files that differ only because the base moved.
#[test]
#[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
fn drill_overlaps_with_different_bases_over_reports() {
    let r = rig();
    let now = now_ms();
    let common: Vec<(String, String)> = (0..20).map(|i| (format!("src/m{i}.rs"), format!("// module {i}\n"))).collect();
    // worktree A branched from base1 and changed m0, m1, m2.
    let a_files: Vec<(String, String)> = common
        .iter()
        .map(|(p, c)| (p.clone(), if ["src/m0.rs", "src/m1.rs", "src/m2.rs"].contains(&p.as_str()) { format!("{c}// A edit\n") } else { c.clone() }))
        .collect();
    // worktree B branched from base2, where upstream had changed m10..m14; B changed m2 (a real
    // conflict with A) and m3 (no conflict).
    let b_files: Vec<(String, String)> = common
        .iter()
        .map(|(p, c)| {
            let up = (10..15).any(|i| p == &format!("src/m{i}.rs"));
            let own = ["src/m2.rs", "src/m3.rs"].contains(&p.as_str());
            (p.clone(), format!("{c}{}{}", if up { "// upstream\n" } else { "" }, if own { "// B edit\n" } else { "" }))
        })
        .collect();
    let refs = |v: &[(String, String)]| -> Vec<(&'static str, &'static str)> {
        v.iter().map(|(p, c)| (&*Box::leak(p.clone().into_boxed_str()), &*Box::leak(c.clone().into_boxed_str()))).collect()
    };
    let ch = |p: &str| PathChange { path: p.into(), kind: ChangeKind::Modified };
    let a_changes: Vec<_> = ["src/m0.rs", "src/m1.rs", "src/m2.rs"].iter().map(|p| ch(p)).collect();
    let b_changes: Vec<_> = ["src/m2.rs", "src/m3.rs"].iter().map(|p| ch(p)).collect();
    let ea = r.publish("wa", "base1", "ha", &refs(&a_files), &a_changes, now);
    let eb = r.publish("wb", "base2", "hb", &refs(&b_files), &b_changes, now);
    let rep = overlaps(&r.be, &ea, &eb).unwrap();
    assert!(!rep.same_base, "different bases are flagged");
    let paths: Vec<&str> = rep.overlaps.iter().map(|o| o.path.as_str()).collect();
    let real = ["src/m2.rs"];
    let fp = paths.iter().filter(|p| !real.contains(p)).count();
    eprintln!("EVIDENCE overlaps-different-bases: reported {} paths, {} real conflict, {} false positives ({}%): {:?}", paths.len(), real.len(), fp, fp * 100 / paths.len(), paths);
    assert!(paths.contains(&"src/m2.rs"), "the real conflict is always found");
    // Measured on 2026-11-08 before the change-list narrowing landed from the S1 land (the Mac S2 line):
    // 9 reported, 1 real, 8 false positives (89%). With both manifests carrying their `changes` lists,
    // `overlaps` now reports only a path changed on both sides: 1 reported, 0 false positives.
    assert_eq!((paths.len(), fp), (1, 0));
}

/// Item 6: a `sync-docs` run that died after tracking new docs but before fetching them, followed by
/// the writer deleting docs. The next run must converge to exactly the docs that still exist.
#[test]
#[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
fn drill_interrupted_sync_docs_then_a_deleted_doc_converges() {
    let r = rig();
    let now = now_ms();
    let e = r.publish("sd", "b", "h1", &[("docs/a.md", "# A\n"), ("docs/b.md", "# B\n")], &[], now);
    let cache = tempfile::tempdir().unwrap();
    sync_docs(&r.be, &e, cache.path()).unwrap();
    let dir = cache.path().join("sd");
    assert!(dir.join("docs/a.md").exists() && dir.join("docs/b.md").exists());

    // the writer adds docs/c.md; the follower's next run tracks it (index first) and dies before
    // fetching it, leaving a half-written b.md behind
    let e2 = r.publish("sd", "b", "h2", &[("docs/a.md", "# A\n"), ("docs/b.md", "# B\n"), ("docs/c.md", "# C\n")], &[], now + MIN);
    std::fs::write(dir.join(".wt-index.json"), br#"["docs/a.md","docs/b.md","docs/c.md"]"#).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.join("docs/b.md"), std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    std::fs::write(dir.join("docs/b.md"), "# B (half written").unwrap();
    assert!(!dir.join("docs/c.md").exists());
    let _ = e2;

    // then the writer deletes b.md and c.md (republish with only a.md)
    let e3 = r.publish("sd", "b", "h3", &[("docs/a.md", "# A\n")], &[], now + 2 * MIN);
    assert_eq!(e3.doc_count, 1);
    let rep = sync_docs(&r.be, &e3, cache.path()).unwrap();
    let mut left: Vec<String> = Vec::new();
    for f in walk(&dir) {
        left.push(f.strip_prefix(&dir).unwrap().to_string_lossy().into_owned());
    }
    left.sort();
    assert_eq!(left, [".wt-index.json", "docs/a.md"], "deleted and half-written docs are gone: {rep:?}");
    assert_eq!(std::fs::read_to_string(dir.join("docs/a.md")).unwrap(), "# A\n");
}

fn walk(p: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut v = vec![];
    for e in std::fs::read_dir(p).unwrap() {
        let e = e.unwrap().path();
        if e.is_dir() {
            v.extend(walk(&e));
        } else {
            v.push(e);
        }
    }
    v
}
