//! Substrate-connected classifier pipeline.
//!
//! Mirrors `clawft-service-whisper::service` shape: subscribe to a
//! configured pcm_chunk path, decode b64 → i16le, run a
//! [`ClassifierBackend`] per window, publish a [`Classification`] to
//! the configured output path via `publish_gated`.
//!
//! # Why one classification per chunk (window-level)
//!
//! Per the task scope: "no per-frame classification — window-level
//! (one classification per pcm_chunk) is fine." The firmware paces
//! pcm_chunks at ~2 Hz / 500 ms each, which is already a sensible
//! VAD analysis window. A future backend that wants finer resolution
//! can re-window inside its `classify` impl.
//!
//! # Backpressure
//!
//! There is none. The energy classifier is microseconds-per-chunk;
//! at 2 Hz cadence the substrate publish dominates and is fire-and-
//! forget. A future llama.cpp backend that takes meaningful time
//! per call should follow the whisper pattern (one in-flight slot,
//! drop-oldest); see journal §A5 for the policy taxonomy.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use clawft_kernel::SubstrateService;
use serde_json::Value;
use tokio::sync::{mpsc, watch};
use tracing::{debug, error, info};

use crate::classifier::{Classification, ClassifierBackend};

/// Configuration for [`ClassifierService`].
#[derive(Clone)]
pub struct ClassifierServiceConfig {
    /// Node id this service publishes under. Required — every
    /// substrate write is node-attributed under the node-identity
    /// gate. Typically the daemon's own node-id (the daemon is "the
    /// node running the classifier" in the ontology).
    pub node_id: String,
    /// Source node-id of the audio stream being classified. Echoed
    /// into [`Classification::source_node`] for downstream
    /// attribution and used for log context. Does NOT affect path
    /// gating — the gate only checks `output_path` against `node_id`.
    pub source_node: String,
    /// Substrate path to read PCM from (the same path the whisper
    /// service subscribes to today: `substrate/<source-node>/sensor/mic/pcm_chunk`).
    pub input_path: String,
    /// Substrate path to write classifications to. Must start with
    /// `substrate/<node_id>/` for the publish gate to accept it. The
    /// daemon wires this to `substrate/<daemon-node>/derived/classify/<source-node>/mic`
    /// today (mesh-canonical `_derived/...` is a follow-up; see R3.0).
    pub output_path: String,
    /// Service-level enable flag. When `false`, the classifier loop
    /// drops incoming chunks before any work — no classification is
    /// run, no publish. Defaults to a fresh `Arc<AtomicBool>(true)` if
    /// you don't supply one. Caller can keep its own clone of the Arc
    /// to flip the flag from a control-plane RPC.
    pub service_enabled: Arc<AtomicBool>,
    /// Source-sensor enable flag (consumer-side soft-disable). When
    /// `false`, the service keeps its substrate subscription alive
    /// but drops every chunk that arrives — same shape as whisper's
    /// source_enabled flag.
    pub source_enabled: Arc<AtomicBool>,
}

impl std::fmt::Debug for ClassifierServiceConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClassifierServiceConfig")
            .field("node_id", &self.node_id)
            .field("source_node", &self.source_node)
            .field("input_path", &self.input_path)
            .field("output_path", &self.output_path)
            .field(
                "service_enabled",
                &self.service_enabled.load(Ordering::SeqCst),
            )
            .field(
                "source_enabled",
                &self.source_enabled.load(Ordering::SeqCst),
            )
            .finish()
    }
}

impl ClassifierServiceConfig {
    /// Build a test-friendly default config for the given node id +
    /// source node. Production callers (the daemon) should construct
    /// the struct directly so the field shape stays explicit.
    pub fn for_test(node_id: &str, source_node: &str) -> Self {
        let input_path = format!("substrate/{source_node}/sensor/mic/pcm_chunk");
        let output_path = format!("substrate/{node_id}/derived/classify/{source_node}/mic");
        Self {
            node_id: node_id.to_string(),
            source_node: source_node.to_string(),
            input_path,
            output_path,
            service_enabled: Arc::new(AtomicBool::new(true)),
            source_enabled: Arc::new(AtomicBool::new(true)),
        }
    }
}

