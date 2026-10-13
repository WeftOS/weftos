//! MCP servers as chief-of-staff tools: discovery with an allow-list, gates,
//! a server that is down and recovers, schema passthrough, output caps and
//! secrets. Fakes: an in-process HTTP MCP server and the stdio example binary.

mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use common::{ScriptedLlm, collect_until_final};
use serde_json::{Value, json};
use weftos_cos::agent::CosAgent;
use weftos_cos::config::{McpOrigin, McpServerConfig};
use weftos_cos::routing::{ModelRoutes, Route};
use weftos_cos::tools::mcp::McpSource;
use weftos_cos::tools::{Gate, ToolLog, ToolRegistry, ToolSource};
use weftos_cos::{Conversation, Event, TurnId, UserUtterance};

const TOKEN: &str = "sk-live-0123456789abcdefghijklmnop";

#[derive(Default)]
struct Fake {
    /// Bearer the server accepts; `None` accepts anyone.
    expect: Option<String>,
    /// Answer 503 to everything while set.
    down: AtomicBool,
    /// `tools/call` names received.
    calls: Mutex<Vec<String>>,
}

fn rpc(id: &Value, result: Value) -> Response {
    axum::Json(json!({"jsonrpc": "2.0", "id": id, "result": result})).into_response()
}

fn text(t: String) -> Value {
    json!({"content": [{"type": "text", "text": t}]})
}

