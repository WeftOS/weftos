use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use super::*;
use crate::project_add_receipt::Receipt;

const ULID: &str = "01K6ZQ8N3T4V5W6X7Y8Z9A0B1C";

fn env_in(home: &Path) -> InstallEnv {
    InstallEnv { home: home.to_path_buf(), manifests_dir: home.join(".weftos/projects"), weft: None, git: Default::default(), pair_requests_dir: None }
}

fn add_receipt(home: &Path, root: &Path) {
    let r = Receipt {
        slug: "demo".into(),
        root: root.to_path_buf(),
        repos: vec![],
        ulid: ULID.into(),
        registered_as: "project".into(),
        route_prefix: None,
        fetcher: "git-remote".into(),
        added_at: "t".into(),
    };
    receipt::write(&receipts_dir(home), &r).unwrap();
}

fn project(root: &Path, compose: &str, pc_port: Option<u16>) {
    std::fs::create_dir_all(root.join("compose")).unwrap();
    std::fs::write(root.join("process-compose.yaml"), compose).unwrap();
    if let Some(p) = pc_port {
        std::fs::write(root.join("compose/ports.yaml"), format!("claims:\n  - {{ port: {p}, use: process-compose-http }}\n")).unwrap();
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

#[test]
fn argument_vectors_are_fixed_and_shell_free() {
    let tmp = tempfile::tempdir().unwrap();
    project(tmp.path(), "processes: {}\n", Some(18110));
    add_receipt(tmp.path(), tmp.path());
    let t = target(&env_in(tmp.path()), "demo").unwrap();
    assert_eq!(up_args(&t), ["up", "-f", &tmp.path().join("process-compose.yaml").to_string_lossy(), "-D", "-t=false", "-p", "18110"]);
    assert_eq!(down_args(18110), ["down", "-p", "18110"]);
}

#[test]
fn unknown_slug_no_compose_file_and_no_port_claim_are_refused_with_reasons() {
    let tmp = tempfile::tempdir().unwrap();
    let env = env_in(tmp.path());
    assert!(target(&env, "nope").unwrap_err().contains("was added on this machine"));
    assert!(target(&env, "Bad Slug").is_err());
    let root = tmp.path().join("p");
    std::fs::create_dir_all(&root).unwrap();
    add_receipt(tmp.path(), &root);
    assert!(target(&env, "demo").unwrap_err().contains("no process-compose file"));
    project(&root, "processes: {}\n", None);
    assert!(target(&env, "demo").unwrap_err().contains("process-compose-http"));
}

#[test]
fn compose_variables_missing_from_env_and_dotenv_are_listed_by_name() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("p");
    project(&root, "command: run ${ZZ_DEVUX_A} ${ZZ_DEVUX_B}\n", Some(18110));
    std::fs::write(root.join(".env"), "ZZ_DEVUX_B=value-never-shown\n").unwrap();
    add_receipt(tmp.path(), &root);
    let t = target(&env_in(tmp.path()), "demo").unwrap();
    assert_eq!(t.missing_env, ["ZZ_DEVUX_A"]);
}

#[tokio::test]
async fn up_refuses_missing_env_vars_by_name_without_starting_anything() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("p");
    project(&root, "command: run ${ZZ_DEVUX_NEEDED}\n", Some(free_port()));
    add_receipt(tmp.path(), &root);
    let t = target(&env_in(tmp.path()), "demo").unwrap();
    let e = up(&t, "/nonexistent/process-compose").await.unwrap_err();
    assert!(e.contains("ZZ_DEVUX_NEEDED") && !e.contains("cannot run"), "{e}");
}

#[tokio::test]
async fn up_refuses_a_port_something_else_holds() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("p");
    let squatter = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = squatter.local_addr().unwrap().port();
    project(&root, "processes: {}\n", Some(port));
    add_receipt(tmp.path(), &root);
    let t = target(&env_in(tmp.path()), "demo").unwrap();
    let e = up(&t, "/nonexistent/process-compose").await.unwrap_err();
    assert!(e.contains(&port.to_string()) && e.contains("not this project's process-compose"), "{e}");
}

#[tokio::test]
async fn up_without_process_compose_installed_says_so() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("p");
    project(&root, "processes: {}\n", Some(free_port()));
    add_receipt(tmp.path(), &root);
    let t = target(&env_in(tmp.path()), "demo").unwrap();
    let e = up(&t, "/nonexistent/process-compose").await.unwrap_err();
    assert!(e.contains("is process-compose installed"), "{e}");
}

/// Answers `GET /processes` like process-compose until dropped.
async fn fake_pc_http() -> (u16, tokio::task::JoinHandle<()>) {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    let h = tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else { return };
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 1024];
                let _ = s.read(&mut buf).await;
                let body = r#"{"data":[{"name":"web","status":"Running","restarts":0}]}"#;
                let _ = s.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await;
            });
        }
    });
    (port, h)
}

#[tokio::test]
async fn running_project_is_reported_up_and_down_calls_process_compose_down() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("p");
    let (port, server) = fake_pc_http().await;
    project(&root, "processes: {}\n", Some(port));
    add_receipt(tmp.path(), &root);
    let env = env_in(tmp.path());
    let t = target(&env, "demo").unwrap();
    assert_eq!(up(&t, "/nonexistent/process-compose").await.unwrap(), UpOutcome::AlreadyRunning);
    let s = status(&env, "demo", "/nonexistent/tailscale").await.unwrap();
    assert_eq!(s.process_compose["state"], "ok");
    assert_eq!(s.process_compose["running"], 1);
    assert!(s.notes.iter().any(|n| n.contains("no route is installed")), "{:?}", s.notes);

    let log = tmp.path().join("pc.log");
    let bin = tmp.path().join("pc");
    std::fs::write(&bin, format!("#!/bin/sh\nfor a in \"$@\"; do echo \"$a\" >> '{}'; done\n", log.display())).unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(down(&t, bin.to_str().unwrap()).await.unwrap());
    assert_eq!(std::fs::read_to_string(&log).unwrap().lines().collect::<Vec<_>>(), ["down", "-p", &port.to_string()]);
    server.abort();
    assert!(!down(&t, bin.to_str().unwrap()).await.unwrap(), "not running: nothing to stop");
}
