//! Handler tests: a fake fetcher and a fake `weft` for install (London school),
//! real git and a real manifest store for update/remove. Temp dirs only; the
//! real `~/.weftos` and `~/.clawft` are never touched.

use std::path::Path;

use clawft_types::project::{adopt_workspace, find_by_id};
use serde_json::{Value, json};

use super::*;
use crate::project_install_test_support::*;

struct Fx {
    _t: tempfile::TempDir,
    home: PathBuf,
    bin: PathBuf,
    env: InstallEnv,
}

fn fx(weft_exit: i32) -> Fx {
    let t = tempfile::tempdir().unwrap();
    let home = t.path().join("home");
    let bin = t.path().join("bin");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    let weft = fake_weft(&bin, weft_exit);
    let run = t.path().join("run");
    std::fs::create_dir_all(&run).unwrap();
    let env = InstallEnv {
        manifests_dir: home.join(".weftos/projects"),
        home: home.clone(),
        weft: Some(weft),
        git: test_runner(),
        pair_requests_dir: Some(run),
    };
    Fx { _t: t, home, bin, env }
}

fn action(kind: &str, payload: Value) -> Action {
    Action { id: "a1".into(), kind: kind.into(), payload, created_at: None }
}

fn install_payload(target: &str, sources: Value) -> Value {
    json!({ "project_ulid": ULID, "target_path": target, "slug": "demo", "sources": sources })
}

fn handler(f: &Fx, fetcher: Arc<FakeFetcher>) -> InstallHandler {
    InstallHandler::new(f.env.clone(), vec![fetcher])
}

const TWO: fn() -> Value = || json!([{"url": "https://example.org/a.git", "dir": "."}, {"url": "https://example.org/b.git", "dir": "tools", "branch": "main"}]);

#[tokio::test]
async fn install_with_root_and_sibling_adopts_with_repo_args() {
    let f = fx(0);
    let fetcher = Arc::new(FakeFetcher::new(false));
    let out = handler(&f, fetcher.clone()).handle(&action("install", install_payload("~/Projects/demo", TWO()))).await;
    assert_eq!(out.status, "succeeded", "{}", out.result);
    let root = f.home.join("Projects/demo");
    assert_eq!(out.result["fetcher"], "git-remote");
    assert_eq!(Path::new(out.result["root"].as_str().unwrap()), root);
    assert_eq!(out.result["repos"].as_array().unwrap().len(), 2);
    assert_eq!(out.result["bytes"], 0);
    assert_eq!(out.result["archived"], json!([]));
    assert_eq!(*fetcher.calls.lock().unwrap(), 1);

    let log = weft_log(&f.bin);
    assert_eq!(Path::new(&log[0]).canonicalize().unwrap(), root.canonicalize().unwrap(), "adopt runs in the root");
    let sibling = f.home.join("Projects/tools");
    assert_eq!(&log[1..], ["project", "init", "--adopt", ULID, "--name", "demo", "--repo", sibling.to_str().unwrap()]);
}

#[tokio::test]
async fn install_without_a_root_source_passes_no_repo_args() {
    let f = fx(0);
    let sources = json!([{"url": "https://example.org/a.git", "dir": "one"}, {"url": "https://example.org/b.git", "dir": "two"}]);
    let out = handler(&f, Arc::new(FakeFetcher::new(false))).handle(&action("install", install_payload("~/Projects/demo", sources))).await;
    assert_eq!(out.status, "succeeded", "{}", out.result);
    assert!(f.home.join("Projects/demo/one/f.txt").is_file());
    let log = weft_log(&f.bin);
    assert!(!log.contains(&"--repo".to_owned()), "{log:?}");
}

