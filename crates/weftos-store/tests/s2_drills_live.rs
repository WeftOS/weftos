//! WO-069 lane H: the ADR-126 section 4a gate drills that run in the library against a REAL RustFS (1a, 2l, helper). The failure
//! drills (3a, 3b, 3c) are in `s2_drills_fail.rs`. Needs a THROWAWAY server (never PG): `WEFTOS_STORE_TEST_RUSTFS_ENV` names an
//! env file with MESH_S3_ENDPOINT / MESH_S3_ACCESS_KEY / MESH_S3_SECRET_KEY. Run via `scripts/store/wo069-drills.sh`, or
//! `cargo test -p weftos-store --features rustfs --test s2_drills_live -- --ignored --nocapture --test-threads=1`.
//! Each drill prints `DRILL|<id>|<step>|PASS|<ms> ms|<detail>` lines; a defect prints `FINDING|...`.
//!
//! - 1a: a writer key and a read-only follower key (two real S3 identities): poll, search, sync-docs, browse, fetch tree + bundle, every write refused, publish-to-poll latency.
//! - 2l: `leftovers` and the orphan state under an injected clock (the daemon verb has no clock override).
#![cfg(feature = "rustfs")]

#[path = "s2_drills_common/mod.rs"]
mod s2_drills_common;
use s2_drills_common::*;
use std::time::Instant;
use weftos_store::backend::AdminBackend;
use weftos_store::follower::{Change, FollowState, LeftoverPolicy, Why, leftovers, poll_fast, search_docs, sync_docs};
use weftos_store::follower_browse::{browse_get, browse_ls, browse_registry};
use weftos_store::name::StoreName;
use weftos_store::orphan::{OrphanPolicy, mark_orphan};
use weftos_store::overlay::ObjectBackend;
use weftos_store::promote::{WorktreeMeta, write_meta};
use weftos_store::share::*;
use weftos_store::share_fetch::*;
use weftos_store::{RustfsBackend, RustfsConfig};

#[test]
#[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
fn drill_2l_leftovers_and_orphan_under_an_injected_clock() {
    let (be, _d) = backend();
    let u = uniq();
    let pb = format!("t-p-{u}");
    make_bucket(&be, &pb);
    let w = who("writer", "m1");
    let base = 1_000 * D;
    let t = Tree::new(&[("docs/plan.md", "# Plan\nunmerged\n".into())]);
    let mut buckets = vec![pb.clone()];
    for (slug, handoff, claim) in [("open-old", false, false), ("handed-off", true, false), ("claimed", true, true), ("orphan-me", false, false)] {
        let wb = format!("t-w-{slug}-{u}");
        make_bucket(&be, &wb);
        write_meta(&be, &wb, &WorktreeMeta { slug: slug.into(), project: "01abc".into(), branch: slug.into(), base_commit: "base".into(), created_ms: 1 }).unwrap();
        t.publish(&be, &wb, "h1", &w, base).unwrap();
        if handoff {
            mark_handoff(&be, &wb, &w, "ready", base).unwrap();
        }
        if claim {
            claim_merge(&be, &wb, &who("merger", "m2"), base).unwrap();
        }
        write_registry_entry(&be, &pb, &summarize(&be, slug, &wb).unwrap().unwrap()).unwrap();
        buckets.push(wb);
    }
    let pol = LeftoverPolicy::default(); // open_idle 3 d, unclaimed 24 h, claim TTL 30 min
    let ents = || list_shared(&be, &pb).unwrap();
    let t0 = Instant::now();
    let at = |ms: u64| leftovers(&ents(), base + ms, &pol).into_iter().map(|l| (l.entry.slug, l.why)).collect::<Vec<_>>();
    assert!(at(29 * MIN).is_empty(), "nothing is a leftover at 29 min");
    let v = at(31 * MIN);
    assert_eq!(v, vec![("claimed".to_string(), Why::AbandonedClaim)]);
    let v = at(25 * H);
    assert!(v.contains(&("handed-off".to_string(), Why::UnclaimedHandoff)) && !v.iter().any(|(s, _)| s == "open-old"), "{v:?}");
    let v = at(3 * D + MIN);
    assert!(v.contains(&("open-old".to_string(), Why::ForgottenOpen)) && v.contains(&("orphan-me".to_string(), Why::ForgottenOpen)), "{v:?}");
    line("2l", "leftovers thresholds (claim 30 min, handoff 24 h, open 3 d) against real registry entries", t0.elapsed().as_millis(), format!("{v:?}"));

    // orphan marking: grace 24 h, then the entry is an orphan and `leftovers` no longer lists it
    let t1 = Instant::now();
    let op = OrphanPolicy::default();
    let r = mark_orphan(&be, &pb, "orphan-me", base + 23 * H, base, &op);
    assert!(r.is_err(), "23 h is inside the 24 h grace");
    let e = mark_orphan(&be, &pb, "orphan-me", base + 24 * H, base, &op).unwrap();
    assert_eq!(e.state, State::Orphan);
    let v = at(10 * D);
    if v.iter().any(|(s, _)| s == "orphan-me") {
        println!("DETAIL|2l|orphan stays in leftovers: {v:?}");
    } else {
        finding("2l", "an entry in the orphan state is not reported by `leftovers` at any age (library `leftovers` has no arm for State::Orphan); it is reported only by `gc`'s plan, so a forgotten orphan is invisible to a follower running `leftovers`. Not blocking: gc lists it. Proposed: report State::Orphan from `leftovers` as Why::Orphan with its orphan age");
    }
    line("2l", "orphan: 23 h refused, 24 h marked", t1.elapsed().as_millis(), "state orphan");
    let bs: Vec<&str> = buckets.iter().map(|s| s.as_str()).collect();
    wipe(&be, &bs);
}

