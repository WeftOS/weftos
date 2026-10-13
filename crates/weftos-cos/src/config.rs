//! Host configuration for the chief-of-staff agent.
//!
//! One small JSON file (default `~/.config/weftos/cos.json`), all fields
//! optional. Model ids are `provider/model` strings resolved by WeftOS's own
//! provider routing; the harness credential is never in this file, only the
//! path to a mode-0600 token file.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::CosError;

/// Default loopback bind (contract §3).
pub const DEFAULT_BIND: &str = "127.0.0.1:18300";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CosConfig {
    /// Loopback address for the conversation API. Non-loopback is refused.
    pub bind: String,
    /// Path to the app config (`.clawft/config.json` shape). When absent the
    /// layered loader (`weave.toml`, home JSON, cwd `.clawft/config.json`) is
    /// used, exactly as `weft agent` does.
    pub app_config: Option<PathBuf>,
    pub models: ModelsConfig,
    pub harness: HarnessConfig,
    /// Where `board.propose` and a stubbed `work.kickoff` write staged files
    /// for the steward.
    pub staging_dir: PathBuf,
    /// JSONL record of every tool call (name, summary, status, turn).
    pub tool_log: Option<PathBuf>,
    pub team: TeamConfig,
    pub voice: VoiceConfig,
    pub brain: BrainConfig,
    /// Remote listening (cos.v1.1).
    pub serve: ServeConfig,
    /// Remote endpoints by id (for example `mentra`). The `host` endpoint's
    /// speech is `voice.tts`.
    pub endpoints: BTreeMap<String, EndpointConfig>,
}

/// Remote listening. The loopback listener at `bind` always runs; this adds
/// a second one on the machine's Tailscale address, token-protected.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServeConfig {
    /// `"tailnet"` or `"tailnet:PORT"` (default port: the loopback port).
    /// Resolved to this machine's Tailscale IPv4; never `0.0.0.0`, and a
    /// non-tailnet address is refused.
    pub bind: Option<String>,
    /// Directory of per-endpoint token files (mode 0600), written by
    /// `weftos-cos endpoint add`.
    pub tokens_dir: PathBuf,
}

