//! Operator-set physical location labels (fleet manager P1).
//!
//! A node's site and room are facts only an operator knows, so they are never
//! inferred (not from an IP, not from a heartbeat). They are set by the Admin
//! verb `fleet.location.set`, recorded on the chain, and persisted in the
//! daemon's runtime directory (`fleet-locations.json`, written atomically).
//! The snapshot shows them as `operator_claimed`.
//!
//! The file is the daemon's own state, kept per runtime directory; it is not
//! rebuilt from the chain (the chain event is the audit record, and a caller
//! holding a Write token can append look-alike events through `chain.append`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

/// Label file under the runtime directory.
pub const FILE: &str = "fleet-locations.json";
/// Chain source of the audit event.
pub const CHAIN_SOURCE: &str = "fleet";
/// Chain kind of the audit event.
pub const CHAIN_KIND: &str = "fleet.location.set";
/// Most nodes that can carry a label.
pub const MAX_LABELS: usize = 1024;
/// Longest site or room label, in characters.
pub const MAX_LABEL_LEN: usize = 64;
/// Longest node id.
pub const MAX_NODE_ID_LEN: usize = 128;

/// Where one node is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Location {
    /// Site (building, lab, rig).
    pub site: String,
    /// Room within the site.
    pub room: String,
    /// When the operator set it, unix seconds.
    pub set_at: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Doc {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    nodes: BTreeMap<String, Location>,
}

static DIR: OnceLock<PathBuf> = OnceLock::new();
/// Serialises read-modify-write of the label file inside this process.
static WRITE: Mutex<()> = Mutex::new(());

/// `s` cut to at most `max` bytes at a character boundary, with `...` appended
/// when something was cut (chain-event detail lines hold operator text).
pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &s[..end])
}

/// Record the runtime directory (daemon boot).
pub fn init(dir: &Path) {
    let _ = DIR.set(dir.to_path_buf());
}

/// The runtime directory, once [`init`] ran.
pub fn dir() -> Option<&'static Path> {
    DIR.get().map(PathBuf::as_path)
}

/// All labels. An absent file is empty; an unreadable or corrupt one is an
/// error, so a write never replaces what it could not read.
pub fn load(dir: &Path) -> Result<BTreeMap<String, Location>, String> {
    let path = dir.join(FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(format!("{FILE} unreadable: {}", e.kind())),
    };
    serde_json::from_str::<Doc>(&text)
        .map(|d| d.nodes)
        .map_err(|_| format!("{FILE} is not valid JSON"))
}

/// A node id is a short token: letters, digits and `. _ : - @`.
pub fn valid_node(node: &str) -> bool {
    !node.is_empty()
        && node.len() <= MAX_NODE_ID_LEN
        && node.chars().all(|c| c.is_ascii_alphanumeric() || "._:-@".contains(c))
}

/// Unicode format characters (category Cf: bidi overrides, zero-width, joiners)
/// plus the line and paragraph separators. They make a label read as something
/// other than what it is, so they are refused.
fn is_invisible_format(c: char) -> bool {
    matches!(c as u32,
        0x00AD | 0x0600..=0x0605 | 0x061C | 0x06DD | 0x070F | 0x08E2 | 0x180E
        | 0x200B..=0x200F | 0x2028..=0x202E | 0x2060..=0x2064 | 0x2066..=0x206F
        | 0xFEFF | 0xFFF9..=0xFFFB | 0x110BD | 0x1D173..=0x1D17A | 0xE0001 | 0xE0020..=0xE007F)
}

fn clean_label(what: &str, v: &str) -> Result<String, String> {
    let t = v.trim();
    if t.is_empty() {
        return Err(format!("{what} must not be empty"));
    }
    if t.chars().count() > MAX_LABEL_LEN {
        return Err(format!("{what} is longer than {MAX_LABEL_LEN} characters"));
    }
    if t.chars().any(|c| c.is_control() || is_invisible_format(c)) {
        return Err(format!("{what} must not contain control or invisible formatting characters"));
    }
    Ok(t.to_owned())
}

/// Validate `(node, site, room)` and return them trimmed.
pub fn validate(node: &str, site: &str, room: &str) -> Result<(String, String, String), String> {
    if !valid_node(node) {
        return Err(format!(
            "node must be 1-{MAX_NODE_ID_LEN} characters of letters, digits and . _ : - @"
        ));
    }
    Ok((node.to_owned(), clean_label("site", site)?, clean_label("room", room)?))
}

/// Set `node`'s label; returns the previous one, if any. Atomic (temp file,
/// then rename), mode 0600.
pub fn set(
    dir: &Path,
    node: &str,
    site: &str,
    room: &str,
    now: u64,
) -> Result<(Option<Location>, Location), String> {
    let _guard = lock();
    set_locked(dir, node, site, room, now)
}

/// The write lock; hold it across a read-then-set (see [`set_locked`]).
pub fn lock() -> std::sync::MutexGuard<'static, ()> {
    WRITE.lock().expect("WRITE lock poisoned")
}

