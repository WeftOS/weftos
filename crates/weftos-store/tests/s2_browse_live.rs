//! WO-069 lane C: the browse reader against a REAL RustFS. Needs a THROWAWAY server (never PG):
//! `WEFTOS_STORE_TEST_RUSTFS_ENV` names an env file with MESH_S3_ENDPOINT / MESH_S3_ACCESS_KEY /
//! MESH_S3_SECRET_KEY. Run via `scripts/store/wo069-live-test.sh`, or
//! `cargo test -p weftos-store --features rustfs --test s2_browse_live -- --ignored --nocapture`.
//!
//! Publish a small tree (two docs, one secret-looking file, one over-cap file), write the registry
//! entry, then list, read, refuse, re-publish with a changed doc, and tear everything down.
#![cfg(feature = "rustfs")]

use std::time::Instant;
use weftos_store::backend::AdminBackend;
use weftos_store::follower::*;
use weftos_store::follower_browse::{EntryKind, MAX_GET_BYTES};
use weftos_store::overlay::ObjectBackend;
use weftos_store::promote::{WorktreeMeta, write_meta};
use weftos_store::share::*;
use weftos_store::share_fetch::*;
use weftos_store::{RustfsBackend, RustfsConfig};

fn timed<T>(label: &str, f: impl FnOnce() -> T) -> T {
    let t = Instant::now();
    let v = f();
    println!("TIMING {label}: {} ms", t.elapsed().as_millis());
    v
}

fn publish(be: &RustfsBackend, wb: &str, pb: &str, files: &[(&str, Vec<u8>)], head: &str, at: u64) {
    let d = tempfile::tempdir().unwrap();
    let mut paths = vec![];
    for (p, c) in files {
        let f = d.path().join(p);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, c).unwrap();
        paths.push(p.to_string());
    }
    let unc = vec!["docs/planning/rooms.md".to_string()];
    let src = TreeSource {
        root: d.path(),
        paths: &paths,
        uncommitted: &unc,
        changes: &[],
        branch: "feat/rooms",
        base_commit: "base",
        head_commit: head,
        bundle: None,
        writer: Actor { agent: "agent-a".into(), host: "machine-a".into() },
    };
    let rep = timed(&format!("publish_tree {head}"), || {
        publish_tree(be, wb, &src, &Limits::default(), at).unwrap()
    });
    assert!(
        rep.skipped.iter().any(|(p, s)| p == "config/secret.env" && matches!(s, Skip::Secret)),
        "the secret-looking file is skipped at publish: {:?}",
        rep.skipped
    );
    let e = summarize(be, "rooms", wb).unwrap().unwrap();
    write_registry_entry(be, pb, &e).unwrap();
}

