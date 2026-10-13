//! The glasses (leaf) side of `glasses.v1`, sans-IO (design sections 3.6, 3.7, 4.2).
//!
//! [`GlassesNode`] is what the on-glasses Android agent drives through JNI. It owns the
//! glasses' pairing machine, the `indicator` machine and the stream reassembly buffer.
//! The shell feeds raw socket bytes and local events (gate open/close, link lost, ticks)
//! in and performs what comes back: stream frames to write, indicator actions (cue, LED),
//! and shell commands (capture, playback, volume).
//!
//! Wire: each stream frame is `[4-byte big-endian length][lane byte][body]`; the lanes
//! are those of [`crate::transport`]. Every frame returned here is complete and ready to
//! write to the socket.
//!
//! Acknowledgement rule: the node answers every accepted command with `res{ok:true}` as
//! soon as it has been parsed and routed. Failures that happen later in the shell (for
//! example an LED write) are reported through status (`led`), not through a late `res`.

use crate::audio::{AudioFrame, Codec, Stream};
use crate::error::NodeError;
use crate::identity::Identity;
use crate::indicator::{Indicator, IndicatorAction, IndicatorInput, IndicatorState};
use crate::led::LedPattern;
use crate::pairing::{GlassesPairing, GlassesState, PairMsg};
use crate::proto::{CaptureMode, Command, CueKind, Envelope, Event, LedAvailability, Status};
use crate::transport::{LANE_PAIR, LANE_REALTIME, LANE_RELIABLE, frame_decode, frame_encode};

/// Commands the node does not handle itself; the shell executes them.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum ShellCommand {
    CaptureStart { mode: CaptureMode, max_s: u32 },
    CaptureStop,
    PlayBegin { utt_id: u32 },
    PlayFlush { utt_id: u32 },
    VolumeSet { level: f32 },
    CueVolumeSet { level: f32 },
    Cue { kind: CueKind },
    LedSet { pattern: LedPattern },
    Probe { kind: String },
}

/// What one call produced. The shell performs all of it, in this order within each list.
#[derive(Debug, Clone, Default, PartialEq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct Handled {
    /// Complete stream frames to write to the socket.
    pub send: Vec<Vec<u8>>,
    /// Cue, thinking-loop and LED actions from the indicator machine (reports are
    /// already turned into `send` frames and not repeated here).
    pub actions: Vec<IndicatorAction>,
    pub commands: Vec<ShellCommand>,
    /// Reply audio from the parent (`Stream::Down`), in arrival order.
    pub audio: Vec<AudioFrame>,
}

/// Pairing progress for the shell: speak or show `sas`, then wait for `Paired`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum GlassesPairingState {
    Idle,
    AwaitingParent,
    AwaitingCert { sas: String },
    Paired { parent_key_hex: String },
    Refused { reason: String },
}

/// Periodic status, as the shell knows it. `led_available` is false once an LED write fails.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct GlassesStatus {
    pub battery: u8,
    pub charging: bool,
    pub wifi_rssi: i16,
    pub wifi_rate_mbps: u16,
    pub mic_open: bool,
    pub mic_level: u16,
    pub speaking: bool,
    pub led_available: bool,
    pub ver: String,
}

pub struct GlassesNode {
    node_id: String,
    public_key: [u8; 32],
    pairing: GlassesPairing,
    indicator: Indicator,
    rx: Vec<u8>,
    out_seq: u64,
    /// Utterance id of the reply audio currently arriving (first frame ends `thinking`).
    reply_utt: Option<u32>,
}

pub(crate) fn stream_frame(lane: u8, body: &[u8]) -> Result<Vec<u8>, NodeError> {
    let mut inner = Vec::with_capacity(1 + body.len());
    inner.push(lane);
    inner.extend_from_slice(body);
    frame_encode(&inner)
}

impl GlassesNode {
    pub fn new(id: Identity) -> Self {
        Self {
            node_id: id.node_id(),
            public_key: id.public_key(),
            pairing: GlassesPairing::new(id),
            indicator: Indicator::new(),
            rx: Vec::new(),
            out_seq: 0,
            reply_utt: None,
        }
    }

    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    pub fn public_key(&self) -> [u8; 32] {
        self.public_key
    }

