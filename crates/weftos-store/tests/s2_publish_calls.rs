//! WO-069 lane X3: the publish path makes fewer store calls and still removes what it must. The
//! bundle DELETE only happens when a bundle object is listed (a crash leftover still goes; since lane
//! D3 a grace period after a publish first sees it, like every unreferenced pool object), the
//! handoff create is not attempted when the record was just read, and a caller-supplied listing
//! behaves like the one `publish_tree` would take, including when it is stale.
mod s2_budget_common;
use s2_budget_common::*;
use weftos_store::overlay::ObjectBackend;
use weftos_store::share::*;
use weftos_store::share_fetch::summarize;

fn whole(b: &Spy, slug: &str) -> Vec<weftos_store::overlay::ObjectMeta> {
    b.list(&bucket(slug), "").unwrap()
}

const G: u64 = DEFAULT_GRACE_MS;

#[test]
fn a_crash_leftover_in_the_pool_is_recorded_on_first_sight_swept_a_grace_later_and_a_clean_bucket_makes_no_delete() {
    // Lane D3: a publish that died after a pool PUT, before its manifest write, leaves an object no
    // manifest names. The listing has no usable modification time, so its age counts from the first
    // publish that sees it, and it goes a full grace later (before: deleted at once).
    let b = spy();
    let s = src(N, D, "t");
    create(&b, "wt0");
    publish(&b, "wt0", &s, "h0", 5_000);
    let leftover = obj_key(&sha256_hex(b"LEFTOVER"), 1);
    b.put(&bucket("wt0"), &leftover, b"LEFTOVER").unwrap();
    let (r, c) = delta(&b, || publish(&b, "wt0", &s, "h0", 6_000));
    assert_eq!((c[5], r.retired), (0, 1), "recorded, not deleted");
    let m = read_manifest(&b, &bucket("wt0")).unwrap().unwrap();
    assert!(m.retiring.iter().any(|x| x.key() == leftover && x.since_ms == 6_000), "{:?}", m.retiring);
    let (_, c) = delta(&b, || publish(&b, "wt0", &s, "h0", 6_000 + G - 1));
    assert_eq!(c[5], 0, "inside the grace");
    let (_, c) = delta(&b, || publish(&b, "wt0", &s, "h0", 6_000 + G));
    assert_eq!(c[5], 1, "the leftover is deleted a grace after it was first seen");
    assert!(b.get(&bucket("wt0"), &leftover).unwrap().is_none(), "leftover gone");
    let (_, c) = delta(&b, || publish(&b, "wt0", &s, "h0", 7_000 + 2 * G));
    assert_eq!(c[5], 0, "nothing unreferenced listed: no DELETE");
}

#[test]
fn a_schema_1_bundle_key_left_in_a_new_bucket_is_kept_for_the_grace_then_removed() {
    let b = spy();
    let s = src(N, D, "t");
    create(&b, "wt0");
    b.put(&bucket("wt0"), BUNDLE_KEY, b"LEFTOVER").unwrap();
    publish(&b, "wt0", &s, "h0", 5_000);
    assert!(b.get(&bucket("wt0"), BUNDLE_KEY).unwrap().is_some(), "a schema-1 object waits a grace");
    assert_eq!(read_manifest(&b, &bucket("wt0")).unwrap().unwrap().v1_retired_ms, Some(5_000));
    publish(&b, "wt0", &s, "h0", 5_000 + G);
    assert!(b.get(&bucket("wt0"), BUNDLE_KEY).unwrap().is_none());
    publish(&b, "wt0", &s, "h0", 6_000 + G);
    assert_eq!(read_manifest(&b, &bucket("wt0")).unwrap().unwrap().v1_retired_ms, None, "nothing schema-1 left");
}

#[test]
fn a_manifest_bundle_is_kept_and_never_deleted() {
    let b = spy();
    let s = src(N, D, "t");
    create(&b, "wt0");
    let dir = tempfile::tempdir().unwrap();
    let bundle = dir.path().join("x.bundle");
    std::fs::write(&bundle, b"BUNDLE").unwrap();
    publish_tree(&b, &bucket("wt0"), &tree_src(&s, "wt0", "h0", Some(&bundle)), &Limits::default(), 5_000).unwrap();
    let key = read_manifest(&b, &bucket("wt0")).unwrap().unwrap().bundle_object().unwrap();
    assert!(key.starts_with(OBJ_PREFIX), "{key}");
    // the same head republished without a bundle path, past a grace: the manifest still names it
    let (_, c) = delta(&b, || publish(&b, "wt0", &s, "h0", 6_000 + 2 * G));
    assert_eq!(c[5], 0);
    let m = read_manifest(&b, &bucket("wt0")).unwrap().unwrap();
    assert_eq!(m.bundle_object().as_deref(), Some(key.as_str()));
    assert_eq!(b.get(&bucket("wt0"), &key).unwrap().as_deref(), Some(&b"BUNDLE"[..]));
}

