//! Broker rules without a socket.

use clawft_types::cog_topics::{InputPolicy, TopicClass};
use serde_json::json;

use crate::broker::{Broker, BrokerConfig, InstanceSpec, Session};
use crate::proto::{ErrorCode, InputSpec, OutputSpec};

const P1: &str = "01J9ZXW0PRJCTAAAAAAAAAAAAA";
const P2: &str = "01J9ZXW0PRJCTBBBBBBBBBBBBB";

fn out(topic: &str, schema: &str, class: TopicClass) -> OutputSpec {
    OutputSpec {
        topic: topic.into(),
        schema: schema.into(),
        class,
    }
}

fn inp(topic: &str, schema: &str, policy: InputPolicy) -> InputSpec {
    InputSpec {
        topic: topic.into(),
        schema: schema.into(),
        policy,
    }
}

fn token(n: u8) -> String {
    format!("{n:02x}").repeat(32)
}

struct Cog {
    outputs: Vec<OutputSpec>,
    inputs: Vec<InputSpec>,
    project: Option<&'static str>,
}

fn writer() -> Cog {
    Cog {
        outputs: vec![out("telemetry", "telemetry.v1", TopicClass::Reading)],
        inputs: vec![],
        project: Some(P1),
    }
}

fn reader(policy: InputPolicy) -> Cog {
    Cog {
        outputs: vec![],
        inputs: vec![inp("telemetry", "telemetry.v1", policy)],
        project: Some(P1),
    }
}

fn join(b: &Broker, n: u8, c: Cog) -> Session {
    let spec = InstanceSpec {
        instance_id: format!("i{n}"),
        cog_id: "c".into(),
        project_id: c.project.map(String::from),
        uid: None,
        outputs: c.outputs,
        inputs: c.inputs,
    };
    b.register(spec, &token(n)).unwrap();
    b.authenticate(&token(n), None).unwrap()
}

fn broker(queue_len: usize) -> Broker {
    Broker::new(BrokerConfig {
        node_id: "n".into(),
        queue_len,
    })
}

fn pubr(b: &Broker, s: &Session, src: &str) -> Result<u64, crate::BusError> {
    b.publish(s, "telemetry", src, "telemetry.v1", 1, json!({"items": []}))
}

#[test]
fn an_undeclared_publish_or_another_class_is_refused() {
    let b = broker(8);
    let mut c = writer();
    c.outputs
        .push(out("audio.pcm", "pcm.s16le.v1", TopicClass::Stream));
    let s = join(&b, 1, c);
    let e = b
        .publish(&s, "detections", "cam-0", "detections.v1", 0, json!({}))
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::Undeclared);
    let e = b
        .publish(&s, "telemetry", "board", "senml.v1", 0, json!({}))
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::SchemaMismatch);
    let e = b
        .publish(&s, "audio.pcm", "mic-0", "pcm.s16le.v1", 0, json!({}))
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::Unsupported);
    assert!(e.message.contains("not in B1"), "{}", e.message);
    let e = b
        .publish(&s, "telemetry", "Bad Source", "telemetry.v1", 0, json!({}))
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::BadRequest);
    let e = b
        .publish(
            &s,
            "telemetry",
            "board",
            "telemetry.v1",
            0,
            json!("x".repeat(70_000)),
        )
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::TooLarge);
    assert!(b.snapshot().is_empty(), "no topic from a refused publish");
}

#[test]
fn a_wrong_schema_or_undeclared_subscribe_is_refused() {
    let b = broker(8);
    let mut s = join(&b, 2, reader(InputPolicy::DropOldest));
    assert_eq!(
        b.subscribe(&mut s, "telemetry/*", "senml.v1")
            .unwrap_err()
            .code,
        ErrorCode::SchemaMismatch
    );
    assert_eq!(
        b.subscribe(&mut s, "audio.pcm/*", "pcm.s16le.v1")
            .unwrap_err()
            .code,
        ErrorCode::Undeclared
    );
    assert_eq!(
        b.subscribe(&mut s, "telemetry", "telemetry.v1")
            .unwrap_err()
            .code,
        ErrorCode::BadRequest
    );
    assert!(b.subscribe(&mut s, "telemetry/*", "telemetry.v1").is_ok());
}

