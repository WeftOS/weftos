//! `project add` when the project's root is not itself a repository: the
//! fetcher picks the repository (or repositories) the primary lists.

use super::tests::{ULID, MESH, Rig, g, mesh_names, rig, w, commit};
use super::*;
use crate::project_install::{InstallRequest, PrimaryRef};

fn req(r: &Rig) -> InstallRequest {
    InstallRequest {
        project_ulid: ULID.into(),
        target_path: Some(r.dest().to_string_lossy().into_owned()),
        slug: None,
        sources: vec![],
        primary: Some(PrimaryRef { node_id: r.host_id.clone() }),
    }
}

/// The primary's root is the project user's home: no `.git` there, `tools` is the repo.
async fn headless_root() -> (Rig, MeshFetcher) {
    let r = rig().await;
    r.peers("pinned");
    r.grant(&[ULID]);
    std::fs::remove_dir_all(r.root.join(".git")).unwrap();
    let f = MeshFetcher::with_plane(r.plane().await.unwrap(), None).with_mesh(mesh_names());
    (r, f)
}

#[tokio::test]
async fn a_sole_repository_below_a_plain_root_is_checked_out_at_the_target() {
    let (r, f) = headless_root().await;
    let dest = r.dest();
    let rep = f.fetch(&req(&r), &dest).await.unwrap();
    assert_eq!(rep.repos.iter().map(|x| x.dir.as_str()).collect::<Vec<_>>(), vec!["tools"]);
    assert_eq!(std::fs::read_to_string(dest.join("run.sh")).unwrap(), "echo hi\n");
    assert_eq!(g(&dest, &["config", "remote.origin.url"]), format!("weftos://{MESH}/projects/{ULID}/repos/tools"));
    assert!(rep.warnings.iter().any(|w| w.contains("repository is `tools`")), "{:?}", rep.warnings);
}

#[tokio::test]
async fn several_repositories_below_a_plain_root_sit_side_by_side() {
    let (r, f) = headless_root().await;
    let docs = r.root.join("docs");
    w(&docs.join("a.md"), "a\n");
    g(&docs, &["init", "-q", "-b", "main"]);
    commit(&docs, "docs");
    let dest = r.dest();
    let rep = f.fetch(&req(&r), &dest).await.unwrap();
    assert_eq!(rep.repos.iter().map(|x| x.dir.as_str()).collect::<Vec<_>>(), vec!["docs", "tools"]);
    assert!(dest.join("tools/run.sh").exists() && dest.join("docs/a.md").exists());
    assert!(rep.warnings.iter().any(|w| w.contains("side by side") && w.contains("docs, tools")), "{:?}", rep.warnings);
}

#[tokio::test]
async fn repo_picks_one_even_when_the_root_is_a_repository() {
    let r = rig().await;
    r.peers("pinned");
    r.grant(&[ULID]);
    let f = MeshFetcher::with_plane(r.plane().await.unwrap(), None).with_mesh(mesh_names()).with_repo(Some("tools"));
    let dest = r.dest();
    let rep = f.fetch(&req(&r), &dest).await.unwrap();
    assert_eq!(rep.repos.iter().map(|x| x.dir.as_str()).collect::<Vec<_>>(), vec!["tools"]);
    assert!(dest.join("run.sh").exists());
    assert_eq!(g(&dest, &["config", "remote.origin.url"]), format!("weftos://{MESH}/projects/{ULID}/repos/tools"));
}

#[tokio::test]
async fn repo_naming_a_missing_repository_says_what_the_project_has() {
    let (r, f) = headless_root().await;
    let f = f.with_repo(Some("nope"));
    let e = f.fetch(&req(&r), &r.dest()).await.unwrap_err();
    assert!(e.contains("no repository `nope`") && e.contains("it has: tools"), "{e}");
    assert!(!r.dest().exists() || std::fs::read_dir(r.dest()).unwrap().next().is_none(), "nothing fetched");
}

#[test]
fn a_call_naming_a_dir_goes_under_that_repositorys_name() {
    let base = format!("weftos://{MESH}/projects/{ULID}");
    let ch = client::DaemonChannel::new(base.clone(), None);
    assert_eq!(ch.uri_for(&json!({"op": "list"})), base);
    assert_eq!(ch.uri_for(&json!({"op": "refs", "dir": "."})), base);
    assert_eq!(ch.uri_for(&json!({"op": "refs", "dir": "Shasta"})), format!("{base}/repos/Shasta"));
}
