//! WO-069 lane E: orphan marking and gc against a REAL RustFS. Needs a THROWAWAY server (never PG):
//! `WEFTOS_STORE_TEST_RUSTFS_ENV` names an env file with MESH_S3_ENDPOINT / MESH_S3_ACCESS_KEY /
//! MESH_S3_SECRET_KEY. Run via `scripts/store/wo069-live-test.sh`, or
//! `cargo test -p weftos-store --features rustfs --test s2_orphan_gc_live -- --ignored --nocapture`.
//!
//! Two worktree stores and a project bucket with its own files. One worktree is orphaned and retired
//! under a test clock; the other worktree store and every other key of the project bucket must be
//! identical before and after (listing and etags).
#![cfg(feature = "rustfs")]

use std::time::Instant;
use weftos_store::backend::AdminBackend;
use weftos_store::orphan::*;
use weftos_store::overlay::{ObjectBackend, S2Error};
use weftos_store::promote::{ArchiveTarget, WorktreeMeta, write_meta};
use weftos_store::share::*;
use weftos_store::share_fetch::*;
use weftos_store::{RustfsBackend, RustfsConfig};

const H: u64 = 3_600_000;
const D: u64 = 24 * H;

fn timed<T>(label: &str, f: impl FnOnce() -> T) -> T {
    let t = Instant::now();
    let v = f();
    println!("TIMING {label}: {} ms", t.elapsed().as_millis());
    v
}

fn publish(be: &RustfsBackend, wb: &str, pb: &str, slug: &str, at: u64) {
    let d = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(d.path().join("docs")).unwrap();
    std::fs::write(d.path().join("docs/plan.md"), format!("# Plan {slug}\nunmerged\n")).unwrap();
    let paths = vec!["docs/plan.md".to_string()];
    let src = TreeSource {
        root: d.path(), paths: &paths, uncommitted: &paths, changes: &[], branch: slug, base_commit: "base",
        head_commit: "head-1", bundle: None, writer: Actor { agent: "agent-a".into(), host: "machine-a".into() },
    };
    let meta = WorktreeMeta { slug: slug.into(), project: "01abc".into(), branch: slug.into(), base_commit: "base".into(), created_ms: 1 };
    write_meta(be, wb, &meta).unwrap();
    be.put(wb, "scratch/overlay.txt", b"work that exists only in this worktree").unwrap();
    publish_tree(be, wb, &src, &Limits::default(), at).unwrap();
    let e = summarize(be, slug, wb).unwrap().unwrap();
    write_registry_entry(be, pb, &e).unwrap();
}

fn snap(be: &RustfsBackend, bucket: &str, skip: &[&str]) -> Vec<(String, String)> {
    let mut v: Vec<_> = ObjectBackend::list(be, bucket, "").unwrap().into_iter().filter(|m| !skip.contains(&m.key.as_str())).map(|m| (m.key, m.etag)).collect();
    v.sort();
    v
}

