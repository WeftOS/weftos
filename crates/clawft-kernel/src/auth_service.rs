//! Centralized credential management service -- Plan 9 Factotum pattern (K5-G2).
//!
//! The [`AuthService`] manages external credentials centrally so that agents
//! never hold raw secrets. Instead, agents request scoped, time-limited tokens
//! via IPC. All credential access is audited.

#[cfg(feature = "exochain")]
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use crate::error::KernelError;
use crate::health::HealthStatus;
use crate::process::Pid;
use crate::secret_seal::SecretSeal;
use crate::service::{ServiceType, SystemService};

#[cfg(feature = "exochain")]
use crate::chain::ChainManager;
#[cfg(feature = "exochain")]
use crate::gate::GateBackend;

// ---------------------------------------------------------------------------
// CredentialType
// ---------------------------------------------------------------------------

/// Classification of a stored credential.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum CredentialType {
    /// API key (e.g., OpenAI, GitHub).
    ApiKey,
    /// Bearer/OAuth token.
    BearerToken,
    /// TLS client certificate.
    Certificate,
    /// User-defined credential type.
    Custom(String),
}

impl std::fmt::Display for CredentialType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ApiKey => write!(f, "api_key"),
            Self::BearerToken => write!(f, "bearer_token"),
            Self::Certificate => write!(f, "certificate"),
            Self::Custom(s) => write!(f, "custom({s})"),
        }
    }
}

// ---------------------------------------------------------------------------
// StoredCredential
// ---------------------------------------------------------------------------

/// A credential stored by the AuthService, its value sealed with
/// XChaCha20-Poly1305 in memory.
#[derive(Debug, Clone)]
pub struct StoredCredential {
    /// Human-readable credential name.
    pub name: String,
    /// Credential classification.
    pub credential_type: CredentialType,
    /// Sealed credential value: nonce || ciphertext || tag (never exposed).
    encrypted_value: Vec<u8>,
    /// Agent IDs allowed to request tokens for this credential.
    pub allowed_agents: Vec<String>,
    /// When the credential was registered.
    pub created_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// IssuedToken
// ---------------------------------------------------------------------------

/// A scoped, time-limited token issued to an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssuedToken {
    /// Unique token identifier.
    pub token_id: String,
    /// Name of the credential this token grants access to.
    pub credential_name: String,
    /// PID of the agent the token was issued to.
    pub issued_to: Pid,
    /// When the token was issued.
    pub issued_at: DateTime<Utc>,
    /// When the token expires.
    pub expires_at: DateTime<Utc>,
    /// Scoped operations this token permits.
    pub scope: Vec<String>,
}

impl IssuedToken {
    /// Check whether the token has expired.
    pub fn is_expired(&self) -> bool {
        Utc::now() > self.expires_at
    }
}

// ---------------------------------------------------------------------------
// CredentialRequest / CredentialGrant
// ---------------------------------------------------------------------------

/// Request from an agent to obtain a credential token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CredentialRequest {
    /// Name of the credential to access.
    pub credential_name: String,
    /// PID of the requesting agent.
    pub requester_pid: Pid,
    /// Agent ID for authorization check.
    pub agent_id: String,
    /// Requested operations scope.
    pub scope: Vec<String>,
    /// Requested time-to-live.
    pub ttl_secs: u64,
}

/// Response granting (or denying) a credential request.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CredentialGrant {
    /// Token granted.
    Granted(IssuedToken),
    /// Request denied.
    Denied { reason: String },
}

// ---------------------------------------------------------------------------
// AuthService
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// HashedCredential — SHA-256 hashed agent credentials
// ---------------------------------------------------------------------------

/// A hashed credential for agent authentication.
///
/// Raw credentials are **never** stored. Only the SHA-256 hash is kept.
#[derive(Debug, Clone)]
pub struct HashedCredential {
    /// Agent identity this credential belongs to.
    pub agent_id: String,
    /// SHA-256 hash of the raw credential.
    pub hash: Vec<u8>,
    /// When the credential was created.
    pub created_at: DateTime<Utc>,
    /// Scopes this credential grants.
    pub scopes: Vec<String>,
}

/// A scoped authentication token issued after successful authentication.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthToken {
    /// Unique token identifier.
    pub token_id: String,
    /// Agent identity this token was issued to.
    pub agent_id: String,
    /// Scopes granted by this token.
    pub scopes: Vec<String>,
    /// When the token expires.
    pub expires_at: DateTime<Utc>,
    /// When the token was issued.
    pub created_at: DateTime<Utc>,
}

impl AuthToken {
    /// Check whether the token has expired.
    pub fn is_expired(&self) -> bool {
        Utc::now() > self.expires_at
    }
}

// ---------------------------------------------------------------------------
// AuthService
// ---------------------------------------------------------------------------

