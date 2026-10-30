//! `weftos-sensor-pipeline` — collect → aggregate → encode → signed wire
//! frames for LeWM (WEFT-523).
//!
//! Stages match the symposium diagram sensor plane:
//!
//! 1. **Collect** — raw samples with novelty / quality gates  
//! 2. **Aggregate** — multi-sensor fusion buffer  
//! 3. **Encode** — sensor → latent via [`weftos_worldmodel::Encoder`]  
//! 4. **Sign** — emit [`weftos_sensor_pipeline_wire::SignedSensorFrame`]
//!
//! The pipeline is weight-free by default ([`HashEncoder`]/ [`NullEncoder`]).
//! Identical wire format whether run in-process or as a fan-in service.

#![deny(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]

use ed25519_dalek::SigningKey;
use thiserror::Error;
use weftos_sensor_pipeline_wire::{
    ConsensusFrame, ControlFrame, ControlKind, EncodedObservation, SignedSensorFrame, WireError,
};
use weftos_worldmodel::{
    zero_latent, Encoder, HashEncoder, Latent, NullEncoder, LATENT_DIM,
};

/// Crate version for diagnostics.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Pipeline-stage errors.
#[derive(Debug, Error)]
pub enum PipelineError {
    /// Collection gate rejected the sample.
    #[error("collect gate rejected sample: {0}")]
    GateRejected(&'static str),
    /// No samples available to aggregate / encode.
    #[error("empty aggregation buffer")]
    EmptyBuffer,
    /// Encoder step failed.
    #[error("encode failed")]
    EncodeFailed,
    /// Wire framing / signing failed.
    #[error(transparent)]
    Wire(#[from] WireError),
}

/// Result alias for pipeline operations.
pub type PipelineResult<T> = Result<T, PipelineError>;

/// One raw sensor sample entering the collect stage.
#[derive(Debug, Clone)]
pub struct RawSample {
    /// Sensor class label (`rgb`, `imu`, `audio`, …).
    pub sensor_class: String,
    /// Raw observation bytes (encoder-interpreted).
    pub bytes: Vec<u8>,
    /// Host / mesh timestamp in milliseconds.
    pub timestamp_ms: u64,
    /// Quality score in `[0, 1]` (transmit gate).
    pub quality: f32,
    /// Novelty score in `[0, 1]` (transmit gate).
    pub novelty: f32,
}

impl RawSample {
    /// Construct a sample with quality/novelty defaults of `1.0`.
    pub fn new(
        sensor_class: impl Into<String>,
        bytes: impl Into<Vec<u8>>,
        timestamp_ms: u64,
    ) -> Self {
        Self {
            sensor_class: sensor_class.into(),
            bytes: bytes.into(),
            timestamp_ms,
            quality: 1.0,
            novelty: 1.0,
        }
    }
}

/// Transmit-gate thresholds for the collect stage.
#[derive(Debug, Clone, Copy)]
pub struct CollectGate {
    /// Minimum quality to accept.
    pub min_quality: f32,
    /// Minimum novelty to accept (0.0 disables novelty filter).
    pub min_novelty: f32,
}

impl Default for CollectGate {
    fn default() -> Self {
        Self {
            min_quality: 0.1,
            min_novelty: 0.0,
        }
    }
}

/// Sensor pipeline configuration.
#[derive(Debug, Clone)]
pub struct PipelineConfig {
    /// Mesh cluster name (topic suffix).
    pub cluster: String,
    /// Emitting node id.
    pub node_id: u64,
    /// Collect gate.
    pub gate: CollectGate,
    /// Max samples retained in the aggregate buffer.
    pub max_buffer: usize,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            cluster: "default".into(),
            node_id: 0,
            gate: CollectGate::default(),
            max_buffer: 32,
        }
    }
}

/// Aggregated multi-sensor frame ready for encode.
#[derive(Debug, Clone)]
pub struct AggregatedFrame {
    /// Fused payload (class-tagged concatenation for the stub encoder).
    pub bytes: Vec<u8>,
    /// Dominant / first sensor class.
    pub sensor_class: String,
    /// Latest sample timestamp.
    pub timestamp_ms: u64,
    /// Mean quality across samples.
    pub quality: f32,
    /// Burst episode id (monotonic per pipeline).
    pub burst_episode_id: u64,
}

