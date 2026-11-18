//! `FollowState::save`: the read-merge-write and the lock-free verify-and-repair pass behind it
//! (WO-069 lane G3, the last open part of item 4).
//!
//! The directory lock and the lock-file fallback in `follower.rs` make the merge atomic wherever
//! an advisory lock works. They are the fast path, not the guarantee: some network and FUSE
//! filesystems fail every lock call, or succeed without excluding anyone. This pass makes the
//! invariant hold without any lock, and it always runs (it costs one extra read), so a lock that
//! silently does nothing is covered too. Nothing here depends on `flock` semantics.
//!
//! INVARIANT. After `save` returns `Ok`, the generation stored on disk for every slug is at least
//! the generation this saver held for it, and no sequence of concurrent saves by correct savers
//! leaves the file with a generation for a slug lower than one any completed save wrote.
//! (A "correct saver" is one that runs this code: it only ever writes the per-slug maximum of what
//! it read, what it holds and what the witness below recorded, so no write lowers a value its
//! writer could see.)
//!
//! MECHANISM. One round is temp + rename (as before), then a read-back; the save is done when the
//! read-back is at least what we wrote and at least what any other save in this process
//! completed. Otherwise something replaced our file with an older view (or damaged it: a corrupt,
//! truncated or deleted file reads as "lower"): fold what we now see into what we hold, back off
//! for a short jittered time, write again. At most `REPAIR_ROUNDS` rounds; then an error says
//! the state could not be made stable and what to do. The caller's in-memory state is still the
//! truth, and the next save merges again.
//!
//! WHY IT CONVERGES. Generations only rise: every write is the maximum of everything its writer
//! saw, and a repair round only ever adds to `held`. A save fails to settle only when another
//! rename landed between our rename and our read-back with a view lacking our values, so each
//! extra round needs a fresh rename by someone else; with finitely many savers each making finitely
//! many saves that cannot go on, and once all in-flight savers have finished the file is the
//! maximum of every value any of them held. Worst case: `REPAIR_ROUNDS` writes and reads, then the
//! documented error (a saver kept being overtaken); nothing is lowered by giving up.
//!
//! WHY A READ-BACK ALONE IS NOT ENOUGH (the hole in the plain verify loop). A overwrites with an
//! older view *after* B's whole save completed: B is gone, and A's read-back sees only A's own
//! file, so nothing in the file says that B's value was erased. Two measures close this:
//! 1. A stat guard: the file's (length, mtime, inode) is taken when we read and again before the
//!    rename; if it changed, someone wrote in between and we read and merge again (a few tries).
//!    That shrinks the window from "read to rename" to the one syscall pair after the guard.
//! 2. An in-process witness: the generations of every save completed on this path in this process
//!    (recorded right after its rename, before its read-back), kept only while some save on the
//!    path is in flight. A saver merges it before writing and checks the file against it after
//!    (Dekker style: each side writes, then looks for the other, so at least one of two racing
//!    savers sees the other). Savers in one process (the daemon serves overlapping `follow` calls
//!    from threads of one process) are fully covered with no lock at all.
//!
//! RESIDUAL. Two *processes* on a filesystem with no working locks can still lose an update when
//! one's rename lands in the other's guard-to-rename gap and the other's save has already
//! finished: nothing but the file itself is shared, and the rename erased the evidence. The window is one
//! syscall pair wide, the loss is a lowered generation (not corruption), and any later save by a
//! run that holds the higher value restores it; pinned by a test so a future change that closes it
//! updates this text. `seen` is not merged (last writer wins, as before; see `save`).
use crate::lock_ext::LockExt;
use super::*;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Rounds of write + read-back before `save` gives up.
const REPAIR_ROUNDS: usize = 8;
/// Re-reads when the file changes between our read and the rename.
const GUARD_TRIES: usize = 6;

type Gens = BTreeMap<String, u64>;

/// Raise `into` to the per-slug maximum with `from`. `true` if anything rose.
fn raise(into: &mut Gens, from: &Gens) -> bool {
    let mut rose = false;
    for (slug, g) in from {
        match into.get_mut(slug) {
            Some(cur) if *cur >= *g => {}
            Some(cur) => {
                *cur = *g;
                rose = true;
            }
            None => {
                into.insert(slug.clone(), *g);
                rose = true;
            }
        }
    }
    rose
}

/// The generations in the file, or `None` when it is missing, unreadable or does not parse. Only
/// the generations are read, so a file whose `seen` this version cannot parse (a newer state, say)
/// still contributes them.
fn read_generations(path: &Path) -> Option<Gens> {
    #[derive(Deserialize)]
    struct Generations {
        #[serde(default)]
        generations: Gens,
    }
    #[cfg(test)]
    crate::follower_seam::count_read();
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice::<Generations>(&bytes).ok().map(|g| g.generations)
}

type Fingerprint = Option<(u64, Option<SystemTime>, u64)>;

/// Cheap identity of the file as it stands: length, mtime and (unix) inode. Every save renames a
/// new file in, so the inode changes on every write.
fn fingerprint(path: &Path) -> Fingerprint {
    let m = std::fs::metadata(path).ok()?;
    #[cfg(unix)]
    let ino = std::os::unix::fs::MetadataExt::ino(&m);
    #[cfg(not(unix))]
    let ino = 0;
    Some((m.len(), m.modified().ok(), ino))
}

