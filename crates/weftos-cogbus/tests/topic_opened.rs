//! A topic coming alive is announced once to the sink (ADR-122 section 14,
//! B6a), so the daemon can list it as an exposure.
#![cfg(unix)]

mod common;

use std::sync::{Arc, Mutex};

use clawft_types::cog_topics::{InputPolicy, TopicClass};
use common::*;
use serde_json::json;
use weftos_cogbus::stream::StreamOptions;
use weftos_cogbus::{BusActivity, BusHooks, BusSink, OutputSpec, StreamConfig, TopicInfo};

#[derive(Default)]
struct Opened(Mutex<Vec<TopicInfo>>);

impl BusSink for Opened {
    fn record(&self, _: &BusActivity) {}
    fn topic_opened(&self, t: &TopicInfo) {
        self.0.lock().unwrap().push(t.clone());
    }
}

fn telemetry() -> OutputSpec {
    OutputSpec { topic: "telemetry".into(), schema: "telemetry.v1".into(), class: TopicClass::Reading }
}

#[tokio::test]
async fn the_first_reading_and_each_stream_open_announce_the_topic_once() {
    let seen = Arc::new(Opened::default());
    let hooks = BusHooks { sink: seen.clone(), ..BusHooks::default() };
    let rig = Rig::with_hooks(StreamConfig::default(), hooks).await;
    rig.cog("board", 'a', vec![telemetry(), pcm_out()], vec![]);
    rig.cog("viewer", 'b', vec![], vec![pcm_in(InputPolicy::DropOldest)]);
    let a = rig.connect('a').await;

    for i in 1..=3u64 {
        a.publish("telemetry", "venq-board", "telemetry.v1", i, json!({"items": []})).await.unwrap();
    }
    {
        let got = seen.0.lock().unwrap();
        assert_eq!(got.len(), 1, "announced once, not per reading: {got:?}");
        let t = &got[0];
        assert_eq!((t.topic.as_str(), t.kind.as_str(), t.source_id.as_str()), ("telemetry/venq-board", "telemetry", "venq-board"));
        assert_eq!((t.schema.as_str(), t.class, t.instance.as_str(), t.cog_id.as_str()), ("telemetry.v1", TopicClass::Reading, "board", "board"));
        assert_eq!(t.project_id.as_deref(), Some(PROJECT));
    }
    // Another source of the same kind is another topic.
    a.publish("telemetry", "venq-board-2", "telemetry.v1", 1, json!({"items": []})).await.unwrap();
    assert_eq!(seen.0.lock().unwrap().len(), 2);

    let _w = a.stream_writer(KIND, SOURCE, PCM, StreamOptions::new(640)).await.unwrap();
    let got = seen.0.lock().unwrap().clone();
    let s = got.last().unwrap();
    assert_eq!((s.topic.as_str(), s.class), (TOPIC, TopicClass::Stream));
}

#[tokio::test]
async fn a_refused_publish_announces_nothing() {
    let seen = Arc::new(Opened::default());
    let hooks = BusHooks { sink: seen.clone(), ..BusHooks::default() };
    let rig = Rig::with_hooks(StreamConfig::default(), hooks).await;
    rig.cog("board", 'a', vec![telemetry()], vec![]);
    let a = rig.connect('a').await;
    assert!(a.publish("nope", "x", "telemetry.v1", 1, json!({})).await.is_err());
    assert!(a.publish("telemetry", "x", "senml.v1", 1, json!({})).await.is_err());
    assert!(seen.0.lock().unwrap().is_empty());
}
