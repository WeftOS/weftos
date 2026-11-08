//! The daemon's per-client store (ADR-126 S5): `ClientStore` started on a daemon that runs as
//! a client account's unix user.
//!
//! The daemon opens a client store only when `<runtime dir>/client-store.json` names an
//! account, and only as the uid it is actually running as: the spec takes `geteuid()`, so a
//! daemon running as another user (or as `weftos`, whose uid is below the client range) is
//! refused by the spec, and a root directory the process does not own is refused by
//! `verify_on_disk`. Without the file nothing starts and `store.client.*` answers that there is
//! no client store. The `store.*` verbs of S1 are unaffected.

use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use clawft_kernel::exposure::registry::now_ms;
use clawft_kernel::exposure::{AuditFn, ExposureRegistry};
use serde::Deserialize;
use weftos_store::audit::AuditSink;
use weftos_store::service::{ChainSink, ExposureGate};

use crate::client_store::ClientStore;
use crate::client_store_snapshot::{Rotation, RotatingKey, SnapshotKey};
use crate::client_store_state::{EventLog, GateState, JournalState};
#[cfg(feature = "exochain")]
use crate::client_store_state::LoggedEvent;
use crate::store_account::ClientStoreSpec;
use crate::store_rt::{ActivitySink, ChainAudit, RegistryGate};
use crate::weftos_uri::Authority;

/// Name of the config file in the runtime dir.
pub const CONFIG_FILE: &str = "client-store.json";
/// How often expired cross-mesh grants are swept out of the service.
const SWEEP_EVERY: Duration = Duration::from_secs(60);

/// Publish a gate snapshot once this many changes have piled up since the last one.
const COMPACT_EVERY: u64 = 256;

static GLOBAL: OnceLock<Arc<ClientStore>> = OnceLock::new();

/// The daemon's client store, once started.
pub fn global() -> Option<Arc<ClientStore>> {
    GLOBAL.get().cloned()
}

/// `client-store.json`: which account this daemon serves, and the loopback port RustFS will use.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientStoreConfig {
    pub account: String,
    #[serde(default = "default_port")]
    pub port: u16,
}

fn default_port() -> u16 {
    9100
}

/// Parse the config file's text.
pub fn parse_config(text: &str) -> Result<ClientStoreConfig, String> {
    serde_json::from_str(text).map_err(|e| format!("{CONFIG_FILE}: {e}"))
}

/// A mesh id as 64 lowercase hex, or `None` (`local` before the licence store has one).
fn mesh_of(t: &str) -> Option<Authority> {
    let ok = t.len() == 64 && t.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    ok.then(|| hex::decode(t).ok()).flatten().and_then(|v| <[u8; 32]>::try_from(v).ok()).map(Authority)
}

/// The daemon's chain as the gate's event log: gate changes are `store.client.gate.*` events.
#[cfg(feature = "exochain")]
pub struct ChainLog(pub Arc<clawft_kernel::chain::ChainManager>);

#[cfg(feature = "exochain")]
impl EventLog for ChainLog {
    fn append(&self, source: &str, kind: &str, payload: serde_json::Value) -> u64 {
        self.0.append(source, kind, Some(payload)).sequence
    }
    fn all(&self) -> Vec<LoggedEvent> {
        self.0
            .tail(0)
            .into_iter()
            .map(|e| LoggedEvent { seq: e.sequence, source: e.source, kind: e.kind, payload: e.payload })
            .collect()
    }
}

/// The chain key rotations in `events` (`identity.key_rotation`, dual-signed by the old and new
/// key). One that does not verify, or whose keys are malformed, is ignored.
#[cfg(feature = "exochain")]
pub fn rotations_in(events: &[LoggedEvent]) -> Vec<Rotation> {
    use clawft_kernel::key_rotation::{KIND_KEY_ROTATION, KeyRotationPayload, SOURCE_IDENTITY, verify_key_rotation_payload};
    events
        .iter()
        .filter(|e| e.source == SOURCE_IDENTITY && e.kind == KIND_KEY_ROTATION)
        .filter_map(|e| {
            let p = KeyRotationPayload::from_event_payload(e.payload.as_ref()?).ok()?;
            verify_key_rotation_payload(&p).ok()?;
            Some(Rotation { old: KeyRotationPayload::decode_pubkey(&p.old_pubkey_hex).ok()?, new: KeyRotationPayload::decode_pubkey(&p.new_pubkey_hex).ok()?, seq: e.seq })
        })
        .collect()
}

