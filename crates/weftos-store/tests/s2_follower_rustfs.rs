//! L1 follower view against a real RustFS (ADR-126 §4a gate item 1, ADR-131 gap "L1 / follower view").
//!
//! Needs a THROWAWAY RustFS (never PG): set `WEFTOS_STORE_TEST_RUSTFS_ENV` to an env file with
//! `MESH_S3_ENDPOINT`, `MESH_S3_ACCESS_KEY`, `MESH_S3_SECRET_KEY`, then
//! `cargo test -p weftos-store --features rustfs --test s2_follower_rustfs -- --ignored`.
//!
//! Three real S3 identities, each with its own scoped key, stand in for three machines:
//! - the **daemon** (root key): creates buckets and keys, writes the registry;
//! - a **writer** (RW on its own worktree bucket only): publishes the tree and a handoff;
//! - a **follower** (read-only on the project bucket and the worktree buckets): polls, searches,
//!   mirrors docs, and must be refused every write.
#![cfg(feature = "rustfs")]

use serde_json::json;
use std::path::Path;
use std::time::{Duration, Instant};
use weftos_store::backend::AdminBackend;
use weftos_store::follower::*;
use weftos_store::name::StoreName;
use weftos_store::overlay::ObjectBackend as S2;
use sha2::{Digest, Sha256};
use weftos_store::promote::{WorktreeMeta, write_meta};
use weftos_store::registry_sign::*;
use weftos_store::share::*;
use weftos_store::share_fetch::*;
use weftos_store::{RustfsBackend, RustfsConfig};

fn live() -> (RustfsConfig, tempfile::TempDir) {
    let f = std::env::var("WEFTOS_STORE_TEST_RUSTFS_ENV").expect("WEFTOS_STORE_TEST_RUSTFS_ENV");
    let d = tempfile::tempdir().unwrap();
    (
        RustfsConfig::from_env_file(Path::new(&f), d.path().join("secrets")).unwrap(),
        d,
    )
}

fn as_key(cfg: &RustfsConfig, root: &RustfsBackend, key_id: &str) -> RustfsBackend {
    let secret = std::fs::read_to_string(root.secret_path(key_id)).unwrap();
    RustfsBackend::new(RustfsConfig {
        root_access_key: key_id.into(),
        root_secret_key: secret,
        ..cfg.clone()
    })
    .unwrap()
}

fn policy(rw: &[&str], ro: &[&str]) -> serde_json::Value {
    let arn = |b: &&str| [format!("arn:aws:s3:::{b}"), format!("arn:aws:s3:::{b}/*")];
    let mut st = vec![];
    if !rw.is_empty() {
        st.push(json!({"Effect": "Allow",
            "Action": ["s3:GetObject", "s3:PutObject", "s3:DeleteObject", "s3:ListBucket"],
            "Resource": rw.iter().flat_map(arn).collect::<Vec<_>>()}));
    }
    if !ro.is_empty() {
        st.push(json!({"Effect": "Allow", "Action": ["s3:GetObject", "s3:ListBucket"],
            "Resource": ro.iter().flat_map(arn).collect::<Vec<_>>()}));
    }
    json!({"Version": "2012-10-17", "Statement": st})
}

/// Keyed-hash stand-in for the daemon's ed25519 key: the point here is the real store round trip.
struct Toy;
impl EntrySigner for Toy {
    fn signer_id(&self) -> String {
        "toy".into()
    }
    fn alg(&self) -> &'static str {
        "toy-sha256"
    }
    fn sign(&self, m: &[u8]) -> Vec<u8> {
        Sha256::new().chain_update(b"k").chain_update(m).finalize().to_vec()
    }
}
impl EntryVerifier for Toy {
    fn verify(&self, s: &str, a: &str, m: &[u8], sig: &[u8]) -> bool {
        s == "toy" && a == "toy-sha256" && self.sign(m) == sig
    }
}

fn actor(n: &str) -> Actor {
    Actor {
        agent: n.into(),
        host: "h".into(),
    }
}

/// Publish `files` into `bucket` through `be` (the writer's own key), as `worktree-store.sh` would.
fn publish<B: S2>(be: &B, bucket: &str, slug: &str, head: &str, files: &[(&str, &str)], now: u64) {
    let d = tempfile::tempdir().unwrap();
    let mut ps = vec![];
    for (p, c) in files {
        let f = d.path().join(p);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, c).unwrap();
        ps.push(p.to_string());
    }
    let src = TreeSource {
        root: d.path(),
        paths: &ps,
        uncommitted: &[],
        changes: &[],
        branch: slug,
        base_commit: "base",
        head_commit: head,
        bundle: None,
        writer: actor("writer"),
    };
    publish_tree(be, bucket, &src, &Limits::default(), now).unwrap();
}

fn drain(root: &RustfsBackend, bucket: &str) {
    use weftos_store::backend::ObjectBackend as S1;
    for m in S1::list(root, bucket, "").unwrap() {
        S1::delete(root, bucket, &m.key).unwrap();
    }
    root.delete_bucket(bucket).unwrap();
}

