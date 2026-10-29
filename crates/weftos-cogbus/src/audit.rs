//! Governance, activity and repeated refusals for the broker (ADR-122
//! sections 5 and 11, phase B3).
//!
//! - The broker asks [`BusPolicy::decide`](crate::policy::BusPolicy::decide)
//!   at declare, at an instance's first publish on a topic and at subscribe,
//!   after its manifest rules pass.
//! - Allowed declares, subscribes and unsubscribes go to the sink as they
//!   happen. Refusals go when the connection records them
//!   ([`Broker::record_refusal`]), so no broker lock is held: identical
//!   refusals (instance, code, topic) are logged once per
//!   [`RefusalLimits::dedupe_ms`](crate::policy::RefusalLimits) with the
//!   count folded in, and an instance refused
//!   [`stop_after`](crate::policy::RefusalLimits) times within the window is
//!   stopped on the bus: revoked (connections closed, token refused) until
//!   the workload runtime registers it again, and the sink's
//!   [`stopped`](crate::policy::BusSink::stopped) hook is called.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use crate::broker::{Broker, InstanceSpec, Session};
use crate::policy::{BusAction, BusActivity, BusHooks, BusRequest, Grant, Outcome, RefusalLimits};
use crate::proto::{ErrorCode, InputSpec, OutputSpec};
use crate::types::{BusError, refuse};

/// Logged refusals kept for `cogbus list`.
pub const RECENT_REFUSALS: usize = 32;
/// Dedupe keys kept before old ones are pruned.
const MAX_KEYS: usize = 4096;

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn code_name_of(e: &BusError) -> String {
    code_name(e.code)
}

/// Snake-case wire name of a code.
pub fn code_name(c: ErrorCode) -> String {
    serde_json::to_value(c)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_else(|| format!("{c:?}"))
}

struct Seen {
    logged_ms: u64,
    suppressed: u64,
}

/// What to do with one refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Verdict {
    /// Log it, with this many identical refusals folded in.
    pub log: Option<u64>,
    /// Stop the instance: it was refused this many times in the window.
    pub stop: Option<u32>,
}

/// Rate limit and repeated-refusal counting, by wall clock in ms.
#[derive(Default)]
pub(crate) struct Tracker {
    seen: HashMap<(String, String, String), Seen>,
    strikes: HashMap<String, VecDeque<u64>>,
    recent: VecDeque<BusActivity>,
}

impl Tracker {
    pub(crate) fn note(
        &mut self,
        lim: &RefusalLimits,
        instance: &str,
        code: &str,
        topic: &str,
        now: u64,
    ) -> Verdict {
        if self.seen.len() >= MAX_KEYS {
            self.seen
                .retain(|_, s| now.saturating_sub(s.logged_ms) < lim.dedupe_ms);
            self.strikes.retain(|_, q| {
                q.back()
                    .is_some_and(|t| now.saturating_sub(*t) <= lim.window_ms)
            });
        }
        let key = (instance.to_string(), code.to_string(), topic.to_string());
        let log = match self.seen.get_mut(&key) {
            Some(s) if now.saturating_sub(s.logged_ms) < lim.dedupe_ms => {
                s.suppressed += 1;
                None
            }
            Some(s) => {
                let folded = s.suppressed;
                *s = Seen {
                    logged_ms: now,
                    suppressed: 0,
                };
                Some(folded)
            }
            None => {
                self.seen.insert(
                    key,
                    Seen {
                        logged_ms: now,
                        suppressed: 0,
                    },
                );
                Some(0)
            }
        };
        // An unknown token, or an internal publisher (`@...`), has no
        // instance to stop.
        let mut stop = None;
        if instance != "-" && !instance.starts_with('@') && lim.stop_after > 0 {
            let q = self.strikes.entry(instance.to_string()).or_default();
            q.push_back(now);
            while q
                .front()
                .is_some_and(|t| now.saturating_sub(*t) > lim.window_ms)
            {
                q.pop_front();
            }
            if q.len() >= lim.stop_after as usize {
                stop = Some(q.len() as u32);
                self.strikes.remove(instance);
            }
        }
        Verdict { log, stop }
    }

    fn remember(&mut self, a: BusActivity) {
        if self.recent.len() >= RECENT_REFUSALS {
            self.recent.pop_front();
        }
        self.recent.push_back(a);
    }
}

/// The broker's hooks and refusal state.
#[derive(Default)]
pub(crate) struct Ext {
    pub hooks: BusHooks,
    tracker: Mutex<Tracker>,
}

