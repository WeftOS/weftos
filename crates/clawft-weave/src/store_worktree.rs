//! `store.worktree.*`: the daemon side of ADR-126 S2 (worktree stores, tree sharing, the L1
//! follower view). `scripts/worktree-store.sh` calls these through `weaver store worktree <verb>`.
//!
//! Every verb is an Admin act on the local daemon (see `capability.rs`). They work on the raw data
//! plane (`StoreService::operator_objects`), the same level as S1's promote and archive. The four
//! that change shared state (create, publish, claim, promote) are recorded in the activity log and
//! as a chain event, including when they fail (`record_verb`). Paths in params (`root`, `dest`, `cache`,
//! the `*_z` lists) are read or written by the daemon itself, so client and daemon share a host.
//! That is a premise, not a protection: every path is checked by `store_worktree_paths::Guard`
//! (absolute, resolved, no symlinked final component, kept out of the daemon's own state).

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use serde::Deserialize;
use serde_json::{Value, json};
use weftos_store::backend::{BackendError, ObjectAttrs, ObjectBackend as S1, ObjectMeta};
use weftos_store::claim_watch::{ClaimWatch, claim_merge_observed};
use weftos_store::follower::{self, Change, FollowState, LeftoverPolicy, Why};
use weftos_store::promote::{ArchiveTarget, OnConflict, PromotionPolicy, WorktreeMeta, is_promoted, promote, read_meta, write_meta};
use weftos_store::service::{ARCHIVE_BUCKET, Parties, archive_key};
use weftos_store::registry_sign::{
    EntrySigner, EntryVerifier, list_trusted_gen, list_trusted_lenient, poll_verified_settled, remove_registry_entry_signed,
    write_registry_entry_signed,
};
use weftos_store::share::{Actor, Limits, TreeSource, mark_handoff, parse_name_status_z};
use weftos_store::share_fetch::{
    self, RegistryEntry, Select, list_shared_lenient, summarize, write_registry_entry,
};
use weftos_store::{StoreName, StoreService};

use crate::store_worktree_paths::{Guard, Use};

#[path = "store_worktree_cache.rs"]
mod wtcache;
#[path = "store_worktree_dispatch.rs"]
mod dispatch;
#[path = "store_worktree_orphan.rs"]
mod orphan;
#[path = "store_worktree_views.rs"]
mod views;

use dispatch::dispatch;
use views::*;

/// Delegates to the service's shared backend so the S2 code (generic over a sized backend) can use it.
pub(crate) struct Op(pub(crate) Arc<dyn S1>);

