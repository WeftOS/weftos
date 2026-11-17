//! The watched inbox: one append-only status log per Grok Bot under
//! `~/.config/weftos/cos/inbox/<bot>.md`.
//!
//! Bots append one line per update. The CoS keeps a persisted cursor per file
//! (byte offset, inode, and a hash of the file's head), so it consumes only
//! what was appended since it last looked, across restarts. A file that
//! shrank, was replaced (new inode) or whose head changed is read again from
//! the start. Only complete lines are consumed: a partial last line waits for
//! its newline.
//!
//! Everything read here is untrusted status text from other programs. It is
//! stripped of control characters, size-capped and handed on as labelled
//! data; it is never interpreted. Watching is by polling `stat` (mtime and
//! length) every [`POLL`], which needs no dependency and behaves the same on
//! every platform.
//!
//! Line format (see the chief-of-staff page): an optional RFC 3339 timestamp,
//! an optional `[ok|working|blocked|error]`, then the text. Blank lines and
//! lines starting with `#` are ignored.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Most bots followed; the rest (in name order) are ignored.
pub const MAX_BOTS: usize = 64;
/// Most bytes consumed from one file per poll. A longer backlog takes more polls.
pub const MAX_READ: usize = 16 * 1024;
/// Longest text kept per entry, in characters; the rest is cut.
pub const MAX_TEXT_CHARS: usize = 1024;
/// Entries kept per bot (newest last).
pub const KEEP: usize = 20;
/// How often the watcher looks at the folder.
pub const POLL: Duration = Duration::from_secs(2);
/// Bytes of the file's head that the cursor hashes to spot a rewrite.
const HEAD_BYTES: u64 = 256;
/// A bot-supplied timestamp this far ahead of now is treated as now.
const MAX_SKEW_SECS: i64 = 300;
/// The statuses a line may carry.
const STATUSES: [&str; 4] = ["ok", "working", "blocked", "error"];

/// One consumed update.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// RFC 3339 UTC: the line's own timestamp, else when the CoS read it.
    pub at: String,
    pub status: Option<String>,
    pub text: String,
}

/// A bot as the CoS sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BotView {
    pub bot: String,
    /// Status of the newest entry.
    pub status: Option<String>,
    /// `at` of the newest entry.
    pub updated: String,
    /// Text of the newest entry.
    pub title: String,
    /// Recent entries, newest first.
    pub entries: Vec<Entry>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct BotState {
    offset: u64,
    inode: Option<u64>,
    head: String,
    entries: Vec<Entry>,
    #[serde(skip)]
    sig: Option<(SystemTime, u64)>,
}

#[derive(Default)]
struct Inner {
    bots: BTreeMap<String, BotState>,
    loaded: bool,
}

/// The inbox: a folder, a cursor file and the entries read so far. Cheap to
/// clone; clones share state.
#[derive(Clone)]
pub struct Inbox {
    dir: PathBuf,
    state_file: PathBuf,
    inner: Arc<Mutex<Inner>>,
}

impl std::fmt::Debug for Inbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inbox").field("dir", &self.dir).finish_non_exhaustive()
    }
}

