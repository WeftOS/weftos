//! WO-069 lane Q: the product defects the follower drills found (`FINDING|6x|...` lines in `s2_drills_follower.rs` and `s2_drills_follower_more.rs`),
//! each as a test of the EXPECTED behaviour, `#[ignore]`d with the finding named. They fail today by design; remove the `#[ignore]` when the product is
//! fixed. Run them all with `cargo test -p weftos-store --test s2_drills_follower_findings -- --ignored` (expect 9 failures until fixed).
#[macro_use]
#[path = "s2_drills_fcommon/mod.rs"]
mod fc;
use fc::*;
use weftos_store::follower::*;
use weftos_store::orphan::{OrphanPolicy, mark_orphan};
use weftos_store::overlay::ObjectBackend as S2;
use weftos_store::registry_sign::{poll_verified, write_registry_entry_signed};
use weftos_store::share::*;
use weftos_store::share_fetch::*;
use weftos_store::testing::MemBackend;

#[test]
#[ignore = "FINDING 6a-1: a claim stamped a day ahead lets the writer republish, and the claimer still marks the old head merged"]
fn a_merge_is_refused_when_the_head_changed_under_the_claim() {
    let be = MemBackend::new();
    let env = Env::new(&be, None);
    let (now, ahead) = (T0 + 10 * DAY, T0 + 11 * DAY);
    let w = up(&env, "under", &[("docs/u.md", "# U\n")], &Pub { now, ..Pub::default() });
    mark_handoff(&be, &w.bucket, &actor("writer"), "go", now).unwrap();
    claim_merge(&be, &w.bucket, &actor("fast"), ahead).unwrap();
    let _ = push(&be, &w.bucket, &tree(&[("docs/u.md", "# U v2\n")]), &Pub { head: "h2", now: now + 1, ..Pub::default() });
    assert!(mark_merged_at(&be, &w.bucket, &actor("fast"), Some(1), "merged h1", ahead).is_err(), "the merge of h1 must not be recorded over a published h2");
}

#[test]
fn an_orphan_stamped_ahead_is_still_reported_as_an_orphan() {
    let be = MemBackend::new();
    let env = Env::new(&be, None);
    let now = T0 + 10 * DAY;
    up(&env, "orph", &[("docs/o.md", "# O\n")], &Pub { now: now + 3 * DAY, ..Pub::default() });
    let marked = mark_orphan(&be, &env.pr, "orph", now + 25 * 3600 * 1000, now, &OrphanPolicy::default()).unwrap();
    let l = leftovers(&[marked], now + 25 * 3600 * 1000, &LeftoverPolicy::default());
    assert!(l.iter().any(|x| x.why == Why::Orphan), "{:?}", l.iter().map(|x| x.why).collect::<Vec<_>>());
}

fn pinned_entry(env: &Env<'_, MemBackend>, state: State) -> RegistryEntry {
    RegistryEntry {
        slug: "pin".into(), bucket: format!("w-{}-pin", env.u), branch: "b".into(), head_commit: "h".into(), state, writer: actor("w"), merger: None,
        doc_count: 0, file_count: 0, updated_ms: u64::MAX, orphan: None,
    }
}

#[test]
fn an_older_signed_entry_is_refused_even_after_a_far_future_stamp() {
    let be = MemBackend::new();
    let env = Env::new(&be, None);
    let pr = env.pr.as_str();
    write_registry_entry_signed(&be, pr, &pinned_entry(&env, State::Open), &Toy).unwrap();
    let (e, s) = (S2::get(&be, pr, "worktrees/pin.json").unwrap().unwrap(), S2::get(&be, pr, "worktrees.sig/pin.json").unwrap().unwrap());
    let mut st = FollowState::default();
    poll_verified(&be, pr, &mut st, &Toy, true, false).unwrap();
    write_registry_entry_signed(&be, pr, &pinned_entry(&env, State::Merged), &Toy).unwrap();
    poll_verified(&be, pr, &mut st, &Toy, true, false).unwrap();
    S2::put(&be, pr, "worktrees/pin.json", &e).unwrap();
    S2::put(&be, pr, "worktrees.sig/pin.json", &s).unwrap();
    assert_eq!(kinds(&poll_verified(&be, pr, &mut st, &Toy, true, false).unwrap()), ["bad:pin"]);
}

#[test]
#[ignore = "FINDING 6b-1: the manifest schema number is never checked on read"]
fn a_manifest_from_a_newer_schema_is_not_served_as_if_current() {
    let be = MemBackend::new();
    let env = Env::new(&be, None);
    let e = up(&env, "good", &[("docs/g.md", "# G\n")], &Pub::default());
    let mut m: serde_json::Value = serde_json::from_slice(&S2::get(&be, &e.bucket, MANIFEST_KEY).unwrap().unwrap()).unwrap();
    m["schema"] = 99.into();
    S2::put(&be, &e.bucket, MANIFEST_KEY, &serde_json::to_vec(&m).unwrap()).unwrap();
    assert_eq!(search_docs(&be, &[e], "g").unreadable, ["good"]);
}

