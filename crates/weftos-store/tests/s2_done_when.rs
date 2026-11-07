//! ADR-126 S2 done-when checks, run on S1's real `FsBackend` (a shared directory stands in for the
//! shared RustFS) with two "machines" that share nothing but the store:
//!   1. two agents on two machines share a worktree (publish, follow, fetch, claim, merge);
//!   2. a new worktree sees the project's gitignored assets with no copy;
//!   3. landing promotes.
//!
//! The same scenario runs against a throwaway RustFS when WEFTOS_STORE_TEST_RUSTFS_ENV is set
//! (`cargo test -p weftos-store --features rustfs --test s2_done_when -- --ignored`).

use weftos_store::FsBackend;
use weftos_store::backend::{AdminBackend, ObjectBackend as S1};
use weftos_store::follower::{Change, FollowState, poll, search_docs};
use weftos_store::overlay::{ObjectBackend, Overlay};
use weftos_store::promote::{
    ArchiveTarget, PromotionPolicy, WorktreeMeta, is_promoted, promote, write_meta,
};
use weftos_store::share::*;
use weftos_store::share_fetch::*;

fn who(agent: &str, host: &str) -> Actor {
    Actor {
        agent: agent.into(),
        host: host.into(),
    }
}

/// One machine's checkout: a directory with a tracked source file and an untracked planning doc.
fn checkout(root: &std::path::Path) -> Vec<String> {
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("docs/planning")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
    std::fs::write(
        root.join("docs/planning/rooms.md"),
        "# Rooms\nthe room plan, not merged yet\n",
    )
    .unwrap();
    vec!["src/lib.rs".into(), "docs/planning/rooms.md".into()]
}