impl S1 for Op {
    fn put(&self, b: &str, k: &str, d: &[u8], n: bool) -> Result<ObjectMeta, BackendError> {
        self.0.put(b, k, d, n)
    }
    fn get(&self, b: &str, k: &str) -> Result<Vec<u8>, BackendError> {
        self.0.get(b, k)
    }
    fn head(&self, b: &str, k: &str) -> Result<ObjectMeta, BackendError> {
        self.0.head(b, k)
    }
    fn delete_if_match(&self, b: &str, k: &str, e: &str) -> Result<(), BackendError> {
        self.0.delete_if_match(b, k, e)
    }
    fn delete(&self, b: &str, k: &str) -> Result<(), BackendError> {
        self.0.delete(b, k)
    }
    fn put_if_match(&self, b: &str, k: &str, d: &[u8], e: &str) -> Result<ObjectMeta, BackendError> {
        self.0.put_if_match(b, k, d, e)
    }
    fn put_attrs(&self, b: &str, k: &str, d: &[u8], n: bool, a: ObjectAttrs) -> Result<ObjectMeta, BackendError> {
        self.0.put_attrs(b, k, d, n, a)
    }
    fn put_if_match_attrs(&self, b: &str, k: &str, d: &[u8], e: &str, a: ObjectAttrs) -> Result<ObjectMeta, BackendError> {
        self.0.put_if_match_attrs(b, k, d, e, a)
    }
    fn attrs(&self, b: &str, k: &str) -> Result<ObjectAttrs, BackendError> {
        self.0.attrs(b, k)
    }
    fn copy(&self, sb: &str, sk: &str, db: &str, dk: &str) -> Result<ObjectMeta, BackendError> {
        self.0.copy(sb, sk, db, dk)
    }
    fn list(&self, b: &str, p: &str) -> Result<Vec<ObjectMeta>, BackendError> {
        self.0.list(b, p)
    }
    fn list_stamped(&self, b: &str, p: &str) -> Result<Vec<(ObjectMeta, String)>, BackendError> {
        self.0.list_stamped(b, p)
    }
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct P {
    project: Option<String>,
    slug: Option<String>,
    branch: Option<String>,
    base: Option<String>,
    head: Option<String>,
    root: Option<String>,
    dest: Option<String>,
    cache: Option<String>,
    state: Option<String>,
    query: Option<String>,
    /// `browse`: a folder (ends in `/`, or empty for the root) or one file of the shared tree.
    path: Option<String>,
    a: Option<String>,
    b: Option<String>,
    #[serde(rename = "paths_z")]
    paths_z: Option<String>,
    uncommitted_z: Option<String>,
    changes_z: Option<String>,
    bundle: Option<String>,
    bundle_out: Option<String>,
    handoff: Option<String>,
    /// The claim epoch the caller claimed under; `promote` refuses a merge made under another.
    epoch: Option<u64>,
    #[serde(default)]
    force: bool,
    /// Output format; the CLI formats, the daemon ignores it.
    #[serde(default)]
    #[allow(dead_code)]
    json: bool,
    #[serde(default)]
    confirmed: bool,
    /// `follow`: list the registry instead of reading the digest.
    #[serde(default)]
    full: bool,
    /// `promote`: what to do with a key the project changed since the worktree saw it
    /// (`fail` by default, `keep_project`, `overwrite`).
    on_conflict: Option<String>,
    /// `promote`: report what landing would do and write nothing.
    #[serde(default)]
    dry_run: bool,
    /// `promote`: refuse a land that copies more than this many bytes (default 1 GiB; 0 = no cap).
    max_bytes: Option<u64>,
    /// `publish`: refuse when the worktree store already holds more than this many bytes
    /// (default 8 GiB, `retention::DEFAULT_WORKTREE_QUOTA`; 0 = no quota).
    quota_bytes: Option<u64>,
    /// `gc`: retire worktree stores landed at least this many days ago (default 14).
    keep_days: Option<u64>,
    /// `gc`: also delete archived worktree objects older than this many days (default: never).
    archive_keep_days: Option<u64>,
    /// `orphan`: when the sweep first saw the worktree's local path missing (ms since the epoch).
    since: Option<u64>,
    /// Who is acting; filled in by the CLI from the environment.
    agent: Option<String>,
    host: Option<String>,
}

fn need<'a>(v: &'a Option<String>, what: &str) -> Result<&'a str, String> {
    v.as_deref().filter(|s| !s.is_empty()).ok_or_else(|| format!("missing --{what}"))
}

fn slug_ok(s: &str) -> Result<&str, String> {
    let ok = !s.is_empty()
        && s.len() <= 48
        && s.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        && !s.starts_with('-');
    if ok { Ok(s) } else { Err(format!("bad slug {s:?} (want [a-z0-9-], max 48)")) }
}

fn project_ok(s: &str) -> Result<&str, String> {
    if !s.is_empty() && s.len() <= 40 && s.bytes().all(|c| c.is_ascii_alphanumeric()) {
        Ok(s)
    } else {
        Err(format!("bad project id {s:?}"))
    }
}

fn read_list(g: &Guard, path: &Option<String>, what: &str) -> Result<Vec<u8>, String> {
    match path {
        None => Ok(Vec::new()),
        Some(p) => {
            let c = g.check(p, what, Use::ReadFile)?;
            let md = std::fs::metadata(&c).map_err(|e| format!("--{what}: {e}"))?;
            if !md.is_file() || md.len() > 32 << 20 {
                return Err(format!("--{what}: not a regular file under 32 MiB"));
            }
            std::fs::read(&c).map_err(|e| format!("--{what}: {e}"))
        }
    }
}

fn nul_list(raw: &[u8]) -> Result<Vec<String>, String> {
    raw.split(|b| *b == 0)
        .filter(|t| !t.is_empty())
        .map(|t| std::str::from_utf8(t).map(str::to_owned).map_err(|_| "non-UTF-8 path in list".to_string()))
        .collect()
}

