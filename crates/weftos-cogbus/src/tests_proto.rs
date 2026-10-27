//! Wire protocol: round trips, limits, strict requests.

use serde_json::json;

use crate::proto::*;
use clawft_types::cog_topics::{InputPolicy, TopicClass};

fn every_request() -> Vec<Request> {
    vec![
        Request::Hello {
            id: 1,
            token: "t".repeat(64),
        },
        Request::Declare {
            id: 2,
            outputs: vec![OutputSpec {
                topic: "telemetry".into(),
                schema: "telemetry.v1".into(),
                class: TopicClass::Reading,
            }],
            inputs: vec![InputSpec {
                topic: "telemetry".into(),
                schema: "telemetry.v1".into(),
                policy: InputPolicy::Sample,
            }],
        },
        Request::Publish {
            id: 3,
            topic_kind: "telemetry".into(),
            source_id: "venq-board".into(),
            schema: "telemetry.v1".into(),
            t_ns: 17,
            body: json!({"items": []}),
        },
        Request::Subscribe {
            id: 4,
            topic: "telemetry/*".into(),
            schema: "telemetry.v1".into(),
            raw: false,
        },
        Request::Unsubscribe { id: 5, sub: 1 },
        Request::List { id: 6 },
    ]
}

#[tokio::test]
async fn every_request_and_event_round_trips_through_a_frame() {
    for r in every_request() {
        let bytes = encode(&r).unwrap();
        let n = u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize;
        assert_eq!(n, bytes.len() - 4, "big-endian length prefix");
        let frame = read_frame(&mut &bytes[..]).await.unwrap().unwrap();
        assert_eq!(serde_json::from_slice::<Request>(&frame).unwrap(), r);
    }
    let d = Delivery {
        sub: 1,
        topic: "telemetry/venq-board".into(),
        schema: "telemetry.v1".into(),
        source_id: "venq-board".into(),
        node_id: "n".into(),
        t_ns: 1,
        seq: 2,
        recv_ns: 3,
        body: json!({}),
    };
    for ev in [
        Event::Ok {
            id: 1,
            result: json!({"seq": 1}),
        },
        Event::Error {
            id: 2,
            code: ErrorCode::Undeclared,
            message: "no".into(),
        },
        Event::Msg(d),
    ] {
        let frame = read_frame(&mut &encode(&ev).unwrap()[..])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(serde_json::from_slice::<Event>(&frame).unwrap(), ev);
    }
}

#[test]
fn the_documented_frames_parse() {
    let frames = [
        r#"{"op":"hello","id":1,"token":"x"}"#,
        r#"{"op":"declare","id":2,"outputs":[{"topic":"telemetry","schema":"telemetry.v1","class":"reading"}],"inputs":[]}"#,
        r#"{"op":"publish","id":3,"topic_kind":"telemetry","source_id":"venq-board","schema":"telemetry.v1","t_ns":17,"body":{"items":[]}}"#,
        r#"{"op":"subscribe","id":4,"topic":"telemetry/*","schema":"telemetry.v1"}"#,
        r#"{"op":"unsubscribe","id":5,"sub":1}"#,
        r#"{"op":"list","id":6}"#,
    ];
    for f in frames {
        serde_json::from_str::<Request>(f).unwrap_or_else(|e| panic!("{f}: {e}"));
    }
    // An input policy defaults to drop-oldest.
    let i: InputSpec = serde_json::from_str(r#"{"topic":"a","schema":"senml.v1"}"#).unwrap();
    assert_eq!(i.policy, InputPolicy::DropOldest);
}

#[test]
fn requests_refuse_unknown_fields_and_events_accept_them() {
    for f in [
        r#"{"op":"hello","id":1,"token":"x","admin":true}"#,
        r#"{"op":"list","id":6,"all_projects":true}"#,
        r#"{"op":"subscribe","id":4,"topic":"a/*","schema":"senml.v1","grant":"x"}"#,
        r#"{"op":"declare","id":2,"outputs":[{"topic":"a","schema":"senml.v1","class":"reading","rate":"1 Hz"}]}"#,
        r#"{"op":"sudo","id":1}"#,
    ] {
        assert!(serde_json::from_str::<Request>(f).is_err(), "accepted {f}");
    }
    let ev: Event = serde_json::from_str(
        r#"{"type":"error","id":1,"code":"future_code","message":"m","extra":1}"#,
    )
    .unwrap();
    assert!(matches!(
        ev,
        Event::Error {
            code: ErrorCode::Unknown,
            ..
        }
    ));
}

#[tokio::test]
async fn an_oversize_length_is_refused_before_reading_the_body() {
    let mut bytes = ((MAX_FRAME + 1) as u32).to_be_bytes().to_vec();
    bytes.extend_from_slice(b"{}");
    assert!(
        matches!(read_frame(&mut &bytes[..]).await, Err(FrameError::Oversize(n)) if n == MAX_FRAME + 1)
    );
    let huge = Request::Publish {
        id: 1,
        topic_kind: "a".into(),
        source_id: "b".into(),
        schema: "senml.v1".into(),
        t_ns: 0,
        body: json!("x".repeat(MAX_FRAME)),
    };
    assert!(matches!(encode(&huge), Err(FrameError::Oversize(_))));
}

#[tokio::test]
async fn end_of_stream_is_clean_only_at_a_frame_boundary() {
    assert!(read_frame(&mut &[][..]).await.unwrap().is_none());
    let mut cut = encode(&Request::List { id: 1 }).unwrap();
    cut.truncate(cut.len() - 1);
    assert!(read_frame(&mut &cut[..]).await.is_err());
}
