//! The chief of staff's models: the local default, how to start it, how to
//! tell it is up, and which cloud provider (if one is keyed) backs it.
//!
//! Local default: GLM-4.7-Flash (MLX, 4-bit, a 30B mixture with 3B active)
//! served by the llm lab's `~/llm/bin/serve` on `127.0.0.1:8081`. It makes
//! the right tool call every time against the full 16-tool prompt (Qwen3-4B,
//! tried first, named the tool in prose about half the time), and answers a
//! tool-calling turn in about three seconds on this Mac. It speaks the OpenAI
//! wire, so WeftOS's own `local` provider reaches it with only
//! `providers.local.apiBase` set. The ADR-060 Hermes alias on `:8090` stays
//! what `weft agent` uses; this host does not depend on it. The lighter
//! `local/mlx-community/Qwen3-4B-Instruct-2507-4bit` works for plain chat
//! (set `models.quick`).
//!
//! Cloud: a provider counts as configured when its key is present in the
//! app config or in the provider's environment variable. Only the presence
//! is ever looked at; the value is never read into a message or log.

use std::path::{Path, PathBuf};
use std::time::Duration;

use clawft_types::config::Config;

use crate::CosError;
use crate::config::ModelsConfig;

/// Route id of the built-in local default (`local/` is the keyless provider).
pub const LOCAL_DEFAULT_MODEL: &str = "local/mlx-community/GLM-4.7-Flash-4bit";
pub const LOCAL_DEFAULT_BASE: &str = "http://127.0.0.1:8081/v1";

/// A cloud provider the host can fall back to or plan with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cloud {
    pub provider: &'static str,
    pub quick: &'static str,
    pub plan: &'static str,
}

/// In preference order.
const CLOUDS: &[(&str, &str, &str, &str)] = &[
    ("anthropic", "ANTHROPIC_API_KEY", "anthropic/claude-haiku-4-5-20251001", "anthropic/claude-sonnet-5-5"),
    ("openrouter", "OPENROUTER_API_KEY", "openrouter/anthropic/claude-haiku-4.5", "openrouter/anthropic/claude-sonnet-4.5"),
];

fn app_key_present(app: &Config, provider: &str) -> bool {
    let key = match provider {
        "anthropic" => &app.providers.anthropic.api_key,
        "openrouter" => &app.providers.openrouter.api_key,
        _ => return false,
    };
    !key.is_empty()
}

/// Every cloud provider with a key, in preference order, by presence only.
/// `env_has` answers whether an environment variable is set and non-empty
/// (injected so tests need not touch the process environment). A key being
/// present does not mean it works (an account can be out of credit), which is
/// why all of them are kept as a fallback chain.
pub fn detect_clouds(app: &Config, env_has: &dyn Fn(&str) -> bool) -> Vec<Cloud> {
    CLOUDS
        .iter()
        .filter(|(p, var, _, _)| app_key_present(app, p) || env_has(var))
        .map(|(p, _, quick, plan)| Cloud { provider: p, quick, plan })
        .collect()
}

pub fn env_present(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|v| !v.is_empty())
}

/// The resolved model plan for a host: ids only, no providers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelPlan {
    pub quick: String,
    pub plan: String,
    /// Tried in order when the model for a turn cannot answer.
    pub fallback: Vec<String>,
}

impl ModelPlan {
    pub fn resolve(models: &ModelsConfig, clouds: &[Cloud]) -> Self {
        let quick = models.quick.clone().unwrap_or_else(|| LOCAL_DEFAULT_MODEL.to_string());
        let plan = models.plan.clone().or_else(|| clouds.first().map(|c| c.plan.to_string())).unwrap_or_else(|| quick.clone());
        let mut fallback: Vec<String> = match &models.fallback {
            // `[]` turns the fallback off.
            Some(ids) => ids.clone(),
            None => clouds.iter().map(|c| c.quick.to_string()).collect(),
        };
        // The primary is not its own fallback; no repeats.
        let mut seen = std::collections::HashSet::from([quick.clone()]);
        fallback.retain(|f| seen.insert(f.clone()));
        Self { quick, plan, fallback }
    }
}

/// Whether a route id goes to a local server (no key, a reachable endpoint).
pub fn is_local_route(model_id: &str) -> bool {
    matches!(model_id.split('/').next(), Some("local" | "ollama" | "hermes"))
}

/// Result of probing a model endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Health {
    Up,
    Down(String),
}

impl Health {
    pub fn is_up(&self) -> bool {
        matches!(self, Health::Up)
    }
}

