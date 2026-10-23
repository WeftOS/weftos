//! Host loopback for the glasses agent's protocol layer (`GlassesNode`): the existing
//! phone-core fake (`Link` over `FakeTransport`, `ParentPairing`) talks to a real
//! `GlassesNode` through stream framing, exchanging pairing, indicator transitions and
//! audio frames in both directions. No Android, no sockets.

use weftos_node_core::audio::{AudioFrame, Codec, Stream};
use weftos_node_core::glasses::{GlassesNode, GlassesPairingState, GlassesStatus, ShellCommand};
use weftos_node_core::identity::Identity;
use weftos_node_core::indicator::{IndicatorAction, IndicatorInput, IndicatorState as S};
use weftos_node_core::pairing::{
    CAP_LED, CAP_MIC, CAP_SPEAKER, PairMsg, PairingState, ParentPairing,
};
use weftos_node_core::proto::{Command, Envelope, Event, LedAvailability};
use weftos_node_core::transport::{
    FakeTransport, Inbound, Link, Transport, frame_decode, frame_encode,
};

/// A glasses node wired to the phone's `Link` through the stream framing a TCP socket would add.
struct Rig {
    node: GlassesNode,
    /// The glasses end of the fake duplex, used raw (frames are lane-prefixed, no length).
    wire: FakeTransport,
    phone: Link<FakeTransport>,
    now: u64,
}

impl Rig {
    fn new(node: GlassesNode) -> Self {
        let (gt, pt) = FakeTransport::pair();
        Self {
            node,
            wire: gt,
            phone: Link::new(pt),
            now: 0,
        }
    }

    /// Write what the glasses produced onto the wire (strip the stream length prefix).
    fn glasses_writes(&mut self, frames: &[Vec<u8>]) {
        for f in frames {
            let mut b = f.clone();
            let inner = frame_decode(&mut b).unwrap().unwrap();
            assert!(b.is_empty(), "one frame per write");
            self.wire.send(inner).unwrap();
        }
    }

    /// Deliver everything the phone sent to the glasses, as socket bytes (re-adding the
    /// length prefix, split into 7-byte reads to exercise reassembly).
    fn deliver_to_glasses(&mut self) -> Vec<weftos_node_core::glasses::Handled> {
        let mut out = vec![];
        while let Some(inner) = self.wire.recv() {
            let bytes = frame_encode(&inner).unwrap();
            let mut last = None;
            for chunk in bytes.chunks(7) {
                let h = self.node.on_bytes(chunk, self.now).unwrap();
                self.glasses_writes(&h.send);
                if !h.send.is_empty()
                    || !h.actions.is_empty()
                    || !h.commands.is_empty()
                    || !h.audio.is_empty()
                {
                    last = Some(h);
                }
            }
            out.extend(last);
        }
        out
    }

    fn feed(&mut self, now: u64, i: IndicatorInput) -> Vec<IndicatorAction> {
        self.now = now;
        let h = self.node.feed(now, i).unwrap();
        self.glasses_writes(&h.send);
        h.actions
    }
}

fn drain_pair_to_phone(rig: &mut Rig, p: &mut ParentPairing) {
    while let Some(m) = rig.phone.recv().unwrap() {
        let Inbound::Pair(b) = m else {
            panic!("unexpected during pairing: {m:?}")
        };
        for out in p.on_message(PairMsg::from_bytes(&b).unwrap()) {
            rig.phone.send_pair(&out.to_bytes().unwrap()).unwrap();
        }
    }
}

