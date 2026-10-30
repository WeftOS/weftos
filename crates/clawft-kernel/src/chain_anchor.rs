//! Chain-head external anchoring beyond [`MockAnchor`](crate::chain::MockAnchor).
//!
//! Implements ADR-041 / WEFT-137:
//! - [`FileLedgerAnchor`] — durable append-only hash-linked local ledger
//!   (useful production default until a public ledger is chosen)
//! - [`ExternalLedgerAnchor`] — wired external-ledger stub (intent log +
//!   optional endpoint binding; network POST deferred until a target is
//!   selected)
//! - [`AnchoringController`] — frequency policy (`min_interval_secs`,
//!   `min_events_between`) over any [`ChainAnchor`](crate::chain::ChainAnchor)
//!
//! Config surface: [`clawft_types::config::ChainExternalAnchorConfig`] on
//! `kernel.chain.external_anchor`.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use rvf_crypto::hash::shake256_256;

use crate::chain::{AnchorReceipt, ChainAnchor, ChainManager, MockAnchor};
use clawft_types::config::{ChainAnchorBackend, ChainExternalAnchorConfig};

/// Chain event source of project anchors: `project.anchor` on the user chain
/// (the daemon's record of an accepted statement) and `project.anchored` on
/// the project chain (the acknowledgement). Reserved in
/// `project_identity::RESERVED_SOURCES` so a `chain.append` caller cannot
/// forge either.
pub const ANCHOR_SOURCE: &str = "project.anchor";

#[cfg(feature = "native")]
#[path = "chain_anchor_parent.rs"]
mod parent;
#[cfg(feature = "native")]
pub use parent::{
    AnchorAck, AnchorSubmitError, ParentAnchor, ParentAnchorConfig, ParentTransport,
    KIND_ANCHORED, KIND_ANCHOR,
};

// ── Helpers ──────────────────────────────────────────────────────────────

fn hex_hash(h: &[u8; 32]) -> String {
    h.iter().map(|b| format!("{b:02x}")).collect()
}

fn parse_hex_hash(s: &str) -> Result<[u8; 32], String> {
    if s.len() != 64 {
        return Err(format!("expected 64 hex chars, got {}", s.len()));
    }
    let mut out = [0u8; 32];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        let hi = hex_nibble(chunk[0])?;
        let lo = hex_nibble(chunk[1])?;
        out[i] = (hi << 4) | lo;
    }
    Ok(out)
}

fn hex_nibble(c: u8) -> Result<u8, String> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(format!("invalid hex digit: {}", c as char)),
    }
}

// ── File ledger entry ────────────────────────────────────────────────────

/// One line of the append-only file ledger (JSONL).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileLedgerEntry {
    /// Monotonic ledger sequence (1-based).
    pub seq: u64,
    /// Hex-encoded SHAKE-256 of the previous entry (zeros for genesis).
    pub prev_entry_hash: String,
    /// Hex-encoded 32-byte hash that was anchored (ExoChain head).
    pub anchored_hash: String,
    /// Hex-encoded SHAKE-256 of this entry's commitment bytes.
    pub entry_hash: String,
    /// RFC3339 timestamp of the anchor operation.
    pub anchored_at: DateTime<Utc>,
}

impl FileLedgerEntry {
    /// Commitment bytes: `seq_be ‖ prev_entry_hash ‖ anchored_hash ‖ ts_secs_be`.
    fn commitment(seq: u64, prev: &[u8; 32], anchored: &[u8; 32], ts_secs: i64) -> [u8; 32] {
        let mut buf = Vec::with_capacity(8 + 32 + 32 + 8);
        buf.extend_from_slice(&seq.to_be_bytes());
        buf.extend_from_slice(prev);
        buf.extend_from_slice(anchored);
        buf.extend_from_slice(&ts_secs.to_be_bytes());
        shake256_256(&buf)
    }

    fn tx_id(seq: u64) -> String {
        format!("file-ledger-{seq}")
    }
}

// ── FileLedgerAnchor ─────────────────────────────────────────────────────

