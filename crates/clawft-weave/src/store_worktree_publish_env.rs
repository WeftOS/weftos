//! The daemon's two switches for worktree publishes (WO-069 lane D3, ADR-126 section 4). Read once,
//! at the first publish, and handed to `weftos_store::share::publish_tree_opts` as
//! [`PublishOptions`]; the store library itself reads no environment.
//!
//! - `WEFTOS_WORKTREE_MANIFEST_SCHEMA`: `2` (default) writes content-addressed objects and a
//!   schema-2 manifest; `1` is the rollback lever: path-named objects and a schema-1 manifest an
//!   rc.1 reader can read (the pool a schema-2 publish left is still retired and swept).
//! - `WEFTOS_WORKTREE_SWEEP`: `on` (default) or `off`, the kill switch: publishes still retire
//!   objects in the manifest but delete nothing.
//!
//! - `WEFTOS_WORKTREE_HASH_CACHE`: `on` (default) or `off`. Off publishes exactly as before the
//!   cache (WO-069 E1): every file read and hashed on every publish.
//! - `WEFTOS_WORKTREE_HASH_REHASH_HOURS`: hours between full passes that ignore the cache
//!   (default 24; 0 = every publish is a full pass).
//!
//! The cache is one file per project and worktree under the daemon's runtime directory
//! (`<runtime>/worktree-hash-cache/`), on the publishing machine's local disk: not in the store, not
//! in the git worktree.
//!
//! - `WEFTOS_WORKTREE_REQUIRE_CURRENT_WRITERS`: `1` makes `claim` and `promote` refuse a worktree whose
//!   bucket was last written by an older daemon (ADR-126 section 4a, gate item 8); `0`, unset or empty
//!   (default) only warns. Never applies to `publish`: a current writer publishing over an older
//!   writer's bucket is how the bucket heals.
//!
//! Any other value is an error on every publish (never a silent default), so a typo in a rollback
//! is seen at once.

use std::sync::OnceLock;
use weftos_store::share::{DEFAULT_FULL_REHASH_MS, HashCacheConfig, PublishOptions, SCHEMA_V1, SCHEMA_V2};

pub(crate) const SCHEMA_VAR: &str = "WEFTOS_WORKTREE_MANIFEST_SCHEMA";
pub(crate) const SWEEP_VAR: &str = "WEFTOS_WORKTREE_SWEEP";
pub(crate) const REQUIRE_VAR: &str = "WEFTOS_WORKTREE_REQUIRE_CURRENT_WRITERS";
pub(crate) const HASH_CACHE_VAR: &str = "WEFTOS_WORKTREE_HASH_CACHE";
pub(crate) const REHASH_HOURS_VAR: &str = "WEFTOS_WORKTREE_HASH_REHASH_HOURS";
const CACHE_DIR: &str = "worktree-hash-cache";

/// The options this daemon publishes with, from its environment (read once).
pub(crate) fn publish_options() -> Result<PublishOptions, String> {
    static O: OnceLock<Result<PublishOptions, String>> = OnceLock::new();
    O.get_or_init(|| parse(std::env::var(SCHEMA_VAR).ok().as_deref(), std::env::var(SWEEP_VAR).ok().as_deref())).clone()
}

/// The two values, unset or empty meaning the default.
pub(crate) fn parse(schema: Option<&str>, sweep: Option<&str>) -> Result<PublishOptions, String> {
    let schema = match schema.map(str::trim).filter(|s| !s.is_empty()) {
        None | Some("2") => SCHEMA_V2,
        Some("1") => SCHEMA_V1,
        Some(v) => return Err(format!("{SCHEMA_VAR}={v:?}: use 1 (rollback) or 2")),
    };
    let sweep = match sweep.map(str::trim).filter(|s| !s.is_empty()) {
        None | Some("on") => true,
        Some("off") => false,
        Some(v) => return Err(format!("{SWEEP_VAR}={v:?}: use on or off")),
    };
    Ok(PublishOptions { schema, sweep, ..PublishOptions::default() })
}

/// Whether `claim` and `promote` refuse a bucket last written by an older daemon (read once).
pub(crate) fn require_current_writers() -> Result<bool, String> {
    #[cfg(test)]
    if let Some(v) = test_require::FORCE.with(std::cell::Cell::get) {
        return Ok(v);
    }
    static O: OnceLock<Result<bool, String>> = OnceLock::new();
    O.get_or_init(|| parse_require(std::env::var(REQUIRE_VAR).ok().as_deref())).clone()
}

/// Tests set the switch per thread: the real one is read once per process.
#[cfg(test)]
pub(crate) mod test_require {
    thread_local! { pub(crate) static FORCE: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) }; }
}