/// Runtime handle for a spawned classifier service task.
#[derive(Debug)]
pub struct ClassifierService {
    shutdown: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

impl ClassifierService {
    /// Spawn the pipeline on the tokio runtime of the caller.
    ///
    /// Wiring:
    /// 1. `substrate.subscribe(input_path)` — gets an mpsc of update
    ///    lines (JSON bytes).
    /// 2. Parses each line, pulls `value` (the PcmChunk JSON), decodes
    ///    base64 → s16le PCM bytes → `&[i16]`.
    /// 3. Calls `backend.classify(&pcm_i16, sample_rate)`.
    /// 4. Stamps `ts_ms` / `source_node` / `source_seq` onto the
    ///    [`Classification`] and `publish_gated`s it to `output_path`.
    ///
    /// # Errors
    ///
    /// Returns `Err` only if the initial `substrate.subscribe` call
    /// fails egress gating. Per-chunk errors (malformed payload,
    /// unknown encoding) are logged and the chunk is dropped.
    pub fn spawn(
        substrate: SubstrateService,
        backend: Arc<dyn ClassifierBackend>,
        config: ClassifierServiceConfig,
    ) -> Result<Self, String> {
        let (id, rx) = substrate
            .subscribe(Some(&config.node_id), &config.input_path)
            .map_err(|e| format!("substrate subscribe failed: {e}"))?;
        info!(
            sub_id = id.0,
            input = %config.input_path,
            output = %config.output_path,
            source_node = %config.source_node,
            "classifier service: subscribed to PCM input"
        );

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let substrate_for_task = substrate.clone();
        let input_path_cleanup = config.input_path.clone();
        let task = tokio::spawn(async move {
            run_pipeline(rx, substrate_for_task.clone(), backend, config, shutdown_rx).await;
            substrate_for_task.unsubscribe(&input_path_cleanup, id);
        });
        Ok(Self {
            shutdown: shutdown_tx,
            task,
        })
    }

    /// Signal shutdown and await the internal task.
    pub async fn shutdown(self) {
        let _ = self.shutdown.send(true);
        let _ = self.task.await;
    }
}

async fn run_pipeline(
    mut rx: mpsc::Receiver<Vec<u8>>,
    substrate: SubstrateService,
    backend: Arc<dyn ClassifierBackend>,
    config: ClassifierServiceConfig,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            changed = shutdown_rx.changed() => {
                if changed.is_ok() && *shutdown_rx.borrow() {
                    debug!("classifier service: shutdown requested");
                    break;
                }
            }
            line = rx.recv() => {
                let Some(bytes) = line else {
                    debug!("classifier service: substrate sender dropped, exiting");
                    break;
                };
                if !config.service_enabled.load(Ordering::SeqCst) {
                    debug!("classifier service: chunk dropped (service disabled)");
                    continue;
                }
                if !config.source_enabled.load(Ordering::SeqCst) {
                    debug!("classifier service: chunk dropped (source sensor disabled)");
                    continue;
                }
                let Some(value) = decode_update_line(&bytes) else { continue };
                match decode_pcm_chunk(&value) {
                    Ok(decoded) => {
                        let mut cls = backend.classify(&decoded.pcm_i16, decoded.sample_rate);
                        // Stamp lineage fields the backend can't know.
                        cls.ts_ms = decoded.start_ts_ms;
                        cls.source_node = config.source_node.clone();
                        cls.source_seq = decoded.start_ts_ms;
                        publish_classification(&substrate, &config, &cls);
                    }
                    Err(e) => {
                        debug!(err = %e, "classifier service: skipping malformed chunk");
                    }
                }
            }
        }
    }
}

