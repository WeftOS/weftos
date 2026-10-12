//! `weaver project up | down | status` (ADR-116 R1 process-compose integration).
//!
//! A project runs the way ADR-098 says: its own process-compose file, served on
//! the `process-compose-http` port its `compose/ports.yaml` claims. `up` runs
//! `process-compose up -D` there (detached, no TUI), `down` runs
//! `process-compose down -p <port>`, `status` reads the same
//! `GET /processes` the router index reads and adds the route and URLs.
//! Everything is an argument vector, no shell. A project with no process-compose
//! file, or no port claim, is refused with the reason; nothing is guessed.

use std::path::PathBuf;
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

use crate::project_add_detect::{RunKind, detect, dotenv_names};
use crate::project_add_receipt::{self as receipt, Receipt, receipts_dir};
use crate::project_install_handlers::InstallEnv;
use crate::router_index::pc_state;
use crate::router_overlay;
use crate::router_sources::{ManifestSource, load};

const PC_START_WAIT: Duration = Duration::from_secs(30);
const PROBE: Duration = Duration::from_millis(1500);

/// A project that can be started.
#[derive(Debug, Clone)]
pub struct RunTarget {
    pub receipt: Receipt,
    pub file: PathBuf,
    pub port: u16,
    /// `${VAR}` names the compose file needs that neither the environment nor the project's `.env` provides.
    pub missing_env: Vec<String>,
}

/// Find the receipt and what to run; refuse with the reason when it cannot run.
pub fn target(env: &InstallEnv, slug: &str) -> Result<RunTarget, String> {
    let r = receipt::read(&receipts_dir(&env.home), slug)?.ok_or_else(|| format!("no project {slug:?} was added on this machine with `weaver project add`"))?;
    let det = detect(&r.root, &|n| std::env::var_os(n).is_some_and(|v| !v.is_empty()));
    let RunKind::ProcessCompose { file } = &det.run else {
        return Err(format!("{slug} has no process-compose file ({}); start it by hand", det.notes.first().cloned().unwrap_or_default()));
    };
    let port = det.pc_http.ok_or_else(|| format!("{slug}: compose/ports.yaml has no `process-compose-http` claim, so the process-compose port is unknown; add `- {{ port: <n>, use: process-compose-http }}` to its claims"))?;
    let provided = dotenv_names(&r.root);
    let compose = std::fs::read_to_string(r.root.join(file)).unwrap_or_default();
    let mut missing_env: Vec<String> = crate::project_add_detect::env_names_of_compose(&compose)
        .into_iter()
        .filter(|n| std::env::var_os(n).is_none_or(|v| v.is_empty()) && !provided.contains(n))
        .collect();
    missing_env.sort();
    missing_env.dedup();
    Ok(RunTarget { file: r.root.join(file), port, missing_env, receipt: r })
}

/// Arguments for `process-compose` that start the project.
pub fn up_args(t: &RunTarget) -> Vec<String> {
    ["up", "-f", &t.file.to_string_lossy(), "-D", "-t=false", "-p", &t.port.to_string()].map(str::to_owned).to_vec()
}

/// Arguments that stop it.
pub fn down_args(port: u16) -> Vec<String> {
    vec!["down".into(), "-p".into(), port.to_string()]
}

fn run_pc(bin: &str, args: &[String], cwd: Option<&std::path::Path>) -> Result<String, String> {
    let mut cmd = std::process::Command::new(bin);
    cmd.args(args).stdin(std::process::Stdio::null());
    if let Some(c) = cwd {
        cmd.current_dir(c);
    }
    let mut child = cmd.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().map_err(|e| format!("cannot run {bin}: {e} (is process-compose installed?)"))?;
    let start = std::time::Instant::now();
    loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(_) => break,
            None if start.elapsed() > PC_START_WAIT => {
                let _ = child.kill();
                return Err(format!("{bin} did not return within {}s", PC_START_WAIT.as_secs()));
            }
            None => std::thread::sleep(Duration::from_millis(100)),
        }
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(if out.status.success() { &out.stdout } else { &out.stderr }).trim().chars().take(400).collect::<String>();
    if out.status.success() { Ok(text) } else { Err(format!("{bin} {} failed ({}): {text}", args.first().map(String::as_str).unwrap_or(""), out.status)) }
}

/// What `up` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpOutcome {
    Started,
    AlreadyRunning,
}

