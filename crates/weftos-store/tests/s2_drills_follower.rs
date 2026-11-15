//! WO-069 lane Q: ADR-126 section 4a gate item 6, follower drills (a) to (d) on a real registry. (e) to (g) are in
//! `s2_drills_follower_more.rs`. Each drill is written once and runs on the in-memory backend, the directory backend and, ignored,
//! a THROWAWAY loopback RustFS (`WEFTOS_STORE_TEST_RUSTFS_ENV`, see `s2_drills_fcommon`). Output: `DRILL|6x|...|PASS|...` and `FINDING|6x|...`.
//!
//! - 6a: a writer (or merger) whose clock is a day ahead: leftovers, claims, a publish under a claim, orphan marking, signature generations.
//! - 6b: corrupt, empty, binary, wrong-shape and unknown-schema registry entries and manifests, through `poll`, `poll_fast` and the digest.
//! - 6c: an empty or wrong project bucket, and a stale, empty or foreign-schema digest.
//! - 6d: a merged worktree whose cleanup never ran, half ran, or whose registry refresh lagged.
//!
//! Already covered in `s2_follower_drills.rs` and not repeated: leftover verdicts for a writer a day ahead or behind (`skewed_writer_clocks`),
//! the plain corrupt/newer/mislabelled entries through `poll` and an empty/other project bucket (`damaged_registry`), merged and orphan cleanup through `poll`.
#[macro_use]
#[path = "s2_drills_fcommon/mod.rs"]
mod fc;
use fc::*;
use std::time::Instant;
use weftos_store::follower::*;
use weftos_store::orphan::{OrphanPolicy, mark_orphan};
use weftos_store::overlay::ObjectBackend as S2;
use weftos_store::registry_sign::*;
use weftos_store::share::*;
use weftos_store::share_fetch::*;

// =====================================================================================================
// 6a: a writer a day ahead
// =====================================================================================================

