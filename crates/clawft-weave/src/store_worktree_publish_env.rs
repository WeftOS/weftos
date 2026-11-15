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
//! Any other value is an error on every publish (never a silent default), so a typo in a rollback
//! is seen at once.

use std::sync::OnceLock;
use weftos_store::share::{PublishOptions, SCHEMA_V1, SCHEMA_V2};

pub(crate) const SCHEMA_VAR: &str = "WEFTOS_WORKTREE_MANIFEST_SCHEMA";
pub(crate) const SWEEP_VAR: &str = "WEFTOS_WORKTREE_SWEEP";

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
}
