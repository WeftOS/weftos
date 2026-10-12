//! ADR-120 on the inference path: `infer.<role>` roles live in the exposure
//! registry, an off/paused/master-off exposure is neither advertised nor
//! served, serving is logged without bodies, and withdrawals reach peers.

use std::sync::Arc;

use super::mesh_forward::{ServeGate, Served, serve_infer};
use super::support::*;
use super::table::PlacementTable;
use super::tests_mesh::*;
use super::types::*;
use super::upstream::Upstream;
use super::wire;
use crate::exposure::{Audience, ExposureKind, ExposureState, Refusal};
use crate::mesh_framing::{FrameType, MeshFrame, write_frame};
use crate::mesh_test_support::connected_pair;

async fn served_table() -> (Arc<PlacementTable>, Fake) {
    let up = fake(Reply::ok("{\"ok\":true}")).await;
    let t = Arc::new(PlacementTable::new("node-b", None, None));
    t.register_local("hermes", &up.base(), Some("m".into()), "openai-v1", "LlamaCpp").unwrap();
    t.expose_to_mesh("hermes", true);
    t.allow_mesh_peer("hermes", "node-a", true);
    (t, up)
}

async fn request(t: &Arc<PlacementTable>, peer: &str, body: &str) -> Served {
    let (mut client, mut server) = connected_pair().await.unwrap();
    let u = Upstream::new(small_limits()).unwrap();
    let tt = t.clone();
    let p = trusted_peer(peer);
    let h = tokio::spawn(async move { serve_infer(&mut server, &p, tt.as_ref(), &u, &ServeGate::default(), None).await.unwrap() });
    let req = ProxyRequest {
        role: "hermes".into(), method: Method::Post, path: "/v1/chat/completions".into(),
        content_type: Some("application/json".into()), accept: None, authorization: None, body: body.as_bytes().to_vec(),
    };
    write_frame(&mut client, &MeshFrame { frame_type: FrameType::InferRequest, payload: wire::encode_request(&req).unwrap() }).await.unwrap();
    // Drain the reply so the server side can finish.
    while let Ok(f) = crate::mesh_framing::read_frame(&mut client).await {
        if matches!(wire::decode_resp(&f.payload), Ok(wire::Resp::End | wire::Resp::Error(_))) {
            break;
        }
    }
    h.await.unwrap()
}

#[tokio::test]
async fn an_exposed_role_is_registered_as_infer_role_with_a_paired_audience() {
    let (t, _up) = served_table().await;
    let e = t.exposures().get("infer.hermes").expect("registered");
    assert_eq!(e.kind, ExposureKind::Inference);
    assert_eq!(e.audience, Audience::Paired);
    assert_eq!(e.state, ExposureState::On);
    assert!(e.announced);
    assert!(t.exposures().check_serve("infer.hermes").is_ok());
    // A role that was never exposed is not in the registry at all.
    let t2 = PlacementTable::new("n", None, None);
    t2.register_local("other", "http://127.0.0.1:9", None, "openai-v1", "LlamaCpp").unwrap();
    assert!(t2.exposures().get("infer.other").is_none());
}

#[tokio::test]
async fn exposed_before_the_instance_exists_is_not_announced_and_not_served() {
    let up = fake(Reply::ok("{}")).await;
    let t = PlacementTable::new("node-b", None, None);
    t.allow_mesh_peer("hermes", "node-a", true);
    t.expose_to_mesh("hermes", true);
    assert_eq!(t.serve_refusal("hermes"), Some(Refusal::NotAnnounced));
    assert!(t.advertisement("hermes", 1).is_none());
    t.register_local("hermes", &up.base(), None, "openai-v1", "LlamaCpp").unwrap();
    assert_eq!(t.serve_refusal("hermes"), None);
    assert!(t.advertisement("hermes", 1).is_some());
    t.deregister_local("hermes");
    assert_eq!(t.serve_refusal("hermes"), Some(Refusal::NotAnnounced), "instance gone: reachable-but-unannounced is refused");
}