fn as_key(cfg: &RustfsConfig, root: &RustfsBackend, key_id: &str) -> RustfsBackend {
    let secret = std::fs::read_to_string(root.secret_path(key_id)).unwrap();
    RustfsBackend::new(RustfsConfig { root_access_key: key_id.into(), root_secret_key: secret, ..cfg.clone() }).unwrap()
}

fn policy(rw: &[&str], ro: &[&str]) -> serde_json::Value {
    let arn = |b: &&str| [format!("arn:aws:s3:::{b}"), format!("arn:aws:s3:::{b}/*")];
    let mut st = vec![];
    if !rw.is_empty() {
        st.push(serde_json::json!({"Effect": "Allow", "Action": ["s3:GetObject", "s3:PutObject", "s3:DeleteObject", "s3:ListBucket"], "Resource": rw.iter().flat_map(arn).collect::<Vec<_>>()}));
    }
    if !ro.is_empty() {
        st.push(serde_json::json!({"Effect": "Allow", "Action": ["s3:GetObject", "s3:ListBucket"], "Resource": ro.iter().flat_map(arn).collect::<Vec<_>>()}));
    }
    serde_json::json!({"Version": "2012-10-17", "Statement": st})
}

#[test]
#[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
fn drill_1a_two_principals_read_only_follower() {
    let f = std::env::var("WEFTOS_STORE_TEST_RUSTFS_ENV").expect("WEFTOS_STORE_TEST_RUSTFS_ENV");
    let d = tempfile::tempdir().unwrap();
    let cfg = RustfsConfig::from_env_file(std::path::Path::new(&f), d.path().join("secrets")).unwrap();
    let root = RustfsBackend::new(cfg.clone()).unwrap();
    let u = ulid::Ulid::new().to_string().to_lowercase();
    let (pb, wa) = (StoreName::project("lab", &u).unwrap().bucket(), StoreName::worktree("lab", &u, "feat-docs").unwrap().bucket());
    for b in [&pb, &wa] {
        make_bucket(&root, b);
    }
    let (kw, kf) = (root.create_access_key(&format!("h-writer:{u}")).unwrap(), root.create_access_key(&format!("h-follower:{u}")).unwrap());
    root.put_policy(&kw.id, &policy(&[&wa], &[])).unwrap();
    root.put_policy(&kf.id, &policy(&[], &[&pb, &wa])).unwrap();
    let (writer, follower) = (as_key(&cfg, &root, &kw.id), as_key(&cfg, &root, &kf.id));
    write_meta(&root, &wa, &WorktreeMeta { slug: "feat-docs".into(), project: u.clone(), branch: "feat-docs".into(), base_commit: "base".into(), created_ms: 1 }).unwrap();
    let w = who("writer", "m1");

    // The bundle is opaque bytes to the store. The shell runner (scripts/store/wo069-drills.sh, drill d1) round-trips a real git
    // bundle; this test uses `WEFTOS_DRILL_BUNDLE` (a file) when given, else a synthetic blob, and checks the bytes come back.
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repo.path().join("docs")).unwrap();
    std::fs::write(repo.path().join("docs/plan.md"), "# Plan\nunmerged design note, second commit\n").unwrap();
    std::fs::write(repo.path().join("src.rs"), "fn main() {}\n").unwrap();
    let bundle = repo.path().join("b.bundle");
    match std::env::var("WEFTOS_DRILL_BUNDLE") {
        Ok(f) => std::fs::copy(f, &bundle).map(|_| ()).unwrap(),
        Err(_) => std::fs::write(&bundle, b"# v2 git bundle\n".iter().cycle().take(64 << 10).copied().collect::<Vec<u8>>()).unwrap(),
    }
    let head = "0123456789abcdef0123456789abcdef01234567".to_string();
    let paths: Vec<String> = vec!["docs/plan.md".into(), "src.rs".into()];
    let src = |head: &'static str| TreeSource { root: repo.path(), paths: &paths, uncommitted: &[], changes: &[], branch: "feat-docs", base_commit: "base", head_commit: head, bundle: None, writer: w.clone() };

    let t = Instant::now();
    let src1 = TreeSource { head_commit: &head, bundle: Some(&bundle), ..src("x") };
    publish_tree(&writer, &wa, &src1, &Limits::default(), 1_000).unwrap();
    write_registry_entry(&root, &pb, &summarize(&root, "feat-docs", &wa).unwrap().unwrap()).unwrap();
    line("1a", "writer key publishes tree + bundle; daemon (root key) writes the registry entry", t.elapsed().as_millis(), "writer key is RW on its own bucket only");

    let dir = tempfile::tempdir().unwrap();
    let mut st = FollowState::default();
    let t = Instant::now();
    let ch = poll_fast(&follower, &pb, &mut st, false, true, 4).unwrap();
    assert!(ch.len() == 1 && matches!(ch[0], Change::New(_)), "{ch:?}");
    line("1a", "follower poll (digest) reports the new worktree", t.elapsed().as_millis(), "read-only key");
    let t = Instant::now();
    let entries = weftos_store::share_fetch::list_shared(&follower, &pb).unwrap();
    let hits = search_docs(&follower, &entries, "plan");
    assert_eq!((hits.hits.len(), hits.unreadable.len()), (1, 0));
    line("1a", "follower search finds docs/plan.md", t.elapsed().as_millis(), format!("{} hit", hits.hits.len()));
    let t = Instant::now();
    let rep = sync_docs(&follower, &entries[0], &dir.path().join("cache")).unwrap();
    assert!(!rep.written.is_empty());
    line("1a", "follower sync-docs mirrors the docs", t.elapsed().as_millis(), format!("{} written", rep.written.len()));
    let t = Instant::now();
    let reg = browse_registry(&follower, &pb).unwrap();
    let ls = browse_ls(&follower, &pb, "feat-docs", "docs").unwrap();
    let got = browse_get(&follower, &pb, "feat-docs", "docs/plan.md", 1 << 20).unwrap();
    assert_eq!((reg.worktrees.len(), ls.entries.len()), (1, 1));
    assert!(String::from_utf8_lossy(got.data.as_deref().unwrap()).contains("second commit"));
    line("1a", "follower browse: registry list, ls docs/, get docs/plan.md", t.elapsed().as_millis(), "read-only key");
    let t = Instant::now();
    let dest = tempfile::tempdir().unwrap();
    let fr = fetch_tree(&follower, &wa, dest.path(), Select::All, false).unwrap();
    assert_eq!((fr.written.len(), fr.missing.len(), fr.refused.len()), (2, 0, 0));
    let out = dest.path().join(".wt-branch.bundle");
    let got_head = fetch_bundle(&follower, &wa, &out).unwrap();
    assert_eq!(got_head, head, "the bundle carries the writer's head");
    assert_eq!(std::fs::read(&out).unwrap(), std::fs::read(&bundle).unwrap(), "the bundle came back byte for byte");
    line("1a", "follower fetch: tree (2 files) and bundle into a fresh directory, bundle bytes identical", t.elapsed().as_millis(), format!("head {}", &got_head[..8]));

    // writes: every one refused
    let t = Instant::now();
    let mut refused = vec![];
    let mut must_fail = |what: &str, ok: bool| {
        assert!(!ok, "the follower key was allowed to: {what}");
        refused.push(what.to_string());
    };
    must_fail("put into the worktree bucket", ObjectBackend::put(&follower, &wa, "scratch/x", b"x").is_ok());
    must_fail("write the handoff record", ObjectBackend::put(&follower, &wa, HANDOFF_KEY, b"{}").is_ok());
    must_fail("write a registry entry", ObjectBackend::put(&follower, &pb, "worktrees/forged.json", b"{}").is_ok());
    must_fail("write the digest", ObjectBackend::put(&follower, &pb, DIGEST_KEY, b"{}").is_ok());
    // the pool object the manifest names for src.rs (schema 2, lane D3); the follower may read the manifest, not delete
    let tree_obj = { let m = read_manifest(&follower, &wa).unwrap().unwrap(); m.object_key(m.files.iter().find(|f| f.path == "src.rs").unwrap()) };
    must_fail("delete a tree object", ObjectBackend::delete(&follower, &wa, &tree_obj).is_ok());
    must_fail("publish_tree", publish_tree(&follower, &wa, &src("zz"), &Limits::default(), 2_000).is_ok());
    mark_handoff(&writer, &wa, &w, "ready", 3_000).unwrap();
    must_fail("claim_merge", claim_merge(&follower, &wa, &who("f", "m9"), 4_000).is_ok());
    must_fail("create a bucket", AdminBackend::create_bucket(&follower, "t-follower-made", &[]).is_ok());
    // and the writer key cannot reach the registry either
    assert!(ObjectBackend::put(&writer, &pb, "worktrees/forged.json", b"{}").is_err());
    line("1a", "follower refused every write", t.elapsed().as_millis(), refused.join("; "));

    // publish -> poll latency: N trials, each = writer publish a new head, daemon refresh, follower poll until it reports
    let mut lat = vec![];
    for i in 0..10u64 {
        publish_tree(&writer, &wa, &TreeSource { head_commit: &format!("h{i}"), bundle: None, ..src("x") }, &Limits::default(), 10_000 + i).unwrap();
        let tw = Instant::now();
        write_registry_entry(&root, &pb, &summarize(&root, "feat-docs", &wa).unwrap().unwrap()).unwrap();
        let mut polls = 0;
        loop {
            polls += 1;
            let ch = poll_fast(&follower, &pb, &mut st, false, true, 4).unwrap();
            if !ch.is_empty() {
                break;
            }
            assert!(polls < 5, "a head move must show within a few polls");
        }
        lat.push((tw.elapsed().as_millis(), polls));
    }
    println!("DETAIL|1a|registry-refresh -> follower poll reports it (ms, polls): {lat:?}");
    assert!(lat.iter().all(|(_, p)| *p == 1), "the first poll after the refresh reports it");
    let mut ms: Vec<u128> = lat.iter().map(|x| x.0).collect();
    ms.sort();
    line("1a", "publish seen within one poll (10 trials)", ms[5], format!("registry refresh -> first poll reports: min {} median {} max {} ms", ms[0], ms[5], ms[9]));

    let _ = ObjectBackend::list(&root, &wa, "").map(|v| v.into_iter().for_each(|m| ObjectBackend::delete(&root, &wa, &m.key).unwrap()));
    wipe(&root, &[&wa, &pb]);
    for k in [&kw, &kf] {
        root.delete_access_key(&k.id).unwrap();
    }
}

