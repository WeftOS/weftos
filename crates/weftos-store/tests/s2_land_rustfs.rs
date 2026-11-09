//! Landing a worktree on a real RustFS: preview, conflict policies, size cap, displaced archive and
//! retention listing (ADR-126 S2). Throwaway RustFS only: `WEFTOS_STORE_TEST_RUSTFS_ENV`, then
//! `cargo test -p weftos-store --features rustfs --test s2_land_rustfs -- --ignored`.
#![cfg(feature = "rustfs")]

use std::path::Path;
use weftos_store::backend::{AdminBackend, ObjectBackend as S1};
use weftos_store::overlay::{ObjectBackend as S2, Overlay, S2Error};
use weftos_store::promote::*;
use weftos_store::retention::*;
use weftos_store::{RustfsBackend, RustfsConfig};

fn live() -> (RustfsConfig, tempfile::TempDir) {
    let f = std::env::var("WEFTOS_STORE_TEST_RUSTFS_ENV").expect("WEFTOS_STORE_TEST_RUSTFS_ENV");
    let d = tempfile::tempdir().unwrap();
    (RustfsConfig::from_env_file(Path::new(&f), d.path().join("secrets")).unwrap(), d)
}

fn drain(be: &RustfsBackend, bucket: &str) {
    for m in S1::list(be, bucket, "").unwrap() {
        S1::delete(be, bucket, &m.key).unwrap();
    }
    be.delete_bucket(bucket).unwrap();
}

#[test]
#[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
fn live_land_preview_conflicts_size_cap_archive_and_retention() {
    let (cfg, _d) = live();
    let be = RustfsBackend::new(cfg).unwrap();
    let u = ulid::Ulid::new().to_string().to_lowercase();
    let (pr, wt, ar) = (format!("p-{u}"), format!("w-{u}-aaaaaaaaaaaa"), format!("a-{u}"));
    for b in [&pr, &wt, &ar] {
        be.create_bucket(b, &[]).unwrap();
    }
    S2::put(&be, &pr, "assets/logo.png", b"v1").unwrap();
    write_meta(&be, &wt, &WorktreeMeta { slug: "x".into(), project: u.clone(), branch: "b".into(), base_commit: "c".into(), created_ms: 1 }).unwrap();
    let ov = Overlay::new(&be, &wt, &pr);
    ov.put("assets/logo.png", b"mine").unwrap();
    ov.put("assets/new.bin", &[5u8; 700]).unwrap();
    ov.put("agents/i1/out.log", b"scratch").unwrap();
    // a teammate lands a different logo first (a real RustFS etag, not an in-memory one)
    S2::put(&be, &pr, "assets/logo.png", b"teammate").unwrap();
    let arch = ArchiveTarget { bucket: &ar, prefix: &format!("w-{u}-aaaaaaaaaaaa@1000/") };

    let pv = preview(&be, &wt, &pr, &PromotionPolicy::default()).unwrap();
    assert_eq!(pv.conflicts, [("assets/logo.png".to_string(), ConflictKind::BothEdited)]);
    assert_eq!(pv.would_promote, ["assets/new.bin"]);
    assert_eq!(pv.would_archive, ["agents/i1/out.log"]);
    assert_eq!(pv.promote_bytes, 700);
    assert!(is_promoted(&be, &wt).unwrap().is_none());

    // fail: nothing written; size cap: refused before any write
    assert!(matches!(promote(&be, &wt, &pr, &arch, &PromotionPolicy::default(), 1_000), Err(S2Error::Conflict(k)) if k == ["assets/logo.png"]));
    let keep = PromotionPolicy { on_conflict: OnConflict::KeepProject, ..Default::default() };
    let tiny = PromotionPolicy { max_promote_bytes: Some(100), ..keep.clone() };
    assert!(matches!(promote(&be, &wt, &pr, &arch, &tiny, 1_000), Err(S2Error::Invalid(m)) if m.contains("byte cap")));
    assert!(S2::get(&be, &pr, "assets/new.bin").unwrap().is_none(), "refused before copying anything");

    // keep_project: the teammate's logo stays, ours is archived, the rest lands (conditional writes on RustFS)
    let r = promote(&be, &wt, &pr, &arch, &keep, 1_000).unwrap();
    assert_eq!((r.promoted.as_slice(), r.kept_project.as_slice()), (&["assets/new.bin".to_string()][..], &["assets/logo.png".to_string()][..]));
    assert_eq!(S2::get(&be, &pr, "assets/logo.png").unwrap().unwrap(), b"teammate");
    assert_eq!(S2::get(&be, &pr, "assets/new.bin").unwrap().unwrap().len(), 700);

    // retention on the real listing: '@' and '/' in archive keys survive SigV4, the timestamp parses
    let keys: Vec<_> = S1::list(&be, &ar, "").unwrap().into_iter().map(|m| m.key).collect();
    assert!(keys.iter().any(|k| k == &format!("w-{u}-aaaaaaaaaaaa@1000/agents/i1/out.log")), "{keys:?}");
    assert_eq!(landed_before(&be, &wt, 999).unwrap(), None);
    assert_eq!(landed_before(&be, &wt, 1_000).unwrap(), Some(1_000));
    let (n, bytes) = usage(&be, &wt).unwrap();
    assert!(n >= 4 && bytes >= 700 + 4);
    let gone = expired_archive_keys(&be, &ar, &format!("w-{u}-"), 2_000).unwrap();
    assert_eq!(gone.len(), keys.len(), "everything archived at ts 1000 is past a 2000 cutoff");
    assert!(expired_archive_keys(&be, &ar, &format!("w-{u}-"), 999).unwrap().is_empty());
    for k in gone {
        S1::delete(&be, &ar, &k).unwrap();
    }
    assert!(S1::list(&be, &ar, "").unwrap().is_empty());

    for b in [&pr, &wt, &ar] {
        drain(&be, b);
    }
}

