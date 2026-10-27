//! Controller-side lifecycle (ADR-099 section 7): mirror each instance's
//! state from its node, notice a `Dead` node, mark its instances `Lost` and
//! reschedule them through the same gate and placement engine as any
//! placement, and settle with a node that comes back.
//!
//! **Delivery is at-least-once, with no fencing.** While a node is cut off
//! but still running, its copy keeps running (and a sensor cog keeps
//! producing) until the controller notices, replaces it and the node
//! returns to be told to unload the old one. For the length of the
//! partition the workload may run twice. The opt-in node lease
//! (`WorkloadHostService::with_lease`) narrows that window: a node that has
//! heard no controller for the lease stops its continuous instances itself.
//! See ADR-099 section 7.
//!
//! [`PlacementControlPlane::lifecycle_tick`] is one pass:
//!
//! 1. re-describe every target (reachability) and note how long each has
//!    been down;
//! 2. a node is `Dead` when it has been unreachable for
//!    `PlaneConfig::dead_after` and membership, when it knows the peer, does
//!    not still hold it as healthy. Direct contact is the authority:
//!    membership never makes a node dead that answers;
//! 3. each instance on a `Dead` node becomes `Lost` and is rescheduled
//!    (`plane_reschedule`) unless it is pinned, non-migratable (operator,
//!    kind, or needing attached hardware), has no stored order, or moved or
//!    failed to move too often; those raise an alert (chained) and stay
//!    `Lost`;
//! 4. the old record of a rescheduled instance is kept as `Rescheduled`.
//!    When its node returns, that copy is unloaded there (signed, gated,
//!    chained), with backoff and one chained report per distinct failure;
//! 5. a node that is reachable but no longer holds an instance is told
//!    apart: it reports explicitly what a revocation or an unload took
//!    down, and anything else was lost with the node (a reboot) and is
//!    placed again;
//! 6. an instance that was `Lost` and is still on its node when it returns
//!    is adopted back (a lease-stopped one is restarted).
//! 7. a `Lost` record whose order its node holds under another record (or
//!    whose instance the operator unloaded) is superseded and dropped, never
//!    placed again (`plane_reschedule`).
//!
//! Every step is chained: `workload.lifecycle` for state changes and alerts,
//! `workload.migrate` for a reschedule.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::lifecycle::{InstanceLife, LifecyclePolicy, LifecycleState};
use super::msg::{RefusalCode, method};
use super::plane::{CallFailure, PlacementControlPlane, PlacementRecord, PlaneError, now_ms};
use super::plane_place::PlaceOrder;
use crate::chain;

/// What the controller keeps per instance beyond its placement record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControllerLife {
    /// Current state and history.
    pub life: InstanceLife,
    /// May it move, and how often.
    pub policy: LifecyclePolicy,
    /// The order it was placed with (what a reschedule re-runs).
    #[serde(default)]
    pub order: Option<PlaceOrder>,
    /// Times it was moved to another node.
    #[serde(default)]
    pub reschedules: u32,
    /// Last reason a reschedule could not happen (chained once per change).
    #[serde(default)]
    pub last_error: Option<String>,
    /// It was meant to be running when its node was lost. A stopped,
    /// failed or finished instance is never started somewhere else.
    #[serde(default = "yes")]
    pub was_running: bool,
    /// Reschedule attempts that found no node (bounded by `max_reschedules`).
    #[serde(default)]
    pub attempts: u32,
    /// Earliest time of the next reschedule attempt (ms).
    #[serde(default)]
    pub next_attempt_ms: u64,
    /// Failed unloads of the replaced copy on its returned node.
    #[serde(default)]
    pub orphan_attempts: u32,
    /// Earliest time of the next orphan unload (ms).
    #[serde(default)]
    pub orphan_next_ms: u64,
    /// Last orphan-unload failure (chained once per change).
    #[serde(default)]
    pub orphan_error: Option<String>,
}

fn yes() -> bool {
    true
}

impl ControllerLife {
    pub(super) fn new(
        state: LifecycleState,
        policy: LifecyclePolicy,
        order: Option<PlaceOrder>,
        reschedules: u32,
    ) -> Self {
        Self {
            life: InstanceLife::new(state, now_ms()),
            policy,
            order,
            reschedules,
            last_error: None,
            was_running: true,
            attempts: 0,
            next_attempt_ms: 0,
            orphan_attempts: 0,
            orphan_next_ms: 0,
            orphan_error: None,
        }
    }
}

