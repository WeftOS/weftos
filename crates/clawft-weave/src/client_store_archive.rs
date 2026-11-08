//! Archiving the gate's compacted chain history before it is pruned (ADR-126 "Chain compaction").
//!
//! A snapshot keeps the gate's state, not who held what, when. Before the chain segment it
//! covers is pruned, the segment is written to a file here: one header line, then one line per
//! gate event in chain order. The header records the scope, `through_seq`, `event_count`,
//! `events_head` (the same hash chain a snapshot carries) and `range_sha256` (over the event
//! lines), so the file can be checked on its own and against the signed snapshot
//! ([`crate::client_store_state::JournalState::verify_archive`]). Pruning itself is the chain
//! owner's, never part of taking a snapshot or an archive.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::client_store_snapshot::{GateSnapshot, canonical, event_bytes, genesis_head, head_extend};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveHeader {
    pub scope: String,
    pub through_seq: u64,
    pub event_count: u64,
    pub events_head: String,
    /// SHA-256 over the event lines, each followed by `\n`.
    pub range_sha256: String,
}

/// One archived gate event: `(chain seq, kind suffix, ev)`.
pub type ArchivedEvent = (u64, String, Value);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveReport {
    pub path: PathBuf,
    pub header: ArchiveHeader,
}

/// A fixed-width name, so archives sort in chain order.
pub fn file_name(through_seq: u64) -> String {
    format!("gate-{through_seq:020}.jsonl")
}

fn event_line(seq: u64, kind: &str, ev: &Value) -> String {
    canonical(&json!({"seq": seq, "kind": kind, "ev": ev}))
}

/// Write the segment `events` (`(seq, kind, ev)` up to `snap.through_seq`) to `dir`, which is
/// created private. Refuses if the events do not reproduce the snapshot's count and head (the
/// chain no longer holds the whole segment), or if the file already exists.
pub fn write_archive(dir: &Path, snap: &GateSnapshot, events: &[(u64, &str, &Value)]) -> Result<ArchiveReport, String> {
    let (mut head, mut hasher, mut body) = (genesis_head(), Sha256::new(), String::new());
    for (seq, kind, ev) in events {
        head = head_extend(&head, &event_bytes(kind, ev));
        let line = event_line(*seq, kind, ev);
        hasher.update(line.as_bytes());
        hasher.update(b"\n");
        body.push_str(&line);
        body.push('\n');
    }
    if events.len() as u64 != snap.event_count || head != snap.events_head {
        return Err("the events on the chain do not reproduce the snapshot; refusing to archive a partial segment".into());
    }
    let header = ArchiveHeader {
        scope: snap.scope.clone(),
        through_seq: snap.through_seq,
        event_count: snap.event_count,
        events_head: head,
        range_sha256: hex::encode(hasher.finalize()),
    };
    let path = dir.join(file_name(snap.through_seq));
    if path.exists() {
        return Err(format!("{} already exists", path.display()));
    }
    create_private_dir(dir)?;
    let tmp = dir.join(format!(".{}.tmp", file_name(snap.through_seq)));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp).map_err(|e| e.to_string())?;
    let header_line = canonical(&serde_json::to_value(&header).map_err(|e| e.to_string())?);
    f.write_all(header_line.as_bytes()).and_then(|()| f.write_all(b"\n")).and_then(|()| f.write_all(body.as_bytes())).map_err(|e| e.to_string())?;
    f.sync_all().map_err(|e| e.to_string())?;
    drop(f);
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    Ok(ArchiveReport { path, header })
}

fn create_private_dir(dir: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).map_err(|e| e.to_string())
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())
    }
}

/// Read an archive and check it on its own: line count, order, head and range hash must all
/// match the header. Returns the header and the events.
pub fn read_archive(path: &Path) -> Result<(ArchiveHeader, Vec<ArchivedEvent>), String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let mut lines = text.lines();
    let header: ArchiveHeader = serde_json::from_str(lines.next().ok_or("empty archive")?).map_err(|e| format!("archive header: {e}"))?;
    let (mut head, mut hasher, mut events, mut last) = (genesis_head(), Sha256::new(), Vec::new(), 0u64);
    for l in lines {
        let v: Value = serde_json::from_str(l).map_err(|e| format!("archive line: {e}"))?;
        let (seq, kind, ev) = (
            v.get("seq").and_then(Value::as_u64).ok_or("archive line has no seq")?,
            v.get("kind").and_then(Value::as_str).ok_or("archive line has no kind")?.to_owned(),
            v.get("ev").cloned().unwrap_or(Value::Null),
        );
        if seq <= last {
            return Err("archive events are not in chain order".into());
        }
        last = seq;
        // The line must be the canonical form, or the range hash would not mean anything.
        if event_line(seq, &kind, &ev) != l {
            return Err("archive line is not in canonical form".into());
        }
        head = head_extend(&head, &event_bytes(&kind, &ev));
        hasher.update(l.as_bytes());
        hasher.update(b"\n");
        events.push((seq, kind, ev));
    }
    if events.len() as u64 != header.event_count || last > header.through_seq {
        return Err("archive count or range does not match its header".into());
    }
    if head != header.events_head {
        return Err("archive events do not reproduce the header's events_head".into());
    }
    if hex::encode(hasher.finalize()) != header.range_sha256 {
        return Err("archive range hash does not match".into());
    }
    Ok((header, events))
}
