//! Pull-style runners for the instance registry: a runner that already tracks
//! its own tasks implements [`InstanceSource`], and [`InstanceRegistry::sync`]
//! registers what appeared, follows state changes and ends what vanished.
//!
//! Today's source is the kernel subagent spawn registry (`agent_spawn` children
//! of an `agent.chat` conversation).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::instance_registry::{InstanceRegistry, InstanceState, NewInstance};

/// One thing a source currently knows about.
#[derive(Debug, Clone)]
pub struct SourceInstance {
    /// Stable id inside the source (a task id).
    pub key: String,
    /// What to register when the key is first seen.
    pub new: NewInstance,
    pub state: InstanceState,
    /// Why it failed (`state == Failed`), or how it ended (`Exited`: `done`, `stopped`).
    pub detail: Option<String>,
}

/// A runner the registry polls.
#[async_trait]
pub trait InstanceSource: Send + Sync {
    /// Runner name stamped on its records.
    fn runner(&self) -> &'static str;
    /// Every instance the runner still tracks, live and finished.
    async fn snapshot(&self) -> Vec<SourceInstance>;
}

impl InstanceRegistry {
    /// Poll `source` for `source.runner()`.
    pub fn add_source(&self, source: Arc<dyn InstanceSource>) {
        self.sources.lock().unwrap_or_else(|e| e.into_inner()).push(source);
    }

    /// Poll every source once.
    pub async fn sync(&self) {
        let sources: Vec<_> = self.sources.lock().unwrap_or_else(|e| e.into_inner()).clone();
        for s in sources {
            self.sync_one(s.runner(), s.snapshot().await);
        }
    }

    fn sync_one(&self, runner: &str, snap: Vec<SourceInstance>) {
        let mut seen = std::collections::HashSet::new();
        for item in snap {
            seen.insert(item.key.clone());
            let k = (runner.to_owned(), item.key.clone());
            let known = self.lock().keys.get(&k).cloned();
            let id = match known {
                Some(id) => id,
                None => {
                    let mut new = item.new.clone();
                    new.runner = runner.to_owned();
                    let id = self.start(new).unwrap_or_default();
                    // A refused start is remembered (empty id) so it is not retried every poll.
                    self.lock().keys.insert(k, id.clone());
                    id
                }
            };
            if id.is_empty() {
                continue;
            }
            let detail = item.detail.as_deref().unwrap_or("");
            let _ = match item.state {
                InstanceState::Exited => self.stop(&id, if detail.is_empty() { "done" } else { detail }),
                InstanceState::Failed => self.fail(&id, if detail.is_empty() { "failed" } else { detail }),
                s => self.set_state(&id, s).map(|_| ()),
            };
        }
        let gone: Vec<String> = {
            let mut g = self.lock();
            let keys: Vec<_> = g.keys.keys().filter(|(r, k)| r == runner && !seen.contains(k)).cloned().collect();
            keys.into_iter().filter_map(|k| g.keys.remove(&k)).collect()
        };
        for id in gone.into_iter().filter(|i| !i.is_empty()) {
            let _ = self.stop(&id, "vanished");
        }
    }

    /// Sync every `every` until the task is dropped.
    pub fn spawn_sync_loop(self: &Arc<Self>, every: Duration) -> tokio::task::JoinHandle<()> {
        let me = self.clone();
        tokio::spawn(async move {
            loop {
                me.sync().await;
                tokio::time::sleep(every).await;
            }
        })
    }
}

/// `agent_spawn` children tracked by the daemon's subagent spawn registry.
pub struct SubagentSource {
    pub registry: Arc<clawft_service_agent::SpawnRegistry>,
}

/// Runner name of [`SubagentSource`].
pub const SUBAGENT_RUNNER: &str = "daemon-subagent";

/// Map a spawn task's status to the instance state and detail.
pub fn subagent_state(status: clawft_core::agent::spawn::TaskStatus, error: Option<&str>) -> (InstanceState, Option<String>) {
    use clawft_core::agent::spawn::TaskStatus as T;
    match status {
        T::Running => (InstanceState::Working, None),
        T::Completed => (InstanceState::Exited, Some("done".into())),
        T::Exhausted => (InstanceState::Exited, Some("recycled".into())),
        T::Cancelled => (InstanceState::Exited, Some("stopped".into())),
        T::Failed => (InstanceState::Failed, Some(error.unwrap_or("failed").to_owned())),
        T::Denied => (InstanceState::Failed, Some(error.unwrap_or("denied").to_owned())),
    }
}

#[async_trait]
impl InstanceSource for SubagentSource {
    fn runner(&self) -> &'static str {
        SUBAGENT_RUNNER
    }

    async fn snapshot(&self) -> Vec<SourceInstance> {
        self.registry
            .all_tasks()
            .into_iter()
            .map(|o| {
                let (state, detail) = subagent_state(o.status, o.error.as_deref());
                SourceInstance { key: o.task_id, new: NewInstance::default(), state, detail }
            })
            .collect()
    }
}
