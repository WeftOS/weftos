//! Pin the chain storage location once at boot, and guard it with a lock.
//!
//! The kernel reads the chain checkpoint path in several places: boot
//! (signing key, RVF restore, tree checkpoint), the daemon's `chain.verify`
//! RPC, and shutdown persistence. [`pin_chain_storage`] resolves the path a
//! single time and writes it into the kernel config, so all of them agree
//! even if the environment changes after boot.
//!
//! Resolution, highest first:
//!
//! 1. an explicit `kernel.chain.checkpoint_path`;
//! 2. `chain.json` under the runtime root
//!    ([`clawft_types::runtime_paths::RuntimePaths`]: `$WEFTOS_RUNTIME_DIR`,
//!    else the project's `.weftos/runtime`, else legacy `~/.clawft`);
//! 3. a project root with no chain yet, while `~/.clawft/chain.*` exists
//!    (and no `--new-chain`): keep using the legacy chain and key. Booting a
//!    fresh genesis here would silently fork the operator's history, so the
//!    legacy chain stays in use (WARN) until `weaver migrate user-chain`
//!    (ADR-103 Phase 1) moves it.
//!
//! Once the legacy chain
//! carries a `MIGRATED-TO-WEFTOS.txt` marker, a boot that would still land on
//! it is refused (it would fork history) unless `WEFTOS_RUNTIME_DIR` isolates
//! it or `--adopt-legacy-chain` is passed (with a WARN).
//!
//! Whichever chain is in use is guarded by [`ChainLock`] (`chain.lock` beside
//! it) for the kernel's lifetime, so two kernels can never append to one
//! chain. In this crate's own unit tests the default is a fresh temp dir per
//! boot, so `cargo test` never touches the operator chain.

use std::path::{Path, PathBuf};

use clawft_types::config::KernelConfig;
use clawft_types::runtime_paths::{
    RootSource, RuntimePaths, legacy_chain_left_behind, legacy_migration_marker,
    user_chain_checkpoint, user_runtime_root,
};

/// The runtime paths this boot uses for every non-chain runtime file
/// (cluster peers, apps, revoked hosts).
///
/// Production: the one shared resolver. Unit tests: the directory holding
/// the pinned per-boot temp chain, so nothing lands in the operator's
/// runtime dir.
pub fn boot_runtime_paths(pinned_chain: Option<&Path>) -> RuntimePaths {
    #[cfg(test)]
    if let Some(dir) = pinned_chain.and_then(Path::parent) {
        return RuntimePaths::at(dir);
    }
    let _ = pinned_chain;
    RuntimePaths::resolve()
}

static NEW_CHAIN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

static ADOPT_LEGACY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Allow the next boot to adopt the legacy `~/.clawft` chain for the first
/// time (`weaver kernel start --adopt-legacy-chain`). Needed only while no
/// `chain.lock` exists beside it, i.e. no lock-aware kernel has used it yet.
pub fn request_adopt_legacy_chain(on: bool) {
    ADOPT_LEGACY.store(on, std::sync::atomic::Ordering::SeqCst);
}

/// Ask the next boot to start a fresh chain at the resolved path instead of
/// adopting the legacy `~/.clawft` chain (`weaver kernel start --new-chain`).
/// Process-wide on purpose: it is an operator flag, not a config-file field.
pub fn request_new_chain(on: bool) {
    NEW_CHAIN.store(on, std::sync::atomic::Ordering::SeqCst);
}

/// Which chain a default-path boot should use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainChoice {
    /// Checkpoint path to pin (RVF, key and tree derive from it).
    pub checkpoint: PathBuf,
    /// True when this is the legacy `~/.clawft` chain, not the resolved one.
    pub legacy_in_use: bool,
    /// WARN text for the operator, when the choice needs explaining.
    pub warning: Option<String>,
    /// Set when adopting the legacy chain looks unsafe: boot must refuse.
    pub refusal: Option<String>,
    /// `refusal` clears by itself within [`LEGACY_ACTIVE_WINDOW`] (an older
    /// kernel's recent write): a plain boot error, so a service manager's
    /// retry is the right response, not the permanent exit 78.
    pub transient: bool,
}

/// A legacy chain modified within this window may still have a live writer.
pub const LEGACY_ACTIVE_WINDOW: std::time::Duration = std::time::Duration::from_secs(120);

