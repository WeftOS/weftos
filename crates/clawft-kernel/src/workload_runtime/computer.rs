//! The `computer` adapter: a desktop reached through the daemon-hosted
//! computer hub, as a workload.
//!
//! The adapter owns no process. An instance is the claim "this node answers
//! for desktop `<id>`", and its health is the hub's own evidence: when the
//! desktop's tool server was last heard from (any frame, ping included).
//!
//! # Instance to hub connection
//!
//! A computer workload's id is its desktop (`computer_workload` sets
//! `id: desktop`), and the hub registers each tool server under its machine
//! id. So the connection is found by name: instance -> workload id ->
//! `HeartbeatSource::last_seen_ms(<desktop>)`. There is no other table.
//!
//! The kernel does not depend on the hub crate: the daemon passes a
//! [`HeartbeatSource`] (the hub implements it) and a clock, so the same code is
//! tested with an injected clock.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use clawft_types::placement::Capability;
use tokio::sync::Mutex;

use super::evidence::RunEvidence;
use super::types::{
    Admission, ControlMode, InstanceHandle, InstanceState, InstanceStatus, RuntimeError, VerifiedWorkload,
    WorkloadConfig, WorkloadRuntime, WorkloadSource,
};
use crate::workload_governance::NetworkPolicy;
use crate::workload_kind::{ComputerKind, WorkloadKind};

/// Adapter id (what `ComputerKind::adapters()` names).
pub const COMPUTER_ID: &str = "computer";

/// Where the adapter reads "when was this desktop last heard from".
pub trait HeartbeatSource: Send + Sync {
    /// Wall-clock milliseconds of the last frame from the tool server of
    /// `desktop`, or `None` when none is connected.
    fn last_seen_ms(&self, desktop: &str) -> Option<u64>;
}

/// The clock the adapter reads "now" from, in the source's time base.
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// Wall-clock milliseconds since the Unix epoch.
pub fn wall_clock_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

struct Instance {
    desktop: String,
    started: bool,
}

/// The adapter.
pub struct ComputerRuntime {
    source: Arc<dyn HeartbeatSource>,
    clock: Clock,
    instances: Mutex<HashMap<String, Instance>>,
}

impl ComputerRuntime {
    /// An adapter over `source`, reading the wall clock.
    pub fn new(source: Arc<dyn HeartbeatSource>) -> Self {
        Self::with_clock(source, Arc::new(wall_clock_ms))
    }

    /// An adapter with an injected clock (tests).
    pub fn with_clock(source: Arc<dyn HeartbeatSource>, clock: Clock) -> Self {
        Self {
            source,
            clock,
            instances: Mutex::new(HashMap::new()),
        }
    }

    /// Age of the desktop's last frame; `None` when it is not connected.
    fn age_of(&self, desktop: &str) -> Option<u64> {
        let seen = self.source.last_seen_ms(desktop)?;
        Some((self.clock)().saturating_sub(seen))
    }

    /// The most silence a live desktop is allowed (the kind's own window).
    fn stale_after_ms() -> u64 {
        let h = ComputerKind.health();
        h.interval_ms.saturating_mul(u64::from(h.miss_limit))
    }
}

fn instance_id(desktop: &str) -> String {
    format!("computer-{desktop}")
}

fn desktop_of(w: &VerifiedWorkload) -> Result<String, RuntimeError> {
    match &w.source {
        WorkloadSource::Computer(c) => Ok(c.desktop.clone()),
        _ => Err(RuntimeError::AdmissionRefused(format!(
            "{COMPUTER_ID} runs only computer workloads"
        ))),
    }
}

#[async_trait]
impl WorkloadRuntime for ComputerRuntime {
    fn id(&self) -> &str {
        COMPUTER_ID
    }

    /// The node that is a desktop advertises `computer.desktop.<id>` itself;
    /// this adapter is the hub-side judge and advertises nothing.
    fn provides(&self) -> Vec<Capability> {
        Vec::new()
    }

