//! The host's instance journal: what a node needs to put its placed
//! workloads back after the daemon restarts.
//!
//! A host keeps its instances in memory, so a restart used to empty the
//! node: the staged binaries stayed on disk, nothing ran, and placing the
//! cog again failed on the stale stage directory. The journal records, for
//! each placed instance, the `place` body, who placed it and whether it was
//! meant to be running. At startup [`WorkloadHostService::restore_instances`]
//! places each one again from the package already in this node's artifact
//! store, through the same path (and the same governance) as the original
//! `place`: a new instance id is never invented, a new cogbus token and
//! ingest lease are issued, and a cog that was running is started.
//!
//! The file holds no secrets: no token, no key, only the request bodies. It
//! is trusted only as far as it can be checked:
//!
//! - **Signature.** The whole file (node id, generation, entries) is signed
//!   with the node key. A file edited by anything but this node, or pasted
//!   from another node, is refused whole and every instance in it reported.
//! - **Generation.** Each save bumps a generation, writes the file, and then
//!   records the generation on the chain (`workload.journal`). A restart
//!   refuses a file whose generation is below the chain's latest, so an
//!   older signed copy (a stopped instance as it was when it ran) cannot be
//!   put back. With no chain to check against, nothing is auto-started: the
//!   instances are restored loaded and the report says why.
//! - **Controller.** The journaled requester must still be a controller
//!   under the node's current policy, or the instance is refused.
//!
//! - **What the chain proves.** The generation is tamper-evidence only up to
//!   the chain's own durability: the chain is written out at a clean shutdown,
//!   so after a crash it can be older than the journal, and an attacker who can
//!   roll the chain back as well as the file defeats the check (owner card
//!   a93f7abe makes the chain durable per event). `workload.journal` is a
//!   reserved event kind: peers cannot replicate it and the `chain.append` door
//!   refuses it, so only this node's own saves count.
//! - **A missing file.** If the chain records journal generations for this node
//!   but the file is gone (deleted, or set aside as unreadable), nothing is
//!   restored and the restore report, the log and the host's status say
//!   "journal missing", rather than silently starting an empty node.
//!
//! A restore that fails for a reason that may pass (the package not in the
//! store yet, the runtime failing) keeps the entry and counts the failure;
//! only a permanent refusal (verification, governance, authorisation, a bad
//! request) or [`MAX_RESTORE_TRIES`] failures drop it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey, Verifier};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::host_service::{HOST_CHAIN_SOURCE, PlaceBody, WorkloadHostService};
use super::msg::{CTL_VERSION, CtlRequest, Refusal, RefusalCode, method};
use crate::workload_governance::gate::REVOCATION_UNREADABLE;
use crate::chain::{ChainManager, EVENT_KIND_WORKLOAD_JOURNAL};
use crate::mesh_artifact_transfer::PeerSet;
use crate::workload_pkg::codec::{hex_decode_exact, hex_encode};

/// Journal file version.
const VERSION: u32 = 2;
/// Largest journal read, bytes.
const MAX_BYTES: u64 = 4 * 1024 * 1024;
/// Restores that may fail transiently before an entry is given up on.
pub const MAX_RESTORE_TRIES: u32 = 5;

/// One placed instance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JournalEntry {
    pub instance_id: String,
    pub decision_id: Option<String>,
    /// Controller node that placed it.
    pub requester: String,
    /// What was placed (its `start` is replaced by `desired_running` on restore).
    pub body: PlaceBody,
    /// It was meant to be running.
    pub desired_running: bool,
    /// Restores that failed for a reason that may pass.
    #[serde(default)]
    pub failures: u32,
}

#[derive(Serialize, Deserialize)]
struct File {
    version: u32,
    /// Raised on every save; the latest is also on the chain.
    generation: u64,
    entries: Vec<JournalEntry>,
    /// Hex signature of the node key over [`signed_bytes`].
    sig: String,
}

/// What is signed: a domain tag, the node id, the generation and the entries.
fn signed_bytes(node_id: &str, generation: u64, entries: &[JournalEntry]) -> Vec<u8> {
    let mut m = format!("weftos.workload-host.journal.v2\n{node_id}\n{generation}\n").into_bytes();
    m.extend(serde_json::to_vec(entries).unwrap_or_default());
    m
}

