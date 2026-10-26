//! A throwaway broker plus two consumers for trying the `npu-infer` cog by
//! hand on a board, without the node daemon (ADR-122 section 12).
//!
//! ```text
//! NPU_TOKEN=<64 chars> cargo run -p weftos-cogbus --example npu_demo -- /tmp/cogbus.sock
//! # in another shell, as the cog:
//! WEFTOS_COGBUS=/tmp/cogbus.sock WEFTOS_COGBUS_TOKEN=<same> cog-npu-infer --state-dir ...
//! ```
//!
//! It registers the cog (`svc.infer` and `telemetry` outputs), two callers
//! (`consumer-a`, `consumer-b`) and a telemetry watcher, waits for the cog to
//! serve, then has both callers issue `CALLS` calls at once (models from
//! `MODEL_A` / `MODEL_B`), printing each answer's timings and every telemetry
//! reading. Not a test and not shipped.

use std::sync::Arc;
use std::time::{Duration, Instant};

use clawft_types::cog_topics::{InputPolicy, TopicClass};
use serde_json::json;
use weftos_cogbus::server::serve;
use weftos_cogbus::{
    Broker, BrokerConfig, Client, ClientError, ErrorCode, InputSpec, InstanceSpec, OutputSpec,
};

const PROJECT: &str = "01J9ZXW0PRJCTAAAAAAAAAAAAA";
const TOPIC: &str = "svc.infer/venq-npu";

fn out(topic: &str, schema: &str) -> OutputSpec {
    OutputSpec {
        topic: topic.into(),
        schema: schema.into(),
        class: TopicClass::Reading,
    }
}

fn inp(topic: &str, schema: &str) -> InputSpec {
    InputSpec {
        topic: topic.into(),
        schema: schema.into(),
        policy: InputPolicy::DropOldest,
    }
}

fn reg(b: &Broker, id: &str, token: &str, outputs: Vec<OutputSpec>, inputs: Vec<InputSpec>) {
    let spec = InstanceSpec {
        instance_id: id.into(),
        cog_id: id.into(),
        project_id: Some(PROJECT.into()),
        uid: None,
        outputs,
        inputs,
    };
    b.register(spec, token).expect("register");
}

fn env(k: &str, d: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| d.to_string())
}