fn d6a_clock_ahead<B: S2>(env: &Env<'_, B>) {
    let (be, pr) = (env.be, env.pr.as_str());
    let pol = LeftoverPolicy::default();
    let now = T0 + 10 * DAY; // this follower's clock
    let ahead = now + DAY; // the writer's

    // the writer publishes, hands off and is registered, every stamp a day ahead
    let t = Instant::now();
    let e = up(env, "ahead", &[("docs/a.md", "# A\n")], &Pub { now: ahead, ..Pub::default() });
    mark_handoff(be, &e.bucket, &actor("writer"), "go", ahead).unwrap();
    let e = register(be, pr, "ahead", &e.bucket);
    assert_eq!(e.updated_ms, ahead);
    let mut st = FollowState::default();
    assert_eq!(kinds(&poll(be, pr, &mut st).unwrap()), ["new:ahead"], "poll has no clock: skew cannot hide a worktree");
    assert_eq!(search_docs(be, std::slice::from_ref(&e), "a").hits.len(), 1);
    sync_docs(be, &e, tempfile::tempdir().unwrap().path()).unwrap();
    let l = leftovers(std::slice::from_ref(&e), now, &pol);
    assert_eq!((l.len(), l[0].why, l[0].idle_ms), (1, Why::FutureTimestamp, 0));
    assert!(leftovers(std::slice::from_ref(&e), ahead, &pol).is_empty(), "when the follower's clock reaches the stamp it is a fresh handoff");
    let l = leftovers(std::slice::from_ref(&e), ahead + DAY + 1, &pol);
    assert_eq!((l[0].why, l[0].idle_ms), (Why::UnclaimedHandoff, DAY + 1), "age counts from the stamp, not from when it was really written");
    line("6a", "poll/search/sync unaffected; leftovers say FutureTimestamp, then count from the stamp", t, "writer +1 d");

    // a merger a day ahead claims: its claim is not trusted as live, a normal-clock merger takes it, the fast one is fenced
    let t = Instant::now();
    let c1 = claim_merge(be, &e.bucket, &actor("fast"), ahead).unwrap();
    let c2 = claim_merge(be, &e.bucket, &actor("slow"), now).unwrap();
    assert_eq!((c1.epoch, c2.epoch), (1, 2));
    assert!(mark_merged_at(be, &e.bucket, &actor("fast"), Some(1), "late", ahead).is_err(), "the fast merger's old epoch is fenced off");
    mark_merged_at(be, &e.bucket, &actor("slow"), Some(2), "done", now).unwrap();
    line("6a", "claim stamped +1 d is taken over at once; the displaced claimer is fenced", t, "epoch 1 -> 2");

    // the same fast claim does not hold off the writer: a publish at the normal clock goes through (claim_is_live is false for it)
    let t = Instant::now();
    let w = up(env, "under", &[("docs/u.md", "# U\n")], &Pub { now, ..Pub::default() });
    mark_handoff(be, &w.bucket, &actor("writer"), "go", now).unwrap();
    claim_merge(be, &w.bucket, &actor("fast"), ahead).unwrap();
    let republished = push(be, &w.bucket, &tree(&[("docs/u.md", "# U v2\n")]), &Pub { head: "h2", now: now + 1, ..Pub::default() });
    let merged = mark_merged_at(be, &w.bucket, &actor("fast"), Some(1), "merged h1", ahead);
    let head_now = read_manifest(be, &w.bucket).unwrap().unwrap().head_commit;
    println!("DETAIL|6a|writer publish under a +1 d claim: {:?}; the fast claimer then marks merged: {:?}; manifest head now {head_now}", republished.is_ok(), merged.is_ok());
    if republished.is_ok() && merged.is_ok() && head_now == "h2" {
        finding("6a", "a claim stamped a day ahead is not live for anyone, so the writer republished (h1 -> h2) under it and the claimer (never displaced, epoch 1) still recorded the merge as done; the record says merged while the tree it merged (h1) is no longer the published head (h2). A claim is only fenced by another claim, not by a head change. Proposed: mark_merged_at takes the head it merged and refuses if the manifest head differs");
    }
    line("6a", "writer publishes under a +1 d claim", t, format!("publish ok={} merged ok={} head {head_now}", republished.is_ok(), merged.is_ok()));

    // an orphan stamped far ahead: the orphan state is hidden behind FutureTimestamp
    let t = Instant::now();
    let o = up(env, "orph", &[("docs/o.md", "# O\n")], &Pub { now: now + 3 * DAY, ..Pub::default() });
    let marked = mark_orphan(be, pr, "orph", now + 25 * 3600 * 1000, now, &OrphanPolicy::default()).unwrap();
    let l = leftovers(std::slice::from_ref(&marked), now + 25 * 3600 * 1000, &pol);
    println!("DETAIL|6a|orphan entry updated_ms {} orphaned_at {:?}; leftovers say {:?}", marked.updated_ms, marked.orphan.as_ref().map(|x| x.orphaned_at_ms), l.iter().map(|x| x.why).collect::<Vec<_>>());
    if !l.iter().any(|x| x.why == Why::Orphan) {
        finding("6a", "an orphan whose entry carries a writer stamp more than the skew allowance ahead is reported as FutureTimestamp, never as Orphan: leftovers checks the future stamp before the state, so the orphan and its age are hidden until the stamp passes. gc's plan (orphaned_at_ms, the daemon's own clock) is unaffected. Proposed: test State::Orphan before the future-stamp check");
    }
    let _ = o;
    line("6a", "orphan marked while the entry is stamped +3 d", t, format!("{:?}", l.iter().map(|x| x.why).collect::<Vec<_>>()));

    signature_generations(env);
}

