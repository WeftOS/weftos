//! Pocket TTS fast-layer engine (WEFT-657 trial) — in-process streaming TTS via
//! the Gradium `ptts` crate. **No Python, no network at runtime.**
//!
//! A sibling to [`crate::kokoro`] behind the default-off `pocket-tts` feature.
//! `ptts` runs the flow-matching LM and the Mimi decoder on two threads and hands
//! back PCM as an iterator; **barge-in is dropping that iterator** (the workers
//! stop and are joined). Model and voice paths come from [`PocketTtsConfig`]
//! (or `WEFTOS_POCKET_TTS_DIR` / `WEFTOS_POCKET_TTS_VOICE`); nothing is ever
//! fetched, and an absent model is a clear [`VoiceError::Config`].
//!
//! Everything except the `ptts` binding compiles without the feature: the
//! engine is written against the small [`PcmSource`] seam so the config parsing,
//! chunking and cancel semantics are tested with a stub and no weights.
//!
//! Weights are separate from this crate's licence: Pocket TTS weights are gated
//! CC-BY-4.0 and voices carry their own licences. See
//! `docs/guides/pocket-tts-trial.md`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use clawft_channels::voice::tts::{TtsChunk, TtsEngine, TtsTier, scrub_tags};
use clawft_channels::voice::types::VoiceError;
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Env override for the model directory (config.json + weights + voices).
pub const MODEL_DIR_ENV: &str = "WEFTOS_POCKET_TTS_DIR";
/// Env override for the voice name.
pub const VOICE_ENV: &str = "WEFTOS_POCKET_TTS_VOICE";

const LANGUAGES: [&str; 5] = ["en", "fr", "de", "es", "pt"];
const DEVICES: [&str; 5] = ["auto", "cpu", "cuda", "vulkan", "metal"];

/// Pocket TTS settings. Unknown keys are refused.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PocketTtsConfig {
    /// Checkpoint directory (or its `config.json`). Required.
    pub model_dir: Option<PathBuf>,
    /// Registered voice name; `None` uses the checkpoint's default voice.
    pub voice: Option<String>,
    /// Extra voice embedding (`.safetensors`) registered under `voice`.
    pub voice_file: Option<PathBuf>,
    /// `en|fr|de|es|pt`.
    pub language: String,
    /// `auto|cpu|cuda|vulkan|metal`.
    pub device: String,
    /// ptts weight format (`f32`, `q8_0`, ...), parsed by ptts at load.
    pub quant: String,
}

impl Default for PocketTtsConfig {
    fn default() -> Self {
        Self {
            model_dir: None,
            voice: None,
            voice_file: None,
            language: "en".into(),
            device: "cpu".into(),
            quant: "f32".into(),
        }
    }
}

impl PocketTtsConfig {
    /// Parse a JSON object, refusing unknown keys and invalid values.
    pub fn from_json(text: &str) -> Result<Self, VoiceError> {
        let cfg: Self = serde_json::from_str(text)
            .map_err(|e| VoiceError::Config(format!("pocket-tts config: {e}")))?;
        cfg.validate_values()?;
        Ok(cfg)
    }

    /// Defaults overlaid with the `WEFTOS_POCKET_TTS_*` environment.
    pub fn from_env() -> Self {
        Self {
            model_dir: std::env::var_os(MODEL_DIR_ENV).map(PathBuf::from),
            voice: std::env::var(VOICE_ENV).ok().filter(|v| !v.is_empty()),
            ..Self::default()
        }
    }

    /// Value checks that need no model on disk.
    pub fn validate_values(&self) -> Result<(), VoiceError> {
        if !LANGUAGES.contains(&self.language.as_str()) {
            return Err(VoiceError::Config(format!(
                "pocket-tts language '{}' unknown; expected one of {}",
                self.language,
                LANGUAGES.join(", ")
            )));
        }
        if !DEVICES.contains(&self.device.as_str()) {
            return Err(VoiceError::Config(format!(
                "pocket-tts device '{}' unknown; expected one of {}",
                self.device,
                DEVICES.join(", ")
            )));
        }
        Ok(())
    }

    /// Values plus the model path: refuses when absent or missing on disk.
    pub fn validate(&self) -> Result<&Path, VoiceError> {
        self.validate_values()?;
        let dir = self.model_dir.as_deref().ok_or_else(|| {
            VoiceError::Config(format!(
                "pocket-tts model_dir not set (config or ${MODEL_DIR_ENV}); weights are never downloaded"
            ))
        })?;
        if !dir.exists() {
            return Err(VoiceError::Config(format!(
                "pocket-tts model not found at {}",
                dir.display()
            )));
        }
        if let Some(vf) = &self.voice_file
            && !vf.is_file()
        {
            return Err(VoiceError::Config(format!(
                "pocket-tts voice_file not found at {}",
                vf.display()
            )));
        }
        Ok(dir)
    }

