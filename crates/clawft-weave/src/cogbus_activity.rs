//! The cogbus activity log (ADR-122 section 5, ADR-120 section 5): every
//! declare, subscribe, unsubscribe, refusal and repeated-refusal stop, in
//! `<runtime dir>/cogbus-activity.jsonl`.
//!
//! It is the exposure activity log's format and code
//! ([`clawft_kernel::exposure::ActivityLog`]: SHA-256 hash-chained, never
//! bodies, 14 days or 50 MB), in its own file so cogbus traffic does not
//! count against exposures. Unlike the exposure log its tip is not anchored
//! on the ExoChain (ADR-122: no chain event per subscription). A record's
//! fields carry:
//!
//! | field | cogbus meaning |
//! |---|---|
//! | `exposure` | `<action>:<topic>`, e.g. `cogbus.subscribe:telemetry/*` |
//! | `caller_node` | the instance id (`-` for an unknown token) |
//! | `caller_user` | the instance's project, if any |
//! | `outcome` | `ok`, `refused:<code> [+N] <reason>` (N identical refusals folded in), `stopped:<n> refusals` |
//!
//! Byte counts and duration are 0.

use std::path::Path;
use std::sync::{Arc, OnceLock};

use clawft_kernel::exposure::registry::now_ms;
use clawft_kernel::exposure::{ActivityLog, NewActivity, Retention};
use clawft_kernel::exposure::{ExposureKind, ExposureSpec};
use weftos_cogbus::{BusActivity, BusSink, Outcome, TopicInfo};

/// Log file under the runtime dir.
pub const LOG_FILE: &str = "cogbus-activity.jsonl";

/// The exposure a topic is registered as (ADR-122 section 14, B6a): id
/// `topic.<kind>/<source_id>`, audience `local` and nothing announced, so it
/// is listed and switchable but never served until an operator widens it.
pub fn topic_exposure(t: &TopicInfo) -> ExposureSpec {
    let mut spec = ExposureSpec::new(
        &format!("topic.{}", t.topic),
        ExposureKind::Topic,
        &format!("cog:{}", t.cog_id),
        "cogbus.v1",
    );
    spec.state = clawft_kernel::exposure::ExposureState::On;
    spec
}

/// What the daemon does when the broker asks for a cog to be stopped.
pub type StopHook = Arc<dyn Fn(&str, &str) + Send + Sync>;

/// The sink the broker writes to.
pub struct CogbusActivity {
    log: ActivityLog,
    stop: OnceLock<StopHook>,
    exposures: OnceLock<Arc<clawft_kernel::exposure::ExposureRegistry>>,
}

/// `outcome` text of one record.
pub fn outcome_text(o: &Outcome) -> String {
    match o {
        Outcome::Ok => "ok".into(),
        Outcome::Refused { code, reason, suppressed: 0 } => format!("refused:{code} {reason}"),
        Outcome::Refused { code, reason, suppressed } => format!("refused:{code} +{suppressed} {reason}"),
        Outcome::Stopped { refusals } => format!("stopped:{refusals} refusals"),
        Outcome::Demoted { policy, reason } => format!("demoted:{policy} {reason}"),
    }
}

impl CogbusActivity {
    /// Open `dir`'s log. A chain that does not verify is moved aside and a
    /// fresh one started (logged), as for the exposure log.
    pub fn open(dir: &Path) -> Self {
        let path = dir.join(LOG_FILE);
        let log = ActivityLog::open(&path, Retention::default(), now_ms()).unwrap_or_else(|e| {
            tracing::error!(error = %e, "cogbus activity log unreadable; moving it aside and starting fresh");
            let _ = std::fs::rename(&path, dir.join(format!("{LOG_FILE}.broken-{}", now_ms())));
            ActivityLog::open(&path, Retention::default(), now_ms()).unwrap_or_else(|e| {
                tracing::error!(error = %e, "cogbus activity log kept in memory only");
                ActivityLog::memory(Retention::default())
            })
        });
        Self { log, stop: OnceLock::new(), exposures: OnceLock::new() }
    }

    /// In memory (tests).
    pub fn memory() -> Self {
        Self { log: ActivityLog::memory(Retention::default()), stop: OnceLock::new(), exposures: OnceLock::new() }
    }

    /// What to run for [`BusSink::stop_requested`] (first call wins). The
    /// workload host does not exist yet when the broker is built, so the
    /// daemon sets this once it does.
    pub fn set_stop_hook(&self, f: StopHook) -> bool {
        self.stop.set(f).is_ok()
    }

    /// The registry topics are registered in as exposures (first call wins);
    /// without one the daemon's global registry is used, if it is running.
    pub fn set_exposures(&self, r: Arc<clawft_kernel::exposure::ExposureRegistry>) -> bool {
        self.exposures.set(r).is_ok()
    }

    /// The log.
    pub fn log(&self) -> &ActivityLog {
        &self.log
    }
}

impl BusSink for CogbusActivity {
    fn record(&self, a: &BusActivity) {
        self.log.append(
            NewActivity {
                exposure: format!("{}:{}", a.action, a.topic),
                caller_node: a.instance.clone(),
                caller_user: a.project.clone(),
                bytes_in: 0,
                bytes_out: 0,
                duration_ms: 0,
                outcome: outcome_text(&a.outcome),
            },
            a.ts_ms,
        );
    }

    fn stopped(&self, instance_id: &str, refusals: u32) {
        tracing::warn!(target: "cogbus", instance = instance_id, refusals, "cog stopped on the bus after repeated refusals");
    }

    fn topic_opened(&self, t: &TopicInfo) {
        let Some(reg) = self.exposures.get().cloned().or_else(crate::expose_rt::global) else {
            return;
        };
        reg.register(topic_exposure(t));
    }

    fn stop_requested(&self, instance_id: &str, reason: &str) {
        match self.stop.get() {
            Some(f) => f(instance_id, reason),
            None => tracing::warn!(target: "cogbus", instance = instance_id, reason, "no workload stop hook: the cog keeps running without the bus"),
        }
    }
}