    pub fn indicator_state(&self) -> IndicatorState {
        self.indicator.state()
    }

    pub fn turn_id(&self) -> u64 {
        self.indicator.turn_id()
    }

    pub fn next_deadline(&self) -> Option<u64> {
        self.indicator.next_deadline()
    }

    pub fn pairing_state(&self) -> GlassesPairingState {
        match self.pairing.state() {
            GlassesState::Idle => GlassesPairingState::Idle,
            GlassesState::AwaitingParent => GlassesPairingState::AwaitingParent,
            GlassesState::AwaitingCert { sas } => {
                GlassesPairingState::AwaitingCert { sas: sas.clone() }
            }
            GlassesState::Paired { parent_pk } => GlassesPairingState::Paired {
                parent_key_hex: crate::identity::bytes_to_hex(parent_pk),
            },
            GlassesState::Refused { reason } => GlassesPairingState::Refused {
                reason: reason.clone(),
            },
        }
    }

    /// The socket was replaced: drop any half-read frame and the reply-utterance memory so
    /// the new stream starts clean. Indicator and pairing state are kept.
    pub fn reset_stream(&mut self) {
        self.rx.clear();
        self.reply_utt = None;
    }

    fn next_seq(&mut self) -> u64 {
        self.out_seq += 1;
        self.out_seq
    }

    fn evt(&mut self, event: Event) -> Result<Vec<u8>, NodeError> {
        let seq = self.next_seq();
        stream_frame(LANE_RELIABLE, &Envelope::Evt { seq, event }.to_cbor()?)
    }

    // ---- outbound builders ------------------------------------------------------

    /// Start pairing: the glasses' hello for the pairing lane.
    pub fn pair_hello(&mut self) -> Result<Vec<u8>, NodeError> {
        stream_frame(LANE_PAIR, &self.pairing.start().to_bytes()?)
    }

    pub fn status_frame(&mut self, s: &GlassesStatus) -> Result<Vec<u8>, NodeError> {
        let seq = self.next_seq();
        let status = Status {
            battery: s.battery,
            charging: s.charging,
            wifi_rssi: s.wifi_rssi,
            wifi_rate_mbps: s.wifi_rate_mbps,
            mic_open: s.mic_open,
            mic_level: s.mic_level,
            speaking: s.speaking,
            led: if s.led_available {
                LedAvailability::Ok
            } else {
                LedAvailability::Unavailable
            },
            ver: s.ver.clone(),
        };
        stream_frame(LANE_RELIABLE, &Envelope::Stat { seq, status }.to_cbor()?)
    }

    pub fn mic_gate_frame(
        &mut self,
        open: bool,
        rms: u32,
        voiced: f32,
    ) -> Result<Vec<u8>, NodeError> {
        self.evt(Event::MicGate { open, rms, voiced })
    }

    pub fn link_up_frame(&mut self) -> Result<Vec<u8>, NodeError> {
        self.evt(Event::LinkUp)
    }

    pub fn playback_done_frame(
        &mut self,
        utt_id: u32,
        spoken_through_seq: u32,
    ) -> Result<Vec<u8>, NodeError> {
        self.evt(Event::PlaybackDone {
            utt_id,
            spoken_through_seq,
        })
    }

    /// One uplink audio frame (`Stream::Up`).
    pub fn audio_up_frame(
        &self,
        codec: Codec,
        utt: u32,
        seq: u32,
        ts_ms: u32,
        payload: Vec<u8>,
    ) -> Result<Vec<u8>, NodeError> {
        let f = AudioFrame {
            stream: Stream::Up,
            codec,
            flags: 0,
            utt,
            seq,
            ts_ms,
            payload,
        };
        stream_frame(LANE_REALTIME, &f.encode()?)
    }

    // ---- indicator input from the glasses themselves ----------------------------

    /// Local indicator event (gate open/close, PTT, link lost, tick, reply audio).
    pub fn feed(&mut self, now_ms: u64, input: IndicatorInput) -> Result<Handled, NodeError> {
        let acts = self.indicator.handle(now_ms, input);
        let mut h = Handled::default();
        self.apply(acts, &mut h)?;
        Ok(h)
    }