    /// True when the config names an existing model (cheap readiness probe).
    pub fn is_ready(&self) -> bool {
        cfg!(feature = "pocket-tts") && self.validate().is_ok()
    }
}

/// A running generation: dropping it stops generation.
pub trait PcmStream: Iterator<Item = Result<Vec<f32>, String>> + Send {}
impl<T: Iterator<Item = Result<Vec<f32>, String>> + Send> PcmStream for T {}

/// Where PCM comes from. The real impl wraps `ptts`; tests supply a stub.
pub trait PcmSource: Send + Sync {
    fn sample_rate(&self) -> u32;
    /// Start generating; the returned stream stops when dropped.
    fn open(&self, text: &str) -> Result<Box<dyn PcmStream>, String>;
}

/// Pocket TTS engine (`TtsTier::Fast`).
#[derive(Clone)]
pub struct PocketTts {
    source: Arc<dyn PcmSource>,
}

impl PocketTts {
    /// Load the model named by `cfg`. Errors (never returns an unusable
    /// engine) when the path is absent or the `pocket-tts` feature is off.
    pub fn load(cfg: &PocketTtsConfig) -> Result<Self, VoiceError> {
        cfg.validate()?;
        #[cfg(feature = "pocket-tts")]
        {
            Ok(Self::from_source(Arc::new(crate::pocket_tts_ptts::PtSource::load(cfg)?)))
        }
        #[cfg(not(feature = "pocket-tts"))]
        {
            Err(VoiceError::Config(
                "clawft-voice-tts built without the `pocket-tts` feature".into(),
            ))
        }
    }

    /// Wrap any [`PcmSource`] (tests, alternative backends).
    pub fn from_source(source: Arc<dyn PcmSource>) -> Self {
        Self { source }
    }

    /// Output sample rate in Hz.
    pub fn sample_rate(&self) -> u32 {
        self.source.sample_rate()
    }
}

