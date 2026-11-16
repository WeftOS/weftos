//! WO-069 lane F1 (ADR-126 gate item 8): detecting that a worktree bucket was last written by an
//! older daemon, from what an older build cannot have written. Additive: no on-store format change
//! an older reader would choke on (the one new field is on the registry entry, and an rc.1-shaped
//! entry parser is shown to ignore it), no refusal unless a caller asks for one.
use serde_json::Value;
use weftos_store::overlay::{ObjectBackend, S2Error};
use weftos_store::share::*;
use weftos_store::share_fetch::*;
use weftos_store::testing::MemBackend;

const WT: &str = "w-01abc-ffffffffffff";
const PR: &str = "p-01ARZ3NDEKTSV4RRFFQ69G5FAV";
const NOW: u64 = 1_000_000_000;

fn who() -> Actor {
    Actor { agent: "writer".into(), host: "h".into() }
}
fn bob() -> Actor {
    Actor { agent: "bob".into(), host: "m2".into() }
}

struct Src {
    dir: tempfile::TempDir,
}
impl Src {
    fn new(files: &[(&str, &str)]) -> Self {
        let s = Src { dir: tempfile::tempdir().unwrap() };
        for (p, c) in files {
            let f = s.dir.path().join("t").join(p);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, c).unwrap();
        }
        s
    }
}
fn publish(b: &MemBackend, wt: &str, files: &[(&str, &str)], head: &str, schema: u32, now: u64) -> PublishReport {
    let s = Src::new(files);
    let paths: Vec<String> = files.iter().map(|(p, _)| p.to_string()).collect();
    let src = TreeSource {
        root: &s.dir.path().join("t"),
        paths: &paths,
        uncommitted: &[],
        changes: &[],
        changes_recorded: false,
        branch: "feat/f1",
        base_commit: "base",
        head_commit: head,
        bundle: None,
        writer: who(),
    };
    let o = PublishOptions { schema, ..PublishOptions::default() };
    publish_tree_opts(b, wt, &src, &Limits::default(), now, None, &o).unwrap().report
}
fn state(b: &MemBackend, wt: &str) -> (TreeManifest, Handoff) {
    (read_manifest(b, wt).unwrap().unwrap(), read_handoff(b, wt).unwrap().unwrap())
}
fn verdict(b: &MemBackend, wt: &str) -> Option<OlderWriter> {
    let (m, h) = state(b, wt);
    detect_older_writer(Some(&m), Some(&h))
}
/// What an older daemon does to the record when it claims, hands off or merges: it rewrites it
/// through a struct that has no `publishing`, `claimed_head` or `claimed_tree`.
fn rewrite_like_older_daemon(b: &MemBackend, wt: &str) {
    let mut v: Value = serde_json::from_slice(&b.get(wt, HANDOFF_KEY).unwrap().unwrap()).unwrap();
    let o = v.as_object_mut().unwrap();
    for k in ["publishing", "claimed_head", "claimed_tree"] {
        o.remove(k);
    }
    b.put(wt, HANDOFF_KEY, &serde_json::to_vec_pretty(&v).unwrap()).unwrap();
}
const F1: &[(&str, &str)] = &[("docs/a.md", "# A\n"), ("src/b.rs", "fn b() {}\n")];
const F2: &[(&str, &str)] = &[("docs/a.md", "# A2\n"), ("src/b.rs", "fn b2() {}\n")];

#[test]
fn nothing_published_is_clean() {
    assert_eq!(detect_older_writer(None, None), None);
    let b = MemBackend::default();
    assert_eq!(read_manifest(&b, WT).unwrap(), None);
}

#[test]
fn a_bucket_this_build_published_is_clean_in_both_schemas() {
    for schema in [SCHEMA_V2, SCHEMA_V1] {
        let b = MemBackend::default();
        let r = publish(&b, WT, F1, "h1", schema, NOW);
        assert_eq!(r.older_writer_before, None, "first publish: nothing there before");
        assert_eq!(state(&b, WT).0.schema, schema);
        // this build in schema-1 mode still announces, so it is told apart from an older writer
        assert_eq!(verdict(&b, WT), None, "schema {schema}");
        let r = publish(&b, WT, F2, "h2", schema, NOW + 1);
        assert_eq!(r.older_writer_before, None);
        assert_eq!(verdict(&b, WT), None);
    }
}

#[test]
fn an_rc1_shaped_writer_is_a_schema_1_manifest_without_an_announcement() {
    let b = MemBackend::default();
    publish(&b, WT, F1, "h1", SCHEMA_V1, NOW);
    rewrite_like_older_daemon(&b, WT);
    let v = verdict(&b, WT).expect("older writer");
    assert_eq!(v.kind, OlderKind::Schema1NoAnnouncement);
    assert!(v.what.contains("older"), "{}", v.what);
}

