//! L1 follower view tests: visibility before merge, change polling, leftovers, overlaps.
use weftos_store::follower::*;
use weftos_store::overlay::{ObjectBackend, ObjectMeta, Result as R};
use weftos_store::promote::{WorktreeMeta, write_meta};
use weftos_store::share::*;
use weftos_store::share_fetch::*;
use weftos_store::testing::MemBackend;

const PR: &str = "p-01abc";
fn actor(n: &str) -> Actor {
    Actor {
        agent: n.into(),
        host: "h".into(),
    }
}

/// Publish a worktree with the given files and register it, as the writer + daemon would.
fn publish(
    b: &MemBackend,
    slug: &str,
    head: &str,
    files: &[(&str, &str)],
    now: u64,
) -> RegistryEntry {
    publish_c(b, slug, head, files, &[], now)
}

fn publish_c(
    b: &MemBackend,
    slug: &str,
    head: &str,
    files: &[(&str, &str)],
    changes: &[PathChange],
    now: u64,
) -> RegistryEntry {
    let d = tempfile::tempdir().unwrap();
    let mut ps = vec![];
    for (p, c) in files {
        let f = d.path().join(p);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, c).unwrap();
        ps.push(p.to_string());
    }
    let bucket = format!("w-{slug}");
    let src = TreeSource {
        root: d.path(),
        paths: &ps,
        uncommitted: &[],
        changes,
        branch: slug,
        base_commit: "base",
        head_commit: head,
        bundle: None,
        writer: actor("w"),
    };
    write_meta(
        b,
        &bucket,
        &WorktreeMeta {
            slug: slug.into(),
            project: "01abc".into(),
            branch: slug.into(),
            base_commit: "base".into(),
            created_ms: 0,
        },
    )
    .unwrap();
    publish_tree(b, &bucket, &src, &Limits::default(), now).unwrap();
    let e = summarize(b, slug, &bucket).unwrap().unwrap();
    write_registry_entry(b, PR, &e).unwrap();
    e
}

#[test]
fn another_node_sees_unmerged_docs_before_any_merge() {
    let b = MemBackend::new();
    publish(
        &b,
        "tapestry",
        "h1",
        &[
            (
                "docs/grok-build-fork/rooms.md",
                "# Rooms and doors contract\n",
            ),
            ("src/a.rs", "x"),
        ],
        1_000,
    );
    publish(
        &b,
        "other",
        "h2",
        &[("docs/plan.md", "# Other plan\n")],
        1_000,
    );
    let entries = list_shared(&b, PR).unwrap();
    let r = search_docs(&b, &entries, "rooms and doors");
    assert_eq!(r.hits.len(), 1);
    assert_eq!(
        (r.hits[0].slug.as_str(), r.hits[0].path.as_str()),
        ("tapestry", "docs/grok-build-fork/rooms.md")
    );
    assert!(r.unreadable.is_empty());
    // all docs across worktrees, deterministic order
    let all = search_docs(&b, &entries, "");
    assert_eq!(
        all.hits.iter().map(|h| h.slug.as_str()).collect::<Vec<_>>(),
        ["other", "tapestry"]
    );
}

#[test]
fn merged_worktrees_are_not_searched_and_unreadable_ones_are_reported() {
    let b = MemBackend::new();
    let mut e = publish(&b, "done", "h", &[("docs/x.md", "# X\n")], 1);
    e.state = State::Merged;
    let ghost = RegistryEntry {
        slug: "ghost".into(),
        bucket: "w-ghost".into(),
        state: State::Open,
        ..e.clone()
    };
    let r = search_docs(&b, &[e, ghost], "");
    assert!(r.hits.is_empty());
    assert_eq!(r.unreadable, ["ghost"]);
}

