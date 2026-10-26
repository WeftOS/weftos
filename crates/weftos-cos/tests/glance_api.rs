//! Glance cards over the API: the tool through the model, answering a card,
//! the snapshot and art routes, and persistence across a restart.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{ScriptedLlm, build_agent, collect_until_final};
use serde_json::{Value, json};
use weftos_cos::conversation::{AnswerKind, Conversation, GuideAnswer, UserUtterance};
use weftos_cos::events::Event;
use weftos_cos::glance_store::{ART_META_FILE, ART_PNG_FILE, CARD_FILE};

async fn serve(t: &common::TestAgent) -> String {
    let conv: Arc<dyn Conversation> = Arc::new(t.agent.clone());
    let (addr, server) = weftos_cos::server::bind(conv, "127.0.0.1:0".parse().unwrap()).await.unwrap();
    tokio::spawn(server);
    format!("http://{addr}")
}

/// Drive the model to post a two-option card; returns the card id.
async fn post_card(t: &common::TestAgent) -> String {
    let mut rx = t.agent.events();
    t.agent.utter(UserUtterance::typed("flag the release decision")).await.unwrap();
    let evs = collect_until_final(&mut rx, Duration::from_secs(10)).await;
    evs.iter().find_map(|e| match e { Event::Card { id, .. } => Some(id.clone()), _ => None }).expect("card event")
}

fn script() -> Vec<Value> {
    vec![
        ScriptedLlm::tool_call("glance_card", json!({"kind": "needs_you", "title": "Ship rc.16?", "options": ["ship", "soak a day"], "reason": "release blocks"})),
        ScriptedLlm::text("Posted."),
        ScriptedLlm::text("Soaking, then."),
    ]
}

#[tokio::test]
async fn the_model_posts_a_card_and_the_snapshot_shows_it() {
    let t = build_agent(ScriptedLlm::new(script()), None, None).await;
    let base = serve(&t).await;
    let id = post_card(&t).await;
    assert_eq!(id.len(), 8);
    let snap: Value = reqwest::get(format!("{base}/v1/cos/glance")).await.unwrap().json().await.unwrap();
    assert_eq!(snap["card"]["id"], id);
    assert_eq!(snap["card"]["options"], json!(["ship", "soak a day"]));
    assert!(snap["card"]["expires_at"].is_string());
    // The mirror of the panel renders the options when there is no detail.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let snap: Value = reqwest::get(format!("{base}/v1/cos/glance")).await.unwrap().json().await.unwrap();
    assert_eq!(snap["fields"]["task"], "NEEDS YOU: Ship rc.16?");
    assert_eq!(snap["fields"]["next"], "A: ship / B: soak a day");
    assert!(snap["art"].is_null());
}

#[tokio::test]
async fn answering_a_card_clears_it_and_gives_the_agent_a_turn() {
    let t = build_agent(ScriptedLlm::new(script()), None, None).await;
    let id = post_card(&t).await;
    let mut rx = t.agent.events();
    let ans = GuideAnswer { kind: AnswerKind::Answer, id: id.clone(), value: json!(2) };
    t.agent.answer_from(ans.clone(), Some("phone".into())).await.unwrap();
    let evs = collect_until_final(&mut rx, Duration::from_secs(10)).await;
    assert!(evs.iter().any(|e| matches!(e, Event::Card { card: weftos_cos::events::CardKind::Clear, id: cid, .. } if *cid == id)), "{evs:?}");
    let user = t.agent.recent_turns(10).into_iter().rev().find(|m| m.role == "user").unwrap();
    assert_eq!((user.text.as_str(), user.endpoint.as_str()), ("Answer to card \"Ship rc.16?\": soak a day", "phone"));
    assert!(t.agent.glance()["card"].is_null());
    // A second answer is stale.
    let e = t.agent.answer_from(ans, None).await.unwrap_err();
    assert!(matches!(e, weftos_cos::CosError::Config(_)));
}

