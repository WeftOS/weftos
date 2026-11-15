//! WO-069 lane F3: the writer half and the follower half of ADR-126 gate item 1, as two separate tests driven only by environment
//! variables, so the same binary serves both machines of the two-machine drill (the follower machine needs no weaver and no daemon,
//! only this binary, an env file with a READ-ONLY key, and the endpoint). `both_halves_on_one_machine` runs both against a throwaway
//! RustFS for local verification. Every test is `#[ignore]` and needs a THROWAWAY server (never PG).
//!
//! `writer_half` (the machine with the writer key): `WEFTOS_FH_WRITER_ENV` (env file: MESH_S3_ENDPOINT / _ACCESS_KEY / _SECRET_KEY, a key
//! that can write both buckets), `WEFTOS_FH_PROJECT_BUCKET`, `WEFTOS_FH_WORKTREE_BUCKET`, `WEFTOS_FH_SLUG`, `WEFTOS_FH_ROUND` (head is
//! `h<round>`), optional `WEFTOS_FH_BUNDLE` (a file to publish as the git bundle), optional `WEFTOS_FH_CREATE=1` (create absent buckets).
//! Prints `FH|writer|round=<n>|published_ms=<local ms>|head=h<n>` once the registry entry and digest are written.
//!
//! `follower_half` (the other machine): `WEFTOS_FH_FOLLOWER_ENV` (read-only key), the same bucket/slug names, `WEFTOS_FH_STATE` (a
//! follow-state file, kept between runs), `WEFTOS_FH_EXPECT_HEAD`, optional `WEFTOS_FH_WAIT_MS` and `WEFTOS_FH_POLL_MS` (default 2000),
//! optional `WEFTOS_FH_BUNDLE` (compared byte for byte when readable). Prints `FH|follower|<step>|PASS|<ms>|<detail>` per step,
//! `FH|follower|poll|seen_ms=<local ms>|head=<head>` and `FH|follower|clock|now_ms=<local ms>`.
//!
//! Run: `cargo test -p weftos-store --features rustfs --test s2_follower_half <test> -- --ignored --nocapture --test-threads=1`.
#![cfg(feature = "rustfs")]

#[path = "s2_drills_common/mod.rs"]
mod s2_drills_common;
use s2_drills_common::*;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use weftos_store::backend::{AdminBackend, BackendError};
use weftos_store::follower::{FollowState, poll_fast, search_docs, sync_docs};
use weftos_store::follower_browse::{browse_get, browse_ls, browse_registry};
use weftos_store::name::StoreName;
use weftos_store::overlay::{ObjectBackend, S2Error};
use weftos_store::promote::{WorktreeMeta, write_meta};
use weftos_store::share::*;
use weftos_store::share_fetch::*;
use weftos_store::{RustfsBackend, RustfsConfig};

struct Names {
    pb: String,
    wb: String,
    slug: String,
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64
}

fn env_req(k: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| panic!("{k} must be set"))
}

fn env_num(k: &str, default: u64) -> u64 {
    std::env::var(k).ok().filter(|v| !v.is_empty()).map_or(default, |v| v.parse().unwrap_or_else(|_| panic!("{k} must be a number")))
}

/// The worktree bucket is `WEFTOS_FH_WORKTREE_BUCKET`, or (unset) the one the registry cross-check expects for a real `p-<ULID>`
/// project bucket and the slug, so a caller that only knows the project and the slug (the gate script) gets the right name.
fn names_from_env() -> Names {
    let (pb, slug) = (env_req("WEFTOS_FH_PROJECT_BUCKET"), env_req("WEFTOS_FH_SLUG"));
    let wb = std::env::var("WEFTOS_FH_WORKTREE_BUCKET").ok().filter(|v| !v.is_empty())
        .or_else(|| expected_worktree_bucket(&pb, &slug))
        .expect("WEFTOS_FH_WORKTREE_BUCKET must be set unless the project bucket is p-<ULID>");
    Names { pb, wb, slug }
}

/// A backend for the key in the env file named by `var`; the directory keeps the key's secret files and must outlive the backend.
fn backend_from(var: &str) -> (RustfsBackend, RustfsConfig, tempfile::TempDir) {
    let f = env_req(var);
    let d = tempfile::tempdir().unwrap();
    let cfg = RustfsConfig::from_env_file(Path::new(&f), d.path().join("secrets")).unwrap();
    (RustfsBackend::new(cfg.clone()).unwrap(), cfg, d)
}

