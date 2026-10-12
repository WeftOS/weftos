//! The display-session wire frames and the leaf-side state machine.

use weftos_leaf_scene::session::{
    b64, Action, DisplayService, LeafInfo, ServiceConfig, SessionFrame, SessionId, SYSTEM_ID_BASE,
};
use weftos_leaf_scene::{
    px, InputEnvelope, InputEvent, Layer, Node, NodeId, Primitive, SceneEnvelope, SceneOp,
};

fn cfg() -> ServiceConfig {
    ServiceConfig {
        display: 0,
        info: LeafInfo { width: 800, height: 480, render_caps: 0, touch: true, ptt: true },
        max_nodes: 8,
        max_ops_per_sec: 100,
        image_budget_bytes: 64,
        hide_on_open: vec![NodeId::from_parts(0, 0xFE_0001)],
    }
}

fn rect(id: u32) -> Node {
    Node::new(NodeId::from_raw(id), Layer::Widget, Primitive::Rect { w: px(10), h: px(10), radius_q8: 0 })
}

fn bitmap(id: u32, bytes: usize) -> Node {
    Node::new(
        NodeId::from_raw(id),
        Layer::Widget,
        Primitive::Bitmap { w: px(1), h: px((bytes / 2) as i32), format: weftos_leaf_scene::BitmapFormat::Raw565, data: vec![0; bytes] },
    )
}

fn open(svc: &mut DisplayService, who: &str, now: u64) -> Vec<Action> {
    svc.handle(SessionFrame::Open { subject: who.into(), display: 0, proof: vec![] }, now)
}

fn session_of(actions: &[Action]) -> SessionId {
    actions
        .iter()
        .find_map(|a| match a {
            Action::Send(SessionFrame::Opened { session, .. }) => Some(*session),
            _ => None,
        })
        .expect("opened")
}

fn ops(session: SessionId, seq: u32, ops: Vec<SceneOp>) -> SessionFrame {
    SessionFrame::Ops { session, seq, more: false, envelope: SceneEnvelope::new(0, ops) }
}

fn refusal(actions: &[Action]) -> String {
    match actions {
        [Action::Send(SessionFrame::Refused { reason })] => reason.clone(),
        [Action::Send(SessionFrame::Rejected { reason, .. })] => reason.clone(),
        [Action::Send(SessionFrame::Ended { reason, .. })] => reason.clone(),
        other => panic!("expected one refusal, got {other:?}"),
    }
}

#[test]
fn frames_survive_the_json_carrier() {
    let frame = ops(SessionId(7), 3, vec![SceneOp::Insert(rect(5)), SceneOp::Remove(NodeId::from_raw(9))]);
    let json = frame.to_carrier("ab".repeat(16).as_str()).unwrap();
    assert!(json.contains("\"type\":\"display_session\""), "{json}");
    assert!(serde_free_json_ok(&json));
    assert_eq!(SessionFrame::from_carrier(json.as_bytes()).unwrap().unwrap(), frame);
}

fn serde_free_json_ok(s: &str) -> bool {
    s.starts_with('{') && s.ends_with('}') && !s.contains('\n')
}