#[test]
fn cross_project_topics_are_refused_or_filtered() {
    let b = broker(8);
    let w = join(&b, 1, writer());
    let mut other = reader(InputPolicy::DropOldest);
    other.project = Some(P2);
    let mut r = join(&b, 2, other);
    pubr(&b, &w, "board").unwrap();
    let e = b
        .subscribe(&mut r, "telemetry/board", "telemetry.v1")
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::CrossProject);
    // A wildcard is accepted but sees only its own project.
    b.subscribe(&mut r, "telemetry/*", "telemetry.v1").unwrap();
    pubr(&b, &w, "board").unwrap();
    assert!(r.drain().is_empty());
    assert!(b.list_for(&r).is_empty());
    assert_eq!(b.snapshot()[0].subscribers, 0);
    // A project-less instance is not in P1 either.
    let mut none = reader(InputPolicy::DropOldest);
    none.project = None;
    let mut n = join(&b, 3, none);
    assert_eq!(
        b.subscribe(&mut n, "telemetry/board", "telemetry.v1")
            .unwrap_err()
            .code,
        ErrorCode::CrossProject
    );
}

#[test]
fn a_wildcard_sees_every_source_of_its_kind_and_seq_is_per_topic() {
    let b = broker(64);
    let w = join(&b, 1, writer());
    let mut r = join(&b, 2, reader(InputPolicy::DropOldest));
    let mut exact = join(&b, 3, reader(InputPolicy::DropOldest));
    b.subscribe(&mut r, "telemetry/*", "telemetry.v1").unwrap();
    b.subscribe(&mut exact, "telemetry/a", "telemetry.v1")
        .unwrap();
    for _ in 0..3 {
        pubr(&b, &w, "a").unwrap();
        pubr(&b, &w, "b").unwrap();
    }
    let got = r.drain();
    let seqs = |src: &str| {
        got.iter()
            .filter(|d| d.source_id == src)
            .map(|d| d.seq)
            .collect::<Vec<_>>()
    };
    assert_eq!((seqs("a"), seqs("b")), (vec![1, 2, 3], vec![1, 2, 3]));
    assert!(
        got.iter()
            .all(|d| d.node_id == "n" && d.recv_ns > 0 && d.t_ns == 1)
    );
    let only_a = exact.drain();
    assert_eq!(
        only_a
            .iter()
            .map(|d| (d.topic.as_str(), d.seq))
            .collect::<Vec<_>>(),
        vec![("telemetry/a", 1), ("telemetry/a", 2), ("telemetry/a", 3)]
    );
    let rows = b.snapshot();
    assert_eq!(
        rows.iter()
            .map(|r| (r.topic.as_str(), r.subscribers, r.messages))
            .collect::<Vec<_>>(),
        vec![("telemetry/a", 2, 3), ("telemetry/b", 1, 3)]
    );
    assert_eq!(rows[0].writer.as_deref(), Some("i1"));
}

#[test]
fn drop_oldest_counts_drops_and_keeps_the_newest() {
    let b = broker(4);
    let w = join(&b, 1, writer());
    let mut r = join(&b, 2, reader(InputPolicy::DropOldest));
    b.subscribe(&mut r, "telemetry/*", "telemetry.v1").unwrap();
    for _ in 0..10 {
        pubr(&b, &w, "a").unwrap();
    }
    assert_eq!(
        r.drain().iter().map(|d| d.seq).collect::<Vec<_>>(),
        vec![7, 8, 9, 10]
    );
    assert_eq!(b.snapshot()[0].drops, 6);
}

