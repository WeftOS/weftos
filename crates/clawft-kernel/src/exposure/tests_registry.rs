use std::sync::{Arc, Mutex};

use super::registry::{ExposureRegistry, ExposureSpec};
use super::{Audience, ExposureKind, ExposureState, NewActivity, Refusal, Retention};

fn spec(id: &str) -> ExposureSpec {
    let mut s = ExposureSpec::new(id, ExposureKind::Inference, "accel.gpu.metal", "openai-chat");
    s.audience = Audience::Paired;
    s
}

fn served(r: &ExposureRegistry, id: &str) -> Result<(), Refusal> {
    r.set_announced(id, true);
    r.check_serve(id)
}

fn tmpdir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("expose-reg-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn default_audience_is_local_and_local_is_never_served() {
    let s = ExposureSpec::new("tts.orpheus", ExposureKind::Tts, "cpu", "openai-audio");
    assert_eq!(s.audience, Audience::Local);
    let r = ExposureRegistry::new("n-a");
    r.register(s);
    r.set_announced("tts.orpheus", true);
    assert_eq!(r.check_serve("tts.orpheus"), Err(Refusal::LocalOnly));
}

#[test]
fn unregistered_and_unannounced_are_refused() {
    let r = ExposureRegistry::new("n-a");
    assert_eq!(r.check_serve("infer.chat"), Err(Refusal::Unregistered));
    r.register(spec("infer.chat"));
    assert_eq!(r.check_serve("infer.chat"), Err(Refusal::NotAnnounced), "reachable but not announced");
    assert_eq!(served(&r, "infer.chat"), Ok(()));
}

#[test]
fn off_and_pause_refuse_and_queue_a_withdrawal() {
    let r = ExposureRegistry::new("n-a");
    r.register(spec("infer.chat"));
    r.register(spec("infer.fast"));
    served(&r, "infer.chat").unwrap();
    served(&r, "infer.fast").unwrap();
    r.set_state("infer.chat", ExposureState::Off, "cli").unwrap();
    r.set_state("infer.fast", ExposureState::Paused, "cli").unwrap();
    assert_eq!(r.check_serve("infer.chat"), Err(Refusal::Off));
    assert_eq!(r.check_serve("infer.fast"), Err(Refusal::Paused));
    assert_eq!(r.drain_withdrawals(), vec!["infer.chat".to_string(), "infer.fast".to_string()]);
    assert!(r.drain_withdrawals().is_empty());
    r.set_state("infer.chat", ExposureState::On, "cli").unwrap();
    assert_eq!(r.check_serve("infer.chat"), Ok(()));
}

#[test]
fn unknown_id_is_refused_not_ignored() {
    let r = ExposureRegistry::new("n-a");
    assert!(r.set_state("nope", ExposureState::Off, "cli").unwrap_err().contains("unknown exposure"));
    assert!(r.set_audience("nope", Audience::Paired, "cli").is_err());
}

#[test]
fn off_all_switches_everything_and_reports_what_changed() {
    let r = ExposureRegistry::new("n-a");
    for id in ["infer.a", "infer.b", "infer.c"] {
        r.register(spec(id));
        served(&r, id).unwrap();
    }
    r.set_state("infer.c", ExposureState::Off, "cli").unwrap();
    r.drain_withdrawals();
    let changed = r.off_all("cli");
    assert_eq!(changed, vec!["infer.a".to_string(), "infer.b".to_string()]);
    assert!(r.list().iter().all(|e| e.state == ExposureState::Off));
    assert_eq!(r.drain_withdrawals().len(), 2);
}

#[test]
fn master_switch_refuses_everything_and_withdraws_announced() {
    let r = ExposureRegistry::new("n-a");
    r.register(spec("infer.chat"));
    served(&r, "infer.chat").unwrap();
    r.set_master(false);
    assert_eq!(r.check_serve("infer.chat"), Err(Refusal::MasterOff));
    assert_eq!(r.check_serve("anything"), Err(Refusal::MasterOff));
    assert_eq!(r.drain_withdrawals(), vec!["infer.chat".to_string()]);
    r.set_master(true);
    assert_eq!(r.check_serve("infer.chat"), Ok(()));
}