async fn handle(State(f): State<Arc<Fake>>, headers: HeaderMap, body: axum::body::Bytes) -> Response {
    if f.down.load(Ordering::SeqCst) {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let got = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    if let Some(want) = &f.expect
        && got != format!("Bearer {want}")
    {
        // A sloppy server that echoes what it was sent.
        return (StatusCode::UNAUTHORIZED, format!("bad credentials: {got}")).into_response();
    }
    let req: Value = serde_json::from_slice(&body).unwrap();
    let Some(id) = req.get("id") else { return StatusCode::ACCEPTED.into_response() };
    match req["method"].as_str().unwrap() {
        "initialize" => rpc(id, json!({"protocolVersion": "2025-06-18", "capabilities": {"tools": {}}, "serverInfo": {"name": "fake", "version": "1"}})),
        "tools/list" => rpc(id, json!({"tools": [
            {"name": "search_flights", "description": "Find flights between two airports.",
             "inputSchema": {"type": "object", "properties": {"from": {"type": "string"}, "to": {"type": "string"}, "date": {"type": "string", "format": "date"}}, "required": ["from", "to"], "additionalProperties": false}},
            {"name": "book_flight", "description": "Book a flight. A write.", "inputSchema": {"type": "object", "properties": {"offer": {"type": "string"}}}},
            {"name": "dump", "description": "A long result.", "inputSchema": {"type": "object"}},
            {"name": "whoami", "description": "Echo the credential it saw.", "inputSchema": {"type": "object"}},
            {"name": "fail", "description": "Always fails.", "inputSchema": {"type": "object"}},
            {"name": "hotel_search", "description": "Find hotels.", "inputSchema": {"type": "object"}},
            {"name": "weather", "description": "Forecast.", "inputSchema": {"type": "object"}}
        ]})),
        "tools/call" => {
            let name = req["params"]["name"].as_str().unwrap().to_string();
            f.calls.lock().unwrap().push(name.clone());
            rpc(id, match name.as_str() {
                "dump" => text("y".repeat(20_000)),
                "whoami" => text(format!("you are {got}")),
                "fail" => json!({"isError": true, "content": [{"type": "text", "text": "upstream said no"}]}),
                other => text(format!("{other} ok {}", req["params"]["arguments"])),
            })
        }
        _ => rpc(id, json!({})),
    }
}

async fn serve_on(listener: tokio::net::TcpListener, fake: Arc<Fake>) {
    let app = Router::new().route("/mcp", post(handle)).with_state(fake);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
}

async fn start(fake: Arc<Fake>) -> SocketAddr {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    serve_on(l, fake).await;
    addr
}

fn token_file(dir: &std::path::Path, token: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join("token");
    std::fs::write(&p, format!("{token}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
    p
}

fn http_cfg(addr: SocketAddr, token: &std::path::Path, extra: Value) -> McpServerConfig {
    let mut v = json!({"id": "trip", "transport": {"type": "http", "url": format!("http://{addr}/mcp")},
        "auth": {"bearer_file": token}, "allow": ["search_*", "book_flight", "dump", "whoami", "fail"], "retry_secs": 0});
    v.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
    serde_json::from_value(v).unwrap()
}

/// Gates only open for catalog servers; give a test config that origin.
fn authorized(mut cfg: McpServerConfig) -> McpServerConfig {
    cfg.origin = McpOrigin::Catalog { publisher: "test".into(), authorized: true, signed: false };
    cfg
}

async fn registry(cfg: McpServerConfig) -> (ToolRegistry, Arc<McpSource>) {
    let src = Arc::new(McpSource::new(&[cfg]).unwrap());
    src.refresh().await;
    (ToolRegistry::new().with_source(src.clone()), src)
}

fn agent(llm: Arc<ScriptedLlm>, tools: ToolRegistry, dir: &std::path::Path) -> CosAgent {
    let quick = Route::new("local/fast-model", llm.clone());
    let plan = Route::new("anthropic/big-model", llm);
    CosAgent::new(ModelRoutes::new(quick, plan, 0.3), tools, ToolLog::new(Some(dir.join("tool-log.jsonl"))))
}

#[tokio::test]
async fn only_allow_listed_tools_are_registered_with_the_servers_schema() {
    let dir = tempfile::tempdir().unwrap();
    let addr = start(Arc::new(Fake::default())).await;
    let (reg, _) = registry(http_cfg(addr, &token_file(dir.path(), TOKEN), json!({}))).await;
    let ids: Vec<_> = reg.specs().iter().map(|s| s.id).collect();
    assert_eq!(ids, ["trip.search_flights", "trip.book_flight", "trip.dump", "trip.whoami", "trip.fail"]);
    let search = reg.find("trip_search_flights").expect("wire name resolves");
    let spec = search.spec();
    assert_eq!(spec.parameters["required"], json!(["from", "to"]));
    assert_eq!(spec.parameters["properties"]["date"]["format"], "date", "schema passes through untouched");
    assert_eq!(spec.parameters["additionalProperties"], false);
    assert!(spec.description.starts_with("[trip] Find flights"));
    assert_eq!(reg.openai_specs()[0]["function"]["name"], "trip_search_flights");
    assert!(reg.find("trip.hotel_search").is_none() && reg.find("trip_weather").is_none());
}

#[tokio::test]
async fn an_empty_allow_list_exposes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let addr = start(Arc::new(Fake::default())).await;
    let (reg, _) = registry(http_cfg(addr, &token_file(dir.path(), TOKEN), json!({"allow": []}))).await;
    assert!(reg.specs().is_empty() && reg.catalog().is_empty());
}

#[tokio::test]
async fn gates_default_to_confirm_and_per_tool_overrides_win() {
    let dir = tempfile::tempdir().unwrap();
    let addr = start(Arc::new(Fake::default())).await;
    let cfg = authorized(http_cfg(addr, &token_file(dir.path(), TOKEN), json!({"tool_gates": {"search_*": "open", "dump": "open", "search_flights": "confirm"}})));
    let (reg, _) = registry(cfg).await;
    let gate = |id: &str| reg.find(id).unwrap().spec().gate;
    assert_eq!(gate("trip.search_flights"), Gate::Confirm, "exact name beats glob");
    assert_eq!(gate("trip.dump"), Gate::Open);
    assert_eq!(gate("trip.book_flight"), Gate::Confirm, "server default is confirm");
    let cfg = authorized(http_cfg(addr, &token_file(dir.path(), TOKEN), json!({"gate": "open", "tool_gates": {"book_*": "confirm"}})));
    let (reg, _) = registry(cfg).await;
    assert_eq!(reg.find("trip.search_flights").unwrap().spec().gate, Gate::Open);
    assert_eq!(reg.find("trip.book_flight").unwrap().spec().gate, Gate::Confirm);
}

#[tokio::test]
async fn a_gated_write_waits_for_yes_and_a_read_does_not() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let addr = start(fake.clone()).await;
    let cfg = authorized(http_cfg(addr, &token_file(dir.path(), TOKEN), json!({"tool_gates": {"search_*": "open"}})));
    let (reg, _) = registry(cfg).await;
    let llm = ScriptedLlm::new(vec![
        ScriptedLlm::tool_call("trip_search_flights", json!({"from": "JFK", "to": "LIS"})),
        ScriptedLlm::tool_call("trip_book_flight", json!({"offer": "o-1"})),
    ]);
    let agent = agent(llm.clone(), reg, dir.path());
    let mut rx = agent.events();
    agent.run_turn(TurnId::new(), UserUtterance::typed("find flights JFK to LIS and book the offer")).await;
    let events = collect_until_final(&mut rx, Duration::from_secs(5)).await;
    let tool: Vec<_> = events.iter().filter_map(|e| match e { Event::Tool { name, status, .. } => Some((name.as_str(), status.as_str())), _ => None }).collect();
    assert!(tool.contains(&("trip.search_flights", "ok")), "{tool:?}");
    assert_eq!(tool.last(), Some(&("trip.book_flight", "awaiting_confirmation")), "{tool:?}");
    assert_eq!(*fake.calls.lock().unwrap(), ["search_flights"], "the write has not reached the server");
    let ask = events.iter().find_map(|e| match e { Event::Message { text, .. } => Some(text.clone()), _ => None }).unwrap();
    assert!(ask.contains("trip.book_flight") && ask.contains("o-1"), "the owner sees what will be sent: {ask}");

    agent.run_turn(TurnId::new(), UserUtterance::voice("yes")).await;
    let _ = collect_until_final(&mut rx, Duration::from_secs(5)).await;
    assert_eq!(*fake.calls.lock().unwrap(), ["search_flights", "book_flight"]);
}

#[tokio::test]
async fn a_server_down_at_startup_is_reported_then_recovers() {
    let dir = tempfile::tempdir().unwrap();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    drop(l); // nothing listens yet
    let (reg, _) = registry(http_cfg(addr, &token_file(dir.path(), TOKEN), json!({}))).await;
    assert!(reg.specs().is_empty(), "no tools while down");
    let down = reg.unavailable_sources();
    assert_eq!(down.len(), 1);
    assert_eq!(down[0].0, "mcp:trip");
    assert!(reg.catalog()[0].contains("`trip` is unavailable"), "{:?}", reg.catalog());

    let l = tokio::net::TcpListener::bind(addr).await.unwrap();
    serve_on(l, Arc::new(Fake::default())).await;
    reg.refresh().await;
    assert!(reg.unavailable_sources().is_empty());
    assert_eq!(reg.specs().len(), 5);
    assert!(reg.catalog()[0].contains("5 tools") && reg.catalog()[0].contains("trip_search_flights"));
}

#[tokio::test]
async fn a_server_that_dies_mid_session_fails_the_call_politely_and_reconnects_lazily() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let addr = start(fake.clone()).await;
    let (reg, _) = registry(http_cfg(addr, &token_file(dir.path(), TOKEN), json!({}))).await;
    let tool = reg.find("trip.search_flights").unwrap();
    assert!(tool.call(json!({"from": "a", "to": "b"})).await.is_ok());

    fake.down.store(true, Ordering::SeqCst);
    let err = tool.call(json!({"from": "a", "to": "b"})).await.unwrap_err().to_string();
    assert!(err.contains("trip.search_flights"), "{err}");
    assert!(!reg.unavailable_sources().is_empty(), "reported down");
    let err = tool.call(json!({"from": "a", "to": "b"})).await.unwrap_err().to_string();
    assert!(err.contains("unavailable"), "{err}");

    fake.down.store(false, Ordering::SeqCst);
    let out = tool.call(json!({"from": "a", "to": "b"})).await.expect("reconnects on the next call");
    assert!(out.result["output"].as_str().unwrap().contains("search_flights ok"));
    assert!(reg.unavailable_sources().is_empty());
}

