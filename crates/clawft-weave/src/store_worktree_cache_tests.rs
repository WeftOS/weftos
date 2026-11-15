//! WO-069 X2: the daemon's read cache. "Another daemon" is a second store service over the same
//! directory: it has its own cache and its writes reach this one only through the store.
use std::sync::Arc;

use clawft_kernel::exposure::ExposureRegistry;
use serde_json::json;
use weftos_store::follower_cache_shared::CacheCfg;
use weftos_store::{FsBackend, StoreService};

use super::*;
use crate::store_rt::{ActivitySink, ChainAudit, RegistryGate};

const TTL: u64 = 5_000;
const DAY: u64 = 24 * 3_600_000;

fn open(dir: &std::path::Path) -> StoreService {
    let reg = Arc::new(ExposureRegistry::new("pg-1"));
    let be = Arc::new(FsBackend::open(dir).unwrap());
    let svc = StoreService::open(
        weftos_store::service::Config { node_id: "pg-1".into(), state_path: None, clock: clawft_kernel::exposure::registry::now_ms },
        be.clone(),
        be,
        Arc::new(RegistryGate(reg.clone())),
        Arc::new(ActivitySink(reg)),
        Arc::new(ChainAudit(None)),
    )
    .unwrap();
    // A long max_age and a TTL that real test time never reaches; tests move the cache clock.
    wtcache::set_cfg(&svc, Some(CacheCfg { ttl_ms: TTL, max_age_ms: 600_000, ..CacheCfg::default() }));
    svc
}

/// This daemon and another one on the same store.
fn two() -> (tempfile::TempDir, StoreService, StoreService) {
    let d = tempfile::tempdir().unwrap();
    let (a, b) = (open(d.path()), open(d.path()));
    (d, a, b)
}

fn publish(svc: &StoreService, slug: &str, dir: &std::path::Path, body: &str, handoff: bool) {
    std::fs::create_dir_all(dir.join(".git")).unwrap();
    std::fs::create_dir_all(dir.join("docs")).unwrap();
    std::fs::write(dir.join("docs/plan.md"), body).unwrap();
    let z = dir.join("paths.z");
    std::fs::write(&z, b"docs/plan.md\0").unwrap();
    let c = dir.join("chg.z");
    std::fs::write(&c, b"A\0docs/plan.md\0").unwrap();
    let (z, c) = (z.to_string_lossy().into_owned(), c.to_string_lossy().into_owned());
    call(svc, "create", json!({"project": PROJ, "slug": slug, "branch": slug, "base": "abc"}), 10).unwrap();
    let mut p = json!({"project": PROJ, "slug": slug, "root": dir.to_string_lossy(), "branch": slug, "base": "abc", "head": "h1",
                       "paths_z": z, "uncommitted_z": z, "changes_z": c, "agent": "alice", "host": "m1"});
    if handoff {
        p["handoff"] = json!("please merge");
    }
    call(svc, "publish", p, 20).unwrap();
}

/// The state `browse <slug>` (one worktree, served from the manifest cache) reports.
fn ls_state(svc: &StoreService, slug: &str) -> String {
    call(svc, "browse", json!({"project": PROJ, "slug": slug}), 1).unwrap()["state"].as_str().unwrap().to_owned()
}

/// The state the registry list (`browse` with no slug; a listing, never stale) reports.
fn list_state(svc: &StoreService, slug: &str) -> String {
    let l = call(svc, "browse", json!({"project": PROJ}), 1).unwrap();
    l["worktrees"].as_array().unwrap().iter().find(|w| w["slug"] == slug).unwrap()["state"].as_str().unwrap().to_owned()
}

fn leftover_why(svc: &StoreService, slug: &str, now: u64) -> Option<String> {
    let l = call(svc, "leftovers", json!({"project": PROJ}), now).unwrap();
    l["leftovers"].as_array().unwrap().iter().find(|x| x["worktree"]["slug"] == slug).map(|x| x["why"].as_str().unwrap().to_owned())
}

