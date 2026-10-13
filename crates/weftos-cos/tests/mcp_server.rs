//! `weftos-cos mcp` against a real agent over its loopback API (scripted
//! model), and against a fake one: the tool list, what `cos.say` returns,
//! the endpoints it may speak as, and the rules `cos.confirm` refuses on.

mod common;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use clawft_services::mcp::{CallToolResult, ContentBlock, ToolProvider};
use clawft_services::mcp::composite::CompositeToolProvider;
use clawft_services::mcp::server::McpServerShell;
use common::ScriptedLlm;
use serde_json::{Value, json};
use weftos_cos::agent::CosAgent;
use weftos_cos::config::DevicesConfig;
use weftos_cos::conversation::Conversation;
use weftos_cos::mcp_server::CosMcp;
use weftos_cos::routing::{ModelRoutes, Route};
use weftos_cos::tools::board::BoardPropose;
use weftos_cos::tools::devices::{DevicesStatus, LeafRpc};
use weftos_cos::tools::kickoff::WorkKickoff;
use weftos_cos::tools::{ToolLog, ToolRegistry};

struct NoDaemon;
#[async_trait]
impl LeafRpc for NoDaemon {
    async fn call(&self, _: &str, _: Value) -> Result<Option<Value>, weftos_cos::CosError> {
        Ok(None)
    }
}

struct Rig {
    mcp: CosMcp,
    agent: CosAgent,
    base: String,
    dir: tempfile::TempDir,
}

/// A real agent (scripted model, `work.kickoff` as the gated tool, a
/// `devices.status` over an empty directory) served on loopback.
async fn rig(script: Vec<Value>) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let llm = ScriptedLlm::new(script);
    let routes = ModelRoutes::new(Route::new("local/fast-model", llm.clone()), Route::new("anthropic/big-model", llm), 0.3);
    let cfg = DevicesConfig {
        mentra_dir: dir.path().join("mentra"),
        glance_status: dir.path().join("glance.json"),
        leaf_registry_dir: dir.path().join("registry"),
        pad_leaf_ids: vec![],
    };
    let tools = ToolRegistry::new()
        .with(Arc::new(BoardPropose { staging_dir: dir.path().join("staging") }))
        .with(Arc::new(WorkKickoff { staging_dir: dir.path().join("staging"), runtime_command: None }))
        .with(Arc::new(DevicesStatus { cfg, rpc: Arc::new(NoDaemon) }));
    let agent = CosAgent::new(routes, tools, ToolLog::new(None));
    let conv: Arc<dyn Conversation> = Arc::new(agent.clone());
    let (addr, server) = weftos_cos::server::bind(conv, "127.0.0.1:0".parse().unwrap()).await.unwrap();
    tokio::spawn(server);
    let base = format!("http://{addr}");
    let mcp = CosMcp::new(&base, "mcp", Duration::from_secs(10), true).unwrap();
    Rig { mcp, agent, base, dir }
}

async fn call(p: &CosMcp, name: &str, args: Value) -> (bool, Value) {
    let r: CallToolResult = p.call_tool(name, args).await.unwrap();
    let ContentBlock::Text { text } = &r.content[0];
    (r.is_error, serde_json::from_str(text).unwrap_or_else(|_| json!({"raw": text})))
}

fn kickoff() -> Value {
    ScriptedLlm::tool_call("work_kickoff", json!({"card": "WEFT-902"}))
}

#[tokio::test]
async fn the_server_lists_the_seven_tools() {
    let r = rig(vec![ScriptedLlm::text("ok")]).await;
    let mut names: Vec<String> = r.mcp.list_tools().into_iter().map(|t| t.name).collect();
    names.sort();
    assert_eq!(names, ["cos.confirm", "cos.devices.status", "cos.say", "cos.status", "cos.tools.list", "cos.trace", "cos.turns"]);
    assert!(r.mcp.list_tools().iter().all(|t| t.input_schema["type"] == "object"));
}

