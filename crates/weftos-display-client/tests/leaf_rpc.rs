//! The real-leaf backend against a stand-in daemon on a Unix socket:
//! the RPC it speaks (`display.open|ops|poll|close`), how it turns replies
//! into events and errors, and that it refuses rather than hangs.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use weftos_display_client::scene::{codec, session::b64, InputEnvelope, InputEvent, NodeId, SceneOp};
use weftos_display_client::{
    DisplayError, DisplayGrant, DisplaySession, DisplayTarget, LeafEndpoint, ObjectEvent, SensorValue,
};

/// What the stand-in daemon saw and what it will answer to `display.poll`.
#[derive(Default)]
struct Daemon {
    calls: Vec<(String, Value)>,
    polls: Vec<Value>,
    open_error: Option<(String, String)>,
}

fn serve(state: Arc<Mutex<Daemon>>) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kernel.sock");
    let listener = UnixListener::bind(&path).unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let state = Arc::clone(&state);
            std::thread::spawn(move || {
                let mut out = stream.try_clone().unwrap();
                for line in BufReader::new(stream).lines().map_while(Result::ok) {
                    let req: Value = serde_json::from_str(&line).unwrap();
                    let method = req["method"].as_str().unwrap().to_owned();
                    let mut st = state.lock().unwrap();
                    st.calls.push((method.clone(), req["params"].clone()));
                    // The wire protocol the daemon requires on every call.
                    assert_eq!(req["proto"], 1);
                    assert_eq!(req["auth"], "admin");
                    let reply = match method.as_str() {
                        "display.open" => match st.open_error.clone() {
                            Some((kind, msg)) => json!({"ok": false, "error_kind": kind, "error": msg}),
                            None => json!({"ok": true, "result": {
                                "session": 9, "leaf_id": "x", "width": 800, "height": 480,
                                "render_caps": 0, "touch": true, "ptt": false,
                                "max_nodes": 4096, "max_ops_per_sec": 1000, "image_budget_bytes": 1 << 20,
                            }}),
                        },
                        "display.poll" => {
                            let p = if st.polls.is_empty() { json!({"events": []}) } else { st.polls.remove(0) };
                            json!({"ok": true, "result": p})
                        }
                        _ => json!({"ok": true, "result": {"ok": true}}),
                    };
                    drop(st);
                    writeln!(out, "{reply}").unwrap();
                }
            });
        }
    });
    (dir, path)
}

fn target(path: &std::path::Path) -> DisplayTarget {
    DisplayTarget::Leaf(LeafEndpoint {
        name: "weftos://m/nodes/x/services/display".into(),
        leaf_id: "x".into(),
        display: 0,
        daemon: Some(path.to_path_buf()),
    })
}

fn input(kind: &str, node: Option<u32>) -> Value {
    let event = match kind {
        "down" => InputEvent::PointerDown { pointer_id: 0, x: 256, y: 256, pressure_q8: 0xFF00 },
        _ => InputEvent::PointerUp { pointer_id: 0, x: 256, y: 256 },
    };
    let env = InputEnvelope::new(0, node.map(NodeId::from_raw), event);
    json!({"kind": "input", "cbor_b64": b64::encode(&codec::encode(&env).unwrap())})
}

