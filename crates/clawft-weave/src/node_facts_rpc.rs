//! Daemon side of node facts (ADR-099 section 2, card mesh-placement-03).
//!
//! At boot the daemon probes this machine off the async runtime, signs the
//! facts with its node key (`<runtime>/node.key`), and caches them in
//! `ClusterMembership` as the local node (`trust_tier: pinned`). A
//! background task re-probes before the TTL runs out. `cluster.facts`
//! returns every fresh cached entry (local and verified peers) with its
//! signed envelope, so a client can re-verify the signature itself.
//!
//! Optional operator inputs in the runtime directory:
//! - `perf.measured.json`: conformance-harness output (`capabilities.json`
//!   or a `probe` document); only `measured` `perf.*` entries are used.
//! - `feeds.declared.json`: a list of `feed.*` capabilities, advertised as
//!   `claimed`.
//! - `facts.config.json`: `{"docker_probe_image": "alpine:3.20" | null}`, the
//!   local image the probe runs inside a VM-backed container engine to list
//!   emulated architectures (for example armv7). `null` disables it. The
//!   `WEFTOS_FACTS_PROBE_IMAGE` environment variable overrides the file
//!   (empty disables). Default `alpine:3.20`; it is never pulled.
//!
//! With a mesh runtime attached, each probe goes through
//! [`FactsExchange`](clawft_kernel::node_facts_exchange::FactsExchange): a
//! changed capability set becomes a new signed base, a live-state change a
//! small signed delta, and both are sent to every connected or joining peer.
//! The live probe reads free memory only; no probe sets a capability's
//! busy/free state, so those deltas appear only when something else (the
//! workload host) marks one. Peers' facts arrive on the same exchange and
//! are cached with a receiver-assigned trust tier.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use clawft_kernel::boot::Kernel;
use clawft_kernel::cluster::ClusterMembership;
use clawft_kernel::mesh_runtime::MeshRuntime;
use clawft_kernel::node_facts::{
    CachedNodeFacts, DEFAULT_FACTS_TTL_SECS, EmulationCache, ProbeConfig, SystemHost, build_facts,
    measured, probe_and_sign, probe_capabilities, refresh_live, valid_image_ref,
};
use clawft_kernel::node_facts_exchange::{FactsExchange, FactsTrustPolicy};
use clawft_platform::NativePlatform;
use clawft_rpc::Response;
use clawft_types::placement::{Capability, TrustTier};
use ed25519_dalek::SigningKey;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{info, warn};

/// Measured results file under the runtime dir.
pub const MEASURED_FILE: &str = "perf.measured.json";
/// Declared feeds file under the runtime dir.
pub const FEEDS_FILE: &str = "feeds.declared.json";
/// Model registry inside the runtime directory (card mesh-placement-17).
pub const MODEL_REGISTRY_FILE: &str = "models/registry.json";
/// Probe settings file under the runtime dir.
pub const CONFIG_FILE: &str = "facts.config.json";
/// Environment override for the container-engine probe image.
pub const PROBE_IMAGE_ENV: &str = "WEFTOS_FACTS_PROBE_IMAGE";
/// How often a connected node re-reads live state (free memory only: no
/// container engine, no privileged command).
pub const LIVE_PROBE_SECS: u64 = 60;
/// How often the full probe runs (80% of the facts TTL).
pub const FULL_PROBE_SECS: u64 = DEFAULT_FACTS_TTL_SECS * 4 / 5;
/// How long the privileged emulation (binfmt) listing is reused.
pub const EMULATION_CACHE_SECS: u64 = 3600;
/// Least gap between operator-forced probes (`cluster.facts` with `refresh`):
/// a forced probe inside it is answered from the facts the last one produced.
pub const MIN_FORCED_REFRESH_SECS: u64 = 30;
/// Longest one probe may run before it is abandoned and reported as failed.
pub const PROBE_TIMEOUT_SECS: u64 = 60;

