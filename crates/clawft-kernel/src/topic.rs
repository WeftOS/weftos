//! Pub/sub topic routing for kernel IPC.
//!
//! The [`TopicRouter`] manages topic subscriptions and delivers
//! published messages to all subscribers. Topics are arbitrary
//! strings (e.g. "build-status", "test-results", "agent.spawned").
//!
//! Subscriptions are stored in a [`DashMap`] for lock-free concurrent
//! access. Dead subscribers (processes that have exited) are lazily
//! cleaned up during publish.
//!
//! # Subscriber kinds
//!
//! Subscriptions are generalized via [`SubscriberSink`]. Two sink
//! kinds exist today:
//!
//! - [`SubscriberSink::PidInbox`] — an in-kernel agent identified by
//!   `Pid`. The A2A router delivers to its `mpsc` inbox.
//! - [`SubscriberSink::ExternalStream`] — an external Unix-socket
//!   client (Python bridge, another Claude Code session) that holds
//!   the read end of a channel the daemon writes one JSON line per
//!   published message into.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
#[cfg(feature = "native")]
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::process::{Pid, ProcessState, ProcessTable};

/// Unique identifier for an external streaming subscription.
///
/// Used so the daemon can unsubscribe a specific external client
/// when its socket disconnects, without touching other subscribers
/// on the same topic.
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SubscriberId(pub u64);

/// Global monotonic counter for assigning [`SubscriberId`]s.
static SUBSCRIBER_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

impl SubscriberId {
    /// Allocate a fresh, process-unique subscriber id.
    pub fn next() -> Self {
        SubscriberId(SUBSCRIBER_ID_COUNTER.fetch_add(1, Ordering::Relaxed))
    }
}

/// A subscription sink — where a delivered topic message goes.
///
/// See the module-level docs for a description of each variant.
///
/// [`SubscriberSink::ExternalStream`] requires the `native` feature
/// (tokio mpsc). Without `native` (e.g. wasm32-unknown-unknown /
/// `--no-default-features`), only [`SubscriberSink::PidInbox`] is
/// available — WEFT-114.
#[derive(Clone)]
pub enum SubscriberSink {
    /// An in-kernel process inbox (existing PID-based delivery).
    PidInbox(Pid),
    /// An external streaming client. The daemon serializes each
    /// published [`crate::ipc::KernelMessage`] as a JSON line (byte
    /// buffer including a trailing `\n`) and feeds it into this
    /// sender. A full channel is treated as back-pressure and the
    /// message is dropped for that client.
    ///
    /// Only available under the `native` feature (tokio runtime).
    #[cfg(feature = "native")]
    ExternalStream(mpsc::Sender<Vec<u8>>),
}

impl SubscriberSink {
    /// The in-kernel process behind this sink; `None` for an external
    /// stream. (A method, not a `filter_map` match at each caller: without
    /// the `native` feature every arm is `Some`, which clippy rejects.)
    fn pid(&self) -> Option<Pid> {
        match self {
            SubscriberSink::PidInbox(pid) => Some(*pid),
            #[cfg(feature = "native")]
            SubscriberSink::ExternalStream(_) => None,
        }
    }
}

impl std::fmt::Debug for SubscriberSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SubscriberSink::PidInbox(pid) => write!(f, "PidInbox({pid})"),
            #[cfg(feature = "native")]
            SubscriberSink::ExternalStream(_) => write!(f, "ExternalStream(..)"),
        }
    }
}

/// A topic subscription entry (wire-format; describes a PID-based
/// subscription only — external stream sinks are ephemeral and not
/// persisted in this shape).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subscription {
    /// The topic pattern this subscription matches.
    pub topic: String,

    /// The subscribing process's PID.
    pub subscriber_pid: Pid,

    /// Optional message filter (future use).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
}

/// Pub/sub topic router for kernel IPC.
///
/// Manages subscriptions as a mapping from topic name to the list of
/// subscribers. A subscriber can be an in-kernel PID inbox or an
/// external streaming client ([`SubscriberSink`]). Uses [`DashMap`] for
/// lock-free concurrent access.
///
/// # Dead subscriber cleanup
///
/// When a message is published, the router checks each PID subscriber's
/// state in the process table. PID subscribers whose processes have
/// exited are automatically removed from the subscription list (lazy
/// cleanup). External streams that have closed their receiver are
/// removed on the next publish attempt.
pub struct TopicRouter {
    /// Topic -> list of sinks subscribed to it.
    ///
    /// Stored as `(SubscriberId, SubscriberSink)` pairs so the daemon
    /// can unsubscribe a specific external client by id.
    subscriptions: DashMap<String, Vec<(SubscriberId, SubscriberSink)>>,

    /// Process table for checking subscriber state.
    process_table: Arc<ProcessTable>,
}

