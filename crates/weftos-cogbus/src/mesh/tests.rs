//! Mesh topics between brokers in one process (ADR-122 section 14): a
//! frame queue stands in for the peer connections, a map for the exposure
//! registry.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use clawft_types::cog_topics::{InputPolicy, MeshLimits, TopicClass};
use serde_json::{Value, json};

use super::wire::{self, TopicData, TopicReq};
use super::{ExposureView, FrameKind, MeshAudience, MeshConfig, MeshLink, MeshRecord, MeshTopicHub, PeerInfo, TopicExposures};
use crate::broker::{Broker, BrokerConfig, InstanceSpec, Session};
use crate::policy::{BusActivity, BusHooks, BusPolicy, BusRequest, BusSink, Grant};
use crate::proto::{InputSpec, OutputSpec};

#[path = "tests_fetch.rs"]
mod tests_fetch;
#[path = "tests_remote.rs"]
mod tests_remote;
#[path = "tests_source.rs"]
mod tests_source;
#[path = "tests_stream.rs"]
mod tests_stream;

pub(super) const P1: &str = "01J9ZXW0PRJCTAAAAAAAAAAAAA";
pub(super) const TOPIC: &str = "telemetry/venq-board";
pub(super) const NAME: &str = "weftos://home/sensors/venq-board/topics/telemetry";

/// One frame in flight: (from, to, kind, payload).
pub(super) type Frame = (String, String, FrameKind, Vec<u8>);

/// Frames in flight.
#[derive(Default)]
pub(super) struct Wire {
    pub frames: Mutex<Vec<Frame>>,
    /// Links that refuse to send.
    pub down: Mutex<Vec<String>>,
}

pub(super) struct Link {
    pub me: String,
    pub wire: Arc<Wire>,
}

impl MeshLink for Link {
    fn send(&self, node: &str, kind: FrameKind, payload: Vec<u8>) -> bool {
        if self.wire.down.lock().unwrap().iter().any(|d| d == node || d == &self.me) {
            return false;
        }
        self.wire.frames.lock().unwrap().push((self.me.clone(), node.into(), kind, payload));
        true
    }
}

/// Exposures by id, each with the peers it covers; every record kept.
#[derive(Default)]
pub(super) struct Expo {
    pub on: Mutex<HashMap<String, (ExposureView, Vec<String>)>>,
    pub records: Mutex<Vec<MeshRecord>>,
}

impl Expo {
    pub fn expose(&self, topic: &str, audience: MeshAudience, raw: bool, peers: &[&str]) {
        let v = ExposureView { audience, raw, rate_per_min: None, bytes_per_sec: None };
        self.on.lock().unwrap().insert(super::exposure_id(topic), (v, peers.iter().map(|p| p.to_string()).collect()));
    }

    pub fn off(&self, topic: &str) {
        self.on.lock().unwrap().remove(&super::exposure_id(topic));
    }

    pub fn outcomes(&self) -> Vec<(String, String)> {
        self.records.lock().unwrap().iter().map(|r| (r.caller_node.clone(), r.outcome.clone())).collect()
    }
}

impl TopicExposures for Expo {
    fn serve(&self, id: &str, peer: &str) -> Result<ExposureView, String> {
        match self.on.lock().unwrap().get(id) {
            Some((v, peers)) if peers.iter().any(|p| p == peer) => Ok(v.clone()),
            Some(_) => Err("this node is not in the audience".into()),
            None => Err("audience is local".into()),
        }
    }
    fn record(&self, r: MeshRecord) {
        self.records.lock().unwrap().push(r);
    }
}

/// Denies `cogbus.mesh.subscribe` for principals in `deny`.
#[derive(Default)]
pub(super) struct Gov {
    pub deny: Mutex<Vec<String>>,
    pub grants: Mutex<Vec<Grant>>,
}

