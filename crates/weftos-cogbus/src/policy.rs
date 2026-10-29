//! What the daemon plugs into the broker (ADR-122 sections 5 and 11, phase
//! B3): a governance gate on top of the manifest rules, cross-project
//! grants, and the sink that keeps the activity log.
//!
//! The broker's own manifest rules (B1) always run first; a [`BusPolicy`]
//! can only refuse more. [`AllowAll`] (the default) refuses nothing and
//! grants nothing, so without a policy the broker behaves exactly as B1.
//! The daemon implements both traits over the kernel's governance rules and
//! the node activity log; this crate does not depend on the kernel.

use std::sync::Arc;

use serde::Serialize;

/// An action governance decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum BusAction {
    /// One entry of a `declare` (topic is the kind).
    Declare,
    /// The first publish of an instance on a topic.
    Publish,
    /// A subscribe.
    Subscribe,
    /// An unsubscribe (recorded, never refused: it only narrows access).
    Unsubscribe,
    /// Becoming the provider of a service (`svc.*` topic).
    Serve,
    /// One call of a service.
    Call,
    /// Starting a blob upload (B5).
    BlobPut,
    /// Starting a blob read (B5).
    BlobGet,
    /// Asking the workload runtime to stop an instance the broker stopped
    /// on the bus (repeated refusals or a publish rate overrun).
    Stop,
    /// Another node subscribing to a topic of this one (B6); the principal
    /// is `mesh:<node id>`.
    MeshSubscribe,
    /// Another node fetching a blob a served reading handed it (B6e).
    MeshFetch,
}

impl BusAction {
    /// The governance action name (`cogbus.declare`, ...).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Declare => "cogbus.declare",
            Self::Publish => "cogbus.publish",
            Self::Subscribe => "cogbus.subscribe",
            Self::Unsubscribe => "cogbus.unsubscribe",
            Self::Serve => "cogbus.serve",
            Self::Call => "cogbus.call",
            Self::BlobPut => "cogbus.blob.put",
            Self::BlobGet => "cogbus.blob.get",
            Self::Stop => "cogbus.stop",
            Self::MeshSubscribe => "cogbus.mesh.subscribe",
            Self::MeshFetch => "cogbus.mesh.fetch",
        }
    }
}

/// One decision request: the instance is the principal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BusRequest<'a> {
    /// What is asked.
    pub action: BusAction,
    /// Instance id (the principal).
    pub instance_id: &'a str,
    /// Its cog id.
    pub cog_id: &'a str,
    /// Its project; `None` is the node's own bucket.
    pub project_id: Option<&'a str>,
    /// `<kind>/<source_id>`, `<kind>/*`, or the bare kind for a declare.
    pub topic: &'a str,
    /// Schema id.
    pub schema: &'a str,
    /// Project of the live topic, when it is known and not the instance's.
    pub topic_project: Option<&'a str>,
    /// Who an in-process session acts for (`local:<addr>`, `mesh:<node id>`).
    /// Only the broker's internal entry points set it (never the wire); the
    /// broker's gate then asks governance with this as the agent and the
    /// instance in [`via`](Self::via).
    pub on_behalf: Option<&'a str>,
    /// Set by the broker's gate on the request governance sees when
    /// `on_behalf` was given: the instance that carried the call, whose id
    /// is no longer the agent (`instance_id` is the real caller then).
    pub via: Option<&'a str>,
}

impl<'a> BusRequest<'a> {
    /// A request by `instance_id` with no topic yet.
    pub fn new(
        action: BusAction,
        instance_id: &'a str,
        cog_id: &'a str,
        project_id: Option<&'a str>,
    ) -> Self {
        Self {
            action,
            instance_id,
            cog_id,
            project_id,
            topic: "-",
            schema: "-",
            topic_project: None,
            on_behalf: None,
            via: None,
        }
    }

    /// The same request made on behalf of `caller` (in-process sessions
    /// only; see [`on_behalf`](Self::on_behalf)).
    pub fn for_caller(self, caller: &'a str) -> Self {
        Self { on_behalf: Some(caller), ..self }
    }

    /// The same request on `topic` with `schema`.
    pub fn on(self, topic: &'a str, schema: &'a str) -> Self {
        Self {
            topic,
            schema,
            ..self
        }
    }
}

/// Another project's topics a subscription may also read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    /// The topic's project.
    pub project: String,
    /// One source of the subscribed kind, or `None` for every source.
    pub source: Option<String>,
}