/// Something one [`PlacementControlPlane::lifecycle_tick`] did.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LifecycleEvent {
    /// Instance id (on the node it was last placed on).
    pub instance_id: String,
    /// That node.
    pub node_id: String,
    /// `lost`, `rescheduled`, `replaced`, `alert`, `reschedule_failed`,
    /// `orphan_unloaded`, `orphan_unload_failed`, `recovered`, `failed`,
    /// `gone` or `superseded`.
    pub action: String,
    /// Human-readable detail.
    pub detail: String,
}

pub(super) fn event(rec: &PlacementRecord, action: &str, detail: impl Into<String>) -> LifecycleEvent {
    LifecycleEvent {
        instance_id: rec.instance_id.clone(),
        node_id: rec.node_id.clone(),
        action: action.into(),
        detail: detail.into(),
    }
}

/// Longest wait between retries of a failed reschedule or orphan unload.
const MAX_BACKOFF_MS: u64 = 300_000;

pub(super) fn backoff_ms(base_ms: u64, attempts: u32) -> u64 {
    base_ms
        .saturating_mul(1u64 << attempts.saturating_sub(1).min(20))
        .min(MAX_BACKOFF_MS)
}

impl PlacementControlPlane {
    /// Remember how a fresh placement was ordered.
    pub(super) fn register_life(&self, rec: &PlacementRecord, order: &PlaceOrder, p: LifecyclePolicy) {
        let state = if order.start {
            LifecycleState::Running
        } else {
            LifecycleState::Loaded
        };
        if let Ok(mut l) = self.lives.lock() {
            l.insert(
                rec.instance_id.clone(),
                ControllerLife::new(state, p, Some(order.clone()), 0),
            );
        }
        self.persist();
    }

    pub(super) fn drop_life(&self, instance_id: &str) {
        if let Ok(mut l) = self.lives.lock() {
            l.remove(instance_id);
        }
    }

    /// Lifecycle state the controller holds for `instance_id`.
    pub fn lifecycle_of(&self, instance_id: &str) -> Option<LifecycleState> {
        self.lives
            .lock()
            .ok()
            .and_then(|l| l.get(instance_id).map(|c| c.life.state))
    }

    /// `{state, restarts, reschedules, has_error}` the controller holds for
    /// `instance_id`; in memory only, no peer is contacted. The error text is
    /// withheld (it can carry paths and peer detail).
    pub fn life_summary(&self, instance_id: &str) -> Option<Value> {
        self.life_of(instance_id).map(|c| {
            json!({
                "state": c.life.state,
                "restarts": c.life.restarts.len(),
                "reschedules": c.reschedules,
                "has_error": c.last_error.is_some(),
            })
        })
    }

    pub(super) fn life_of(&self, instance_id: &str) -> Option<ControllerLife> {
        self.lives.lock().ok().and_then(|l| l.get(instance_id).cloned())
    }

    pub(super) fn update_life(&self, instance_id: &str, f: impl FnOnce(&mut ControllerLife)) {
        if let Ok(mut l) = self.lives.lock()
            && let Some(c) = l.get_mut(instance_id)
        {
            f(c);
        }
    }

    pub(super) fn set_state(&self, rec: &PlacementRecord, to: LifecycleState, reason: &str, force: bool) {
        let kind = chain::EVENT_KIND_WORKLOAD_LIFECYCLE;
        let from = {
            let Ok(mut l) = self.lives.lock() else { return };
            let c = l.entry(rec.instance_id.clone()).or_insert_with(|| {
                ControllerLife::new(LifecycleState::Running, LifecyclePolicy::default(), None, 0)
            });
            if to == LifecycleState::Lost {
                c.was_running = c.life.state.is_live();
            }
            match c.life.transition(to, now_ms()) {
                Ok(f) => f,
                // What the node reports wins over what the table allows.
                Err(_) if force => {
                    let f = c.life.state;
                    c.life.state = to;
                    c.life.since_ms = now_ms();
                    f
                }
                Err(_) => return,
            }
        };
        self.chain_event(
            kind,
            json!({ "phase": "lifecycle", "instance_id": rec.instance_id, "node": rec.node_id,
                    "workload": rec.workload, "from": from, "to": to, "reason": reason }),
        );
    }

    /// Whether `node` is `Dead` now: unreachable for `dead_after`, and
    /// membership (when it knows the peer) does not hold it healthy.
    fn node_dead(&self, node: &str, now: u64) -> bool {
        let since = self.down_since.lock().ok().and_then(|d| d.get(node).copied());
        let down = since.is_some_and(|s| now.saturating_sub(s) >= self.cfg.dead_after.as_millis() as u64);
        if !down {
            return false;
        }
        match self.membership.as_ref().and_then(|m| m.get_peer(node)) {
            Some(p) => !matches!(
                super::facts::liveness_of(&p.state),
                clawft_types::placement::engine::Liveness::Alive
            ),
            None => true,
        }
    }