#[tokio::test]
async fn a_non_empty_destination_is_refused_and_nothing_is_fetched() {
    let f = fx(0);
    let target = f.home.join("Projects/demo");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(target.join("mine.txt"), "keep").unwrap();
    let fetcher = Arc::new(FakeFetcher::new(false));
    let out = handler(&f, fetcher.clone()).handle(&action("install", install_payload("~/Projects/demo", TWO()))).await;
    assert_eq!(out.status, "failed");
    assert!(out.result["error"].as_str().unwrap().contains("not empty"));
    assert_eq!(*fetcher.calls.lock().unwrap(), 0);
    assert!(target.join("mine.txt").is_file());
    assert!(weft_log(&f.bin).is_empty(), "adopt must not run");

    // A non-empty sibling destination is refused too.
    let other = fx(0);
    std::fs::create_dir_all(other.home.join("Projects/tools")).unwrap();
    std::fs::write(other.home.join("Projects/tools/x"), "1").unwrap();
    let out = handler(&other, Arc::new(FakeFetcher::new(false))).handle(&action("install", install_payload("~/Projects/demo", TWO()))).await;
    assert_eq!(out.status, "failed");
    assert!(!other.home.join("Projects/demo").exists());
}

#[tokio::test]
async fn a_failed_fetch_leaves_nothing_behind() {
    let f = fx(0);
    let out = handler(&f, Arc::new(FakeFetcher::new(true))).handle(&action("install", install_payload("~/Projects/demo", TWO()))).await;
    assert_eq!(out.status, "failed");
    assert_eq!(out.result["error"], "boom");
    assert!(!f.home.join("Projects").exists(), "root, sibling and created parents are gone");
    assert!(weft_log(&f.bin).is_empty());
}

#[tokio::test]
async fn a_failed_adopt_rolls_the_checkout_back() {
    let f = fx(1);
    let out = handler(&f, Arc::new(FakeFetcher::new(false))).handle(&action("install", install_payload("~/Projects/demo", TWO()))).await;
    assert_eq!(out.status, "failed");
    let e = out.result["error"].as_str().unwrap();
    assert!(e.contains("registering the workspace failed") && e.contains("adopt said no"), "{e}");
    assert!(!f.home.join("Projects").exists());
}

#[tokio::test]
async fn malformed_payloads_fail_without_echoing_them() {
    let f = fx(0);
    let h = handler(&f, Arc::new(FakeFetcher::new(false)));
    let bad = [
        json!(null),
        json!("secret-looking-string"),
        json!({"project_ulid": ULID, "target_path": "~/p", "sources": [], "extra": "leak-me"}),
        json!({"project_ulid": "nope", "sources": [{"url": "https://example.org/a.git"}], "slug": "d"}),
        json!({"project_ulid": ULID, "target_path": "~/p", "sources": [{"url": "https://user:pw@example.org/a.git"}]}),
        json!({"project_ulid": ULID, "target_path": "/etc/elsewhere", "sources": [{"url": "https://example.org/a.git"}]}),
        json!({"project_ulid": ULID, "target_path": "~/p", "sources": []}),
    ];
    for p in bad {
        let out = h.handle(&action("install", p.clone())).await;
        assert_eq!(out.status, "failed", "{p}");
        let text = out.result.to_string();
        assert!(!text.contains("leak-me") && !text.contains("secret-looking") && !text.contains("pw@"), "{text}");
    }
    assert_eq!(*FakeFetcher::new(false).calls.lock().unwrap(), 0);
}

#[tokio::test]
async fn no_usable_fetcher_fails_cleanly() {
    let f = fx(0);
    let mesh_only = InstallHandler::new(f.env.clone(), vec![]);
    let out = mesh_only.handle(&action("install", install_payload("~/p", TWO()))).await;
    assert_eq!(out.status, "failed");
    assert!(out.result["error"].as_str().unwrap().contains("no fetcher"));
    let run = f.env.pair_requests_dir.as_ref().unwrap();
    assert!(crate::mesh_pair_requests::list(run).unwrap().is_empty(), "no primary named: nothing to pair with");
}