/// The newest journal generation the chain holds for this node. Only events
/// this node appended count: `workload.journal` is a reserved kind, refused
/// from replication and from the `chain.append` and bridge doors.
fn chain_generation(chain: &ChainManager, node_id: &str) -> u64 {
    chain
        .tail(0)
        .iter()
        .filter(|e| e.kind == EVENT_KIND_WORKLOAD_JOURNAL && e.source == HOST_CHAIN_SOURCE)
        .filter_map(|e| e.payload.as_ref())
        .filter(|p| p["node"] == node_id)
        .filter_map(|p| p["generation"].as_u64())
        .max()
        .unwrap_or(0)
}

/// Write `bytes` to `path` as a new file readable by this user only: the
/// mode is set when the file is created, so there is no moment it is wider.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    // A leftover file keeps its own (maybe wider) mode: start from nothing.
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

/// Unreadable journals kept (`journal.json.bad.<unix ms>`); older ones go.
const KEEP_BAD: usize = 3;

/// Move an unreadable journal out of the way under a timestamped name, so an
/// earlier one is never overwritten, and keep only the newest few.
fn set_aside(path: &Path) {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let target = path.with_file_name(format!("{name}.bad.{ms:020}"));
    let _ = std::fs::rename(path, &target);
    let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()).or(Some(Path::new("."))) else { return };
    let prefix = format!("{name}.bad.");
    let mut bad: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with(&prefix)))
        .collect();
    bad.sort();
    while bad.len() > KEEP_BAD {
        let _ = std::fs::remove_file(bad.remove(0));
    }
}

/// Flush a directory's entries to disk (after a rename into it).
fn sync_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::File::open(dir)?.sync_all()
}

#[cfg(test)]
pub(super) fn sync_dir_for_test(dir: &Path) -> std::io::Result<()> {
    sync_dir(dir)
}

struct State {
    entries: BTreeMap<String, JournalEntry>,
    generation: u64,
}

/// What opening the file found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OpenState {
    /// No file.
    Missing,
    /// A file that verified.
    Loaded,
    /// A file that did not parse (set aside as `.bad`).
    Unreadable,
    /// A file whose signature did not verify.
    Refused,
}

pub(super) struct Journal {
    path: PathBuf,
    node_id: String,
    key: SigningKey,
    state: Mutex<State>,
    chain: Mutex<Option<Arc<ChainManager>>>,
    /// The generation the file on disk had when opened.
    loaded_generation: u64,
    open_state: OpenState,
    /// Entries refused at open (instance id, reason); each is returned once.
    rejected: Mutex<Vec<(String, String)>>,
}