// ---- read-modify-write with S1's put_if_match, against the real conditional write -----------------

fn actor(n: &str) -> weftos_store::share::Actor {
    weftos_store::share::Actor { agent: n.into(), host: "h".into() }
}

fn published(be: &RustfsBackend, wt: &str) {
    use weftos_store::share::*;
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("a.md"), "x").unwrap();
    let ps = vec!["a.md".to_string()];
    let src = TreeSource {
        root: d.path(), paths: &ps, uncommitted: &[], changes: &[], branch: "b", base_commit: "b",
        head_commit: "h", bundle: None, writer: actor("writer"),
    };
    publish_tree(be, wt, &src, &Limits::default(), 1_000).unwrap();
}

#[test]
#[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
fn live_concurrent_claims_have_exactly_one_winner_and_a_handoff_cannot_erase_a_claim() {
    use weftos_store::share::*;
    let (cfg, _d) = live();
    let be = RustfsBackend::new(cfg).unwrap();
    let u = ulid::Ulid::new().to_string().to_lowercase();
    let mut buckets = Vec::new();
    for round in 0..8 {
        let wt = format!("w-{u}-{round:012}");
        be.create_bucket(&wt, &[]).unwrap();
        buckets.push(wt.clone());
        published(&be, &wt);
        mark_handoff(&be, &wt, &actor("writer"), "ready", 1_001).unwrap();

        // 16 claimers released together on one real store
        let gate = std::sync::Barrier::new(16);
        let results: Vec<_> = std::thread::scope(|s| {
            let hs: Vec<_> = (0..16)
                .map(|i| {
                    let (be, wt, gate) = (&be, &wt, &gate);
                    s.spawn(move || {
                        gate.wait();
                        claim_merge(be, wt, &actor(&format!("agent{i}")), 2_000)
                    })
                })
                .collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let winners: Vec<_> = results.iter().filter_map(|r| r.as_ref().ok()).collect();
        assert_eq!(winners.len(), 1, "round {round}: exactly one claimer wins, got {}: {results:?}", winners.len());
        for r in results.iter().filter(|r| r.is_err()) {
            assert!(matches!(r, Err(S2Error::Claimed(_))), "losers are told someone holds it: {r:?}");
        }
        let h = read_handoff(&be, &wt).unwrap().unwrap();
        assert_eq!((h.state, h.epoch), (State::Claimed, 1), "one epoch bump, not sixteen");
        assert_eq!(h.merger.as_ref(), winners[0].merger.as_ref());

        // the writer's late re-handoff cannot erase the claim
        assert!(matches!(mark_handoff(&be, &wt, &actor("writer"), "again", 2_001), Err(S2Error::Claimed(_))));
        // a loser's merge is refused; the winner's lands
        let winner = h.merger.clone().unwrap();
        let loser = results.iter().position(|r| r.is_err()).map(|i| actor(&format!("agent{i}"))).unwrap();
        assert!(mark_merged_at(&be, &wt, &loser, Some(1), "no", 2_002).is_err());
        mark_merged_at(&be, &wt, &winner, Some(1), "done", 2_003).unwrap();
        assert_eq!(read_handoff(&be, &wt).unwrap().unwrap().state, State::Merged);
    }
    for b in &buckets {
        drain(&be, b);
    }
}