/// Start the project. Refuses on missing env vars (by name) and on a port that
/// something else holds.
pub async fn up(t: &RunTarget, bin: &str) -> Result<UpOutcome, String> {
    if !t.missing_env.is_empty() {
        return Err(format!("{} needs these environment variables, set in neither your environment nor its .env: {}", t.receipt.slug, t.missing_env.join(", ")));
    }
    if pc_state(Some(t.port), PROBE).await["state"] == "ok" {
        return Ok(UpOutcome::AlreadyRunning);
    }
    if tokio::net::TcpStream::connect(("127.0.0.1", t.port)).await.is_ok() {
        return Err(format!("port {} (process-compose-http) is held by something that is not this project's process-compose", t.port));
    }
    let (bin, args, root) = (bin.to_owned(), up_args(t), t.receipt.root.clone());
    tokio::task::spawn_blocking(move || run_pc(&bin, &args, Some(&root))).await.map_err(|e| e.to_string())??;
    for _ in 0..30 {
        if pc_state(Some(t.port), PROBE).await["state"] == "ok" {
            return Ok(UpOutcome::Started);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Err(format!("process-compose was started but did not answer on port {} within 15s; check its log in the project", t.port))
}

/// Stop the project. `Ok(false)` when it was not running.
pub async fn down(t: &RunTarget, bin: &str) -> Result<bool, String> {
    if pc_state(Some(t.port), PROBE).await["state"] != "ok" {
        return Ok(false);
    }
    let (bin, args) = (bin.to_owned(), down_args(t.port));
    tokio::task::spawn_blocking(move || run_pc(&bin, &args, None)).await.map_err(|e| e.to_string())??;
    Ok(true)
}

/// The route, its URLs and the process state of one added project.
#[derive(Debug, Clone, Serialize)]
pub struct StatusReport {
    pub slug: String,
    pub root: PathBuf,
    pub ulid: String,
    pub registered_as: String,
    /// `pc_state`'s document: `{state, running, total, processes, port}` or `{state: down|unknown, detail}`.
    pub process_compose: Value,
    pub route_prefix: Option<String>,
    pub route_source: Option<String>,
    pub local_url: Option<String>,
    pub tailnet_url: Option<String>,
    pub notes: Vec<String>,
}

/// This machine's tailnet DNS name (`tailscale status --json`, `Self.DNSName`), if it has one.
pub fn tailnet_host(bin: &str) -> Option<String> {
    let out = std::process::Command::new(bin).args(["status", "--json"]).stdin(std::process::Stdio::null()).output().ok()?;
    let v: Value = serde_json::from_slice(&out.stdout).ok()?;
    let name = v["Self"]["DNSName"].as_str()?.trim_end_matches('.');
    (!name.is_empty()).then(|| name.to_owned())
}

/// Read one added project's state. `tailscale` is the binary to ask for the machine's name.
pub async fn status(env: &InstallEnv, slug: &str, tailscale: &str) -> Result<StatusReport, String> {
    let r = receipt::read(&receipts_dir(&env.home), slug)?.ok_or_else(|| format!("no project {slug:?} was added on this machine with `weaver project add`"))?;
    let det = detect(&r.root, &|_| false);
    let mut notes = Vec::new();
    if !r.root.is_dir() {
        notes.push(format!("the checkout {} is missing", r.root.display()));
    }
    let process_compose = pc_state(det.pc_http, PROBE).await;
    let table = load(&ManifestSource { manifests_dir: env.manifests_dir.clone(), overlays_dir: router_overlay::overlays_dir(&env.home) });
    let route = table.routes.iter().find(|x| table.project(&x.project).is_some_and(|p| p.ulid.as_deref() == Some(r.ulid.as_str())));
    let (mut local_url, mut tailnet_url) = (None, None);
    if let Some(x) = route {
        local_url = Some(format!("http://127.0.0.1:{}{}/", x.port, x.prefix));
        tailnet_url = tailnet_host(tailscale).map(|h| format!("https://{h}{}/", x.prefix));
        if tailnet_url.is_none() {
            notes.push("no tailnet name: tailscale is not installed or not logged in".into());
        }
    } else {
        notes.push("no route is installed for this project; the tailnet router does not serve it".into());
    }
    Ok(StatusReport {
        slug: r.slug.clone(),
        root: r.root.clone(),
        ulid: r.ulid.clone(),
        registered_as: r.registered_as.clone(),
        process_compose,
        route_prefix: route.map(|x| x.prefix.clone()),
        route_source: route.map(|x| format!("{:?}", x.source).to_lowercase()),
        local_url,
        tailnet_url,
        notes,
    })
}

#[cfg(test)]
#[path = "project_add_run_tests.rs"]
mod tests;
