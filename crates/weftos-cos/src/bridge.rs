//! `weftos-cos bridge`: relay between the hosted dashboard and the local
//! chief of staff (`docs/plans/cos-dashboard-bridge.md`).
//!
//! Every interval the bridge POSTs `/api/nodes/cos/sync` to the dashboard with
//! the new final turns, a status snapshot and the results of earlier requests.
//! The answer lists queued phone-page requests; each is posted to the local
//! `POST /v1/cos/utterance` as a `typed` turn over loopback (no bearer), and
//! its outcome goes out in the next sync. The node token is read from the
//! `[dashboard]` `token_file` in `~/.weftos/weave.toml`, the same file the
//! weaver reporter uses, and is never logged or printed.

use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::Notify;

/// Turns per sync (contract).
pub const MAX_TURNS_PER_SYNC: usize = 50;
/// Back-off after a dashboard error.
pub const ERROR_BACKOFF: Duration = Duration::from_secs(30);
/// Longest request text accepted from the dashboard (contract: 2000).
const MAX_REQUEST_TEXT: usize = 2000;
/// Turn text is cut here so a sync stays well under the 256 KB route limit.
const MAX_TURN_TEXT: usize = 2000;
/// A status file larger than this is replaced by an error object.
const MAX_STATUS_FILE: u64 = 32 * 1024;
/// Remembered `(turn_id, role)` keys.
const SEEN_CAP: usize = 1000;
/// Traces per sync (the dashboard accepts 50).
pub const MAX_TRACES_PER_SYNC: usize = 50;
/// Largest JSON list of a trace (the dashboard stores 8 KB, 4 KB for gates).
const MAX_TRACE_LIST_BYTES: usize = 7000;
const MAX_TRACE_GATES_BYTES: usize = 3500;
/// Longest phone clip (contract: 15 s of 16 kHz mono PCM16).
const MAX_AUDIO_SECS: u32 = 15;
/// Largest download accepted: 15 s of PCM16 plus a generous header.
const MAX_AUDIO_BYTES: usize = 16_000 * 2 * MAX_AUDIO_SECS as usize + 4096;
/// Long-poll wait asked of the dashboard (it caps at 25).
const POLL_WAIT_SECS: u64 = 25;
/// Text length kept by `--dry-run`.
const DRY_RUN_TEXT: usize = 80;

#[derive(clap::Args, Clone, Debug)]
pub struct BridgeArgs {
    /// The local cos.v1 agent.
    #[arg(long, default_value = "http://127.0.0.1:18300")]
    pub agent: String,
    /// Milliseconds between syncs.
    #[arg(long, default_value_t = 2000)]
    pub interval_ms: u64,
    /// Run one sync and exit (non-zero when it failed).
    #[arg(long)]
    pub once: bool,
    /// Print the sync body (text truncated, no token) and do not POST.
    #[arg(long)]
    pub dry_run: bool,
}

/// Where the bridge talks. The token stays in its file and is read per sync,
/// so a token the weaver rotates is picked up.
#[derive(Clone, Debug)]
pub struct BridgeConfig {
    /// Dashboard base URL without a trailing slash.
    pub dashboard_url: String,
    pub token_file: PathBuf,
    pub agent: String,
    /// Directory holding `listen.status.json` and `speak.status.json`.
    pub mentra_dir: PathBuf,
}

/// Read `[dashboard]` from `<home>/.weftos/weave.toml` the way the weaver
/// reporter does (`url`, `token_file`), with the same URL and token-file
/// checks. `enabled` is not required: the bridge is a separate process.
pub fn load_config(home: &Path, agent: &str) -> Result<BridgeConfig, String> {
    let path = home.join(".weftos").join("weave.toml");
    let raw = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let root: toml::Value = toml::from_str(&raw).map_err(|e| format!("{}: {e}", path.display()))?;
    let dash = root.get("dashboard").ok_or_else(|| format!("{}: no [dashboard] section", path.display()))?;
    let url = dash.get("url").and_then(|v| v.as_str()).unwrap_or("");
    let token_file = dash
        .get("token_file")
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
        .ok_or("[dashboard] token_file is not set")?;
    if !token_file.is_absolute() {
        return Err(format!("[dashboard] token_file {} must be an absolute path", token_file.display()));
    }
    Ok(BridgeConfig {
        dashboard_url: base_url(url)?,
        token_file,
        agent: agent.trim_end_matches('/').to_owned(),
        mentra_dir: home.join(".weftos/run/mentra"),
    })
}