#[test]
fn poll_reports_new_moved_state_and_gone_once() {
    let b = MemBackend::new();
    let mut st = FollowState::default();
    publish(&b, "a", "h1", &[("docs/a.md", "# A\n")], 1_000);
    let c = poll(&b, PR, &mut st).unwrap();
    assert!(matches!(c.as_slice(), [Change::New(e)] if e.slug == "a"));
    assert!(
        poll(&b, PR, &mut st).unwrap().is_empty(),
        "nothing new, nothing reported"
    );
    publish(&b, "a", "h2", &[("docs/a.md", "# A2\n")], 2_000);
    let c = poll(&b, PR, &mut st).unwrap();
    assert!(
        matches!(c.as_slice(), [Change::HeadMoved { from, entry }] if from == "h1" && entry.head_commit == "h2")
    );
    mark_handoff(&b, "w-a", &actor("w"), "ready", 3_000).unwrap();
    let e = summarize(&b, "a", "w-a").unwrap().unwrap();
    write_registry_entry(&b, PR, &e).unwrap();
    let c = poll(&b, PR, &mut st).unwrap();
    assert!(
        matches!(c.as_slice(), [Change::StateChanged { from: State::Open, entry }] if entry.state == State::Handoff)
    );
    remove_registry_entry(&b, PR, "a").unwrap();
    assert_eq!(
        poll_confirmed(&b, PR, &mut st).unwrap(),
        [Change::Gone { slug: "a".into() }]
    );
    assert!(st.seen.is_empty());
    // state survives a JSON round trip (persisted between polls)
    let j = serde_json::to_string(&st).unwrap();
    assert_eq!(serde_json::from_str::<FollowState>(&j).unwrap(), st);
}

#[test]
fn a_new_follower_sees_everything_as_new() {
    let b = MemBackend::new();
    publish(&b, "a", "h", &[("docs/a.md", "# A\n")], 1);
    publish(&b, "b", "h", &[("docs/b.md", "# B\n")], 1);
    assert_eq!(poll(&b, PR, &mut FollowState::default()).unwrap().len(), 2);
}

#[test]
fn sync_docs_mirrors_read_only_and_prunes_removed_docs() {
    let b = MemBackend::new();
    let e = publish(
        &b,
        "a",
        "h1",
        &[
            ("docs/a.md", "# A\n"),
            ("docs/b.md", "# B\n"),
            ("src/x.rs", "code"),
        ],
        1,
    );
    let cache = tempfile::tempdir().unwrap();
    let r = sync_docs(&b, &e, cache.path()).unwrap();
    assert_eq!(r.written.len(), 2);
    let a = cache.path().join("a/docs/a.md");
    assert_eq!(std::fs::read_to_string(&a).unwrap(), "# A\n");
    assert!(std::fs::metadata(&a).unwrap().permissions().readonly());
    assert!(!cache.path().join("a/src/x.rs").exists(), "docs only");
    let e = publish(
        &b,
        "a",
        "h2",
        &[("docs/a.md", "# A changed\n"), ("src/x.rs", "code")],
        2,
    );
    sync_docs(&b, &e, cache.path()).unwrap();
    assert_eq!(
        std::fs::read_to_string(&a).unwrap(),
        "# A changed\n",
        "read-only files still refresh"
    );
    assert!(
        !cache.path().join("a/docs/b.md").exists(),
        "removed doc pruned"
    );
    // a hostile slug cannot escape the cache
    let mut bad = e.clone();
    bad.slug = "../x".into();
    assert!(sync_docs(&b, &bad, cache.path()).is_err());
}

#[test]
fn a_forged_manifest_cannot_touch_the_followers_own_files_or_leave_the_cache() {
    let b = MemBackend::new();
    let e = publish(&b, "a", "h1", &[("docs/a.md", "# A\n")], 1);
    let cache = tempfile::tempdir().unwrap();
    sync_docs(&b, &e, cache.path()).unwrap();
    // someone with direct write access to the worktree bucket lists the follower's control file
    // and names that climb out of the cache as documents
    let mut m = read_manifest(&b, &e.bucket).unwrap().unwrap();
    for (path, body) in [(".wt-index.json", "[\"docs/a.md\"]"), ("../escape.md", "x"), (".wt-other/n.md", "x"), ("docs/../../up.md", "x")] {
        b.put(&e.bucket, &format!("{TREE_PREFIX}{path}"), body.as_bytes()).unwrap();
        m.files.push(FileEntry { path: path.into(), size: body.len() as u64, sha256: sha256_hex(body.as_bytes()) });
        m.docs.push(DocEntry { path: path.into(), title: None });
    }
    b.put(&e.bucket, MANIFEST_KEY, &serde_json::to_vec(&m).unwrap()).unwrap();
    let r = sync_docs(&b, &e, cache.path()).unwrap();
    assert_eq!(r.written, Vec::<String>::new(), "the honest doc is unchanged: {r:?}");
    assert!(!cache.path().join("escape.md").exists() && !cache.path().join("up.md").exists());
    assert!(!cache.path().join("a/.wt-other").exists());
    // the follower's own index is the one it wrote, not the forged body, and stays writable
    let idx = cache.path().join("a/.wt-index.json");
    assert_eq!(std::fs::read_to_string(&idx).unwrap(), "[\"docs/a.md\"]");
    assert!(!std::fs::metadata(&idx).unwrap().permissions().readonly());
    // and later syncs still work
    let e2 = publish(&b, "a", "h2", &[("docs/a.md", "# A2\n")], 2);
    sync_docs(&b, &e2, cache.path()).unwrap();
    assert_eq!(std::fs::read_to_string(cache.path().join("a/docs/a.md")).unwrap(), "# A2\n");
}

