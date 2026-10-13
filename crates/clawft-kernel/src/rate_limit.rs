//! Keyed sliding-window rate limiter (WEFT-148).
//!
//! Used for DoS protection on:
//! - [`crate::cluster::ClusterMembership::add_peer`] — per source
//! - [`crate::governance::GovernanceEngine::evaluate`] — per principal
//!
//! Thresholds are fully configurable via [`RateLimitConfig`]. A limit of
//! `0` disables enforcement (always allow).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Configuration for a keyed rate limiter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimitConfig {
    /// Maximum events allowed per key within [`Self::window`].
    ///
    /// `0` means unlimited (rate limiting disabled).
    pub max_per_window: u32,
    /// Sliding window length.
    pub window: Duration,
}

impl RateLimitConfig {
    /// Disabled rate limiting (always allow).
    pub const fn unlimited() -> Self {
        Self {
            max_per_window: 0,
            window: Duration::from_secs(0),
        }
    }

    /// `max` events per key per minute.
    pub const fn per_minute(max: u32) -> Self {
        Self {
            max_per_window: max,
            window: Duration::from_secs(60),
        }
    }

    /// `max` events per key per second.
    pub const fn per_second(max: u32) -> Self {
        Self {
            max_per_window: max,
            window: Duration::from_secs(1),
        }
    }

    /// Whether this config enforces a limit.
    pub const fn is_enabled(&self) -> bool {
        self.max_per_window > 0
    }
}

/// Default peer-add limit: 1 addition per source per 100 ms
/// (preserves the pre-WEFT-148 aggressiveness, but keyed per source).
pub const DEFAULT_PEER_ADD_RATE: RateLimitConfig = RateLimitConfig {
    max_per_window: 1,
    window: Duration::from_millis(100),
};

/// Default governance-evaluation limit: 256 evaluations per principal
/// per second. High enough for normal agent tool loops; low enough to
/// stop a flood from a single principal.
pub const DEFAULT_GOVERNANCE_EVAL_RATE: RateLimitConfig = RateLimitConfig {
    max_per_window: 256,
    window: Duration::from_secs(1),
};

/// Thread-safe sliding-window rate limiter keyed by an arbitrary string
/// (source address, principal / agent id, etc.).
pub struct KeyedRateLimiter {
    config: Mutex<RateLimitConfig>,
    /// Per-key timestamps of recent allowed events (oldest first).
    windows: Mutex<HashMap<String, Vec<Instant>>>,
    /// Calls so far, to pace the idle-key sweep.
    calls: std::sync::atomic::AtomicU64,
}

/// Sweep idle keys every this many `check` calls.
const SWEEP_EVERY: u64 = 1024;
/// ...or on any call once the map holds more keys than this.
const SWEEP_OVER_KEYS: usize = 4096;

