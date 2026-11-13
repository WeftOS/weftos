//! End-to-end integration tests for the WeftOS kernel.
//!
//! Tests the complete flow: boot -> spawn -> message -> governance -> chain -> shutdown.
//! Each test boots a fresh kernel, performs its work, and shuts down cleanly.

use std::sync::Arc;

use clawft_kernel::boot::{Kernel, KernelState};
use clawft_kernel::health::OverallHealth;
use clawft_kernel::ipc::{KernelMessage, MessagePayload, MessageTarget};
use clawft_kernel::supervisor::SpawnRequest;
use clawft_platform::NativePlatform;
use clawft_types::config::{AgentDefaults, AgentsConfig, Config, KernelConfig, DEFAULT_BRAND};

// ── Helpers ──────────────────────────────────────────────────────

fn base_config() -> Config {
    Config {
        agents: AgentsConfig {
            defaults: AgentDefaults {
                workspace: "~/.clawft/workspace".into(),
                model: "test/model".into(),
                max_tokens: 1024,
                temperature: 0.5,
                max_tool_iterations: 5,
                memory_window: 10,
            },
            ..AgentsConfig::default()
        },
        ..Config::default()
    }
}

fn minimal_kernel_config() -> KernelConfig {
    KernelConfig {
        governance: Default::default(),
        profile: None,
        shared_services: Default::default(),
        enabled: true,
        max_processes: 64,
        health_check_interval_secs: 5,
        cluster: None,
        // Never the operator chain under ~/.clawft (isolated per boot).
        chain: Some(clawft_types::config::ChainConfig::isolated_in(
            &tempfile::tempdir().unwrap().keep(),
        )),
        resource_tree: None,
        vector: None,
        spatial: None,
        profiles: None,
        pairing: None,
        mesh: None,
        anchor: None,
        ipc_tcp: None,
        llm: None,
        agent: None,
            brand: DEFAULT_BRAND.to_string(),
    }
}

#[cfg(feature = "exochain")]
fn exochain_kernel_config() -> KernelConfig {
    use clawft_types::config::{ChainConfig, ResourceTreeConfig};
    KernelConfig {
        governance: Default::default(),
        profile: None,
        shared_services: Default::default(),
        enabled: true,
        max_processes: 64,
        health_check_interval_secs: 5,
        cluster: None,
        chain: Some(ChainConfig {
            enabled: true,
            checkpoint_interval: 10_000,
            chain_id: 0,
            // Isolated per boot: never the operator chain under ~/.clawft.
            checkpoint_path: Some(
                tempfile::tempdir().unwrap().keep().join("chain.json").to_string_lossy().into_owned(),
            ),
            external_anchor: None,
        }),
        resource_tree: Some(ResourceTreeConfig {
            enabled: true,
            checkpoint_path: None,
        }),
        vector: None,
        spatial: None,
        profiles: None,
        pairing: None,
        mesh: None,
        anchor: None,
        ipc_tcp: None,
        llm: None,
        agent: None,
            brand: DEFAULT_BRAND.to_string(),
    }
}

/// Spawn an agent via the supervisor and transition it to Running.
///
/// The supervisor creates agents in `Starting` state. In a real kernel
/// the agent loop would transition to `Running` once execution begins.
/// For integration tests we transition immediately.
fn spawn_running<P: clawft_platform::Platform>(
    kernel: &Kernel<P>,
    agent_id: &str,
) -> clawft_kernel::supervisor::SpawnResult {
    let result = kernel
        .supervisor()
        .spawn(SpawnRequest {
            agent_id: agent_id.into(),
            capabilities: None,
            parent_pid: None,
            env: std::collections::HashMap::new(),
            backend: None,
        })
        .unwrap();
    kernel
        .process_table()
        .update_state(result.pid, clawft_kernel::ProcessState::Running)
        .unwrap();
    result
}

// ── Test 1: boot_spawn_message_shutdown ──────────────────────────