/// `GET {base}/models` with a short timeout: any 2xx means the server is up.
pub async fn probe(api_base: &str) -> Health {
    let http = match reqwest::Client::builder().timeout(Duration::from_secs(2)).build() {
        Ok(h) => h,
        Err(e) => return Health::Down(e.to_string()),
    };
    let url = format!("{}/models", api_base.trim_end_matches('/'));
    match http.get(&url).send().await {
        Ok(r) if r.status().is_success() => Health::Up,
        Ok(r) => Health::Down(format!("{url} answered HTTP {}", r.status().as_u16())),
        Err(_) => Health::Down(format!("nothing is answering on {url}")),
    }
}

/// The model repo or path the default serve command loads: the local default
/// route without its `local/` prefix.
fn local_repo(model_id: &str) -> &str {
    model_id.strip_prefix("local/").unwrap_or(model_id)
}

fn port_of(api_base: &str) -> String {
    api_base
        .rsplit_once(':')
        .and_then(|(_, rest)| rest.split('/').next())
        .filter(|p| p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty())
        .unwrap_or("8081")
        .to_string()
}

/// The command that serves the local quick model: `models.serve_command`, or
/// the llm lab's `bin/serve` (the controller for local models on this Mac,
/// `~/llm`) on the port of `local_api_base`.
pub fn serve_argv(models: &ModelsConfig, quick_model: &str) -> Vec<String> {
    if let Some(cmd) = &models.serve_command {
        return cmd.clone();
    }
    let lab = dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join("llm/bin/serve");
    vec![lab.to_string_lossy().into_owned(), local_repo(quick_model).to_string(), "--port".into(), port_of(&models.local_api_base)]
}

/// Where the started server's pid and log live.
pub fn state_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".config/weftos/cos")
}

/// Start the local server detached (own process group, output to a log) and
/// record its pid. Returns the pid. Refuses when the command is missing.
pub fn start_detached(argv: &[String], dir: &Path) -> Result<u32, CosError> {
    let (prog, args) = argv.split_first().ok_or_else(|| CosError::Config("models.serve_command is empty".into()))?;
    if !Path::new(prog).exists() && which(prog).is_none() {
        return Err(CosError::Config(format!(
            "cannot start the local model: {prog} not found (set models.serve_command in cos.json)"
        )));
    }
    std::fs::create_dir_all(dir).map_err(|e| CosError::Config(format!("{}: {e}", dir.display())))?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("model.log"))
        .map_err(|e| CosError::Config(format!("model.log: {e}")))?;
    let log2 = log.try_clone().map_err(|e| CosError::Config(e.to_string()))?;
    let mut cmd = std::process::Command::new(prog);
    cmd.args(args).stdin(std::process::Stdio::null()).stdout(log).stderr(log2);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let child = cmd.spawn().map_err(|e| CosError::Config(format!("start {prog}: {e}")))?;
    let pid = child.id();
    std::fs::write(dir.join("model.pid"), pid.to_string()).map_err(|e| CosError::Config(format!("model.pid: {e}")))?;
    // The child keeps running after this handle drops.
    drop(child);
    Ok(pid)
}

fn which(prog: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|p| std::env::split_paths(&p).map(|d| d.join(prog)).find(|c| c.exists()))
}

/// Stop the server this host started: the pid in `model.pid`, only when that
/// process still looks like a model server. Never matches by name.
pub fn stop_started(dir: &Path) -> Result<String, CosError> {
    let pid_file = dir.join("model.pid");
    let raw = std::fs::read_to_string(&pid_file).map_err(|_| CosError::Config("no model started by this host (no model.pid)".into()))?;
    let pid: u32 = raw.trim().parse().map_err(|_| CosError::Config("model.pid is not a pid".into()))?;
    let out = std::process::Command::new("ps").args(["-p", &pid.to_string(), "-o", "command="]).output().map_err(|e| CosError::Config(e.to_string()))?;
    let cmdline = String::from_utf8_lossy(&out.stdout).to_string();
    if cmdline.trim().is_empty() {
        let _ = std::fs::remove_file(&pid_file);
        return Ok(format!("pid {pid} was already gone"));
    }
    if !(cmdline.contains("mlx_lm") || cmdline.contains("llama-server") || cmdline.contains("/serve")) {
        return Err(CosError::Config(format!("pid {pid} is not a model server any more; not stopping it")));
    }
    let status = std::process::Command::new("kill").arg(pid.to_string()).status().map_err(|e| CosError::Config(e.to_string()))?;
    let _ = std::fs::remove_file(&pid_file);
    if status.success() { Ok(format!("stopped pid {pid}")) } else { Err(CosError::Config(format!("kill {pid} failed"))) }
}

