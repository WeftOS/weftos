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

// ---- relay notification path -------------------------------------------------------------------
// Needs RustFS started with a NOTIFY webhook target named `wake` posting to 127.0.0.1:<port>, and
// WEFTOS_STORE_TEST_NOTIFY_PORT=<port>; see docs/research/adr126-s2-l1-follower-view.md for the exact
// start command. The listener below plays the site subscriber; the mpsc channel plays the relay hop.

/// Minimal webhook receiver: one POST per connection, body to the channel, 200 back.
fn webhook_listener(port: u16) -> std::sync::mpsc::Receiver<(Instant, String)> {
    use std::io::{Read, Write};
    let l = std::net::TcpListener::bind(("127.0.0.1", port)).expect("bind the notify port");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let tx = tx.clone();
            std::thread::spawn(move || {
                let mut s = s;
                s.set_read_timeout(Some(Duration::from_secs(5))).ok();
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                let (head_end, want) = loop {
                    let Ok(n) = s.read(&mut tmp) else { return };
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..i]).to_lowercase();
                        let len = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
                            .unwrap_or(0);
                        break (i + 4, len);
                    }
                };
                while buf.len() < head_end + want {
                    let Ok(n) = s.read(&mut tmp) else { return };
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                }
                let _ = s.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
                let _ = tx.send((Instant::now(), String::from_utf8_lossy(&buf[head_end..]).into_owned()));
            });
        }
    });
    rx
}

#[test]
#[ignore = "needs a throwaway RustFS with a notify target: WEFTOS_STORE_TEST_RUSTFS_ENV + WEFTOS_STORE_TEST_NOTIFY_PORT"]
fn live_a_bucket_notification_wakes_the_follower_before_its_fallback_poll() {
    use weftos_store::wake::{Waker, registry_hints};
    let port: u16 = std::env::var("WEFTOS_STORE_TEST_NOTIFY_PORT").expect("WEFTOS_STORE_TEST_NOTIFY_PORT").parse().unwrap();
    let rx = webhook_listener(port);
    let (cfg, _d) = live();
    let root = RustfsBackend::new(cfg.clone()).unwrap();
    let uniq = ulid::Ulid::new().to_string().to_lowercase();
    let slug = format!("wake-{}", &uniq[18..]);
    let pb = StoreName::project("lab", &uniq).unwrap().bucket();
    let wa = StoreName::worktree("lab", &uniq, &slug).unwrap().bucket();
    for b in [&pb, &wa] {
        root.create_bucket(b, &[]).unwrap();
    }
    root.put_bucket_notification(&pb, "wake").expect("notify target `wake` must exist on the RustFS");
    let kf = root.create_access_key(&format!("follower:{uniq}")).unwrap();
    root.put_policy(&kf.id, &policy(&[], &[&pb, &wa])).unwrap();
    let follower = as_key(&cfg, &root, &kf.id);
    write_meta(&root, &wa, &WorktreeMeta { slug: slug.clone(), project: uniq.clone(), branch: slug.clone(), base_commit: "base".into(), created_ms: 0 }).unwrap();
    publish(&root, &wa, &slug, "h1", &[("docs/plan.md", "# p")], 1);

    // The follower: fallback poll a minute away, so any change seen within seconds came from a wake-up.
    let t0 = Instant::now();
    let now = || t0.elapsed().as_millis() as u64;
    let mut waker = Waker::new(300, 60_000, now());
    let mut st = FollowState::default();
    assert!(poll(&follower, &pb, &mut st).unwrap().is_empty(), "empty registry, nothing known yet");

    // Wait for the follower's next change: block on the relay channel, poll when the waker says so.
    let mut next_change = |what: &str| -> (Vec<Change>, Duration) {
        let started = Instant::now();
        loop {
            assert!(started.elapsed() < Duration::from_secs(20), "no wake-up for: {what}");
            if waker.due(now()) {
                // confirmed: the last worktree leaving makes the registry legitimately empty
                let ch = poll_confirmed(&follower, &pb, &mut st).unwrap();
                waker.polled(now());
                if !ch.is_empty() {
                    return (ch, started.elapsed());
                }
            }
            let wait = Duration::from_millis(waker.wait_ms(now()).clamp(10, 1_000));
            if let Ok((_, body)) = rx.recv_timeout(wait)
                && !registry_hints(&body, &pb).is_empty()
            {
                waker.hint(now());
            }
        }
    };

    // 1. the daemon registers the worktree (entry, then signature)
    let e = summarize(&root, &slug, &wa).unwrap().unwrap();
    write_registry_entry_signed(&root, &pb, &e, &Toy).unwrap();
    let (ch, took) = next_change("new worktree");
    assert!(matches!(&ch[..], [Change::New(x)] if x.slug == slug), "{ch:?}");
    assert!(took < Duration::from_secs(15), "{took:?}");
    eprintln!("relay spike: registry write -> notification -> settle -> follower saw New in {took:?}");
    assert_eq!(verify_registry_entry(&follower, &pb, &slug, &Toy).unwrap(), Trust::Verified, "signature had landed by the time we polled");

    // 2. head moves
    publish(&root, &wa, &slug, "h2", &[("docs/plan.md", "# p2")], 2);
    let e = summarize(&root, &slug, &wa).unwrap().unwrap();
    write_registry_entry_signed(&root, &pb, &e, &Toy).unwrap();
    let (ch, took) = next_change("head move");
    assert!(matches!(&ch[..], [Change::HeadMoved { from, .. }] if from == "h1"), "{ch:?}");
    eprintln!("relay spike: head move seen in {took:?}");

    // 3. removal arrives as a wake-up too (the removed flag is a hint, the poll decides)
    remove_registry_entry_signed(&root, &pb, &slug).unwrap();
    let (ch, _) = next_change("removal");
    assert!(matches!(&ch[..], [Change::Gone { slug: s }] if *s == slug), "{ch:?}");

    // 4. nothing else is pending: no phantom wake-ups
    assert!(rx.recv_timeout(Duration::from_millis(800)).map(|(_, b)| registry_hints(&b, &pb).is_empty()).unwrap_or(true));

    for b in [&pb, &wa] {
        drain(&root, b);
    }
    root.delete_access_key(&kf.id).unwrap();
}