#[tokio::test]
async fn large_results_are_truncated_with_a_count() {
    let dir = tempfile::tempdir().unwrap();
    let addr = start(Arc::new(Fake::default())).await;
    let (reg, _) = registry(http_cfg(addr, &token_file(dir.path(), TOKEN), json!({"max_output_bytes": 500}))).await;
    let out = reg.find("trip.dump").unwrap().call(json!({})).await.unwrap();
    assert_eq!(out.result["output"].as_str().unwrap().len(), 500);
    assert_eq!(out.result["more"], "+19500 more bytes; ask for less");
    assert!(out.summary.contains("+19500 more"), "{}", out.summary);
}

#[tokio::test]
async fn a_tool_error_from_the_server_is_a_tool_error() {
    let dir = tempfile::tempdir().unwrap();
    let addr = start(Arc::new(Fake::default())).await;
    let (reg, _) = registry(http_cfg(addr, &token_file(dir.path(), TOKEN), json!({}))).await;
    let err = reg.find("trip.fail").unwrap().call(json!({})).await.unwrap_err().to_string();
    assert!(err.contains("upstream said no"), "{err}");
    assert!(reg.unavailable_sources().is_empty(), "a tool failure is not an outage");
}

#[tokio::test]
async fn the_bearer_reaches_the_server_and_never_leaks() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake { expect: Some(TOKEN.into()), ..Default::default() });
    let addr = start(fake).await;
    let file = token_file(dir.path(), TOKEN);
    let cfg = authorized(http_cfg(addr, &file, json!({"gate": "open"})));
    assert!(!format!("{cfg:?}").contains(TOKEN), "config holds a path, not the secret");
    let (reg, _) = registry(cfg).await;
    assert_eq!(reg.specs().len(), 5, "the right token connects");

    // The server echoes the credential back in a result; the agent never shows it.
    let llm = ScriptedLlm::new(vec![ScriptedLlm::tool_call("trip_whoami", json!({})), ScriptedLlm::text(&format!("It said {TOKEN}."))]);
    let agent = agent(llm.clone(), reg, dir.path());
    let mut rx = agent.events();
    agent.run_turn(TurnId::new(), UserUtterance::typed("who am i to the trip server")).await;
    let events = collect_until_final(&mut rx, Duration::from_secs(5)).await;
    let dump = serde_json::to_string(&events).unwrap() + &std::fs::read_to_string(dir.path().join("tool-log.jsonl")).unwrap() + &serde_json::to_string(&*llm.seen.lock().unwrap()).unwrap();
    assert!(!dump.contains(TOKEN), "token leaked: {dump}");
    assert!(dump.contains("[redacted]"));
}

