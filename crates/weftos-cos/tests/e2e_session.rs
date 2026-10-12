//! A recorded end-to-end session. Scripted utterances go in through the HTTP
//! API (one of them as a held push-to-talk button with a fake STT), the
//! routing is the real WeftOS routing from an app config (the `local`
//! provider pointed at a stub OpenAI-compatible server), and the tools talk
//! to a fake dashboard harness, a static node source and a real RVF brain.
//! The one SSE stream of the whole session is normalised and compared with
//! `tests/fixtures/e2e-session.jsonl`; set `COS_UPDATE_GOLDEN=1` to rewrite
//! it after an intended change.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::post;
use clawft_channels::voice::stt::{SttBackend, SttModel, Utterance};
use clawft_channels::voice::types::VoiceError;
use common::{FakeHarness, TEST_PROJECT, TEST_TOKEN, harness_client};
use serde_json::{Value, json};
use weftos_cos::agent::CosAgent;
use weftos_cos::brain::Brain;
use weftos_cos::config::{BrainConfig, ModelsConfig};
use weftos_cos::conversation::Conversation;
use weftos_cos::routing::ModelRoutes;
use weftos_cos::tools::board::BoardPropose;
use weftos_cos::tools::cards::{BoardClaim, BoardNote, BoardRead, CardCurrent, Focus};
use weftos_cos::tools::harness::{NotesRecord, ProjectsActivity};
use weftos_cos::tools::nodes::{NodesHealth, StaticHealth};
use weftos_cos::tools::{ToolLog, ToolRegistry};
use weftos_cos::voice::{PttControl, PttEdge, PttOutcome, PushToTalk};

const DIGEST: &str = "sha256:537fbd002578137017a15183931acc31d8e8800500ec4c6fcb72d472bb5942d6";
const GOLDEN: &str = "tests/fixtures/e2e-session.jsonl";

