//! `weftos-cos bridge` against a fake dashboard and a fake agent.

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use async_trait::async_trait;
use weftos_cos::bridge::{Bridge, BridgeConfig, SyncError, Transcriber};

const TOKEN: &str = "wft_0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

#[derive(Default)]
struct Dash {
    bodies: Vec<Value>,
    auth: Vec<String>,
    /// Requests handed out on the next sync.
    queue: Vec<Value>,
    status: u16,
    /// Bytes served at `/api/nodes/cos/audio/{id}`, and what the poll saw.
    audio: Vec<u8>,
    audio_auth: Vec<String>,
    polls: Vec<String>,
}

#[derive(Default)]
struct Agent {
    turns: Vec<Value>,
    utterances: Vec<Value>,
    /// Answer utterances with 500.
    broken: bool,
}

async fn spawn(app: Router) -> SocketAddr {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    addr
}

async fn fake_dashboard() -> (SocketAddr, Arc<Mutex<Dash>>) {
    let st = Arc::new(Mutex::new(Dash { status: 200, ..Default::default() }));
    async fn sync(State(st): State<Arc<Mutex<Dash>>>, h: HeaderMap, Json(b): Json<Value>) -> (StatusCode, Json<Value>) {
        let mut d = st.lock().unwrap();
        d.auth.push(h.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("").to_owned());
        d.bodies.push(b);
        let code = StatusCode::from_u16(d.status).unwrap();
        let reqs = std::mem::take(&mut d.queue);
        (code, Json(json!({"requests": reqs})))
    }
    async fn pending(State(st): State<Arc<Mutex<Dash>>>, h: HeaderMap, axum::extract::RawQuery(q): axum::extract::RawQuery) -> Json<Value> {
        let mut d = st.lock().unwrap();
        d.polls.push(format!("{}|{}", q.unwrap_or_default(), h.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("")));
        let reqs = std::mem::take(&mut d.queue);
        Json(json!({"requests": reqs}))
    }
    async fn audio(State(st): State<Arc<Mutex<Dash>>>, h: HeaderMap) -> Vec<u8> {
        let mut d = st.lock().unwrap();
        d.audio_auth.push(h.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("").to_owned());
        d.audio.clone()
    }
    let app = Router::new()
        .route("/api/nodes/cos/sync", post(sync))
        .route("/api/nodes/cos/pending", get(pending))
        .route("/api/nodes/cos/audio/{id}", get(audio))
        .with_state(st.clone());
    (spawn(app).await, st)
}

async fn fake_agent() -> (SocketAddr, Arc<Mutex<Agent>>) {
    let st = Arc::new(Mutex::new(Agent::default()));
    async fn turns(State(st): State<Arc<Mutex<Agent>>>) -> Json<Value> {
        Json(json!({"turns": st.lock().unwrap().turns.clone()}))
    }
    async fn utter(State(st): State<Arc<Mutex<Agent>>>, Json(b): Json<Value>) -> (StatusCode, Json<Value>) {
        let mut a = st.lock().unwrap();
        a.utterances.push(b);
        if a.broken {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "model: boom"})));
        }
        (StatusCode::ACCEPTED, Json(json!({"turn_id": format!("t{}", a.utterances.len())})))
    }
    let app = Router::new()
        .route("/v1/cos/turns", get(turns))
        .route("/v1/cos/utterance", post(utter))
        .route("/v1/cos/status", get(|| async { Json(json!({"state": "idle", "task": "waiting", "model": "m", "lanes_running": 0})) }))
        .route("/healthz", get(|| async { Json(json!({"ok": true, "api": "cos.v1"})) }))
        .with_state(st.clone());
    (spawn(app).await, st)
}

struct Rig {
    cfg: BridgeConfig,
    bridge: Bridge,
    dash: Arc<Mutex<Dash>>,
    agent: Arc<Mutex<Agent>>,
    dir: tempfile::TempDir,
}

async fn rig() -> Rig {
    let (d, dash) = fake_dashboard().await;
    let (a, agent) = fake_agent().await;
    let dir = tempfile::tempdir().unwrap();
    let token_file = dir.path().join("node.token");
    std::fs::write(&token_file, format!("{TOKEN}\n")).unwrap();
    std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    let mentra_dir = dir.path().join("mentra");
    std::fs::create_dir(&mentra_dir).unwrap();
    let cfg = BridgeConfig {
        dashboard_url: format!("http://{d}"),
        token_file,
        agent: format!("http://{a}"),
        mentra_dir,
    };
    Rig { bridge: Bridge::new(cfg.clone()), cfg, dash, agent, dir }
}

