//! B6b: the source side serves a reading topic to a peer under credit,
//! withdraws it when switched off, and refuses and logs what it must.

use clawft_types::cog_topics::MeshLimits;

use super::super::wire::{TopicData, TopicReq};
use super::super::{Credit, MeshAudience};
use super::*;

fn node_a() -> (Arc<Wire>, Node, Session) {
    let wire = Arc::new(Wire::default());
    let a = Node::new("node-a", &wire);
    let s = a.board(Some(MeshLimits::STARTING));
    (wire, a, s)
}

fn errors(d: &[TopicData]) -> Vec<String> {
    d.iter()
        .filter_map(|x| match x {
            TopicData::Error { code, .. } => Some(code.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn a_peer_subscribes_and_receives_readings_against_its_credit() {
    let (wire, a, s) = node_a();
    a.expo.expose(TOPIC, MeshAudience::Paired, false, &["node-b"]);
    send_req(&a, "node-b", true, &subscribe(1, 2));
    for n in 1..=5 {
        a.publish(&s, n);
    }
    a.hub.pump();
    let got = take_for(&wire, "node-b");
    // Two of credit: two readings; the other three were dropped and counted.
    assert_eq!(readings(&got).iter().map(|r| r.0).collect::<Vec<_>>(), [2, 3]);
    assert_eq!(a.hub.served(), [("node-b".into(), TOPIC.into(), 2, 3)]);
    // More credit: the next reading carries the source seq, so the drops are a gap.
    send_req(&a, "node-b", true, &TopicReq::Credit { sub: 1, credit: Credit { n: 4, bytes: 0 } });
    a.publish(&s, 6);
    a.hub.pump();
    assert_eq!(readings(&take_for(&wire, "node-b")), [(7, json!({"n": 6}))]);
    assert_eq!(a.expo.outcomes()[0], ("node-b".into(), "ok".into()));
    let mesh_ok = a.acts.0.lock().unwrap().iter().any(|x| x.action == "cogbus.mesh.subscribe" && x.instance == "mesh:node-b");
    assert!(mesh_ok, "the subscribe is in the cogbus activity log");
}

#[test]
fn switching_the_exposure_off_ends_the_subscription_at_once() {
    let (wire, a, s) = node_a();
    a.expo.expose(TOPIC, MeshAudience::Paired, false, &["node-b"]);
    send_req(&a, "node-b", true, &subscribe(1, 8));
    a.expo.off(TOPIC);
    a.hub.withdraw(&super::super::exposure_id(TOPIC));
    a.publish(&s, 1);
    a.hub.pump();
    let got = take_for(&wire, "node-b");
    assert!(readings(&got).is_empty());
    assert!(matches!(&got[..], [TopicData::End { sub: 1, reason }] if reason.contains("withdrawn")), "{got:?}");
    assert!(a.hub.served().is_empty());
    assert!(a.expo.outcomes().last().unwrap().1.starts_with("ended:withdrawn"));
    // The pump also notices an exposure that went away without a withdraw call.
    a.expo.expose(TOPIC, MeshAudience::Paired, false, &["node-b"]);
    send_req(&a, "node-b", true, &subscribe(2, 8));
    a.expo.off(TOPIC);
    a.hub.pump();
    assert!(matches!(&take_for(&wire, "node-b")[..], [TopicData::End { sub: 2, .. }]));
}

#[test]
fn peers_outside_the_audience_or_denied_by_governance_are_refused_and_logged() {
    let (wire, a, _s) = node_a();
    a.expo.expose(TOPIC, MeshAudience::Nodes(vec!["node-b".into()]), false, &["node-b"]);
    send_req(&a, "node-c", true, &subscribe(1, 8));
    assert_eq!(errors(&take_for(&wire, "node-c")), ["denied"]);
    let (who, what) = a.expo.outcomes().pop().unwrap();
    assert_eq!(who, "node-c");
    assert!(what.starts_with("refused:denied") && what.contains("not in the audience"), "{what}");
    // Governance names the peer as the principal.
    a.gov.deny.lock().unwrap().push("mesh:node-b".into());
    send_req(&a, "node-b", true, &subscribe(2, 8));
    assert_eq!(errors(&take_for(&wire, "node-b")), ["denied"]);
    let refused = a.acts.0.lock().unwrap().iter().any(|x| x.instance == "mesh:node-b" && x.outcome != crate::policy::Outcome::Ok);
    assert!(refused, "the refusal is in the cogbus activity log");
    assert!(a.hub.served().is_empty());
}

#[test]
fn an_unadmitted_peer_gets_no_answer_at_all() {
    let (wire, a, _s) = node_a();
    a.expo.expose(TOPIC, MeshAudience::Paired, false, &["node-b"]);
    send_req(&a, "node-b", false, &subscribe(1, 8));
    assert!(take_for(&wire, "node-b").is_empty());
    assert!(a.expo.outcomes().is_empty() && a.hub.served().is_empty());
}

#[test]
fn a_cog_without_mesh_limits_is_never_served() {
    let wire = Arc::new(Wire::default());
    let a = Node::new("node-a", &wire);
    let _s = a.board(None);
    a.expo.expose(TOPIC, MeshAudience::Paired, false, &["node-b"]);
    send_req(&a, "node-b", true, &subscribe(1, 8));
    assert_eq!(errors(&take_for(&wire, "node-b")), ["unavailable"]);
    // A released cog with no table runs with the starting values, still unserved.
    a.broker.set_default_mesh_limits("board");
    assert_eq!(a.broker.mesh_limits("board"), Some(MeshLimits::STARTING));
    assert!(a.broker.topic_limits_declared(TOPIC).unwrap_err().contains("declares no [mesh_limits]"));
    send_req(&a, "node-b", true, &subscribe(2, 8));
    assert_eq!(errors(&take_for(&wire, "node-b")), ["unavailable"]);
    a.broker.set_mesh_limits("board", MeshLimits::STARTING);
    assert!(a.broker.topic_limits_declared(TOPIC).is_ok());
}

#[test]
fn the_cogs_limits_bound_subscriptions_rate_and_credit_lifetime() {
    let wire = Arc::new(Wire::default());
    let a = Node::new("node-a", &wire);
    let limits = MeshLimits { readings_per_min: 60, subscriptions_per_peer: 1, credit_timeout_s: 5, ..MeshLimits::STARTING };
    let s = a.board(Some(limits));
    a.expo.expose(TOPIC, MeshAudience::Paired, false, &["node-b"]);
    send_req(&a, "node-b", true, &subscribe(1, 64));
    send_req(&a, "node-b", true, &subscribe(2, 64));
    assert_eq!(errors(&take_for(&wire, "node-b")), ["busy"], "one subscription per peer");
    // 60 a minute is a burst of one, then one a second.
    for n in 1..=4 {
        a.publish(&s, n);
    }
    a.hub.pump();
    assert_eq!(readings(&take_for(&wire, "node-b")).len(), 1);
    // Credit not renewed within the cog's timeout ends it.
    a.hub.age_credit_for_test("node-b", 1, std::time::Duration::from_secs(6));
    a.hub.pump();
    let got = take_for(&wire, "node-b");
    assert!(matches!(&got[..], [TopicData::End { reason, .. }] if reason == "credit not renewed"), "{got:?}");
}

#[test]
fn block_policy_schema_and_raw_rules_hold_at_the_source() {
    let (wire, a, _s) = node_a();
    a.expo.expose(TOPIC, MeshAudience::Paired, false, &["node-b"]);
    let mut r = subscribe(1, 8);
    if let TopicReq::Subscribe { policy, .. } = &mut r {
        *policy = "block".into();
    }
    send_req(&a, "node-b", true, &r);
    if let TopicReq::Subscribe { schema, .. } = &mut r {
        *schema = "senml.v1".into();
    }
    send_req(&a, "node-b", true, &r);
    assert_eq!(errors(&take_for(&wire, "node-b")), ["unsupported", "schema_mismatch"]);
}
