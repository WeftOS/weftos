//! Phase B3: governance on top of the manifest rules, cross-project grants,
//! the activity sink, the refusal rate limit and the repeated-refusal stop.

use std::sync::{Arc, Mutex};

use clawft_types::cog_topics::{InputPolicy, TopicClass};
use serde_json::json;

use crate::audit::Tracker;
use crate::broker::{Broker, BrokerConfig, InstanceSpec, Session};
use crate::policy::{
    BusAction, BusActivity, BusHooks, BusPolicy, BusRequest, BusSink, Grant, Outcome, RefusalLimits,
};
use crate::proto::{ErrorCode, InputSpec, OutputSpec};

const P1: &str = "01J9ZXW0PRJCTAAAAAAAAAAAAA";
const P2: &str = "01J9ZXW0PRJCTBBBBBBBBBBBBB";

#[derive(Default)]
struct Log {
    rows: Mutex<Vec<BusActivity>>,
    stopped: Mutex<Vec<(String, u32)>>,
}

impl BusSink for Log {
    fn record(&self, a: &BusActivity) {
        self.rows.lock().unwrap().push(a.clone());
    }
    fn stopped(&self, id: &str, n: u32) {
        self.stopped.lock().unwrap().push((id.into(), n));
    }
}

impl Log {
    fn actions(&self) -> Vec<(String, String, bool)> {
        let rows = self.rows.lock().unwrap();
        rows.iter()
            .map(|a| (a.action.clone(), a.topic.clone(), a.outcome == Outcome::Ok))
            .collect()
    }
}

/// Denies `action` on topics starting with `prefix`, or for `cog`; grants
/// `grants` to every subscriber.
#[derive(Default)]
struct Rules {
    deny: Vec<(BusAction, &'static str)>,
    deny_cog: Option<&'static str>,
    grants: Vec<Grant>,
    asked: Mutex<Vec<(BusAction, String, Option<String>)>>,
}

impl BusPolicy for Rules {
    fn decide(&self, r: &BusRequest<'_>) -> Result<(), String> {
        self.asked.lock().unwrap().push((
            r.action,
            r.topic.into(),
            r.topic_project.map(String::from),
        ));
        if self.deny_cog == Some(r.cog_id) {
            return Err(format!("cog {} is denied", r.cog_id));
        }
        match self
            .deny
            .iter()
            .find(|(a, p)| *a == r.action && r.topic.starts_with(p))
        {
            Some(_) => Err(format!("{} is denied", r.topic)),
            None => Ok(()),
        }
    }
    fn grants(&self, _: &BusRequest<'_>) -> Vec<Grant> {
        self.grants.clone()
    }
}

fn hooks(policy: Rules, log: &Arc<Log>, limits: RefusalLimits) -> BusHooks {
    BusHooks {
        policy: Arc::new(policy),
        sink: log.clone(),
        limits,
    }
}

fn token(n: u8) -> String {
    format!("{n:02x}").repeat(32)
}

fn join(b: &Broker, n: u8, cog: &str, project: Option<&str>, writes: bool) -> Session {
    let outputs = vec![OutputSpec {
        topic: "telemetry".into(),
        schema: "telemetry.v1".into(),
        class: TopicClass::Reading,
    }];
    let inputs = vec![InputSpec {
        topic: "telemetry".into(),
        schema: "telemetry.v1".into(),
        policy: InputPolicy::DropOldest,
    }];
    let (outputs, inputs) = if writes {
        (outputs, vec![])
    } else {
        (vec![], inputs)
    };
    let spec = InstanceSpec {
        instance_id: format!("i{n}"),
        cog_id: cog.into(),
        project_id: project.map(String::from),
        uid: None,
        outputs,
        inputs,
    };
    b.register(spec, &token(n)).unwrap();
    b.authenticate(&token(n), None).unwrap()
}

fn pubr(b: &Broker, s: &Session, src: &str) -> Result<u64, crate::BusError> {
    b.publish(s, "telemetry", src, "telemetry.v1", 1, json!({"items": []}))
}

fn with(policy: Rules, limits: RefusalLimits) -> (Broker, Arc<Log>) {
    let log = Arc::new(Log::default());
    let b = Broker::new(BrokerConfig::new("n")).with_hooks(hooks(policy, &log, limits));
    (b, log)
}

#[test]
fn the_default_policy_permits_b1_and_logs_declare_subscribe_unsubscribe() {
    let (b, log) = with(Rules::default(), RefusalLimits::default());
    let mut w = join(&b, 1, "w", Some(P1), true);
    let mut r = join(&b, 2, "r", Some(P1), false);
    let outs = b.instance("i1").unwrap().outputs;
    b.declare(&mut w, outs, vec![]).unwrap();
    let spec = b.instance("i2").unwrap();
    b.declare(&mut r, vec![], spec.inputs.clone()).unwrap();
    let sub = b.subscribe(&mut r, "telemetry/*", "telemetry.v1").unwrap();
    pubr(&b, &w, "board").unwrap();
    assert_eq!(r.drain().len(), 1);
    b.unsubscribe(&mut r, sub).unwrap();
    assert_eq!(
        log.actions(),
        vec![
            ("cogbus.declare".into(), "telemetry".into(), true),
            ("cogbus.declare".into(), "telemetry".into(), true),
            ("cogbus.subscribe".into(), "telemetry/*".into(), true),
            ("cogbus.unsubscribe".into(), "telemetry/*".into(), true),
        ]
    );
}

#[test]
fn a_governance_deny_refuses_a_subscribe_and_is_logged() {
    let rules = Rules {
        deny: vec![(BusAction::Subscribe, "telemetry/secret")],
        ..Rules::default()
    };
    let (b, log) = with(rules, RefusalLimits::default());
    let mut r = join(&b, 2, "r", Some(P1), false);
    let e = b
        .subscribe(&mut r, "telemetry/secret", "telemetry.v1")
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::Denied);
    assert!(e.message.contains("cogbus.subscribe denied"), "{e}");
    assert!(!b.record_refusal(&e, "i2"));
    // Other topics of the kind are still allowed.
    b.subscribe(&mut r, "telemetry/board", "telemetry.v1")
        .unwrap();
    let rows = log.rows.lock().unwrap().clone();
    let refused = rows
        .iter()
        .find(|a| a.outcome != Outcome::Ok)
        .expect("refusal logged");
    assert_eq!(refused.action, "cogbus.subscribe");
    assert_eq!(refused.instance, "i2");
    assert_eq!(refused.topic, "telemetry/secret");
    assert_eq!(refused.project.as_deref(), Some(P1));
    assert!(
        matches!(&refused.outcome, Outcome::Refused { code, suppressed: 0, .. } if code == "denied")
    );
    assert_eq!(b.recent_refusals().len(), 1);
}