/// In-process sensor pipeline: collect → aggregate → encode → sign.
///
/// Generic over the encoder so hosts can swap null / hash / candle later.
#[derive(Debug)]
pub struct SensorPipeline<E: Encoder = HashEncoder> {
    /// Pipeline configuration.
    pub config: PipelineConfig,
    /// Sensor → latent encoder.
    pub encoder: E,
    /// Ed25519 signing key for wire frames.
    signing_key: SigningKey,
    /// Pending collected samples.
    buffer: Vec<RawSample>,
    /// Monotonic burst id.
    next_burst: u64,
}

impl SensorPipeline<HashEncoder> {
    /// New pipeline with deterministic hash encoder (weight-free demos).
    pub fn with_hash(config: PipelineConfig, signing_key: SigningKey) -> Self {
        Self::new(config, HashEncoder, signing_key)
    }
}

impl SensorPipeline<NullEncoder> {
    /// New pipeline with null (zero-latent) encoder.
    pub fn with_null(config: PipelineConfig, signing_key: SigningKey) -> Self {
        Self::new(config, NullEncoder, signing_key)
    }
}

impl<E: Encoder> SensorPipeline<E> {
    /// Construct a pipeline around an arbitrary encoder.
    pub fn new(config: PipelineConfig, encoder: E, signing_key: SigningKey) -> Self {
        Self {
            config,
            encoder,
            signing_key,
            buffer: Vec::new(),
            next_burst: 1,
        }
    }

    /// ❶ Collect: apply transmit gate and push into the buffer.
    pub fn collect(&mut self, sample: RawSample) -> PipelineResult<()> {
        if sample.quality < self.config.gate.min_quality {
            return Err(PipelineError::GateRejected("quality below threshold"));
        }
        if sample.novelty < self.config.gate.min_novelty {
            return Err(PipelineError::GateRejected("novelty below threshold"));
        }
        if self.buffer.len() >= self.config.max_buffer {
            self.buffer.remove(0);
        }
        self.buffer.push(sample);
        Ok(())
    }

    /// Number of samples currently buffered.
    pub fn buffered(&self) -> usize {
        self.buffer.len()
    }

    /// ❷ Aggregate: fuse buffered samples into one frame and clear the buffer.
    pub fn aggregate(&mut self) -> PipelineResult<AggregatedFrame> {
        if self.buffer.is_empty() {
            return Err(PipelineError::EmptyBuffer);
        }
        let burst_episode_id = self.next_burst;
        self.next_burst = self.next_burst.saturating_add(1);

        let count = self.buffer.len() as f32;
        let mut bytes = Vec::new();
        let mut quality_sum = 0.0f32;
        let mut timestamp_ms = 0u64;
        let sensor_class = self.buffer[0].sensor_class.clone();

        for s in self.buffer.drain(..) {
            // Multi-sensor fusion: class tag + length-prefixed payload.
            bytes.extend_from_slice(s.sensor_class.as_bytes());
            bytes.push(0);
            let len = (s.bytes.len() as u32).to_le_bytes();
            bytes.extend_from_slice(&len);
            bytes.extend_from_slice(&s.bytes);
            quality_sum += s.quality;
            timestamp_ms = timestamp_ms.max(s.timestamp_ms);
        }

        Ok(AggregatedFrame {
            bytes,
            sensor_class,
            timestamp_ms,
            quality: quality_sum / count,
            burst_episode_id,
        })
    }

    /// ❸ Encode: run the encoder on an aggregated frame → latent.
    pub fn encode(&self, frame: &AggregatedFrame) -> PipelineResult<Latent> {
        self.encoder
            .encode(&frame.bytes)
            .map_err(|_| PipelineError::EncodeFailed)
    }

    /// ❸+sign: encode and emit a signed `mesh.sensor.v1.encoded` frame.
    pub fn encode_and_sign(&self, frame: &AggregatedFrame) -> PipelineResult<SignedSensorFrame> {
        let z = self.encode(frame)?;
        let obs = EncodedObservation::v1(
            z.to_vec(),
            frame.quality,
            frame.burst_episode_id,
            frame.sensor_class.clone(),
        )?;
        let signed = SignedSensorFrame::sign_encoded(
            &obs,
            &self.config.cluster,
            frame.timestamp_ms,
            self.config.node_id,
            &self.signing_key,
        )?;
        signed.check_size_budget()?;
        Ok(signed)
    }

    /// One-shot: collect all samples already buffered via prior [`collect`],
    /// aggregate, encode, and sign. Convenience for tests / weak-sensor fan-in.
    pub fn flush_encoded(&mut self) -> PipelineResult<SignedSensorFrame> {
        let agg = self.aggregate()?;
        self.encode_and_sign(&agg)
    }