fn step(name: &str, t: Instant, detail: impl std::fmt::Display) {
    println!("FH|follower|{name}|PASS|{} ms|{detail}", t.elapsed().as_millis());
}

/// A small synthetic tree whose bytes carry `head`, so every round differs in the docs and in the source file.
fn make_tree(head: &str, who_: &str) -> (tempfile::TempDir, Vec<String>) {
    let d = tempfile::tempdir().unwrap();
    let files = [
        ("docs/plan.md", format!("# Plan\nunmerged design note, head {head}\n")),
        ("docs/d0.md", format!("# Doc zero\nwritten by {who_} at {head}\n")),
        ("docs/d1.md", "# Doc one\nnever changes\n".to_string()),
        ("src.rs", format!("fn main() {{ /* {who_} {head} */ }}\n")),
    ];
    let mut paths = vec![];
    for (p, c) in files {
        let f = d.path().join(p);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, c).unwrap();
        paths.push(p.to_string());
    }
    (d, paths)
}

/// Publish round `round` and refresh the registry entry and digest the way the daemon does (`summarize_with`, then
/// `write_registry_entry`, which rewrites the digest). Returns the local clock when the refresh finished.
fn writer_round(be: &RustfsBackend, nm: &Names, round: u64, bundle: Option<&Path>) -> u64 {
    let head = format!("h{round}");
    let w = who("writer", "m1");
    if std::env::var("WEFTOS_FH_CREATE").as_deref() == Ok("1") {
        for b in [&nm.pb, &nm.wb] {
            match AdminBackend::create_bucket(be, b, &[]) {
                Ok(()) | Err(BackendError::BucketExists(_)) => {}
                Err(e) => panic!("create_bucket {b}: {e}"),
            }
        }
    }
    if round <= 1 {
        let project = nm.pb.strip_prefix("p-").unwrap_or(&nm.pb).to_string();
        let meta = WorktreeMeta { slug: nm.slug.clone(), project, branch: nm.slug.clone(), base_commit: "base".into(), created_ms: now_ms() };
        if let Err(e) = write_meta(be, &nm.wb, &meta) {
            println!("DETAIL|fh|write_meta (best effort): {e}");
        }
    }
    let (dir, paths) = make_tree(&head, "writer");
    let src = TreeSource {
        root: dir.path(), paths: &paths, uncommitted: &[], changes: &[], changes_recorded: false, branch: &nm.slug, base_commit: "base",
        head_commit: &head, bundle, writer: w,
    };
    let t = Instant::now();
    let rep = publish_tree(be, &nm.wb, &src, &Limits::default(), now_ms()).unwrap();
    let m = read_manifest(be, &nm.wb).unwrap().expect("manifest after publish");
    let h = read_handoff(be, &nm.wb).unwrap().expect("handoff record after publish");
    write_registry_entry(be, &nm.pb, &summarize_with(&nm.slug, &nm.wb, m, h)).unwrap();
    let d = read_digest(be, &nm.pb).unwrap().expect("the registry refresh left a digest");
    assert!(d.entries.contains_key(&nm.slug), "the digest names the worktree after the refresh: {:?}", d.entries.keys().collect::<Vec<_>>());
    let published = now_ms();
    println!("FH|writer|names|project_bucket={}|worktree_bucket={}|slug={}", nm.pb, nm.wb, nm.slug);
    println!("DETAIL|fh|writer round {round}: uploaded {} unchanged {} in {} ms", rep.uploaded, rep.unchanged, t.elapsed().as_millis());
    println!("FH|writer|round={round}|published_ms={published}|head={head}");
    published
}

fn denied(e: &S2Error) -> bool {
    let s = e.to_string().to_lowercase();
    s.contains("forbidden") || s.contains("accessdenied") || s.contains("access denied")
}

fn head_of(e: &impl std::fmt::Display) -> String {
    e.to_string().chars().take(90).collect()
}

struct Seen {
    polls: u32,
    seen_ms: u64,
}