impl BusPolicy for Gov {
    fn decide(&self, r: &BusRequest<'_>) -> Result<(), String> {
        if self.deny.lock().unwrap().iter().any(|d| d == r.instance_id) {
            return Err("denied by test policy".into());
        }
        Ok(())
    }
    fn grants(&self, _: &BusRequest<'_>) -> Vec<Grant> {
        self.grants.lock().unwrap().clone()
    }
}

#[derive(Default)]
pub(super) struct Acts(pub Mutex<Vec<BusActivity>>);

impl BusSink for Acts {
    fn record(&self, a: &BusActivity) {
        self.0.lock().unwrap().push(a.clone());
    }
}

/// One node: broker, hub, exposures, governance, activity.
pub(super) struct Node {
    pub id: String,
    pub broker: Arc<Broker>,
    pub hub: Arc<MeshTopicHub>,
    pub expo: Arc<Expo>,
    pub gov: Arc<Gov>,
    pub acts: Arc<Acts>,
    pub _blobs: tempfile::TempDir,
}

impl Node {
    pub fn new(id: &str, wire: &Arc<Wire>) -> Self {
        let (gov, acts) = (Arc::new(Gov::default()), Arc::new(Acts::default()));
        let hooks = BusHooks { policy: gov.clone(), sink: acts.clone(), ..BusHooks::default() };
        let blobs = tempfile::tempdir().unwrap();
        let store = crate::blob::BlobStore::open(blobs.path(), crate::blob::BlobConfig::default(), crate::audit::now_ms()).unwrap();
        let broker = Arc::new(Broker::new(BrokerConfig::new(id)).with_hooks(hooks).with_blobs(Arc::new(store)));
        let expo = Arc::new(Expo::default());
        let cfg = MeshConfig { max_backoff: std::time::Duration::from_millis(5), ..MeshConfig::default() };
        let link = Arc::new(Link { me: id.into(), wire: wire.clone() });
        let hub = MeshTopicHub::new(broker.clone(), link, expo.clone(), cfg);
        Self { id: id.into(), broker, hub, expo, gov, acts, _blobs: blobs }
    }

    /// Register `instance` with token `c`x64 and open its session.
    pub fn cog(&self, instance: &str, c: char, project: Option<&str>, outputs: Vec<OutputSpec>, inputs: Vec<InputSpec>) -> Session {
        let spec = InstanceSpec {
            instance_id: instance.into(),
            cog_id: format!("{instance}-cog"),
            project_id: project.map(String::from),
            uid: None,
            outputs,
            inputs,
        };
        let token: String = std::iter::repeat_n(c, 64).collect();
        self.broker.register(spec, &token).unwrap();
        self.broker.authenticate(&token, None).unwrap()
    }

    /// The board-telemetry writer, limits declared, its first reading published.
    pub fn board(&self, limits: Option<MeshLimits>) -> Session {
        let s = self.cog("board", 'b', Some(P1), vec![telemetry_out()], vec![]);
        if let Some(l) = limits {
            self.broker.set_mesh_limits("board", l);
        }
        self.publish(&s, 0);
        s
    }

    pub fn publish(&self, s: &Session, n: u64) -> u64 {
        self.broker.publish(s, "telemetry", "venq-board", "telemetry.v1", n, json!({"n": n})).unwrap()
    }

    pub fn peer(&self) -> PeerInfo {
        PeerInfo { node_id: self.id.clone(), admitted: true }
    }
}

pub(super) fn telemetry_out() -> OutputSpec {
    OutputSpec { topic: "telemetry".into(), schema: "telemetry.v1".into(), class: TopicClass::Reading }
}

pub(super) fn telemetry_in(policy: InputPolicy) -> InputSpec {
    InputSpec { topic: "telemetry".into(), schema: "telemetry.v1".into(), policy }
}