fn claim_watch() -> &'static Mutex<ClaimWatch> {
    static W: OnceLock<Mutex<ClaimWatch>> = OnceLock::new();
    W.get_or_init(|| Mutex::new(ClaimWatch::new()))
}

/// Milliseconds on a monotonic clock, from the first call in this process.
fn mono_ms() -> u64 {
    static T0: OnceLock<Instant> = OnceLock::new();
    T0.get_or_init(Instant::now).elapsed().as_millis() as u64
}

fn who(p: &P) -> Actor {
    Actor {
        agent: p.agent.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| "agent".into()),
        host: p.host.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| "unknown".into()),
    }
}

struct Names {
    wt_name: StoreName,
    wt: String,
    pr_name: StoreName,
    pr: String,
    /// Where the shared-worktree registry lives: the project store, or (switch on) a store of its
    /// own that only the daemon writes.
    reg: String,
}

/// Channel of the registry-only store used when the registry switch is on.
const REGISTRY_CHANNEL: &str = "wt-registry";

/// The switch: `WEFTOS_WT_REGISTRY_STORE=1` moves the registry out of the project store. Off by default.
fn registry_switch() -> bool {
    std::env::var("WEFTOS_WT_REGISTRY_STORE").is_ok_and(|v| v == "1")
}

/// Registry signing (ADR-126 S2, `docs/research/adr126-s2-registry-signing-and-claim-clock.md`).
/// All three parts are off unless their env var is set, so a daemon started as before writes and
/// reads unsigned entries exactly as it did.
/// - `WEFTOS_WT_REGISTRY_SIGN_KEY`: file holding a 32-byte ed25519 seed as 64 hex characters.
///   The daemon signs every registry entry it writes with it. Which key this is (the node key, a
///   project key) is the owner's open decision; the daemon only reads the file it is pointed at.
/// - `WEFTOS_WT_REGISTRY_TRUSTED`: file of trusted public keys, one 64-hex key per line (`#` comments).
///   With it, follow/search/leftovers check each entry's signature.
/// - `WEFTOS_WT_REGISTRY_REQUIRE_SIGNED`: with the trusted file, entries that do not verify are dropped
///   (and reported by `follow` as unreadable). Setting a trusted file turns this on by itself; `=0`
///   keeps unverified entries visible with their trust shown in each verb's `trust` field (an audit
///   mode for rolling signing out). `=1` without a trusted file is an error.
#[derive(Default)]
pub struct SignCfg {
    key: Option<ed25519_dalek::SigningKey>,
    trusted: Option<Vec<[u8; 32]>>,
    require: bool,
    /// `WEFTOS_WT_FOLLOW_RETRY_MS`: re-list once after this many ms when a known worktree is refused
    /// (option B in the registry-format decision note). 0 = off, the default.
    follow_retry_ms: u64,
}

/// 32 bytes from 64 hex characters. The error never repeats any of the input: this parses a secret seed.
fn hex32(s: &str) -> Result<[u8; 32], String> {
    const BAD: &str = "a key is 32 bytes written as 64 hex characters";
    let v = hex::decode(s.trim()).map_err(|_| BAD.to_string())?;
    v.try_into().map_err(|_| BAD.to_string())
}

/// A public key that may be trusted: 32 bytes that decode to a curve point that is not small-order.
/// A small-order (weak) key makes signatures forgeable without knowing any secret, so it is refused
/// where trusted keys are loaded as well as where signatures are checked.
fn trusted_key(s: &str) -> Result<[u8; 32], String> {
    let k = hex32(s)?;
    let vk = ed25519_dalek::VerifyingKey::from_bytes(&k).map_err(|_| "a trusted key is not a valid ed25519 public key".to_string())?;
    if vk.is_weak() {
        return Err("a trusted key is a small-order (weak) point; refusing it".into());
    }
    Ok(k)
}

impl SignCfg {
    pub fn off() -> Self {
        Self::default()
    }

    pub fn new(key: Option<[u8; 32]>, trusted: Option<Vec<[u8; 32]>>, require: bool) -> Self {
        Self { key: key.map(|k| ed25519_dalek::SigningKey::from_bytes(&k)), trusted, require, follow_retry_ms: 0 }
    }