#[test]
fn a_registry_object_is_known_by_its_key_and_an_entry_claiming_another_slug_is_unreadable() {
    let b = MemBackend::new();
    let a = publish(&b, "a", "h1", &[("docs/a.md", "# A\n")], 1);
    let c = publish(&b, "c", "h1", &[("docs/a.md", "# C\n")], 1);
    // a second key whose JSON claims the slug of the first
    let mut forged = a.clone();
    forged.head_commit = "evil".into();
    b.put(PR, &format!("{REGISTRY_PREFIX}zzz.json"), &serde_json::to_vec(&forged).unwrap()).unwrap();
    let mut st = FollowState::default();
    let ch = poll(&b, PR, &mut st).unwrap();
    let news: Vec<_> = ch.iter().filter_map(|c| if let Change::New(e) = c { Some((e.slug.clone(), e.head_commit.clone())) } else { None }).collect();
    assert!(news.contains(&("a".into(), a.head_commit.clone())) && news.contains(&("c".into(), c.head_commit.clone())), "{news:?}");
    assert!(!news.iter().any(|(_, h)| h == "evil"), "{news:?}");
    assert_eq!(st.seen.len(), 2);
}

#[test]
fn leftovers_flag_forgotten_unclaimed_and_abandoned_oldest_first() {
    let day = 24 * 3600 * 1000u64;
    let b = MemBackend::new();
    let now = 10 * day;
    let forgotten = publish(&b, "forgot", "h", &[("docs/a.md", "#\n")], 2 * day);
    let fresh = publish(&b, "fresh", "h", &[("docs/a.md", "#\n")], now - 1000);
    publish(&b, "waiting", "h", &[("docs/a.md", "#\n")], 5 * day);
    mark_handoff(&b, "w-waiting", &actor("w"), "merge me", 5 * day).unwrap();
    let waiting = summarize(&b, "waiting", "w-waiting").unwrap().unwrap();
    publish(&b, "stuck", "h", &[("docs/a.md", "#\n")], 8 * day);
    claim_merge(&b, "w-stuck", &actor("m"), 8 * day).unwrap();
    let stuck = summarize(&b, "stuck", "w-stuck").unwrap().unwrap();
    let l = leftovers(
        &[fresh, waiting, stuck, forgotten],
        now,
        &LeftoverPolicy::default(),
    );
    let got: Vec<_> = l.iter().map(|x| (x.entry.slug.as_str(), x.why)).collect();
    assert_eq!(
        got,
        [
            ("forgot", Why::ForgottenOpen),
            ("waiting", Why::UnclaimedHandoff),
            ("stuck", Why::AbandonedClaim)
        ]
    );
}

#[test]
fn overlaps_show_conflicting_files_before_merge() {
    let b = MemBackend::new();
    let a = publish(
        &b,
        "a",
        "h",
        &[
            ("docs/plan.md", "# plan A\n"),
            ("same.txt", "same"),
            ("only_a.txt", "1"),
        ],
        1,
    );
    let c = publish(
        &b,
        "c",
        "h",
        &[
            ("docs/plan.md", "# plan C\n"),
            ("same.txt", "same"),
            ("only_c.txt", "1"),
        ],
        1,
    );
    let rep = overlaps(&b, &a, &c).unwrap();
    assert!(rep.same_base);
    let o = rep.overlaps;
    assert_eq!(
        o.iter().map(|x| x.path.as_str()).collect::<Vec<_>>(),
        ["docs/plan.md"]
    );
    let nobody = RegistryEntry {
        slug: "n".into(),
        bucket: "w-n".into(),
        ..a.clone()
    };
    assert!(overlaps(&b, &a, &nobody).is_err());
    assert!(overlaps(&b, &a, &a).is_err(), "same bucket twice");
}