/// One scripted model response.
enum Step {
    Text(&'static str),
    Call(&'static str, Value),
}

/// A stub OpenAI-compatible server that replays `steps` in order and keeps
/// every request body it was sent.
#[derive(Clone)]
struct Stub {
    steps: Arc<Mutex<std::collections::VecDeque<Step>>>,
    seen: Arc<Mutex<Vec<Value>>>,
}

async fn completions(State(s): State<Stub>, axum::Json(body): axum::Json<Value>) -> impl IntoResponse {
    s.seen.lock().unwrap().push(body);
    let step = s.steps.lock().unwrap().pop_front().expect("the model was asked more often than the script allows");
    let chunk = |delta: Value, finish: Value| format!("data: {}\n\n", json!({"id": "c", "object": "chat.completion.chunk", "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}));
    let mut out = String::new();
    match step {
        Step::Text(t) => {
            for w in t.split_inclusive(' ') {
                out.push_str(&chunk(json!({"role": "assistant", "content": w}), Value::Null));
            }
            out.push_str(&chunk(json!({}), json!("stop")));
        }
        Step::Call(name, args) => {
            out.push_str(&chunk(
                json!({"role": "assistant", "tool_calls": [{"index": 0, "id": "call_1", "type": "function", "function": {"name": name, "arguments": args.to_string()}}]}),
                Value::Null,
            ));
            out.push_str(&chunk(json!({}), json!("tool_calls")));
        }
    }
    out.push_str("data: [DONE]\n\n");
    ([("content-type", "text/event-stream")], out)
}

async fn start_stub(steps: Vec<Step>) -> (std::net::SocketAddr, Stub) {
    let stub = Stub { steps: Arc::new(Mutex::new(steps.into())), seen: Arc::default() };
    let app = Router::new().route("/v1/chat/completions", post(completions)).with_state(stub.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (addr, stub)
}

/// Fake STT: always hears the same words.
struct HeardWords(&'static str);

#[async_trait]
impl SttBackend for HeardWords {
    async fn warm(&self) -> Result<(), VoiceError> {
        Ok(())
    }
    async fn transcribe(&self, _: &Utterance) -> Result<String, VoiceError> {
        Ok(self.0.to_string())
    }
    fn model(&self) -> SttModel {
        SttModel::ParakeetEnglish
    }
}

/// The button as a test sees it: `up` runs one second of tone through the
/// real transcribe-and-post path with the fake STT.
struct FakeMic {
    ptt: PushToTalk,
    active: std::sync::atomic::AtomicBool,
}

#[async_trait]
impl PttControl for FakeMic {
    async fn on_edge(&self, edge: PttEdge) -> Result<Option<PttOutcome>, weftos_cos::CosError> {
        match edge {
            PttEdge::Down => {
                self.active.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(None)
            }
            PttEdge::Up => {
                self.active.store(false, std::sync::atomic::Ordering::SeqCst);
                self.ptt.transcribe_and_post(vec![1000; 16_000]).await.map(Some)
            }
        }
    }
    fn is_active(&self) -> bool {
        self.active.load(std::sync::atomic::Ordering::SeqCst)
    }
}

fn script() -> Vec<Step> {
    vec![
        // 1 (voice) where are we
        Step::Call("projects_activity", json!({"status": "in_progress"})),
        Step::Text("Two cards are in progress: WEFT-901, the console screen, and WEFT-902, the pad button. The pad button is next."),
        // 2 claim WEFT-902 -> held for a yes, no model call for the yes
        Step::Call("board_claim", json!({"ticket": "WEFT-902"})),
        // 3 a decision to keep
        Step::Call("notes_record", json!({"text": "Pad button debounce is 30 ms", "kind": "decision", "why": "the GPIO bounces for about 20 ms", "where": ["WEFT-902"], "ticket": "WEFT-902"})),
        Step::Text("Noted on WEFT-902 and in the brain."),
        // 4 mesh
        Step::Call("nodes_health", json!({})),
        Step::Text("Two of three nodes are healthy."),
        // 5 plan today: one survey question, then (after the tap) a checklist
        Step::Call("ui_ask", json!({"question": "What first?", "choices": ["Pad button", "Glasses bridge"], "index": 1, "total": 1})),
        Step::Call("ui_checklist", json!({"title": "Today", "items": ["Pad button wiring", "Contract review"]})),
        Step::Text("Here is today's list."),
    ]
}

async fn say(c: &reqwest::Client, base: &str, text: &str, source: &str) -> String {
    let r = c.post(format!("{base}/v1/cos/utterance")).json(&json!({"text": text, "source": source})).send().await.unwrap();
    assert_eq!(r.status(), 202);
    r.json::<Value>().await.unwrap()["turn_id"].as_str().unwrap().to_string()
}

/// Wait until `n` turns have gone idle on the recorded stream.
async fn until_idle(log: &Arc<Mutex<String>>, n: usize) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while log.lock().unwrap().matches("\"state\":\"idle\"").count() < n {
        assert!(tokio::time::Instant::now() < deadline, "turn {n} never went idle:\n{}", log.lock().unwrap());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Turn ids become t1, t2, ...; runs of `token` events become one.
fn normalise(raw: &str) -> Vec<Value> {
    let mut ids: Vec<String> = Vec::new();
    let mut out: Vec<Value> = Vec::new();
    for line in raw.lines().filter_map(|l| l.strip_prefix("data: ")) {
        let mut v: Value = serde_json::from_str(line).unwrap();
        if let Some(id) = v["turn_id"].as_str().map(str::to_string) {
            let n = ids.iter().position(|i| *i == id).unwrap_or_else(|| {
                ids.push(id);
                ids.len() - 1
            });
            v["turn_id"] = json!(format!("t{}", n + 1));
        }
        if v["kind"] == "token"
            && let Some(last) = out.last_mut()
            && last["kind"] == "token"
            && last["turn_id"] == v["turn_id"]
        {
            let joined = format!("{}{}", last["text"].as_str().unwrap(), v["text"].as_str().unwrap());
            last["text"] = json!(joined);
            continue;
        }
        out.push(v);
    }
    out
}

#[tokio::test]
async fn a_scripted_session_runs_the_whole_loop() {
    let dir = tempfile::tempdir().unwrap();
    let (harness, posts) = FakeHarness { with_events_route: true, ..Default::default() }.start().await;
    let (stub_addr, stub) = start_stub(script()).await;

    // Real routing: the app config points the `local` provider at the stub.
    let app_cfg_path = dir.path().join("app.json");
    std::fs::write(&app_cfg_path, json!({"providers": {"local": {"apiBase": format!("http://{stub_addr}/v1")}}}).to_string()).unwrap();
    let app = weftos_cos::host::load_app_config(&weftos_cos::CosConfig { app_config: Some(app_cfg_path), ..Default::default() }).await.unwrap();
    // No cloud in the chain, and a planning cue stays on the stub.
    let local = weftos_cos::model::LOCAL_DEFAULT_MODEL.to_string();
    let models = ModelsConfig { plan: Some(local.clone()), fallback: Some(vec![]), ..Default::default() };
    let routes = ModelRoutes::from_app_config(&app, &models);

    let brain = Arc::new(
        Brain::open(&BrainConfig { path: Some(dir.path().join("cos.rvf")), installation: Some("weftos://local/installations/e2e".into()), package: Some(DIGEST.into()), catalog: None }, dir.path()).unwrap(),
    );
    let client = harness_client(harness, Some(TEST_PROJECT));
    let focus = Focus::default();
    let tools = ToolRegistry::new()
        .with(Arc::new(BoardPropose { staging_dir: dir.path().join("staging") }))
        .with(Arc::new(NodesHealth { source: Arc::new(StaticHealth(json!({"reachable": true, "summary": {"healthy_nodes": 2, "total_nodes": 3}, "nodes": [1, 2, 3]}))) }))
        .with(Arc::new(ProjectsActivity { client: client.clone() }))
        .with(Arc::new(BoardRead { client: client.clone(), focus: focus.clone() }))
        .with(Arc::new(BoardClaim { client: client.clone(), focus: focus.clone() }))
        .with(Arc::new(BoardNote { client: client.clone(), focus: focus.clone() }))
        .with(Arc::new(CardCurrent { client: client.clone(), focus }))
        .with(Arc::new(NotesRecord { client: Some(client), brain: Some(brain.clone()) }));
    let agent = CosAgent::new(routes, tools, ToolLog::new(Some(dir.path().join("tool-log.jsonl"))));
    let conv: Arc<dyn Conversation> = Arc::new(agent.clone());
    let mic: Arc<dyn PttControl> = Arc::new(FakeMic { ptt: PushToTalk::new(Arc::new(HeardWords("where are we on the board")), conv.clone()), active: false.into() });
    let (addr, server) = weftos_cos::server::bind_with(conv, Some(mic), "127.0.0.1:0".parse().unwrap()).await.unwrap();
    tokio::spawn(server);
    let base = format!("http://{addr}");
    let c = reqwest::Client::new();

    // One recorder for the whole session.
    let log = Arc::new(Mutex::new(String::new()));
    let mut sse = c.get(format!("{base}/v1/cos/events")).send().await.unwrap();
    let sink = log.clone();
    tokio::spawn(async move {
        while let Ok(Some(chunk)) = sse.chunk().await {
            sink.lock().unwrap().push_str(&String::from_utf8_lossy(&chunk));
        }
    });

    // Turn 1: a spoken question through the held button and the fake STT.
    let r = c.post(format!("{base}/v1/cos/ptt")).json(&json!({"down": true})).send().await.unwrap();
    assert_eq!(r.json::<Value>().await.unwrap(), json!({"active": true}));
    let up: Value = c.post(format!("{base}/v1/cos/ptt")).json(&json!({"down": false})).send().await.unwrap().json().await.unwrap();
    assert_eq!(up["posted"]["text"], "where are we on the board");
    until_idle(&log, 1).await;
    assert!(agent.is_voice_turn(&weftos_cos::TurnId(up["posted"]["turn_id"].as_str().unwrap().to_string())), "the spoken turn is marked for speech");

    // Turn 2: a claim waits for the yes; turn 3 is the yes.
    say(&c, &base, "claim the pad button card", "typed").await;
    until_idle(&log, 2).await;
    // (history reads are recorded as `GET ...`; only writes count here)
    let writes = || posts.lock().unwrap().iter().filter(|(p, _)| !p.starts_with("GET ")).cloned().collect::<Vec<_>>();
    assert!(writes().is_empty(), "nothing is written before the yes");
    say(&c, &base, "yes", "voice").await;
    until_idle(&log, 3).await;
    assert_eq!(writes()[0].1["action"], "claim");

    // Turn 4: a decision is kept in history, on the card and in the brain.
    say(&c, &base, "note that the pad button debounce is 30 ms", "typed").await;
    until_idle(&log, 4).await;
    // Turn 5: the mesh.
    say(&c, &base, "is the mesh healthy", "typed").await;
    until_idle(&log, 5).await;
    // Turn 6: a survey; turn 7 is the owner's tap on choice 1.
    say(&c, &base, "help me plan today", "typed").await;
    until_idle(&log, 6).await;
    let ans = c.post(format!("{base}/v1/cos/answer")).json(&json!({"kind": "answer", "id": "q1", "value": 1})).send().await.unwrap();
    assert_eq!(ans.status(), 202);
    until_idle(&log, 7).await;

    // What the harness saw: the claim, the card comment and the history note.
    let actions: Vec<String> = writes().iter().map(|(p, b)| format!("{p} {}", b["action"].as_str().or(b["kind"].as_str()).unwrap_or(""))).collect();
    assert_eq!(
        actions,
        ["/api/harness/tickets claim", "/api/harness/tickets comment", &format!("/api/harness/projects/{TEST_PROJECT}/events note")]
    );
    // The brain holds the decision with its metadata.
    assert_eq!(brain.len(), 1);
    let hit = &brain.search("pad button debounce", 1)[0];
    assert_eq!(hit.2["kind"], "decision");
    assert_eq!(hit.2["why"], "the GPIO bounces for about 20 ms");
    assert_eq!(hit.2["where"], json!(["WEFT-902"]));
    weftos_cos::brain::validate(&hit.2).unwrap();

    // What the model saw: the bare model name, every tool, the host's system
    // prompt first, and tool results fed back. No credential anywhere.
    let seen = stub.seen.lock().unwrap();
    assert_eq!(seen.len(), 10, "ten model rounds; the confirmation turn needs none");
    assert!(seen.iter().all(|r| r["model"] == "mlx-community/GLM-4.7-Flash-4bit"));
    let names: Vec<&str> = seen[0]["tools"].as_array().unwrap().iter().map(|t| t["function"]["name"].as_str().unwrap()).collect();
    for want in ["projects_activity", "board_claim", "board_note", "board_read", "card_current", "notes_record", "nodes_health", "ui_ask", "ui_checklist"] {
        assert!(names.contains(&want), "{want} is offered to the model");
    }
    assert_eq!(seen[0]["messages"][0]["role"], "system");
    assert!(seen[1]["messages"].as_array().unwrap().iter().any(|m| m["role"] == "tool" && m["content"].as_str().unwrap().contains("Console C2 screen")));
    let everything = format!("{}{}{}", log.lock().unwrap(), serde_json::to_string(&*seen).unwrap(), std::fs::read_to_string(dir.path().join("tool-log.jsonl")).unwrap());
    assert!(!everything.contains(TEST_TOKEN));

    // The recorded stream.
    let recorded = normalise(&log.lock().unwrap());
    let text: String = recorded.iter().map(|v| format!("{v}\n")).collect();
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(GOLDEN);
    if std::env::var("COS_UPDATE_GOLDEN").as_deref() == Ok("1") {
        std::fs::write(&path, &text).unwrap();
    }
    let golden = std::fs::read_to_string(&path).unwrap_or_else(|_| panic!("missing {GOLDEN}; run with COS_UPDATE_GOLDEN=1"));
    assert_eq!(text, golden, "the session stream changed; if intended, rerun with COS_UPDATE_GOLDEN=1");
}