/// The signature generation is `max(previous + 1, updated_ms)`, so a writer's stamp feeds it.
fn signature_generations<B: S2>(env: &Env<'_, B>) {
    let (be, t) = (env.be, Instant::now());
    let pr = format!("p-gen{}", env.u);
    env.make(&pr);
    let gen_of = |slug: &str| serde_json::from_slice::<serde_json::Value>(&S2::get(be, &pr, &format!("worktrees.sig/{slug}.json")).unwrap().unwrap()).unwrap()["generation"].as_u64().unwrap();
    let mk = |slug: &str, updated: u64, st: State| RegistryEntry {
        slug: slug.into(), bucket: format!("w-x-{slug}"), branch: "b".into(), head_commit: "h".into(), state: st, writer: actor("w"), merger: None,
        doc_count: 0, file_count: 0, updated_ms: updated, orphan: None,
    };
    let now = T0 + 10 * DAY;
    let mut e = mk("g", now + DAY, State::Open);
    write_registry_entry_signed(be, &pr, &e, &Toy).unwrap();
    let g1 = gen_of("g");
    e.updated_ms = now; // the writer's clock was corrected
    e.state = State::Handoff;
    write_registry_entry_signed(be, &pr, &e, &Toy).unwrap();
    let g2 = gen_of("g");
    assert_eq!((g1, g2), (now + DAY, now + DAY + 1), "a day-ahead stamp lifts the generation; later writes still increase it");
    let mut st = FollowState::default();
    assert_eq!(kinds(&poll_verified(be, &pr, &mut st, &Toy, true, false).unwrap()), ["new:g"]);

    // a far-future stamp pins the generation at u64::MAX: after that a later signed entry carries the same generation
    let mut e = mk("pin", u64::MAX, State::Open);
    write_registry_entry_signed(be, &pr, &e, &Toy).unwrap();
    let (old_e, old_s) = (S2::get(be, &pr, "worktrees/pin.json").unwrap().unwrap(), S2::get(be, &pr, "worktrees.sig/pin.json").unwrap().unwrap());
    poll_verified(be, &pr, &mut st, &Toy, true, false).unwrap();
    e.state = State::Merged;
    write_registry_entry_signed(be, &pr, &e, &Toy).unwrap();
    poll_verified(be, &pr, &mut st, &Toy, true, false).unwrap();
    let (g_old, g_new) = (gen_of("pin"), st.generations["pin"]);
    S2::put(be, &pr, "worktrees/pin.json", &old_e).unwrap();
    S2::put(be, &pr, "worktrees.sig/pin.json", &old_s).unwrap();
    let ch = poll_verified(be, &pr, &mut st, &Toy, true, false).unwrap();
    println!("DETAIL|6a|pinned generation {g_old} (follower holds {g_new}); replay of the older signed Open entry reports {:?}", kinds(&ch));
    if !kinds(&ch).iter().any(|k| k == "bad:pin") {
        finding("6a", "a worktree whose manifest or handoff is stamped u64::MAX makes every signed registry write for it carry generation u64::MAX (prev + 1 saturates), so a genuinely signed OLDER entry replays without being refused as a rollback: the generation check is `<`, and equal generations pass. Needs write access to that worktree's bucket (the stamp comes from its manifest). Proposed: clamp the stamp that feeds the generation to now + skew, or refuse an entry whose updated_ms is absurdly far ahead");
    }
    line("6a", "signature generation with a +1 d stamp and with u64::MAX", t, format!("gen {g1} -> {g2}; pinned {g_old}"));
    for k in ["worktrees/g.json", "worktrees/pin.json", "worktrees.sig/g.json", "worktrees.sig/pin.json"] {
        let _ = S2::delete(be, &pr, k);
    }
}
drill!(clock_ahead, d6a_clock_ahead);

// =====================================================================================================
// 6b: corrupt and unknown-schema registry entries, through poll, poll_fast and the digest
// =====================================================================================================

fn fresh_kinds<B: S2 + Sync>(be: &B, pr: &str, mode: &str) -> Vec<String> {
    let mut st = FollowState::default();
    let c = match mode {
        "poll" => poll(be, pr, &mut st),
        "fast" => poll_fast(be, pr, &mut st, false, false, 3),
        _ => poll_fast(be, pr, &mut st, false, true, 3),
    };
    kinds(&c.unwrap())
}

