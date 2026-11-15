//! Test seam for `FollowState::save` (WO-069 lane G3). Compiled only under `cfg(test)`, so the
//! production build carries none of it. Thread-local, so parallel tests do not see each other's
//! settings; a closure hook may itself call `save` on the same thread (it is taken out while it
//! runs, so the nested save sees no hook).
//!
//! - `no_locks`: every advisory lock (directory, lock file) is skipped, simulating a filesystem
//!   whose lock calls fail or succeed without excluding anyone.
//! - `no_memory`: the in-process witness is skipped, simulating savers in separate processes.
//! - `on_phase`: a closure run at the three points where another saver can interleave.
//! - counters of the file reads and writes a save makes.
use std::cell::{Cell, RefCell};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    /// After the saver has read the file and merged, before it renames.
    AfterRead,
    /// After the last check before the rename, immediately before it.
    BeforeRename,
    /// After the rename, before the saver reads the file back.
    AfterRename,
}

type Hook = Box<dyn FnMut(Phase)>;

thread_local! {
    static NO_LOCKS: Cell<bool> = const { Cell::new(false) };
    static NO_MEMORY: Cell<bool> = const { Cell::new(false) };
    static HOOK: RefCell<Option<Hook>> = const { RefCell::new(None) };
    static READS: Cell<u32> = const { Cell::new(0) };
    static WRITES: Cell<u32> = const { Cell::new(0) };
}

pub(crate) fn no_locks() -> bool {
    NO_LOCKS.with(Cell::get)
}
pub(crate) fn no_memory() -> bool {
    NO_MEMORY.with(Cell::get)
}
pub(crate) fn set_no_locks(v: bool) {
    NO_LOCKS.with(|c| c.set(v));
}
pub(crate) fn set_no_memory(v: bool) {
    NO_MEMORY.with(|c| c.set(v));
}
pub(crate) fn set_hook(h: Option<Hook>) {
    HOOK.with(|c| *c.borrow_mut() = h);
}
pub(crate) fn fire(p: Phase) {
    let taken = HOOK.with(|c| c.borrow_mut().take());
    if let Some(mut h) = taken {
        h(p);
        HOOK.with(|c| {
            let mut slot = c.borrow_mut();
            if slot.is_none() {
                *slot = Some(h);
            }
        });
    }
}
pub(crate) fn count_read() {
    READS.with(|c| c.set(c.get() + 1));
}
pub(crate) fn count_write() {
    WRITES.with(|c| c.set(c.get() + 1));
}
pub(crate) fn reset_counts() {
    READS.with(|c| c.set(0));
    WRITES.with(|c| c.set(0));
}
pub(crate) fn counts() -> (u32, u32) {
    (READS.with(Cell::get), WRITES.with(Cell::get))
}
/// Clear every setting (tests call this first; the test harness reuses threads).
pub(crate) fn reset() {
    set_no_locks(false);
    set_no_memory(false);
    set_hook(None);
    reset_counts();
}