impl Journal {
    /// Open the journal at `path` for the node owning `key`. A missing file is
    /// empty. A file that does not parse is set aside (`.bad.<time>`, newest three kept) and the journal
    /// starts empty, so a damaged file never blocks the daemon. A file whose
    /// signature does not verify under this node's key and id is refused whole.
    pub(super) fn open(path: &Path, node_id: &str, key: &SigningKey) -> Self {
        let vk = key.verifying_key();
        let existed = std::fs::symlink_metadata(path).is_ok();
        let (entries, generation, rejected, open_state) = match read(path, node_id, &vk) {
            Ok((e, g, r)) if !existed => (e, g, r, OpenState::Missing),
            Ok((e, g, r)) if !r.is_empty() => (e, g, r, OpenState::Refused),
            Ok((e, g, r)) => (e, g, r, OpenState::Loaded),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "instance journal unreadable; starting empty");
                set_aside(path);
                (BTreeMap::new(), 0, Vec::new(), OpenState::Unreadable)
            }
        };
        for (id, why) in &rejected {
            tracing::warn!(instance = %id, reason = %why, "journal entry refused");
        }
        Self {
            path: path.to_path_buf(),
            node_id: node_id.to_string(),
            key: key.clone(),
            state: Mutex::new(State { entries, generation }),
            chain: Mutex::new(None),
            loaded_generation: generation,
            open_state,
            rejected: Mutex::new(rejected),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Record generations on `chain` from now on. A journal that restarts
    /// below the chain's generation (file deleted, then rewritten) continues
    /// above it, so the next restart does not take it for a replay.
    pub(super) fn set_chain(&self, chain: Arc<ChainManager>) {
        let latest = chain_generation(&chain, &self.node_id);
        {
            let mut g = self.lock();
            g.generation = g.generation.max(latest);
        }
        *self.chain.lock().unwrap_or_else(|p| p.into_inner()) = Some(chain);
    }

    /// The newest generation on the chain; `None` with no chain to ask.
    pub(super) fn chain_latest(&self) -> Option<u64> {
        let c = self.chain.lock().unwrap_or_else(|p| p.into_inner()).clone()?;
        Some(chain_generation(&c, &self.node_id))
    }

    pub(super) fn loaded_generation(&self) -> u64 {
        self.loaded_generation
    }

    pub(super) fn open_state(&self) -> OpenState {
        self.open_state
    }

    /// Test hook: pretend the journal has reached `generation`.
    #[cfg(test)]
    pub(super) fn set_generation(&self, generation: u64) {
        self.lock().generation = generation;
    }

    pub(super) fn entries(&self) -> Vec<JournalEntry> {
        self.lock().entries.values().cloned().collect()
    }

    /// Entries refused at open (instance id, reason); each is returned once.
    pub(super) fn take_rejected(&self) -> Vec<(String, String)> {
        std::mem::take(&mut *self.rejected.lock().unwrap_or_else(|p| p.into_inner()))
    }

    /// Forget every entry without saving (a replayed file: its entries were
    /// reported, and the next real change writes a fresh generation).
    pub(super) fn clear_in_memory(&self) {
        self.lock().entries.clear();
    }

    pub(super) fn record(&self, e: JournalEntry) {
        let mut g = self.lock();
        g.entries.insert(e.instance_id.clone(), e);
        self.save(&mut g);
    }

    pub(super) fn set_desired(&self, instance_id: &str, running: bool) {
        let mut g = self.lock();
        if let Some(e) = g.entries.get_mut(instance_id)
            && e.desired_running != running
        {
            e.desired_running = running;
            self.save(&mut g);
        }
    }

    pub(super) fn set_failures(&self, instance_id: &str, failures: u32) {
        let mut g = self.lock();
        if let Some(e) = g.entries.get_mut(instance_id)
            && e.failures != failures
        {
            e.failures = failures;
            self.save(&mut g);
        }
    }

    pub(super) fn remove(&self, instance_id: &str) {
        let mut g = self.lock();
        if g.entries.remove(instance_id).is_some() {
            self.save(&mut g);
        }
    }

    /// Write the next generation: the file first, then the chain. A crash
    /// between the two leaves the file ahead of the chain, which is accepted;
    /// the other order would leave it behind and look like a replay.
    fn save(&self, g: &mut State) {
        let Some(next) = g.generation.checked_add(1) else {
            tracing::error!(path = %self.path.display(), "instance journal generation overflowed; not saved");
            return;
        };
        g.generation = next;
        let entries: Vec<JournalEntry> = g.entries.values().cloned().collect();
        let sig = hex_encode(&self.key.sign(&signed_bytes(&self.node_id, g.generation, &entries)).to_bytes());
        let file = File {
            version: VERSION,
            generation: g.generation,
            entries,
            sig,
        };
        let tmp = self.path.with_extension("json.tmp");
        let write = || -> std::io::Result<()> {
            let text = serde_json::to_vec_pretty(&file).map_err(std::io::Error::other)?;
            write_private(&tmp, &text)?;
            std::fs::rename(&tmp, &self.path)?;
            // The rename is only durable once the directory entry is.
            sync_dir(self.path.parent().unwrap_or(Path::new(".")))
        };
        if let Err(e) = write() {
            tracing::warn!(path = %self.path.display(), error = %e, "instance journal not saved");
            return;
        }
        if let Some(c) = self.chain.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
            c.append(
                HOST_CHAIN_SOURCE,
                EVENT_KIND_WORKLOAD_JOURNAL,
                Some(json!({ "node": self.node_id, "generation": g.generation, "entries": file.entries.len() })),
            );
        }
    }
}

type Loaded = (BTreeMap<String, JournalEntry>, u64, Vec<(String, String)>);

