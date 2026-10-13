//! The member's side of `project.fetch` (ADR-108 P3b): the channel the calls
//! travel on and the pipelined chunk download with checksums. Repository
//! names are `weftos://<mesh>/projects/<ULID>[/repos/<dir>]`
//! ([`crate::weftos_uri`]).
//!
//! Two channels carry the same request bodies: [`PlaneChannel`] (inside the
//! user daemon, straight onto the signed `workload.ctl` wire) and
//! [`DaemonChannel`] (the `git-remote-weftos` helper, over the daemon's local
//! RPC socket, which forwards to the primary). Control calls (`list`, `refs`,
//! the opens, `close`) go through [`FetchChannel::call`]; the chunk reads go
//! through a window of [`FetchLink`]s, each a connection kept open for the
//! whole transfer and each reading every `window`-th chunk, so several chunks
//! are in flight while the file is written strictly in order. Every chunk's
//! SHA-256 and the whole file's are checked; a mismatch, a refusal or a lost
//! link fails the fetch. Bundles land in the repository through git itself
//! (`git bundle unbundle` for the helper, `git fetch <bundle>` for an install),
//! tars through the checked unpack in [`crate::project_fetch_tar`].

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use clawft_kernel::workload_ctl::{CtlSession, PlacementControlPlane, msg::method};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::project_fetch_repos::{Refs, git, is_oid, ref_ok};
use crate::project_fetch_tar::{Unpacked, unpack_tar};

/// Local daemon RPC verb the helper calls (forwarded as the node-admin method).
pub const LOCAL_RPC: &str = "project.fetch";
/// Largest chunk a member accepts (the server caps at the same value).
pub const MAX_CHUNK_BYTES: u64 = 2 * 1024 * 1024;
const FETCH_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// One chunk over a slow link.
const CHUNK_TIMEOUT: Duration = Duration::from_secs(120);

/// How a download runs: chunk size asked of the primary, links in flight,
/// and whether links keep their connection (the old path opened one per call).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FetchTuning {
    pub chunk_bytes: u64,
    pub window: usize,
    pub reuse: bool,
}

impl Default for FetchTuning {
    fn default() -> Self {
        Self { chunk_bytes: MAX_CHUNK_BYTES, window: 6, reuse: true }
    }
}

/// One connection used for chunk reads, one request at a time.
#[async_trait]
pub trait FetchLink: Send {
    async fn call(&mut self, body: Value) -> Result<Value, String>;
    /// Whether a `raw: true` chunk comes back as bytes beside the JSON
    /// (a kept-open mesh session can; a JSON-only path cannot).
    fn raw_capable(&self) -> bool {
        false
    }
    /// [`Self::call`] with the raw frame, when one was asked for and sent.
    async fn call_raw(&mut self, body: Value) -> Result<(Value, Option<Vec<u8>>), String> {
        self.call(body).await.map(|v| (v, None))
    }
}

/// Carries `project.fetch` bodies to the primary.
#[async_trait]
pub trait FetchChannel: Send + Sync {
    /// A control call (opens, refs, close).
    async fn call(&self, body: Value) -> Result<Value, String>;
    /// `n` links for chunk reads.
    async fn links(&self, n: usize) -> Result<Vec<Box<dyn FetchLink>>, String>;
    fn tuning(&self) -> FetchTuning {
        FetchTuning::default()
    }
}

/// Inside the daemon: straight onto the signed mesh wire.
pub struct PlaneChannel {
    pub plane: Arc<PlacementControlPlane>,
    pub node: String,
    pub tuning: FetchTuning,
}

impl PlaneChannel {
    pub fn new(plane: Arc<PlacementControlPlane>, node: String) -> Self {
        Self { plane, node, tuning: FetchTuning::default() }
    }

    pub fn with_tuning(mut self, tuning: FetchTuning) -> Self {
        self.tuning = tuning;
        self
    }
}

/// A kept-open signed session.
struct SessionLink(CtlSession);

#[async_trait]
impl FetchLink for SessionLink {
    async fn call(&mut self, body: Value) -> Result<Value, String> {
        self.0.call(method::PROJECT_FETCH, body).await.map_err(|e| e.to_string())
    }
    fn raw_capable(&self) -> bool {
        true
    }
    async fn call_raw(&mut self, body: Value) -> Result<(Value, Option<Vec<u8>>), String> {
        self.0.call_raw(method::PROJECT_FETCH, body).await.map_err(|e| e.to_string())
    }
}