/// One probe at a time, and a floor on how often an operator can force one.
///
/// Every refresh (the background tick and forced ones alike) takes the same
/// lock, so two never race to publish and the older `seq` cannot lose to the
/// newer one with a stale-facts error. A forced refresh that waited behind
/// another, or that comes inside `min` of the last forced one, does not probe
/// again.
struct RefreshGate {
    last_forced: tokio::sync::Mutex<Option<tokio::time::Instant>>,
    min: Duration,
    /// A probe running longer than this is abandoned (an engine that hangs
    /// must not hold the lock, and with it every later refresh, forever).
    timeout: Duration,
}

impl RefreshGate {
    fn new(min: Duration, timeout: Duration) -> Self {
        Self {
            last_forced: tokio::sync::Mutex::new(None),
            min,
            timeout,
        }
    }

    /// Run `probe` alone. `None`: a forced refresh was skipped because one
    /// ran less than `min` ago (including the one it queued behind).
    async fn run<F, Fut>(&self, forced: bool, probe: F) -> Result<Option<u64>, String>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<u64, String>>,
    {
        let mut last = self.last_forced.lock().await;
        if forced && last.is_some_and(|at| at.elapsed() < self.min) {
            return Ok(None);
        }
        let result = match tokio::time::timeout(self.timeout, probe()).await {
            Ok(r) => r,
            Err(_) => Err(format!("probe timed out after {}s", self.timeout.as_secs())),
        };
        if forced {
            // A failed attempt counts too: a failing probe is not retried in a loop.
            *last = Some(tokio::time::Instant::now());
        }
        result.map(Some)
    }
}

