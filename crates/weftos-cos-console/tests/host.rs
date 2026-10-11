//! The console host against a fake agent over real loopback sockets:
//! tokens stream into the log, an `ask` shows the survey and a tap posts
//! the answer, push-to-talk edges post to the agent and the transcript
//! lands, typing works without a mic, and an unreachable agent shows
//! Blocked until it comes back.

mod fake_agent;

use std::time::{Duration, Instant};

use fake_agent::FakeAgent;
use serde_json::json;
use weftos_cos_console::chat_log::Role;
use weftos_cos_console::frame::open_sim_unmetered;
use weftos_cos_console::host::{Host, HostConfig, OFFLINE_REASON};
use weftos_cos_console::guide::BUTTON_Y;
use weftos_cos_console::theme::{ids, nid, GUIDE_HEAD, GUIDE_SIDE, PAD};
use weftos_cos_console::{AgentState, Layout, LeafSim, SensorValue};
use weftos_display_client::{KEY_RETURN, SENSOR_KEY_CHAR, SENSOR_KEY_NAMED};
use weftos_leaf_scene::Primitive;

const WAIT: Duration = Duration::from_secs(5);

fn config(base: String) -> HostConfig {
    HostConfig { agent: base, typing: true, reconnect_min: Duration::from_millis(100), reconnect_max: Duration::from_millis(400) }
}