/// Governance on top of the manifest rules.
pub trait BusPolicy: Send + Sync {
    /// `Err(reason)` refuses the request with [`ErrorCode::Denied`](crate::ErrorCode::Denied).
    fn decide(&self, req: &BusRequest<'_>) -> Result<(), String>;

    /// Foreign-project topics of `req.topic`'s kind that a subscribe may
    /// read. Asked once per subscribe; a grant added later applies to new
    /// subscriptions.
    fn grants(&self, req: &BusRequest<'_>) -> Vec<Grant>;
}

/// The default policy: refuse nothing, grant nothing (B1 behaviour).
#[derive(Debug, Clone, Copy, Default)]
pub struct AllowAll;

impl BusPolicy for AllowAll {
    fn decide(&self, _: &BusRequest<'_>) -> Result<(), String> {
        Ok(())
    }

    fn grants(&self, _: &BusRequest<'_>) -> Vec<Grant> {
        Vec::new()
    }
}

/// What happened, for the activity log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    /// Allowed.
    Ok,
    /// Refused; `suppressed` identical refusals were not logged since the
    /// last logged one.
    Refused {
        /// Error code (snake case, as on the wire).
        code: String,
        /// Reason sent to the cog.
        reason: String,
        /// Identical refusals folded into this record.
        suppressed: u64,
    },
    /// A `block` stream reader held its writer past the deadline and now
    /// reads under another policy (B2).
    Demoted {
        /// The policy from now on.
        policy: String,
        /// Why.
        reason: String,
    },
    /// Too many refusals: the instance was stopped on the bus.
    Stopped {
        /// Refusals counted in the window.
        refusals: u32,
    },
}

/// One activity record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BusActivity {
    /// Unix milliseconds.
    pub ts_ms: u64,
    /// `cogbus.declare`, `cogbus.hello`, ...
    pub action: String,
    /// Instance id (`-` when the token was unknown).
    pub instance: String,
    /// Instance's project, when known.
    pub project: Option<String>,
    /// Topic, kind or `-`.
    pub topic: String,
    /// The instance that carried the action when `instance` is the real
    /// caller it acted for (`infer-proxy` for a proxied inference call).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
    /// What happened.
    #[serde(flatten)]
    pub outcome: Outcome,
}

/// A topic that has just come alive (its first reading, or its ring opened).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopicInfo {
    /// `<kind>/<source_id>`.
    pub topic: String,
    /// Topic kind.
    pub kind: String,
    /// Source id.
    pub source_id: String,
    /// Schema id.
    pub schema: String,
    /// `reading` or `stream`.
    pub class: clawft_types::cog_topics::TopicClass,
    /// The instance writing it.
    pub instance: String,
    /// Its cog id.
    pub cog_id: String,
    /// Its project.
    pub project_id: Option<String>,
}

/// Where activity goes.
pub trait BusSink: Send + Sync {
    /// Record one entry. Must not call back into the broker.
    fn record(&self, a: &BusActivity);

    /// The instance crossed the repeated-refusal limit and the broker
    /// stopped it on the bus (connections closed, token refused until the
    /// workload runtime registers it again). The hook for stopping the cog
    /// itself; called with no broker lock held.
    fn stopped(&self, _instance_id: &str, _refusals: u32) {}

    /// The broker, having stopped `instance_id` on the bus, asks the
    /// workload runtime to stop the cog itself, and governance
    /// (`cogbus.stop`) agreed. `reason` says why (repeated refusals, with
    /// the last code). Called with no broker lock held; it must not block.
    fn stop_requested(&self, _instance_id: &str, _reason: &str) {}

    /// A topic came alive: the daemon registers it as an ADR-120 exposure
    /// (audience `local`, so nothing is served). Called at a topic's first
    /// reading and at each stream open (registering is idempotent), with no
    /// broker lock held; it must not block.
    fn topic_opened(&self, _topic: &TopicInfo) {}
}

/// Logs activity through `tracing` only.
#[derive(Debug, Clone, Copy, Default)]
pub struct TracingSink;

impl BusSink for TracingSink {
    fn record(&self, a: &BusActivity) {
        match &a.outcome {
            Outcome::Ok => {
                tracing::debug!(target: "cogbus", action = %a.action, instance = %a.instance, topic = %a.topic, "cogbus")
            }
            o => {
                tracing::warn!(target: "cogbus", action = %a.action, instance = %a.instance, topic = %a.topic, outcome = ?o, "cogbus refusal")
            }
        }
    }
}

/// Repeated-refusal and log rate-limit settings (ADR-122 section 11).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefusalLimits {
    /// Identical refusals (instance, code, topic) inside this window are
    /// counted, not logged, until the window has passed.
    pub dedupe_ms: u64,
    /// Refusals of one instance within `window_ms` that stop it.
    pub stop_after: u32,
    /// The repeated-refusal window.
    pub window_ms: u64,
}

impl Default for RefusalLimits {
    fn default() -> Self {
        Self {
            dedupe_ms: 10_000,
            stop_after: 20,
            window_ms: 60_000,
        }
    }
}

/// The policy, sink and limits a broker runs with.
#[derive(Clone)]
pub struct BusHooks {
    /// Governance and grants.
    pub policy: Arc<dyn BusPolicy>,
    /// Activity log.
    pub sink: Arc<dyn BusSink>,
    /// Rate limit and repeated-refusal stop.
    pub limits: RefusalLimits,
}

impl Default for BusHooks {
    fn default() -> Self {
        Self {
            policy: Arc::new(AllowAll),
            sink: Arc::new(TracingSink),
            limits: RefusalLimits::default(),
        }
    }
}

impl std::fmt::Debug for BusHooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BusHooks")
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}
