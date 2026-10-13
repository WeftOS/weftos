//! Reliable-lane envelopes (glasses.v1 section 4.2): CBOR maps, `t` is the type.
//! Signing, sequence floors and ACKs (WLF1) layer on top in P1; P0 defines the shapes.

use crate::error::NodeError;
use crate::indicator::IndicatorState;
use crate::led::LedPattern;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum CaptureMode {
    Ptt,
    Vad,
    Continuous,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum CueKind {
    Heard,
    Working,
    Done,
    Error,
}

/// Commands, wire name in `k`, arguments in `p`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "k", content = "p")]
pub enum Command {
    #[serde(rename = "capture.start")]
    CaptureStart { mode: CaptureMode, max_s: u32 },
    #[serde(rename = "capture.stop")]
    CaptureStop,
    #[serde(rename = "play.begin")]
    PlayBegin { utt_id: u32 },
    #[serde(rename = "play.flush")]
    PlayFlush { utt_id: u32 },
    #[serde(rename = "volume.set")]
    VolumeSet { level: f32 },
    #[serde(rename = "cue_volume.set")]
    CueVolumeSet { level: f32 },
    #[serde(rename = "cue")]
    Cue { kind: CueKind },
    #[serde(rename = "indicator.set")]
    IndicatorSet {
        state: IndicatorState,
        turn_id: u64,
        seq: u64,
    },
    #[serde(rename = "indicator.hold")]
    IndicatorHold { ms: u32 },
    #[serde(rename = "led.set")]
    LedSet { pattern: LedPattern },
    #[serde(rename = "probe")]
    Probe { kind: String },
    #[serde(rename = "ping")]
    Ping,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LedAvailability {
    Ok,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Status {
    pub battery: u8,
    pub charging: bool,
    pub wifi_rssi: i16,
    pub wifi_rate_mbps: u16,
    pub mic_open: bool,
    pub mic_level: u16,
    pub speaking: bool,
    pub led: LedAvailability,
    pub ver: String,
}

/// Events from the glasses, wire name in `k`, arguments in `p`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "k", content = "p")]
pub enum Event {
    #[serde(rename = "mic.gate")]
    MicGate { open: bool, rms: u32, voiced: f32 },
    #[serde(rename = "link.up")]
    LinkUp,
    #[serde(rename = "link.down")]
    LinkDown,
    #[serde(rename = "playback.done")]
    PlaybackDone {
        utt_id: u32,
        spoken_through_seq: u32,
    },
    #[serde(rename = "indicator.state")]
    IndicatorState { state: IndicatorState, turn_id: u64 },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Envelope {
    Cmd {
        id: u64,
        seq: u64,
        ttl_ms: u32,
        #[serde(flatten)]
        cmd: Command,
    },
    Res {
        id: u64,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        err: Option<String>,
    },
    Stat {
        seq: u64,
        #[serde(flatten)]
        status: Status,
    },
    Evt {
        seq: u64,
        #[serde(flatten)]
        event: Event,
    },
}

impl Envelope {
    pub fn to_cbor(&self) -> Result<Vec<u8>, NodeError> {
        let mut out = Vec::new();
        ciborium::into_writer(self, &mut out).map_err(|e| NodeError::Codec(e.to_string()))?;
        Ok(out)
    }

    pub fn from_cbor(b: &[u8]) -> Result<Self, NodeError> {
        ciborium::from_reader(b).map_err(|e| NodeError::Codec(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::led::LedColor;

    fn rt(e: Envelope) {
        let b = e.to_cbor().unwrap();
        assert_eq!(Envelope::from_cbor(&b).unwrap(), e);
    }

    #[test]
    fn every_envelope_roundtrips() {
        rt(Envelope::Cmd {
            id: 1,
            seq: 2,
            ttl_ms: 5000,
            cmd: Command::CaptureStart {
                mode: CaptureMode::Vad,
                max_s: 20,
            },
        });
        rt(Envelope::Cmd {
            id: 2,
            seq: 3,
            ttl_ms: 1000,
            cmd: Command::CaptureStop,
        });
        rt(Envelope::Cmd {
            id: 3,
            seq: 4,
            ttl_ms: 1000,
            cmd: Command::IndicatorSet {
                state: IndicatorState::DoneOk,
                turn_id: 9,
                seq: 1,
            },
        });
        rt(Envelope::Cmd {
            id: 4,
            seq: 5,
            ttl_ms: 1000,
            cmd: Command::LedSet {
                pattern: LedPattern {
                    color: LedColor::Orange,
                    on_ms: 900,
                    off_ms: 900,
                    cycles: 3,
                    brightness: 100,
                },
            },
        });
        rt(Envelope::Res {
            id: 4,
            ok: false,
            err: Some("led unavailable".into()),
        });
        rt(Envelope::Res {
            id: 4,
            ok: true,
            err: None,
        });
        rt(Envelope::Stat {
            seq: 6,
            status: Status {
                battery: 80,
                charging: false,
                wifi_rssi: -55,
                wifi_rate_mbps: 390,
                mic_open: false,
                mic_level: 12,
                speaking: false,
                led: LedAvailability::Ok,
                ver: "0.1".into(),
            },
        });
        rt(Envelope::Evt {
            seq: 7,
            event: Event::MicGate {
                open: true,
                rms: 812,
                voiced: 0.42,
            },
        });
        rt(Envelope::Evt {
            seq: 8,
            event: Event::LinkDown,
        });
        rt(Envelope::Evt {
            seq: 9,
            event: Event::IndicatorState {
                state: IndicatorState::Thinking,
                turn_id: 9,
            },
        });
    }

    #[test]
    fn cmd_uses_documented_wire_names() {
        let e = Envelope::Cmd {
            id: 1,
            seq: 1,
            ttl_ms: 1,
            cmd: Command::CaptureStop,
        };
        let v: ciborium::Value = ciborium::from_reader(&e.to_cbor().unwrap()[..]).unwrap();
        let map = v.as_map().unwrap();
        let get = |k: &str| {
            map.iter()
                .find(|(a, _)| a.as_text() == Some(k))
                .map(|(_, b)| b.clone())
        };
        assert_eq!(get("t").unwrap().as_text(), Some("cmd"));
        assert_eq!(get("k").unwrap().as_text(), Some("capture.stop"));
    }

    #[test]
    fn garbage_is_rejected() {
        assert!(Envelope::from_cbor(&[0xff, 0x00, 0x13]).is_err());
    }
}
