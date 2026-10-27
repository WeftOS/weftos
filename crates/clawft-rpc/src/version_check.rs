//! Background version check with 24-hour cache.
//!
//! On every CLI invocation, checks whether a newer version of WeftOS
//! is available. The check result is cached for 24 hours to avoid
//! hammering the GitHub API. Prints a one-line notice to stderr if
//! an update is available.
//!
//! The check is non-blocking — it runs in a spawned thread and never
//! delays the main command.

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

const REPO: &str = "WeftOS/weftos";
const CACHE_FILE: &str = "version-check.json";
const CACHE_TTL_SECS: u64 = 86400; // 24 hours
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Cached version check result.
#[derive(serde::Serialize, serde::Deserialize)]
struct CacheEntry {
    latest_version: String,
    checked_at: u64,
}

fn cache_path() -> PathBuf {
    crate::runtime_dir().join(CACHE_FILE)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Check if a cached result is still fresh.
fn read_cache() -> Option<CacheEntry> {
    let data = fs::read_to_string(cache_path()).ok()?;
    let entry: CacheEntry = serde_json::from_str(&data).ok()?;
    if now_secs() - entry.checked_at < CACHE_TTL_SECS {
        Some(entry)
    } else {
        None
    }
}

/// Write a cache entry.
fn write_cache(latest: &str) {
    let entry = CacheEntry {
        latest_version: latest.to_string(),
        checked_at: now_secs(),
    };
    let dir = crate::runtime_dir();
    let _ = fs::create_dir_all(&dir);
    let _ = fs::write(
        dir.join(CACHE_FILE),
        serde_json::to_string(&entry).unwrap_or_default(),
    );
}

/// Fetch latest version from GitHub Releases API.
///
/// Uses `reqwest::blocking` so the call works inside the
/// `std::thread::spawn` in `check_and_notify_sync` without needing a
/// tokio runtime. Replaces the prior `curl` shell-out (which broke on
/// hosts without curl — Windows, minimal Alpine containers).
fn fetch_latest() -> Option<String> {
    fetch_latest_from(&format!(
        "https://api.github.com/repos/{REPO}/releases/latest"
    ))
}

fn fetch_latest_from(url: &str) -> Option<String> {
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(3))
        .timeout(std::time::Duration::from_secs(5))
        .user_agent(concat!("weftos/", env!("CARGO_PKG_VERSION")))
        .build()
        .ok()?;

    let resp = client
        .get(url)
        .header("Accept", "application/vnd.github.v3+json")
        .send()
        .ok()?;

    if !resp.status().is_success() {
        return None;
    }

    let body: serde_json::Value = resp.json().ok()?;
    let tag = body["tag_name"].as_str()?;
    Some(tag.strip_prefix('v').unwrap_or(tag).to_string())
}

/// Compare two semver-ish version strings (`MAJOR.MINOR.PATCH[-PRE][+BUILD]`).
/// Returns true if `latest` is newer than `current`. A pre-release sorts before
/// its release (`0.8.4-rc.1` < `0.8.4`) and after the previous release
/// (`0.8.3` < `0.8.4-rc.1`); pre-release identifiers compare per semver
/// (numeric parts numerically, `rc.2` < `rc.10`). Unparseable input is never newer.
fn is_newer(current: &str, latest: &str) -> bool {
    match (parse_version(current), parse_version(latest)) {
        (Some(c), Some(l)) => cmp_version(&l, &c) == std::cmp::Ordering::Greater,
        _ => false,
    }
}

type Version<'a> = ([u64; 3], Option<Vec<&'a str>>);

fn parse_version(v: &str) -> Option<Version<'_>> {
    let v = v.trim().trim_start_matches('v');
    let v = v.split('+').next()?;
    let (core, pre) = match v.split_once('-') {
        Some((c, p)) => (c, Some(p.split('.').collect::<Vec<_>>())),
        None => (v, None),
    };
    let mut nums = [0u64; 3];
    let mut parts = core.split('.');
    for n in &mut nums {
        *n = parts.next()?.parse().ok()?;
    }
    if parts.next().is_some() || pre.as_ref().is_some_and(|p| p.iter().any(|s| s.is_empty())) {
        return None;
    }
    Some((nums, pre))
}

fn cmp_version(a: &Version<'_>, b: &Version<'_>) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    a.0.cmp(&b.0).then_with(|| match (&a.1, &b.1) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(x), Some(y)) => {
            for (p, q) in x.iter().zip(y.iter()) {
                let o = match (p.parse::<u64>(), q.parse::<u64>()) {
                    (Ok(m), Ok(n)) => m.cmp(&n),
                    (Ok(_), Err(_)) => Ordering::Less,
                    (Err(_), Ok(_)) => Ordering::Greater,
                    (Err(_), Err(_)) => p.cmp(q),
                };
                if o != Ordering::Equal {
                    return o;
                }
            }
            x.len().cmp(&y.len())
        }
    })
}

