//! `Glance::apply_at` over recorded streams that contain `card` events: a
//! live card owns task/next/alert, expiry and clear fall back to the
//! derivation, and agent state keeps flowing.

use chrono::{DateTime, Duration, Utc};
use weftos_cos::events::Event;
use weftos_cos::glance::{Glance, SseParser};

fn t0() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-25T09:00:00Z").unwrap().with_timezone(&Utc)
}

fn events(stream: &str) -> Vec<Event> {
    SseParser::default().feed(stream)
}

const STREAM: &str = r#"data: {"kind":"status","task":"voice turn","model":"local/glm","lanes_running":0}

data: {"kind":"message","turn_id":"0b3f7c1e5a9d4e2f8c6a1d7b3e9f5a20","text":"All quiet","final":true}

data: {"kind":"card","card":"needs_you","title":"Ship rc.16?","detail":"A: ship / B: soak 1 day","alert":"rc blocks","ttl_minutes":10,"reason":"release"}

data: {"kind":"state","state":"thinking"}

data: {"kind":"future-kind","x":1}

data: {"kind":"card","card":"clear","reason":"decided"}

"#;

#[test]
fn live_card_owns_fields_until_clear_and_state_still_flows() {
    let evs = events(STREAM);
    // The unknown kind is still dropped: status, message, card, state, clear.
    assert_eq!(evs.len(), 5);
    let mut g = Glance::default();
    g.apply_at(&evs[0], t0());
    g.apply_at(&evs[1], t0());
    assert_eq!((g.task.as_str(), g.next.as_str()), ("voice turn", "All quiet"));
    g.apply_at(&evs[2], t0());
    assert_eq!((g.task.as_str(), g.next.as_str(), g.alert.as_str()), ("NEEDS YOU: Ship rc.16?", "A: ship / B: soak 1 day", "rc blocks"));
    g.apply_at(&evs[3], t0());
    assert_eq!(g.state, "thinking");
    assert_eq!(g.task, "NEEDS YOU: Ship rc.16?", "a new turn does not drop the card");
    g.apply_at(&evs[4], t0() + Duration::minutes(1));
    assert_eq!((g.task.as_str(), g.next.as_str(), g.alert.as_str()), ("voice turn", "All quiet", ""));
}

#[test]
fn expiry_falls_back_and_tick_reports_it_once() {
    let evs = events(STREAM);
    let mut g = Glance::default();
    for e in &evs[..3] {
        g.apply_at(e, t0());
    }
    assert!(!g.tick(t0() + Duration::minutes(9)), "still live");
    assert_eq!(g.task, "NEEDS YOU: Ship rc.16?");
    assert!(g.tick(t0() + Duration::minutes(10)), "expiry is a change the bridge must notice");
    assert_eq!((g.task.as_str(), g.next.as_str()), ("voice turn", "All quiet"));
    assert!(!g.tick(t0() + Duration::minutes(11)), "reported once");
}

#[test]
fn a_zero_ttl_uses_the_kind_default_and_prefixes_differ() {
    let mut g = Glance::default();
    for (kind, prefix, ttl) in [("now", "NOW", 120), ("waiting_on", "WAITING", 480), ("eod", "EOD", 720)] {
        let ev = events(&format!("data: {{\"kind\":\"card\",\"card\":\"{kind}\",\"title\":\"T\"}}\n"));
        g.apply_at(&ev[0], t0());
        assert_eq!(g.task, format!("{prefix}: T"));
        assert!(!g.tick(t0() + Duration::minutes(ttl - 1)));
        assert!(g.tick(t0() + Duration::minutes(ttl)));
    }
}

#[test]
fn blocked_reason_beats_the_card_alert() {
    let mut g = Glance::default();
    g.apply_at(&events("data: {\"kind\":\"card\",\"card\":\"now\",\"title\":\"T\",\"alert\":\"a\"}\n")[0], t0());
    g.apply_at(&Event::blocked("model down"), t0());
    assert_eq!((g.state.as_str(), g.alert.as_str()), ("blocked", "model down"));
}

#[test]
fn card_text_goes_out_as_bounded_ascii() {
    let mut g = Glance::default();
    let title = format!("caf\u{e9} \u{201c}ship\u{201d}? {}", "z".repeat(200));
    let ev = Event::Card { card: weftos_cos::events::CardKind::NeedsYou, title, detail: "A \u{2014} B".into(), alert: String::new(), id: String::new(), options: vec![], ttl_minutes: 5, reason: String::new() };
    g.apply_at(&ev, t0());
    let line = g.to_line("09:00");
    assert!(line.is_ascii() && line.matches('\n').count() == 1);
    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
    let task = v["task"].as_str().unwrap();
    assert!(task.starts_with("NEEDS YOU: caf? \"ship\"? zzz") && task.len() <= 120 && task.ends_with("..."));
    assert_eq!(v["next"], "A - B");
}

#[test]
fn redraw_flag_is_sent_only_when_set_and_is_not_content() {
    let mut g = Glance::default();
    let plain = g.to_line("09:00");
    assert!(!plain.contains("redraw"));
    let before = g.clone();
    g.redraw = true;
    assert!(g.to_line("09:00").contains("\"redraw\":true"));
    assert_eq!(before.content(), g.content());
}