fn three(env: &Env<'_, weftos_store::FsBackend>) {
    for s in ["a", "b", "c"] {
        up(env, s, &[("docs/x.md", "# X\n")], &Pub::default());
    }
    write_digest(env.be, &env.pr).unwrap();
}

#[test]
#[ignore = "FINDING 6b-2: a stale digest hides a removal from a knowing follower and shows a new one an unreadable slug"]
fn a_digest_slug_with_no_object_is_gone_not_unreadable() {
    let d = tempfile::tempdir().unwrap();
    let be = weftos_store::FsBackend::open(d.path()).unwrap();
    let env = Env::new(&be, Some(&be));
    three(&env);
    let mut st = FollowState::default();
    poll_fast(&be, &env.pr, &mut st, false, true, 2).unwrap();
    S2::delete(&be, &env.pr, "worktrees/b.json").unwrap(); // the digest rewrite failed
    assert!(kinds(&poll_fast(&be, &env.pr, &mut st, false, true, 2).unwrap()).contains(&"gone:b".to_string()));
    let fresh = kinds(&poll_fast(&be, &env.pr, &mut FollowState::default(), false, true, 2).unwrap());
    assert!(!fresh.contains(&"bad:b".to_string()), "{fresh:?}");
}

#[test]
#[ignore = "FINDING 6c: a follower believes a partial or empty digest fully"]
fn a_partial_digest_does_not_make_a_knowing_follower_report_worktrees_gone() {
    let d = tempfile::tempdir().unwrap();
    let be = weftos_store::FsBackend::open(d.path()).unwrap();
    let env = Env::new(&be, Some(&be));
    three(&env);
    let mut st = FollowState::default();
    poll_fast(&be, &env.pr, &mut st, false, false, 2).unwrap();
    let partial = serde_json::json!({"schema": 1, "entries": {"a": S2::head(&be, &env.pr, "worktrees/a.json").unwrap().unwrap().etag}});
    S2::put(&be, &env.pr, DIGEST_KEY, &serde_json::to_vec(&partial).unwrap()).unwrap();
    let r = poll_fast(&be, &env.pr, &mut st, false, true, 2);
    assert!(r.is_err() || !kinds(&r.unwrap()).iter().any(|k| k.starts_with("gone:")), "two of three worktrees vanished on the say-so of a hint");
}

#[test]
#[ignore = "FINDING 6e: a damaged or missing .wt-index.json leaves deleted docs in the mirror"]
fn a_missing_mirror_index_still_prunes_docs_deleted_upstream() {
    let be = MemBackend::new();
    let env = Env::new(&be, None);
    let e1 = up(&env, "m", &[("docs/a.md", "# A\n"), ("docs/b.md", "# B\n")], &Pub::default());
    let cache = tempfile::tempdir().unwrap();
    sync_docs(&be, &e1, cache.path()).unwrap();
    std::fs::remove_file(cache.path().join("m/.wt-index.json")).unwrap();
    push(&be, &e1.bucket, &tree(&[("docs/a.md", "# A\n")]), &Pub { head: "h2", now: T0 + 5, ..Pub::default() }).unwrap();
    let e2 = register(&be, &env.pr, "m", &e1.bucket);
    sync_docs(&be, &e2, cache.path()).unwrap();
    assert!(!cache.path().join("m/docs/b.md").exists(), "b.md was deleted upstream");
}

#[test]
#[ignore = "FINDING 6f: a worktree with no changes is indistinguishable from an old manifest, so overlaps reports false conflicts"]
fn a_worktree_that_changed_nothing_has_no_overlaps() {
    let be = MemBackend::new();
    let env = Env::new(&be, None);
    let base: Vec<(String, Vec<u8>)> = (0..10).map(|i| (format!("src/f{i}.rs"), format!("base {i}").into_bytes())).collect();
    let mut a = base.clone();
    a[3].1 = b"a edit".to_vec();
    let (wa, wb) = (env.wt("a"), env.wt("b"));
    init_wt(&be, &wa, "a", "c1");
    init_wt(&be, &wb, "b", "c1");
    push(&be, &wa, &tree_bytes(&a), &Pub { base: "c1", changes: &[ch("src/f3.rs", ChangeKind::Modified)], ..Pub::default() }).unwrap();
    push(&be, &wb, &tree_bytes(&base), &Pub { base: "c1", ..Pub::default() }).unwrap();
    let (ea, eb) = (register(&be, &env.pr, "a", &wa), register(&be, &env.pr, "b", &wb));
    assert!(overlaps(&be, &ea, &eb).unwrap().overlaps.is_empty());
}

#[test]
#[ignore = "FINDING 6g: the last saver of a shared FollowState file can lower the stored signature generation"]
fn saving_an_older_view_does_not_lower_a_remembered_generation() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("follow.json");
    let mut newer = FollowState::default();
    newer.generations.insert("wt".into(), 200);
    newer.save(&path).unwrap();
    let mut older = FollowState::default();
    older.generations.insert("wt".into(), 100);
    older.save(&path).unwrap();
    assert!(FollowState::load(&path).unwrap().generations["wt"] >= 200);
}
