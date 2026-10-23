//! The phone (parent) side of one glasses link, sans-IO (design sections 3 to 5, P0 gate A1).
//!
//! [`PhoneSession`] is what the phone shell drives. The shell owns the TCP listener, the HTTP
//! call to the speech endpoint and the clock; this module owns everything between:
//!
//! - stream reassembly and lane routing for the glasses' bytes,
//! - the leaf-parent pairing exchange and the owner's code comparison,
//! - utterance assembly: uplink PCM16 frames between the glasses' `listening` and `captured`
//!   indicator reports become one buffer (the shell wraps it in a WAV),
//! - the relay turn: `indicator.set thinking` while waiting, `done_ok` after the reply audio
//!   has played, `done_error` on error or the 20 s budget, a quiet `idle` on no speech,
//! - the reply audio as `Stream::Down` frames, preceded by `play.begin`.
//!
//! Every call returns a [`PhoneOut`]: complete stream frames to write to the socket plus
//! [`PhoneEvent`]s for the UI and the relay. Nothing here blocks or reads a clock.
//!
//! P0 stubs: pairing messages are plain CBOR on the pairing lane (Noise XX is P1), and a
//! reconnect from the same address as an already paired peer is accepted as `resumed`
//! without a fresh proof (the glasses keep their pairing in memory and do not re-send hello).

use crate::audio::{AudioFrame, Codec, Stream};
use crate::error::NodeError;
use crate::glasses::stream_frame;
use crate::identity::{Identity, bytes_to_hex};
use crate::indicator::IndicatorState;
use crate::pairing::{CAP_LED, CAP_MIC, CAP_SPEAKER, PairMsg, PairingState, ParentPairing};
use crate::proto::{Command, Envelope, Event};
use crate::transport::{LANE_PAIR, LANE_REALTIME, LANE_RELIABLE, frame_decode};

/// Shortest utterance worth sending (the endpoint rejects less than 0.2 s).
pub const UTT_MIN_MS: u32 = 200;
/// Longest utterance the endpoint takes (15 s, 480 000 bytes of PCM16 at 16 kHz mono).
pub const UTT_MAX_MS: u32 = 15_000;
/// From `captured` to a verdict. Matches the glasses' own thinking timeout.
pub const RELAY_TIMEOUT_MS: u64 = 20_000;
/// How long past the reply's duration we wait for `playback.done` before showing `done_ok`.
pub const PLAYBACK_GRACE_MS: u64 = 3_000;
/// Reply frame payload: 20 ms of PCM16 16 kHz mono.
pub const REPLY_FRAME_BYTES: usize = 640;
/// PCM16 16 kHz mono: 32 bytes per millisecond.
const BYTES_PER_MS: usize = 32;

/// How the speech endpoint answered, already reduced by the shell.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum RelayResult {
    /// `status:"ok"`; `pcm` is the reply audio (PCM16 16 kHz mono), possibly empty.
    Reply { turn_id: u64, pcm: Vec<u8> },
    /// `status:"no_speech"`.
    NoSpeech { turn_id: u64 },
    /// `status:"error"`, an HTTP failure, the budget, or a timeout in the shell.
    Failed { turn_id: u64, code: String },
}

#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum PhoneEvent {
    /// Compare this code with the one the glasses show, then call `confirm_pairing`.
    PairingCode {
        sas: String,
        glasses_key_hex: String,
    },
    Paired {
        glasses_key_hex: String,
    },
    PairingRefused {
        reason: String,
    },
    /// A connection sent traffic but has not paired (and is not a resumed peer).
    Unauthorized,
    GlassesIndicator {
        state: IndicatorState,
        turn_id: u64,
    },
    GlassesStatus {
        battery: u8,
        charging: bool,
        wifi_rssi: i16,
    },
    /// One complete utterance for the shell to wrap as a WAV and POST.
    Utterance {
        turn_id: u64,
        pcm: Vec<u8>,
        duration_ms: u32,
        truncated: bool,
    },
    UtteranceDropped {
        reason: String,
    },
    ReplyStarted {
        turn_id: u64,
        audio_ms: u32,
    },
    RelayTimedOut {
        turn_id: u64,
    },
    /// The phone has shown the end of a turn (`done_ok`, `done_error` or a quiet `idle`).
    TurnDone {
        turn_id: u64,
        state: IndicatorState,
    },
    /// The link closed, or a newer utterance replaced it, with a turn in flight.
    TurnAborted {
        turn_id: u64,
    },
}