impl TopicRouter {
    /// Create a new topic router.
    pub fn new(process_table: Arc<ProcessTable>) -> Self {
        Self {
            subscriptions: DashMap::new(),
            process_table,
        }
    }

    /// Subscribe a sink to a topic.
    ///
    /// Returns the assigned [`SubscriberId`]. For PID inbox sinks, if
    /// the same PID is already subscribed to the topic the existing
    /// subscription is kept and its id is returned (idempotent).
    pub fn subscribe_sink(&self, topic: &str, sink: SubscriberSink) -> SubscriberId {
        // For PidInbox, preserve the previous idempotent behaviour so
        // repeated kernel-side subscribe calls do not duplicate inboxes.
        if let SubscriberSink::PidInbox(pid) = &sink
            && let Some(subs) = self.subscriptions.get(topic)
        {
            for (existing_id, existing) in subs.iter() {
                if let SubscriberSink::PidInbox(p) = existing
                    && p == pid
                {
                    debug!(pid = *pid, topic, "already subscribed");
                    return *existing_id;
                }
            }
        }

        let id = SubscriberId::next();
        debug!(?sink, topic, id = id.0, "subscribing to topic");
        self.subscriptions
            .entry(topic.to_owned())
            .or_default()
            .push((id, sink));
        id
    }

    /// Subscribe a process to a topic (convenience for PID inboxes).
    ///
    /// If the process is already subscribed, this is a no-op — the
    /// existing subscription is preserved.
    pub fn subscribe(&self, pid: Pid, topic: &str) {
        let _ = self.subscribe_sink(topic, SubscriberSink::PidInbox(pid));
    }

    /// Unsubscribe a process from a topic.
    ///
    /// If the process is not subscribed, this is a no-op.
    /// Empty subscription lists are removed.
    pub fn unsubscribe(&self, pid: Pid, topic: &str) {
        debug!(pid, topic, "unsubscribing pid from topic");
        if let Some(mut subs) = self.subscriptions.get_mut(topic) {
            subs.retain(|(_, sink)| !matches!(sink, SubscriberSink::PidInbox(p) if *p == pid));
        }

        // Clean up empty topics
        self.subscriptions.retain(|_, subs| !subs.is_empty());
    }

    /// Unsubscribe a specific subscriber (by id) from a topic.
    ///
    /// Use this to tear down external streaming subscriptions on
    /// disconnect without affecting other subscribers on the same
    /// topic.
    pub fn unsubscribe_id(&self, topic: &str, id: SubscriberId) {
        debug!(topic, id = id.0, "unsubscribing id from topic");
        if let Some(mut subs) = self.subscriptions.get_mut(topic) {
            subs.retain(|(existing, _)| *existing != id);
        }
        self.subscriptions.retain(|_, subs| !subs.is_empty());
    }

    /// Get the list of live sinks for a topic.
    ///
    /// Performs lazy cleanup: removes PID sinks whose processes have
    /// exited and [`SubscriberSink::ExternalStream`] sinks whose
    /// receiver has been dropped. Returns a cloned list safe to use
    /// outside the lock.
    pub fn live_sinks(&self, topic: &str) -> Vec<(SubscriberId, SubscriberSink)> {
        let mut live = Vec::new();
        let mut dead_ids: Vec<SubscriberId> = Vec::new();

        if let Some(subs) = self.subscriptions.get(topic) {
            for (id, sink) in subs.iter() {
                match sink {
                    SubscriberSink::PidInbox(pid) => {
                        if self.is_alive(*pid) {
                            live.push((*id, sink.clone()));
                        } else {
                            dead_ids.push(*id);
                        }
                    }
                    #[cfg(feature = "native")]
                    SubscriberSink::ExternalStream(tx) => {
                        if tx.is_closed() {
                            dead_ids.push(*id);
                        } else {
                            live.push((*id, sink.clone()));
                        }
                    }
                }
            }
        }

        if !dead_ids.is_empty() {
            if let Some(mut subs) = self.subscriptions.get_mut(topic) {
                subs.retain(|(id, _)| !dead_ids.contains(id));
            }
            warn!(
                topic,
                dead_count = dead_ids.len(),
                "cleaned up dead subscribers"
            );
        }

        live
    }

    /// Get the list of running PID subscribers for a topic.
    ///
    /// Convenience wrapper around [`Self::live_sinks`] that returns
    /// only in-kernel agent PIDs. External streaming subscribers are
    /// handled separately by the caller via [`Self::live_sinks`].
    pub fn live_subscribers(&self, topic: &str) -> Vec<Pid> {
        self.live_sinks(topic)
            .into_iter()
            .filter_map(|(_, sink)| sink.pid())
            .collect()
    }