/// Load the model into memory with a one-token completion. The MLX server
/// answers `/models` at once but loads weights on the first request (about
/// 45 s for the default model), which would otherwise be the owner's first
/// spoken turn. `model_id` is the route id (`local/<repo>`).
pub async fn warm(api_base: &str, model_id: &str, timeout: Duration) -> Health {
    let http = match reqwest::Client::builder().timeout(timeout).build() {
        Ok(h) => h,
        Err(e) => return Health::Down(e.to_string()),
    };
    let url = format!("{}/chat/completions", api_base.trim_end_matches('/'));
    let body = serde_json::json!({"model": local_repo(model_id), "max_tokens": 1, "messages": [{"role": "user", "content": "ok"}]});
    match http.post(&url).json(&body).send().await {
        Ok(r) if r.status().is_success() => Health::Up,
        Ok(r) => Health::Down(format!("warm-up answered HTTP {}", r.status().as_u16())),
        Err(e) => Health::Down(format!("warm-up failed: {e}")),
    }
}

/// Wait until `api_base` answers or `timeout` passes.
pub async fn wait_up(api_base: &str, timeout: Duration) -> Health {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let h = probe(api_base).await;
        if h.is_up() || tokio::time::Instant::now() >= deadline {
            return h;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none(_: &str) -> bool {
        false
    }

    #[test]
    fn defaults_to_the_local_model_and_no_cloud() {
        let p = ModelPlan::resolve(&ModelsConfig::default(), &[]);
        assert_eq!(p.quick, LOCAL_DEFAULT_MODEL);
        assert_eq!(p.plan, LOCAL_DEFAULT_MODEL);
        assert!(p.fallback.is_empty());
    }

    #[test]
    fn keyed_clouds_plan_and_back_the_local_model_in_order() {
        let clouds = detect_clouds(&Config::default(), &|v| v == "ANTHROPIC_API_KEY" || v == "OPENROUTER_API_KEY");
        assert_eq!(clouds.iter().map(|c| c.provider).collect::<Vec<_>>(), ["anthropic", "openrouter"]);
        let p = ModelPlan::resolve(&ModelsConfig::default(), &clouds);
        assert_eq!(p.quick, LOCAL_DEFAULT_MODEL);
        assert_eq!(p.plan, "anthropic/claude-sonnet-5-5");
        assert_eq!(p.fallback, ["anthropic/claude-haiku-4-5-20251001", "openrouter/anthropic/claude-haiku-4.5"]);
    }

    #[test]
    fn no_key_means_no_cloud() {
        assert!(detect_clouds(&Config::default(), &none).is_empty());
    }

    #[test]
    fn an_empty_fallback_list_disables_and_explicit_ids_win() {
        let clouds = detect_clouds(&Config::default(), &|_| true);
        let off = ModelsConfig { fallback: Some(vec![]), ..Default::default() };
        assert!(ModelPlan::resolve(&off, &clouds).fallback.is_empty());
        let set = ModelsConfig { quick: Some("openai/x".into()), plan: Some("openai/y".into()), fallback: Some(vec!["openai/x".into(), "openai/z".into(), "openai/z".into()]), ..Default::default() };
        let p = ModelPlan::resolve(&set, &clouds);
        assert_eq!((p.quick.as_str(), p.plan.as_str()), ("openai/x", "openai/y"));
        assert_eq!(p.fallback, ["openai/z"], "the primary and repeats are dropped");
    }

    #[test]
    fn serve_argv_uses_the_lab_controller_and_port() {
        let m = ModelsConfig { local_api_base: "http://127.0.0.1:8123/v1".into(), ..Default::default() };
        let argv = serve_argv(&m, LOCAL_DEFAULT_MODEL);
        assert!(argv[0].ends_with("llm/bin/serve"));
        assert_eq!(argv[1], "mlx-community/GLM-4.7-Flash-4bit");
        assert_eq!(&argv[2..], ["--port", "8123"]);
    }

    #[test]
    fn routes_are_local_by_prefix() {
        assert!(is_local_route(LOCAL_DEFAULT_MODEL));
        assert!(!is_local_route("anthropic/claude-sonnet-5-5"));
    }

    #[tokio::test]
    async fn probe_says_down_for_a_closed_port() {
        let h = probe("http://127.0.0.1:9/v1").await;
        assert!(matches!(h, Health::Down(m) if m.contains("nothing is answering")));
    }

    #[test]
    fn start_refuses_a_missing_command() {
        let dir = tempfile::tempdir().unwrap();
        let err = start_detached(&["/nonexistent/serve".into()], dir.path()).unwrap_err();
        assert!(matches!(err, CosError::Config(m) if m.contains("not found")));
        assert!(start_detached(&[], dir.path()).is_err());
    }

    #[test]
    fn stop_refuses_without_a_pid_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(stop_started(dir.path()).is_err());
    }
}
