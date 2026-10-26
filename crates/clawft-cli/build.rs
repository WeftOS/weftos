//! Build-time stamp emission for the `weft` binary.
//!
//! Mirrors `crates/clawft-weave/build.rs` so `weft` and `weaver` carry the
//! same provenance convention. Exposes the git short hash (+ `-dirty`), a
//! UTC build timestamp, and the composed `BUILD_VERSION` (`<pkg> (<hash>
//! <ts>)`) via `env!`:
//!
//! - `env!("BUILD_GIT_HASH")` — short hash, consumed by the daemon-mismatch
//!   guard to compare against the daemon's stamp.
//! - `env!("BUILD_TIMESTAMP")` — UTC compile time.
//! - `env!("BUILD_VERSION")` — the `weft --version` string.
//!
//! `scripts/build.sh install` (and `native` / `native-debug`) export
//! `GIT_SHA`, `GIT_DIRTY`, and `BUILD_TS` so the stamp is authoritative and
//! re-stamped on every install. A plain `cargo build` falls back to `git`,
//! and to `unknown` when git is unavailable. All failure modes are
//! non-fatal. The `--short=8` width and dirty logic match the weaver build
//! script exactly so the two hashes are directly comparable at runtime.

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs");
    println!("cargo:rerun-if-env-changed=WEFTOS_BUILD_COMMIT");
    println!("cargo:rerun-if-env-changed=GIT_SHA");
    println!("cargo:rerun-if-env-changed=GIT_DIRTY");
    println!("cargo:rerun-if-env-changed=BUILD_TS");
    println!("cargo:rerun-if-env-changed=BUILD_FEATURES");

    let hash_full = build_git_hash();
    println!("cargo:rustc-env=BUILD_GIT_HASH={hash_full}");

    let ts = build_timestamp();
    println!("cargo:rustc-env=BUILD_TIMESTAMP={ts}");

    let pkg = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".to_string());
    // Cargo-feature suffix (WEFT-656): `scripts/build.sh` exports
    // BUILD_FEATURES so `--version` reads `0.6.20 (<sha> <ts>) [voice-onnx]`
    // and the install guard can refuse a build that drops a feature the
    // installed binary has (e.g. diskann, whose absence silently degrades
    // the vector backend to a brute-force stub). Plain cargo builds carry
    // no suffix.
    let feats = env_override("BUILD_FEATURES")
        .map(|f| format!(" [{f}]"))
        .unwrap_or_default();
    println!("cargo:rustc-env=BUILD_VERSION={pkg} ({hash_full} {ts}){feats}");
}

/// Read an env var, treating empty/whitespace as absent.
fn env_override(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn is_truthy(v: &str) -> bool {
    matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "dirty" | "yes")
}

/// Resolve the short git hash with an optional `-dirty` suffix.
fn build_git_hash() -> String {
    let sha = env_override("WEFTOS_BUILD_COMMIT")
        .or_else(|| env_override("GIT_SHA"))
        .or_else(git_short_hash)
        .unwrap_or_else(|| "unknown".to_string());
    let dirty = match env_override("GIT_DIRTY") {
        Some(v) => is_truthy(&v),
        // An explicit commit names a clean source tree.
        None if env_override("WEFTOS_BUILD_COMMIT").is_some() => false,
        None => git_is_dirty().unwrap_or(false),
    };
    if dirty && sha != "unknown" {
        format!("{sha}-dirty")
    } else {
        sha
    }
}

/// Resolve the build timestamp: `BUILD_TS` env override wins; otherwise a
/// UTC stamp of the compile time.
fn build_timestamp() -> String {
    if let Some(ts) = env_override("BUILD_TS") {
        return ts;
    }
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_unix_secs(secs)
}

fn git_short_hash() -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--short=8", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_string())
}

/// `dist-manifest.json` is excluded: the release workflow redirects
/// `dist build` output into it at the repo root (untracked) before cargo
/// runs, which made every tagged release binary report `<sha>-dirty`.
fn git_is_dirty() -> Option<bool> {
    let out = Command::new("git")
        .args(["status", "--porcelain", "--", ".", ":(exclude)dist-manifest.json"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(!out.stdout.is_empty())
}

/// Convert unix-seconds to `YYYY-MM-DDTHH:MM:SSZ` UTC. Pure-stdlib so the
/// build script doesn't grow chrono / time deps.
fn format_unix_secs(secs: u64) -> String {
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let hh = rem / 3600;
    let mm = (rem % 3600) / 60;
    let ss = rem % 60;
    let (y, mo, d) = days_to_ymd(days as i64);
    format!("{y:04}-{mo:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

fn days_to_ymd(days_since_epoch: i64) -> (i32, u32, u32) {
    // Days since 1970-01-01 → (year, month, day). Howard Hinnant's
    // civil_from_days algorithm — fast, exact, well-known.
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (year as i32, m as u32, d as u32)
}