/// Counts every mutating call, so overwriting an existing key is caught, not just growth.
struct Counting<'a> {
    inner: &'a MemBackend,
    writes: std::cell::Cell<usize>,
}
impl ObjectBackend for Counting<'_> {
    fn head(&self, b: &str, k: &str) -> R<Option<ObjectMeta>> {
        self.inner.head(b, k)
    }
    fn get(&self, b: &str, k: &str) -> R<Option<Vec<u8>>> {
        self.inner.get(b, k)
    }
    fn put(&self, b: &str, k: &str, d: &[u8]) -> R<ObjectMeta> {
        self.writes.set(self.writes.get() + 1);
        self.inner.put(b, k, d)
    }
    fn delete(&self, b: &str, k: &str) -> R<()> {
        self.writes.set(self.writes.get() + 1);
        self.inner.delete(b, k)
    }
    fn list(&self, b: &str, p: &str) -> R<Vec<ObjectMeta>> {
        self.inner.list(b, p)
    }
}

#[test]
fn follower_makes_zero_backend_writes() {
    let b = MemBackend::new();
    let e = publish(&b, "a", "h", &[("docs/a.md", "# A\n")], 1);
    let publish_b = publish(&b, "b", "h", &[("docs/a.md", "# B\n")], 1);
    let c = Counting {
        inner: &b,
        writes: Default::default(),
    };
    let cache = tempfile::tempdir().unwrap();
    let (entries, _) = list_shared_lenient(&c, PR).unwrap();
    poll(&c, PR, &mut FollowState::default()).unwrap();
    search_docs(&c, &entries, "a");
    sync_docs(&c, &e, cache.path()).unwrap();
    overlaps(&c, &e, &publish_b).unwrap();
    leftovers(&entries, 99, &LeftoverPolicy::default());
    assert_eq!(c.writes.get(), 0);
}

#[test]
fn head_and_state_change_in_one_poll_are_both_reported() {
    let b = MemBackend::new();
    let mut st = FollowState::default();
    publish(&b, "a", "h1", &[("docs/a.md", "# A\n")], 1);
    poll(&b, PR, &mut st).unwrap();
    publish(&b, "a", "h2", &[("docs/a.md", "# A2\n")], 2);
    mark_handoff(&b, "w-a", &actor("w"), "go", 3).unwrap();
    write_registry_entry(&b, PR, &summarize(&b, "a", "w-a").unwrap().unwrap()).unwrap();
    let c = poll(&b, PR, &mut st).unwrap();
    assert!(c.iter().any(|x| matches!(x, Change::HeadMoved { .. })));
    assert!(c.iter().any(|x| matches!(
        x,
        Change::StateChanged {
            from: State::Open,
            ..
        }
    )));
}

#[test]
fn recreated_slug_is_gone_then_new() {
    let b = MemBackend::new();
    let mut st = FollowState::default();
    let e = publish(&b, "a", "h1", &[("docs/a.md", "# A\n")], 1);
    poll(&b, PR, &mut st).unwrap();
    let moved = RegistryEntry {
        bucket: "w-a2".into(),
        ..e
    };
    write_registry_entry(&b, PR, &moved).unwrap();
    let c = poll(&b, PR, &mut st).unwrap();
    assert!(matches!(
        c.as_slice(),
        [Change::Gone { .. }, Change::New(_)]
    ));
}

#[test]
fn corrupt_entry_does_not_blind_poll_and_keeps_prior_state() {
    let b = MemBackend::new();
    let mut st = FollowState::default();
    publish(&b, "good", "h", &[("docs/a.md", "# A\n")], 1);
    publish(&b, "bad", "h", &[("docs/a.md", "# B\n")], 1);
    poll(&b, PR, &mut st).unwrap();
    b.put(PR, "worktrees/bad.json", b"{ truncated").unwrap();
    b.put(
        PR,
        "worktrees/new.json",
        br#"{"slug":"new","state":"reviewing"}"#,
    )
    .unwrap();
    let c = poll(&b, PR, &mut st).unwrap();
    assert!(c.contains(&Change::Unreadable { slug: "bad".into() }));
    assert!(c.contains(&Change::Unreadable { slug: "new".into() }));
    assert!(
        !c.iter().any(|x| matches!(x, Change::Gone { .. })),
        "bad entry is not 'gone'"
    );
    assert!(st.seen.contains_key("bad") && st.seen.contains_key("good"));
}