    /// Read the three env settings. A named file that cannot be read is an error, never a silent
    /// fall back to unsigned: someone who asked for signing must not get an unsigned registry.
    pub fn from_env() -> Result<Self, String> {
        Self::from_lookup(|v| std::env::var(v).ok())
    }

    /// [`Self::from_env`] with the variable lookup supplied.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let file = |var: &str| -> Result<Option<String>, String> {
            match get(var) {
                Some(p) if !p.is_empty() => std::fs::read_to_string(&p).map(Some).map_err(|e| format!("{var} {p}: {e}")),
                _ => Ok(None),
            }
        };
        // A seed other users can read is no secret: refuse it rather than sign with it.
        #[cfg(unix)]
        if let Some(p) = get("WEFTOS_WT_REGISTRY_SIGN_KEY").filter(|p| !p.is_empty()) {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&p).map_err(|e| format!("WEFTOS_WT_REGISTRY_SIGN_KEY {p}: {e}"))?.permissions().mode();
            if mode & 0o077 != 0 {
                return Err(format!("WEFTOS_WT_REGISTRY_SIGN_KEY {p} is accessible to other users (mode {:o}); chmod 600 it", mode & 0o777));
            }
        }
        let key = file("WEFTOS_WT_REGISTRY_SIGN_KEY")?.map(|t| hex32(&t)).transpose()?;
        let trusted = file("WEFTOS_WT_REGISTRY_TRUSTED")?
            .map(|t| {
                t.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).map(trusted_key).collect::<Result<Vec<_>, _>>()
            })
            .transpose()?;
        let require = match get("WEFTOS_WT_REGISTRY_REQUIRE_SIGNED").as_deref() {
            Some("1") if trusted.is_none() => {
                return Err("WEFTOS_WT_REGISTRY_REQUIRE_SIGNED=1 needs WEFTOS_WT_REGISTRY_TRUSTED".into());
            }
            Some("1") => true,
            Some("0") => false,
            // unset: a trusted-key file means "filter", which is what setting one is for
            _ => trusted.is_some(),
        };
        let retry = match get("WEFTOS_WT_FOLLOW_RETRY_MS").as_deref() {
            None | Some("") => 0,
            Some(v) => v.parse::<u64>().ok().filter(|n| *n <= 10_000).ok_or("WEFTOS_WT_FOLLOW_RETRY_MS is milliseconds, 0 to 10000")?,
        };
        Ok(Self::new(key, trusted, require).with_follow_retry_ms(retry))
    }

    /// Option B of the registry-format decision note: see `follow_retry_ms`.
    pub fn with_follow_retry_ms(mut self, ms: u64) -> Self {
        self.follow_retry_ms = ms;
        self
    }
}

struct Ed(ed25519_dalek::SigningKey);
impl EntrySigner for Ed {
    fn signer_id(&self) -> String {
        hex::encode(self.0.verifying_key().to_bytes())
    }
    fn alg(&self) -> &'static str {
        "ed25519"
    }
    fn sign(&self, msg: &[u8]) -> Vec<u8> {
        use ed25519_dalek::Signer;
        self.0.sign(msg).to_bytes().to_vec()
    }
}

struct Trusted<'a>(&'a [[u8; 32]]);
impl EntryVerifier for Trusted<'_> {
    fn verify(&self, signer: &str, alg: &str, msg: &[u8], sig: &[u8]) -> bool {
        use ed25519_dalek::{Signature, VerifyingKey};
        let Ok(k) = hex32(signer) else { return false };
        if alg != "ed25519" || !self.0.contains(&k) {
            return false;
        }
        let (Ok(vk), Ok(sig)) = (VerifyingKey::from_bytes(&k), Signature::from_slice(sig)) else { return false };
        // Strict: rejects non-canonical signatures and small-order components; a weak key is never trusted.
        !vk.is_weak() && vk.verify_strict(msg, &sig).is_ok()
    }
}

