//! Per-project rate limit and token budget for the shared services
//! (ADR-103 Phase 2 F, user-daemon side).
//!
//! Two independent limits, both per project:
//!
//! * **rate**: at most `max_calls_per_min` admitted calls in any 60 s;
//! * **budget**: at most `token_budget` tokens in any `budget_window_secs`.
//!
//! The budget counts tokens, not calls, so splitting one large request into
//! many small ones cannot bypass it. Each call is charged at least
//! [`MIN_CALL_TOKENS`] (so a flood of near-empty calls still drains the
//! budget), and a call is *reserved* at its estimated cost before any work
//! runs: concurrent calls cannot each pass a check the sum would fail. The
//! reservation is replaced by the actual cost when the call finishes
//! ([`UsageMeter::settle`]); a failed call settles at the minimum.
//!
//! Limits come from the project manifest's `[shared]` table
//! (`~/.weftos/projects/<id>.toml`), read once per project into the user
//! daemon's memory (see `shared_rpc`) and refreshed only by a restart or an
//! operator `shared.reload`. Honest limit: the manifest is a file of the
//! user's uid, and a child kernel runs as that uid, so a hostile child can
//! edit it; the edit then needs a reload or restart to bite, which is a speed
//! bump, not a boundary. The real boundary is the Phase 4 sandbox. Absent keys
//! use the defaults below.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use clawft_types::project::ProjectManifest;

/// Smallest charge for any admitted call.
pub const MIN_CALL_TOKENS: u64 = 8;

/// Default calls per minute.
pub const DEFAULT_CALLS_PER_MIN: u32 = 120;
/// Default tokens per window.
pub const DEFAULT_TOKEN_BUDGET: u64 = 1_000_000;
/// Default budget window, seconds.
pub const DEFAULT_BUDGET_WINDOW_SECS: u64 = 3600;
/// Default cap on the estimated cost of one call.
pub const DEFAULT_MAX_TOKENS_PER_CALL: u64 = 32_768;

/// A project's limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SharedLimits {
    pub max_calls_per_min: u32,
    pub token_budget: u64,
    pub budget_window_secs: u64,
    pub max_tokens_per_call: u64,
}

impl Default for SharedLimits {
    fn default() -> Self {
        Self {
            max_calls_per_min: DEFAULT_CALLS_PER_MIN,
            token_budget: DEFAULT_TOKEN_BUDGET,
            budget_window_secs: DEFAULT_BUDGET_WINDOW_SECS,
            max_tokens_per_call: DEFAULT_MAX_TOKENS_PER_CALL,
        }
    }
}

impl SharedLimits {
    /// Read `[shared]` from a manifest's unknown-keys table. Wrong-typed or
    /// zero values fall back to the default rather than disabling a limit.
    pub fn from_manifest(m: &ProjectManifest) -> Self {
        let mut l = Self::default();
        let Some(t) = m.extra.get("shared").and_then(|v| v.as_table()) else {
            return l;
        };
        let int = |k: &str| t.get(k).and_then(|v| v.as_integer()).filter(|n| *n > 0);
        if let Some(n) = int("max_calls_per_min") {
            l.max_calls_per_min = u32::try_from(n).unwrap_or(u32::MAX);
        }
        if let Some(n) = int("token_budget") {
            l.token_budget = n as u64;
        }
        if let Some(n) = int("budget_window_secs") {
            l.budget_window_secs = n as u64;
        }
        if let Some(n) = int("max_tokens_per_call") {
            l.max_tokens_per_call = n as u64;
        }
        l
    }
}

/// Why a call was refused. `kind()` is the wire `error_kind`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    RateLimited { retry_after_secs: u64 },
    BudgetExceeded { used: u64, budget: u64, requested: u64, retry_after_secs: u64 },
    TooLarge { requested: u64, max: u64 },
}

impl Refusal {
    pub fn kind(&self) -> &'static str {
        match self {
            Refusal::RateLimited { .. } => "rate_limited",
            Refusal::BudgetExceeded { .. } => "budget_exceeded",
            Refusal::TooLarge { .. } => "request_too_large",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Refusal::RateLimited { retry_after_secs } => {
                format!("project call rate exceeded; retry in {retry_after_secs}s")
            }
            Refusal::BudgetExceeded { used, budget, requested, retry_after_secs } => format!(
                "project token budget exceeded ({used} used + {requested} requested > {budget}); \
                 oldest spend ages out in {retry_after_secs}s"
            ),
            Refusal::TooLarge { requested, max } => {
                format!("estimated {requested} tokens exceeds the per-call cap of {max}")
            }
        }
    }
}