/// https, or http to a loopback host only; no credentials, query or fragment.
fn base_url(raw: &str) -> Result<String, String> {
    let url = raw.trim_end_matches('/');
    let (rest, https) = match (url.strip_prefix("https://"), url.strip_prefix("http://")) {
        (Some(r), _) => (r, true),
        (None, Some(r)) => (r, false),
        _ => return Err("[dashboard] url must start with https://".into()),
    };
    let authority = rest.split('/').next().unwrap_or("");
    if authority.is_empty() || authority.contains('@') || rest.contains(['?', '#']) {
        return Err("[dashboard] url must be scheme://host[:port][/path] with no credentials, query or fragment".into());
    }
    if !https {
        let host = if authority.starts_with('[') {
            authority.split_inclusive(']').next().unwrap_or("")
        } else {
            authority.split(':').next().unwrap_or("")
        };
        if !matches!(host, "localhost" | "127.0.0.1" | "[::1]") {
            return Err("[dashboard] url: plain http is only accepted for a loopback host; use https".into());
        }
    }
    Ok(url.to_owned())
}

/// Read the node token: a regular 0600 file in a directory others cannot
/// write; printable ASCII, no whitespace. Errors never contain the token.
fn read_token(path: &Path) -> Result<String, String> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| format!("token file {}: {e}", path.display()))?;
    if !meta.file_type().is_file() {
        return Err(format!("token file {} is not a regular file (symlinks are refused)", path.display()));
    }
    // Mode bits exist only on Unix; on Windows the file's ACL governs access.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode() & 0o777;
        if mode != 0o600 {
            return Err(format!("token file {} has mode {mode:03o}; it must be 600", path.display()));
        }
        if let Some(dir) = path.parent() {
            let d = std::fs::metadata(dir).map_err(|e| format!("token directory {}: {e}", dir.display()))?;
            if d.permissions().mode() & 0o022 != 0 {
                return Err(format!("token directory {} is writable by group or others", dir.display()));
            }
        }
    }
    let raw = std::fs::read_to_string(path).map_err(|e| format!("token file {}: {e}", path.display()))?;
    let t = raw.trim();
    if t.is_empty() || t.len() > 512 || !t.chars().all(|c| c.is_ascii_graphic()) {
        return Err(format!("token file {} does not hold a token", path.display()));
    }
    Ok(t.to_owned())
}

/// Why a sync failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncError {
    /// 401 or 403 from the dashboard.
    Rejected(String),
    /// Anything else: config, network, a non-2xx answer, an unreadable reply.
    Failed(String),
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyncError::Rejected(m) | SyncError::Failed(m) => f.write_str(m),
        }
    }
}

/// What one sync did.
#[derive(Debug, Clone, PartialEq)]
pub struct SyncReport {
    pub turns_sent: usize,
    pub traces_sent: usize,
    pub results_sent: usize,
    pub requests: usize,
    /// Rows (turns, traces) the dashboard rejected twice and the cursor moved past.
    pub rows_skipped: usize,
    /// The body that was (or, with dry run, would have been) posted.
    pub body: Value,
}

/// Speech to text for phone clips. The bridge binary wires the parakeet STT
/// (`onnx` build); a build without it has none and refuses audio requests.
#[async_trait]
pub trait Transcriber: Send + Sync {
    async fn transcribe(&self, samples: Vec<i16>) -> Result<String, String>;
}

/// The crate's configured STT backend.
pub struct SttTranscriber(pub Arc<dyn clawft_channels::voice::stt::SttBackend>);

#[async_trait]
impl Transcriber for SttTranscriber {
    async fn transcribe(&self, samples: Vec<i16>) -> Result<String, String> {
        let utt = clawft_channels::voice::stt::Utterance { samples, sample_rate: 16_000 };
        self.0.transcribe(&utt).await.map_err(|e| format!("stt: {e}"))
    }
}

/// How the dashboard took one sync body.
enum Posted {
    Reply(Value),
    /// A 4xx other than 401/403.
    Rejected(u16),
}

/// What a split sync delivered.
struct Split {
    replies: Vec<Value>,
    turns: usize,
    traces: usize,
    results: usize,
    skipped: usize,
}

