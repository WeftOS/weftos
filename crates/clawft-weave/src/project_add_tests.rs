//! `add` end to end without a network: a temp bare git repository, a stand-in
//! `weft` that writes the manifest, the real fetcher, register, finish, status
//! and remove, all under a temp home.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use super::*;
use crate::project_add_receipt as receipt;
use crate::project_add_remove::remove;
use crate::project_add_route::RoutePlan;
use crate::project_add_spec::plan_add;
use crate::project_install::ProjectFetcher;
use crate::project_install_git::GitRemoteFetcher;
use crate::project_install_test_support::{git, test_runner};

const FIXED_ULID: &str = "01K6ZQ8N3T4V5W6X7Y8Z9A0B1C";

fn env_in(home: &Path, weft: PathBuf) -> InstallEnv {
    InstallEnv {
        home: home.to_path_buf(),
        manifests_dir: home.join(".weftos/projects"),
        weft: Some(weft),
        git: test_runner(),
        pair_requests_dir: None,
    }
}

/// A `weft` that registers the working directory under the `--adopt` id (or a fixed one).
fn registering_weft(dir: &Path) -> PathBuf {
    let bin = dir.join("weft");
    std::fs::write(
        &bin,
        format!(
            "#!/bin/sh\nid={FIXED_ULID}\nwhile [ $# -gt 0 ]; do [ \"$1\" = \"--adopt\" ] && id=\"$2\"; shift; done\n\
             mkdir -p \"$WEFTOS_MANIFESTS_DIR\"\nroot=$(pwd -P)\n\
             printf 'schema = 1\\nid = \"%s\"\\nname = \"x\"\\nroot = \"%s\"\\nstate = \"active\"\\ncreated = \"2026-10-07T15:19:11Z\"\\nlast_seen = \"2026-10-07T15:19:11Z\"\\nproject_toml = \"present\"\\n' \"$id\" \"$root\" > \"$WEFTOS_MANIFESTS_DIR/$id.toml\"\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

/// A bare repository holding a small process-compose project.
fn demo_remote(base: &Path, ports: &str) -> PathBuf {
    let work = base.join("demo-seed");
    std::fs::create_dir_all(work.join("compose")).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    std::fs::write(work.join("process-compose.yaml"), "processes:\n  web:\n    command: echo ${SECRET_THING}\n").unwrap();
    std::fs::write(work.join("compose/ports.yaml"), ports).unwrap();
    std::fs::write(work.join(".env.example"), "OPENROUTER_API_KEY=sk-never-copied\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "first"]);
    let bare = base.join("demo.git");
    git(base, &["clone", "-q", "--bare", work.to_str().unwrap(), bare.to_str().unwrap()]);
    bare
}

const PORTS: &str = "project: demo\nclaims:\n  - { port: 18110, use: process-compose-http }\n  - { port: 18120, use: web }\n";

/// Every file under the state directories with its content, relative to home.
fn state(home: &Path) -> Vec<(String, String)> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<(String, String)>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(base, &p, out);
            } else {
                let rel = p.strip_prefix(base).unwrap().to_string_lossy().into_owned();
                out.push((rel, std::fs::read_to_string(&p).unwrap_or_default()));
            }
        }
    }
    let mut out = Vec::new();
    walk(home, &home.join(".weftos"), &mut out);
    out.sort();
    out
}

async fn do_add(env: &InstallEnv, remote: &Path) -> Result<AddReport, String> {
    // The URL the user types is checked by `plan_add`; the fetch itself is pointed at the temp remote.
    let mut plan = plan_add("https://example.invalid/org/demo.git", None, None, None, &env.home).map_err(|e| e.to_string())?;
    plan.request.sources[0].url = format!("file://{}", remote.display());
    let report = GitRemoteFetcher::with_runner(env.git.clone()).fetch(&plan.request, &plan.target).await?;
    let (ulid, registered_as) = register(env, env.weft.as_ref().unwrap(), &plan.target, None, Some(&plan.slug), &[]).await?;
    Ok(finish(env, Finish { slug: &plan.slug, root: &plan.target, siblings: vec![], ulid, registered_as, fetcher: report.fetcher, repos: report.repos }))
}