/// Centralized credential management service (Factotum pattern).
///
/// - Credentials are registered once and held in memory only, their values
///   sealed with XChaCha20-Poly1305 (see `secret_seal.rs`).
/// - Agents request scoped tokens; raw credentials are never exposed.
/// - Token issuance and access are audited.
/// - SHA-256 hashed credentials support agent authentication.
pub struct AuthService {
    /// Registered credentials (sealed, name-based).
    credentials: DashMap<String, StoredCredential>,
    /// SHA-256 hashed credentials (agent-id-based).
    hashed_credentials: DashMap<String, HashedCredential>,
    /// Active tokens (token-based credential access).
    active_tokens: DashMap<String, IssuedToken>,
    /// Active auth tokens (from `authenticate`).
    auth_tokens: DashMap<String, AuthToken>,
    /// Audit log.
    audit_log: std::sync::RwLock<Vec<AuditEntry>>,
    /// AEAD key for credential values (caller-supplied or per-process random).
    seal: SecretSeal,
    /// Monotonic token counter.
    token_counter: AtomicU64,
    /// ExoChain manager for event logging.
    #[cfg(feature = "exochain")]
    chain_manager: Option<Arc<ChainManager>>,
    /// Governance gate for access control.
    #[cfg(feature = "exochain")]
    governance_gate: Option<Arc<dyn GateBackend>>,
}

/// An audit log entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    /// What happened.
    pub action: String,
    /// Who did it.
    pub agent_id: String,
    /// Credential involved.
    pub credential_name: String,
    /// When it happened.
    pub timestamp: DateTime<Utc>,
    /// Whether it was allowed.
    pub allowed: bool,
}

impl AuthService {
    /// Create a new AuthService whose credential values are sealed under the
    /// given 256-bit key. The key never leaves this struct.
    pub fn new(encryption_key: [u8; 32]) -> Self {
        Self::with_seal(SecretSeal::from_key(&encryption_key))
    }

    /// Create an AuthService with a fresh random key for this process.
    /// Credentials are in memory only, so nothing needs the key later.
    pub fn new_default() -> Self {
        Self::with_seal(SecretSeal::random())
    }

    fn with_seal(seal: SecretSeal) -> Self {
        Self {
            credentials: DashMap::new(),
            hashed_credentials: DashMap::new(),
            active_tokens: DashMap::new(),
            auth_tokens: DashMap::new(),
            audit_log: std::sync::RwLock::new(Vec::new()),
            seal,
            token_counter: AtomicU64::new(0),
            #[cfg(feature = "exochain")]
            chain_manager: None,
            #[cfg(feature = "exochain")]
            governance_gate: None,
        }
    }

    /// Set the chain manager for exochain event logging.
    #[cfg(feature = "exochain")]
    pub fn with_chain_manager(mut self, cm: Arc<ChainManager>) -> Self {
        self.chain_manager = Some(cm);
        self
    }

    /// Set the governance gate for access control.
    #[cfg(feature = "exochain")]
    pub fn with_governance_gate(mut self, gate: Arc<dyn GateBackend>) -> Self {
        self.governance_gate = Some(gate);
        self
    }

    // ── Credential registration ───────────────────────────────────

    /// Register a new credential.
    pub fn register_credential(
        &self,
        name: &str,
        credential_type: CredentialType,
        value: &[u8],
        allowed_agents: Vec<String>,
    ) -> Result<(), KernelError> {
        // Governance gate: credential registration is a critical action.
        #[cfg(feature = "exochain")]
        if let Some(ref gate) = self.governance_gate {
            use crate::gate::GateDecision;
            use crate::governance::{GateEffectKind, with_effect_context};
            let ctx = with_effect_context(
                serde_json::json!({
                    "credential_name": name,
                    "credential_type": credential_type.to_string(),
                }),
                &GateEffectKind::AuthCredentialRegister.effect_vector(),
            );
            let decision = gate.check("auth-service", "auth.credential.register", &ctx);
            if let GateDecision::Deny { reason, .. } = decision {
                return Err(KernelError::GovernanceDenied(format!(
                    "credential registration denied: {reason}"
                )));
            }
        }

        if self.credentials.contains_key(name) {
            return Err(KernelError::Service(format!(
                "credential already registered: {name}"
            )));
        }

        let encrypted = self
            .seal
            .seal(name.as_bytes(), value)
            .map_err(|e| KernelError::Service(e.to_string()))?;
        #[cfg(feature = "exochain")]
        let cred_type_for_chain = credential_type.to_string();
        self.credentials.insert(
            name.to_string(),
            StoredCredential {
                name: name.to_string(),
                credential_type,
                encrypted_value: encrypted,
                allowed_agents,
                created_at: Utc::now(),
            },
        );

        #[cfg(feature = "exochain")]
        if let Some(ref cm) = self.chain_manager {
            cm.append(
                "auth",
                crate::chain::EVENT_KIND_AUTH_CREDENTIAL_REGISTER,
                Some(serde_json::json!({
                    "credential_name": name,
                    "credential_type": cred_type_for_chain,
                })),
            );
        }

        info!(name, "credential registered");
        Ok(())
    }