/// Seconds since the legacy chain (json or rvf) was last modified, when that
/// is inside [`LEGACY_ACTIVE_WINDOW`] and no `chain.lock` exists beside it.
///
/// Kernels that know about the lock create `chain.lock` the first time they
/// use a chain, so its absence means the last writer was an older,
/// lock-unaware build that may still be running.
pub(crate) fn lock_unaware_writer_age(checkpoint: &Path, now: std::time::SystemTime) -> Option<u64> {
    if ChainLock::lock_path(checkpoint).exists() {
        return None;
    }
    [checkpoint.to_path_buf(), checkpoint.with_extension("rvf")]
        .iter()
        .filter_map(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
        .filter_map(|m| now.duration_since(m).ok())
        .min()
        .filter(|age| *age < LEGACY_ACTIVE_WINDOW)
        .map(|age| age.as_secs())
}

fn has_chain(checkpoint: &Path) -> bool {
    checkpoint.exists() || checkpoint.with_extension("rvf").exists()
}

/// Refusal for adopting a legacy chain that only lock-unaware kernels have
/// written. With no `chain.lock` beside it an older daemon may still be
/// running, so the first adoption must be explicit; even then a very recent
/// write is refused.
fn legacy_adoption_refusal(
    legacy: &Path,
    adopt_legacy: bool,
    now: std::time::SystemTime,
) -> Option<(String, bool)> {
    if !ChainLock::lock_path(legacy).exists() && !adopt_legacy {
        return Some((format!(
            "The legacy chain at {} has never been used by a lock-aware kernel. Stop every \
             older weaver daemon (check `ps`/`weaver doctor daemon`) and then run \
             `weaver kernel start --adopt-legacy-chain` once. Or use --new-chain.",
            legacy.display()
        ), false));
    }
    // The age window passes on its own, so this one is transient.
    lock_unaware_writer_age(legacy, now).map(|age| {
        (
            format!(
                "the legacy chain at {} looks in use by an older kernel (modified {age}s ago); \
                 stop it first or use --new-chain",
                legacy.display()
            ),
            true,
        )
    })
}

/// Migration-marker refusal for `dir`, else the legacy adoption refusal for
/// `legacy`; the flag is [`ChainChoice::transient`].
fn migrated_or_adoption_refusal(
    dir: &Path,
    legacy: &Path,
    adopt_legacy: bool,
    now: std::time::SystemTime,
) -> (Option<String>, bool) {
    if let Some(m) = migrated_refusal(dir, adopt_legacy) {
        return (Some(m), false);
    }
    match legacy_adoption_refusal(legacy, adopt_legacy, now) {
        Some((m, t)) => (Some(m), t),
        None => (None, false),
    }
}

/// Fork hazard: the legacy chain in `dir` was migrated (marker beside it), and
/// this boot would still append to it. `--adopt-legacy-chain` overrides, with
/// a loud WARN.
fn migrated_refusal(dir: &Path, adopt_legacy: bool) -> Option<String> {
    let (marker, dest) = legacy_migration_marker(dir)?;
    if adopt_legacy {
        tracing::warn!(
            marker = %marker.display(),
            "--adopt-legacy-chain overrides a migration marker: this kernel appends to the \
             legacy chain and forks history from the migrated user chain"
        );
        return None;
    }
    Some(format!(
        "the legacy chain in {} was migrated to {} (see {}); booting on it would fork \
         history. Use the migrated chain (`weaver kernel start --profile user`), isolate this run with WEFTOS_RUNTIME_DIR, or \
         pass --adopt-legacy-chain to knowingly continue on the legacy copy",
        dir.display(),
        dest.as_deref().unwrap_or("~/.weftos/chain"),
        marker.display()
    ))
}

/// Explicit `kernel.chain.checkpoint_path` guard (Phase 1 review S5): an
/// explicit path skips every default-chain rule, so a config that pins the
/// legacy chain would keep appending to it after `weaver migrate user-chain`.
/// Refuse when the path's directory carries the migration marker, unless
/// `--adopt-legacy-chain` was passed (loud WARN, as for the default path).
fn explicit_path_refusal(checkpoint: &Path, adopt_legacy: bool) -> Option<String> {
    let dir = checkpoint.parent()?;
    let (marker, dest) = legacy_migration_marker(dir)?;
    if adopt_legacy {
        tracing::warn!(
            marker = %marker.display(),
            "--adopt-legacy-chain overrides a migration marker for an explicit \
             kernel.chain.checkpoint_path: this kernel forks history from the migrated chain"
        );
        return None;
    }
    Some(format!(
        "kernel.chain.checkpoint_path ({}) points into {}, whose chain was migrated to {} \
         (see {}); booting on it would fork history. Remove kernel.chain.checkpoint_path from \
         the config (check ~/.clawft/config.json), point it at the migrated chain, or pass \
         --adopt-legacy-chain to knowingly continue on the legacy copy",
        checkpoint.display(),
        dir.display(),
        dest.as_deref().unwrap_or("~/.weftos/chain"),
        marker.display()
    ))
}

/// Choose the default chain for `paths` (see module docs, rule 2 and 3).
///
/// Once a user chain exists at `~/.weftos/chain` (the result of
/// `weaver migrate user-chain`), a kernel that would still adopt the legacy
/// `~/.clawft` chain is refused: it would append to the old copy and fork
/// history. `--adopt-legacy-chain` overrides, with a loud warning.
pub fn choose_default_chain(
    paths: &RuntimePaths,
    home: Option<&Path>,
    new_chain: bool,
    adopt_legacy: bool,
    now: std::time::SystemTime,
) -> ChainChoice {
    let mut choice = choose_default_chain_inner(paths, home, new_chain, adopt_legacy, now);
    let Some(home) = home else {
        return choice;
    };
    let user = user_chain_checkpoint(home);
    let legacy = home
        .join(".clawft")
        .join(clawft_types::runtime_paths::CHAIN_CHECKPOINT_FILE);
    let on_legacy = !matches!(paths.source(), RootSource::User)
        && choice.legacy_in_use
        && choice.checkpoint == legacy;
    if on_legacy && has_chain(&user) {
        if adopt_legacy {
            choice.warning = Some(format!(
                "WARNING: appending to the legacy chain at {} although the chain was migrated to \
                 {} (--adopt-legacy-chain): the two chains will diverge",
                legacy.display(),
                user.display()
            ));
        } else {
            choice.refusal = Some(format!(
                "the legacy chain was migrated to {}; start the user daemon \
                 (`weaver kernel start --profile user`) or pass --adopt-legacy-chain to override",
                user.display()
            ));
        }
    }
    choice
}

fn choose_default_chain_inner(
    paths: &RuntimePaths,
    home: Option<&Path>,
    new_chain: bool,
    adopt_legacy: bool,
    now: std::time::SystemTime,
) -> ChainChoice {
    if matches!(paths.source(), RootSource::User) {
        return choose_user_chain(paths, home, new_chain, adopt_legacy, now);
    }
    let resolved = paths.chain_checkpoint();
    let plain = |checkpoint: PathBuf, warning: Option<String>, refusal: Option<String>| {
        ChainChoice {
            checkpoint,
            legacy_in_use: false,
            warning,
            refusal,
            transient: false,
        }
    };
    // Rooted at ~/.clawft itself (any non-project cwd, e.g. $HOME): the
    // resolved chain IS the legacy chain, so the first-adoption guard applies
    // (Phase 0 review R1), plus the migration marker. `--new-chain` cannot
    // start a fresh chain in place of it: its first checkpoint would
    // overwrite history.
    if matches!(paths.source(), RootSource::LegacyHome) && has_chain(&resolved) {
        let (refusal, transient) = if new_chain {
            (
                Some(format!(
                    "--new-chain cannot start a fresh chain at {} because the legacy chain \
                     lives there; start the kernel from a project, or set \
                     kernel.chain.checkpoint_path to a new location",
                    resolved.display()
                )),
                false,
            )
        } else {
            migrated_or_adoption_refusal(paths.root(), &resolved, adopt_legacy, now)
        };
        return ChainChoice {
            legacy_in_use: true,
            transient,
            ..plain(resolved, None, refusal)
        };
    }
    let Some(legacy) = legacy_chain_left_behind(paths, home) else {
        return plain(resolved, None, None);
    };
    if new_chain {
        let warning = format!(
            "starting a fresh chain at {} (--new-chain); the legacy chain at {} is untouched \
             and this kernel will not append to it",
            resolved.display(),
            legacy.display()
        );
        return plain(resolved, Some(warning), None);
    }
    let legacy_dir = legacy.parent().unwrap_or(Path::new("."));
    let (refusal, transient) =
        migrated_or_adoption_refusal(legacy_dir, &legacy, adopt_legacy, now);
    let warning = format!(
        "no chain at {} but a legacy chain exists at {}; continuing on the legacy chain \
         and its key so history is not forked (nothing was moved). Phase 1 \
         `weaver migrate user-chain` will move it. Pass --new-chain to start a fresh \
         chain at the resolved path instead, or set kernel.chain.checkpoint_path.",
        resolved.display(),
        legacy.display()
    );
    ChainChoice {
        checkpoint: legacy,
        legacy_in_use: true,
        warning: Some(warning),
        refusal,
        transient,
    }
}

/// Chain choice for the user daemon (`--profile user`, ADR-103 Phase 1).
///
/// At the standard root (`~/.weftos/run`) the chain is, in order: the user
/// chain `~/.weftos/chain` when one exists (what `weaver migrate user-chain`
/// produces); else the legacy `~/.clawft` chain under the same first-adoption
/// guard as Phase 0 (never a silent fresh genesis that would fork history);
/// else a fresh user chain. Under an isolated root (`WEFTOS_RUNTIME_DIR`)
/// the chain is that root's own and the operator's chains are never read.
/// `legacy_in_use` is true whenever the chain is not at `paths`' own root,
/// which keeps the anchor ledger beside the chain actually in use.
fn choose_user_chain(
    paths: &RuntimePaths,
    home: Option<&Path>,
    new_chain: bool,
    adopt_legacy: bool,
    now: std::time::SystemTime,
) -> ChainChoice {
    let standard = home.filter(|h| paths.root() == user_runtime_root(h));
    let Some(home) = standard else {
        return ChainChoice {
            checkpoint: paths.chain_checkpoint(),
            legacy_in_use: false,
            warning: None,
            refusal: None,
            transient: false,
        };
    };
    let user = user_chain_checkpoint(home);
    let legacy = home
        .join(".clawft")
        .join(clawft_types::runtime_paths::CHAIN_CHECKPOINT_FILE);
    if new_chain && has_chain(&user) {
        return ChainChoice {
            refusal: Some(format!(
                "--new-chain would orphan the existing user chain at {}; move that directory \
                 aside first if a fresh chain is really intended",
                user.display()
            )),
            checkpoint: user,
            legacy_in_use: true,
            warning: None,
            transient: false,
        };
    }
    if has_chain(&user) || new_chain || !has_chain(&legacy) {
        let warning = (new_chain && has_chain(&legacy)).then(|| {
            format!(
                "starting a fresh user chain at {} (--new-chain); the legacy chain at {} is \
                 untouched and this kernel will not append to it",
                user.display(),
                legacy.display()
            )
        });
        return ChainChoice {
            checkpoint: user,
            legacy_in_use: true,
            warning,
            refusal: None,
            transient: false,
        };
    }
    let (refusal, transient) =
        migrated_or_adoption_refusal(home.join(".clawft").as_path(), &legacy, adopt_legacy, now);
    ChainChoice {
        refusal,
        transient,
        warning: Some(format!(
            "no user chain at {} but a legacy chain exists at {}; continuing on the legacy \
             chain and its key so history is not forked (nothing was moved). Run \
             `weaver migrate user-chain` to move it, or pass --new-chain for a fresh user chain.",
            user.display(),
            legacy.display()
        )),
        checkpoint: legacy,
        legacy_in_use: true,
    }
}

/// Resolve the chain checkpoint path and pin it into `kernel_config.chain`.
///
/// Returns the pinned checkpoint path, or `None` when the chain is disabled
/// or no location can be resolved (no home dir, no runtime dir).
pub fn pin_chain_storage(kernel_config: &mut KernelConfig) -> Option<PathBuf> {
    pin_chain_storage_noted(kernel_config).path
}

/// What [`pin_chain_storage_noted`] decided.
#[derive(Debug, Default)]
pub struct PinOutcome {
    /// Pinned checkpoint path (`None` when the chain is disabled).
    pub path: Option<PathBuf>,
    /// WARN text explaining a non-obvious choice.
    pub warning: Option<String>,
    /// Boot must refuse with this message.
    pub refusal: Option<String>,
    /// The refusal clears on its own (see [`ChainChoice::transient`]).
    pub refusal_transient: bool,
}

/// [`pin_chain_storage`] plus the warning and refusal for the choice.
pub fn pin_chain_storage_noted(kernel_config: &mut KernelConfig) -> PinOutcome {
    let mut chain = kernel_config.chain.clone().unwrap_or_default();
    if !chain.enabled {
        return PinOutcome::default();
    }
    let mut warning = None;
    let mut refusal = None;
    let mut refusal_transient = false;
    let mut legacy_in_use = false;
    if chain.checkpoint_path.is_none() {
        let (path, w, legacy, r, transient) = default_checkpoint_path(&chain);
        chain.checkpoint_path = path;
        warning = w;
        refusal = r;
        refusal_transient = transient;
        legacy_in_use = legacy;
    } else if let Some(p) = &chain.checkpoint_path {
        refusal = explicit_path_refusal(
            Path::new(p),
            ADOPT_LEGACY.load(std::sync::atomic::Ordering::SeqCst),
        );
    }
    if let (Some(anchor), Some(ckpt)) = (chain.external_anchor.as_mut(), &chain.checkpoint_path)
        && anchor.ledger_path.is_none()
        && (cfg!(test) || legacy_in_use)
    {
        // Keep the anchor ledger beside the chain in use.
        let dir = PathBuf::from(ckpt);
        let dir = dir.parent().unwrap_or(Path::new("."));
        anchor.ledger_path = Some(
            dir.join("chain")
                .join("anchors.jsonl")
                .to_string_lossy()
                .into_owned(),
        );
    }
    let pinned = chain.checkpoint_path.clone().map(PathBuf::from);
    kernel_config.chain = Some(chain);
    PinOutcome {
        path: pinned,
        warning,
        refusal,
        refusal_transient,
    }
}

#[cfg(not(test))]
fn default_checkpoint_path(
    _chain: &clawft_types::config::ChainConfig,
) -> (Option<String>, Option<String>, bool, Option<String>, bool) {
    let paths = RuntimePaths::resolve();
    let home = clawft_types::runtime_paths::home_dir();
    let new_chain = NEW_CHAIN.load(std::sync::atomic::Ordering::SeqCst);
    let adopt = ADOPT_LEGACY.load(std::sync::atomic::Ordering::SeqCst);
    let choice = choose_default_chain(
        &paths,
        home.as_deref(),
        new_chain,
        adopt,
        std::time::SystemTime::now(),
    );
    (
        Some(choice.checkpoint.to_string_lossy().into_owned()),
        choice.warning,
        choice.legacy_in_use,
        choice.refusal,
        choice.transient,
    )
}

/// Unit-test default: a fresh temp dir per boot, never `~/.clawft`.
#[cfg(test)]
fn default_checkpoint_path(
    _chain: &clawft_types::config::ChainConfig,
) -> (Option<String>, Option<String>, bool, Option<String>, bool) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "weftos-kernel-unit-chain-{}-{n}",
        std::process::id()
    ));
    (
        Some(
            dir.join(clawft_types::runtime_paths::CHAIN_CHECKPOINT_FILE)
                .to_string_lossy()
                .into_owned(),
        ),
        None,
        false,
        None,
        false,
    )
}

