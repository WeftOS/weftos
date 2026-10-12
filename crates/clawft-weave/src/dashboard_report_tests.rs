//! Reporter and rotation tests against a local fake dashboard (never the real
//! one) and a temp token directory (never `~/.weftos`).

use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use serde_json::{Value, json};

use super::*;
use crate::dashboard_test_support::*;

const HB: &str = "/api/nodes/heartbeat";
const ROT: &str = "/api/nodes/token/rotate";

fn rotate_ok(tok: &str) -> String {
    json!({ "token": tok, "rotated_at": "2026-10-05T12:00:00Z" }).to_string()
}

fn file_tok(p: &std::path::Path) -> String {
    std::fs::read_to_string(p).unwrap().trim().to_owned()
}

fn not_root() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() != 0 }
}

#[tokio::test]
async fn heartbeat_sends_the_expected_body_and_bearer() {
    let fake = FakeDash::start().await;
    fake.answer(HB, &[(200, "{}")]);
    let tok = token('a');
    let (_d, path) = token_dir(&tok);
    let dash = dashboard(config(&fake.url, &path));

    assert_eq!(dash.heartbeat_once().await, Beat::Ok);

    let reqs = fake.requests(HB);
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].authorization.as_deref(), Some(format!("Bearer {tok}").as_str()));
    assert_eq!(reqs[0].content_type.as_deref(), Some("application/json"));
    let body: Value = serde_json::from_str(&reqs[0].body).unwrap();
    let v = env!("CARGO_PKG_VERSION");
    assert_eq!(body["node_id"], NODE_ID);
    assert_eq!(body["installation_id"], "photo-gallery");
    assert_eq!(body["report"]["host"], "photo-gallery");
    assert_eq!(body["report"]["weaver_version"], v);
    let o = &body["report"]["observed"][ULID];
    assert_eq!(o["release"], format!("v{v}"));
    assert_eq!(o["project_child_state"], "running");
    assert_eq!(o["mesh_listen"], "0.0.0.0:9470");
    assert_eq!(o["gateway_url"], "http://127.0.0.1:8080");
    assert!(!reqs[0].body.contains(&tok), "the token is a header, never in the body");

    let st = dash.status_json();
    assert_eq!(st["last_heartbeat"], "ok");
    assert_eq!(st["consecutive_failures"], 0);
    assert!(!st.to_string().contains(&tok));
}

#[tokio::test]
async fn unit_keys_match_what_the_shell_heartbeat_sent() {
    assert_eq!(unit_key("weftos.service"), "unit_weftos");
    assert_eq!(unit_key("weftos-gateway.service"), "unit_gateway");
    assert_eq!(unit_key("my-thing@1.service"), "unit_my_thing_1");
}

