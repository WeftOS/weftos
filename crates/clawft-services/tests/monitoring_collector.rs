//! WEFT-305: `/api/monitoring/*` handlers against the real in-process
//! metrics collector (`RoutingDecisionHistory`, shared with the agent
//! pipeline). Covers empty state, counters, concurrent updates, the
//! serialized wire format, and response stability.
//!
//! Stub access impls are copied from `api_middleware.rs`.

#![cfg(feature = "api")]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use clawft_services::api::{
    AgentAccess, AgentInfo, ApiState, BusAccess, ChannelAccess, ChannelStatusInfo, ConfigAccess,
    InMemoryKernelFacade, MemoryAccess, MemoryEntryInfo, SessionAccess, SessionDetail, SessionInfo,
    SkillAccess, SkillInfo, ToolInfo, ToolRegistryAccess, TtsProviderInfo, VoiceAccess,
    VoiceSettingsInfo, VoiceSettingsUpdate, VoiceStatusInfo, auth::MemoryTokenValidator,
    broadcaster::TopicBroadcaster, build_router,
};
use tower::ServiceExt;

// ─── Stub access impls ──────────────────────────────────────────────────

struct StubTools;
impl ToolRegistryAccess for StubTools {
    fn list_tools(&self) -> Vec<ToolInfo> {
        vec![]
    }
    fn tool_schema(&self, _: &str) -> Option<serde_json::Value> {
        None
    }
}

struct StubSessions;
impl SessionAccess for StubSessions {
    fn list_sessions(&self) -> Vec<SessionInfo> {
        vec![]
    }
    fn get_session(&self, _: &str) -> Option<SessionDetail> {
        None
    }
    fn delete_session(&self, _: &str) -> bool {
        false
    }
}

struct StubAgents;
impl AgentAccess for StubAgents {
    fn list_agents(&self) -> Vec<AgentInfo> {
        vec![]
    }
    fn get_agent(&self, _: &str) -> Option<AgentInfo> {
        None
    }
}

struct StubBus;
impl BusAccess for StubBus {
    fn send_message(&self, _: &str, _: &str, _: &str) {}
}

struct StubSkills;
impl SkillAccess for StubSkills {
    fn list_skills(&self) -> Vec<SkillInfo> {
        vec![]
    }
    fn install_skill(&self, _: &str) -> Result<(), String> {
        Ok(())
    }
    fn uninstall_skill(&self, _: &str) -> Result<(), String> {
        Ok(())
    }
}

struct StubMemory;
impl MemoryAccess for StubMemory {
    fn list_entries(&self) -> Vec<MemoryEntryInfo> {
        vec![]
    }
    fn search(&self, _: &str, _: usize) -> Vec<MemoryEntryInfo> {
        vec![]
    }
    fn store(&self, _: &str, _: &str, _: &str, _: &[String]) -> Result<MemoryEntryInfo, String> {
        Err("stub".into())
    }
    fn delete(&self, _: &str) -> bool {
        false
    }
}

struct StubConfig;
impl ConfigAccess for StubConfig {
    fn get_config(&self) -> serde_json::Value {
        serde_json::json!({})
    }
    fn save_config(&self, _: serde_json::Value) -> Result<(), String> {
        Ok(())
    }
}

struct StubChannels;
impl ChannelAccess for StubChannels {
    fn list_channels(&self) -> Vec<ChannelStatusInfo> {
        vec![]
    }
}

struct StubVoice;
impl VoiceAccess for StubVoice {
    fn get_status(&self) -> VoiceStatusInfo {
        VoiceStatusInfo {
            state: "idle".into(),
            talk_mode_active: false,
            wake_word_enabled: false,
        }
    }
    fn get_settings(&self) -> VoiceSettingsInfo {
        VoiceSettingsInfo {
            enabled: false,
            wake_word_enabled: false,
            language: "en".into(),
            echo_cancel: false,
            noise_suppression: false,
            push_to_talk: false,
        }
    }
    fn update_settings(&self, _: VoiceSettingsUpdate) -> Result<(), String> {
        Ok(())
    }
    fn get_tts_config(&self) -> TtsProviderInfo {
        TtsProviderInfo {
            provider: "browser".into(),
            model: "default".into(),
            voice: "default".into(),
            speed: 1.0,
            api_key: String::new(),
            api_base: None,
        }
    }
}