#[tokio::test]
async fn no_usable_fetcher_with_a_primary_records_a_pair_request() {
    let f = fx(0);
    let primary = "0123456789abcdef0123456789abcdef";
    let mesh_only = InstallHandler::new(f.env.clone(), vec![]);
    let mut payload = install_payload("~/p", TWO());
    payload["primary"] = json!({ "node_id": primary });
    let out = mesh_only.handle(&action("install", payload.clone())).await;
    assert_eq!(out.status, "failed");
    let err = out.result["error"].as_str().unwrap();
    assert!(err.contains("no fetcher") && err.contains("pair request") && err.contains(primary), "{err}");
    let run = f.env.pair_requests_dir.as_ref().unwrap();
    let reqs = crate::mesh_pair_requests::list(run).unwrap();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].with_node, primary);
    assert_eq!(reqs[0].projects, vec![ULID.to_owned()]);
    assert!(err.contains(&reqs[0].request_id));
    assert!(!f.home.join("p").exists(), "nothing fetched");

    // A retry before approval does not pile up requests.
    mesh_only.handle(&action("install", payload)).await;
    assert_eq!(crate::mesh_pair_requests::list(run).unwrap().len(), 1);
}

#[tokio::test]
async fn a_missing_weft_binary_fails_before_fetching() {
    let mut f = fx(0);
    f.env.weft = Some(f.home.join("no-such-weft"));
    let fetcher = Arc::new(FakeFetcher::new(false));
    let out = handler(&f, fetcher.clone()).handle(&action("install", install_payload("~/Projects/demo", TWO()))).await;
    assert_eq!(out.status, "failed");
    assert!(!f.home.join("Projects").exists());
}

/// A registered workspace cloned from a temp bare repo; returns (bare, root).
fn registered_workspace(f: &Fx) -> (PathBuf, PathBuf) {
    let base = f.home.join("srv");
    std::fs::create_dir_all(&base).unwrap();
    let bare = bare_repo(&base, "a", "a.txt");
    let root = f.home.join("Projects/demo");
    git(&f.home, &["clone", "-q", bare.to_str().unwrap(), root.to_str().unwrap()]);
    adopt_workspace(&root, &f.env.manifests_dir, ULID, Some("demo"), &[]).unwrap();
    (bare, root)
}

fn push_new_commit(f: &Fx, bare: &Path) {
    let other = f.home.join("other-clone");
    git(&f.home, &["clone", "-q", bare.to_str().unwrap(), other.to_str().unwrap()]);
    std::fs::write(other.join("new.txt"), "n").unwrap();
    git(&other, &["add", "."]);
    git(&other, &["commit", "-q", "-m", "second"]);
    git(&other, &["push", "-q", "origin", "main"]);
}

#[tokio::test]
async fn update_pulls_fast_forward_and_reports_the_new_head() {
    let f = fx(0);
    let (bare, root) = registered_workspace(&f);
    push_new_commit(&f, &bare);
    let out = UpdateHandler { env: f.env.clone() }.handle(&action("update", json!({"project_ulid": ULID}))).await;
    assert_eq!(out.status, "succeeded", "{}", out.result);
    assert!(root.join("new.txt").is_file());
    assert_eq!(out.result["repos"][0]["updated"], true);
    assert_eq!(out.result["repos"][0]["path"], ".");

    let again = UpdateHandler { env: f.env.clone() }.handle(&action("update", json!({"project_ulid": ULID}))).await;
    assert_eq!(again.result["repos"][0]["updated"], false);
}

#[tokio::test]
async fn update_that_cannot_fast_forward_fails_and_keeps_local_work() {
    let f = fx(0);
    let (bare, root) = registered_workspace(&f);
    push_new_commit(&f, &bare);
    std::fs::write(root.join("local.txt"), "l").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "local"]);
    let out = UpdateHandler { env: f.env.clone() }.handle(&action("update", json!({"project_ulid": ULID}))).await;
    assert_eq!(out.status, "failed");
    assert!(out.result["repos"][0]["error"].is_string());
    assert!(root.join("local.txt").is_file());
}