impl KeyedRateLimiter {
    /// Create a limiter with the given config.
    pub fn new(config: RateLimitConfig) -> Self {
        Self {
            config: Mutex::new(config),
            windows: Mutex::new(HashMap::new()),
            calls: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Create an unlimited (always-allow) limiter.
    pub fn unlimited() -> Self {
        Self::new(RateLimitConfig::unlimited())
    }

    /// Current configuration snapshot.
    pub fn config(&self) -> RateLimitConfig {
        *self
            .config
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Replace the configuration at runtime.
    ///
    /// Does not clear existing windows; the next `check` uses the new
    /// thresholds against remaining timestamps.
    pub fn set_config(&self, config: RateLimitConfig) {
        *self
            .config
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = config;
    }

    /// Check whether an event for `key` is allowed.
    ///
    /// On allow, records the event. On deny, does not record (so a
    /// denied flood does not permanently fill the window).
    ///
    /// Returns `true` if allowed, `false` if rate-limited.
    pub fn check(&self, key: &str) -> bool {
        let config = self.config();
        if !config.is_enabled() {
            return true;
        }

        let now = Instant::now();
        let mut map = match self.windows.lock() {
            Ok(g) => g,
            // Fail open on poisoned lock — DoS protection must not become
            // a hard-fault denial of service itself.
            Err(_) => return true,
        };

        // Amortized sweep: every SWEEP_EVERY calls, or when the map is large,
        // drop keys whose events have all aged out of the window.
        let calls = self.calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if calls % SWEEP_EVERY == SWEEP_EVERY - 1 || map.len() > SWEEP_OVER_KEYS {
            map.retain(|_, v| v.last().is_some_and(|ts| now.duration_since(*ts) < config.window));
        }

        let entry = map.entry(key.to_owned()).or_default();
        entry.retain(|ts| now.duration_since(*ts) < config.window);

        if entry.len() >= config.max_per_window as usize {
            return false;
        }

        entry.push(now);
        true
    }

    /// Number of tracked keys (for tests / metrics).
    pub fn tracked_keys(&self) -> usize {
        self.windows
            .lock()
            .map(|m| m.len())
            .unwrap_or(0)
    }

    /// Clear all recorded windows (tests / admin reset).
    pub fn reset(&self) {
        if let Ok(mut m) = self.windows.lock() {
            m.clear();
        }
    }
}

impl Default for KeyedRateLimiter {
    fn default() -> Self {
        Self::new(RateLimitConfig::per_minute(60))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_keys_are_pruned_and_a_recreated_key_still_enforces() {
        let lim = KeyedRateLimiter::new(RateLimitConfig {
            max_per_window: 2,
            window: Duration::from_millis(20),
        });
        for i in 0..100 {
            assert!(lim.check(&format!("k{i}")));
        }
        assert_eq!(lim.tracked_keys(), 100);
        std::thread::sleep(Duration::from_millis(40));
        // Drive the sweep with a single hot key.
        for _ in 0..SWEEP_EVERY {
            lim.check("hot");
        }
        assert!(lim.tracked_keys() <= 2, "idle keys were removed, got {}", lim.tracked_keys());
        // A pruned key comes back with a fresh, still-enforced window.
        assert!(lim.check("k1"));
        assert!(lim.check("k1"));
        assert!(!lim.check("k1"));
    }

    #[test]
    fn unlimited_always_allows() {
        let lim = KeyedRateLimiter::unlimited();
        for _ in 0..100 {
            assert!(lim.check("any"));
        }
    }

    #[test]
    fn blocks_after_quota() {
        let lim = KeyedRateLimiter::new(RateLimitConfig {
            max_per_window: 3,
            window: Duration::from_secs(60),
        });
        assert!(lim.check("a"));
        assert!(lim.check("a"));
        assert!(lim.check("a"));
        assert!(!lim.check("a"));
    }

    #[test]
    fn separate_keys_independent() {
        let lim = KeyedRateLimiter::new(RateLimitConfig {
            max_per_window: 1,
            window: Duration::from_secs(60),
        });
        assert!(lim.check("source-1"));
        assert!(!lim.check("source-1"));
        assert!(lim.check("source-2"));
    }

    #[test]
    fn set_config_disables() {
        let lim = KeyedRateLimiter::new(RateLimitConfig {
            max_per_window: 1,
            window: Duration::from_secs(60),
        });
        assert!(lim.check("x"));
        assert!(!lim.check("x"));
        lim.set_config(RateLimitConfig::unlimited());
        assert!(lim.check("x"));
    }

    #[test]
    fn default_constants_enabled() {
        assert!(DEFAULT_PEER_ADD_RATE.is_enabled());
        assert!(DEFAULT_GOVERNANCE_EVAL_RATE.is_enabled());
        assert_eq!(DEFAULT_PEER_ADD_RATE.max_per_window, 1);
        assert_eq!(DEFAULT_GOVERNANCE_EVAL_RATE.max_per_window, 256);
    }
}