struct LocalSigner {
    key: SigningKey,
    runtime_dir: PathBuf,
    last_seq: Mutex<u64>,
    exchange: Option<Arc<FactsExchange>>,
    /// The last full probe and when it ran: the base live refreshes start from.
    last_full: Mutex<Option<(std::time::Instant, clawft_types::placement::NodeFacts)>>,
    emulation: Arc<EmulationCache>,
    /// The kernel revocation list models are checked against.
    revocations: Option<Arc<clawft_kernel::revocation::RevocationList>>,
    gate: RefreshGate,
    /// Unix seconds of the last forced probe that ran (0: none yet).
    last_forced_at: std::sync::atomic::AtomicU64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FactsConfig {
    /// Absent: keep the default. `null`: no probe image.
    #[serde(default, deserialize_with = "some_or_null")]
    docker_probe_image: Option<Option<String>>,
}

fn some_or_null<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Option<String>>, D::Error> {
    Option::<String>::deserialize(d).map(Some)
}

/// The probe image: environment, then `facts.config.json`, then `default`.
fn probe_image(runtime_dir: &Path, env: Option<String>, default: Option<String>) -> Option<String> {
    let checked = |v: Option<String>| {
        v.filter(|v| !v.trim().is_empty()).filter(|v| {
            let ok = valid_image_ref(v);
            if !ok {
                warn!(image = %v, "probe image is not a plain image reference; no probe image used");
            }
            ok
        })
    };
    if let Some(v) = env {
        return checked(Some(v));
    }
    if let Some(text) = read_small(&runtime_dir.join(CONFIG_FILE)) {
        match serde_json::from_str::<FactsConfig>(&text) {
            Ok(FactsConfig { docker_probe_image: Some(v) }) => {
                return checked(v);
            }
            Ok(_) => {}
            Err(e) => warn!(error = %e, "facts config unreadable; ignored"),
        }
    }
    default
}

static LOCAL: OnceLock<Arc<LocalSigner>> = OnceLock::new();
static INIT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn read_small(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() as usize > measured::MAX_MEASURED_BYTES {
        warn!(path = %path.display(), "node facts input too large; ignored");
        return None;
    }
    std::fs::read_to_string(path).ok()
}

/// Build the probe config from the runtime directory's optional inputs.
pub fn probe_config(runtime_dir: &Path) -> ProbeConfig {
    probe_config_with(runtime_dir, None)
}

/// [`probe_config`] with the kernel's revocation list. Models are advertised
/// only when the daemon has trust anchors and a revocation list to check them
/// against; without either, no models are advertised.
pub fn probe_config_with(
    runtime_dir: &Path,
    revocations: Option<&Arc<clawft_kernel::revocation::RevocationList>>,
) -> ProbeConfig {
    let mut cfg = ProbeConfig::default();
    cfg.docker_probe_image = probe_image(
        runtime_dir,
        std::env::var(PROBE_IMAGE_ENV).ok(),
        cfg.docker_probe_image.take(),
    );
    if let Some(text) = read_small(&runtime_dir.join(MEASURED_FILE)) {
        match measured::parse_measured(&text) {
            Ok(m) => {
                if m.skipped > 0 {
                    warn!(
                        skipped = m.skipped,
                        "measured results: non-perf or invalid entries skipped"
                    );
                }
                cfg.measured = m.caps;
            }
            Err(e) => warn!(error = %e, "measured results unreadable; ignored"),
        }
    }
    if let Some(text) = read_small(&runtime_dir.join(FEEDS_FILE)) {
        match serde_json::from_str::<Vec<Capability>>(&text) {
            Ok(feeds) => cfg.declared_feeds = feeds,
            Err(e) => warn!(error = %e, "declared feeds unreadable; ignored"),
        }
    }
    cfg.models = advertised_models(runtime_dir, revocations);
    cfg
}

/// The `model.present` / store capabilities for the node's model registry,
/// checked against the daemon's trust: pinned anchors from the runtime dir
/// plus the kernel revocation list. A revoked or unattested model advertises
/// no hashes (see `model_capabilities`). With no registry, no anchors or no
/// revocation list, nothing is advertised rather than advertising unchecked.
#[cfg(all(feature = "ecc", feature = "exochain", feature = "placement", unix))]
pub fn advertised_models(
    runtime_dir: &Path,
    revocations: Option<&Arc<clawft_kernel::revocation::RevocationList>>,
) -> Vec<Capability> {
    use clawft_kernel::model_manifest::{ModelRegistry, ModelTrust, TierResolver, model_capabilities};
    let registry_path = runtime_dir.join(MODEL_REGISTRY_FILE);
    if !registry_path.exists() {
        return Vec::new();
    }
    let Some(list) = revocations else {
        warn!("no revocation list yet; models are not advertised");
        return Vec::new();
    };
    let anchors = match crate::workload_place_policy::load_anchors(runtime_dir) {
        Ok(a) if !a.signers.is_empty() => a,
        Ok(_) => {
            warn!("no pinned model signers; models are not advertised");
            return Vec::new();
        }
        Err(e) => {
            warn!(error = %e, "trust anchors unreadable; models are not advertised");
            return Vec::new();
        }
    };
    match ModelRegistry::open(&registry_path) {
        Ok(reg) => {
            let reg = reg.with_trust(ModelTrust { anchors, revocations: Some(list.clone()) });
            model_capabilities(&reg, &TierResolver::system_default())
        }
        Err(e) => {
            warn!(error = %e, "model registry unreadable; models not advertised");
            Vec::new()
        }
    }
}

/// Without model support compiled in, no models are advertised.
#[cfg(not(all(feature = "ecc", feature = "exochain", feature = "placement", unix)))]
pub fn advertised_models(
    _runtime_dir: &Path,
    _revocations: Option<&Arc<clawft_kernel::revocation::RevocationList>>,
) -> Vec<Capability> {
    Vec::new()
}

/// Probe this machine into unsigned facts. Blocking: call off the runtime.
fn probe_blocking(signer: &LocalSigner) -> clawft_types::placement::NodeFacts {
    let mut cfg = probe_config_with(&signer.runtime_dir, signer.revocations.as_ref());
    cfg.emulation_cache = Some(signer.emulation.clone());
    let node_id = clawft_kernel::node_id_from_pubkey(&signer.key.verifying_key().to_bytes());
    let facts = build_facts(
        &node_id,
        now_secs(),
        DEFAULT_FACTS_TTL_SECS,
        0,
        probe_capabilities(&SystemHost::default(), &cfg),
    );
    *signer.last_full.lock().unwrap_or_else(|p| p.into_inner()) =
        Some((std::time::Instant::now(), facts.clone()));
    facts
}

/// Facts for this tick: a cheap live refresh of the last full probe, or a
/// full probe when asked, when none exists or when it is due.
fn facts_blocking(signer: &LocalSigner, full: bool) -> clawft_types::placement::NodeFacts {
    let base = signer
        .last_full
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone()
        .filter(|(at, _)| at.elapsed() < Duration::from_secs(FULL_PROBE_SECS));
    match base {
        Some((_, base)) if !full => refresh_live(&SystemHost::default(), &base, now_secs()),
        _ => probe_blocking(signer),
    }
}

/// Probe, sign and cache this node's facts. Blocking: call off the runtime.
fn refresh_blocking(signer: &LocalSigner, membership: &ClusterMembership) -> Result<u64, String> {
    let cfg = probe_config_with(&signer.runtime_dir, signer.revocations.as_ref());
    let now = now_secs();
    let seq = {
        let mut last = signer.last_seq.lock().map_err(|_| "seq lock poisoned")?;
        *last = (*last + 1).max(now);
        *last
    };
    let signed = probe_and_sign(
        &SystemHost::default(),
        &cfg,
        &signer.key,
        now,
        seq,
        DEFAULT_FACTS_TTL_SECS,
    )
    .map_err(|e| e.to_string())?;
    membership
        .facts()
        .insert(signed, TrustTier::Pinned, now)
        .map_err(|e| e.to_string())?;
    Ok(seq)
}

/// Probe and publish once, alone. `full` forces a full probe and counts as an
/// operator-forced refresh. `Ok(None)`: skipped, a forced probe ran moments ago.
async fn refresh(
    signer: Arc<LocalSigner>,
    membership: Arc<ClusterMembership>,
    full: bool,
) -> Result<Option<u64>, String> {
    let s = signer.clone();
    signer
        .gate
        .run(full, move || probe_and_publish(s, membership, full))
        .await
}

async fn probe_and_publish(
    signer: Arc<LocalSigner>,
    membership: Arc<ClusterMembership>,
    full: bool,
) -> Result<u64, String> {
    if let Some(exchange) = signer.exchange.clone() {
        let s = signer.clone();
        let facts = tokio::task::spawn_blocking(move || facts_blocking(&s, full))
            .await
            .map_err(|e| format!("probe task failed: {e}"))?;
        let published = exchange
            .update_live(facts, now_secs())
            .await
            .map_err(|e| e.to_string())?;
        use clawft_kernel::node_facts_exchange::Published;
        return Ok(match published {
            Published::Base(seq) => seq,
            Published::Delta { seq, .. } => seq,
            Published::Nothing => 0,
        });
    }
    tokio::task::spawn_blocking(move || refresh_blocking(&signer, &membership))
        .await
        .map_err(|e| format!("probe task failed: {e}"))?
}

/// Chain an operator-forced probe: who asked is the RPC caller's concern;
/// what the chain keeps is that a full probe ran on this node, and the `seq`
/// it published.
#[cfg(feature = "exochain")]
fn record_forced_refresh(chain: &clawft_kernel::chain::ChainManager, node_id: &str, seq: u64) {
    chain.append(
        "node.facts",
        clawft_kernel::chain::EVENT_KIND_NODE_FACTS_REFRESH,
        Some(serde_json::json!({ "node": node_id, "seq": seq })),
    );
}

/// Start local facts: first probe now, then refresh at 80% of the TTL.
/// Idempotent: only the first call starts the task.
pub fn init(
    key: SigningKey,
    runtime_dir: PathBuf,
    membership: Arc<ClusterMembership>,
    mesh: Option<Arc<MeshRuntime>>,
    revocations: Option<Arc<clawft_kernel::revocation::RevocationList>>,
) {
    // Claim first: a second call must not build (and start) a second exchange.
    if INIT.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let exchange = mesh.map(|rt| {
        // Liveness/RTT ping between verified peers (fleet `last_seen`, `rtt_ms`).
        rt.start_liveness(clawft_kernel::mesh_liveness::LivenessConfig::default());
        let ex = FactsExchange::new(
            key.clone(),
            membership.clone(),
            rt,
            FactsTrustPolicy::default(),
        );
        ex.start();
        ex
    });
    crate::expose_rt::set_facts_source(
        membership.clone(),
        clawft_kernel::node_registry::node_id_from_pubkey(&key.verifying_key().to_bytes()),
    );
    let signer = Arc::new(LocalSigner {
        key,
        runtime_dir,
        last_seq: Mutex::new(0),
        exchange,
        last_full: Mutex::new(None),
        emulation: Arc::new(EmulationCache::new(Duration::from_secs(EMULATION_CACHE_SECS))),
        revocations,
        gate: RefreshGate::new(
            Duration::from_secs(MIN_FORCED_REFRESH_SECS),
            Duration::from_secs(PROBE_TIMEOUT_SECS),
        ),
        last_forced_at: std::sync::atomic::AtomicU64::new(0),
    });
    if LOCAL.set(signer.clone()).is_err() {
        return;
    }
    tokio::spawn(async move {
        // Connected nodes re-probe often enough to turn live-state changes into
        // deltas; the exchange re-signs a base by itself before its TTL.
        let every = if signer.exchange.is_some() {
            Duration::from_secs(LIVE_PROBE_SECS)
        } else {
            Duration::from_secs(DEFAULT_FACTS_TTL_SECS * 4 / 5)
        };
        loop {
            match refresh(signer.clone(), membership.clone(), false).await {
                Ok(Some(seq)) => info!(seq, "node facts probed and signed"),
                Ok(None) => {}
                Err(e) => warn!(error = %e, "node facts probe failed"),
            }
            membership.facts().evict_expired(now_secs());
            tokio::time::sleep(every).await;
        }
    });
}

/// `cluster.facts` params.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FactsParams {
    /// Re-probe the local node before answering.
    #[serde(default)]
    pub refresh: bool,
    /// Only this node.
    #[serde(default)]
    pub node_id: Option<String>,
}

