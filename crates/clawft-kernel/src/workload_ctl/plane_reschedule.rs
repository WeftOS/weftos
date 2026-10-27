//! Placing a lost instance again (ADR-099 section 7): after its node died
//! (`node_dead`), or after a node that is up came back without it (a
//! reboot). Always through [`PlacementControlPlane::place_timed`], so the
//! same gate and engine decide: only `Alive`, verified, non-demoted nodes
//! qualify, and a revoked package is refused.
//!
//! A node derives an instance's id from its order (package, config, project,
//! controller), so on one node two records with the same order name one
//! instance. A `Lost` record whose order another record on its node now holds
//! is superseded and dropped, never retried: retrying it would fail with
//! "already loaded" until that instance is unloaded, and then put the
//! unloaded instance back (card 82a59d97).

use serde_json::json;

use super::lifecycle::LifecycleState;
use super::plane::{PlacementControlPlane, PlacementRecord, now_ms};
use super::plane_place::PlaceOrder;
use super::plane_lifecycle::{ControllerLife, LifecycleEvent, backoff_ms, event};
use crate::chain;

/// Whether `a` and `b` would place the same instance on one node: what the
/// node derives the instance id from (pin, affinity and `start` do not count).
fn same_instance(a: &PlaceOrder, b: &PlaceOrder) -> bool {
    a.package_dir == b.package_dir && a.config == b.config && a.project_id == b.project_id
}

impl PlacementControlPlane {
    /// Drop every `Lost` record on `node` other than `keep` whose order would
    /// place the same instance as `order`, chaining each as `superseded`.
    /// Called when an instance with that order is placed (the new record
    /// replaces the lost one) and when one is unloaded (the operator took
    /// that instance down; a lost record must not bring it back).
    pub(super) fn supersede_lost(&self, node: &str, keep: &str, order: &PlaceOrder, why: &str) -> usize {
        let stale: Vec<PlacementRecord> = self
            .placements()
            .into_iter()
            .filter(|r| r.node_id == node && r.instance_id != keep)
            .filter(|r| {
                self.life_of(&r.instance_id).is_some_and(|c| {
                    c.life.state == LifecycleState::Lost && c.order.as_ref().is_some_and(|o| same_instance(o, order))
                })
            })
            .collect();
        for rec in &stale {
            self.chain_event(
                chain::EVENT_KIND_WORKLOAD_LIFECYCLE,
                json!({ "phase": "superseded", "instance_id": rec.instance_id, "node": rec.node_id,
                        "workload": rec.workload, "by": keep, "reason": why }),
            );
        }
        let ids: Vec<String> = stale.into_iter().map(|r| r.instance_id).collect();
        self.forget_instances(&ids);
        ids.len()
    }

    /// Another record on `rec`'s node, not itself lost or replaced, holding
    /// the same order: that instance is the one `rec` would place again.
    fn holder_of(&self, rec: &PlacementRecord, order: &PlaceOrder) -> Option<String> {
        self.placements()
            .into_iter()
            .filter(|r| r.node_id == rec.node_id && r.instance_id != rec.instance_id)
            .find(|r| {
                self.life_of(&r.instance_id).is_some_and(|c| {
                    !matches!(c.life.state, LifecycleState::Lost | LifecycleState::Rescheduled)
                        && c.order.as_ref().is_some_and(|o| same_instance(o, order))
                })
            })
            .map(|r| r.instance_id)
    }

    /// What stops `rec` being placed again at all (`hard`), and what only
    /// stops it being *moved* (`soft`: pinned, or non-migratable by the
    /// operator, its kind, or because it needs attached hardware).
    fn blockers(&self, rec: &PlacementRecord, c: &ControllerLife) -> (Option<String>, Option<String>) {
        let kind_ok = self.kinds.get(&rec.kind).is_none_or(|k| k.migratable());
        let hard = match &c.order {
            _ if !c.was_running => Some("it was not running when it was lost".to_string()),
            None => Some("no stored placement order (adopted instance)".to_string()),
            Some(o) if !o.package_dir.is_dir() => Some("its package directory is gone".to_string()),
            Some(_) if c.reschedules >= c.policy.max_reschedules => {
                Some(format!("already rescheduled {} times", c.reschedules))
            }
            Some(_) => None,
        };
        let soft = match &c.order {
            Some(o) if o.pin.is_some() => Some(format!("pinned to {}", o.pin.as_deref().unwrap_or(""))),
            _ if !c.policy.migratable => {
                Some("non-migratable (operator, or the workload needs attached hardware)".to_string())
            }
            _ if !kind_ok => Some(format!("kind {} is non-migratable", rec.kind)),
            _ => None,
        };
        (hard, soft)
    }

    fn alert_once(&self, rec: &PlacementRecord, why: String, out: &mut Vec<LifecycleEvent>) {
        let seen = self.life_of(&rec.instance_id).and_then(|c| c.last_error);
        if seen.as_deref() == Some(why.as_str()) {
            return;
        }
        self.chain_event(
            chain::EVENT_KIND_WORKLOAD_LIFECYCLE,
            json!({ "phase": "alert", "instance_id": rec.instance_id, "node": rec.node_id,
                    "workload": rec.workload, "reason": format!("lost, not placed again: {why}") }),
        );
        self.update_life(&rec.instance_id, |c| c.last_error = Some(why.clone()));
        out.push(event(rec, "alert", why));
    }