/// Every follower step against a store somebody else published to, with whatever key `be` holds. `expect` is the head that must be
/// visible; with `wait_ms` the digest poll repeats every `poll_ms` until it is.
fn follower_round(be: &RustfsBackend, nm: &Names, state: &Path, expect: Option<&str>, wait_ms: u64, poll_ms: u64, bundle: Option<&Path>) -> Seen {
    println!("FH|follower|clock|now_ms={}", now_ms());
    // poll, digest path: repeated until the expected head shows
    let mut st = FollowState::load(state).unwrap();
    let (t, mut polls) = (Instant::now(), 0u32);
    let mut seen = None;
    loop {
        polls += 1;
        let ch = poll_fast(be, &nm.pb, &mut st, false, true, 4).expect("poll (digest path)");
        let head = st.seen.get(&nm.slug).map(|s| s.head_commit.clone());
        if expect.is_none() || head.as_deref() == expect {
            seen = Some((now_ms(), head, ch.len()));
            break;
        }
        if t.elapsed() >= Duration::from_millis(wait_ms) {
            println!("DETAIL|fh|poll {polls}: saw {head:?}, expected {expect:?}");
            break;
        }
        std::thread::sleep(Duration::from_millis(poll_ms));
    }
    st.save(state).unwrap();
    let Some((seen_ms, head, nchanges)) = seen else { panic!("the expected head {expect:?} was not seen within {wait_ms} ms ({polls} polls)") };
    let head = head.unwrap_or_default();
    step("poll", t, format!("digest path: {polls} poll(s), {nchanges} change(s) on the last, head {head}"));
    println!("FH|follower|poll|seen_ms={seen_ms}|head={head}");
    // poll, --full path: a fresh state, no digest, the registry listing
    let t = Instant::now();
    let mut full = FollowState::default();
    poll_fast(be, &nm.pb, &mut full, false, false, 4).expect("poll (full path)");
    let fh = full.seen.get(&nm.slug).map(|s| s.head_commit.clone());
    assert_eq!(fh.as_deref(), Some(head.as_str()), "the --full path and the digest path agree on the head");
    step("poll-full", t, format!("listing path, head {}", fh.unwrap_or_default()));

    let entries = list_shared(be, &nm.pb).unwrap();
    let entry = entries.iter().find(|e| e.slug == nm.slug).expect("the registry lists the worktree").clone();
    let m = read_manifest(be, &nm.wb).unwrap().expect("manifest");
    let h = read_handoff(be, &nm.wb).unwrap();
    // the older-writer check (lane F1): what the entry's writer recorded, and the same check recomputed from the bucket
    let t = Instant::now();
    let (from_entry, from_bucket) = (entry_older_writer(&entry), detect_older_writer(Some(&m), h.as_ref()));
    let verdict = |o: &Option<OlderWriter>| o.as_ref().map_or("clean".to_string(), |o| format!("{:?}: {}", o.kind, o.what));
    println!("DETAIL|fh|older-writer: entry says {}; bucket says {}", verdict(&from_entry), verdict(&from_bucket));
    assert!(from_entry.is_none() && from_bucket.is_none(), "an older-writer verdict on a store this build's writer published: entry {from_entry:?}, bucket {from_bucket:?}");
    step("older-writer", t, format!("clean (schema {}, pooled {}, writer_check level {})", m.schema, m.is_pooled(), entry.writer_check.as_ref().map_or(0, |c| c.level)));

    let t = Instant::now();
    let hits = search_docs(be, &entries, "plan");
    assert!(hits.hits.iter().any(|x| x.slug == nm.slug && x.path == "docs/plan.md") && hits.unreadable.is_empty(), "{hits:?}");
    step("search", t, format!("{} hit(s)", hits.hits.len()));

    let t = Instant::now();
    let cache = tempfile::tempdir().unwrap();
    let rep = sync_docs(be, &entry, cache.path()).unwrap();
    assert!(!rep.written.is_empty() && rep.refused.is_empty() && rep.missing.is_empty(), "{rep:?}");
    step("sync-docs", t, format!("{} written into a temp dir", rep.written.len()));

    let t = Instant::now();
    let reg = browse_registry(be, &nm.pb).unwrap();
    let ls = browse_ls(be, &nm.pb, &nm.slug, "docs").unwrap();
    let got = browse_get(be, &nm.pb, &nm.slug, "docs/plan.md", 1 << 20).unwrap();
    let body = String::from_utf8_lossy(got.data.as_deref().unwrap()).to_string();
    assert!(reg.worktrees.iter().any(|w| w.slug == nm.slug) && !ls.entries.is_empty(), "{reg:?}");
    assert!(body.contains(&format!("head {head}")), "docs/plan.md carries the published head: {body}");
    step("browse", t, format!("registry {} worktree(s), ls docs/ {} entries, get docs/plan.md ok", reg.worktrees.len(), ls.entries.len()));

    let t = Instant::now();
    let dest = tempfile::tempdir().unwrap();
    let fr = fetch_tree(be, &nm.wb, dest.path(), Select::All, false).unwrap();
    assert_eq!((fr.written.len(), fr.missing.len(), fr.refused.len()), (m.files.len(), 0, 0), "{fr:?}");
    let plan = std::fs::read_to_string(dest.path().join("docs/plan.md")).unwrap();
    assert!(plan.contains(&format!("head {head}")));
    let mut detail = format!("{} files written, 0 missing, 0 refused", fr.written.len());
    if m.has_bundle {
        let out = dest.path().join(".wt-branch.bundle");
        let got_head = fetch_bundle(be, &nm.wb, &out).unwrap();
        assert_eq!(got_head, m.head_commit, "the bundle carries the manifest's head");
        let bytes = std::fs::read(&out).unwrap();
        assert!(!bytes.is_empty());
        if let Some(b) = bundle.filter(|b| b.is_file()) {
            assert_eq!(bytes, std::fs::read(b).unwrap(), "the bundle came back byte for byte");
            detail.push_str(&format!("; bundle {} bytes identical to {}", bytes.len(), b.display()));
        } else {
            detail.push_str(&format!("; bundle {} bytes (no local copy to compare)", bytes.len()));
        }
    } else {
        detail.push_str("; no bundle in this manifest");
    }
    step("fetch", t, detail);

    refusals(be, nm, &m);
    Seen { polls, seen_ms }
}