#[test]
#[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
fn live_follower_sees_unmerged_worktrees_across_three_identities() {
    let (cfg, _d) = live();
    let root = RustfsBackend::new(cfg.clone()).unwrap();
    let uniq = ulid::Ulid::new().to_string().to_lowercase();
    // Unique per run so a leftover from an aborted run cannot affect this one.
    let project = &uniq;
    let pb = StoreName::project("lab", project).unwrap().bucket();
    let (sa, sb) = (format!("docs-{}", &uniq[18..]), format!("code-{}", &uniq[18..]));
    let wa = StoreName::worktree("lab", project, &sa).unwrap().bucket();
    let wb = StoreName::worktree("lab", project, &sb).unwrap().bucket();
    for b in [&pb, &wa, &wb] {
        root.create_bucket(b, &[]).unwrap();
    }

    // Identities. The daemon is the root key; agents never get the project bucket for writing.
    let ka = root.create_access_key(&format!("writer-a:{uniq}")).unwrap();
    let kb = root.create_access_key(&format!("writer-b:{uniq}")).unwrap();
    let kf = root.create_access_key(&format!("follower:{uniq}")).unwrap();
    let km = root.create_access_key(&format!("merger:{uniq}")).unwrap();
    root.put_policy(&ka.id, &policy(&[&wa], &[])).unwrap();
    root.put_policy(&kb.id, &policy(&[&wb], &[])).unwrap();
    root.put_policy(&kf.id, &policy(&[], &[&pb, &wa, &wb])).unwrap();
    root.put_policy(&km.id, &policy(&[&wa], &[&pb])).unwrap();
    let (writer_a, writer_b, follower, merger) = (
        as_key(&cfg, &root, &ka.id),
        as_key(&cfg, &root, &kb.id),
        as_key(&cfg, &root, &kf.id),
        as_key(&cfg, &root, &km.id),
    );

    // Daemon side: stamp each worktree bucket with its identity (meta), as `create` does.
    for (slug, b) in [(&sa, &wa), (&sb, &wb)] {
        write_meta(
            &root,
            b,
            &WorktreeMeta {
                slug: slug.clone(),
                project: project.clone(),
                branch: slug.clone(),
                base_commit: "base".into(),
                created_ms: 0,
            },
        )
        .unwrap();
    }

    // Writers publish unmerged work with their own keys; nothing is merged anywhere.
    let t0 = 1_000_000u64;
    publish(&writer_a, &wa, &sa, "h1", &[
        ("docs/plan.md", "# Plan\nunmerged design note"),
        ("docs/shared.md", "writer a version"),
        ("src/lib.rs", "// a"),
    ], t0);
    publish(&writer_b, &wb, &sb, "h1", &[
        ("docs/shared.md", "writer b version"),
        ("src/lib.rs", "// b"),
    ], t0);

    // A writer cannot touch the registry or another worktree: the daemon is the only registry writer.
    let reg_probe = S2::put(&writer_a, &pb, "worktrees/forged.json", b"{}");
    assert!(reg_probe.is_err(), "a worktree key must not write the project registry");
    assert!(S2::put(&writer_a, &wb, "x", b"y").is_err(), "a writer key is scoped to its own bucket");

    // Daemon publishes the registry entries.
    for (slug, b) in [(&sa, &wa), (&sb, &wb)] {
        let e = summarize(&root, slug, b).unwrap().expect("published");
        write_registry_entry(&root, &pb, &e).unwrap();
    }

    // ---- the follower, on its own read-only key ----
    let dir = tempfile::tempdir().unwrap();
    let state_path = dir.path().join("follow.json");
    let mut st = FollowState::load(&state_path).unwrap();
    let ch = poll(&follower, &pb, &mut st).unwrap();
    st.save(&state_path).unwrap();
    assert_eq!(ch.len(), 2, "both unmerged worktrees are new: {ch:?}");
    assert!(ch.iter().all(|c| matches!(c, Change::New(_))));

    let entries = list_shared(&follower, &pb).unwrap();
    let hits = search_docs(&follower, &entries, "plan");
    assert!(hits.unreadable.is_empty());
    assert_eq!(hits.hits.len(), 1);
    assert_eq!((hits.hits[0].slug.as_str(), hits.hits[0].path.as_str()), (sa.as_str(), "docs/plan.md"));

    let cache = dir.path().join("cache");
    let ea = entries.iter().find(|e| e.slug == sa).unwrap();
    let rep = sync_docs(&follower, ea, &cache).unwrap();
    assert!(!rep.written.is_empty(), "{rep:?}");
    assert_eq!(
        std::fs::read_to_string(cache.join(&sa).join("docs/plan.md")).unwrap(),
        "# Plan\nunmerged design note"
    );

    let eb = entries.iter().find(|e| e.slug == sb).unwrap();
    let ov = overlaps(&follower, ea, eb).unwrap();
    let paths: Vec<_> = ov.overlaps.iter().map(|o| o.path.as_str()).collect();
    assert!(paths.contains(&"docs/shared.md") && paths.contains(&"src/lib.rs"), "{paths:?}");

    // The follower is read-only everywhere, including its own cache's source buckets.
    for b in [&pb, &wa, &wb] {
        assert!(S2::put(&follower, b, "worktrees/x.json", b"{}").is_err(), "follower wrote {b}");
        assert!(S2::put(&follower, b, ".wt/handoff.json", b"{}").is_err());
    }
    assert!(claim_merge(&follower, &wa, &actor("f"), t0).is_err(), "a follower cannot claim");

    // ---- signed registry on a real store: the signature covers the bytes RustFS actually holds ----
    let e = summarize(&root, &sb, &wb).unwrap().unwrap();
    write_registry_entry_signed(&root, &pb, &e, &Toy).unwrap();
    assert_eq!(verify_registry_entry(&follower, &pb, &sb, &Toy).unwrap(), Trust::Verified);
    assert_eq!(verify_registry_entry(&follower, &pb, &sa, &Toy).unwrap(), Trust::Unsigned);
    // a project writer cannot re-sign: the follower key (and a worktree key) cannot write the sidecar
    assert!(S2::put(&follower, &pb, "worktrees.sig/forged.json", b"{}").is_err());
    assert!(S2::put(&writer_b, &pb, "worktrees.sig/forged.json", b"{}").is_err());
    // an edit by anyone who does hold project write is caught
    let mut moved = e.clone();
    moved.head_commit = "forged".into();
    write_registry_entry(&root, &pb, &moved).unwrap();
    assert_eq!(verify_registry_entry(&follower, &pb, &sb, &Toy).unwrap(), Trust::Bad);
    let strict = list_shared_verified(&follower, &pb, &Toy, true).unwrap();
    assert!(strict.is_empty(), "strict follower sees neither unsigned nor edited entries");
    write_registry_entry_signed(&root, &pb, &e, &Toy).unwrap(); // daemon restores it

    // ---- head moves: a follower sees it on its next poll; measure the real latency ----
    let moved = Instant::now();
    publish(&writer_a, &wa, &sa, "h2", &[("docs/plan.md", "# Plan v2"), ("src/lib.rs", "// a2")], t0 + 10);
    let e = summarize(&root, &sa, &wa).unwrap().unwrap();
    write_registry_entry(&root, &pb, &e).unwrap();
    let ch = poll(&follower, &pb, &mut st).unwrap();
    let latency = moved.elapsed();
    assert!(
        ch.iter().any(|c| matches!(c, Change::HeadMoved { entry, from } if entry.slug == sa && from == "h1")),
        "{ch:?}"
    );
    assert!(latency < Duration::from_secs(5), "publish->poll took {latency:?}");
    eprintln!("L1 spike: publish -> registry refresh -> follower poll saw HeadMoved in {latency:?}");
    // A second poll with nothing new is quiet.
    assert!(poll(&follower, &pb, &mut st).unwrap().is_empty());

    // ---- handoff and claim: states cross identities ----
    mark_handoff(&writer_a, &wa, &actor("writer-a"), "ready", t0 + 20).unwrap();
    let e = summarize(&root, &sa, &wa).unwrap().unwrap();
    write_registry_entry(&root, &pb, &e).unwrap();
    let ch = poll(&follower, &pb, &mut st).unwrap();
    assert!(ch.iter().any(|c| matches!(c, Change::StateChanged { entry, .. } if entry.state == State::Handoff)), "{ch:?}");
    let h = claim_merge(&merger, &wa, &actor("merger"), t0 + 30).unwrap();
    assert_eq!(h.state, State::Claimed);
    assert_eq!(h.epoch, 1);
    let late = claim_merge(&writer_a, &wa, &actor("writer-a"), t0 + 31);
    assert!(late.is_err(), "a live claim by another agent is refused: {late:?}");

    // Leftovers: a worktree idle for days is flagged from the follower's own view.
    let entries = list_shared(&follower, &pb).unwrap();
    let far = t0 + 4 * 24 * 3600 * 1000;
    let lo = leftovers(&entries, far, &LeftoverPolicy::default());
    assert!(lo.iter().any(|l| l.entry.slug == sb), "idle open worktree is a leftover: {lo:?}");

    // ---- merged and cleaned up: gone from the follower's view ----
    mark_merged(&merger, &wa, &actor("merger"), "done", t0 + 40).unwrap();
    remove_registry_entry(&root, &pb, &sa).unwrap();
    let ch = poll(&follower, &pb, &mut st).unwrap();
    assert!(ch.iter().any(|c| matches!(c, Change::Gone { slug } if *slug == sa)), "{ch:?}");

    // Clean up the throwaway buckets and keys.
    remove_registry_entry_signed(&root, &pb, &sb).unwrap();
    for b in [&pb, &wa, &wb] {
        drain(&root, b);
    }
    for k in [&ka, &kb, &kf, &km] {
        root.delete_access_key(&k.id).unwrap();
    }
}