    /// Emit a consensus frame (world-model path; optional).
    pub fn sign_consensus(
        &self,
        z_cluster: Latent,
        z_hat_next: Latent,
        health: f32,
        timestamp_ms: u64,
    ) -> PipelineResult<SignedSensorFrame> {
        let body = ConsensusFrame::v1(z_cluster.to_vec(), z_hat_next.to_vec(), health)?;
        Ok(SignedSensorFrame::sign_consensus(
            &body,
            &self.config.cluster,
            timestamp_ms,
            self.config.node_id,
            &self.signing_key,
        )?)
    }

    /// Emit a control-plane frame (drift / surprise / plan).
    pub fn sign_control(
        &self,
        kind: ControlKind,
        magnitude: f32,
        timestamp_ms: u64,
        detail: Vec<u8>,
    ) -> PipelineResult<SignedSensorFrame> {
        let body = ControlFrame {
            kind,
            magnitude,
            node_id: self.config.node_id,
            detail: weftos_sensor_pipeline_wire::ByteString::new(detail),
        };
        Ok(SignedSensorFrame::sign_control(
            &body,
            &self.config.cluster,
            timestamp_ms,
            self.config.node_id,
            &self.signing_key,
        )?)
    }

    /// Access the signing key's public bytes.
    pub fn public_key_bytes(&self) -> Vec<u8> {
        self.signing_key.verifying_key().to_bytes().to_vec()
    }
}

/// Encode a latent slice into a fixed array (helper for hosts).
pub fn latent_from_slice(v: &[f32]) -> Option<Latent> {
    if v.len() != LATENT_DIM {
        return None;
    }
    let mut z = zero_latent();
    z.copy_from_slice(v);
    Some(z)
}

// ── Mesh topic publish path (WEFT-526) ──────────────────────────────────────

/// Sink for publishing signed sensor frames onto the mesh topic bus.
///
/// Kernel hosts implement this over `clawft_kernel::mesh_sensor::SensorTopicBus`;
/// tests use [`InMemorySensorBus`].
pub trait SensorTopicSink {
    /// Publish CBOR of a signed frame on the given topic string.
    fn publish_sensor_frame(
        &mut self,
        topic: &str,
        cluster: &str,
        node_id: u64,
        timestamp_ms: u64,
        cbor: &[u8],
    ) -> Result<u64, PipelineError>;
}

/// One indexed publish record (ExoChain stand-in for offline tests).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SensorBusRecord {
    /// Assigned sequence.
    pub seq: u64,
    /// Full topic (with cluster).
    pub topic: String,
    /// Cluster name.
    pub cluster: String,
    /// Node id.
    pub node_id: u64,
    /// Timestamp ms.
    pub timestamp_ms: u64,
    /// CBOR payload bytes.
    pub cbor: Vec<u8>,
}

/// In-memory mesh sensor bus: subscriber queues + ExoChain-like index.
#[derive(Debug, Default)]
pub struct InMemorySensorBus {
    /// Indexed publishes in order.
    pub records: Vec<SensorBusRecord>,
    /// Local subscriber queues by full topic name.
    subs: std::collections::HashMap<String, Vec<Vec<u8>>>,
    next_seq: u64,
}

impl InMemorySensorBus {
    /// Empty bus.
    pub fn new() -> Self {
        Self::default()
    }

    /// Subscribe to a full topic name (buffers publishes).
    pub fn subscribe(&mut self, topic: &str) {
        self.subs.entry(topic.to_string()).or_default();
    }

    /// Drain buffered frames for a topic.
    pub fn drain(&mut self, topic: &str) -> Vec<Vec<u8>> {
        self.subs
            .get_mut(topic)
            .map(std::mem::take)
            .unwrap_or_default()
    }

