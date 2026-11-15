//! WO-069 lane B: the dashboard browse reader (`follower::browse_*`) on the in-memory backend and on
//! S1's real `FsBackend`: listing, capped get, every refusal, the metadata cross-check, and a
//! recording backend that proves nothing outside the registry entry, meta, manifest and tree is read.
use std::cell::RefCell;
use weftos_store::FsBackend;
use weftos_store::backend::AdminBackend;
use weftos_store::follower::*;
use weftos_store::follower_browse::{EntryKind, MAX_GET_BYTES};
use weftos_store::overlay::{ObjectBackend, ObjectMeta, Result as R};
use weftos_store::promote::{WorktreeMeta, write_meta};
use weftos_store::share::*;
use weftos_store::share_fetch::*;
use weftos_store::testing::MemBackend;

const PR: &str = "p-01abc";

fn actor() -> Actor {
    Actor { agent: "w".into(), host: "h".into() }
}

/// Publish `slug` with `files`, mark `uncommitted`, and register it.
fn publish<B: ObjectBackend>(b: &B, slug: &str, files: &[(&str, &[u8])], uncommitted: &[String]) -> RegistryEntry {
    let d = tempfile::tempdir().unwrap();
    let mut ps = vec![];
    for (p, c) in files {
        let f = d.path().join(p);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, c).unwrap();
        ps.push(p.to_string());
    }
    let bucket = format!("w-{slug}");
    let src = TreeSource {
        root: d.path(),
        paths: &ps,
        uncommitted,
        changes: &[],
        changes_recorded: false,
        branch: slug,
        base_commit: "base",
        head_commit: "head1",
        bundle: None,
        writer: actor(),
    };
    let meta = WorktreeMeta { slug: slug.into(), project: "01abc".into(), branch: slug.into(), base_commit: "base".into(), created_ms: 0 };
    write_meta(b, &bucket, &meta).unwrap();
    publish_tree(b, &bucket, &src, &Limits::default(), 5_000).unwrap();
    let e = summarize(b, slug, &bucket).unwrap().unwrap();
    write_registry_entry(b, PR, &e).unwrap();
    e
}

const BIG: usize = (MAX_GET_BYTES as usize) + 1;

fn scenario<B: ObjectBackend>(b: &B) {
    let big = vec![b'z'; BIG];
    publish(
        b,
        "rooms",
        &[
            ("docs/planning/rooms.md", b"# Rooms plan\nbody\n"),
            ("docs/readme.md", b"# Readme\n"),
            ("src/lib.rs", b"pub fn a() {}\n"),
            ("top.txt", b"top"),
            ("big.bin", &big),
        ],
        &["docs/planning/rooms.md".to_string()],
    );
    happy_path(b);
    caps(b);
    refusals(b);
    registry_list(b);
}

fn happy_path<B: ObjectBackend>(b: &B) {
    let root = browse_ls(b, PR, "rooms", "").unwrap();
    assert_eq!((root.branch.as_str(), root.head.as_str(), root.state), ("rooms", "head1", State::Open));
    assert_eq!((root.writer.as_str(), root.published_ms, root.file_count), ("w@h", 5_000, 5));
    let names: Vec<_> = root.entries.iter().map(|e| (e.name.as_str(), e.kind)).collect();
    assert_eq!(
        names,
        [("docs", EntryKind::Dir), ("src", EntryKind::Dir), ("big.bin", EntryKind::File), ("top.txt", EntryKind::File)],
        "folders first, one level only"
    );
    assert_eq!(root.entries[0].files, 2);
    let docs = browse_ls(b, PR, "rooms", "docs/").unwrap();
    assert_eq!(docs.path, "docs");
    assert_eq!(docs.entries.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(), ["docs/planning", "docs/readme.md"]);
    let pl = browse_ls(b, PR, "rooms", "docs/planning").unwrap();
    let f = &pl.entries[0];
    assert_eq!((f.path.as_str(), f.title.as_deref(), f.uncommitted), ("docs/planning/rooms.md", Some("Rooms plan"), true));
    assert_eq!(f.sha256.as_deref(), Some(sha256_hex(b"# Rooms plan\nbody\n").as_str()));
    let got = browse_get(b, PR, "rooms", "docs/planning/rooms.md", 1 << 20).unwrap();
    assert_eq!(got.data.as_deref(), Some(&b"# Rooms plan\nbody\n"[..]));
    assert_eq!((got.withheld, got.title.as_deref(), got.uncommitted), (false, Some("Rooms plan"), true));
}