    fn note_reachability(&self, now: u64) {
        let targets = self.targets();
        let Ok(mut d) = self.down_since.lock() else { return };
        for t in targets {
            if t.reachable {
                d.remove(&t.node_id);
            } else {
                d.entry(t.node_id).or_insert(now);
            }
        }
    }

    /// One pass of the controller lifecycle; see the module docs.
    pub async fn lifecycle_tick(&self) -> Vec<LifecycleEvent> {
        self.refresh().await;
        let now = now_ms();
        self.note_reachability(now);
        let mut out = Vec::new();
        self.mirror_states(&mut out).await;
        let known: Vec<String> = self.targets().into_iter().map(|t| t.node_id).collect();
        for rec in self.placements() {
            // Seed placements and instances on nodes we have no target for
            // are not this pass's business (a Seed is hardware-bound).
            if !known.contains(&rec.node_id) {
                continue;
            }
            let Some(state) = self.lifecycle_of(&rec.instance_id) else {
                self.set_state(&rec, LifecycleState::Running, "adopted by the lifecycle", false);
                continue;
            };
            let dead = self.node_dead(&rec.node_id, now);
            match (dead, state) {
                (false, LifecycleState::Rescheduled) => self.unload_orphan(&rec, &mut out).await,
                (_, LifecycleState::Rescheduled) => {}
                (true, LifecycleState::Lost) => self.reschedule(&rec, true, &mut out).await,
                (true, s) if !s.is_terminal() => {
                    self.set_state(&rec, LifecycleState::Lost, "node is dead", false);
                    out.push(event(&rec, "lost", format!("node {} is dead", rec.node_id)));
                    self.reschedule(&rec, true, &mut out).await;
                }
                (false, LifecycleState::Lost) => self.rejoined(&rec, &mut out).await,
                _ => {}
            }
        }
        self.persist();
        out
    }

    /// Mirror each reachable node's instance states. An instance the node no
    /// longer holds is a real teardown when the node says so (revoked,
    /// unloaded), and otherwise was lost with the node (a reboot): it is
    /// placed again.
    async fn mirror_states(&self, out: &mut Vec<LifecycleEvent>) {
        let recs = self.placements();
        for t in self.targets().into_iter().filter(|t| t.reachable) {
            let mine: Vec<&PlacementRecord> = recs.iter().filter(|r| r.node_id == t.node_id).collect();
            if mine.is_empty() {
                continue;
            }
            let body = json!({ "include_departed": true });
            let Ok(Value::Array(rows)) = self.call(&t.node_id, method::STATUS, None, body).await else {
                continue;
            };
            for rec in mine {
                let state = self.lifecycle_of(&rec.instance_id);
                if matches!(state, Some(LifecycleState::Rescheduled | LifecycleState::Lost)) {
                    continue; // handled by the rejoin path
                }
                let row = rows
                    .iter()
                    .find(|r| r["instance_id"].as_str() == Some(rec.instance_id.as_str()));
                match row {
                    None => self.missing_on_node(rec, &rows, state, out).await,
                    Some(r) if r["lease_stopped"] == true => {
                        // The node stopped it because it heard no controller;
                        // one is here now. Start it (never mirror this as an
                        // operator stop).
                        match self.instance(method::START, &rec.instance_id).await {
                            Ok(_) => {
                                self.set_state(rec, LifecycleState::Running, "restarted after a lease stop", true);
                                out.push(event(rec, "recovered", "restarted after the node's lease stopped it"));
                            }
                            Err(e) => out.push(event(rec, "alert", format!("lease-stopped instance not restarted: {e}"))),
                        }
                    }
                    Some(r) => {
                        let to = serde_json::from_value::<LifecycleState>(r["lifecycle"].clone()).ok();
                        if let Some(to) = to
                            && Some(to) != state
                        {
                            self.set_state(rec, to, "reported by its node", true);
                        }
                    }
                }
            }
        }
    }

    async fn missing_on_node(
        &self,
        rec: &PlacementRecord,
        rows: &[Value],
        state: Option<LifecycleState>,
        out: &mut Vec<LifecycleEvent>,
    ) {
        let told = rows
            .iter()
            .find(|r| r["departed_instance"].as_str() == Some(rec.instance_id.as_str()))
            .and_then(|r| r["reason"].as_str());
        let was_live = state.is_some_and(|s| s.is_live() || s == LifecycleState::Loaded);
        if told.is_none() && was_live {
            self.set_state(rec, LifecycleState::Lost, "instance missing though its node is up (restart?)", false);
            out.push(event(rec, "lost", "the node answers but no longer holds it"));
            self.reschedule(rec, false, out).await;
            return;
        }
        let why = told.unwrap_or("not running when it went");
        self.set_state(rec, LifecycleState::Unloaded, &format!("no longer on its node: {why}"), false);
        self.forget_instances(std::slice::from_ref(&rec.instance_id));
        out.push(event(rec, "gone", format!("the node no longer holds it ({why})")));
    }

