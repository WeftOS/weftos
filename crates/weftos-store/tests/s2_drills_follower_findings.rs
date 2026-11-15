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
#[ignore = "FINDING 6a-2: an orphan whose entry is stamped far ahead is reported as FutureTimestamp, not Orphan"]
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
#[ignore = "FINDING 6a-3: a u64::MAX stamp pins the signature generation, so an older signed entry replays without being refused"]
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
fn a_digest_slug_with_no_object_is_gone_not_unreadable() {
    let d = tempfile::tempdir().unwrap();
    let be = weftos_store::FsBackend::open(d.path()).unwrap();
    let env = Env::new(&be, Some(&be));
    three(&env);
    let mut st = FollowState::default();
    poll_fast(&be, &env.pr, &mut st, false, true, 2).unwrap();
    S2::delete(&be, &env.pr, "worktrees/b.json").unwrap(); // the digest rewrite failed
    let fresh = kinds(&poll_fast(&be, &env.pr, &mut FollowState::default(), false, true, 2).unwrap());
    assert!(!fresh.contains(&"bad:b".to_string()) && !fresh.contains(&"new:b".to_string()), "{fresh:?}");
    // a follower whose recorded etag for b differs from the digest's fetches it, finds it absent, and reports it gone
    st.seen.get_mut("b").unwrap().etag = Some("older".into());
    assert!(kinds(&poll_fast(&be, &env.pr, &mut st, false, true, 2).unwrap()).contains(&"gone:b".to_string()));
}

#[test]
#[ignore = "NOT PROMISED by ADR 126 4a: a knowing follower with an unchanged digest etag cannot see a removal the digest does not show; the digest-hit poll is one GET (ADR 4a poll cost)"]
fn a_knowing_follower_sees_a_removal_even_when_the_digest_did_not_change() {
    let d = tempfile::tempdir().unwrap();
    let be = weftos_store::FsBackend::open(d.path()).unwrap();
    let env = Env::new(&be, Some(&be));
    three(&env);
    let mut st = FollowState::default();
    poll_fast(&be, &env.pr, &mut st, false, true, 2).unwrap();
    S2::delete(&be, &env.pr, "worktrees/b.json").unwrap();
    assert!(kinds(&poll_fast(&be, &env.pr, &mut st, false, true, 2).unwrap()).contains(&"gone:b".to_string()));
}

#[test]
fn a_digest_slug_with_a_corrupt_object_is_still_unreadable() {
    let d = tempfile::tempdir().unwrap();
    let be = weftos_store::FsBackend::open(d.path()).unwrap();
    let env = Env::new(&be, Some(&be));
    three(&env);
    S2::put(&be, &env.pr, "worktrees/b.json", b"{ nope").unwrap(); // present but damaged, digest stale
    let fresh = kinds(&poll_fast(&be, &env.pr, &mut FollowState::default(), false, true, 2).unwrap());
    assert!(fresh.contains(&"bad:b".to_string()), "{fresh:?}");
}

#[test]
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
fn a_digest_hit_with_nothing_changed_is_still_one_get_and_a_real_removal_is_still_reported() {
    let d = tempfile::tempdir().unwrap();
    let fs = weftos_store::FsBackend::open(d.path()).unwrap();
    let env = Env::new(&fs, Some(&fs));
    three(&env);
    let be = weftos_store::testing::CountingBackend::new(weftos_store::FsBackend::open(d.path()).unwrap());
    let mut st = FollowState::default();
    poll_fast(&be, &env.pr, &mut st, false, true, 2).unwrap();
    be.reset();
    assert!(poll_fast(&be, &env.pr, &mut st, false, true, 2).unwrap().is_empty());
    let c = be.counts();
    assert_eq!((c.get, c.head, c.list), (1, 0, 0), "{c:?}");
    // a removal the daemon did publish (digest rewritten): reported gone, via the listing fallback
    remove_registry_entry(&be, &env.pr, "b").unwrap();
    assert_eq!(kinds(&poll_fast(&be, &env.pr, &mut st, false, true, 2).unwrap()), ["gone:b"]);
}