/// The `cluster.facts` result when `refresh` was asked for (without it the
/// result is the bare entry list, as before).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefreshedFacts {
    /// True if this call probed; false if the minimum gap since the last
    /// forced probe skipped it and the entries are the cached ones.
    pub refreshed: bool,
    /// Unix seconds of the last forced probe that ran (0: none since boot).
    pub cached_as_of: u64,
    /// The entries.
    pub entries: Vec<FactsEntry>,
}

/// One entry of the `cluster.facts` result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FactsEntry {
    /// Node id.
    pub node_id: String,
    /// True for this daemon's own node.
    pub local: bool,
    /// Receiver-assigned trust tier.
    pub trust_tier: TrustTier,
    /// Where that tier came from: an operator, or how the peer's connection
    /// was admitted. `paired` from the mesh means "node id verified at
    /// admission", not "operator-paired" as placement uses the word. Absent
    /// from daemons that predate the field.
    #[serde(default)]
    pub tier_source: Option<clawft_kernel::node_facts::TierSource>,
    /// When this daemon accepted the facts, unix seconds.
    pub received_at: u64,
    /// When the facts expire, unix seconds.
    pub expires_at: u64,
    /// Last applied delta seq.
    pub delta_seq: u64,
    /// Current facts (base plus deltas).
    pub facts: clawft_types::placement::NodeFacts,
    /// Signed base envelope, for independent verification.
    pub signed: clawft_kernel::node_facts_advert::SignedNodeFacts,
}

