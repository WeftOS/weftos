//! Pasqal Cloud backend — live implementation (EXPERIMENTAL, 0.6.x).
//!
//! Wires up Auth0 service-account authentication, batch submission, polling,
//! and result retrieval against `https://apis.pasqal.cloud`. Intended to work
//! against the free `EMU_FREE` device with zero QPU-hour cost.
//!
//! **Known caveat**: the exact JSON schema produced by Pulser's
//! `Sequence.to_abstract_repr()` is not fully documented in Rust. The builder
//! in `build_sequence_json` implements the stable subset (AnalogDevice,
//! global Rydberg channel, constant pulse) that matches Pulser v0.15+ output
//! for simple cases. For complex sequences, generate the JSON from the Pulser
//! Python SDK and submit via `submit_raw_sequence`.
//!
//! See `.planning/development_notes/pasqal-integration.md` §6 and §13.
//!
//! Feature: `quantum-pasqal` (off by default).

use async_trait::async_trait;
use serde::Deserialize;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

use crate::quantum_backend::{
    BackendStatus, EvolutionParams, JobHandle, JobStatus, QuantumBackend, QuantumError,
    QuantumResults,
};
use crate::quantum_state::QuantumCognitiveState;

const DEFAULT_API_URL: &str = "https://apis.pasqal.cloud";
const DEFAULT_AUTH_URL: &str = "https://pasqal.eu.auth0.com/oauth/token";
const AUTH_AUDIENCE: &str = "https://apis.pasqal.cloud/account/api/v1";
// Refresh the token this many seconds before actual expiry to avoid races.
const TOKEN_REFRESH_SKEW_SECS: u64 = 300;

/// Pasqal Cloud device selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasqalDevice {
    EmuFree,
    EmuTn,
    Fresnel,
}

impl PasqalDevice {
    pub fn as_api_str(self) -> &'static str {
        match self {
            PasqalDevice::EmuFree => "EMU_FREE",
            PasqalDevice::EmuTn => "EMU_TN",
            PasqalDevice::Fresnel => "FRESNEL",
        }
    }

    pub fn max_qubits(self) -> usize {
        match self {
            PasqalDevice::EmuFree => 25,
            PasqalDevice::EmuTn => 100,
            PasqalDevice::Fresnel => 100,
        }
    }

    /// Pulser abstract-repr `device.name` matching this Pasqal device.
    pub fn pulser_device_name(self) -> &'static str {
        match self {
            // EMU_FREE and FRESNEL both accept AnalogDevice sequences.
            PasqalDevice::EmuFree | PasqalDevice::EmuTn | PasqalDevice::Fresnel => "AnalogDevice",
        }
    }
}

#[derive(Debug, Clone)]
pub struct PasqalConfig {
    pub api_url: String,
    pub auth_url: String,
    pub project_id: String,
    pub device: PasqalDevice,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub http_timeout: Duration,
}