/// Deliver every queued frame to its node (and the frames those cause),
/// pumping both hubs in between. Returns the data frames delivered.
pub(super) fn flush(wire: &Arc<Wire>, nodes: &[&Node]) -> Vec<(String, TopicData)> {
    let mut seen = Vec::new();
    for _ in 0..32 {
        for n in nodes {
            n.hub.pump();
        }
        let frames: Vec<_> = std::mem::take(&mut *wire.frames.lock().unwrap());
        if frames.is_empty() {
            break;
        }
        for (from, to, kind, payload) in frames {
            if kind == FrameKind::Data
                && let Ok((d, _)) = wire::decode::<TopicData>(&payload)
            {
                seen.push((to.clone(), d));
            }
            if let Some(n) = nodes.iter().find(|n| n.id == to) {
                n.hub.on_frame(&PeerInfo { node_id: from, admitted: true }, kind, &payload);
            }
        }
    }
    seen
}

/// A raw request from `from` to `to` (a peer with no broker of its own).
pub(super) fn send_req(to: &Node, from: &str, admitted: bool, r: &TopicReq) {
    let peer = PeerInfo { node_id: from.into(), admitted };
    to.hub.on_frame(&peer, FrameKind::Request, &wire::encode(r, &[]));
}

/// Frames queued for `to`, decoded, removed from the wire.
pub(super) fn take_for(wire: &Arc<Wire>, to: &str) -> Vec<TopicData> {
    let mut g = wire.frames.lock().unwrap();
    let (mine, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut *g).into_iter().partition(|f| f.1 == to);
    *g = rest;
    mine.into_iter().filter_map(|f| wire::decode::<TopicData>(&f.3).ok().map(|(d, _)| d)).collect()
}

pub(super) fn readings(d: &[TopicData]) -> Vec<(u64, Value)> {
    d.iter()
        .filter_map(|x| match x {
            TopicData::Reading { seq, body, .. } => Some((*seq, body.clone())),
            _ => None,
        })
        .collect()
}

pub(super) fn subscribe(sub: u64, n: u64) -> TopicReq {
    TopicReq::Subscribe {
        sub,
        topic: TOPIC.into(),
        schema: "telemetry.v1".into(),
        policy: "drop-oldest".into(),
        raw: false,
        credit: super::Credit { n, bytes: 0 },
    }
}

#[test]
fn frames_round_trip_with_binary_tails() {
    let r = subscribe(7, 3);
    let p = wire::encode(&r, &[]);
    assert_eq!(wire::decode::<TopicReq>(&p).unwrap().0, r);
    let d = TopicData::Chunk { sub: 1, seq: 9, t_ns: 5 };
    let p = wire::encode(&d, &[0, 255, 1]);
    let (back, tail) = wire::decode::<TopicData>(&p).unwrap();
    assert_eq!((back, tail), (d, &[0u8, 255, 1][..]));
    assert!(wire::decode::<TopicReq>(&[2, 0, 0, 0, 0]).is_err(), "unknown version");
    assert!(wire::decode::<TopicReq>(&[1, 0, 0, 0, 9, b'{']).is_err(), "short head");
    let unknown = wire::encode(&json!({"op": "subscribe", "sub": 1, "evil": true}), &[]);
    assert!(wire::decode::<TopicReq>(&unknown).is_err(), "unknown fields are refused");
    assert_eq!(FrameKind::from_byte(0x12), Some(FrameKind::Request));
    assert_eq!(FrameKind::Data.byte(), 0x13);
}

#[test]
fn weftos_names_parse_and_reject() {
    use crate::broker::mesh_ops::parse_mesh_topic;
    assert_eq!(parse_mesh_topic(NAME), Some(("home", "telemetry", "venq-board")));
    assert!(parse_mesh_topic("weftos://home/services/npu/topics/svc.infer").is_some());
    for bad in ["weftos://home/sensors/venq-board/topics", "weftos:///sensors/a/topics/t", "weftos://h/files/a/topics/t", "weftos://h/sensors/A/topics/t", "weftos://h/sensors/a/topics/t/x"] {
        assert!(parse_mesh_topic(bad).is_none(), "{bad}");
    }
}