fn caps<B: ObjectBackend>(b: &B) {
    let w = browse_get(b, PR, "rooms", "docs/readme.md", 4).unwrap();
    assert!(w.withheld && w.data.is_none());
    assert_eq!((w.size, w.cap, w.withheld_reason.as_deref()), (9, 4, Some("over_cap")));
    assert_eq!(w.sha256, sha256_hex(b"# Readme\n"));
    assert!(!browse_get(b, PR, "rooms", "docs/readme.md", 9).unwrap().withheld, "at the cap is allowed");
    // The hard bound wins over a larger ask.
    let big = browse_get(b, PR, "rooms", "big.bin", u64::MAX).unwrap();
    assert!(big.withheld && big.data.is_none() && big.cap == MAX_GET_BYTES && big.size == BIG as u64);
}

fn refusals<B: ObjectBackend>(b: &B) {
    for bad in [
        "../x", "/etc/passwd", "docs/../src/lib.rs", "docs//readme.md", ".git/config", ".wt/meta.json", ".wt/handoff.json",
        ".wt/git/branch.bundle", ".wt/tree.manifest.json", ".wt/tree/docs/readme.md", "docs/.wt/x", "a\\b", "x\0y", "", "docs/", ".env",
        "src/lib.rs/", "nope.txt", "docs", "COM1", "a:b",
    ] {
        assert!(browse_get(b, PR, "rooms", bad, 1 << 20).is_err(), "get {bad:?} must be refused");
    }
    for bad in ["..", "/", "../x", ".git", ".wt", ".wt/git", ".wt/base/", "docs/../.wt", "src/lib.rs", "missing/", ".kube"] {
        assert!(browse_ls(b, PR, "rooms", bad).is_err(), "ls {bad:?} must be refused");
    }
    for bad in ["", "../x", "UP", "a/b", "-x", &"x".repeat(49), "ghost"] {
        assert!(browse_ls(b, PR, bad, "").is_err(), "slug {bad:?} must be refused");
        assert!(browse_get(b, PR, bad, "top.txt", 9).is_err());
    }
}

fn registry_list<B: ObjectBackend>(b: &B) {
    publish(b, "other", &[("docs/a.md", b"# A\n")], &[]);
    // One corrupt entry and one that names another worktree: reported, the list and browse survive.
    b.put(PR, "worktrees/broken.json", b"{not json").unwrap();
    let swapped = serde_json::to_vec(&RegistryEntry { slug: "liar".into(), ..list_shared(&MemOnly::of(b, "other"), PR).unwrap()[0].clone() }).unwrap();
    b.put(PR, "worktrees/mislabel.json", &swapped).unwrap();
    let r = browse_registry(b, PR).unwrap();
    let slugs: Vec<_> = r.worktrees.iter().map(|w| w.slug.as_str()).collect();
    assert_eq!(slugs.len(), 2);
    assert!(slugs.contains(&"rooms") && slugs.contains(&"other"));
    assert_eq!(r.unreadable, ["broken", "mislabel"]);
    let w = r.worktrees.iter().find(|w| w.slug == "rooms").unwrap();
    assert_eq!((w.branch.as_str(), w.head.as_str(), w.state, w.doc_count, w.writer.as_str()), ("rooms", "head1", State::Open, 2, "w@h"));
    assert!(browse_ls(b, PR, "rooms", "").is_ok());
    assert!(browse_ls(b, PR, "broken", "").is_err());
    assert!(browse_ls(b, PR, "mislabel", "").is_err());
}

