//! Process state behind `shared.*` (ADR-103 Phase 2 F): the usage meter,
//! cached project limits, concurrency permits, the model allow-list cache and
//! the embedder/LLM sources. Kept apart from the handlers in `shared_rpc`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

use clawft_kernel::embedding::EmbeddingProvider;
use clawft_service_llm::{LlmClient, SharedLlmClient};

use crate::shared_meter::{SharedLimits, UsageMeter};

/// In-flight `shared.*` calls allowed per project.
pub const PER_PROJECT_CONCURRENT: usize = 2;
/// In-flight `shared.*` calls allowed in the whole daemon.
pub const GLOBAL_CONCURRENT: usize = 4;
/// How long a project that was refused for want of a global slot counts as
/// waiting (it must retry; `shared.*` does not queue).
pub const WAITING_TTL: Duration = Duration::from_secs(10);
/// Wall-clock cap on one `shared.llm.chat`: it bounds how long the user's own
/// next turn can wait behind it on the single model slot.
pub const LLM_CALL_TIMEOUT: Duration = Duration::from_secs(60);
/// Wall-clock cap on one `shared.embed`.
pub const EMBED_CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the parent's model list is trusted.
const MODELS_TTL: Duration = Duration::from_secs(60);
/// A failed or slow listing is remembered (as "no models") this long, and a
/// listing may take at most [`MODELS_TIMEOUT`].
const MODELS_NEGATIVE_TTL: Duration = Duration::from_secs(5);
const MODELS_TIMEOUT: Duration = Duration::from_secs(3);

static METER: OnceLock<UsageMeter> = OnceLock::new();
static LIMITS: Mutex<Option<HashMap<String, SharedLimits>>> = Mutex::new(None);
/// (mtime, length) of each project's manifest when its limits were cached.
type Stamp = (std::time::SystemTime, u64);
static STAMPS: Mutex<Option<HashMap<String, Stamp>>> = Mutex::new(None);
static SLOTS: Mutex<Option<FairSlots>> = Mutex::new(None);
static MODELS: Mutex<Option<(Instant, Duration, Vec<String>)>> = Mutex::new(None);
static EMBEDDER: tokio::sync::OnceCell<Arc<dyn EmbeddingProvider>> =
    tokio::sync::OnceCell::const_new();
static LLM: RwLock<Option<SharedLlmClient>> = RwLock::new(None);

/// The process usage meter.
pub fn meter() -> &'static UsageMeter {
    METER.get_or_init(UsageMeter::default)
}

/// The project's limits: loaded once with `load`, then served from memory
/// until [`reload`] or a restart.
pub fn cached_limits<E>(
    project: &str,
    load: impl FnOnce() -> Result<SharedLimits, E>,
) -> Result<SharedLimits, E> {
    let mut g = LIMITS.lock().unwrap_or_else(|e| e.into_inner());
    let map = g.get_or_insert_with(HashMap::new);
    if let Some(l) = map.get(project) {
        return Ok(*l);
    }
    let l = load()?;
    map.insert(project.to_owned(), l);
    Ok(l)
}

/// Drop every cached limit (operator `shared.reload`).
pub fn reload() {
    *LIMITS.lock().unwrap_or_else(|e| e.into_inner()) = None;
    *STAMPS.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// The cached limits of `project`, if any.
pub fn cached(project: &str) -> Option<SharedLimits> {
    LIMITS.lock().unwrap_or_else(|e| e.into_inner()).as_ref()?.get(project).copied()
}

/// Cache `limits` for `project` together with the manifest `stamp` they were
/// read under.
pub fn store(project: &str, limits: SharedLimits, stamp: Stamp) {
    LIMITS.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).insert(project.to_owned(), limits);
    STAMPS.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).insert(project.to_owned(), stamp);
}

/// Has the manifest changed since `project`'s limits were cached? True when
/// nothing is cached.
pub fn stamp_changed(project: &str, now: Stamp) -> bool {
    STAMPS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|m| m.get(project))
        .is_none_or(|s| *s != now)
}

/// Drop one project's cached limits (it was archived, unregistered or
/// registered again): the next call reloads them and re-checks that the
/// project is registered and active.
pub fn invalidate(project: &str) {
    if let Some(map) = LIMITS.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        map.remove(project);
    }
    if let Some(map) = STAMPS.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        map.remove(project);
    }
}