#[test]
fn pairing_indicators_audio_and_commands_round_trip() {
    let dir_p = tempfile::tempdir().unwrap();
    let dir_g = tempfile::tempdir().unwrap();
    let phone_id = Identity::load_or_create(dir_p.path()).unwrap();
    let glasses_id = Identity::load_or_create(dir_g.path()).unwrap();
    let glasses_pk = glasses_id.public_key();
    let mut rig = Rig::new(GlassesNode::new(glasses_id));
    let mut p = ParentPairing::new(phone_id, Some(glasses_pk), CAP_MIC | CAP_SPEAKER | CAP_LED);

    // ---- pairing over the pairing lane ----
    let hello = rig.node.pair_hello().unwrap();
    rig.glasses_writes(&[hello]);
    drain_pair_to_phone(&mut rig, &mut p);
    rig.deliver_to_glasses();
    let GlassesPairingState::AwaitingCert { sas } = rig.node.pairing_state() else {
        panic!("{:?}", rig.node.pairing_state())
    };
    for out in p.confirm_sas(&sas, 10) {
        rig.phone.send_pair(&out.to_bytes().unwrap()).unwrap();
    }
    rig.deliver_to_glasses();
    assert_eq!(*p.state(), PairingState::Paired);
    assert!(matches!(
        rig.node.pairing_state(),
        GlassesPairingState::Paired { .. }
    ));

    // ---- glasses own the fast edges: gate opens, audio flows up, gate closes ----
    let acts = rig.feed(0, IndicatorInput::GateOpen);
    assert!(
        acts.iter()
            .any(|a| matches!(a, IndicatorAction::PlayCue { .. }))
    );
    let f = rig
        .node
        .audio_up_frame(Codec::Pcm16, 1, 0, 0, vec![3; 640])
        .unwrap();
    rig.glasses_writes(&[f]);
    rig.feed(100, IndicatorInput::GateClose);
    rig.feed(500, IndicatorInput::Tick);
    let st = rig
        .node
        .status_frame(&GlassesStatus {
            battery: 90,
            charging: false,
            wifi_rssi: -48,
            wifi_rate_mbps: 390,
            mic_open: false,
            mic_level: 10,
            speaking: false,
            led_available: false,
            ver: "0.1.0-p0".into(),
        })
        .unwrap();
    rig.glasses_writes(&[st]);

    let (mut states, mut audio_up, mut led_status) = (vec![], vec![], None);
    while let Some(m) = rig.phone.recv().unwrap() {
        match m {
            Inbound::Envelope(Envelope::Evt {
                event: Event::IndicatorState { state, .. },
                ..
            }) => states.push(state),
            Inbound::Envelope(Envelope::Stat { status, .. }) => led_status = Some(status.led),
            Inbound::Audio(f) => audio_up.push(f),
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(states, vec![S::Listening, S::Captured, S::Thinking]);
    assert_eq!(audio_up.len(), 1);
    assert_eq!(audio_up[0].stream, Stream::Up);
    assert_eq!(audio_up[0].payload.len(), 640);
    assert_eq!(led_status, Some(LedAvailability::Unavailable));

    // ---- phone replies: audio down, then volume and done_ok ----
    rig.now = 600;
    rig.phone
        .send_audio(&AudioFrame {
            stream: Stream::Down,
            codec: Codec::Pcm16,
            flags: 0,
            utt: 2,
            seq: 0,
            ts_ms: 0,
            payload: vec![7; 640],
        })
        .unwrap();
    let hs = rig.deliver_to_glasses();
    assert_eq!(hs.len(), 1);
    assert_eq!(hs[0].audio.len(), 1);
    assert!(hs[0].actions.contains(&IndicatorAction::StopThinkingLoop));

    rig.now = 700;
    rig.phone
        .send_envelope(&Envelope::Cmd {
            id: 11,
            seq: 1,
            ttl_ms: 5000,
            cmd: Command::VolumeSet { level: 0.6 },
        })
        .unwrap();
    rig.phone
        .send_envelope(&Envelope::Cmd {
            id: 12,
            seq: 2,
            ttl_ms: 5000,
            cmd: Command::IndicatorSet {
                state: S::DoneOk,
                turn_id: 4,
                seq: 1,
            },
        })
        .unwrap();
    let hs = rig.deliver_to_glasses();
    assert!(
        hs.iter()
            .any(|h| h.commands == vec![ShellCommand::VolumeSet { level: 0.6 }])
    );
    assert_eq!(rig.node.indicator_state(), S::DoneOk);

    let (mut saw_done, mut acks) = (false, vec![]);
    while let Some(m) = rig.phone.recv().unwrap() {
        match m {
            Inbound::Envelope(Envelope::Evt {
                event:
                    Event::IndicatorState {
                        state: S::DoneOk,
                        turn_id: 4,
                    },
                ..
            }) => saw_done = true,
            Inbound::Envelope(Envelope::Res { id, ok: true, .. }) => acks.push(id),
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(saw_done);
    assert_eq!(acks, vec![11, 12]);

    // ---- the flash ends, LED authority is released ----
    let acts = rig.feed(700 + 1200, IndicatorInput::Tick);
    assert!(acts.contains(&IndicatorAction::ReleaseLed));
    assert_eq!(rig.node.indicator_state(), S::Idle);
}

#[test]
fn wrong_peer_cannot_pair_with_the_glasses() {
    let dir = tempfile::tempdir().unwrap();
    let phone = Identity::load_or_create(dir.path()).unwrap();
    let glasses = Identity::from_seed([6; 32]);
    let beacon_key = Identity::from_seed([5; 32]).public_key(); // not this glasses' key
    let mut rig = Rig::new(GlassesNode::new(glasses));
    let mut p = ParentPairing::new(phone, Some(beacon_key), CAP_MIC);
    let hello = rig.node.pair_hello().unwrap();
    rig.glasses_writes(&[hello]);
    drain_pair_to_phone(&mut rig, &mut p);
    rig.deliver_to_glasses();
    assert!(matches!(p.state(), PairingState::Refused { .. }));
    assert!(matches!(
        rig.node.pairing_state(),
        GlassesPairingState::Refused { .. }
    ));
}