/// Append-only, hash-linked local ledger of chain-head anchors.
///
/// Each successful [`ChainAnchor::anchor`] appends one JSONL line whose
/// `entry_hash` commits to `(seq, prev_entry_hash, anchored_hash, timestamp)`.
/// [`ChainAnchor::verify`] reloads the ledger, finds the receipt's `tx_id`,
/// and checks both the stored hash and the entry commitment.
///
/// This is the production-useful default backend until a public external
/// ledger (OpenTimestamps, Ethereum, …) is selected per deployment.
pub struct FileLedgerAnchor {
    path: PathBuf,
    lock: Mutex<()>,
}

impl FileLedgerAnchor {
    /// Open (or create) a ledger at `path`. Parent directories are created.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, String> {
        let path = path.into();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)
                .map_err(|e| format!("create ledger dir {}: {e}", parent.display()))?;
        }
        if !path.exists() {
            File::create(&path).map_err(|e| format!("create ledger {}: {e}", path.display()))?;
        }
        Ok(Self {
            path,
            lock: Mutex::new(()),
        })
    }

    /// Path of the underlying JSONL ledger.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Load all ledger entries (best-effort parse; skips blank lines).
    pub fn load_entries(&self) -> Result<Vec<FileLedgerEntry>, String> {
        let file = File::open(&self.path)
            .map_err(|e| format!("open ledger {}: {e}", self.path.display()))?;
        let reader = BufReader::new(file);
        let mut out = Vec::new();
        for (lineno, line) in reader.lines().enumerate() {
            let line = line.map_err(|e| format!("read ledger line {}: {e}", lineno + 1))?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let entry: FileLedgerEntry = serde_json::from_str(trimmed)
                .map_err(|e| format!("parse ledger line {}: {e}", lineno + 1))?;
            out.push(entry);
        }
        Ok(out)
    }

    /// Verify the entire ledger's hash-link integrity.
    pub fn verify_chain(&self) -> Result<bool, String> {
        let entries = self.load_entries()?;
        let mut prev = [0u8; 32];
        for entry in &entries {
            let anchored = parse_hex_hash(&entry.anchored_hash)?;
            let prev_stored = parse_hex_hash(&entry.prev_entry_hash)?;
            if prev_stored != prev {
                return Ok(false);
            }
            let expected = FileLedgerEntry::commitment(
                entry.seq,
                &prev,
                &anchored,
                entry.anchored_at.timestamp(),
            );
            let entry_hash = parse_hex_hash(&entry.entry_hash)?;
            if entry_hash != expected {
                return Ok(false);
            }
            prev = entry_hash;
        }
        Ok(true)
    }

    fn append_entry(&self, hash: &[u8; 32]) -> Result<AnchorReceipt, String> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| "file ledger lock poisoned".to_string())?;

        let entries = self.load_entries()?;
        let (seq, prev) = if let Some(last) = entries.last() {
            (last.seq + 1, parse_hex_hash(&last.entry_hash)?)
        } else {
            (1u64, [0u8; 32])
        };

        let anchored_at = Utc::now();
        let entry_hash = FileLedgerEntry::commitment(seq, &prev, hash, anchored_at.timestamp());
        let entry = FileLedgerEntry {
            seq,
            prev_entry_hash: hex_hash(&prev),
            anchored_hash: hex_hash(hash),
            entry_hash: hex_hash(&entry_hash),
            anchored_at,
        };

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| format!("append ledger {}: {e}", self.path.display()))?;
        let line = serde_json::to_string(&entry)
            .map_err(|e| format!("serialize ledger entry: {e}"))?;
        writeln!(file, "{line}").map_err(|e| format!("write ledger entry: {e}"))?;
        file.sync_all()
            .map_err(|e| format!("sync ledger {}: {e}", self.path.display()))?;

        Ok(AnchorReceipt {
            hash: *hash,
            tx_id: FileLedgerEntry::tx_id(seq),
            anchored_at,
        })
    }
}

impl ChainAnchor for FileLedgerAnchor {
    fn anchor(&self, hash: &[u8; 32]) -> Result<AnchorReceipt, String> {
        self.append_entry(hash)
    }