/// Exclusive advisory lock on `chain.lock` beside the chain in use.
///
/// Held for the kernel's lifetime (the OS drops it on any exit, including a
/// crash). A second kernel on the same chain is refused with the holder's
/// PID instead of appending to the chain concurrently.
#[derive(Debug)]
pub struct ChainLock {
    _file: std::fs::File,
    path: PathBuf,
}

/// Why [`ChainLock::try_acquire`] failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainLockError {
    /// Another kernel holds the lock.
    InUse(String),
    /// A restart re-exec could not take back its own chain lock: the lock
    /// file names this very pid (exec keeps the pid), so a process that
    /// inherited the descriptor still holds it. The kernel is not running.
    SelfHeld(String),
    /// Filesystem or locking failure.
    Other(String),
}

/// Set (to `1`) in the environment of a SIGHUP re-exec: the new image has the
/// same pid as its predecessor, so a lock that names this pid is the
/// predecessor's, still draining, not another kernel. Owned here so the
/// kernel (chain lock) and the daemon (instance lock) agree on one name.
pub const REEXEC_ENV: &str = "WEFTOS_REEXEC";

/// How long a re-exec waits for an inherited chain-lock descriptor to close.
pub const REEXEC_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

impl ChainLock {
    /// Lock file path for a chain checkpoint (`chain.json` -> `chain.lock`).
    pub fn lock_path(checkpoint: &Path) -> PathBuf {
        checkpoint.with_extension("lock")
    }

