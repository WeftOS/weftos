//! B6e and the raw rule for readings: a frame handle from one node resolves
//! on another once, by hash, only for a node the raw exposure covers.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use clawft_types::cog_topics::{InputPolicy, MeshLimits, TopicClass};

use super::super::wire::{TopicData, TopicReq};
use super::super::MeshAudience;
use super::*;
use crate::proto::ErrorCode;

const FRAMES: &str = "video.frame/cam0";
const FRAME_NAME: &str = "weftos://home/sensors/cam0/topics/video.frame";
const SCHEMA: &str = "video.frame-ref.v1";

fn camera(a: &Node) -> Session {
    let outs = vec![
        OutputSpec { topic: "video.frame".into(), schema: SCHEMA.into(), class: TopicClass::Reading },
        OutputSpec { topic: "video.jpeg".into(), schema: "blob.v1".into(), class: TopicClass::Blob },
    ];
    let s = a.cog("cam", 'c', None, outs, vec![]);
    a.broker.set_mesh_limits("cam", MeshLimits::STARTING);
    s
}

/// Store `bytes` as `cam`'s blob and publish a frame ref carrying its handle.
fn frame(a: &Node, s: &Session, bytes: &[u8]) -> Value {
    let up = a.broker.blob_begin(s, "image/jpeg", bytes.len() as u64, None).unwrap()["upload"].as_u64().unwrap();
    let mut handle = Value::Null;
    for (i, part) in bytes.chunks(crate::blob::CHUNK).enumerate() {
        let last = (i + 1) * crate::blob::CHUNK >= bytes.len();
        let r = a.broker.blob_chunk(s, up, (i * crate::blob::CHUNK) as u64, &B64.encode(part), last).unwrap();
        if last {
            handle = r["handle"].clone();
        }
    }
    a.broker.publish(s, "video.frame", "cam0", SCHEMA, 1, json!({"blob": handle})).unwrap();
    handle
}

fn reader(b: &Node, c: char) -> Session {
    let input = InputSpec { topic: "video.frame".into(), schema: SCHEMA.into(), policy: InputPolicy::DropOldest };
    b.cog(&format!("viewer-{c}"), c, None, vec![], vec![input])
}

fn code<T: std::fmt::Debug>(r: Result<T, crate::types::BusError>) -> ErrorCode {
    r.expect_err("refused").code
}

#[test]
fn a_raw_topic_is_served_only_when_exposed_raw_to_named_nodes_and_asked_raw() {
    let wire = Arc::new(Wire::default());
    let (a, b) = (Node::new("node-a", &wire), Node::new("node-b", &wire));
    let cam = camera(&a);
    frame(&a, &cam, b"jpeg");
    let mut v = reader(&b, 'v');
    let refused_at_a = |b: &Node, v: &mut Session| {
        a.hub.announce(&[b.peer()]);
        flush(&wire, &[&a, b]);
        b.broker.subscribe_with(v, FRAME_NAME, SCHEMA, true).unwrap();
        flush(&wire, &[&a, b]);
        // A refused subscription retries after its (test-short) backoff.
        std::thread::sleep(std::time::Duration::from_millis(10));
        flush(&wire, &[&a, b]);
        a.expo.outcomes().last().unwrap().1.clone()
    };
    // Exposed to the node but not --raw: refused at the source and logged.
    a.expo.expose(FRAMES, MeshAudience::Nodes(vec!["node-b".into()]), false, &["node-b"]);
    assert!(refused_at_a(&b, &mut v).starts_with("refused:raw_not_exposed"));
    // --raw, but for every paired peer: still refused.
    a.expo.expose(FRAMES, MeshAudience::Paired, true, &["node-b"]);
    let mut v2 = reader(&b, 'w');
    assert!(refused_at_a(&b, &mut v2).contains("named nodes"));
    // A subscriber that did not ask raw is refused on its own node.
    let mut v3 = reader(&b, 'x');
    assert_eq!(code(b.broker.subscribe_with(&mut v3, FRAME_NAME, SCHEMA, false)), ErrorCode::RawNotExposed);
}