    fn verify(&self, receipt: &AnchorReceipt) -> Result<bool, String> {
        let entries = self.load_entries()?;
        let Some(entry) = entries
            .iter()
            .find(|e| e.tx_id_matches(&receipt.tx_id) || FileLedgerEntry::tx_id(e.seq) == receipt.tx_id)
        else {
            return Ok(false);
        };
        let stored = parse_hex_hash(&entry.anchored_hash)?;
        if stored != receipt.hash {
            return Ok(false);
        }
        // Recompute commitment for the found entry in ledger context.
        let prev = parse_hex_hash(&entry.prev_entry_hash)?;
        let expected = FileLedgerEntry::commitment(
            entry.seq,
            &prev,
            &receipt.hash,
            entry.anchored_at.timestamp(),
        );
        let entry_hash = parse_hex_hash(&entry.entry_hash)?;
        Ok(entry_hash == expected)
    }

    fn backend_name(&self) -> &str {
        "file-ledger"
    }
}

impl FileLedgerEntry {
    fn tx_id_matches(&self, tx_id: &str) -> bool {
        FileLedgerEntry::tx_id(self.seq) == tx_id
    }
}

// ── ExternalLedgerAnchor (stub with real wiring) ─────────────────────────

/// Intent-log entry for the external-ledger stub.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExternalIntentEntry {
    pub seq: u64,
    pub anchored_hash: String,
    pub tx_id: String,
    pub endpoint: Option<String>,
    pub submitted: bool,
    pub anchored_at: DateTime<Utc>,
}

/// Wired external-ledger stub.
///
/// Always persists an intent to a local JSONL log. When `endpoint` is set,
/// the receipt records the binding (`external-stub:{endpoint}:{seq}`) so a
/// future HTTP/gRPC transport can submit without changing the trait surface.
/// Network POST is intentionally not performed until a ledger target is
/// chosen (ADR-041 defers chain selection).
pub struct ExternalLedgerAnchor {
    path: PathBuf,
    endpoint: Option<String>,
    lock: Mutex<()>,
}

impl ExternalLedgerAnchor {
    /// Create a stub bound to `path` and optional `endpoint`.
    pub fn open(path: impl Into<PathBuf>, endpoint: Option<String>) -> Result<Self, String> {
        let path = path.into();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)
                .map_err(|e| format!("create external intent dir {}: {e}", parent.display()))?;
        }
        if !path.exists() {
            File::create(&path)
                .map_err(|e| format!("create external intent log {}: {e}", path.display()))?;
        }
        Ok(Self {
            path,
            endpoint,
            lock: Mutex::new(()),
        })
    }

    /// Configured external endpoint (if any).
    pub fn endpoint(&self) -> Option<&str> {
        self.endpoint.as_deref()
    }

    /// Intent log path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Load all recorded intents.
    pub fn load_intents(&self) -> Result<Vec<ExternalIntentEntry>, String> {
        let file = File::open(&self.path)
            .map_err(|e| format!("open external intent log {}: {e}", self.path.display()))?;
        let reader = BufReader::new(file);
        let mut out = Vec::new();
        for (lineno, line) in reader.lines().enumerate() {
            let line = line.map_err(|e| format!("read intent line {}: {e}", lineno + 1))?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let entry: ExternalIntentEntry = serde_json::from_str(trimmed)
                .map_err(|e| format!("parse intent line {}: {e}", lineno + 1))?;
            out.push(entry);
        }
        Ok(out)
    }
}