fn turn(id: &str, role: &str, text: &str) -> Value {
    json!({"turn_id": id, "role": role, "endpoint": "mentra", "text": text, "ts": "2026-10-13T00:00:00.000Z"})
}

#[tokio::test]
async fn full_cycle_pushes_turns_delivers_requests_and_reports_results() {
    let r = rig().await;
    r.agent.lock().unwrap().turns = vec![turn("a", "user", "hi"), turn("a", "assistant", "hello")];
    r.dash.lock().unwrap().queue = vec![json!({"id": "r1", "endpoint": "dashboard", "text": " what is next? ", "requested_by": "u", "created_at": "x"})];

    let rep = r.bridge.sync_once(false).await.unwrap();
    assert_eq!((rep.turns_sent, rep.requests), (2, 1));
    {
        let d = r.dash.lock().unwrap();
        assert_eq!(d.auth[0], format!("Bearer {TOKEN}"));
        assert_eq!(d.bodies[0]["turns"].as_array().unwrap().len(), 2);
        assert_eq!(d.bodies[0]["status"]["agent"]["state"], "idle");
        assert_eq!(d.bodies[0]["status"]["agent"]["healthy"], true);
        assert_eq!(d.bodies[0]["results"].as_array().unwrap().len(), 0);
    }
    // The request reached the agent as a typed utterance for its endpoint.
    assert_eq!(r.agent.lock().unwrap().utterances, vec![json!({"text": "what is next?", "source": "typed", "endpoint": "dashboard"})]);

    // The next sync reports it, once.
    r.bridge.sync_once(false).await.unwrap();
    r.bridge.sync_once(false).await.unwrap();
    let d = r.dash.lock().unwrap();
    assert_eq!(d.bodies[1]["results"], json!([{"id": "r1", "status": "accepted", "turn_id": "t1"}]));
    assert_eq!(d.bodies[2]["results"], json!([]));
}

#[tokio::test]
async fn failed_requests_are_reported_with_the_error() {
    let r = rig().await;
    r.agent.lock().unwrap().broken = true;
    r.dash.lock().unwrap().queue = vec![
        json!({"id": "r1", "endpoint": "mentra", "text": "hello"}),
        json!({"id": "r2", "endpoint": "phone", "text": "hello"}),
        json!({"id": "r3", "endpoint": "mentra", "text": "   "}),
    ];
    r.bridge.sync_once(false).await.unwrap();
    r.bridge.sync_once(false).await.unwrap();
    let d = r.dash.lock().unwrap();
    let res = d.bodies[1]["results"].as_array().unwrap();
    assert_eq!(res.len(), 3);
    assert!(res.iter().all(|x| x["status"] == "failed"));
    assert_eq!(res[0]["error"], "model: boom");
    // Only the valid request reached the agent.
    assert_eq!(r.agent.lock().unwrap().utterances.len(), 1);
}

#[tokio::test]
async fn the_turn_cursor_sends_each_turn_once_and_at_most_fifty_per_sync() {
    let r = rig().await;
    r.agent.lock().unwrap().turns = (0..60).map(|i| turn(&format!("t{i:02}"), "assistant", "x")).collect();
    assert_eq!(r.bridge.sync_once(false).await.unwrap().turns_sent, 50);
    assert_eq!(r.bridge.sync_once(false).await.unwrap().turns_sent, 10);
    assert_eq!(r.bridge.sync_once(false).await.unwrap().turns_sent, 0);
    r.agent.lock().unwrap().turns.push(turn("t60", "user", "new"));
    assert_eq!(r.bridge.sync_once(false).await.unwrap().turns_sent, 1);

    let d = r.dash.lock().unwrap();
    let mut ids: Vec<String> = d.bodies.iter().flat_map(|b| b["turns"].as_array().unwrap().clone()).map(|t| t["turn_id"].as_str().unwrap().to_owned()).collect();
    assert_eq!(ids.len(), 61);
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 61);
}

#[tokio::test]
async fn a_turn_not_delivered_is_resent() {
    let r = rig().await;
    r.agent.lock().unwrap().turns = vec![turn("a", "user", "hi")];
    r.dash.lock().unwrap().status = 500;
    assert!(matches!(r.bridge.sync_once(false).await, Err(SyncError::Failed(_))));
    r.dash.lock().unwrap().status = 200;
    assert_eq!(r.bridge.sync_once(false).await.unwrap().turns_sent, 1);
}

