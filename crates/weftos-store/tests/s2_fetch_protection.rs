//! WO-069 lane X1: `fetch_tree` no longer HEADs every file. What the per-file HEAD protected must
//! still hold on both size-check paths: up to `HEAD_LIST_THRESHOLD` objects are HEADed, more are
//! sized by ONE listing of the tree prefix. Each case runs on both paths.
//! Kinds: `[get, head, list, put, put_cas, delete, copy]`.
mod s2_budget_common;
use s2_budget_common::*;
use weftos_store::overlay::ObjectBackend;
use weftos_store::share::*;
use weftos_store::share_fetch::*;

/// A published tree of `n` plain files; `n` above the threshold takes the listing path.
fn published(n: u64) -> (Spy, Src) {
    let b = spy();
    let s = src(n, 0, "t");
    create(&b, "wt0");
    publish(&b, "wt0", &s, "h0", 5_000);
    (b, s)
}

fn key(i: u64) -> String {
    format!("{TREE_PREFIX}src/f{i}.rs")
}

const SMALL: u64 = HEAD_LIST_THRESHOLD as u64;
const LARGE: u64 = HEAD_LIST_THRESHOLD as u64 + 12;

#[test]
fn a_missing_object_is_reported_missing_on_both_paths() {
    for n in [SMALL, LARGE] {
        let (b, _s) = published(n);
        b.be.inner.delete(&bucket("wt0"), &key(3)).unwrap();
        let out = tempfile::tempdir().unwrap();
        let (r, c) = delta(&b, || fetch_tree(&b, &bucket("wt0"), out.path(), Select::All, false).unwrap());
        assert_eq!(r.missing, ["src/f3.rs"], "n = {n}");
        assert_eq!(r.written.len() as u64, n - 1);
        assert!(!out.path().join("src/f3.rs").exists());
        if n > SMALL {
            // manifest HEAD; one tree LIST; manifest GET + a GET for each object that is listed
            assert_eq!(c, [1 + (n - 1), 1, 1, 0, 0, 0, 0], "listing path: no per-file HEAD, absent object costs no GET");
        } else {
            assert_eq!(c, [1 + (n - 1), 1 + n, 0, 0, 0, 0, 0], "HEAD path: a HEAD per object, absent one costs no GET");
        }
    }
}

#[test]
fn an_object_over_the_cap_is_refused_before_download_and_never_written() {
    for n in [SMALL, LARGE] {
        let (b, _s) = published(n);
        // The manifest still says this file is a few bytes; the stored object is not.
        b.be.inner.put(&bucket("wt0"), &key(2), &vec![b'x'; 5_000]).unwrap();
        let lim = Limits { max_file: 1_000, ..Limits::default() };
        let out = tempfile::tempdir().unwrap();
        // manifest order is by path, not by number
        let before = read_manifest(&b, &bucket("wt0")).unwrap().unwrap().files.iter().position(|f| f.path == "src/f2.rs").unwrap() as u64;
        b.reset();
        let e = fetch_tree_with(&b, &bucket("wt0"), out.path(), Select::All, false, &lim).unwrap_err().to_string();
        assert!(e.contains("per-file limit") && e.contains("src/f2.rs"), "n = {n}: {e}");
        assert!(!out.path().join("src/f2.rs").exists(), "n = {n}: nothing of the oversized object is written");
        // Files before it were fine and are written; the oversized object itself was never GET:
        // the manifest GET plus the objects before it.
        assert_eq!(b.counts().get, 1 + before, "n = {n}: the oversized object is not downloaded");
    }
}

#[test]
fn a_manifest_that_claims_an_oversized_file_is_refused_on_both_paths() {
    for n in [SMALL, LARGE] {
        let (b, _s) = published(n);
        let lim = Limits { max_file: 10, ..Limits::default() }; // every file is a bit over 10 bytes
        let out = tempfile::tempdir().unwrap();
        b.reset();
        let e = fetch_tree_with(&b, &bucket("wt0"), out.path(), Select::All, false, &lim).unwrap_err().to_string();
        assert!(e.contains("per-file limit"), "n = {n}: {e}");
        assert_eq!(b.counts().get, 1, "n = {n}: manifest only; no object GET");
        assert_eq!(std::fs::read_dir(out.path()).unwrap().count(), 0, "n = {n}: nothing written");
    }
}

#[test]
fn the_total_cap_is_enforced_from_stored_sizes_on_both_paths() {
    for n in [SMALL, LARGE] {
        let (b, _s) = published(n);
        let lim = Limits { max_total: 60, ..Limits::default() };
        let out = tempfile::tempdir().unwrap();
        let e = fetch_tree_with(&b, &bucket("wt0"), out.path(), Select::All, false, &lim).unwrap_err().to_string();
        assert!(e.contains("total limit"), "n = {n}: {e}");
        assert!(std::fs::read_dir(out.path()).unwrap().count() < n as usize, "n = {n}: stopped part way");
    }
}

