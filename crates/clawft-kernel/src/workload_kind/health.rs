//! Kind-defined instance health (ADR-099 section 7: "instance health from a
//! periodic status heartbeat whose meaning is kind-defined").
//!
//! A kind says how often its instances are polled, how many bad polls in a
//! row make one unhealthy, and how a status sample is judged. The sample is
//! deliberately small and runtime-agnostic (the adapter's `InstanceStatus`
//! is mapped onto it by the host), so a kind defines health without
//! depending on any adapter.

use serde::{Deserialize, Serialize};

/// How a kind's instances are polled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthSpec {
    /// Poll period.
    pub interval_ms: u64,
    /// Consecutive bad polls that make an instance unhealthy.
    pub miss_limit: u32,
}

impl Default for HealthSpec {
    fn default() -> Self {
        Self {
            interval_ms: 10_000,
            miss_limit: 3,
        }
    }
}

/// The adapter state a poll saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SampleState {
    /// Loaded, never started.
    Loaded,
    /// Running.
    Running,
    /// Exited (finished or stopped).
    Exited,
    /// The adapter reports it degraded.
    Degraded,
    /// The adapter does not know it.
    Unknown,
}

/// One poll of an instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HealthSample {
    /// State the adapter reported.
    pub state: SampleState,
    /// Exit code when it exited.
    pub exit_code: Option<i32>,
    /// The instance is meant to run until stopped (listener or interval
    /// mode); false for a one-shot run that ends by itself.
    pub continuous: bool,
    /// How long ago the instance was last heard from, for kinds whose health
    /// is a remote heartbeat (`computer`); `None` for process-backed kinds
    /// and when the adapter has no connection to measure.
    pub heartbeat_age_ms: Option<u64>,
}

/// A kind's verdict on one poll.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Health {
    /// Fine: resets the miss count.
    Healthy,
    /// A one-shot run ended cleanly: nothing to restart.
    Finished,
    /// Not fine; counts as a miss. The text is chained.
    Unhealthy(String),
}

/// The default judgment (a process-backed workload): running is healthy, a
/// degraded or unexpectedly exited process is not, an unknown instance is a
/// miss, and a clean one-shot exit is finished.
pub fn judge_process(s: &HealthSample) -> Health {
    match s.state {
        SampleState::Running => Health::Healthy,
        SampleState::Loaded => Health::Healthy,
        SampleState::Degraded => Health::Unhealthy("adapter reports it degraded".into()),
        SampleState::Unknown => Health::Unhealthy("adapter does not know the instance".into()),
        SampleState::Exited if !s.continuous && s.exit_code == Some(0) => Health::Finished,
        SampleState::Exited => Health::Unhealthy(match s.exit_code {
            Some(c) => format!("exited with code {c}"),
            None => "exited (killed by a signal)".into(),
        }),
    }
}