#[derive(Debug, Clone, Default, PartialEq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct PhoneOut {
    /// Complete stream frames, in order.
    pub send: Vec<Vec<u8>>,
    pub events: Vec<PhoneEvent>,
}

enum Phase {
    /// Utterance sent to the endpoint; waiting for the verdict.
    Waiting { deadline: u64 },
    /// Reply audio sent; waiting for the glasses to finish speaking it.
    Playing { utt: u32, deadline: u64 },
}

struct Turn {
    id: u64,
    ind_seq: u64,
    thinking_sent: bool,
    phase: Phase,
}

pub struct PhoneSession {
    make_id: Box<dyn Fn() -> Identity + Send>,
    trusted: Option<[u8; 32]>,
    turn_base: u64,
    turn_n: u64,
    rx: Vec<u8>,
    pairing: Option<ParentPairing>,
    hello_pk: Option<[u8; 32]>,
    code_announced: bool,
    authorized: bool,
    unauthorized_warned: bool,
    out_id: u64,
    out_seq: u64,
    collecting: Option<Vec<u8>>,
    bad_codec: bool,
    turn: Option<Turn>,
}

impl PhoneSession {
    /// `make_id` yields the phone's identity for each pairing (the same key every time).
    /// `trusted`: the glasses key the owner already confirmed; a hello from it is
    /// confirmed without asking again. `turn_base`: any value unique to this process
    /// start (epoch ms): the glasses ignore a reused turn id with a stale sequence number.
    pub fn new(
        make_id: impl Fn() -> Identity + Send + 'static,
        trusted: Option<[u8; 32]>,
        turn_base: u64,
    ) -> Self {
        Self {
            make_id: Box::new(make_id),
            trusted,
            turn_base,
            turn_n: 0,
            rx: Vec::new(),
            pairing: None,
            hello_pk: None,
            code_announced: false,
            authorized: false,
            unauthorized_warned: false,
            out_id: 0,
            out_seq: 0,
            collecting: None,
            bad_codec: false,
            turn: None,
        }
    }

    /// Remember (or forget) the glasses key the owner confirmed, for later connections.
    pub fn set_trusted(&mut self, key: Option<[u8; 32]>) {
        self.trusted = key;
    }

    /// A glasses socket was accepted. `resumed`: same address as an already paired peer.
    pub fn on_connect(&mut self, resumed: bool) {
        self.rx.clear();
        self.pairing = None;
        self.hello_pk = None;
        self.code_announced = false;
        self.authorized = resumed;
        self.unauthorized_warned = false;
        self.collecting = None;
        self.bad_codec = false;
        self.turn = None;
    }

    pub fn on_disconnect(&mut self) -> PhoneOut {
        let mut out = PhoneOut::default();
        if let Some(t) = self.turn.take() {
            out.events.push(PhoneEvent::TurnAborted { turn_id: t.id });
        }
        self.collecting = None;
        self.authorized = false;
        self.pairing = None;
        out
    }

    pub fn is_authorized(&self) -> bool {
        self.authorized
    }

    pub fn pairing_code(&self) -> Option<String> {
        match self.pairing.as_ref()?.state() {
            PairingState::AwaitingConfirm { sas } => Some(sas.clone()),
            _ => None,
        }
    }

    /// The owner compared the codes and they match: issue the certificate.
    /// `cert_clock_ms` should be epoch milliseconds (the glasses compare expiry against a
    /// smaller monotonic clock, so an epoch value can never look expired to them).
    pub fn confirm_pairing(&mut self, cert_clock_ms: u64) -> Result<PhoneOut, NodeError> {
        let mut out = PhoneOut::default();
        let Some(code) = self.pairing_code() else {
            return Err(NodeError::Pairing(
                "no pairing awaiting confirmation".into(),
            ));
        };
        self.do_confirm(&code, cert_clock_ms, &mut out)?;
        Ok(out)
    }