    /// Number of indexed records.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// True when no publishes have been indexed.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

impl SensorTopicSink for InMemorySensorBus {
    fn publish_sensor_frame(
        &mut self,
        topic: &str,
        cluster: &str,
        node_id: u64,
        timestamp_ms: u64,
        cbor: &[u8],
    ) -> Result<u64, PipelineError> {
        let full = if cluster.is_empty() || topic.ends_with(cluster) {
            topic.to_string()
        } else {
            format!("{topic}.{cluster}")
        };
        if let Some(q) = self.subs.get_mut(&full) {
            q.push(cbor.to_vec());
        }
        // Also deliver on bare topic when cluster-suffixed.
        if full != topic
            && let Some(q) = self.subs.get_mut(topic) {
                q.push(cbor.to_vec());
            }
        let seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        self.records.push(SensorBusRecord {
            seq,
            topic: full,
            cluster: cluster.to_string(),
            node_id,
            timestamp_ms,
            cbor: cbor.to_vec(),
        });
        Ok(seq)
    }
}

impl<E: Encoder> SensorPipeline<E> {
    /// Publish a signed frame onto a [`SensorTopicSink`] (mesh + ExoChain index).
    pub fn publish_frame<S: SensorTopicSink>(
        &self,
        sink: &mut S,
        frame: &SignedSensorFrame,
    ) -> PipelineResult<u64> {
        let cbor = frame.to_cbor()?;
        sink.publish_sensor_frame(
            frame.topic.as_str(),
            &frame.cluster,
            frame.node_id,
            frame.timestamp_ms,
            &cbor,
        )
    }