#[test]
fn empty_registry_is_an_error_not_mass_gone_unless_confirmed() {
    let b = MemBackend::new();
    let mut st = FollowState::default();
    publish(&b, "a", "h", &[("docs/a.md", "# A\n")], 1);
    poll(&b, PR, &mut st).unwrap();
    let before = st.clone();
    assert!(poll(&b, "p-wrong-bucket", &mut st).is_err());
    assert_eq!(st, before, "state untouched by the refused poll");
    let c = poll_confirmed(&b, "p-wrong-bucket", &mut st).unwrap();
    assert_eq!(c, [Change::Gone { slug: "a".into() }]);
}

#[test]
fn interrupted_sync_still_converges_after_a_doc_is_deleted() {
    let b = MemBackend::new();
    let e = publish(
        &b,
        "a",
        "h1",
        &[("docs/keep.md", "# K\n"), ("docs/new.md", "# N\n")],
        1,
    );
    let cache = tempfile::tempdir().unwrap();
    // tamper so the fetch fails midway (checksum mismatch on new.md), after keep.md was written
    b.put("w-a", ".wt/tree/docs/new.md", b"tampered").unwrap();
    assert!(sync_docs(&b, &e, cache.path()).is_err());
    let e2 = publish(&b, "a", "h2", &[("docs/keep.md", "# K\n")], 2);
    sync_docs(&b, &e2, cache.path()).unwrap();
    assert!(
        !cache.path().join("a/docs/new.md").exists(),
        "orphan from the failed run is pruned"
    );
    assert!(cache.path().join("a/docs/keep.md").exists());
}

#[test]
fn registry_entry_cannot_redirect_a_follower_to_another_worktrees_bucket() {
    let b = MemBackend::new();
    let victim = publish(
        &b,
        "victim",
        "h",
        &[("docs/secret-plan.md", "# Secret plan\n")],
        1,
    );
    let evil = RegistryEntry {
        slug: "evil".into(),
        bucket: victim.bucket.clone(),
        ..victim.clone()
    };
    assert!(
        search_docs(&b, std::slice::from_ref(&evil), "")
            .hits
            .is_empty()
    );
    let cache = tempfile::tempdir().unwrap();
    assert!(sync_docs(&b, &evil, cache.path()).is_err());
    assert!(overlaps(&b, &victim, &evil).is_err());
    let slashy = RegistryEntry {
        slug: "a/b".into(),
        ..victim
    };
    assert!(sync_docs(&b, &slashy, cache.path()).is_err());
}

#[cfg(unix)]
#[test]
fn symlinked_cache_slug_directory_is_refused() {
    let b = MemBackend::new();
    let e = publish(&b, "a", "h", &[("docs/a.md", "# A\n")], 1);
    let cache = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), cache.path().join("a")).unwrap();
    assert!(sync_docs(&b, &e, cache.path()).is_err());
    assert!(std::fs::read_dir(outside.path()).unwrap().next().is_none());
}

#[test]
fn leftovers_report_future_timestamps_and_stuck_cleanup() {
    let day = 24 * 3600 * 1000u64;
    let b = MemBackend::new();
    let now = 10 * day;
    let ahead = publish(&b, "ahead", "h", &[("docs/a.md", "#\n")], now + day);
    let mut merged = publish(&b, "merged", "h", &[("docs/a.md", "#\n")], 2 * day);
    merged.state = State::Merged;
    let l = leftovers(&[ahead, merged], now, &LeftoverPolicy::default());
    let got: Vec<_> = l.iter().map(|x| (x.entry.slug.as_str(), x.why)).collect();
    assert!(got.contains(&("ahead", Why::FutureTimestamp)));
    assert!(got.contains(&("merged", Why::StuckCleanup)));
}