#[test]
fn a_deny_can_name_a_cog_and_gates_the_first_publish_and_declare() {
    let rules = Rules {
        deny_cog: Some("bad"),
        deny: vec![(BusAction::Publish, "telemetry/locked")],
        ..Rules::default()
    };
    let (b, _log) = with(rules, RefusalLimits::default());
    let mut bad = join(&b, 1, "bad", Some(P1), true);
    assert_eq!(pubr(&b, &bad, "board").unwrap_err().code, ErrorCode::Denied);
    let outs = b.instance("i1").unwrap().outputs;
    assert_eq!(
        b.declare(&mut bad, outs, vec![]).unwrap_err().code,
        ErrorCode::Denied
    );
    // A denied publish leaves no topic behind.
    assert!(b.snapshot().is_empty());
    let good = join(&b, 2, "good", Some(P1), true);
    assert_eq!(
        pubr(&b, &good, "locked").unwrap_err().code,
        ErrorCode::Denied
    );
    pubr(&b, &good, "board").unwrap();
    pubr(&b, &good, "board").unwrap();
    assert_eq!(b.snapshot().len(), 1);
}

#[test]
fn only_the_first_publish_on_a_topic_asks_governance() {
    let rules = Arc::new(Rules::default());
    let log = Arc::new(Log::default());
    let b = Broker::new(BrokerConfig::new("n")).with_hooks(BusHooks {
        policy: rules.clone(),
        sink: log,
        limits: RefusalLimits::default(),
    });
    let w = join(&b, 1, "w", Some(P1), true);
    for _ in 0..5 {
        pubr(&b, &w, "board").unwrap();
    }
    pubr(&b, &w, "other").unwrap();
    let asked = rules.asked.lock().unwrap();
    let publishes: Vec<_> = asked
        .iter()
        .filter(|(a, ..)| *a == BusAction::Publish)
        .map(|(_, t, _)| t.as_str())
        .collect();
    assert_eq!(publishes, vec!["telemetry/board", "telemetry/other"]);
}

#[test]
fn cross_project_needs_a_grant() {
    // Without a grant: refused for a named topic, filtered for a wildcard.
    let (b, _) = with(Rules::default(), RefusalLimits::default());
    let w = join(&b, 1, "w", Some(P1), true);
    let mut r = join(&b, 2, "r", Some(P2), false);
    pubr(&b, &w, "board").unwrap();
    let e = b
        .subscribe(&mut r, "telemetry/board", "telemetry.v1")
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::CrossProject);
    b.subscribe(&mut r, "telemetry/*", "telemetry.v1").unwrap();
    pubr(&b, &w, "board").unwrap();
    assert!(r.drain().is_empty());

    // With a grant for P1's `telemetry/board` only.
    let rules = Rules {
        grants: vec![Grant {
            project: P1.into(),
            source: Some("board".into()),
        }],
        ..Rules::default()
    };
    let (b, _) = with(rules, RefusalLimits::default());
    let w = join(&b, 1, "w", Some(P1), true);
    let mut r = join(&b, 2, "r", Some(P2), false);
    pubr(&b, &w, "board").unwrap();
    b.subscribe(&mut r, "telemetry/board", "telemetry.v1")
        .unwrap();
    b.subscribe(&mut r, "telemetry/*", "telemetry.v1").unwrap();
    pubr(&b, &w, "board").unwrap();
    pubr(&b, &w, "other").unwrap();
    let got: Vec<String> = r.drain().into_iter().map(|d| d.topic).collect();
    assert_eq!(
        got,
        vec!["telemetry/board", "telemetry/board"],
        "the ungranted source stays filtered"
    );
    let e = b
        .subscribe(&mut r, "telemetry/other", "telemetry.v1")
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::CrossProject);
}