/// The eight writes a read-only key must be refused. Each must fail with an access error, not with some other error (a missing
/// object, a bad state or a transport failure would hide a write that was merely not attempted).
fn refusals(be: &RustfsBackend, nm: &Names, m: &TreeManifest) {
    let t = Instant::now();
    let mut done = vec![];
    let mut must = |what: &str, r: Result<(), S2Error>| {
        let e = r.err().unwrap_or_else(|| panic!("the follower key was ALLOWED to: {what}"));
        assert!(denied(&e), "{what}: refused, but not with an access error: {e}");
        done.push(format!("{what}: {}", head_of(&e)));
    };
    must("put into the worktree bucket", ObjectBackend::put(be, &nm.wb, "scratch/x", b"x").map(|_| ()));
    must("write the handoff record", ObjectBackend::put(be, &nm.wb, HANDOFF_KEY, b"{}").map(|_| ()));
    must("write a registry entry", ObjectBackend::put(be, &nm.pb, "worktrees/forged.json", b"{}").map(|_| ()));
    must("write the digest", ObjectBackend::put(be, &nm.pb, DIGEST_KEY, b"{}").map(|_| ()));
    let victim = m.object_key(m.files.iter().find(|f| f.path == "src.rs").expect("src.rs in the manifest"));
    must("delete a tree object", ObjectBackend::delete(be, &nm.wb, &victim));
    let (dir, paths) = make_tree("zz-follower", "follower");
    let src = TreeSource {
        root: dir.path(), paths: &paths, uncommitted: &[], changes: &[], changes_recorded: false, branch: &nm.slug, base_commit: "base",
        head_commit: "zz-follower", bundle: None, writer: who("follower", "m9"),
    };
    must("publish_tree", publish_tree(be, &nm.wb, &src, &Limits::default(), now_ms()).map(|_| ()));
    must("claim_merge", claim_merge(be, &nm.wb, &who("follower", "m9"), now_ms()).map(|_| ()));
    let e = AdminBackend::create_bucket(be, "t-follower-made", &[]).err().unwrap_or_else(|| panic!("the follower key was ALLOWED to create a bucket"));
    assert!(matches!(e, BackendError::Forbidden(_)), "create a bucket: refused, but not with an access error: {e}");
    done.push(format!("create a bucket: {}", head_of(&e)));
    assert_eq!(done.len(), 8);
    // the object a refused delete named is still there
    assert!(ObjectBackend::head(be, &nm.wb, &victim).unwrap().is_some(), "the refused delete left the object");
    for (i, d) in done.iter().enumerate() {
        println!("FH|follower|refused-{}|PASS|0 ms|{d}", i + 1);
    }
    step("refuse-8-writes", t, "8 of 8 refused with an access error, the targeted object intact");
}

#[test]
#[ignore = "the writer half of the two-machine drill: WEFTOS_FH_* variables, a throwaway RustFS"]
fn writer_half() {
    let (be, _cfg, _d) = backend_from("WEFTOS_FH_WRITER_ENV");
    let round = env_num("WEFTOS_FH_ROUND", 1);
    let bundle = std::env::var("WEFTOS_FH_BUNDLE").ok().filter(|v| !v.is_empty()).map(PathBuf::from);
    writer_round(&be, &names_from_env(), round, bundle.as_deref());
}

