//! S2 tests: what fetch and publish refuse (control files, secrets, case collisions, hostile names).
use weftos_store::overlay::*;
use weftos_store::share::*;
use weftos_store::share_fetch::*;

#[path = "s2_worktree_common/mod.rs"]
mod common;
use common::*;

fn manifest_with(files: &[(&str, &[u8])]) -> TreeManifest {
    TreeManifest {
        schema: 1,
        generation: 0,
        branch: "f".into(),
        base_commit: "a".into(),
        head_commit: "b".into(),
        files: files
            .iter()
            .map(|(p, d)| FileEntry {
                path: p.to_string(),
                size: d.len() as u64,
                sha256: sha256_hex(d),
                generation: None,
            })
            .collect(),
        uncommitted: vec![],
        changes: vec![],
        docs: vec![],
        has_bundle: false,
        bundle_sha256: None,
        changes_recorded: false,
        bundle_gen: None,
        retiring: vec![],
        v1_retired_ms: None,
        writer: me("w", "h"),
        published_ms: 1,
    }
}

#[test]
fn fetch_refuses_git_control_files_secrets_and_case_collisions() {
    let b = setup();
    let files: &[(&str, &[u8])] = &[
        (".git/hooks/post-checkout", b"#!/bin/sh\nevil"),
        (".gitattributes", b"x"),
        ("a/.envrc", b"x"),
        ("k/prod.env", b"x"),
        ("win/a:b", b"x"),
        ("Doc.md", b"one"),
        ("doc.md", b"two"),
        ("ok.txt", b"ok"),
    ];
    for (p, d) in files {
        b.put(WT, &format!("{TREE_PREFIX}{p}"), d).unwrap();
    }
    b.put(
        WT,
        MANIFEST_KEY,
        &serde_json::to_vec(&manifest_with(files)).unwrap(),
    )
    .unwrap();
    let dest = tempfile::tempdir().unwrap();
    let r = fetch_tree(&b, WT, dest.path(), Select::All, true).unwrap();
    assert_eq!(r.written, ["Doc.md", "ok.txt"]);
    assert_eq!(r.refused.len(), 6);
    assert!(!dest.path().join(".git").exists());
}

#[test]
fn fetch_temp_file_never_clobbers_neighbours() {
    let b = setup();
    let files: &[(&str, &[u8])] = &[("foo.rs", b"new")];
    b.put(WT, &format!("{TREE_PREFIX}foo.rs"), b"new").unwrap();
    b.put(
        WT,
        MANIFEST_KEY,
        &serde_json::to_vec(&manifest_with(files)).unwrap(),
    )
    .unwrap();
    let dest = tempfile::tempdir().unwrap();
    std::fs::write(dest.path().join("foo.wt-tmp"), "precious").unwrap();
    fetch_tree(&b, WT, dest.path(), Select::All, false).unwrap();
    assert_eq!(
        std::fs::read_to_string(dest.path().join("foo.wt-tmp")).unwrap(),
        "precious"
    );
    assert_eq!(
        std::fs::read_to_string(dest.path().join("foo.rs")).unwrap(),
        "new"
    );
}

#[cfg(unix)]
#[test]
fn publish_does_not_follow_symlinked_parent_directories() {
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("passwd"), "root:x").unwrap();
    let d = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), d.path().join("docs")).unwrap();
    let b = setup();
    let ps = paths(&["docs/passwd"]);
    let src = TreeSource {
        root: d.path(),
        paths: &ps,
        uncommitted: &[],
        changes: &[],
        changes_recorded: false,
        branch: "f",
        base_commit: "a",
        head_commit: "b",
        bundle: None,
        writer: me("w", "h"),
    };
    let r = publish_tree(&b, WT, &src, &Limits::default(), 1).unwrap();
    assert_eq!(r.uploaded, 0);
    assert!(b.get(WT, ".wt/tree/docs/passwd").unwrap().is_none());
}

#[test]
fn secret_name_gaps_are_closed_but_templates_pass() {
    for p in [
        "prod.env",
        ".envrc",
        ".git-credentials",
        ".pgpass",
        "id_ecdsa",
        "a.jks",
        "terraform.tfstate",
        ".htpasswd",
        "x/.env.local",
    ] {
        assert!(is_secret_path(p), "{p}");
    }
    for p in [".env.example", "docs/secrets-policy.md", "src/main.rs"] {
        assert!(!is_secret_path(p), "{p}");
    }
}

#[test]
fn more_secret_names_are_refused_and_lookalikes_pass() {
    for p in [
        "config/prod.env.bak",
        "config/PROD.ENV.OLD",
        "terraform.tfvars",
        "infra/prod.auto.tfvars",
        "home/.kube/config",
        ".kube/cache/token",
        ".docker/config.json",
        "x/.docker/config.json",
        ".pypirc",
        "ci/deploy.token",
        "store-secrets/wk-abc123",
        "run/store-secrets/wk-abc123.json",
    ] {
        assert!(is_secret_path(p), "{p}");
    }
    for p in [
        "docs/kube-config.md",
        "src/docker/config.rs",
        "src/token.rs",
        "environment.backup",
        "config/prod.env.example",
        "README.md",
        "src/store_keys.rs",
    ] {
        assert!(!is_secret_path(p), "{p}");
    }
}