fn d6b_damaged_entries<B: S2 + Sync>(env: &Env<'_, B>) {
    let (be, pr) = (env.be, env.pr.as_str());
    let t = Instant::now();
    let good = up(env, "good", &[("docs/g.md", "# G\n")], &Pub::default());
    let fwd = up(env, "fwd", &[("docs/f.md", "# F\n")], &Pub::default());
    let mut j = serde_json::to_value(&fwd).unwrap();
    j["added_in_v2"] = serde_json::json!({"x": 1});
    S2::put(be, pr, "worktrees/fwd.json", &serde_json::to_vec(&j).unwrap()).unwrap();
    // damaged: truncated, zero bytes, binary, a JSON array, an unknown state, a newer layout that renamed every field, a wrong type
    let mut newer = serde_json::json!({"schema": 2, "slug": "newer", "worktree": {"bucket": fwd.bucket}});
    newer["state"] = "open".into();
    let mut wrong_type = serde_json::to_value(&fwd).unwrap();
    wrong_type["slug"] = "wrongtype".into();
    wrong_type["doc_count"] = "many".into();
    let mut bad_state = serde_json::to_value(&fwd).unwrap();
    bad_state["slug"] = "badstate".into();
    bad_state["state"] = "reviewing".into();
    for (slug, bytes) in [
        ("trunc", b"{ \"slug\": \"trunc\"".to_vec()), ("empty", vec![]), ("binary", vec![0xff, 0xfe, 0x00, 0x9c, 0x80]), ("array", b"[]".to_vec()),
        ("badstate", serde_json::to_vec(&bad_state).unwrap()), ("newer", serde_json::to_vec(&newer).unwrap()), ("wrongtype", serde_json::to_vec(&wrong_type).unwrap()),
    ] {
        S2::put(be, pr, &format!("worktrees/{slug}.json"), &bytes).unwrap();
    }
    write_digest(be, pr).unwrap();
    let want = ["bad:array", "bad:badstate", "bad:binary", "bad:empty", "bad:newer", "bad:trunc", "bad:wrongtype", "new:fwd", "new:good"];
    for mode in ["poll", "fast", "digest"] {
        assert_eq!(fresh_kinds(be, pr, mode), want, "{mode}: every flavour of poll reports the same seven damaged entries and keeps the two good ones");
    }
    // reported on every poll until fixed; nothing else repeats; the good ones are not blinded or re-fetched into noise
    for mode in ["fast", "digest"] {
        let mut st = FollowState::default();
        let d = mode == "digest";
        poll_fast(be, pr, &mut st, false, d, 3).unwrap();
        let again = kinds(&poll_fast(be, pr, &mut st, false, d, 3).unwrap());
        assert_eq!(again, ["bad:array", "bad:badstate", "bad:binary", "bad:empty", "bad:newer", "bad:trunc", "bad:wrongtype"], "{mode}");
    }
    let (ok, bad) = list_shared_lenient(be, pr).unwrap();
    assert_eq!((ok.len(), bad.len()), (2, 7));
    assert_eq!(search_docs(be, &ok, "").hits.len(), 2);
    assert_eq!(leftovers(&ok, T0 + 30 * DAY, &LeftoverPolicy::default()).len(), 2, "leftovers see only what parsed");
    line("6b", "7 damaged entries (truncated, 0 bytes, binary, array, unknown state, newer layout, wrong type) via poll, poll_fast, digest", t, "same verdict three ways");

    // an entry whose manifest is from a newer schema: extra fields read fine, a renamed core field is flagged, never served
    let t = Instant::now();
    let mut m: serde_json::Value = serde_json::from_slice(&S2::get(be, &good.bucket, MANIFEST_KEY).unwrap().unwrap()).unwrap();
    m["schema"] = 99.into();
    m["added_in_v99"] = serde_json::json!(["x"]);
    S2::put(be, &good.bucket, MANIFEST_KEY, &serde_json::to_vec(&m).unwrap()).unwrap();
    let extra = search_docs(be, &ok, "g");
    let mut m2 = m.clone();
    m2.as_object_mut().unwrap().remove("files");
    S2::put(be, &good.bucket, MANIFEST_KEY, &serde_json::to_vec(&m2).unwrap()).unwrap();
    let renamed = search_docs(be, &ok, "g");
    println!("DETAIL|6b|manifest schema 99 with an extra field: {} hit(s) {:?}; with `files` removed: {} hit(s), unreadable {:?}", extra.hits.len(), extra.unreadable, renamed.hits.len(), renamed.unreadable);
    assert_eq!(extra.hits.len(), 0, "a manifest from a newer schema is refused, not read as current");
    assert_eq!(extra.unreadable, ["good"]);
    assert_eq!((renamed.hits.len(), renamed.unreadable.as_slice()), (0, ["good".to_string()].as_slice()));
    line("6b", "manifest schema 99 is refused as unreadable (FIXED, was: read as current); a renamed core field is flagged too", t, "never an error for the rest");

    // the digest names a slug whose object was deleted after the digest was written (the daemon swallows a failed digest rewrite)
    let t = Instant::now();
    write_digest(be, pr).unwrap();
    let mut st = FollowState::default();
    poll_fast(be, pr, &mut st, false, true, 3).unwrap();
    S2::delete(be, pr, "worktrees/fwd.json").unwrap(); // digest not refreshed
    let known = kinds(&poll_fast(be, pr, &mut st, false, true, 3).unwrap());
    let fresh = fresh_kinds(be, pr, "digest");
    println!("DETAIL|6b|entry deleted, digest stale: a follower that knew it sees {known:?}; a new follower sees fwd as {:?}", fresh.iter().filter(|k| k.ends_with(":fwd")).collect::<Vec<_>>());
    assert!(!known.contains(&"gone:fwd".to_string()), "a stale digest hides the removal from a follower that already knew the entry");
    assert!(fresh.contains(&"bad:fwd".to_string()), "and shows it to a new follower as unreadable, not as absent");
    finding("6b", "after an entry is deleted and the digest rewrite is swallowed (remove_registry_entry ignores that error), digest followers keep the worktree as live for a follower that knew it and report a NEW follower an `Unreadable` slug that does not exist, until the next digest write. Listing followers (poll, poll_fast without the digest) are right. Bounded by the next registry write; no repair path if the daemon is idle. Proposed: treat a digest slug whose object is missing as Gone, not Unreadable, and let `follow --full` be the documented cure");
    let healed = { write_digest(be, pr).unwrap(); kinds(&poll_fast(be, pr, &mut st, false, true, 3).unwrap()) };
    assert!(healed.contains(&"gone:fwd".to_string()), "the next digest write heals it: {healed:?}");
    line("6b", "entry deleted under a stale digest: hidden from a knowing follower, 'unreadable' to a new one, healed by the next digest", t, "documented");
}
drill_sync!(damaged_entries, d6b_damaged_entries);

