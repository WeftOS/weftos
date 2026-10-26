//! A throwaway broker plus an IQ sink for trying the Pluto cogs by hand on a board, without the
//! node daemon (ADR-122), in the manner of npu_demo. Not a test and not shipped.
//!
//! ```text
//! RX_TOKEN=<64> SPEC_TOKEN=<64> cargo run --release -p weftos-cogbus --example pluto_demo -- /tmp/cogbus.sock
//! ```
//! It registers `sdr-pluto-rx` (outputs svc.sdr, sdr.status, iq.cs16), `spectrum` (input iq.cs16;
//! outputs spectrum.psd, band.occupancy), a sink that reads the IQ ring and checks it
//! independently of the cog (record seq gaps, gap events, flags, bands), a watcher on the readings,
//! and a caller that runs `CALLS` (a JSON array of {"at": seconds, "body": {...}}) against
//! svc.sdr. Every 60 s it prints one JSON line per source.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use clawft_types::cog_topics::{InputPolicy, TopicClass};
use serde_json::{json, Value};
use weftos_cogbus::server::serve;
use weftos_cogbus::{Broker, BrokerConfig, Client, InputSpec, InstanceSpec, OutputSpec};

const PROJECT: &str = "01J9ZXW0PRJCTAAAAAAAAAAAAA";
const SRC: &str = "venq-pluto";

fn out(topic: &str, schema: &str, class: TopicClass) -> OutputSpec {
    OutputSpec { topic: topic.into(), schema: schema.into(), class }
}

