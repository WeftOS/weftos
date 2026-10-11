//! The `ptts` binding behind [`crate::pocket_tts`] (feature `pocket-tts`).
//! Reads local files only; the library never downloads weights.

use ptts::checkpoint::{Checkpoint, ResolveOptions};
use ptts::preprocess::Lang;
use ptts::synth::{DeviceKind, SpeechOptions, Synth};

use clawft_channels::voice::types::VoiceError;

use crate::pocket_tts::{PcmSource, PcmStream, PocketTtsConfig};

pub(crate) struct PtSource {
    synth: Synth,
    voice: Option<String>,
}

fn cfg_err(what: &str, e: impl std::fmt::Display) -> VoiceError {
    VoiceError::Config(format!("pocket-tts {what}: {e}"))
}

impl PtSource {
    pub(crate) fn load(cfg: &PocketTtsConfig) -> Result<Self, VoiceError> {
        let dir = cfg.validate()?;
        let quant = cfg.quant.parse().map_err(|e| cfg_err("quant", e))?;
        let lang: Lang = cfg.language.parse().map_err(|e| cfg_err("language", e))?;
        let device: DeviceKind = cfg.device.parse().map_err(|e| cfg_err("device", e))?;
        let checkpoint = Checkpoint::resolve(dir, ResolveOptions { quant, weights: None })
            .map_err(|e| cfg_err("checkpoint", e))?;
        let mut synth = checkpoint
            .builder(lang)
            .device(device)
            .build()
            .map_err(|e| cfg_err("load", e))?;
        checkpoint.register_voices(&mut synth);
        if let Some(path) = &cfg.voice_file {
            let name = cfg.voice.clone().ok_or_else(|| {
                VoiceError::Config("pocket-tts voice_file needs a voice name".into())
            })?;
            synth.add_voice_file(&name, path).map_err(|e| cfg_err("voice_file", e))?;
        }
        if let Some(v) = &cfg.voice
            && !synth.voices().contains(v)
        {
            return Err(VoiceError::Config(format!(
                "pocket-tts voice '{v}' not in checkpoint (have: {})",
                synth.voices().join(", ")
            )));
        }
        Ok(Self { synth, voice: cfg.voice.clone() })
    }
}

impl PcmSource for PtSource {
    fn sample_rate(&self) -> u32 {
        self.synth.sample_rate()
    }

    fn open(&self, text: &str) -> Result<Box<dyn PcmStream>, String> {
        let mut opts = SpeechOptions::default();
        if let Some(v) = &self.voice {
            opts = opts.voice(v.clone());
        }
        let stream = self.synth.stream_with(text, &opts).map_err(|e| e.to_string())?;
        // Dropping the SpeechStream stops and joins both generation workers.
        Ok(Box::new(stream.map(|r| r.map_err(|e| e.to_string()))))
    }
}
