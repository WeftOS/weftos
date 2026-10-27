//! B6c: a cog on one node reads another node's reading topic by its
//! `weftos://` name, sees the origin `node_id`, and keeps reading across a
//! withdrawal and a link drop.

use clawft_types::cog_topics::{InputPolicy, MeshLimits};

use super::super::MeshAudience;
use super::*;
use crate::proto::ErrorCode;

/// node-a runs the board cog; node-b runs a reader `reader` in `project`.
fn pair(audience: MeshAudience, project: Option<&str>) -> (Arc<Wire>, Node, Session, Node, Session) {
    let wire = Arc::new(Wire::default());
    let a = Node::new("node-a", &wire);
    let b = Node::new("node-b", &wire);
    let sa = a.board(Some(MeshLimits::STARTING));
    a.expo.expose(TOPIC, audience, false, &["node-b"]);
    a.hub.announce(&[b.peer()]);
    flush(&wire, &[&a, &b]);
    let sb = b.cog("reader", 'r', project, vec![], vec![telemetry_in(InputPolicy::DropOldest)]);
    (wire, a, sa, b, sb)
}

fn code<T: std::fmt::Debug>(r: Result<T, crate::types::BusError>) -> ErrorCode {
    r.expect_err("refused").code
}

#[test]
fn a_cog_reads_a_remote_reading_with_the_origin_node_id() {
    let (wire, a, sa, b, mut sb) = pair(MeshAudience::Paired, None);
    let sub = b.broker.subscribe_with(&mut sb, NAME, "telemetry.v1", false).unwrap();
    flush(&wire, &[&a, &b]);
    a.publish(&sa, 41);
    a.publish(&sa, 42);
    flush(&wire, &[&a, &b]);
    let got = sb.drain();
    assert_eq!(got.len(), 2, "{got:?}");
    assert!(got.iter().all(|d| d.sub == sub && d.node_id == "node-a" && d.topic == TOPIC && d.source_id == "venq-board"));
    assert_eq!(got.iter().map(|d| d.body["n"].as_u64().unwrap()).collect::<Vec<_>>(), [41, 42]);
    // The local proxy topic is written by the mesh, not by a cog here.
    let row = b.broker.snapshot().into_iter().find(|r| r.topic == TOPIC).unwrap();
    assert_eq!(row.writer.as_deref(), Some("@mesh:node-a"));
    assert_eq!(b.broker.mesh_remote_rows()[0].2, "open");
}

#[test]
fn the_audience_decides_the_project_a_remote_topic_belongs_to() {
    // `paired` puts it in the node bucket: a project's cog is refused.
    let (_w, _a, _sa, b, mut sb) = pair(MeshAudience::Paired, Some(P1));
    assert_eq!(code(b.broker.subscribe_with(&mut sb, NAME, "telemetry.v1", false)), ErrorCode::CrossProject);
    // `project:<id>` makes it that project's.
    let (wire, a, sa, b, mut sb) = pair(MeshAudience::Project(P1.into()), Some(P1));
    b.broker.subscribe_with(&mut sb, NAME, "telemetry.v1", false).unwrap();
    flush(&wire, &[&a, &b]);
    a.publish(&sa, 1);
    flush(&wire, &[&a, &b]);
    assert_eq!(sb.drain().len(), 1);
    let mut other = b.cog("other", 'o', None, vec![], vec![telemetry_in(InputPolicy::DropOldest)]);
    assert_eq!(code(b.broker.subscribe_with(&mut other, NAME, "telemetry.v1", false)), ErrorCode::CrossProject);
}

#[test]
fn names_nobody_offers_and_local_writers_are_refused() {
    let (_w, _a, _sa, b, mut sb) = pair(MeshAudience::Paired, None);
    let missing = "weftos://home/sensors/venq-other/topics/telemetry";
    assert_eq!(code(b.broker.subscribe_with(&mut sb, missing, "telemetry.v1", false)), ErrorCode::Unavailable);
    assert_eq!(code(b.broker.subscribe_with(&mut sb, NAME, "senml.v1", false)), ErrorCode::SchemaMismatch);
    b.broker.set_mesh_name("other-mesh");
    assert_eq!(code(b.broker.subscribe_with(&mut sb, NAME, "telemetry.v1", false)), ErrorCode::BadRequest);
    let (_w, _a, _sa, b, mut sb) = pair(MeshAudience::Paired, None);
    let local = b.cog("local", 'l', None, vec![telemetry_out()], vec![]);
    b.publish(&local, 1);
    assert_eq!(code(b.broker.subscribe_with(&mut sb, NAME, "telemetry.v1", false)), ErrorCode::WriterConflict);
}

#[test]
fn the_subscription_resubscribes_after_a_withdrawal_and_a_link_drop() {
    let (wire, a, sa, b, mut sb) = pair(MeshAudience::Paired, None);
    b.broker.subscribe_with(&mut sb, NAME, "telemetry.v1", false).unwrap();
    flush(&wire, &[&a, &b]);
    // The operator withdraws and re-exposes: the subscriber retries on its own.
    a.hub.withdraw(&super::super::exposure_id(TOPIC));
    flush(&wire, &[&a, &b]);
    assert!(b.broker.mesh_remote_rows()[0].2.starts_with("retrying"), "{:?}", b.broker.mesh_remote_rows());
    std::thread::sleep(std::time::Duration::from_millis(10));
    flush(&wire, &[&a, &b]);
    a.publish(&sa, 7);
    flush(&wire, &[&a, &b]);
    assert_eq!(sb.drain().iter().map(|d| d.body["n"].clone()).collect::<Vec<_>>(), [json!(7)]);
    // The link drops: both sides forget; the next advert brings it back.
    a.hub.peer_down("node-b");
    b.hub.peer_down("node-a");
    assert!(a.hub.served().is_empty());
    a.hub.announce(&[b.peer()]);
    std::thread::sleep(std::time::Duration::from_millis(10));
    flush(&wire, &[&a, &b]);
    a.publish(&sa, 8);
    flush(&wire, &[&a, &b]);
    assert_eq!(sb.drain().iter().map(|d| d.body["n"].clone()).collect::<Vec<_>>(), [json!(8)]);
}

#[test]
fn credit_is_renewed_as_readings_arrive_and_the_last_reader_unsubscribes() {
    let (wire, a, sa, b, mut sb) = pair(MeshAudience::Paired, None);
    // A fast cog declares a high rate; the starting 600/min would drop most.
    a.broker.set_mesh_limits("board", MeshLimits { readings_per_min: 60_000, ..MeshLimits::STARTING });
    let sub = b.broker.subscribe_with(&mut sb, NAME, "telemetry.v1", false).unwrap();
    flush(&wire, &[&a, &b]);
    // More readings than one credit window (64) arrive, none dropped.
    for n in 0..150 {
        a.publish(&sa, n);
        if n % 10 == 0 {
            flush(&wire, &[&a, &b]);
        }
    }
    flush(&wire, &[&a, &b]);
    assert_eq!(sb.drain().len(), 150);
    assert_eq!(a.hub.served()[0].3, 0, "no drops at the source");
    b.broker.unsubscribe(&mut sb, sub).unwrap();
    flush(&wire, &[&a, &b]);
    assert!(a.hub.served().is_empty(), "the source stopped serving");
    assert!(b.broker.mesh_remote_rows().is_empty());
}