fn to_i16(pcm: &[f32]) -> Vec<i16> {
    pcm.iter().map(|s| (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16).collect()
}

#[async_trait]
impl TtsEngine for PocketTts {
    async fn synthesize_stream(
        &self,
        text: &str,
        tx: mpsc::Sender<TtsChunk>,
        cancel: CancellationToken,
    ) -> Result<(), VoiceError> {
        let text = scrub_tags(text);
        if text.trim().is_empty() {
            return Ok(());
        }
        let source = self.source.clone();
        let rate = source.sample_rate();
        // ptts iteration blocks; run it off the async threads. Cancel and a gone
        // receiver both end the loop, and dropping `stream` stops generation.
        tokio::task::spawn_blocking(move || -> Result<(), VoiceError> {
            if cancel.is_cancelled() {
                return Ok(());
            }
            let mut stream = source.open(&text).map_err(VoiceError::Audio)?;
            for item in stream.by_ref() {
                if cancel.is_cancelled() {
                    break;
                }
                let pcm = item.map_err(VoiceError::Audio)?;
                let mut chunk = Some(TtsChunk { samples: to_i16(&pcm), sample_rate: rate });
                // try_send + poll so a full queue never pins us past a cancel.
                while let Some(c) = chunk.take() {
                    match tx.try_send(c) {
                        Ok(()) => {}
                        Err(mpsc::error::TrySendError::Closed(_)) => return Ok(()),
                        Err(mpsc::error::TrySendError::Full(c)) => {
                            if cancel.is_cancelled() {
                                return Ok(());
                            }
                            chunk = Some(c);
                            std::thread::sleep(Duration::from_millis(2));
                        }
                    }
                }
            }
            drop(stream);
            Ok(())
        })
        .await
        .map_err(|e| VoiceError::Audio(format!("pocket-tts worker: {e}")))?
    }

    fn tier(&self) -> TtsTier {
        TtsTier::Fast
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Instant;

    /// Endless 80 ms frames at 24 kHz; records production and drop.
    pub(crate) struct StubSource {
        pub produced: Arc<AtomicUsize>,
        pub dropped: Arc<AtomicBool>,
        pub limit: usize,
    }
    struct StubStream {
        n: usize,
        limit: usize,
        produced: Arc<AtomicUsize>,
        dropped: Arc<AtomicBool>,
    }
    impl Iterator for StubStream {
        type Item = Result<Vec<f32>, String>;
        fn next(&mut self) -> Option<Self::Item> {
            if self.n >= self.limit {
                return None;
            }
            self.n += 1;
            self.produced.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(5));
            Some(Ok(vec![0.5; 1920]))
        }
    }
    impl Drop for StubStream {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }
    impl PcmSource for StubSource {
        fn sample_rate(&self) -> u32 {
            24_000
        }
        fn open(&self, _text: &str) -> Result<Box<dyn PcmStream>, String> {
            Ok(Box::new(StubStream {
                n: 0,
                limit: self.limit,
                produced: self.produced.clone(),
                dropped: self.dropped.clone(),
            }))
        }
    }
    pub(crate) fn stub(limit: usize) -> (PocketTts, Arc<AtomicUsize>, Arc<AtomicBool>) {
        let produced = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicBool::new(false));
        let src = StubSource { produced: produced.clone(), dropped: dropped.clone(), limit };
        (PocketTts::from_source(Arc::new(src)), produced, dropped)
    }

    #[test]
    fn config_parses_and_defaults() {
        let c = PocketTtsConfig::from_json(
            r#"{"model_dir":"/m","voice":"alba","language":"fr","device":"metal"}"#,
        )
        .unwrap();
        assert_eq!(c.model_dir.as_deref(), Some(Path::new("/m")));
        assert_eq!(c.voice.as_deref(), Some("alba"));
        assert_eq!((c.language.as_str(), c.device.as_str(), c.quant.as_str()), ("fr", "metal", "f32"));
        assert_eq!(PocketTtsConfig::from_json("{}").unwrap(), PocketTtsConfig::default());
    }

    #[test]
    fn config_refuses_bad_input() {
        for bad in [r#"{"bogus":1}"#, r#"{"language":"xx"}"#, r#"{"device":"tpu"}"#, "[1]"] {
            assert!(matches!(PocketTtsConfig::from_json(bad), Err(VoiceError::Config(_))), "{bad}");
        }
    }

    #[test]
    fn absent_model_is_a_clear_error() {
        let e = PocketTtsConfig::default().validate().unwrap_err().to_string();
        assert!(e.contains("model_dir not set") && e.contains("never downloaded"), "{e}");
        let c = PocketTtsConfig {
            model_dir: Some("/definitely/not/here".into()),
            ..Default::default()
        };
        assert!(c.validate().unwrap_err().to_string().contains("not found"));
        assert!(!c.is_ready());
        assert!(PocketTts::load(&c).is_err());
    }

    #[tokio::test]
    async fn streams_chunks_with_rate_and_tier() {
        let (eng, produced, dropped) = stub(3);
        assert_eq!(eng.tier(), TtsTier::Fast);
        let (tx, mut rx) = mpsc::channel(8);
        eng.synthesize_stream("Hello <laugh> there", tx, CancellationToken::new()).await.unwrap();
        let mut n = 0;
        while let Some(c) = rx.recv().await {
            assert_eq!((c.sample_rate, c.samples.len()), (24_000, 1920));
            n += 1;
        }
        assert_eq!((n, produced.load(Ordering::SeqCst)), (3, 3));
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn cancel_drops_the_generator_promptly() {
        let (eng, produced, dropped) = stub(1_000_000);
        let (tx, mut rx) = mpsc::channel(2);
        let cancel = CancellationToken::new();
        let c2 = cancel.clone();
        let task = tokio::spawn(async move { eng.synthesize_stream("go on", tx, c2).await });
        rx.recv().await.expect("first chunk");
        let t = Instant::now();
        cancel.cancel();
        task.await.unwrap().unwrap();
        assert!(t.elapsed() < Duration::from_millis(500), "{:?}", t.elapsed());
        assert!(dropped.load(Ordering::SeqCst), "generator not dropped");
        // Bounded by queue depth + a step or two, nowhere near the limit.
        assert!(produced.load(Ordering::SeqCst) < 20);
    }

    #[tokio::test]
    async fn gone_receiver_stops_generation() {
        let (eng, _p, dropped) = stub(1_000_000);
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        eng.synthesize_stream("hello", tx, CancellationToken::new()).await.unwrap();
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn markup_only_text_renders_nothing() {
        let (eng, produced, _d) = stub(5);
        let (tx, mut rx) = mpsc::channel(4);
        eng.synthesize_stream("<laugh>", tx, CancellationToken::new()).await.unwrap();
        assert!(rx.recv().await.is_none());
        assert_eq!(produced.load(Ordering::SeqCst), 0);
    }

    #[test]
    #[ignore = "needs real Pocket TTS weights: set WEFTOS_POCKET_TTS_DIR (build with --features pocket-tts)"]
    fn real_model_loads() {
        let cfg = PocketTtsConfig::from_env();
        let eng = PocketTts::load(&cfg).expect("load");
        assert_eq!(eng.sample_rate(), 24_000);
    }
}