/// The daemon's chain key as the gate's snapshot key. It reads the chain's current signing key
/// and rotation history at each use, so a rotation while the daemon runs is followed: new
/// snapshots are signed by the new key and ones signed by a retired key still verify if they
/// were written before it was rotated out.
#[cfg(feature = "exochain")]
pub struct ChainKey(pub Arc<clawft_kernel::chain::ChainManager>);

#[cfg(feature = "exochain")]
impl ChainKey {
    fn current(&self) -> Option<RotatingKey> {
        let key = self.0.signing_key_clone()?;
        Some(RotatingKey { key, rotations: rotations_in(&ChainLog(self.0.clone()).all()) })
    }
}

#[cfg(feature = "exochain")]
impl SnapshotKey for ChainKey {
    fn sign(&self, msg: &[u8]) -> Option<(String, Vec<u8>)> {
        self.current()?.sign(msg)
    }
    fn verify(&self, signer: &str, msg: &[u8], sig: &[u8], at_seq: u64) -> bool {
        self.current().is_some_and(|k| k.verify(signer, msg, sig, at_seq))
    }
}

/// Build the store for `cfg` as the process uid `euid`. Everything the daemon decides is here, so
/// it can be tested without a real client user: the spec, the mesh and the on-disk check.
/// With `log` the gate is rebuilt from, and recorded to, that event log (the chain); without
/// it the gate lives in `gate.json` under the client root.
#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
pub fn open_as(
    cfg: &ClientStoreConfig, euid: u32, mesh_token: &str, exposure: Arc<dyn ExposureGate>, audit: Arc<dyn AuditSink>,
    chain: Arc<dyn ChainSink>, log: Option<Arc<dyn EventLog>>, key: Option<Arc<dyn SnapshotKey>>,
) -> Result<ClientStore, String> {
    let mesh = mesh_of(mesh_token).ok_or_else(|| "this node has no mesh id yet; the client store waits for one".to_owned())?;
    let spec = ClientStoreSpec::standard(&cfg.account, euid, cfg.port).map_err(|e| e.to_string())?;
    let state = log.map(|l| {
        let j = JournalState::new(l, mesh, &cfg.account);
        Arc::new(match key {
            Some(k) => j.with_key(k),
            None => j,
        }) as Arc<dyn GateState>
    });
    ClientStore::open_fs_with(spec, mesh, exposure, audit, chain, state, now_ms).map_err(|e| e.to_string())
}

/// Start the client store if `dir` has a config. `Ok(None)` when it does not. `chain` is the
/// audit function S1's service records grants through; `log` is the gate's event log.
#[cfg(unix)]
pub fn start(
    dir: &Path, exposures: Arc<ExposureRegistry>, chain: Option<AuditFn>, log: Option<Arc<dyn EventLog>>,
    key: Option<Arc<dyn SnapshotKey>>,
) -> Result<Option<Arc<ClientStore>>, String> {
    if let Some(g) = global() {
        return Ok(Some(g));
    }
    let text = match std::fs::read_to_string(dir.join(CONFIG_FILE)) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{CONFIG_FILE}: {e}")),
    };
    let cfg = parse_config(&text)?;
    let euid = nix::unistd::geteuid().as_raw();
    let cs = open_as(
        &cfg, euid, &crate::store_rt::mesh_token(), Arc::new(RegistryGate(exposures.clone())), Arc::new(ActivitySink(exposures)),
        Arc::new(ChainAudit(chain)), log, key,
    )?;
    Ok(Some(GLOBAL.get_or_init(|| Arc::new(cs)).clone()))
}

/// Without unix there are no client uids to check: nothing starts.
#[cfg(not(unix))]
pub fn start(
    _dir: &Path, _exposures: Arc<ExposureRegistry>, _chain: Option<AuditFn>, _log: Option<Arc<dyn EventLog>>,
    _key: Option<Arc<dyn SnapshotKey>>,
) -> Result<Option<Arc<ClientStore>>, String> {
    Ok(None)
}

/// Sweep expired cross-mesh grants out of the service once a minute.
pub fn spawn_sweeper(cs: Arc<ClientStore>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(SWEEP_EVERY);
        loop {
            tick.tick().await;
            let c = cs.clone();
            let _ = tokio::task::spawn_blocking(move || {
                c.sweep("sweeper");
                c.compact_if_due(COMPACT_EVERY);
            })
            .await;
        }
    });
}

#[cfg(test)]
#[path = "client_store_rt_tests.rs"]
mod tests;