fn make_state() -> (ApiState, Arc<MemoryTokenValidator>) {
    let auth = Arc::new(MemoryTokenValidator::new());
    let state = ApiState {
        tools: Arc::new(StubTools),
        sessions: Arc::new(StubSessions),
        agents: Arc::new(StubAgents),
        bus: Arc::new(StubBus),
        auth: auth.clone(),
        skills: Arc::new(StubSkills),
        memory: Arc::new(StubMemory),
        config: Arc::new(StubConfig),
        channels: Arc::new(StubChannels),
        voice: Arc::new(StubVoice),
        broadcaster: Arc::new(TopicBroadcaster::new()),
        kernel_facade: Arc::new(InMemoryKernelFacade::new()),
        routing_history: Arc::new(
            clawft_core::pipeline::decision_history::RoutingDecisionHistory::new(),
        ),
        rate_limiter: Arc::new(clawft_core::pipeline::rate_limiter::RateLimiter::new(60, 100)),
        health_cache: Default::default(),
        mcp: None,
    };
    (state, auth)
}

// ─── Helpers ────────────────────────────────────────────────────────────

use clawft_core::pipeline::decision_history::{CallOutcome, RoutingDecisionHistory};
use clawft_core::pipeline::traits::RoutingDecision;
use http_body_util::BodyExt;

fn dec(provider: &str, model: &str, tier: Option<&str>, sender: &str, cost: f64) -> RoutingDecision {
    RoutingDecision {
        provider: provider.into(),
        model: model.into(),
        reason: "test".into(),
        tier: tier.map(str::to_string),
        cost_estimate_usd: Some(cost),
        sender_id: Some(sender.into()),
        ..Default::default()
    }
}

fn finish(h: &RoutingDecisionHistory, d: &RoutingDecision, ok: bool, i: u64, o: u64, ms: u64) -> u64 {
    let id = h.record_with_channel(d, None);
    h.record_outcome(
        id,
        CallOutcome {
            ok,
            input_tokens: ok.then_some(i),
            output_tokens: ok.then_some(o),
            latency_ms: ms,
        },
    );
    id
}

struct Harness {
    app: axum::Router,
    token: String,
    history: Arc<RoutingDecisionHistory>,
}

fn harness_with(history: RoutingDecisionHistory) -> Harness {
    let (mut state, auth) = make_state();
    let history = Arc::new(history);
    state.routing_history = history.clone();
    let token = auth.generate_token(3600).unwrap();
    Harness {
        app: build_router(state, &[], None),
        token,
        history,
    }
}

fn harness() -> Harness {
    harness_with(RoutingDecisionHistory::new())
}

impl Harness {
    async fn get(&self, uri: &str) -> serde_json::Value {
        let req = Request::builder()
            .method(Method::GET)
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {}", self.token))
            .body(Body::empty())
            .unwrap();
        let resp = self.app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "GET {uri}");
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }
}

fn keys(v: &serde_json::Value) -> Vec<String> {
    let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
    k.sort();
    k
}

// ─── Empty state ────────────────────────────────────────────────────────

#[tokio::test]
async fn empty_collector_reports_zeros_not_fixtures() {
    let h = harness();

    let usage = h.get("/api/monitoring/token-usage").await;
    assert_eq!(usage["total_input"], 0);
    assert_eq!(usage["total_output"], 0);
    assert_eq!(usage["total_requests"], 0);
    assert_eq!(usage["by_provider"], serde_json::json!([]));
    assert_eq!(usage["by_session"], serde_json::json!([]));

    let costs = h.get("/api/monitoring/costs").await;
    assert_eq!(costs["total_cost_usd"], 0.0);
    assert_eq!(costs["by_provider"], serde_json::json!([]));
    assert_eq!(costs["by_tier"], serde_json::json!([]));

    let runs = h.get("/api/monitoring/pipeline-runs").await;
    assert_eq!(runs, serde_json::json!([]));
}

// ─── Counters ───────────────────────────────────────────────────────────

