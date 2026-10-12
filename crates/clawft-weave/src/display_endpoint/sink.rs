//! Where host -> leaf frames go: the daemon's A2A router, which forwards
//! to the leaf over the certified-leaf channel when the leaf has
//! subscribed to its push topic.

use std::sync::Arc;

use async_trait::async_trait;
use clawft_kernel::a2a::A2ARouter;
use clawft_kernel::{KernelMessage, MessagePayload, MessageTarget};
use weftos_leaf_types::push_topic;

#[async_trait]
pub trait FrameSink: Send + Sync {
    /// Publish one carrier JSON text to the leaf's push topic. An error
    /// means the leaf is known to be unreachable, never a silent drop.
    async fn push(&self, leaf_id: &str, carrier: String) -> Result<(), String>;
}

pub struct RouterSink {
    router: Arc<A2ARouter>,
}

impl RouterSink {
    pub fn new(router: Arc<A2ARouter>) -> Self {
        Self { router }
    }
}

#[async_trait]
impl FrameSink for RouterSink {
    async fn push(&self, leaf_id: &str, carrier: String) -> Result<(), String> {
        let topic = push_topic(leaf_id);
        #[cfg(feature = "mesh")]
        if let Some(rt) = self.router.mesh_runtime()
            && rt.peers_for_topic(&topic).is_empty()
        {
            return Err(format!("leaf {leaf_id} is not connected (no route for {topic})"));
        }
        let payload = serde_json::from_str(&carrier)
            .map(MessagePayload::Json)
            .map_err(|e| format!("carrier is not JSON: {e}"))?;
        let msg = KernelMessage::new(0, MessageTarget::Topic(topic), payload);
        self.router.send(msg).await.map_err(|e| format!("publish failed: {e}"))
    }
}