#[test]
fn different_bases_mark_overlaps_as_unreliable() {
    let b = MemBackend::new();
    let a = publish(&b, "a", "h", &[("x.txt", "1")], 1);
    let c = publish(&b, "c", "h", &[("x.txt", "2")], 1);
    let mut m = read_manifest(&b, "w-c").unwrap().unwrap();
    m.base_commit = "other-base".into();
    b.put("w-c", MANIFEST_KEY, &serde_json::to_vec(&m).unwrap())
        .unwrap();
    let r = overlaps(&b, &a, &c).unwrap();
    assert!(!r.same_base);
    assert_eq!((r.base_a.as_str(), r.base_b.as_str()), ("base", "other-base"), "the report names both bases");
}

#[test]
fn follow_state_round_trips_and_rejects_corruption() {
    use weftos_store::follower::FollowState;
    let d = tempfile::tempdir().unwrap();
    let f = d.path().join("sub/follow.json");
    assert_eq!(FollowState::load(&f).unwrap(), FollowState::default());
    let mut st = FollowState::default();
    st.seen.insert(
        "a".into(),
        weftos_store::follower::Seen {
            etag: None,
            bucket: "w-1-x".into(),
            head_commit: "abc".into(),
            state: weftos_store::share::State::Open,
        },
    );
    st.save(&f).unwrap();
    assert_eq!(FollowState::load(&f).unwrap(), st);
    // no temp files left behind
    let left: Vec<_> = std::fs::read_dir(f.parent().unwrap()).unwrap().collect();
    assert_eq!(left.len(), 1);
    std::fs::write(&f, b"{ not json").unwrap();
    assert!(FollowState::load(&f).is_err());
}

fn ch(p: &str, k: ChangeKind) -> PathChange {
    PathChange {
        path: p.into(),
        kind: k,
    }
}

#[test]
fn overlaps_report_modify_delete_and_rename_conflicts() {
    let b = MemBackend::new();
    // A deletes old.rs (a rename to new.rs); C edits old.rs. Content comparison alone sees nothing.
    let a = publish_c(
        &b,
        "a",
        "h",
        &[("new.rs", "x")],
        &[
            ch("old.rs", ChangeKind::Deleted),
            ch("new.rs", ChangeKind::Added),
        ],
        1,
    );
    let c = publish_c(
        &b,
        "c",
        "h",
        &[("old.rs", "edited")],
        &[ch("old.rs", ChangeKind::Modified)],
        1,
    );
    let o = overlaps(&b, &a, &c).unwrap().overlaps;
    assert_eq!(o.len(), 1);
    assert_eq!(o[0].path, "old.rs");
    assert_eq!(
        o[0].kind,
        OverlapKind::ModifyDelete {
            deleted_in: "a".into()
        }
    );
    // symmetric: swapping the arguments still names the deleter
    let o = overlaps(&b, &c, &a).unwrap().overlaps;
    assert_eq!(
        o[0].kind,
        OverlapKind::ModifyDelete {
            deleted_in: "a".into()
        }
    );
}

#[test]
fn both_deleting_or_untouched_is_not_a_conflict() {
    let b = MemBackend::new();
    let a = publish_c(
        &b,
        "a",
        "h",
        &[("k", "1")],
        &[ch("gone", ChangeKind::Deleted)],
        1,
    );
    let c = publish_c(
        &b,
        "c",
        "h",
        &[("k", "1")],
        &[ch("gone", ChangeKind::Deleted)],
        1,
    );
    assert!(overlaps(&b, &a, &c).unwrap().overlaps.is_empty());
    // C merely still contains the file (did not change it): the deletion is not contested
    let d = publish_c(&b, "d", "h", &[("gone", "same")], &[], 1);
    assert!(overlaps(&b, &a, &d).unwrap().overlaps.is_empty());
}

#[test]
fn name_status_parser_handles_all_kinds_and_refuses_renames() {
    let raw = b"M\0a.rs\0D\0b.rs\0T\0c.rs\0A\0docs/new.md\0A\0a.rs\0";
    let v = parse_name_status_z(raw).unwrap();
    assert_eq!(
        v,
        vec![
            ch("a.rs", ChangeKind::Added), // later record wins
            ch("b.rs", ChangeKind::Deleted),
            ch("c.rs", ChangeKind::Modified),
            ch("docs/new.md", ChangeKind::Added),
        ]
    );
    assert!(parse_name_status_z(b"R100\0a\0b\0").is_err());
    assert!(parse_name_status_z(b"M\0").is_err());
    assert!(parse_name_status_z(b"M\0../escape\0").is_err());
    assert!(parse_name_status_z(b"").unwrap().is_empty());
}

