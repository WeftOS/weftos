//! B6d: a raw IQ stream crosses to another node only when exposed with
//! `--raw` to named nodes and asked with `raw: true`; then its records flow
//! as binary chunks under byte credit, and drops show as a `seq` gap.

use std::time::Duration;

use clawft_types::cog_topics::{InputPolicy, MeshLimits, TopicClass};

use super::super::MeshAudience;
use super::*;
use crate::client::{Client, ClientError};
use crate::proto::ErrorCode;
use crate::server::{ServerHandle, serve};
use crate::stream::StreamOptions;

const IQ: &str = "iq/pluto0";
const IQ_NAME: &str = "weftos://home/sensors/pluto0/topics/iq";
const SCHEMA: &str = "iq.cs16.v1";

async fn socket(n: &Node) -> (ServerHandle, std::path::PathBuf) {
    let path = n._blobs.path().join("cogbus.sock");
    let h = serve(n.broker.clone(), &path).await.unwrap();
    n.hub.set_socket(path.clone());
    (h, path)
}

/// Pump both hubs and deliver frames while the taps and proxies run.
async fn settle(wire: &Arc<Wire>, nodes: &[&Node]) {
    for _ in 0..20 {
        flush(wire, nodes);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn code<T: std::fmt::Debug>(r: Result<T, ClientError>) -> ErrorCode {
    match r {
        Err(ClientError::Refused { code, .. }) => code,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_raw_iq_stream_crosses_only_when_exposed_and_asked_raw_then_flows_under_credit() {
    let wire = Arc::new(Wire::default());
    let (a, b) = (Node::new("node-a", &wire), Node::new("node-b", &wire));
    let ((_ha, sock_a), (_hb, sock_b)) = (socket(&a).await, socket(&b).await);
    // node-a: the SDR cog writes the IQ ring.
    let out = OutputSpec { topic: "iq".into(), schema: SCHEMA.into(), class: TopicClass::Stream };
    let _sdr = a.cog("sdr", 's', None, vec![out], vec![]);
    a.broker.set_mesh_limits("sdr", MeshLimits::STARTING);
    let ca = Client::connect(&sock_a, &"s".repeat(64)).await.unwrap();
    let mut w = ca.stream_writer("iq", "pluto0", SCHEMA, StreamOptions::new(1024)).await.unwrap();
    w.write(0, &[0; 256]).unwrap();
    // node-b: a reader cog.
    let input = InputSpec { topic: "iq".into(), schema: SCHEMA.into(), policy: InputPolicy::DropOldest };
    let _rx = b.cog("rx", 'r', None, vec![], vec![input]);
    let cb = Client::connect(&sock_b, &"r".repeat(64)).await.unwrap();

    // Exposed to node-b but not --raw.
    a.expo.expose(IQ, MeshAudience::Nodes(vec!["node-b".into()]), false, &["node-b"]);
    a.hub.announce(&[b.peer()]);
    settle(&wire, &[&a, &b]).await;
    assert_eq!(code(cb.subscribe_with(IQ_NAME, SCHEMA, false).await), ErrorCode::RawNotExposed, "not asked raw: refused on node-b");
    let _mesh = cb.subscribe_with(IQ_NAME, SCHEMA, true).await.unwrap();
    settle(&wire, &[&a, &b]).await;
    assert!(a.expo.outcomes().iter().any(|(who, o)| who == "node-b" && o.starts_with("refused:raw_not_exposed")), "{:?}", a.expo.outcomes());
    assert!(a.hub.served().is_empty());

    // --raw for the named node: the retry is served.
    a.expo.expose(IQ, MeshAudience::Nodes(vec!["node-b".into()]), true, &["node-b"]);
    settle(&wire, &[&a, &b]).await;
    assert_eq!(a.hub.served().len(), 1, "served after --raw");
    let mut r = cb.subscribe_stream(IQ, SCHEMA).await.unwrap();
    let mut got = Vec::new();
    // The reader maps the proxy ring once its writer opened it.
    w.write(0, &[0; 256]).unwrap();
    for _ in 0..50 {
        settle(&wire, &[&a, &b]).await;
        if let Some(rec) = r.try_next() {
            got.push(rec);
            break;
        }
    }
    assert!(!got.is_empty(), "the proxy ring never carried a record");

    // A burst past one credit window (64 records) before credit can return:
    // the excess is dropped at the source and the reader sees a gap.
    for i in 1..=150u64 {
        w.write(i, &[i as u8; 256]).unwrap();
    }
    let drain = |r: &mut crate::stream::StreamReader, got: &mut Vec<crate::stream::StreamRecord>| {
        while let Some(rec) = r.try_next() {
            got.push(rec);
        }
    };
    settle(&wire, &[&a, &b]).await;
    drain(&mut r, &mut got);
    for i in 151..=160u64 {
        w.write(i, &[i as u8; 256]).unwrap();
        settle(&wire, &[&a, &b]).await;
        drain(&mut r, &mut got);
    }
    assert!(!got.is_empty(), "no records crossed");
    assert!(got.iter().all(|x| x.data.len() == 256 && x.data[0] == x.t_ns as u8), "bytes arrive intact");
    let seqs: Vec<u64> = got.iter().map(|x| x.seq).collect();
    assert!(seqs.windows(2).all(|p| p[1] > p[0]), "{seqs:?}");
    assert!(seqs.windows(2).any(|p| p[1] > p[0] + 1), "drops show as a gap: {seqs:?}");
    assert!(got.iter().any(|x| x.t_ns == 160), "credit came back and the stream continued: {seqs:?}");
    let (_, _, sent, drops) = a.hub.served()[0].clone();
    assert!(drops > 0 && sent >= 64, "sent {sent} dropped {drops}");
}
