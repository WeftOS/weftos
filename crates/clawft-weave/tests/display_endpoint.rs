//! The daemon-side display endpoint against a loopback leaf: the real
//! `DisplayService` state machine (the one the pad firmware runs) behind
//! a fake sink, so frames take exactly the wire path without a socket.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use clawft_weave::display_endpoint::{
    chunk_ops, DisplayEndpoint, EndpointConfig, EndpointError, FrameSink, PollEvent, PollReply, RevocablePolicy,
};
use weftos_leaf_scene::session::{Action, DisplayService, LeafInfo, ServiceConfig, SessionFrame, SessionId, MAX_PUSH_CBOR};
use weftos_leaf_scene::{
    codec, px, InputEnvelope, InputEvent, Layer, Node, NodeId, Primitive, SceneEnvelope, SceneOp,
};

const LEAF: &str = "0123456789abcdef0123456789abcdef";

fn service() -> DisplayService {
    DisplayService::new(ServiceConfig {
        display: 0,
        info: LeafInfo { width: 800, height: 480, render_caps: 0, touch: true, ptt: true },
        max_nodes: 4096,
        max_ops_per_sec: 10_000,
        image_budget_bytes: 1 << 20,
        hide_on_open: vec![],
    })
}

/// The leaf: runs frames through `DisplayService`, records what it was
/// told to render and send, and answers into the endpoint.
struct LoopLeaf {
    svc: Mutex<DisplayService>,
    frames: Mutex<Vec<SessionFrame>>,
    applied: Mutex<Vec<(usize, bool)>>,
    replies: Option<tokio::sync::mpsc::UnboundedSender<SessionFrame>>,
    reachable: Mutex<bool>,
    started: std::time::Instant,
}

impl LoopLeaf {
    fn new(replies: Option<tokio::sync::mpsc::UnboundedSender<SessionFrame>>) -> Arc<Self> {
        Arc::new(Self {
            svc: Mutex::new(service()),
            frames: Mutex::new(Vec::new()),
            applied: Mutex::new(Vec::new()),
            replies,
            reachable: Mutex::new(true),
            started: std::time::Instant::now(),
        })
    }
}

#[async_trait]
impl FrameSink for LoopLeaf {
    async fn push(&self, _leaf: &str, carrier: String) -> Result<(), String> {
        if !*self.reachable.lock().unwrap() {
            return Err("leaf is not connected".into());
        }
        let frame = SessionFrame::from_carrier(carrier.as_bytes()).expect("a session carrier").expect("decodes");
        self.frames.lock().unwrap().push(frame.clone());
        let now = self.started.elapsed().as_millis() as u64;
        for action in self.svc.lock().unwrap().handle(frame, now) {
            match action {
                Action::Send(f) => {
                    if let Some(tx) = &self.replies {
                        let _ = tx.send(f);
                    }
                }
                Action::Apply { envelope, render } => self.applied.lock().unwrap().push((envelope.ops.len(), render)),
                Action::SessionEnded => {}
            }
        }
        Ok(())
    }
}

struct Harness {
    ep: Arc<DisplayEndpoint>,
    leaf: Arc<LoopLeaf>,
    policy: Arc<RevocablePolicy>,
}

fn harness(cfg: EndpointConfig) -> Harness {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let leaf = LoopLeaf::new(Some(tx));
    let policy = Arc::new(RevocablePolicy::default());
    let ep = Arc::new(DisplayEndpoint::new(leaf.clone(), policy.clone(), cfg));
    let feed = Arc::clone(&ep);
    tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            feed.on_leaf_frame(LEAF, frame);
        }
    });
    Harness { ep, leaf, policy }
}

fn quick() -> EndpointConfig {
    EndpointConfig { open_timeout: Duration::from_millis(300), ..EndpointConfig::default() }
}

fn rect(id: u32) -> SceneOp {
    SceneOp::Insert(Node::new(NodeId::from_raw(id), Layer::Widget, Primitive::Rect { w: px(10), h: px(10), radius_q8: 0 }))
}

fn text(id: u32, len: usize) -> SceneOp {
    SceneOp::Insert(Node::new(
        NodeId::from_raw(id),
        Layer::Text,
        Primitive::Text {
            content: "x".repeat(len),
            face: weftos_leaf_scene::FontFace::Builtin(weftos_leaf_scene::BuiltinFont::Mono10x20),
            size_q8: 20 << 8,
            weight: 400,
            kerning: weftos_leaf_scene::KerningHint::Auto,
        },
    ))
}