#[test]
fn an_empty_digest_does_not_blind_a_knowing_follower_when_the_registry_is_populated() {
    let d = tempfile::tempdir().unwrap();
    let be = weftos_store::FsBackend::open(d.path()).unwrap();
    let env = Env::new(&be, Some(&be));
    for s in ["a", "b"] {
        up(&env, s, &[("docs/x.md", "# X\n")], &Pub::default());
    }
    let mut st = FollowState::default();
    poll_fast(&be, &env.pr, &mut st, false, false, 2).unwrap();
    S2::put(&be, &env.pr, DIGEST_KEY, br#"{"schema":1,"entries":{}}"#).unwrap();
    assert!(poll_fast(&be, &env.pr, &mut st, false, true, 2).unwrap().is_empty());
}

#[test]
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

/// Sync `m` with two docs, then lose the index, plant `extra`, delete b.md upstream and sync again.
fn lose_index_then_sync(plant: impl FnOnce(&std::path::Path)) -> (tempfile::TempDir, std::path::PathBuf) {
    let be = MemBackend::new();
    let env = Env::new(&be, None);
    let e1 = up(&env, "m", &[("docs/a.md", "# A\n"), ("docs/b.md", "# B\n")], &Pub::default());
    let cache = tempfile::tempdir().unwrap();
    sync_docs(&be, &e1, cache.path()).unwrap();
    let mirror = cache.path().join("m");
    std::fs::write(mirror.join(".wt-index.json"), b"[\"docs/a").unwrap(); // truncated
    plant(&mirror);
    push(&be, &e1.bucket, &tree(&[("docs/a.md", "# A\n")]), &Pub { head: "h2", now: T0 + 5, ..Pub::default() }).unwrap();
    let e2 = register(&be, &env.pr, "m", &e1.bucket);
    sync_docs(&be, &e2, cache.path()).unwrap();
    assert!(!mirror.join("docs/b.md").exists());
    assert!(mirror.join("docs/a.md").exists());
    (cache, mirror)
}

#[test]
fn a_rebuilt_index_never_deletes_what_the_mirror_did_not_write() {
    let (_keep, mirror) = lose_index_then_sync(|m| {
        std::fs::create_dir_all(m.join("docs")).unwrap();
        std::fs::write(m.join("docs/notes-by-me.md"), b"mine").unwrap(); // doc-shaped but writable: not the mirror's
        std::fs::write(m.join("scratch.bin"), b"x").unwrap(); // not doc-shaped
    });
    assert_eq!(std::fs::read(mirror.join("docs/notes-by-me.md")).unwrap(), b"mine");
    assert!(mirror.join("scratch.bin").exists());
}

#[cfg(unix)]
#[test]
fn a_rebuilt_index_does_not_follow_a_symlink_out_of_the_mirror() {
    let outside = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(outside.path().join("docs")).unwrap();
    let victim = outside.path().join("docs/victim.md");
    std::fs::write(&victim, b"keep").unwrap();
    let mut p = std::fs::metadata(&victim).unwrap().permissions();
    p.set_readonly(true);
    std::fs::set_permissions(&victim, p).unwrap();
    let out_path = outside.path().to_path_buf();
    let (_keep, mirror) = lose_index_then_sync(|m| {
        std::os::unix::fs::symlink(&out_path, m.join("link")).unwrap(); // a directory symlink
        std::os::unix::fs::symlink(&victim, m.join("docs-link.md")).unwrap(); // a file symlink
    });
    assert_eq!(std::fs::read(&victim).unwrap(), b"keep", "nothing outside the mirror is touched");
    assert!(std::fs::symlink_metadata(mirror.join("link")).is_ok());
}

#[test]
fn a_rebuilt_index_prunes_nested_docs_too() {
    let be = MemBackend::new();
    let env = Env::new(&be, None);
    let e1 = up(&env, "m", &[("docs/a.md", "# A\n"), ("docs/sub/deep/c.md", "# C\n")], &Pub::default());
    let cache = tempfile::tempdir().unwrap();
    sync_docs(&be, &e1, cache.path()).unwrap();
    std::fs::write(cache.path().join("m/.wt-index.json"), b"").unwrap();
    push(&be, &e1.bucket, &tree(&[("docs/a.md", "# A\n")]), &Pub { head: "h2", now: T0 + 5, ..Pub::default() }).unwrap();
    sync_docs(&be, &register(&be, &env.pr, "m", &e1.bucket), cache.path()).unwrap();
    assert!(!cache.path().join("m/docs/sub/deep/c.md").exists());
}

#[test]
#[ignore = "FINDING 6f (not fixed in WO-069 W2): needs an additive manifest field `changes_recorded` AND a publish-side signal that the caller computed the list (TreeSource gains a field/Option, ~20 literals plus store_worktree_dispatch and the hook that sends changes-z); see lane-w2.md"]
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

#[test]
fn a_newer_manifest_error_names_both_schema_numbers_and_the_current_one_still_reads() {
    let be = MemBackend::new();
    let env = Env::new(&be, None);
    let e = up(&env, "good", &[("docs/g.md", "# G\n")], &Pub::default());
    assert!(weftos_store::share::read_manifest(&be, &e.bucket).unwrap().is_some());
    let mut m: serde_json::Value = serde_json::from_slice(&S2::get(&be, &e.bucket, MANIFEST_KEY).unwrap().unwrap()).unwrap();
    m["schema"] = (weftos_store::share::SCHEMA + 1).into();
    S2::put(&be, &e.bucket, MANIFEST_KEY, &serde_json::to_vec(&m).unwrap()).unwrap();
    let msg = weftos_store::share::read_manifest(&be, &e.bucket).unwrap_err().to_string();
    assert!(msg.contains(&(weftos_store::share::SCHEMA + 1).to_string()) && msg.contains(&weftos_store::share::SCHEMA.to_string()), "{msg}");
    m["schema"] = 0.into(); // an older schema is accepted
    S2::put(&be, &e.bucket, MANIFEST_KEY, &serde_json::to_vec(&m).unwrap()).unwrap();
    assert!(weftos_store::share::read_manifest(&be, &e.bucket).unwrap().is_some());
}
