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

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use serde::Deserialize;
use serde_json::{Value, json};
use weftos_store::backend::{BackendError, ObjectBackend as S1, ObjectMeta};
use weftos_store::claim_watch::{ClaimWatch, claim_merge_observed};
use weftos_store::follower::{self, Change, FollowState, LeftoverPolicy, Why};
use weftos_store::promote::{ArchiveTarget, PromotionPolicy, WorktreeMeta, is_promoted, promote, read_meta, write_meta};
use weftos_store::service::{ARCHIVE_BUCKET, Parties, archive_key};
use weftos_store::share::{
    Actor, Limits, TreeSource, mark_handoff, mark_merged_at, parse_name_status_z, publish_tree,
};
use weftos_store::registry_sign::{
    EntrySigner, EntryVerifier, list_trusted_lenient, poll_verified, remove_registry_entry_signed,
    write_registry_entry_signed,
};
use weftos_store::share_fetch::{
    RegistryEntry, Select, fetch_bundle, fetch_tree, summarize, write_registry_entry,
};
use weftos_store::{StoreName, StoreService};

use crate::store_worktree_paths::{Guard, Use};

/// Delegates to the service's shared backend so the S2 code (generic over a sized backend) can use it.
pub(crate) struct Op(pub(crate) Arc<dyn S1>);