fn read(path: &Path, node_id: &str, vk: &VerifyingKey) -> Result<Loaded, String> {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Default::default()),
        Err(e) => return Err(e.to_string()),
    };
    if meta.len() > MAX_BYTES {
        return Err("journal too large".into());
    }
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let f: File = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    if f.version != VERSION {
        return Err(format!("journal version {}", f.version));
    }
    let verified = hex_decode_exact::<64>(&f.sig)
        .map(|b| Signature::from_bytes(&b))
        .ok_or("signature missing or malformed")
        .and_then(|sig| {
            vk.verify(&signed_bytes(node_id, f.generation, &f.entries), &sig)
                .map_err(|_| "signature does not match: the journal was changed or is not this node's")
        });
    if let Err(why) = verified {
        let bad = f.entries.into_iter().map(|e| (e.instance_id, why.to_string())).collect();
        return Ok((BTreeMap::new(), 0, bad));
    }
    Ok((
        f.entries.into_iter().map(|e| (e.instance_id.clone(), e)).collect(),
        f.generation,
        Vec::new(),
    ))
}

/// How the journal stands, for the host's status and the restore report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct JournalStatus {
    /// `fresh`, `ok`, `unverified` (no chain to check against, or none of this
    /// journal on it), `replayed`, `missing`, `unreadable` or `refused`.
    pub state: String,
    /// The generation of the file when the node started.
    pub file_generation: u64,
    /// The newest generation on the chain; `None` with no chain.
    pub chain_generation: Option<u64>,
    /// Instances restored, and not restored.
    pub restored: usize,
    pub failed: usize,
}

/// What a restore did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestoreReport {
    /// Placed again.
    pub restored: Vec<String>,
    /// Not placed again, with the reason. A permanent refusal also drops the
    /// entry; a transient one keeps it for the next start.
    pub failed: Vec<(String, String)>,
    /// Something that applies to the whole restore (no chain to check the
    /// journal against: nothing was auto-started).
    pub note: Option<String>,
}

/// A refusal that retrying cannot change. A governance denial caused by the
/// revocation list being unreadable is the exception: that is the node's file
/// being bad now, not a verdict, so it is retried at the next start.
fn permanent(r: &Refusal) -> bool {
    match r.code {
        RefusalCode::Governance => !r.reason.contains(REVOCATION_UNREADABLE),
        RefusalCode::Signature
        | RefusalCode::Unauthorized
        | RefusalCode::InvalidRequest
        | RefusalCode::Verify
        | RefusalCode::UnknownMethod => true,
        _ => false,
    }
}

impl WorkloadHostService {
    /// Journal placed instances at `path`, so [`Self::restore_instances`]
    /// can put them back after a restart.
    pub fn with_journal(mut self, path: &Path) -> Self {
        let j = Journal::open(path, &self.node_id, &self.key);
        if let Some(c) = &self.chain {
            j.set_chain(c.clone());
        }
        self.journal = Some(j);
        self
    }