/// `list_shared` through a borrowed backend (the scenario is generic and has no clone).
struct MemOnly<'a, B>(&'a B);
impl<'a, B> MemOnly<'a, B> {
    fn of(b: &'a B, _: &str) -> Self {
        Self(b)
    }
}
impl<B: ObjectBackend> ObjectBackend for MemOnly<'_, B> {
    fn head(&self, b: &str, k: &str) -> R<Option<ObjectMeta>> { self.0.head(b, k) }
    fn get(&self, b: &str, k: &str) -> R<Option<Vec<u8>>> { self.0.get(b, k) }
    fn put(&self, b: &str, k: &str, d: &[u8]) -> R<ObjectMeta> { self.0.put(b, k, d) }
    fn delete(&self, b: &str, k: &str) -> R<()> { self.0.delete(b, k) }
    fn list(&self, b: &str, p: &str) -> R<Vec<ObjectMeta>> {
        // hide the corrupt fixtures so `list_shared` (strict) can read the good entries
        Ok(self.0.list(b, p)?.into_iter().filter(|m| !m.key.contains("broken") && !m.key.contains("mislabel")).collect())
    }
}

#[test]
fn browse_on_the_in_memory_backend() {
    scenario(&MemBackend::new());
}

#[test]
fn browse_on_the_real_fs_backend() {
    let d = tempfile::tempdir().unwrap();
    let be = FsBackend::open(d.path()).unwrap();
    for bkt in [PR, "w-rooms", "w-other"] {
        AdminBackend::create_bucket(&be, bkt, &[]).unwrap();
    }
    scenario(&be);
}

#[test]
fn a_slug_whose_meta_names_another_worktree_is_refused() {
    let b = MemBackend::new();
    publish(&b, "rooms", &[("a.txt", b"a")], &[]);
    assert!(browse_ls(&b, PR, "rooms", "").is_ok());
    let meta = WorktreeMeta { slug: "victim".into(), project: "01abc".into(), branch: "x".into(), base_commit: "b".into(), created_ms: 0 };
    write_meta(&b, "w-rooms", &meta).unwrap();
    let e = browse_ls(&b, PR, "rooms", "").unwrap_err().to_string();
    assert!(e.contains("bucket of worktree victim"), "{e}");
    assert!(browse_get(&b, PR, "rooms", "a.txt", 9).is_err());
    b.delete("w-rooms", ".wt/meta.json").unwrap();
    assert!(browse_ls(&b, PR, "rooms", "").is_err(), "no metadata, no browse");
}

#[test]
fn an_entry_pointing_at_the_wrong_bucket_is_refused_on_a_real_project() {
    let b = MemBackend::new();
    let ulid = "01HZZZZZZZZZZZZZZZZZZZZZZZ";
    let pb = format!("p-{}", ulid.to_lowercase());
    publish(&b, "rooms", &[("a.txt", b"a")], &[]);
    let mut e = summarize(&b, "rooms", "w-rooms").unwrap().unwrap();
    e.bucket = "w-rooms".into(); // not the bucket this project derives for `rooms`
    b.put(&pb, "worktrees/rooms.json", &serde_json::to_vec(&e).unwrap()).unwrap();
    assert!(browse_ls(&b, &pb, "rooms", "").is_err());
    assert_eq!(browse_registry(&b, &pb).unwrap().unreadable, ["rooms"]);
}