impl Default for ServeConfig {
    fn default() -> Self {
        Self { bind: None, tokens_dir: config_home().join("cos/endpoints") }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct EndpointConfig {
    /// Whether this endpoint speaks replies itself (the `speak` flag on its
    /// final `message`). Default true.
    pub speak: Option<bool>,
    /// Legacy form of `speak`, still accepted: `engine: "none"` means
    /// `speak: false`, any other engine means `speak: true`.
    pub tts: Option<EndpointTts>,
}

impl EndpointConfig {
    /// Whether replies to this endpoint carry `speak: true`.
    pub fn speaks(&self) -> bool {
        match (self.speak, &self.tts) {
            (Some(s), _) => s,
            (None, Some(t)) => t.engine != "none",
            (None, None) => true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EndpointTts {
    pub engine: String,
}

impl Default for EndpointTts {
    fn default() -> Self {
        Self { engine: "auto".into() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelsConfig {
    /// Fast model for quick turns. Default: the local MLX Qwen3-4B
    /// (`model::LOCAL_DEFAULT_MODEL`).
    pub quick: Option<String>,
    /// Model for planning turns. Default: the keyed cloud provider's
    /// frontier model when there is one, else the quick model.
    pub plan: Option<String>,
    /// Models tried in order when the model for a turn cannot answer.
    /// Default: the fast model of every keyed cloud provider; `[]` turns the
    /// fallback off.
    pub fallback: Option<Vec<String>>,
    /// OpenAI-compatible endpoint of the local server. Applied as
    /// `providers.local.apiBase` unless the app config already sets one.
    pub local_api_base: String,
    /// Local models think before answering unless told not to. `auto`
    /// (default): off for quick turns, on for planning turns; `on`, `off`, or
    /// `default` (leave the server template alone). A plan model that is the
    /// quick model shares the quick setting. Cloud models ignore it.
    pub reasoning: String,
    /// argv that starts the local server (detached by `weftos-cos model
    /// start`). Default: the llm lab's `~/llm/bin/serve`.
    pub serve_command: Option<Vec<String>>,
    /// Complexity score at or above which a turn is a planning turn (ADR-026
    /// uses 30%).
    pub plan_threshold: f32,
    /// Tool rounds per turn before the agent must answer without tools
    /// (1..=12). Default 6.
    pub max_tool_rounds: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HarnessConfig {
    pub base_url: String,
    /// Mode-0600 file holding a `wfb_` harness credential.
    pub token_file: PathBuf,
    /// The dashboard project this console belongs to (UUID). Required for
    /// project-history notes; the token must reach that project.
    pub project_id: Option<String>,
    /// Project-history route template (dashboard PR #19:
    /// `POST /api/harness/projects/[id]/events`). `{project_id}` is
    /// substituted from `project_id`.
    pub events_path: String,
}

fn is_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TeamConfig {
    /// The ADR-112 §4 run surface as an argv (for example
    /// `["weaver", "team", "up"]`). When unset, `work.kickoff` records a
    /// staged request and reports itself as stubbed.
    pub runtime_command: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct VoiceConfig {
    /// Input device name substring; default input when unset.
    pub device: Option<String>,
    /// Parakeet bundle directory; `WEFTOS_PARAKEET_DIR` / defaults when unset.
    pub stt_model_dir: Option<PathBuf>,
    /// Debug only: keep the captured audio as a WAV under `debug_audio_dir`.
    pub keep_audio_debug: bool,
    pub debug_audio_dir: Option<PathBuf>,
    /// Spoken replies. Default: the Mac's `say` where it exists, else off.
    pub tts: TtsConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TtsConfig {
    /// `say` (macOS), `command` (any argv that reads the text on stdin and
    /// plays it: a Kokoro, Pocket TTS or Orpheus wrapper) or `none`.
    /// `auto` picks `say` when it exists.
    pub engine: String,
    /// `say -v`; the system voice when unset.
    pub voice: Option<String>,
    /// `say -r` words per minute.
    pub rate: Option<u32>,
    /// argv for `engine: command`.
    pub command: Option<Vec<String>>,
}

impl Default for TtsConfig {
    fn default() -> Self {
        Self { engine: "auto".into(), voice: None, rate: None, command: None }
    }
}

/// The installation brain (ADR-121 decision 9): a ruvector (RVF) store the
/// chief of staff writes its notes to, each with brain metadata
/// (`agents/schemas/brain-metadata.schema.json`).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct BrainConfig {
    /// RVF file. Default `~/.weftos/brains/chief-of-staff/<installation>.rvf`.
    pub path: Option<PathBuf>,
    /// The installation the brain is bound to (a `weftos://.../installations/<id>`
    /// name or its id). Default `local`.
    pub installation: Option<String>,
    /// `sha256:` digest of the agent package. Default: read from
    /// `agents/catalog.json` (found from the working directory upward).
    pub package: Option<String>,
    /// Catalog to read the digest from, when not found from the working directory.
    pub catalog: Option<PathBuf>,
}

fn config_home() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config/weftos")
}

impl Default for ModelsConfig {
    fn default() -> Self {
        Self {
            quick: None,
            plan: None,
            fallback: None,
            local_api_base: crate::model::LOCAL_DEFAULT_BASE.into(),
            reasoning: "auto".into(),
            serve_command: None,
            plan_threshold: 0.3,
            max_tool_rounds: crate::agent::DEFAULT_MAX_TOOL_ROUNDS,
        }
    }
}

impl Default for HarnessConfig {
    fn default() -> Self {
        Self {
            base_url: std::env::var("WEFTOS_DASHBOARD_URL")
                .unwrap_or_else(|_| "https://weftos-dashboard.vercel.app".into()),
            token_file: std::env::var_os("WEFTOS_BOARD_TOKEN_FILE")
                .map(PathBuf::from)
                .unwrap_or_else(|| config_home().join("board-token")),
            project_id: std::env::var("WEFTOS_PROJECT_ID").ok().filter(|s| !s.is_empty()),
            events_path: "/api/harness/projects/{project_id}/events".into(),
        }
    }
}

impl Default for CosConfig {
    fn default() -> Self {
        Self {
            bind: DEFAULT_BIND.into(),
            app_config: None,
            models: ModelsConfig::default(),
            harness: HarnessConfig::default(),
            staging_dir: config_home().join("cos/staging"),
            tool_log: Some(config_home().join("cos/tool-log.jsonl")),
            team: TeamConfig::default(),
            voice: VoiceConfig::default(),
            brain: BrainConfig::default(),
            serve: ServeConfig::default(),
            endpoints: BTreeMap::new(),
        }
    }
}

impl CosConfig {
    /// Default config file location.
    pub fn default_path() -> PathBuf {
        config_home().join("cos.json")
    }

    /// Load from `path`, or from [`Self::default_path`] when it exists, or
    /// defaults otherwise. An explicit `path` that does not exist is an error.
    pub fn load(path: Option<&Path>) -> Result<Self, CosError> {
        let (path, required) = match path {
            Some(p) => (p.to_path_buf(), true),
            None => (Self::default_path(), false),
        };
        if !path.exists() {
            if required {
                return Err(CosError::Config(format!("config not found: {}", path.display())));
            }
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|e| CosError::Config(format!("read {}: {e}", path.display())))?;
        let cfg: Self = serde_json::from_str(&text)
            .map_err(|e| CosError::Config(format!("parse {}: {e}", path.display())))?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Parse and refuse a non-loopback bind.
    pub fn bind_addr(&self) -> Result<SocketAddr, CosError> {
        let addr: SocketAddr = self
            .bind
            .parse()
            .map_err(|e| CosError::Config(format!("bind {:?}: {e}", self.bind)))?;
        if !addr.ip().is_loopback() {
            return Err(CosError::Config(format!(
                "bind {addr} is not loopback; the conversation API is local only"
            )));
        }
        Ok(addr)
    }

    /// The port to serve on the tailnet address, when `serve.bind` asks for
    /// one. Anything but `tailnet` / `tailnet:PORT` is refused.
    pub fn tailnet_port(&self) -> Result<Option<u16>, CosError> {
        let Some(spec) = &self.serve.bind else { return Ok(None) };
        let port = match spec.as_str() {
            "tailnet" => self.bind_addr()?.port(),
            s => s
                .strip_prefix("tailnet:")
                .and_then(|p| p.parse::<u16>().ok())
                .filter(|p| *p != 0)
                .ok_or_else(|| CosError::Config(format!("serve.bind {spec:?} must be \"tailnet\" or \"tailnet:PORT\"; other addresses are refused")))?,
        };
        Ok(Some(port))
    }

    /// Remote endpoints whose engine is `none`.
    pub fn silent_endpoints(&self) -> Vec<String> {
        self.endpoints.iter().filter(|(_, c)| !c.speaks()).map(|(n, _)| n.clone()).collect()
    }

    fn validate(&self) -> Result<(), CosError> {
        self.bind_addr()?;
        self.tailnet_port()?;
        for (name, ep) in &self.endpoints {
            if !crate::conversation::valid_endpoint_name(name) || name == crate::conversation::HOST_ENDPOINT {
                return Err(CosError::Config(format!("endpoints.{name}: not a remote endpoint id (host is configured by voice.tts)")));
            }
            if let Some(t) = &ep.tts {
                if !["auto", "say", "command", "none"].contains(&t.engine.as_str()) {
                    return Err(CosError::Config(format!(
                        "endpoints.{name}.tts.engine must be auto, say, command or none; a remote endpoint only needs endpoints.{name}.speak: true or false (none means speak: false)"
                    )));
                }
                if let Some(speak) = ep.speak
                    && speak != (t.engine != "none")
                {
                    return Err(CosError::Config(format!(
                        "endpoints.{name}: speak: {speak} contradicts tts.engine {:?}; set endpoints.{name}.speak alone (tts.engine is the legacy form)",
                        t.engine
                    )));
                }
            }
        }
        if !["auto", "on", "off", "default"].contains(&self.models.reasoning.as_str()) {
            return Err(CosError::Config("models.reasoning must be auto, on, off or default".into()));
        }
        if !(0.0..=1.0).contains(&self.models.plan_threshold) {
            return Err(CosError::Config("models.plan_threshold must be within 0..=1".into()));
        }
        if !(1..=12).contains(&self.models.max_tool_rounds) {
            return Err(CosError::Config("models.max_tool_rounds must be within 1..=12".into()));
        }
        if !self.harness.events_path.starts_with('/') {
            return Err(CosError::Config("harness.events_path must start with '/'".into()));
        }
        if let Some(id) = &self.harness.project_id
            && !is_uuid(id)
        {
            return Err(CosError::Config("harness.project_id must be a UUID".into()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_bind_loopback() {
        let cfg = CosConfig::default();
        assert_eq!(cfg.bind_addr().unwrap().port(), 18300);
        assert!(cfg.bind_addr().unwrap().ip().is_loopback());
    }

    #[test]
    fn refuses_non_loopback_bind() {
        let cfg = CosConfig { bind: "0.0.0.0:18300".into(), ..Default::default() };
        assert!(matches!(cfg.bind_addr(), Err(CosError::Config(_))));
    }

    #[test]
    fn serve_bind_accepts_only_tailnet_specs() {
        let mut cfg = CosConfig::default();
        assert_eq!(cfg.tailnet_port().unwrap(), None);
        cfg.serve.bind = Some("tailnet".into());
        assert_eq!(cfg.tailnet_port().unwrap(), Some(18300));
        cfg.serve.bind = Some("tailnet:19000".into());
        assert_eq!(cfg.tailnet_port().unwrap(), Some(19000));
        for bad in ["0.0.0.0", "0.0.0.0:18300", "192.168.1.5:18300", "tailnet:", "tailnet:0", "tailnet:abc", "Tailnet"] {
            cfg.serve.bind = Some(bad.into());
            assert!(matches!(cfg.tailnet_port(), Err(CosError::Config(_))), "{bad}");
        }
    }

    #[test]
    fn endpoints_are_validated_and_silence_is_per_endpoint() {
        let cfg: CosConfig = serde_json::from_str(r#"{"endpoints": {"mentra": {"tts": {"engine": "none"}}, "pad": {}}}"#).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.silent_endpoints(), ["mentra"]);
        for bad in [r#"{"endpoints": {"host": {}}}"#, r#"{"endpoints": {"Bad Name": {}}}"#, r#"{"endpoints": {"x": {"tts": {"engine": "festival"}}}}"#] {
            let cfg: CosConfig = serde_json::from_str(bad).unwrap();
            assert!(matches!(cfg.validate(), Err(CosError::Config(_))), "{bad}");
        }
    }

    #[test]
    fn speak_setting_and_legacy_engine_agree() {
        let cfg: CosConfig = serde_json::from_str(
            r#"{"endpoints": {"a": {"speak": false}, "b": {"speak": true}, "c": {}, "d": {"tts": {"engine": "none"}}, "e": {"tts": {"engine": "say"}}, "f": {"speak": false, "tts": {"engine": "none"}}}}"#,
        )
        .unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.silent_endpoints(), ["a", "d", "f"]);
    }

    #[test]
    fn speak_contradicting_engine_is_refused_with_an_explanation() {
        let cfg: CosConfig = serde_json::from_str(r#"{"endpoints": {"x": {"speak": true, "tts": {"engine": "none"}}}}"#).unwrap();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("contradicts") && err.contains("endpoints.x.speak"), "{err}");
        let cfg: CosConfig = serde_json::from_str(r#"{"endpoints": {"x": {"tts": {"engine": "festival"}}}}"#).unwrap();
        assert!(cfg.validate().unwrap_err().to_string().contains("speak: true or false"));
        assert!(serde_json::from_str::<CosConfig>(r#"{"endpoints": {"x": {"speak": "no"}}}"#).is_err());
    }

    #[test]
    fn missing_explicit_path_is_an_error() {
        let err = CosConfig::load(Some(Path::new("/nonexistent/cos.json"))).unwrap_err();
        assert!(matches!(err, CosError::Config(_)));
    }
}