#[tokio::test]
async fn a_wrong_token_fails_without_echoing_either_token() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake { expect: Some("the-real-one-0123456789".into()), ..Default::default() });
    let addr = start(fake).await;
    let (reg, _) = registry(http_cfg(addr, &token_file(dir.path(), TOKEN), json!({}))).await;
    let down = reg.unavailable_sources();
    assert_eq!(down.len(), 1);
    assert!(down[0].1.contains("401"), "{down:?}");
    let shown = format!("{down:?}{:?}", reg.catalog());
    assert!(!shown.contains(TOKEN) && !shown.contains("the-real-one"), "{shown}");
}

#[tokio::test]
async fn credential_problems_name_the_source_never_the_value() {
    let dir = tempfile::tempdir().unwrap();
    let addr: SocketAddr = "127.0.0.1:9".parse().unwrap();
    // Missing env var.
    let cfg: McpServerConfig = serde_json::from_value(json!({"id": "a", "transport": {"type": "http", "url": format!("http://{addr}/mcp")},
        "auth": {"bearer_env": "COS_TEST_NO_SUCH_TOKEN_VAR"}, "allow": ["*"]})).unwrap();
    let (reg, _) = registry(cfg).await;
    assert!(reg.unavailable_sources()[0].1.contains("COS_TEST_NO_SUCH_TOKEN_VAR is not set"));
    // A token file readable by others is refused.
    let p = token_file(dir.path(), TOKEN);
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    let (reg, _) = registry(http_cfg(addr, &p, json!({}))).await;
    let why = &reg.unavailable_sources()[0].1;
    assert!(why.contains("must be 0600") && !why.contains(TOKEN), "{why}");
}

