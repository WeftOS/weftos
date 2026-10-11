//! Time-to-first-audio and real-time-factor measurement for [`PocketTts`]
//! (used by `examples/pocket_tts_bench.rs`; weights-free via a stub source).

use std::time::{Duration, Instant};

use clawft_channels::voice::tts::TtsEngine;
use clawft_channels::voice::types::VoiceError;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::pocket_tts::PocketTts;

/// One synthesis run.
#[derive(Debug, Clone, PartialEq)]
pub struct BenchReport {
    /// Request start to first PCM chunk.
    pub ttfa: Duration,
    /// Request start to last chunk.
    pub wall: Duration,
    /// Seconds of audio produced.
    pub audio_secs: f64,
    /// `wall / audio` — below 1.0 is faster than real time.
    pub rtf: f64,
    pub chunks: usize,
}

/// Synthesize `text` once and measure. Refuses an empty result: a run that
/// produced no audio is an error, not a 0 ms TTFA.
pub async fn bench_once(engine: &PocketTts, text: &str) -> Result<BenchReport, VoiceError> {
    let (tx, mut rx) = mpsc::channel(64);
    let start = Instant::now();
    let eng = engine.clone();
    let text = text.to_string();
    let run = tokio::spawn(async move {
        eng.synthesize_stream(&text, tx, CancellationToken::new()).await
    });
    let (mut ttfa, mut samples, mut chunks, mut rate) = (None, 0usize, 0usize, 0u32);
    let mut last = start;
    while let Some(c) = rx.recv().await {
        last = Instant::now();
        ttfa.get_or_insert(last - start);
        samples += c.samples.len();
        rate = c.sample_rate;
        chunks += 1;
    }
    run.await.map_err(|e| VoiceError::Audio(format!("bench task: {e}")))??;
    let ttfa = ttfa.ok_or_else(|| VoiceError::Audio("bench produced no audio".into()))?;
    let wall = last - start;
    let audio_secs = samples as f64 / f64::from(rate);
    Ok(BenchReport { ttfa, wall, audio_secs, rtf: wall.as_secs_f64() / audio_secs, chunks })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pocket_tts::tests::stub;

    #[tokio::test]
    async fn reports_ttfa_and_rtf_for_a_stub() {
        let (eng, _p, _d) = stub(10);
        let r = bench_once(&eng, "hello world").await.unwrap();
        assert_eq!(r.chunks, 10);
        assert!((r.audio_secs - 0.8).abs() < 1e-9);
        assert!(r.ttfa <= r.wall && r.rtf > 0.0);
    }

    #[tokio::test]
    async fn no_audio_is_refused() {
        let (eng, _p, _d) = stub(0);
        assert!(bench_once(&eng, "hello").await.is_err());
    }
}