#[tokio::main]
async fn main() {
    let sock = std::env::args().nth(1).expect("socket path");
    let cog_token = std::env::var("NPU_TOKEN").expect("NPU_TOKEN (64 chars)");
    let (ta, tb, tw) = ("a".repeat(64), "b".repeat(64), "c".repeat(64));
    let broker = Arc::new(Broker::new(BrokerConfig::new("0123456789abcdef0123456789abcdef")));
    reg(
        &broker,
        "npu-infer",
        &cog_token,
        vec![out("svc.infer", "infer.v1"), out("telemetry", "telemetry.v1")],
        vec![],
    );
    reg(&broker, "consumer-a", &ta, vec![], vec![inp("svc.infer", "infer.v1")]);
    reg(&broker, "consumer-b", &tb, vec![], vec![inp("svc.infer", "infer.v1")]);
    reg(&broker, "watcher", &tw, vec![], vec![inp("telemetry", "telemetry.v1")]);
    let _server = serve(broker.clone(), std::path::Path::new(&sock)).await.expect("serve");
    println!("broker ready on {sock}");

    let path = std::path::PathBuf::from(&sock);
    let watcher = Client::connect(&path, &tw).await.unwrap();
    let mut sub = watcher.subscribe("telemetry/*", "telemetry.v1").await.unwrap();
    let t0 = Instant::now();
    tokio::spawn(async move {
        while let Some(m) = sub.recv().await {
            let pick = |n: &str| {
                m.body["items"]
                    .as_array()
                    .and_then(|a| a.iter().find(|i| i["name"] == n))
                    .and_then(|i| i["value"].as_f64())
                    .unwrap_or(-1.0)
            };
            let loaded: Vec<String> = m.body["items"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|i| i["name"].as_str()?.strip_prefix("model.loaded.").map(|n| format!("{n}={}", i["value"])))
                .collect();
            println!(
                "[{:6.1}s] telemetry seq={} queue.depth={} npu.busy={} resident={} mem.resident={}MiB loaded=[{}] served={} evictions={} restarts={}",
                t0.elapsed().as_secs_f64(), m.body["seq"], pick("queue.depth"), pick("npu.busy"), pick("models.resident"),
                pick("mem.resident"), loaded.join(","), pick("calls.served"), pick("models.evictions"), pick("geniex.restarts"),
            );
        }
    });

    let a = Arc::new(Client::connect(&path, &ta).await.unwrap());
    let b = Arc::new(Client::connect(&path, &tb).await.unwrap());
    // wait for the cog: an unknown model is answered once it serves
    loop {
        let r = a
            .invoke(TOPIC, "infer.v1", json!({"model": "nope", "text": "x"}), Duration::from_secs(2))
            .await;
        match r {
            Err(ClientError::Refused { code: ErrorCode::Unavailable, .. }) => {
                tokio::time::sleep(Duration::from_millis(500)).await
            }
            Err(ClientError::Refused { code: ErrorCode::ServiceError, message }) => {
                println!("cog is serving; an unknown model is refused: {message}");
                break;
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    let calls: usize = env("CALLS", "3").parse().unwrap();
    let max_tokens: u32 = env("MAX_TOKENS", "32").parse().unwrap();
    let mut tasks = Vec::new();
    for (who, client, model, prio) in [
        ("consumer-a", a.clone(), env("MODEL_A", "gemma3-1b"), 5u8),
        ("consumer-b", b.clone(), env("MODEL_B", "qwen3.5-0.8b"), 4u8),
    ] {
        tasks.push(tokio::spawn(async move {
            for i in 0..calls {
                let started = Instant::now();
                let body = json!({"model": model, "priority": prio, "max_tokens": max_tokens,
                    "text": format!("In five words, say something about the number {i}.")});
                let r = client.invoke(TOPIC, "infer.v1", body, Duration::from_secs(120)).await;
                match r {
                    Ok(v) => println!(
                        "[{:6.1}s] {who} {model} #{i}: {} ms total (queue {} load {} run {} loaded={}) -> {:?}",
                        t0.elapsed().as_secs_f64(), started.elapsed().as_millis(), v["queue_ms"], v["load_ms"], v["run_ms"], v["loaded"],
                        v["text"].as_str().unwrap_or("").chars().take(60).collect::<String>(),
                    ),
                    Err(e) => println!("[{:6.1}s] {who} {model} #{i}: FAILED {e}", t0.elapsed().as_secs_f64()),
                }
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    // IMAGE_B64_FILE: one inline-image call to a vision model (IMAGE_MODEL).
    if let Ok(f) = std::env::var("IMAGE_B64_FILE") {
        let b64 = std::fs::read_to_string(f).expect("image file").trim().to_string();
        let model = env("IMAGE_MODEL", "qwen3-vl-4b");
        let started = Instant::now();
        let body = json!({"model": model, "priority": 6, "max_tokens": 48, "image_b64": b64, "image_mime": "image/png",
            "text": "Describe this image in one sentence."});
        match a.invoke(TOPIC, "infer.v1", body, Duration::from_secs(180)).await {
            Ok(v) => println!(
                "[{:6.1}s] image call {model}: {} ms (load {} run {}) -> {:?}",
                t0.elapsed().as_secs_f64(), started.elapsed().as_millis(), v["load_ms"], v["run_ms"], v["text"]
            ),
            Err(e) => println!("[{:6.1}s] image call {model}: FAILED {e}", t0.elapsed().as_secs_f64()),
        }
    }
    tokio::time::sleep(Duration::from_secs(env("TAIL_SECS", "7").parse().unwrap())).await;
    println!("done");
}