#[test]
fn manifest_without_changes_still_parses() {
    let j = r#"{"schema":1,"branch":"f","base_commit":"a","head_commit":"b","files":[],
        "uncommitted":[],"docs":[],"has_bundle":false,
        "writer":{"agent":"w","host":"h"},"published_ms":1}"#;
    let m: TreeManifest = serde_json::from_str(j).unwrap();
    assert!(m.changes.is_empty());
}

#[test]
fn overlapping_saves_from_threads_of_one_process_never_fail_or_corrupt_the_file() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("follow.json");
    let st = FollowState::default();
    std::thread::scope(|s| {
        for _ in 0..8 {
            s.spawn(|| {
                for _ in 0..200 {
                    st.save(&path).expect("a concurrent save must not lose its temp file");
                    FollowState::load(&path).expect("the file always parses");
                }
            });
        }
    });
    let left: Vec<_> = std::fs::read_dir(d.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(left.len(), 1, "no temp files left behind: {left:?}");
}

const PROJ_A: &str = "01JBXK2M3N4P5Q6R7S8T9V0W1X";
const PROJ_B: &str = "01JBXK2M3N4P5Q6R7S8T9V0W2X";

#[test]
fn a_registry_entry_pointing_at_another_projects_worktree_is_unreadable_not_followed() {
    let b = MemBackend::new();
    let bucket_of = |proj: &str, slug: &str| weftos_store::StoreName::worktree("local", proj, slug).unwrap().bucket();
    let (pa, pb) = (format!("p-{}", PROJ_A.to_lowercase()), format!("p-{}", PROJ_B.to_lowercase()));
    // Project B has a real worktree "fix-typo" with a secret doc.
    let victim_bucket = bucket_of(PROJ_B, "fix-typo");
    let d = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(d.path().join("docs")).unwrap();
    std::fs::write(d.path().join("docs/secret-plan.md"), "# B's secret plan\n").unwrap();
    write_meta(&b, &victim_bucket, &WorktreeMeta { slug: "fix-typo".into(), project: PROJ_B.into(), branch: "x".into(), base_commit: "base".into(), created_ms: 0 }).unwrap();
    let src = TreeSource { root: d.path(), paths: &["docs/secret-plan.md".to_string()], uncommitted: &[], changes: &[], branch: "x", base_commit: "base", head_commit: "h", bundle: None, writer: actor("w") };
    publish_tree(&b, &victim_bucket, &src, &Limits::default(), 1).unwrap();
    let victim = summarize(&b, "fix-typo", &victim_bucket).unwrap().unwrap();
    // A writer in project A plants an entry for the same slug that points at B's bucket.
    write_registry_entry(&b, &pa, &victim).unwrap();
    // an honest entry for A's own worktree of that slug is fine
    let own_bucket = bucket_of(PROJ_A, "fix-typo");
    assert_ne!(own_bucket, victim_bucket);

    let (good, bad) = list_shared_lenient(&b, &pa).unwrap();
    assert!(good.is_empty(), "the redirected entry is not trusted");
    assert_eq!(bad, vec!["fix-typo".to_string()]);
    let mut st = FollowState::default();
    let ch = poll(&b, &pa, &mut st).unwrap();
    assert_eq!(ch, vec![Change::Unreadable { slug: "fix-typo".into() }]);
    // the entry is fine where it belongs
    write_registry_entry(&b, &pb, &victim).unwrap();
    assert_eq!(list_shared_lenient(&b, &pb).unwrap().0.len(), 1);
    // a bucket whose own metadata names another project is refused by verify_entry too
    let mut forged = victim.clone();
    forged.bucket = victim_bucket.clone();
    write_meta(&b, &victim_bucket, &WorktreeMeta { slug: "fix-typo".into(), project: PROJ_A.into(), branch: "x".into(), base_commit: "base".into(), created_ms: 0 }).unwrap();
    assert!(verify_entry(&b, &forged).is_err());
}