#[test]
fn the_handoff_create_is_skipped_when_the_record_exists_and_reported_when_made() {
    let b = spy();
    let s = src(N, D, "t");
    create(&b, "wt0");
    let first = publish_tree_listed(&b, &bucket("wt0"), &tree_src(&s, "wt0", "h0", None), &Limits::default(), 5_000, None).unwrap();
    let h = first.handoff_created.expect("this publish created the record");
    assert_eq!(read_handoff(&b, &bucket("wt0")).unwrap().unwrap(), h, "what is reported is what is stored");
    b.reset();
    let again = publish_tree_listed(&b, &bucket("wt0"), &tree_src(&s, "wt0", "h0", None), &Limits::default(), 6_000, None).unwrap();
    assert!(again.handoff_created.is_none());
    // No create-only put on a republish. Three conditional puts: the announcement on the record read
    // at the start (lane Y A4), the manifest on the version read at the start, and the post-check
    // that marks the announcement done (lane D3, design note B1). The record is otherwise untouched.
    assert_eq!(b.counts().put_cas, 3, "announcement, manifest, settle; not a create");
    let now = read_handoff(&b, &bucket("wt0")).unwrap().unwrap();
    let i = now.publishing.clone().unwrap();
    assert_eq!((i.ms, i.done), (6_000, true));
    assert_eq!(Handoff { publishing: h.publishing.clone(), ..now }, h, "the record is otherwise untouched");
}

#[test]
fn a_supplied_listing_replaces_the_internal_one_and_summarize_with_equals_summarize() {
    let b = spy();
    let s = src(N, D, "t");
    create(&b, "wt0");
    publish(&b, "wt0", &s, "h0", 5_000);
    let l = whole(&b, "wt0");
    let (p, c) = delta(&b, || {
        publish_tree_listed(&b, &bucket("wt0"), &tree_src(&s, "wt0", "h1", None), &Limits::default(), 6_000, Some(l)).unwrap()
    });
    assert_eq!(c[2], 0, "a supplied listing means no LIST inside publish_tree");
    assert_eq!(p.report.unchanged as u64, N);
    let h = read_handoff(&b, &bucket("wt0")).unwrap().unwrap();
    let viaw = summarize_with("wt0", &bucket("wt0"), p.manifest, h);
    assert_eq!(Some(viaw), summarize(&b, "wt0", &bucket("wt0")).unwrap());
}

#[test]
fn an_object_another_writer_adds_after_the_listing_survives_this_sweep_and_goes_on_the_next() {
    let b = spy();
    let s = src(N, D, "t");
    create(&b, "wt0");
    publish(&b, "wt0", &s, "h0", 5_000);
    // an unreferenced object put before the listing is recorded now; one put after it is not seen by
    // this run (the sweep works from its listing) and is recorded by the next
    let (early, late) = (obj_key(&sha256_hex(b"early"), 1), obj_key(&sha256_hex(b"late"), 1));
    b.put(&bucket("wt0"), &early, b"early").unwrap();
    let l = whole(&b, "wt0");
    b.put(&bucket("wt0"), &late, b"late").unwrap();
    let p = publish_tree_listed(&b, &bucket("wt0"), &tree_src(&s, "wt0", "h0", None), &Limits::default(), 6_000, Some(l)).unwrap();
    assert_eq!((p.report.removed, p.report.retired), (0, 1));
    assert!(!p.manifest.retiring.iter().any(|r| r.key() == late), "not in the listing: left, never half-handled");
    let p = publish(&b, "wt0", &s, "h0", 7_000);
    assert_eq!((p.removed, p.retired), (0, 1), "the next publish lists it and records it");
    let p = publish(&b, "wt0", &s, "h0", 6_000 + G);
    assert_eq!(p.removed, 1, "early goes a grace after it was first seen");
    assert!(b.get(&bucket("wt0"), &early).unwrap().is_none() && b.get(&bucket("wt0"), &late).unwrap().is_some());
    let p = publish(&b, "wt0", &s, "h0", 7_000 + G);
    assert_eq!(p.removed, 1);
    assert!(b.get(&bucket("wt0"), &late).unwrap().is_none());
}

#[test]
fn only_tree_objects_are_swept_from_a_whole_bucket_listing() {
    let b = spy();
    let s = src(N, D, "t");
    create(&b, "wt0");
    publish(&b, "wt0", &s, "h0", 5_000);
    b.put(&bucket("wt0"), "assets/overlay.bin", &[1u8; 64]).unwrap();
    let p = publish(&b, "wt0", &s, "h0", 6_000);
    assert_eq!(p.removed, 0);
    assert!(b.get(&bucket("wt0"), "assets/overlay.bin").unwrap().is_some(), "overlay objects are not tree objects");
    assert!(b.get(&bucket("wt0"), weftos_store::promote::META_KEY).unwrap().is_some(), "control objects stay");
}