/// A connection per call (the plane's ordinary node-admin path).
struct PerCallLink(Arc<PlacementControlPlane>, String);

#[async_trait]
impl FetchLink for PerCallLink {
    async fn call(&mut self, body: Value) -> Result<Value, String> {
        self.0.node_admin(&self.1, method::PROJECT_FETCH, body).await.map_err(|e| e.to_string())
    }
}

#[async_trait]
impl FetchChannel for PlaneChannel {
    async fn call(&self, body: Value) -> Result<Value, String> {
        self.plane.node_admin(&self.node, method::PROJECT_FETCH, body).await.map_err(|e| e.to_string())
    }

    async fn links(&self, n: usize) -> Result<Vec<Box<dyn FetchLink>>, String> {
        let mut out: Vec<Box<dyn FetchLink>> = Vec::with_capacity(n);
        for _ in 0..n {
            if self.tuning.reuse {
                let s = self.plane.open_session(&self.node).await.map_err(|e| e.to_string())?;
                out.push(Box::new(SessionLink(s.with_timeout(CHUNK_TIMEOUT))));
            } else {
                out.push(Box::new(PerCallLink(self.plane.clone(), self.node.clone())));
            }
        }
        Ok(out)
    }

    fn tuning(&self) -> FetchTuning {
        self.tuning
    }
}

/// From a helper process: the local daemon resolves the repository name to
/// the project's paired primary and forwards (keeping a session per link
/// there, see `project_fetch_rpc`).
pub struct DaemonChannel {
    /// The repository's `weftos://` name, as git holds it.
    pub uri: String,
    /// Socket path; `None` is the daemon's default.
    pub socket: Option<PathBuf>,
    client: tokio::sync::Mutex<Option<clawft_rpc::DaemonClient>>,
}

impl DaemonChannel {
    pub fn new(uri: String, socket: Option<PathBuf>) -> Self {
        Self { uri, socket, client: tokio::sync::Mutex::new(None) }
    }

    /// The daemon takes the repository from the name, not the body, so a call
    /// that names a `dir` goes under that repository's name.
    pub(crate) fn uri_for(&self, body: &Value) -> String {
        let Some(dir) = body["dir"].as_str().filter(|d| !d.is_empty()) else { return self.uri.clone() };
        match crate::weftos_uri::WeftosUri::parse(&self.uri) {
            Ok(u) if u.project_repo().is_some() => crate::weftos_uri::WeftosUri::for_project_repo(u.authority, &u.id, dir).to_string(),
            _ => self.uri.clone(),
        }
    }

    async fn call_once(&self, body: Value) -> Result<Value, String> {
        let mut guard = self.client.lock().await;
        if guard.is_none() {
            let c = match &self.socket {
                Some(p) => clawft_rpc::DaemonClient::connect_path(p).await,
                None => clawft_rpc::DaemonClient::connect().await,
            };
            *guard = Some(c.ok_or("the WeftOS user daemon is not running (weaver kernel boot)")?);
        }
        let req = clawft_rpc::Request::with_params(LOCAL_RPC, json!({ "uri": self.uri_for(&body), "body": body }));
        let resp = match guard.as_mut().expect("connected above").call(req).await {
            Ok(r) => r,
            Err(e) => {
                *guard = None;
                return Err(format!("daemon: {e}"));
            }
        };
        if resp.ok { Ok(resp.result.unwrap_or(Value::Null)) } else { Err(resp.error.unwrap_or_else(|| "daemon refused".into())) }
    }
}

#[async_trait]
impl FetchLink for DaemonChannel {
    async fn call(&mut self, body: Value) -> Result<Value, String> {
        self.call_once(body).await
    }
}

#[async_trait]
impl FetchChannel for DaemonChannel {
    async fn call(&self, body: Value) -> Result<Value, String> {
        self.call_once(body).await
    }

    async fn links(&self, n: usize) -> Result<Vec<Box<dyn FetchLink>>, String> {
        Ok((0..n).map(|_| Box::new(DaemonChannel::new(self.uri.clone(), self.socket.clone())) as Box<dyn FetchLink>).collect())
    }
}

/// An opened transfer.
#[derive(Debug, Clone)]
pub struct Opened {
    pub bytes: u64,
    /// Signed round trips it took, opens and close included.
    pub round_trips: u64,
    /// The open result minus the session fields (`files`, `archived`, ...).
    pub meta: Value,
}

/// The project's repositories and non-git summary (`op: list`).
pub async fn list(ch: &dyn FetchChannel, project: &str) -> Result<Value, String> {
    ch.call(json!({ "op": "list", "project": project })).await
}