fn publish_classification(
    substrate: &SubstrateService,
    config: &ClassifierServiceConfig,
    cls: &Classification,
) {
    let payload = match serde_json::to_value(cls) {
        Ok(v) => v,
        Err(e) => {
            error!(err = %e, "classifier service: classification did not serialise");
            return;
        }
    };
    match substrate.publish_gated(Some(&config.node_id), &config.output_path, payload) {
        Ok(tick) => debug!(
            tick,
            class = %cls.class,
            confidence = cls.confidence,
            rms_db = cls.rms_db,
            ts_ms = cls.ts_ms,
            "classifier service: classification published"
        ),
        Err(e) => error!(
            err = %e,
            output_path = %config.output_path,
            node_id = %config.node_id,
            "classifier service: gate denied classification publish"
        ),
    }
}

/// Decoded form of an inbound `pcm_chunk` JSON value.
struct DecodedChunk {
    pcm_i16: Vec<i16>,
    sample_rate: u32,
    start_ts_ms: u64,
}

/// Parse a substrate-subscribe update line and return the inner
/// `value` field iff `kind == "publish"`.
///
/// Mirror of `clawft_service_whisper::service::decode_update_line`;
/// duplicated rather than depended-on so the classify crate has no
/// edge into the whisper crate.
fn decode_update_line(line: &[u8]) -> Option<Value> {
    let end = if line.last() == Some(&b'\n') {
        line.len() - 1
    } else {
        line.len()
    };
    let v: Value = serde_json::from_slice(&line[..end]).ok()?;
    if v.get("kind")?.as_str()? != "publish" {
        return None;
    }
    Some(v.get("value")?.clone())
}

