//! Node-local supervision of hosted instances (ADR-099 section 7): a
//! periodic health heartbeat whose meaning each kind defines, and bounded
//! restarts with backoff, every step chained.
//!
//! [`WorkloadHostService::supervise`] polls each instance that is meant to
//! run, once per its kind's interval. A kind's verdict
//! ([`crate::workload_kind::WorkloadKind::judge`]) counts a miss; enough
//! misses in a row make the instance `Unhealthy`, and the restart policy
//! then restarts it (stop, then start, both gated and chained by the
//! adapter host) until its budget inside the window is spent, at which
//! point it is `Failed` and held for the operator. An instance the
//! operator stopped, one that was never started and a finished one-shot run
//! are never restarted.

use std::time::Duration;

use serde::Serialize;
use serde_json::json;

use super::host_service::WorkloadHostService;
use super::lifecycle::{InstanceLife, LifecycleState, RestartDecision, RestartPolicy};
use crate::chain;
use crate::workload_kind::{Health, HealthSample, HealthSpec, SampleState, judge_process};
use crate::workload_runtime::{InstanceState, InstanceStatus};

/// Grace given to a hung instance before it is killed for a restart.
const RESTART_GRACE: Duration = Duration::from_secs(2);

/// One thing a [`WorkloadHostService::supervise`] pass did.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Supervised {
    /// Instance id on this node.
    pub instance_id: String,
    /// Workload name.
    pub workload: String,
    /// State before.
    pub from: LifecycleState,
    /// State after.
    pub to: LifecycleState,
    /// Why.
    pub reason: String,
}

fn now_ms() -> u64 {
    chrono::Utc::now().timestamp_millis().max(0) as u64
}

/// Move `life` to `to` if that step is legal (an operator verb racing the
/// supervisor can make it illegal; the verb wins).
pub(super) fn enter(life: &mut InstanceLife, to: LifecycleState) {
    let _ = life.transition(to, now_ms());
}

fn sample(st: &InstanceStatus, continuous: bool) -> HealthSample {
    HealthSample {
        state: match st.state {
            InstanceState::Loaded => SampleState::Loaded,
            InstanceState::Running => SampleState::Running,
            InstanceState::Exited => SampleState::Exited,
            InstanceState::Degraded => SampleState::Degraded,
            InstanceState::Unknown => SampleState::Unknown,
        },
        exit_code: st.exit_code,
        continuous,
    }
}

impl WorkloadHostService {
    /// Use these kinds' health definitions (default: the built-in kinds).
    pub fn with_kind_registry(mut self, kinds: crate::workload_kind::KindRegistry) -> Self {
        self.kinds = kinds;
        self
    }

    /// Opt in to the node-side lease: continuous instances stop once no
    /// controller other than this node itself has been heard from for
    /// `lease`, and only for instances another node placed. It must be
    /// longer than the controller's tick (a lease shorter than a few ticks
    /// would stop work for ordinary jitter) and, to stop the old copy before
    /// a replacement starts, at most the controller's `dead_after`. Set it at or below the controller's `dead_after`, so the
    /// copy on a partitioned node stops before the controller starts a
    /// replacement. Off by default.
    pub fn with_lease(mut self, lease: Duration) -> Self {
        self.lease = Some(lease);
        self
    }

    /// Replace the restart bounds.
    pub fn with_restart_policy(mut self, p: RestartPolicy) -> Self {
        self.restart = p;
        self
    }

    /// Stop continuous instances when the controller lease is on and has
    /// expired (opt-in, see [`Self::with_lease`]). Returns what it stopped.
    async fn expire_lease(&self, now_ms: u64) -> Vec<Supervised> {
        let Some(lease) = self.lease else {
            return Vec::new();
        };
        let silent = now_ms.saturating_sub(self.last_contact_ms.load(std::sync::atomic::Ordering::Relaxed));
        if silent <= lease.as_millis() as u64 {
            return Vec::new();
        }
        let mut targets = Vec::new();
        {
            let mut map = self.instances.lock().await;
            for (id, p) in map.iter_mut() {
                if !p.continuous
                    || !p.desired_running
                    || !p.life.state.is_live()
                    || p.requester == self.node_id
                {
                    continue;
                }
                let Some(host) = self.routes.get(&p.route).cloned() else {
                    continue;
                };
                // The token goes first, as for an operator stop.
                if let (Some(hk), Some(l)) = (&self.ingest, &p.ingest) {
                    hk.deactivate(l);
                }
                self.bus_off(p.cogbus.as_ref());
                p.desired_running = false;
                targets.push((id.clone(), p.handle.clone(), host, p.life.state));
            }
        }
        let mut out = Vec::new();
        for (id, handle, host, from) in targets {
            let name = host.workload_for(&handle).await.map(|w| w.id).unwrap_or_default();
            let r = host.stop(&handle, RESTART_GRACE).await;
            let mut map = self.instances.lock().await;
            if let Some(p) = map.get_mut(&id) {
                p.lease_stopped = r.is_ok();
                if let Ok(ev) = r {
                    p.last = Some(ev);
                }
                enter(&mut p.life, LifecycleState::Stopped);
            }
            let s = Supervised {
                instance_id: id,
                workload: name,
                from,
                to: LifecycleState::Stopped,
                reason: format!("controller lease expired: no controller heard for {silent} ms"),
            };
            self.chain_life(&s, &self.node_id, json!({ "phase": "lease" }));
            out.push(s);
        }
        out
    }