#[tokio::test]
async fn too_many_tools_are_cut_to_the_best_matches_for_the_request() {
    let dir = tempfile::tempdir().unwrap();
    let addr = start(Arc::new(Fake::default())).await;
    let cfg = http_cfg(addr, &token_file(dir.path(), TOKEN), json!({"allow": ["*"]}));
    let (reg, _) = registry(cfg).await;
    let reg = reg.with_dynamic_cap(3);
    assert_eq!(reg.specs().len(), 7, "everything stays callable");
    let offered: Vec<_> = reg.specs_for("find me hotel options in Lisbon").iter().map(|s| s.id).collect();
    assert_eq!(offered.len(), 3);
    assert!(offered.contains(&"trip.hotel_search"), "{offered:?}");
    let offered: Vec<_> = reg.specs_for("what is the weather").iter().map(|s| s.id).collect();
    assert!(offered.contains(&"trip.weather"), "{offered:?}");
}

#[tokio::test]
async fn the_example_stdio_server_works_end_to_end() {
    let cfg: McpServerConfig = serde_json::from_value(json!({"id": "local",
        "transport": {"type": "stdio", "command": env!("CARGO_BIN_EXE_weftos-cos-mcp-echo")},
        "allow": ["echo", "time.*", "big", "note.append"], "tool_gates": {"echo": "open", "time.*": "open", "big": "open"}, "max_output_bytes": 300})).unwrap();
    let (reg, _) = registry(authorized(cfg)).await;
    let ids: Vec<_> = reg.specs().iter().map(|s| s.id).collect();
    assert_eq!(ids, ["local.echo", "local.time_now", "local.note_append", "local.big"]);
    let echo = reg.find("local_echo").unwrap().call(json!({"text": "hi there"})).await.unwrap();
    assert_eq!(echo.result["output"], "hi there");
    let now = reg.find("local.time_now").unwrap().call(json!({})).await.unwrap();
    assert!(now.result["output"].as_str().unwrap().starts_with("20"));
    assert_eq!(reg.find("local.note_append").unwrap().spec().gate, Gate::Confirm);
    let big = reg.find("local.big").unwrap().call(json!({"bytes": 5000})).await.unwrap();
    assert_eq!(big.result["output"].as_str().unwrap().len(), 300);
}

#[tokio::test]
async fn a_missing_stdio_command_is_reported_not_fatal() {
    let cfg: McpServerConfig = serde_json::from_value(json!({"id": "gone", "transport": {"type": "stdio", "command": "/nonexistent/mcp-server"}, "allow": ["*"]})).unwrap();
    let (reg, _) = registry(cfg).await;
    assert!(reg.specs().is_empty());
    assert!(reg.unavailable_sources()[0].1.contains("cannot start"));
}