#[tokio::test]
async fn counters_increment_per_call_and_aggregate_by_provider_and_principal() {
    let h = harness();
    let hist = &h.history;

    finish(hist, &dec("openai", "m1", Some("haiku"), "alice", 0.25), true, 100, 20, 10);
    let u = h.get("/api/monitoring/token-usage").await;
    assert_eq!((u["total_input"].as_u64(), u["total_output"].as_u64(), u["total_requests"].as_u64()), (Some(100), Some(20), Some(1)));

    finish(hist, &dec("openai", "m1", Some("haiku"), "bob", 0.25), true, 50, 5, 20);
    finish(hist, &dec("anthropic", "m2", Some("sonnet"), "alice", 1.0), true, 7, 3, 30);
    let u = h.get("/api/monitoring/token-usage").await;
    assert_eq!((u["total_input"].as_u64(), u["total_output"].as_u64(), u["total_requests"].as_u64()), (Some(157), Some(28), Some(3)));

    let rows = u["by_provider"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    let openai = rows.iter().find(|r| r["provider"] == "openai").unwrap();
    assert_eq!(openai["input_tokens"], 150);
    assert_eq!(openai["output_tokens"], 25);
    assert_eq!(openai["total_tokens"], 175);
    assert_eq!(openai["request_count"], 2);

    let sessions = u["by_session"].as_array().unwrap();
    let alice = sessions.iter().find(|s| s["session_key"] == "alice").unwrap();
    assert_eq!((alice["input_tokens"].as_u64(), alice["output_tokens"].as_u64(), alice["request_count"].as_u64()), (Some(107), Some(23), Some(2)));

    let c = h.get("/api/monitoring/costs").await;
    assert!((c["total_cost_usd"].as_f64().unwrap() - 1.5).abs() < 1e-9);
    let tiers = c["by_tier"].as_array().unwrap();
    assert_eq!(tiers.len(), 2);
    assert_eq!((tiers[0]["label"].as_str(), tiers[0]["tier"].as_u64(), tiers[0]["request_count"].as_u64()), (Some("haiku"), Some(2), Some(2)));
    assert_eq!((tiers[1]["label"].as_str(), tiers[1]["tier"].as_u64()), (Some("sonnet"), Some(3)));
}

#[tokio::test]
async fn failed_call_adds_a_run_but_no_tokens_and_unfinished_is_not_a_run() {
    let h = harness();
    finish(&h.history, &dec("openai", "m", None, "alice", 0.0), false, 0, 0, 9);
    h.history.record_with_channel(&dec("openai", "m", None, "bob", 0.0), None); // in flight

    let u = h.get("/api/monitoring/token-usage").await;
    assert_eq!((u["total_input"].as_u64(), u["total_output"].as_u64()), (Some(0), Some(0)));
    let runs = h.get("/api/monitoring/pipeline-runs").await;
    let runs = runs.as_array().unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["status"], "error");
    assert_eq!(runs[0]["latency_ms"], 9);
    assert_eq!(runs[0]["session_key"], "alice");
}

#[tokio::test]
async fn outcome_for_evicted_entry_is_a_noop_and_does_not_corrupt_neighbours() {
    let h = harness_with(RoutingDecisionHistory::with_capacity(2));
    let a = h.history.record_with_channel(&dec("p", "m", None, "a", 0.0), None);
    finish(&h.history, &dec("p", "m", None, "b", 0.0), true, 1, 1, 1);
    finish(&h.history, &dec("p", "m", None, "c", 0.0), true, 2, 2, 2); // evicts `a`
    h.history.record_outcome(a, CallOutcome { ok: true, input_tokens: Some(999), output_tokens: Some(999), latency_ms: 999 });
    let u = h.get("/api/monitoring/token-usage").await;
    assert_eq!((u["total_input"].as_u64(), u["total_requests"].as_u64()), (Some(3), Some(2)));
}

// ─── Concurrent updates ─────────────────────────────────────────────────