/// Whether `name` may be a bot name: 1 to 64 of `[A-Za-z0-9_-]`.
pub fn valid_bot_name(name: &str) -> bool {
    (1..=64).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Strip control and bidi-format characters and cut to `max` characters.
pub fn clean(s: &str, max: usize) -> String {
    s.chars()
        .map(|c| if c == '\t' { ' ' } else { c })
        .filter(|c| !c.is_control() && !matches!(*c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}'))
        .take(max)
        .collect::<String>()
        .trim()
        .to_string()
}

/// Parse one complete line into an entry; `None` for blank and comment lines.
fn parse_line(line: &str, now: DateTime<Utc>) -> Option<Entry> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let mut rest = line;
    let mut at = now;
    if let Some((first, tail)) = rest.split_once(char::is_whitespace)
        && let Ok(t) = DateTime::parse_from_rfc3339(first)
    {
        at = t.with_timezone(&Utc);
        rest = tail.trim_start();
    }
    if at > now + chrono::Duration::seconds(MAX_SKEW_SECS) {
        at = now;
    }
    let mut status = None;
    if let Some(tail) = rest.strip_prefix('[')
        && let Some((word, after)) = tail.split_once(']')
        && STATUSES.contains(&word)
    {
        status = Some(word.to_string());
        rest = after.trim_start();
    }
    let text = clean(rest, MAX_TEXT_CHARS);
    if text.is_empty() {
        return None;
    }
    Some(Entry { at: at.to_rfc3339_opts(SecondsFormat::Secs, true), status, text })
}

#[cfg(unix)]
fn inode_of(m: &fs::Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    Some(m.ino())
}

#[cfg(not(unix))]
fn inode_of(_: &fs::Metadata) -> Option<u64> {
    None
}

fn head_hash(f: &mut fs::File, upto: u64) -> std::io::Result<String> {
    let n = upto.min(HEAD_BYTES) as usize;
    let mut buf = vec![0u8; n];
    f.seek(SeekFrom::Start(0))?;
    f.read_exact(&mut buf)?;
    Ok(hex::encode(Sha256::digest(&buf)))
}

impl Inbox {
    /// An inbox over `dir`. Touches nothing until [`Inbox::scan`]. The cursor
    /// file sits beside the folder as `inbox-state.json`.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        let state_file = match dir.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.join("inbox-state.json"),
            _ => dir.join(".inbox-state.json"),
        };
        Self { dir, state_file, inner: Arc::default() }
    }

    /// The watched folder.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("inbox lock poisoned")
    }

    /// Look at the folder once and consume what was appended. Returns whether
    /// anything changed. A missing folder is an empty inbox.
    pub fn scan(&self) -> bool {
        self.scan_at(Utc::now())
    }

    /// [`Inbox::scan`] with an explicit clock.
    pub fn scan_at(&self, now: DateTime<Utc>) -> bool {
        let mut inner = self.lock();
        if !inner.loaded {
            inner.loaded = true;
            if let Some(saved) = fs::read_to_string(&self.state_file).ok().and_then(|t| serde_json::from_str::<BTreeMap<String, BotState>>(&t).ok()) {
                inner.bots = saved.into_iter().filter(|(n, _)| valid_bot_name(n)).take(MAX_BOTS).collect();
            }
        }
        let names = self.bot_files();
        let mut changed = false;
        let before = inner.bots.len();
        inner.bots.retain(|n, _| names.contains(n));
        changed |= inner.bots.len() != before;
        for name in &names {
            let st = inner.bots.entry(name.clone()).or_default();
            changed |= self.consume(name, st, now);
        }
        if changed {
            let json = serde_json::to_string(&inner.bots).unwrap_or_default();
            if let Err(e) = crate::glance_store::write_private(&self.state_file, json.as_bytes()) {
                tracing::warn!(target: "cos.inbox", error = %e, "could not save the inbox cursors");
            }
        }
        changed
    }

    /// Names of the files to follow: plain `<valid-name>.md` files, no symlinks.
    fn bot_files(&self) -> Vec<String> {
        let Ok(rd) = fs::read_dir(&self.dir) else { return Vec::new() };
        let mut names: Vec<String> = rd
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
            .filter_map(|e| e.file_name().into_string().ok())
            .filter_map(|n| n.strip_suffix(".md").map(str::to_string))
            .filter(|stem| valid_bot_name(stem))
            .collect();
        names.sort();
        names.truncate(MAX_BOTS);
        names
    }

    /// Consume new complete lines of one file. Returns whether state changed.
    fn consume(&self, name: &str, st: &mut BotState, now: DateTime<Utc>) -> bool {
        let path = self.dir.join(format!("{name}.md"));
        let Ok(mut f) = fs::File::open(&path) else { return false };
        let Ok(meta) = f.metadata() else { return false };
        let sig = (meta.modified().unwrap_or(SystemTime::UNIX_EPOCH), meta.len());
        if st.sig == Some(sig) {
            return false;
        }
        let mut changed = false;
        let replaced = meta.len() < st.offset
            || (st.inode.is_some() && inode_of(&meta) != st.inode)
            || (st.offset > 0 && head_hash(&mut f, st.offset).ok().as_deref() != Some(st.head.as_str()));
        if replaced {
            *st = BotState::default();
            changed = true;
        }
        st.inode = inode_of(&meta);
        if f.seek(SeekFrom::Start(st.offset)).is_err() {
            return changed;
        }
        let mut buf = Vec::new();
        if f.by_ref().take(MAX_READ as u64).read_to_end(&mut buf).is_err() {
            return changed;
        }
        // Only complete lines count. A full chunk with no newline is one
        // oversize line: take it whole, the text cap trims it.
        let used = match buf.iter().rposition(|b| *b == b'\n') {
            Some(i) => i + 1,
            None if buf.len() == MAX_READ => buf.len(),
            None => 0,
        };
        let more_waiting = st.offset + (used as u64) < meta.len() && used > 0;
        if used > 0 {
            for line in String::from_utf8_lossy(&buf[..used]).lines() {
                if let Some(e) = parse_line(line, now) {
                    st.entries.push(e);
                    changed = true;
                }
            }
            if st.entries.len() > KEEP {
                let drop = st.entries.len() - KEEP;
                st.entries.drain(..drop);
            }
            st.offset += used as u64;
            st.head = head_hash(&mut f, st.offset).unwrap_or_default();
            changed = true;
        }
        // Leave the signature unset while a backlog remains so the next poll continues.
        st.sig = (!more_waiting).then_some(sig);
        changed
    }

    /// Every bot with at least one entry, in name order.
    pub fn bots(&self) -> Vec<BotView> {
        let inner = self.lock();
        inner
            .bots
            .iter()
            .filter_map(|(bot, st)| {
                let last = st.entries.last()?;
                Some(BotView {
                    bot: bot.clone(),
                    status: last.status.clone(),
                    updated: last.at.clone(),
                    title: last.text.clone(),
                    entries: st.entries.iter().rev().cloned().collect(),
                })
            })
            .collect()
    }

    /// The newest update across all bots: `(bot, entry)`.
    pub fn latest(&self) -> Option<(String, Entry)> {
        let inner = self.lock();
        inner
            .bots
            .iter()
            .filter_map(|(bot, st)| st.entries.last().map(|e| (bot, e)))
            .max_by_key(|(_, e)| DateTime::parse_from_rfc3339(&e.at).ok())
            .map(|(b, e)| (b.clone(), e.clone()))
    }

    /// Create the folder and poll it forever, on a blocking thread per scan.
    pub fn spawn_watcher(&self, every: Duration) -> tokio::task::JoinHandle<()> {
        let inbox = self.clone();
        tokio::spawn(async move {
            if let Err(e) = fs::create_dir_all(&inbox.dir) {
                tracing::warn!(target: "cos.inbox", dir = %inbox.dir.display(), error = %e, "cannot create the inbox folder");
            }
            loop {
                let scanner = inbox.clone();
                match tokio::task::spawn_blocking(move || scanner.scan()).await {
                    Ok(true) => tracing::info!(target: "cos.inbox", "inbox updated"),
                    Ok(false) => {}
                    Err(e) => tracing::warn!(target: "cos.inbox", error = %e, "inbox scan failed"),
                }
                tokio::time::sleep(every).await;
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_parse_with_optional_time_and_status() {
        let now = Utc::now();
        let e = parse_line("2026-11-17T19:00:00Z [blocked] waiting on CI", now).unwrap();
        assert_eq!((e.at.as_str(), e.status.as_deref(), e.text.as_str()), ("2026-11-17T19:00:00Z", Some("blocked"), "waiting on CI"));
        let e = parse_line("[nonsense] hello", now).unwrap();
        assert_eq!((e.status, e.text.as_str()), (None, "[nonsense] hello"));
        assert!(parse_line("   ", now).is_none() && parse_line("# note", now).is_none());
    }

    #[test]
    fn future_timestamps_are_clamped_and_controls_stripped() {
        let now = Utc::now();
        let e = parse_line("2099-01-01T00:00:00Z a\u{1b}[31mb\u{202e}c", now).unwrap();
        assert_eq!(e.at, now.to_rfc3339_opts(SecondsFormat::Secs, true));
        assert_eq!(e.text, "a[31mbc");
    }

    #[test]
    fn bot_names_are_validated() {
        assert!(valid_bot_name("grok-1_a") && !valid_bot_name("") && !valid_bot_name("a.b") && !valid_bot_name("a b") && !valid_bot_name(&"x".repeat(65)));
    }
}