    /// Place every journaled instance again, started if it was meant to be
    /// running. Call once at startup, after the cogbus and ingest wiring are
    /// set. See the module docs for what is refused and what is retried.
    pub async fn restore_instances(&self) -> RestoreReport {
        let mut report = RestoreReport::default();
        let Some(j) = &self.journal else { return report };
        report.failed.extend(j.take_rejected());

        let file_generation = j.loaded_generation();
        let chain_latest = j.chain_latest();
        let status = |state: &str, report: &RestoreReport| JournalStatus {
            state: state.into(),
            file_generation,
            chain_generation: chain_latest,
            restored: report.restored.len(),
            failed: report.failed.len(),
        };

        // A file that is gone, or unusable, while the chain says this node kept one:
        // nothing to restore, and that must be loud, not an empty node.
        let gone = matches!(j.open_state(), OpenState::Missing | OpenState::Unreadable);
        if gone && chain_latest.is_some_and(|n| n > 0) {
            let state = if j.open_state() == OpenState::Missing { "missing" } else { "unreadable" };
            let why = format!(
                "journal {state}: the chain records generation {} but the file is {}; no placed instance was restored",
                chain_latest.unwrap_or(0),
                if state == "missing" { "absent" } else { "unreadable (set aside as .bad)" }
            );
            tracing::error!(path = %j.path.display(), "{why}");
            report.note = Some(why);
            self.set_journal_status(status(state, &report));
            return report;
        }

        // A file that failed its signature says so; its generation is not meaningful
        // (it was not trusted), so it must not be mistaken for a replay below.
        if j.open_state() == OpenState::Refused {
            let why = "journal refused (signature): the file was changed or is not this node's; no placed instance was restored".to_string();
            tracing::error!(path = %j.path.display(), "{why}");
            report.note = Some(why);
            self.set_journal_status(status("refused (signature)", &report));
            return report;
        }

        // Replay: a file older than what the chain says was saved last.
        let mut may_start = true;
        let mut state = if matches!(j.open_state(), OpenState::Missing) { "fresh" } else { "ok" };
        match chain_latest {
            Some(latest) if file_generation < latest => {
                let why = format!(
                    "the journal is generation {file_generation} but the chain records {latest}: an older copy of the file; refused"
                );
                for e in j.entries() {
                    report.failed.push((e.instance_id, why.clone()));
                }
                j.clear_in_memory();
                tracing::error!("{why}");
                report.note = Some(why);
                self.set_journal_status(status("replayed", &report));
                return report;
            }
            // A file claiming generations the chain has never heard of cannot be
            // told from a replayed or forged one: same as having no chain.
            Some(0) if file_generation > 0 => {
                may_start = false;
                state = "unverified";
                report.note = Some(
                    "the chain has no record of this journal (generation above 0, no workload.journal events): \
                     instances were restored loaded and none was started"
                        .into(),
                );
            }
            Some(_) => {}
            None => {
                may_start = false;
                state = "unverified";
                report.note = Some(
                    "no chain to check the journal against (a replayed copy could not be told from the real one): \
                     instances were restored loaded and none was started"
                        .into(),
                );
            }
        }

        for e in j.entries() {
            // The controller as the policy sees it now.
            if e.requester != self.node_id && !self.controllers.allows_node(&e.requester) {
                let why = format!("requester {} is no longer a controller of this node", e.requester);
                tracing::warn!(instance = %e.instance_id, reason = %why, "placed instance not restored; dropped");
                j.remove(&e.instance_id);
                report.failed.push((e.instance_id, why));
                continue;
            }
            let mut body = e.body.clone();
            body.start = e.desired_running && may_start;
            let now = super::host_service::now_ms();
            let req = CtlRequest {
                version: CTL_VERSION,
                method: method::PLACE.to_string(),
                requester: e.requester.clone(),
                target: self.node_id.clone(),
                nonce: "0".repeat(32),
                issued_at_ms: now,
                expires_at_ms: now + 60_000,
                decision_id: e.decision_id.clone(),
                body: json!(body),
            };
            // The package is in this node's own artifact store: no peers.
            let mut none = PeerSet::new();
            match self.place(&req, Some(&mut none)).await {
                Ok(_) => {
                    // `place` journaled the instance again with the intent the
                    // body carried; keep the operator's intent when it was held back.
                    if e.desired_running && !may_start {
                        j.set_desired(&e.instance_id, true);
                    }
                    j.set_failures(&e.instance_id, 0);
                    tracing::info!(instance = %e.instance_id, running = body.start, "placed instance restored");
                    report.restored.push(e.instance_id);
                }
                Err(r) if permanent(&r) => {
                    tracing::warn!(instance = %e.instance_id, reason = %r.reason, "placed instance refused; dropped from the journal");
                    j.remove(&e.instance_id);
                    report.failed.push((e.instance_id, r.reason));
                }
                Err(r) => {
                    let n = e.failures + 1;
                    if n >= MAX_RESTORE_TRIES {
                        tracing::warn!(instance = %e.instance_id, reason = %r.reason, tries = n, "placed instance given up on");
                        j.remove(&e.instance_id);
                        report.failed.push((e.instance_id, format!("{}; given up after {n} starts", r.reason)));
                    } else {
                        tracing::warn!(instance = %e.instance_id, reason = %r.reason, tries = n, "placed instance not restored; kept for the next start");
                        j.set_failures(&e.instance_id, n);
                        report.failed.push((e.instance_id, format!("{}; will retry at the next start ({n} of {MAX_RESTORE_TRIES})", r.reason)));
                    }
                }
            }
        }
        self.set_journal_status(status(state, &report));
        report
    }

    fn set_journal_status(&self, s: JournalStatus) {
        *self.journal_status.lock().unwrap_or_else(|p| p.into_inner()) = Some(s);
    }

    /// How the journal stood at the last restore; `None` before one ran, or
    /// with no journal.
    pub fn journal_status(&self) -> Option<JournalStatus> {
        self.journal_status.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Instances the host holds: `(id, desired_running)`.
    #[cfg(test)]
    pub(super) async fn held(&self) -> Vec<(String, bool)> {
        let mut v: Vec<_> = self
            .instances
            .lock()
            .await
            .iter()
            .map(|(i, p)| (i.clone(), p.desired_running))
            .collect();
        v.sort();
        v
    }
}
