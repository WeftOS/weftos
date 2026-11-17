//! The one place this crate takes a `std::sync::Mutex`.
//!
//! The crate's locks guard plain in-memory state that is rewritten whole under the guard, so a
//! poisoned lock still holds usable data and every site recovers it rather than propagating the
//! panic. [`LockExt::lock_or_recover`] names that choice. Clippy's `disallowed-methods`
//! (`clippy.toml` beside `Cargo.toml`) rejects a direct `Mutex::lock` so a new site cannot pick a
//! different poison policy by accident.

use std::sync::{Mutex, MutexGuard};

/// Lock a [`Mutex`], recovering the guard if a previous holder panicked.
pub(crate) trait LockExt<T> {
    /// Returns the guard, ignoring poison. Behaves exactly like
    /// `lock().unwrap_or_else(|e| e.into_inner())`.
    fn lock_or_recover(&self) -> MutexGuard<'_, T>;
}

impl<T> LockExt<T> for Mutex<T> {
    #[expect(
        clippy::disallowed_methods,
        reason = "this is the single sanctioned call site that wraps Mutex::lock"
    )]
    fn lock_or_recover(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn locks_a_healthy_mutex() {
        let m = Mutex::new(1);
        *m.lock_or_recover() += 1;
        assert_eq!(*m.lock_or_recover(), 2);
    }

    #[test]
    fn recovers_the_value_after_a_holder_panics() {
        let m = Arc::new(Mutex::new(vec![1, 2]));
        let m2 = Arc::clone(&m);
        let joined = std::thread::spawn(move || {
            let mut g = m2.lock_or_recover();
            g.push(3);
            panic!("poison the lock on purpose");
        })
        .join();
        assert!(joined.is_err(), "the holder thread must have panicked");
        assert!(m.is_poisoned(), "the mutex must be poisoned");
        assert_eq!(*m.lock_or_recover(), vec![1, 2, 3]);
    }
}