#[test]
fn sample_keeps_only_the_latest_and_block_falls_back_to_drop_oldest() {
    let b = broker(4);
    let w = join(&b, 1, writer());
    let mut s = join(&b, 2, reader(InputPolicy::Sample));
    let mut k = join(&b, 3, reader(InputPolicy::Block));
    b.subscribe(&mut s, "telemetry/*", "telemetry.v1").unwrap();
    b.subscribe(&mut k, "telemetry/*", "telemetry.v1").unwrap();
    for _ in 0..6 {
        pubr(&b, &w, "a").unwrap();
    }
    assert_eq!(s.drain().iter().map(|d| d.seq).collect::<Vec<_>>(), vec![6]);
    assert_eq!(
        k.drain().iter().map(|d| d.seq).collect::<Vec<_>>(),
        vec![3, 4, 5, 6]
    );
    assert_eq!(b.snapshot()[0].drops, 5 + 2);
}

#[test]
fn one_writer_per_topic_and_revoke_frees_it() {
    let b = broker(4);
    let w1 = join(&b, 1, writer());
    let w2 = join(&b, 2, writer());
    pubr(&b, &w1, "a").unwrap();
    assert_eq!(
        pubr(&b, &w2, "a").unwrap_err().code,
        ErrorCode::WriterConflict
    );
    let mut rv = w1.revoked();
    assert!(b.revoke("i1"));
    assert!(*rv.borrow_and_update());
    assert_eq!(pubr(&b, &w1, "a").unwrap_err().code, ErrorCode::Revoked);
    assert!(b.authenticate(&token(1), None).is_err(), "revoked token");
    assert_eq!(
        pubr(&b, &w2, "a").unwrap(),
        2,
        "seq continues for the new writer"
    );
}

#[test]
fn tokens_uids_and_declarations_are_checked() {
    let b = broker(4);
    assert_eq!(
        b.authenticate(&token(9), None).unwrap_err().code,
        ErrorCode::Unauthorized
    );
    let spec = InstanceSpec {
        instance_id: "u".into(),
        cog_id: "c".into(),
        project_id: None,
        uid: Some(501),
        outputs: writer().outputs,
        inputs: vec![],
    };
    b.register(spec.clone(), &token(5)).unwrap();
    assert!(b.register(spec, &token(6)).is_err(), "duplicate instance");
    assert_eq!(
        b.authenticate(&token(5), Some(0)).unwrap_err().code,
        ErrorCode::UidMismatch
    );
    assert_eq!(
        b.authenticate(&token(5), None).unwrap_err().code,
        ErrorCode::UidMismatch
    );
    let mut s = b.authenticate(&token(5), Some(501)).unwrap();
    // Declare narrows: a declared-away output can no longer be published.
    let e = b
        .declare(
            &mut s,
            vec![out("telemetry", "senml.v1", TopicClass::Reading)],
            vec![],
        )
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::Undeclared);
    b.declare(&mut s, vec![], vec![]).unwrap();
    assert_eq!(pubr(&b, &s, "a").unwrap_err().code, ErrorCode::Undeclared);
}

#[test]
fn unsubscribe_and_close_drop_subscriptions() {
    let b = broker(4);
    let w = join(&b, 1, writer());
    let mut r = join(&b, 2, reader(InputPolicy::DropOldest));
    let id = b.subscribe(&mut r, "telemetry/*", "telemetry.v1").unwrap();
    assert_eq!(
        b.unsubscribe(&mut r, id + 100).unwrap_err().code,
        ErrorCode::UnknownSubscription
    );
    b.unsubscribe(&mut r, id).unwrap();
    pubr(&b, &w, "a").unwrap();
    assert!(r.drain().is_empty());
    b.subscribe(&mut r, "telemetry/*", "telemetry.v1").unwrap();
    b.close(&r);
    assert_eq!(b.snapshot()[0].subscribers, 0);
}