#[tokio::test]
async fn add_status_remove_round_trips_and_leaves_the_checkout() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    // A port nothing listens on (18110 may be a real process-compose on a dev machine).
    let pc = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let remote = demo_remote(tmp.path(), &PORTS.replace("18110", &pc.to_string()));
    let env = env_in(&home, registering_weft(tmp.path()));
    let before = state(&home);

    let r = do_add(&env, &remote).await.unwrap();
    assert_eq!(r.slug, "demo");
    assert_eq!(r.registered_as, RegisteredAs::Project);
    assert_eq!(r.ulid, FIXED_ULID);
    assert_eq!(r.route, RoutePlan::Overlay { prefix: "/demo".into(), port: 18120 });
    assert_eq!(r.process_compose_port, Some(pc));
    let names: Vec<_> = r.env_vars.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["OPENROUTER_API_KEY", "SECRET_THING"]);
    assert!(!serde_json::to_string(&r).unwrap().contains("sk-never-copied"));
    assert!(home.join("Projects/demo/process-compose.yaml").is_file());
    assert!(!home.join("Projects/demo/.env").exists(), "no .env is ever created");
    assert!(env.manifests_dir.join(format!("{FIXED_ULID}.toml")).is_file());
    assert!(receipt::read(&receipts_dir(&home), "demo").unwrap().is_some());

    let s = crate::project_add_run::status(&env, "demo", "/nonexistent/tailscale").await.unwrap();
    assert_eq!(s.route_prefix.as_deref(), Some("/demo"));
    assert_eq!(s.route_source.as_deref(), Some("dashboard"));
    assert_eq!(s.local_url.as_deref(), Some("http://127.0.0.1:18120/demo/"));
    assert_eq!(s.tailnet_url, None);
    assert_eq!(s.process_compose["state"], "down", "nothing listens on {pc}: {}", s.process_compose);

    let out = remove(&env, "demo", false, false).unwrap();
    assert!(out.undone.iter().any(|u| u.contains("removed route /demo")), "{out:?}");
    assert!(home.join("Projects/demo/process-compose.yaml").is_file(), "the checkout stays");
    assert_eq!(state(&home), before, "remove leaves exactly the state add found");
    assert!(remove(&env, "demo", false, false).is_err(), "a second remove has nothing to undo");
}

#[tokio::test]
async fn purge_deletes_the_checkout_and_refuses_uncommitted_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let remote = demo_remote(tmp.path(), PORTS);
    let env = env_in(&home, registering_weft(tmp.path()));
    do_add(&env, &remote).await.unwrap();
    let co = home.join("Projects/demo");
    std::fs::write(co.join("scratch.txt"), "work in progress").unwrap();

    let e = remove(&env, "demo", true, false).unwrap_err();
    assert!(e.contains("uncommitted"), "{e}");
    assert!(co.join("scratch.txt").is_file());
    assert!(receipt::read(&receipts_dir(&home), "demo").unwrap().is_some(), "a refused purge changes nothing");
    assert!(env.manifests_dir.join(format!("{FIXED_ULID}.toml")).is_file());

    let out = remove(&env, "demo", true, true).unwrap();
    assert!(out.undone.iter().any(|u| u.starts_with("deleted ")), "{out:?}");
    assert!(!co.exists());
}

#[tokio::test]
async fn a_project_that_declares_its_own_routes_gets_no_overlay() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let ports = format!("{PORTS}routes:\n  - {{ prefix: /demo, port: 18120 }}\n");
    let remote = demo_remote(tmp.path(), &ports);
    let env = env_in(&home, registering_weft(tmp.path()));
    let r = do_add(&env, &remote).await.unwrap();
    assert_eq!(r.route, RoutePlan::Repo { prefixes: vec!["/demo".into()] });
    assert!(!home.join(".weftos/routes").exists(), "nothing written to the overlay");
    let out = remove(&env, "demo", false, false).unwrap();
    assert!(!out.undone.iter().any(|u| u.contains("route")), "{out:?}");
}

#[tokio::test]
async fn adding_over_a_taken_prefix_installs_no_route_and_says_why() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    // Another registered project already routes /demo.
    let other = home.join("other");
    std::fs::create_dir_all(other.join("compose")).unwrap();
    std::fs::write(other.join("compose/ports.yaml"), "project: other\nroutes:\n  - { prefix: /demo, port: 19000 }\n").unwrap();
    let env = env_in(&home, registering_weft(tmp.path()));
    std::fs::create_dir_all(&env.manifests_dir).unwrap();
    std::fs::write(
        env.manifests_dir.join("01K6ZQ8N3T4V5W6X7Y8Z9A0B1D.toml"),
        format!("schema = 1\nid = \"01K6ZQ8N3T4V5W6X7Y8Z9A0B1D\"\nname = \"other\"\nroot = \"{}\"\nstate = \"active\"\ncreated = \"2026-10-07T15:19:11Z\"\nlast_seen = \"2026-10-07T15:19:11Z\"\nproject_toml = \"present\"\n", other.canonicalize().unwrap().display()),
    )
    .unwrap();
    let remote = demo_remote(tmp.path(), PORTS);
    let r = do_add(&env, &remote).await.unwrap();
    assert!(matches!(&r.route, RoutePlan::Conflict { reason } if reason.contains("already routed by project other")), "{:?}", r.route);
    assert!(!home.join(".weftos/routes").exists());
    assert!(r.next.iter().any(|n| n.contains("no route was installed")));
}

#[tokio::test]
async fn a_failed_registration_is_reported_by_the_caller_not_swallowed() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let bad = tmp.path().join("weft-bad");
    std::fs::write(&bad, "#!/bin/sh\necho 'init said no' >&2\nexit 3\n").unwrap();
    std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o755)).unwrap();
    let env = env_in(&home, bad.clone());
    let e = register(&env, &bad, tmp.path(), None, Some("x"), &[]).await.unwrap_err();
    assert!(e.contains("registering the workspace failed") && e.contains("init said no"), "{e}");
    let e = register(&env, &bad, tmp.path(), None, None, &[tmp.path()]).await.unwrap_err();
    assert!(e.contains("no project identity"), "{e}");
}