impl Default for PasqalConfig {
    fn default() -> Self {
        Self {
            api_url: DEFAULT_API_URL.into(),
            auth_url: DEFAULT_AUTH_URL.into(),
            project_id: String::new(),
            device: PasqalDevice::EmuFree,
            client_id: String::new(),
            client_secret: None,
            http_timeout: Duration::from_secs(30),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct Auth0Response {
    access_token: String,
    expires_in: u64,
    #[serde(default)]
    #[allow(dead_code)]
    token_type: String,
}

#[derive(Debug)]
struct CachedToken {
    access_token: String,
    expires_at: Instant,
}

impl CachedToken {
    fn is_valid(&self) -> bool {
        Instant::now() + Duration::from_secs(TOKEN_REFRESH_SKEW_SECS) < self.expires_at
    }
}

/// Live Pasqal backend. Holds a reqwest client and a cached Auth0 token.
pub struct PasqalBackend {
    cfg: PasqalConfig,
    http: reqwest::Client,
    token: Arc<Mutex<Option<CachedToken>>>,
}

impl PasqalBackend {
    pub fn new(cfg: PasqalConfig) -> Result<Self, QuantumError> {
        let http = reqwest::Client::builder()
            .timeout(cfg.http_timeout)
            .build()
            .map_err(|e| QuantumError::Transport(e.to_string()))?;
        Ok(Self {
            cfg,
            http,
            token: Arc::new(Mutex::new(None)),
        })
    }

    pub fn config(&self) -> &PasqalConfig {
        &self.cfg
    }

    async fn get_token(&self) -> Result<String, QuantumError> {
        let mut slot = self.token.lock().await;
        if let Some(t) = slot.as_ref() {
            if t.is_valid() {
                return Ok(t.access_token.clone());
            }
        }

        let secret = self
            .cfg
            .client_secret
            .as_ref()
            .ok_or_else(|| QuantumError::Auth("missing client_secret".into()))?;
        if self.cfg.client_id.is_empty() {
            return Err(QuantumError::Auth("missing client_id".into()));
        }

        let body = serde_json::json!({
            "grant_type": "client_credentials",
            "client_id": self.cfg.client_id,
            "client_secret": secret,
            "audience": AUTH_AUDIENCE,
        });

        let resp = self
            .http
            .post(&self.cfg.auth_url)
            .json(&body)
            .send()
            .await
            .map_err(|e| QuantumError::Transport(e.to_string()))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(QuantumError::Auth(format!("auth0 {}: {}", status, text)));
        }

        let parsed: Auth0Response = resp
            .json()
            .await
            .map_err(|e| QuantumError::Serde(e.to_string()))?;

        let token = CachedToken {
            access_token: parsed.access_token.clone(),
            expires_at: Instant::now() + Duration::from_secs(parsed.expires_in),
        };
        *slot = Some(token);
        Ok(parsed.access_token)
    }

    /// Submit a pre-built Pulser-format JSON sequence directly. Useful when
    /// the caller already has a golden JSON from the Python Pulser SDK.
    pub async fn submit_raw_sequence(
        &self,
        sequence_json: serde_json::Value,
        runs: u32,
    ) -> Result<JobHandle, QuantumError> {
        let token = self.get_token().await?;
        let body = serde_json::json!({
            "sequence_builder": sequence_json,
            "jobs": [{ "runs": runs, "variables": {} }],
            "device_type": self.cfg.device.as_api_str(),
            "project_id": self.cfg.project_id,
        });

        let url = format!("{}/api/v1/batches", self.cfg.api_url);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&token)
            .json(&body)
            .send()
            .await
            .map_err(|e| QuantumError::Transport(e.to_string()))?;

        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| QuantumError::Transport(e.to_string()))?;
        if !status.is_success() {
            return Err(QuantumError::Rejected(format!("{}: {}", status, text)));
        }

        let parsed: BatchCreateResponse = serde_json::from_str(&text)
            .map_err(|e| QuantumError::Serde(format!("{}: {}", e, text)))?;
        let job_id = parsed
            .data
            .jobs
            .first()
            .map(|j| j.id.clone())
            .unwrap_or_else(|| parsed.data.id.clone());
        Ok(JobHandle {
            backend: "pasqal",
            job_id,
            batch_id: Some(parsed.data.id),
        })
    }
}

/// Build a Pulser abstract-repr-compatible JSON for a constant-pulse evolution
/// on a named 2D register. Best-effort for `AnalogDevice` with a single global
/// Rydberg channel. Matches the stable subset of Pulser v0.15+ output.
pub fn build_sequence_json(
    register: &[(String, [f64; 2])],
    device: PasqalDevice,
    params: EvolutionParams,
) -> serde_json::Value {
    let atoms: Vec<serde_json::Value> = register
        .iter()
        .map(|(name, p)| serde_json::json!({ "name": name, "x": p[0], "y": p[1] }))
        .collect();

    serde_json::json!({
        "version": "1",
        "name": "weft-evolution",
        "device": { "name": device.pulser_device_name() },
        "register": atoms,
        "channels": { "rydberg_global": "rydberg_global" },
        "variables": {},
        "operations": [
            {
                "op": "pulse",
                "channel": "rydberg_global",
                "protocol": "min-delay",
                "amplitude": {
                    "kind": "constant",
                    "duration": params.duration_ns,
                    "value": params.omega_rad_per_us,
                },
                "detuning": {
                    "kind": "constant",
                    "duration": params.duration_ns,
                    "value": params.detuning_rad_per_us,
                },
                "phase": params.phase_rad,
                "post_phase_shift": 0.0,
            },
            { "op": "measure", "basis": "ground-rydberg" }
        ],
        "measurement": "ground-rydberg"
    })
}

