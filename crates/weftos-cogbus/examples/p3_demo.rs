//! A throwaway broker plus consumers for trying the P3 device-owner cogs by hand on a board,
//! without the node daemon (ADR-122). Not a test and not shipped.
//!
//!   p3_demo SOCK audio|camera|mcu SECONDS
//!
//! It registers the cogs (token = the instance name padded with '-' to 64 chars) and a
//! consumer, serves SOCK, and for SECONDS acts as the consumer, then prints a JSON summary.

use std::sync::Arc;
use std::time::{Duration, Instant};

use clawft_types::cog_topics::{InputPolicy, TopicClass};
use serde_json::{json, Value};
use weftos_cogbus::server::serve;
use weftos_cogbus::{Broker, BrokerConfig, Client, InputSpec, InstanceSpec, OutputSpec};

const PROJECT: &str = "01J9ZXW0PRJCTAAAAAAAAAAAAA";

fn token(name: &str) -> String {
    format!("{name:-<64}")
}

fn out(topic: &str, schema: &str, class: TopicClass) -> OutputSpec {
    OutputSpec { topic: topic.into(), schema: schema.into(), class }
}

fn inp(topic: &str, schema: &str, policy: InputPolicy) -> InputSpec {
    InputSpec { topic: topic.into(), schema: schema.into(), policy }
}

fn reg(b: &Broker, id: &str, outputs: Vec<OutputSpec>, inputs: Vec<InputSpec>) {
    let spec = InstanceSpec {
        instance_id: id.into(),
        cog_id: id.into(),
        project_id: Some(PROJECT.into()),
        uid: None,
        outputs,
        inputs,
    };
    b.register(spec, &token(id)).expect("register");
}

fn now_ns() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64
}