/// [`set`] for a caller that already holds [`lock`].
pub fn set_locked(
    dir: &Path,
    node: &str,
    site: &str,
    room: &str,
    now: u64,
) -> Result<(Option<Location>, Location), String> {
    let (node, site, room) = validate(node, site, room)?;
    let mut nodes = load(dir)?;
    if !nodes.contains_key(&node) && nodes.len() >= MAX_LABELS {
        return Err(format!("{MAX_LABELS} labels is the limit"));
    }
    let loc = Location { site, room, set_at: now };
    let previous = nodes.insert(node, loc.clone());
    let text = serde_json::to_string_pretty(&Doc { version: 1, nodes })
        .map_err(|e| format!("encode failed: {e}"))?;
    write_atomic(dir, &text).map_err(|e| format!("could not save {FILE}: {}", e.kind()))?;
    Ok((previous, loc))
}

fn write_atomic(dir: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    std::fs::create_dir_all(dir)?;
    // A unique temp name; `create_new` makes a stale or planted file an error
    // rather than something we write through.
    let tmp = dir.join(format!(
        "{FILE}.{}.{}.tmp",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&tmp);
    let mut opts = std::fs::OpenOptions::new();
    opts.create_new(true).write(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    let res = (|| {
        let mut f = opts.open(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, dir.join(FILE))?;
        // Make the rename itself durable.
        if let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
        }
        Ok(())
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_persists_and_reports_the_previous_label() {
        let d = tempfile::tempdir().unwrap();
        let (prev, loc) = set(d.path(), "node-1", " Lab A ", "Rack 2", 10).unwrap();
        assert_eq!(prev, None);
        assert_eq!((loc.site.as_str(), loc.room.as_str()), ("Lab A", "Rack 2"));
        let (prev, _) = set(d.path(), "node-1", "Lab B", "Rack 3", 20).unwrap();
        assert_eq!(prev.map(|p| p.site), Some("Lab A".into()));
        let all = load(d.path()).unwrap();
        assert_eq!(all["node-1"].room, "Rack 3");
        assert_eq!(all["node-1"].set_at, 20);
        let leftovers = std::fs::read_dir(d.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(leftovers, 0);
    }

    #[test]
    fn absent_file_is_empty_and_corrupt_file_is_refused_not_replaced() {
        let d = tempfile::tempdir().unwrap();
        assert!(load(d.path()).unwrap().is_empty());
        std::fs::write(d.path().join(FILE), "{not json").unwrap();
        assert!(load(d.path()).is_err());
        assert!(set(d.path(), "n", "s", "r", 1).is_err());
        assert_eq!(std::fs::read_to_string(d.path().join(FILE)).unwrap(), "{not json");
    }

    #[test]
    fn inputs_are_validated() {
        for bad in ["", "a b", "a/b", "../x", &"x".repeat(MAX_NODE_ID_LEN + 1)] {
            assert!(validate(bad, "s", "r").is_err(), "{bad:?}");
        }
        assert!(validate("n", "", "r").is_err());
        assert!(validate("n", "s", "   ").is_err());
        assert!(validate("n", "s\n", "r").is_ok(), "trailing newline is trimmed");
        assert!(validate("n", "a\u{7}b", "r").is_err());
        for bad in ["a\u{202E}b", "a\u{200B}b", "a\u{FEFF}", "a\u{2028}b", "x\u{2066}"] {
            assert!(validate("n", bad, "r").is_err(), "{bad:?}");
        }
        assert!(validate("n", "Zürich \u{4e2d}\u{6587}", "r").is_ok());
        assert!(validate("n", &"s".repeat(MAX_LABEL_LEN + 1), "r").is_err());
        assert!(validate("5e1c-a9f0:node@host.local_1", "s", "r").is_ok());
    }

    #[test]
    fn truncation_never_splits_a_multibyte_character() {
        // A non-ASCII label straddling byte 60 of the serialised payload.
        let payload = serde_json::json!({ "node": "n", "site": format!("{}Zürich 東京", "x".repeat(40)) }).to_string();
        for cut in 55..70 {
            let out = truncate_chars(&payload, cut);
            assert!(out.ends_with("...") || out == payload);
        }
        let at_u = payload.find('ü').unwrap();
        let out = truncate_chars(&payload, at_u + 1); // lands inside the two-byte 'ü'
        assert!(out.starts_with(&payload[..at_u]) && out.ends_with("..."));
        assert_eq!(truncate_chars("short", 60), "short");
    }

    #[test]
    fn label_count_is_capped() {
        let d = tempfile::tempdir().unwrap();
        let nodes: BTreeMap<String, Location> = (0..MAX_LABELS)
            .map(|i| (format!("n{i}"), Location { site: "s".into(), room: "r".into(), set_at: 0 }))
            .collect();
        let text = serde_json::to_string(&Doc { version: 1, nodes }).unwrap();
        std::fs::write(d.path().join(FILE), text).unwrap();
        assert!(set(d.path(), "extra", "s", "r", 1).is_err());
        assert!(set(d.path(), "n0", "s2", "r", 1).is_ok(), "updating an existing node still works");
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        set(d.path(), "n", "s", "r", 1).unwrap();
        let mode = std::fs::metadata(d.path().join(FILE)).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0);
    }
}