    fn health_spec(&self, kind: &str) -> HealthSpec {
        self.kinds.get(kind).map_or_else(HealthSpec::default, |k| k.health())
    }

    fn judge(&self, kind: &str, s: &HealthSample) -> Health {
        self.kinds
            .get(kind)
            .map_or_else(|| judge_process(s), |k| k.judge(s))
    }

    /// Lifecycle state of a hosted instance.
    pub async fn lifecycle_of(&self, instance_id: &str) -> Option<LifecycleState> {
        self.instances.lock().await.get(instance_id).map(|p| p.life.state)
    }

    fn chain_life(&self, s: &Supervised, node: &str, extra: serde_json::Value) {
        let mut payload = json!({
            "node": node, "instance_id": s.instance_id, "workload": s.workload,
            "from": s.from, "to": s.to, "reason": s.reason,
        });
        if let (Some(p), Some(e)) = (payload.as_object_mut(), extra.as_object()) {
            p.extend(e.clone());
        }
        self.record(chain::EVENT_KIND_WORKLOAD_LIFECYCLE, payload);
    }

    /// Run the health heartbeat and the bounded restarts once, at `now_ms`.
    /// Returns what changed. Instances are polled outside the instance map's
    /// lock, so a slow adapter never holds up `status` or `place`.
    pub async fn supervise(&self, now_ms: u64) -> Vec<Supervised> {
        struct Due {
            id: String,
            handle: crate::workload_runtime::InstanceHandle,
            host: std::sync::Arc<crate::workload_runtime::WorkloadHost>,
            name: String,
            kind: String,
            continuous: bool,
        }
        let mut out = self.expire_lease(now_ms).await;
        let mut due = Vec::new();
        {
            let mut map = self.instances.lock().await;
            for (id, p) in map.iter_mut() {
                let skip = !p.desired_running
                    || !matches!(
                        p.life.state,
                        LifecycleState::Running | LifecycleState::Unhealthy | LifecycleState::Restarting
                    );
                let Some(host) = self.routes.get(&p.route).cloned() else {
                    continue;
                };
                if skip {
                    continue;
                }
                let Some(w) = host.workload_for(&p.handle).await else {
                    continue;
                };
                let spec = self.health_spec(&w.kind);
                if now_ms.saturating_sub(p.life.last_poll_ms) < spec.interval_ms {
                    continue;
                }
                p.life.last_poll_ms = now_ms;
                due.push(Due {
                    id: id.clone(),
                    handle: p.handle.clone(),
                    host,
                    name: w.id.clone(),
                    kind: w.kind.clone(),
                    continuous: p.continuous,
                });
            }
        }
        for d in due {
            let st = d.host.status(&d.handle).await;
            let verdict = self.judge(&d.kind, &sample(&st, d.continuous));
            if let Some(s) = self.apply_verdict(&d.id, &d.name, &d.kind, &d.handle, &d.host, verdict, now_ms).await {
                out.extend(s);
            }
        }
        out
    }