#[derive(Default)]
struct Shared {
    seen: HashSet<(String, String)>,
    seen_order: VecDeque<(String, String)>,
    /// Outcomes not yet delivered to the dashboard.
    results: Vec<Value>,
    /// Turn ids whose trace was delivered, and the delivery order for the cap.
    seen_traces: HashSet<String>,
    seen_trace_order: VecDeque<String>,
    /// Newest trace timestamp delivered; asked of the agent as `since`.
    trace_since: Option<String>,
}

pub struct Bridge {
    cfg: BridgeConfig,
    http: reqwest::Client,
    /// No overall timeout beyond the poll window; used only by the long-poll.
    poll_http: reqwest::Client,
    state: Mutex<Shared>,
    stt: Option<Arc<dyn Transcriber>>,
    /// Wakes the sync loop for an immediate sync.
    pub wake: Notify,
}

impl Bridge {
    /// A bridge with no STT: audio requests are reported failed.
    pub fn new(cfg: BridgeConfig) -> Self {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(10)).build().unwrap_or_default();
        let poll_http = reqwest::Client::builder().timeout(Duration::from_secs(POLL_WAIT_SECS + 10)).build().unwrap_or_default();
        Self { cfg, http, poll_http, state: Mutex::new(Shared::default()), stt: None, wake: Notify::new() }
    }

    pub fn with_transcriber(mut self, stt: Option<Arc<dyn Transcriber>>) -> Self {
        self.stt = stt;
        self
    }

    fn st(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// One sync. With `dry_run` nothing is posted and no request is run.
    pub async fn sync_once(&self, dry_run: bool) -> Result<SyncReport, SyncError> {
        let token = if dry_run { None } else { Some(read_token(&self.cfg.token_file).map_err(SyncError::Failed)?) };
        let new_turns = self.new_turns().await;
        let new_traces = self.new_traces().await;
        let results: Vec<Value> = self.st().results.clone();
        let body = json!({
            "turns": new_turns,
            "status": self.status().await,
            "results": results,
            "traces": new_traces,
        });
        let (turns_sent, results_sent, traces_sent) = (new_turns.len(), results.len(), new_traces.len());
        let Some(token) = token else {
            return Ok(SyncReport { turns_sent, traces_sent, results_sent, rows_skipped: 0, requests: 0, body: dry_run_view(&body) });
        };
        let first = self.post_sync(&token, &body).await?;
        let sent = match first {
            Posted::Reply(reply) => {
                // Delivered: advance the cursor and drop the reported results
                // (the poller may have added newer ones meanwhile).
                let mut st = self.st();
                for t in &new_turns {
                    mark_seen(&mut st, key_of(t));
                }
                for t in &new_traces {
                    mark_trace_seen(&mut st, t);
                }
                let n = results_sent.min(st.results.len());
                st.results.drain(..n);
                Split { replies: vec![reply], turns: turns_sent, traces: traces_sent, results: results_sent, skipped: 0 }
            }
            Posted::Rejected(code) => {
                tracing::warn!("bridge sync: dashboard answered HTTP {code}; retrying with the batch split");
                self.sync_split(&token, &new_turns, &new_traces, &results, body["status"].clone()).await?
            }
        };

        let mut requests = Vec::new();
        for reply in &sent.replies {
            requests.extend(reply.get("requests").and_then(Value::as_array).cloned().unwrap_or_default());
        }
        self.handle_all(&token, &requests).await;
        Ok(SyncReport { turns_sent: sent.turns, traces_sent: sent.traces, results_sent: sent.results, rows_skipped: sent.skipped, requests: requests.len(), body })
    }

    /// POST one sync body. A 4xx other than 401/403 is `Rejected` (the body,
    /// not the caller, is at fault); everything else that is not 2xx is an error.
    async fn post_sync(&self, token: &str, body: &Value) -> Result<Posted, SyncError> {
        let resp = self
            .http
            .post(format!("{}/api/nodes/cos/sync", self.cfg.dashboard_url))
            .bearer_auth(token)
            .json(body)
            .send()
            .await
            .map_err(|e| SyncError::Failed(format!("could not reach the dashboard: {}", e.without_url())))?;
        let status = resp.status().as_u16();
        if matches!(status, 401 | 403) {
            return Err(SyncError::Rejected(format!("node token rejected (HTTP {status})")));
        }
        if (400..500).contains(&status) {
            return Ok(Posted::Rejected(status));
        }
        if !(200..300).contains(&status) {
            return Err(SyncError::Failed(format!("dashboard answered HTTP {status}")));
        }
        let reply: Value = resp
            .json()
            .await
            .map_err(|e| SyncError::Failed(format!("dashboard reply is not JSON: {}", e.without_url())))?;
        Ok(Posted::Reply(reply))
    }

    /// The whole batch was rejected: send status and results alone, then the
    /// turns and then the traces by halves, so one bad row cannot hold back the
    /// rest. A row that is rejected on its own (the second rejection) is
    /// skipped: the cursor moves past it and the skip is logged once. If status
    /// and results are themselves rejected the rows are not blamed and nothing
    /// advances.
    async fn sync_split(&self, token: &str, turns: &[Value], traces: &[Value], results: &[Value], status: Value) -> Result<Split, SyncError> {
        let mut out = Split { replies: Vec::new(), turns: 0, traces: 0, results: 0, skipped: 0 };
        let head = json!({"turns": [], "status": status, "results": results, "traces": []});
        match self.post_sync(token, &head).await? {
            Posted::Reply(r) => {
                out.replies.push(r);
                out.results = results.len();
                let mut st = self.st();
                let n = results.len().min(st.results.len());
                st.results.drain(..n);
            }
            Posted::Rejected(code) => return Err(SyncError::Failed(format!("dashboard answered HTTP {code} to status and results alone"))),
        }
        for (rows, is_turns) in [(turns, true), (traces, false)] {
            let mut stack: Vec<&[Value]> = if rows.is_empty() { Vec::new() } else { vec![rows] };
            while let Some(chunk) = stack.pop() {
                let body = if is_turns {
                    json!({"turns": chunk, "status": head["status"], "results": [], "traces": []})
                } else {
                    json!({"turns": [], "status": head["status"], "results": [], "traces": chunk})
                };
                match self.post_sync(token, &body).await? {
                    Posted::Reply(r) => {
                        out.replies.push(r);
                        let mut st = self.st();
                        for row in chunk {
                            if is_turns { mark_seen(&mut st, key_of(row)) } else { mark_trace_seen(&mut st, row) }
                        }
                        if is_turns { out.turns += chunk.len() } else { out.traces += chunk.len() }
                    }
                    Posted::Rejected(code) if chunk.len() == 1 => {
                        let row = &chunk[0];
                        let id = row.get("turn_id").and_then(Value::as_str).unwrap_or("?");
                        tracing::warn!("bridge sync: skipping {} {id} after two rejections (HTTP {code})", if is_turns { "turn" } else { "trace" });
                        let mut st = self.st();
                        if is_turns { mark_seen(&mut st, key_of(row)) } else { mark_trace_seen(&mut st, row) }
                        out.skipped += 1;
                    }
                    Posted::Rejected(_) => {
                        let (l, r) = chunk.split_at(chunk.len() / 2);
                        // Pushed so the first half is tried first.
                        stack.push(r);
                        stack.push(l);
                    }
                }
            }
        }
        Ok(out)
    }

    /// Unsent final turns from the agent, oldest first, at most 50. An
    /// unreachable agent yields none (the status says why).
    async fn new_turns(&self) -> Vec<Value> {
        let url = format!("{}/v1/cos/turns?limit=100", self.cfg.agent);
        let Some(v) = self.agent_get(&url).await else { return Vec::new() };
        v.get("turns")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter(|t| !self.st().seen.contains(&key_of(t)))
                    .take(MAX_TURNS_PER_SYNC)
                    .map(|t| {
                        let mut t = t.clone();
                        if let Some(s) = t.get("text").and_then(Value::as_str).map(|s| cut(s, MAX_TURN_TEXT)) {
                            t["text"] = Value::String(s);
                        }
                        t
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Traces of finished turns not yet delivered, oldest first, at most 50,
    /// reduced to the fields the dashboard stores. An agent without the
    /// route, or an unreachable one, yields none.
    async fn new_traces(&self) -> Vec<Value> {
        let since = self.st().trace_since.clone();
        let mut url = format!("{}/v1/cos/traces?limit=100", self.cfg.agent);
        if let Some(s) = &since {
            // Only characters of an RFC 3339 UTC stamp reach the query.
            if s.bytes().all(|b| b.is_ascii_digit() || matches!(b, b'-' | b':' | b'.' | b'T' | b'Z')) {
                url.push_str(&format!("&since={}", s.replace(':', "%3A")));
            }
        }
        let Some(v) = self.agent_get(&url).await else { return Vec::new() };
        v.get("traces")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(contract_trace)
                    .filter(|t| !self.st().seen_traces.contains(t["turn_id"].as_str().unwrap_or("")))
                    .take(MAX_TRACES_PER_SYNC)
                    .collect()
            })
            .unwrap_or_default()
    }

    async fn agent_get(&self, url: &str) -> Option<Value> {
        let r = self.http.get(url).send().await.ok()?;
        if !r.status().is_success() {
            return None;
        }
        r.json().await.ok()
    }

    async fn status(&self) -> Value {
        let health = self.agent_get(&format!("{}/healthz", self.cfg.agent)).await;
        let mut agent = self
            .agent_get(&format!("{}/v1/cos/status", self.cfg.agent))
            .await
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({}));
        agent["healthy"] = Value::Bool(health.as_ref().and_then(|h| h.get("ok")).and_then(Value::as_bool) == Some(true));
        json!({
            "agent": agent,
            "mentra": {
                "listen": read_status_file(&self.cfg.mentra_dir.join("listen.status.json")),
                "speak": read_status_file(&self.cfg.mentra_dir.join("speak.status.json")),
            },
            "updated_at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        })
    }

    /// Run each request and queue its outcome for the next sync.
    async fn handle_all(&self, token: &str, requests: &[Value]) {
        for r in requests {
            let result = self.handle(token, r).await;
            self.st().results.push(result);
        }
    }

    /// One long-poll of `GET /api/nodes/cos/pending`: waits up to 25 s for
    /// queued requests, runs them, and wakes the sync loop so their results
    /// go out at once. Returns how many requests it ran.
    pub async fn poll_once(&self) -> Result<usize, SyncError> {
        let token = read_token(&self.cfg.token_file).map_err(SyncError::Failed)?;
        let resp = self
            .poll_http
            .get(format!("{}/api/nodes/cos/pending?wait={POLL_WAIT_SECS}", self.cfg.dashboard_url))
            .bearer_auth(&token)
            .send()
            .await
            .map_err(|e| SyncError::Failed(format!("could not reach the dashboard: {}", e.without_url())))?;
        let status = resp.status().as_u16();
        if matches!(status, 401 | 403) {
            return Err(SyncError::Rejected(format!("node token rejected (HTTP {status})")));
        }
        if !(200..300).contains(&status) {
            return Err(SyncError::Failed(format!("dashboard answered HTTP {status}")));
        }
        let reply: Value = resp
            .json()
            .await
            .map_err(|e| SyncError::Failed(format!("dashboard reply is not JSON: {}", e.without_url())))?;
        let requests = reply.get("requests").and_then(Value::as_array).cloned().unwrap_or_default();
        self.handle_all(&token, &requests).await;
        if !requests.is_empty() {
            self.wake.notify_one();
        }
        Ok(requests.len())
    }

    /// Dispatch on the request kind (`utterance` when absent).
    async fn handle(&self, token: &str, req: &Value) -> Value {
        let id = req.get("id").and_then(Value::as_str).unwrap_or("").to_owned();
        let fail = |e: String| json!({"id": id, "status": "failed", "error": e});
        if id.is_empty() {
            return fail("request has no id".into());
        }
        match req.get("kind").and_then(Value::as_str).unwrap_or("utterance") {
            "utterance" => self.deliver(req).await,
            kind @ ("listen" | "ptt_mentra" | "voice_volume") => match self.write_control(kind, req.get("payload").unwrap_or(&Value::Null)) {
                Ok(()) => json!({"id": id, "status": "accepted"}),
                Err(e) => fail(e),
            },
            "audio" => self.handle_audio(token, &id, req).await,
            other => fail(format!("unknown request kind {other:?}")),
        }
    }

    /// Merge a `listen`, `ptt_mentra` or `voice_volume` change into `control.json` (atomic
    /// temp file + rename); the glasses loop polls it.
    fn write_control(&self, kind: &str, payload: &Value) -> Result<(), String> {
        let path = self.cfg.mentra_dir.join("control.json");
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        // A missing or corrupt file starts fresh: listening on, no hold.
        let mut ctl = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({"listening": true}));
        match kind {
            "listen" => {
                let on = payload.get("on").and_then(Value::as_bool).ok_or("listen payload needs a boolean `on`")?;
                ctl["listening"] = Value::Bool(on);
            }
            "voice_volume" => {
                // Linear gain the glasses speak loop applies to the reply wav;
                // it can sit below the glasses' hardware volume floor.
                let v = payload.get("volume").and_then(Value::as_f64).filter(|v| v.is_finite() && (0.0..=1.0).contains(v)).ok_or("voice_volume payload needs `volume` between 0 and 1")?;
                ctl["voice_volume"] = json!(v);
            }
            _ => {
                let hold = payload.get("hold_id").and_then(Value::as_str).filter(|h| !h.is_empty() && h.len() <= 64).ok_or("ptt_mentra payload needs a `hold_id`")?;
                let phase = payload.get("phase").and_then(Value::as_str).filter(|p| matches!(*p, "down" | "up")).ok_or("ptt_mentra phase must be \"down\" or \"up\"")?;
                ctl["ptt"] = json!({"hold_id": hold, "phase": phase, "at": now});
            }
        }
        ctl["updated_at"] = Value::String(now);
        write_atomic(&path, &serde_json::to_string(&ctl).map_err(|e| e.to_string())?).map_err(|e| format!("control file {}: {e}", path.display()))
    }

    /// Fetch, validate and transcribe a phone clip, then post it as a voice turn.
    async fn handle_audio(&self, token: &str, id: &str, req: &Value) -> Value {
        let fail = |e: String| json!({"id": id, "status": "failed", "error": e});
        let Some(stt) = &self.stt else { return fail("this bridge build has no STT".into()) };
        if id.contains(['/', '?', '#']) {
            return fail("request id is not a plain id".into());
        }
        let wav = match self.fetch_audio(token, id).await {
            Ok(b) => b,
            Err(e) => return fail(e),
        };
        let samples = match decode_clip(&wav) {
            Ok(s) => s,
            Err(e) => return fail(e),
        };
        let text = match stt.transcribe(samples).await {
            Ok(t) => t.trim().to_owned(),
            Err(e) => return fail(e),
        };
        if text.is_empty() {
            return fail("no speech".into());
        }
        let _ = req;
        match self.post_utterance(&text, "voice", "dashboard").await {
            Ok(turn_id) => json!({"id": id, "status": "accepted", "turn_id": turn_id, "text": text}),
            Err(e) => fail(e),
        }
    }

    async fn fetch_audio(&self, token: &str, id: &str) -> Result<Vec<u8>, String> {
        let mut resp = self
            .http
            .get(format!("{}/api/nodes/cos/audio/{id}", self.cfg.dashboard_url))
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| format!("audio download failed: {}", e.without_url()))?;
        if !resp.status().is_success() {
            return Err(format!("audio download answered HTTP {}", resp.status().as_u16()));
        }
        let mut out = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(|e| format!("audio download failed: {}", e.without_url()))? {
            if out.len() + chunk.len() > MAX_AUDIO_BYTES {
                return Err(format!("audio is larger than {MAX_AUDIO_SECS} s of 16 kHz mono PCM16"));
            }
            out.extend_from_slice(&chunk);
        }
        Ok(out)
    }

    /// `POST /v1/cos/utterance`; the turn id, or the agent's error.
    async fn post_utterance(&self, text: &str, source: &str, endpoint: &str) -> Result<String, String> {
        let r = self
            .http
            .post(format!("{}/v1/cos/utterance", self.cfg.agent))
            .json(&json!({"text": text, "source": source, "endpoint": endpoint}))
            .send()
            .await
            .map_err(|e| format!("agent unreachable: {}", e.without_url()))?;
        let code = r.status().as_u16();
        let v: Value = r.json().await.unwrap_or(Value::Null);
        match (code, v.get("turn_id").and_then(Value::as_str)) {
            (200..=299, Some(t)) => Ok(t.to_owned()),
            _ => Err(v.get("error").and_then(Value::as_str).map(str::to_owned).unwrap_or_else(|| format!("agent answered HTTP {code}"))),
        }
    }

    /// Run one dashboard request through the local agent and describe the outcome.
    async fn deliver(&self, req: &Value) -> Value {
        let id = req.get("id").and_then(Value::as_str).unwrap_or("").to_owned();
        let fail = |e: String| json!({"id": id, "status": "failed", "error": e});
        if id.is_empty() {
            return fail("request has no id".into());
        }
        let endpoint = req.get("endpoint").and_then(Value::as_str).unwrap_or("");
        if !matches!(endpoint, "mentra" | "dashboard") {
            return fail(format!("unknown endpoint {endpoint:?}"));
        }
        let text = req.get("text").and_then(Value::as_str).map(str::trim).unwrap_or("");
        if text.is_empty() || text.chars().count() > MAX_REQUEST_TEXT {
            return fail(format!("text must be 1 to {MAX_REQUEST_TEXT} characters"));
        }
        match self.post_utterance(text, "typed", endpoint).await {
            Ok(turn_id) => json!({"id": id, "status": "accepted", "turn_id": turn_id}),
            Err(e) => fail(e),
        }
    }
}

