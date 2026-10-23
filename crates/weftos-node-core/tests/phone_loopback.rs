//! Host loopback for the phone's glasses link (`PhoneSession`): a real `GlassesNode` (the
//! glasses agent's protocol layer) talks to the phone core through stream framing. The
//! phone assembles the utterance, a stand-in for the Android shell wraps it in a WAV and
//! POSTs it to a fake speech endpoint (a real loopback HTTP server with a canned answer),
//! and the verdict goes back through the core as reply frames and indicator commands.
//! No Android, no real network.

use base64::Engine;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread::JoinHandle;
use weftos_node_core::glasses::{GlassesNode, GlassesPairingState, Handled, ShellCommand};
use weftos_node_core::identity::Identity;
use weftos_node_core::indicator::{
    CAPTURED_MS, IndicatorAction, IndicatorInput, IndicatorState as S,
};
use weftos_node_core::phone::{
    PLAYBACK_GRACE_MS, PhoneEvent, PhoneOut, PhoneSession, RELAY_TIMEOUT_MS, RelayResult,
};

const EPOCH: u64 = 1_790_000_000_000;

// ---- a stand-in for the shell: WAV, HTTP, response mapping ------------------------------

fn wav(pcm: &[u8]) -> Vec<u8> {
    let mut v = b"RIFF".to_vec();
    v.extend_from_slice(&(36 + pcm.len() as u32).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes()); // PCM
    v.extend_from_slice(&1u16.to_le_bytes()); // mono
    v.extend_from_slice(&16_000u32.to_le_bytes());
    v.extend_from_slice(&32_000u32.to_le_bytes());
    v.extend_from_slice(&2u16.to_le_bytes());
    v.extend_from_slice(&16u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
    v.extend_from_slice(pcm);
    v
}

#[derive(Debug, Default)]
struct Seen {
    request_line: String,
    auth: String,
    content_type: String,
    turn: String,
    body: Vec<u8>,
}

/// One-shot fake of `POST /api/speech/turn`: records the request, answers `canned`.
fn fake_endpoint(canned: String) -> (String, JoinHandle<Seen>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap().to_string();
    let h = std::thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        let head_end = loop {
            let n = s.read(&mut tmp).unwrap();
            assert!(n > 0, "client closed before the headers ended");
            buf.extend_from_slice(&tmp[..n]);
            if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break i + 4;
            }
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
        let mut seen = Seen::default();
        let mut len = 0usize;
        for (i, line) in head.split("\r\n").enumerate() {
            if i == 0 {
                seen.request_line = line.to_string();
            }
            let low = line.to_ascii_lowercase();
            if let Some(v) = low.strip_prefix("content-length:") {
                len = v.trim().parse().unwrap();
            }
            let val = |p: &str| line[p.len()..].trim().to_string();
            if low.starts_with("authorization:") {
                seen.auth = val("authorization:");
            }
            if low.starts_with("content-type:") {
                seen.content_type = val("content-type:");
            }
            if low.starts_with("x-weftos-turn:") {
                seen.turn = val("x-weftos-turn:");
            }
        }
        while buf.len() < head_end + len {
            let n = s.read(&mut tmp).unwrap();
            buf.extend_from_slice(&tmp[..n]);
        }
        seen.body = buf[head_end..head_end + len].to_vec();
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            canned.len(),
            canned
        );
        s.write_all(resp.as_bytes()).unwrap();
        seen
    });
    (addr, h)
}

