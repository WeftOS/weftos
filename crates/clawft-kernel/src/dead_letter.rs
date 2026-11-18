//! Dead letter queue for undeliverable messages.
//!
//! When a [`KernelMessage`] cannot be delivered (target not found, inbox full,
//! timeout, governance denied, agent exited), it is routed to the
//! [`DeadLetterQueue`] instead of being silently dropped.

use std::collections::VecDeque;
use std::sync::{Arc, RwLock};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ipc::KernelMessage;
use crate::process::Pid;

/// Default maximum number of dead letters retained.
pub const DEFAULT_DLQ_CAPACITY: usize = 10_000;

/// Reason a message was dead-lettered.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DeadLetterReason {
    /// Target PID not found in ProcessTable.
    TargetNotFound {
        /// The PID that was not found.
        pid: Pid,
    },
    /// Target inbox channel is full.
    InboxFull {
        /// The PID whose inbox was full.
        pid: Pid,
    },
    /// Delivery timed out.
    Timeout {
        /// How long we waited before timing out (milliseconds).
        duration_ms: u64,
    },
    /// GovernanceGate denied delivery.
    GovernanceDenied {
        /// Reason the gate denied the message.
        reason: String,
    },
    /// Target agent exited before delivery.
    AgentExited {
        /// The PID of the exited agent.
        pid: Pid,
    },
}

impl std::fmt::Display for DeadLetterReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TargetNotFound { pid } => write!(f, "target_not_found(pid={pid})"),
            Self::InboxFull { pid } => write!(f, "inbox_full(pid={pid})"),
            Self::Timeout { duration_ms } => write!(f, "timeout({duration_ms}ms)"),
            Self::GovernanceDenied { reason } => write!(f, "governance_denied({reason})"),
            Self::AgentExited { pid } => write!(f, "agent_exited(pid={pid})"),
        }
    }
}

/// A dead-lettered message with metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeadLetter {
    /// The original undeliverable message.
    pub message: KernelMessage,
    /// Why delivery failed.
    pub reason: DeadLetterReason,
    /// When the message was dead-lettered.
    pub timestamp: DateTime<Utc>,
    /// Number of retry attempts made.
    pub retry_count: u32,
}

/// Queue for messages that could not be delivered.
///
/// Bounded FIFO queue: when full, the oldest entry is evicted.
/// Queryable by target PID, reason variant, and time range.
///
/// When an optional [`ChainManager`](crate::chain::ChainManager) is
/// attached, every intake is recorded as an `ipc.dead_letter` event
/// in the ExoChain audit trail.
pub struct DeadLetterQueue {
    letters: RwLock<VecDeque<DeadLetter>>,
    max_size: usize,
    /// Optional chain manager for audit logging.
    #[cfg(feature = "exochain")]
    chain: Option<Arc<crate::chain::ChainManager>>,
}

impl DeadLetterQueue {
    /// Create a new dead letter queue with the given capacity.
    pub fn new(max_size: usize) -> Self {
        Self {
            letters: RwLock::new(VecDeque::with_capacity(max_size.min(1024))),
            max_size,
            #[cfg(feature = "exochain")]
            chain: None,
        }
    }

    /// Create a dead letter queue with the default capacity (10,000).
    pub fn with_default_capacity() -> Self {
        Self::new(DEFAULT_DLQ_CAPACITY)
    }

    /// Attach a chain manager for audit logging (builder pattern).
    ///
    /// When set, every `intake` call emits an `ipc.dead_letter` event
    /// to the ExoChain.
    #[cfg(feature = "exochain")]
    pub fn with_chain(mut self, cm: Arc<crate::chain::ChainManager>) -> Self {
        self.chain = Some(cm);
        self
    }

