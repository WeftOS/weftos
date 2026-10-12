//! Push-to-talk (contract §4): `ptt.down` starts capture from the default
//! input, `ptt.up` stops it, runs local STT and posts `{text, source: voice}`.
//!
//! Capture is behind the `mic` feature (cpal). The STT and post path is
//! feature-free and shared with tests, which feed it PCM decoded from a WAV
//! file. Raw audio is zeroed after transcription unless the debug flag keeps
//! a WAV copy.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use clawft_channels::voice::stt::{SttBackend, Utterance};

use crate::CosError;
use crate::conversation::{Conversation, TurnId, UserUtterance};

/// STT sample rate the backends expect.
pub const STT_RATE: u32 = 16_000;

/// A button edge from the pad sensor or the simulator key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PttEdge {
    Down,
    Up,
}

impl PttEdge {
    /// Map a leaf sensor name (`ptt.down` / `ptt.up`) to an edge.
    pub fn from_sensor(name: &str) -> Option<Self> {
        match name {
            "ptt.down" => Some(Self::Down),
            "ptt.up" => Some(Self::Up),
            _ => None,
        }
    }
}

/// Debug retention: off by default (contract §4).
#[derive(Debug, Clone, Default)]
pub struct Retention {
    pub keep_audio_debug: bool,
    pub debug_audio_dir: Option<PathBuf>,
}

/// What `ptt_up` / `transcribe_and_post` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PttOutcome {
    /// A transcript was posted as this turn.
    Posted { text: String, turn_id: TurnId },
    /// Nothing usable was heard (too short, silence, or an empty transcript).
    Nothing { reason: String },
}

/// The button as the API drives it (`POST /v1/cos/ptt`): whoever owns the
/// microphone implements this. [`PushToTalk`] is the real one; tests use a
/// recorder.
#[async_trait::async_trait]
pub trait PttControl: Send + Sync {
    /// `Down` starts capture and returns `None`; `Up` finishes the turn.
    async fn on_edge(&self, edge: PttEdge) -> Result<Option<PttOutcome>, CosError>;
    /// Whether a capture is in progress.
    fn is_active(&self) -> bool;
}

#[async_trait::async_trait]
impl PttControl for PushToTalk {
    async fn on_edge(&self, edge: PttEdge) -> Result<Option<PttOutcome>, CosError> {
        PushToTalk::on_edge(self, edge).await
    }

    fn is_active(&self) -> bool {
        PushToTalk::is_active(self)
    }
}

pub struct PushToTalk {
    stt: Arc<dyn SttBackend>,
    conv: Arc<dyn Conversation>,
    retention: Retention,
    device: Option<String>,
    barge_in: Option<Arc<crate::speak::Speaker>>,
    #[cfg(feature = "mic")]
    capture: Mutex<Option<capture::Capture>>,
    #[cfg(not(feature = "mic"))]
    capture: Mutex<Option<()>>,
}

impl PushToTalk {
    pub fn new(stt: Arc<dyn SttBackend>, conv: Arc<dyn Conversation>) -> Self {
        Self { stt, conv, retention: Retention::default(), device: None, barge_in: None, capture: Mutex::new(None) }
    }

    pub fn with_retention(mut self, retention: Retention) -> Self {
        self.retention = retention;
        self
    }

    pub fn with_device(mut self, device: Option<String>) -> Self {
        self.device = device;
        self
    }

    /// A press of the button interrupts this speaker's reply in progress.
    pub fn with_barge_in(mut self, speaker: Arc<crate::speak::Speaker>) -> Self {
        self.barge_in = Some(speaker);
        self
    }

    /// Whether a capture is in progress (the console shows `ptt_active`).
    pub fn is_active(&self) -> bool {
        self.capture.lock().unwrap_or_else(|e| e.into_inner()).is_some()
    }

    /// Handle a button edge: `Down` starts capture, `Up` finishes the turn.
    pub async fn on_edge(&self, edge: PttEdge) -> Result<Option<PttOutcome>, CosError> {
        match edge {
            PttEdge::Down => {
                if let Some(s) = &self.barge_in {
                    s.interrupt();
                }
                self.ptt_down().map(|()| None)
            }
            PttEdge::Up => self.ptt_up().await.map(Some),
        }
    }

    /// Start capturing from the default (or configured) input.
    #[cfg(feature = "mic")]
    pub fn ptt_down(&self) -> Result<(), CosError> {
        let mut slot = self.capture.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_some() {
            return Ok(());
        }
        *slot = Some(capture::Capture::start(self.device.as_deref())?);
        Ok(())
    }

    /// Stop capturing, transcribe, post.
    #[cfg(feature = "mic")]
    pub async fn ptt_up(&self) -> Result<PttOutcome, CosError> {
        let cap = self.capture.lock().unwrap_or_else(|e| e.into_inner()).take();
        let Some(cap) = cap else {
            return Ok(PttOutcome::Nothing { reason: "button released without a capture".into() });
        };
        let (rate, mut mono) = tokio::task::spawn_blocking(move || cap.stop())
            .await
            .map_err(|e| CosError::Voice(format!("capture thread: {e}")))??;
        let samples = resample_to_16k(&mono, rate);
        mono.fill(0.0);
        self.transcribe_and_post(samples).await
    }

