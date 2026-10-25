//! PCM soak (ADR-122 B2 done-when): 16 kHz s16le mono in 20 ms chunks
//! (640 bytes, 50 per second) from a writer cog to a reader cog, each its
//! own process, through a broker in this one, for `COGBUS_SOAK_SECS`
//! (default 3). The reader checks every sample and reports drops.
//!
//! ```text
//! COGBUS_SOAK_SECS=3600 scripts/build.sh test weftos-cogbus --filter pcm_soak
//! ```
//!
//! `COGBUS_SOAK_POLICY` picks the reader policy (`drop-oldest`, `block`).
//! The children are this test binary re-run as `soak_child`.
#![cfg(unix)]

mod common;

use std::process::Stdio;
use std::time::{Duration, Instant};

use clawft_types::cog_topics::InputPolicy;
use common::*;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use weftos_cogbus::stream::StreamOptions;
use weftos_cogbus::{Client, StreamConfig};

const CHUNK_FRAMES: u64 = 320;
const NS_PER_FRAME: u64 = 62_500;

fn env_u64(k: &str, d: u64) -> u64 {
    std::env::var(k)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(d)
}

fn chunk(seq: u64) -> Vec<u8> {
    let first = (seq - 1) * CHUNK_FRAMES;
    (0..CHUNK_FRAMES)
        .flat_map(|i| ((first + i) as u16 as i16).to_le_bytes())
        .collect()
}

fn spawn(role: &str, path: &std::path::Path, token: &str, chunks: u64) -> tokio::process::Child {
    tokio::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "soak_child", "--nocapture", "--test-threads=1"])
        .env("COGBUS_SOAK_ROLE", role)
        .env(weftos_cogbus::ENV_SOCKET, path)
        .env(weftos_cogbus::ENV_TOKEN, token)
        .env("COGBUS_SOAK_CHUNKS", chunks.to_string())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap()
}

/// The first `<tag> {json}` line of a child, within `wait`.
async fn line(child: &mut tokio::process::Child, tag: &str, wait: Duration) -> Value {
    let out = child.stdout.as_mut().unwrap();
    let mut lines = BufReader::new(out).lines();
    let found = tokio::time::timeout(wait, async {
        while let Ok(Some(l)) = lines.next_line().await {
            // libtest may print "test soak_child ... " ahead of it.
            if let Some((_, rest)) = l.split_once(&format!("{tag} ")) {
                return serde_json::from_str(rest.trim()).unwrap();
            }
        }
        panic!("child ended before {tag}");
    });
    found
        .await
        .unwrap_or_else(|_| panic!("no {tag} within {wait:?}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pcm_soak() {
    if std::env::var("COGBUS_SOAK_ROLE").is_ok() {
        return;
    }
    let secs = env_u64("COGBUS_SOAK_SECS", 3);
    let chunks = secs * 50;
    let policy = std::env::var("COGBUS_SOAK_POLICY").ok();
    let policy = policy
        .as_deref()
        .and_then(InputPolicy::parse)
        .unwrap_or_default();
    let rig = Rig::new(StreamConfig::default()).await;
    rig.cog("mic", 'a', vec![pcm_out()], vec![]);
    rig.cog("asr", 'b', vec![], vec![pcm_in(policy)]);
    let path = rig.server.path().to_path_buf();

    let mut reader = spawn("reader", &path, &token('b'), chunks);
    line(&mut reader, "READY", Duration::from_secs(30)).await;
    let mut writer = spawn("writer", &path, &token('a'), chunks);
    let started = Instant::now();
    let wait = Duration::from_secs(secs + 30);
    let w = line(&mut writer, "RESULT", wait).await;
    let r = line(&mut reader, "RESULT", wait).await;
    let rows = rig.broker.snapshot();
    let report = json!({
        "secs": secs, "policy": policy, "chunks": chunks, "elapsed_s": started.elapsed().as_secs_f64(),
        "writer": w, "reader": r, "ring_head": rows[0].messages, "demotions": rows[0].drops,
    });
    eprintln!("pcm_soak {report}");
    if let Ok(p) = std::env::var("COGBUS_SOAK_REPORT") {
        std::fs::write(p, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    }
    assert_eq!(r["received"], chunks, "{report}");
    assert_eq!(
        (r["skipped"].as_u64(), r["corrupt"].as_u64()),
        (Some(0), Some(0)),
        "{report}"
    );
    let _ = (writer.wait().await, reader.wait().await);
}

/// A soak child: `writer` or `reader` (does nothing outside a soak).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn soak_child() {
    let Ok(role) = std::env::var("COGBUS_SOAK_ROLE") else {
        return;
    };
    let chunks = env_u64("COGBUS_SOAK_CHUNKS", 150);
    let bus = Client::from_env().await.unwrap();
    let result = match role.as_str() {
        "writer" => soak_writer(&bus, chunks).await,
        _ => soak_reader(&bus, chunks).await,
    };
    println!("RESULT {result}");
}

async fn soak_writer(bus: &Client, chunks: u64) -> Value {
    let mut w = bus
        .stream_writer(KIND, SOURCE, PCM, StreamOptions::new(640))
        .await
        .unwrap();
    let mut tick = tokio::time::interval(Duration::from_millis(20));
    let (mut worst, t0) = (Duration::ZERO, Instant::now());
    for seq in 1..=chunks {
        tick.tick().await;
        let data = chunk(seq);
        let t = Instant::now();
        assert_eq!(
            w.write_async((seq - 1) * CHUNK_FRAMES * NS_PER_FRAME, &data)
                .await
                .unwrap(),
            seq
        );
        worst = worst.max(t.elapsed());
    }
    json!({ "written": chunks, "secs": t0.elapsed().as_secs_f64(),
            "worst_write_us": worst.as_micros() as u64, "demotions": w.demotions() })
}

async fn soak_reader(bus: &Client, chunks: u64) -> Value {
    let mut r = bus.subscribe_stream(TOPIC, PCM).await.unwrap();
    println!("READY {{}}");
    let (mut received, mut corrupt, mut lat_sum, mut lat_max) = (0u64, 0u64, 0u64, 0u64);
    while r.last_seq() < chunks {
        let Ok(Some(rec)) = tokio::time::timeout(Duration::from_secs(10), r.next()).await else {
            break;
        };
        received += 1;
        if rec.data != chunk(rec.seq) {
            corrupt += 1;
        }
        let lat = weftos_cogbus_now_ns().saturating_sub(rec.w_ns);
        lat_sum += lat;
        lat_max = lat_max.max(lat);
    }
    json!({ "received": received, "last_seq": r.last_seq(), "skipped": r.skipped(), "corrupt": corrupt,
            "mean_latency_us": lat_sum / received.max(1) / 1_000, "max_latency_us": lat_max / 1_000,
            "policy_changes": r.take_events().len() })
}

fn weftos_cogbus_now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}