    /// An old record whose node answers again: unload what is left there,
    /// backing off after a failure and chaining each distinct failure once.
    async fn unload_orphan(&self, rec: &PlacementRecord, out: &mut Vec<LifecycleEvent>) {
        let now = now_ms();
        let Some(c) = self.life_of(&rec.instance_id) else { return };
        if now < c.orphan_next_ms {
            return;
        }
        let failure = match self.instance(method::UNLOAD, &rec.instance_id).await {
            Ok(_) => {
                out.push(event(rec, "orphan_unloaded", "node returned; the replaced copy was unloaded"));
                return;
            }
            Err(PlaneError::Call(CallFailure::Refused(r))) if r.code == RefusalCode::UnknownInstance => {
                // The node restarted and no longer holds it.
                self.drop_life(&rec.instance_id);
                self.forget_instances(std::slice::from_ref(&rec.instance_id));
                out.push(event(rec, "orphan_unloaded", "node returned without the replaced copy"));
                return;
            }
            Err(e) => e.to_string(),
        };
        let attempts = c.orphan_attempts + 1;
        let next = now + backoff_ms(self.cfg.retry_backoff.as_millis() as u64, attempts);
        let fresh = c.orphan_error.as_deref() != Some(failure.as_str());
        self.update_life(&rec.instance_id, |l| {
            l.orphan_attempts = attempts;
            l.orphan_next_ms = next;
            l.orphan_error = Some(failure.clone());
        });
        if fresh {
            self.chain_event(
                chain::EVENT_KIND_WORKLOAD_LIFECYCLE,
                json!({ "phase": "alert", "instance_id": rec.instance_id, "node": rec.node_id,
                        "workload": rec.workload, "attempt": attempts,
                        "reason": format!("replaced copy not unloaded on the returned node: {failure}") }),
            );
            out.push(event(rec, "orphan_unload_failed", failure));
        }
    }

    /// A `Lost` instance whose node is back: adopt it if it is still there
    /// (restarting it when the node's lease stopped it), otherwise place it
    /// again.
    async fn rejoined(&self, rec: &PlacementRecord, out: &mut Vec<LifecycleEvent>) {
        let body = json!({ "include_departed": true });
        let Ok(Value::Array(rows)) = self.call(&rec.node_id, method::STATUS, None, body).await else {
            return;
        };
        let row = rows
            .iter()
            .find(|r| r["instance_id"].as_str() == Some(rec.instance_id.as_str()));
        let Some(row) = row else {
            // Taken down on purpose while we could not see it (an unload
            // during a partition): not undone.
            if let Some(why) = rows
                .iter()
                .find(|r| r["departed_instance"].as_str() == Some(rec.instance_id.as_str()))
                .and_then(|r| r["reason"].as_str())
            {
                self.set_state(rec, LifecycleState::Unloaded, &format!("taken down while unreachable: {why}"), true);
                self.forget_instances(std::slice::from_ref(&rec.instance_id));
                out.push(event(rec, "gone", format!("the node took it down while unreachable ({why})")));
                return;
            }
            return self.reschedule(rec, false, out).await;
        };
        if row["lease_stopped"] == true
            && let Err(e) = self.instance(method::START, &rec.instance_id).await
        {
            // Retried next tick; the instance stays Lost until it runs.
            out.push(event(rec, "alert", format!("lease-stopped instance not restarted: {e}")));
            return;
        }
        self.set_state(rec, LifecycleState::Running, "node rejoined with the instance", false);
        self.update_life(&rec.instance_id, |l| {
            l.last_error = None;
            l.attempts = 0;
        });
        out.push(event(rec, "recovered", "node rejoined; the instance is still there"));
    }

    /// Run [`Self::lifecycle_tick`] every `period` for as long as the runtime lives.
    pub fn spawn_lifecycle(
        self: &std::sync::Arc<Self>,
        period: std::time::Duration,
    ) -> tokio::task::JoinHandle<()> {
        let me = std::sync::Arc::clone(self);
        tokio::spawn(async move {
            let mut t = tokio::time::interval(period);
            t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            t.tick().await;
            loop {
                t.tick().await;
                let acted = me.lifecycle_tick().await;
                if !acted.is_empty() {
                    tracing::warn!(n = acted.len(), "placement lifecycle");
                }
            }
        })
    }
}