#[test]
fn a_body_that_differs_from_the_manifest_is_refused_by_the_checksum_on_both_paths() {
    for n in [SMALL, LARGE] {
        let (b, _s) = published(n);
        b.be.inner.put(&bucket("wt0"), &key(1), b"tampered, and of another length").unwrap();
        let out = tempfile::tempdir().unwrap();
        let e = fetch_tree(&b, &bucket("wt0"), out.path(), Select::All, false).unwrap_err().to_string();
        assert!(e.contains("checksum mismatch for src/f1.rs"), "n = {n}: {e}");
        assert!(!out.path().join("src/f1.rs").exists(), "n = {n}: a refused body is not written");
    }
}

#[test]
fn a_replaced_object_that_grew_after_the_listing_is_still_held_to_the_cap() {
    // The listing said "small"; the body that comes back is over the cap: refused before any write.
    let (b, _s) = published(LARGE);
    let lim = Limits { max_file: 1_000, ..Limits::default() };
    let out = tempfile::tempdir().unwrap();
    struct Grow<'a>(&'a Spy);
    impl ObjectBackend for Grow<'_> {
        fn head(&self, b: &str, k: &str) -> weftos_store::overlay::Result<Option<weftos_store::overlay::ObjectMeta>> {
            self.0.head(b, k)
        }
        fn get(&self, b: &str, k: &str) -> weftos_store::overlay::Result<Option<Vec<u8>>> {
            Ok(self.0.get(b, k)?.map(|d| if k.ends_with("src/f5.rs") { vec![b'y'; 5_000] } else { d }))
        }
        fn put(&self, b: &str, k: &str, d: &[u8]) -> weftos_store::overlay::Result<weftos_store::overlay::ObjectMeta> {
            self.0.put(b, k, d)
        }
        fn delete(&self, b: &str, k: &str) -> weftos_store::overlay::Result<()> {
            self.0.delete(b, k)
        }
        fn list(&self, b: &str, p: &str) -> weftos_store::overlay::Result<Vec<weftos_store::overlay::ObjectMeta>> {
            self.0.list(b, p)
        }
    }
    let e = fetch_tree_with(&Grow(&b), &bucket("wt0"), out.path(), Select::All, false, &lim).unwrap_err().to_string();
    assert!(e.contains("per-file limit") && e.contains("src/f5.rs"), "{e}");
    assert!(!out.path().join("src/f5.rs").exists());
}

#[test]
fn a_repeat_fetch_and_a_fetch_of_few_paths_do_not_list() {
    let (b, _s) = published(LARGE);
    let out = tempfile::tempdir().unwrap();
    fetch_tree(&b, &bucket("wt0"), out.path(), Select::All, false).unwrap();
    let (r, c) = delta(&b, || fetch_tree(&b, &bucket("wt0"), out.path(), Select::All, false).unwrap());
    assert_eq!(r.unchanged.len() as u64, LARGE);
    assert_eq!(c, [1, 1, 0, 0, 0, 0, 0], "repeat: nothing to download, so no listing and no HEAD");
    let out2 = tempfile::tempdir().unwrap();
    let paths = vec!["src/f0.rs".to_string(), "src/f1.rs".to_string()];
    let (r, c) = delta(&b, || fetch_tree(&b, &bucket("wt0"), out2.path(), Select::Paths(&paths), false).unwrap());
    assert_eq!(r.written.len(), 2);
    assert_eq!(c, [1 + 2, 1 + 2, 0, 0, 0, 0, 0], "two paths: a HEAD each, no listing of the whole tree");
}

#[test]
fn one_manifest_snapshot_serves_tree_and_bundle() {
    let b = spy();
    let s = src(N, D, "t");
    create(&b, "wt0");
    let bdir = tempfile::tempdir().unwrap();
    let bundle = bdir.path().join("x.bundle");
    std::fs::write(&bundle, b"BUNDLE").unwrap();
    publish_tree(&b, &bucket("wt0"), &tree_src(&s, "wt0", "h0", Some(&bundle)), &Limits::default(), 5_000).unwrap();
    let m = read_manifest(&b, &bucket("wt0")).unwrap().unwrap();
    let out = tempfile::tempdir().unwrap();
    let bo = out.path().join("x.bundle");
    let ((r, head), c) = delta(&b, || {
        let r = fetch_tree_from(&b, &bucket("wt0"), &m, out.path(), Select::All, false, &Limits::default()).unwrap();
        (r, fetch_bundle_from(&b, &bucket("wt0"), &m, &bo).unwrap())
    });
    assert_eq!((r.written.len() as u64, head.as_str()), (N, "h0"));
    // no manifest call at all: N object GETs + 1 tree LIST, bundle HEAD + GET
    assert_eq!(c, [N + 1, 1, 1, 0, 0, 0, 0], "tree from the snapshot (N object GETs, one LIST), bundle HEAD + GET");
}