#[tokio::test]
async fn off_and_pause_withdraw_the_advertisement_and_refuse_serving_at_once() {
    let (t, _up) = served_table().await;
    assert!(t.advertisement("hermes", 1).is_some() && t.local_for_peer("hermes", "node-a").is_some());
    let reg = t.exposures();
    reg.set_state("infer.hermes", ExposureState::Off, "test").unwrap();
    assert!(t.advertisement("hermes", 1).is_none());
    assert!(t.advertisements(1).is_empty());
    assert!(t.local_for_peer("hermes", "node-a").is_none());
    assert_eq!(reg.drain_withdrawals(), vec!["infer.hermes".to_string()]);
    reg.set_state("infer.hermes", ExposureState::On, "test").unwrap();
    assert!(t.local_for_peer("hermes", "node-a").is_some());
    reg.set_state("infer.hermes", ExposureState::Paused, "test").unwrap();
    assert!(t.local_for_peer("hermes", "node-a").is_none());
    reg.set_state("infer.hermes", ExposureState::On, "test").unwrap();
    reg.set_master(false);
    assert!(t.advertisement("hermes", 1).is_none() && t.local_for_peer("hermes", "node-a").is_none());
}

#[tokio::test]
async fn stopping_the_exposure_queues_a_withdrawal_for_peers() {
    let (t, _up) = served_table().await;
    t.expose_to_mesh("hermes", false);
    assert_eq!(t.exposures().drain_withdrawals(), vec!["infer.hermes".to_string()]);
    assert_eq!(t.exposures().get("infer.hermes").unwrap().state, ExposureState::Off);
}

#[tokio::test]
async fn config_cannot_re_expose_what_the_operator_switched_off() {
    let (t, _up) = served_table().await;
    t.exposures().set_state("infer.hermes", ExposureState::Off, "cli").unwrap();
    t.expose_to_mesh("hermes", true);
    assert_eq!(t.exposures().get("infer.hermes").unwrap().state, ExposureState::Off);
    assert!(t.local_for_peer("hermes", "node-a").is_none());
}

#[tokio::test]
async fn serving_is_logged_with_sizes_and_never_the_body() {
    let (t, _up) = served_table().await;
    let body = r#"{"messages":[{"role":"user","content":"SECRET-PROMPT"}]}"#;
    assert_eq!(request(&t, "node-a", body).await, Served::Ok);
    let recs = t.exposures().activity().query(Some("infer.hermes"), 10);
    assert_eq!(recs.len(), 1);
    let r = &recs[0];
    assert_eq!((r.caller_node.as_str(), r.outcome.as_str()), ("node-a", "ok"));
    assert!(r.bytes_in > 0 && r.bytes_out > 0, "{r:?}");
    assert!(!serde_json::to_string(&recs).unwrap().contains("SECRET-PROMPT"));
    assert!(t.exposures().activity().verify().is_ok());
}

#[tokio::test]
async fn a_refused_request_is_logged_with_the_reason() {
    let (t, up) = served_table().await;
    t.exposures().set_state("infer.hermes", ExposureState::Off, "cli").unwrap();
    assert!(matches!(request(&t, "node-a", "{}").await, Served::Failed(_)));
    assert_eq!(up.count(), 0, "the model server was not reached");
    let recs = t.exposures().activity().query(Some("infer.hermes"), 10);
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].outcome, "refused:switched off locally");
    let report = t.exposures().report(super::super::exposure::registry::now_ms());
    let item = &report["items"][0];
    assert_eq!((item["n24"].as_u64(), item["err24"].as_u64()), (Some(1), Some(1)));
}

#[tokio::test]
async fn a_peer_withdrawal_removes_only_the_senders_entry() {
    let c = cluster(Reply::ok("b")).await;
    assert!(advertise(&c));
    assert!(c.a.resolve("hermes").is_some());
    assert!(!c.a.withdraw_remote("node-z", "infer.hermes"), "someone else's withdrawal changes nothing");
    assert!(c.a.resolve("hermes").is_some());
    assert!(!c.a.withdraw_remote("node-b", "not-infer"), "malformed names are refused");
    assert!(c.a.withdraw_remote("node-b", "infer.hermes"));
    assert!(c.a.resolve("hermes").is_none());
}
