//! Contract §4 without a microphone: a WAV fixture goes through the same
//! decode -> STT -> post path `ptt.up` uses, with a mocked STT backend (the
//! parakeet int8 bundle is ~600 MB and needs `ort`, so CI never loads it).
//! Nothing here records audio.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use clawft_channels::voice::stt::{SttBackend, SttModel, Utterance};
use clawft_channels::voice::types::VoiceError;
use common::{ScriptedLlm, build_agent, collect_until_final};
use weftos_cos::conversation::Conversation;
use weftos_cos::voice::{PttOutcome, PushToTalk, Retention, read_wav_16k, write_wav_16k};
use weftos_cos::Event;

/// Mock STT: returns a fixed transcript and records what it was handed.
struct MockStt {
    transcript: &'static str,
    seen: Mutex<Vec<(usize, u32)>>,
}

#[async_trait]
impl SttBackend for MockStt {
    async fn warm(&self) -> Result<(), VoiceError> {
        Ok(())
    }
    async fn transcribe(&self, utt: &Utterance) -> Result<String, VoiceError> {
        self.seen.lock().unwrap().push((utt.samples.len(), utt.sample_rate));
        Ok(self.transcript.to_string())
    }
    fn model(&self) -> SttModel {
        SttModel::ParakeetEnglish
    }
}

/// A one-second 48 kHz stereo tone, written as a WAV fixture at test time so
/// no binary lives in the repo. The loader downmixes and resamples it.
fn fixture(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("ptt-fixture.wav");
    let spec = hound::WavSpec { channels: 2, sample_rate: 48_000, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
    let mut w = hound::WavWriter::create(&path, spec).unwrap();
    for i in 0..48_000 {
        let s = ((i as f32 / 48_000.0 * 440.0 * std::f32::consts::TAU).sin() * 8000.0) as i16;
        w.write_sample(s).unwrap();
        w.write_sample(s).unwrap();
    }
    w.finalize().unwrap();
    path
}

#[tokio::test]
async fn wav_fixture_through_stt_posts_a_voice_utterance() {
    let llm = ScriptedLlm::new(vec![ScriptedLlm::text("Noted.")]);
    let t = build_agent(llm, None, None).await;
    let conv: Arc<dyn Conversation> = Arc::new(t.agent.clone());
    let mut rx = conv.events();
    let stt = Arc::new(MockStt { transcript: "note we move the agent to pg after c four", seen: Mutex::new(vec![]) });
    let ptt = PushToTalk::new(stt.clone(), conv.clone());

    let samples = read_wav_16k(&fixture(t.dir.path())).unwrap();
    assert_eq!(samples.len(), 16_000, "48 kHz stereo second -> 16 kHz mono second");
    let outcome = ptt.transcribe_and_post(samples).await.unwrap();
    let PttOutcome::Posted { text, turn_id } = outcome else { panic!("expected a posted turn") };
    assert_eq!(text, "note we move the agent to pg after c four");
    assert_eq!(stt.seen.lock().unwrap().as_slice(), [(16_000, 16_000)]);

    let events = collect_until_final(&mut rx, Duration::from_secs(2)).await;
    let msg = events.iter().find_map(|e| match e { Event::Message { turn_id: id, text, .. } => Some((id.clone(), text.clone())), _ => None }).unwrap();
    assert_eq!(msg.0, turn_id);
    assert_eq!(msg.1, "Noted.");
    let status = events.iter().find_map(|e| match e { Event::Status { task, .. } => Some(task.clone()), _ => None }).unwrap();
    assert!(status.contains("voice"), "{status}");

    // No audio left behind: retention is off by default.
    let leftovers: Vec<_> = std::fs::read_dir(t.dir.path()).unwrap().filter_map(Result::ok).filter(|e| e.file_name().to_string_lossy().starts_with("ptt-2")).collect();
    assert!(leftovers.is_empty());
}

#[tokio::test]
async fn short_or_silent_captures_post_nothing() {
    let llm = ScriptedLlm::new(vec![ScriptedLlm::text("unused")]);
    let t = build_agent(llm.clone(), None, None).await;
    let conv: Arc<dyn Conversation> = Arc::new(t.agent.clone());
    let empty = Arc::new(MockStt { transcript: "   ", seen: Mutex::new(vec![]) });
    let ptt = PushToTalk::new(empty, conv);
    assert!(matches!(ptt.transcribe_and_post(vec![0; 1000]).await.unwrap(), PttOutcome::Nothing { .. }));
    assert!(matches!(ptt.transcribe_and_post(vec![0; 16_000]).await.unwrap(), PttOutcome::Nothing { .. }));
    assert!(llm.seen.lock().unwrap().is_empty(), "no turn was started");
}

#[tokio::test]
async fn debug_retention_keeps_a_wav_only_when_asked() {
    let llm = ScriptedLlm::new(vec![ScriptedLlm::text("ok")]);
    let t = build_agent(llm, None, None).await;
    let conv: Arc<dyn Conversation> = Arc::new(t.agent.clone());
    let debug_dir = t.dir.path().join("debug-audio");
    let stt = Arc::new(MockStt { transcript: "hello", seen: Mutex::new(vec![]) });
    let ptt = PushToTalk::new(stt, conv).with_retention(Retention { keep_audio_debug: true, debug_audio_dir: Some(debug_dir.clone()) });
    let pcm: Vec<i16> = (0..8000).map(|i| (i % 200) as i16).collect();
    let _ = ptt.transcribe_and_post(pcm.clone()).await.unwrap();
    let kept: Vec<_> = std::fs::read_dir(&debug_dir).unwrap().filter_map(Result::ok).collect();
    assert_eq!(kept.len(), 1);
    let back = read_wav_16k(&kept[0].path()).unwrap();
    assert_eq!(back.len(), pcm.len());
    // And the writer used by retention round-trips through the loader.
    let p = t.dir.path().join("rt.wav");
    write_wav_16k(&p, &pcm).unwrap();
    assert_eq!(read_wav_16k(&p).unwrap().len(), pcm.len());
}