#[test]
#[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
fn orphan_gc_on_a_real_rustfs_touches_only_the_orphans_own_store() {
    let f = std::env::var("WEFTOS_STORE_TEST_RUSTFS_ENV").expect("WEFTOS_STORE_TEST_RUSTFS_ENV");
    let d = tempfile::tempdir().unwrap();
    let cfg = RustfsConfig::from_env_file(std::path::Path::new(&f), d.path().join("secrets")).unwrap();
    let be = RustfsBackend::new(cfg).unwrap();
    let u = ulid::Ulid::new().to_string().to_lowercase();
    let u = &u[10..];
    let (pb, wa, wo, ar) = (format!("t-p-{u}"), format!("t-w-a-{u}"), format!("t-w-o-{u}"), format!("t-arch-{u}"));
    for b in [&pb, &wa, &wo, &ar] {
        AdminBackend::create_bucket(&be, b, &[]).unwrap();
    }
    be.put(&pb, "docs/readme.md", b"project readme").unwrap();
    be.put(&pb, "assets/hero.bin", &[7u8; 2048]).unwrap();
    timed("publish two worktrees", || {
        publish(&be, &wa, &pb, "gone", 1_000);
        publish(&be, &wo, &pb, "other", 1_000);
    });
    // The registry entries name these test buckets: the real-name cross-check does not apply to made-up names.
    let pol = OrphanPolicy::default();
    let since = 100 * D;
    let skip_pb = ["worktrees/gone.json", DIGEST_KEY];

    // grace: 23 h is not enough, 24 h is; the other worktree is never a candidate
    assert!(matches!(mark_orphan(&be, &pb, "gone", since + 23 * H, since, &pol), Err(OrphanError::Refused(Refusal::TooSoon { .. }))));
    let t0 = since + 24 * H;
    let m = timed("mark_orphan", || mark_orphan(&be, &pb, "gone", t0, since, &pol).unwrap());
    assert_eq!(m.state, State::Orphan);
    assert_eq!(list_shared(&be, &pb).unwrap().iter().find(|e| e.slug == "other").unwrap().state, State::Open);

    // before: the unrelated worktree store, the project's other keys, the orphan's own keys
    let other_before = snap(&be, &wo, &[]);
    let proj_before = snap(&be, &pb, &skip_pb);
    let own_before = snap(&be, &wa, &[]);
    println!("BEFORE other({wo}) {} keys; project({pb}) {} keys excluding the entry and digest; orphan({wa}) {} keys", other_before.len(), proj_before.len(), own_before.len());
    println!("BEFORE project keys: {:?}", proj_before.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>());

    // the dry run lists it and changes nothing
    let late = t0 + 14 * D;
    let plan = timed("plan_orphans (dry run)", || plan_orphans(&be, &pb, late, &pol).unwrap());
    assert_eq!(plan.len(), 1);
    assert_eq!((plan[0].slug.as_str(), plan[0].due, plan[0].objects as usize), ("gone", true, own_before.len()));
    assert_eq!(snap(&be, &wa, &[]), own_before, "dry run changed nothing");
    assert_eq!(snap(&be, &pb, &[]).len(), proj_before.len() + 2);
    // one day short of due: confirmed keeps it
    let r = retire_orphan(&be, &pb, "gone", late - 1, &pol, &ArchiveTarget { bucket: &ar, prefix: "x@1/" }, &|_| Ok(()));
    assert!(matches!(r, Err(OrphanError::Refused(Refusal::NotDue { .. }))));
    assert_eq!(snap(&be, &wa, &[]), own_before);

    // confirmed: archive, verify, delete, drop the bucket, remove the entry
    let prefix = format!("{wa}@{late}/");
    let done = timed("retire_orphan (archive, delete, drop bucket, remove entry)", || {
        retire_orphan(&be, &pb, "gone", late, &pol, &ArchiveTarget { bucket: &ar, prefix: &prefix }, &|bk| {
            AdminBackend::delete_bucket(&be, bk).map_err(|e| S2Error::Backend(e.to_string()))
        })
        .unwrap()
    });
    assert_eq!(done.archived as usize, own_before.len());

    // after
    assert!(!AdminBackend::bucket_exists(&be, &wa).unwrap(), "the orphan's bucket is gone");
    let arch = snap(&be, &ar, &[]);
    assert_eq!(arch.len(), own_before.len(), "every key archived once");
    for (k, _) in &own_before {
        assert!(arch.iter().any(|(ak, _)| *ak == format!("{prefix}{k}")), "archived {k}");
    }
    assert_eq!(snap(&be, &wo, &[]), other_before, "the other worktree store is byte-identical (list + etags)");
    assert_eq!(snap(&be, &pb, &skip_pb), proj_before, "every other project key is identical (list + etags)");
    assert!(list_shared(&be, &pb).unwrap().iter().all(|e| e.slug != "gone"), "registry entry removed");
    assert!(read_digest(&be, &pb).unwrap().is_some_and(|dg| !dg.entries.contains_key("gone") && dg.entries.contains_key("other")));
    println!("AFTER other({wo}) {} keys identical; project {} keys identical; orphan bucket gone; archive {} keys", other_before.len(), proj_before.len(), arch.len());

    // teardown: every object and bucket this test made
    for b in [&wo, &pb, &ar] {
        for m in ObjectBackend::list(&be, b, "").unwrap() {
            ObjectBackend::delete(&be, b, &m.key).unwrap();
        }
        AdminBackend::delete_bucket(&be, b).unwrap();
    }
}