/// Print an update notice to stderr if a newer version is available.
///
/// This is the main entry point. Call from main() before command dispatch.
/// It checks the cache first, and only hits the network if the cache is stale.
/// The network check runs in a background thread so it never blocks.
fn check_and_notify_sync() {
    // 1. Check cache
    if let Some(cached) = read_cache() {
        if is_newer(CURRENT_VERSION, &cached.latest_version) {
            eprintln!(
                "\x1b[33m↑ WeftOS v{} available (current: v{}). Run: weaver update\x1b[0m",
                cached.latest_version, CURRENT_VERSION
            );
        }
        return; // Cache is fresh, done
    }

    // 2. Cache is stale — fetch in background thread (don't block the command)
    std::thread::spawn(|| {
        if let Some(latest) = fetch_latest() {
            write_cache(&latest);
            if is_newer(CURRENT_VERSION, &latest) {
                eprintln!(
                    "\x1b[33m↑ WeftOS v{} available (current: v{}). Run: weaver update\x1b[0m",
                    latest, CURRENT_VERSION
                );
            }
        }
    });
}

/// Non-blocking version check. Call from main() before command dispatch.
///
/// Prints to stderr if an update is available:
/// ```text
/// ↑ WeftOS v0.5.6 available (current: v0.5.5). Run: weaver update
/// ```
///
/// Suppressed by setting `WEFTOS_NO_UPDATE_CHECK=1`.
pub fn check_for_updates() {
    // Allow users to disable
    if std::env::var("WEFTOS_NO_UPDATE_CHECK")
        .map(|v| v == "1")
        .unwrap_or(false)
    {
        return;
    }
    check_and_notify_sync();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_newer_works() {
        assert!(is_newer("0.5.4", "0.5.5"));
        assert!(is_newer("0.5.5", "0.6.0"));
        assert!(is_newer("0.5.5", "1.0.0"));
        assert!(!is_newer("0.5.5", "0.5.5"));
        assert!(!is_newer("0.5.5", "0.5.4"));
        assert!(!is_newer("1.0.0", "0.9.9"));
    }

    #[test]
    fn prereleases_order_per_semver() {
        assert!(!is_newer("0.8.4-rc.1", "0.8.3"), "a release candidate is newer than the previous release");
        assert!(is_newer("0.8.3", "0.8.4-rc.1"));
        assert!(is_newer("0.8.4-rc.1", "0.8.4"));
        assert!(!is_newer("0.8.4", "0.8.4-rc.1"));
        assert!(is_newer("0.8.4-rc.2", "0.8.4-rc.10"));
        assert!(is_newer("0.8.4-rc.1", "0.8.4-rc.1.1"));
        assert!(!is_newer("0.8.4-rc.1", "0.8.4-rc.1"));
        assert!(!is_newer("0.8.4-rc.1+abc", "0.8.4-rc.1"));
        assert!(is_newer("0.8.4-alpha", "0.8.4-rc.1"));
        assert!(!is_newer("0.8.3", "garbage"));
        assert!(!is_newer("0.8.3", "0.8"));
        assert!(!is_newer("0.8.3", "0.8.4-"));
    }

    #[test]
    fn current_version_is_set() {
        assert!(!CURRENT_VERSION.is_empty());
    }

    #[test]
    fn fetch_latest_parses_release_json() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/repos/x/y/releases/latest")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"tag_name": "v1.2.3"}"#)
            .create();

        let url = format!("{}/repos/x/y/releases/latest", server.url());
        assert_eq!(fetch_latest_from(&url).as_deref(), Some("1.2.3"));
    }

    #[test]
    fn fetch_latest_strips_v_prefix() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/r/latest")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"tag_name": "0.6.19"}"#)
            .create();

        let url = format!("{}/r/latest", server.url());
        assert_eq!(fetch_latest_from(&url).as_deref(), Some("0.6.19"));
    }

    #[test]
    fn fetch_latest_returns_none_on_404() {
        let mut server = mockito::Server::new();
        let _m = server.mock("GET", "/missing").with_status(404).create();

        let url = format!("{}/missing", server.url());
        assert!(fetch_latest_from(&url).is_none());
    }

    #[test]
    fn fetch_latest_returns_none_on_unparseable_body() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/garbage")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body("not json at all")
            .create();

        let url = format!("{}/garbage", server.url());
        assert!(fetch_latest_from(&url).is_none());
    }

    #[test]
    fn fetch_latest_returns_none_on_unreachable_host() {
        // Unbound port; reqwest connect_timeout (3s) fails fast.
        assert!(fetch_latest_from("http://127.0.0.1:1/x").is_none());
    }
}