/// Branches, tags and HEAD of one repository (`op: refs`).
pub async fn refs(ch: &dyn FetchChannel, project: &str, dir: &str) -> Result<Refs, String> {
    let v = ch.call(json!({ "op": "refs", "project": project, "dir": dir })).await?;
    let refs = v["refs"]
        .as_array()
        .ok_or("refs: malformed answer")?
        .iter()
        .filter_map(|r| Some((r["oid"].as_str()?.to_owned(), r["name"].as_str()?.to_owned())))
        .filter(|(o, n)| is_oid(o) && ref_ok(n))
        .collect();
    let head = v["head"].as_str().filter(|h| ref_ok(h)).map(str::to_owned);
    Ok(Refs { refs, head })
}

/// Open a transfer with `open` (the channel's chunk size is added) and stream
/// it to `out`. `None` when the primary had nothing to send (`empty`).
pub async fn download(ch: &dyn FetchChannel, mut open: Value, out: &Path) -> Result<Option<Opened>, String> {
    let tuning = ch.tuning();
    open["chunk_bytes"] = json!(tuning.chunk_bytes.clamp(1, MAX_CHUNK_BYTES));
    let mut v = ch.call(open).await?;
    if v["empty"] == true {
        return Ok(None);
    }
    let session = v["session"].as_str().filter(|s| s.len() == 32).ok_or("open: no session")?.to_owned();
    let bytes = v["bytes"].as_u64().ok_or("open: no size")?;
    let sha256 = v["sha256"].as_str().ok_or("open: no checksum")?.to_owned();
    let chunk_bytes = v["chunk_bytes"].as_u64().filter(|c| (1..=MAX_CHUNK_BYTES).contains(c)).ok_or("open: bad chunk size")?;
    let chunks = v["chunks"].as_u64().ok_or("open: no chunk count")?;
    if chunks != bytes.div_ceil(chunk_bytes) {
        return Err("open: chunk count disagrees with size".into());
    }
    let plan = Pull { session: &session, bytes, chunk_bytes, chunks, sha256: &sha256, window: tuning.window };
    let result = pull(ch, &plan, out).await;
    let _ = ch.call(json!({ "op": "close", "session": session })).await;
    result?;
    if let Some(o) = v.as_object_mut() {
        for k in ["session", "sha256", "chunk_bytes", "chunks"] {
            o.remove(k);
        }
    }
    Ok(Some(Opened { bytes, round_trips: chunks + 2, meta: v }))
}

struct Pull<'a> {
    session: &'a str,
    bytes: u64,
    chunk_bytes: u64,
    chunks: u64,
    sha256: &'a str,
    window: usize,
}

type ChunkResult = Result<(u64, Vec<u8>), String>;

/// Read chunk `index` on `link` and check it: as a raw frame when the link
/// can carry one (no base64, and the signature covers the hash, not the
/// bytes), base64 in the JSON otherwise.
async fn read_chunk(link: &mut dyn FetchLink, session: &str, index: u64, expect: usize) -> ChunkResult {
    let raw = link.raw_capable();
    let (c, frame) = link.call_raw(json!({ "op": "chunk", "session": session, "index": index, "raw": raw })).await?;
    if c["index"].as_u64() != Some(index) {
        return Err("chunk: answer for another index".into());
    }
    let data = match frame {
        Some(bytes) if raw => bytes,
        _ => base64::engine::general_purpose::STANDARD
            .decode(c["data"].as_str().ok_or("chunk: no data")?)
            .map_err(|_| "chunk: bad encoding")?,
    };
    if data.len() != expect || hex::encode(Sha256::digest(&data)) != c["sha256"].as_str().unwrap_or("") {
        return Err(format!("chunk {index}: size or checksum mismatch"));
    }
    Ok((index, data))
}