#[test]
fn identical_refusals_are_logged_once_per_window_with_a_count() {
    let lim = RefusalLimits {
        dedupe_ms: 10_000,
        stop_after: 0,
        window_ms: 60_000,
    };
    let mut t = Tracker::default();
    assert_eq!(t.note(&lim, "i1", "denied", "a/b", 0).log, Some(0));
    for ms in 1..=5 {
        assert_eq!(t.note(&lim, "i1", "denied", "a/b", ms).log, None);
    }
    // Another topic or code is its own key.
    assert_eq!(t.note(&lim, "i1", "denied", "a/c", 6).log, Some(0));
    assert_eq!(t.note(&lim, "i1", "undeclared", "a/b", 7).log, Some(0));
    // After the window the next one is logged with the folded count.
    assert_eq!(t.note(&lim, "i1", "denied", "a/b", 10_000).log, Some(5));
    assert_eq!(t.note(&lim, "i1", "denied", "a/b", 10_001).log, None);

    // Through the broker: a flood of one refusal is one log row.
    let rules = Rules {
        deny: vec![(BusAction::Subscribe, "telemetry/")],
        ..Rules::default()
    };
    let (b, log) = with(rules, lim);
    let mut r = join(&b, 2, "r", Some(P1), false);
    for _ in 0..50 {
        let e = b
            .subscribe(&mut r, "telemetry/x", "telemetry.v1")
            .unwrap_err();
        b.record_refusal(&e, "i2");
    }
    assert_eq!(log.rows.lock().unwrap().len(), 1);
}

#[test]
fn repeated_refusals_stop_the_instance_on_the_bus() {
    let lim = RefusalLimits {
        dedupe_ms: 10_000,
        stop_after: 3,
        window_ms: 60_000,
    };
    let (b, log) = with(Rules::default(), lim);
    let mut r = join(&b, 2, "r", Some(P1), false);
    let revoked = r.revoked();
    for n in 0..3 {
        // An undeclared kind: the manifest rule, not governance.
        let e = b
            .subscribe(&mut r, "detections/x", "detections.v1")
            .unwrap_err();
        assert_eq!(e.code, ErrorCode::Undeclared);
        assert_eq!(b.record_refusal(&e, "i2"), n == 2, "stopped on the third");
    }
    assert!(
        revoked.has_changed().unwrap_or(true),
        "its connections are told"
    );
    assert!(b.instance("i2").is_none());
    assert_eq!(
        b.authenticate(&token(2), None).unwrap_err().code,
        ErrorCode::Unauthorized
    );
    assert_eq!(*log.stopped.lock().unwrap(), vec![("i2".to_string(), 3)]);
    let last = b.recent_refusals().pop().unwrap();
    assert_eq!(last.action, "cogbus.stop");
    assert_eq!(last.outcome, Outcome::Stopped { refusals: 3 });
    // The workload runtime can register it again (start after stop).
    let spec = InstanceSpec {
        instance_id: "i2".into(),
        cog_id: "r".into(),
        project_id: Some(P1.into()),
        uid: None,
        outputs: vec![],
        inputs: vec![],
    };
    b.register(spec, &token(2)).unwrap();
    assert!(b.authenticate(&token(2), None).is_ok());
    // Unknown tokens are logged but have no instance to stop.
    for _ in 0..5 {
        let e = b.authenticate(&token(9), None).unwrap_err();
        assert!(!b.record_refusal(&e, "-"));
    }
}

#[test]
fn refusals_outside_the_window_do_not_add_up() {
    let lim = RefusalLimits {
        dedupe_ms: 0,
        stop_after: 3,
        window_ms: 1_000,
    };
    let mut t = Tracker::default();
    assert_eq!(t.note(&lim, "i1", "x", "t", 0).stop, None);
    assert_eq!(t.note(&lim, "i1", "x", "t", 900).stop, None);
    assert_eq!(
        t.note(&lim, "i1", "x", "t", 1_850).stop,
        None,
        "the first fell out"
    );
    assert_eq!(t.note(&lim, "i1", "x", "t", 1_890).stop, Some(3));
    assert_eq!(t.note(&lim, "-", "x", "t", 1_895).stop, None);
    for ms in 0..10 {
        assert_eq!(t.note(&lim, "@ingest-bridge", "x", "t", 2_000 + ms).stop, None);
    }
}
