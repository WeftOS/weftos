//! Shared rig for the stream tests: a broker on a temp socket and helpers
//! to register cogs with stream outputs and inputs.
#![allow(dead_code)]

use std::sync::Arc;

use clawft_types::cog_topics::{InputPolicy, TopicClass};
use weftos_cogbus::server::{ServerHandle, serve};
use weftos_cogbus::{
    Broker, BrokerConfig, ClientError, ErrorCode, InputSpec, InstanceSpec, OutputSpec, StreamConfig,
};

pub const PROJECT: &str = "01J9ZXW0PRJCTAAAAAAAAAAAAA";
pub const PCM: &str = "pcm.s16le.v1";
pub const KIND: &str = "audio.pcm";
pub const SOURCE: &str = "venq-mic-0";
pub const TOPIC: &str = "audio.pcm/venq-mic-0";

pub fn token(c: char) -> String {
    std::iter::repeat_n(c, 64).collect()
}

pub fn pcm_out() -> OutputSpec {
    OutputSpec {
        topic: KIND.into(),
        schema: PCM.into(),
        class: TopicClass::Stream,
    }
}

pub fn pcm_in(policy: InputPolicy) -> InputSpec {
    InputSpec {
        topic: KIND.into(),
        schema: PCM.into(),
        policy,
    }
}

pub fn refused<T: std::fmt::Debug>(r: Result<T, ClientError>) -> ErrorCode {
    match r {
        Err(ClientError::Refused { code, .. }) => code,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

pub struct Rig {
    pub broker: Arc<Broker>,
    pub server: ServerHandle,
    pub tmp: tempfile::TempDir,
}

impl Rig {
    pub async fn new(cfg: StreamConfig) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let broker =
            Arc::new(Broker::new(BrokerConfig::new("0".repeat(32))).with_stream_config(cfg));
        let server = serve(broker.clone(), &tmp.path().join("cogbus.sock"))
            .await
            .unwrap();
        Self {
            broker,
            server,
            tmp,
        }
    }

    /// Register `id` with token `c` x 64.
    pub fn cog(&self, id: &str, c: char, outputs: Vec<OutputSpec>, inputs: Vec<InputSpec>) {
        let spec = InstanceSpec {
            instance_id: id.into(),
            cog_id: id.into(),
            project_id: Some(PROJECT.into()),
            uid: None,
            outputs,
            inputs,
        };
        self.broker.register(spec, &token(c)).unwrap();
    }

    pub async fn connect(&self, c: char) -> weftos_cogbus::Client {
        weftos_cogbus::Client::connect(self.server.path(), &token(c))
            .await
            .unwrap()
    }
}

/// Wait up to 5 s for the reader's next record.
pub async fn next(r: &mut weftos_cogbus::StreamReader) -> weftos_cogbus::StreamRecord {
    tokio::time::timeout(std::time::Duration::from_secs(5), r.next())
        .await
        .expect("no record within 5 s")
        .expect("stream closed")
}