/// Parse Pasqal measurement bitstrings into a `QuantumResults` with per-atom
/// Rydberg-excitation probabilities.
pub fn parse_results(bitstrings: Vec<Vec<u8>>, n_atoms: usize) -> QuantumResults {
    let shots = bitstrings.len() as u32;
    let mut counts = vec![0u64; n_atoms];
    for bs in &bitstrings {
        for (i, &b) in bs.iter().take(n_atoms).enumerate() {
            if b != 0 {
                counts[i] += 1;
            }
        }
    }
    let denom = shots.max(1) as f64;
    let rydberg_probs: Vec<f64> = counts.iter().map(|&c| c as f64 / denom).collect();
    QuantumResults {
        bitstrings,
        rydberg_probs,
        shots,
    }
}

// ---------------------------------------------------------------------------
// REST response types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct BatchCreateResponse {
    data: BatchData,
}

#[derive(Debug, Deserialize)]
struct BatchData {
    id: String,
    #[serde(default)]
    jobs: Vec<JobSummary>,
}

#[derive(Debug, Deserialize)]
struct JobSummary {
    id: String,
}

#[derive(Debug, Deserialize)]
struct JobStatusResponse {
    data: JobStatusData,
}

#[derive(Debug, Deserialize)]
struct JobStatusData {
    #[allow(dead_code)]
    id: String,
    status: String,
}

#[derive(Debug, Deserialize)]
struct BatchResultsResponse {
    #[serde(default)]
    data: Vec<JobResultData>,
}

#[derive(Debug, Deserialize)]
struct JobResultData {
    #[allow(dead_code)]
    #[serde(default)]
    id: Option<String>,
    /// Measurement counts: bitstring (as "01001") -> count.
    /// Pasqal returns counts keyed by bitstring; we expand to per-shot array.
    #[serde(default)]
    counts: std::collections::HashMap<String, u64>,
}

fn status_from_str(s: &str) -> JobStatus {
    match s.to_ascii_uppercase().as_str() {
        "PENDING" => JobStatus::Pending,
        "RUNNING" => JobStatus::Running,
        "DONE" => JobStatus::Done,
        "CANCELED" | "CANCELLED" => JobStatus::Canceled,
        _ => JobStatus::Error,
    }
}

fn counts_to_bitstrings(counts: &std::collections::HashMap<String, u64>) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for (bs, &n) in counts {
        let row: Vec<u8> = bs.chars().map(|c| if c == '1' { 1 } else { 0 }).collect();
        for _ in 0..n {
            out.push(row.clone());
        }
    }
    out
}

// ---------------------------------------------------------------------------
// QuantumBackend impl
// ---------------------------------------------------------------------------