#[test]
fn a_v2_manifest_whose_record_an_older_daemon_rewrote_is_the_mixed_case() {
    let b = MemBackend::default();
    publish(&b, WT, F1, "h1", SCHEMA_V2, NOW);
    claim_merge(&b, WT, &bob(), NOW + 1).unwrap();
    rewrite_like_older_daemon(&b, WT);
    // the record is Claimed with no claimed_head: the claim itself is the older daemon's
    assert_eq!(verdict(&b, WT).unwrap().kind, OlderKind::OldClaim);
    // a handoff or merge by an older daemon (record not claimed, announcement gone)
    let b2 = MemBackend::default();
    publish(&b2, WT, F1, "h1", SCHEMA_V2, NOW);
    rewrite_like_older_daemon(&b2, WT);
    let v = verdict(&b2, WT).unwrap();
    assert_eq!(v.kind, OlderKind::HandoffRewritten);
    assert!(v.evidence.contains("schema 2"), "{}", v.evidence);
}

#[test]
fn a_manifest_replaced_after_the_last_announcement_is_an_older_writer() {
    let b = MemBackend::default();
    publish(&b, WT, F1, "h1", SCHEMA_V1, NOW);
    // an older daemon's unconditional manifest PUT: bytes of another tree over the manifest, the
    // record untouched (an older publish creates the record only when absent)
    let other = MemBackend::default();
    publish(&other, WT, F2, "h2", SCHEMA_V1, NOW + 5);
    b.put(WT, MANIFEST_KEY, &other.get(WT, MANIFEST_KEY).unwrap().unwrap()).unwrap();
    let v = verdict(&b, WT).unwrap();
    assert_eq!(v.kind, OlderKind::ManifestReplaced);
    // a current publish over it heals it, and says what it found
    let r = publish(&b, WT, F2, "h3", SCHEMA_V2, NOW + 10);
    assert_eq!(r.older_writer_before.unwrap().kind, OlderKind::ManifestReplaced);
    assert_eq!(verdict(&b, WT), None);
}

#[test]
fn a_pending_announcement_is_not_a_verdict() {
    // An announcement whose manifest is not stored (in flight, or a publisher that died): the
    // manifest is older than the record, not written by someone who does not announce.
    let b = MemBackend::default();
    publish(&b, WT, F1, "h1", SCHEMA_V2, NOW);
    let (m, mut h) = state(&b, WT);
    let mut i = h.publishing.clone().unwrap();
    i.done = false;
    i.tree = "t1:other".into();
    h.publishing = Some(i);
    assert_eq!(detect_older_writer(Some(&m), Some(&h)), None);
}

#[test]
fn a_manifest_with_no_record_and_a_record_with_no_manifest() {
    let b = MemBackend::default();
    publish(&b, WT, F1, "h1", SCHEMA_V2, NOW);
    let (m, h) = state(&b, WT);
    assert_eq!(detect_older_writer(Some(&m), None).unwrap().kind, OlderKind::NoHandoff);
    assert_eq!(detect_older_writer(None, Some(&h)), None, "a record that still carries its announcement");
    let h2 = Handoff { publishing: None, ..h };
    assert_eq!(detect_older_writer(None, Some(&h2)).unwrap().kind, OlderKind::HandoffNoAnnouncement);
}

#[test]
fn a_claim_from_before_head_fencing_is_an_older_claim_even_with_an_announcement() {
    let b = MemBackend::default();
    publish(&b, WT, F1, "h1", SCHEMA_V2, NOW);
    let (m, h) = state(&b, WT);
    let claimed = Handoff { state: State::Claimed, merger: Some(bob()), epoch: 3, claimed_head: None, ..h };
    assert_eq!(detect_older_writer(Some(&m), Some(&claimed)).unwrap().kind, OlderKind::OldClaim);
}

#[test]
fn landed_or_archived_worktrees_are_not_reported() {
    let b = MemBackend::default();
    publish(&b, WT, F1, "h1", SCHEMA_V1, NOW);
    rewrite_like_older_daemon(&b, WT);
    let (m, h) = state(&b, WT);
    for s in [State::Merged, State::Archived, State::Orphan] {
        let h = Handoff { state: s, ..h.clone() };
        assert_eq!(detect_older_writer(Some(&m), Some(&h)), None, "{s:?}");
    }
}