#[test]
fn another_daemons_claim_merge_and_orphan_mark_show_within_one_ttl() {
    let (_d, a, other) = two();
    let src = tempfile::tempdir().unwrap();
    publish(&a, "feat-x", src.path(), "# Plan\n", true);
    assert_eq!(ls_state(&a, "feat-x"), "handoff");
    // claim by the other daemon: the listing shows it at once, the cached slot within one TTL
    call(&other, "claim", json!({"project": PROJ, "slug": "feat-x", "agent": "bob", "host": "m2"}), 30).unwrap();
    assert_eq!(list_state(&a, "feat-x"), "claimed", "a registry listing is never stale");
    assert_eq!(ls_state(&a, "feat-x"), "claimed", "the listing just made dropped the moved slot");
    // with no listing in between: stale inside the TTL (the documented bound), current at it
    let s2 = tempfile::tempdir().unwrap();
    publish(&a, "feat-y", s2.path(), "# Plan Y\n", true);
    assert_eq!(ls_state(&a, "feat-y"), "handoff");
    call(&other, "claim", json!({"project": PROJ, "slug": "feat-y", "agent": "bob", "host": "m2"}), 31).unwrap();
    assert_eq!(ls_state(&a, "feat-y"), "handoff", "inside the TTL");
    wtcache::advance(&a, TTL);
    assert_eq!(ls_state(&a, "feat-y"), "claimed", "after one TTL: the entry etag moved");
    // merge by the other daemon
    call(&other, "promote", json!({"project": PROJ, "slug": "feat-y", "agent": "bob", "host": "m2"}), 40).unwrap();
    wtcache::advance(&a, TTL);
    assert_eq!(ls_state(&a, "feat-y"), "merged");
    // orphan mark by the other daemon (only an open worktree can be orphaned): leftovers (a listing)
    // shows it at once, browse within one TTL
    let s3 = tempfile::tempdir().unwrap();
    publish(&a, "feat-z", s3.path(), "# Plan Z\n", false);
    assert_eq!(ls_state(&a, "feat-z"), "open");
    let since = 5 * DAY;
    let m = call(&other, "orphan", json!({"project": PROJ, "slug": "feat-z", "since": since}), since + 2 * DAY).unwrap();
    assert_eq!(m["marked"], json!(true));
    assert_eq!(ls_state(&a, "feat-z"), "open", "inside the TTL");
    wtcache::advance(&a, TTL);
    assert_eq!(ls_state(&a, "feat-z"), "orphan", "after one TTL");
    assert_eq!(leftover_why(&a, "feat-z", since + 2 * DAY).as_deref(), Some("orphan"));
}

#[test]
fn this_daemons_own_writes_show_at_once() {
    let (_d, a, _other) = two();
    let get = |slug: &str| call(&a, "browse", json!({"project": PROJ, "slug": slug, "path": "docs/plan.md"}), 1).unwrap()["text"].clone();
    // republish inside the TTL: the new bytes at once
    let src = tempfile::tempdir().unwrap();
    publish(&a, "feat-x", src.path(), "# Plan one\n", false);
    assert_eq!(get("feat-x"), json!("# Plan one\n"));
    publish(&a, "feat-x", src.path(), "# Plan two\n", true);
    assert_eq!(get("feat-x"), json!("# Plan two\n"));
    assert_eq!(ls_state(&a, "feat-x"), "handoff", "the handoff publish shows at once");
    call(&a, "claim", json!({"project": PROJ, "slug": "feat-x", "agent": "bob", "host": "m2"}), 30).unwrap();
    assert_eq!(ls_state(&a, "feat-x"), "claimed", "no TTL wait after this daemon's claim");
    call(&a, "promote", json!({"project": PROJ, "slug": "feat-x", "agent": "bob", "host": "m2"}), 40).unwrap();
    assert_eq!(ls_state(&a, "feat-x"), "merged", "no TTL wait after this daemon's merge");
    let s2 = tempfile::tempdir().unwrap();
    publish(&a, "feat-o", s2.path(), "# O\n", false);
    assert_eq!(ls_state(&a, "feat-o"), "open");
    let since = 5 * DAY;
    call(&a, "orphan", json!({"project": PROJ, "slug": "feat-o", "since": since}), since + 2 * DAY).unwrap();
    assert_eq!(ls_state(&a, "feat-o"), "orphan", "no TTL wait after this daemon's orphan mark");
}