#[test]
fn a_hostile_manifest_cannot_widen_what_is_listed_or_read() {
    let b = MemBackend::new();
    publish(&b, "rooms", &[("docs/ok.md", b"# Ok\n")], &[]);
    let mut m = read_manifest(&b, "w-rooms").unwrap().unwrap();
    for (p, body) in [(".env", &b"K=V"[..]), ("../x", b"x"), ("a/.git/config", b"c"), ("sub/id_rsa", b"k"), (".wt/meta.json", b"m")] {
        // a well-formed schema-2 entry with its object in the pool: only the path makes it hostile
        let f = FileEntry { path: p.into(), size: body.len() as u64, sha256: sha256_hex(body), generation: Some(m.generation) };
        let _ = b.put("w-rooms", &m.object_key(&f), body);
        let _ = b.put("w-rooms", &format!("{TREE_PREFIX}{p}"), body);
        m.files.push(f);
    }
    b.put("w-rooms", MANIFEST_KEY, &serde_json::to_vec(&m).unwrap()).unwrap();
    b.put("w-rooms", ".wt/tree/planted.txt", b"not in manifest").unwrap();
    b.put("w-rooms", &obj_key(&sha256_hex(b"planted"), 1), b"planted").unwrap();
    let root = browse_ls(&b, PR, "rooms", "").unwrap();
    assert_eq!(root.entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["docs"]);
    for p in [".env", "../x", "a/.git/config", "sub/id_rsa", ".wt/meta.json", "planted.txt"] {
        assert!(browse_get(&b, PR, "rooms", p, 99).is_err(), "{p}");
    }
    // A manifest that lies about size is caught by the stored object's size, and a wrong hash fails.
    let mut m = read_manifest(&b, "w-rooms").unwrap().unwrap();
    m.files[0].size = 1;
    b.put("w-rooms", MANIFEST_KEY, &serde_json::to_vec(&m).unwrap()).unwrap();
    assert!(browse_get(&b, PR, "rooms", "docs/ok.md", 3).unwrap().withheld, "stored size 5 is over a 3 byte cap");
    m.files[0].sha256 = sha256_hex(b"other");
    b.put("w-rooms", MANIFEST_KEY, &serde_json::to_vec(&m).unwrap()).unwrap();
    assert!(browse_get(&b, PR, "rooms", "docs/ok.md", 99).is_err(), "checksum mismatch");
}

/// Wraps a backend and records every read.
struct Spy<'a> {
    inner: &'a MemBackend,
    reads: RefCell<Vec<(String, String)>>,
}
impl ObjectBackend for Spy<'_> {
    fn head(&self, b: &str, k: &str) -> R<Option<ObjectMeta>> { self.reads.borrow_mut().push((b.into(), k.into())); self.inner.head(b, k) }
    fn get(&self, b: &str, k: &str) -> R<Option<Vec<u8>>> { self.reads.borrow_mut().push((b.into(), k.into())); self.inner.get(b, k) }
    fn put(&self, b: &str, k: &str, d: &[u8]) -> R<ObjectMeta> { self.inner.put(b, k, d) }
    fn delete(&self, b: &str, k: &str) -> R<()> { self.inner.delete(b, k) }
    fn list(&self, b: &str, p: &str) -> R<Vec<ObjectMeta>> { self.reads.borrow_mut().push((b.into(), p.into())); self.inner.list(b, p) }
}

#[test]
fn browse_only_ever_reads_the_entry_meta_manifest_and_tree() {
    let m = MemBackend::new();
    publish(&m, "rooms", &[("docs/a.md", b"# A\n"), ("b.txt", b"b")], &[]);
    m.put("w-rooms", ".wt/handoff.json", b"{}").unwrap_or_else(|_| unreachable!());
    m.put("w-rooms", ".wt/git/branch.bundle", b"BUNDLE").unwrap();
    m.put("w-rooms", ".wt/base/x", b"base").unwrap();
    m.put("w-rooms", "overlay-file.txt", b"overlay").unwrap();
    let s = Spy { inner: &m, reads: RefCell::new(vec![]) };
    browse_ls(&s, PR, "rooms", "").unwrap();
    browse_ls(&s, PR, "rooms", "docs").unwrap();
    browse_get(&s, PR, "rooms", "docs/a.md", 99).unwrap();
    browse_get(&s, PR, "rooms", ".wt/git/branch.bundle", 99).unwrap_err();
    browse_get(&s, PR, "rooms", "overlay-file.txt", 99).unwrap_err();
    // the one tree object read is the pool object the manifest names for docs/a.md (schema 2)
    let man = read_manifest(&m, "w-rooms").unwrap().unwrap();
    let a = man.object_key(man.files.iter().find(|f| f.path == "docs/a.md").unwrap());
    assert!(a.starts_with(OBJ_PREFIX), "{a}");
    for (bucket, key) in s.reads.borrow().iter() {
        let ok = (bucket == PR && key == "worktrees/rooms.json")
            || (bucket == "w-rooms" && (key == ".wt/meta.json" || key == MANIFEST_KEY || *key == a));
        assert!(ok, "unexpected read of {bucket}/{key}");
    }
    assert!(s.reads.borrow().iter().any(|(_, k)| *k == a));
}