    /// Update an existing credential's value (rotation).
    pub fn rotate_credential(&self, name: &str, new_value: &[u8]) -> Result<(), KernelError> {
        // Governance gate: credential rotation is a critical security action.
        #[cfg(feature = "exochain")]
        if let Some(ref gate) = self.governance_gate {
            use crate::gate::GateDecision;
            use crate::governance::{GateEffectKind, with_effect_context};
            let ctx = with_effect_context(
                serde_json::json!({
                    "credential_name": name,
                }),
                &GateEffectKind::AuthCredentialRotate.effect_vector(),
            );
            let decision = gate.check("auth-service", "auth.credential.rotate", &ctx);
            if let GateDecision::Deny { reason, .. } = decision {
                return Err(KernelError::GovernanceDenied(format!(
                    "credential rotation denied: {reason}"
                )));
            }
        }

        let mut cred = self
            .credentials
            .get_mut(name)
            .ok_or_else(|| KernelError::Service(format!("credential not found: {name}")))?;
        cred.encrypted_value = self
            .seal
            .seal(name.as_bytes(), new_value)
            .map_err(|e| KernelError::Service(e.to_string()))?;

        #[cfg(feature = "exochain")]
        if let Some(ref cm) = self.chain_manager {
            cm.append(
                "auth",
                crate::chain::EVENT_KIND_AUTH_CREDENTIAL_ROTATE,
                Some(serde_json::json!({
                    "credential_name": name,
                })),
            );
        }

        info!(name, "credential rotated");
        Ok(())
    }

    // ── Token issuance ────────────────────────────────────────────

    /// Request a scoped, time-limited token.
    pub fn request_token(&self, request: &CredentialRequest) -> Result<IssuedToken, KernelError> {
        // Governance gate: token issuance is governed by policy.
        #[cfg(feature = "exochain")]
        if let Some(ref gate) = self.governance_gate {
            use crate::gate::GateDecision;
            use crate::governance::{GateEffectKind, with_effect_context};
            let ctx = with_effect_context(
                serde_json::json!({
                    "credential_name": &request.credential_name,
                    "agent_id": &request.agent_id,
                    "ttl_secs": request.ttl_secs,
                }),
                &GateEffectKind::AuthTokenIssue.effect_vector(),
            );
            let decision = gate.check("auth-service", "auth.token.issue", &ctx);
            if let GateDecision::Deny { reason, .. } = decision {
                return Err(KernelError::GovernanceDenied(format!(
                    "token issuance denied: {reason}"
                )));
            }
        }

        let cred = self
            .credentials
            .get(&request.credential_name)
            .ok_or_else(|| {
                KernelError::Service(format!("credential not found: {}", request.credential_name))
            })?;

        // Authorization check.
        if !cred.allowed_agents.is_empty() && !cred.allowed_agents.contains(&request.agent_id) {
            self.audit(
                "token.denied",
                &request.agent_id,
                &request.credential_name,
                false,
            );
            warn!(
                agent_id = %request.agent_id,
                credential = %request.credential_name,
                "token request denied"
            );
            return Err(KernelError::CapabilityDenied {
                pid: request.requester_pid,
                action: "request_token".into(),
                reason: format!(
                    "agent '{}' not authorized for credential '{}'",
                    request.agent_id, request.credential_name
                ),
            });
        }

        let ttl = Duration::from_secs(request.ttl_secs.max(1));
        let token = IssuedToken {
            token_id: uuid::Uuid::new_v4().to_string(),
            credential_name: request.credential_name.clone(),
            issued_to: request.requester_pid,
            issued_at: Utc::now(),
            expires_at: Utc::now()
                + chrono::Duration::from_std(ttl).unwrap_or(chrono::Duration::hours(1)),
            scope: request.scope.clone(),
        };

        self.active_tokens
            .insert(token.token_id.clone(), token.clone());
        self.audit(
            "token.issued",
            &request.agent_id,
            &request.credential_name,
            true,
        );

        #[cfg(feature = "exochain")]
        if let Some(ref cm) = self.chain_manager {
            cm.append(
                "auth",
                crate::chain::EVENT_KIND_AUTH_TOKEN_ISSUE,
                Some(serde_json::json!({
                    "token_id": token.token_id,
                    "credential_name": request.credential_name,
                    "agent_id": request.agent_id,
                    "ttl_secs": request.ttl_secs,
                })),
            );
        }

        info!(
            token_id = %token.token_id,
            credential = %request.credential_name,
            agent = %request.agent_id,
            ttl_secs = request.ttl_secs,
            "token issued"
        );
        Ok(token)
    }

    /// Validate an issued token. Returns `Err` if expired or not found.
    pub fn validate_token(&self, token_id: &str) -> Result<IssuedToken, KernelError> {
        let token = self
            .active_tokens
            .get(token_id)
            .ok_or_else(|| KernelError::Service("token not found".into()))?;

        if token.is_expired() {
            return Err(KernelError::Service("token expired".into()));
        }

        Ok(token.clone())
    }