    /// Get all PID subscribers for a topic (including potentially dead ones).
    ///
    /// Use [`TopicRouter::live_subscribers`] for a filtered list.
    pub fn subscribers(&self, topic: &str) -> Vec<Pid> {
        self.subscriptions
            .get(topic)
            .map(|subs| {
                subs.iter()
                    .filter_map(|(_, sink)| sink.pid())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// List all topics with their subscriber counts.
    pub fn list_topics(&self) -> Vec<(String, usize)> {
        self.subscriptions
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().len()))
            .collect()
    }

    /// List all topics a specific PID is subscribed to.
    pub fn topics_for_pid(&self, pid: Pid) -> Vec<String> {
        self.subscriptions
            .iter()
            .filter(|entry| {
                entry
                    .value()
                    .iter()
                    .any(|(_, sink)| matches!(sink, SubscriberSink::PidInbox(p) if *p == pid))
            })
            .map(|entry| entry.key().clone())
            .collect()
    }

    /// Get the total number of active topics.
    pub fn topic_count(&self) -> usize {
        self.subscriptions.len()
    }

    /// Check whether a topic has any subscribers.
    pub fn has_subscribers(&self, topic: &str) -> bool {
        self.subscriptions
            .get(topic)
            .is_some_and(|subs| !subs.is_empty())
    }

    /// Remove all subscriptions for a PID (used during process cleanup).
    pub fn unsubscribe_all(&self, pid: Pid) {
        debug!(pid, "unsubscribing from all topics");
        for mut entry in self.subscriptions.iter_mut() {
            entry
                .value_mut()
                .retain(|(_, sink)| !matches!(sink, SubscriberSink::PidInbox(p) if *p == pid));
        }
        // Clean up empty topics
        self.subscriptions.retain(|_, subs| !subs.is_empty());
    }

    /// Check whether a PID corresponds to an alive process.
    fn is_alive(&self, pid: Pid) -> bool {
        self.process_table
            .get(pid)
            .is_some_and(|entry| !matches!(entry.state, ProcessState::Exited(_)))
    }
}

#[cfg(all(test, feature = "native"))]
mod tests {
    use super::*;
    use crate::capability::AgentCapabilities;
    use crate::process::{ProcessEntry, ResourceUsage};
    use tokio_util::sync::CancellationToken;

    fn make_router_with_processes(count: usize) -> (TopicRouter, Vec<Pid>) {
        let table = Arc::new(ProcessTable::new(64));
        let mut pids = Vec::new();
        for i in 0..count {
            let entry = ProcessEntry {
                pid: 0,
                agent_id: format!("agent-{i}"),
                state: ProcessState::Running,
                capabilities: AgentCapabilities::default(),
                resource_usage: ResourceUsage::default(),
                cancel_token: CancellationToken::new(),
                parent_pid: None,
            };
            let pid = table.insert(entry).unwrap();
            pids.push(pid);
        }
        (TopicRouter::new(table), pids)
    }

    #[test]
    fn subscribe_and_list() {
        let (router, pids) = make_router_with_processes(2);
        router.subscribe(pids[0], "build");
        router.subscribe(pids[1], "build");

        let subs = router.subscribers("build");
        assert_eq!(subs.len(), 2);
        assert!(subs.contains(&pids[0]));
        assert!(subs.contains(&pids[1]));
    }

    #[test]
    fn subscribe_idempotent() {
        let (router, pids) = make_router_with_processes(1);
        router.subscribe(pids[0], "build");
        router.subscribe(pids[0], "build");

        let subs = router.subscribers("build");
        assert_eq!(subs.len(), 1);
    }