impl Ext {
    fn tracker(&self) -> std::sync::MutexGuard<'_, Tracker> {
        self.tracker.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Governance for `req`; a denial comes back as a marked refusal.
    ///
    /// A request made `on_behalf` of a caller is put to governance with the
    /// caller as the agent and the carrying instance in `via`, so a rule or
    /// a `cogbus-policy.json` deny that names the caller applies. The
    /// refusal is still marked against the instance that made the call.
    /// The carrying instance is asked as well, as itself: a deny or rule
    /// that names it refuses everything it carries, not only its own calls.
    pub(crate) fn gate(&self, req: &BusRequest<'_>) -> Result<(), BusError> {
        let decided = match req.on_behalf {
            Some(caller) => {
                let for_caller = BusRequest {
                    instance_id: caller,
                    via: Some(req.instance_id),
                    on_behalf: None,
                    ..*req
                };
                self.hooks
                    .policy
                    .decide(&for_caller)
                    .and_then(|()| self.hooks.policy.decide(&BusRequest { on_behalf: None, ..*req }))
            }
            None => self.hooks.policy.decide(req),
        };
        decided.map_err(|reason| {
            let short = req.action.as_str().trim_start_matches("cogbus.");
            let who = req.on_behalf.map(|c| format!(" for {c}")).unwrap_or_default();
            let e = BusError::new(
                ErrorCode::Denied,
                format!("{} denied by governance{who}: {reason}", req.action.as_str()),
            );
            refuse(req.instance_id, short, req.topic, e)
        })
    }

    /// Record an allowed action.
    pub(crate) fn allowed(
        &self,
        action: BusAction,
        instance: &str,
        project: Option<&str>,
        topic: &str,
    ) {
        self.allowed_via(action, instance, None, project, topic);
    }

    /// Record an allowed action of `instance`, carried by `via`.
    pub(crate) fn allowed_via(
        &self,
        action: BusAction,
        instance: &str,
        via: Option<&str>,
        project: Option<&str>,
        topic: &str,
    ) {
        self.hooks.sink.record(&BusActivity {
            ts_ms: now_ms(),
            action: action.as_str().into(),
            instance: instance.into(),
            project: project.map(String::from),
            topic: topic.into(),
            via: via.map(String::from),
            outcome: Outcome::Ok,
        });
    }
}

impl Ext {
    /// Governance for each entry of a declare, each allowed one recorded.
    pub(crate) fn declare(
        &self,
        spec: &InstanceSpec,
        outputs: &[OutputSpec],
        inputs: &[InputSpec],
    ) -> Result<(), BusError> {
        let project = spec.project_id.as_deref();
        let base = BusRequest::new(BusAction::Declare, &spec.instance_id, &spec.cog_id, project);
        let outs = outputs.iter().map(|o| (&o.topic, &o.schema));
        for (topic, schema) in outs.chain(inputs.iter().map(|i| (&i.topic, &i.schema))) {
            self.gate(&base.on(topic, schema))?;
            self.allowed(BusAction::Declare, &spec.instance_id, project, topic);
        }
        Ok(())
    }

    /// Record an allowed action of session `s`.
    pub(crate) fn noted(&self, s: &Session, action: BusAction, topic: &str) {
        self.allowed(action, &s.instance_id, s.project_id.as_deref(), topic);
    }

    /// Record an allowed action of session `s` made on behalf of `caller`:
    /// the activity names the caller, with the session's instance in `via`.
    pub(crate) fn noted_for(&self, s: &Session, action: BusAction, topic: &str, caller: Option<&str>) {
        match caller {
            Some(c) => self.allowed_via(action, c, Some(&s.instance_id), s.project_id.as_deref(), topic),
            None => self.noted(s, action, topic),
        }
    }

    /// The policy's grants for a subscribe, narrowed to its `source`.
    pub(crate) fn grants(&self, req: &BusRequest<'_>, source: Option<&str>) -> Vec<Grant> {
        let mut g = self.hooks.policy.grants(req);
        g.retain(|g| source.is_none() || g.source.is_none() || g.source.as_deref() == source);
        g
    }
}

