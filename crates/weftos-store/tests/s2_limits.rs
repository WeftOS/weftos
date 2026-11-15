//! WO-069 lane D1: the per-worktree size ceiling (100 000 files, 4 GiB) is one value shared by the
//! writer and every reader, and the caps derived from it hold. Trees are built in memory.
mod s2_budget_common;
use s2_budget_common::*;
use weftos_store::follower_cache_shared::{CacheCfg, SharedCache};
use weftos_store::overlay::ObjectBackend;
use weftos_store::retention::DEFAULT_WORKTREE_QUOTA;
use weftos_store::share::*;
use weftos_store::testing::MemBackend;

const WT: &str = "w-01abc-aaaaaaaaaaaa";

fn me() -> Actor {
    Actor { agent: "w".into(), host: "h".into() }
}

/// A manifest of `n` synthetic files, paths the shape of a real tree (about 55 bytes).
fn manifest(n: usize) -> TreeManifest {
    let sha = "a".repeat(64);
    TreeManifest {
        schema: SCHEMA_V2,
        generation: 1,
        branch: "feat/x".into(),
        base_commit: "a".repeat(40),
        head_commit: "b".repeat(40),
        files: (0..n)
            .map(|i| FileEntry { path: format!("crates/some-crate-{:03}/src/module_{i:05}.rs", i % 500), size: 1_000 + i as u64, sha256: sha.clone(), generation: Some(1) })
            .collect(),
        uncommitted: vec![],
        changes: vec![],
        docs: vec![],
        has_bundle: false,
        bundle_sha256: None,
        changes_recorded: false,
        bundle_gen: None,
        retiring: vec![],
        v1_retired_ms: None,
        writer: me(),
        published_ms: 1,
    }
}

fn bytes(m: &TreeManifest) -> Vec<u8> {
    serde_json::to_vec_pretty(m).unwrap()
}

#[test]
fn the_default_ceiling_is_100k_files_and_4_gib_with_the_old_per_file_and_bundle_caps() {
    let l = Limits::default();
    assert_eq!((l.max_files, l.max_total, l.max_file, l.max_bundle), (100_000, 4 << 30, 16 << 20, 256 << 20));
    assert_eq!((Limits::DEFAULT_MAX_FILES, Limits::DEFAULT_MAX_TOTAL), (l.max_files, l.max_total));
}

#[test]
fn a_manifest_of_exactly_100_000_entries_parses_and_100_001_is_refused_naming_the_limit() {
    let at = bytes(&manifest(100_000));
    assert_eq!(parse_manifest_with(&at, &Limits::default()).unwrap().files.len(), 100_000);
    let over = bytes(&manifest(100_001));
    let e = parse_manifest_with(&over, &Limits::default()).unwrap_err().to_string();
    assert!(e.contains("100000"), "{e}");
    // and through the store: the plain reader is the same limit
    let b = MemBackend::new();
    b.put(WT, MANIFEST_KEY, &at).unwrap();
    assert_eq!(read_manifest(&b, WT).unwrap().unwrap().files.len(), 100_000);
    b.put(WT, MANIFEST_KEY, &over).unwrap();
    assert!(read_manifest(&b, WT).unwrap_err().to_string().contains("100000"));
    // the doc list is bounded by the same number
    let mut m = manifest(1);
    m.docs = (0..100_001).map(|i| DocEntry { path: format!("docs/{i}.md"), title: None }).collect();
    assert!(parse_manifest_with(&bytes(&m), &Limits::default()).unwrap_err().to_string().contains("100000"));
}

#[test]
fn manifest_cap_holds_a_full_tree_with_margin() {
    let m = bytes(&manifest(100_000));
    let per_file = m.len() as u64 / 100_000;
    println!("manifest_cap: {} bytes for 100000 files = {per_file} per file", m.len());
    // 157 bytes a file was measured on a real tree (7.9 MB for 50 000); these 55-byte paths give 175.
    assert!(per_file < 200, "a pretty manifest entry is 150 to 180 bytes; got {per_file}");
    assert!((m.len() as u64) * 3 < MAX_MANIFEST_BYTES * 2, "100k files must use under two thirds of the cap: {} of {MAX_MANIFEST_BYTES}", m.len());
    assert!(MAX_MANIFEST_BYTES / per_file > 3 * Limits::DEFAULT_MAX_FILES as u64 / 2, "the cap is hit at {} files", MAX_MANIFEST_BYTES / per_file);
}

#[test]
fn the_quota_holds_a_full_tree_plus_bundle_plus_control_objects_with_room_for_the_overlay() {
    assert_eq!(DEFAULT_WORKTREE_QUOTA, 8 << 30);
    assert_eq!(DEFAULT_WORKTREE_QUOTA, 2 * Limits::DEFAULT_MAX_TOTAL);
    let l = Limits::default();
    let control = 1 << 20; // handoff, meta, claim and sweep markers are a few KiB; 1 MiB is generous
    let full = l.max_total + l.max_bundle + MAX_MANIFEST_BYTES + control;
    assert!(full < DEFAULT_WORKTREE_QUOTA, "{full} must fit in {DEFAULT_WORKTREE_QUOTA}");
    assert!(DEFAULT_WORKTREE_QUOTA - full > 3 << 30, "room left for overlay objects: {}", DEFAULT_WORKTREE_QUOTA - full);
}