async fn open(h: &Harness, who: &str) -> Result<u64, EndpointError> {
    h.ep.open(LEAF, who, "weftos://m/nodes/x/services/display", vec![], 0).await.map(|r| r.session)
}

async fn until<F: FnMut() -> bool>(mut f: F) -> bool {
    for _ in 0..400 {
        if f() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    false
}

async fn until_poll(h: &Harness, s: u64, ok: impl Fn(&PollReply) -> bool) -> PollReply {
    let mut acc = PollReply::default();
    for _ in 0..400 {
        let p = h.ep.poll(s, 64).await.unwrap();
        acc.events.extend(p.events);
        acc.rejected.extend(p.rejected);
        acc.ended = acc.ended.or(p.ended);
        if ok(&acc) {
            return acc;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("condition never held: {acc:?}");
}

#[tokio::test]
async fn open_reports_the_leaf_and_is_idempotent_for_the_subject() {
    let h = harness(quick());
    let r = h.ep.open(LEAF, "cos-console", "", vec![1, 2], 0).await.unwrap();
    assert_eq!((r.width, r.height, r.touch, r.ptt, r.max_nodes), (800, 480, true, true, 4096));
    let again = h.ep.open(LEAF, "cos-console", "", vec![], 0).await.unwrap();
    assert_eq!(again.session, r.session, "same subject, same session");
    let frames = h.leaf.frames.lock().unwrap();
    let opens = frames.iter().filter(|f| matches!(f, SessionFrame::Open { .. })).count();
    assert_eq!(opens, 1, "the leaf was asked once");
    let SessionFrame::Open { proof, .. } = &frames[0] else { panic!() };
    assert_eq!(proof, &vec![1, 2], "the proof reaches the leaf");
}

#[tokio::test]
async fn a_second_app_is_refused_by_the_leaf_with_its_reason() {
    let h = harness(quick());
    open(&h, "cos-console").await.unwrap();
    let err = open(&h, "intruder").await.unwrap_err();
    assert_eq!(err, EndpointError::Refused("display busy: held by cos-console".into()));
    assert_eq!(err.kind(), "display_refused");
}

#[tokio::test]
async fn an_unreachable_or_silent_leaf_is_an_error_not_a_hang() {
    let h = harness(quick());
    *h.leaf.reachable.lock().unwrap() = false;
    let err = open(&h, "a").await.unwrap_err();
    assert!(matches!(&err, EndpointError::Unreachable(w) if w.contains("not connected")), "{err}");

    // Reachable, but its answers go nowhere.
    let silent = LoopLeaf::new(None);
    let ep = DisplayEndpoint::new(silent, Arc::new(RevocablePolicy::default()), quick());
    let err = ep.open(LEAF, "a", "", vec![], 0).await.unwrap_err();
    assert!(matches!(&err, EndpointError::Unreachable(w) if w.contains("no answer")), "{err}");
}

#[tokio::test]
async fn malformed_leaf_ids_and_unknown_sessions_refuse() {
    let h = harness(quick());
    for bad in ["", "abc", &"A".repeat(32), &"g".repeat(32), &"a".repeat(33)] {
        let err = h.ep.open(bad, "a", "", vec![], 0).await.unwrap_err();
        assert!(matches!(err, EndpointError::Bad(_)), "{bad:?}: {err}");
    }
    assert_eq!(h.ep.poll(99, 8).await.unwrap_err(), EndpointError::NoSession);
    assert_eq!(h.ep.ops(99, SceneEnvelope::new(0, vec![])).await.unwrap_err(), EndpointError::NoSession);
    assert!(h.ep.close(99).await.is_ok(), "closing nothing is fine");
}

#[tokio::test]
async fn open_fields_are_bounded() {
    let h = harness(quick());
    for (subject, resource, proof) in [
        ("", "r", vec![]),
        (&"s".repeat(65)[..], "r", vec![]),
        ("bad\nname", "r", vec![]),
        ("ok", &"r".repeat(257)[..], vec![]),
        ("ok", "r", vec![0u8; 2049]),
    ] {
        let err = h.ep.open(LEAF, subject, resource, proof, 0).await.unwrap_err();
        assert!(matches!(err, EndpointError::Bad(_)), "{subject:?} {err}");
    }
    assert_eq!(h.leaf.frames.lock().unwrap().len(), 0, "nothing reached the leaf");
}

#[tokio::test]
async fn a_large_batch_goes_out_in_fragments_that_render_once() {
    let h = harness(quick());
    let s = open(&h, "a").await.unwrap();
    // 40 nodes of 400-byte text: far past one frame.
    let ops: Vec<SceneOp> = (1..=40).map(|i| text(i, 400)).collect();
    let chunks = chunk_ops(&ops, MAX_PUSH_CBOR).unwrap();
    assert!(chunks.len() > 2, "{} chunks", chunks.len());
    h.ep.ops(s, SceneEnvelope::new(0, ops)).await.unwrap();

    let frames = h.leaf.frames.lock().unwrap().clone();
    let mut seqs = vec![];
    for f in &frames {
        if let SessionFrame::Ops { seq, more, envelope, .. } = f {
            let cbor = codec::encode(f).unwrap();
            assert!(cbor.len() <= MAX_PUSH_CBOR, "frame of {} bytes", cbor.len());
            seqs.push((*seq, *more, envelope.ops.len()));
        }
    }
    assert_eq!(seqs.len(), chunks.len());
    assert!(seqs.windows(2).all(|w| w[1].0 == w[0].0 + 1), "seq counts up: {seqs:?}");
    assert!(seqs[..seqs.len() - 1].iter().all(|s| s.1) && !seqs.last().unwrap().1, "only the last closes the batch");
    assert_eq!(seqs.iter().map(|s| s.2).sum::<usize>(), 40);
    let applied = h.leaf.applied.lock().unwrap().clone();
    assert_eq!(applied.iter().filter(|a| a.1).count(), 1, "one render for the whole batch");
}

#[tokio::test]
async fn one_op_too_big_for_the_channel_is_refused_not_dropped() {
    let h = harness(quick());
    let s = open(&h, "a").await.unwrap();
    let err = h.ep.ops(s, SceneEnvelope::new(0, vec![text(1, MAX_PUSH_CBOR)])).await.unwrap_err();
    assert!(matches!(&err, EndpointError::Bad(w) if w.contains("one scene op")), "{err}");
    assert_eq!(h.leaf.frames.lock().unwrap().iter().filter(|f| matches!(f, SessionFrame::Ops { .. })).count(), 0);
}

#[tokio::test]
async fn input_and_sensor_events_reach_the_app_and_a_leaf_rejection_is_reported() {
    let h = harness(quick());
    let s = open(&h, "a").await.unwrap();
    let session = h.leaf.svc.lock().unwrap().active().unwrap();
    let env = InputEnvelope::new(0, Some(NodeId::from_raw(7)), InputEvent::PointerUp { pointer_id: 0, x: 256, y: 512 });
    h.ep.on_leaf_frame(LEAF, SessionFrame::Input { session, envelope: env.clone() });
    h.ep.on_leaf_frame(LEAF, SessionFrame::Sensor { session, name: "ptt.down".into(), pressed: Some(true), value: None });
    // A frame for some other session is not ours.
    h.ep.on_leaf_frame(LEAF, SessionFrame::Sensor { session: SessionId(99), name: "ptt.up".into(), pressed: Some(false), value: None });
    // The leaf refuses a batch that touches a firmware id.
    h.ep.ops(s, SceneEnvelope::new(0, vec![rect(0xF0_0001)])).await.unwrap();

    let p = until_poll(&h, s, |p| !p.rejected.is_empty()).await;
    assert_eq!(p.events.len(), 2);
    let PollEvent::Input { cbor_b64 } = &p.events[0] else { panic!("{:?}", p.events) };
    let back = codec::decode_input_envelope(&weftos_leaf_scene::session::b64::decode(cbor_b64.as_bytes()).unwrap()).unwrap();
    assert_eq!(back, env);
    assert_eq!(p.events[1], PollEvent::Sensor { name: "ptt.down".into(), pressed: Some(true), value: None });
    assert!(p.rejected[0].contains("reserved for the firmware"), "{:?}", p.rejected);
    assert!(p.ended.is_none());
    assert!(h.ep.poll(s, 8).await.unwrap().events.is_empty(), "polled events are consumed");
}

#[tokio::test]
async fn the_event_queue_is_bounded_and_counts_what_it_drops() {
    let h = harness(EndpointConfig { max_events: 3, ..quick() });
    let s = open(&h, "a").await.unwrap();
    let session = h.leaf.svc.lock().unwrap().active().unwrap();
    for i in 0..5 {
        h.ep.on_leaf_frame(LEAF, SessionFrame::Sensor { session, name: format!("e{i}"), pressed: None, value: Some(i as f64) });
    }
    let p = h.ep.poll(s, 64).await.unwrap();
    assert_eq!(p.dropped, 2);
    let names: Vec<_> = p
        .events
        .iter()
        .map(|e| match e {
            PollEvent::Sensor { name, .. } => name.clone(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(names, ["e2", "e3", "e4"], "the oldest went");
}

#[tokio::test]
async fn revoking_a_grant_ends_the_session_on_the_next_call() {
    let h = harness(quick());
    let s = open(&h, "cos-console").await.unwrap();
    h.ep.ops(s, SceneEnvelope::new(0, vec![rect(1)])).await.unwrap();
    assert_eq!(h.leaf.svc.lock().unwrap().owned_nodes(), 1);
    h.policy.revoke("cos-console");

    let err = h.ep.ops(s, SceneEnvelope::new(0, vec![rect(2)])).await.unwrap_err();
    assert_eq!(err.kind(), "display_denied");
    assert!(until(|| h.leaf.svc.lock().unwrap().active().is_none()).await, "the leaf was told to close");
    assert_eq!(h.leaf.svc.lock().unwrap().owned_nodes(), 0, "its objects are cleared");
    assert_eq!(h.ep.poll(s, 8).await.unwrap_err(), EndpointError::NoSession);
    // And a revoked subject cannot reopen.
    assert_eq!(open(&h, "cos-console").await.unwrap_err().kind(), "display_denied");
}

#[tokio::test]
async fn a_leaf_that_ends_the_session_is_reported_and_blocks_further_ops() {
    let h = harness(quick());
    let s = open(&h, "a").await.unwrap();
    let session = h.leaf.svc.lock().unwrap().active().unwrap();
    h.ep.on_leaf_frame(LEAF, SessionFrame::Ended { session, reason: "link to the host dropped".into() });
    let p = h.ep.poll(s, 8).await.unwrap();
    assert_eq!(p.ended.as_deref(), Some("link to the host dropped"));
    let err = h.ep.ops(s, SceneEnvelope::new(0, vec![rect(1)])).await.unwrap_err();
    assert_eq!(err, EndpointError::Ended("link to the host dropped".into()));
}

#[tokio::test]
async fn close_frees_the_screen_for_the_next_app() {
    let h = harness(quick());
    let s = open(&h, "a").await.unwrap();
    h.ep.ops(s, SceneEnvelope::new(0, vec![rect(1), rect(2)])).await.unwrap();
    h.ep.close(s).await.unwrap();
    assert!(until(|| h.leaf.svc.lock().unwrap().active().is_none()).await);
    assert_eq!(h.leaf.svc.lock().unwrap().owned_nodes(), 0);
    h.ep.close(s).await.unwrap();
    open(&h, "b").await.expect("the screen is free");
}

#[tokio::test]
async fn keepalives_flow_and_an_abandoned_session_is_closed() {
    let h = harness(EndpointConfig { keepalive: Duration::from_millis(40), client_idle: Duration::from_millis(400), ..quick() });
    let s = open(&h, "a").await.unwrap();
    assert!(
        until(|| h.leaf.frames.lock().unwrap().iter().any(|f| matches!(f, SessionFrame::Ops { envelope, .. } if envelope.ops.is_empty()))).await,
        "an empty Ops keepalive reaches the leaf"
    );
    // Nobody polls: the endpoint closes the session and the leaf frees the screen.
    assert!(until(|| h.leaf.svc.lock().unwrap().active().is_none()).await, "the abandoned session is reaped");
    assert_eq!(h.ep.poll(s, 8).await.unwrap_err(), EndpointError::NoSession);
}
