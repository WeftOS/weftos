//! WO-069 lane T: exact backend-call budgets of the S2 writer and landing verbs, as the library
//! makes them (the daemon adds its own calls on top; see `clawft-weave` `store_worktree_budget`).
//! Order of the six kinds everywhere: `[get, head, list, put, put_cas, delete, copy]`.
mod s2_budget_common;
use s2_budget_common::*;
use weftos_store::overlay::Overlay;
use weftos_store::orphan::{OrphanPolicy, mark_orphan};
use weftos_store::promote::{ArchiveTarget, PromotionPolicy, promote};
use weftos_store::share::*;
use weftos_store::share_fetch::*;

#[test]
fn publish_first_no_change_one_changed_and_refresh() {
    let b = spy();
    let s = src(N, D, "t");
    create(&b, "wt0");
    // first publish_tree: handoff HEAD (no record; lane Y A4 reads it with its etag, so HEAD replaces
    // the GET); manifest HEAD (no manifest yet); one whole-bucket LIST; N object PUT (pool objects,
    // lane D3); no bundle DELETE (the listing shows no bundle object, WO-069 X3); the handoff create,
    // carrying the publish announcement and made BEFORE the manifest, as ONE conditional put
    // (put_cas). Lane D3: the manifest write is a create-only put_cas (was a PUT: put -1, put_cas
    // +1), then the post-check (design note B1) reads the record (HEAD + GET) and marks the
    // announcement done with a conditional put (+1 HEAD, +1 GET, +1 put_cas).
    let (r, c) = delta(&b, || publish(&b, "wt0", &s, "h0", 5_000));
    assert_eq!((r.uploaded as u64, r.unchanged), (N, 0));
    assert_eq!(c, [1, 3, 1, N, 3, 0, 0], "first publish_tree: handoff HEAD, manifest HEAD, LIST, N PUT, announcement + manifest + settle put_cas, settle HEAD + GET");
    // the daemon's refresh: manifest HEAD + GET, handoff GET, entry PUT, registry LIST, digest PUT
    let (_, c) = delta(&b, || refresh(&b, "wt0"));
    assert_eq!(c, [2, 1, 1, 2, 0, 0, 0], "refresh: summarize (manifest HEAD+GET, handoff GET), entry PUT, digest = LIST + PUT");
    // nothing changed: the unchanged check uses the single LIST, no object is written; the manifest
    // is still rewritten. X3: no bundle DELETE (none listed) and no handoff create (it was just
    // read): delete 1 -> 0 and put_cas 1 -> 0. Lane Y A4 adds two: the handoff is read with its etag
    // (+1 HEAD) and the publish announcement is one conditional put on that version before the
    // manifest write (+1 put_cas), so a fenced merge cannot land between its fence and its write.
    // Lane D3: the manifest write is a put_cas on the version read (PUT -1, put_cas +1), and the
    // post-check re-reads the record and marks the announcement done (+1 HEAD, +1 GET, +1 put_cas):
    // 7 calls -> 10.
    let (r, c) = delta(&b, || publish(&b, "wt0", &s, "h0", 6_000));
    assert_eq!((r.uploaded, r.unchanged as u64), (0, N));
    assert_eq!(c, [3, 3, 1, 0, 3, 0, 0], "no-change publish_tree: handoff HEAD + GET, manifest HEAD + GET, LIST, announcement put_cas, manifest put_cas, settle HEAD + GET + put_cas");
    // one file changed: the same plus one object PUT
    std::fs::write(s.dir.path().join("src/f10.rs"), "fn f10() { changed }\n").unwrap();
    let (r, c) = delta(&b, || publish(&b, "wt0", &s, "h1", 7_000));
    assert_eq!((r.uploaded, r.unchanged as u64), (1, N - 1));
    assert_eq!(c, [3, 3, 1, 1, 3, 0, 0], "one-file-changed publish_tree = no-change + 1 object PUT (the old object is retired, deleted after the grace)");
}