    /// Revoke an active token.
    ///
    /// Returns `Ok(true)` if a token was revoked, `Ok(false)` if the
    /// token was not present (idempotent re-revoke), and
    /// `Err(KernelError::GovernanceDenied)` if the configured
    /// governance gate denies the `auth.token.revoke` action.
    ///
    /// WEFT-98: revocation is a privileged operation -- it is the
    /// only mechanism for invalidating an issued credential before
    /// its TTL elapses. Without a governance gate, a compromised
    /// caller could mass-revoke tokens to deny service. The gate
    /// runs *before* state mutation so a denied revoke leaves the
    /// active-token map untouched.
    pub fn revoke_token(&self, token_id: &str) -> Result<bool, KernelError> {
        // Governance gate: token revocation is governed by policy
        // (mirrors the rotate_credential / request_token gates).
        #[cfg(feature = "exochain")]
        if let Some(ref gate) = self.governance_gate {
            use crate::gate::GateDecision;
            use crate::governance::{GateEffectKind, with_effect_context};
            let ctx = with_effect_context(
                serde_json::json!({
                    "token_id": token_id,
                }),
                &GateEffectKind::AuthTokenRevoke.effect_vector(),
            );
            let decision = gate.check("auth-service", "auth.token.revoke", &ctx);
            if let GateDecision::Deny { reason, .. } = decision {
                return Err(KernelError::GovernanceDenied(format!(
                    "token revocation denied: {reason}"
                )));
            }
        }

        let removed = self.active_tokens.remove(token_id).is_some();

        #[cfg(feature = "exochain")]
        if removed && let Some(ref cm) = self.chain_manager {
            cm.append(
                "auth",
                crate::chain::EVENT_KIND_AUTH_TOKEN_REVOKE,
                Some(serde_json::json!({
                    "token_id": token_id,
                })),
            );
        }

        Ok(removed)
    }

    /// List all active (non-expired) tokens.
    pub fn active_token_count(&self) -> usize {
        self.active_tokens
            .iter()
            .filter(|t| !t.value().is_expired())
            .count()
    }

    // ── SHA-256 hashed credential operations ─────────────────────

    /// Compute the SHA-256 hash of a raw credential.
    fn sha256_hash(data: &[u8]) -> Vec<u8> {
        let mut hasher = Sha256::new();
        hasher.update(data);
        hasher.finalize().to_vec()
    }

    /// Register a hashed credential for an agent.
    ///
    /// The raw credential is **never stored**; only its SHA-256 hash is kept.
    pub fn register_hashed_credential(
        &self,
        agent_id: &str,
        raw_credential: &[u8],
        scopes: Vec<String>,
    ) -> Result<(), KernelError> {
        // Governance gate: hashed credential registration is a critical action.
        #[cfg(feature = "exochain")]
        if let Some(ref gate) = self.governance_gate {
            use crate::gate::GateDecision;
            use crate::governance::{GateEffectKind, with_effect_context};
            let ctx = with_effect_context(
                serde_json::json!({
                    "agent_id": agent_id,
                }),
                &GateEffectKind::AuthCredentialRegister.effect_vector(),
            );
            let decision = gate.check("auth-service", "auth.credential.register", &ctx);
            if let GateDecision::Deny { reason, .. } = decision {
                return Err(KernelError::GovernanceDenied(format!(
                    "hashed credential registration denied: {reason}"
                )));
            }
        }

        if self.hashed_credentials.contains_key(agent_id) {
            return Err(KernelError::Service(format!(
                "hashed credential already registered for agent: {agent_id}"
            )));
        }

        let hash = Self::sha256_hash(raw_credential);
        self.hashed_credentials.insert(
            agent_id.to_string(),
            HashedCredential {
                agent_id: agent_id.to_string(),
                hash,
                created_at: Utc::now(),
                scopes,
            },
        );

        #[cfg(feature = "exochain")]
        if let Some(ref cm) = self.chain_manager {
            cm.append(
                "auth",
                crate::chain::EVENT_KIND_AUTH_CREDENTIAL_REGISTER,
                Some(serde_json::json!({
                    "agent_id": agent_id,
                    "type": "hashed",
                })),
            );
        }

        info!(agent_id, "hashed credential registered");
        Ok(())
    }

    /// Authenticate an agent by verifying its raw credential against the stored hash.
    ///
    /// On success, issues a scoped [`AuthToken`] valid for one hour.
    pub fn authenticate(
        &self,
        agent_id: &str,
        raw_credential: &[u8],
    ) -> Result<AuthToken, KernelError> {
        let cred = self
            .hashed_credentials
            .get(agent_id)
            .ok_or_else(|| KernelError::Service(format!("no credential for agent: {agent_id}")))?;

        let provided_hash = Self::sha256_hash(raw_credential);
        if cred.hash != provided_hash {
            self.audit("authenticate.failed", agent_id, agent_id, false);

            #[cfg(feature = "exochain")]
            if let Some(ref cm) = self.chain_manager {
                cm.append(
                    "auth",
                    crate::chain::EVENT_KIND_AUTH_ATTEMPT,
                    Some(serde_json::json!({
                        "agent_id": agent_id,
                        "success": false,
                    })),
                );
            }

            warn!(agent_id, "authentication failed — hash mismatch");
            return Err(KernelError::Service("authentication failed".into()));
        }

        let seq = self.token_counter.fetch_add(1, Ordering::Relaxed);
        let token = AuthToken {
            token_id: format!("auth-{}-{seq}", uuid::Uuid::new_v4()),
            agent_id: agent_id.to_string(),
            scopes: cred.scopes.clone(),
            expires_at: Utc::now() + chrono::Duration::hours(1),
            created_at: Utc::now(),
        };

        self.auth_tokens
            .insert(token.token_id.clone(), token.clone());
        self.audit("authenticate.success", agent_id, agent_id, true);

        #[cfg(feature = "exochain")]
        if let Some(ref cm) = self.chain_manager {
            cm.append(
                "auth",
                crate::chain::EVENT_KIND_AUTH_ATTEMPT,
                Some(serde_json::json!({
                    "agent_id": agent_id,
                    "success": true,
                    "token_id": token.token_id,
                })),
            );
        }

        info!(agent_id, token_id = %token.token_id, "agent authenticated");
        Ok(token)
    }