/// Lane D3 flipped this (owner decision 2026-11-15, accept up to one cache TTL of previous-version
/// bytes): objects are content-named and immutable, so inside the TTL the cached manifest's object
/// still exists and is served, consistent with the cached listing; past the TTL the new one.
#[test]
fn a_republish_by_another_daemon_inside_the_ttl_returns_the_previous_bytes_then_the_new_ones() {
    let (_d, a, other) = two();
    let src = tempfile::tempdir().unwrap();
    publish(&a, "feat-x", src.path(), "# Plan one\n", false);
    let get = |now| call(&a, "browse", json!({"project": PROJ, "slug": "feat-x", "path": "docs/plan.md"}), now).unwrap()["text"].clone();
    assert_eq!(get(1), json!("# Plan one\n"));
    publish(&other, "feat-x", src.path(), "# Plan two, longer\n", false);
    assert_eq!(get(1), json!("# Plan one\n"), "inside the TTL: the cached snapshot");
    wtcache::advance(&a, TTL);
    assert_eq!(get(2), json!("# Plan two, longer\n"), "past the TTL: revalidated, the new bytes");
}

#[test]
fn a_forgotten_and_recreated_slug_never_serves_the_old_slot() {
    let (_d, a, _other) = two();
    let src = tempfile::tempdir().unwrap();
    publish(&a, "feat-x", src.path(), "# Old life\n", false);
    assert_eq!(call(&a, "browse", json!({"project": PROJ, "slug": "feat-x", "path": "docs/plan.md"}), 1).unwrap()["text"], json!("# Old life\n"));
    let n = names("local", PROJ, "feat-x").unwrap();
    // what `store archive --delete` does after deleting the bucket
    forget(&a, &n.wt_name).unwrap();
    assert!(call(&a, "browse", json!({"project": PROJ, "slug": "feat-x"}), 1).is_err(), "gone at once, not served from the slot");
    let src2 = tempfile::tempdir().unwrap();
    publish(&a, "feat-x", src2.path(), "# New life\n", false);
    assert_eq!(call(&a, "browse", json!({"project": PROJ, "slug": "feat-x", "path": "docs/plan.md"}), 1).unwrap()["text"], json!("# New life\n"));
}

#[test]
fn cache_off_by_config_reads_the_store_every_time() {
    let (_d, a, other) = two();
    wtcache::set_cfg(&a, None);
    let src = tempfile::tempdir().unwrap();
    publish(&a, "feat-x", src.path(), "# Plan\n", true);
    assert_eq!(ls_state(&a, "feat-x"), "handoff");
    call(&other, "claim", json!({"project": PROJ, "slug": "feat-x", "agent": "bob", "host": "m2"}), 30).unwrap();
    assert_eq!(ls_state(&a, "feat-x"), "claimed", "no cache: no window at all");
    assert!(wtcache::cache(&a).is_none());
}

#[test]
fn concurrent_browse_calls_from_threads_do_not_deadlock() {
    let (_d, a, other) = two();
    let (s1, s2) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    publish(&a, "feat-a", s1.path(), "# A\n", false);
    publish(&a, "feat-b", s2.path(), "# B\n", false);
    std::thread::scope(|sc| {
        for t in 0..8 {
            let a = &a;
            sc.spawn(move || {
                for i in 0..15 {
                    let slug = if (t + i) % 2 == 0 { "feat-a" } else { "feat-b" };
                    call(a, "browse", json!({"project": PROJ}), 1).unwrap();
                    // a cold load that straddles a publish can fail (finding 3a, as without a cache);
                    // a returned text is always one whole published version
                    match call(a, "browse", json!({"project": PROJ, "slug": slug, "path": "docs/plan.md"}), 1) {
                        Ok(g) => assert!(g["text"].as_str().is_some_and(|t| t.starts_with("# ") && t.ends_with('\n')), "{g}"),
                        Err(e) => assert!(e.contains("checksum") || e.contains("missing") || e.contains("not in the shared tree"), "{e}"),
                    }
                    call(a, "search", json!({"project": PROJ, "query": "plan"}), 1).unwrap();
                    if i % 5 == 0 {
                        wtcache::advance(a, TTL); // revalidations race the reads
                    }
                }
            });
        }
        // writes from the other daemon while the reads run
        sc.spawn(|| {
            for i in 0..5 {
                publish(&other, "feat-a", s1.path(), &format!("# A {i}\n"), false);
            }
        });
    });
}