    fn apply(&mut self, acts: Vec<IndicatorAction>, h: &mut Handled) -> Result<(), NodeError> {
        for a in acts {
            match a {
                IndicatorAction::Report { state, turn_id } => {
                    let f = self.evt(Event::IndicatorState { state, turn_id })?;
                    h.send.push(f);
                }
                other => h.actions.push(other),
            }
        }
        Ok(())
    }

    // ---- inbound ----------------------------------------------------------------

    /// Feed raw socket bytes; handles every complete frame now buffered.
    pub fn on_bytes(&mut self, bytes: &[u8], now_ms: u64) -> Result<Handled, NodeError> {
        self.rx.extend_from_slice(bytes);
        let mut h = Handled::default();
        loop {
            let frame = match frame_decode(&mut self.rx) {
                Ok(Some(f)) => f,
                Ok(None) => break,
                Err(e) => {
                    // A bad length poisons the stream; drop the buffer and let the shell reconnect.
                    self.rx.clear();
                    return Err(e);
                }
            };
            self.on_frame(&frame, now_ms, &mut h)?;
        }
        Ok(h)
    }

    fn on_frame(&mut self, frame: &[u8], now_ms: u64, h: &mut Handled) -> Result<(), NodeError> {
        let (lane, body) = frame
            .split_first()
            .ok_or_else(|| NodeError::Codec("empty frame".into()))?;
        match *lane {
            LANE_RELIABLE => self.on_envelope(Envelope::from_cbor(body)?, now_ms, h),
            LANE_REALTIME => self.on_audio(AudioFrame::decode(body)?, now_ms, h),
            LANE_PAIR => {
                self.pairing.on_message(PairMsg::from_bytes(body)?, now_ms);
                Ok(())
            }
            n => Err(NodeError::Codec(format!("unknown lane {n}"))),
        }
    }

    fn on_audio(&mut self, f: AudioFrame, now_ms: u64, h: &mut Handled) -> Result<(), NodeError> {
        if f.stream != Stream::Down {
            return Ok(()); // uplink-direction frame from the parent: not ours to play
        }
        // Only the first reply frame of an utterance ends `thinking`, and only while
        // thinking: late audio must not clear the deadline of captured/done states.
        if self.reply_utt != Some(f.utt) {
            self.reply_utt = Some(f.utt);
            if self.indicator.state() == IndicatorState::Thinking {
                let acts = self
                    .indicator
                    .handle(now_ms, IndicatorInput::ReplyAudioStarted);
                self.apply(acts, h)?;
            }
        }
        h.audio.push(f);
        Ok(())
    }

    fn res(&self, id: u64) -> Result<Vec<u8>, NodeError> {
        stream_frame(
            LANE_RELIABLE,
            &Envelope::Res {
                id,
                ok: true,
                err: None,
            }
            .to_cbor()?,
        )
    }

