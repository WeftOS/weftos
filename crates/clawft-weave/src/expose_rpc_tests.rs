use clawft_kernel::exposure::{Audience, ExposureKind, ExposureRegistry, ExposureSpec, NewActivity};
use serde_json::{Value, json};

use super::handle_with;

fn reg() -> ExposureRegistry {
    let r = ExposureRegistry::new("n-test");
    for id in ["tts.orpheus", "harness.meta"] {
        let mut s = ExposureSpec::new(id, ExposureKind::Tts, "cpu", "openai-audio");
        s.audience = Audience::Paired;
        r.register(s);
        r.set_announced(id, true);
    }
    r
}

async fn ok(r: &ExposureRegistry, m: &str, p: Value) -> Value {
    let resp = handle_with(r, m, p).await;
    assert!(resp.ok, "{m}: {:?}", resp.error);
    resp.result.unwrap()
}

async fn err(r: &ExposureRegistry, m: &str, p: Value) -> String {
    let resp = handle_with(r, m, p).await;
    assert!(!resp.ok, "{m} should refuse");
    resp.error.unwrap()
}

#[tokio::test]
async fn list_shows_served_and_why_not() {
    let r = reg();
    let v = ok(&r, "expose.list", Value::Null).await;
    assert_eq!(v["enabled"], true);
    assert_eq!(v["exposures"].as_array().unwrap().len(), 2);
    assert_eq!(v["exposures"][0]["served"], true);
    ok(&r, "expose.set", json!({"id": "tts.orpheus", "state": "paused"})).await;
    let v = ok(&r, "expose.list", Value::Null).await;
    let row = v["exposures"].as_array().unwrap().iter().find(|e| e["id"] == "tts.orpheus").unwrap().clone();
    assert_eq!((row["served"].clone(), row["refused"].clone()), (json!(false), json!("paused locally")));
}

#[tokio::test]
async fn set_on_off_pause_and_audience() {
    let r = reg();
    let v = ok(&r, "expose.set", json!({"id": "tts.orpheus", "state": "off"})).await;
    assert_eq!(v["state"], "off");
    assert_eq!(r.drain_withdrawals(), vec!["tts.orpheus".to_string()]);
    let v = ok(&r, "expose.set", json!({"id": "tts.orpheus", "state": "on", "audience": "local"})).await;
    assert_eq!((v["state"].clone(), v["audience"].clone(), v["served"].clone()), (json!("on"), json!("local"), json!(false)));
    assert_eq!(v["refused"], "audience is local", "on with a local audience still offers nothing");
}

#[tokio::test]
async fn off_all_switches_everything_off() {
    let r = reg();
    let v = ok(&r, "expose.off_all", json!({})).await;
    assert_eq!(v["switched_off"].as_array().unwrap().len(), 2);
    assert!(r.list().iter().all(|e| e.state.as_str() == "off"));
    assert_eq!(r.drain_withdrawals().len(), 2);
    let v = ok(&r, "expose.off_all", json!({})).await;
    assert!(v["switched_off"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn malformed_requests_are_refused_never_answered_empty() {
    let r = reg();
    assert!(err(&r, "expose.set", json!({"id": "nope", "state": "off"})).await.contains("unknown exposure"));
    assert!(err(&r, "expose.set", json!({"id": "tts.orpheus", "state": "maybe"})).await.contains("unknown state"));
    assert!(err(&r, "expose.set", json!({"id": "tts.orpheus", "state": "on", "audience": "everyone"})).await.contains("unknown audience"));
    assert!(err(&r, "expose.set", json!({"id": "tts.orpheus"})).await.contains("invalid params"));
    assert!(err(&r, "expose.set", json!({"id": "tts.orpheus", "state": "on", "extra": 1})).await.contains("invalid params"));
    assert!(err(&r, "expose.log", json!({"id": "nope"})).await.contains("unknown exposure"));
    assert!(err(&r, "expose.log", json!({"limit": 0})).await.contains("limit"));
    assert!(err(&r, "expose.log", json!({"limit": 5000})).await.contains("limit"));
    assert!(err(&r, "expose.bogus", json!({})).await.contains("unknown method"));
    // A refused audience left the exposure untouched.
    assert_eq!(r.get("tts.orpheus").unwrap().audience, Audience::Paired);
}

#[tokio::test]
async fn log_returns_records_and_the_chain_verdict() {
    let r = reg();
    for outcome in ["ok", "refused:paused locally"] {
        r.record(NewActivity {
            exposure: "tts.orpheus".into(), caller_node: "n-peer".into(), caller_user: None,
            bytes_in: 4, bytes_out: 8, duration_ms: 2, outcome: outcome.into(),
        });
    }
    let v = ok(&r, "expose.log", json!({"id": "tts.orpheus", "limit": 10})).await;
    assert_eq!((v["chain_ok"].clone(), v["chain"].clone()), (json!(true), json!("ok")));
    assert_eq!(v["records"].as_array().unwrap().len(), 2);
    assert_eq!(v["records"][1]["outcome"], "refused:paused locally");
    let v = ok(&r, "expose.log", Value::Null).await;
    assert_eq!(v["records"].as_array().unwrap().len(), 2, "no params: the default window");
    let v = ok(&r, "expose.log", json!({"id": "harness.meta"})).await;
    assert!(v["records"].as_array().unwrap().is_empty());
}
