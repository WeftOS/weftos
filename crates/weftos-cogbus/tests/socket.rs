//! Two cogs exchange readings over a real Unix socket (ADR-122 B1 done-when):
//! A publishes 100 readings, B gets all of them in order; an undeclared
//! publish, a wrong-schema subscribe and a bad token are refused.
#![cfg(unix)]

use std::sync::Arc;
use std::time::Duration;

use clawft_types::cog_topics::{InputPolicy, TopicClass};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use weftos_cogbus::proto::{MAX_FRAME, read_frame};
use weftos_cogbus::server::serve;
use weftos_cogbus::{
    Broker, BrokerConfig, Client, ClientError, ErrorCode, Event, InputSpec, InstanceSpec,
    OutputSpec,
};

const PROJECT: &str = "01J9ZXW0PRJCTAAAAAAAAAAAAA";
const TOKEN_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const TOKEN_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

/// This process's uid: the owner of a file it just created.
fn uid(created: &std::path::Path) -> u32 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(created).unwrap().uid()
}

fn refused(r: Result<impl std::fmt::Debug, ClientError>) -> ErrorCode {
    match r {
        Err(ClientError::Refused { code, .. }) => code,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

async fn rig() -> (
    Arc<Broker>,
    weftos_cogbus::server::ServerHandle,
    tempfile::TempDir,
) {
    let tmp = tempfile::tempdir().unwrap();
    let broker = Arc::new(Broker::new(BrokerConfig::new("0".repeat(32))));
    let spec = |id: &str, outputs, inputs| InstanceSpec {
        instance_id: id.into(),
        cog_id: id.into(),
        project_id: Some(PROJECT.into()),
        uid: None,
        outputs,
        inputs,
    };
    let telemetry_out = OutputSpec {
        topic: "telemetry".into(),
        schema: "telemetry.v1".into(),
        class: TopicClass::Reading,
    };
    let telemetry_in = InputSpec {
        topic: "telemetry".into(),
        schema: "telemetry.v1".into(),
        policy: InputPolicy::DropOldest,
    };
    broker
        .register(spec("cog-a", vec![telemetry_out], vec![]), TOKEN_A)
        .unwrap();
    broker
        .register(spec("cog-b", vec![], vec![telemetry_in]), TOKEN_B)
        .unwrap();
    let handle = serve(broker.clone(), &tmp.path().join("cogbus.sock"))
        .await
        .unwrap();
    (broker, handle, tmp)
}

#[tokio::test]
async fn two_cogs_exchange_readings_and_the_rules_hold_on_the_socket() {
    let (broker, server, _tmp) = rig().await;
    let path = server.path().to_path_buf();
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o660
    );

    let a = Client::connect(&path, TOKEN_A).await.unwrap();
    let b = Client::connect(&path, TOKEN_B).await.unwrap();
    assert_eq!((a.instance_id(), b.instance_id()), ("cog-a", "cog-b"));

    assert_eq!(
        refused(b.subscribe("telemetry/*", "senml.v1").await),
        ErrorCode::SchemaMismatch
    );
    let mut sub = b.subscribe("telemetry/*", "telemetry.v1").await.unwrap();

    for i in 0..100u64 {
        let body =
            json!({"items": [{"name": "cpu.temp", "value": 40.0 + i as f64, "unit": "Cel"}]});
        assert_eq!(
            a.publish("telemetry", "venq-board", "telemetry.v1", 1_000 + i, body)
                .await
                .unwrap(),
            i + 1
        );
    }
    assert_eq!(
        refused(
            a.publish("detections", "cam-0", "detections.v1", 0, json!({}))
                .await
        ),
        ErrorCode::Undeclared
    );

    for want in 1..=100u64 {
        let m = tokio::time::timeout(Duration::from_secs(5), sub.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (m.seq, m.t_ns, m.topic.as_str()),
            (want, 999 + want, "telemetry/venq-board")
        );
        assert_eq!(
            (m.source_id.as_str(), m.schema.as_str()),
            ("venq-board", "telemetry.v1")
        );
        assert!(m.recv_ns > 0);
    }

    let rows = broker.snapshot();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (rows[0].messages, rows[0].drops, rows[0].subscribers),
        (100, 0, 1)
    );
    assert_eq!(rows[0].writer.as_deref(), Some("cog-a"));
    let listed = b.list().await.unwrap();
    assert_eq!(listed["topics"][0]["topic"], "telemetry/venq-board");

    // Revoking B closes its connection; its subscription ends.
    assert!(broker.revoke("cog-b"));
    assert!(
        tokio::time::timeout(Duration::from_secs(5), sub.recv())
            .await
            .unwrap()
            .is_none()
    );
    assert!(b.list().await.is_err());
}

#[tokio::test]
async fn a_bad_token_a_wrong_uid_and_garbage_are_refused() {
    let (broker, server, _tmp) = rig().await;
    let path = server.path().to_path_buf();
    assert_eq!(
        refused(Client::connect(&path, &"c".repeat(64)).await),
        ErrorCode::Unauthorized
    );

    let spec = InstanceSpec {
        instance_id: "cog-u".into(),
        cog_id: "u".into(),
        project_id: None,
        uid: Some(uid(&path).wrapping_add(1)),
        outputs: vec![],
        inputs: vec![],
    };
    broker.register(spec, &"d".repeat(64)).unwrap();
    assert_eq!(
        refused(Client::connect(&path, &"d".repeat(64)).await),
        ErrorCode::UidMismatch
    );

    // A frame other than hello first, then garbage, then an oversize length:
    // each gets an error frame and the connection closes.
    for raw in [br#"{"op":"list","id":7}"#.to_vec(), b"not json".to_vec()] {
        let mut s = tokio::net::UnixStream::connect(&path).await.unwrap();
        s.write_all(&(raw.len() as u32).to_be_bytes())
            .await
            .unwrap();
        s.write_all(&raw).await.unwrap();
        let ev: Event =
            serde_json::from_slice(&read_frame(&mut s).await.unwrap().unwrap()).unwrap();
        assert!(
            matches!(
                ev,
                Event::Error {
                    code: ErrorCode::BadRequest,
                    ..
                }
            ),
            "{ev:?}"
        );
        assert!(read_frame(&mut s).await.unwrap().is_none(), "closed");
    }
    let mut s = tokio::net::UnixStream::connect(&path).await.unwrap();
    s.write_all(&((MAX_FRAME as u32) + 1).to_be_bytes())
        .await
        .unwrap();
    let ev: Event = serde_json::from_slice(&read_frame(&mut s).await.unwrap().unwrap()).unwrap();
    assert!(
        matches!(
            ev,
            Event::Error {
                code: ErrorCode::TooLarge,
                ..
            }
        ),
        "{ev:?}"
    );
    let mut rest = Vec::new();
    let _ = s.read_to_end(&mut rest).await;
    assert!(rest.is_empty());
}

#[tokio::test]
async fn a_stale_socket_file_is_replaced_but_another_file_is_not() {
    let tmp = tempfile::tempdir().unwrap();
    let broker = Arc::new(Broker::new(BrokerConfig::new("n")));
    let path = tmp.path().join("cogbus.sock");
    drop(serve(broker.clone(), &path).await.unwrap());
    let _l = std::os::unix::net::UnixListener::bind(&path).unwrap();
    drop(_l);
    let h = serve(broker.clone(), &path).await.unwrap();
    drop(h);
    assert!(!path.exists(), "dropping the handle removes the socket");
    std::fs::write(&path, "x").unwrap();
    assert!(serve(broker, &path).await.is_err());
}