/// The dashboard's `cos_traces` fields from an agent trace; `None` for one
/// without a usable turn id. Lists are cut to the dashboard's size limits and
/// no field outside the contract is sent.
fn contract_trace(t: &Value) -> Option<Value> {
    let turn_id = t.get("turn_id").and_then(Value::as_str).filter(|s| (1..=128).contains(&s.len()))?;
    let list = |k: &str, max_bytes: usize| -> Vec<Value> {
        let mut items: Vec<Value> = t.get(k).and_then(Value::as_array).cloned().unwrap_or_default();
        while !items.is_empty() && serde_json::to_string(&items).map(|s| s.len()).unwrap_or(usize::MAX) > max_bytes {
            items.pop();
        }
        items
    };
    let text = |k: &str, max: usize| t.get(k).and_then(Value::as_str).map(|s| s.chars().take(max).collect::<String>()).unwrap_or_default();
    Some(json!({
        "turn_id": turn_id,
        "runtime": t.get("runtime").and_then(Value::as_str).filter(|r| matches!(*r, "local" | "shadow" | "cloud")).unwrap_or("local"),
        "model": text("model", 128),
        "role": text("role", 64),
        "tools_offered": list("tools_offered", MAX_TRACE_LIST_BYTES),
        "tools_called": list("tools_called", MAX_TRACE_LIST_BYTES),
        "gates": list("gates", MAX_TRACE_GATES_BYTES),
        "latency_ms": t.get("latency_ms").and_then(Value::as_u64).filter(|n| *n <= 999_999_999),
        "cost_usd": t.get("cost_usd").and_then(Value::as_f64).filter(|c| c.is_finite() && (0.0..1000.0).contains(c)),
        "ts": t.get("ts").and_then(Value::as_str).unwrap_or(""),
    }))
}