    fn on_envelope(&mut self, e: Envelope, now_ms: u64, h: &mut Handled) -> Result<(), NodeError> {
        let Envelope::Cmd { id, cmd, .. } = e else {
            return Ok(()); // res/stat/evt travel glasses -> parent only
        };
        match cmd {
            Command::IndicatorSet {
                state,
                turn_id,
                seq,
            } => {
                let acts = self.indicator.handle(
                    now_ms,
                    IndicatorInput::Set {
                        state,
                        turn_id,
                        seq,
                    },
                );
                self.apply(acts, h)?;
            }
            Command::IndicatorHold { ms } => {
                let acts = self.indicator.handle(now_ms, IndicatorInput::Hold { ms });
                self.apply(acts, h)?;
            }
            Command::Ping => {}
            Command::CaptureStart { mode, max_s } => {
                h.commands.push(ShellCommand::CaptureStart { mode, max_s })
            }
            Command::CaptureStop => h.commands.push(ShellCommand::CaptureStop),
            Command::PlayBegin { utt_id } => {
                self.reply_utt = None;
                h.commands.push(ShellCommand::PlayBegin { utt_id })
            }
            Command::PlayFlush { utt_id } => h.commands.push(ShellCommand::PlayFlush { utt_id }),
            Command::VolumeSet { level } => h.commands.push(ShellCommand::VolumeSet { level }),
            Command::CueVolumeSet { level } => {
                h.commands.push(ShellCommand::CueVolumeSet { level })
            }
            Command::Cue { kind } => h.commands.push(ShellCommand::Cue { kind }),
            Command::LedSet { pattern } => h.commands.push(ShellCommand::LedSet { pattern }),
            Command::Probe { kind } => h.commands.push(ShellCommand::Probe { kind }),
        }
        h.send.push(self.res(id)?);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indicator::IndicatorState as S;

    fn node() -> GlassesNode {
        GlassesNode::new(Identity::from_seed([9; 32]))
    }

    fn cmd_bytes(id: u64, cmd: Command) -> Vec<u8> {
        let env = Envelope::Cmd {
            id,
            seq: id,
            ttl_ms: 1000,
            cmd,
        };
        stream_frame(LANE_RELIABLE, &env.to_cbor().unwrap()).unwrap()
    }

    fn envelopes(h: &Handled) -> Vec<Envelope> {
        h.send
            .iter()
            .map(|f| {
                let mut b = f.clone();
                let inner = frame_decode(&mut b).unwrap().unwrap();
                assert_eq!(inner[0], LANE_RELIABLE);
                Envelope::from_cbor(&inner[1..]).unwrap()
            })
            .collect()
    }

    #[test]
    fn split_and_coalesced_reads_are_reassembled() {
        let mut n = node();
        let a = cmd_bytes(1, Command::Ping);
        let b = cmd_bytes(2, Command::CaptureStop);
        let mut all = a.clone();
        all.extend_from_slice(&b);
        let h1 = n.on_bytes(&all[..5], 0).unwrap();
        assert!(h1.send.is_empty());
        let h2 = n.on_bytes(&all[5..], 0).unwrap();
        assert_eq!(h2.send.len(), 2);
        assert_eq!(h2.commands, vec![ShellCommand::CaptureStop]);
    }

    #[test]
    fn indicator_set_is_acked_reported_and_idempotent() {
        let mut n = node();
        // thinking first (glasses own listening/captured; here via the workflow path)
        let set = |seq| {
            cmd_bytes(
                seq,
                Command::IndicatorSet {
                    state: S::DoneOk,
                    turn_id: 3,
                    seq,
                },
            )
        };
        n.on_bytes(
            &cmd_bytes(
                1,
                Command::IndicatorSet {
                    state: S::Thinking,
                    turn_id: 3,
                    seq: 1,
                },
            ),
            0,
        )
        .unwrap();
        let h = n.on_bytes(&set(2), 100).unwrap();
        assert_eq!(n.indicator_state(), S::DoneOk);
        assert!(
            h.actions
                .iter()
                .any(|a| matches!(a, IndicatorAction::PlayCue { .. }))
        );
        let envs = envelopes(&h);
        assert!(envs.iter().any(|e| matches!(
            e,
            Envelope::Evt {
                event: Event::IndicatorState {
                    state: S::DoneOk,
                    turn_id: 3
                },
                ..
            }
        )));
        assert!(envs.iter().any(|e| matches!(
            e,
            Envelope::Res {
                id: 2,
                ok: true,
                ..
            }
        )));
        // replay of the same seq: acked, no second report or cue
        let h = n.on_bytes(&set(2), 110).unwrap();
        assert!(h.actions.is_empty());
        assert_eq!(envelopes(&h).len(), 1);
    }

    #[test]
    fn first_reply_frame_ends_thinking_only_while_thinking() {
        let mut n = node();
        n.feed(0, IndicatorInput::GateOpen).unwrap();
        n.feed(10, IndicatorInput::GateClose).unwrap();
        n.feed(10 + crate::indicator::CAPTURED_MS, IndicatorInput::Tick)
            .unwrap();
        assert_eq!(n.indicator_state(), S::Thinking);
        let down = |utt, seq| {
            let f = AudioFrame {
                stream: Stream::Down,
                codec: Codec::Pcm16,
                flags: 0,
                utt,
                seq,
                ts_ms: 0,
                payload: vec![0; 640],
            };
            stream_frame(LANE_REALTIME, &f.encode().unwrap()).unwrap()
        };
        let h = n.on_bytes(&down(5, 0), 500).unwrap();
        assert_eq!(h.audio.len(), 1);
        assert!(h.actions.contains(&IndicatorAction::StopThinkingLoop));
        let h = n.on_bytes(&down(5, 1), 520).unwrap();
        assert!(
            h.actions.is_empty(),
            "second frame of the utterance is inert"
        );

        // reply audio arriving while captured must not clear its deadline
        let mut n = node();
        n.feed(0, IndicatorInput::GateOpen).unwrap();
        n.feed(10, IndicatorInput::GateClose).unwrap();
        assert_eq!(n.indicator_state(), S::Captured);
        n.on_bytes(&down(7, 0), 20).unwrap();
        assert!(n.next_deadline().is_some());
    }

    #[test]
    fn shell_commands_are_routed_and_acked() {
        let mut n = node();
        let h = n
            .on_bytes(&cmd_bytes(4, Command::VolumeSet { level: 0.5 }), 0)
            .unwrap();
        assert_eq!(h.commands, vec![ShellCommand::VolumeSet { level: 0.5 }]);
        assert!(matches!(
            envelopes(&h)[0],
            Envelope::Res {
                id: 4,
                ok: true,
                ..
            }
        ));
    }

    #[test]
    fn link_loss_while_thinking_ends_in_error() {
        let mut n = node();
        n.feed(0, IndicatorInput::GateOpen).unwrap();
        n.feed(1, IndicatorInput::GateClose).unwrap();
        n.feed(1 + crate::indicator::CAPTURED_MS, IndicatorInput::Tick)
            .unwrap();
        let h = n.feed(900, IndicatorInput::LinkLost).unwrap();
        assert_eq!(n.indicator_state(), S::DoneError);
        assert!(
            h.actions
                .iter()
                .any(|a| matches!(a, IndicatorAction::PlayCue { .. }))
        );
        assert!(!h.send.is_empty(), "the shown state is still reported");
    }

    #[test]
    fn reset_stream_drops_a_partial_frame() {
        let mut n = node();
        let f = cmd_bytes(1, Command::Ping);
        n.on_bytes(&f[..6], 0).unwrap();
        n.reset_stream();
        // a fresh, whole frame parses; the stale 6 bytes are gone
        let h = n.on_bytes(&f, 0).unwrap();
        assert_eq!(h.send.len(), 1);
    }

    #[test]
    fn oversize_length_poisons_and_clears_the_buffer() {
        let mut n = node();
        let bad = [0xff, 0xff, 0xff, 0xff, 0];
        assert!(n.on_bytes(&bad, 0).is_err());
        // stream recovers for a fresh, well-formed frame
        let h = n.on_bytes(&cmd_bytes(1, Command::Ping), 0).unwrap();
        assert_eq!(h.send.len(), 1);
    }

    #[test]
    fn outbound_builders_produce_decodable_frames() {
        let mut n = node();
        let s = GlassesStatus {
            battery: 77,
            charging: false,
            wifi_rssi: -50,
            wifi_rate_mbps: 390,
            mic_open: true,
            mic_level: 321,
            speaking: false,
            led_available: false,
            ver: "t".into(),
        };
        let mut b = n.status_frame(&s).unwrap();
        let inner = frame_decode(&mut b).unwrap().unwrap();
        match Envelope::from_cbor(&inner[1..]).unwrap() {
            Envelope::Stat { seq: 1, status } => {
                assert_eq!(status.led, LedAvailability::Unavailable);
                assert_eq!(status.mic_level, 321);
            }
            other => panic!("{other:?}"),
        }
        let mut a = n
            .audio_up_frame(Codec::Pcm16, 1, 2, 40, vec![1; 640])
            .unwrap();
        let inner = frame_decode(&mut a).unwrap().unwrap();
        assert_eq!(inner[0], LANE_REALTIME);
        assert_eq!(AudioFrame::decode(&inner[1..]).unwrap().seq, 2);
        assert!(!n.pair_hello().unwrap().is_empty());
        assert_eq!(n.pairing_state(), GlassesPairingState::AwaitingParent);
    }
}