fn post(addr: &str, token: &str, turn: u64, body: &[u8]) -> String {
    let mut s = TcpStream::connect(addr).unwrap();
    let head = format!(
        "POST /api/speech/turn HTTP/1.1\r\nHost: speech\r\nAuthorization: Bearer {token}\r\nContent-Type: audio/wav\r\nX-Weftos-Endpoint: glasses\r\nX-Weftos-Turn: {turn}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes()).unwrap();
    s.write_all(body).unwrap();
    let mut resp = String::new();
    s.read_to_string(&mut resp).unwrap();
    resp.split_once("\r\n\r\n").unwrap().1.to_string()
}

/// What the Android shell does with the JSON answer.
fn map(turn_id: u64, json: &str) -> RelayResult {
    let v: serde_json::Value = serde_json::from_str(json).unwrap();
    match v["status"].as_str() {
        Some("ok") => RelayResult::Reply {
            turn_id,
            pcm: base64::engine::general_purpose::STANDARD
                .decode(v["audio"]["b64"].as_str().unwrap())
                .unwrap(),
        },
        Some("no_speech") => RelayResult::NoSpeech { turn_id },
        _ => RelayResult::Failed {
            turn_id,
            code: v["code"].as_str().unwrap_or("error").to_string(),
        },
    }
}

fn ok_json(reply_pcm: &[u8]) -> String {
    serde_json::json!({
        "status": "ok", "turn_id": "t1", "transcript": "what time is it",
        "reply_text": "It is noon.", "spoken_text": "It is noon.",
        "audio": {"format": "pcm_s16le", "rate": 16000, "channels": 1,
                  "b64": base64::engine::general_purpose::STANDARD.encode(reply_pcm)},
        "timings_ms": {"stt": 300, "llm": 900, "tts": 400}
    })
    .to_string()
}

// ---- the rig -------------------------------------------------------------------------

struct Rig {
    g: GlassesNode,
    p: PhoneSession,
    now: u64,
    events: Vec<PhoneEvent>,
    /// Indicator states the phone has seen the glasses report, in order.
    shown: Vec<S>,
    actions: Vec<IndicatorAction>,
    commands: Vec<ShellCommand>,
    reply_frames: usize,
}

impl Rig {
    fn new(trusted: Option<[u8; 32]>) -> Self {
        let g = GlassesNode::new(Identity::from_seed([6; 32]));
        let mut p = PhoneSession::new(|| Identity::from_seed([1; 32]), trusted, 5_000);
        p.on_connect(false);
        Self {
            g,
            p,
            now: 0,
            events: vec![],
            shown: vec![],
            actions: vec![],
            commands: vec![],
            reply_frames: 0,
        }
    }

    /// Glasses output to the phone, then phone output back, until both are quiet.
    fn pump(&mut self, mut g_out: Vec<Vec<u8>>) {
        while !g_out.is_empty() {
            let mut p_out = PhoneOut::default();
            for f in g_out.drain(..) {
                // split into 5-byte reads to exercise reassembly
                for c in f.chunks(5) {
                    let o = self.p.on_bytes(c, self.now, EPOCH).unwrap();
                    p_out.send.extend(o.send);
                    p_out.events.extend(o.events);
                }
            }
            self.take(p_out, &mut g_out);
        }
    }

    /// Record phone events and deliver its frames to the glasses; replies go to `g_out`.
    fn take(&mut self, out: PhoneOut, g_out: &mut Vec<Vec<u8>>) {
        for e in &out.events {
            if let PhoneEvent::GlassesIndicator { state, .. } = e {
                self.shown.push(*state);
            }
        }
        self.events.extend(out.events);
        for f in out.send {
            let h = self.g.on_bytes(&f, self.now).unwrap();
            self.absorb(h, g_out);
        }
    }

    fn absorb(&mut self, h: Handled, g_out: &mut Vec<Vec<u8>>) {
        g_out.extend(h.send);
        self.actions.extend(h.actions);
        self.commands.extend(h.commands);
        self.reply_frames += h.audio.len();
    }

    fn glasses_feed(&mut self, now: u64, i: IndicatorInput) {
        self.now = now;
        let h = self.g.feed(now, i).unwrap();
        let mut g_out = vec![];
        self.absorb(h, &mut g_out);
        self.pump(g_out);
    }

    fn phone(&mut self, out: PhoneOut) {
        let mut g_out = vec![];
        self.take(out, &mut g_out);
        self.pump(g_out);
    }

    /// Pairing with the owner comparing codes. Returns the code both sides showed.
    fn pair(&mut self) -> String {
        let hello = self.g.pair_hello().unwrap();
        self.pump(vec![hello]);
        let code = self.p.pairing_code().expect("phone shows a code");
        let GlassesPairingState::AwaitingCert { sas } = self.g.pairing_state() else {
            panic!("{:?}", self.g.pairing_state())
        };
        assert_eq!(sas, code, "owner sees the same code on both devices");
        let out = self.p.confirm_pairing(EPOCH).unwrap();
        self.phone(out);
        assert!(matches!(
            self.g.pairing_state(),
            GlassesPairingState::Paired { .. }
        ));
        code
    }

    /// The glasses speak for `frames` 20 ms frames; returns the time at `captured`.
    fn speak(&mut self, t0: u64, frames: u32) -> u64 {
        self.glasses_feed(t0, IndicatorInput::GateOpen);
        let mut out = vec![];
        for i in 0..frames {
            out.push(
                self.g
                    .audio_up_frame(
                        weftos_node_core::audio::Codec::Pcm16,
                        1,
                        i,
                        i * 20,
                        vec![(i % 200) as u8; 640],
                    )
                    .unwrap(),
            );
        }
        self.pump(out);
        let t1 = t0 + u64::from(frames) * 20;
        self.glasses_feed(t1, IndicatorInput::GateClose);
        t1
    }

    fn utterance(&self) -> Option<(u64, Vec<u8>, u32)> {
        self.events.iter().rev().find_map(|e| match e {
            PhoneEvent::Utterance {
                turn_id,
                pcm,
                duration_ms,
                ..
            } => Some((*turn_id, pcm.clone(), *duration_ms)),
            _ => None,
        })
    }

    fn turn_done(&self) -> Option<S> {
        self.events.iter().rev().find_map(|e| match e {
            PhoneEvent::TurnDone { state, .. } => Some(*state),
            _ => None,
        })
    }
}

#[test]
fn full_turn_glasses_to_endpoint_and_reply_back_with_indicator_order() {
    let mut r = Rig::new(None);
    r.pair();
    assert!(r.p.is_authorized());

    // glasses capture 1.0 s
    let t1 = r.speak(1_000, 50);
    assert_eq!(r.g.indicator_state(), S::Captured);
    let (turn, pcm, ms) = r.utterance().expect("utterance assembled");
    assert_eq!((pcm.len(), ms), (32_000, 1_000));
    assert_eq!(turn, 5_000, "turn ids start at the process base");
    assert_eq!(r.p.next_deadline(), Some(t1 + RELAY_TIMEOUT_MS));

    // shell: WAV, POST, canned answer (0.5 s of reply audio)
    let reply_pcm: Vec<u8> = (0..16_000u32).map(|i| (i % 251) as u8).collect();
    let (addr, srv) = fake_endpoint(ok_json(&reply_pcm));
    let body = post(&addr, "node-token-abc", turn, &wav(&pcm));
    let seen = srv.join().unwrap();
    assert_eq!(seen.request_line, "POST /api/speech/turn HTTP/1.1");
    assert_eq!(seen.auth, "Bearer node-token-abc");
    assert_eq!(seen.content_type, "audio/wav");
    assert_eq!(seen.turn, "5000");
    assert_eq!(&seen.body[..4], b"RIFF");
    assert_eq!(&seen.body[44..], &pcm[..], "the assembled PCM, unmodified");
    assert!(seen.body.len() < 500_000);

    // 400 ms after captured the glasses enter thinking; the phone confirms it
    r.glasses_feed(t1 + CAPTURED_MS, IndicatorInput::Tick);
    assert_eq!(r.g.indicator_state(), S::Thinking);
    assert_eq!(r.g.turn_id(), 5_000, "glasses adopted the phone's turn id");

    // reply: first frame stops the thinking loop; the state stays thinking until done
    let t2 = t1 + 1_500;
    r.now = t2;
    r.actions.clear();
    let out = r.p.relay_result(t2, map(turn, &body)).unwrap();
    assert!(out.events.contains(&PhoneEvent::ReplyStarted {
        turn_id: 5_000,
        audio_ms: 500
    }));
    r.phone(out);
    assert_eq!(r.reply_frames, 25, "500 ms in 20 ms frames");
    assert!(r.actions.contains(&IndicatorAction::StopThinkingLoop));
    assert!(
        r.commands
            .iter()
            .any(|c| matches!(c, ShellCommand::PlayBegin { .. }))
    );
    assert_eq!(r.g.indicator_state(), S::Thinking);
    assert!(
        r.turn_done().is_none(),
        "done_ok waits for the glasses to finish speaking"
    );

    // the glasses report the reply played out; the phone shows done_ok
    r.now = t2 + 600;
    let f = r.g.playback_done_frame(5_000, 24).unwrap();
    r.pump(vec![f]);
    assert_eq!(r.g.indicator_state(), S::DoneOk);
    assert_eq!(r.turn_done(), Some(S::DoneOk));
    // the glasses repeat `thinking` once when they accept the phone's confirmation
    let mut order = r.shown.clone();
    order.dedup();
    assert_eq!(
        order,
        vec![S::Listening, S::Captured, S::Thinking, S::DoneOk],
        "indicator states in order"
    );
    assert!(r.p.next_deadline().is_none());

    // and the glasses go idle after the flash
    r.glasses_feed(t2 + 600 + 1_200, IndicatorInput::Tick);
    assert_eq!(r.g.indicator_state(), S::Idle);
}

#[test]
fn error_response_budget_and_timeout_end_in_done_error() {
    // endpoint error (e.g. budget exceeded)
    let mut r = Rig::new(None);
    r.pair();
    let t1 = r.speak(0, 20);
    let (turn, pcm, _) = r.utterance().unwrap();
    let (addr, srv) = fake_endpoint(r#"{"status":"error","code":"budget","message":"cap"}"#.into());
    let body = post(&addr, "t", turn, &wav(&pcm));
    srv.join().unwrap();
    r.glasses_feed(t1 + CAPTURED_MS, IndicatorInput::Tick);
    let out = r.p.relay_result(t1 + 900, map(turn, &body)).unwrap();
    r.phone(out);
    assert_eq!(r.g.indicator_state(), S::DoneError);
    assert_eq!(r.turn_done(), Some(S::DoneError));
    assert_eq!(r.reply_frames, 0);

    // no answer within 20 s
    let mut r = Rig::new(None);
    r.pair();
    let t1 = r.speak(0, 20);
    let (turn, _, _) = r.utterance().unwrap();
    r.glasses_feed(t1 + CAPTURED_MS, IndicatorInput::Tick);
    assert!(r.p.tick(t1 + RELAY_TIMEOUT_MS - 1).unwrap().send.is_empty());
    let out = r.p.tick(t1 + RELAY_TIMEOUT_MS).unwrap();
    assert!(
        out.events
            .contains(&PhoneEvent::RelayTimedOut { turn_id: turn })
    );
    r.phone(out);
    assert_eq!(r.g.indicator_state(), S::DoneError);
    // a verdict that arrives after the timeout changes nothing
    let late =
        r.p.relay_result(
            t1 + RELAY_TIMEOUT_MS + 5,
            RelayResult::Reply {
                turn_id: turn,
                pcm: vec![0; 640],
            },
        )
        .unwrap();
    assert_eq!(late, PhoneOut::default());
}

#[test]
fn no_speech_returns_quietly_to_idle() {
    let mut r = Rig::new(None);
    r.pair();
    let t1 = r.speak(0, 20);
    let (turn, pcm, _) = r.utterance().unwrap();
    let (addr, srv) = fake_endpoint(r#"{"status":"no_speech","turn_id":"x"}"#.into());
    let body = post(&addr, "t", turn, &wav(&pcm));
    srv.join().unwrap();
    r.glasses_feed(t1 + CAPTURED_MS, IndicatorInput::Tick);
    r.actions.clear();
    let out = r.p.relay_result(t1 + 800, map(turn, &body)).unwrap();
    r.phone(out);
    assert_eq!(r.g.indicator_state(), S::Idle);
    assert_eq!(r.turn_done(), Some(S::Idle));
    assert!(
        !r.actions
            .iter()
            .any(|a| matches!(a, IndicatorAction::PlayCue { .. })),
        "no chime on no_speech: {:?}",
        r.actions
    );
}

#[test]
fn missing_playback_done_falls_back_to_a_timer() {
    let mut r = Rig::new(None);
    r.pair();
    let t1 = r.speak(0, 20);
    let (turn, _, _) = r.utterance().unwrap();
    r.glasses_feed(t1 + CAPTURED_MS, IndicatorInput::Tick);
    let out =
        r.p.relay_result(
            t1 + 700,
            RelayResult::Reply {
                turn_id: turn,
                pcm: vec![1; 32_000], // 1 s
            },
        )
        .unwrap();
    r.phone(out);
    let deadline = r.p.next_deadline().unwrap();
    assert_eq!(deadline, t1 + 700 + 1_000 + PLAYBACK_GRACE_MS);
    r.now = deadline;
    let out = r.p.tick(deadline).unwrap();
    r.phone(out);
    assert_eq!(r.g.indicator_state(), S::DoneOk);
}

#[test]
fn unpaired_traffic_is_ignored_and_resumed_peers_are_accepted() {
    // not paired, not resumed: audio and indicator reports are dropped
    let mut r = Rig::new(None);
    r.glasses_feed(0, IndicatorInput::GateOpen);
    assert!(r.events.contains(&PhoneEvent::Unauthorized));
    assert!(r.utterance().is_none());
    assert!(!r.p.is_authorized());

    // a reconnect from the paired address is accepted without a new hello
    let mut r = Rig::new(None);
    r.p.on_connect(true);
    let t1 = r.speak(0, 20);
    assert!(r.utterance().is_some(), "resumed peer's utterance relayed");
    let _ = t1;
    let out = r.p.on_disconnect();
    assert!(matches!(out.events[0], PhoneEvent::TurnAborted { .. }));
    assert!(!r.p.is_authorized());
}

#[test]
fn trusted_glasses_pair_without_asking_and_short_utterances_are_dropped() {
    let known = Identity::from_seed([6; 32]).public_key();
    let mut r = Rig::new(Some(known));
    let hello = r.g.pair_hello().unwrap();
    r.pump(vec![hello]);
    assert!(
        !r.events
            .iter()
            .any(|e| matches!(e, PhoneEvent::PairingCode { .. })),
        "no code for a key the owner already confirmed"
    );
    assert!(r.p.is_authorized());
    assert!(matches!(
        r.g.pairing_state(),
        GlassesPairingState::Paired { .. }
    ));

    // 100 ms is below the endpoint's 0.2 s floor: no request, glasses go quietly idle
    let t1 = r.speak(0, 5);
    assert!(r.utterance().is_none());
    assert!(
        r.events
            .iter()
            .any(|e| matches!(e, PhoneEvent::UtteranceDropped { .. }))
    );
    assert_eq!(r.g.indicator_state(), S::Idle);
    let _ = t1;
}

#[test]
fn owner_rejecting_the_code_refuses_both_sides() {
    let mut r = Rig::new(None);
    let hello = r.g.pair_hello().unwrap();
    r.pump(vec![hello]);
    let out = r.p.reject_pairing().unwrap();
    r.phone(out);
    assert!(!r.p.is_authorized());
    assert!(matches!(
        r.g.pairing_state(),
        GlassesPairingState::Refused { .. }
    ));
}

#[test]
fn an_overlong_utterance_is_cut_to_fifteen_seconds() {
    let mut r = Rig::new(None);
    r.p.on_connect(true);
    r.speak(0, 800); // 16 s
    let (_, pcm, ms) = r.utterance().unwrap();
    assert_eq!(ms, 15_000);
    assert_eq!(pcm.len(), 480_000);
    assert!(r.events.iter().any(|e| matches!(
        e,
        PhoneEvent::Utterance {
            truncated: true,
            ..
        }
    )));
}

#[test]
fn a_restarted_glasses_agent_pairs_again_without_a_code_once_confirmed() {
    let mut r = Rig::new(None);
    r.pair();
    let key = Identity::from_seed([6; 32]).public_key();
    r.p.set_trusted(Some(key));
    // the glasses agent restarts: same identity, fresh memory, new socket
    r.p.on_connect(false);
    r.g = GlassesNode::new(Identity::from_seed([6; 32]));
    r.events.clear();
    let hello = r.g.pair_hello().unwrap();
    r.pump(vec![hello]);
    assert!(
        !r.events
            .iter()
            .any(|e| matches!(e, PhoneEvent::PairingCode { .. }))
    );
    assert!(r.p.is_authorized());
    assert!(matches!(
        r.g.pairing_state(),
        GlassesPairingState::Paired { .. }
    ));
    // an unknown key still needs the owner
    r.p.on_connect(false);
    r.g = GlassesNode::new(Identity::from_seed([7; 32]));
    let hello = r.g.pair_hello().unwrap();
    r.pump(vec![hello]);
    assert!(!r.p.is_authorized());
    assert!(r.p.pairing_code().is_some());
}