    async fn admit(&self, w: &VerifiedWorkload) -> Result<Admission, RuntimeError> {
        let desktop = desktop_of(w)?;
        Ok(Admission {
            runtime: COMPUTER_ID.into(),
            arch: std::env::consts::ARCH.into(),
            emulated: false,
            notes: vec![format!("desktop {desktop} judged by its hub heartbeat")],
        })
    }

    async fn load(&self, w: &VerifiedWorkload, _cfg: &WorkloadConfig) -> Result<InstanceHandle, RuntimeError> {
        let desktop = desktop_of(w)?;
        let iid = instance_id(&desktop);
        let mut map = self.instances.lock().await;
        if map.contains_key(&iid) {
            return Err(RuntimeError::InvalidState(format!("{iid} is already loaded")));
        }
        map.insert(
            iid.clone(),
            Instance {
                desktop: desktop.clone(),
                started: false,
            },
        );
        Ok(InstanceHandle {
            runtime: COMPUTER_ID.into(),
            instance_id: iid,
            workload_id: desktop,
            store_installed: false,
        })
    }

    /// Start watching: the desktop is not launched, it is expected to be
    /// connected. Health says whether it is.
    async fn start(&self, h: &InstanceHandle) -> Result<(), RuntimeError> {
        let mut map = self.instances.lock().await;
        let inst = map
            .get_mut(&h.instance_id)
            .ok_or_else(|| RuntimeError::UnknownInstance(h.instance_id.clone()))?;
        if inst.started {
            return Err(RuntimeError::InvalidState("already running".into()));
        }
        inst.started = true;
        Ok(())
    }

    async fn stop(&self, h: &InstanceHandle, _grace: Duration) -> Result<RunEvidence, RuntimeError> {
        let mut map = self.instances.lock().await;
        let inst = map
            .get_mut(&h.instance_id)
            .ok_or_else(|| RuntimeError::UnknownInstance(h.instance_id.clone()))?;
        inst.started = false;
        Ok(RunEvidence {
            runtime: COMPUTER_ID.into(),
            instance_id: h.instance_id.clone(),
            ..RunEvidence::default()
        })
    }

    async fn unload(&self, h: InstanceHandle) -> Result<(), RuntimeError> {
        let mut map = self.instances.lock().await;
        match map.get(&h.instance_id) {
            None => Err(RuntimeError::UnknownInstance(h.instance_id)),
            Some(i) if i.started => Err(RuntimeError::InvalidState("stop the instance before unloading".into())),
            Some(_) => {
                map.remove(&h.instance_id);
                Ok(())
            }
        }
    }

    /// Not started: `Loaded`. Started: `Running` while the hub has heard the
    /// desktop within the kind's window, `Degraded` past it, `Unknown` with no
    /// connection.
    async fn status(&self, h: &InstanceHandle) -> InstanceStatus {
        let map = self.instances.lock().await;
        let Some(inst) = map.get(&h.instance_id) else {
            return InstanceStatus::of(InstanceState::Unknown);
        };
        if !inst.started {
            return InstanceStatus::of(InstanceState::Loaded);
        }
        match self.age_of(&inst.desktop) {
            None => InstanceStatus {
                state: InstanceState::Unknown,
                exit_code: None,
                detail: Some("no hub connection for this desktop".into()),
            },
            Some(age) if age > Self::stale_after_ms() => InstanceStatus {
                state: InstanceState::Degraded,
                exit_code: None,
                detail: Some(format!("hub heartbeat stale: last seen {}s ago", age / 1000)),
            },
            Some(age) => InstanceStatus {
                state: InstanceState::Running,
                exit_code: None,
                detail: Some(format!("last seen {}s ago", age / 1000)),
            },
        }
    }

    async fn heartbeat_age_ms(&self, h: &InstanceHandle) -> Option<u64> {
        let map = self.instances.lock().await;
        self.age_of(&map.get(&h.instance_id)?.desktop)
    }

    fn control_mode(&self) -> ControlMode {
        ControlMode::Adopted
    }

    fn network_exposure(&self) -> NetworkPolicy {
        NetworkPolicy::None
    }

    async fn console(&self, _h: &InstanceHandle, _command: &str) -> Result<RunEvidence, RuntimeError> {
        Err(RuntimeError::Unsupported("a computer has no console cycle".into()))
    }
}