impl ChainAnchor for ExternalLedgerAnchor {
    fn anchor(&self, hash: &[u8; 32]) -> Result<AnchorReceipt, String> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| "external ledger lock poisoned".to_string())?;

        let intents = self.load_intents()?;
        let seq = intents.last().map(|e| e.seq + 1).unwrap_or(1);
        let anchored_at = Utc::now();
        let tx_id = match &self.endpoint {
            Some(ep) => format!("external-stub:{ep}:{seq}"),
            None => format!("external-stub:pending:{seq}"),
        };
        // `submitted` is false until a real transport is wired; endpoint
        // presence only means the binding is configured.
        let entry = ExternalIntentEntry {
            seq,
            anchored_hash: hex_hash(hash),
            tx_id: tx_id.clone(),
            endpoint: self.endpoint.clone(),
            submitted: false,
            anchored_at,
        };

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| format!("append external intent {}: {e}", self.path.display()))?;
        let line = serde_json::to_string(&entry)
            .map_err(|e| format!("serialize external intent: {e}"))?;
        writeln!(file, "{line}").map_err(|e| format!("write external intent: {e}"))?;
        file.sync_all()
            .map_err(|e| format!("sync external intent {}: {e}", self.path.display()))?;

        Ok(AnchorReceipt {
            hash: *hash,
            tx_id,
            anchored_at,
        })
    }

    fn verify(&self, receipt: &AnchorReceipt) -> Result<bool, String> {
        let intents = self.load_intents()?;
        Ok(intents.iter().any(|e| {
            e.tx_id == receipt.tx_id && parse_hex_hash(&e.anchored_hash).ok() == Some(receipt.hash)
        }))
    }

    fn backend_name(&self) -> &str {
        "external-stub"
    }
}

// ── Frequency policy + controller ────────────────────────────────────────

/// Last successful anchor observed by the controller.
#[derive(Debug, Clone, Default)]
struct LastAnchorState {
    at: Option<DateTime<Utc>>,
    chain_seq: u64,
}

/// Frequency policy for external anchoring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnchorFrequencyPolicy {
    /// Minimum wall-clock duration between anchors (`0` = no time gate).
    pub min_interval: Duration,
    /// Minimum ExoChain sequence delta between anchors (`0` = no event gate).
    pub min_events_between: u64,
}

impl AnchorFrequencyPolicy {
    /// Build from config fields.
    pub fn from_config(cfg: &ChainExternalAnchorConfig) -> Self {
        Self {
            min_interval: Duration::from_secs(cfg.min_interval_secs),
            min_events_between: cfg.min_events_between,
        }
    }

    /// Whether both gates permit an anchor at `now` for `chain_seq`.
    fn allows(&self, last: &LastAnchorState, now: DateTime<Utc>, chain_seq: u64) -> bool {
        if self.min_events_between > 0 {
            let delta = chain_seq.saturating_sub(last.chain_seq);
            // First anchor (last.chain_seq == 0 and never anchored) always allowed
            // on the event gate only when we have never anchored, or delta is enough.
            if last.at.is_some() && delta < self.min_events_between {
                return false;
            }
        }
        if !self.min_interval.is_zero()
            && let Some(at) = last.at
        {
            let elapsed = now
                .signed_duration_since(at)
                .to_std()
                .unwrap_or(Duration::ZERO);
            if elapsed < self.min_interval {
                return false;
            }
        }
        true
    }
}

/// Policy-gated wrapper around a [`ChainAnchor`] backend.
pub struct AnchoringController {
    backend: Arc<dyn ChainAnchor>,
    policy: AnchorFrequencyPolicy,
    last: Mutex<LastAnchorState>,
}

impl AnchoringController {
    /// Wrap an existing backend with a frequency policy.
    pub fn new(backend: Arc<dyn ChainAnchor>, policy: AnchorFrequencyPolicy) -> Self {
        Self {
            backend,
            policy,
            last: Mutex::new(LastAnchorState::default()),
        }
    }

    /// Build from kernel config. Returns `None` when backend is `None`.
    pub fn from_config(cfg: &ChainExternalAnchorConfig) -> Result<Option<Self>, String> {
        let backend: Arc<dyn ChainAnchor> = match cfg.backend {
            ChainAnchorBackend::None => return Ok(None),
            ChainAnchorBackend::Mock => Arc::new(MockAnchor),
            ChainAnchorBackend::File => {
                let path = cfg
                    .effective_ledger_path()
                    .ok_or_else(|| {
                        "file-ledger: ledger_path required (no default available)".to_string()
                    })?;
                Arc::new(FileLedgerAnchor::open(path)?)
            }
            ChainAnchorBackend::External => {
                let path = cfg.effective_ledger_path().ok_or_else(|| {
                    "external-stub: ledger_path required for intent log (no default available)"
                        .to_string()
                })?;
                Arc::new(ExternalLedgerAnchor::open(path, cfg.endpoint.clone())?)
            }
            ChainAnchorBackend::Parent => {
                return Err(
                    "parent anchor: needs the project key and a parent link; \
                     project boot builds ParentAnchor and wraps it with AnchoringController::new"
                        .to_string(),
                );
            }
        };
        Ok(Some(Self::new(
            backend,
            AnchorFrequencyPolicy::from_config(cfg),
        )))
    }