#[tokio::test]
async fn say_returns_the_final_message_the_trace_and_marks_it_untrusted() {
    let r = rig(vec![ScriptedLlm::tool_call("devices_status", json!({})), ScriptedLlm::text("Glasses: nothing reported.")]).await;
    let (err, v) = call(&r.mcp, "cos.say", json!({"text": "is the glasses microphone working?"})).await;
    assert!(!err, "{v}");
    assert_eq!(v["status"], "ok");
    assert_eq!(v["message"], "Glasses: nothing reported.");
    assert_eq!(v["data_trust"], "untrusted");
    assert_eq!(v["trace"]["tools_called"][0]["name"], "devices.status");
    assert_eq!(v["trace"]["endpoint"], "mcp");
    assert_eq!(v["trace"]["role"], "quick");
    assert!(v["held"].is_null());

    // The turn went in as a typed turn of the mcp endpoint: it is not spoken.
    let (_, turns) = call(&r.mcp, "cos.turns", json!({"limit": 5})).await;
    let user = turns["turns"].as_array().unwrap().iter().find(|t| t["role"] == "user").unwrap();
    assert_eq!(user["endpoint"], "mcp");
    assert_eq!(turns["data_trust"], "untrusted");
    let id = v["turn_id"].as_str().unwrap();
    let (_, again) = call(&r.mcp, "cos.trace", json!({"turn_id": id})).await;
    assert_eq!(again["turn_id"], id);
}

#[tokio::test]
async fn say_refuses_to_speak_as_a_device_or_with_bad_arguments() {
    let r = rig(vec![ScriptedLlm::text("ok")]).await;
    for (args, what) in [
        (json!({"text": "hi", "endpoint": "host"}), "endpoint"),
        (json!({"text": "hi", "endpoint": "mentra"}), "endpoint"),
        (json!({"text": "hi", "endpoint": "dashboard"}), "endpoint"),
        (json!({"text": "hi", "endpoint": "test-"}), "endpoint"),
        (json!({"text": "hi", "endpoint": 7}), "endpoint"),
        (json!({"text": "  "}), "text"),
        (json!({}), "text"),
        (json!({"text": "hi", "timeout_s": 0}), "timeout_s"),
        (json!({"text": "hi", "timeout_s": "soon"}), "timeout_s"),
    ] {
        let (err, v) = call(&r.mcp, "cos.say", args.clone()).await;
        assert!(err, "{args} should be refused");
        assert_eq!((v["status"].as_str(), v["what"].as_str()), (Some("refused"), Some(what)), "{args}: {v}");
    }
    let (_, turns) = call(&r.mcp, "cos.turns", json!({})).await;
    assert!(turns["turns"].as_array().unwrap().is_empty(), "no refused call reached the agent");
    // A test run's own endpoint is allowed.
    let (err, v) = call(&r.mcp, "cos.say", json!({"text": "hi", "endpoint": "test-run1"})).await;
    assert!(!err && v["trace"]["endpoint"] == "test-run1", "{v}");
}

#[tokio::test]
async fn the_server_refuses_an_endpoint_that_is_a_device() {
    for bad in ["host", "mentra", "dashboard", "", "MCP", "test"] {
        assert!(CosMcp::new("http://127.0.0.1:1", bad, Duration::from_secs(1), true).is_err(), "{bad:?}");
    }
    assert!(CosMcp::new("ftp://x", "mcp", Duration::from_secs(1), true).is_err());
}

#[tokio::test]
async fn a_gated_turn_comes_back_held_and_confirm_yes_runs_it() {
    let r = rig(vec![kickoff()]).await;
    let (err, v) = call(&r.mcp, "cos.say", json!({"text": "start the developer lane on WEFT-902"})).await;
    assert!(!err, "{v}");
    assert_eq!(v["status"], "held");
    assert_eq!(v["held"]["tool"], "work.kickoff");
    assert_eq!(v["trace"]["gates"][0], "work.kickoff:held");
    assert!(!r.dir.path().join("staging").exists(), "say alone cannot lift the gate");
    let turn = v["turn_id"].as_str().unwrap().to_string();

    let (err, v) = call(&r.mcp, "cos.confirm", json!({"turn_id": turn, "decision": "yes"})).await;
    assert!(!err, "{v}");
    assert_eq!(v["trace"]["gates"][0], "work.kickoff:confirmed");
    assert!(v["message"].as_str().unwrap().starts_with("Done"), "{v}");
    assert!(r.dir.path().join("staging").exists());
    let (_, st) = call(&r.mcp, "cos.status", json!({})).await;
    assert!(st["pending"].is_null());
    assert_eq!(st["healthy"], true);
}

