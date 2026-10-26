//! A throwaway broker for trying the P5 voice and vision pipelines by hand on a board, without the
//! node daemon (ADR-122). Not a test and not shipped.
//!
//!   p5_chain SOCK SECONDS
//!
//! It registers the cogs of the hear-and-answer loop (token = the instance name padded with '-'
//! to 64 chars) plus a watcher, serves SOCK, and for SECONDS prints one JSON line per event it
//! sees: every transcript, every telemetry reading, the first and last chunk of the `tts0`
//! stream and the count of the `mic0` stream. With `FAKE_INFER=1` it also answers `svc.infer`
//! itself with a canned reply (a stand-in for `npu-infer`); without it, `npu-infer` registers
//! under the name `npu-infer`. `DUMP_TTS=FILE` writes the speech stream's PCM to FILE.

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
    let spec = InstanceSpec { instance_id: id.into(), cog_id: id.into(), project_id: Some(PROJECT.into()), uid: None, outputs, inputs };
    b.register(spec, &token(id)).expect("register");
}

fn now_ms() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis()
}

fn say(v: Value) {
    println!("{}", json!({"at_ms": now_ms() % 1_000_000, "event": v}));
}

#[tokio::main]
async fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (sock, secs) = (a[1].clone(), a[2].parse::<u64>().unwrap());
    // The node's blob store (B5), so frames can be handed around by handle.
    let blobs = std::path::PathBuf::from(format!("{sock}.blobs"));
    let _ = std::fs::remove_dir_all(&blobs);
    let store = Arc::new(weftos_cogbus::blob::BlobStore::open(&blobs, weftos_cogbus::blob::BlobConfig::default(), now_ms() as u64).expect("blob store"));
    let broker = Arc::new(Broker::new(BrokerConfig::new("0123456789abcdef0123456789abcdef")).with_blobs(store));
    let tel = || out("telemetry", "telemetry.v1", TopicClass::Reading);
    let pcm = || out("audio.pcm", "pcm.s16le.v1", TopicClass::Stream);
    let tr = || out("transcript", "transcript.v1", TopicClass::Reading);
    let pcm_in = || inp("audio.pcm", "pcm.s16le.v1", InputPolicy::DropOldest);
    let tr_in = || inp("transcript", "transcript.v1", InputPolicy::DropOldest);
    let infer_in = || inp("svc.infer", "infer.v1", InputPolicy::DropOldest);
    reg(&broker, "audio-in", vec![pcm(), tel()], vec![]);
    reg(&broker, "asr", vec![tr(), tel()], vec![pcm_in()]);
    reg(&broker, "wake-word", vec![tr(), tel()], vec![tr_in()]);
    reg(&broker, "assistant", vec![tr(), tel()], vec![tr_in(), infer_in()]);
    reg(&broker, "tts", vec![pcm(), tel()], vec![tr_in()]);
    reg(&broker, "audio-out", vec![tel()], vec![pcm_in()]);
    reg(
        &broker,
        "npu-infer",
        vec![out("svc.infer", "infer.v1", TopicClass::Reading), tel()],
        vec![],
    );
    reg(
        &broker,
        "camera",
        vec![out("video.frame-ref", "video.frame-ref.v1", TopicClass::Reading), out("video.frame", "blob.v1", TopicClass::Blob), tel()],
        vec![],
    );
    reg(
        &broker,
        "vision-detect",
        vec![out("detections", "detections.v1", TopicClass::Reading), tel()],
        vec![inp("video.frame-ref", "video.frame-ref.v1", InputPolicy::Sample), infer_in()],
    );
    reg(
        &broker,
        "watcher",
        vec![],
        vec![tr_in(), pcm_in(), inp("telemetry", "telemetry.v1", InputPolicy::DropOldest), inp("detections", "detections.v1", InputPolicy::DropOldest)],
    );
    let _server = serve(broker.clone(), std::path::Path::new(&sock)).await.expect("serve");
    say(json!({"ready": sock}));
    let path = std::path::PathBuf::from(&sock);
    let end = Instant::now() + Duration::from_secs(secs);

    if std::env::var("FAKE_INFER").is_ok() {
        let p = Client::connect(&path, &token("npu-infer")).await.expect("npu-infer connects");
        let mut svc = p.serve("svc.infer", "venq-npu", "infer.v1").await.expect("serve infer");
        tokio::spawn(async move {
            while let Some(call) = svc.recv().await {
                // A vision request: the frame arrives as a blob handle; prove the provider can
                // read it (the handle travelled in the call) and that it is the camera's JPEG.
                if let Some(h) = call.body.get("image_blob") {
                    let verdict = match serde_json::from_value::<clawft_types::cog_topics::BlobHandle>(h.clone()) {
                        Ok(handle) => match p.get_blob(&handle).await {
                            Ok(bytes) => {
                                let ok = blake3::hash(&bytes).to_hex().to_string() == handle.hash;
                                say(json!({"fake_infer_blob": {"bytes": bytes.len(), "hash_matches": ok, "jpeg_magic": bytes.starts_with(&[0xff, 0xd8])}}));
                                format!("frame of {} bytes, hash {}", bytes.len(), if ok { "verified" } else { "WRONG" })
                            }
                            Err(e) => {
                                say(json!({"fake_infer_blob_error": e.to_string()}));
                                let _ = p.reply(call.call, Err(format!("cannot read the image handle: {e}"))).await;
                                continue;
                            }
                        },
                        Err(e) => {
                            let _ = p.reply(call.call, Err(format!("bad image_blob: {e}"))).await;
                            continue;
                        }
                    };
                    let list = json!([{"label": verdict, "bbox": [0.1, 0.2, 0.5, 0.6], "score": 0.9}, {"label": "stand in object"}]);
                    let _ = p.reply(call.call, Ok(json!({"text": format!("```json\n{list}\n```"), "model": call.body["model"], "finish": "stop"}))).await;
                    continue;
                }
                let text = call.body["text"].as_str().unwrap_or("");
                let last = text.lines().rev().find(|l| l.starts_with("User:")).unwrap_or("User: ").trim_start_matches("User: ").to_string();
                say(json!({"fake_infer_call": last, "model": call.body["model"]}));
                let reply = format!("<think></think>You asked: {last}. I am a stand in for the language model.");
                let _ = p.reply(call.call, Ok(json!({"text": reply, "model": call.body["model"], "finish": "stop"}))).await;
            }
        });
    }

    let w = Arc::new(Client::connect(&path, &token("watcher")).await.expect("watcher connects"));
    let mut tsub = w.subscribe("transcript/*", "transcript.v1").await.unwrap();
    tokio::spawn(async move {
        while let Some(m) = tsub.recv().await {
            say(json!({"transcript": m.topic, "text": m.body["text"], "seq": m.seq}));
        }
    });
    let mut dsub = w.subscribe("detections/*", "detections.v1").await.unwrap();
    tokio::spawn(async move {
        while let Some(m) = dsub.recv().await {
            say(json!({"detections": m.topic, "frame_hash": m.body["frame_hash"], "boxes": m.body["boxes"], "model": m.body["model"]}));
        }
    });
    let mut esub = w.subscribe("telemetry/*", "telemetry.v1").await.unwrap();
    tokio::spawn(async move {
        while let Some(m) = esub.recv().await {
            say(json!({"telemetry": m.topic, "items": m.body["items"].as_array().map(|a| a.iter().map(|i| format!("{}={}", i["name"].as_str().unwrap_or("?"), i["value"])).collect::<Vec<_>>())}));
        }
    });
    let dump = std::env::var("DUMP_TTS").ok().filter(|f| !f.is_empty());
    let (w2, d2) = (w.clone(), dump.clone());
    let tts = tokio::spawn(async move {
        let mut r = loop {
            match w2.subscribe_stream("audio.pcm/tts0", "pcm.s16le.v1").await {
                Ok(r) => break r,
                Err(_) => tokio::time::sleep(Duration::from_millis(300)).await,
            }
        };
        let (mut n, mut pcm_bytes, mut first) = (0u64, Vec::new(), None::<u128>);
        while Instant::now() < end {
            if let Ok(Some(rec)) = tokio::time::timeout(Duration::from_millis(500), r.next()).await {
                if first.is_none() {
                    first = Some(now_ms() % 1_000_000);
                    say(json!({"tts_first_chunk": true}));
                }
                n += 1;
                if d2.is_some() {
                    pcm_bytes.extend_from_slice(&rec.data);
                }
            }
        }
        if let Some(f) = d2 {
            std::fs::write(f, &pcm_bytes).unwrap();
        }
        say(json!({"tts_stream": {"records": n, "skipped": r.skipped(), "seconds": n as f64 * 0.02}}));
    });
    let mic = tokio::spawn({
        let w = w.clone();
        async move {
            let mut r = loop {
                match w.subscribe_stream("audio.pcm/mic0", "pcm.s16le.v1").await {
                    Ok(r) => break r,
                    Err(_) => tokio::time::sleep(Duration::from_millis(300)).await,
                }
            };
            let mut n = 0u64;
            while Instant::now() < end {
                if let Ok(Some(_)) = tokio::time::timeout(Duration::from_millis(500), r.next()).await {
                    n += 1;
                }
            }
            say(json!({"mic_stream": {"records": n, "skipped": r.skipped()}}));
        }
    });
    let _ = tokio::join!(tts, mic);
    say(json!({"done": true}));
}