    /// Backend name for logging/display.
    pub fn backend_name(&self) -> &str {
        self.backend.backend_name()
    }

    /// Frequency policy in force.
    pub fn policy(&self) -> AnchorFrequencyPolicy {
        self.policy
    }

    /// Underlying backend (for direct verify / tests).
    pub fn backend(&self) -> &dyn ChainAnchor {
        self.backend.as_ref()
    }

    /// Anchor only when the frequency policy allows it.
    ///
    /// Returns `Ok(None)` when the policy suppresses the call (not an error).
    pub fn try_anchor(
        &self,
        hash: &[u8; 32],
        chain_seq: u64,
    ) -> Result<Option<AnchorReceipt>, String> {
        let now = Utc::now();
        {
            let last = self
                .last
                .lock()
                .map_err(|_| "anchor policy lock poisoned".to_string())?;
            if !self.policy.allows(&last, now, chain_seq) {
                return Ok(None);
            }
        }
        let receipt = self.backend.anchor(hash)?;
        let mut last = self
            .last
            .lock()
            .map_err(|_| "anchor policy lock poisoned".to_string())?;
        last.at = Some(receipt.anchored_at);
        last.chain_seq = chain_seq;
        Ok(Some(receipt))
    }

    /// Force an anchor, bypassing the frequency policy (still updates state).
    pub fn force_anchor(
        &self,
        hash: &[u8; 32],
        chain_seq: u64,
    ) -> Result<AnchorReceipt, String> {
        let receipt = self.backend.anchor(hash)?;
        let mut last = self
            .last
            .lock()
            .map_err(|_| "anchor policy lock poisoned".to_string())?;
        last.at = Some(receipt.anchored_at);
        last.chain_seq = chain_seq;
        Ok(receipt)
    }

    /// Anchor the current ExoChain head when policy allows.
    ///
    /// On success, also appends a `chain.external_anchor` audit event to
    /// the local chain so operators can see the receipt on the ExoChain.
    pub fn try_anchor_chain_head(
        &self,
        chain: &ChainManager,
    ) -> Result<Option<AnchorReceipt>, String> {
        let hash = chain.head_hash();
        let seq = chain.sequence();
        let Some(receipt) = self.try_anchor(&hash, seq)? else {
            return Ok(None);
        };
        // Best-effort audit event; ignore if chain is locked somehow.
        let _ = chain.append(
            "chain",
            "external_anchor",
            Some(serde_json::json!({
                "backend": self.backend_name(),
                "tx_id": receipt.tx_id,
                "hash": hex_hash(&receipt.hash),
                "anchored_at": receipt.anchored_at.to_rfc3339(),
                "chain_seq": seq,
            })),
        );
        Ok(Some(receipt))
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn temp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("clawft-weft-137-tests");
        let _ = fs::create_dir_all(&dir);
        dir.join(format!(
            "{name}-{}-{}.jsonl",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ))
    }

