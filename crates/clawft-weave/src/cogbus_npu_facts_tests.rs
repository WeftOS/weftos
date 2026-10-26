use clawft_types::cog_topics::{InputPolicy, TopicClass};
use clawft_types::placement::{Capability, CapabilityId, Provenance};
use serde_json::json;
use weftos_cogbus::{Broker, BrokerConfig, InputSpec, InstanceSpec, OutputSpec};

use super::*;

fn body(depth: f64, busy: f64) -> Value {
    json!({"source_id": "venq-npu", "node_id": "0123456789abcdef0123456789abcdef", "t_ns": 1, "seq": 1, "items": [
        {"name": "queue.depth", "value": depth, "unit": ""},
        {"name": "npu.busy", "value": busy, "unit": ""},
        {"name": "mem.resident", "value": 5900.0, "unit": "MiB"},
        {"name": "mem.budget", "value": 8000.0, "unit": "MiB"},
        {"name": "model.loaded.qwen3_4b", "value": 4500.0, "unit": "MiB"},
        {"name": "model.loaded.gemma3_1b", "value": 1400.0, "unit": "MiB"},
    ]})
}

fn facts_with_npu() -> NodeFacts {
    let mut f = NodeFacts::new("n", 100, 60, 1);
    f.capabilities.push(Capability::new(
        CapabilityId::new("accel.npu.qualcomm").unwrap(),
        Provenance::Probed,
    ));
    f.capabilities.push(Capability::new(
        CapabilityId::new("mem.system").unwrap(),
        Provenance::Probed,
    ));
    f
}

fn attr<'a>(f: &'a NodeFacts, k: &str) -> Option<&'a AttrValue> {
    f.capabilities
        .iter()
        .find(|c| c.id.as_str() == "accel.npu.qualcomm")?
        .attrs
        .get(k)
}

#[test]
fn a_telemetry_body_becomes_a_status() {
    let s = parse(&body(3.0, 1.0), 50).unwrap();
    assert_eq!(
        (s.queue_depth, s.busy, s.mem_resident_mib, s.mem_budget_mib),
        (3, true, 5900, 8000)
    );
    assert_eq!(s.models, ["gemma3_1b", "qwen3_4b"]);
    assert!(
        parse(
            &json!({"items": [{"name": "x", "value": 1.0, "unit": ""}]}),
            0
        )
        .is_none()
    );
    assert!(parse(&json!({}), 0).is_none());
}

#[test]
fn a_fresh_status_lands_on_the_npu_capability() {
    let mut f = facts_with_npu();
    apply_status(&mut f, parse(&body(3.0, 1.0), 100).as_ref(), 110);
    assert_eq!(
        attr(&f, "service_state"),
        Some(&AttrValue::Str("up".into()))
    );
    assert_eq!(attr(&f, "service_queue_depth"), Some(&AttrValue::Int(3)));
    assert_eq!(attr(&f, "service_busy"), Some(&AttrValue::Bool(true)));
    assert_eq!(
        attr(&f, "service_models"),
        Some(&AttrValue::List(vec![
            AttrValue::Str("gemma3_1b".into()),
            AttrValue::Str("qwen3_4b".into())
        ]))
    );
    assert_eq!(
        attr(&f, "service_mem_budget_mib"),
        Some(&AttrValue::Int(8000))
    );
    // the facts still validate and serialize
    for c in &f.capabilities {
        c.validate().unwrap();
    }
    serde_json::to_string(&f).unwrap();
}

#[test]
fn a_stale_status_says_stale_and_carries_no_queue_depth() {
    let mut f = facts_with_npu();
    apply_status(&mut f, parse(&body(0.0, 0.0), 100).as_ref(), 110);
    assert!(attr(&f, "service_queue_depth").is_some());
    apply_status(
        &mut f,
        parse(&body(0.0, 0.0), 100).as_ref(),
        100 + STALE_SECS + 1,
    );
    assert_eq!(
        attr(&f, "service_state"),
        Some(&AttrValue::Str("stale".into()))
    );
    assert_eq!(attr(&f, "service_queue_depth"), None);
    assert_eq!(attr(&f, "service_models"), None);
}

#[test]
fn nothing_is_added_without_a_status_or_without_an_npu() {
    let mut f = facts_with_npu();
    apply_status(&mut f, None, 1);
    assert_eq!(attr(&f, "service_state"), None);
    let mut no_npu = NodeFacts::new("n", 1, 60, 1);
    no_npu.capabilities.push(Capability::new(
        CapabilityId::new("mem.system").unwrap(),
        Provenance::Probed,
    ));
    let before = no_npu.clone();
    apply_status(&mut no_npu, parse(&body(1.0, 0.0), 1).as_ref(), 2);
    assert_eq!(no_npu, before);
}

fn spec(id: &str, outputs: Vec<OutputSpec>) -> InstanceSpec {
    InstanceSpec {
        instance_id: id.into(),
        cog_id: id.into(),
        project_id: None,
        uid: None,
        outputs,
        inputs: vec![InputSpec {
            topic: "telemetry".into(),
            schema: "telemetry.v1".into(),
            policy: InputPolicy::DropOldest,
        }],
    }
}

fn out(topic: &str, schema: &str) -> OutputSpec {
    OutputSpec {
        topic: topic.into(),
        schema: schema.into(),
        class: TopicClass::Reading,
    }
}

#[tokio::test]
async fn only_the_cog_that_provides_the_service_feeds_the_facts() {
    let broker = Arc::new(Broker::new(BrokerConfig::new("0".repeat(32))));
    let handle = attach(&broker).unwrap();
    let tok = |c: char| c.to_string().repeat(40);
    broker
        .register(
            spec(
                "npu",
                vec![
                    out("svc.infer", "infer.v1"),
                    out("telemetry", "telemetry.v1"),
                ],
            ),
            &tok('a'),
        )
        .unwrap();
    broker
        .register(
            spec("other", vec![out("telemetry", "telemetry.v1")]),
            &tok('b'),
        )
        .unwrap();
    let npu = broker.authenticate(&tok('a'), None).unwrap();
    let other = broker.authenticate(&tok('b'), None).unwrap();
    *latest().lock().unwrap() = None;
    // a non-NPU cog reporting a queue depth is ignored
    broker
        .publish(
            &other,
            "telemetry",
            "other",
            "telemetry.v1",
            1,
            body(99.0, 1.0),
        )
        .unwrap();
    broker
        .publish(
            &npu,
            "telemetry",
            "venq-npu",
            "telemetry.v1",
            2,
            body(2.0, 1.0),
        )
        .unwrap();
    for _ in 0..100 {
        if latest().lock().unwrap().is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let got = latest()
        .lock()
        .unwrap()
        .clone()
        .expect("the NPU service's reading");
    assert_eq!(got.queue_depth, 2);
    handle.abort();
}