#[test]
fn concurrent_recorders_lose_no_updates() {
    let h = Arc::new(RoutingDecisionHistory::new());
    let threads: Vec<_> = (0..8)
        .map(|t| {
            let h = h.clone();
            std::thread::spawn(move || {
                for _ in 0..50 {
                    finish(&h, &dec("openai", "m", Some("haiku"), &format!("u{t}"), 0.5), true, 10, 5, 3);
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    assert_eq!(h.len(), 400);
    let entries = h.recent(h.capacity());
    let mut ids: Vec<u64> = entries.iter().map(|e| e.id).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 400, "ids are ring-unique under contention");
    assert!(entries.iter().all(|e| e.ok == Some(true) && e.input_tokens == Some(10)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn endpoint_reads_are_consistent_while_writers_run() {
    let h = harness();
    let hist = h.history.clone();
    let writers: Vec<_> = (0..4)
        .map(|t| {
            let hist = hist.clone();
            std::thread::spawn(move || {
                for _ in 0..100 {
                    finish(&hist, &dec("openai", "m", Some("haiku"), &format!("u{t}"), 0.1), true, 10, 5, 1);
                }
            })
        })
        .collect();

    let mut last = 0;
    for _ in 0..50 {
        let u = h.get("/api/monitoring/token-usage").await;
        let (i, o) = (u["total_input"].as_u64().unwrap(), u["total_output"].as_u64().unwrap());
        // Tokens attach atomically to an entry, so every snapshot has the 2:1 ratio.
        assert_eq!(i, o * 2, "torn snapshot: in={i} out={o}");
        let n = u["total_requests"].as_u64().unwrap();
        assert!(n >= last, "request count went backwards");
        last = n;
        tokio::task::yield_now().await;
    }
    for w in writers {
        w.join().unwrap();
    }
    let u = h.get("/api/monitoring/token-usage").await;
    assert_eq!(u["total_requests"], 400);
    assert_eq!(u["total_input"], 4000);
    assert_eq!(u["total_output"], 2000);
    let runs = h.get("/api/monitoring/pipeline-runs").await;
    assert_eq!(runs.as_array().unwrap().len(), 200, "runs page is capped at 200");
}

// ─── Serialized format ──────────────────────────────────────────────────

#[tokio::test]
async fn wire_format_field_names_and_types_are_stable() {
    let h = harness();
    finish(&h.history, &dec("openai", "m", Some("sonnet"), "alice", 0.5), true, 10, 4, 77);
    finish(&h.history, &dec("openai", "m", Some("sonnet"), "alice", 0.5), false, 0, 0, 5);

    let u = h.get("/api/monitoring/token-usage").await;
    assert_eq!(keys(&u), ["by_provider", "by_session", "total_input", "total_output", "total_requests"]);
    assert_eq!(keys(&u["by_provider"][0]), ["input_tokens", "model", "output_tokens", "provider", "request_count", "total_tokens"]);
    assert_eq!(keys(&u["by_session"][0]), ["input_tokens", "output_tokens", "request_count", "session_key"]);

    let c = h.get("/api/monitoring/costs").await;
    assert_eq!(keys(&c), ["by_provider", "by_tier", "total_cost_usd"]);
    assert_eq!(keys(&c["by_provider"][0]), ["input_cost_usd", "model", "output_cost_usd", "provider", "total_cost_usd"]);
    assert_eq!(keys(&c["by_tier"][0]), ["label", "request_count", "tier", "total_cost_usd"]);
    assert!(c["total_cost_usd"].is_f64());

    let r = h.get("/api/monitoring/pipeline-runs").await;
    let r = r.as_array().unwrap();
    assert_eq!(r.len(), 2);
    assert_eq!(keys(&r[0]), ["complexity", "id", "latency_ms", "model", "session_key", "status", "timestamp"]);
    let statuses: Vec<&str> = r.iter().map(|x| x["status"].as_str().unwrap()).collect();
    assert!(statuses.contains(&"success") && statuses.contains(&"error"), "lowercase statuses: {statuses:?}");
    assert!(r[0]["latency_ms"].is_u64() && r[0]["timestamp"].as_str().unwrap().contains('T'));
}

// ─── Bugs found while closing gaps ──────────────────────────────────────

/// Run ids were `route-<ts>-<index in this response>`, so the same run got a
/// new id every time a newer run arrived, and the UI could not key on it.
#[tokio::test]
async fn pipeline_run_ids_do_not_change_when_newer_runs_arrive() {
    let h = harness();
    finish(&h.history, &dec("p", "m", None, "a", 0.0), true, 1, 1, 1);
    finish(&h.history, &dec("p", "m", None, "b", 0.0), true, 1, 1, 1);
    let before = h.get("/api/monitoring/pipeline-runs").await;
    let id_of = |v: &serde_json::Value, who: &str| {
        v.as_array().unwrap().iter().find(|r| r["session_key"] == who).unwrap()["id"].as_str().unwrap().to_string()
    };
    let (a0, b0) = (id_of(&before, "a"), id_of(&before, "b"));

    finish(&h.history, &dec("p", "m", None, "c", 0.0), true, 1, 1, 1);
    let after = h.get("/api/monitoring/pipeline-runs").await;
    assert_eq!(id_of(&after, "a"), a0);
    assert_eq!(id_of(&after, "b"), b0);
    let mut ids: Vec<String> = after.as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap().to_string()).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 3, "ids are unique");
}

/// The handler took the newest 200 entries and only then dropped the
/// unfinished ones, so a burst of in-flight calls hid every finished run.
#[tokio::test]
async fn in_flight_burst_does_not_hide_finished_runs() {
    let h = harness();
    for i in 0..5 {
        finish(&h.history, &dec("p", "m", None, &format!("done{i}"), 0.0), true, 1, 1, 1);
    }
    for i in 0..250 {
        h.history.record_with_channel(&dec("p", "m", None, &format!("busy{i}"), 0.0), None);
    }
    let runs = h.get("/api/monitoring/pipeline-runs").await;
    assert_eq!(runs.as_array().unwrap().len(), 5);
}

/// Rows came out in HashMap order: a different order on every request, so
/// the UI tables reshuffled on each poll.
#[tokio::test]
async fn aggregate_rows_have_a_stable_sorted_order() {
    let h = harness();
    for (p, m, tier, who) in [
        ("zed", "m1", "zeta", "u9"),
        ("alpha", "m2", "alpha", "u1"),
        ("mid", "m1", "mid", "u5"),
        ("alpha", "m1", "beta", "u3"),
        ("beta", "m3", "gamma", "u7"),
        ("kappa", "m1", "delta", "u2"),
        ("omega", "m1", "epsilon", "u8"),
        ("beta", "m1", "eta", "u4"),
    ] {
        finish(&h.history, &dec(p, m, Some(tier), who, 0.1), true, 1, 1, 1);
    }
    let u = h.get("/api/monitoring/token-usage").await;
    let provs: Vec<(String, String)> = u["by_provider"].as_array().unwrap().iter()
        .map(|r| (r["provider"].as_str().unwrap().into(), r["model"].as_str().unwrap().into())).collect();
    let mut sorted = provs.clone();
    sorted.sort();
    assert_eq!(provs, sorted, "by_provider sorted by (provider, model)");

    let sess: Vec<String> = u["by_session"].as_array().unwrap().iter().map(|r| r["session_key"].as_str().unwrap().into()).collect();
    let mut sorted = sess.clone();
    sorted.sort();
    assert_eq!(sess, sorted, "by_session sorted by key");

    let c = h.get("/api/monitoring/costs").await;
    let cprovs: Vec<(String, String)> = c["by_provider"].as_array().unwrap().iter()
        .map(|r| (r["provider"].as_str().unwrap().into(), r["model"].as_str().unwrap().into())).collect();
    let mut sorted = cprovs.clone();
    sorted.sort();
    assert_eq!(cprovs, sorted, "cost by_provider sorted");

    // Unknown tier names all map to 0; ties must break by label.
    let labels: Vec<String> = c["by_tier"].as_array().unwrap().iter().map(|r| r["label"].as_str().unwrap().into()).collect();
    let mut sorted = labels.clone();
    sorted.sort();
    assert_eq!(labels, sorted, "by_tier sorted by (tier, label)");
}

/// `with_capacity(0)` is documented as unbounded, but every handler asked for
/// `capacity()` rows, i.e. zero, so an unbounded ring always read as empty.
#[tokio::test]
async fn unbounded_ring_is_not_reported_as_empty() {
    let h = harness_with(RoutingDecisionHistory::with_capacity(0));
    finish(&h.history, &dec("p", "m", Some("haiku"), "a", 0.5), true, 10, 4, 7);
    let u = h.get("/api/monitoring/token-usage").await;
    assert_eq!((u["total_input"].as_u64(), u["total_requests"].as_u64()), (Some(10), Some(1)));
    let c = h.get("/api/monitoring/costs").await;
    assert!((c["total_cost_usd"].as_f64().unwrap() - 0.5).abs() < 1e-9);
    let r = h.get("/api/monitoring/pipeline-runs").await;
    assert_eq!(r.as_array().unwrap().len(), 1);
    assert_eq!(h.history.recent(10).len(), 1);
    assert_eq!(h.history.query(Default::default()).len(), 1, "query() default limit");
}