#[test]
fn local_override_survives_reregistration_and_restart() {
    let d = tmpdir("persist");
    {
        let r = ExposureRegistry::open("n-a", &d, Retention::default(), 1).unwrap();
        r.register(spec("infer.chat"));
        r.set_state("infer.chat", ExposureState::Off, "cli").unwrap();
        // Config registers it as on again: the local decision wins.
        assert_eq!(r.register(spec("infer.chat")).state, ExposureState::Off);
    }
    let r = ExposureRegistry::open("n-a", &d, Retention::default(), 2).unwrap();
    assert_eq!(r.register(spec("infer.chat")).state, ExposureState::Off, "stays off after restart");
    r.set_state("infer.chat", ExposureState::On, "cli").unwrap();
    let r2 = ExposureRegistry::open("n-a", &d, Retention::default(), 3).unwrap();
    assert_eq!(r2.register(spec("infer.chat")).state, ExposureState::On);
}

#[test]
fn corrupt_state_file_is_refused() {
    let d = tmpdir("corrupt");
    std::fs::write(d.join("exposures.json"), "{not json").unwrap();
    assert!(ExposureRegistry::open("n-a", &d, Retention::default(), 1).is_err());
}

#[test]
fn narrowing_the_audience_to_local_withdraws() {
    let r = ExposureRegistry::new("n-a");
    r.register(spec("infer.chat"));
    served(&r, "infer.chat").unwrap();
    r.set_audience("infer.chat", Audience::Local, "cli").unwrap();
    assert_eq!(r.check_serve("infer.chat"), Err(Refusal::LocalOnly));
    assert_eq!(r.drain_withdrawals(), vec!["infer.chat".to_string()]);
}

#[test]
fn audience_parses_and_refuses_garbage() {
    assert_eq!(Audience::parse("local"), Ok(Audience::Local));
    assert_eq!(Audience::parse("nodes:a, b"), Ok(Audience::Nodes(vec!["a".into(), "b".into()])));
    assert_eq!(Audience::parse("project:p1"), Ok(Audience::Project("p1".into())));
    for bad in ["", "world", "nodes:", "project:", "paired:x"] {
        assert!(Audience::parse(bad).is_err(), "{bad:?}");
    }
    assert!(ExposureKind::parse("cog-api").is_ok());
    assert!(ExposureKind::parse("bogus").is_err());
}

#[test]
fn changes_and_failed_requests_reach_the_audit_sink() {
    let r = ExposureRegistry::new("n-a");
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let s2 = seen.clone();
    r.set_audit(Arc::new(move |k, _| s2.lock().unwrap().push(k.to_string())));
    r.register(spec("infer.chat"));
    r.set_state("infer.chat", ExposureState::Off, "cli").unwrap();
    r.record(NewActivity {
        exposure: "infer.chat".into(),
        caller_node: "n-b".into(),
        caller_user: None,
        bytes_in: 1,
        bytes_out: 0,
        duration_ms: 1,
        outcome: "refused:off".into(),
    });
    let k = seen.lock().unwrap().clone();
    assert_eq!(k, vec!["exposure.change", "exposure.activity.anchor"]);
}

#[test]
fn report_has_counts_and_last_used_but_no_bodies() {
    let r = ExposureRegistry::new("n-a");
    r.register(spec("infer.chat"));
    r.register(spec("infer.idle"));
    let now = super::registry::now_ms();
    for outcome in ["ok", "ok", "refused:off"] {
        r.record(NewActivity {
            exposure: "infer.chat".into(),
            caller_node: "n-b".into(),
            caller_user: Some("u".into()),
            bytes_in: 5,
            bytes_out: 6,
            duration_ms: 7,
            outcome: outcome.into(),
        });
    }
    let v = r.report(now + 1_000);
    assert_eq!(v["enabled"], true);
    let items = v["items"].as_array().unwrap();
    let chat = items.iter().find(|i| i["id"] == "infer.chat").unwrap();
    assert_eq!((chat["n24"].as_u64(), chat["err24"].as_u64()), (Some(3), Some(1)));
    assert!(chat["last_used"].is_u64());
    assert_eq!(chat["audience"], "paired");
    let idle = items.iter().find(|i| i["id"] == "infer.idle").unwrap();
    assert!(idle["last_used"].is_null() && idle["n24"] == 0);
    assert!(!v.to_string().contains("n-b"), "caller identity stays on the node");
}