/// Decode an inbound pcm_chunk into i16 samples + metadata.
///
/// Wire shape matches `clawft_service_whisper::PcmChunk` (alias `data`
/// or legacy `pcm_b64`; `encoding: "base64"`; `format: "i16le"`).
fn decode_pcm_chunk(value: &Value) -> Result<DecodedChunk, String> {
    let obj = value.as_object().ok_or("pcm_chunk value not an object")?;

    let data_str = obj
        .get("data")
        .or_else(|| obj.get("pcm_b64"))
        .and_then(|v| v.as_str())
        .ok_or("pcm_chunk missing data/pcm_b64 string")?;

    // encoding/format default to base64/i16le when absent — the
    // firmware always sets them, but the legacy publish_wav fixture
    // relied on defaults. Match PcmChunk's `#[serde(default)]` shape.
    let encoding = obj
        .get("encoding")
        .and_then(|v| v.as_str())
        .unwrap_or("base64");
    if encoding != "base64" {
        return Err(format!(
            "unsupported encoding: {encoding:?} (want \"base64\")"
        ));
    }
    let format = obj
        .get("format")
        .and_then(|v| v.as_str())
        .unwrap_or("i16le");
    if format != "i16le" {
        return Err(format!("unsupported format: {format:?} (want \"i16le\")"));
    }

    let sample_rate = obj
        .get("sample_rate")
        .and_then(|v| v.as_u64())
        .ok_or("pcm_chunk missing sample_rate")?;
    let sample_rate = u32::try_from(sample_rate)
        .map_err(|_| format!("pcm_chunk sample_rate {sample_rate} out of range"))?;

    // start_ts_ms is the source's monotonic ms-stamp; PcmChunk also
    // accepts the legacy alias `seq` so we mirror the same lookup.
    let start_ts_ms = obj
        .get("start_ts_ms")
        .or_else(|| obj.get("seq"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    let pcm_bytes = B64
        .decode(data_str.as_bytes())
        .map_err(|e| format!("data b64 decode: {e}"))?;
    if pcm_bytes.len() % 2 != 0 {
        return Err(format!(
            "pcm payload length {} not a multiple of 2 (i16le)",
            pcm_bytes.len()
        ));
    }
    let pcm_i16: Vec<i16> = pcm_bytes
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect();

    Ok(DecodedChunk {
        pcm_i16,
        sample_rate,
        start_ts_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CLASS_SILENCE;
    use crate::classifier::EnergyClassifier;
    use serde_json::json;
    use std::time::Duration;

    fn publish_pcm_chunk(
        substrate: &SubstrateService,
        actor_id: &str,
        path: &str,
        pcm_i16: &[i16],
        sample_rate: u32,
        start_ts_ms: u64,
    ) {
        let mut bytes = Vec::with_capacity(pcm_i16.len() * 2);
        for &s in pcm_i16 {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        let payload = json!({
            "data": B64.encode(&bytes),
            "encoding": "base64",
            "format": "i16le",
            "sample_rate": sample_rate,
            "channels": 1,
            "samples": pcm_i16.len() as u64,
            "start_ts_ms": start_ts_ms,
        });
        substrate.publish(Some(actor_id), path, payload);
    }

    #[tokio::test]
    async fn decode_update_line_extracts_publish_value() {
        let raw = br#"{"path":"p","tick":1,"kind":"publish","value":{"x":1},"actor_id":null}"#;
        let v = decode_update_line(raw).unwrap();
        assert_eq!(v["x"], 1);
    }

    #[tokio::test]
    async fn decode_update_line_ignores_notify() {
        let raw = br#"{"path":"p","tick":1,"kind":"notify","value":null,"actor_id":null}"#;
        assert!(decode_update_line(raw).is_none());
    }

    #[tokio::test]
    async fn decode_pcm_chunk_rejects_wrapping_sample_rate() {
        // 2^32 + 16000 used to truncate to 16000 and be accepted.
        let v = json!({
            "data": "AAAA",
            "sample_rate": 4_294_967_296u64 + 16_000,
        });
        let err = decode_pcm_chunk(&v).err().expect("sample_rate must be rejected");
        assert!(err.contains("out of range"), "got {err:?}");
    }

    #[tokio::test]
    async fn decode_pcm_chunk_roundtrips_i16() {
        // Build a tiny known sample buffer, encode + decode.
        let samples: Vec<i16> = vec![0, 1, -1, 32_767, -32_768];
        let mut bytes = Vec::with_capacity(samples.len() * 2);
        for &s in &samples {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        let v = json!({
            "data": B64.encode(&bytes),
            "encoding": "base64",
            "format": "i16le",
            "sample_rate": 16_000,
            "samples": samples.len() as u64,
            "start_ts_ms": 12_345,
        });
        let decoded = decode_pcm_chunk(&v).unwrap();
        assert_eq!(decoded.pcm_i16, samples);
        assert_eq!(decoded.sample_rate, 16_000);
        assert_eq!(decoded.start_ts_ms, 12_345);
    }

    #[tokio::test]
    async fn end_to_end_publishes_classification_for_loud_chunk() {
        // Source: 200ms of -20 dBFS sine at 16 kHz mono.
        // Classifier with default -45 dB threshold → should emit speech.
        let substrate = SubstrateService::new();
        // ADR-057: sensor/** is deny-by-default; subscriber must be the
        // owning node (or scope:admin). Use the same node id for service
        // and source so tests exercise the happy path without grants.
        let cfg = ClassifierServiceConfig::for_test("n-bfc4cd", "n-bfc4cd");
        let input_path = cfg.input_path.clone();
        let output_path = cfg.output_path.clone();
        let actor = cfg.node_id.clone();

        // Pre-subscribe to OUTPUT before spawning the service so we
        // never race the first publish.
        let (_oid, mut out_rx) = substrate.subscribe(Some(&actor), &output_path).unwrap();

        let backend: Arc<dyn ClassifierBackend> = Arc::new(EnergyClassifier::new(-45.0));
        let svc = ClassifierService::spawn(substrate.clone(), backend, cfg).unwrap();

        // Build a -20 dBFS sine (peak ≈ 4632), 200ms at 16 kHz.
        let n = 16_000 * 200 / 1000;
        let mut sine = Vec::with_capacity(n);
        let step = 2.0 * std::f32::consts::PI * 440.0 / 16_000.0;
        for i in 0..n {
            let v = 4_632.0 * (step * (i as f32)).sin();
            sine.push(v.round() as i16);
        }

        publish_pcm_chunk(&substrate, &actor, &input_path, &sine, 16_000, 99_999);

        let line = tokio::time::timeout(Duration::from_secs(2), out_rx.recv())
            .await
            .expect("classification not published in time")
            .expect("substrate sender closed");
        let update: Value = serde_json::from_slice(&line[..line.len() - 1]).unwrap();
        assert_eq!(update["kind"], "publish");
        assert_eq!(update["path"], output_path);
        let body = &update["value"];
        assert_eq!(body["class"], "speech");
        assert_eq!(body["sample_rate"], 16_000);
        assert_eq!(body["source_node"], "n-bfc4cd");
        assert_eq!(body["ts_ms"], 99_999);
        assert_eq!(body["source_seq"], 99_999);
        assert!(body["rms_db"].as_f64().unwrap() > -25.0);

        svc.shutdown().await;
    }

    #[tokio::test]
    async fn end_to_end_publishes_silence_for_zero_chunk() {
        let substrate = SubstrateService::new();
        let cfg = ClassifierServiceConfig::for_test("n-bfc4cd", "n-bfc4cd");
        let input_path = cfg.input_path.clone();
        let output_path = cfg.output_path.clone();
        let actor = cfg.node_id.clone();
        let (_oid, mut out_rx) = substrate.subscribe(Some(&actor), &output_path).unwrap();

        let backend: Arc<dyn ClassifierBackend> = Arc::new(EnergyClassifier::default());
        let svc = ClassifierService::spawn(substrate.clone(), backend, cfg).unwrap();

        let zeros = vec![0i16; 8_000];
        publish_pcm_chunk(&substrate, &actor, &input_path, &zeros, 16_000, 1);

        let line = tokio::time::timeout(Duration::from_secs(2), out_rx.recv())
            .await
            .expect("classification not published in time")
            .expect("substrate sender closed");
        let update: Value = serde_json::from_slice(&line[..line.len() - 1]).unwrap();
        let body = &update["value"];
        assert_eq!(body["class"], CLASS_SILENCE);
        svc.shutdown().await;
    }

    #[tokio::test]
    async fn disabled_service_drops_chunks_no_publish() {
        let substrate = SubstrateService::new();
        let mut cfg = ClassifierServiceConfig::for_test("n-bfc4cd", "n-bfc4cd");
        cfg.service_enabled = Arc::new(AtomicBool::new(false));
        let input_path = cfg.input_path.clone();
        let output_path = cfg.output_path.clone();
        let actor = cfg.node_id.clone();
        let (_oid, mut out_rx) = substrate.subscribe(Some(&actor), &output_path).unwrap();

        let backend: Arc<dyn ClassifierBackend> = Arc::new(EnergyClassifier::default());
        let svc = ClassifierService::spawn(substrate.clone(), backend, cfg).unwrap();

        let zeros = vec![0i16; 8_000];
        publish_pcm_chunk(&substrate, &actor, &input_path, &zeros, 16_000, 1);

        // Wait briefly; the receiver should NOT see anything.
        let res = tokio::time::timeout(Duration::from_millis(200), out_rx.recv()).await;
        assert!(res.is_err(), "expected timeout — got publish: {res:?}");
        svc.shutdown().await;
    }

    #[tokio::test]
    async fn disabled_source_drops_chunks_no_publish() {
        let substrate = SubstrateService::new();
        let mut cfg = ClassifierServiceConfig::for_test("n-bfc4cd", "n-bfc4cd");
        cfg.source_enabled = Arc::new(AtomicBool::new(false));
        let input_path = cfg.input_path.clone();
        let output_path = cfg.output_path.clone();
        let actor = cfg.node_id.clone();
        let (_oid, mut out_rx) = substrate.subscribe(Some(&actor), &output_path).unwrap();

        let backend: Arc<dyn ClassifierBackend> = Arc::new(EnergyClassifier::default());
        let svc = ClassifierService::spawn(substrate.clone(), backend, cfg).unwrap();

        let zeros = vec![0i16; 8_000];
        publish_pcm_chunk(&substrate, &actor, &input_path, &zeros, 16_000, 1);

        let res = tokio::time::timeout(Duration::from_millis(200), out_rx.recv()).await;
        assert!(res.is_err(), "expected timeout — got publish: {res:?}");
        svc.shutdown().await;
    }
}