#[test]
fn registry_entries_carry_the_check_and_an_entry_without_it_is_an_older_daemons() {
    let b = MemBackend::default();
    publish(&b, WT, F1, "h1", SCHEMA_V2, NOW);
    let e = summarize(&b, "feat-f1", WT).unwrap().unwrap();
    let wc = e.writer_check.clone().expect("a checking build writes the marker");
    assert_eq!((wc.level, wc.older.clone()), (WRITER_LEVEL, None));
    assert_eq!(entry_older_writer(&e), None);
    // what an older daemon wrote: no marker at all
    let old = RegistryEntry { writer_check: None, ..e.clone() };
    assert_eq!(entry_older_writer(&old).unwrap().kind, OlderKind::EntryFromOlderBuild);
    for s in [State::Merged, State::Archived, State::Orphan] {
        assert_eq!(entry_older_writer(&RegistryEntry { state: s, ..old.clone() }), None);
    }
    // a checking build that found an older writer says so on the entry
    rewrite_like_older_daemon(&b, WT);
    let found = summarize(&b, "feat-f1", WT).unwrap().unwrap();
    assert_eq!(entry_older_writer(&found).unwrap().kind, OlderKind::HandoffRewritten);
}

/// The registry entry as rc.1 reads it: the same fields, no `deny_unknown_fields`.
#[derive(serde::Deserialize, Debug)]
#[allow(dead_code)]
struct Rc1Entry {
    slug: String,
    bucket: String,
    branch: String,
    head_commit: String,
    state: State,
    writer: Actor,
    merger: Option<Actor>,
    doc_count: usize,
    file_count: usize,
    updated_ms: u64,
    #[serde(default)]
    orphan: Option<Value>,
}

#[test]
fn an_rc1_shaped_entry_parser_reads_an_entry_that_carries_the_check() {
    let b = MemBackend::default();
    publish(&b, WT, F1, "h1", SCHEMA_V2, NOW);
    rewrite_like_older_daemon(&b, WT);
    let e = summarize(&b, "feat-f1", WT).unwrap().unwrap();
    assert!(e.writer_check.as_ref().unwrap().older.is_some());
    let bytes = serde_json::to_vec_pretty(&e).unwrap();
    assert!(String::from_utf8_lossy(&bytes).contains("writer_check"));
    let r: Rc1Entry = serde_json::from_slice(&bytes).expect("an older reader ignores the field");
    assert_eq!((r.slug.as_str(), r.bucket.as_str()), ("feat-f1", WT));
    // and this build reads an entry written without it
    let mut v: Value = serde_json::from_slice(&bytes).unwrap();
    v.as_object_mut().unwrap().remove("writer_check");
    let back: RegistryEntry = serde_json::from_value(v).unwrap();
    assert_eq!(back.writer_check, None);
    // it does not touch the tree digest: the manifest has no new field at all
    let (m, _) = state(&b, WT);
    let d = tree_digest(&m);
    publish(&b, WT, F1, "h1", SCHEMA_V2, NOW + 1);
    assert_eq!(tree_digest(&state(&b, WT).0), d);
}

#[test]
fn a_registry_entry_written_to_the_project_bucket_is_listed_with_its_check() {
    let b = MemBackend::default();
    let wt = expected_worktree_bucket(PR, "feat-f1").unwrap();
    publish(&b, &wt, F1, "h1", SCHEMA_V2, NOW);
    let e = summarize(&b, "feat-f1", &wt).unwrap().unwrap();
    write_registry_entry(&b, PR, &e).unwrap();
    let got = list_shared(&b, PR).unwrap();
    assert_eq!(got[0].writer_check, e.writer_check);
}

#[test]
fn a_claim_reports_the_older_writer_and_can_refuse_it_without_writing() {
    let b = MemBackend::default();
    publish(&b, WT, F1, "h1", SCHEMA_V2, NOW);
    let (h, v) = claim_merge_checked(&b, WT, &bob(), NOW + 1, |_| true, true).unwrap();
    assert_eq!((h.state, v), (State::Claimed, None), "clean: claimed, no verdict, even when refusing");

    let b = MemBackend::default();
    publish(&b, WT, F1, "h1", SCHEMA_V2, NOW);
    rewrite_like_older_daemon(&b, WT);
    let before = b.get(WT, HANDOFF_KEY).unwrap();
    let e = claim_merge_checked(&b, WT, &bob(), NOW + 1, |_| true, true).unwrap_err();
    assert!(matches!(e, S2Error::Invalid(_)), "{e:?}");
    assert!(e.to_string().contains("gate item 8"), "{e}");
    assert_eq!(b.get(WT, HANDOFF_KEY).unwrap(), before, "a refused claim writes nothing");
    // warn-only: the claim goes through and the verdict comes back
    let (h, v) = claim_merge_checked(&b, WT, &bob(), NOW + 2, |_| true, false).unwrap();
    assert_eq!(h.state, State::Claimed);
    assert_eq!(v.unwrap().kind, OlderKind::HandoffRewritten);
    // the existing entry point is unchanged
    let b = MemBackend::default();
    publish(&b, WT, F1, "h1", SCHEMA_V2, NOW);
    rewrite_like_older_daemon(&b, WT);
    assert_eq!(claim_merge(&b, WT, &bob(), NOW + 1).unwrap().state, State::Claimed);
}