    /// Validate an auth token. Returns `Err` if expired or not found.
    pub fn validate_auth_token(&self, token_id: &str) -> Result<AuthToken, KernelError> {
        let token = self
            .auth_tokens
            .get(token_id)
            .ok_or_else(|| KernelError::Service("auth token not found".into()))?;

        if token.is_expired() {
            return Err(KernelError::Service("auth token expired".into()));
        }

        Ok(token.clone())
    }

    /// Revoke an auth token. Returns `true` if it existed.
    pub fn revoke_auth_token(&self, token_id: &str) -> bool {
        self.auth_tokens.remove(token_id).is_some()
    }

    /// Check whether an auth token has a specific scope.
    pub fn check_scope(&self, token_id: &str, required_scope: &str) -> bool {
        self.auth_tokens
            .get(token_id)
            .map(|t| !t.is_expired() && t.scopes.contains(&required_scope.to_string()))
            .unwrap_or(false)
    }

    // ── Audit ─────────────────────────────────────────────────────

    fn audit(&self, action: &str, agent_id: &str, credential_name: &str, allowed: bool) {
        if let Ok(mut log) = self.audit_log.write() {
            log.push(AuditEntry {
                action: action.to_string(),
                agent_id: agent_id.to_string(),
                credential_name: credential_name.to_string(),
                timestamp: Utc::now(),
                allowed,
            });
        }
    }

    /// Get the audit log.
    pub fn audit_log(&self) -> Vec<AuditEntry> {
        self.audit_log.read().map(|l| l.clone()).unwrap_or_default()
    }

    // ── Sealing ───────────────────────────────────────────────────

    /// Open a stored credential value. Nothing outside tests reads raw
    /// values (agents get tokens, not credentials), so this is test-only.
    #[cfg(test)]
    fn open_credential(&self, name: &str) -> Result<Vec<u8>, KernelError> {
        let cred = self
            .credentials
            .get(name)
            .ok_or_else(|| KernelError::Service(format!("credential not found: {name}")))?;
        self.seal
            .open(name.as_bytes(), &cred.encrypted_value)
            .map_err(|e| KernelError::Service(e.to_string()))
    }
}

#[async_trait]
impl SystemService for AuthService {
    fn name(&self) -> &str {
        "auth-service"
    }

    fn service_type(&self) -> ServiceType {
        ServiceType::Core
    }

    async fn start(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        info!("auth service started");
        Ok(())
    }

    async fn stop(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        info!(
            credentials = self.credentials.len(),
            active_tokens = self.active_token_count(),
            "auth service stopped"
        );
        Ok(())
    }

    async fn health_check(&self) -> HealthStatus {
        HealthStatus::Healthy
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn pid(n: u64) -> Pid {
        n
    }

    fn make_request(cred: &str, agent: &str, pid_val: u64) -> CredentialRequest {
        CredentialRequest {
            credential_name: cred.to_string(),
            requester_pid: pid(pid_val),
            agent_id: agent.to_string(),
            scope: vec!["read".to_string()],
            ttl_secs: 3600,
        }
    }

    #[test]
    fn register_and_request_token() {
        let svc = AuthService::new_default();
        svc.register_credential(
            "github-key",
            CredentialType::ApiKey,
            b"ghp_secret_value",
            vec!["deploy-agent".to_string()],
        )
        .unwrap();

        let req = make_request("github-key", "deploy-agent", 1);
        let token = svc.request_token(&req).unwrap();
        assert!(!token.token_id.is_empty());
        assert_eq!(token.credential_name, "github-key");
    }

    #[test]
    fn unauthorized_agent_denied() {
        let svc = AuthService::new_default();
        svc.register_credential(
            "secret",
            CredentialType::BearerToken,
            b"token",
            vec!["allowed-agent".to_string()],
        )
        .unwrap();

        let req = make_request("secret", "other-agent", 2);
        let result = svc.request_token(&req);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("denied") || err.contains("authorized"),
            "got: {err}"
        );
    }

    #[test]
    fn empty_allowed_agents_permits_all() {
        let svc = AuthService::new_default();
        svc.register_credential(
            "open-cred",
            CredentialType::ApiKey,
            b"value",
            vec![], // any agent
        )
        .unwrap();

        let req = make_request("open-cred", "random-agent", 5);
        let token = svc.request_token(&req).unwrap();
        assert!(!token.is_expired());
    }

