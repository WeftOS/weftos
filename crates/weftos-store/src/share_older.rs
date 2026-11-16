//! Older-writer detection (WO-069 lane F1, ADR-126 gate item 8). A child of `share`.
//!
//! Gate item 8 wants every daemon that writes a worktree bucket to run the claim fence and the
//! conditional manifest write before a client project uses shared worktrees. An older daemon drops
//! the handoff fields the fence needs when it rewrites the record, never announces a publish, and
//! writes the manifest unconditionally. Nothing used to tell an operator that a bucket was last
//! written by one. This module reads the answer off what a current daemon ALWAYS leaves and an older
//! one cannot:
//!
//! - every handoff record a current daemon has written carries `publishing` (the announcement of its
//!   last publish; claims, handoffs and merges keep it, and it is marked `done`, never removed). A
//!   record without it was created or last rewritten by an older daemon.
//! - a claim a current daemon takes records `claimed_head`. A `Claimed` record without it is an older
//!   daemon's claim.
//! - a finished announcement (`done`) names the tree digest of the manifest it wrote. A manifest
//!   whose digest differs was replaced afterwards by a writer that does not announce.
//!
//! This build in schema-1 mode (`WEFTOS_WORKTREE_MANIFEST_SCHEMA=1`) still announces, so its output
//! is told apart from an rc.1 writer's. What it cannot tell apart: an older daemon's rewrite of the
//! record over a schema-1 manifest this build published (reported as `schema1_no_announcement`, which
//! says "older writer or the record rewritten by one"), and any older daemon that has not written
//! since this build did. No on-store format changes: the verdict is computed, never stored in the
//! manifest or the record. The registry entry carries the result (`writer_check`, additive, ignored by
//! older followers) so the verbs that read only the registry can show it without a store call.

use super::*;
use crate::share_fetch::RegistryEntry;

/// Feature level of this build's writer check. Written into every registry entry by a build that
/// runs the check; an entry without it was last written by a build that does not.
pub const WRITER_LEVEL: u32 = 1;

/// What the evidence points at.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OlderKind {
    /// `Claimed` without `claimed_head`: the claim was taken by a daemon from before head fencing.
    OldClaim,
    /// No publish announcement in the record, manifest not in hand.
    HandoffNoAnnouncement,
    /// A schema-1 manifest and no publish announcement.
    Schema1NoAnnouncement,
    /// A schema-2 manifest but no publish announcement: the record was rewritten by an older daemon.
    HandoffRewritten,
    /// The manifest is not the one the last finished announcement named.
    ManifestReplaced,
    /// A manifest and no handoff record.
    NoHandoff,
    /// Registry only: the entry carries no writer check, so a daemon without it wrote the entry.
    EntryFromOlderBuild,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OlderWriter {
    pub kind: OlderKind,
    /// One sentence for an operator.
    pub what: String,
    /// The observation it rests on.
    pub evidence: String,
}

impl OlderWriter {
    fn new(kind: OlderKind, what: &str, evidence: String) -> Self {
        OlderWriter { kind, what: what.into(), evidence }
    }

    /// The one-line warning the CLI prints for `slug`.
    pub fn warning_line(&self, slug: &str) -> String {
        format!(
            "worktree {slug} was last written by an older daemon ({}). Upgrade that writer before a client project uses shared worktrees (ADR-126 section 4a, gate item 8). Evidence: {}",
            self.what, self.evidence
        )
    }
}

/// What a checking build records on the registry entry it writes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WriterCheck {
    pub level: u32,
    /// What the check found in the bucket when the entry was written. `None`: clean.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub older: Option<OlderWriter>,
}