pub(crate) fn parse_require(v: Option<&str>) -> Result<bool, String> {
    match v.map(str::trim).filter(|s| !s.is_empty()) {
        None | Some("0") => Ok(false),
        Some("1") => Ok(true),
        Some(v) => Err(format!("{REQUIRE_VAR}={v:?}: use 1 (refuse older writers) or 0")),
    }
}

/// Milliseconds between full passes, or `None` when the cache is switched off (read once).
pub(crate) fn hash_cache_interval() -> Result<Option<u64>, String> {
    static O: OnceLock<Result<Option<u64>, String>> = OnceLock::new();
    O.get_or_init(|| parse_cache(std::env::var(HASH_CACHE_VAR).ok().as_deref(), std::env::var(REHASH_HOURS_VAR).ok().as_deref())).clone()
}

pub(crate) fn parse_cache(switch: Option<&str>, hours: Option<&str>) -> Result<Option<u64>, String> {
    match switch.map(str::trim).filter(|s| !s.is_empty()) {
        None | Some("on") => {}
        Some("off") => return Ok(None),
        Some(v) => return Err(format!("{HASH_CACHE_VAR}={v:?}: use on or off")),
    }
    match hours.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(Some(DEFAULT_FULL_REHASH_MS)),
        Some(h) => h
            .parse::<u64>()
            .ok()
            .and_then(|h| h.checked_mul(3_600_000))
            .map(Some)
            .ok_or_else(|| format!("{REHASH_HOURS_VAR}={h:?}: use a whole number of hours (0 = every publish)")),
    }
}

/// The cache for worktree bucket `wt` of this daemon, or `None` when it is off or the daemon has no
/// runtime directory (a test, or no object store started): never a path outside that directory.
pub(crate) fn hash_cache_config(wt: &str) -> Result<Option<HashCacheConfig>, String> {
    let Some(every) = hash_cache_interval()? else { return Ok(None) };
    let Some(dir) = crate::store_rt::protected_paths().into_iter().next() else { return Ok(None) };
    Ok(Some(cache_config(&dir, wt, every)))
}

pub(crate) fn cache_config(runtime_dir: &std::path::Path, wt: &str, every_ms: u64) -> HashCacheConfig {
    let name: String = wt.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    HashCacheConfig { path: runtime_dir.join(CACHE_DIR).join(format!("{name}.json")), full_rehash_every_ms: every_ms }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_rollback_kill_switch_and_typos() {
        let d = parse(None, None).unwrap();
        assert_eq!((d.schema, d.sweep), (SCHEMA_V2, true));
        assert_eq!(parse(Some(""), Some("")).unwrap(), d);
        assert_eq!(parse(Some("1"), None).unwrap().schema, SCHEMA_V1);
        assert_eq!(parse(Some(" 2 "), Some("on")).unwrap(), d);
        let off = parse(None, Some("off")).unwrap();
        assert!(!off.sweep && off.grace_ms == d.grace_ms);
        assert!(parse(Some("3"), None).unwrap_err().contains(SCHEMA_VAR));
        assert!(parse(None, Some("false")).unwrap_err().contains(SWEEP_VAR));
    }

    #[test]
    fn the_require_current_writers_switch_defaults_off_and_rejects_typos() {
        assert_eq!(parse_require(None), Ok(false));
        assert_eq!(parse_require(Some("")), Ok(false));
        assert_eq!(parse_require(Some("0")), Ok(false));
        assert_eq!(parse_require(Some(" 1 ")), Ok(true));
        assert!(parse_require(Some("yes")).unwrap_err().contains(REQUIRE_VAR));
        assert!(parse_require(Some("on")).unwrap_err().contains(REQUIRE_VAR));
    }

    #[test]
    fn the_hash_cache_switch_hours_and_location() {
        assert_eq!(parse_cache(None, None).unwrap(), Some(DEFAULT_FULL_REHASH_MS));
        assert_eq!(parse_cache(Some("on"), Some(" 2 ")).unwrap(), Some(2 * 3_600_000));
        assert_eq!(parse_cache(Some("on"), Some("0")).unwrap(), Some(0));
        assert_eq!(parse_cache(Some("off"), Some("junk")).unwrap(), None, "off wins");
        assert!(parse_cache(Some("no"), None).unwrap_err().contains(HASH_CACHE_VAR));
        assert!(parse_cache(None, Some("-1")).unwrap_err().contains(REHASH_HOURS_VAR));
        assert!(parse_cache(None, Some("x")).unwrap_err().contains(REHASH_HOURS_VAR));
        let c = cache_config(std::path::Path::new("/run/weftos"), "t-w-01abc/../x", 5);
        assert_eq!(c.path, std::path::PathBuf::from("/run/weftos/worktree-hash-cache/t-w-01abc____x.json"));
        assert_eq!(c.full_rehash_every_ms, 5);
    }
}