#[tokio::test]
async fn a_rejected_token_is_reported_clearly_and_the_reporter_recovers() {
    let fake = FakeDash::start().await;
    fake.answer(HB, &[(401, r#"{"error":"revoked"}"#), (200, "{}")]);
    let (_d, path) = token_dir(&token('a'));
    let dash = dashboard(config(&fake.url, &path));

    assert_eq!(dash.heartbeat_once().await, Beat::Rejected(401));
    let st = dash.status_json();
    assert_eq!(st["last_heartbeat"], "rejected (401)");
    assert_eq!(st["consecutive_failures"], 1);

    // A fresh token written by the operator is picked up on the next beat.
    std::fs::write(&path, format!("{}\n", token('b'))).unwrap();
    assert_eq!(dash.heartbeat_once().await, Beat::Ok);
    assert_eq!(dash.status_json()["consecutive_failures"], 0);
    let reqs = fake.requests(HB);
    assert_eq!(reqs[1].authorization.as_deref(), Some(format!("Bearer {}", token('b')).as_str()));
}

#[tokio::test]
async fn the_loop_keeps_running_after_a_401_and_sends_a_first_beat() {
    let fake = FakeDash::start().await;
    fake.answer(HB, &[(401, "{}")]);
    let (_d, path) = token_dir(&token('a'));
    let dash = dashboard(config(&fake.url, &path));
    let handle = dash.spawn_loop(Duration::ZERO);
    for _ in 0..200 {
        if fake.total() > 0 && dash.status_json()["last_heartbeat"] == "rejected (401)" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(dash.status_json()["last_heartbeat"], "rejected (401)");
    assert!(!handle.is_finished(), "a rejected token does not stop the reporter");
    handle.abort();
}

#[tokio::test]
async fn a_rotation_between_reading_and_sending_is_retried_with_the_new_token() {
    let fake = FakeDash::start().await;
    fake.answer(HB, &[(401, "{}"), (200, "{}")]);
    let (_d, path) = token_dir(&token('a'));
    let p2 = path.clone();
    // The "dashboard" revokes the old token and the new one lands on disk
    // while the first request is in flight.
    fake.on_request(move |n| {
        if n == 1 {
            std::fs::write(&p2, format!("{}\n", token('b'))).unwrap();
        }
    });
    let dash = dashboard(config(&fake.url, &path));
    assert_eq!(dash.heartbeat_once().await, Beat::Ok);
    assert_eq!(fake.requests(HB).len(), 2);
}

#[tokio::test]
async fn a_token_file_with_bad_permissions_is_refused_and_nothing_is_sent() {
    let fake = FakeDash::start().await;
    fake.answer(HB, &[(200, "{}")]);
    let (_d, path) = token_dir(&token('a'));
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let dash = dashboard(config(&fake.url, &path));
    match dash.heartbeat_once().await {
        Beat::Failed(e) => assert!(e.contains("must be 600"), "{e}"),
        other => panic!("{other:?}"),
    }
    assert!(dash.rotate().await.unwrap_err().contains("must be 600"));
    assert_eq!(fake.total(), 0, "no request left the node");
    assert!(dash.status_json()["token_file_check"].as_str().unwrap().contains("600"));
}

#[tokio::test]
async fn the_http_failure_modes_never_reach_the_logs_or_errors_with_a_token() {
    let log = LogCapture::default();
    let _g = log.install();
    let tok = token('c');
    let (_d, path) = token_dir(&tok);
    // Nothing is listening: a transport error.
    let dash = dashboard(config("http://127.0.0.1:1", &path));
    let beat = dash.heartbeat_once().await;
    assert!(matches!(beat, Beat::Failed(_)), "{beat:?}");
    let e = dash.rotate().await.unwrap_err();
    assert!(e.contains("could not reach"), "{e}");

    let fake = FakeDash::start().await;
    fake.answer(HB, &[(500, "boom")]);
    fake.answer(ROT, &[(200, &rotate_ok(&token('d')))]);
    let dash = dashboard(config(&fake.url, &path));
    assert!(matches!(dash.heartbeat_once().await, Beat::Failed(m) if m.contains("HTTP 500")));
    dash.rotate().await.unwrap();
    dash.heartbeat_once().await;
    let text = log.text();
    assert!(text.contains("dashboard token rotated"), "the rotation is logged:\n{text}");
    for secret in [tok, token('d')] {
        assert!(!text.contains(&secret), "a token reached the logs:\n{text}");
    }
}

#[tokio::test]
async fn rotate_replaces_the_file_atomically_with_mode_600_and_heartbeats_use_the_new_token() {
    let fake = FakeDash::start().await;
    let (old, new) = (token('a'), token('b'));
    fake.answer(ROT, &[(200, &rotate_ok(&new))]);
    fake.answer(HB, &[(200, "{}")]);
    let (dir, path) = token_dir(&old);
    let dash = dashboard(config(&fake.url, &path));

    let out = dash.rotate().await.unwrap();
    assert_eq!(out["rotated"], true);
    assert_eq!(out["rotated_at"], "2026-10-05T12:00:00Z");
    assert!(!out.to_string().contains(&new), "the result carries no token");

    let reqs = fake.requests(ROT);
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].authorization.as_deref(), Some(format!("Bearer {old}").as_str()));
    assert_eq!(serde_json::from_str::<Value>(&reqs[0].body).unwrap(), json!({ "node_id": NODE_ID }));

    assert_eq!(file_tok(&path), new);
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    let names: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(names.len(), 1, "no temp file is left behind: {names:?}");
    crate::dashboard_cfg::check_token_file(&path).unwrap();

    assert_eq!(dash.heartbeat_once().await, Beat::Ok);
    assert_eq!(
        fake.requests(HB)[0].authorization.as_deref(),
        Some(format!("Bearer {new}").as_str())
    );
    let st = dash.status_json();
    assert_eq!(st["last_rotation"], "ok");
    assert!(st["last_rotation_at"].is_string());
}

#[tokio::test]
async fn rotate_when_the_dashboard_refuses_leaves_the_file_alone() {
    for (status, needle) in [(401, "rejected"), (403, "rejected"), (500, "unchanged")] {
        let fake = FakeDash::start().await;
        fake.answer(ROT, &[(status, "{}")]);
        let old = token('a');
        let (_d, path) = token_dir(&old);
        let dash = dashboard(config(&fake.url, &path));
        let e = dash.rotate().await.unwrap_err();
        assert!(e.contains(needle), "{status}: {e}");
        assert_eq!(file_tok(&path), old);
        assert!(dash.status_json()["last_rotation"].as_str().unwrap().contains(needle));
        assert_eq!(dash.status_json()["token_unpersisted"], false);
    }
}

#[tokio::test]
async fn an_unusable_rotate_answer_says_the_old_token_is_revoked_and_saves_nothing() {
    for body in ["not json", "{}", r#"{"token":"wft_short"}"#, r#"{"token":"x y"}"#] {
        let fake = FakeDash::start().await;
        fake.answer(ROT, &[(200, body)]);
        let old = token('a');
        let (_d, path) = token_dir(&old);
        let dash = dashboard(config(&fake.url, &path));
        let e = dash.rotate().await.unwrap_err();
        assert!(e.contains("revoked") && e.contains("node.token"), "{body}: {e}");
        assert_eq!(file_tok(&path), old);
    }
}

#[tokio::test]
async fn a_write_failure_after_the_dashboard_rotated_is_loud_keeps_the_token_and_recovers() {
    if !not_root() {
        return; // root ignores directory permissions
    }
    let log = LogCapture::default();
    let _g = log.install();
    let fake = FakeDash::start().await;
    let (old, new) = (token('a'), token('b'));
    fake.answer(ROT, &[(200, &rotate_ok(&new))]);
    fake.answer(HB, &[(200, "{}")]);
    let (dir, path) = token_dir(&old);
    let dash = dashboard(config(&fake.url, &path));

    // The directory stops being writable: the temp file cannot be created.
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
    let e = dash.rotate().await.unwrap_err();
    assert!(e.contains("could NOT be written") && e.contains("already revoked"), "{e}");
    assert!(!e.contains(&new) && !log.text().contains(&new));
    assert!(log.text().contains("could NOT be written"), "logged loudly:\n{}", log.text());
    assert_eq!(file_tok(&path), old, "the file is untouched");
    let st = dash.status_json();
    assert_eq!(st["token_unpersisted"], true);
    assert!(st["last_rotation"].as_str().unwrap().contains("could NOT"));

    // Heartbeats keep working on the in-memory token, not the revoked one.
    assert_eq!(dash.heartbeat_once().await, Beat::Ok);
    assert_eq!(fake.requests(HB)[0].authorization.as_deref(), Some(format!("Bearer {new}").as_str()));

    // Once the directory is fixed the next beat's persist step saves it.
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    dash.persist_pending().await;
    assert_eq!(file_tok(&path), new);
    assert_eq!(dash.status_json()["token_unpersisted"], false);
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
}

#[tokio::test]
async fn concurrent_rotations_are_serialised() {
    let fake = FakeDash::start().await;
    fake.answer(ROT, &[(200, &rotate_ok(&token('b'))), (401, "{}")]);
    let (_d, path) = token_dir(&token('a'));
    let dash = dashboard(config(&fake.url, &path));
    let (a, b) = tokio::join!(dash.rotate(), dash.rotate());
    // Whichever ran first rotated; the second presented the NEW token (which
    // the fake now refuses), so exactly one succeeded and the file is intact.
    assert_eq!([a.is_ok(), b.is_ok()].iter().filter(|x| **x).count(), 1, "{a:?} {b:?}");
    assert_eq!(file_tok(&path), token('b'));
    let rot = fake.requests(ROT);
    assert_eq!(rot.len(), 2);
    assert_ne!(rot[0].authorization, rot[1].authorization, "the second call used the rotated token");
}

#[test]
fn backoff_doubles_then_caps() {
    let i = Duration::from_secs(60);
    assert_eq!(Dashboard::backoff(i, 0), i);
    assert_eq!(Dashboard::backoff(i, 1), Duration::from_secs(120));
    assert_eq!(Dashboard::backoff(i, 3), Duration::from_secs(480));
    assert_eq!(Dashboard::backoff(i, 4), MAX_BACKOFF);
    assert_eq!(Dashboard::backoff(i, 40), MAX_BACKOFF);
    assert_eq!(Dashboard::backoff(Duration::from_secs(3600), 5), Duration::from_secs(3600));
}

#[tokio::test]
async fn an_invalid_config_cannot_build_a_reporter() {
    let (_d, path) = token_dir(&token('a'));
    let mut cfg = config("http://example.com", &path);
    assert!(Dashboard::new(cfg.clone(), Ambient::default(), std::sync::Arc::new(Kids(vec![]))).is_err());
    cfg.url = "https://example.com".into();
    cfg.node_id = "nope".into();
    assert!(Dashboard::new(cfg, Ambient::default(), std::sync::Arc::new(Kids(vec![]))).is_err());
}

#[tokio::test]
async fn a_413_is_reported_as_too_large_not_as_a_rejected_token() {
    let fake = FakeDash::start().await;
    fake.answer(HB, &[(413, r#"{"error":"Report too large"}"#)]);
    let (_d, path) = token_dir(&token('a'));
    let dash = dashboard(config(&fake.url, &path));

    let beat = dash.heartbeat_once().await;
    let Beat::TooLarge(n) = beat else { panic!("expected TooLarge, got {beat:?}") };
    assert!(n > 0 && n <= crate::dashboard_budget::REPORT_BUDGET_BYTES, "{n}");
    let st = dash.status_json();
    assert_eq!(st["last_heartbeat"], format!("report too large ({n} bytes)"));
    assert_eq!(st["consecutive_failures"], 1);
}

#[tokio::test]
async fn the_body_carries_the_node_kind_and_the_instances_summary() {
    let (_d, path) = token_dir(&token('a'));
    let mut cfg = config("http://127.0.0.1:1", &path);
    cfg.kind = crate::dashboard_cfg::NodeKind::ProjectDaemon;
    let dash = dashboard(cfg);

    let body = dash.build_body().await;
    assert_eq!(body["report"]["kind"], "project-daemon");
    assert!(body["report"].get("instances").is_none(), "no registry, no key");

    let reg = crate::instance_registry::InstanceRegistry::new(None);
    assert!(dash.set_instance_registry(reg.clone()));
    let id = reg.start(crate::instance_registry::NewInstance { runner: "harness-job".into(), ..Default::default() }).unwrap();
    let body = dash.build_body().await;
    let i = &body["report"]["instances"];
    assert_eq!(i["total"], 1);
    assert_eq!(i["counts"]["starting"], 1);
    assert_eq!(i["recent"][0]["instance_id"], id.as_str());
    assert_eq!(i["recent"][0]["node"], NODE_ID);
}

#[test]
fn the_default_node_kind_is_machine_and_an_unknown_kind_is_refused() {
    let c = DashboardConfig::from_config(&json!({ "dashboard": { "enabled": false } })).unwrap();
    assert_eq!(serde_json::to_value(c.kind).unwrap(), "machine");
    for k in ["machine", "project-daemon", "dev", "vm"] {
        let c = DashboardConfig::from_config(&json!({ "dashboard": { "kind": k } })).unwrap();
        assert_eq!(serde_json::to_value(c.kind).unwrap(), k);
    }
    assert!(DashboardConfig::from_config(&json!({ "dashboard": { "kind": "laptop" } })).is_err());
}