#[tokio::test]
async fn confirm_no_cancels() {
    let r = rig(vec![kickoff()]).await;
    let (_, v) = call(&r.mcp, "cos.say", json!({"text": "kick off WEFT-902"})).await;
    let turn = v["turn_id"].as_str().unwrap().to_string();
    let (err, v) = call(&r.mcp, "cos.confirm", json!({"turn_id": turn, "decision": "no"})).await;
    assert!(!err, "{v}");
    assert_eq!(v["trace"]["gates"][0], "work.kickoff:cancelled");
    assert!(!r.dir.path().join("staging").exists());
}

#[tokio::test]
async fn confirm_refuses_without_a_gate_a_matching_turn_or_a_valid_decision() {
    let r = rig(vec![kickoff()]).await;
    // Nothing held.
    let (err, v) = call(&r.mcp, "cos.confirm", json!({"turn_id": "abc", "decision": "yes"})).await;
    assert!(err);
    assert_eq!(v["status"], "refused");
    assert!(v["reason"].as_str().unwrap().contains("no action is waiting"), "{v}");

    let (_, said) = call(&r.mcp, "cos.say", json!({"text": "kick off WEFT-902"})).await;
    let turn = said["turn_id"].as_str().unwrap().to_string();
    for (args, needle) in [
        (json!({"turn_id": "someone-elses-turn", "decision": "yes"}), "different turn"),
        (json!({"turn_id": turn, "decision": "maybe"}), "yes"),
        (json!({"turn_id": turn}), "yes"),
        (json!({"decision": "yes"}), "turn_id"),
        (json!({"turn_id": "../status", "decision": "yes"}), "turn_id"),
        (json!({"turn_id": turn, "decision": "yes", "endpoint": "test-other"}), "only that endpoint"),
        (json!({"turn_id": turn, "decision": "yes", "endpoint": "host"}), "only that endpoint"),
    ] {
        let (err, v) = call(&r.mcp, "cos.confirm", args.clone()).await;
        assert!(err, "{args}");
        assert_eq!(v["status"], "refused", "{args}");
        assert!(v["reason"].as_str().unwrap().contains(needle), "{args}: {v}");
    }
    // Every refusal left the gate held.
    assert_eq!(r.agent.status()["pending"]["turn_id"], turn.as_str());
    assert!(!r.dir.path().join("staging").exists());
}