fn inp(topic: &str, schema: &str) -> InputSpec {
    InputSpec { topic: topic.into(), schema: schema.into(), policy: InputPolicy::DropOldest }
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

fn every() -> Duration {
    Duration::from_secs(std::env::var("PRINT_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(60))
}

fn now_s() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

#[derive(Default)]
struct Sink {
    records: u64,
    samples: u64,
    gap_events: u64,
    gap_samples: u64,
    zero_records: u64,
    discont: u64,
    last_seg: u32,
    segments: u64,
    bands: BTreeMap<u64, u64>,
    bad: u64,
}

#[tokio::main]
async fn main() {
    let sock = std::env::args().nth(1).expect("socket path");
    let rx_token = std::env::var("RX_TOKEN").expect("RX_TOKEN (64 chars)");
    let spec_token = std::env::var("SPEC_TOKEN").unwrap_or_else(|_| "s".repeat(64));
    let (ts, tw, tc) = ("k".repeat(64), "w".repeat(64), "c".repeat(64));
    let broker = Arc::new(Broker::new(BrokerConfig::new("0123456789abcdef0123456789abcdef")));
    reg(
        &broker,
        "sdr-pluto-rx",
        &rx_token,
        vec![
            out("svc.sdr", "sdr.v1", TopicClass::Reading),
            out("sdr.status", "telemetry.v1", TopicClass::Reading),
            out("iq.cs16", "iq.cs16.v1", TopicClass::Stream),
        ],
        vec![],
    );
    reg(
        &broker,
        "spectrum",
        &spec_token,
        vec![
            out("spectrum.psd", "spectrum.psd.v1", TopicClass::Reading),
            out("band.occupancy", "telemetry.v1", TopicClass::Reading),
        ],
        vec![inp("iq.cs16", "iq.cs16.v1"), inp("svc.sdr", "sdr.v1")],
    );
    reg(&broker, "sink", &ts, vec![], vec![inp("iq.cs16", "iq.cs16.v1")]);
    reg(
        &broker,
        "watcher",
        &tw,
        vec![],
        vec![inp("sdr.status", "telemetry.v1"), inp("band.occupancy", "telemetry.v1"), inp("spectrum.psd", "spectrum.psd.v1")],
    );
    reg(&broker, "caller", &tc, vec![], vec![inp("svc.sdr", "sdr.v1")]);
    let _server = serve(broker.clone(), std::path::Path::new(&sock)).await.expect("serve");
    println!("broker ready on {sock}");
    let path = std::path::PathBuf::from(&sock);
    let t0 = Instant::now();

    // The sink: an independent reader of the IQ ring.
    let sink_client = Client::connect(&path, &ts).await.unwrap();
    let sink_path = format!("iq.cs16/{SRC}");
    tokio::spawn(async move {
        let mut r = sink_client.subscribe_stream(&sink_path, "iq.cs16.v1").await.unwrap();
        let mut s = Sink::default();
        let mut last = Instant::now();
        loop {
            match tokio::time::timeout(Duration::from_secs(1), r.next()).await {
                Ok(Some(rec)) => {
                    let Ok(v) = serde_json::from_slice::<Value>(&rec.data) else {
                        s.bad += 1;
                        continue;
                    };
                    s.records += 1;
                    let n = v["samples"].as_u64().unwrap_or(0);
                    if v.get("gap_samples").is_some() {
                        s.gap_events += 1;
                        s.gap_samples += v["gap_samples"].as_u64().unwrap_or(0);
                    } else {
                        s.samples += n;
                        *s.bands.entry(v["center_hz"].as_u64().unwrap_or(0)).or_default() += 1;
                        if v["discontinuity"] == true {
                            s.discont += 1;
                        }
                        if v["all_zero"] == true {
                            s.zero_records += 1;
                        }
                        let seg = v["segment"].as_u64().unwrap_or(0) as u32;
                        if seg != s.last_seg {
                            s.segments += 1;
                            s.last_seg = seg;
                        }
                    }
                }
                Ok(None) => break,
                Err(_) => {}
            }
            if last.elapsed() >= every() {
                last = Instant::now();
                println!(
                    "{}",
                    json!({"src": "sink", "t": now_s(), "up_s": t0.elapsed().as_secs(), "records": s.records, "samples": s.samples,
                        "ring_skipped": r.skipped(), "gap_events": s.gap_events, "gap_samples": s.gap_samples,
                        "discontinuity_records": s.discont, "zero_records": s.zero_records, "segments": s.segments,
                        "bands": s.bands, "bad": s.bad})
                );
            }
        }
    });

    // The watcher: readings from the cog and the spectrum cog, one line a minute each.
    let wc = Client::connect(&path, &tw).await.unwrap();
    let mut sub = wc.subscribe("*/*", "").await.ok();
    if sub.is_none() {
        for (topic, schema) in [(format!("sdr.status/{SRC}"), "telemetry.v1"), (format!("band.occupancy/{SRC}"), "telemetry.v1")] {
            let c = Client::connect(&path, &tw).await.unwrap();
            let mut s = c.subscribe(&topic, schema).await.unwrap();
            let topic2 = topic.clone();
            tokio::spawn(async move {
                let _keep = c;
                let mut last = Instant::now().checked_sub(every()).unwrap_or_else(Instant::now);
                while let Some(m) = s.recv().await {
                    if last.elapsed() >= every() {
                        last = Instant::now();
                        println!("{}", json!({"src": topic2, "t": now_s(), "body": m.body}));
                    }
                }
            });
        }
    }
    drop(sub.take());

    // The caller: scripted service calls.
    if let Ok(script) = std::env::var("CALLS") {
        let calls: Vec<Value> = serde_json::from_str(&script).expect("CALLS json");
        let c = Client::connect(&path, &tc).await.unwrap();
        tokio::spawn(async move {
            for call in calls {
                let at = call["at"].as_f64().unwrap_or(0.0);
                let wait = Duration::from_secs_f64(at).saturating_sub(t0.elapsed());
                tokio::time::sleep(wait).await;
                let r = c.invoke(&format!("svc.sdr/{SRC}"), "sdr.v1", call["body"].clone(), Duration::from_secs(20)).await;
                println!("{}", json!({"src": "call", "t": now_s(), "body": call["body"], "reply": format!("{r:?}")}));
            }
        });
    }
    tokio::time::sleep(Duration::from_secs(std::env::var("RUN_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(86_400))).await;
    println!("done");
}
