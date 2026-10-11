//! Pipeline tests with synthetic audio and a scripted stub model.

use super::*;

struct Scripted {
    probs: Vec<f32>,
    calls: usize,
    fail: bool,
}

impl TurnModel for Scripted {
    fn probability(&mut self, _pcm: &[f32]) -> Result<f32, String> {
        if self.fail {
            return Err("boom".into());
        }
        let p = self.probs.get(self.calls).copied().unwrap_or(1.0);
        self.calls += 1;
        Ok(p)
    }
}

fn noise(n: usize, amp: f32, seed: &mut u32) -> Vec<i16> {
    (0..n)
        .map(|_| {
            *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (((*seed >> 16) as f32 / 32_768.0 - 1.0) * amp) as i16
        })
        .collect()
}

/// Voiced-looking tone: harmonics of 150 Hz inside the speech band.
fn speech(ms: usize) -> Vec<i16> {
    (0..ms * 16)
        .map(|n| {
            let t = n as f32 / 16_000.0;
            let v: f32 = (1..=8)
                .map(|h| (2.0 * std::f32::consts::PI * 150.0 * h as f32 * t).sin() / h as f32)
                .sum();
            (v * 6_000.0) as i16
        })
        .collect()
}

/// The gate's 400 ms hangover counts as voiced, so a `gap_ms` gap yields
/// `gap_ms - 400` ms of silence.
/// 2 s ambient, speech, silence, speech, silence.
fn session(gap1_ms: usize, tail_ms: usize) -> Vec<i16> {
    let mut seed = 7;
    let mut a = noise(2 * 16_000, 20.0, &mut seed);
    a.extend(speech(1_200));
    a.extend(noise(gap1_ms * 16, 20.0, &mut seed));
    a.extend(speech(900));
    a.extend(noise(tail_ms * 16, 20.0, &mut seed));
    a
}

fn run(audio: &[i16], model: &mut Scripted) -> Result<FileResult, String> {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(compare_audio(
            "s.wav",
            audio,
            &CompareConfig::default(),
            model,
        ))
}

#[test]
fn flags_confirmed_cut_in_then_agreement() {
    // Pause 1 probes at 500/600/700 ms: 0.1, 0.2, 0.6. Pause 2 first probe 0.9.
    let mut m = Scripted {
        probs: vec![0.1, 0.2, 0.6, 0.9],
        calls: 0,
        fail: false,
    };
    let r = run(&session(1_400, 1_600), &mut m).unwrap();
    assert_eq!(r.pauses.len(), 2, "{:?}", r.pauses);
    let (a, b) = (&r.pauses[0], &r.pauses[1]);
    assert!(a.ours_finalized && !a.model_complete);
    assert!(a.premature_cut_in && a.confirmed_cut_in && a.resumed && !a.agree);
    assert_eq!(a.model_end_silence_ms, Some(700));
    assert!((a.model_prob - 0.1).abs() < 1e-6);
    assert!(b.agree && !b.premature_cut_in && !b.resumed);
    assert_eq!(r.latencies_ms.len(), 4);
}

#[test]
fn short_pauses_are_skipped_not_probed() {
    let mut m = Scripted {
        probs: vec![0.9],
        calls: 0,
        fail: false,
    };
    let r = run(&session(600, 1_400), &mut m).unwrap();
    assert_eq!(r.skipped_short_pauses, 1);
    assert_eq!(r.pauses.len(), 1);
    assert_eq!(m.calls, 1);
}

#[test]
fn model_error_refuses_the_run() {
    let mut m = Scripted {
        probs: vec![],
        calls: 0,
        fail: true,
    };
    let err = run(&session(1_400, 1_400), &mut m).unwrap_err();
    assert!(err.contains("model: boom"), "{err}");
}

#[test]
fn silence_only_audio_has_no_candidates() {
    let mut seed = 3;
    let mut m = Scripted {
        probs: vec![],
        calls: 0,
        fail: false,
    };
    let r = run(&noise(5 * 16_000, 20.0, &mut seed), &mut m).unwrap();
    assert!(r.pauses.is_empty());
    assert_eq!(m.calls, 0);
}

#[test]
fn invalid_config_is_refused() {
    let bad = CompareConfig {
        threshold: 1.5,
        ..Default::default()
    };
    assert!(bad.validate().is_err());
    let bad = CompareConfig {
        max_silence_ms: 100,
        ..Default::default()
    };
    assert!(bad.validate().is_err());
    assert!(
        CompareConfig {
            step_ms: 0,
            ..Default::default()
        }
        .validate()
        .is_err()
    );
}