    /// The codes differ or the owner declined: refuse and tell the glasses.
    pub fn reject_pairing(&mut self) -> Result<PhoneOut, NodeError> {
        let mut out = PhoneOut::default();
        if self.pairing_code().is_some() {
            let msg = PairMsg::Refuse {
                reason: "owner rejected the code".into(),
            };
            out.send.push(stream_frame(LANE_PAIR, &msg.to_bytes()?)?);
            out.events.push(PhoneEvent::PairingRefused {
                reason: "owner rejected the code".into(),
            });
            self.pairing = None;
        }
        Ok(out)
    }

    fn do_confirm(&mut self, code: &str, clock: u64, out: &mut PhoneOut) -> Result<(), NodeError> {
        let Some(p) = self.pairing.as_mut() else {
            return Ok(());
        };
        for m in p.confirm_sas(code, clock) {
            out.send.push(stream_frame(LANE_PAIR, &m.to_bytes()?)?);
        }
        if *p.state() == PairingState::Paired {
            self.authorized = true;
            let key = self.hello_pk.map(|k| bytes_to_hex(&k)).unwrap_or_default();
            out.events.push(PhoneEvent::Paired {
                glasses_key_hex: key,
            });
        }
        Ok(())
    }

    // ---- inbound ------------------------------------------------------------------

    /// Raw socket bytes in. `cert_clock_ms` is only used if a trusted glasses is auto-confirmed.
    pub fn on_bytes(
        &mut self,
        bytes: &[u8],
        now_ms: u64,
        cert_clock_ms: u64,
    ) -> Result<PhoneOut, NodeError> {
        self.rx.extend_from_slice(bytes);
        let mut out = PhoneOut::default();
        loop {
            let frame = match frame_decode(&mut self.rx) {
                Ok(Some(f)) => f,
                Ok(None) => break,
                Err(e) => {
                    self.rx.clear();
                    return Err(e);
                }
            };
            self.on_frame(&frame, now_ms, cert_clock_ms, &mut out)?;
        }
        Ok(out)
    }

    fn on_frame(
        &mut self,
        frame: &[u8],
        now: u64,
        clock: u64,
        out: &mut PhoneOut,
    ) -> Result<(), NodeError> {
        let (lane, body) = frame
            .split_first()
            .ok_or_else(|| NodeError::Codec("empty frame".into()))?;
        match *lane {
            LANE_PAIR => self.on_pair(body, clock, out),
            LANE_RELIABLE | LANE_REALTIME if !self.authorized => {
                if !self.unauthorized_warned {
                    self.unauthorized_warned = true;
                    out.events.push(PhoneEvent::Unauthorized);
                }
                Ok(())
            }
            LANE_RELIABLE => match Envelope::from_cbor(body)? {
                Envelope::Evt { event, .. } => self.on_event(event, now, out),
                Envelope::Stat { status, .. } => {
                    out.events.push(PhoneEvent::GlassesStatus {
                        battery: status.battery,
                        charging: status.charging,
                        wifi_rssi: status.wifi_rssi,
                    });
                    Ok(())
                }
                _ => Ok(()),
            },
            LANE_REALTIME => {
                self.on_audio(AudioFrame::decode(body)?);
                Ok(())
            }
            n => Err(NodeError::Codec(format!("unknown lane {n}"))),
        }
    }

    fn on_pair(&mut self, body: &[u8], clock: u64, out: &mut PhoneOut) -> Result<(), NodeError> {
        let msg = PairMsg::from_bytes(body)?;
        if let PairMsg::Hello { pk, .. } = &msg {
            self.hello_pk = pk.as_slice().try_into().ok();
            if self.pairing.is_none() {
                self.pairing = Some(ParentPairing::new(
                    (self.make_id)(),
                    None,
                    CAP_MIC | CAP_SPEAKER | CAP_LED,
                ));
            }
        }
        let Some(p) = self.pairing.as_mut() else {
            return Ok(()); // a pairing message without a hello first
        };
        for m in p.on_message(msg) {
            out.send.push(stream_frame(LANE_PAIR, &m.to_bytes()?)?);
        }
        match p.state().clone() {
            PairingState::AwaitingConfirm { sas } if !self.code_announced => {
                self.code_announced = true;
                if self.trusted.is_some() && self.trusted == self.hello_pk {
                    self.do_confirm(&sas, clock, out)?;
                } else {
                    out.events.push(PhoneEvent::PairingCode {
                        sas,
                        glasses_key_hex: self
                            .hello_pk
                            .map(|k| bytes_to_hex(&k))
                            .unwrap_or_default(),
                    });
                }
            }
            PairingState::Refused { reason } => {
                self.pairing = None;
                out.events.push(PhoneEvent::PairingRefused { reason });
            }
            _ => {}
        }
        Ok(())
    }