    /// Intake a message that could not be delivered.
    ///
    /// If the queue is at capacity, the oldest entry is evicted (FIFO).
    /// When a [`ChainManager`](crate::chain::ChainManager) is attached,
    /// the dead letter is recorded as an `ipc.dead_letter` chain event.
    pub fn intake(&self, message: KernelMessage, reason: DeadLetterReason) {
        // Log to chain before moving the message into the letter.
        #[cfg(feature = "exochain")]
        if let Some(ref cm) = self.chain {
            let event = crate::chain::IpcDeadLetterEvent {
                message_id: message.id.clone(),
                from_pid: message.from,
                target: format!("{:?}", message.target),
                payload_type: message.payload.type_name().to_owned(),
                reason: format!("{reason}"),
                timestamp: Utc::now(),
            };
            cm.append_loggable(&event);
        }

        let letter = DeadLetter {
            message,
            reason,
            timestamp: Utc::now(),
            retry_count: 0,
        };

        let mut letters = self.letters.write().expect("DLQ write lock poisoned");
        while letters.len() >= self.max_size {
            letters.pop_front();
        }
        letters.push_back(letter);
    }

    /// Number of dead letters currently in the queue.
    pub fn len(&self) -> usize {
        self.letters.read().expect("DLQ read lock poisoned").len()
    }

    /// Whether the queue is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Maximum capacity of the queue.
    pub fn capacity(&self) -> usize {
        self.max_size
    }

    /// Query dead letters by target PID.
    pub fn query_by_target(&self, pid: Pid) -> Vec<DeadLetter> {
        let letters = self.letters.read().expect("DLQ read lock poisoned");
        letters
            .iter()
            .filter(|l| match &l.reason {
                DeadLetterReason::TargetNotFound { pid: p } => *p == pid,
                DeadLetterReason::InboxFull { pid: p } => *p == pid,
                DeadLetterReason::AgentExited { pid: p } => *p == pid,
                _ => {
                    // Also match on the message target
                    matches!(&l.message.target, crate::ipc::MessageTarget::Process(p) if *p == pid)
                }
            })
            .cloned()
            .collect()
    }

    /// Query dead letters by reason variant name.
    pub fn query_by_reason(&self, reason_name: &str) -> Vec<DeadLetter> {
        let letters = self.letters.read().expect("DLQ read lock poisoned");
        letters
            .iter()
            .filter(|l| {
                let variant = match &l.reason {
                    DeadLetterReason::TargetNotFound { .. } => "TargetNotFound",
                    DeadLetterReason::InboxFull { .. } => "InboxFull",
                    DeadLetterReason::Timeout { .. } => "Timeout",
                    DeadLetterReason::GovernanceDenied { .. } => "GovernanceDenied",
                    DeadLetterReason::AgentExited { .. } => "AgentExited",
                };
                variant == reason_name
            })
            .cloned()
            .collect()
    }