fn mark_trace_seen(st: &mut Shared, t: &Value) {
    let Some(id) = t["turn_id"].as_str() else { return };
    if st.seen_traces.insert(id.to_owned()) {
        st.seen_trace_order.push_back(id.to_owned());
        while st.seen_trace_order.len() > SEEN_CAP {
            if let Some(old) = st.seen_trace_order.pop_front() {
                st.seen_traces.remove(&old);
            }
        }
    }
    if let Some(ts) = t["ts"].as_str().filter(|s| !s.is_empty())
        && st.trace_since.as_deref().is_none_or(|cur| ts > cur)
    {
        st.trace_since = Some(ts.to_owned());
    }
}

fn mark_seen(st: &mut Shared, key: (String, String)) {
    if st.seen.insert(key.clone()) {
        st.seen_order.push_back(key);
        while st.seen_order.len() > SEEN_CAP {
            if let Some(old) = st.seen_order.pop_front() {
                st.seen.remove(&old);
            }
        }
    }
}

/// Atomic file write: a temp file in the same directory, then rename.
fn write_atomic(path: &Path, contents: &str) -> std::io::Result<()> {
    let dir = path.parent().ok_or_else(|| std::io::Error::other("path has no directory"))?;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".{}.{}.tmp", file_name(path), std::process::id()));
    let r = std::fs::write(&tmp, contents).and_then(|()| std::fs::rename(&tmp, path));
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