/// Who holds the daemon's `shared.*` slots, and who is waiting for one.
///
/// A plain pool lets two busy projects hold every global slot for as long as
/// they keep calling, and a third never gets in. This pool keeps the global
/// cap and the per-project cap, and adds one rule: a project that already
/// holds a slot may not take another while the free slots are needed by
/// projects that hold none and were refused within [`WAITING_TTL`] (the per-project cap is checked first). The
/// starved project gets the next slot that frees up; the holders get theirs
/// back when nobody is waiting.
#[derive(Debug)]
pub struct FairSlots {
    global: usize,
    per_project: usize,
    ttl: Duration,
    in_flight: HashMap<String, usize>,
    total: usize,
    waiting: HashMap<String, Instant>,
}

impl FairSlots {
    /// A pool of `global` slots, at most `per_project` of them per project.
    pub fn new(global: usize, per_project: usize, ttl: Duration) -> Self {
        Self { global, per_project, ttl, in_flight: HashMap::new(), total: 0, waiting: HashMap::new() }
    }

    /// Take a slot for `project` at `now`, or say there is none for it.
    pub fn try_take(&mut self, project: &str, now: Instant) -> bool {
        let ttl = self.ttl;
        self.waiting.retain(|_, at| now.saturating_duration_since(*at) < ttl);
        let mine = self.in_flight.get(project).copied().unwrap_or(0);
        if mine >= self.per_project {
            return false;
        }
        if self.total >= self.global {
            if mine == 0 {
                self.waiting.insert(project.to_owned(), now);
            }
            return false;
        }
        if mine > 0 {
            let starved = self
                .waiting
                .keys()
                .filter(|p| p.as_str() != project && !self.in_flight.contains_key(p.as_str()))
                .count();
            if self.global - self.total <= starved {
                return false;
            }
        }
        *self.in_flight.entry(project.to_owned()).or_insert(0) += 1;
        self.total += 1;
        self.waiting.remove(project);
        true
    }

    /// Give back a slot `project` holds.
    pub fn release(&mut self, project: &str) {
        if let Some(n) = self.in_flight.get_mut(project) {
            *n -= 1;
            self.total -= 1;
            if *n == 0 {
                self.in_flight.remove(project);
            }
        }
    }

    /// Slots `project` holds (tests).
    pub fn held(&self, project: &str) -> usize {
        self.in_flight.get(project).copied().unwrap_or(0)
    }
}

/// Held for the duration of one call; the slot goes back on drop.
pub struct Permits {
    project: String,
}

impl Drop for Permits {
    fn drop(&mut self) {
        if let Some(slots) = SLOTS.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            slots.release(&self.project);
        }
    }
}

/// Take a per-project and a global slot without waiting; `None` when either
/// is full, or when the free ones are kept for a project that is waiting.
pub fn try_acquire(project: &str) -> Option<Permits> {
    let mut g = SLOTS.lock().unwrap_or_else(|e| e.into_inner());
    let slots = g.get_or_insert_with(|| FairSlots::new(GLOBAL_CONCURRENT, PER_PROJECT_CONCURRENT, WAITING_TTL));
    slots.try_take(project, Instant::now()).then(|| Permits { project: project.to_owned() })
}

/// `model` if the parent lists it, else `None` (use the parent default). A
/// failed listing also gives `None`: never trust the child's choice blindly.
pub async fn allowed_model(client: &LlmClient, requested: Option<&str>) -> Option<String> {
    let want = requested.map(str::trim).filter(|m| !m.is_empty())?;
    let cached = {
        let g = MODELS.lock().unwrap_or_else(|e| e.into_inner());
        g.as_ref()
            .filter(|(at, ttl, _)| at.elapsed() < *ttl)
            .map(|(_, _, l)| l.clone())
    };
    let list = match cached {
        Some(l) => l,
        None => {
            let listed = tokio::time::timeout(MODELS_TIMEOUT, client.list_models()).await;
            let (l, ttl) = match listed {
                Ok(Ok(l)) => (l, MODELS_TTL),
                _ => (Vec::new(), MODELS_NEGATIVE_TTL),
            };
            *MODELS.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), ttl, l.clone()));
            l
        }
    };
    list.iter().any(|m| m == want).then(|| want.to_owned())
}

/// The LLM `shared.llm.*` serves: an installed override, else the daemon's.
pub fn llm_client() -> Option<SharedLlmClient> {
    LLM.read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .or_else(crate::daemon::daemon_llm)
}