    /// Query dead letters within a time range (inclusive).
    pub fn query_by_time_range(
        &self,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Vec<DeadLetter> {
        let letters = self.letters.read().expect("DLQ read lock poisoned");
        letters
            .iter()
            .filter(|l| l.timestamp >= since && l.timestamp <= until)
            .cloned()
            .collect()
    }

    /// Remove and return a dead letter by message ID for retry.
    ///
    /// The `retry_count` on the returned letter is incremented.
    pub fn take_for_retry(&self, msg_id: &str) -> Option<DeadLetter> {
        let mut letters = self.letters.write().expect("DLQ write lock poisoned");
        let idx = letters.iter().position(|l| l.message.id == msg_id)?;
        let mut letter = letters.remove(idx)?;
        letter.retry_count += 1;
        Some(letter)
    }

    /// Re-add a letter that failed retry back into the queue.
    pub fn re_add(&self, letter: DeadLetter) {
        let mut letters = self.letters.write().expect("DLQ write lock poisoned");
        while letters.len() >= self.max_size {
            letters.pop_front();
        }
        letters.push_back(letter);
    }

    /// Get all dead letters (snapshot).
    pub fn snapshot(&self) -> Vec<DeadLetter> {
        self.letters
            .read()
            .expect("DLQ read lock poisoned")
            .iter()
            .cloned()
            .collect()
    }

    /// Clear all dead letters.
    pub fn clear(&self) {
        self.letters
            .write()
            .expect("DLQ write lock poisoned")
            .clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::{MessagePayload, MessageTarget};

    fn make_msg(from: Pid, target: Pid) -> KernelMessage {
        KernelMessage::new(
            from,
            MessageTarget::Process(target),
            MessagePayload::Text("test".into()),
        )
    }

    #[test]
    fn intake_and_len() {
        let dlq = DeadLetterQueue::new(100);
        assert!(dlq.is_empty());

        dlq.intake(
            make_msg(0, 99),
            DeadLetterReason::TargetNotFound { pid: 99 },
        );
        assert_eq!(dlq.len(), 1);
    }

    #[test]
    fn fifo_eviction_at_capacity() {
        let dlq = DeadLetterQueue::new(3);

        for i in 0..5u64 {
            dlq.intake(make_msg(0, i), DeadLetterReason::TargetNotFound { pid: i });
        }

        assert_eq!(dlq.len(), 3);
        // Oldest (pid 0 and 1) should have been evicted
        let snap = dlq.snapshot();
        let target_pids: Vec<Pid> = snap
            .iter()
            .filter_map(|l| match &l.reason {
                DeadLetterReason::TargetNotFound { pid } => Some(*pid),
                _ => None,
            })
            .collect();
        assert_eq!(target_pids, vec![2, 3, 4]);
    }

    #[test]
    fn query_by_target() {
        let dlq = DeadLetterQueue::new(100);
        dlq.intake(
            make_msg(0, 10),
            DeadLetterReason::TargetNotFound { pid: 10 },
        );
        dlq.intake(make_msg(0, 20), DeadLetterReason::InboxFull { pid: 20 });
        dlq.intake(make_msg(0, 10), DeadLetterReason::AgentExited { pid: 10 });

        let results = dlq.query_by_target(10);
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn query_by_reason() {
        let dlq = DeadLetterQueue::new(100);
        dlq.intake(make_msg(0, 1), DeadLetterReason::TargetNotFound { pid: 1 });
        dlq.intake(make_msg(0, 2), DeadLetterReason::InboxFull { pid: 2 });
        dlq.intake(make_msg(0, 3), DeadLetterReason::TargetNotFound { pid: 3 });

        let results = dlq.query_by_reason("TargetNotFound");
        assert_eq!(results.len(), 2);
        let results = dlq.query_by_reason("InboxFull");
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn query_by_time_range() {
        let dlq = DeadLetterQueue::new(100);
        let before = Utc::now();
        dlq.intake(make_msg(0, 1), DeadLetterReason::TargetNotFound { pid: 1 });
        let after = Utc::now();

        let results = dlq.query_by_time_range(before, after);
        assert_eq!(results.len(), 1);

        // Query with a range that excludes the entry
        let future = after + chrono::Duration::hours(1);
        let far_future = future + chrono::Duration::hours(1);
        let results = dlq.query_by_time_range(future, far_future);
        assert!(results.is_empty());
    }

    #[test]
    fn take_for_retry_increments_count() {
        let dlq = DeadLetterQueue::new(100);
        let msg = make_msg(0, 1);
        let msg_id = msg.id.clone();
        dlq.intake(msg, DeadLetterReason::TargetNotFound { pid: 1 });

        let letter = dlq.take_for_retry(&msg_id).unwrap();
        assert_eq!(letter.retry_count, 1);
        assert!(dlq.is_empty());
    }

    #[test]
    fn take_for_retry_not_found() {
        let dlq = DeadLetterQueue::new(100);
        assert!(dlq.take_for_retry("nonexistent").is_none());
    }

    #[test]
    fn re_add_after_failed_retry() {
        let dlq = DeadLetterQueue::new(100);
        let msg = make_msg(0, 1);
        let msg_id = msg.id.clone();
        dlq.intake(msg, DeadLetterReason::TargetNotFound { pid: 1 });

        let letter = dlq.take_for_retry(&msg_id).unwrap();
        assert_eq!(letter.retry_count, 1);
        dlq.re_add(letter);
        assert_eq!(dlq.len(), 1);

        let snap = dlq.snapshot();
        assert_eq!(snap[0].retry_count, 1);
    }

    #[test]
    fn clear_removes_all() {
        let dlq = DeadLetterQueue::new(100);
        for i in 0..5 {
            dlq.intake(make_msg(0, i), DeadLetterReason::TargetNotFound { pid: i });
        }
        assert_eq!(dlq.len(), 5);
        dlq.clear();
        assert!(dlq.is_empty());
    }

    #[test]
    fn governance_denied_reason() {
        let dlq = DeadLetterQueue::new(100);
        dlq.intake(
            make_msg(0, 1),
            DeadLetterReason::GovernanceDenied {
                reason: "policy violation".into(),
            },
        );

        let results = dlq.query_by_reason("GovernanceDenied");
        assert_eq!(results.len(), 1);
        match &results[0].reason {
            DeadLetterReason::GovernanceDenied { reason } => {
                assert_eq!(reason, "policy violation");
            }
            _ => panic!("expected GovernanceDenied"),
        }
    }

    #[test]
    fn timeout_reason() {
        let dlq = DeadLetterQueue::new(100);
        dlq.intake(
            make_msg(0, 1),
            DeadLetterReason::Timeout { duration_ms: 5000 },
        );

        let results = dlq.query_by_reason("Timeout");
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn reason_display() {
        let reason = DeadLetterReason::TargetNotFound { pid: 42 };
        assert_eq!(format!("{reason}"), "target_not_found(pid=42)");

        let reason = DeadLetterReason::InboxFull { pid: 7 };
        assert_eq!(format!("{reason}"), "inbox_full(pid=7)");
    }

    #[test]
    fn dead_letter_serde_roundtrip() {
        let letter = DeadLetter {
            message: make_msg(0, 1),
            reason: DeadLetterReason::TargetNotFound { pid: 1 },
            timestamp: Utc::now(),
            retry_count: 2,
        };
        let json = serde_json::to_string(&letter).unwrap();
        let restored: DeadLetter = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.retry_count, 2);
        assert_eq!(restored.message.id, letter.message.id);
    }

    #[cfg(feature = "exochain")]
    #[test]
    fn intake_logs_to_chain() {
        let cm = std::sync::Arc::new(crate::chain::ChainManager::new(0, 100));
        let initial_len = cm.len();

        let dlq = DeadLetterQueue::new(100).with_chain(cm.clone());
        dlq.intake(
            make_msg(0, 99),
            DeadLetterReason::TargetNotFound { pid: 99 },
        );

        assert_eq!(dlq.len(), 1);
        assert_eq!(cm.len(), initial_len + 1);

        let events = cm.tail(1);
        assert_eq!(events[0].source, "ipc");
        assert_eq!(events[0].kind, "ipc.dead_letter");

        let payload = events[0].payload.as_ref().unwrap();
        assert_eq!(payload["from_pid"], 0);
        assert!(
            payload["reason"]
                .as_str()
                .unwrap()
                .contains("target_not_found")
        );
    }

    #[cfg(feature = "exochain")]
    #[test]
    fn intake_without_chain_does_not_log() {
        // Without with_chain, intake should still work normally
        let dlq = DeadLetterQueue::new(100);
        dlq.intake(
            make_msg(0, 1),
            DeadLetterReason::Timeout { duration_ms: 1000 },
        );
        assert_eq!(dlq.len(), 1);
    }
}