    #[allow(clippy::too_many_arguments)]
    async fn apply_verdict(
        &self,
        id: &str,
        name: &str,
        kind: &str,
        handle: &crate::workload_runtime::InstanceHandle,
        host: &crate::workload_runtime::WorkloadHost,
        verdict: Health,
        now_ms: u64,
    ) -> Option<Vec<Supervised>> {
        let limit = self.health_spec(kind).miss_limit.max(1);
        let mut steps: Vec<Supervised> = Vec::new();
        let step = |from: LifecycleState, to: LifecycleState, reason: String| Supervised {
            instance_id: id.to_string(),
            workload: name.to_string(),
            from,
            to,
            reason,
        };
        // Decide under the lock (state only); act on the adapter outside it.
        let restart_attempt;
        {
            let mut map = self.instances.lock().await;
            let p = map.get_mut(id)?;
            if !p.desired_running {
                return None; // an operator verb overtook this poll
            }
            match verdict {
                Health::Healthy => {
                    p.life.misses = 0;
                    let from = p.life.state;
                    if matches!(from, LifecycleState::Unhealthy | LifecycleState::Restarting) {
                        enter(&mut p.life, LifecycleState::Running);
                        steps.push(step(from, LifecycleState::Running, "health recovered".into()));
                    }
                    restart_attempt = None;
                }
                Health::Finished => {
                    let from = p.life.state;
                    p.desired_running = false;
                    enter(&mut p.life, LifecycleState::Finished);
                    steps.push(step(from, LifecycleState::Finished, "one-shot run ended cleanly".into()));
                    restart_attempt = None;
                }
                Health::Unhealthy(why) => {
                    p.life.misses += 1;
                    let from = p.life.state;
                    if p.life.misses < limit {
                        return None;
                    }
                    if from == LifecycleState::Running {
                        enter(&mut p.life, LifecycleState::Unhealthy);
                        steps.push(step(
                            from,
                            LifecycleState::Unhealthy,
                            format!("{} missed heartbeats: {why}", p.life.misses),
                        ));
                    }
                    match p.life.restart_decision(&self.restart, now_ms) {
                        RestartDecision::Wait { .. } => restart_attempt = None,
                        RestartDecision::Exhausted => {
                            let f = p.life.state;
                            p.desired_running = false;
                            enter(&mut p.life, LifecycleState::Failed);
                            steps.push(step(
                                f,
                                LifecycleState::Failed,
                                format!(
                                    "restart budget spent ({} in {} ms): {why}",
                                    self.restart.max_restarts, self.restart.window_ms
                                ),
                            ));
                            restart_attempt = None;
                        }
                        RestartDecision::Restart { attempt } => {
                            let f = p.life.state;
                            enter(&mut p.life, LifecycleState::Restarting);
                            p.life.record_restart(&self.restart, now_ms);
                            steps.push(step(
                                f,
                                LifecycleState::Restarting,
                                format!("restart {attempt}/{}: {why}", self.restart.max_restarts),
                            ));
                            restart_attempt = Some(attempt);
                        }
                    }
                }
            }
        }
        // A failed instance is also taken off the CPU (best effort).
        let failed = steps.iter().any(|s| s.to == LifecycleState::Failed);
        if failed {
            let _ = host.stop(handle, RESTART_GRACE).await;
        }
        if let Some(attempt) = restart_attempt {
            // Stop (a hung process), then start: both gated and chained by
            // the adapter host. "Not started" / "already exited" on stop is
            // fine; the start decides.
            let _ = host.stop(handle, RESTART_GRACE).await;
            let started = host.start(handle).await;
            let mut map = self.instances.lock().await;
            if let Some(p) = map.get_mut(id) {
                let from = p.life.state;
                match &started {
                    Ok(()) => {
                        enter(&mut p.life, LifecycleState::Running);
                        steps.push(step(from, LifecycleState::Running, format!("restarted (attempt {attempt})")));
                    }
                    Err(e) => {
                        // Back to unhealthy: the next poll retries within the budget.
                        enter(&mut p.life, LifecycleState::Unhealthy);
                        steps.push(step(from, LifecycleState::Unhealthy, format!("restart failed: {e}")));
                    }
                }
            }
        }
        for s in &steps {
            self.chain_life(s, &self.node_id, json!({ "phase": "supervise" }));
        }
        (!steps.is_empty()).then_some(steps)
    }

    /// Run [`Self::supervise`] every `period` for as long as the runtime lives.
    pub fn spawn_supervisor(
        self: &std::sync::Arc<Self>,
        period: Duration,
    ) -> tokio::task::JoinHandle<()> {
        let me = std::sync::Arc::clone(self);
        tokio::spawn(async move {
            let mut t = tokio::time::interval(period);
            t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                t.tick().await;
                let acted = me.supervise(now_ms()).await;
                if !acted.is_empty() {
                    tracing::info!(n = acted.len(), "instance supervision");
                }
            }
        })
    }
}
