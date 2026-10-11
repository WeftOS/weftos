//! `POST /v1/cos/ptt`: the console host's button edges reach the agent's
//! push-to-talk owner, the reply carries what was posted, and the route
//! answers 503 when the agent has no microphone.

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use common::{ScriptedLlm, build_agent};
use weftos_cos::CosError;
use weftos_cos::conversation::Conversation;
use weftos_cos::voice::{PttControl, PttEdge, PttOutcome};
use weftos_cos::TurnId;

/// Records edges; `Up` reports a posted transcript.
#[derive(Default)]
struct RecorderPtt {
    edges: Mutex<Vec<PttEdge>>,
    active: AtomicBool,
}

#[async_trait]
impl PttControl for RecorderPtt {
    async fn on_edge(&self, edge: PttEdge) -> Result<Option<PttOutcome>, CosError> {
        self.edges.lock().unwrap().push(edge);
        match edge {
            PttEdge::Down => {
                self.active.store(true, Ordering::SeqCst);
                Ok(None)
            }
            PttEdge::Up => {
                self.active.store(false, Ordering::SeqCst);
                Ok(Some(PttOutcome::Posted { text: "hello from the mic".into(), turn_id: TurnId::new() }))
            }
        }
    }

    fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }
}

#[tokio::test]
async fn ptt_edges_reach_the_control_and_the_up_reply_carries_the_transcript() {
    let t = build_agent(ScriptedLlm::new(vec![ScriptedLlm::text("ok")]), None, None).await;
    let conv: Arc<dyn Conversation> = Arc::new(t.agent.clone());
    let ptt = Arc::new(RecorderPtt::default());
    let control: Arc<dyn PttControl> = ptt.clone();
    let (addr, server) = weftos_cos::server::bind_with(conv, Some(control), "127.0.0.1:0".parse().unwrap()).await.unwrap();
    tokio::spawn(server);
    let client = reqwest::Client::new();

    let res = client.post(format!("http://{addr}/v1/cos/ptt")).json(&serde_json::json!({"down": true})).send().await.unwrap();
    assert_eq!(res.status(), 202);
    let body = res.json::<serde_json::Value>().await.unwrap();
    assert_eq!(body["active"], true);

    let res = client.post(format!("http://{addr}/v1/cos/ptt")).json(&serde_json::json!({"down": false})).send().await.unwrap();
    assert_eq!(res.status(), 202);
    let body = res.json::<serde_json::Value>().await.unwrap();
    assert_eq!(body["active"], false);
    assert_eq!(body["posted"]["text"], "hello from the mic");
    assert!(body["posted"]["turn_id"].is_string());
    assert_eq!(ptt.edges.lock().unwrap().as_slice(), [PttEdge::Down, PttEdge::Up]);

    let res = client.post(format!("http://{addr}/v1/cos/ptt")).json(&serde_json::json!({"down": "yes"})).send().await.unwrap();
    assert_eq!(res.status(), 400);
}

#[tokio::test]
async fn without_a_microphone_the_route_answers_503() {
    let t = build_agent(ScriptedLlm::new(vec![ScriptedLlm::text("ok")]), None, None).await;
    let conv: Arc<dyn Conversation> = Arc::new(t.agent.clone());
    let (addr, server) = weftos_cos::server::bind(conv, "127.0.0.1:0".parse().unwrap()).await.unwrap();
    tokio::spawn(server);
    let res = reqwest::Client::new().post(format!("http://{addr}/v1/cos/ptt")).json(&serde_json::json!({"down": true})).send().await.unwrap();
    assert_eq!(res.status(), 503);
    let body = res.json::<serde_json::Value>().await.unwrap();
    assert!(body["error"].as_str().unwrap().contains("--mic"));
}
