//! WO-069 lane X3: the publish path makes fewer store calls and still removes what it must. The
//! bundle DELETE only happens when a bundle object is listed (a crash leftover still goes), the
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

#[test]
fn a_crash_leftover_bundle_is_still_removed_and_a_clean_bucket_makes_no_delete() {
    let b = spy();
    let s = src(N, D, "t");
    create(&b, "wt0");
    publish(&b, "wt0", &s, "h0", 5_000);
    // A publish that died after its bundle PUT, before its manifest PUT: a bundle no manifest names.
    b.put(&bucket("wt0"), BUNDLE_KEY, b"LEFTOVER").unwrap();
    assert!(read_manifest(&b, &bucket("wt0")).unwrap().is_some_and(|m| !m.has_bundle));
    let (_, c) = delta(&b, || publish(&b, "wt0", &s, "h0", 6_000));
    assert_eq!(c[5], 1, "the listed leftover is deleted");
    assert!(b.get(&bucket("wt0"), BUNDLE_KEY).unwrap().is_none(), "leftover gone");
    let (_, c) = delta(&b, || publish(&b, "wt0", &s, "h0", 7_000));
    assert_eq!(c[5], 0, "no bundle listed: no DELETE");
}

#[test]
fn a_leftover_bundle_on_a_first_publish_is_removed_too() {
    let b = spy();
    let s = src(N, D, "t");
    create(&b, "wt0");
    b.put(&bucket("wt0"), BUNDLE_KEY, b"LEFTOVER").unwrap();
    publish(&b, "wt0", &s, "h0", 5_000);
    assert!(b.get(&bucket("wt0"), BUNDLE_KEY).unwrap().is_none());
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
    // the same head republished without a bundle path: the manifest still says has_bundle, keep it
    let (_, c) = delta(&b, || publish(&b, "wt0", &s, "h0", 6_000));
    assert_eq!(c[5], 0);
    assert!(b.get(&bucket("wt0"), BUNDLE_KEY).unwrap().is_some());
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
    assert_eq!(b.counts().put_cas, 0, "no create-only put on a republish");
    assert_eq!(read_handoff(&b, &bucket("wt0")).unwrap().unwrap(), h, "the record is untouched");
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
    // an object put before the listing is swept now; one put after it is not seen by this run (the
    // same as the tree-prefix listing publish_tree took before: the sweep works from its listing)
    b.put(&bucket("wt0"), ".wt/tree/early.rs", b"early").unwrap();
    let l = whole(&b, "wt0");
    b.put(&bucket("wt0"), ".wt/tree/late.rs", b"late").unwrap();
    let p = publish_tree_listed(&b, &bucket("wt0"), &tree_src(&s, "wt0", "h0", None), &Limits::default(), 6_000, Some(l)).unwrap();
    assert_eq!(p.report.removed, 1);
    assert!(b.get(&bucket("wt0"), ".wt/tree/early.rs").unwrap().is_none());
    assert!(b.get(&bucket("wt0"), ".wt/tree/late.rs").unwrap().is_some(), "not in the listing: left, never half-handled");
    let p = publish(&b, "wt0", &s, "h0", 7_000);
    assert_eq!(p.removed, 1, "the next publish lists it and removes it");
    assert!(b.get(&bucket("wt0"), ".wt/tree/late.rs").unwrap().is_none());
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
