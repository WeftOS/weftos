//! P0 loopback: a fake glasses node and the phone core pair over a fake transport, then
//! exchange indicator transitions and audio frames in both directions. Host only.

use weftos_node_core::audio::{AudioFrame, Codec, Stream};
use weftos_node_core::identity::Identity;
use weftos_node_core::indicator::{
    Indicator, IndicatorAction, IndicatorInput, IndicatorState as S,
};
use weftos_node_core::pairing::*;
use weftos_node_core::proto::{Command, Envelope, Event};
use weftos_node_core::transport::{FakeTransport, Inbound, Link};

fn drain_pair(l: &mut Link<FakeTransport>) -> Vec<PairMsg> {
    let mut v = vec![];
    while let Some(m) = l.recv().unwrap() {
        match m {
            Inbound::Pair(b) => v.push(PairMsg::from_bytes(&b).unwrap()),
            other => panic!("unexpected during pairing: {other:?}"),
        }
    }
    v
}

#[test]
fn pair_then_indicator_transitions_and_audio() {
    let dir_p = tempfile::tempdir().unwrap();
    let dir_g = tempfile::tempdir().unwrap();
    let phone_id = Identity::load_or_create(dir_p.path()).unwrap();
    let glasses_id = Identity::load_or_create(dir_g.path()).unwrap();
    let glasses_pk = glasses_id.public_key();
    let phone_pk = phone_id.public_key();

    let (gt, pt) = FakeTransport::pair();
    let (mut g_link, mut p_link) = (Link::new(gt), Link::new(pt));

    // ---- pairing ----
    let mut g = GlassesPairing::new(glasses_id);
    let mut p = ParentPairing::new(phone_id, Some(glasses_pk), CAP_MIC | CAP_SPEAKER | CAP_LED);
    g_link.send_pair(&g.start().to_bytes().unwrap()).unwrap();
    for m in drain_pair(&mut p_link) {
        for out in p.on_message(m) {
            p_link.send_pair(&out.to_bytes().unwrap()).unwrap();
        }
    }
    for m in drain_pair(&mut g_link) {
        g.on_message(m, 10);
    }
    let GlassesState::AwaitingCert { sas } = g.state().clone() else {
        panic!("{:?}", g.state())
    };
    assert_eq!(
        *p.state(),
        PairingState::AwaitingConfirm { sas: sas.clone() }
    );
    for out in p.confirm_sas(&sas, 10) {
        p_link.send_pair(&out.to_bytes().unwrap()).unwrap();
    }
    for m in drain_pair(&mut g_link) {
        g.on_message(m, 10);
    }
    assert_eq!(*p.state(), PairingState::Paired);
    assert!(matches!(g.state(), GlassesState::Paired { parent_pk } if *parent_pk == phone_pk));

    // ---- indicator: glasses own the fast edges, phone sends done_ok ----
    let mut ind = Indicator::new();
    let mut gseq = 0u64;
    let mut report = |link: &mut Link<FakeTransport>, acts: Vec<IndicatorAction>| {
        for a in acts {
            if let IndicatorAction::Report { state, turn_id } = a {
                gseq += 1;
                link.send_envelope(&Envelope::Evt {
                    seq: gseq,
                    event: Event::IndicatorState { state, turn_id },
                })
                .unwrap();
            }
        }
    };
    let acts = ind.handle(0, IndicatorInput::GateOpen);
    report(&mut g_link, acts);
    // three utterance frames up while the gate is open
    for seq in 0..3u32 {
        g_link
            .send_audio(&AudioFrame {
                stream: Stream::Up,
                codec: Codec::Opus,
                flags: 0,
                utt: 1,
                seq,
                ts_ms: seq * 20,
                payload: vec![seq as u8; 60],
            })
            .unwrap();
    }
    let acts = ind.handle(100, IndicatorInput::GateClose);
    report(&mut g_link, acts);
    let acts = ind.handle(500, IndicatorInput::Tick);
    report(&mut g_link, acts);

    // phone side: collect events and audio
    let mut states = vec![];
    let mut audio_up = vec![];
    while let Some(m) = p_link.recv().unwrap() {
        match m {
            Inbound::Envelope(Envelope::Evt {
                event: Event::IndicatorState { state, .. },
                ..
            }) => states.push(state),
            Inbound::Audio(f) => audio_up.push(f),
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(states, vec![S::Listening, S::Captured, S::Thinking]);
    assert_eq!(
        audio_up.iter().map(|f| f.seq).collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    assert!(
        audio_up
            .iter()
            .all(|f| f.stream == Stream::Up && f.payload.len() == 60)
    );

    // phone answers: reply audio down, then indicator.set{done_ok}
    for seq in 0..2u32 {
        p_link
            .send_audio(&AudioFrame {
                stream: Stream::Down,
                codec: Codec::Opus,
                flags: 0,
                utt: 2,
                seq,
                ts_ms: seq * 20,
                payload: vec![0xEE; 40],
            })
            .unwrap();
    }
    p_link
        .send_envelope(&Envelope::Cmd {
            id: 1,
            seq: 1,
            ttl_ms: 5000,
            cmd: Command::IndicatorSet {
                state: S::DoneOk,
                turn_id: 4,
                seq: 1,
            },
        })
        .unwrap();

    let mut audio_down = 0;
    while let Some(m) = g_link.recv().unwrap() {
        match m {
            Inbound::Audio(f) => {
                assert_eq!(f.stream, Stream::Down);
                audio_down += 1;
                if audio_down == 1 {
                    let acts = ind.handle(600, IndicatorInput::ReplyAudioStarted);
                    assert_eq!(acts, vec![IndicatorAction::StopThinkingLoop]);
                }
            }
            Inbound::Envelope(Envelope::Cmd {
                id,
                cmd:
                    Command::IndicatorSet {
                        state,
                        turn_id,
                        seq,
                    },
                ..
            }) => {
                let acts = ind.handle(
                    700,
                    IndicatorInput::Set {
                        state,
                        turn_id,
                        seq,
                    },
                );
                report(&mut g_link, acts);
                g_link
                    .send_envelope(&Envelope::Res {
                        id,
                        ok: true,
                        err: None,
                    })
                    .unwrap();
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(audio_down, 2);
    assert_eq!(ind.state(), S::DoneOk);

    // phone sees the glasses' own report of done_ok and the ack
    let mut saw_done = false;
    let mut acked = false;
    while let Some(m) = p_link.recv().unwrap() {
        match m {
            Inbound::Envelope(Envelope::Evt {
                event:
                    Event::IndicatorState {
                        state: S::DoneOk,
                        turn_id: 4,
                    },
                ..
            }) => saw_done = true,
            Inbound::Envelope(Envelope::Res {
                id: 1, ok: true, ..
            }) => acked = true,
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(saw_done && acked);

    // finally the done flash ends and the LED is released
    let acts = ind.handle(700 + 1200, IndicatorInput::Tick);
    assert!(acts.contains(&IndicatorAction::ReleaseLed));
    assert_eq!(ind.state(), S::Idle);
}

#[test]
fn wrong_glasses_cannot_pair_in_loopback() {
    let dir = tempfile::tempdir().unwrap();
    let phone = Identity::load_or_create(dir.path()).unwrap();
    let beacon_key = Identity::from_seed([5; 32]).public_key();
    let impostor = Identity::from_seed([6; 32]);

    let (gt, pt) = FakeTransport::pair();
    let (mut g_link, mut p_link) = (Link::new(gt), Link::new(pt));
    let mut g = GlassesPairing::new(impostor);
    let mut p = ParentPairing::new(phone, Some(beacon_key), CAP_MIC);
    g_link.send_pair(&g.start().to_bytes().unwrap()).unwrap();
    for m in drain_pair(&mut p_link) {
        for out in p.on_message(m) {
            p_link.send_pair(&out.to_bytes().unwrap()).unwrap();
        }
    }
    for m in drain_pair(&mut g_link) {
        g.on_message(m, 1);
    }
    assert!(matches!(p.state(), PairingState::Refused { .. }));
    assert!(matches!(g.state(), GlassesState::Refused { .. }));
    assert!(p.certificate().is_none() && g.certificate().is_none());
}