    #[test]
    fn validate_token() {
        let svc = AuthService::new_default();
        svc.register_credential("cred", CredentialType::ApiKey, b"val", vec![])
            .unwrap();
        let req = make_request("cred", "agent", 1);
        let token = svc.request_token(&req).unwrap();
        let validated = svc.validate_token(&token.token_id).unwrap();
        assert_eq!(validated.token_id, token.token_id);
    }

    #[test]
    fn validate_nonexistent_token_fails() {
        let svc = AuthService::new_default();
        assert!(svc.validate_token("no-such-token").is_err());
    }

    #[test]
    fn revoke_token() {
        let svc = AuthService::new_default();
        svc.register_credential("cred", CredentialType::ApiKey, b"val", vec![])
            .unwrap();
        let req = make_request("cred", "agent", 1);
        let token = svc.request_token(&req).unwrap();
        assert!(svc.revoke_token(&token.token_id).unwrap());
        assert!(svc.validate_token(&token.token_id).is_err());
        // Revoking an unknown / already-revoked token is idempotent.
        assert!(!svc.revoke_token(&token.token_id).unwrap());
    }

    /// WEFT-98: revoke_token must consult the governance gate and
    /// surface a `GovernanceDenied` error when the gate denies the
    /// `auth.token.revoke` action.
    ///
    /// Uses a selective gate that only denies `auth.token.revoke`
    /// so register/issue still succeed during setup.
    #[cfg(feature = "exochain")]
    #[test]
    fn revoke_token_denied_by_governance() {
        use crate::gate::{GateBackend, GateDecision};
        use std::sync::Arc;

        struct RevokeDenyGate;
        impl GateBackend for RevokeDenyGate {
            fn check(&self, _source: &str, action: &str, _ctx: &serde_json::Value) -> GateDecision {
                if action == "auth.token.revoke" {
                    GateDecision::Deny {
                        reason: "policy: revoke disabled".into(),
                        receipt: None,
                    }
                } else {
                    GateDecision::Permit { token: None }
                }
            }
        }

        let svc = AuthService::new_default()
            .with_governance_gate(Arc::new(RevokeDenyGate) as Arc<dyn GateBackend>);
        svc.register_credential("cred", CredentialType::ApiKey, b"val", vec![])
            .unwrap();
        let req = make_request("cred", "agent", 1);
        let token = svc.request_token(&req).unwrap();

        let err = svc.revoke_token(&token.token_id).unwrap_err();
        match err {
            KernelError::GovernanceDenied(msg) => {
                assert!(msg.contains("token revocation denied"), "got: {msg}");
            }
            other => panic!("expected GovernanceDenied, got {other:?}"),
        }

        // Token must still be valid -- a denied revoke is a no-op
        // on the active-token map.
        assert!(svc.validate_token(&token.token_id).is_ok());
    }

    /// WEFT-544: canonical "may rotate but not revoke" policy —
    /// separate gate actions `auth.credential.rotate` (allow) vs
    /// `auth.token.revoke` (deny). Existing DSL is sufficient; no new
    /// construct. See docs/plans/decisions/0.8-decision-batch-open-questions.md.
    #[cfg(feature = "exochain")]
    #[test]
    fn rotate_allowed_revoke_denied_policy() {
        use crate::gate::{GateBackend, GateDecision};
        use std::sync::Arc;

        struct RotateAllowRevokeDeny;
        impl GateBackend for RotateAllowRevokeDeny {
            fn check(&self, _source: &str, action: &str, _ctx: &serde_json::Value) -> GateDecision {
                if action == "auth.token.revoke" {
                    GateDecision::Deny {
                        reason: "policy: agent may rotate but not revoke".into(),
                        receipt: None,
                    }
                } else {
                    // auth.credential.rotate, auth.token.issue, register, …
                    GateDecision::Permit { token: None }
                }
            }
        }

        let svc = AuthService::new_default()
            .with_governance_gate(Arc::new(RotateAllowRevokeDeny) as Arc<dyn GateBackend>);
        svc.register_credential("cred", CredentialType::ApiKey, b"old", vec![])
            .unwrap();
        let req = make_request("cred", "agent", 1);
        let token = svc.request_token(&req).unwrap();

        // Rotate succeeds under this policy.
        svc.rotate_credential("cred", b"new").unwrap();

        // Revoke is denied; token remains valid.
        let err = svc.revoke_token(&token.token_id).unwrap_err();
        match err {
            KernelError::GovernanceDenied(msg) => {
                assert!(msg.contains("token revocation denied"), "got: {msg}");
            }
            other => panic!("expected GovernanceDenied, got {other:?}"),
        }
        assert!(svc.validate_token(&token.token_id).is_ok());
    }

    #[test]
    fn credential_rotation_preserves_tokens() {
        let svc = AuthService::new_default();
        svc.register_credential("rotate-cred", CredentialType::ApiKey, b"old_val", vec![])
            .unwrap();
        let req = make_request("rotate-cred", "agent", 1);
        let token = svc.request_token(&req).unwrap();

        // Rotate credential.
        svc.rotate_credential("rotate-cred", b"new_val").unwrap();

        // Existing token still valid.
        let validated = svc.validate_token(&token.token_id).unwrap();
        assert_eq!(validated.credential_name, "rotate-cred");
    }