    #[cfg(not(feature = "mic"))]
    pub fn ptt_down(&self) -> Result<(), CosError> {
        Err(CosError::Voice("microphone capture is not compiled in (feature `mic`)".into()))
    }

    #[cfg(not(feature = "mic"))]
    pub async fn ptt_up(&self) -> Result<PttOutcome, CosError> {
        Err(CosError::Voice("microphone capture is not compiled in (feature `mic`)".into()))
    }

    /// The feature-free half: 16 kHz mono PCM in, transcript posted as a voice
    /// utterance. The samples are zeroed on drop (`Utterance`'s `Drop`).
    pub async fn transcribe_and_post(&self, samples: Vec<i16>) -> Result<PttOutcome, CosError> {
        if samples.len() < (STT_RATE / 4) as usize {
            return Ok(PttOutcome::Nothing { reason: "capture shorter than 250 ms".into() });
        }
        if self.retention.keep_audio_debug {
            self.keep_debug_copy(&samples);
        }
        let utt = Utterance { samples, sample_rate: STT_RATE };
        let text = self.stt.transcribe(&utt).await.map_err(|e| CosError::Voice(e.to_string()))?;
        drop(utt);
        let text = text.trim().to_string();
        if text.is_empty() {
            return Ok(PttOutcome::Nothing { reason: "empty transcript".into() });
        }
        let turn_id = self.conv.utter(UserUtterance::voice(text.clone())).await?;
        Ok(PttOutcome::Posted { text, turn_id })
    }

    fn keep_debug_copy(&self, samples: &[i16]) {
        let Some(dir) = &self.retention.debug_audio_dir else { return };
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
        let path = dir.join(format!("ptt-{}.wav", chrono::Utc::now().format("%Y%m%dT%H%M%S%.3fZ")));
        if let Err(e) = write_wav_16k(&path, samples) {
            tracing::warn!(error = %e, "debug audio copy failed");
        }
    }
}

/// Linear resample of mono f32 at `rate` to 16 kHz s16.
pub fn resample_to_16k(mono: &[f32], rate: u32) -> Vec<i16> {
    if mono.is_empty() {
        return Vec::new();
    }
    let to_i16 = |s: f32| (s.clamp(-1.0, 1.0) * 32_767.0).round() as i16;
    if rate == STT_RATE {
        return mono.iter().map(|&s| to_i16(s)).collect();
    }
    let ratio = rate as f64 / STT_RATE as f64;
    let out_len = ((mono.len() as f64) / ratio).floor() as usize;
    (0..out_len)
        .map(|i| {
            let pos = i as f64 * ratio;
            let idx = pos.floor() as usize;
            let frac = (pos - idx as f64) as f32;
            let a = mono[idx.min(mono.len() - 1)];
            let b = mono[(idx + 1).min(mono.len() - 1)];
            to_i16(a + (b - a) * frac)
        })
        .collect()
}

/// Decode a WAV file to 16 kHz mono s16 (any rate, channels, int or float).
pub fn read_wav_16k(path: &Path) -> Result<Vec<i16>, CosError> {
    let mut reader = hound::WavReader::open(path).map_err(|e| CosError::Voice(format!("wav {}: {e}", path.display())))?;
    let spec = reader.spec();
    let ch = spec.channels.max(1) as usize;
    let mono: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => downmix(reader.samples::<f32>().filter_map(Result::ok), ch),
        hound::SampleFormat::Int => {
            let scale = ((1u64 << (spec.bits_per_sample as u64 - 1)) as f32).max(1.0);
            downmix(reader.samples::<i32>().filter_map(Result::ok).map(|s| s as f32 / scale), ch)
        }
    };
    Ok(resample_to_16k(&mono, spec.sample_rate))
}

fn downmix(samples: impl Iterator<Item = f32>, channels: usize) -> Vec<f32> {
    let mut out = Vec::new();
    let mut frame = Vec::with_capacity(channels);
    for s in samples {
        frame.push(s);
        if frame.len() == channels {
            out.push(frame.iter().sum::<f32>() / channels as f32);
            frame.clear();
        }
    }
    out
}

