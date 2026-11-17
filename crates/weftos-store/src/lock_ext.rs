//! The one place this crate takes a `std::sync::Mutex`.
//!
//! The release profile is `panic = "abort"`, so a holder can never unwind and poison a lock
//! there; poison only exists in unwinding builds (tests). A poisoned lock is therefore a bug,
//! and [`LockExt::lock_or_die`] panics with the lock's name rather than recovering. Clippy's
//! `disallowed-methods` (`clippy.toml` beside `Cargo.toml`) rejects a direct `Mutex::lock` so a
//! new site cannot pick a different poison policy by accident.

use std::sync::{Mutex, MutexGuard};

/// Lock a [`Mutex`], treating poison as fatal.
pub(crate) trait LockExt<T> {
    /// Returns the guard.
    ///
    /// # Panics
    ///
    /// Panics with `"<what> lock poisoned"` if a previous holder panicked.
    fn lock_or_die(&self, what: &str) -> MutexGuard<'_, T>;
}

impl<T> LockExt<T> for Mutex<T> {
    #[track_caller]
    #[expect(
        clippy::disallowed_methods,
        reason = "this is the single sanctioned call site that wraps Mutex::lock"
    )]
    fn lock_or_die(&self, what: &str) -> MutexGuard<'_, T> {
        self.lock()
            .unwrap_or_else(|_| panic!("{what} lock poisoned"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn locks_a_healthy_mutex() {
        let m = Mutex::new(1);
        *m.lock_or_die("counter") += 1;
        assert_eq!(*m.lock_or_die("counter"), 2);
    }

    #[test]
    #[should_panic(expected = "widget lock poisoned")]
    fn panics_naming_the_lock_after_a_holder_panics() {
        let m = Arc::new(Mutex::new(vec![1, 2]));
        let m2 = Arc::clone(&m);
        let joined = std::thread::spawn(move || {
            let mut g = m2.lock_or_die("widget");
            g.push(3);
            panic!("poison the lock on purpose");
        })
        .join();
        assert!(joined.is_err(), "the holder thread must have panicked");
        assert!(m.is_poisoned(), "the mutex must be poisoned");
        drop(m.lock_or_die("widget"));
    }
}
