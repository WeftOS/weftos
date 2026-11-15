//! S2 worktree tests: fixtures shared by the `s2_worktree*` test binaries. Test support only.
#![allow(dead_code)]
use weftos_store::overlay::*;
use weftos_store::promote::*;
use weftos_store::share::*;
use weftos_store::testing::MemBackend;

pub const WT: &str = "w-01abc-aaaaaaaaaaaa";
pub const PR: &str = "p-01abc";
pub const AR: &str = "p-01abc"; // archive lives in the project bucket under a prefix

pub fn me(a: &str, h: &str) -> Actor {
    Actor {
        agent: a.into(),
        host: h.into(),
    }
}

/// The object the current manifest names for `path` (schema 2: a pool key, lane D3).
pub fn obj(b: &MemBackend, path: &str) -> String {
    let m = read_manifest(b, WT).unwrap().unwrap();
    m.object_key(m.files.iter().find(|f| f.path == path).unwrap())
}

/// The bundle object the current manifest names.
pub fn bundle_obj(b: &MemBackend) -> String {
    read_manifest(b, WT).unwrap().unwrap().bundle_object().unwrap()
}

pub fn setup() -> MemBackend {
    let b = MemBackend::new();
    b.set_clock(1_000);
    b.put(PR, "assets/logo.png", b"logo-v1").unwrap();
    b.put(PR, "assets/old.txt", b"old").unwrap();
    b.set_clock(2_000);
    write_meta(
        &b,
        WT,
        &WorktreeMeta {
            slug: "s2".into(),
            project: "01abc".into(),
            branch: "feat/x".into(),
            base_commit: "aaa".into(),
            created_ms: 2_000,
        },
    )
    .unwrap();
    b
}

pub fn policy() -> PromotionPolicy {
    PromotionPolicy::default()
}
pub const ARCH: ArchiveTarget<'static> = ArchiveTarget {
    bucket: AR,
    prefix: "archive/worktrees/s2/",
};

pub fn tree_dir() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    let w = |p: &str, c: &str| {
        let f = d.path().join(p);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, c).unwrap();
    };
    w(
        "docs/grok-build-fork/rooms.md",
        "# Rooms and doors contract\nbody",
    );
    w(".planning/plan.md", "# Plan\n");
    w("src/lib.rs", "fn main(){}");
    w(".env", "TOKEN=1");
    w("keys/server.pem", "pem");
    d
}

pub fn paths(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}