    /// Produce and publish one frame on each of the three mesh.sensor.v1 topics.
    ///
    /// Wire-level integration helper for WEFT-526: encoded (from buffer),
    /// consensus, and control — each signed, published, and indexed.
    // Public wire-integration helper with a single test caller; a params struct would change the public API.
    #[allow(clippy::too_many_arguments)]
    pub fn publish_all_topics<S: SensorTopicSink>(
        &mut self,
        sink: &mut S,
        z_cluster: Latent,
        z_hat_next: Latent,
        health: f32,
        control_kind: ControlKind,
        control_magnitude: f32,
        timestamp_ms: u64,
    ) -> PipelineResult<[u64; 3]> {
        if self.buffered() == 0 {
            // Synthesize a minimal sample so encoded has content.
            self.collect(RawSample::new("synthetic", b"mesh-wire", timestamp_ms))?;
        }
        let encoded = self.flush_encoded()?;
        let cons = self.sign_consensus(z_cluster, z_hat_next, health, timestamp_ms)?;
        let ctrl = self.sign_control(control_kind, control_magnitude, timestamp_ms, vec![])?;

        let s0 = self.publish_frame(sink, &encoded)?;
        let s1 = self.publish_frame(sink, &cons)?;
        let s2 = self.publish_frame(sink, &ctrl)?;
        Ok([s0, s1, s2])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use weftos_sensor_pipeline_wire::{SensorTopic, TOPIC_CONSENSUS, TOPIC_CONTROL, TOPIC_ENCODED};

    fn key() -> SigningKey {
        SigningKey::from_bytes(&[9u8; 32])
    }

    fn pipeline() -> SensorPipeline<HashEncoder> {
        let cfg = PipelineConfig {
            cluster: "test".into(),
            node_id: 42,
            ..PipelineConfig::default()
        };
        SensorPipeline::with_hash(cfg, key())
    }

    #[test]
    fn collect_gate_rejects_low_quality() {
        let mut p = pipeline();
        let mut s = RawSample::new("rgb", b"frame", 1);
        s.quality = 0.0;
        assert!(matches!(
            p.collect(s),
            Err(PipelineError::GateRejected(_))
        ));
        assert_eq!(p.buffered(), 0);
    }

    #[test]
    fn collect_aggregate_encode_sign_roundtrip() {
        let mut p = pipeline();
        p.collect(RawSample::new("rgb", b"hello-rgb", 100))
            .unwrap();
        p.collect(RawSample::new("imu", b"hello-imu", 110))
            .unwrap();
        assert_eq!(p.buffered(), 2);

        let signed = p.flush_encoded().unwrap();
        signed.verify().unwrap();
        assert_eq!(signed.topic, SensorTopic::Encoded);
        assert_eq!(signed.topic.as_str(), TOPIC_ENCODED);
        assert_eq!(signed.node_id, 42);

        let obs = signed.decode_encoded().unwrap();
        let latent = obs.latent().unwrap();
        assert_eq!(latent.len(), LATENT_DIM);
        assert_eq!(obs.burst_episode_id, 1);
        // Hash encoder ⇒ non-zero latent for non-empty input.
        assert_ne!(latent, vec![0.0; LATENT_DIM]);
        assert!(obs.quality > 0.0);
        assert!(signed.to_cbor().unwrap().len() <= weftos_sensor_pipeline_wire::MAX_ENCODED_FRAME_BYTES);
        assert_eq!(p.buffered(), 0);
    }

    #[test]
    fn empty_aggregate_errors() {
        let mut p = pipeline();
        assert!(matches!(p.aggregate(), Err(PipelineError::EmptyBuffer)));
    }

    #[test]
    fn consensus_and_control_topics() {
        let p = pipeline();
        let z = zero_latent();
        let mut z2 = zero_latent();
        z2[0] = 0.5;

        let cons = p.sign_consensus(z, z2, 0.88, 200).unwrap();
        cons.verify().unwrap();
        assert_eq!(cons.topic.as_str(), TOPIC_CONSENSUS);
        let body = cons.decode_consensus().unwrap();
        assert!((body.health - 0.88).abs() < f32::EPSILON);

        let ctrl = p
            .sign_control(ControlKind::Surprise, 1.25, 300, b"voe".to_vec())
            .unwrap();
        ctrl.verify().unwrap();
        assert_eq!(ctrl.topic.as_str(), TOPIC_CONTROL);
        assert_eq!(ctrl.decode_control().unwrap().kind, ControlKind::Surprise);
    }

    #[test]
    fn three_topics_from_pipeline() {
        let mut p = pipeline();
        p.collect(RawSample::new("rgb", b"x", 1)).unwrap();
        let enc = p.flush_encoded().unwrap();
        let cons = p
            .sign_consensus(zero_latent(), zero_latent(), 1.0, 2)
            .unwrap();
        let ctrl = p
            .sign_control(ControlKind::Drift, 0.1, 3, vec![])
            .unwrap();

        for (f, topic) in [
            (enc, TOPIC_ENCODED),
            (cons, TOPIC_CONSENSUS),
            (ctrl, TOPIC_CONTROL),
        ] {
            let bytes = f.to_cbor().unwrap();
            let back = SignedSensorFrame::from_cbor(&bytes).unwrap();
            assert_eq!(back.topic.as_str(), topic);
        }
    }

    #[test]
    fn null_encoder_emits_zero_latent() {
        let cfg = PipelineConfig::default();
        let mut p = SensorPipeline::with_null(cfg, key());
        p.collect(RawSample::new("audio", b"pcm", 1)).unwrap();
        let signed = p.flush_encoded().unwrap();
        let obs = signed.decode_encoded().unwrap();
        assert_eq!(obs.latent().unwrap(), vec![0.0; LATENT_DIM]);
    }

    #[test]
    fn publish_all_three_topics_to_mesh_bus() {
        use weftos_sensor_pipeline_wire::{TOPIC_CONSENSUS, TOPIC_CONTROL, TOPIC_ENCODED};

        let mut p = pipeline();
        p.collect(RawSample::new("rgb", b"frame", 50)).unwrap();

        let mut bus = InMemorySensorBus::new();
        for t in [TOPIC_ENCODED, TOPIC_CONSENSUS, TOPIC_CONTROL] {
            bus.subscribe(&format!("{t}.test"));
        }

        let seqs = p
            .publish_all_topics(
                &mut bus,
                zero_latent(),
                zero_latent(),
                0.92,
                ControlKind::PlanResult,
                0.5,
                50,
            )
            .unwrap();
        assert_eq!(seqs, [0, 1, 2]);
        assert_eq!(bus.len(), 3);

        // Subscriber path received one CBOR frame per topic; each verifies.
        for (topic, _kind) in [
            (TOPIC_ENCODED, SensorTopic::Encoded),
            (TOPIC_CONSENSUS, SensorTopic::Consensus),
            (TOPIC_CONTROL, SensorTopic::Control),
        ] {
            let full = format!("{topic}.test");
            let frames = bus.drain(&full);
            assert_eq!(frames.len(), 1, "{full}");
            let signed = SignedSensorFrame::from_cbor(&frames[0]).unwrap();
            assert_eq!(signed.topic.as_str(), topic);
        }

        // ExoChain index covers all three.
        let topics: Vec<_> = bus.records.iter().map(|r| r.topic.clone()).collect();
        assert!(topics.iter().any(|t| t.contains("encoded")));
        assert!(topics.iter().any(|t| t.contains("consensus")));
        assert!(topics.iter().any(|t| t.contains("control")));
    }

    #[test]
    fn latent_from_slice_checks_length() {
        let ok = [0.5f32; LATENT_DIM];
        assert_eq!(latent_from_slice(&ok).unwrap(), ok);
        assert!(latent_from_slice(&[0.5f32; LATENT_DIM - 1]).is_none());
        assert!(latent_from_slice(&[0.5f32; LATENT_DIM + 1]).is_none());
        assert!(latent_from_slice(&[]).is_none());
    }
}