/// Test-only manipulation of the THROWAWAY store for the shell drills (the daemon verbs have no clock override and the registry
/// is reserved control state a governed `put` refuses). `WEFTOS_DRILL_OP` is one of
/// `backdate|<bucket>|<key>|<field>|<days>` (field `updated_ms` or `orphan.orphaned_at_ms`, moved back by that many days),
/// `list|<bucket>|<prefix>` (prints the object count and keys), `exists|<bucket>`. Prints `HELPER|...`. Without the env var: a no-op.
#[test]
#[ignore = "helper for scripts/store/wo069-drills.sh; needs a throwaway RustFS"]
fn helper_throwaway_store_op() {
    let Ok(op) = std::env::var("WEFTOS_DRILL_OP") else { return };
    let (be, _d) = backend();
    let p: Vec<&str> = op.split('|').collect();
    match p.as_slice() {
        ["backdate", bucket, key, field, days] => {
            let mut v: serde_json::Value = serde_json::from_slice(&be.get(bucket, key).unwrap().expect("entry")).unwrap();
            let by = days.parse::<u64>().unwrap() * D;
            let slot = match *field {
                "updated_ms" => &mut v["updated_ms"],
                "orphan.orphaned_at_ms" => &mut v["orphan"]["orphaned_at_ms"],
                f => panic!("unknown field {f}"),
            };
            let old = slot.as_u64().expect("numeric field");
            *slot = serde_json::json!(old - by);
            be.put(bucket, key, &serde_json::to_vec_pretty(&v).unwrap()).unwrap();
            println!("HELPER|backdate|{key}|{field}|{old} -> {}", old - by);
        }
        ["list", bucket, prefix] => {
            let r = ObjectBackend::list(&be, bucket, prefix);
            match r {
                Ok(v) => println!("HELPER|list|{bucket}|{prefix}|count={}|{}", v.len(), v.iter().map(|m| m.key.as_str()).take(12).collect::<Vec<_>>().join(",")),
                Err(e) => println!("HELPER|list|{bucket}|{prefix}|error={e}"),
            }
        }
        ["exists", bucket] => println!("HELPER|exists|{bucket}|{}", AdminBackend::bucket_exists(&be, bucket).unwrap()),
        _ => panic!("unknown WEFTOS_DRILL_OP {op}"),
    }
}