#[test]
fn a_land_reports_the_older_writer_and_can_refuse_it_without_writing() {
    // unfenced: only the record is read, so the verdict is the record's
    let b = MemBackend::default();
    publish(&b, WT, F1, "h1", SCHEMA_V2, NOW);
    assert_eq!(mark_landed_checked(&b, WT, &bob(), None, "landed", NOW + 1, true).unwrap(), (true, None));
    let b = MemBackend::default();
    publish(&b, WT, F1, "h1", SCHEMA_V2, NOW);
    rewrite_like_older_daemon(&b, WT);
    let before = b.get(WT, HANDOFF_KEY).unwrap();
    let e = mark_landed_checked(&b, WT, &bob(), None, "landed", NOW + 1, true).unwrap_err();
    assert!(matches!(e, S2Error::Invalid(_)) && e.to_string().contains("gate item 8"), "{e}");
    assert_eq!(b.get(WT, HANDOFF_KEY).unwrap(), before);
    let (landed, v) = mark_landed_checked(&b, WT, &bob(), None, "landed", NOW + 2, false).unwrap();
    assert!(landed);
    assert_eq!(v.unwrap().kind, OlderKind::HandoffNoAnnouncement, "the record alone cannot see the schema");
}

#[test]
fn a_fenced_land_sees_the_manifest_too() {
    // schema-1 manifest by an older writer, record rewritten by this build's claim afterwards
    let b = MemBackend::default();
    publish(&b, WT, F1, "h1", SCHEMA_V1, NOW);
    rewrite_like_older_daemon(&b, WT);
    let h = claim_merge(&b, WT, &bob(), NOW + 1).unwrap();
    // this build's claim records the head, but the record still has no announcement
    let (landed, v) = mark_landed_checked(&b, WT, &bob(), Some(h.epoch), "landed", NOW + 2, false).unwrap();
    assert!(landed);
    assert_eq!(v.unwrap().kind, OlderKind::Schema1NoAnnouncement);
}

#[test]
fn a_clean_land_and_claim_cost_no_more_store_calls_than_the_unchecked_ones() {
    use std::cell::Cell;
    struct Count<'a>(&'a MemBackend, Cell<usize>);
    impl ObjectBackend for Count<'_> {
        fn head(&self, b: &str, k: &str) -> weftos_store::overlay::Result<Option<weftos_store::overlay::ObjectMeta>> {
            self.1.set(self.1.get() + 1);
            self.0.head(b, k)
        }
        fn get(&self, b: &str, k: &str) -> weftos_store::overlay::Result<Option<Vec<u8>>> {
            self.1.set(self.1.get() + 1);
            self.0.get(b, k)
        }
        fn put(&self, b: &str, k: &str, d: &[u8]) -> weftos_store::overlay::Result<weftos_store::overlay::ObjectMeta> {
            self.1.set(self.1.get() + 1);
            self.0.put(b, k, d)
        }
        fn put_cas(&self, b: &str, k: &str, d: &[u8], e: Option<&str>) -> weftos_store::overlay::Result<bool> {
            self.1.set(self.1.get() + 1);
            self.0.put_cas(b, k, d, e)
        }
        fn delete(&self, b: &str, k: &str) -> weftos_store::overlay::Result<()> {
            self.0.delete(b, k)
        }
        fn list(&self, b: &str, p: &str) -> weftos_store::overlay::Result<Vec<weftos_store::overlay::ObjectMeta>> {
            self.0.list(b, p)
        }
    }
    let calls = |f: &dyn Fn(&Count<'_>)| {
        let m = MemBackend::default();
        publish(&m, WT, F1, "h1", SCHEMA_V2, NOW);
        let c = Count(&m, Cell::new(0));
        f(&c);
        c.1.get()
    };
    let plain = calls(&|c| {
        claim_merge_with(c, WT, &bob(), NOW + 1, |_| true).unwrap();
    });
    let checked = calls(&|c| {
        claim_merge_checked(c, WT, &bob(), NOW + 1, |_| true, true).unwrap();
    });
    assert_eq!(plain, checked, "claim");
    let plain = calls(&|c| {
        mark_landed_at(c, WT, &bob(), None, "l", NOW + 1).unwrap();
    });
    let checked = calls(&|c| {
        mark_landed_checked(c, WT, &bob(), None, "l", NOW + 1, true).unwrap();
    });
    assert_eq!(plain, checked, "unfenced land");
}