    #[test]
    fn unsubscribe() {
        let (router, pids) = make_router_with_processes(2);
        router.subscribe(pids[0], "build");
        router.subscribe(pids[1], "build");

        router.unsubscribe(pids[0], "build");

        let subs = router.subscribers("build");
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0], pids[1]);
    }

    #[test]
    fn unsubscribe_nonexistent_is_noop() {
        let (router, _pids) = make_router_with_processes(0);
        router.unsubscribe(999, "build"); // Should not panic
        assert!(router.subscribers("build").is_empty());
    }

    #[test]
    fn unsubscribe_removes_empty_topic() {
        let (router, pids) = make_router_with_processes(1);
        router.subscribe(pids[0], "build");
        router.unsubscribe(pids[0], "build");

        assert_eq!(router.topic_count(), 0);
    }

    #[test]
    fn list_topics() {
        let (router, pids) = make_router_with_processes(2);
        router.subscribe(pids[0], "build");
        router.subscribe(pids[0], "test");
        router.subscribe(pids[1], "build");

        let topics = router.list_topics();
        assert_eq!(topics.len(), 2);

        let build_count = topics
            .iter()
            .find(|(t, _)| t == "build")
            .map(|(_, c)| *c)
            .unwrap();
        assert_eq!(build_count, 2);
    }

    #[test]
    fn topics_for_pid() {
        let (router, pids) = make_router_with_processes(1);
        router.subscribe(pids[0], "build");
        router.subscribe(pids[0], "test");
        router.subscribe(pids[0], "deploy");

        let topics = router.topics_for_pid(pids[0]);
        assert_eq!(topics.len(), 3);
    }

    #[test]
    fn has_subscribers() {
        let (router, pids) = make_router_with_processes(1);
        assert!(!router.has_subscribers("build"));

        router.subscribe(pids[0], "build");
        assert!(router.has_subscribers("build"));
    }

    #[test]
    fn live_subscribers_filters_dead() {
        let table = Arc::new(ProcessTable::new(64));

        // Create a running process
        let entry1 = ProcessEntry {
            pid: 0,
            agent_id: "alive".to_owned(),
            state: ProcessState::Running,
            capabilities: AgentCapabilities::default(),
            resource_usage: ResourceUsage::default(),
            cancel_token: CancellationToken::new(),
            parent_pid: None,
        };
        let pid1 = table.insert(entry1).unwrap();

        // Create a dead process
        let entry2 = ProcessEntry {
            pid: 0,
            agent_id: "dead".to_owned(),
            state: ProcessState::Running,
            capabilities: AgentCapabilities::default(),
            resource_usage: ResourceUsage::default(),
            cancel_token: CancellationToken::new(),
            parent_pid: None,
        };
        let pid2 = table.insert(entry2).unwrap();
        table.update_state(pid2, ProcessState::Exited(0)).unwrap();

        let router = TopicRouter::new(table);
        router.subscribe(pid1, "build");
        router.subscribe(pid2, "build");

        // All subscribers includes dead
        assert_eq!(router.subscribers("build").len(), 2);

        // Live subscribers excludes dead and cleans up
        let live = router.live_subscribers("build");
        assert_eq!(live.len(), 1);
        assert_eq!(live[0], pid1);

        // After cleanup, subscribers list is also cleaned
        assert_eq!(router.subscribers("build").len(), 1);
    }

    #[test]
    fn unsubscribe_all() {
        let (router, pids) = make_router_with_processes(2);
        router.subscribe(pids[0], "build");
        router.subscribe(pids[0], "test");
        router.subscribe(pids[1], "build");

        router.unsubscribe_all(pids[0]);

        assert!(router.topics_for_pid(pids[0]).is_empty());
        assert_eq!(router.subscribers("build").len(), 1);
        assert_eq!(router.topic_count(), 1); // "test" removed (empty)
    }

    #[test]
    fn subscription_serde_roundtrip() {
        let sub = Subscription {
            topic: "build".into(),
            subscriber_pid: 42,
            filter: Some("status:*".into()),
        };
        let json = serde_json::to_string(&sub).unwrap();
        let restored: Subscription = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.topic, "build");
        assert_eq!(restored.subscriber_pid, 42);
        assert_eq!(restored.filter, Some("status:*".into()));
    }

    #[test]
    fn external_stream_sink_lives_alongside_pid_sinks() {
        let (router, pids) = make_router_with_processes(1);
        router.subscribe(pids[0], "events");

        let (tx, _rx) = mpsc::channel::<Vec<u8>>(4);
        let ext_id = router.subscribe_sink("events", SubscriberSink::ExternalStream(tx));

        let sinks = router.live_sinks("events");
        assert_eq!(sinks.len(), 2);
        assert!(sinks.iter().any(|(id, _)| *id == ext_id));
        // PID subscribers view excludes the external stream
        assert_eq!(router.subscribers("events"), vec![pids[0]]);
    }

    #[test]
    fn unsubscribe_id_removes_only_target() {
        let (router, pids) = make_router_with_processes(1);
        router.subscribe(pids[0], "events");
        let (tx, _rx) = mpsc::channel::<Vec<u8>>(4);
        let ext_id = router.subscribe_sink("events", SubscriberSink::ExternalStream(tx));

        router.unsubscribe_id("events", ext_id);

        let sinks = router.live_sinks("events");
        assert_eq!(sinks.len(), 1);
        assert!(matches!(sinks[0].1, SubscriberSink::PidInbox(_)));
    }

    #[test]
    fn closed_external_stream_is_cleaned_on_live_query() {
        let (router, _) = make_router_with_processes(0);
        let (tx, rx) = mpsc::channel::<Vec<u8>>(4);
        router.subscribe_sink("events", SubscriberSink::ExternalStream(tx));
        drop(rx); // close the receiver

        let sinks = router.live_sinks("events");
        assert!(sinks.is_empty(), "closed external streams must be pruned");
    }

    #[test]
    fn subscriber_id_is_unique_and_monotonic() {
        let a = SubscriberId::next();
        let b = SubscriberId::next();
        assert_ne!(a, b);
        assert!(b.0 > a.0);
    }
}