#[test]
fn fetch_tree_bundle_and_repeat() {
    let b = spy();
    let s = src(N, D, "t");
    create(&b, "wt0");
    let bdir = tempfile::tempdir().unwrap();
    let bundle = bdir.path().join("x.bundle");
    std::fs::write(&bundle, b"BUNDLE").unwrap();
    publish_tree(&b, &bucket("wt0"), &tree_src(&s, "wt0", "h0", Some(&bundle)), &Limits::default(), 5_000).unwrap();
    refresh(&b, "wt0");
    let out = tempfile::tempdir().unwrap();
    // whole tree, first: manifest HEAD + GET, then ONE tree LIST (N is over HEAD_LIST_THRESHOLD, so
    // no per-file HEAD: presence and size come from the listing) and one GET per file
    let (r, c) = delta(&b, || fetch_tree(&b, &bucket("wt0"), out.path(), Select::All, false).unwrap());
    assert_eq!(r.written.len() as u64, N);
    assert_eq!(c, [1 + N, 1, 1, 0, 0, 0, 0], "manifest HEAD+GET, 1 tree LIST, N x GET");
    // repeat, nothing changed: manifest only; local sha256 equal so no object call
    let (r, c) = delta(&b, || fetch_tree(&b, &bucket("wt0"), out.path(), Select::All, false).unwrap());
    assert_eq!(r.unchanged.len() as u64, N);
    assert_eq!(c, [1, 1, 0, 0, 0, 0, 0], "repeat fetch: manifest HEAD + GET");
    // bundle: manifest read again inside fetch_bundle, bundle HEAD + GET (whole bundle every time)
    let bo = out.path().join("x.bundle");
    let (_, c) = delta(&b, || fetch_bundle(&b, &bucket("wt0"), &bo).unwrap());
    assert_eq!(c, [2, 2, 0, 0, 0, 0, 0], "fetch_bundle: manifest HEAD+GET, bundle HEAD+GET");
    let (_, c) = delta(&b, || fetch_bundle(&b, &bucket("wt0"), &bo).unwrap());
    assert_eq!(c, [2, 2, 0, 0, 0, 0, 0], "repeat bundle download is not skipped");
}

#[test]
fn claim_and_mark_merged() {
    let b = spy();
    project(&b, 1);
    // claim_merge: the published head (manifest HEAD + GET, recorded as the claimed head, 6a-1),
    // handoff GET with its version (HEAD + GET), conditional write (put_cas)
    let (h, c) = delta(&b, || claim_merge(&b, &bucket("wt0"), &actor("bob"), 9_000).unwrap());
    assert_eq!(h.epoch, 1);
    assert_eq!(c, [2, 2, 0, 0, 1, 0, 0], "claim_merge");
    let (_, c) = delta(&b, || refresh(&b, "wt0"));
    assert_eq!(c, [2, 1, 1, 2, 0, 0, 0], "claim refresh");
    let (_, c) = delta(&b, || mark_merged(&b, &bucket("wt0"), &actor("bob"), "done", 9_500).unwrap());
    // + manifest HEAD + GET: the merge is fenced on the head that was claimed (6a-1)
    assert_eq!(c, [2, 2, 0, 0, 1, 0, 0], "mark_merged");
}

#[test]
fn promote_scales_with_edits_and_reserved_objects() {
    for e in [1u64, 3] {
        let b = spy();
        let s = src(N, D, "t");
        create(&b, "wt0");
        b.make_bucket("archive-bkt");
        publish(&b, "wt0", &s, "h0", 5_000);
        let (wt, pr) = (bucket("wt0"), PR);
        let ov = Overlay::new(&b, &wt, pr);
        for i in 0..e {
            ov.put(&format!("assets/a{i}.bin"), b"x").unwrap();
        }
        let arch = ArchiveTarget { bucket: "archive-bkt", prefix: "wt0/" };
        // Reserved objects archived at the end: N tree objects, manifest, handoff, meta, and one
        // base record per edited key.
        let r = N + 3 + e;
        let (rep, c) = delta(&b, || promote(&b, &wt, pr, &arch, &PromotionPolicy::default(), 10_000).unwrap());
        assert_eq!(rep.promoted.len() as u64, e);
        // GET: is_promoted + read_meta (2), per edit: base read + data read for the CAS copy (2e)
        // HEAD: per edit, plan HEADs the project key twice and `displace` once (3e)
        // LIST: local objects, tombstones, reserved
        // PUT: intent + PROMOTED record; CAS: one per promoted key; COPY: every reserved object
        assert_eq!(c, [2 + 2 * e, 3 * e, 3, 2, e, 0, r], "promote e={e}");
    }
}

#[test]
fn orphan_mark_refusal_and_mark() {
    let b = spy();
    project(&b, 1);
    let pol = OrphanPolicy::default();
    // missing for less than the grace: three reads, refused TooSoon
    let (r, c) = delta(&b, || mark_orphan(&b, PR, "wt0", 100_000, 99_000, &pol));
    assert!(r.is_err());
    assert_eq!(c, [3, 0, 0, 0, 0, 0, 0], "too soon: entry GET, is_promoted GET, handoff GET (no HEAD)");
    let (r, c) = delta(&b, || mark_orphan(&b, PR, "wt0", 100_000_000, 1_000, &pol));
    assert!(r.is_ok());
    assert_eq!(c, [3, 0, 1, 2, 0, 0, 0], "marked: the three reads, entry PUT, digest = LIST + PUT");
    // a later sweep pass over an orphan: the entry GET alone
    let (r, c) = delta(&b, || mark_orphan(&b, PR, "wt0", 100_000_001, 1_000, &pol));
    assert!(r.is_ok());
    assert_eq!(c, [1, 0, 0, 0, 0, 0, 0], "already orphan: entry GET");
}