    /// Place a `Lost` instance again. `node_dead`: its node is gone (the
    /// replacement avoids it and the old record waits as `Rescheduled`).
    /// Otherwise the node is up but lost the instance, and the replacement
    /// may land on the same node; an instance that must not move is then
    /// pinned to it.
    pub(super) async fn reschedule(&self, rec: &PlacementRecord, node_dead: bool, out: &mut Vec<LifecycleEvent>) {
        let Some(c) = self.life_of(&rec.instance_id) else { return };
        // Its order already runs on this node under another record (placed
        // again by hand, say, from a package rebuilt in place): superseded.
        if let Some(holder) = c.order.as_ref().and_then(|o| self.holder_of(rec, o)) {
            self.chain_event(
                chain::EVENT_KIND_WORKLOAD_LIFECYCLE,
                json!({ "phase": "superseded", "instance_id": rec.instance_id, "node": rec.node_id,
                        "workload": rec.workload, "by": holder,
                        "reason": "its order is placed on this node as another instance" }),
            );
            self.forget_instances(std::slice::from_ref(&rec.instance_id));
            out.push(event(rec, "superseded", format!("its order runs here as {holder}")));
            return;
        }
        let now = now_ms();
        if now < c.next_attempt_ms {
            return;
        }
        let (hard, soft) = self.blockers(rec, &c);
        if let Some(why) = hard {
            return self.alert_once(rec, why, out);
        }
        if let (Some(why), true) = (&soft, node_dead) {
            return self.alert_once(rec, why.clone(), out);
        }
        let Some(mut order) = c.order.clone() else { return };
        if node_dead {
            order.avoid.push(rec.node_id.clone());
        } else if soft.is_some() {
            order.pin = Some(rec.node_id.clone());
        }
        order.dry_run = false;
        let attempt = self.place_timed(&order, Some(self.cfg.reschedule_timeout)).await;
        let new = match attempt {
            Ok(r) if r.placed.is_some() => (r.placed.clone().expect("checked"), r.decision_id),
            Ok(r) => {
                let why = r
                    .attempts
                    .last()
                    .map(|a| format!("{}: {}", a.outcome, a.reason.clone().unwrap_or_default()))
                    .unwrap_or_else(|| "no node accepted it".into());
                return self.attempt_failed(rec, &c, why, out);
            }
            Err(e) => return self.attempt_failed(rec, &c, e.to_string(), out),
        };
        let (new, decision_id) = new;
        // Recorded and saved before anything else, so a crash right after
        // the placement cannot lose the order and place it a third time.
        if let Ok(mut l) = self.lives.lock() {
            let mut fresh = ControllerLife::new(
                if order.start { LifecycleState::Running } else { LifecycleState::Loaded },
                c.policy,
                c.order.clone(),
                c.reschedules + 1,
            );
            fresh.was_running = true;
            l.insert(new.instance_id.clone(), fresh);
        }
        self.persist();
        let phase = if node_dead {
            self.set_state(rec, LifecycleState::Rescheduled, &format!("replaced on {}", new.node_id), false);
            "rescheduled"
        } else {
            // The node no longer has it: nothing to unload later. (A node
            // derives instance ids from the workload, so the replacement may
            // carry the very id of the lost copy; its record is then the
            // one just written.)
            if new.instance_id != rec.instance_id {
                self.forget_instances(std::slice::from_ref(&rec.instance_id));
            } else {
                debug_assert_eq!(new.node_id, rec.node_id, "same instance id on another node");
            }
            "replaced_after_loss"
        };
        self.chain_event(
            chain::EVENT_KIND_WORKLOAD_MIGRATE,
            json!({ "phase": phase, "reason": if node_dead { "node_dead" } else { "instance_lost" },
                    "workload": rec.workload, "from_node": rec.node_id, "from_instance": rec.instance_id,
                    "to_node": new.node_id, "to_instance": new.instance_id, "decision_id": decision_id }),
        );
        let action = if node_dead { "rescheduled" } else { "replaced" };
        out.push(event(rec, action, format!("now {} on {}", new.instance_id, new.node_id)));
    }

    /// A placement attempt found no node: back off, count it toward the
    /// cap, chain a distinct reason once, and say so when giving up.
    fn attempt_failed(&self, rec: &PlacementRecord, c: &ControllerLife, why: String, out: &mut Vec<LifecycleEvent>) {
        let attempts = c.attempts + 1;
        let next = now_ms() + backoff_ms(self.cfg.retry_backoff.as_millis() as u64, attempts);
        let fresh = c.last_error.as_deref() != Some(why.as_str());
        self.update_life(&rec.instance_id, |l| {
            l.attempts = attempts;
            l.next_attempt_ms = next;
            l.last_error = Some(why.clone());
        });
        if fresh {
            self.chain_event(
                chain::EVENT_KIND_WORKLOAD_REFUSE,
                json!({ "phase": "reschedule", "instance_id": rec.instance_id, "node": rec.node_id,
                        "workload": rec.workload, "reason": why, "attempt": attempts,
                        "next": "retry with backoff" }),
            );
            out.push(event(rec, "reschedule_failed", why));
        }
        if attempts == c.policy.max_reschedules {
            self.chain_event(
                chain::EVENT_KIND_WORKLOAD_LIFECYCLE,
                json!({ "phase": "alert", "instance_id": rec.instance_id, "node": rec.node_id,
                        "workload": rec.workload,
                        "reason": format!("no node after {attempts} attempts; still retrying with backoff (at most every 5 minutes)") }),
            );
            out.push(event(rec, "alert", format!("no node after {attempts} attempts; still retrying")));
        }
    }
}