/// A caller named by an in-process session: `local:<addr>` or
/// `mesh:<node id>`, printable and short, so it is a sane governance agent id.
pub(crate) fn check_caller(c: &str) -> Result<(), BusError> {
    let ok = (c.starts_with("local:") || c.starts_with("mesh:"))
        && c.len() <= 256
        && c.chars().all(|ch| !ch.is_control() && !ch.is_whitespace());
    if ok {
        Ok(())
    } else {
        Err(BusError::new(ErrorCode::BadRequest, "on_behalf is local:<addr> or mesh:<node id>"))
    }
}

/// Does a grant in `also` cover `source` of a topic in `project`?
pub(crate) fn granted(also: &[Grant], project: &Option<String>, source: &str) -> bool {
    also.iter().any(|g| {
        project.as_deref() == Some(g.project.as_str())
            && g.source.as_deref().is_none_or(|s| s == source)
    })
}

impl Broker {
    /// Run with `hooks` instead of the defaults (allow all, `tracing` only).
    pub fn with_hooks(mut self, hooks: BusHooks) -> Self {
        self.ext = Ext {
            hooks,
            tracker: Mutex::default(),
        };
        self
    }

    /// Record a refusal the broker handed back: the activity log (rate
    /// limited) and the repeated-refusal count. `fallback_instance` names
    /// the instance for an error the broker did not mark (a bad frame on an
    /// authenticated connection). Returns true if this refusal stopped the
    /// instance. Call it with no broker method in progress on this thread.
    pub fn record_refusal(&self, e: &BusError, fallback_instance: &str) -> bool {
        let (instance, action, topic) = match &e.ctx {
            Some(c) => (c.instance.as_str(), c.action.as_str(), c.topic.as_str()),
            None => (fallback_instance, "frame", "-"),
        };
        let code = code_name(e.code);
        let now = now_ms();
        let spec = self.instance(instance);
        let project = spec.as_ref().and_then(|s| s.project_id.clone());
        let cog = spec.map(|s| s.cog_id).unwrap_or_default();
        let lim = self.ext.hooks.limits;
        let v = self.ext.tracker().note(&lim, instance, &code, topic, now);
        if let Some(suppressed) = v.log {
            let a = BusActivity {
                ts_ms: now,
                action: format!("cogbus.{action}"),
                instance: instance.into(),
                project: project.clone(),
                topic: topic.into(),
                via: None,
                outcome: Outcome::Refused {
                    code,
                    reason: e.message.clone(),
                    suppressed,
                },
            };
            self.ext.hooks.sink.record(&a);
            self.ext.tracker().remember(a);
        }
        let Some(refusals) = v.stop else {
            return false;
        };
        if !self.revoke(instance) {
            return false;
        }
        let a = BusActivity {
            ts_ms: now,
            action: "cogbus.stop".into(),
            instance: instance.into(),
            project: project.clone(),
            topic: "-".into(),
            via: None,
            outcome: Outcome::Stopped { refusals },
        };
        self.ext.hooks.sink.record(&a);
        self.ext.tracker().remember(a);
        self.ext.hooks.sink.stopped(instance, refusals);
        self.ask_stop(instance, &cog, project.as_deref(), refusals, &code_name_of(e));
        true
    }

    /// The instance was stopped on the bus: governance decides whether the
    /// workload is stopped too (`cogbus.stop`), and the sink does it. A
    /// denial is in the activity log; the cog keeps running without the bus.
    fn ask_stop(&self, instance: &str, cog: &str, project: Option<&str>, refusals: u32, last: &str) {
        let req = BusRequest::new(BusAction::Stop, instance, cog, project);
        let now = now_ms();
        match self.ext.hooks.policy.decide(&req) {
            Ok(()) => {
                let reason = format!("{refusals} refusals in {} s, last {last}", self.ext.hooks.limits.window_ms / 1000);
                self.ext.hooks.sink.stop_requested(instance, &reason);
            }
            Err(why) => {
                let a = BusActivity {
                    ts_ms: now,
                    action: BusAction::Stop.as_str().into(),
                    instance: instance.into(),
                    project: project.map(String::from),
                    topic: "-".into(),
                    via: None,
                    outcome: Outcome::Refused {
                        code: "denied".into(),
                        reason: format!("cogbus.stop denied by governance: {why}"),
                        suppressed: 0,
                    },
                };
                self.ext.hooks.sink.record(&a);
                self.ext.tracker().remember(a);
            }
        }
    }

    /// The last [`RECENT_REFUSALS`] logged refusals and stops, oldest first.
    pub fn recent_refusals(&self) -> Vec<BusActivity> {
        self.ext.tracker().recent.iter().cloned().collect()
    }
}