/// An admitted call's reserved spend.
#[derive(Debug)]
pub struct Reservation {
    project: String,
    id: u64,
}

struct Spend {
    id: u64,
    at: Instant,
    tokens: u64,
}

#[derive(Default)]
struct ProjectUsage {
    calls: VecDeque<Instant>,
    spend: VecDeque<Spend>,
    next_id: u64,
}

/// Usage of every project, in memory (a daemon restart resets it; the chain
/// keeps the durable `shared.use` record).
#[derive(Default)]
pub struct UsageMeter {
    inner: Mutex<HashMap<String, ProjectUsage>>,
}

impl UsageMeter {
    /// Count one call against the rate limit, before the request is parsed
    /// or anything else is checked, so refused and malformed calls count too.
    pub fn note_call(
        &self,
        project: &str,
        limits: &SharedLimits,
        now: Instant,
    ) -> Result<(), Refusal> {
        let minute = Duration::from_secs(60);
        let mut map = self.inner.lock().expect("inner lock poisoned");
        let u = map.entry(project.to_owned()).or_default();
        while u.calls.front().is_some_and(|t| now.duration_since(*t) >= minute) {
            u.calls.pop_front();
        }
        if u.calls.len() >= limits.max_calls_per_min as usize {
            let oldest = *u.calls.front().expect("non-empty when at the limit");
            return Err(Refusal::RateLimited {
                retry_after_secs: minute.saturating_sub(now.duration_since(oldest)).as_secs() + 1,
            });
        }
        u.calls.push_back(now);
        Ok(())
    }

    /// Reserve `estimated` tokens (at least the minimum) against the budget.
    pub fn reserve(
        &self,
        project: &str,
        limits: &SharedLimits,
        estimated: u64,
        now: Instant,
    ) -> Result<Reservation, Refusal> {
        let charge = estimated.max(MIN_CALL_TOKENS);
        if charge > limits.max_tokens_per_call {
            return Err(Refusal::TooLarge { requested: charge, max: limits.max_tokens_per_call });
        }
        let window = Duration::from_secs(limits.budget_window_secs.max(1));
        let mut map = self.inner.lock().expect("inner lock poisoned");
        let u = map.entry(project.to_owned()).or_default();
        while u.spend.front().is_some_and(|s| now.duration_since(s.at) >= window) {
            u.spend.pop_front();
        }
        let used: u64 = u.spend.iter().map(|s| s.tokens).sum();
        if used.saturating_add(charge) > limits.token_budget {
            let retry = u
                .spend
                .front()
                .map(|s| window.saturating_sub(now.duration_since(s.at)).as_secs() + 1)
                .unwrap_or(0);
            return Err(Refusal::BudgetExceeded {
                used,
                budget: limits.token_budget,
                requested: charge,
                retry_after_secs: retry,
            });
        }
        let id = u.next_id;
        u.next_id += 1;
        u.spend.push_back(Spend { id, at: now, tokens: charge });
        Ok(Reservation { project: project.to_owned(), id })
    }

    /// [`note_call`](Self::note_call) then [`reserve`](Self::reserve).
    pub fn admit(
        &self,
        project: &str,
        limits: &SharedLimits,
        estimated: u64,
        now: Instant,
    ) -> Result<Reservation, Refusal> {
        self.note_call(project, limits, now)?;
        self.reserve(project, limits, estimated, now)
    }

    /// Replace the reservation with the actual cost (at least the minimum).
    /// Returns the tokens charged.
    pub fn settle(&self, r: &Reservation, actual: u64) -> u64 {
        let charged = actual.max(MIN_CALL_TOKENS);
        let mut map = self.inner.lock().expect("inner lock poisoned");
        if let Some(s) = map
            .get_mut(&r.project)
            .and_then(|u| u.spend.iter_mut().find(|s| s.id == r.id))
        {
            s.tokens = charged;
        }
        charged
    }
}