/// `n` files of `size` identical bytes. Since lane D3 identical content is stored once, so a test
/// counts files published as `uploaded + unchanged` (one upload, the rest share its object).
fn tree(n: usize, size: usize) -> (tempfile::TempDir, Vec<String>) {
    let d = tempfile::tempdir().unwrap();
    let paths: Vec<String> = (0..n).map(|i| format!("src/f{i}.txt")).collect();
    std::fs::create_dir_all(d.path().join("src")).unwrap();
    for p in &paths {
        std::fs::write(d.path().join(p), vec![b'x'; size]).unwrap();
    }
    (d, paths)
}

fn src<'a>(d: &'a tempfile::TempDir, paths: &'a [String]) -> TreeSource<'a> {
    TreeSource { root: d.path(), paths, uncommitted: &[], changes: &[], branch: "f", base_commit: "a", head_commit: "h", bundle: None, writer: me() }
}

fn lim(max_files: usize, max_total: u64) -> Limits {
    Limits { max_files, max_total, ..Limits::default() }
}

#[test]
fn a_tree_just_over_the_file_budget_skips_the_surplus_and_a_reader_with_the_same_limits_reads_it_back() {
    let (d, ps) = tree(6, 10);
    let b = MemBackend::new();
    let l = lim(5, 1 << 30);
    let r = publish_tree(&b, WT, &src(&d, &ps), &l, 1).unwrap();
    assert_eq!((r.uploaded + r.unchanged, r.skipped.len()), (5, 1));
    assert_eq!(r.skipped[0], ("src/f5.txt".to_string(), Skip::OverBudget));
    assert_eq!(read_manifest_with(&b, WT, &l).unwrap().unwrap().files.len(), 5);
    // a reader on tighter limits refuses it, naming the limit it applied
    let e = read_manifest_with(&b, WT, &lim(4, 1 << 30)).unwrap_err().to_string();
    assert!(e.contains("more than the 4 file limit"), "{e}");
    // exactly at the budget: nothing skipped
    let b2 = MemBackend::new();
    let r = publish_tree(&b2, WT, &src(&d, &ps), &lim(6, 1 << 30), 1).unwrap();
    assert_eq!((r.uploaded + r.unchanged, r.skipped.len()), (6, 0));
}

#[test]
fn a_publish_reads_its_previous_manifest_under_its_own_limits() {
    // a tree published under raised limits (100 001 entries) is previous state for the next publish
    let b = MemBackend::new();
    b.put(WT, MANIFEST_KEY, &bytes(&manifest(100_001))).unwrap();
    let (d, ps) = tree(2, 5);
    assert!(publish_tree(&b, WT, &src(&d, &ps), &Limits::default(), 2).is_err(), "default limits cannot read it");
    let raised = lim(200_000, 4 << 30);
    let r = publish_tree(&b, WT, &src(&d, &ps), &raised, 3).unwrap();
    assert_eq!(r.uploaded + r.unchanged, 2);
    assert_eq!(read_manifest_with(&b, WT, &raised).unwrap().unwrap().files.len(), 2);
}

#[test]
fn the_total_size_budget_boundary_is_inclusive() {
    let (d, ps) = tree(3, 10); // 30 bytes
    for (budget, uploaded) in [(30u64, 3usize), (29, 2), (20, 2), (19, 1), (9, 0)] {
        let b = MemBackend::new();
        let r = publish_tree(&b, WT, &src(&d, &ps), &lim(10, budget), 1).unwrap();
        assert_eq!(r.uploaded + r.unchanged, uploaded, "budget {budget}");
        assert_eq!(r.skipped.iter().filter(|(_, s)| *s == Skip::OverBudget).count(), 3 - uploaded, "budget {budget}");
    }
}

#[test]
fn a_manifest_larger_than_the_cache_budget_is_served_uncached_not_refused() {
    let b = spy();
    project(&b, 2);
    let size = ObjectBackend::head(&b, &bucket(&slug(0)), MANIFEST_KEY).unwrap().unwrap().size;
    // a budget under one manifest, and a small slot count: nothing can be stored
    let c = SharedCache::new(CacheCfg { ttl_ms: 1_000, max_age_ms: 10_000, max_bytes: size / 2, max_entries: 8 });
    for now in 0..3 {
        let l = c.browse_ls(&b, PR, &slug(0), "", now).unwrap();
        assert!(l.file_count > 0, "served");
        assert_eq!(c.footprint(), (0, 0), "not stored");
    }
    let (s, _) = c.stats();
    assert_eq!((s.cold, s.evicted), (3, 0), "each read loads cold and evicts nothing");
    // a small manifest next to it is unaffected by the oversize one
    let c = SharedCache::new(CacheCfg { ttl_ms: 1_000, max_age_ms: 10_000, max_bytes: size + size / 2, max_entries: 8 });
    c.browse_ls(&b, PR, &slug(0), "", 0).unwrap();
    assert_eq!(c.footprint().0, 1);
}