impl S1 for Op {
    fn put(&self, b: &str, k: &str, d: &[u8], n: bool) -> Result<ObjectMeta, BackendError> {
        self.0.put(b, k, d, n)
    }
    fn put_if_match(&self, b: &str, k: &str, d: &[u8], etag: &str) -> Result<ObjectMeta, BackendError> {
        self.0.put_if_match(b, k, d, etag)
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
    fn copy(&self, sb: &str, sk: &str, db: &str, dk: &str) -> Result<ObjectMeta, BackendError> {
        self.0.copy(sb, sk, db, dk)
    }
    fn list(&self, b: &str, p: &str) -> Result<Vec<ObjectMeta>, BackendError> {
        self.0.list(b, p)
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
/// - `WEFTOS_WT_REGISTRY_REQUIRE_SIGNED=1`: with the trusted file, drop (and report) entries that do not verify.
#[derive(Default)]
pub struct SignCfg {
    key: Option<ed25519_dalek::SigningKey>,
    trusted: Option<Vec<[u8; 32]>>,
    require: bool,
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
        Self { key: key.map(|k| ed25519_dalek::SigningKey::from_bytes(&k)), trusted, require }
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
        let key = file("WEFTOS_WT_REGISTRY_SIGN_KEY")?.map(|t| hex32(&t)).transpose()?;
        let trusted = file("WEFTOS_WT_REGISTRY_TRUSTED")?
            .map(|t| {
                t.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).map(trusted_key).collect::<Result<Vec<_>, _>>()
            })
            .transpose()?;
        let require = get("WEFTOS_WT_REGISTRY_REQUIRE_SIGNED").is_some_and(|v| v == "1");
        if require && trusted.is_none() {
            return Err("WEFTOS_WT_REGISTRY_REQUIRE_SIGNED=1 needs WEFTOS_WT_REGISTRY_TRUSTED".into());
        }
        Ok(Self::new(key, trusted, require))
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

#[cfg(test)]
fn names(mesh: &str, project: &str, slug: &str) -> Result<Names, String> {
    names_with(mesh, project, slug, false)
}

fn names_with(mesh: &str, project: &str, slug: &str, registry_store: bool) -> Result<Names, String> {
    let e = |e: weftos_store::StoreError| e.to_string();
    let wt_name = StoreName::worktree(mesh, project_ok(project)?, slug_ok(slug)?).map_err(e)?;
    let pr_name = StoreName::project(mesh, project).map_err(e)?;
    let reg_name = if registry_store {
        StoreName::release(mesh, project, REGISTRY_CHANNEL).map_err(e)?
    } else {
        pr_name.clone()
    };
    Ok(Names { wt: wt_name.bucket(), pr: pr_name.bucket(), reg: reg_name.bucket(), wt_name, pr_name })
}

fn entry_json(e: &RegistryEntry) -> Value {
    json!({
        "slug": e.slug, "branch": e.branch, "head": e.head_commit, "state": e.state,
        "writer": format!("{}@{}", e.writer.agent, e.writer.host),
        "merger": e.merger.as_ref().map(|m| format!("{}@{}", m.agent, m.host)),
        "docs": e.doc_count, "files": e.file_count, "updated_ms": e.updated_ms,
    })
}

fn change_json(c: &Change) -> Value {
    match c {
        Change::New(e) => json!({"change": "new", "worktree": entry_json(e)}),
        Change::HeadMoved { entry, from } => json!({"change": "head_moved", "from": from, "worktree": entry_json(entry)}),
        Change::StateChanged { entry, from } => json!({"change": "state_changed", "from": from, "worktree": entry_json(entry)}),
        Change::Gone { slug } => json!({"change": "gone", "slug": slug}),
        Change::Unreadable { slug } => json!({"change": "unreadable", "slug": slug}),
    }
}

fn why_str(w: Why) -> &'static str {
    match w {
        Why::ForgottenOpen => "forgotten_open",
        Why::UnclaimedHandoff => "unclaimed_handoff",
        Why::AbandonedClaim => "abandoned_claim",
        Why::StuckCleanup => "stuck_cleanup",
        Why::FutureTimestamp => "future_timestamp",
    }
}

/// A verb that writes into a worktree must find a live, matching worktree (review F6, F8): not
/// archived or closed, created (so its metadata names this project and slug, which also stops a
/// slug whose 48-bit bucket hash collides with another from writing into that other's bucket).
/// A store another daemon created is fine: it need not be known here, only be a real worktree.
fn live_worktree(svc: &StoreService, b: &Op, n: &Names, project: &str, slug: &str) -> Result<(), String> {
    for name in [&n.wt_name, &n.pr_name] {
        if svc.is_archived(name) == Some(true) {
            return Err(format!("{name} is archived; refusing"));
        }
    }
    let meta = match read_meta(b, &n.wt) {
        Ok(m) => m,
        Err(weftos_store::overlay::S2Error::Backend(m)) if m.contains("no such bucket") => None,
        Err(e) => return Err(e.to_string()),
    };
    match meta {
        None => Err(format!("worktree {slug} was never created here (no metadata); run create first")),
        Some(m) if m.slug == slug && m.project.eq_ignore_ascii_case(project) => Ok(()),
        Some(m) => Err(format!("bucket {} belongs to worktree {} of project {}, not {slug}", n.wt, m.slug, m.project)),
    }
}

/// Re-derive one worktree's registry entry from its own bucket (the daemon is the only writer).
fn refresh(b: &Op, pr: &str, slug: &str, wt: &str, sign: &SignCfg) -> Result<(), String> {
    let e = |e: weftos_store::overlay::S2Error| e.to_string();
    if let Some(ent) = summarize(b, slug, wt).map_err(e)? {
        match &sign.key {
            Some(k) => write_registry_entry_signed(b, pr, &ent, &Ed(k.clone())).map_err(e)?,
            None => write_registry_entry(b, pr, &ent).map_err(e)?,
        }
    }
    Ok(())
}

/// This project's worktree stores on the node.
fn worktree_stores(svc: &StoreService, mesh: &str, project: &str) -> Vec<weftos_store::StoreInfo> {
    let marker = format!("weftos://{mesh}/projects/{project}/worktrees/");
    svc.list_stores().into_iter().filter(|i| i.kind == "worktree" && i.name.to_lowercase().starts_with(&marker.to_lowercase())).collect()
}

/// The registry's entries as this follower should see them: all of them, or with a trusted-key
/// file the ones that verify (all of them when `REQUIRE_SIGNED` is off).
fn trusted_entries(b: &Op, reg: &str, sign: &SignCfg) -> weftos_store::overlay::Result<Vec<RegistryEntry>> {
    Ok(match &sign.trusted {
        Some(t) => list_trusted_lenient(b, reg, &Trusted(t), sign.require)?.0.into_iter().map(|(e, _)| e).collect(),
        None => weftos_store::share_fetch::list_shared_lenient(b, reg)?.0,
    })
}

/// Run one verb. `mesh` is the store-name mesh token of this daemon.
pub fn run(svc: &StoreService, mesh: &str, method: &str, params: Value, now_ms: u64) -> Result<Value, String> {
    let sign = SignCfg::from_env()?;
    run_full(svc, mesh, method, params, now_ms, registry_switch(), &sign)
}

/// [`run`] with the registry switch given explicitly.
pub fn run_with(svc: &StoreService, mesh: &str, method: &str, params: Value, now_ms: u64, registry_store: bool) -> Result<Value, String> {
    run_full(svc, mesh, method, params, now_ms, registry_store, &SignCfg::off())
}

/// [`run_with`] with registry signing given explicitly.
pub fn run_full(svc: &StoreService, mesh: &str, method: &str, params: Value, now_ms: u64, registry_store: bool, sign: &SignCfg) -> Result<Value, String> {
    let names = |m: &str, pr: &str, s: &str| names_with(m, pr, s, registry_store);
    let p: P = serde_json::from_value(params).map_err(|e| format!("invalid params: {e}"))?;
    let e = |e: weftos_store::overlay::S2Error| e.to_string();
    let b = Op(svc.operator_objects());
    let project = project_ok(need(&p.project, "project")?)?.to_owned();
    let verb = method.strip_prefix("store.worktree.").unwrap_or(method);
    if registry_store && !matches!(verb, "buckets") {
        // No admins, writers or appenders: only the daemon's operator path can write it. Idempotent,
        // so a worktree created before the switch was turned on still finds its registry store.
        let rn = StoreName::release(mesh, &project, REGISTRY_CHANNEL).map_err(|e| e.to_string())?;
        svc.ensure_store(&rn, "cli", Parties::default()).map_err(|e| e.to_string())?;
    }
    match verb {
        "buckets" => {
            let n = names(mesh, &project, need(&p.slug, "slug")?)?;
            Ok(json!({"worktree": n.wt, "project": n.pr}))
        }
        "create" => {
            let slug = need(&p.slug, "slug")?;
            let n = names(mesh, &project, slug)?;
            // The project store may not exist yet on a fresh node; create it, never fail on "exists".
            svc.ensure_store(&n.pr_name, "cli", Parties::defaults_for(&n.pr_name)).map_err(|e| e.to_string())?;
            svc.ensure_store(&n.wt_name, "cli", Parties::defaults_for(&n.wt_name)).map_err(|e| e.to_string())?;

            if weftos_store::promote::is_promoted(&b, &n.wt).map_err(e)?.is_some() {
                return Err(format!("worktree {slug} was already landed; use a new slug"));
            }
            write_meta(
                &b,
                &n.wt,
                &WorktreeMeta {
                    slug: slug.into(),
                    project: project.clone(),
                    branch: need(&p.branch, "branch")?.into(),
                    base_commit: need(&p.base, "base")?.into(),
                    created_ms,
                },
            )
            .map_err(e)?;
            Ok(json!({"worktree": n.wt, "project": n.pr}))
        }
        "publish" => {
            let slug = need(&p.slug, "slug")?;
            let n = names(mesh, &project, slug)?;
            live_worktree(svc, &b, &n, &project, slug)?;
            let root = guard.check(need(&p.root, "root")?, "root", Use::ReadDir)?;
            // The hook publishes from the top of a git worktree; anything else is not a worktree.
            if !root.join(".git").exists() {
                return Err("--root must be the top of a git worktree (no .git there)".into());
            }
            let paths = nul_list(&read_list(guard, &p.paths_z, "paths-z")?)?;
            let uncommitted = nul_list(&read_list(guard, &p.uncommitted_z, "uncommitted-z")?)?;
            let changes = parse_name_status_z(&read_list(guard, &p.changes_z, "changes-z")?).map_err(e)?;
            // A listed path that resolves into the daemon's own state is refused outright, not skipped.
            if let Some(bad) = paths.iter().find(|rel| guard.is_protected(&root.join(rel))) {
                return Err(format!("{bad}: inside the daemon's own state; refused"));
            }
            let bundle = match p.bundle.as_deref() {
                Some(b) => Some(guard.check(b, "bundle", Use::ReadFile)?),
                None => None,
            };
            let bundle = bundle.as_deref();
            let writer = who(&p);
            let rep = publish_tree(
                &b,
                &n.wt,
                &TreeSource {
                    root: &root,
                    paths: &paths,
                    uncommitted: &uncommitted,
                    changes: &changes,
                    branch: need(&p.branch, "branch")?,
                    base_commit: need(&p.base, "base")?,
                    head_commit: need(&p.head, "head")?,
                    bundle,
                    writer: writer.clone(),
                },
                &Limits::default(),
                now_ms,
            )
            .map_err(e)?;
            if let Some(note) = p.handoff.as_deref() {
                mark_handoff(&b, &n.wt, &writer, note, now_ms).map_err(e)?;
            }
            refresh(&b, &n.reg, slug, &n.wt, sign)?;
            Ok(json!({
                "uploaded": rep.uploaded, "unchanged": rep.unchanged, "removed": rep.removed,
                "skipped": rep.skipped.iter().map(|(k, w)| format!("{k}: {w:?}")).collect::<Vec<_>>(),
                "bundle_uploaded": rep.bundle_uploaded, "handoff": p.handoff.is_some(),
            }))
        }
        "fetch" => {
            let n = names(mesh, &project, need(&p.slug, "slug")?)?;
            let dest = guard.check(need(&p.dest, "dest")?, "dest", Use::WriteDir)?;
            let out = match &p.bundle_out {
                Some(o) => Some(guard.check(o, "bundle-out", Use::WriteFile)?),
                None => None,
            };
            std::fs::create_dir_all(&dest).map_err(|e| e.to_string())?;
            let r = fetch_tree(&b, &n.wt, &dest, Select::All, p.force).map_err(e)?;
            let mut head = None;
            if let Some(out) = &out {
                head = Some(fetch_bundle(&b, &n.wt, out).map_err(e)?);
            }
            Ok(json!({
                "written": r.written.len(), "unchanged": r.unchanged.len(), "conflicts": r.conflicts,
                "missing": r.missing, "refused": r.refused, "bundle_head": head,
            }))
        }
        "claim" => {
            let slug = need(&p.slug, "slug")?;
            let n = names(mesh, &project, slug)?;
            // Expiry is judged on how long this daemon has watched the claim sit unchanged, on its
            // own monotonic clock; the stored stamp (some caller's wall clock) decides nothing.
            let h = {
                let mut w = claim_watch().lock().map_err(|_| "claim watch poisoned".to_string())?;
                claim_merge_observed(&b, &n.wt, &who(&p), &mut w, mono_ms(), now_ms).map_err(e)?
            };
            refresh(&b, &n.reg, slug, &n.wt, sign)?;
            Ok(json!({"state": h.state, "epoch": h.epoch,
                      "merger": h.merger.map(|m| format!("{}@{}", m.agent, m.host))}))
        }
        "promote" => {
            let slug = need(&p.slug, "slug")?;
            let n = names(mesh, &project, slug)?;
            let on_conflict = match p.on_conflict.as_deref() {
                None | Some("fail") => OnConflict::Fail,
                Some("keep_project") => OnConflict::KeepProject,
                Some("overwrite") => OnConflict::Overwrite,
                Some(o) => return Err(format!("--on-conflict is fail, keep_project or overwrite, not '{o}'")),
            };
            let defaults = PromotionPolicy::default();
            let policy = PromotionPolicy {
                on_conflict,
                max_promote_bytes: match p.max_bytes {
                    Some(0) => None,
                    Some(n) => Some(n),
                    None => defaults.max_promote_bytes,
                },
                ..defaults
            };
            if p.dry_run {
                let pv = weftos_store::promote::preview(&b, &n.wt, &n.pr, &policy).map_err(e)?;
                return Ok(json!({"dry_run": true, "preview": pv}));
            }
            svc.ensure_archive().map_err(|e| e.to_string())?;
            let prefix = archive_key(&n.wt, now_ms, "");
            let r = promote(
                &b,
                &n.wt,
                &n.pr,
                &ArchiveTarget { bucket: ARCHIVE_BUCKET, prefix: &prefix },
                &policy,
                now_ms,
            )
            .map_err(e)?;
            // A land that went through git is the merge: record it if a claimer did not. Only after
            // the promotion succeeded, so a conflict leaves the handoff claimable and republishable.
            if let Ok(Some(h)) = weftos_store::share::read_handoff(&b, &n.wt)
                && h.state == weftos_store::share::State::Claimed
            {
                mark_merged_at(&b, &n.wt, &who(&p), p.epoch, "landed", now_ms).map_err(e)?;
            }
            // Landed: nothing unmerged is left to see, so the entry (and its signature) goes. Left in
            // place it would read as a forgotten claim to every follower.
            remove_registry_entry_signed(&b, &n.reg, slug).map_err(e)?;
            Ok(json!({
                "promoted": r.promoted, "deleted": r.deleted, "kept_project": r.kept_project,
                "archived": r.archived.len(), "dropped_tombstones": r.dropped_tombstones,
            }))
        }
        "usage" => {
            let n = names(mesh, &project, "x")?;
            let (pobj, pbytes) = weftos_store::retention::usage(&b, &n.pr).map_err(e)?;
            let quota = p.quota_bytes.unwrap_or(weftos_store::retention::DEFAULT_WORKTREE_QUOTA);
            let mut wts = Vec::new();
            for info in worktree_stores(svc, mesh, &project) {
                let landed = weftos_store::promote::is_promoted(&b, &info.bucket).map_err(e)?.map(|r| r.promoted_ms);
                wts.push(json!({
                    "store": info.name, "objects": info.objects, "bytes": info.bytes,
                    "landed_ms": landed, "over_quota": quota > 0 && info.bytes > quota,
                }));
            }
            Ok(json!({"project": {"objects": pobj, "bytes": pbytes}, "worktrees": wts, "quota_bytes": quota}))
        }
        "gc" => {
            use weftos_store::retention::{DAY_MS, DEFAULT_LANDED_KEEP_DAYS, expired_archive_keys, landed_before};
            let keep = p.keep_days.unwrap_or(DEFAULT_LANDED_KEEP_DAYS);
            let cutoff = now_ms.saturating_sub(keep.saturating_mul(DAY_MS));
            let mut retire = Vec::new();
            for info in worktree_stores(svc, mesh, &project) {
                if landed_before(&b, &info.bucket, cutoff).map_err(e)?.is_some() {
                    retire.push(info);
                }
            }
            let mut expired = Vec::new();
            if let Some(days) = p.archive_keep_days {
                let prefix = format!("w-{}-", project.to_lowercase());
                let cutoff = now_ms.saturating_sub(days.saturating_mul(DAY_MS));
                svc.ensure_archive().map_err(|x| x.to_string())?;
                expired = expired_archive_keys(&b, ARCHIVE_BUCKET, &prefix, cutoff).map_err(e)?;
            }
            let names: Vec<String> = retire.iter().map(|i| i.name.clone()).collect();
            if !p.confirmed {
                return Ok(json!({"dry_run": true, "retire": names, "expire_archived": expired.len(),
                                 "keep_days": keep, "note": "nothing was changed; pass --confirmed to act"}));
            }
            let (mut retired, mut failed) = (Vec::new(), Vec::new());
            for info in &retire {
                let r = StoreName::parse(&info.name).map_err(|x| x.to_string()).and_then(|nm| svc.archive(&nm, true, "gc").map_err(|x| x.to_string()));
                match r {
                    Ok(_) => retired.push(info.name.clone()),
                    Err(x) => failed.push(json!({"store": info.name, "error": x})),
                }
            }
            for k in &expired {
                weftos_store::overlay::ObjectBackend::delete(&b, ARCHIVE_BUCKET, k).map_err(e)?;
            }
            Ok(json!({"dry_run": false, "retired": retired, "failed": failed, "expired_archived": expired.len(), "keep_days": keep}))
        }
        "follow" => {
            let n = names(mesh, &project, "x")?;
            let sp = guard.check(need(&p.state, "state")?, "state", Use::WriteFile)?;
            let mut st = FollowState::load(&sp).map_err(e)?;
            let ch = match &sign.trusted {
                Some(t) => poll_verified(&b, &n.reg, &mut st, &Trusted(t), sign.require, p.confirmed),
                None if p.confirmed => follower::poll_confirmed(&b, &n.reg, &mut st),
                None => follower::poll(&b, &n.reg, &mut st),
            }
            .map_err(e)?;
            st.save(&sp).map_err(e)?;
            Ok(json!({"changes": ch.iter().map(change_json).collect::<Vec<_>>()}))
        }
        "search" => {
            let n = names(mesh, &project, "x")?;
            let entries = trusted_entries(&b, &n.reg, sign).map_err(e)?;
            let r = follower::search_docs(&b, &entries, need(&p.query, "query")?);
            Ok(json!({
                "hits": r.hits.iter().map(|h| json!({"slug": h.slug, "path": h.path, "title": h.title, "state": h.state})).collect::<Vec<_>>(),
                "unreadable": r.unreadable,
                "unreadable_entries": bad_entries,
            }))
        }
        "leftovers" => {
            let n = names(mesh, &project, "x")?;
            let entries = trusted_entries(&b, &n.reg, sign).map_err(e)?;
            let l = follower::leftovers(&entries, now_ms, &LeftoverPolicy::default());
            Ok(json!({"leftovers": l.iter().map(|x| json!({"why": why_str(x.why), "idle_ms": x.idle_ms, "worktree": entry_json(&x.entry)})).collect::<Vec<_>>()}))
        }
        "sync-docs" => {
            let slug = need(&p.slug, "slug")?;
            let n = names(mesh, &project, slug)?;
            let entry = summarize(&b, slug, &n.wt).map_err(e)?.ok_or("worktree has nothing published")?;
            let r = follower::sync_docs(&b, &entry, &guard.check(need(&p.cache, "cache")?, "cache", Use::WriteDir)?).map_err(e)?;
            Ok(json!({"written": r.written.len(), "unchanged": r.unchanged.len(), "refused": r.refused}))
        }
        "overlaps" => {
            let (sa, sb) = (need(&p.a, "a")?, need(&p.b, "b")?);
            let (na, nb) = (names(mesh, &project, sa)?, names(mesh, &project, sb)?);
            let ea = summarize(&b, sa, &na.wt).map_err(e)?.ok_or_else(|| format!("{sa} has nothing published"))?;
            let eb = summarize(&b, sb, &nb.wt).map_err(e)?.ok_or_else(|| format!("{sb} has nothing published"))?;
            let r = follower::overlaps(&b, &ea, &eb).map_err(e)?;
            Ok(json!({
                "same_base": r.same_base,
                // On different bases a path can differ only because the base moved: discount it.
                "reliable": r.same_base,
                "base_a": r.base_a, "base_b": r.base_b,
                "overlaps": r.overlaps.iter().map(|o| json!({"path": o.path, "kind": format!("{:?}", o.kind)})).collect::<Vec<_>>(),
            }))
        }
        other => Err(format!("unknown worktree verb: {other}")),
    }
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