/// A phone clip must be 16 kHz mono PCM16 WAV of at most 15 s.
fn decode_clip(bytes: &[u8]) -> Result<Vec<i16>, String> {
    let mut r = hound::WavReader::new(std::io::Cursor::new(bytes)).map_err(|e| format!("not a WAV file: {e}"))?;
    let spec = r.spec();
    if spec.channels != 1 || spec.sample_rate != 16_000 || spec.bits_per_sample != 16 || spec.sample_format != hound::SampleFormat::Int {
        return Err(format!(
            "audio must be 16 kHz mono PCM16 WAV (got {} Hz, {} channel(s), {}-bit)",
            spec.sample_rate, spec.channels, spec.bits_per_sample
        ));
    }
    if r.len() > 16_000 * MAX_AUDIO_SECS {
        return Err(format!("audio is longer than {MAX_AUDIO_SECS} s"));
    }
    r.samples::<i16>().collect::<Result<Vec<_>, _>>().map_err(|e| format!("bad WAV data: {e}"))
}

fn key_of(t: &Value) -> (String, String) {
    let s = |k: &str| t.get(k).and_then(Value::as_str).unwrap_or("").to_owned();
    (s("turn_id"), s("role"))
}

/// `null` when the file is missing, `{"error": ...}` when it cannot be used.
fn read_status_file(path: &Path) -> Value {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Value::Null,
        Err(e) => return json!({"error": format!("{}: {e}", file_name(path))}),
    };
    if meta.len() > MAX_STATUS_FILE {
        return json!({"error": format!("{} is larger than {MAX_STATUS_FILE} bytes", file_name(path))});
    }
    match std::fs::read_to_string(path) {
        Err(e) => json!({"error": format!("{}: {e}", file_name(path))}),
        Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| json!({"error": format!("{} is not valid JSON: {e}", file_name(path))})),
    }
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