fn wait_event(s: &mut DisplaySession) -> Option<ObjectEvent> {
    let end = Instant::now() + Duration::from_secs(5);
    while Instant::now() < end {
        if let Some(e) = s.poll_event() {
            return Some(e);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    None
}

#[test]
fn open_sends_the_grant_and_reports_the_leaf() {
    let st = Arc::new(Mutex::new(Daemon::default()));
    let (_dir, path) = serve(Arc::clone(&st));
    let grant = DisplayGrant { subject: "cos-console".into(), resource: String::new(), proof: vec![0xAB, 0x01] };
    let s = DisplaySession::open(target(&path), grant).unwrap();
    assert_eq!((s.size().width, s.size().height), (800, 480));
    assert!(s.capabilities().input.touch);
    assert!(s.capabilities().input.buttons.is_empty(), "this leaf reported no button");
    let calls = st.lock().unwrap().calls.clone();
    assert_eq!(calls[0].0, "display.open");
    assert_eq!(calls[0].1["subject"], "cos-console");
    assert_eq!(calls[0].1["leaf_id"], "x");
    assert_eq!(calls[0].1["proof_hex"], "ab01");
    assert_eq!(calls[0].1["resource"], "weftos://m/nodes/x/services/display");
}

#[test]
fn ops_go_out_as_one_cbor_envelope_per_apply() {
    let st = Arc::new(Mutex::new(Daemon::default()));
    let (_dir, path) = serve(Arc::clone(&st));
    let mut s = DisplaySession::open(target(&path), DisplayGrant::dev("a")).unwrap();
    s.apply(&[SceneOp::Remove(NodeId::from_raw(1)), SceneOp::Remove(NodeId::from_raw(2))]).unwrap();
    let calls = st.lock().unwrap().calls.clone();
    let (_, params) = calls.iter().find(|(m, _)| m == "display.ops").expect("an ops call");
    assert_eq!(params["session"], 9);
    let env = codec::decode_scene_envelope(&b64::decode(params["cbor_b64"].as_str().unwrap().as_bytes()).unwrap()).unwrap();
    assert_eq!(env.ops.len(), 2);
}

#[test]
fn events_decode_into_taps_and_sensors() {
    let st = Arc::new(Mutex::new(Daemon::default()));
    let (_dir, path) = serve(Arc::clone(&st));
    let mut s = DisplaySession::open(target(&path), DisplayGrant::dev("a")).unwrap();
    st.lock().unwrap().polls.push(json!({"events": [
        input("down", Some(5)),
        input("up", Some(5)),
        {"kind": "sensor", "name": "ptt.down", "pressed": true, "value": null},
        {"kind": "sensor", "name": "light", "pressed": null, "value": 12.5},
        {"kind": "unknown-future-kind"},
    ]}));
    assert_eq!(wait_event(&mut s), Some(ObjectEvent::Tap { node: NodeId::from_raw(5) }));
    assert_eq!(wait_event(&mut s), Some(ObjectEvent::ptt(true)));
    assert_eq!(
        wait_event(&mut s),
        Some(ObjectEvent::Sensor { name: "light".into(), value: SensorValue::Float(12.5) })
    );
}

#[test]
fn polling_is_rate_limited_not_one_call_per_frame() {
    let st = Arc::new(Mutex::new(Daemon::default()));
    let (_dir, path) = serve(Arc::clone(&st));
    let mut s = DisplaySession::open(target(&path), DisplayGrant::dev("a")).unwrap();
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(300) {
        let _ = s.poll_event();
    }
    let polls = st.lock().unwrap().calls.iter().filter(|(m, _)| m == "display.poll").count();
    assert!((5..=14).contains(&polls), "{polls} polls in 300 ms (about one per 25 ms)");
}

#[test]
fn a_rejected_batch_is_reported_on_the_next_apply() {
    let st = Arc::new(Mutex::new(Daemon::default()));
    let (_dir, path) = serve(Arc::clone(&st));
    let mut s = DisplaySession::open(target(&path), DisplayGrant::dev("a")).unwrap();
    st.lock().unwrap().polls.push(json!({"events": [], "rejected": ["batch 3 refused: node budget exceeded"]}));
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        let _ = s.poll_event();
        match s.apply(&[SceneOp::Clear]) {
            Err(DisplayError::Backend(why)) => {
                assert!(why.contains("node budget exceeded"), "{why}");
                break;
            }
            Ok(()) if Instant::now() < end => std::thread::sleep(Duration::from_millis(5)),
            other => panic!("expected the leaf's refusal, got {other:?}"),
        }
    }
    s.apply(&[SceneOp::Clear]).expect("reported once, then the session carries on");
}

#[test]
fn a_session_the_leaf_ended_closes_the_client() {
    let st = Arc::new(Mutex::new(Daemon::default()));
    let (_dir, path) = serve(Arc::clone(&st));
    let mut s = DisplaySession::open(target(&path), DisplayGrant::dev("a")).unwrap();
    st.lock().unwrap().polls.push(json!({"events": [], "ended": "link to the host dropped"}));
    let end = Instant::now() + Duration::from_secs(5);
    while !s.is_closed() && Instant::now() < end {
        let _ = s.poll_event();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(s.is_closed());
    assert_eq!(s.apply(&[SceneOp::Clear]), Err(DisplayError::Closed));
}

#[test]
fn daemon_refusals_keep_their_kind() {
    let st = Arc::new(Mutex::new(Daemon::default()));
    let (_dir, path) = serve(Arc::clone(&st));
    st.lock().unwrap().open_error = Some(("display_denied".into(), "display grant denied: grant for a was revoked".into()));
    assert!(matches!(DisplaySession::open(target(&path), DisplayGrant::dev("a")), Err(DisplayError::Denied(w)) if w.contains("revoked")));
    st.lock().unwrap().open_error = Some(("display_refused".into(), "the leaf refused the session: display busy: held by b".into()));
    assert!(matches!(DisplaySession::open(target(&path), DisplayGrant::dev("a")), Err(DisplayError::Backend(w)) if w.contains("display busy")));
}

#[test]
fn close_tells_the_daemon_once() {
    let st = Arc::new(Mutex::new(Daemon::default()));
    let (_dir, path) = serve(Arc::clone(&st));
    let mut s = DisplaySession::open(target(&path), DisplayGrant::dev("a")).unwrap();
    s.close().unwrap();
    s.close().unwrap();
    drop(s);
    let closes = st.lock().unwrap().calls.iter().filter(|(m, _)| m == "display.close").count();
    assert_eq!(closes, 1);
}