    /// Take the lock for the chain at `checkpoint`.
    ///
    /// # Errors
    ///
    /// The message names the holder PID and the ways out (`--new-chain`,
    /// `kernel.chain.checkpoint_path`).
    pub fn acquire(checkpoint: &Path) -> Result<Self, String> {
        Self::try_acquire(checkpoint).map_err(|e| match e {
            ChainLockError::InUse(m) | ChainLockError::SelfHeld(m) | ChainLockError::Other(m) => m,
        })
    }

    /// [`acquire`](Self::acquire), telling "another kernel holds it" (a
    /// refusal no retry fixes) apart from I/O failures.
    pub fn try_acquire(checkpoint: &Path) -> Result<Self, ChainLockError> {
        let path = Self::lock_path(checkpoint);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| {
                ChainLockError::Other(format!("cannot create chain dir {}: {e}", dir.display()))
            })?;
        }
        let mut file = open_lock_file(&path).map_err(|e| {
            ChainLockError::Other(format!("cannot open chain lock {}: {e}", path.display()))
        })?;
        match try_lock(&file) {
            Ok(true) => {
                record_pid(&mut file);
                Ok(Self { _file: file, path })
            }
            Ok(false) => Err(ChainLockError::InUse(format!(
                "chain {} is in use by another kernel (pid {}); refusing to share a chain. \
                 Give this kernel its own: start with --new-chain or set \
                 kernel.chain.checkpoint_path",
                checkpoint.display(),
                holder_pid(&path)
            ))),
            Err(e) => Err(ChainLockError::Other(format!("cannot lock {}: {e}", path.display()))),
        }
    }

    /// [`try_acquire`](Self::try_acquire) for a process that came up through
    /// the SIGHUP re-exec (same pid as its predecessor). The predecessor's
    /// lock is released by the exec, but a child it was spawning at that
    /// instant can briefly hold an inherited descriptor, so a lock that names
    /// this very pid is retried for `wait` before giving up with
    /// [`ChainLockError::SelfHeld`]. A lock held under another pid fails at
    /// once, as in `try_acquire`.
    pub fn try_acquire_after_reexec(
        checkpoint: &Path,
        wait: std::time::Duration,
    ) -> Result<Self, ChainLockError> {
        let start = std::time::Instant::now();
        loop {
            match Self::try_acquire(checkpoint) {
                Err(ChainLockError::InUse(m))
                    if holder_pid(&Self::lock_path(checkpoint)) == std::process::id().to_string() =>
                {
                    if start.elapsed() >= wait {
                        return Err(ChainLockError::SelfHeld(format!(
                            "restart could not re-take the chain lock for {}: it is still held \
                             under this process's own pid {} by a process that inherited the \
                             descriptor; the kernel is NOT running, start it again ({m})",
                            checkpoint.display(),
                            std::process::id()
                        )));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                other => return other,
            }
        }
    }

    /// Check that `checkpoint`'s chain is not locked, without creating or
    /// writing the lock file. `Ok` when no lock file exists or it is free.
    pub fn probe(checkpoint: &Path) -> Result<(), String> {
        let path = Self::lock_path(checkpoint);
        if !path.exists() {
            return Ok(());
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|e| format!("cannot open chain lock {}: {e}", path.display()))?;
        match try_lock(&file) {
            Ok(true) => Ok(()),
            Ok(false) => Err(format!(
                "chain {} is in use by another kernel (pid {})",
                checkpoint.display(),
                holder_pid(&path)
            )),
            Err(e) => Err(format!("cannot probe {}: {e}", path.display())),
        }
    }

    /// The lock file.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn open_lock_file(path: &Path) -> std::io::Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    o.read(true).write(true).create(true).truncate(false);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        o.share_mode(0);
    }
    o.open(path)
}

