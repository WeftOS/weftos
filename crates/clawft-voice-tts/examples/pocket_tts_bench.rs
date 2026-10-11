//! Pocket TTS bench: time-to-first-audio and real-time factor.
//!
//! ```text
//! cargo run --release -p clawft-voice-tts \
//!   --features pocket-tts --example pocket_tts_bench -- <model dir> [voice] [text]
//! ```
//! Needs real weights on disk; nothing is downloaded.

use clawft_voice_tts::pocket_tts::{PocketTts, PocketTtsConfig};
use clawft_voice_tts::pocket_tts_bench::bench_once;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let mut cfg = PocketTtsConfig::from_env();
    if let Some(dir) = args.next() {
        cfg.model_dir = Some(dir.into());
    }
    if let Some(v) = args.next() {
        cfg.voice = Some(v);
    }
    let text = args.next().unwrap_or_else(|| {
        "The quick brown fox jumps over the lazy dog. Then it sat down.".to_string()
    });
    let t = std::time::Instant::now();
    let engine = PocketTts::load(&cfg)?;
    println!("load: {:.2}s", t.elapsed().as_secs_f64());
    // First run warms caches; report both.
    for label in ["cold", "warm"] {
        let r = bench_once(&engine, &text).await?;
        println!(
            "{label}: ttfa={:.0}ms wall={:.2}s audio={:.2}s rtf={:.3} chunks={}",
            r.ttfa.as_secs_f64() * 1e3,
            r.wall.as_secs_f64(),
            r.audio_secs,
            r.rtf,
            r.chunks
        );
    }
    Ok(())
}
