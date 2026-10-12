use std::sync::{Arc, Mutex};

use clawft_kernel::exposure::{Audience, ExposureKind, ExposureRegistry, ExposureSpec};
use serde_json::{Value, json};

use super::*;

fn reg() -> Arc<ExposureRegistry> {
    let r = ExposureRegistry::new("n-test");
    let mut s = ExposureSpec::new("tts.orpheus", ExposureKind::Tts, "cpu", "openai-audio");
    s.audience = Audience::Paired;
    r.register(s);
    r.set_announced("tts.orpheus", true);
    Arc::new(r)
}

fn act(payload: Value) -> Action {
    Action { id: "a1".into(), kind: KIND.into(), payload, created_at: None }
}

async fn run(r: &Arc<ExposureRegistry>, payload: Value) -> ActionOutcome {
    ExposeSetHandler::new(r.clone()).handle(&act(payload)).await
}

#[tokio::test]
async fn off_withdraws_at_once_and_records_the_dashboard() {
    let r = reg();
    let seen = Arc::new(Mutex::new(Vec::<Value>::new()));
    let s2 = seen.clone();
    r.set_audit(Arc::new(move |_, v| s2.lock().unwrap().push(v)));
    let o = run(&r, json!({"id": "tts.orpheus", "state": "off"})).await;
    assert_eq!(o.status, "succeeded", "{:?}", o.result);
    assert_eq!((o.result["state"].clone(), o.result["changed"].clone()), (json!("off"), json!(true)));
    assert_eq!(r.drain_withdrawals(), vec!["tts.orpheus".to_string()]);
    assert_eq!(seen.lock().unwrap()[0]["actor"], "dashboard");
}

#[tokio::test]
async fn pause_withdraws_and_reports_paused() {
    let r = reg();
    let o = run(&r, json!({"id": "tts.orpheus", "state": "paused"})).await;
    assert_eq!(o.status, "succeeded");
    assert_eq!(o.result["state"], "paused");
    assert_eq!(r.drain_withdrawals(), vec!["tts.orpheus".to_string()]);
    assert!(r.check_serve("tts.orpheus").is_err());
}

#[tokio::test]
async fn on_is_refused_with_a_reason_and_changes_nothing() {
    let r = reg();
    let o = run(&r, json!({"id": "tts.orpheus", "state": "on"})).await;
    assert_eq!((o.status, o.result["refused"].clone()), ("failed", json!(true)));
    assert!(o.result["error"].as_str().unwrap().contains("not on"));
    assert_eq!(r.get("tts.orpheus").unwrap().state.as_str(), "on");
    assert!(r.drain_withdrawals().is_empty());
}

#[tokio::test]
async fn unknown_id_state_and_fields_are_refused() {
    let r = reg();
    for p in [
        json!({"id": "nope", "state": "off"}),
        json!({"id": "tts.orpheus", "state": "sideways"}),
        json!({"id": "tts.orpheus", "state": "off", "audience": "public"}),
        json!({"id": "tts.orpheus"}),
        Value::Null,
    ] {
        let o = run(&r, p.clone()).await;
        assert_eq!((o.status, o.result["refused"].clone()), ("failed", json!(true)), "{p}");
    }
    assert_eq!(r.get("tts.orpheus").unwrap().state.as_str(), "on");
}

#[tokio::test]
async fn master_switch_off_refuses() {
    let r = reg();
    r.set_master(false);
    let o = run(&r, json!({"id": "tts.orpheus", "state": "off"})).await;
    assert_eq!(o.status, "failed");
    assert!(o.result["error"].as_str().unwrap().contains("master switch"));
}

#[tokio::test]
async fn local_off_stays_off() {
    let r = reg();
    r.set_state("tts.orpheus", ExposureState::Off, "cli").unwrap();
    let o = run(&r, json!({"id": "tts.orpheus", "state": "paused"})).await;
    assert_eq!(o.status, "succeeded");
    assert_eq!((o.result["state"].clone(), o.result["changed"].clone()), (json!("off"), json!(false)));
    assert_eq!(r.get("tts.orpheus").unwrap().state.as_str(), "off");
}