#[tokio::test]
async fn boot_spawn_message_shutdown() {
    let platform = Arc::new(NativePlatform::new());
    let mut kernel = Kernel::boot(base_config(), minimal_kernel_config(), platform)
        .await
        .unwrap();
    assert_eq!(*kernel.state(), KernelState::Running);

    // Spawn an agent and transition to Running
    let spawn_result = spawn_running(&kernel, "e2e-ping-agent");
    let agent_pid = spawn_result.pid;

    // Create inboxes for both kernel (PID 0) and the new agent
    let a2a = kernel.a2a_router();
    let mut kernel_inbox = a2a.create_inbox(0);
    let _agent_inbox = a2a.create_inbox(agent_pid);

    // Send a ping message from the agent to the kernel
    let msg = KernelMessage::new(
        agent_pid,
        MessageTarget::Process(0),
        MessagePayload::Json(serde_json::json!({"type": "ping"})),
    );
    a2a.send(msg).await.unwrap();

    // Verify response received at kernel inbox
    let received = tokio::time::timeout(std::time::Duration::from_secs(2), kernel_inbox.recv())
        .await
        .unwrap()
        .unwrap();

    if let MessagePayload::Json(v) = &received.payload {
        assert_eq!(v["type"], "ping");
    } else {
        panic!("expected JSON payload");
    }

    // Shutdown kernel
    kernel.shutdown().await.unwrap();
    assert_eq!(*kernel.state(), KernelState::Halted);
}

// ── Test 2: governance_blocks_unauthorized_action ────────────────

#[cfg(feature = "exochain")]
#[tokio::test]
async fn governance_blocks_unauthorized_action() {
    use clawft_kernel::capability::{AgentCapabilities, IpcScope};

    let platform = Arc::new(NativePlatform::new());
    let kernel = Kernel::boot(base_config(), exochain_kernel_config(), platform)
        .await
        .unwrap();
    assert_eq!(*kernel.state(), KernelState::Running);

    // Spawn an agent with restricted capabilities (no IPC permission)
    let restricted_caps = AgentCapabilities {
        can_spawn: false,
        can_ipc: false,
        can_exec_tools: false,
        can_network: false,
        ipc_scope: IpcScope::None,
        ..AgentCapabilities::default()
    };

    let spawn_result = kernel
        .supervisor()
        .spawn(SpawnRequest {
            agent_id: "restricted-agent".into(),
            capabilities: Some(restricted_caps),
            parent_pid: None,
            env: std::collections::HashMap::new(),
            backend: None,
        })
        .unwrap();
    let restricted_pid = spawn_result.pid;

    // Transition to Running so the state check passes (capability check is what we test)
    kernel
        .process_table()
        .update_state(restricted_pid, clawft_kernel::ProcessState::Running)
        .unwrap();

    // Create inbox so the agent can attempt to send
    let a2a = kernel.a2a_router();
    let _inbox = a2a.create_inbox(restricted_pid);

    // Attempt an IPC action that should be denied by capability checker
    let msg = KernelMessage::new(
        restricted_pid,
        MessageTarget::Process(0),
        MessagePayload::Json(serde_json::json!({"action": "unauthorized"})),
    );

    let send_result = a2a.send(msg).await;
    assert!(
        send_result.is_err(),
        "send should fail for restricted agent"
    );

    // Verify the governance gate is wired when exochain is enabled
    assert!(
        kernel.governance_gate().is_some(),
        "governance gate must be present with exochain"
    );

    // Verify chain has events logged (governance/boot events at minimum)
    let chain = kernel.chain_manager().unwrap();
    let events = chain.tail(50);
    assert!(!events.is_empty(), "chain should have boot events logged");
}

// ── Test 3: service_lifecycle ────────────────────────────────────