#[tokio::test]
async fn status_files_are_null_when_missing_and_an_error_when_corrupt() {
    let r = rig().await;
    r.bridge.sync_once(false).await.unwrap();
    std::fs::write(r.dir.path().join("mentra/listen.status.json"), "{ not json").unwrap();
    std::fs::write(r.dir.path().join("mentra/speak.status.json"), r#"{"up": true, "pid": 1}"#).unwrap();
    r.bridge.sync_once(false).await.unwrap();
    let d = r.dash.lock().unwrap();
    assert_eq!(d.bodies[0]["status"]["mentra"], json!({"listen": null, "speak": null}));
    let m = &d.bodies[1]["status"]["mentra"];
    assert!(m["listen"]["error"].as_str().unwrap().contains("listen.status.json"));
    assert_eq!(m["speak"], json!({"up": true, "pid": 1}));
}

#[tokio::test]
async fn a_401_is_a_rejection_and_keeps_pending_results() {
    let r = rig().await;
    r.dash.lock().unwrap().queue = vec![json!({"id": "r1", "endpoint": "mentra", "text": "hello"})];
    r.bridge.sync_once(false).await.unwrap();
    r.dash.lock().unwrap().status = 401;
    let err = r.bridge.sync_once(false).await.unwrap_err();
    assert_eq!(err, SyncError::Rejected("node token rejected (HTTP 401)".into()));
    assert!(!err.to_string().contains(TOKEN));
    r.dash.lock().unwrap().status = 200;
    r.bridge.sync_once(false).await.unwrap();
    let d = r.dash.lock().unwrap();
    assert_eq!(d.bodies.last().unwrap()["results"][0]["id"], "r1");
}

#[tokio::test]
async fn dry_run_posts_nothing_truncates_text_and_prints_no_token() {
    let r = rig().await;
    r.agent.lock().unwrap().turns = vec![turn("a", "user", &"y".repeat(500))];
    let rep = r.bridge.sync_once(true).await.unwrap();
    assert!(r.dash.lock().unwrap().bodies.is_empty());
    assert!(r.agent.lock().unwrap().utterances.is_empty());
    let text = rep.body["turns"][0]["text"].as_str().unwrap();
    assert!(text.chars().count() <= 81);
    assert!(!rep.body.to_string().contains(TOKEN));
    // Dry run does not advance the cursor.
    assert_eq!(r.bridge.sync_once(false).await.unwrap().turns_sent, 1);
}

#[tokio::test]
async fn a_loose_token_file_is_refused_without_contacting_the_dashboard() {
    let r = rig().await;
    std::fs::set_permissions(r.dir.path().join("node.token"), std::fs::Permissions::from_mode(0o644)).unwrap();
    let err = r.bridge.sync_once(false).await.unwrap_err();
    assert!(matches!(&err, SyncError::Failed(m) if m.contains("mode 644")));
    assert!(r.dash.lock().unwrap().bodies.is_empty());
}

#[test]
fn config_comes_from_the_dashboard_section_and_refuses_bad_urls() {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".weftos")).unwrap();
    let write = |s: &str| std::fs::write(home.path().join(".weftos/weave.toml"), s).unwrap();

    write("[dashboard]\nurl = \"https://dash.example.test/\"\ntoken_file = \"/secret/node.token\"\n");
    let c = weftos_cos::bridge::load_config(home.path(), "http://127.0.0.1:18300/").unwrap();
    assert_eq!(c.dashboard_url, "https://dash.example.test");
    assert_eq!(c.agent, "http://127.0.0.1:18300");

    write("[dashboard]\nurl = \"http://dash.example.test\"\ntoken_file = \"/secret/node.token\"\n");
    assert!(weftos_cos::bridge::load_config(home.path(), "http://127.0.0.1:18300").unwrap_err().contains("loopback"));
    write("[dashboard]\nurl = \"https://dash.example.test\"\ntoken_file = \"rel.token\"\n");
    assert!(weftos_cos::bridge::load_config(home.path(), "http://127.0.0.1:18300").unwrap_err().contains("absolute"));
    write("[other]\n");
    assert!(weftos_cos::bridge::load_config(home.path(), "http://127.0.0.1:18300").unwrap_err().contains("no [dashboard]"));
}