#[test]
fn other_carriers_are_not_session_frames() {
    let touch = br#"{"type":"leaf_input","cbor_b64":"AAAA","target_pubkey":"x"}"#;
    assert!(SessionFrame::from_carrier(touch).is_none());
    // A session carrier with bad CBOR is an error, not "not mine".
    let bad = br#"{"type":"display_session","cbor_b64":"AAAA"}"#;
    assert!(SessionFrame::from_carrier(bad).unwrap().is_err());
    assert!(SessionFrame::from_carrier(br#"{"type":"display_session"}"#).is_none());
}

#[test]
fn base64_is_strict() {
    for len in 0..12usize {
        let bytes: Vec<u8> = (0..len as u8).map(|b| b.wrapping_mul(37)).collect();
        assert_eq!(b64::decode(b64::encode(&bytes).as_bytes()).unwrap(), bytes);
    }
    assert!(b64::decode(b"ab!d").is_none(), "non-alphabet byte");
    assert!(b64::decode(b"ab===").is_none(), "three pad bytes");
    assert!(b64::decode(b"=abc").is_none(), "pad inside the data");
}

#[test]
fn open_reports_the_leaf_and_hides_the_boot_screen() {
    let mut svc = DisplayService::new(cfg());
    let out = open(&mut svc, "cos-console", 0);
    assert!(matches!(&out[0], Action::Apply { envelope, render: true }
        if envelope.ops == vec![SceneOp::Remove(NodeId::from_parts(0, 0xFE_0001))]));
    let Action::Send(SessionFrame::Opened { session, info, max_nodes, .. }) = &out[1] else { panic!("{out:?}") };
    assert_eq!((*session, info.width, info.height, *max_nodes), (SessionId(1), 800, 480, 8));
    assert_eq!(svc.active(), Some(SessionId(1)));
}

#[test]
fn open_is_idempotent_for_the_same_subject_and_refused_for_another() {
    let mut svc = DisplayService::new(cfg());
    let first = session_of(&open(&mut svc, "cos-console", 0));
    let again = open(&mut svc, "cos-console", 10);
    assert_eq!(session_of(&again), first);
    assert_eq!(again.len(), 1, "the boot screen is hidden once");
    assert_eq!(refusal(&open(&mut svc, "intruder", 20)), "display busy: held by cos-console");
    assert_eq!(svc.active(), Some(first));
}

#[test]
fn open_on_another_display_is_refused() {
    let mut svc = DisplayService::new(cfg());
    let out = svc.handle(SessionFrame::Open { subject: "a".into(), display: 3, proof: vec![] }, 0);
    assert_eq!(refusal(&out), "no such display");
    assert_eq!(svc.active(), None);
}

#[test]
fn ops_apply_and_replays_are_dropped() {
    let mut svc = DisplayService::new(cfg());
    let s = session_of(&open(&mut svc, "a", 0));
    let out = svc.handle(ops(s, 1, vec![SceneOp::Insert(rect(1))]), 1);
    assert!(matches!(&out[..], [Action::Apply { render: true, .. }]));
    assert_eq!(svc.owned_nodes(), 1);
    assert!(svc.handle(ops(s, 1, vec![SceneOp::Insert(rect(2))]), 2).is_empty(), "same seq: replay");
    assert_eq!(svc.owned_nodes(), 1);
    assert!(!svc.handle(ops(s, 2, vec![SceneOp::Insert(rect(2))]), 3).is_empty());
    assert_eq!(svc.owned_nodes(), 2);
}

#[test]
fn fragments_defer_the_render_to_the_last_one() {
    let mut svc = DisplayService::new(cfg());
    let s = session_of(&open(&mut svc, "a", 0));
    let frag = SessionFrame::Ops { session: s, seq: 1, more: true, envelope: SceneEnvelope::new(0, vec![SceneOp::Insert(rect(1))]) };
    assert!(matches!(&svc.handle(frag, 1)[..], [Action::Apply { render: false, .. }]));
    assert!(matches!(&svc.handle(ops(s, 2, vec![SceneOp::Insert(rect(2))]), 2)[..], [Action::Apply { render: true, .. }]));
}

#[test]
fn ops_for_an_unknown_session_end_it() {
    let mut svc = DisplayService::new(cfg());
    assert_eq!(refusal(&svc.handle(ops(SessionId(9), 1, vec![SceneOp::Clear]), 0)), "no such session");
    let s = session_of(&open(&mut svc, "a", 0));
    assert_eq!(refusal(&svc.handle(ops(SessionId(s.0 + 1), 1, vec![SceneOp::Clear]), 1)), "no such session");
    assert_eq!(svc.active(), Some(s), "a stranger's frame does not end the holder");
}

#[test]
fn firmware_ids_are_out_of_reach_and_the_batch_is_all_or_nothing() {
    let mut svc = DisplayService::new(cfg());
    let s = session_of(&open(&mut svc, "a", 0));
    let batch = vec![SceneOp::Insert(rect(1)), SceneOp::Insert(rect(SYSTEM_ID_BASE + 1))];
    assert_eq!(refusal(&svc.handle(ops(s, 1, batch), 1)), "node id is reserved for the firmware");
    assert_eq!(svc.owned_nodes(), 0, "the first op of the refused batch was not kept");
    let remove = vec![SceneOp::Remove(NodeId::from_raw(0xFE_0001))];
    assert_eq!(refusal(&svc.handle(ops(s, 2, remove), 2)), "node id is reserved for the firmware");
    let other_display = vec![SceneOp::Remove(NodeId::from_parts(1, 5))];
    assert_eq!(refusal(&svc.handle(ops(s, 3, other_display), 3)), "node is on another display");
}

#[test]
fn tweens_and_layer_blend_are_checked() {
    let mut svc = DisplayService::new(cfg());
    let s = session_of(&open(&mut svc, "a", 0));
    let cancel = vec![SceneOp::CancelTween { id: NodeId::from_raw(4), property: None }];
    assert_eq!(refusal(&svc.handle(ops(s, 1, cancel), 1)), "node not owned by this session");
    let blend = vec![SceneOp::SetLayerBlend { layer: Layer::Widget, mode: weftos_leaf_scene::BlendMode::Normal }];
    assert_eq!(refusal(&svc.handle(ops(s, 2, blend), 2)), "layer blend is not allowed in a session");
}

#[test]
fn node_image_and_rate_budgets_refuse_the_batch() {
    let mut svc = DisplayService::new(cfg());
    let s = session_of(&open(&mut svc, "a", 0));
    let nine: Vec<SceneOp> = (1..=9).map(|i| SceneOp::Insert(rect(i))).collect();
    assert_eq!(refusal(&svc.handle(ops(s, 1, nine), 1)), "node budget exceeded");
    assert_eq!(svc.owned_nodes(), 0);
    assert_eq!(refusal(&svc.handle(ops(s, 2, vec![SceneOp::Insert(bitmap(1, 66))]), 2)), "image budget exceeded");
    let many: Vec<SceneOp> = (0..101).map(|_| SceneOp::Remove(NodeId::from_raw(1))).collect();
    assert_eq!(refusal(&svc.handle(ops(s, 3, many), 3)), "op rate exceeded");
    // A new second, a new window.
    assert!(!svc.handle(ops(s, 4, vec![SceneOp::Insert(rect(1))]), 1_500).is_empty());
}

#[test]
fn clear_and_replace_stay_inside_the_session() {
    let mut svc = DisplayService::new(cfg());
    let s = session_of(&open(&mut svc, "a", 0));
    svc.handle(ops(s, 1, vec![SceneOp::Insert(rect(1)), SceneOp::Insert(rect(2))]), 1);
    let out = svc.handle(ops(s, 2, vec![SceneOp::Clear, SceneOp::Insert(rect(3))]), 2);
    let [Action::Apply { envelope, .. }] = &out[..] else { panic!("{out:?}") };
    assert_eq!(
        envelope.ops,
        vec![
            SceneOp::Remove(NodeId::from_raw(1)),
            SceneOp::Remove(NodeId::from_raw(2)),
            SceneOp::Insert(rect(3)),
        ],
        "Clear removes the session's nodes one by one, never the firmware's"
    );
    assert_eq!(svc.owned_nodes(), 1);
}

#[test]
fn close_removes_the_nodes_and_frees_the_screen() {
    let mut svc = DisplayService::new(cfg());
    let s = session_of(&open(&mut svc, "a", 0));
    svc.handle(ops(s, 1, vec![SceneOp::Insert(rect(1))]), 1);
    let out = svc.handle(SessionFrame::Close { session: s }, 2);
    assert!(matches!(&out[0], Action::Apply { envelope, render: true } if envelope.ops == vec![SceneOp::Remove(NodeId::from_raw(1))]));
    assert!(matches!(&out[1], Action::SessionEnded));
    assert_eq!(out.len(), 2, "a close the host asked for needs no Ended frame");
    assert_eq!(svc.active(), None);
    // The screen is free for the next app.
    assert_eq!(session_of(&open(&mut svc, "b", 3)), SessionId(2));
}

#[test]
fn a_silent_host_loses_the_screen_and_is_told() {
    let mut svc = DisplayService::new(cfg());
    let s = session_of(&open(&mut svc, "a", 0));
    svc.handle(ops(s, 1, vec![SceneOp::Insert(rect(1))]), 1);
    assert!(svc.tick(40_000).is_empty());
    // A keepalive (empty ops) keeps it.
    svc.handle(ops(s, 2, vec![]), 40_000);
    assert!(svc.tick(80_000).is_empty());
    let out = svc.tick(90_000);
    assert!(matches!(&out[0], Action::Apply { .. }));
    assert!(out.iter().any(|a| matches!(a, Action::Send(SessionFrame::Ended { session, .. }) if *session == s)));
    assert_eq!(svc.active(), None);
}

#[test]
fn input_and_sensors_name_the_session_and_need_one() {
    let mut svc = DisplayService::new(cfg());
    let env = InputEnvelope::new(0, Some(NodeId::from_raw(1)), InputEvent::PointerUp { pointer_id: 0, x: 0, y: 0 });
    assert!(svc.input(env.clone()).is_none(), "no session: the raw touch path applies");
    assert!(svc.sensor("ptt.down", true).is_none());
    let s = session_of(&open(&mut svc, "a", 0));
    assert_eq!(svc.input(env.clone()), Some(SessionFrame::Input { session: s, envelope: env }));
    assert_eq!(
        svc.sensor("ptt.up", false),
        Some(SessionFrame::Sensor { session: s, name: "ptt.up".into(), pressed: Some(false), value: None })
    );
}

#[test]
fn link_loss_ends_the_session() {
    let mut svc = DisplayService::new(cfg());
    let s = session_of(&open(&mut svc, "a", 0));
    let out = svc.link_lost();
    assert!(out.iter().any(|a| matches!(a, Action::Send(SessionFrame::Ended { session, .. }) if *session == s)));
    assert!(svc.link_lost().is_empty());
}

#[test]
fn leaf_to_host_frames_are_ignored_in_the_wrong_direction() {
    let mut svc = DisplayService::new(cfg());
    let s = session_of(&open(&mut svc, "a", 0));
    assert!(svc.handle(SessionFrame::Ended { session: s, reason: "x".into() }, 1).is_empty());
    assert_eq!(svc.active(), Some(s));
}

// Silence an unused-import warning on toolchains that inline `px`.
#[allow(dead_code)]
fn _px() -> i32 {
    px(1)
}

#[test]
fn envelopes_are_the_json_the_parent_decodes() {
    use weftos_leaf_scene::session::envelope::{publish_envelope, session_publish, subscribe_envelope};
    let id = "ab".repeat(16);
    let sub: serde_json::Value = serde_json::from_str(&subscribe_envelope(&id, "mesh.leaf.x.push")).unwrap();
    assert_eq!(sub["message"]["target"]["Topic"], "mesh.subscribe");
    assert_eq!(sub["message"]["payload"]["Json"]["topic"], "mesh.leaf.x.push");

    let frame = SessionFrame::Sensor { session: SessionId(2), name: "ptt.down".into(), pressed: Some(true), value: None };
    let (topic, env) = session_publish(&id, &frame).unwrap();
    assert_eq!(topic, format!("mesh.leaf.{id}.input"));
    let v: serde_json::Value = serde_json::from_str(&env).unwrap();
    assert_eq!(v["source_node"], id.as_str());
    assert_eq!(v["message"]["target"]["Topic"], "ipc.publish");
    assert_eq!(v["message"]["payload"]["Json"]["topic"], topic.as_str());
    // The inner message is itself JSON text carrying the frame.
    let inner = v["message"]["payload"]["Json"]["message"].as_str().unwrap();
    assert_eq!(SessionFrame::from_carrier(inner.as_bytes()).unwrap().unwrap(), frame);
    // Quotes and backslashes in a message survive the escaping.
    let odd: serde_json::Value = serde_json::from_str(&publish_envelope(&id, "t", "a\"b\\c")).unwrap();
    assert_eq!(odd["message"]["payload"]["Json"]["message"], "a\"b\\c");
}