    #[test]
    fn file_ledger_anchor_roundtrip_and_chain() {
        let path = temp_path("file-roundtrip");
        let _ = fs::remove_file(&path);
        let anchor = FileLedgerAnchor::open(&path).expect("open");
        let h1 = [0x11u8; 32];
        let h2 = [0x22u8; 32];

        let r1 = anchor.anchor(&h1).expect("anchor1");
        assert_eq!(r1.hash, h1);
        assert!(r1.tx_id.starts_with("file-ledger-"));
        assert!(anchor.verify(&r1).expect("verify1"));

        let r2 = anchor.anchor(&h2).expect("anchor2");
        assert!(anchor.verify(&r2).expect("verify2"));
        assert!(anchor.verify_chain().expect("chain ok"));

        let entries = anchor.load_entries().expect("load");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].seq, 1);
        assert_eq!(entries[1].seq, 2);
        assert_ne!(entries[0].entry_hash, entries[1].entry_hash);

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn file_ledger_verify_rejects_unknown_tx() {
        let path = temp_path("file-unknown");
        let _ = fs::remove_file(&path);
        let anchor = FileLedgerAnchor::open(&path).expect("open");
        let hash = [7u8; 32];
        let _ = anchor.anchor(&hash).expect("anchor");
        let bogus = AnchorReceipt {
            hash,
            tx_id: "file-ledger-999".into(),
            anchored_at: Utc::now(),
        };
        assert!(!anchor.verify(&bogus).expect("verify"));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn file_ledger_detects_tamper() {
        let path = temp_path("file-tamper");
        let _ = fs::remove_file(&path);
        let anchor = FileLedgerAnchor::open(&path).expect("open");
        let _ = anchor.anchor(&[9u8; 32]).expect("anchor");
        // Corrupt the file.
        fs::write(&path, "{\"seq\":1,\"prev_entry_hash\":\"00\",\"anchored_hash\":\"xx\",\"entry_hash\":\"yy\",\"anchored_at\":\"2020-01-01T00:00:00Z\"}\n")
            .expect("corrupt");
        assert!(anchor.verify_chain().is_err() || !anchor.verify_chain().unwrap_or(true));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn external_stub_records_intent_with_endpoint() {
        let path = temp_path("ext-intent");
        let _ = fs::remove_file(&path);
        let anchor = ExternalLedgerAnchor::open(
            &path,
            Some("https://ledger.example/anchor".into()),
        )
        .expect("open");
        assert_eq!(anchor.backend_name(), "external-stub");
        let hash = [0xABu8; 32];
        let receipt = anchor.anchor(&hash).expect("anchor");
        assert!(receipt.tx_id.contains("ledger.example"));
        assert!(anchor.verify(&receipt).expect("verify"));
        let intents = anchor.load_intents().expect("load");
        assert_eq!(intents.len(), 1);
        assert_eq!(intents[0].endpoint.as_deref(), Some("https://ledger.example/anchor"));
        assert!(!intents[0].submitted);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn external_stub_pending_without_endpoint() {
        let path = temp_path("ext-pending");
        let _ = fs::remove_file(&path);
        let anchor = ExternalLedgerAnchor::open(&path, None).expect("open");
        let receipt = anchor.anchor(&[1u8; 32]).expect("anchor");
        assert!(receipt.tx_id.contains("pending"));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn frequency_policy_event_gate() {
        let policy = AnchorFrequencyPolicy {
            min_interval: Duration::ZERO,
            min_events_between: 10,
        };
        let last = LastAnchorState {
            at: Some(Utc::now()),
            chain_seq: 5,
        };
        assert!(!policy.allows(&last, Utc::now(), 10)); // delta 5 < 10
        assert!(policy.allows(&last, Utc::now(), 15)); // delta 10
        // Never anchored: always allow
        assert!(policy.allows(&LastAnchorState::default(), Utc::now(), 1));
    }

    #[test]
    fn frequency_policy_time_gate() {
        let policy = AnchorFrequencyPolicy {
            min_interval: Duration::from_secs(60),
            min_events_between: 0,
        };
        let last = LastAnchorState {
            at: Some(Utc::now()),
            chain_seq: 1,
        };
        assert!(!policy.allows(&last, Utc::now(), 100));
        let later = Utc::now() + chrono::Duration::seconds(61);
        assert!(policy.allows(&last, later, 100));
    }

    #[test]
    fn controller_try_anchor_respects_policy() {
        let path = temp_path("ctrl-policy");
        let _ = fs::remove_file(&path);
        let backend = Arc::new(FileLedgerAnchor::open(&path).expect("open"));
        let ctrl = AnchoringController::new(
            backend,
            AnchorFrequencyPolicy {
                min_interval: Duration::ZERO,
                min_events_between: 5,
            },
        );
        let h = [3u8; 32];
        let r1 = ctrl.try_anchor(&h, 1).expect("a1").expect("first allowed");
        assert!(ctrl.backend().verify(&r1).expect("v"));
        // Same sequence delta too small
        assert!(ctrl.try_anchor(&h, 3).expect("a2").is_none());
        // Enough events
        let r3 = ctrl.try_anchor(&[4u8; 32], 10).expect("a3");
        assert!(r3.is_some());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn controller_from_config_file() {
        let path = temp_path("cfg-file");
        let _ = fs::remove_file(&path);
        let cfg = ChainExternalAnchorConfig {
            backend: ChainAnchorBackend::File,
            ledger_path: Some(path.to_string_lossy().into_owned()),
            endpoint: None,
            min_interval_secs: 0,
            min_events_between: 0,
        };
        let ctrl = AnchoringController::from_config(&cfg)
            .expect("build")
            .expect("some");
        assert_eq!(ctrl.backend_name(), "file-ledger");
        let receipt = ctrl
            .force_anchor(&[0xFFu8; 32], 42)
            .expect("force");
        assert!(ctrl.backend().verify(&receipt).expect("verify"));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn controller_from_config_none_returns_none() {
        let cfg = ChainExternalAnchorConfig {
            backend: ChainAnchorBackend::None,
            ..Default::default()
        };
        assert!(AnchoringController::from_config(&cfg)
            .expect("ok")
            .is_none());
    }

    #[test]
    fn controller_from_config_mock() {
        let cfg = ChainExternalAnchorConfig {
            backend: ChainAnchorBackend::Mock,
            ledger_path: None,
            endpoint: None,
            min_interval_secs: 0,
            min_events_between: 0,
        };
        let ctrl = AnchoringController::from_config(&cfg)
            .expect("build")
            .expect("some");
        assert_eq!(ctrl.backend_name(), "mock");
        let r = ctrl.force_anchor(&[0u8; 32], 1).expect("anchor");
        assert!(ctrl.backend().verify(&r).expect("v"));
    }

    #[test]
    fn controller_from_config_external() {
        let path = temp_path("cfg-ext");
        let _ = fs::remove_file(&path);
        let cfg = ChainExternalAnchorConfig {
            backend: ChainAnchorBackend::External,
            ledger_path: Some(path.to_string_lossy().into_owned()),
            endpoint: Some("https://ots.example".into()),
            min_interval_secs: 0,
            min_events_between: 0,
        };
        let ctrl = AnchoringController::from_config(&cfg)
            .expect("build")
            .expect("some");
        assert_eq!(ctrl.backend_name(), "external-stub");
        let r = ctrl.try_anchor(&[5u8; 32], 1).expect("a").expect("some");
        assert!(r.tx_id.contains("ots.example"));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn try_anchor_chain_head_emits_audit_event() {
        let path = temp_path("head-audit");
        let _ = fs::remove_file(&path);
        let cm = ChainManager::new(0, 1000);
        cm.append("kernel", "boot.init", None);
        let head = cm.head_hash();

        let backend = Arc::new(FileLedgerAnchor::open(&path).expect("open"));
        let ctrl = AnchoringController::new(
            backend,
            AnchorFrequencyPolicy {
                min_interval: Duration::ZERO,
                min_events_between: 0,
            },
        );
        let receipt = ctrl
            .try_anchor_chain_head(&cm)
            .expect("anchor")
            .expect("some");
        assert_eq!(receipt.hash, head);

        // Audit event should be on the chain.
        let events = cm.tail(0);
        let found = events.iter().any(|e| e.kind == "external_anchor");
        assert!(found, "expected chain.external_anchor event");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn mock_anchor_still_works_via_controller() {
        let ctrl = AnchoringController::new(
            Arc::new(MockAnchor),
            AnchorFrequencyPolicy {
                min_interval: Duration::ZERO,
                min_events_between: 0,
            },
        );
        let r = ctrl.force_anchor(&[42u8; 32], 0).expect("a");
        assert!(r.tx_id.starts_with("mock-"));
    }
}