fn entry(c: CachedNodeFacts, local_id: Option<&str>) -> FactsEntry {
    FactsEntry {
        node_id: c.node_id().to_string(),
        local: local_id == Some(c.node_id()),
        trust_tier: c.trust_tier,
        tier_source: Some(c.tier_source),
        received_at: c.received_at,
        expires_at: c.facts.expires_at(),
        delta_seq: c.delta_seq,
        facts: c.facts,
        signed: c.signed,
    }
}

/// The fresh cached facts of every node (or just `only`), as `cluster.facts`
/// returns them. Read-only: no probe runs.
pub fn facts_entries(membership: &ClusterMembership, only: Option<&str>) -> Vec<FactsEntry> {
    let local_id = LOCAL
        .get()
        .map(|s| clawft_kernel::node_id_from_pubkey(&s.key.verifying_key().to_bytes()));
    membership
        .facts()
        .list(now_secs())
        .into_iter()
        .filter(|c| only.is_none_or(|n| n == c.node_id()))
        .map(|c| entry(c, local_id.as_deref()))
        .collect()
}

/// Handle `cluster.facts`.
pub async fn handle(
    params: Value,
    kernel: Arc<tokio::sync::RwLock<Kernel<NativePlatform>>>,
) -> Response {
    let p: FactsParams = if params.is_null() {
        FactsParams::default()
    } else {
        match serde_json::from_value(params) {
            Ok(p) => p,
            Err(e) => return Response::error(format!("invalid params: {e}")),
        }
    };
    let membership = kernel.read().await.cluster_membership().clone();
    let local = LOCAL.get().cloned();
    // `Some((refreshed, cached_as_of))` when a refresh was asked for.
    let mut refresh_result: Option<(bool, u64)> = None;
    if p.refresh {
        let Some(signer) = local.clone() else {
            return Response::error("local node facts not initialised");
        };
        match refresh(signer.clone(), membership.clone(), true).await {
            Err(e) => return Response::error(format!("probe failed: {e}")),
            Ok(Some(seq)) => {
                let at = now_secs();
                signer.last_forced_at.store(at, std::sync::atomic::Ordering::SeqCst);
                refresh_result = Some((true, at));
                #[cfg(feature = "exochain")]
                if let Some(chain) = kernel.read().await.chain_manager().cloned() {
                    let id = clawft_kernel::node_id_from_pubkey(&signer.key.verifying_key().to_bytes());
                    record_forced_refresh(&chain, &id, seq);
                }
                #[cfg(not(feature = "exochain"))]
                let _ = seq;
            }
            Ok(None) => {
                let at = signer.last_forced_at.load(std::sync::atomic::Ordering::SeqCst);
                refresh_result = Some((false, at));
            }
        }
    }
    let entries = facts_entries(&membership, p.node_id.as_deref());
    let value = match refresh_result {
        None => serde_json::to_value(entries),
        // A refresh was asked for: say whether it ran or the floor skipped it.
        Some((refreshed, cached_as_of)) => serde_json::to_value(RefreshedFacts {
            refreshed,
            cached_as_of,
            entries,
        }),
    };
    match value {
        Ok(v) => Response::success(v),
        Err(e) => Response::error(format!("encode failed: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir_with(config: Option<&str>) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        if let Some(c) = config {
            std::fs::write(d.path().join(CONFIG_FILE), c).unwrap();
        }
        d
    }

    fn alpine() -> Option<String> {
        Some("alpine:3.20".into())
    }

    #[test]
    fn probe_image_defaults_then_file_then_environment() {
        let none = dir_with(None);
        assert_eq!(probe_image(none.path(), None, alpine()), alpine());

        let file = dir_with(Some(r#"{"docker_probe_image": "armv7/probe:1"}"#));
        assert_eq!(
            probe_image(file.path(), None, alpine()).as_deref(),
            Some("armv7/probe:1")
        );
        // Environment wins over the file.
        assert_eq!(
            probe_image(file.path(), Some("local/img:2".into()), alpine()).as_deref(),
            Some("local/img:2")
        );
    }

    #[test]
    fn probe_image_can_be_disabled() {
        let null = dir_with(Some(r#"{"docker_probe_image": null}"#));
        assert_eq!(probe_image(null.path(), None, alpine()), None);
        let none = dir_with(None);
        assert_eq!(probe_image(none.path(), Some("  ".into()), alpine()), None);
        // An empty config object keeps the default.
        let empty = dir_with(Some("{}"));
        assert_eq!(probe_image(empty.path(), None, alpine()), alpine());
    }

    #[test]
    fn an_image_reference_that_docker_would_read_as_an_option_is_not_used() {
        let none = dir_with(None);
        assert_eq!(probe_image(none.path(), Some("--privileged".into()), alpine()), None);
        assert_eq!(probe_image(none.path(), Some("a b".into()), alpine()), None);
        let file = dir_with(Some(r#"{"docker_probe_image": "-v /:/host"}"#));
        assert_eq!(probe_image(file.path(), None, alpine()), None);
    }

    #[test]
    fn unreadable_config_falls_back_to_the_default() {
        let bad = dir_with(Some("{not json"));
        assert_eq!(probe_image(bad.path(), None, alpine()), alpine());
        let unknown = dir_with(Some(r#"{"surprise": 1}"#));
        assert_eq!(probe_image(unknown.path(), None, alpine()), alpine());
    }

    use std::sync::atomic::{AtomicU64, Ordering};

    /// A probe that takes a moment and counts itself and how many ran at once.
    fn slow_probe(
        runs: &Arc<AtomicU64>,
        live: &Arc<AtomicU64>,
        peak: &Arc<AtomicU64>,
    ) -> impl std::future::Future<Output = Result<u64, String>> + use<> {
        let (runs, live, peak) = (runs.clone(), live.clone(), peak.clone());
        async move {
            let now = live.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(30)).await;
            live.fetch_sub(1, Ordering::SeqCst);
            Ok(runs.fetch_add(1, Ordering::SeqCst) + 1)
        }
    }

    #[tokio::test]
    async fn concurrent_forced_refreshes_probe_once() {
        let gate = RefreshGate::new(Duration::from_secs(60), Duration::from_secs(5));
        let (runs, live, peak) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
        let go = || gate.run(true, || slow_probe(&runs, &live, &peak));
        let (a, b, c) = tokio::join!(go(), go(), go());
        let results = [a.unwrap(), b.unwrap(), c.unwrap()];
        assert_eq!(runs.load(Ordering::SeqCst), 1, "one probe served all three");
        assert_eq!(results.iter().filter(|r| r.is_some()).count(), 1);
        assert_eq!(results.iter().filter(|r| r.is_none()).count(), 2);
        assert_eq!(peak.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_forced_refresh_is_allowed_again_after_the_floor() {
        let gate = RefreshGate::new(Duration::from_millis(60), Duration::from_secs(5));
        let (runs, live, peak) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
        assert!(gate.run(true, || slow_probe(&runs, &live, &peak)).await.unwrap().is_some());
        assert!(gate.run(true, || slow_probe(&runs, &live, &peak)).await.unwrap().is_none());
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(gate.run(true, || slow_probe(&runs, &live, &peak)).await.unwrap().is_some());
        assert_eq!(runs.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn the_background_tick_and_a_forced_refresh_never_overlap() {
        let gate = RefreshGate::new(Duration::from_secs(60), Duration::from_secs(5));
        let (runs, live, peak) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
        let bg = || gate.run(false, || slow_probe(&runs, &live, &peak));
        let forced = || gate.run(true, || slow_probe(&runs, &live, &peak));
        let (a, b, c, d) = tokio::join!(bg(), forced(), bg(), forced());
        for r in [a, b, c, d] {
            r.unwrap();
        }
        assert_eq!(peak.load(Ordering::SeqCst), 1, "probes ran one at a time");
        // Two background ticks and one forced probe; the second forced one was skipped.
        assert_eq!(runs.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn a_failed_forced_probe_is_rate_limited_too() {
        let gate = RefreshGate::new(Duration::from_secs(60), Duration::from_secs(5));
        let first = gate.run(true, || async { Err::<u64, _>("engine down".to_string()) }).await;
        assert_eq!(first, Err("engine down".to_string()));
        let again = gate
            .run(true, || async { panic!("must not probe again inside the floor") })
            .await;
        assert_eq!(again, Ok(None));
    }

    #[tokio::test]
    async fn a_probe_that_hangs_is_abandoned_and_releases_the_lock() {
        let gate = RefreshGate::new(Duration::ZERO, Duration::from_millis(50));
        let r = gate
            .run(true, || async {
                tokio::time::sleep(Duration::from_secs(30)).await;
                Ok(1)
            })
            .await;
        assert!(r.unwrap_err().contains("timed out"));
        // The lock is free again: the next refresh runs.
        assert_eq!(gate.run(false, || async { Ok(7) }).await, Ok(Some(7)));
    }

    #[cfg(feature = "exochain")]
    #[test]
    fn a_forced_probe_is_chained_with_the_node_and_seq() {
        let chain = clawft_kernel::chain::ChainManager::new(0, 1000);
        record_forced_refresh(&chain, "n-local", 42);
        let ev: Vec<_> = chain
            .tail(chain.len())
            .into_iter()
            .filter(|e| e.kind == clawft_kernel::chain::EVENT_KIND_NODE_FACTS_REFRESH)
            .collect();
        assert_eq!(ev.len(), 1);
        let p = ev[0].payload.clone().unwrap();
        assert_eq!(p["node"], "n-local");
        assert_eq!(p["seq"], 42);
    }
}

#[cfg(all(test, feature = "ecc", feature = "exochain", feature = "placement", unix))]
mod model_advert_tests {
    use super::*;
    use clawft_kernel::model_manifest::{AdoptInput, ModelFormat, ModelRegistry, ModelSource, scan_dir};
    use clawft_kernel::revocation::{RevocationKind, RevocationList};
    use clawft_kernel::workload_pkg::{KeyOrigin, TrustAnchors, key_id_for};
    use clawft_types::placement::AttrValue;
    use std::os::unix::fs::PermissionsExt;

    /// A runtime dir with one adopted fake model; returns (dir, package id, key id, pubkey hex).
    fn runtime_with_model(pin_signer: bool) -> (tempfile::TempDir, String) {
        let tmp = tempfile::tempdir().unwrap();
        let model = tmp.path().join("quant");
        std::fs::create_dir_all(&model).unwrap();
        std::fs::write(model.join("model-0.safetensors"), vec![1u8; 512]).unwrap();
        let key = ed25519_dalek::SigningKey::from_bytes(&[5u8; 32]);
        let pk = key.verifying_key().to_bytes();
        let kid = key_id_for(&pk);
        let hexpk = hex::encode(pk);
        let mut anchors = TrustAnchors::default();
        anchors.push_signer(&kid, &hexpk, KeyOrigin::Operator).unwrap();
        let run = tmp.path().join("run");
        std::fs::create_dir_all(run.join("models")).unwrap();
        let reg = ModelRegistry::open(run.join(MODEL_REGISTRY_FILE)).unwrap();
        let scanned = scan_dir(
            &model,
            AdoptInput {
                name: "Fake".into(),
                format: ModelFormat::Mlx,
                source: ModelSource { hf_repo: Some("o/m".into()), ..ModelSource::default() },
                redistributable: false,
            },
        )
        .unwrap();
        let id = reg.adopt(scanned, &key, &kid, &anchors, false).unwrap().package_id;
        if pin_signer {
            let trust = serde_json::json!({
                "schema": "weftos.workload-trust.v1",
                "operator_keys": [{"key_id": kid, "public_key": hexpk}],
            });
            let p = run.join(crate::workload_place_policy::TRUST_FILE);
            std::fs::write(&p, trust.to_string()).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        (tmp, id)
    }

    fn list(tmp: &tempfile::TempDir) -> Arc<RevocationList> {
        Arc::new(RevocationList::new(tmp.path().join("revoked.json")))
    }

    fn shards(c: &Capability) -> usize {
        match c.attrs.get("shards") {
            Some(AttrValue::List(l)) => l.len(),
            _ => 0,
        }
    }

    #[test]
    fn a_pinned_unrevoked_model_is_advertised() {
        let (tmp, _) = runtime_with_model(true);
        let caps = probe_config_with(&tmp.path().join("run"), Some(&list(&tmp))).models;
        assert_eq!(caps.len(), 1);
        assert_eq!(shards(&caps[0]), 2, "marker plus one shard hash");
    }

    #[test]
    fn a_revoked_model_advertises_no_hashes() {
        let (tmp, id) = runtime_with_model(true);
        let l = list(&tmp);
        l.revoke_subject(RevocationKind::Package, &id, "test").unwrap();
        let caps = probe_config_with(&tmp.path().join("run"), Some(&l)).models;
        assert!(caps.iter().all(|c| shards(c) == 0), "{caps:?}");
    }

    #[test]
    fn no_anchors_no_revocation_list_or_plain_probe_config_advertises_nothing() {
        let (tmp, _) = runtime_with_model(false);
        let run = tmp.path().join("run");
        assert!(probe_config_with(&run, Some(&list(&tmp))).models.is_empty(), "no pinned signers");
        let (tmp, _) = runtime_with_model(true);
        let run = tmp.path().join("run");
        assert!(probe_config_with(&run, None).models.is_empty(), "no revocation list");
        assert!(probe_config(&run).models.is_empty(), "plain probe_config has no list");
    }
}