/// `Ok(true)` when locked, `Ok(false)` when another holder has it.
#[cfg(unix)]
fn try_lock(file: &std::fs::File) -> std::io::Result<bool> {
    use std::os::fd::AsRawFd;
    // SAFETY: flock on a valid, open file descriptor owned by `file`.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(true);
    }
    let err = std::io::Error::last_os_error();
    if err.kind() == std::io::ErrorKind::WouldBlock {
        Ok(false)
    } else {
        Err(err)
    }
}

/// Windows holds the lock through the no-sharing open; other targets have
/// no advisory lock.
#[cfg(not(unix))]
fn try_lock(_file: &std::fs::File) -> std::io::Result<bool> {
    Ok(true)
}

fn record_pid(file: &mut std::fs::File) {
    use std::io::{Seek, Write};
    let _ = file.set_len(0);
    let _ = file.rewind();
    let _ = write!(file, "{}", std::process::id());
    let _ = file.flush();
}

fn holder_pid(lock: &Path) -> String {
    std::fs::read_to_string(lock)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .map_or_else(|| "?".into(), |p| p.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clawft_types::config::ChainConfig;

    /// A "now" far past any fixture mtime, so the recency guard stays quiet.
    fn far_future() -> std::time::SystemTime {
        std::time::SystemTime::now() + std::time::Duration::from_secs(86_400)
    }

    /// A fake home holding a legacy chain, and a project without one.
    fn legacy_fixture(t: &tempfile::TempDir) -> (PathBuf, PathBuf) {
        let home = t.path().join("home");
        std::fs::create_dir_all(home.join(".clawft")).unwrap();
        std::fs::write(home.join(".clawft/chain.json"), "{}").unwrap();
        let proj = t.path().join("proj");
        std::fs::create_dir_all(proj.join(".weftos")).unwrap();
        std::fs::write(proj.join(".weftos/project.toml"), "").unwrap();
        (home, proj)
    }

    #[test]
    fn legacy_present_and_project_missing_uses_legacy_chain() {
        let t = tempfile::tempdir().unwrap();
        let (home, proj) = legacy_fixture(&t);
        let paths = RuntimePaths::resolve_with(None, Some(&proj), Some(&home));
        let c = choose_default_chain(&paths, Some(&home), false, false, far_future());
        assert!(c.legacy_in_use);
        assert_eq!(c.checkpoint, home.join(".clawft/chain.json"));
        let w = c.warning.expect("warns");
        assert!(w.contains(".weftos/runtime/chain.json"), "{w}");
        assert!(w.contains(".clawft/chain.json"), "{w}");
        assert!(w.contains("weaver migrate user-chain"), "{w}");
        assert!(w.contains("--new-chain"), "{w}");
    }

    /// Phase 0 review R1: a kernel rooted at ~/.clawft itself (any
    /// non-project cwd, e.g. $HOME) gets the same first-adoption guard.
    #[test]
    fn legacy_home_root_requires_explicit_first_adoption() {
        let t = tempfile::tempdir().unwrap();
        let (home, _proj) = legacy_fixture(&t);
        let paths = RuntimePaths::resolve_with(None, Some(&home), Some(&home));
        assert_eq!(paths.source(), &RootSource::LegacyHome);
        let legacy = home.join(".clawft/chain.json");

        let c = choose_default_chain(&paths, Some(&home), false, false, far_future());
        assert_eq!(c.checkpoint, legacy);
        let r = c.refusal.expect("refused without --adopt-legacy-chain");
        assert!(r.contains("--adopt-legacy-chain"), "{r}");

        let c = choose_default_chain(&paths, Some(&home), false, true, far_future());
        assert!(c.refusal.is_none() && c.legacy_in_use);

        std::fs::write(ChainLock::lock_path(&legacy), "").unwrap();
        let c = choose_default_chain(&paths, Some(&home), false, false, far_future());
        assert!(c.refusal.is_none(), "a lock-aware kernel already used it");
    }

    #[test]
    fn only_the_age_window_refusal_is_transient() {
        let t = tempfile::tempdir().unwrap();
        let (home, _proj) = legacy_fixture(&t);
        let paths = RuntimePaths::resolve_with(None, Some(&home), Some(&home));
        let now = std::time::SystemTime::now();
        // Never used by a lock-aware kernel: needs an operator, permanent.
        let c = choose_default_chain(&paths, Some(&home), false, false, now);
        assert!(c.refusal.is_some() && !c.transient);
        // Adopted explicitly but written moments ago: clears by itself.
        let c = choose_default_chain(&paths, Some(&home), false, true, now);
        assert!(c.refusal.expect("recent write").contains("looks in use"));
        assert!(c.transient);
        // Same chain, long idle: no refusal at all.
        let c = choose_default_chain(&paths, Some(&home), false, true, far_future());
        assert!(c.refusal.is_none() && !c.transient);
    }

    #[test]
    fn legacy_home_root_refuses_new_chain_over_the_legacy_chain() {
        let t = tempfile::tempdir().unwrap();
        let (home, _proj) = legacy_fixture(&t);
        let paths = RuntimePaths::resolve_with(None, Some(&home), Some(&home));
        let c = choose_default_chain(&paths, Some(&home), true, false, far_future());
        let r = c.refusal.expect("--new-chain would overwrite the legacy chain");
        assert!(r.contains("checkpoint_path"), "{r}");
    }

    #[test]
    fn legacy_home_root_without_a_chain_starts_fresh() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let paths = RuntimePaths::resolve_with(None, Some(&home), Some(&home));
        let c = choose_default_chain(&paths, Some(&home), false, false, far_future());
        assert!(c.refusal.is_none() && !c.legacy_in_use);
    }

    #[test]
    fn user_profile_prefers_the_user_chain_then_legacy_then_fresh() {
        let t = tempfile::tempdir().unwrap();
        let (home, _proj) = legacy_fixture(&t);
        let paths = RuntimePaths::user_with(None, Some(&home));
        let legacy = home.join(".clawft/chain.json");
        let user = home.join(".weftos/chain/chain.json");

        // Legacy only: legacy chain, guarded like Phase 0.
        let c = choose_default_chain(&paths, Some(&home), false, false, far_future());
        assert_eq!(c.checkpoint, legacy);
        assert!(c.refusal.expect("guard").contains("--adopt-legacy-chain"));
        let c = choose_default_chain(&paths, Some(&home), false, true, far_future());
        assert!(c.refusal.is_none() && c.checkpoint == legacy);

        // --new-chain: a fresh user chain, legacy untouched.
        let c = choose_default_chain(&paths, Some(&home), true, false, far_future());
        assert_eq!(c.checkpoint, user);
        assert!(c.refusal.is_none() && c.warning.unwrap().contains("--new-chain"));

        // A user chain wins over the legacy one.
        std::fs::create_dir_all(user.parent().unwrap()).unwrap();
        std::fs::write(&user, "{}").unwrap();
        let c = choose_default_chain(&paths, Some(&home), false, false, far_future());
        assert_eq!((c.checkpoint.clone(), c.refusal), (user.clone(), None));

        // --new-chain now would orphan it: refused.
        let c = choose_default_chain(&paths, Some(&home), true, false, far_future());
        assert!(c.refusal.expect("refused").contains("orphan"));
    }

    #[test]
    fn default_daemon_is_refused_the_legacy_chain_once_a_user_chain_exists() {
        let t = tempfile::tempdir().unwrap();
        let (home, proj) = legacy_fixture(&t);
        let user = home.join(".weftos/chain/chain.json");
        std::fs::create_dir_all(user.parent().unwrap()).unwrap();
        std::fs::write(&user, "{}").unwrap();
        let legacy = home.join(".clawft/chain.json");
        // A project with no chain of its own would fall back to legacy: refused.
        let pp = RuntimePaths::resolve_with(None, Some(&proj), Some(&home));
        let c = choose_default_chain(&pp, Some(&home), false, false, far_future());
        assert!(c.legacy_in_use && c.refusal.expect("refused").contains("--profile user"));
        // A legacy-rooted kernel would fall back to the legacy chain: refused.
        let paths = RuntimePaths::resolve_with(None, Some(&home), Some(&home));
        let c = choose_default_chain(&paths, Some(&home), false, false, far_future());
        let r = c.refusal.expect("refused");
        assert!(
            r.contains("--profile user") && r.contains("--adopt-legacy-chain"),
            "{r}"
        );
        // Override is kept, with a loud warning.
        let c = choose_default_chain(&paths, Some(&home), false, true, far_future());
        assert_eq!(c.checkpoint, legacy);
        assert!(c.refusal.is_none());
        assert!(c.warning.unwrap().contains("diverge"));
        // A project that owns its own chain is not affected.
        std::fs::create_dir_all(proj.join(".weftos/runtime")).unwrap();
        std::fs::write(proj.join(".weftos/runtime/chain.json"), "{}").unwrap();
        let paths = RuntimePaths::resolve_with(None, Some(&proj), Some(&home));
        let c = choose_default_chain(&paths, Some(&home), false, false, far_future());
        assert!(c.refusal.is_none() && !c.legacy_in_use);
    }

    #[test]
    fn user_profile_with_no_chain_anywhere_starts_a_user_chain() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let paths = RuntimePaths::user_with(None, Some(&home));
        let c = choose_default_chain(&paths, Some(&home), false, false, far_future());
        assert_eq!(c.checkpoint, home.join(".weftos/chain/chain.json"));
        assert!(c.refusal.is_none() && c.warning.is_none());
    }

    #[test]
    fn user_profile_under_an_isolated_root_never_reads_home_chains() {
        let t = tempfile::tempdir().unwrap();
        let (home, _proj) = legacy_fixture(&t);
        let iso = t.path().join("iso");
        let paths = RuntimePaths::user_with(iso.to_str(), Some(&home));
        let c = choose_default_chain(&paths, Some(&home), false, false, far_future());
        assert_eq!(c.checkpoint, iso.join("chain.json"));
        assert!(c.refusal.is_none() && !c.legacy_in_use);
    }

    #[test]
    fn new_chain_flag_starts_fresh_at_the_resolved_path() {
        let t = tempfile::tempdir().unwrap();
        let (home, proj) = legacy_fixture(&t);
        let paths = RuntimePaths::resolve_with(None, Some(&proj), Some(&home));
        let c = choose_default_chain(&paths, Some(&home), true, false, far_future());
        assert!(!c.legacy_in_use);
        assert_eq!(c.checkpoint, proj.join(".weftos/runtime/chain.json"));
        assert!(c.warning.unwrap().contains("--new-chain"));
    }

    #[test]
    fn fresh_chain_when_no_chain_exists_anywhere() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("home");
        let proj = t.path().join("proj");
        std::fs::create_dir_all(proj.join(".weftos")).unwrap();
        std::fs::write(proj.join(".weftos/project.toml"), "").unwrap();
        let paths = RuntimePaths::resolve_with(None, Some(&proj), Some(&home));
        let c = choose_default_chain(&paths, Some(&home), false, false, far_future());
        assert!(!c.legacy_in_use && c.warning.is_none());
        assert_eq!(c.checkpoint, proj.join(".weftos/runtime/chain.json"));
    }

    #[test]
    fn existing_project_chain_wins_and_isolated_runs_never_adopt_legacy() {
        let t = tempfile::tempdir().unwrap();
        let (home, proj) = legacy_fixture(&t);
        std::fs::create_dir_all(proj.join(".weftos/runtime")).unwrap();
        std::fs::write(proj.join(".weftos/runtime/chain.json"), "{}").unwrap();
        let paths = RuntimePaths::resolve_with(None, Some(&proj), Some(&home));
        let c = choose_default_chain(&paths, Some(&home), false, false, far_future());
        assert!(!c.legacy_in_use);
        let iso = RuntimePaths::resolve_with(Some("/x"), Some(&proj), Some(&home));
        let c = choose_default_chain(&iso, Some(&home), false, false, far_future());
        assert_eq!(c.checkpoint, PathBuf::from("/x/chain.json"));
        assert!(!c.legacy_in_use);
    }

    #[test]
    fn first_legacy_adoption_needs_the_flag_then_becomes_normal() {
        let t = tempfile::tempdir().unwrap();
        let (home, proj) = legacy_fixture(&t);
        let paths = RuntimePaths::resolve_with(None, Some(&proj), Some(&home));
        let quiet = far_future();

        // No chain.lock and no flag: refused, with the operator recipe.
        let c = choose_default_chain(&paths, Some(&home), false, false, quiet);
        let r = c.refusal.expect("first adoption must be explicit");
        assert!(r.contains("never been used by a lock-aware kernel"), "{r}");
        assert!(r.contains("--adopt-legacy-chain"), "{r}");
        assert!(r.contains("--new-chain"), "{r}");

        // With the flag: adopted; booting creates chain.lock.
        let c = choose_default_chain(&paths, Some(&home), false, true, quiet);
        assert!(c.refusal.is_none() && c.legacy_in_use);
        drop(ChainLock::acquire(&c.checkpoint).unwrap());
        assert!(home.join(".clawft/chain.lock").exists());

        // Next boot without the flag: adopted normally.
        let c = choose_default_chain(&paths, Some(&home), false, false, quiet);
        assert!(c.refusal.is_none() && c.legacy_in_use);

        // --new-chain never needs the flag.
        let fresh = tempfile::tempdir().unwrap();
        let (home2, proj2) = legacy_fixture(&fresh);
        let p2 = RuntimePaths::resolve_with(None, Some(&proj2), Some(&home2));
        assert!(
            choose_default_chain(&p2, Some(&home2), true, false, quiet)
                .refusal
                .is_none()
        );
    }

    #[test]
    fn flag_does_not_override_a_very_recent_write() {
        let t = tempfile::tempdir().unwrap();
        let (home, proj) = legacy_fixture(&t);
        let paths = RuntimePaths::resolve_with(None, Some(&proj), Some(&home));
        let now = std::time::SystemTime::now();

        let r = choose_default_chain(&paths, Some(&home), false, true, now)
            .refusal
            .expect("recent write must be refused even with the flag");
        assert!(r.contains("older kernel"), "{r}");
        assert!(r.contains("--new-chain"), "{r}");

        let later = now + LEGACY_ACTIVE_WINDOW + std::time::Duration::from_secs(5);
        assert!(
            choose_default_chain(&paths, Some(&home), false, true, later)
                .refusal
                .is_none()
        );

        // Once a lock exists, a quick restart is fine.
        std::fs::write(home.join(".clawft/chain.lock"), "1").unwrap();
        assert!(
            choose_default_chain(&paths, Some(&home), false, false, now)
                .refusal
                .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn second_kernel_on_one_chain_is_refused_with_holder_pid() {
        let t = tempfile::tempdir().unwrap();
        let ckpt = t.path().join("chain.json");
        let first = ChainLock::acquire(&ckpt).expect("first lock");
        assert_eq!(first.path(), t.path().join("chain.lock"));
        let err = ChainLock::acquire(&ckpt).expect_err("second must fail");
        assert!(
            err.contains(&format!("pid {}", std::process::id())),
            "{err}"
        );
        assert!(err.contains("--new-chain"), "{err}");
        assert!(err.contains("kernel.chain.checkpoint_path"), "{err}");
        // A held lock is the refusal class; an unusable path is not.
        assert!(matches!(ChainLock::try_acquire(&ckpt), Err(ChainLockError::InUse(_))));
        let blocked = t.path().join("file");
        std::fs::write(&blocked, "x").unwrap();
        assert!(matches!(
            ChainLock::try_acquire(&blocked.join("sub/chain.json")),
            Err(ChainLockError::Other(_))
        ));
        drop(first);
        ChainLock::acquire(&ckpt).expect("free after drop");
    }

    #[cfg(unix)]
    #[test]
    fn reexec_chain_lock_naming_our_own_pid_is_retried_then_refused_clearly() {
        let t = tempfile::tempdir().unwrap();
        let ckpt = t.path().join("chain.json");
        // The lock file names this pid, as after an exec that keeps it.
        let holder = ChainLock::acquire(&ckpt).unwrap();
        // A plain acquire (no re-exec) refuses at once.
        let t0 = std::time::Instant::now();
        assert!(matches!(ChainLock::try_acquire(&ckpt), Err(ChainLockError::InUse(_))));
        assert!(t0.elapsed() < std::time::Duration::from_millis(100));
        // The re-exec retries first, then says so.
        let t0 = std::time::Instant::now();
        let err = ChainLock::try_acquire_after_reexec(&ckpt, std::time::Duration::from_millis(300))
            .expect_err("still held");
        assert!(t0.elapsed() >= std::time::Duration::from_millis(300), "retried first");
        match err {
            ChainLockError::SelfHeld(m) => assert!(m.contains("NOT running"), "{m}"),
            other => panic!("{other:?}"),
        }
        // Released while retrying: the re-exec comes up.
        let c2 = ckpt.clone();
        let j = std::thread::spawn(move || {
            ChainLock::try_acquire_after_reexec(&c2, std::time::Duration::from_secs(5)).map(|_| ())
        });
        std::thread::sleep(std::time::Duration::from_millis(250));
        drop(holder);
        j.join().unwrap().expect("acquired once the descriptor closed");
    }

    #[test]
    fn explicit_path_is_kept() {
        let mut k = KernelConfig {
            chain: Some(ChainConfig {
                checkpoint_path: Some("/data/c.json".into()),
                ..ChainConfig::default()
            }),
            ..KernelConfig::default()
        };
        assert_eq!(
            pin_chain_storage(&mut k),
            Some(PathBuf::from("/data/c.json"))
        );
    }

    #[test]
    fn disabled_chain_is_not_pinned() {
        let mut k = KernelConfig {
            chain: Some(ChainConfig {
                enabled: false,
                ..ChainConfig::default()
            }),
            ..KernelConfig::default()
        };
        assert_eq!(pin_chain_storage(&mut k), None);
        assert!(k.chain.unwrap().checkpoint_path.is_none());
    }

    #[test]
    fn unit_test_default_is_isolated_and_pinned() {
        let mut k = KernelConfig::default();
        let p = pin_chain_storage(&mut k).expect("pinned");
        assert!(p.starts_with(std::env::temp_dir()), "{}", p.display());
        if let Some(home) = std::env::var_os("HOME") {
            assert!(!p.starts_with(PathBuf::from(home).join(".clawft")));
        }
        assert_eq!(
            k.chain.unwrap().checkpoint_path.as_deref(),
            Some(p.to_string_lossy().as_ref())
        );
    }

    #[test]
    fn explicit_checkpoint_path_into_a_migrated_dir_is_refused() {
        let t = tempfile::tempdir().unwrap();
        let legacy = t.path().join(".clawft");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("chain.json"), "{}").unwrap();
        std::fs::write(
            legacy.join(clawft_types::runtime_paths::LEGACY_MIGRATED_MARKER),
            "migrated-to: /u/.weftos/chain\n",
        )
        .unwrap();
        let ckpt = legacy.join("chain.json");

        let mut cfg = KernelConfig::default();
        let chain = ChainConfig {
            checkpoint_path: Some(ckpt.to_string_lossy().into_owned()),
            ..ChainConfig::default()
        };
        cfg.chain = Some(chain);
        let out = pin_chain_storage_noted(&mut cfg);
        let msg = out.refusal.expect("refused");
        assert!(msg.contains("kernel.chain.checkpoint_path"), "{msg}");
        assert!(msg.contains("--adopt-legacy-chain"), "{msg}");

        // The override passes, and an unmarked directory is never refused.
        assert!(explicit_path_refusal(&ckpt, true).is_none());
        let other = t.path().join("elsewhere").join("chain.json");
        assert!(explicit_path_refusal(&other, false).is_none());
    }
}
