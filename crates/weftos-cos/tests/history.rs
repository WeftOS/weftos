//! `projects.activity` reads project history from the dashboard GET route:
//! one page, a second page on request, 404 as "no access" and 503 as
//! "unavailable", the last two reported plainly and never failing the turn.

mod common;

use common::{FakeHarness, TEST_PROJECT, TEST_TOKEN, UNAVAILABLE_PROJECT, harness_client};
use serde_json::{Value, json};
use weftos_cos::tools::Tool;
use weftos_cos::tools::harness::ProjectsActivity;

async fn run(args: Value) -> (Value, std::sync::Arc<std::sync::Mutex<Vec<(String, Value)>>>) {
    let (addr, posts) = FakeHarness { with_events_route: true, ..Default::default() }.start().await;
    let tool = ProjectsActivity { client: harness_client(addr, Some(TEST_PROJECT)) };
    let out = tool.call(args).await.expect("the turn never fails on history");
    (out.result["history"].clone(), posts)
}

#[tokio::test]
async fn one_page_newest_first_with_a_voice_sized_default() {
    let (h, posts) = run(json!({})).await;
    assert_eq!(h["available"], true);
    assert_eq!(h["items"][0]["title"], "Newest milestone");
    assert_eq!(h["next_before"], "cursor-1");
    let posts = posts.lock().unwrap();
    let (_, q) = posts.iter().find(|(p, _)| p.starts_with("GET ")).unwrap();
    assert_eq!(q["query"], "limit=10");
}

#[tokio::test]
async fn second_page_only_with_the_cursor_verbatim() {
    let (h, posts) = run(json!({"history_before": "cursor-1", "history_kind": "milestone", "history_limit": 5})).await;
    assert_eq!(h["items"][0]["title"], "Older milestone");
    assert!(h["next_before"].is_null());
    let posts = posts.lock().unwrap();
    let q = posts.iter().find(|(p, _)| p.starts_with("GET ")).unwrap().1["query"].as_str().unwrap().to_string();
    assert!(q.contains("before=cursor-1") && q.contains("kind=milestone") && q.contains("limit=5"), "{q}");
}

#[tokio::test]
async fn not_found_means_no_access() {
    let (h, _) = run(json!({"project": "44444444-4444-4444-8444-444444444444"})).await;
    assert_eq!(h["available"], false);
    assert_eq!(h["reason"], "no history access for this project");
}

#[tokio::test]
async fn service_unavailable_means_unavailable_right_now() {
    let (h, _) = run(json!({"project": UNAVAILABLE_PROJECT})).await;
    assert_eq!(h["available"], false);
    let r = h["reason"].as_str().unwrap();
    assert!(r.contains("unavailable right now") && r.contains("503"), "{r}");
    assert!(!r.contains(TEST_TOKEN));
}

#[tokio::test]
async fn unreachable_dashboard_is_unavailable_not_an_error() {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    drop(l);
    let tool = ProjectsActivity { client: harness_client(addr, Some(TEST_PROJECT)) };
    let err = tool.call(json!({})).await;
    // Tickets come from the same dead server, so the tool errors there; the
    // history read itself must still classify the failure plainly.
    assert!(err.is_err() || err.unwrap().result["history"]["available"] == false);
    let h = harness_client(addr, Some(TEST_PROJECT)).history(None, &[]).await;
    assert!(matches!(h, weftos_cos::tools::harness::History::Unavailable(_)));
}