fn cut(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// The body for `--dry-run`: turn text and transcripts shortened.
fn dry_run_view(body: &Value) -> Value {
    fn walk(v: &mut Value) {
        match v {
            Value::String(s) => *s = cut(s, DRY_RUN_TEXT),
            Value::Array(a) => a.iter_mut().for_each(walk),
            Value::Object(o) => o.values_mut().for_each(walk),
            _ => {}
        }
    }
    let mut v = body.clone();
    walk(&mut v);
    v
}

/// The STT the binary wires in: parakeet in an `onnx` build, none otherwise.
fn default_transcriber() -> Option<Arc<dyn Transcriber>> {
    #[cfg(feature = "onnx")]
    {
        let cfg = crate::CosConfig::load(None).ok()?;
        Some(Arc::new(SttTranscriber(crate::cli::build_stt(&cfg))))
    }
    #[cfg(not(feature = "onnx"))]
    {
        None
    }
}

fn log_change(last: &mut Option<String>, who: &str, line: String) {
    if last.as_deref() != Some(line.as_str()) {
        tracing::info!("bridge {who}: {line}");
        *last = Some(line);
    }
}

fn outcome(r: Result<(), SyncError>, ok: &str, interval: Duration) -> (String, Duration) {
    match r {
        Ok(()) => (ok.to_owned(), interval),
        Err(SyncError::Rejected(m)) => (format!("{m}; backing off"), ERROR_BACKOFF),
        Err(SyncError::Failed(m)) => (format!("failed: {m}; backing off"), ERROR_BACKOFF),
    }
}

/// The `bridge` command.
pub async fn run(args: BridgeArgs) -> anyhow::Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home directory"))?;
    let cfg = load_config(&home, &args.agent).map_err(|e| anyhow::anyhow!(e))?;
    let bridge = Arc::new(Bridge::new(cfg).with_transcriber(default_transcriber()));
    if args.dry_run || args.once {
        return match bridge.sync_once(args.dry_run).await {
            Ok(r) if args.dry_run => {
                println!("{}", serde_json::to_string_pretty(&r.body)?);
                Ok(())
            }
            Ok(r) => {
                eprintln!("synced: {} turns, {} traces, {} requests", r.turns_sent, r.traces_sent, r.requests);
                Ok(())
            }
            Err(e) => Err(anyhow::anyhow!("{e}")),
        };
    }
    let interval = Duration::from_millis(args.interval_ms.max(200));

    // The long-poll: always open, re-opened at once, backing off only on errors.
    let poller = bridge.clone();
    tokio::spawn(async move {
        let mut last = None;
        loop {
            let (line, delay) = outcome(poller.poll_once().await.map(|_| ()), "long-poll open", Duration::ZERO);
            log_change(&mut last, "poll", line);
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
        }
    });

    let mut last = None;
    loop {
        let (line, delay) = outcome(bridge.sync_once(false).await.map(|_| ()), "syncing with the dashboard", interval);
        log_change(&mut last, "sync", line);
        tokio::select! {
            () = tokio::time::sleep(delay) => {}
            () = bridge.wake.notified(), if delay == interval => {}
        }
    }
}