fn wav(rate: u32, channels: u16, secs: f32) -> Vec<u8> {
    let spec = hound::WavSpec { channels, sample_rate: rate, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut w = hound::WavWriter::new(&mut buf, spec).unwrap();
        for i in 0..((rate as f32 * secs) as usize * channels as usize) {
            w.write_sample((i % 100) as i16).unwrap();
        }
        w.finalize().unwrap();
    }
    buf.into_inner()
}

struct Stub(&'static str, Mutex<Vec<usize>>);

#[async_trait]
impl Transcriber for Stub {
    async fn transcribe(&self, samples: Vec<i16>) -> Result<String, String> {
        self.1.lock().unwrap().push(samples.len());
        Ok(self.0.to_owned())
    }
}

fn audio_req() -> Value {
    json!({"id": "a1", "kind": "audio", "endpoint": "dashboard", "text": "", "payload": {"hold_id": "h1", "bytes": 1, "ms": 1}})
}

async fn result_of(r: &Rig, req: Value) -> Value {
    r.dash.lock().unwrap().queue = vec![req];
    r.bridge.sync_once(false).await.unwrap();
    r.bridge.sync_once(false).await.unwrap();
    let d = r.dash.lock().unwrap();
    d.bodies.last().unwrap()["results"][0].clone()
}

#[tokio::test]
async fn the_long_poll_delivers_requests_and_wakes_the_sync_loop() {
    let r = rig().await;
    r.dash.lock().unwrap().queue = vec![json!({"id": "r1", "kind": "utterance", "endpoint": "mentra", "text": "hello"})];
    assert_eq!(r.bridge.poll_once().await.unwrap(), 1);
    assert_eq!(r.dash.lock().unwrap().polls, vec![format!("wait=25|Bearer {TOKEN}")]);
    assert_eq!(r.agent.lock().unwrap().utterances.len(), 1);
    // The wake permit is stored, so this returns at once.
    tokio::time::timeout(std::time::Duration::from_secs(2), r.bridge.wake.notified()).await.unwrap();
    r.bridge.sync_once(false).await.unwrap();
    assert_eq!(r.dash.lock().unwrap().bodies[0]["results"][0]["status"], "accepted");
    // An empty poll runs nothing and does not wake.
    assert_eq!(r.bridge.poll_once().await.unwrap(), 0);

    r.dash.lock().unwrap().polls.clear();
    std::fs::set_permissions(r.dir.path().join("node.token"), std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(r.bridge.poll_once().await.is_err());
    assert!(r.dash.lock().unwrap().polls.is_empty());
}

#[tokio::test]
async fn control_requests_merge_into_an_atomically_written_control_file() {
    let r = rig().await;
    let path = r.dir.path().join("mentra/control.json");
    let read = || serde_json::from_str::<Value>(&std::fs::read_to_string(&path).unwrap()).unwrap();

    let res = result_of(&r, json!({"id": "c1", "kind": "listen", "payload": {"on": false}})).await;
    assert_eq!(res, json!({"id": "c1", "status": "accepted"}));
    assert_eq!(read()["listening"], false);
    assert!(read().get("ptt").is_none());

    result_of(&r, json!({"id": "c2", "kind": "ptt_mentra", "payload": {"hold_id": "h-1", "phase": "down"}})).await;
    let c = read();
    assert_eq!((c["listening"].clone(), c["ptt"]["hold_id"].clone(), c["ptt"]["phase"].clone()), (json!(false), json!("h-1"), json!("down")));
    assert!(c["ptt"]["at"].is_string() && c["updated_at"].is_string());

    // Toggling listening keeps the last ptt; releasing keeps listening.
    result_of(&r, json!({"id": "c3", "kind": "listen", "payload": {"on": true}})).await;
    assert_eq!((read()["listening"].clone(), read()["ptt"]["phase"].clone()), (json!(true), json!("down")));
    result_of(&r, json!({"id": "c4", "kind": "ptt_mentra", "payload": {"hold_id": "h-1", "phase": "up"}})).await;
    assert_eq!((read()["listening"].clone(), read()["ptt"]["phase"].clone()), (json!(true), json!("up")));

    // Bad payloads are refused and leave the file alone.
    let before = read();
    for (i, p) in [("c5", json!({"on": "yes"})), ("c6", json!(null))] {
        assert_eq!(result_of(&r, json!({"id": i, "kind": "listen", "payload": p})).await["status"], "failed");
    }
    let bad = result_of(&r, json!({"id": "c7", "kind": "ptt_mentra", "payload": {"hold_id": "h", "phase": "sideways"}})).await;
    assert_eq!(bad["status"], "failed");
    assert_eq!(read(), before);

    // No temp files are left behind.
    let names: Vec<_> = std::fs::read_dir(r.dir.path().join("mentra")).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(names, vec!["control.json"]);
}

#[tokio::test]
async fn a_failed_control_write_is_reported_failed() {
    let r = rig().await;
    // A directory where the file belongs makes the rename fail.
    std::fs::create_dir(r.dir.path().join("mentra/control.json")).unwrap();
    let res = result_of(&r, json!({"id": "c1", "kind": "listen", "payload": {"on": true}})).await;
    assert_eq!(res["status"], "failed");
    assert!(res["error"].as_str().unwrap().contains("control file"));
}

#[tokio::test]
async fn an_audio_request_is_downloaded_transcribed_and_posted_as_a_voice_turn() {
    let mut r = rig().await;
    let stub = Arc::new(Stub("what is on today", Mutex::new(vec![])));
    r.bridge = Bridge::new(r.cfg.clone()).with_transcriber(Some(stub.clone()));
    r.dash.lock().unwrap().audio = wav(16_000, 1, 2.0);

    let res = result_of(&r, audio_req()).await;
    assert_eq!(res, json!({"id": "a1", "status": "accepted", "turn_id": "t1", "text": "what is on today"}));
    assert_eq!(stub.1.lock().unwrap().as_slice(), [32_000]);
    assert_eq!(r.dash.lock().unwrap().audio_auth, vec![format!("Bearer {TOKEN}")]);
    assert_eq!(r.agent.lock().unwrap().utterances, vec![json!({"text": "what is on today", "source": "voice", "endpoint": "dashboard"})]);
}

#[tokio::test]
async fn bad_audio_is_refused_and_never_reaches_the_transcriber_or_the_agent() {
    let mut r = rig().await;
    let stub = Arc::new(Stub("x", Mutex::new(vec![])));
    r.bridge = Bridge::new(r.cfg.clone()).with_transcriber(Some(stub.clone()));

    let cases: [(&str, Vec<u8>, &str); 4] = [
        ("too long", wav(16_000, 1, 15.05), "longer than 15 s"),
        ("wrong rate", wav(44_100, 1, 1.0), "16 kHz mono PCM16"),
        ("stereo", wav(16_000, 2, 1.0), "16 kHz mono PCM16"),
        ("garbage", b"not a wav at all".to_vec(), "not a WAV"),
    ];
    for (name, bytes, want) in cases {
        r.dash.lock().unwrap().audio = bytes;
        let res = result_of(&r, audio_req()).await;
        assert_eq!(res["status"], "failed", "{name}");
        assert!(res["error"].as_str().unwrap().contains(want), "{name}: {res}");
    }
    // A download past the size cap is cut off before decoding.
    r.dash.lock().unwrap().audio = vec![0u8; 700_000];
    let res = result_of(&r, audio_req()).await;
    assert!(res["error"].as_str().unwrap().contains("larger than"), "{res}");

    assert!(stub.1.lock().unwrap().is_empty());
    assert!(r.agent.lock().unwrap().utterances.is_empty());
}

#[tokio::test]
async fn silence_is_reported_as_no_speech_and_a_build_without_stt_refuses() {
    let mut r = rig().await;
    r.dash.lock().unwrap().audio = wav(16_000, 1, 1.0);

    // No transcriber wired in.
    let res = result_of(&r, audio_req()).await;
    assert_eq!(res, json!({"id": "a1", "status": "failed", "error": "this bridge build has no STT"}));

    r.bridge = Bridge::new(r.cfg.clone()).with_transcriber(Some(Arc::new(Stub("  ", Mutex::new(vec![])))));
    let res = result_of(&r, audio_req()).await;
    assert_eq!(res, json!({"id": "a1", "status": "failed", "error": "no speech"}));
    assert!(r.agent.lock().unwrap().utterances.is_empty());
}

#[tokio::test]
async fn an_unknown_kind_is_reported_failed() {
    let r = rig().await;
    let res = result_of(&r, json!({"id": "k1", "kind": "teleport"})).await;
    assert_eq!(res["status"], "failed");
}
