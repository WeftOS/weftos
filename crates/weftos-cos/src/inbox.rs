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
//! A backlog is not paged through: on first sight of a file, after a reset, or
//! when more than [`MAX_BACKLOG`] bytes are waiting, the cursor jumps to the
//! last [`MAX_READ`] bytes (aligned to a line start) and only those lines are
//! kept; the skipped older lines are dropped, since only the newest
//! [`KEEP`] are ever shown. One overlong line is one truncated entry; the rest
//! of it is discarded up to its newline.
//!
//! Known limit: a bot that rewrites a file in place (not append, not rename)
//! while keeping the first 256 bytes and the length at or past the cursor is
//! not detected; the cursor then lands mid-line and yields a fragment. Bots
//! must append, or replace the file by rename.
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
/// Most bytes consumed from one file per poll.
pub const MAX_READ: usize = 16 * 1024;
/// A backlog beyond this many bytes is skipped to the tail instead of drained.
pub const MAX_BACKLOG: usize = 8 * MAX_READ;
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
    /// Inside an overlong line whose start was already taken: drop bytes to its newline.
    #[serde(default)]
    discard: bool,
    #[serde(skip)]
    sig: Option<(SystemTime, u64)>,
}

#[derive(Default, Debug)]
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
    /// Held for a whole scan so scans never overlap; file IO happens under
    /// this, never under `inner`, which readers (the glance) take briefly.
    scan_lock: Arc<Mutex<()>>,
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

/// Whether `c` is invisible or reorders text: format characters (Unicode
/// category Cf, including the tag block that can smuggle hidden ASCII),
/// variation selectors and the line and paragraph separators.
fn is_invisible(c: char) -> bool {
    matches!(c,
        '\u{00AD}' | '\u{0600}'..='\u{0605}' | '\u{061C}' | '\u{06DD}' | '\u{070F}' | '\u{0890}'..='\u{0891}' | '\u{08E2}'
        | '\u{180B}'..='\u{180F}' | '\u{200B}'..='\u{200F}' | '\u{2028}'..='\u{202E}' | '\u{2060}'..='\u{206F}'
        | '\u{FE00}'..='\u{FE0F}' | '\u{FEFF}' | '\u{FFF9}'..='\u{FFFB}' | '\u{110BD}' | '\u{110CD}'
        | '\u{13430}'..='\u{1343F}' | '\u{1BCA0}'..='\u{1BCA3}' | '\u{1D173}'..='\u{1D17A}'
        | '\u{E0000}'..='\u{E007F}' | '\u{E0100}'..='\u{E01EF}')
}