/// Rough token count: 4 bytes of text per token, at least 1 for any text.
pub fn estimate_tokens(text: &str) -> u64 {
    if text.is_empty() { 0 } else { (text.len() as u64).div_ceil(4) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(calls: u32, budget: u64) -> SharedLimits {
        SharedLimits {
            max_calls_per_min: calls,
            token_budget: budget,
            budget_window_secs: 3600,
            max_tokens_per_call: 10_000,
        }
    }

    #[test]
    fn rate_limit_refuses_then_recovers_after_a_minute() {
        let m = UsageMeter::default();
        let l = limits(3, 1_000_000);
        let t0 = Instant::now();
        for _ in 0..3 {
            m.admit("p", &l, 10, t0).unwrap();
        }
        let e = m.admit("p", &l, 10, t0).unwrap_err();
        assert_eq!(e.kind(), "rate_limited");
        assert!(m.admit("p", &l, 10, t0 + Duration::from_secs(61)).is_ok());
    }

    #[test]
    fn many_small_calls_cannot_bypass_the_token_budget() {
        // 100 tokens of budget; each call asks for 1 token but is charged the
        // minimum, so the budget runs out after 100 / MIN_CALL_TOKENS calls,
        // far fewer than 100, with the rate limit nowhere near.
        let m = UsageMeter::default();
        let l = limits(10_000, 100);
        let t0 = Instant::now();
        let mut admitted = 0;
        loop {
            match m.admit("p", &l, 1, t0) {
                Ok(r) => {
                    m.settle(&r, 1);
                    admitted += 1;
                }
                Err(e) => {
                    assert_eq!(e.kind(), "budget_exceeded");
                    break;
                }
            }
        }
        assert_eq!(admitted, 100 / MIN_CALL_TOKENS);
    }

    #[test]
    fn reservations_stop_concurrent_calls_from_overspending() {
        let m = UsageMeter::default();
        let l = limits(100, 1000);
        let t0 = Instant::now();
        let _a = m.admit("p", &l, 600, t0).unwrap();
        // Not yet settled, but the 600 is already held.
        assert_eq!(m.admit("p", &l, 600, t0).unwrap_err().kind(), "budget_exceeded");
    }

    #[test]
    fn settling_down_frees_budget_and_a_failure_costs_the_minimum() {
        let m = UsageMeter::default();
        let l = limits(100, 1000);
        let t0 = Instant::now();
        let r = m.admit("p", &l, 900, t0).unwrap();
        assert_eq!(m.settle(&r, 0), MIN_CALL_TOKENS);
        assert!(m.admit("p", &l, 900, t0).is_ok());
    }

    #[test]
    fn spend_ages_out_and_projects_are_independent() {
        let m = UsageMeter::default();
        let l = limits(100, 100);
        let t0 = Instant::now();
        m.admit("a", &l, 100, t0).unwrap();
        assert!(m.admit("a", &l, 10, t0).is_err());
        assert!(m.admit("b", &l, 100, t0).is_ok(), "b has its own budget");
        assert!(m.admit("a", &l, 10, t0 + Duration::from_secs(3601)).is_ok());
    }

    #[test]
    fn refused_calls_still_count_against_the_rate_limit() {
        let m = UsageMeter::default();
        let l = limits(3, 1_000_000);
        let t0 = Instant::now();
        // Oversized: refused by `reserve`, but `note_call` already counted it.
        for _ in 0..3 {
            m.note_call("p", &l, t0).unwrap();
            assert!(m.reserve("p", &l, 10_001, t0).is_err());
        }
        assert_eq!(m.note_call("p", &l, t0).unwrap_err().kind(), "rate_limited");
    }

    #[test]
    fn oversized_call_is_refused_before_anything_is_reserved() {
        let m = UsageMeter::default();
        let l = limits(100, 1_000_000);
        let e = m.admit("p", &l, 10_001, Instant::now()).unwrap_err();
        assert_eq!(e.kind(), "request_too_large");
    }

    #[test]
    fn limits_come_from_the_manifest_shared_table_with_safe_fallbacks() {
        let mut m = ProjectManifest {
            schema_version: 1,
            id: "01JB8Z3Q0V6X9KQ4M2N7T5R1WD".into(),
            name: "p".into(),
            root: "/p".into(),
            state: Default::default(),
            created: chrono::Utc::now(),
            last_seen: chrono::Utc::now(),
            project_toml: Default::default(),
            seed: None,
            legacy: None,
            serve: None,
            chain: None,
            binary: None,
            extra: Default::default(),
        };
        assert_eq!(SharedLimits::from_manifest(&m), SharedLimits::default());
        // Round-trip through the store so `extra` is real parsed TOML.
        let dir = tempfile::tempdir().unwrap();
        clawft_types::project::write_manifest(dir.path(), &m).unwrap();
        let path = clawft_types::project::manifest_path(dir.path(), &m.id).unwrap();
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("\n[shared]\nmax_calls_per_min = 5\ntoken_budget = 0\nbudget_window_secs = \"x\"\n");
        std::fs::write(&path, text).unwrap();
        m = clawft_types::project::read_manifest(dir.path(), &m.id).unwrap().unwrap();
        let l = SharedLimits::from_manifest(&m);
        assert_eq!(l.max_calls_per_min, 5);
        assert_eq!(l.token_budget, DEFAULT_TOKEN_BUDGET, "zero never disables the budget");
        assert_eq!(l.budget_window_secs, DEFAULT_BUDGET_WINDOW_SECS);
    }
}