#[test]
#[ignore = "the follower half of the two-machine drill: WEFTOS_FH_* variables, a throwaway RustFS, a read-only key"]
fn follower_half() {
    let (be, _cfg, _d) = backend_from("WEFTOS_FH_FOLLOWER_ENV");
    let expect = std::env::var("WEFTOS_FH_EXPECT_HEAD").ok().filter(|v| !v.is_empty());
    let bundle = std::env::var("WEFTOS_FH_BUNDLE").ok().filter(|v| !v.is_empty()).map(PathBuf::from);
    let s = follower_round(
        &be, &names_from_env(), Path::new(&env_req("WEFTOS_FH_STATE")), expect.as_deref(), env_num("WEFTOS_FH_WAIT_MS", 0), env_num("WEFTOS_FH_POLL_MS", 2000),
        bundle.as_deref(),
    );
    println!("DETAIL|fh|follower saw the head after {} poll(s) at {} ms (local clock)", s.polls, s.seen_ms);
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

/// Mints a writer key (RW on both buckets) and a read-only follower key on a throwaway RustFS (`WEFTOS_STORE_TEST_RUSTFS_ENV`, an
/// admin key), then: writer round 1, follower, writer round 2, follower again (the head moved and is seen within one poll). The
/// buckets and keys it made are removed at the end.
#[test]
#[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV (an admin key)"]
fn both_halves_on_one_machine() {
    let f = env_req("WEFTOS_STORE_TEST_RUSTFS_ENV");
    let d = tempfile::tempdir().unwrap();
    let cfg = RustfsConfig::from_env_file(Path::new(&f), d.path().join("secrets")).unwrap();
    let root = RustfsBackend::new(cfg.clone()).unwrap();
    let u = ulid::Ulid::new().to_string().to_lowercase();
    let nm = Names {
        pb: StoreName::project("lab", &u).unwrap().bucket(),
        wb: StoreName::worktree("lab", &u, "feat-fh").unwrap().bucket(),
        slug: "feat-fh".into(),
    };
    for b in [&nm.pb, &nm.wb] {
        make_bucket(&root, b);
    }
    let (kw, kf) = (root.create_access_key(&format!("fh-writer:{u}")).unwrap(), root.create_access_key(&format!("fh-follower:{u}")).unwrap());
    root.put_policy(&kw.id, &policy(&[&nm.pb, &nm.wb], &[])).unwrap();
    root.put_policy(&kf.id, &policy(&[], &[&nm.pb, &nm.wb])).unwrap();
    let (writer, follower) = (as_key(&cfg, &root, &kw.id), as_key(&cfg, &root, &kf.id));
    let bundle = d.path().join("b.bundle");
    std::fs::write(&bundle, b"# v2 git bundle\n".iter().cycle().take(64 << 10).copied().collect::<Vec<u8>>()).unwrap();
    let state = d.path().join("follow-state.json");

    let p1 = writer_round(&writer, &nm, 1, Some(&bundle));
    let s1 = follower_round(&follower, &nm, &state, Some("h1"), 0, 0, Some(&bundle));
    println!("DETAIL|fh|round 1: registry refreshed at {p1} ms, follower saw it at {} ms after {} poll(s)", s1.seen_ms, s1.polls);
    assert_eq!(s1.polls, 1);
    let p2 = writer_round(&writer, &nm, 2, Some(&bundle));
    let s2 = follower_round(&follower, &nm, &state, Some("h2"), 0, 0, Some(&bundle));
    assert_eq!(s2.polls, 1, "the head moved and the first poll after the refresh sees it");
    println!("DETAIL|fh|round 2: publish-to-seen {} ms, {} poll(s)", s2.seen_ms.saturating_sub(p2), s2.polls);

    // cleanup: the buckets (as root) and the keys
    for b in [&nm.wb, &nm.pb] {
        wipe(&root, &[b]);
    }
    for k in [&kw, &kf] {
        root.delete_access_key(&k.id).unwrap();
    }
    assert!(!AdminBackend::bucket_exists(&root, &nm.wb).unwrap() && !AdminBackend::bucket_exists(&root, &nm.pb).unwrap());
    println!("DETAIL|fh|buckets and both keys removed");
}