/// Strip control, format and other invisible characters and cut to `max` characters.
pub fn clean(s: &str, max: usize) -> String {
    s.chars()
        .map(|c| if c == '\t' { ' ' } else { c })
        .filter(|c| !c.is_control() && !is_invisible(*c))
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

/// Open for reading without blocking on a FIFO that replaced the file after the check.
fn open_read(path: &Path) -> std::io::Result<fs::File> {
    let mut opts = fs::OpenOptions::new();
    opts.read(true);
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(0o4000); // O_NONBLOCK
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(0x4); // O_NONBLOCK
    }
    opts.open(path)
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
        Self { dir, state_file, scan_lock: Arc::default(), inner: Arc::default() }
    }

    /// The watched folder.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("inbox lock poisoned")
    }

    /// Look at the folder once and consume what was appended. Returns whether
    /// anything changed. If the folder cannot be listed (missing, unmounted,
    /// permission flap) nothing changes and the next poll tries again.
    pub fn scan(&self) -> bool {
        self.scan_at(Utc::now())
    }

    /// Load the saved cursors once, outside the `inner` lock.
    fn load_once(&self) {
        if self.lock().loaded {
            return;
        }
        let saved = fs::read_to_string(&self.state_file).ok().and_then(|t| serde_json::from_str::<BTreeMap<String, BotState>>(&t).ok());
        let mut inner = self.lock();
        inner.loaded = true;
        if let Some(saved) = saved {
            inner.bots = saved.into_iter().filter(|(n, _)| valid_bot_name(n)).take(MAX_BOTS).collect();
        }
    }

    /// [`Inbox::scan`] with an explicit clock.
    pub fn scan_at(&self, now: DateTime<Utc>) -> bool {
        let _serial = self.scan_lock.lock().expect("inbox scan lock poisoned");
        self.load_once();
        let names = match self.bot_files() {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(target: "cos.inbox", dir = %self.dir.display(), error = %e, "cannot list the inbox folder; keeping the cursors");
                return false;
            }
        };
        // Work on a copy so file IO never holds the lock the glance takes.
        let mut bots = self.lock().bots.clone();
        let before = bots.len();
        bots.retain(|n, _| names.contains(n));
        let mut changed = bots.len() != before;
        for name in &names {
            let st = bots.entry(name.clone()).or_default();
            changed |= self.consume(name, st, now);
        }
        if changed {
            let json = serde_json::to_string(&bots).unwrap_or_default();
            self.lock().bots = bots;
            if let Err(e) = crate::glance_store::write_private(&self.state_file, json.as_bytes()) {
                tracing::warn!(target: "cos.inbox", error = %e, "could not save the inbox cursors");
            }
        }
        changed
    }

    /// Names of the files to follow: plain `<valid-name>.md` files, no symlinks.
    fn bot_files(&self) -> std::io::Result<Vec<String>> {
        let rd = fs::read_dir(&self.dir)?;
        let mut names: Vec<String> = rd
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
            .filter_map(|e| e.file_name().into_string().ok())
            .filter_map(|n| n.strip_suffix(".md").map(str::to_string))
            .filter(|stem| valid_bot_name(stem))
            .collect();
        names.sort();
        names.truncate(MAX_BOTS);
        Ok(names)
    }

    /// Consume new complete lines of one file. Returns whether state changed.
    fn consume(&self, name: &str, st: &mut BotState, now: DateTime<Utc>) -> bool {
        let path = self.dir.join(format!("{name}.md"));
        // Only regular files: a symlink or FIFO is skipped before it can block the open.
        if !fs::symlink_metadata(&path).is_ok_and(|m| m.is_file()) {
            return false;
        }
        let Ok(mut f) = open_read(&path) else { return false };
        let Ok(meta) = f.metadata() else { return false };
        if !meta.is_file() {
            return false;
        }
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
        // A big backlog is skipped to the tail: read the last MAX_READ bytes
        // plus the byte before them, to tell whether they start on a line.
        let limit = if st.offset == 0 { MAX_READ } else { MAX_BACKLOG } as u64;
        let jump = meta.len().saturating_sub(st.offset) > limit;
        let (base, cap) = if jump { (meta.len() - MAX_READ as u64 - 1, MAX_READ + 1) } else { (st.offset, MAX_READ) };
        if f.seek(SeekFrom::Start(base)).is_err() {
            return changed;
        }
        let mut buf = Vec::new();
        if f.by_ref().take(cap as u64).read_to_end(&mut buf).is_err() {
            return changed;
        }
        // Bytes to drop before the first line: the tail of a line we skipped into,
        // or the rest of an overlong line already taken.
        let newline = buf.iter().position(|b| *b == b'\n');
        let lead = if jump {
            st.discard = newline.is_none();
            newline.map_or(buf.len(), |i| i + 1)
        } else if st.discard {
            st.discard = newline.is_none();
            newline.map_or(buf.len(), |i| i + 1)
        } else {
            0
        };
        let body = &buf[lead..];
        // Only complete lines count. A full chunk with no newline is one
        // overlong line: keep its start as one entry, then discard to its newline.
        let (used, overlong) = match body.iter().rposition(|b| *b == b'\n') {
            Some(i) => (i + 1, false),
            None if body.len() >= MAX_READ => (body.len(), true),
            None => (0, false),
        };
        for line in String::from_utf8_lossy(&body[..used]).lines() {
            if let Some(e) = parse_line(line, now) {
                st.entries.push(e);
            }
        }
        if overlong {
            st.discard = true;
        }
        if st.entries.len() > KEEP {
            let drop = st.entries.len() - KEEP;
            st.entries.drain(..drop);
        }
        let advance = (lead + used) as u64;
        if advance > 0 {
            st.offset = base + advance;
            st.head = head_hash(&mut f, st.offset).unwrap_or_default();
            changed = true;
        }
        // Leave the signature unset while a backlog remains so the next poll continues.
        st.sig = (!(advance > 0 && st.offset < meta.len())).then_some(sig);
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