#[test]
fn a_frame_handle_resolves_on_the_other_node_once_and_never_for_a_node_without_exposure() {
    let wire = Arc::new(Wire::default());
    let (a, b) = (Node::new("node-a", &wire), Node::new("node-b", &wire));
    let cam = camera(&a);
    frame(&a, &cam, b"warm-up");
    a.expo.expose(FRAMES, MeshAudience::Nodes(vec!["node-b".into()]), true, &["node-b"]);
    a.hub.announce(&[b.peer()]);
    flush(&wire, &[&a, &b]);
    let mut v = reader(&b, 'v');
    b.broker.subscribe_with(&mut v, FRAME_NAME, SCHEMA, true).unwrap();
    flush(&wire, &[&a, &b]);
    let bytes: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
    let handle = frame(&a, &cam, &bytes);
    flush(&wire, &[&a, &b]);
    let got = v.drain();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].body["blob"], handle, "the handle travelled in the reading, not the bytes");
    let hash = handle["hash"].as_str().unwrap().to_string();
    // The first read starts the fetch; the bytes land in node-b's store.
    assert_eq!(code(b.broker.blob_get(&v, &hash, 0, None)), ErrorCode::Unavailable);
    let data = flush(&wire, &[&a, &b]);
    let slices = data.iter().filter(|(to, d)| to == "node-b" && matches!(d, TopicData::Blob { .. })).count();
    assert_eq!(slices, bytes.len().div_ceil(crate::blob::CHUNK));
    let mut back = Vec::new();
    let mut off = 0;
    loop {
        let r = b.broker.blob_get(&v, &hash, off, None).unwrap();
        let part = B64.decode(r["data"].as_str().unwrap()).unwrap();
        off += part.len() as u64;
        back.extend(part);
        if r["eof"] == true {
            break;
        }
    }
    assert_eq!(back, bytes);
    // Once: reading it again sends nothing over the mesh.
    b.broker.blob_get(&v, &hash, 0, None).unwrap();
    assert!(flush(&wire, &[&a, &b]).is_empty());
    // node-c was never handed the frame: refused, whatever it knows.
    send_req(&a, "node-c", true, &TopicReq::Fetch { req: 1, hash: hash.clone(), offset: 0, len: 1024 });
    let c = take_for(&wire, "node-c");
    assert!(matches!(&c[..], [TopicData::BlobError { code, .. }] if code == "not_found"), "{c:?}");
    assert!(a.expo.outcomes().iter().any(|(who, o)| who == "node-c" && o.starts_with("refused:not_found")));
}

#[test]
fn a_withdrawn_exposure_refuses_the_fetch() {
    let wire = Arc::new(Wire::default());
    let (a, b) = (Node::new("node-a", &wire), Node::new("node-b", &wire));
    let cam = camera(&a);
    frame(&a, &cam, b"warm-up");
    a.expo.expose(FRAMES, MeshAudience::Nodes(vec!["node-b".into()]), true, &["node-b"]);
    a.hub.announce(&[b.peer()]);
    flush(&wire, &[&a, &b]);
    let mut v = reader(&b, 'v');
    b.broker.subscribe_with(&mut v, FRAME_NAME, SCHEMA, true).unwrap();
    flush(&wire, &[&a, &b]);
    let hash = frame(&a, &cam, b"secret")["hash"].as_str().unwrap().to_string();
    flush(&wire, &[&a, &b]);
    a.expo.off(FRAMES);
    assert_eq!(code(b.broker.blob_get(&v, &hash, 0, None)), ErrorCode::Unavailable);
    flush(&wire, &[&a, &b]);
    let e = b.broker.blob_get(&v, &hash, 0, None).expect_err("refused");
    assert_eq!(e.code, ErrorCode::NotFound);
    assert!(e.message.contains("could not be fetched"), "{}", e.message);
}