#[tokio::main]
async fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (sock, scenario, secs) = (a[1].clone(), a[2].clone(), a[3].parse::<u64>().unwrap());
    let broker = Arc::new(Broker::new(BrokerConfig::new("0123456789abcdef0123456789abcdef")));
    let tel = || out("telemetry", "telemetry.v1", TopicClass::Reading);
    reg(&broker, "audio-in", vec![out("audio.pcm", "pcm.s16le.v1", TopicClass::Stream), tel()], vec![]);
    reg(&broker, "tts", vec![out("audio.pcm", "pcm.s16le.v1", TopicClass::Stream)], vec![]);
    reg(&broker, "audio-out", vec![tel()], vec![inp("audio.pcm", "pcm.s16le.v1", InputPolicy::DropOldest)]);
    reg(&broker, "camera", vec![out("video.frame-ref", "video.frame-ref.v1", TopicClass::Reading), tel()], vec![]);
    reg(&broker, "mcu-bridge", vec![out("svc.mcu", "mcu.call.v1", TopicClass::Reading), tel()], vec![]);
    reg(
        &broker,
        "reader",
        vec![],
        vec![
            inp("audio.pcm", "pcm.s16le.v1", InputPolicy::DropOldest),
            inp("video.frame-ref", "video.frame-ref.v1", InputPolicy::DropOldest),
            inp("telemetry", "telemetry.v1", InputPolicy::DropOldest),
            inp("svc.mcu", "mcu.call.v1", InputPolicy::DropOldest),
        ],
    );
    let _server = serve(broker.clone(), std::path::Path::new(&sock)).await.expect("serve");
    println!("{}", json!({"event": "ready", "socket": sock, "scenario": scenario}));
    let path = std::path::PathBuf::from(&sock);
    let reader = Client::connect(&path, &token("reader")).await.expect("reader connects");
    let end = Instant::now() + Duration::from_secs(secs);
    let summary: Value = match scenario.as_str() {
        "audio" => {
            let src = std::env::var("AUDIO_SOURCE").unwrap_or_else(|_| "mic0".into());
            let tts_src = std::env::var("TTS_SOURCE").unwrap_or_else(|_| "tts0".into());
            // The reader hears the microphone; a writer named tts feeds audio-out if asked.
            let mut r = loop {
                match reader.subscribe_stream(&format!("audio.pcm/{src}"), "pcm.s16le.v1").await {
                    Ok(r) => break r,
                    Err(e) => {
                        if Instant::now() > end {
                            panic!("subscribe: {e}");
                        }
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                }
            };
            let tts_file = std::env::var("TTS_FILE").ok().filter(|f| !f.is_empty());
            let tts = if tts_file.is_some() {
                let c = Client::connect(&path, &token("tts")).await.unwrap();
                Some(c)
            } else {
                None
            };
            if let (Some(c), Some(f)) = (tts.as_ref(), tts_file) {
                let pcm = std::fs::read(f).unwrap();
                let mut w = c
                    .stream_writer("audio.pcm", &tts_src, "pcm.s16le.v1", weftos_cogbus::StreamOptions::new(640))
                    .await
                    .unwrap();
                tokio::spawn(async move {
                    // Wait for audio-out to attach, then send in real time.
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    let mut next = Instant::now();
                    for ch in pcm.chunks_exact(640) {
                        let _ = w.write_async(now_ns(), ch).await;
                        next += Duration::from_millis(20);
                        tokio::time::sleep_until(next.into()).await;
                    }
                    println!("{}", json!({"event": "tts_done", "records": w.last_seq()}));
                });
            }
            let (mut n, mut bytes_ok, mut lat_sum, mut lat_max, mut mono, mut last_t) = (0u64, true, 0f64, 0f64, true, 0u64);
            let mut first = None;
            let dump = std::env::var("DUMP_PCM").ok().filter(|f| !f.is_empty());
            let mut pcm = Vec::new();
            while Instant::now() < end {
                match tokio::time::timeout(Duration::from_millis(500), r.next()).await {
                    Ok(Some(rec)) => {
                        n += 1;
                        first.get_or_insert(rec.seq);
                        bytes_ok &= rec.data.len() == 640;
                        let l = (now_ns().saturating_sub(rec.w_ns)) as f64 / 1e6;
                        lat_sum += l;
                        lat_max = lat_max.max(l);
                        mono &= rec.t_ns >= last_t;
                        last_t = rec.t_ns;
                        if dump.is_some() {
                            pcm.extend_from_slice(&rec.data);
                        }
                    }
                    _ => {}
                }
            }
            if let Some(f) = dump {
                std::fs::write(f, &pcm).unwrap();
            }
            json!({"scenario": "audio", "records": n, "first_seq": first, "last_seq": r.last_seq(), "skipped": r.skipped(),
                   "record_bytes_all_640": bytes_ok, "t_ns_monotonic": mono,
                   "mean_latency_ms": if n > 0 { lat_sum / n as f64 } else { -1.0 }, "max_latency_ms": lat_max,
                   "rate_per_s": n as f64 / secs as f64, "pcm_bytes": pcm.len()})
        }
        "camera" => {
            let mut sub = reader.subscribe("video.frame-ref/*", "video.frame-ref.v1").await.unwrap();
            let dir = std::env::var("FRAMES_DIR").unwrap_or_else(|_| "frames".into());
            let (mut n, mut ok, mut missing, mut dims) = (0u64, 0u64, 0u64, Value::Null);
            // PROGRESS_SECS=60 prints one line per minute, for soaks; frames per second since
            // the start are what a reader saw, not what the camera made.
            let progress = std::env::var("PROGRESS_SECS").ok().and_then(|v| v.parse::<u64>().ok());
            let (started, mut next_line, mut gaps, mut last_seq) = (Instant::now(), Instant::now(), 0u64, 0u64);
            while Instant::now() < end {
                if let Some(p) = progress.filter(|_| Instant::now() >= next_line) {
                    next_line = Instant::now() + Duration::from_secs(p);
                    println!("{}", json!({"progress": {"at_s": started.elapsed().as_secs(), "refs": n, "hash_ok": ok, "missing_or_mismatch": missing, "seq_gaps": gaps, "fps": n as f64 / started.elapsed().as_secs_f64()}}));
                }
                if let Ok(Some(m)) = tokio::time::timeout(Duration::from_millis(500), sub.recv()).await {
                    n += 1;
                    if last_seq != 0 && m.seq > last_seq + 1 {
                        gaps += m.seq - last_seq - 1;
                    }
                    last_seq = m.seq;
                    let h = m.body["hash"].as_str().unwrap_or("").to_string();
                    dims = json!([m.body["width"], m.body["height"]]);
                    match std::fs::read(format!("{dir}/{h}.jpg")) {
                        Ok(b) if blake3::hash(&b).to_hex().to_string() == h => ok += 1,
                        _ => missing += 1,
                    }
                }
            }
            json!({"scenario": "camera", "refs": n, "hash_matches_bytes_on_disk": ok, "missing_or_mismatch": missing, "seq_gaps": gaps, "seconds": started.elapsed().as_secs(), "dims": dims})
        }
        "mcu" => {
            let mut results = Vec::new();
            for (m, p) in [("weftos_ping", json!([])), ("weftos_calls", json!([])), ("weftos_calls", json!([])), ("gpio.write", json!([13, true]))] {
                let t = Instant::now();
                let r = loop {
                    let r = reader.invoke("svc.mcu/venq-mcu", "mcu.call.v1", json!({"method": m, "params": p}), Duration::from_secs(5)).await;
                    match &r {
                        Err(weftos_cogbus::ClientError::Refused { code: weftos_cogbus::ErrorCode::Unavailable, .. }) if Instant::now() < end => {
                            tokio::time::sleep(Duration::from_millis(300)).await
                        }
                        _ => break r,
                    }
                };
                results.push(json!({"method": m, "ms": t.elapsed().as_secs_f64() * 1000.0, "answer": match r { Ok(v) => v, Err(e) => json!({"error": e.to_string()}) }}));
            }
            json!({"scenario": "mcu", "calls": results})
        }
        other => panic!("unknown scenario {other}"),
    };
    println!("{}", json!({"event": "summary", "summary": summary}));
}