#[test]
#[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
fn browse_on_a_real_rustfs() {
    let f = std::env::var("WEFTOS_STORE_TEST_RUSTFS_ENV").expect("WEFTOS_STORE_TEST_RUSTFS_ENV");
    let d = tempfile::tempdir().unwrap();
    let cfg = RustfsConfig::from_env_file(std::path::Path::new(&f), d.path().join("secrets")).unwrap();
    let be = RustfsBackend::new(cfg).unwrap();
    let u = ulid::Ulid::new().to_string().to_lowercase();
    let u = &u[10..];
    let (pb, wb) = (format!("t-p-{u}"), format!("t-w-{u}"));
    for b in [&pb, &wb] {
        AdminBackend::create_bucket(&be, b, &[]).unwrap();
    }
    let meta = WorktreeMeta {
        slug: "rooms".into(), project: "01abc".into(), branch: "feat/rooms".into(),
        base_commit: "base".into(), created_ms: 1,
    };
    write_meta(&be, &wb, &meta).unwrap();

    let rooms = b"# Rooms plan\nthe room plan, not merged yet\n".to_vec();
    let big = vec![b'z'; MAX_GET_BYTES as usize + 1];
    let files = |rooms: &[u8]| -> Vec<(&'static str, Vec<u8>)> {
        vec![
            ("docs/planning/rooms.md", rooms.to_vec()),
            ("docs/readme.md", b"# Readme\n".to_vec()),
            ("config/secret.env", b"TOKEN=abc".to_vec()),
            ("big.bin", big.clone()),
        ]
    };
    publish(&be, &wb, &pb, &files(&rooms), "head1", 1_000);

    // registry
    let reg = timed("browse_registry", || browse_registry(&be, &pb).unwrap());
    assert!(reg.unreadable.is_empty());
    let w = reg.worktrees.iter().find(|w| w.slug == "rooms").expect("registered");
    assert_eq!((w.branch.as_str(), w.head.as_str(), w.doc_count), ("feat/rooms", "head1", 2));

    // listing: docs with titles, the secret file absent
    let root = timed("browse_ls root", || browse_ls(&be, &pb, "rooms", "").unwrap());
    let names: Vec<_> = root.entries.iter().map(|e| (e.name.as_str(), e.kind)).collect();
    assert_eq!(names, [("docs", EntryKind::Dir), ("big.bin", EntryKind::File)], "no config/secret.env");
    let pl = timed("browse_ls docs/planning", || browse_ls(&be, &pb, "rooms", "docs/planning").unwrap());
    assert_eq!(pl.entries[0].title.as_deref(), Some("Rooms plan"));
    assert!(pl.entries[0].uncommitted);

    // get: right hash, then the over-cap file withheld
    let got = timed("browse_get doc", || browse_get(&be, &pb, "rooms", "docs/planning/rooms.md", 1 << 20).unwrap());
    assert_eq!(got.data.as_deref(), Some(&rooms[..]));
    assert_eq!(got.sha256, sha256_hex(&rooms));
    let wh = timed("browse_get over-cap", || browse_get(&be, &pb, "rooms", "big.bin", u64::MAX).unwrap());
    assert!(wh.withheld && wh.data.is_none() && wh.cap == MAX_GET_BYTES);

    // refusals: secret path, wrong slug, outside the tree prefix
    for bad in ["config/secret.env", ".wt/meta.json", "../x", ".wt/tree.manifest.json"] {
        assert!(browse_get(&be, &pb, "rooms", bad, 99).is_err(), "get {bad}");
    }
    assert!(browse_ls(&be, &pb, "ghost", "").is_err(), "wrong slug");
    assert!(browse_get(&be, &pb, "ghost", "big.bin", 9).is_err(), "wrong slug");
    assert!(browse_ls(&be, &pb, "rooms", ".wt").is_err(), "outside the tree prefix");

    // re-publish with one doc changed: the listing moves
    let rooms2 = b"# Rooms plan v2\nchanged\n".to_vec();
    publish(&be, &wb, &pb, &files(&rooms2), "head2", 2_000);
    let reg = browse_registry(&be, &pb).unwrap();
    assert_eq!(reg.worktrees.iter().find(|w| w.slug == "rooms").unwrap().head, "head2");
    let pl = browse_ls(&be, &pb, "rooms", "docs/planning").unwrap();
    assert_eq!(pl.entries[0].title.as_deref(), Some("Rooms plan v2"));
    assert_eq!(pl.entries[0].sha256.as_deref(), Some(sha256_hex(&rooms2).as_str()));
    let got = browse_get(&be, &pb, "rooms", "docs/planning/rooms.md", 1 << 20).unwrap();
    assert_eq!(got.data.as_deref(), Some(&rooms2[..]));

    // teardown: delete the objects, the registry entry goes, browse finds nothing
    for b in [&wb, &pb] {
        for m in ObjectBackend::list(&be, b, "").unwrap() {
            ObjectBackend::delete(&be, b, &m.key).unwrap();
        }
    }
    assert!(browse_registry(&be, &pb).unwrap().worktrees.is_empty());
    assert!(browse_ls(&be, &pb, "rooms", "").is_err(), "deleted worktree is gone");
}