    fn on_event(&mut self, ev: Event, now: u64, out: &mut PhoneOut) -> Result<(), NodeError> {
        match ev {
            Event::IndicatorState { state, turn_id } => {
                out.events
                    .push(PhoneEvent::GlassesIndicator { state, turn_id });
                match state {
                    IndicatorState::Listening => {
                        self.collecting = Some(Vec::new());
                        self.bad_codec = false;
                    }
                    IndicatorState::Captured => self.finish_capture(now, out)?,
                    IndicatorState::Thinking => self.confirm_thinking(out)?,
                    _ => self.collecting = None,
                }
            }
            Event::PlaybackDone { utt_id, .. } => {
                let playing = matches!(
                    self.turn.as_ref().map(|t| &t.phase),
                    Some(Phase::Playing { utt, .. }) if *utt == utt_id
                );
                if playing {
                    self.finish(IndicatorState::DoneOk, out)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn on_audio(&mut self, f: AudioFrame) {
        if f.stream != Stream::Up {
            return;
        }
        let Some(buf) = self.collecting.as_mut() else {
            return;
        };
        if f.codec != Codec::Pcm16 {
            self.bad_codec = true;
            return;
        }
        // Bound memory: a stuck-open gate must not grow the buffer without limit.
        if buf.len() < (UTT_MAX_MS as usize + 2_000) * BYTES_PER_MS {
            buf.extend_from_slice(&f.payload);
        }
    }

    fn finish_capture(&mut self, now: u64, out: &mut PhoneOut) -> Result<(), NodeError> {
        let Some(mut pcm) = self.collecting.take() else {
            return Ok(());
        };
        let bad = std::mem::take(&mut self.bad_codec);
        // A new utterance supersedes any turn still in flight; its late verdict is ignored.
        if let Some(old) = self.turn.take() {
            out.events.push(PhoneEvent::TurnAborted { turn_id: old.id });
        }
        pcm.truncate(pcm.len() & !1);
        let ms = (pcm.len() / BYTES_PER_MS) as u32;
        let id = self.turn_base + self.turn_n;
        self.turn_n += 1;
        self.turn = Some(Turn {
            id,
            ind_seq: 0,
            thinking_sent: false,
            phase: Phase::Waiting {
                deadline: now + RELAY_TIMEOUT_MS,
            },
        });
        if bad || ms < UTT_MIN_MS {
            let reason = if bad {
                "uplink codec is not PCM16 (P0 relays PCM16 only)"
            } else {
                "utterance shorter than 0.2 s"
            };
            out.events.push(PhoneEvent::UtteranceDropped {
                reason: reason.into(),
            });
            return self.finish(IndicatorState::Idle, out);
        }
        let truncated = ms > UTT_MAX_MS;
        let ms = ms.min(UTT_MAX_MS);
        pcm.truncate(ms as usize * BYTES_PER_MS);
        out.events.push(PhoneEvent::Utterance {
            turn_id: id,
            pcm,
            duration_ms: ms,
            truncated,
        });
        Ok(())
    }

    /// The glasses reached `thinking` on their own; confirm it with our turn id.
    fn confirm_thinking(&mut self, out: &mut PhoneOut) -> Result<(), NodeError> {
        let Some(t) = self.turn.as_mut() else {
            return Ok(());
        };
        if t.thinking_sent || !matches!(t.phase, Phase::Waiting { .. }) {
            return Ok(());
        }
        t.thinking_sent = true;
        let id = t.id;
        self.send_indicator(IndicatorState::Thinking, id, out)
    }

    // ---- relay verdict, reply audio, time -----------------------------------------

    pub fn relay_result(&mut self, now: u64, r: RelayResult) -> Result<PhoneOut, NodeError> {
        let mut out = PhoneOut::default();
        let (turn_id, waiting) = match &self.turn {
            Some(t) => (t.id, matches!(t.phase, Phase::Waiting { .. })),
            None => return Ok(out),
        };
        let rid = match &r {
            RelayResult::Reply { turn_id, .. }
            | RelayResult::NoSpeech { turn_id }
            | RelayResult::Failed { turn_id, .. } => *turn_id,
        };
        if rid != turn_id || !waiting {
            return Ok(out); // late verdict for a turn that is over
        }
        match r {
            RelayResult::NoSpeech { .. } => self.finish(IndicatorState::Idle, &mut out)?,
            RelayResult::Failed { .. } => self.finish(IndicatorState::DoneError, &mut out)?,
            RelayResult::Reply { mut pcm, .. } => {
                pcm.truncate(pcm.len() & !1);
                if pcm.is_empty() {
                    self.finish(IndicatorState::DoneOk, &mut out)?;
                } else {
                    self.stream_reply(now, turn_id, &pcm, &mut out)?;
                }
            }
        }
        Ok(out)
    }

    fn stream_reply(
        &mut self,
        now: u64,
        turn_id: u64,
        pcm: &[u8],
        out: &mut PhoneOut,
    ) -> Result<(), NodeError> {
        let utt = (turn_id & 0xffff_ffff) as u32;
        let ms = (pcm.len() / BYTES_PER_MS) as u32;
        let begin = self.cmd(Command::PlayBegin { utt_id: utt }, 5_000)?;
        out.send.push(begin);
        for (i, chunk) in pcm.chunks(REPLY_FRAME_BYTES).enumerate() {
            let f = AudioFrame {
                stream: Stream::Down,
                codec: Codec::Pcm16,
                flags: 0,
                utt,
                seq: i as u32,
                ts_ms: (i * REPLY_FRAME_BYTES / BYTES_PER_MS) as u32,
                payload: chunk.to_vec(),
            };
            out.send.push(stream_frame(LANE_REALTIME, &f.encode()?)?);
        }
        if let Some(t) = self.turn.as_mut() {
            t.phase = Phase::Playing {
                utt,
                deadline: now + u64::from(ms) + PLAYBACK_GRACE_MS,
            };
        }
        out.events.push(PhoneEvent::ReplyStarted {
            turn_id,
            audio_ms: ms,
        });
        Ok(())
    }

    /// Call at `next_deadline`.
    pub fn tick(&mut self, now: u64) -> Result<PhoneOut, NodeError> {
        let mut out = PhoneOut::default();
        let Some(t) = &self.turn else {
            return Ok(out);
        };
        let id = t.id;
        match t.phase {
            Phase::Waiting { deadline } if now >= deadline => {
                out.events.push(PhoneEvent::RelayTimedOut { turn_id: id });
                self.finish(IndicatorState::DoneError, &mut out)?;
            }
            // The glasses never said the reply finished: show done anyway.
            Phase::Playing { deadline, .. } if now >= deadline => {
                self.finish(IndicatorState::DoneOk, &mut out)?;
            }
            _ => {}
        }
        Ok(out)
    }

    pub fn next_deadline(&self) -> Option<u64> {
        match self.turn.as_ref()?.phase {
            Phase::Waiting { deadline } | Phase::Playing { deadline, .. } => Some(deadline),
        }
    }

    // ---- outbound -----------------------------------------------------------------

    fn finish(&mut self, state: IndicatorState, out: &mut PhoneOut) -> Result<(), NodeError> {
        let Some(t) = self.turn.as_ref() else {
            return Ok(());
        };
        let id = t.id;
        self.send_indicator(state, id, out)?;
        self.turn = None;
        out.events.push(PhoneEvent::TurnDone { turn_id: id, state });
        Ok(())
    }

    fn send_indicator(
        &mut self,
        state: IndicatorState,
        turn_id: u64,
        out: &mut PhoneOut,
    ) -> Result<(), NodeError> {
        let seq = match self.turn.as_mut() {
            Some(t) => {
                t.ind_seq += 1;
                t.ind_seq
            }
            None => 1,
        };
        let f = self.cmd(
            Command::IndicatorSet {
                state,
                turn_id,
                seq,
            },
            5_000,
        )?;
        out.send.push(f);
        Ok(())
    }

    fn cmd(&mut self, cmd: Command, ttl_ms: u32) -> Result<Vec<u8>, NodeError> {
        self.out_id += 1;
        self.out_seq += 1;
        let env = Envelope::Cmd {
            id: self.out_id,
            seq: self.out_seq,
            ttl_ms,
            cmd,
        };
        stream_frame(LANE_RELIABLE, &env.to_cbor()?)
    }
}