/// The embedder `shared.embed` serves, loaded once (the model load runs off
/// the async threads and concurrent first calls share one load).
pub async fn embedder() -> Arc<dyn EmbeddingProvider> {
    EMBEDDER
        .get_or_init(|| async {
            tokio::task::spawn_blocking(|| {
                // Real model, else an "unavailable" provider: `shared.embed` then
                // reports "semantic search unavailable" instead of serving hashes.
                Arc::from(clawft_kernel::embedding::select_embedding_provider_or_unavailable(None))
            })
            .await
            .expect("embedder selection panicked")
        })
        .await
        .clone()
}

/// Serve `shared.llm.*` from `client` (tests and the `test-support` feature).
#[cfg(any(test, feature = "test-support"))]
pub fn install_llm(client: SharedLlmClient) {
    *LLM.write().unwrap_or_else(|e| e.into_inner()) = Some(client);
}

/// Serve `shared.embed` from `embedder`; the first install wins.
#[cfg(any(test, feature = "test-support"))]
pub fn install_embedder(embedder: Arc<dyn EmbeddingProvider>) {
    let _ = EMBEDDER.set(embedder);
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "01J0000000000000000000000A";
    const B: &str = "01J0000000000000000000000B";
    const C: &str = "01J0000000000000000000000C";

    fn pool() -> FairSlots {
        FairSlots::new(GLOBAL_CONCURRENT, PER_PROJECT_CONCURRENT, WAITING_TTL)
    }

    #[test]
    fn two_projects_cannot_keep_a_third_out_of_the_global_slots() {
        let mut p = pool();
        let t0 = Instant::now();
        // A and B fill all four slots (two each).
        assert!(p.try_take(A, t0) && p.try_take(A, t0) && p.try_take(B, t0) && p.try_take(B, t0));
        // C is refused, and is now waiting.
        assert!(!p.try_take(C, t0));
        // A finishes one call and immediately wants another: the free slot is C's.
        p.release(A);
        assert!(!p.try_take(A, t0), "a holder may not take the slot a starved project is waiting for");
        assert!(p.try_take(C, t0), "the starved project gets it");
        assert_eq!((p.held(A), p.held(B), p.held(C)), (1, 2, 1));
        // With nobody waiting any more the holders are not held back.
        p.release(B);
        assert!(p.try_take(B, t0));
    }

    #[test]
    fn a_waiting_project_that_goes_away_stops_holding_slots_back_after_the_ttl() {
        let mut p = pool();
        let t0 = Instant::now();
        for id in [A, A, B, B] {
            assert!(p.try_take(id, t0));
        }
        assert!(!p.try_take(C, t0));
        p.release(A);
        assert!(!p.try_take(A, t0));
        // C never came back.
        let later = t0 + WAITING_TTL + Duration::from_millis(1);
        assert!(p.try_take(A, later));
    }

    #[test]
    fn the_per_project_cap_and_the_global_cap_still_hold() {
        let mut p = pool();
        let t0 = Instant::now();
        assert!(p.try_take(A, t0) && p.try_take(A, t0));
        assert!(!p.try_take(A, t0), "per-project cap");
        assert!(p.try_take(B, t0) && p.try_take(B, t0));
        assert!(!p.try_take(C, t0), "global cap");
        for _ in 0..2 {
            p.release(A);
            p.release(B);
        }
        assert_eq!(p.total, 0);
        // Releasing more than was taken is harmless.
        p.release(A);
        assert_eq!(p.total, 0);
    }

    #[test]
    fn invalidate_drops_one_projects_cached_limits_only() {
        let limits = SharedLimits::default();
        let (pa, pb) = ("01J0000000000000000000INVA", "01J0000000000000000000INVB");
        let loads = std::cell::Cell::new(0);
        let load = || -> Result<SharedLimits, ()> {
            loads.set(loads.get() + 1);
            Ok(limits)
        };
        cached_limits(pa, load).unwrap();
        cached_limits(pb, load).unwrap();
        cached_limits(pa, load).unwrap();
        assert_eq!(loads.get(), 2, "served from the cache");
        invalidate(pa);
        cached_limits(pa, load).unwrap();
        cached_limits(pb, load).unwrap();
        assert_eq!(loads.get(), 3, "only the invalidated project reloaded");
    }
}