    #[test]
    fn raw_credential_never_exposed() {
        let key = [0xAB; 32];
        let svc = AuthService::new(key);
        svc.register_credential("secret", CredentialType::ApiKey, b"raw_secret", vec![])
            .unwrap();

        // The stored value should be encrypted, not raw.
        let cred = svc.credentials.get("secret").unwrap();
        assert_ne!(cred.encrypted_value, b"raw_secret");
    }

    #[test]
    fn audit_log_records_events() {
        let svc = AuthService::new_default();
        svc.register_credential(
            "audited",
            CredentialType::ApiKey,
            b"val",
            vec!["agent-a".to_string()],
        )
        .unwrap();

        // Successful request.
        let req = make_request("audited", "agent-a", 1);
        svc.request_token(&req).unwrap();

        // Failed request.
        let bad_req = make_request("audited", "agent-b", 2);
        let _ = svc.request_token(&bad_req);

        let log = svc.audit_log();
        assert_eq!(log.len(), 2);
        assert!(log[0].allowed);
        assert!(!log[1].allowed);
    }

    #[test]
    fn duplicate_credential_registration_fails() {
        let svc = AuthService::new_default();
        svc.register_credential("dup", CredentialType::ApiKey, b"val", vec![])
            .unwrap();
        let result = svc.register_credential("dup", CredentialType::ApiKey, b"val2", vec![]);
        assert!(result.is_err());
    }

    #[test]
    fn request_nonexistent_credential_fails() {
        let svc = AuthService::new_default();
        let req = make_request("missing", "agent", 1);
        assert!(svc.request_token(&req).is_err());
    }

    #[tokio::test]
    async fn system_service_impl() {
        let svc = AuthService::new_default();
        assert_eq!(svc.name(), "auth-service");
        assert_eq!(svc.service_type(), ServiceType::Core);
        svc.start().await.unwrap();
        assert_eq!(svc.health_check().await, HealthStatus::Healthy);
        svc.stop().await.unwrap();
    }

    // ── SHA-256 hashed credential tests ──────────────────────────

    #[test]
    fn register_and_authenticate_success() {
        let svc = AuthService::new_default();
        svc.register_hashed_credential("agent-1", b"my_password", vec!["read".into()])
            .unwrap();
        let token = svc.authenticate("agent-1", b"my_password").unwrap();
        assert_eq!(token.agent_id, "agent-1");
        assert!(!token.token_id.is_empty());
        assert_eq!(token.scopes, vec!["read".to_string()]);
    }