/// After a worktree store is archived and its bucket deleted, drop its registry entry from the
/// project bucket (ADR-126 §4a: the daemon "removes it after promotion and bucket deletion").
/// Any other kind of store is a no-op. Removing an entry that is already gone is fine.
pub fn forget(svc: &StoreService, name: &StoreName) -> Result<(), String> {
    let StoreName::Worktree { mesh, project, slug } = name else { return Ok(()) };
    let _inv = wtcache::on_exit_slug(svc, slug);
    let pr = StoreName::project(mesh, &project.to_string()).map_err(|e| e.to_string())?.bucket();
    remove_registry_entry_signed(&Op(svc.operator_objects()), &pr, slug).map_err(|e| e.to_string())
}

/// Run one verb. `mesh` is the store-name mesh token of this daemon.
pub fn run(svc: &StoreService, mesh: &str, method: &str, params: Value, now_ms: u64) -> Result<Value, String> {
    let sign = SignCfg::from_env()?;
    run_full(svc, mesh, method, params, now_ms, registry_switch(), &sign, &Guard::from_runtime())
}

/// [`run`] with an explicit set of protected paths.
pub fn run_with(svc: &StoreService, mesh: &str, method: &str, params: Value, now_ms: u64, guard: &Guard) -> Result<Value, String> {
    run_full(svc, mesh, method, params, now_ms, false, &SignCfg::off(), guard)
}

/// [`run_with`] with the registry switch and registry signing given explicitly.
#[allow(clippy::too_many_arguments)]
pub fn run_full(
    svc: &StoreService,
    mesh: &str,
    method: &str,
    params: Value,
    now_ms: u64,
    registry_store: bool,
    sign: &SignCfg,
    guard: &Guard,
) -> Result<Value, String> {
    let verb = method.strip_prefix("store.worktree.").unwrap_or(method).to_owned();
    let who = (params["project"].as_str().map(str::to_owned), params["slug"].as_str().map(str::to_owned), params["agent"].as_str().map(str::to_owned), params["host"].as_str().map(str::to_owned));
    let r = dispatch(svc, mesh, method, params, now_ms, registry_store, sign, guard);
    record_verb(svc, mesh, &verb, who, &r);
    r
}

/// Activity-log and chain record for the verbs that change shared state, success or failure.
fn record_verb(svc: &StoreService, mesh: &str, verb: &str, who: (Option<String>, Option<String>, Option<String>, Option<String>), r: &Result<Value, String>) {
    let (kind, op) = match verb {
        "create" => ("store.worktree.create", "WorktreeCreate"),
        "publish" => ("store.worktree.publish", "WorktreePublish"),
        "claim" => ("store.worktree.claim", "WorktreeClaim"),
        "promote" => ("store.worktree.promote", "WorktreePromote"),
        _ => return,
    };
    let (Some(project), Some(slug)) = (who.0, who.1) else { return };
    let Ok(n) = names(mesh, &project, &slug) else { return };
    let actor = format!("{}@{}", who.2.as_deref().unwrap_or("agent"), who.3.as_deref().unwrap_or("?"));
    let (outcome, detail) = match r {
        Ok(v) => (weftos_store::Outcome::Ok, v.clone()),
        Err(e) => (weftos_store::Outcome::Error(e.clone()), json!({"error": e})),
    };
    let bytes = detail["uploaded"].as_u64().unwrap_or(0);
    svc.record_act(&actor, &n.wt_name, op, None, bytes, outcome, Some((kind, json!({"store": n.wt_name.to_string(), "project": project, "slug": slug, "actor": actor, "result": detail}))));
}

/// Dispatch one `store.worktree.*` verb on the daemon's service.
pub async fn handle(method: &str, params: Value) -> clawft_rpc::Response {
    let Some(svc) = crate::store_rt::global() else {
        return clawft_rpc::Response::error("the object store is not running on this daemon".to_string());
    };
    let method = method.to_owned();
    let r = tokio::task::spawn_blocking(move || {
        run(&svc, &crate::store_rt::mesh_token(), &method, params, clawft_kernel::exposure::registry::now_ms())
    })
    .await;
    match r {
        Ok(Ok(v)) => clawft_rpc::Response::success(v),
        Ok(Err(e)) => clawft_rpc::Response::error(e),
        Err(e) => clawft_rpc::Response::error(format!("store task failed: {e}")),
    }
}

#[cfg(test)]
#[path = "store_worktree_tests.rs"]
mod tests;
