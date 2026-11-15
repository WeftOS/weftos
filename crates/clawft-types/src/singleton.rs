//! Process-wide singletons shared across crates.
//!
//! Some native runtimes allow only one instance per process (ONNX Runtime's
//! `ort` 2.0-rc.15 `Environment`: a second `ort::init().build()` errors, even
//! after the first is dropped, and the first cannot be fetched back). Crates
//! that cannot depend on each other still all link this one, so each of them
//! asks [`process_singleton`] for the instance and the first caller builds it.
//!
//! The registry is keyed by the value's [`TypeId`], so every crate that links
//! the same version of the runtime crate sees the same slot.

use std::any::{Any, TypeId};
use std::sync::Mutex;

type Slot = (TypeId, &'static (dyn Any + Send + Sync));

static REGISTRY: Mutex<Vec<Slot>> = Mutex::new(Vec::new());

/// Returns the process-wide `T`, building it with `init` on the first call.
///
/// `init` runs at most once per successful build, under the registry lock: it
/// must not call `process_singleton` itself. A failed `init` stores nothing, so
/// the next call retries. The value lives for the rest of the process.
pub fn process_singleton<T, E>(init: impl FnOnce() -> Result<T, E>) -> Result<&'static T, E>
where
    T: Any + Send + Sync,
{
    let mut reg = REGISTRY.lock().unwrap_or_else(|p| p.into_inner());
    let id = TypeId::of::<T>();
    if let Some((_, v)) = reg.iter().find(|(t, _)| *t == id) {
        // Keyed by TypeId, so the downcast cannot fail.
        return Ok(v.downcast_ref::<T>().expect("registry slot type matches its TypeId"));
    }
    let leaked: &'static T = Box::leak(Box::new(init()?));
    reg.push((id, leaked));
    Ok(leaked)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Probe(usize);

    #[test]
    fn first_call_builds_later_calls_reuse_and_a_failure_retries() {
        static BUILDS: AtomicUsize = AtomicUsize::new(0);
        let failed: Result<&'static Probe, &str> = process_singleton(|| Err("boom"));
        assert_eq!(failed.err(), Some("boom"));
        let a = process_singleton::<Probe, ()>(|| {
            BUILDS.fetch_add(1, Ordering::SeqCst);
            Ok(Probe(7))
        })
        .unwrap();
        let b = process_singleton::<Probe, ()>(|| {
            BUILDS.fetch_add(1, Ordering::SeqCst);
            Ok(Probe(8))
        })
        .unwrap();
        assert!(std::ptr::eq(a, b));
        assert_eq!(b.0, 7);
        assert_eq!(BUILDS.load(Ordering::SeqCst), 1);
    }
}