#[tokio::test]
async fn confirm_never_lifts_a_gate_another_endpoint_asked() {
    let r = rig(vec![kickoff()]).await;
    // The owner's phone asks; the gate is the phone's.
    let phone = r.agent.utter(weftos_cos::UserUtterance::typed("kick off WEFT-902").from_endpoint("dashboard")).await.unwrap();
    for _ in 0..100 {
        if r.agent.status()["pending"]["turn_id"] == phone.0.as_str() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let (err, v) = call(&r.mcp, "cos.confirm", json!({"turn_id": phone.0, "decision": "yes"})).await;
    assert!(err, "{v}");
    assert!(v["reason"].as_str().unwrap().contains("\"dashboard\""), "{v}");
    assert!(!r.dir.path().join("staging").exists(), "the phone's gate is still the phone's");
    assert_eq!(r.agent.status()["pending"]["turn_id"], phone.0.as_str());
}

#[tokio::test]
async fn confirm_is_refused_for_an_agent_off_this_machine_and_with_no_confirm() {
    for base in ["http://100.64.0.9:18300", "http://example.com", "http://127.0.0.1@evil.example:80"] {
        let m = CosMcp::new(base, "mcp", Duration::from_secs(1), true).unwrap();
        let (err, v) = call(&m, "cos.confirm", json!({"turn_id": "t", "decision": "yes"})).await;
        assert!(err, "{base}");
        assert!(v["reason"].as_str().unwrap().contains("not on this machine"), "{base}: {v}");
    }
    let r = rig(vec![kickoff()]).await;
    let off = CosMcp::new(&r.base, "mcp", Duration::from_secs(5), false).unwrap();
    let (_, said) = call(&off, "cos.say", json!({"text": "kick off WEFT-902"})).await;
    let (err, v) = call(&off, "cos.confirm", json!({"turn_id": said["turn_id"], "decision": "yes"})).await;
    assert!(err);
    assert!(v["reason"].as_str().unwrap().contains("--no-confirm"), "{v}");
    assert!(!r.dir.path().join("staging").exists());
}

#[tokio::test]
async fn devices_tools_and_status_pass_through_without_a_model_turn() {
    let r = rig(vec![ScriptedLlm::text("unused")]).await;
    let (err, d) = call(&r.mcp, "cos.devices.status", json!({})).await;
    assert!(!err, "{d}");
    assert!(d.get("glasses").is_some() || d.get("pad").is_some(), "{d}");
    let (_, tools) = call(&r.mcp, "cos.tools.list", json!({})).await;
    let ids: Vec<&str> = tools["tools"].as_array().unwrap().iter().map(|t| t["id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"devices.status") && ids.contains(&"work.kickoff"), "{ids:?}");
    let kick = tools["tools"].as_array().unwrap().iter().find(|t| t["id"] == "work.kickoff").unwrap();
    assert_eq!(kick["gate"], "confirm");
    let (_, turns) = call(&r.mcp, "cos.turns", json!({})).await;
    assert!(turns["turns"].as_array().unwrap().is_empty(), "no turn was run");
}

#[tokio::test]
async fn an_unreachable_agent_is_a_refusal_not_an_empty_result() {
    let m = CosMcp::new("http://127.0.0.1:1", "mcp", Duration::from_secs(1), true).unwrap();
    for (name, args) in [
        ("cos.status", json!({})),
        ("cos.turns", json!({})),
        ("cos.tools.list", json!({})),
        ("cos.devices.status", json!({})),
        ("cos.trace", json!({"turn_id": "abc"})),
        ("cos.say", json!({"text": "hi"})),
    ] {
        let (err, v) = call(&m, name, args).await;
        assert!(err, "{name}: {v}");
        assert_eq!(v["status"], "refused", "{name}");
        assert!(v["reason"].as_str().unwrap().contains("unreachable"), "{name}: {v}");
    }
    assert!(m.call_tool("cos.nope", json!({})).await.is_err(), "an unknown tool is not found");
}

#[tokio::test]
async fn a_stuck_turn_times_out_with_its_turn_id() {
    // A model that never returns: say gives up at its own timeout.
    struct Hang;
    #[async_trait]
    impl clawft_core::pipeline::transport::LlmProvider for Hang {
        async fn complete(&self, _: &str, _: &[Value], _: &[Value], _: Option<i32>, _: Option<f64>, _: Option<&Value>) -> Result<Value, String> {
            tokio::time::sleep(Duration::from_secs(60)).await;
            Err("late".into())
        }
        async fn complete_stream(&self, m: &str, msgs: &[Value], t: &[Value], mt: Option<i32>, temp: Option<f64>, tc: Option<&Value>, _tx: tokio::sync::mpsc::Sender<String>) -> Result<Value, String> {
            self.complete(m, msgs, t, mt, temp, tc).await
        }
    }
    let routes = ModelRoutes::new(Route::new("local/m", Arc::new(Hang)), Route::new("local/m", Arc::new(Hang)), 0.3);
    let agent = CosAgent::new(routes, ToolRegistry::new(), ToolLog::new(None));
    let conv: Arc<dyn Conversation> = Arc::new(agent);
    let (addr, server) = weftos_cos::server::bind(conv, "127.0.0.1:0".parse().unwrap()).await.unwrap();
    tokio::spawn(server);
    let m = CosMcp::new(&format!("http://{addr}"), "mcp", Duration::from_secs(1), true).unwrap();
    let (err, v) = call(&m, "cos.say", json!({"text": "hello", "timeout_s": 1})).await;
    assert!(err);
    assert_eq!(v["status"], "timeout");
    assert!(v["turn_id"].is_string());
}

#[tokio::test]
async fn the_mcp_shell_serves_initialize_list_and_call() {
    let r = rig(vec![ScriptedLlm::text("pong")]).await;
    let mut composite = CompositeToolProvider::new();
    composite.register(Box::new(CosMcp::new(&r.base, "mcp", Duration::from_secs(10), true).unwrap()));
    let mut shell = McpServerShell::new(composite);
    let init = shell
        .handle_message(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}}), None)
        .await
        .unwrap();
    assert!(init["result"]["capabilities"]["tools"].is_object(), "{init}");
    shell.handle_message(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}), None).await;
    let list = shell.handle_message(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}), None).await.unwrap();
    let names: Vec<&str> = list["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"cos.say") && names.contains(&"cos.confirm"), "{names:?}");
    let said = shell
        .handle_message(json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "cos.say", "arguments": {"text": "ping"}}}), None)
        .await
        .unwrap();
    let text = said["result"]["content"][0]["text"].as_str().unwrap();
    let body: Value = serde_json::from_str(text).unwrap();
    assert_eq!(body["message"], "pong");
}