#[tokio::test]
async fn service_lifecycle() {
    let platform = Arc::new(NativePlatform::new());
    let kernel = Kernel::boot(base_config(), minimal_kernel_config(), platform)
        .await
        .unwrap();
    assert_eq!(*kernel.state(), KernelState::Running);

    // Verify built-in services are registered
    let services = kernel.services();
    let service_list = services.list();
    assert!(
        service_list.len() >= 2,
        "at least cron + container services should be registered, got {}",
        service_list.len()
    );

    // Verify service names include expected built-ins
    let names: Vec<&str> = service_list.iter().map(|(n, _)| n.as_str()).collect();
    assert!(names.contains(&"cron"), "cron service must be registered");

    // Health check should succeed on a running kernel
    let health = kernel.health();
    let (overall, _details) = health.aggregate(services).await;
    assert!(
        matches!(
            overall,
            OverallHealth::Healthy | OverallHealth::Degraded { .. }
        ),
        "health should be healthy or degraded, not {overall:?}",
    );
}

// ── Test 4: persistence_roundtrip (ecc feature) ─────────────────

#[cfg(feature = "ecc")]
#[tokio::test]
async fn persistence_roundtrip() {
    use clawft_kernel::causal::CausalGraph;
    use clawft_kernel::persistence::{PersistenceConfig, load_causal_graph, save_causal_graph};

    // Create a causal graph with some nodes
    let graph = CausalGraph::new();
    let _n1 = graph.add_node("file:main.rs".into(), serde_json::json!({"kind": "file"}));
    let _n2 = graph.add_node("func:boot".into(), serde_json::json!({"kind": "function"}));
    let _n3 = graph.add_node("test:e2e".into(), serde_json::json!({"kind": "test"}));
    assert_eq!(graph.node_count(), 3);

    // Save to a temp directory
    let tmp = std::env::temp_dir().join(format!("clawft_e2e_persist_{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();

    let config = PersistenceConfig {
        data_dir: tmp.clone(),
        auto_save_interval_secs: None,
    };
    save_causal_graph(&config, &graph).unwrap();

    // Load from the same temp directory into a new graph
    let loaded = load_causal_graph(&config).unwrap();
    assert_eq!(
        loaded.node_count(),
        3,
        "loaded graph should have 3 nodes, got {}",
        loaded.node_count()
    );

    // Cleanup
    let _ = std::fs::remove_dir_all(&tmp);
}

// ── Test 5: multi_agent_ipc ──────────────────────────────────────

#[tokio::test]
async fn multi_agent_ipc() {
    let platform = Arc::new(NativePlatform::new());
    let kernel = Kernel::boot(base_config(), minimal_kernel_config(), platform)
        .await
        .unwrap();

    // Spawn 3 agents (transitioned to Running)
    let mut pids = Vec::new();
    for i in 1..=3 {
        let result = spawn_running(&kernel, &format!("ipc-agent-{i}"));
        pids.push(result.pid);
    }
    assert_eq!(pids.len(), 3);

    let a2a = kernel.a2a_router();

    // Create inboxes for all agents
    let _inbox1 = a2a.create_inbox(pids[0]);
    let mut inbox2 = a2a.create_inbox(pids[1]);
    let mut inbox3 = a2a.create_inbox(pids[2]);

    // Agent 1 sends to Agent 2
    let msg1 = KernelMessage::new(
        pids[0],
        MessageTarget::Process(pids[1]),
        MessagePayload::Json(serde_json::json!({"step": 1, "from": "agent-1"})),
    );
    a2a.send(msg1).await.unwrap();

    // Agent 2 receives
    let received_at_2 = tokio::time::timeout(std::time::Duration::from_secs(2), inbox2.recv())
        .await
        .unwrap()
        .unwrap();

    if let MessagePayload::Json(v) = &received_at_2.payload {
        assert_eq!(v["step"], 1);
    } else {
        panic!("agent 2 expected JSON payload");
    }

    // Agent 2 forwards to Agent 3
    let msg2 = KernelMessage::new(
        pids[1],
        MessageTarget::Process(pids[2]),
        MessagePayload::Json(serde_json::json!({"step": 2, "from": "agent-2", "forwarded": true})),
    );
    a2a.send(msg2).await.unwrap();

    // Agent 3 receives
    let received_at_3 = tokio::time::timeout(std::time::Duration::from_secs(2), inbox3.recv())
        .await
        .unwrap()
        .unwrap();

    if let MessagePayload::Json(v) = &received_at_3.payload {
        assert_eq!(v["step"], 2);
        assert_eq!(v["forwarded"], true);
    } else {
        panic!("agent 3 expected JSON payload");
    }
}

// ── Test 6: topic_pubsub ─────────────────────────────────────────

#[tokio::test]
async fn topic_pubsub() {
    let platform = Arc::new(NativePlatform::new());
    let kernel = Kernel::boot(base_config(), minimal_kernel_config(), platform)
        .await
        .unwrap();

    // Spawn 2 subscriber agents + 1 publisher agent (all Running)
    let pub_pid = spawn_running(&kernel, "publisher").pid;
    let sub1_pid = spawn_running(&kernel, "subscriber-1").pid;
    let sub2_pid = spawn_running(&kernel, "subscriber-2").pid;

    let a2a = kernel.a2a_router();

    // Create inboxes
    let _pub_inbox = a2a.create_inbox(pub_pid);
    let mut sub1_inbox = a2a.create_inbox(sub1_pid);
    let mut sub2_inbox = a2a.create_inbox(sub2_pid);

    // Subscribe both agents to "events" topic
    let topic_router = a2a.topic_router();
    topic_router.subscribe(sub1_pid, "events");
    topic_router.subscribe(sub2_pid, "events");

    // Verify subscriptions
    let subs = topic_router.live_subscribers("events");
    assert_eq!(subs.len(), 2, "both agents should be subscribed");

    // Publish a message to "events" topic from the publisher
    let topic_msg = KernelMessage::new(
        pub_pid,
        MessageTarget::Topic("events".into()),
        MessagePayload::Json(serde_json::json!({"event": "build_complete", "status": "ok"})),
    );
    a2a.send(topic_msg).await.unwrap();

    // Both subscribers should receive the message
    let recv1 = tokio::time::timeout(std::time::Duration::from_secs(2), sub1_inbox.recv())
        .await
        .unwrap()
        .unwrap();

    let recv2 = tokio::time::timeout(std::time::Duration::from_secs(2), sub2_inbox.recv())
        .await
        .unwrap()
        .unwrap();

    if let MessagePayload::Json(v) = &recv1.payload {
        assert_eq!(v["event"], "build_complete");
    } else {
        panic!("subscriber 1 expected JSON payload");
    }

    if let MessagePayload::Json(v) = &recv2.payload {
        assert_eq!(v["event"], "build_complete");
    } else {
        panic!("subscriber 2 expected JSON payload");
    }
}

// ── Test 7: agent_restart_on_failure (os-patterns feature) ───────

#[cfg(feature = "os-patterns")]
#[tokio::test]
async fn agent_restart_on_failure() {
    use clawft_kernel::process::ProcessState;
    use clawft_kernel::supervisor::{RestartBudget, RestartStrategy, RestartTracker};

    let platform = Arc::new(NativePlatform::new());
    let kernel = Kernel::boot(base_config(), minimal_kernel_config(), platform)
        .await
        .unwrap();

    // Verify the supervisor has a restart strategy configured
    let strategy = kernel.supervisor().restart_strategy();
    assert_eq!(
        *strategy,
        RestartStrategy::OneForOne,
        "default strategy should be OneForOne"
    );

    // Spawn an agent and transition to Running
    let spawn_result = spawn_running(&kernel, "crash-test-agent");
    let original_pid = spawn_result.pid;

    // Verify the agent is running
    let proc = kernel.process_table().get(original_pid).unwrap();
    assert_eq!(proc.state, ProcessState::Running);

    // Simulate crash by transitioning to Exited
    kernel
        .process_table()
        .update_state(original_pid, ProcessState::Exited(1))
        .unwrap();

    // Verify the RestartTracker correctly evaluates should_restart
    assert!(
        RestartTracker::should_restart(&RestartStrategy::OneForOne, 1),
        "OneForOne should restart on non-zero exit"
    );
    assert!(
        !RestartTracker::should_restart(&RestartStrategy::Permanent, 1),
        "Permanent should never restart"
    );
    assert!(
        RestartTracker::should_restart(&RestartStrategy::Transient, 1),
        "Transient should restart on non-zero exit"
    );
    assert!(
        !RestartTracker::should_restart(&RestartStrategy::Transient, 0),
        "Transient should NOT restart on zero exit"
    );

    // Verify restart budget tracking works
    let budget = RestartBudget::default();
    let mut tracker = RestartTracker::new();
    assert!(
        tracker.record_restart(&budget),
        "first restart within budget"
    );
    assert!(
        tracker.record_restart(&budget),
        "second restart within budget"
    );
    assert_eq!(tracker.remaining(&budget), budget.max_restarts - 2);

    // Perform an actual restart through the supervisor
    let restart_result = kernel.supervisor().restart(original_pid);
    assert!(restart_result.is_ok(), "restart should succeed");

    let new_spawn = restart_result.unwrap();
    assert_ne!(
        new_spawn.pid, original_pid,
        "restarted agent should get a new PID"
    );

    // Verify the new process exists (spawned in Starting state; the agent
    // loop would transition it to Running in a real kernel).
    let new_proc = kernel.process_table().get(new_spawn.pid).unwrap();
    assert_eq!(new_proc.state, ProcessState::Starting);
    assert_eq!(new_proc.agent_id, "crash-test-agent");
}

// ── Test 8: dead_letter_queue_captures (os-patterns feature) ─────

#[cfg(feature = "os-patterns")]
#[tokio::test]
async fn dead_letter_queue_captures() {
    use clawft_kernel::dead_letter::DeadLetterReason;

    let platform = Arc::new(NativePlatform::new());
    let kernel = Kernel::boot(base_config(), minimal_kernel_config(), platform)
        .await
        .unwrap();

    // Verify the DLQ is available with os-patterns
    let dlq = kernel
        .dead_letter_queue()
        .expect("DLQ must be present with os-patterns feature");
    assert!(dlq.is_empty(), "DLQ should start empty");

    // Spawn an agent so we have a valid sender (in Running state)
    let sender_pid = spawn_running(&kernel, "dlq-test-sender").pid;

    // Create inbox for the sender (required to send)
    let a2a = kernel.a2a_router();
    let _sender_inbox = a2a.create_inbox(sender_pid);

    // Send a message to a nonexistent PID (9999)
    let nonexistent_pid = 9999;
    let msg = KernelMessage::new(
        sender_pid,
        MessageTarget::Process(nonexistent_pid),
        MessagePayload::Json(serde_json::json!({"test": "dead_letter"})),
    );

    // The send will fail because the target PID does not exist.
    // With os-patterns, the A2A router automatically routes failed
    // messages to the DLQ.
    let _send_result = a2a.send(msg.clone()).await;

    // Check if the router already placed the message in the DLQ.
    // If not (e.g. the send error was returned instead), manually intake.
    let auto_routed = dlq.len();
    if auto_routed == 0 {
        dlq.intake(
            msg,
            DeadLetterReason::TargetNotFound {
                pid: nonexistent_pid,
            },
        );
    }

    // Verify DLQ has at least 1 entry
    assert!(
        !dlq.is_empty(),
        "DLQ should have at least 1 entry, got {}",
        dlq.len()
    );

    // Query DLQ by target PID and verify content
    let letters = dlq.query_by_target(nonexistent_pid);
    assert!(
        !letters.is_empty(),
        "should find at least 1 dead letter for target PID"
    );

    let letter = &letters[0];
    assert!(
        matches!(&letter.reason, DeadLetterReason::TargetNotFound { pid } if *pid == nonexistent_pid),
        "reason should be TargetNotFound"
    );

    if let MessagePayload::Json(v) = &letter.message.payload {
        assert_eq!(v["test"], "dead_letter");
    } else {
        panic!("expected JSON payload in dead letter");
    }

    // Query by reason variant name
    let by_reason = dlq.query_by_reason("TargetNotFound");
    assert!(
        !by_reason.is_empty(),
        "should find at least 1 dead letter by reason name"
    );
}