/// Chunks in a window of links, written in order. Link `k` reads chunks
/// `k, k+w, k+2w, ...` and hands each over a one-slot queue, so at most two
/// chunks per link are held at once.
async fn pull(ch: &dyn FetchChannel, p: &Pull<'_>, out: &Path) -> Result<(), String> {
    let mut f = std::io::BufWriter::new(std::fs::File::create(out).map_err(|e| format!("write: {e}"))?);
    let w = p.window.clamp(1, 16).min(p.chunks.max(1) as usize);
    let links = ch.links(w).await?;
    let expect = |i: u64| (p.bytes - i * p.chunk_bytes).min(p.chunk_bytes) as usize;
    let mut rxs = Vec::with_capacity(w);
    let mut tasks = Vec::with_capacity(w);
    for (k, mut link) in links.into_iter().enumerate() {
        let (tx, rx) = tokio::sync::mpsc::channel::<ChunkResult>(1);
        rxs.push(rx);
        let (session, chunks, sizes): (String, u64, Vec<usize>) =
            (p.session.to_owned(), p.chunks, (k as u64..p.chunks).step_by(w).map(expect).collect());
        tasks.push(tokio::spawn(async move {
            for (i, size) in (k as u64..chunks).step_by(w).zip(sizes) {
                let r = read_chunk(link.as_mut(), &session, i, size).await;
                let failed = r.is_err();
                if tx.send(r).await.is_err() || failed {
                    break;
                }
            }
        }));
    }
    let result = async {
        let mut total = Sha256::new();
        let mut got = 0u64;
        for i in 0..p.chunks {
            let (index, data) = rxs[(i as usize) % w].recv().await.ok_or("chunk reader stopped")??;
            if index != i {
                return Err("chunk: out of order".to_string());
            }
            total.update(&data);
            f.write_all(&data).map_err(|e| format!("write: {e}"))?;
            got += data.len() as u64;
        }
        f.flush().map_err(|e| format!("write: {e}"))?;
        if got != p.bytes || hex::encode(total.finalize()) != p.sha256 {
            return Err("transfer checksum mismatch".to_string());
        }
        Ok(())
    }
    .await;
    for t in tasks {
        t.abort();
    }
    result
}

/// How a downloaded bundle enters the local repository.
pub enum BundleMode {
    /// Objects only; git updates refs itself (remote-helper `fetch`).
    Unbundle,
    /// `git fetch <bundle> <refspecs>`: objects and refs (an install).
    Fetch(Vec<String>),
}

/// Fetch `want` refnames of `project`/`dir` minus `have` ids into the
/// repository at `repo`. `Ok(None)` when nothing was missing.
pub async fn fetch_bundle(ch: &dyn FetchChannel, project: &str, dir: &str, want: &[String], have: &[String], repo: &Path, mode: BundleMode) -> Result<Option<u64>, String> {
    let tmp = tempfile::Builder::new().prefix("weftos-fetch-").suffix(".bundle").tempfile_in(repo).map_err(|e| format!("tmp: {e}"))?;
    let open = json!({ "op": "bundle.open", "project": project, "dir": dir, "want": want, "have": have });
    let Some(opened) = download(ch, open, tmp.path()).await? else { return Ok(None) };
    let path = tmp.path().to_str().ok_or("tmp path is not UTF-8")?;
    match mode {
        BundleMode::Unbundle => git(repo, &["bundle", "unbundle", path], None, FETCH_TIMEOUT)?,
        BundleMode::Fetch(specs) => {
            let mut args = vec!["fetch", "--quiet", "--no-write-fetch-head", path];
            args.extend(specs.iter().map(String::as_str));
            git(repo, &args, None, FETCH_TIMEOUT)?
        }
    };
    Ok(Some(opened.bytes))
}

/// What a non-git fetch did.
#[derive(Debug, Clone, Default)]
pub struct TarReport {
    /// Bytes received on the wire.
    pub bytes: u64,
    pub round_trips: u64,
    pub unpacked: Unpacked,
    pub archived: Vec<String>,
    pub excluded: u64,
    pub truncated: bool,
}

/// Fetch the project's non-git content under `dest`.
pub async fn fetch_tar(ch: &dyn FetchChannel, project: &str, dest: &Path) -> Result<Option<TarReport>, String> {
    let tmp = tempfile::Builder::new().prefix("weftos-fetch-").suffix(".tar").tempfile().map_err(|e| format!("tmp: {e}"))?;
    let Some(opened) = download(ch, json!({ "op": "tar.open", "project": project }), tmp.path()).await? else {
        return Ok(None);
    };
    let unpacked = unpack_tar(tmp.path(), dest)?;
    Ok(Some(TarReport {
        bytes: opened.bytes,
        round_trips: opened.round_trips,
        unpacked,
        archived: opened.meta["archived"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect()).unwrap_or_default(),
        excluded: opened.meta["excluded"].as_u64().unwrap_or(0),
        truncated: opened.meta["truncated"].as_bool().unwrap_or(false),
    }))
}

#[cfg(test)]
#[path = "project_fetch_speed_tests.rs"]
mod speed_tests;