#[tokio::test]
async fn hand_written_servers_are_unvetted_and_catalog_servers_are_labelled_and_narrowed() {
    use weftos_cos::config::CosConfig;
    use weftos_cos::mcp_catalog::resolve_servers;
    let dir = tempfile::tempdir().unwrap();
    let addr = start(Arc::new(Fake::default())).await;
    let catalog = json!({"schema": 1, "publisher": "weavelogic", "servers": [{
        "id": "trip", "name": "Trip", "publisher": "weavelogic", "authorized": true,
        "transport": {"type": "http", "url": format!("http://{addr}/mcp")},
        "auth": {"kind": "bearer_env", "env": "COS_TEST_NO_SUCH_TOKEN_VAR"},
        "allow": ["search_*", "book_flight"], "gate": "open", "tool_gates": {"book_*": "confirm"}}]});
    std::fs::write(dir.path().join("catalog.json"), catalog.to_string()).unwrap();
    let file = token_file(dir.path(), TOKEN);
    let cfg: CosConfig = serde_json::from_value(json!({
        "mcp_catalog": {"path": dir.path().join("catalog.json")},
        "mcp_enable": [{"id": "trip", "allow": ["search_*"], "confirm": ["search_flights"], "auth": {"bearer_file": file}}],
        "mcp_servers": [{"id": "mine", "transport": {"type": "stdio", "command": env!("CARGO_BIN_EXE_weftos-cos-mcp-echo")}, "allow": ["echo"]}]
    })).unwrap();
    let servers = resolve_servers(&cfg).unwrap();
    let src = Arc::new(McpSource::new(&servers).unwrap());
    src.refresh().await;
    let labels = src.describe();
    assert_eq!(labels[0], "mine: unvetted");
    assert_eq!(labels[1], "trip: authorized by weavelogic, unsigned");
    let reg = ToolRegistry::new().with_source(src);
    let ids: Vec<_> = reg.specs().iter().map(|s| s.id).collect();
    assert_eq!(ids, ["mine.echo", "trip.search_flights"], "book_flight is outside the user's narrowing");
    assert_eq!(reg.find("mine.echo").unwrap().spec().gate, Gate::Confirm, "hand-written defaults to confirm");
    assert_eq!(reg.find("trip.search_flights").unwrap().spec().gate, Gate::Confirm, "the user tightened a catalog open gate");
}

#[tokio::test]
async fn unvetted_servers_refuse_open_and_confirm_every_tool() {
    let dir = tempfile::tempdir().unwrap();
    let addr = start(Arc::new(Fake::default())).await;
    let file = token_file(dir.path(), TOKEN);
    for extra in [json!({"gate": "open"}), json!({"tool_gates": {"search_*": "open"}})] {
        let err = McpSource::new(&[http_cfg(addr, &file, extra)]).err().expect("refused").to_string();
        assert!(err.contains("unvetted servers always need confirmation"), "{err}");
    }
    let (reg, _) = registry(http_cfg(addr, &file, json!({"tool_gates": {"search_*": "confirm"}}))).await;
    assert!(reg.specs().iter().all(|s| s.gate == Gate::Confirm), "every unvetted tool needs a yes");
    assert_eq!(reg.specs().len(), 5);

    // End to end: even a read-only search is held until the owner says yes.
    let fake = Arc::new(Fake::default());
    let addr = start(fake.clone()).await;
    let (reg, _) = registry(http_cfg(addr, &file, json!({}))).await;
    let llm = ScriptedLlm::new(vec![ScriptedLlm::tool_call("trip_search_flights", json!({"from": "a", "to": "b"}))]);
    let agent = agent(llm, reg, dir.path());
    let mut rx = agent.events();
    agent.run_turn(TurnId::new(), UserUtterance::typed("find flights a to b")).await;
    let events = collect_until_final(&mut rx, Duration::from_secs(5)).await;
    assert!(events.iter().any(|e| matches!(e, Event::Tool { status, .. } if status == "awaiting_confirmation")));
    assert!(fake.calls.lock().unwrap().is_empty());
    agent.run_turn(TurnId::new(), UserUtterance::voice("yes")).await;
    let _ = collect_until_final(&mut rx, Duration::from_secs(5)).await;
    assert_eq!(*fake.calls.lock().unwrap(), ["search_flights"]);
}