// =====================================================================================================
// 6c: an empty or wrong project bucket, and a bad digest
// =====================================================================================================

fn d6c_wrong_project_bucket<B: S2 + Sync>(env: &Env<'_, B>) {
    let (be, pr) = (env.be, env.pr.as_str());
    let t = Instant::now();
    for s in ["a", "b", "c"] {
        up(env, s, &[("docs/x.md", "# X\n")], &Pub::default());
    }
    let empty = env.wt("emptyproj"); // exists, holds nothing
    let wrong_kind = format!("w-{}-a", env.u); // the real worktree bucket of "a", handed in as the project bucket
    for (label, bucket) in [("empty bucket", empty.as_str()), ("a worktree bucket", wrong_kind.as_str())] {
        assert!(poll(be, bucket, &mut FollowState::default()).unwrap().is_empty(), "{label}: a first look is an empty project, not an error");
        for (flavour, digest) in [("poll_fast", false), ("poll_fast+digest", true)] {
            let mut st = FollowState::default();
            poll_fast(be, pr, &mut st, false, digest, 3).unwrap(); // the follower knows the real three
            let before = st.clone();
            let r = poll_fast(be, bucket, &mut st, false, digest, 3);
            assert!(r.is_err(), "{label}/{flavour}: refused after worktrees were known");
            assert_eq!(st, before, "{label}/{flavour}: the follower's memory is untouched");
            let gone = kinds(&poll_fast(be, bucket, &mut st, true, digest, 3).unwrap());
            assert_eq!(gone, ["gone:a", "gone:b", "gone:c"], "{label}/{flavour}: only the explicit flag accepts it");
        }
    }
    line("6c", "empty bucket and a worktree bucket as the project bucket: refused once worktrees are known, accepted only with the flag", t, "poll + poll_fast, state kept");

    // digests: empty, stale, a newer schema, corrupt
    let t = Instant::now();
    write_digest(be, pr).unwrap();
    let real: serde_json::Value = serde_json::from_slice(&digest_bytes(be, pr)).unwrap();
    let put_digest = |v: &[u8]| S2::put(be, pr, &digest_key(be, pr), v).unwrap();
    let cases: [(&str, Vec<u8>); 4] = [
        ("empty digest, entries present", serde_json::to_vec(&serde_json::json!({"schema": 1, "entries": {}})).unwrap()),
        ("newer-schema digest", serde_json::to_vec(&serde_json::json!({"schema": 2, "entries": {}})).unwrap()),
        ("corrupt digest", b"{ nope".to_vec()),
        ("digest with only one of three", serde_json::to_vec(&serde_json::json!({"schema": 1, "entries": real["entries"].as_object().unwrap().iter().take(1).map(|(k, v)| (k.clone(), v.clone())).collect::<serde_json::Map<_, _>>()})).unwrap()),
    ];
    let mut out = vec![];
    for (label, bytes) in &cases {
        put_digest(bytes);
        let seen = fresh_kinds(be, pr, "digest").len();
        out.push(format!("{label}: new follower sees {seen} of 3"));
    }
    println!("DETAIL|6c|{}", out.join("; "));
    put_digest(&cases[0].1);
    assert_eq!(fresh_kinds(be, pr, "digest").len(), 0, "an empty-but-valid digest is believed: a NEW follower sees no worktrees");
    put_digest(&cases[1].1);
    assert_eq!(fresh_kinds(be, pr, "digest").len(), 3, "a newer-schema digest is ignored and the listing used");
    put_digest(&cases[2].1);
    assert_eq!(fresh_kinds(be, pr, "digest").len(), 3, "a corrupt digest falls back to the listing");
    put_digest(&cases[3].1);
    assert_eq!(fresh_kinds(be, pr, "digest").len(), 1, "a digest naming one of three hides the other two");
    // a follower that knows all three is protected from the empty digest (error, not mass-gone), but not from the partial one
    let mut st = FollowState::default();
    poll_fast(be, pr, &mut st, false, false, 3).unwrap();
    put_digest(&cases[0].1);
    assert!(poll_fast(be, pr, &mut st, false, true, 3).is_err(), "empty digest vs known worktrees: refused, not 'everything gone'");
    put_digest(&cases[3].1);
    let c = kinds(&poll_fast(be, pr, &mut st, false, true, 3).unwrap());
    assert_eq!(c.len(), 2, "a partial digest reads as two worktrees going away: {c:?}");
    finding("6c", "the digest is a hint the daemon rewrites after each registry write, but a follower that uses it believes it fully: an empty digest shows a NEW follower no worktrees, and a digest that lost entries makes a knowing follower report them `Gone` (the all-empty guard only stops the empty case). A truncated or half-written digest object is the likely cause. Mitigation today: `follow --full` (listing). Proposed: a digest-driven follower cross-checks the digest's entry count against one cheap listing before trusting a drop");
    write_digest(be, pr).unwrap();
    assert_eq!(fresh_kinds(be, pr, "digest").len(), 3);
    line("6c", "empty, newer-schema, corrupt and partial digests", t, out.join("; "));
}
drill_sync!(wrong_project_bucket, d6c_wrong_project_bucket);