/// Step the host at frame pace until `done` or the timeout.
fn pump(host: &mut Host<'_>, now: &mut u32, mut done: impl FnMut(&Host<'_>) -> bool) -> bool {
    let deadline = Instant::now() + WAIT;
    loop {
        *now += 33;
        host.step(*now).unwrap();
        if done(host) {
            return true;
        }
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn texts(sim: &LeafSim) -> Vec<String> {
    sim.snapshot()
        .nodes
        .iter()
        .filter_map(|n| match &n.primitive {
            Primitive::Text { content, .. } => Some(content.clone()),
            _ => None,
        })
        .collect()
}

fn scene_has(sim: &LeafSim, needle: &str) -> bool {
    texts(sim).iter().any(|t| t.contains(needle))
}

fn agent_lines(host: &Host<'_>) -> Vec<String> {
    host.console().chat().entries().filter(|e| e.role == Role::Agent).map(|e| e.text.clone()).collect()
}

fn user_lines(host: &Host<'_>) -> Vec<String> {
    host.console().chat().entries().filter(|e| e.role == Role::User).map(|e| e.text.clone()).collect()
}

fn online(agent: &FakeAgent) -> (weftos_cos_console::DisplaySession, LeafSim, HostConfig) {
    let (s, sim) = open_sim_unmetered().unwrap();
    (s, sim, config(agent.base()))
}

#[test]
fn tokens_stream_into_the_log_and_the_final_message_completes_it() {
    let agent = FakeAgent::start();
    let (mut s, sim, cfg) = online(&agent);
    let mut host = Host::open(&mut s, cfg).unwrap();
    let mut now = 0;
    assert!(pump(&mut host, &mut now, |h| h.is_online()));

    agent.send(json!({"kind": "state", "state": "thinking"}));
    agent.send(json!({"kind": "status", "task": "triage", "model": "local/fast", "lanes_running": 2}));
    agent.send(json!({"kind": "token", "turn_id": "t1", "text": "Three "}));
    agent.send(json!({"kind": "token", "turn_id": "t1", "text": "things."}));
    assert!(pump(&mut host, &mut now, |h| agent_lines(h) == ["Three things."]));
    assert_eq!(*host.console().status().state(), AgentState::Speaking, "first token turns thinking into speaking");
    assert!(scene_has(&sim, "Three things."));
    assert!(scene_has(&sim, "model local/fast"));
    assert!(scene_has(&sim, "2 lanes running"));

    agent.send(json!({"kind": "message", "turn_id": "t1", "text": "Three things. Done.", "final": true}));
    agent.send(json!({"kind": "state", "state": "idle"}));
    assert!(pump(&mut host, &mut now, |h| *h.console().status().state() == AgentState::Idle));
    assert_eq!(agent_lines(&host), ["Three things. Done."], "the final message adds only what the tokens did not");

    // A later message on the same turn id is a new line, not an append.
    agent.send(json!({"kind": "message", "turn_id": "t1", "text": "I hit a problem: x", "final": true}));
    assert!(pump(&mut host, &mut now, |h| agent_lines(h).len() == 2));
}

#[test]
fn ask_shows_the_survey_and_a_tap_posts_the_answer() {
    let agent = FakeAgent::start();
    let (mut s, sim, cfg) = online(&agent);
    let mut host = Host::open(&mut s, cfg).unwrap();
    let mut now = 0;
    assert!(pump(&mut host, &mut now, |h| h.is_online()));

    agent.send(json!({"kind": "ask", "id": "q1", "question": "How did standup land?", "explain": "One tap.",
        "choices": [{"n": 1, "caption": "Clear"}, {"n": 2, "caption": "One open question"}, {"n": 3, "caption": "Rough"}],
        "index": 1, "total": 3}));
    assert!(pump(&mut host, &mut now, |h| h.console().layout() == Layout::Guide));
    assert!(scene_has(&sim, "How did standup land?"));
    assert!(scene_has(&sim, "Q1/3"));

    // Second button in the left column (the demo's row geometry).
    sim.tap(GUIDE_SIDE.x + 46, GUIDE_SIDE.y + 40 + 68 + 34);
    assert!(pump(&mut host, &mut now, |_| !agent.posts_to("/v1/cos/answer").is_empty()));
    assert_eq!(agent.posts_to("/v1/cos/answer"), [json!({"kind": "answer", "id": "q1", "value": 2})]);

    // A checklist toggle and a step both take their contract shapes.
    agent.send(json!({"kind": "checklist", "id": "rel", "title": "Release", "items": [{"id": "gate", "text": "Gate green", "done": false}]}));
    assert!(pump(&mut host, &mut now, |_| scene_has(&sim, "Gate green")));
    sim.tap(GUIDE_SIDE.x + 150, GUIDE_SIDE.y + 40 + 32);
    assert!(pump(&mut host, &mut now, |_| agent.posts_to("/v1/cos/answer").len() == 2));
    assert_eq!(agent.posts_to("/v1/cos/answer")[1], json!({"kind": "toggle", "id": "rel", "value": {"item_id": "gate", "done": true}}));
}

#[test]
fn back_on_a_later_question_posts_the_back_answer() {
    let agent = FakeAgent::start();
    let (mut s, sim, cfg) = online(&agent);
    let mut host = Host::open(&mut s, cfg).unwrap();
    let mut now = 0;
    assert!(pump(&mut host, &mut now, |h| h.is_online()));

    agent.send(json!({"kind": "ask", "id": "q2", "question": "Second question?", "explain": "",
        "choices": [{"n": 1, "caption": "Yes"}, {"n": 2, "caption": "No"}], "index": 2, "total": 3}));
    assert!(pump(&mut host, &mut now, |_| scene_has(&sim, "Second question?")));
    sim.tap(GUIDE_HEAD.x + PAD + 10, BUTTON_Y + 10);
    assert!(pump(&mut host, &mut now, |_| !agent.posts_to("/v1/cos/answer").is_empty()));
    assert_eq!(agent.posts_to("/v1/cos/answer"), [json!({"kind": "back", "id": "q2", "value": null})]);
}

#[test]
fn ptt_sensor_events_post_push_to_talk_and_the_transcript_lands() {
    let agent = FakeAgent::start();
    agent.set_ptt_up_reply(json!({"active": false, "posted": {"text": "hello from the mic", "turn_id": "t9"}}));
    let (mut s, sim, cfg) = online(&agent);
    let mut host = Host::open(&mut s, cfg).unwrap();
    let mut now = 0;
    assert!(pump(&mut host, &mut now, |h| h.is_online()));
    agent.send(json!({"kind": "status", "task": "", "model": "m", "lanes_running": 0}));
    assert!(pump(&mut host, &mut now, |h| h.console().status().status().model == "m"));

    sim.ptt(true);
    assert!(pump(&mut host, &mut now, |_| agent.posts_to("/v1/cos/ptt") == [json!({"down": true})]));
    assert_eq!(*host.console().status().state(), AgentState::Listening);
    assert!(host.console().status().status().ptt_active, "the indicator follows the button");

    sim.ptt(false);
    assert!(pump(&mut host, &mut now, |h| user_lines(h) == ["hello from the mic"]));
    assert_eq!(agent.posts_to("/v1/cos/ptt"), [json!({"down": true}), json!({"down": false})]);
    assert!(!host.console().status().status().ptt_active);

    // A release that heard nothing says so and returns to idle.
    agent.set_ptt_up_reply(json!({"active": false, "nothing": "capture shorter than 250 ms"}));
    sim.ptt(true);
    sim.ptt(false);
    assert!(pump(&mut host, &mut now, |h| agent_lines(h).iter().any(|l| l.contains("heard nothing"))));
    assert_eq!(*host.console().status().state(), AgentState::Idle);
}

#[test]
fn enter_opens_a_typed_line_that_posts_a_typed_utterance() {
    let agent = FakeAgent::start();
    let (mut s, sim, cfg) = online(&agent);
    let mut host = Host::open(&mut s, cfg).unwrap();
    let mut now = 0;
    assert!(pump(&mut host, &mut now, |h| h.is_online()));

    sim.sensor(SENSOR_KEY_NAMED, SensorValue::Text(KEY_RETURN.into()));
    assert!(pump(&mut host, &mut now, |h| h.typing() == Some("")));
    assert!(scene_has(&sim, "> _"));
    for ch in ["h", "i", " ", "t", "h", "e", "r", "e", "e"] {
        sim.sensor(SENSOR_KEY_CHAR, SensorValue::Text(ch.into()));
    }
    sim.sensor(SENSOR_KEY_NAMED, SensorValue::Text("Backspace".into()));
    // Space while typing is a space, not the button.
    sim.ptt(true);
    sim.ptt(false);
    assert!(pump(&mut host, &mut now, |h| h.typing() == Some("hi there")));
    assert!(scene_has(&sim, "> hi there_"));
    assert!(agent.posts_to("/v1/cos/ptt").is_empty());

    sim.sensor(SENSOR_KEY_NAMED, SensorValue::Text(KEY_RETURN.into()));
    assert!(pump(&mut host, &mut now, |_| !agent.posts_to("/v1/cos/utterance").is_empty()));
    assert_eq!(agent.posts_to("/v1/cos/utterance"), [json!({"text": "hi there", "source": "typed"})]);
    assert_eq!(user_lines(&host), ["hi there"]);
    assert_eq!(host.typing(), None);
    assert!(!sim.has_node(nid(ids::CHAT_TYPED_TEXT)), "the typed line is gone after send");
    assert!(!sim.has_node(nid(ids::CHAT_TYPED_BG)));
}

#[test]
fn offline_agent_shows_blocked_then_recovers() {
    let port = FakeAgent::free_port();
    let (mut s, _sim) = open_sim_unmetered().unwrap();
    let mut host = Host::open(&mut s, config(format!("http://{port}"))).unwrap();
    let mut now = 0;
    assert!(pump(&mut host, &mut now, |h| *h.console().status().state() == AgentState::Blocked(OFFLINE_REASON.into())));
    assert!(!host.is_online());
    std::thread::sleep(Duration::from_millis(300));
    host.step(now).unwrap();
    assert_eq!(*host.console().status().state(), AgentState::Blocked(OFFLINE_REASON.into()), "stays blocked while retrying");

    let agent = FakeAgent::start_at(port);
    assert!(pump(&mut host, &mut now, |h| h.is_online()), "reconnects with backoff");
    assert_eq!(*host.console().status().state(), AgentState::Idle);
    assert_eq!(agent.stream_count(), 1);
}

#[test]
fn a_stream_drop_mid_turn_finishes_the_line_and_the_next_token_starts_a_new_one() {
    let agent = FakeAgent::start();
    let (mut s, _sim, cfg) = online(&agent);
    let mut host = Host::open(&mut s, cfg).unwrap();
    let mut now = 0;
    assert!(pump(&mut host, &mut now, |h| h.is_online()));

    agent.send(json!({"kind": "token", "turn_id": "t1", "text": "Half"}));
    assert!(pump(&mut host, &mut now, |h| agent_lines(h) == ["Half"]));
    agent.drop_streams();
    assert!(pump(&mut host, &mut now, |h| *h.console().status().state() == AgentState::Blocked(OFFLINE_REASON.into())));
    assert!(pump(&mut host, &mut now, |h| h.is_online()));

    agent.send(json!({"kind": "token", "turn_id": "t1", "text": "New"}));
    assert!(pump(&mut host, &mut now, |h| agent_lines(h).len() == 2));
    assert_eq!(agent_lines(&host), ["Half", "New"]);
}