    #[test]
    fn authenticate_wrong_credential_fails() {
        let svc = AuthService::new_default();
        svc.register_hashed_credential("agent-2", b"correct", vec![])
            .unwrap();
        let result = svc.authenticate("agent-2", b"wrong");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("failed"), "got: {err}");
    }

    #[test]
    fn authenticate_issues_token_with_scopes() {
        let svc = AuthService::new_default();
        svc.register_hashed_credential(
            "scoped-agent",
            b"secret",
            vec!["read".into(), "write".into()],
        )
        .unwrap();
        let token = svc.authenticate("scoped-agent", b"secret").unwrap();
        assert_eq!(token.scopes, vec!["read".to_string(), "write".to_string()]);
    }

    #[test]
    fn validate_auth_token_succeeds() {
        let svc = AuthService::new_default();
        svc.register_hashed_credential("v-agent", b"pass", vec![])
            .unwrap();
        let token = svc.authenticate("v-agent", b"pass").unwrap();
        let validated = svc.validate_auth_token(&token.token_id).unwrap();
        assert_eq!(validated.agent_id, "v-agent");
    }

    #[test]
    fn validate_expired_auth_token_fails() {
        let svc = AuthService::new_default();
        svc.register_hashed_credential("exp-agent", b"pass", vec![])
            .unwrap();
        let token = svc.authenticate("exp-agent", b"pass").unwrap();

        // Manually insert an expired token.
        let expired = AuthToken {
            token_id: "expired-tok".to_string(),
            agent_id: "exp-agent".to_string(),
            scopes: vec![],
            expires_at: Utc::now() - chrono::Duration::hours(1),
            created_at: Utc::now() - chrono::Duration::hours(2),
        };
        svc.auth_tokens.insert("expired-tok".into(), expired);

        // The real token should be valid.
        assert!(svc.validate_auth_token(&token.token_id).is_ok());
        // The expired token should fail.
        let result = svc.validate_auth_token("expired-tok");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("expired"));
    }

    #[test]
    fn revoke_auth_token_works() {
        let svc = AuthService::new_default();
        svc.register_hashed_credential("rev-agent", b"pass", vec![])
            .unwrap();
        let token = svc.authenticate("rev-agent", b"pass").unwrap();
        assert!(svc.revoke_auth_token(&token.token_id));
        assert!(svc.validate_auth_token(&token.token_id).is_err());
        // Second revoke returns false.
        assert!(!svc.revoke_auth_token(&token.token_id));
    }

    #[test]
    fn check_scope_with_matching_scope() {
        let svc = AuthService::new_default();
        svc.register_hashed_credential("sc-agent", b"cred", vec!["admin".into(), "read".into()])
            .unwrap();
        let token = svc.authenticate("sc-agent", b"cred").unwrap();
        assert!(svc.check_scope(&token.token_id, "admin"));
        assert!(svc.check_scope(&token.token_id, "read"));
    }

    #[test]
    fn check_scope_with_missing_scope_fails() {
        let svc = AuthService::new_default();
        svc.register_hashed_credential("sc-agent2", b"cred", vec!["read".into()])
            .unwrap();
        let token = svc.authenticate("sc-agent2", b"cred").unwrap();
        assert!(!svc.check_scope(&token.token_id, "write"));
        assert!(!svc.check_scope(&token.token_id, "admin"));
    }

    #[test]
    fn raw_credentials_never_stored_in_hash() {
        let svc = AuthService::new_default();
        let raw = b"super_secret_password";
        svc.register_hashed_credential("hash-check", raw, vec![])
            .unwrap();

        let cred = svc.hashed_credentials.get("hash-check").unwrap();
        // The hash must not equal the raw input.
        assert_ne!(cred.hash.as_slice(), raw.as_slice());
        // The hash should be 32 bytes (SHA-256).
        assert_eq!(cred.hash.len(), 32);
    }

    #[test]
    fn check_scope_on_nonexistent_token_returns_false() {
        let svc = AuthService::new_default();
        assert!(!svc.check_scope("no-such-token", "read"));
    }
    // ── card a134bcf7: credentials are sealed with an AEAD, not keyed XOR ──

    #[test]
    fn same_credential_value_seals_differently() {
        let svc = AuthService::new([0xAB; 32]);
        svc.register_credential("a", CredentialType::ApiKey, b"same-value", vec![])
            .unwrap();
        svc.register_credential("b", CredentialType::ApiKey, b"same-value", vec![])
            .unwrap();
        let a = svc.credentials.get("a").unwrap().encrypted_value.clone();
        let b = svc.credentials.get("b").unwrap().encrypted_value.clone();
        assert_ne!(a, b, "a fresh nonce per seal must change the ciphertext");
        svc.rotate_credential("a", b"same-value").unwrap();
        assert_ne!(svc.credentials.get("a").unwrap().encrypted_value, a);
    }

    #[test]
    fn credential_round_trips_and_rotates() {
        let svc = AuthService::new([0xAB; 32]);
        svc.register_credential("c", CredentialType::ApiKey, b"first", vec![])
            .unwrap();
        assert_eq!(svc.open_credential("c").unwrap(), b"first");
        svc.rotate_credential("c", b"second").unwrap();
        assert_eq!(svc.open_credential("c").unwrap(), b"second");
    }

    #[test]
    fn tampered_credential_fails_to_open() {
        let svc = AuthService::new([0xAB; 32]);
        svc.register_credential("c", CredentialType::ApiKey, b"tamper-me", vec![])
            .unwrap();
        let original = svc.credentials.get("c").unwrap().encrypted_value.clone();
        for i in 0..original.len() {
            let mut t = original.clone();
            t[i] ^= 0x01;
            svc.credentials.get_mut("c").unwrap().encrypted_value = t;
            assert!(svc.open_credential("c").is_err(), "byte {i}");
        }
    }

    #[test]
    fn credential_under_other_key_or_name_does_not_open() {
        let a = AuthService::new([0x11; 32]);
        let b = AuthService::new([0x22; 32]);
        a.register_credential("c", CredentialType::ApiKey, b"cross", vec![])
            .unwrap();
        a.register_credential("d", CredentialType::ApiKey, b"other", vec![])
            .unwrap();
        b.register_credential("c", CredentialType::ApiKey, b"placeholder", vec![])
            .unwrap();
        let sealed = a.credentials.get("c").unwrap().encrypted_value.clone();
        b.credentials.get_mut("c").unwrap().encrypted_value = sealed.clone();
        assert!(b.open_credential("c").is_err(), "wrong key must fail");
        a.credentials.get_mut("d").unwrap().encrypted_value = sealed;
        assert!(a.open_credential("d").is_err(), "wrong slot must fail");
    }

    #[test]
    fn default_auth_services_use_distinct_random_keys() {
        let a = AuthService::new_default();
        let b = AuthService::new_default();
        a.register_credential("c", CredentialType::ApiKey, b"v", vec![])
            .unwrap();
        b.register_credential("c", CredentialType::ApiKey, b"w", vec![])
            .unwrap();
        let sealed = a.credentials.get("c").unwrap().encrypted_value.clone();
        b.credentials.get_mut("c").unwrap().encrypted_value = sealed;
        assert!(b.open_credential("c").is_err());
    }

    #[test]
    fn no_keyed_xor_left_in_auth_service() {
        let src = include_str!("auth_service.rs");
        assert!(!src.contains(concat!("xor", "_encrypt")));
        assert!(!src.contains(concat!("xor", "_decrypt")));
    }
}
