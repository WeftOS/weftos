//! Built-in sinks and internal publishers (B4).

use clawft_types::cog_topics::{InputPolicy, TopicClass};
use serde_json::json;

use crate::broker::{Broker, BrokerConfig, InstanceSpec, Reading, Session};
use crate::proto::{ErrorCode, InputSpec, OutputSpec};

const P1: &str = "01J9ZXW0PRJCTAAAAAAAAAAAAA";
const P2: &str = "01J9ZXW0PRJCTBBBBBBBBBBBBB";
const KIND: &str = "store.vector";
const SCHEMA: &str = "store.vector.v1";

fn join(b: &Broker, n: u8, project: &str, writes: bool) -> Session {
    let out = OutputSpec { topic: KIND.into(), schema: SCHEMA.into(), class: TopicClass::Reading };
    let inp = InputSpec { topic: KIND.into(), schema: SCHEMA.into(), policy: InputPolicy::DropOldest };
    let spec = InstanceSpec {
        instance_id: format!("i{n}"),
        cog_id: "c".into(),
        project_id: Some(project.into()),
        uid: None,
        outputs: if writes { vec![out] } else { vec![] },
        inputs: if writes { vec![] } else { vec![inp] },
    };
    let token = format!("{n:02x}").repeat(32);
    b.register(spec, &token).unwrap();
    b.authenticate(&token, None).unwrap()
}

fn reading(src: &str) -> Reading {
    Reading {
        kind: KIND.into(),
        source_id: src.into(),
        schema: SCHEMA.into(),
        t_ns: 5,
        body: json!({"vectors": [[1, [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]]]}),
    }
}

fn broker() -> Broker {
    Broker::new(BrokerConfig::new("n"))
}

#[test]
fn a_cogs_reading_reaches_the_sink_with_its_project_and_instance() {
    let b = broker();
    let mut rx = b.add_sink(KIND, SCHEMA, 4).unwrap();
    assert_eq!(b.add_sink(KIND, SCHEMA, 4).unwrap_err().code, ErrorCode::BadRequest);
    let w1 = join(&b, 1, P1, true);
    let w2 = join(&b, 2, P2, true);
    let body = reading("x").body;
    b.publish(&w1, KIND, "radar-a", SCHEMA, 7, body.clone()).unwrap();
    b.publish(&w2, KIND, "radar-b", SCHEMA, 8, body).unwrap();
    let a = rx.try_recv().unwrap();
    assert_eq!((a.project_id.as_deref(), a.instance_id.as_str()), (Some(P1), "i1"));
    assert_eq!((a.delivery.topic.as_str(), a.delivery.t_ns, a.delivery.seq), ("store.vector/radar-a", 7, 1));
    let b2 = rx.try_recv().unwrap();
    assert_eq!((b2.project_id.as_deref(), b2.instance_id.as_str()), (Some(P2), "i2"));
    assert!(rx.try_recv().is_err());
    // The sink shows as a subscriber of every store.vector topic.
    assert!(b.snapshot().iter().all(|r| r.subscribers == 1));
}

#[test]
fn a_full_sink_drops_and_counts_without_blocking_the_cog() {
    let b = broker();
    let _rx = b.add_sink(KIND, SCHEMA, 1).unwrap();
    let w = join(&b, 1, P1, true);
    for _ in 0..3 {
        b.publish(&w, KIND, "radar-a", SCHEMA, 1, reading("x").body).unwrap();
    }
    assert_eq!((b.snapshot()[0].messages, b.snapshot()[0].drops), (3, 2));
}

#[test]
fn an_internal_publish_reaches_its_projects_readers_and_comes_back_for_the_sink() {
    let b = broker();
    let mut rx = b.add_sink(KIND, SCHEMA, 4).unwrap();
    let mut mine = join(&b, 1, P1, false);
    let mut other = join(&b, 2, P2, false);
    b.subscribe(&mut mine, "store.vector/*", SCHEMA).unwrap();
    b.subscribe(&mut other, "store.vector/*", SCHEMA).unwrap();
    let sr = b.publish_internal("@ingest-bridge", "cog-1", Some(P1.into()), reading("cog-1")).unwrap();
    assert_eq!((sr.project_id.as_deref(), sr.instance_id.as_str(), sr.delivery.seq), (Some(P1), "cog-1", 1));
    assert_eq!(mine.drain().len(), 1);
    assert!(other.drain().is_empty(), "another project's reader sees nothing");
    assert!(rx.try_recv().is_err(), "the caller hands it to the sink, not the queue");
    let row = &b.snapshot()[0];
    assert_eq!((row.writer.as_deref(), row.project_id.as_deref()), (Some("@ingest-bridge"), Some(P1)));
}

#[test]
fn internal_publishers_keep_the_topic_rules() {
    let b = broker();
    let bad = |w: &str, r: Reading| b.publish_internal(w, "i", None, r).unwrap_err().code;
    assert_eq!(bad("ingest-bridge", reading("s")), ErrorCode::BadRequest);
    assert_eq!(bad("@", reading("s")), ErrorCode::BadRequest);
    assert_eq!(bad("@b", Reading { schema: "store.vector.v9".into(), ..reading("s") }), ErrorCode::BadRequest);
    assert_eq!(bad("@b", Reading { source_id: "Bad".into(), ..reading("s") }), ErrorCode::BadRequest);
    let big = json!({"pad": "x".repeat(crate::proto::MAX_BODY)});
    assert_eq!(bad("@b", Reading { body: big, ..reading("s") }), ErrorCode::TooLarge);
    // A topic of one project stays that project's, and one writer keeps it.
    b.publish_internal("@b", "i", Some(P1.into()), reading("s")).unwrap();
    let e = b.publish_internal("@b", "i", Some(P2.into()), reading("s")).unwrap_err();
    assert_eq!(e.code, ErrorCode::CrossProject);
    let w = join(&b, 1, P1, true);
    let e = b.publish(&w, KIND, "s", SCHEMA, 1, reading("s").body).unwrap_err();
    assert_eq!(e.code, ErrorCode::WriterConflict);
}