#[async_trait]
impl QuantumBackend for PasqalBackend {
    fn name(&self) -> &'static str {
        "pasqal"
    }

    fn max_qubits(&self) -> usize {
        self.cfg.device.max_qubits()
    }

    async fn health_check(&self) -> Result<BackendStatus, QuantumError> {
        // Minimal reachability probe: HEAD / on the API base.
        let url = format!("{}/api/v1/devices", self.cfg.api_url);
        let reachable = match self.get_token().await {
            Ok(token) => self
                .http
                .get(&url)
                .bearer_auth(&token)
                .send()
                .await
                .map(|r| r.status().is_success())
                .unwrap_or(false),
            Err(_) => false,
        };
        Ok(BackendStatus {
            name: "pasqal",
            reachable,
            queue_depth: None,
            estimated_wait: None,
            max_qubits: self.max_qubits(),
        })
    }

    async fn submit_evolution(
        &self,
        register: &[(String, [f64; 2])],
        _state: &QuantumCognitiveState,
        params: EvolutionParams,
    ) -> Result<JobHandle, QuantumError> {
        if register.len() > self.max_qubits() {
            return Err(QuantumError::GraphTooLarge {
                nodes: register.len(),
                max: self.max_qubits(),
            });
        }
        let seq = build_sequence_json(register, self.cfg.device, params);
        self.submit_raw_sequence(seq, params.shots).await
    }

    async fn poll(&self, handle: &JobHandle) -> Result<JobStatus, QuantumError> {
        let token = self.get_token().await?;
        let url = format!("{}/api/v2/jobs/{}", self.cfg.api_url, handle.job_id);
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&token)
            .send()
            .await
            .map_err(|e| QuantumError::Transport(e.to_string()))?;
        let status_code = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| QuantumError::Transport(e.to_string()))?;
        if !status_code.is_success() {
            return Err(QuantumError::Rejected(format!("{}: {}", status_code, text)));
        }
        let parsed: JobStatusResponse = serde_json::from_str(&text)
            .map_err(|e| QuantumError::Serde(format!("{}: {}", e, text)))?;
        Ok(status_from_str(&parsed.data.status))
    }

    async fn get_results(
        &self,
        handle: &JobHandle,
    ) -> Result<Option<QuantumResults>, QuantumError> {
        let batch_id = handle
            .batch_id
            .as_ref()
            .ok_or_else(|| QuantumError::Rejected("job handle missing batch_id".into()))?;
        let token = self.get_token().await?;
        let url = format!("{}/api/v1/batches/{}/results", self.cfg.api_url, batch_id);
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&token)
            .send()
            .await
            .map_err(|e| QuantumError::Transport(e.to_string()))?;
        let status_code = resp.status();
        if status_code.as_u16() == 404 || status_code.as_u16() == 204 {
            return Ok(None);
        }
        let text = resp
            .text()
            .await
            .map_err(|e| QuantumError::Transport(e.to_string()))?;
        if !status_code.is_success() {
            return Err(QuantumError::Rejected(format!("{}: {}", status_code, text)));
        }
        let parsed: BatchResultsResponse = serde_json::from_str(&text)
            .map_err(|e| QuantumError::Serde(format!("{}: {}", e, text)))?;
        let Some(first) = parsed.data.into_iter().next() else {
            return Ok(None);
        };
        if first.counts.is_empty() {
            return Ok(None);
        }
        let bitstrings = counts_to_bitstrings(&first.counts);
        let n_atoms = bitstrings.first().map(|b| b.len()).unwrap_or(0);
        Ok(Some(parse_results(bitstrings, n_atoms)))
    }

    async fn cancel(&self, handle: &JobHandle) -> Result<(), QuantumError> {
        let token = self.get_token().await?;
        let url = format!("{}/api/v2/jobs/{}/cancel", self.cfg.api_url, handle.job_id);
        let resp = self
            .http
            .patch(&url)
            .bearer_auth(&token)
            .send()
            .await
            .map_err(|e| QuantumError::Transport(e.to_string()))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(QuantumError::Rejected(format!("{}: {}", status, text)));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_api_strings() {
        assert_eq!(PasqalDevice::EmuFree.as_api_str(), "EMU_FREE");
        assert_eq!(PasqalDevice::EmuTn.as_api_str(), "EMU_TN");
        assert_eq!(PasqalDevice::Fresnel.as_api_str(), "FRESNEL");
    }

    #[test]
    fn build_sequence_json_shape() {
        let reg = vec![("q0".into(), [0.0, 0.0]), ("q1".into(), [5.0, 0.0])];
        let j = build_sequence_json(&reg, PasqalDevice::EmuFree, EvolutionParams::default());
        assert_eq!(j["version"], "1");
        assert_eq!(j["device"]["name"], "AnalogDevice");
        assert_eq!(j["register"].as_array().unwrap().len(), 2);
        assert_eq!(j["register"][0]["name"], "q0");
        assert_eq!(j["register"][1]["x"], 5.0);
        assert_eq!(j["operations"][0]["op"], "pulse");
        assert_eq!(j["operations"][0]["channel"], "rydberg_global");
        assert_eq!(j["operations"][1]["op"], "measure");
    }

    #[test]
    fn parse_results_computes_rydberg_probs() {
        // 4 shots, 3 atoms. Atom 0 excited 3/4, atom 1 excited 1/4, atom 2 excited 2/4.
        let bs = vec![vec![1, 0, 1], vec![1, 0, 0], vec![1, 1, 1], vec![0, 0, 0]];
        let r = parse_results(bs, 3);
        assert_eq!(r.shots, 4);
        assert!((r.rydberg_probs[0] - 0.75).abs() < 1e-9);
        assert!((r.rydberg_probs[1] - 0.25).abs() < 1e-9);
        assert!((r.rydberg_probs[2] - 0.5).abs() < 1e-9);
    }

    #[test]
    fn counts_to_bitstrings_expands_correctly() {
        let mut counts = std::collections::HashMap::new();
        counts.insert("10".to_string(), 2u64);
        counts.insert("01".to_string(), 1u64);
        let bs = counts_to_bitstrings(&counts);
        assert_eq!(bs.len(), 3);
        let ones_atom_0: u32 = bs.iter().map(|b| b[0] as u32).sum();
        let ones_atom_1: u32 = bs.iter().map(|b| b[1] as u32).sum();
        assert_eq!(ones_atom_0, 2);
        assert_eq!(ones_atom_1, 1);
    }

    #[test]
    fn status_from_str_maps_known_values() {
        assert_eq!(status_from_str("PENDING"), JobStatus::Pending);
        assert_eq!(status_from_str("running"), JobStatus::Running);
        assert_eq!(status_from_str("DONE"), JobStatus::Done);
        assert_eq!(status_from_str("CANCELED"), JobStatus::Canceled);
        assert_eq!(status_from_str("CANCELLED"), JobStatus::Canceled);
        assert_eq!(status_from_str("nonsense"), JobStatus::Error);
    }

    #[test]
    fn missing_client_secret_is_auth_error() {
        let cfg = PasqalConfig {
            client_id: "abc".into(),
            client_secret: None,
            ..Default::default()
        };
        let backend = PasqalBackend::new(cfg).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = rt.block_on(backend.get_token()).unwrap_err();
        assert!(matches!(err, QuantumError::Auth(_)));
    }

    #[test]
    fn max_qubits_reflects_device() {
        let b = PasqalBackend::new(PasqalConfig {
            device: PasqalDevice::Fresnel,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(b.max_qubits(), 100);
    }
}

#[cfg(test)]
mod live_tests {
    //! End-to-end tests using a local wiremock server. These exercise the
    //! full HTTP + JSON path without hitting Pasqal Cloud.
    use super::*;
    use wiremock::matchers::{bearer_token, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_config(mock: &MockServer, auth_path: &str) -> PasqalConfig {
        PasqalConfig {
            api_url: mock.uri(),
            auth_url: format!("{}{}", mock.uri(), auth_path),
            project_id: "proj-1".into(),
            device: PasqalDevice::EmuFree,
            client_id: "client-1".into(),
            client_secret: Some("secret-1".into()),
            http_timeout: Duration::from_secs(5),
        }
    }

    #[tokio::test]
    async fn token_is_fetched_and_cached() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "tok-abc",
                "expires_in": 3600,
                "token_type": "Bearer"
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let cfg = test_config(&mock, "/oauth/token");
        let backend = PasqalBackend::new(cfg).unwrap();

        let t1 = backend.get_token().await.unwrap();
        let t2 = backend.get_token().await.unwrap();
        assert_eq!(t1, "tok-abc");
        assert_eq!(t2, "tok-abc");
        // Auth endpoint should have been hit exactly once (cache hit on 2nd call).
    }

    #[tokio::test]
    async fn submit_evolution_posts_batch_and_returns_handle() {
        let mock = MockServer::start().await;
        mount_auth_async(&mock, "tok-xyz", 3600).await;

        Mock::given(method("POST"))
            .and(path("/api/v1/batches"))
            .and(bearer_token("tok-xyz"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": {
                    "id": "batch-1",
                    "jobs": [{ "id": "job-1" }]
                }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let cfg = test_config(&mock, "/oauth/token");
        let backend = PasqalBackend::new(cfg).unwrap();
        let reg = vec![("q0".into(), [0.0, 0.0]), ("q1".into(), [5.0, 0.0])];
        let state = QuantumCognitiveState::uniform(2, &[0, 1]);
        let handle = backend
            .submit_evolution(&reg, &state, EvolutionParams::default())
            .await
            .unwrap();
        assert_eq!(handle.backend, "pasqal");
        assert_eq!(handle.job_id, "job-1");
        assert_eq!(handle.batch_id.as_deref(), Some("batch-1"));
    }

    #[tokio::test]
    async fn poll_parses_job_status() {
        let mock = MockServer::start().await;
        mount_auth_async(&mock, "tok-1", 3600).await;

        Mock::given(method("GET"))
            .and(path("/api/v2/jobs/job-1"))
            .and(bearer_token("tok-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": { "id": "job-1", "status": "DONE" }
            })))
            .mount(&mock)
            .await;

        let cfg = test_config(&mock, "/oauth/token");
        let backend = PasqalBackend::new(cfg).unwrap();
        let handle = JobHandle {
            backend: "pasqal",
            job_id: "job-1".into(),
            batch_id: Some("batch-1".into()),
        };
        assert_eq!(backend.poll(&handle).await.unwrap(), JobStatus::Done);
    }

    #[tokio::test]
    async fn get_results_parses_counts_to_probs() {
        let mock = MockServer::start().await;
        mount_auth_async(&mock, "tok-2", 3600).await;

        Mock::given(method("GET"))
            .and(path("/api/v1/batches/batch-1/results"))
            .and(bearer_token("tok-2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{
                    "id": "job-1",
                    "counts": { "10": 3, "01": 1 }
                }]
            })))
            .mount(&mock)
            .await;

        let cfg = test_config(&mock, "/oauth/token");
        let backend = PasqalBackend::new(cfg).unwrap();
        let handle = JobHandle {
            backend: "pasqal",
            job_id: "job-1".into(),
            batch_id: Some("batch-1".into()),
        };
        let results = backend.get_results(&handle).await.unwrap().unwrap();
        assert_eq!(results.shots, 4);
        // Atom 0 excited in "10" -> 3/4 = 0.75
        assert!((results.rydberg_probs[0] - 0.75).abs() < 1e-9);
        // Atom 1 excited in "01" -> 1/4 = 0.25
        assert!((results.rydberg_probs[1] - 0.25).abs() < 1e-9);
    }

    #[tokio::test]
    async fn get_results_returns_none_on_404() {
        let mock = MockServer::start().await;
        mount_auth_async(&mock, "tok-3", 3600).await;

        Mock::given(method("GET"))
            .and(path("/api/v1/batches/batch-x/results"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&mock)
            .await;

        let cfg = test_config(&mock, "/oauth/token");
        let backend = PasqalBackend::new(cfg).unwrap();
        let handle = JobHandle {
            backend: "pasqal",
            job_id: "job-x".into(),
            batch_id: Some("batch-x".into()),
        };
        assert!(backend.get_results(&handle).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn cancel_sends_patch_request() {
        let mock = MockServer::start().await;
        mount_auth_async(&mock, "tok-4", 3600).await;

        Mock::given(method("PATCH"))
            .and(path("/api/v2/jobs/job-7/cancel"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(1)
            .mount(&mock)
            .await;

        let cfg = test_config(&mock, "/oauth/token");
        let backend = PasqalBackend::new(cfg).unwrap();
        let handle = JobHandle {
            backend: "pasqal",
            job_id: "job-7".into(),
            batch_id: Some("batch-7".into()),
        };
        backend.cancel(&handle).await.unwrap();
    }

    #[tokio::test]
    async fn too_large_register_rejected_before_submit() {
        let mock = MockServer::start().await;
        let cfg = test_config(&mock, "/oauth/token");
        let backend = PasqalBackend::new(cfg).unwrap();
        // EMU_FREE max_qubits = 25; submit 26.
        let reg: Vec<(String, [f64; 2])> = (0..26)
            .map(|i| (format!("q{}", i), [i as f64 * 5.0, 0.0]))
            .collect();
        let state = QuantumCognitiveState::uniform(26, &(0..26).collect::<Vec<_>>());
        let err = backend
            .submit_evolution(&reg, &state, EvolutionParams::default())
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            QuantumError::GraphTooLarge { nodes: 26, max: 25 }
        ));
    }

    async fn mount_auth_async(mock: &MockServer, token: &str, expires_in: u64) {
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": token,
                "expires_in": expires_in,
                "token_type": "Bearer"
            })))
            .mount(mock)
            .await;
    }
}