fn digest_key<B: S2>(_be: &B, _pr: &str) -> String {
    weftos_store::share_fetch::DIGEST_KEY.to_string()
}
fn digest_bytes<B: S2>(be: &B, pr: &str) -> Vec<u8> {
    S2::get(be, pr, &digest_key(be, pr)).unwrap().unwrap()
}

// =====================================================================================================
// 6d: a merged worktree whose cleanup never ran
// =====================================================================================================

fn d6d_merged_cleanup_variants<B: S2>(env: &Env<'_, B>) {
    let (be, pr) = (env.be, env.pr.as_str());
    let pol = LeftoverPolicy::default();
    let (w, m) = (actor("writer"), actor("m"));

    // the merge finished, the registry refresh never ran: the entry still says Claimed
    let t = Instant::now();
    let e = up(env, "lag", &[("docs/l.md", "# L\n")], &Pub::default());
    mark_handoff(be, &e.bucket, &w, "go", T0 + 1).unwrap();
    claim_merge(be, &e.bucket, &m, T0 + 2).unwrap();
    let stale = register(be, pr, "lag", &e.bucket);
    mark_merged_at(be, &e.bucket, &m, Some(1), "landed", T0 + 3).unwrap();
    assert_eq!(summarize(be, "lag", &e.bucket).unwrap().unwrap().state, State::Merged);
    let l = leftovers(std::slice::from_ref(&stale), T0 + 31 * 60_000, &pol);
    assert_eq!(l.iter().map(|x| x.why).collect::<Vec<_>>(), [Why::AbandonedClaim], "a lagging registry makes a finished merge look like an abandoned claim");
    let mut st = FollowState::default();
    poll(be, pr, &mut st).unwrap();
    register(be, pr, "lag", &e.bucket); // the daemon catches up
    assert_eq!(kinds(&poll(be, pr, &mut st).unwrap()), ["state:lag"]);
    assert!(leftovers(&entries_of(be, pr), T0 + 31 * 60_000, &pol).is_empty(), "after the refresh it is Merged and quiet for a day");
    line("6d", "merge done, registry refresh lagging: AbandonedClaim until the daemon catches up", t, "report only, healed by the refresh");

    // cleanup half ran: the bucket's manifest is gone but the entry says Merged
    let t = Instant::now();
    let h = up(env, "half", &[("docs/h.md", "# H\n")], &Pub::default());
    mark_handoff(be, &h.bucket, &w, "go", T0 + 1).unwrap();
    claim_merge(be, &h.bucket, &m, T0 + 2).unwrap();
    mark_merged_at(be, &h.bucket, &m, Some(1), "landed", T0 + 3).unwrap();
    let h = register(be, pr, "half", &h.bucket);
    S2::delete(be, &h.bucket, MANIFEST_KEY).unwrap();
    assert!(search_docs(be, std::slice::from_ref(&h), "").unreadable.is_empty(), "merged entries are never searched, so the half-clean bucket is not even looked at");
    assert!(sync_docs(be, &h, tempfile::tempdir().unwrap().path()).is_err(), "mirroring it is an error, not a panic or an empty mirror");
    let l = leftovers(std::slice::from_ref(&h), T0 + DAY + 10, &pol);
    assert_eq!(l.iter().map(|x| x.why).collect::<Vec<_>>(), [Why::StuckCleanup]);
    line("6d", "merged entry whose manifest is already deleted: StuckCleanup, mirror refused", t, "no panic");

    // the entry was removed but the bucket survived; a later sweep re-registers it
    let t = Instant::now();
    let mut st = FollowState::default();
    let full = up(env, "back", &[("docs/b.md", "# B\n")], &Pub::default());
    mark_handoff(be, &full.bucket, &w, "go", T0 + 1).unwrap();
    claim_merge(be, &full.bucket, &m, T0 + 2).unwrap();
    mark_merged_at(be, &full.bucket, &m, Some(1), "landed", T0 + 3).unwrap();
    register(be, pr, "back", &full.bucket);
    poll(be, pr, &mut st).unwrap();
    remove_registry_entry(be, pr, "back").unwrap();
    assert!(kinds(&poll(be, pr, &mut st).unwrap()).contains(&"gone:back".to_string()));
    register(be, pr, "back", &full.bucket); // the bucket was never deleted, the daemon sweep finds it again
    let again = kinds(&poll(be, pr, &mut st).unwrap());
    assert_eq!(again, ["new:back"], "it comes back as New, once");
    let l = leftovers(&entries_of(be, pr), T0 + DAY + 10, &pol);
    assert!(l.iter().any(|x| x.entry.slug == "back" && x.why == Why::StuckCleanup));
    assert!(poll(be, pr, &mut st).unwrap().is_empty() || kinds(&poll(be, pr, &mut st).unwrap()).iter().all(|k| k.starts_with("state:") || k.starts_with("head:")));
    line("6d", "entry removed, bucket kept, re-registered by a sweep: Gone then New once, StuckCleanup after a day", t, "no flapping");
}
drill!(merged_cleanup_variants, d6d_merged_cleanup_variants);

fn entries_of<B: S2>(be: &B, pr: &str) -> Vec<RegistryEntry> {
    list_shared(be, pr).unwrap()
}