pub fn scenario<B>(be: &B, pbucket: &str, wbucket: &str, abucket: &str)
where
    B: S1 + AdminBackend,
{
    for b in [pbucket, wbucket, abucket] {
        AdminBackend::create_bucket(be, b, &[]).unwrap();
    }
    // The project's gitignored assets live in the project bucket only.
    S1::put(
        be,
        pbucket,
        "assets/models/hero.bin",
        b"HERO-MODEL-BYTES",
        false,
    )
    .unwrap();
    S1::put(be, pbucket, "shared/palette.json", b"{}", false).unwrap();
    write_meta(
        be,
        wbucket,
        &WorktreeMeta {
            slug: "feat-rooms".into(),
            project: "01abc".into(),
            branch: "feat/rooms".into(),
            base_commit: "base".into(),
            created_ms: 1,
        },
    )
    .unwrap();

    // (2) a new worktree sees the project's assets with no copy.
    let ov = Overlay::new(be, wbucket, pbucket);
    assert_eq!(
        ov.get("assets/models/hero.bin").unwrap().unwrap(),
        b"HERO-MODEL-BYTES"
    );
    let keys: Vec<_> = ov.list("").unwrap().into_iter().map(|m| m.key).collect();
    assert_eq!(keys, ["assets/models/hero.bin", "shared/palette.json"]);
    assert!(
        ov.local_objects().unwrap().is_empty()
            && ObjectBackend::list(be, wbucket, "assets/")
                .unwrap()
                .is_empty(),
        "nothing was copied into the worktree bucket"
    );

    // (1) machine A publishes and hands off.
    let a_dir = tempfile::tempdir().unwrap();
    let paths = checkout(a_dir.path());
    let a = who("agent-a", "machine-a");
    publish_tree(
        be,
        wbucket,
        &TreeSource {
            root: a_dir.path(),
            paths: &paths,
            uncommitted: &["docs/planning/rooms.md".to_string()],
            changes: &[],
            branch: "feat/rooms",
            base_commit: "base",
            head_commit: "head1",
            bundle: None,
            writer: a.clone(),
        },
        &Limits::default(),
        1_000,
    )
    .unwrap();
    mark_handoff(be, wbucket, &a, "please merge the rooms plan", 1_100).unwrap();
    let entry = summarize(be, "feat-rooms", wbucket).unwrap().unwrap();
    write_registry_entry(be, pbucket, &entry).unwrap(); // the daemon's job

    // machine B has none of A's files; it only has the store.
    let mut st = FollowState::default();
    let ch = poll(be, pbucket, &mut st).unwrap();
    assert!(matches!(&ch[..], [Change::New(e)] if e.slug == "feat-rooms"));
    let entries = list_shared(be, pbucket).unwrap();
    let hits = search_docs(be, &entries, "rooms");
    assert_eq!(hits.hits.len(), 1, "B finds A's unmerged planning doc");
    let b_dir = tempfile::tempdir().unwrap();
    let rep = fetch_tree(be, wbucket, b_dir.path(), Select::All, false).unwrap();
    assert!(rep.conflicts.is_empty() && rep.refused.is_empty() && rep.missing.is_empty());
    assert_eq!(
        std::fs::read_to_string(b_dir.path().join("docs/planning/rooms.md")).unwrap(),
        "# Rooms\nthe room plan, not merged yet\n"
    );

    // B claims; A can no longer hand off or republish over the claim; B finishes.
    let b = who("agent-b", "machine-b");
    claim_merge(be, wbucket, &b, 2_000).unwrap();
    assert!(
        publish_tree(
            be,
            wbucket,
            &TreeSource {
                root: a_dir.path(),
                paths: &paths,
                uncommitted: &[],
                changes: &[],
                branch: "feat/rooms",
                base_commit: "base",
                head_commit: "head2",
                bundle: None,
                writer: a.clone(),
            },
            &Limits::default(),
            2_100,
        )
        .is_err(),
        "A cannot overwrite B's claimed merge"
    );
    mark_merged(be, wbucket, &b, "landed on master", 3_000).unwrap();
    let ch = poll(be, pbucket, &mut st).unwrap();
    let _ = ch; // registry refresh is the daemon's; the handoff state is what B just wrote
    assert_eq!(
        read_handoff(be, wbucket).unwrap().unwrap().state,
        State::Merged
    );

    // (3) landing promotes: the asset made in the worktree reaches the project, once.
    Overlay::new(be, wbucket, pbucket)
        .put("assets/models/room.bin", b"ROOM-MODEL")
        .unwrap();
    let arch = ArchiveTarget {
        bucket: abucket,
        prefix: "archive/worktrees/feat-rooms/",
    };
    let r = promote(
        be,
        wbucket,
        pbucket,
        &arch,
        &PromotionPolicy::default(),
        4_000,
    )
    .unwrap();
    assert_eq!(r.promoted, ["assets/models/room.bin"]);
    assert_eq!(
        S1::get(be, pbucket, "assets/models/room.bin").unwrap(),
        b"ROOM-MODEL"
    );
    assert!(is_promoted(be, wbucket).unwrap().is_some());
    let again = promote(
        be,
        wbucket,
        pbucket,
        &arch,
        &PromotionPolicy::default(),
        5_000,
    )
    .unwrap();
    assert_eq!(again.promoted, r.promoted, "second land is a no-op");
    // the unmerged planning doc was archived, not lost
    assert!(
        S1::list(be, abucket, "archive/worktrees/feat-rooms/")
            .unwrap()
            .iter()
            .any(|m| m.key.contains(".wt/tree/docs/planning/rooms.md"))
    );
}

#[test]
fn two_machines_share_a_worktree_assets_overlay_and_landing_promotes_on_fs() {
    let d = tempfile::tempdir().unwrap();
    let be = FsBackend::open(d.path()).unwrap();
    scenario(&be, "p-01abc", "w-01abc-aaaaaaaaaaaa", "p-01abc-arc");
}

/// Needs a THROWAWAY RustFS (never PG): WEFTOS_STORE_TEST_RUSTFS_ENV names its env file.
#[cfg(feature = "rustfs")]
#[test]
#[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
fn two_machines_share_a_worktree_on_a_real_rustfs() {
    use weftos_store::{RustfsBackend, RustfsConfig};
    let f = std::env::var("WEFTOS_STORE_TEST_RUSTFS_ENV").expect("WEFTOS_STORE_TEST_RUSTFS_ENV");
    let d = tempfile::tempdir().unwrap();
    let cfg =
        RustfsConfig::from_env_file(std::path::Path::new(&f), d.path().join("secrets")).unwrap();
    let be = RustfsBackend::new(cfg).unwrap();
    let u = ulid::Ulid::new().to_string().to_lowercase();
    let u = &u[10..];
    scenario(
        &be,
        &format!("t-p-{u}"),
        &format!("t-w-{u}"),
        &format!("t-a-{u}"),
    );
}