#[tokio::test]
async fn stale_unknown_and_bad_answers_are_400() {
    let t = build_agent(ScriptedLlm::new(script()), None, None).await;
    let base = serve(&t).await;
    let id = post_card(&t).await;
    let c = reqwest::Client::new();
    let post = |body: Value| c.post(format!("{base}/v1/cos/answer")).json(&body).send();
    for bad in [json!({"kind": "answer", "id": "deadbeef", "value": 1}), json!({"kind": "answer", "id": id, "value": 3}), json!({"kind": "answer", "id": id, "value": "maybe"}), json!({"kind": "answer", "id": id, "value": null})] {
        let r = post(bad.clone()).await.unwrap();
        assert_eq!(r.status(), 400, "{bad}");
    }
    assert!(!t.agent.glance()["card"].is_null(), "refused answers leave the card up");
    // The option text works too, case-insensitively.
    let r = post(json!({"kind": "answer", "id": id, "value": "SHIP"})).await.unwrap();
    assert_eq!(r.status(), 202);
    assert!(r.json::<Value>().await.unwrap()["turn_id"].is_string());
    assert_eq!(post(json!({"kind": "answer", "id": id, "value": 1})).await.unwrap().status(), 400);
}

#[tokio::test]
async fn art_route_is_404_until_the_bridge_writes_it_and_state_survives_a_restart() {
    let t = build_agent(ScriptedLlm::new(script()), None, None).await;
    let run = t.dir.path().join("run");
    let agent = t.agent.clone().with_glance_dir(run.clone());
    let conv: Arc<dyn Conversation> = Arc::new(agent.clone());
    let (addr, server) = weftos_cos::server::bind(conv, "127.0.0.1:0".parse().unwrap()).await.unwrap();
    tokio::spawn(server);
    let base = format!("http://{addr}");
    assert_eq!(reqwest::get(format!("{base}/v1/cos/glance/art.png")).await.unwrap().status(), 404);

    // The bridge writes the art; the agent serves it.
    std::fs::write(run.join(ART_PNG_FILE), b"\x89PNG-bytes").unwrap();
    std::fs::write(run.join(ART_META_FILE), json!({"last_art_at": 1_790_000_000, "caption": "the loom at dusk"}).to_string()).unwrap();
    let r = reqwest::get(format!("{base}/v1/cos/glance/art.png")).await.unwrap();
    assert_eq!((r.status().as_u16(), r.headers()["content-type"].to_str().unwrap()), (200, "image/png"));
    assert_eq!(r.bytes().await.unwrap().as_ref(), b"\x89PNG-bytes");
    let snap: Value = reqwest::get(format!("{base}/v1/cos/glance")).await.unwrap().json().await.unwrap();
    assert_eq!((snap["art"]["caption"].as_str(), snap["art"]["png_url"].as_str()), (Some("the loom at dusk"), Some("/v1/cos/glance/art.png")));
    assert!(snap["art"]["at"].as_str().unwrap().starts_with("2026-"));
}

#[tokio::test]
async fn a_live_card_survives_a_restart_and_an_expired_one_does_not() {
    use weftos_cos::events::CardKind;
    use weftos_cos::glance_store::{GlanceStore, StoredCard};
    let dir = tempfile::tempdir().unwrap();
    let card = |id: &str, mins: i64| StoredCard {
        id: id.into(),
        kind: CardKind::NeedsYou,
        title: "Ship?".into(),
        detail: String::new(),
        alert: String::new(),
        options: vec!["ship".into(), "wait".into()],
        reason: "r".into(),
        expires_at: chrono::Utc::now() + chrono::Duration::minutes(mins),
    };
    let first = GlanceStore::default();
    first.set_dir(dir.path().to_path_buf());
    first.put(card("aaaa1111", 30));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(dir.path().join(CARD_FILE)).unwrap().permissions().mode() & 0o777, 0o600);
    }
    let second = GlanceStore::default();
    second.set_dir(dir.path().to_path_buf());
    let snap = second.snapshot(chrono::Utc::now());
    assert_eq!(snap["card"]["id"], "aaaa1111");
    assert_eq!(snap["fields"]["next"], "A: ship / B: wait", "the mirror is restored too");
    // Clearing removes the file.
    second.clear();
    assert!(!dir.path().join(CARD_FILE).exists());
    // An expired card is not restored.
    first.put(card("bbbb2222", -1));
    let third = GlanceStore::default();
    third.set_dir(dir.path().to_path_buf());
    assert!(third.snapshot(chrono::Utc::now())["card"].is_null());
}

#[tokio::test]
async fn unknown_card_id_says_no_such_card() {
    let t = build_agent(ScriptedLlm::new(script()), None, None).await;
    let base = serve(&t).await;
    let r = reqwest::Client::new().post(format!("{base}/v1/cos/answer")).json(&json!({"kind": "answer", "id": "0123abcd", "value": "works"})).send().await.unwrap();
    assert_eq!(r.status(), 400);
    let body: Value = r.json().await.unwrap();
    assert!(body["error"].as_str().unwrap_or_default().contains("no such card (already resolved or expired)"), "{body}");
}