/// The verdict for a bucket whose manifest and handoff record the caller holds (either may be
/// absent: `None` means not read or not there). `None` back: nothing points at an older writer, or
/// the worktree is landed or gone (nothing more is written to it).
pub fn detect_older_writer(m: Option<&TreeManifest>, h: Option<&Handoff>) -> Option<OlderWriter> {
    let Some(h) = h else {
        return m.map(|m| {
            OlderWriter::new(
                OlderKind::NoHandoff,
                "a manifest with no handoff record",
                format!("schema {} manifest published by {}@{}, no .wt/handoff.json", m.schema, m.writer.agent, m.writer.host),
            )
        });
    };
    if matches!(h.state, State::Merged | State::Archived | State::Orphan) {
        return None;
    }
    if h.state == State::Claimed && h.claimed_head.is_none() {
        let who = h.merger.as_ref().map_or_else(|| "someone".into(), |m| format!("{}@{}", m.agent, m.host));
        return Some(OlderWriter::new(
            OlderKind::OldClaim,
            "the claim was taken by a daemon from before head fencing",
            format!("record is claimed by {who} at epoch {} with no claimed_head", h.epoch),
        ));
    }
    let Some(i) = &h.publishing else {
        return Some(match m {
            Some(m) if m.is_pooled() => OlderWriter::new(
                OlderKind::HandoffRewritten,
                "the handoff record was last rewritten by an older daemon (a claim, handoff or merge) after this tree was published",
                format!("schema {} manifest, but the record has no publish announcement, which every record a current daemon wrote has", m.schema),
            ),
            Some(m) => OlderWriter::new(
                OlderKind::Schema1NoAnnouncement,
                "a schema-1 manifest with no publish announcement: published by an older daemon, or the record rewritten by one over a schema-1 publish of this build",
                format!("schema {} manifest, and the record has no publish announcement, which every record a current daemon wrote has", m.schema),
            ),
            None => OlderWriter::new(
                OlderKind::HandoffNoAnnouncement,
                "the handoff record has no publish announcement: created or last rewritten by an older daemon",
                "no `publishing` in the record (the manifest was not read)".into(),
            ),
        });
    };
    // A finished announcement names the manifest it wrote; a pending one is in flight or abandoned
    // (the manifest is then older than the record, not replaced), so it decides nothing.
    let m = m?;
    if i.done && i.tree != tree_digest(m) {
        return Some(OlderWriter::new(
            OlderKind::ManifestReplaced,
            "the published tree was replaced after the last announced publish, by a writer that does not announce",
            format!(
                "schema {} manifest (head {}) does not match the tree {} announced by {}@{} at {} ms",
                m.schema, m.head_commit, i.tree, i.writer.agent, i.writer.host, i.ms
            ),
        ));
    }
    None
}

/// The check a registry entry written now records.
pub fn writer_check_for(m: Option<&TreeManifest>, h: Option<&Handoff>) -> WriterCheck {
    WriterCheck { level: WRITER_LEVEL, older: detect_older_writer(m, h) }
}

/// The verdict a reader of registry entries shows for `e`: what the daemon that wrote it found, or
/// (no check recorded) that a daemon without the check wrote it. Landed and gone worktrees: none.
pub fn entry_older_writer(e: &RegistryEntry) -> Option<OlderWriter> {
    if matches!(e.state, State::Merged | State::Archived | State::Orphan) {
        return None;
    }
    match &e.writer_check {
        Some(c) => c.older.clone(),
        None => Some(OlderWriter::new(
            OlderKind::EntryFromOlderBuild,
            "the registry entry was last written by a daemon that predates the older-writer check",
            "the entry has no writer_check; a daemon with the check always writes one (the bucket itself is not read)".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn who() -> Actor {
        Actor { agent: "a".into(), host: "h".into() }
    }
    fn rec() -> Handoff {
        Handoff {
            state: State::Open,
            note: String::new(),
            writer: who(),
            merger: None,
            updated_ms: 1,
            epoch: 0,
            claimed_head: None,
            claimed_tree: None,
            publishing: None,
        }
    }

    #[test]
    fn the_warning_line_names_the_slug_the_gate_and_the_remedy() {
        let w = detect_older_writer(None, Some(&rec())).unwrap();
        let l = w.warning_line("feat-x");
        assert!(l.contains("feat-x") && l.contains("gate item 8") && l.contains("Upgrade that writer"), "{l}");
        assert!(!l.contains('\n'));
    }

    #[test]
    fn the_wire_form_is_snake_case_and_a_clean_check_omits_the_verdict() {
        let w = detect_older_writer(None, Some(&rec())).unwrap();
        let v = serde_json::to_value(&w).unwrap();
        assert_eq!(v["kind"], "handoff_no_announcement");
        let c = serde_json::to_value(WriterCheck { level: 1, older: None }).unwrap();
        assert_eq!(c, serde_json::json!({"level": 1}));
    }
}