#[tokio::test]
async fn update_and_remove_reject_bad_payloads_and_unknown_projects() {
    let f = fx(0);
    let u = UpdateHandler { env: f.env.clone() };
    let r = RemoveHandler { env: f.env.clone() };
    for p in [json!(null), json!({}), json!({"project_ulid": "x"}), json!({"project_ulid": ULID, "extra": 1}), json!({"project_ulid": "../../etc/passwd"})] {
        assert_eq!(u.handle(&action("update", p.clone())).await.status, "failed", "{p}");
        assert_eq!(r.handle(&action("remove", p.clone())).await.status, "failed", "{p}");
    }
    let out = u.handle(&action("update", json!({"project_ulid": ULID}))).await;
    assert!(out.result["error"].as_str().unwrap().contains("not registered"));
}

#[tokio::test]
async fn remove_unregisters_and_never_deletes_files() {
    let f = fx(0);
    let (_bare, root) = registered_workspace(&f);
    let out = RemoveHandler { env: f.env.clone() }.handle(&action("remove", json!({"project_ulid": ULID}))).await;
    assert_eq!(out.status, "succeeded", "{}", out.result);
    assert_eq!(out.result["unregistered"], true);
    assert_eq!(out.result["deleted_files"], false);
    assert_eq!(Path::new(out.result["root"].as_str().unwrap()), root.canonicalize().unwrap());
    assert!(find_by_id(&f.env.manifests_dir, ULID).unwrap().is_none());
    assert!(root.join("a.txt").is_file() && root.join(".git").is_dir());

    let again = RemoveHandler { env: f.env.clone() }.handle(&action("remove", json!({"project_ulid": ULID}))).await;
    assert_eq!(again.status, "failed");
}

#[tokio::test]
async fn remove_refuses_a_non_workspace_project() {
    let f = fx(0);
    let root = f.home.join("Projects/own");
    std::fs::create_dir_all(&root).unwrap();
    clawft_types::project::adopt_or_init(&root, &f.env.manifests_dir, None).unwrap();
    let id = find_by_root_id(&f);
    let out = RemoveHandler { env: f.env.clone() }.handle(&action("remove", json!({"project_ulid": id}))).await;
    assert_eq!(out.status, "failed");
    assert!(find_by_id(&f.env.manifests_dir, &id).unwrap().is_some());
}

fn find_by_root_id(f: &Fx) -> String {
    clawft_types::project::list_manifests(&f.env.manifests_dir).unwrap().manifests[0].id.clone()
}

#[tokio::test]
async fn install_runs_the_same_add_step_as_the_cli_and_remove_undoes_it() {
    let f = fx(0);
    let out = handler(&f, Arc::new(FakeFetcher::new(false))).handle(&action("install", install_payload("~/Projects/demo", json!([{"url": "https://example.org/a.git"}])))).await;
    assert_eq!(out.status, "succeeded", "{}", out.result);
    let add = &out.result["add"];
    assert_eq!(add["slug"], "demo");
    assert_eq!(add["ulid"], ULID);
    assert_eq!(add["registered_as"], "workspace");
    assert_eq!(add["run"]["kind"], "unknown", "the fake checkout holds no compose file: said, not guessed");
    assert!(add["notes"].as_array().unwrap().iter().any(|n| n.as_str().unwrap().contains("nothing to start")));
    let rdir = crate::project_add_receipt::receipts_dir(&f.home);
    let r = crate::project_add_receipt::read(&rdir, "demo").unwrap().expect("the dashboard path writes the same receipt");
    assert_eq!(r.ulid, ULID);
    // The CLI's remove understands what the dashboard path added (the fake weft wrote no manifest).
    let rm = crate::project_add_remove::remove(&f.env, "demo", false, false).unwrap();
    assert!(rm.kept.iter().any(|k| k.contains("already unregistered")), "{rm:?}");
    assert!(crate::project_add_receipt::read(&rdir, "demo").unwrap().is_none());
    assert!(root_exists(&f), "the checkout stays");
}

fn root_exists(f: &Fx) -> bool {
    f.home.join("Projects/demo").is_dir()
}
