//! WAV loading for the offline turn-comparison tool: any hound-readable PCM or
//! float WAV is downmixed to mono and resampled to 16 kHz `i16`.
//!
//! Resampling is deliberately simple (box-average when decimating, linear
//! interpolation when upsampling). It is adequate for endpointing analysis,
//! not for audio fidelity work.

use std::path::Path;

/// Rate every downstream stage (gate and model) works at.
pub const TARGET_RATE: u32 = 16_000;

/// Decode `path` to 16 kHz mono `i16`. Refuses (never returns empty) on
/// unreadable, empty, zero-rate or unsupported-format files.
pub fn load_wav_16k(path: &Path) -> Result<Vec<i16>, String> {
    let mut reader =
        hound::WavReader::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let spec = reader.spec();
    if spec.channels == 0 || spec.sample_rate == 0 {
        return Err(format!("{}: zero channels or sample rate", path.display()));
    }
    let interleaved: Vec<f32> = match (spec.sample_format, spec.bits_per_sample) {
        (hound::SampleFormat::Int, 8..=32) => {
            let scale = (1u64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.map(|v| v as f32 / scale))
                .collect::<Result<_, _>>()
        }
        (hound::SampleFormat::Float, 32) => reader.samples::<f32>().collect::<Result<_, _>>(),
        (fmt, bits) => {
            return Err(format!(
                "{}: unsupported WAV format {fmt:?}/{bits} bit",
                path.display()
            ));
        }
    }
    .map_err(|e| format!("{}: {e}", path.display()))?;

    let mono = downmix(&interleaved, spec.channels as usize);
    let resampled = resample(&mono, spec.sample_rate, TARGET_RATE);
    if resampled.is_empty() {
        return Err(format!("{}: no audio samples", path.display()));
    }
    Ok(resampled
        .iter()
        .map(|&v| (v.clamp(-1.0, 1.0) * 32_767.0).round() as i16)
        .collect())
}

/// Average interleaved channels into one.
pub fn downmix(interleaved: &[f32], channels: usize) -> Vec<f32> {
    let ch = channels.max(1);
    interleaved
        .chunks_exact(ch)
        .map(|f| f.iter().sum::<f32>() / ch as f32)
        .collect()
}

/// Resample mono `input` from `from` Hz to `to` Hz.
pub fn resample(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    if input.is_empty() || from == 0 || to == 0 {
        return Vec::new();
    }
    if from == to {
        return input.to_vec();
    }
    let ratio = f64::from(from) / f64::from(to); // source samples per output sample
    let out_len = ((input.len() as f64) / ratio).floor() as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let start = i as f64 * ratio;
        if ratio > 1.0 {
            // Decimate: average the source span this output sample covers.
            let a = start.floor() as usize;
            let b = ((start + ratio).ceil() as usize)
                .min(input.len())
                .max(a + 1);
            out.push(input[a..b].iter().sum::<f32>() / (b - a) as f32);
        } else {
            let a = start.floor() as usize;
            let frac = (start - a as f64) as f32;
            let x0 = input[a];
            let x1 = input.get(a + 1).copied().unwrap_or(x0);
            out.push(x0 + (x1 - x0) * frac);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_wav(path: &Path, rate: u32, channels: u16, samples: &[i16]) {
        let spec = hound::WavSpec {
            channels,
            sample_rate: rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        for &s in samples {
            w.write_sample(s).unwrap();
        }
        w.finalize().unwrap();
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("smart-turn-wav-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn downmix_averages_channels() {
        assert_eq!(downmix(&[1.0, 0.0, 0.5, 0.5], 2), vec![0.5, 0.5]);
    }

    #[test]
    fn resample_lengths_scale_with_ratio() {
        let x = vec![0.25f32; 48_000];
        assert_eq!(resample(&x, 48_000, 16_000).len(), 16_000);
        assert_eq!(resample(&x[..8_000], 8_000, 16_000).len(), 16_000);
        assert_eq!(resample(&x, 16_000, 16_000).len(), 48_000);
        assert!(resample(&[], 48_000, 16_000).is_empty());
    }

    #[test]
    fn resample_preserves_dc_level() {
        let x = vec![0.5f32; 4_410];
        for v in resample(&x, 44_100, 16_000) {
            assert!((v - 0.5).abs() < 1e-5);
        }
    }

    #[test]
    fn loads_stereo_48k_as_mono_16k() {
        let dir = scratch("stereo");
        let p = dir.join("a.wav");
        // 0.5 s stereo, L=16384 R=0 -> mono 8192.
        let frames = 24_000;
        let mut s = Vec::new();
        for _ in 0..frames {
            s.push(16_384);
            s.push(0);
        }
        write_wav(&p, 48_000, 2, &s);
        let out = load_wav_16k(&p).unwrap();
        assert_eq!(out.len(), 8_000);
        assert!((i32::from(out[100]) - 8_192).abs() <= 2);
    }

    #[test]
    fn refuses_missing_and_empty_files() {
        let dir = scratch("refuse");
        assert!(load_wav_16k(&dir.join("nope.wav")).is_err());
        let p = dir.join("empty.wav");
        write_wav(&p, 16_000, 1, &[]);
        let err = load_wav_16k(&p).unwrap_err();
        assert!(err.contains("no audio samples"), "{err}");
    }
}