/// Generations written by saves in this process, per state file, kept only while a save on that
/// file is in flight (so an operator who resets the file between runs is not fought).
struct Witness {
    savers: usize,
    done: Gens,
}
static WITNESS: Mutex<BTreeMap<PathBuf, Witness>> = Mutex::new(BTreeMap::new());

/// One saver's registration with the witness; dropping it leaves.
struct Presence {
    key: Option<PathBuf>,
}

impl Presence {
    fn enter(path: &Path) -> Self {
        #[cfg(test)]
        if crate::follower_seam::no_memory() {
            return Self { key: None };
        }
        let key = match (path.parent().and_then(|d| d.canonicalize().ok()), path.file_name()) {
            (Some(d), Some(n)) => d.join(n),
            _ => path.to_path_buf(),
        };
        let mut all = WITNESS.lock_or_die("follower witness");
        all.entry(key.clone()).or_insert_with(|| Witness { savers: 0, done: Gens::new() }).savers += 1;
        Self { key: Some(key) }
    }

    /// Record `gens` as written (call right after the rename, before reading back).
    fn record(&self, gens: &Gens) {
        if let Some(key) = &self.key {
            let mut all = WITNESS.lock_or_die("follower witness");
            if let Some(w) = all.get_mut(key) {
                raise(&mut w.done, gens);
            }
        }
    }

    /// Whether another save on this file is in flight in this process right now.
    fn crowded(&self) -> bool {
        let Some(key) = &self.key else { return false };
        let all = WITNESS.lock_or_die("follower witness");
        all.get(key).is_some_and(|w| w.savers > 1)
    }

    fn done(&self) -> Gens {
        let Some(key) = &self.key else { return Gens::new() };
        let all = WITNESS.lock_or_die("follower witness");
        all.get(key).map(|w| w.done.clone()).unwrap_or_default()
    }
}

impl Drop for Presence {
    fn drop(&mut self) {
        let Some(key) = &self.key else { return };
        let mut all = WITNESS.lock_or_die("follower witness");
        if let Some(w) = all.get_mut(key) {
            w.savers -= 1;
            if w.savers == 0 {
                all.remove(key);
            }
        }
    }
}

/// A short pause before the next round, jittered from the process id, a counter and the clock so
/// two savers that collided do not collide again in step. Grows with the round, capped.
fn backoff(round: usize) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let t = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| u64::from(d.subsec_nanos()));
    let mut x = t ^ (u64::from(std::process::id()) << 32) ^ n.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    let cap_us = 50u64 << round.min(5);
    std::thread::sleep(Duration::from_micros(x % cap_us + 1));
}

#[cfg(test)]
use crate::follower_seam::fire;

impl FollowState {
    /// Merge with the file and write, then verify and repair (see the module comment). Called by
    /// `save_locked` while it holds whatever lock it could get.
    pub(super) fn save_repairing(
        &self,
        path: &Path,
        dir: &Path,
        io: &dyn Fn(std::io::Error) -> S2Error,
    ) -> Result<()> {
        #[cfg(test)]
        use crate::follower_seam::Phase;
        let presence = Presence::enter(path);
        let mut held = self.clone();
        if presence.crowded() {
            // Others are saving right now: step aside by a jittered moment so we do not stage in lockstep.
            backoff(1);
        }
        for round in 0..REPAIR_ROUNDS {
            // Merge what the file and the witness hold into what we hold, stage that, and check the
            // file did not change while we did (if it did, merge again). The check is the last
            // thing before the rename, so the unguarded gap is one syscall wide.
            let mut staged = None;
            for _ in 0..GUARD_TRIES {
                let before = fingerprint(path);
                if let Some(disk) = read_generations(path) {
                    raise(&mut held.generations, &disk);
                }
                raise(&mut held.generations, &presence.done());
                #[cfg(test)]
                fire(Phase::AfterRead);
                if let Some(old) = staged.take() {
                    let _ = std::fs::remove_file(old);
                }
                staged = Some(held.stage_temp(path, dir, io)?);
                // Last look before the rename: the file unchanged, and nothing completed in this
                // process that our staged content lacks (a memory read, so the window is tiny).
                if fingerprint(path) == before && !presence.done().iter().any(|(s, g)| held.generations.get(s).is_none_or(|h| h < g)) {
                    break;
                }
            }
            #[cfg(test)]
            fire(Phase::BeforeRename);
            Self::commit_temp(&staged.expect("staged at least once"), path, io)?;
            presence.record(&held.generations);
            #[cfg(test)]
            fire(Phase::AfterRename);
            // What must be on disk now: what we wrote, and what any save here completed.
            let mut need = held.generations.clone();
            raise(&mut need, &presence.done());
            let disk = read_generations(path);
            let settled = disk
                .as_ref()
                .is_some_and(|d| need.iter().all(|(slug, g)| d.get(slug).is_some_and(|on| on >= g)));
            if settled {
                return Ok(());
            }
            raise(&mut held.generations, &need);
            backoff(round);
        }
        Err(S2Error::Invalid(format!(
            "follow state {}: could not be made stable after {REPAIR_ROUNDS} rounds: another saver kept \
             replacing it with an older view (several processes sharing one state file on a filesystem \
             without working file locks). The in-memory state is unchanged and still correct: save again \
             to merge, or give each follower its own state file",
            path.display()
        )))
    }
}