struct GetCounter<'a> {
    inner: &'a weftos_store::FsBackend,
    gets: std::sync::atomic::AtomicUsize,
}
impl ObjectBackend for GetCounter<'_> {
    fn head(&self, b: &str, k: &str) -> R<Option<ObjectMeta>> {
        ObjectBackend::head(self.inner, b, k)
    }
    fn get(&self, b: &str, k: &str) -> R<Option<Vec<u8>>> {
        self.gets.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        ObjectBackend::get(self.inner, b, k)
    }
    fn put(&self, b: &str, k: &str, d: &[u8]) -> R<ObjectMeta> {
        ObjectBackend::put(self.inner, b, k, d)
    }
    fn delete(&self, b: &str, k: &str) -> R<()> {
        ObjectBackend::delete(self.inner, b, k)
    }
    fn list(&self, b: &str, p: &str) -> R<Vec<ObjectMeta>> {
        ObjectBackend::list(self.inner, b, p)
    }
}

#[test]
fn a_poll_with_the_digest_fetches_only_entries_that_changed() {
    use weftos_store::AdminBackend;
    let d = tempfile::tempdir().unwrap();
    let fs = weftos_store::FsBackend::open(d.path()).unwrap();
    fs.create_bucket(PR, &[]).unwrap();
    let be = GetCounter { inner: &fs, gets: Default::default() };
    let entry = |i: usize, head: &str| RegistryEntry {
        slug: format!("wt-{i}"),
        bucket: format!("w-{i}"),
        branch: "b".into(),
        head_commit: head.into(),
        state: State::Open,
        writer: actor("w"),
        merger: None,
        doc_count: 0,
        file_count: 0,
        updated_ms: i as u64,
    };
    for i in 0..12 {
        write_registry_entry(&be, PR, &entry(i, "h1")).unwrap();
    }
    let digest = read_digest(&be, PR).unwrap().expect("the daemon wrote a digest");
    assert_eq!(digest.entries.len(), 12);
    assert!(!digest.entries.contains_key("_digest"), "the digest is not one of its own entries");
    assert_eq!(list_shared(&be, PR).unwrap().len(), 12, "listings skip the digest");

    let gets = || be.gets.swap(0, std::sync::atomic::Ordering::SeqCst);
    let mut st = FollowState::default();
    gets();
    let ch = poll_fast(&be, PR, &mut st, false, true, FETCH_JOBS).unwrap();
    assert_eq!(ch.len(), 12);
    assert_eq!(gets(), 13, "the digest plus every entry once");
    // nothing changed: the digest alone
    assert!(poll_fast(&be, PR, &mut st, false, true, FETCH_JOBS).unwrap().is_empty());
    assert_eq!(gets(), 1);
    // one worktree moves: the digest and that entry
    write_registry_entry(&be, PR, &entry(7, "h2")).unwrap();
    gets();
    let ch = poll_fast(&be, PR, &mut st, false, true, FETCH_JOBS).unwrap();
    assert!(matches!(ch.as_slice(), [Change::HeadMoved { entry, from }] if entry.slug == "wt-7" && from == "h1"), "{ch:?}");
    assert_eq!(gets(), 2);
    // a missing digest falls back to the listing and still sees every change
    ObjectBackend::delete(&be, PR, DIGEST_KEY).unwrap();
    write_registry_entry(&be, PR, &entry(3, "h3")).unwrap(); // rewrites the digest
    ObjectBackend::delete(&be, PR, DIGEST_KEY).unwrap();
    gets();
    let ch = poll_fast(&be, PR, &mut st, false, true, FETCH_JOBS).unwrap();
    assert!(matches!(ch.as_slice(), [Change::HeadMoved { entry, .. }] if entry.slug == "wt-3"), "{ch:?}");
    assert_eq!(gets(), 2, "no digest: one get for it (absent) and one changed entry, the rest by listing etag");
    // gone, and the empty-registry guard still applies
    remove_registry_entry(&be, PR, "wt-0").unwrap();
    let ch = poll_fast(&be, PR, &mut st, false, true, FETCH_JOBS).unwrap();
    assert_eq!(ch, vec![Change::Gone { slug: "wt-0".into() }]);
    // the serial poll agrees with the fast one
    let mut st2 = FollowState::default();
    assert_eq!(poll(&be, PR, &mut st2).unwrap().len(), 11);
}