/// Write 16 kHz mono s16 PCM as a WAV file.
pub fn write_wav_16k(path: &Path, samples: &[i16]) -> Result<(), CosError> {
    let spec = hound::WavSpec { channels: 1, sample_rate: STT_RATE, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
    let mut w = hound::WavWriter::create(path, spec).map_err(|e| CosError::Voice(e.to_string()))?;
    for s in samples {
        w.write_sample(*s).map_err(|e| CosError::Voice(e.to_string()))?;
    }
    w.finalize().map_err(|e| CosError::Voice(e.to_string()))
}

/// cpal capture on its own thread (a `cpal::Stream` is not `Send`).
#[cfg(feature = "mic")]
mod capture {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    use crate::CosError;

    pub struct Capture {
        stop: Arc<AtomicBool>,
        done: std::sync::mpsc::Receiver<Result<(u32, Vec<f32>), String>>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl Capture {
        pub fn start(device: Option<&str>) -> Result<Self, CosError> {
            let stop = Arc::new(AtomicBool::new(false));
            let (tx, done) = std::sync::mpsc::channel();
            let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
            let want = device.map(str::to_string);
            let stop2 = stop.clone();
            let thread = std::thread::Builder::new()
                .name("cos-ptt-capture".into())
                .spawn(move || run(want.as_deref(), stop2, tx, ready_tx))
                .map_err(|e| CosError::Voice(format!("capture thread: {e}")))?;
            ready_rx
                .recv()
                .map_err(|_| CosError::Voice("capture thread died before opening the input".into()))?
                .map_err(CosError::Voice)?;
            Ok(Self { stop, done, thread: Some(thread) })
        }

        /// Stop and return `(native_rate, mono_f32)`. Blocking.
        pub fn stop(mut self) -> Result<(u32, Vec<f32>), CosError> {
            self.stop.store(true, Ordering::SeqCst);
            let out = self.done.recv().map_err(|_| CosError::Voice("capture thread lost".into()))?;
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
            out.map_err(CosError::Voice)
        }
    }

    fn run(
        want: Option<&str>,
        stop: Arc<AtomicBool>,
        done: std::sync::mpsc::Sender<Result<(u32, Vec<f32>), String>>,
        ready: std::sync::mpsc::Sender<Result<(), String>>,
    ) {
        let host = cpal::default_host();
        let device = match pick_input(&host, want) {
            Ok(d) => d,
            Err(e) => {
                let _ = ready.send(Err(e));
                return;
            }
        };
        let supported = match device.default_input_config() {
            Ok(s) => s,
            Err(e) => {
                let _ = ready.send(Err(e.to_string()));
                return;
            }
        };
        let rate = supported.sample_rate().0;
        let n_ch = supported.channels().max(1) as usize;
        let cfg: cpal::StreamConfig = supported.clone().into();
        let buf: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::new()));
        let err_fn = |e| tracing::warn!("[cos-ptt] input stream error: {e}");
        let stream = match supported.sample_format() {
            cpal::SampleFormat::F32 => {
                let b = buf.clone();
                device.build_input_stream(
                    &cfg,
                    move |data: &[f32], _| {
                        let mut g = b.lock().unwrap_or_else(|e| e.into_inner());
                        g.extend(data.chunks(n_ch).map(|fr| fr.iter().sum::<f32>() / n_ch as f32));
                    },
                    err_fn,
                    None,
                )
            }
            cpal::SampleFormat::I16 => {
                let b = buf.clone();
                device.build_input_stream(
                    &cfg,
                    move |data: &[i16], _| {
                        let mut g = b.lock().unwrap_or_else(|e| e.into_inner());
                        g.extend(data.chunks(n_ch).map(|fr| fr.iter().map(|&x| f32::from(x) / 32_768.0).sum::<f32>() / n_ch as f32));
                    },
                    err_fn,
                    None,
                )
            }
            other => {
                let _ = ready.send(Err(format!("unsupported input sample format {other:?}")));
                return;
            }
        };
        let stream = match stream.map_err(|e| e.to_string()).and_then(|s| s.play().map(|()| s).map_err(|e| e.to_string())) {
            Ok(s) => s,
            Err(e) => {
                let _ = ready.send(Err(e));
                return;
            }
        };
        let _ = ready.send(Ok(()));
        while !stop.load(Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        drop(stream);
        let samples = std::mem::take(&mut *buf.lock().unwrap_or_else(|e| e.into_inner()));
        let _ = done.send(Ok((rate, samples)));
    }

    fn pick_input(host: &cpal::Host, want: Option<&str>) -> Result<cpal::Device, String> {
        if let Some(sub) = want {
            let sub_l = sub.to_lowercase();
            for d in host.input_devices().map_err(|e| e.to_string())? {
                if d.name().is_ok_and(|n| n.to_lowercase().contains(&sub_l)) {
                    return Ok(d);
                }
            }
            return Err(format!("no input device matching {sub:?}"));
        }
        host.default_input_device().ok_or_else(|| "no default input device".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resample_halves_48k() {
        let mono: Vec<f32> = (0..48_000).map(|i| ((i % 100) as f32 / 100.0) - 0.5).collect();
        let out = resample_to_16k(&mono, 48_000);
        assert_eq!(out.len(), 16_000);
    }

    #[test]
    fn wav_roundtrip_through_the_loader() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.wav");
        let pcm: Vec<i16> = (0..8000).map(|i| ((i as f32 * 0.1).sin() * 10_000.0) as i16).collect();
        write_wav_16k(&path, &pcm).unwrap();
        let back = read_wav_16k(&path).unwrap();
        assert_eq!(back.len(), pcm.len());
        assert!((back[100] - pcm[100]).abs() <= 1);
    }

    #[test]
    fn sensor_names_map_to_edges() {
        assert_eq!(PttEdge::from_sensor("ptt.down"), Some(PttEdge::Down));
        assert_eq!(PttEdge::from_sensor("ptt.up"), Some(PttEdge::Up));
        assert_eq!(PttEdge::from_sensor("tap"), None);
    }
}
