//! Configuration and secrets service (K5-G1).
//!
//! Provides [`ConfigService`] for runtime configuration management with
//! change notification, typed values, and AEAD-sealed secret storage. Backed
//! by in-memory stores only (tree integration deferred to when `exochain`
//! feature is enabled); nothing here is persisted. Secrets are sealed with
//! XChaCha20-Poly1305, see `secret_seal.rs` for the threat model.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tracing::info;

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
// ConfigValue — typed configuration values
// ---------------------------------------------------------------------------

/// A typed configuration value.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ConfigValue {
    /// Plain text.
    Text(String),
    /// 64-bit signed integer.
    Integer(i64),
    /// 64-bit floating point.
    Float(f64),
    /// Boolean flag.
    Boolean(bool),
    /// Arbitrary JSON blob.
    Json(serde_json::Value),
}

impl ConfigValue {
    /// Convert to a `serde_json::Value` representation.
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            ConfigValue::Text(s) => serde_json::Value::String(s.clone()),
            ConfigValue::Integer(n) => serde_json::json!(n),
            ConfigValue::Float(f) => serde_json::json!(f),
            ConfigValue::Boolean(b) => serde_json::json!(b),
            ConfigValue::Json(v) => v.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// ConfigEntry
// ---------------------------------------------------------------------------

/// A stored configuration entry with metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigEntry {
    /// Configuration key.
    pub key: String,
    /// Configuration namespace.
    pub namespace: String,
    /// Typed value.
    pub value: ConfigValue,
    /// When the entry was last updated.
    pub updated_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// ConfigChange
// ---------------------------------------------------------------------------

/// A change notification for a configuration key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigChange {
    /// Configuration namespace.
    pub namespace: String,
    /// Configuration key.
    pub key: String,
    /// Previous value (if any).
    pub old_value: Option<serde_json::Value>,
    /// New value (if any -- `None` for deletions).
    pub new_value: Option<serde_json::Value>,
    /// PID of the process that made the change.
    pub changed_by: Pid,
    /// When the change occurred.
    pub timestamp: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// SecretRef
// ---------------------------------------------------------------------------

/// Metadata about a stored secret.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretRef {
    /// Secret namespace.
    pub namespace: String,
    /// Secret key.
    pub key: String,
    /// When the secret expires.
    pub expires_at: DateTime<Utc>,
    /// PIDs allowed to read this secret.
    pub scoped_to: Vec<Pid>,
}

// ---------------------------------------------------------------------------
// ConfigService
// ---------------------------------------------------------------------------

/// Configuration and secrets service.
///
/// Stores configuration values at `/kernel/config/{namespace}/{key}` and
/// secrets at `/kernel/secrets/{namespace}/{key}`, all in memory. Secret
/// values are sealed with XChaCha20-Poly1305 and bound to their slot.
/// Supports change notification via subscriptions.
pub struct ConfigService {
    /// Config store: "namespace/key" -> value.
    configs: DashMap<String, serde_json::Value>,
    /// Typed config entries: "namespace/key" -> ConfigEntry.
    entries: DashMap<String, ConfigEntry>,
    /// Secret store: "namespace/key" -> sealed bytes (nonce || ciphertext || tag).
    secrets: DashMap<String, Vec<u8>>,
    /// Secret metadata: "namespace/key" -> SecretRef.
    secret_refs: DashMap<String, SecretRef>,
    /// Change subscribers: namespace -> list of subscription queues.
    subscribers: DashMap<String, Vec<Arc<RwLock<Vec<ConfigChange>>>>>,
    /// AEAD key for secret values (caller-supplied or per-process random).
    seal: SecretSeal,
    /// Change log for auditing.
    change_log: RwLock<Vec<ConfigChange>>,
    /// Total config sets.
    set_count: AtomicU64,
    /// ExoChain manager for event logging.
    #[cfg(feature = "exochain")]
    chain_manager: Option<Arc<ChainManager>>,
    /// Governance gate for access control.
    #[cfg(feature = "exochain")]
    governance_gate: Option<Arc<dyn GateBackend>>,
}

impl ConfigService {
    /// Create a new config service whose secrets are sealed under the given
    /// 256-bit key. The key never leaves this struct.
    pub fn new(encryption_key: [u8; 32]) -> Self {
        Self::with_seal(SecretSeal::from_key(&encryption_key))
    }

    /// Create a config service with a fresh random secret key for this
    /// process. Secrets are in memory only, so nothing needs the key later.
    pub fn new_default() -> Self {
        Self::with_seal(SecretSeal::random())
    }

    fn with_seal(seal: SecretSeal) -> Self {
        Self {
            configs: DashMap::new(),
            entries: DashMap::new(),
            secrets: DashMap::new(),
            secret_refs: DashMap::new(),
            subscribers: DashMap::new(),
            seal,
            change_log: RwLock::new(Vec::new()),
            set_count: AtomicU64::new(0),
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

    // ── Config operations ─────────────────────────────────────────

    /// Set a configuration value.
    pub fn set(
        &self,
        namespace: &str,
        key: &str,
        value: serde_json::Value,
        changed_by: Pid,
    ) -> Result<(), KernelError> {
        // Governance gate: config changes are gated.
        #[cfg(feature = "exochain")]
        if let Some(ref gate) = self.governance_gate {
            use crate::gate::GateDecision;
            use crate::governance::{GateEffectKind, with_effect_context};
            let ctx = with_effect_context(
                serde_json::json!({
                    "namespace": namespace,
                    "key": key,
                    "changed_by": changed_by,
                }),
                &GateEffectKind::ConfigSet.effect_vector(),
            );
            let decision = gate.check("config-service", "config.set", &ctx);
            if let GateDecision::Deny { reason, .. } = decision {
                return Err(KernelError::GovernanceDenied(format!(
                    "config set denied: {reason}"
                )));
            }
        }

        let config_key = format!("{namespace}/{key}");
        let old_value = self.configs.get(&config_key).map(|v| v.value().clone());
        self.configs.insert(config_key, value.clone());

        let change = ConfigChange {
            namespace: namespace.to_string(),
            key: key.to_string(),
            old_value,
            new_value: Some(value),
            changed_by,
            timestamp: Utc::now(),
        };

        // Notify subscribers.
        self.notify_subscribers(namespace, &change);

        // Record in change log.
        if let Ok(mut log) = self.change_log.write() {
            log.push(change);
        }
        self.set_count.fetch_add(1, Ordering::Relaxed);

        #[cfg(feature = "exochain")]
        if let Some(ref cm) = self.chain_manager {
            cm.append(
                "config",
                crate::chain::EVENT_KIND_CONFIG_SET,
                Some(serde_json::json!({
                    "namespace": namespace,
                    "key": key,
                    "changed_by": changed_by,
                })),
            );
        }

        Ok(())
    }

    /// Get a configuration value.
    pub fn get(&self, namespace: &str, key: &str) -> Option<serde_json::Value> {
        let config_key = format!("{namespace}/{key}");
        self.configs.get(&config_key).map(|v| v.value().clone())
    }

    /// Delete a configuration value.
    pub fn delete(&self, namespace: &str, key: &str, changed_by: Pid) -> Result<(), KernelError> {
        // Governance gate: config deletion is gated.
        #[cfg(feature = "exochain")]
        if let Some(ref gate) = self.governance_gate {
            use crate::gate::GateDecision;
            use crate::governance::{GateEffectKind, with_effect_context};
            let ctx = with_effect_context(
                serde_json::json!({
                    "namespace": namespace,
                    "key": key,
                    "changed_by": changed_by,
                }),
                &GateEffectKind::ConfigDelete.effect_vector(),
            );
            let decision = gate.check("config-service", "config.delete", &ctx);
            if let GateDecision::Deny { reason, .. } = decision {
                return Err(KernelError::GovernanceDenied(format!(
                    "config delete denied: {reason}"
                )));
            }
        }

        let config_key = format!("{namespace}/{key}");
        let old_value = self.configs.remove(&config_key).map(|(_, v)| v);

        let change = ConfigChange {
            namespace: namespace.to_string(),
            key: key.to_string(),
            old_value,
            new_value: None,
            changed_by,
            timestamp: Utc::now(),
        };
        self.notify_subscribers(namespace, &change);

        #[cfg(feature = "exochain")]
        if let Some(ref cm) = self.chain_manager {
            cm.append(
                "config",
                crate::chain::EVENT_KIND_CONFIG_DELETE,
                Some(serde_json::json!({
                    "namespace": namespace,
                    "key": key,
                    "changed_by": changed_by,
                })),
            );
        }

        Ok(())
    }

    /// List all config keys in a namespace.
    pub fn list_keys(&self, namespace: &str) -> Vec<String> {
        let prefix = format!("{namespace}/");
        self.configs
            .iter()
            .filter(|e| e.key().starts_with(&prefix))
            .map(|e| e.key()[prefix.len()..].to_string())
            .collect()
    }

    // ── Typed config operations ──────────────────────────────────

    /// Store a typed configuration value.
    pub fn set_typed(
        &self,
        namespace: &str,
        key: &str,
        value: ConfigValue,
        changed_by: Pid,
    ) -> Result<(), KernelError> {
        let config_key = format!("{namespace}/{key}");
        let json_value = value.to_json();

        // Also store in the legacy JSON map for backward compatibility.
        let old_value = self.configs.get(&config_key).map(|v| v.value().clone());
        self.configs.insert(config_key.clone(), json_value.clone());

        let entry = ConfigEntry {
            key: key.to_string(),
            namespace: namespace.to_string(),
            value,
            updated_at: Utc::now(),
        };
        self.entries.insert(config_key, entry);

        let change = ConfigChange {
            namespace: namespace.to_string(),
            key: key.to_string(),
            old_value,
            new_value: Some(json_value),
            changed_by,
            timestamp: Utc::now(),
        };
        self.notify_subscribers(namespace, &change);

        if let Ok(mut log) = self.change_log.write() {
            log.push(change);
        }
        self.set_count.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Retrieve a typed configuration entry.
    pub fn get_typed(&self, namespace: &str, key: &str) -> Option<ConfigEntry> {
        let config_key = format!("{namespace}/{key}");
        self.entries.get(&config_key).map(|e| e.value().clone())
    }

    /// List all typed entries in a namespace.
    pub fn list(&self, namespace: &str) -> Vec<ConfigEntry> {
        let prefix = format!("{namespace}/");
        self.entries
            .iter()
            .filter(|e| e.key().starts_with(&prefix))
            .map(|e| e.value().clone())
            .collect()
    }

    /// Delete a typed configuration entry. Returns `true` if it existed.
    ///
    /// # Errors
    ///
    /// Returns `KernelError::GovernanceDenied` if the governance gate
    /// rejects the deletion (only when the `exochain` feature is enabled
    /// and a gate is attached).
    pub fn delete_typed(
        &self,
        namespace: &str,
        key: &str,
        changed_by: Pid,
    ) -> Result<bool, KernelError> {
        // Governance gate: typed config deletion is gated.
        #[cfg(feature = "exochain")]
        if let Some(ref gate) = self.governance_gate {
            use crate::gate::GateDecision;
            use crate::governance::{GateEffectKind, with_effect_context};
            let ctx = with_effect_context(
                serde_json::json!({
                    "namespace": namespace,
                    "key": key,
                    "changed_by": changed_by,
                }),
                &GateEffectKind::ConfigDelete.effect_vector(),
            );
            let decision = gate.check("config-service", "config.delete_typed", &ctx);
            if let GateDecision::Deny { reason, .. } = decision {
                return Err(KernelError::GovernanceDenied(format!(
                    "config delete_typed denied: {reason}"
                )));
            }
        }

        let config_key = format!("{namespace}/{key}");
        let removed = self.entries.remove(&config_key).is_some();
        // Also remove from legacy store.
        let old_value = self.configs.remove(&config_key).map(|(_, v)| v);

        let change = ConfigChange {
            namespace: namespace.to_string(),
            key: key.to_string(),
            old_value,
            new_value: None,
            changed_by,
            timestamp: Utc::now(),
        };
        self.notify_subscribers(namespace, &change);
        Ok(removed)
    }

    // ── Subscription ──────────────────────────────────────────────

    /// Subscribe to changes in a namespace.
    ///
    /// Returns a shared reference to the change queue. Callers can read
    /// accumulated changes from the returned `Arc<RwLock<Vec<ConfigChange>>>`.
    pub fn subscribe(&self, namespace: &str) -> Arc<RwLock<Vec<ConfigChange>>> {
        let queue = Arc::new(RwLock::new(Vec::new()));
        self.subscribers
            .entry(namespace.to_string())
            .or_default()
            .push(queue.clone());
        queue
    }

    /// Notify all subscribers for a namespace.
    fn notify_subscribers(&self, namespace: &str, change: &ConfigChange) {
        if let Some(mut subs) = self.subscribers.get_mut(namespace) {
            subs.retain(|queue| {
                if let Ok(mut q) = queue.write() {
                    q.push(change.clone());
                    true
                } else {
                    false // remove dead subscribers
                }
            });
        }
    }

    // ── Secret operations ─────────────────────────────────────────

    /// Store a secret, sealed with XChaCha20-Poly1305 and bound to its slot.
    pub fn set_secret(
        &self,
        namespace: &str,
        key: &str,
        value: &[u8],
        scoped_to: Vec<Pid>,
    ) -> Result<(), KernelError> {
        // Governance gate: secret storage is a critical action.
        #[cfg(feature = "exochain")]
        if let Some(ref gate) = self.governance_gate {
            use crate::gate::GateDecision;
            use crate::governance::{GateEffectKind, with_effect_context};
            let ctx = with_effect_context(
                serde_json::json!({
                    "namespace": namespace,
                    "key": key,
                }),
                &GateEffectKind::ConfigSecretSet.effect_vector(),
            );
            let decision = gate.check("config-service", "config.secret.set", &ctx);
            if let GateDecision::Deny { reason, .. } = decision {
                return Err(KernelError::GovernanceDenied(format!(
                    "secret set denied: {reason}"
                )));
            }
        }

        let secret_key = format!("{namespace}/{key}");

        let sealed = self
            .seal
            .seal(secret_key.as_bytes(), value)
            .map_err(|e| KernelError::Service(e.to_string()))?;
        self.secrets.insert(secret_key.clone(), sealed);

        let secret_ref = SecretRef {
            namespace: namespace.to_string(),
            key: key.to_string(),
            expires_at: Utc::now() + Duration::hours(24),
            scoped_to,
        };
        self.secret_refs.insert(secret_key, secret_ref);

        #[cfg(feature = "exochain")]
        if let Some(ref cm) = self.chain_manager {
            cm.append(
                "config",
                crate::chain::EVENT_KIND_CONFIG_SECRET_SET,
                Some(serde_json::json!({
                    "namespace": namespace,
                    "key": key,
                })),
            );
        }

        Ok(())
    }

    /// Retrieve a secret (opened). Checks PID authorization and expiry, and
    /// fails if the sealed value was tampered with or moved between slots.
    pub fn get_secret(
        &self,
        namespace: &str,
        key: &str,
        requester_pid: Pid,
    ) -> Result<Vec<u8>, KernelError> {
        let secret_key = format!("{namespace}/{key}");

        let secret_ref = self
            .secret_refs
            .get(&secret_key)
            .ok_or_else(|| KernelError::Service("secret not found".into()))?;

        // Check authorization.
        if !secret_ref.scoped_to.is_empty() && !secret_ref.scoped_to.contains(&requester_pid) {
            return Err(KernelError::CapabilityDenied {
                pid: requester_pid,
                action: "read_secret".into(),
                reason: format!(
                    "PID {} not authorized for secret {secret_key}",
                    requester_pid
                ),
            });
        }

        // Check expiry.
        if Utc::now() > secret_ref.expires_at {
            return Err(KernelError::Service("secret expired".into()));
        }

        let sealed = self
            .secrets
            .get(&secret_key)
            .ok_or_else(|| KernelError::Service("secret data missing".into()))?;

        self.seal
            .open(secret_key.as_bytes(), &sealed)
            .map_err(|e| KernelError::Service(e.to_string()))
    }

    /// Get the change log (for auditing).
    pub fn change_log(&self) -> Vec<ConfigChange> {
        self.change_log
            .read()
            .map(|l| l.clone())
            .unwrap_or_default()
    }

    /// Total number of config sets performed.
    pub fn set_count(&self) -> u64 {
        self.set_count.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl SystemService for ConfigService {
    fn name(&self) -> &str {
        "config-service"
    }

    fn service_type(&self) -> ServiceType {
        ServiceType::Core
    }

    async fn start(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        info!("config service started");
        Ok(())
    }

    async fn stop(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        info!(
            configs = self.configs.len(),
            secrets = self.secrets.len(),
            "config service stopped"
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

    #[test]
    fn set_and_get_config() {
        let svc = ConfigService::new_default();
        svc.set("app", "timeout", serde_json::json!(30), pid(1))
            .unwrap();
        let val = svc.get("app", "timeout").unwrap();
        assert_eq!(val, serde_json::json!(30));
    }

    #[test]
    fn get_nonexistent_returns_none() {
        let svc = ConfigService::new_default();
        assert!(svc.get("app", "missing").is_none());
    }

    #[test]
    fn delete_config() {
        let svc = ConfigService::new_default();
        svc.set("app", "key", serde_json::json!("val"), pid(1))
            .unwrap();
        svc.delete("app", "key", pid(1)).unwrap();
        assert!(svc.get("app", "key").is_none());
    }

    #[test]
    fn list_keys_in_namespace() {
        let svc = ConfigService::new_default();
        svc.set("ns", "a", serde_json::json!(1), pid(1)).unwrap();
        svc.set("ns", "b", serde_json::json!(2), pid(1)).unwrap();
        svc.set("other", "c", serde_json::json!(3), pid(1)).unwrap();
        let mut keys = svc.list_keys("ns");
        keys.sort();
        assert_eq!(keys, vec!["a", "b"]);
    }

    #[test]
    fn config_change_notification() {
        let svc = ConfigService::new_default();
        let sub = svc.subscribe("watch");
        svc.set("watch", "flag", serde_json::json!(true), pid(1))
            .unwrap();
        let changes = sub.read().unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].key, "flag");
        assert_eq!(changes[0].new_value, Some(serde_json::json!(true)));
    }

    #[test]
    fn config_change_includes_old_value() {
        let svc = ConfigService::new_default();
        let sub = svc.subscribe("ver");
        svc.set("ver", "v", serde_json::json!(1), pid(1)).unwrap();
        svc.set("ver", "v", serde_json::json!(2), pid(1)).unwrap();
        let changes = sub.read().unwrap();
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[1].old_value, Some(serde_json::json!(1)));
        assert_eq!(changes[1].new_value, Some(serde_json::json!(2)));
    }

    #[test]
    fn secret_set_and_get() {
        let key = [0xAB; 32];
        let svc = ConfigService::new(key);
        svc.set_secret("creds", "api_key", b"secret123", vec![pid(1)])
            .unwrap();
        let val = svc.get_secret("creds", "api_key", pid(1)).unwrap();
        assert_eq!(val, b"secret123");
    }

    #[test]
    fn secret_stored_sealed_not_plaintext() {
        let key = [0xAB; 32];
        let svc = ConfigService::new(key);
        svc.set_secret("creds", "pass", b"plaintext", vec![pid(1)])
            .unwrap();
        // Verify stored data is not plaintext.
        let stored = svc.secrets.get("creds/pass").unwrap();
        assert_ne!(stored.as_slice(), b"plaintext");
    }

    #[test]
    fn unauthorized_pid_cannot_read_secret() {
        let svc = ConfigService::new_default();
        svc.set_secret("creds", "key", b"val", vec![pid(1)])
            .unwrap();
        let result = svc.get_secret("creds", "key", pid(99));
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("denied") || err.contains("authorized"),
            "got: {err}"
        );
    }

    #[test]
    fn empty_scope_allows_any_pid() {
        let svc = ConfigService::new_default();
        svc.set_secret("open", "key", b"val", vec![]).unwrap();
        let val = svc.get_secret("open", "key", pid(42)).unwrap();
        assert_eq!(val, b"val");
    }

    #[test]
    fn change_log_recorded() {
        let svc = ConfigService::new_default();
        svc.set("ns", "k", serde_json::json!("v"), pid(1)).unwrap();
        let log = svc.change_log();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].namespace, "ns");
    }

    #[tokio::test]
    async fn system_service_impl() {
        let svc = ConfigService::new_default();
        assert_eq!(svc.name(), "config-service");
        assert_eq!(svc.service_type(), ServiceType::Core);
        svc.start().await.unwrap();
        assert_eq!(svc.health_check().await, HealthStatus::Healthy);
        svc.stop().await.unwrap();
    }

    // ── Typed config value tests ─────────────────────────────────

    #[test]
    fn typed_set_get_roundtrip() {
        let svc = ConfigService::new_default();
        svc.set_typed("app", "name", ConfigValue::Text("myapp".into()), pid(1))
            .unwrap();
        let entry = svc.get_typed("app", "name").unwrap();
        assert_eq!(entry.value, ConfigValue::Text("myapp".into()));
        assert_eq!(entry.namespace, "app");
        assert_eq!(entry.key, "name");
    }

    #[test]
    fn typed_get_nonexistent_returns_none() {
        let svc = ConfigService::new_default();
        assert!(svc.get_typed("app", "missing").is_none());
    }

    #[test]
    fn typed_list_returns_all_in_namespace() {
        let svc = ConfigService::new_default();
        svc.set_typed("db", "host", ConfigValue::Text("localhost".into()), pid(1))
            .unwrap();
        svc.set_typed("db", "port", ConfigValue::Integer(5432), pid(1))
            .unwrap();
        svc.set_typed("cache", "ttl", ConfigValue::Integer(60), pid(1))
            .unwrap();

        let mut entries = svc.list("db");
        entries.sort_by(|a, b| a.key.cmp(&b.key));
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].key, "host");
        assert_eq!(entries[1].key, "port");
    }

    #[test]
    fn typed_delete_removes_entry() {
        let svc = ConfigService::new_default();
        svc.set_typed("ns", "k", ConfigValue::Boolean(true), pid(1))
            .unwrap();
        assert!(svc.delete_typed("ns", "k", pid(1)).unwrap());
        assert!(svc.get_typed("ns", "k").is_none());
        // Second delete returns false.
        assert!(!svc.delete_typed("ns", "k", pid(1)).unwrap());
    }

    #[test]
    fn typed_subscribe_receives_change_notification() {
        let svc = ConfigService::new_default();
        let sub = svc.subscribe("typed-ns");
        svc.set_typed("typed-ns", "flag", ConfigValue::Boolean(true), pid(1))
            .unwrap();
        let changes = sub.read().unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].key, "flag");
        assert_eq!(changes[0].new_value, Some(serde_json::json!(true)));
    }

    #[test]
    fn typed_set_updates_existing_value() {
        let svc = ConfigService::new_default();
        svc.set_typed("app", "level", ConfigValue::Integer(1), pid(1))
            .unwrap();
        svc.set_typed("app", "level", ConfigValue::Integer(2), pid(1))
            .unwrap();
        let entry = svc.get_typed("app", "level").unwrap();
        assert_eq!(entry.value, ConfigValue::Integer(2));
    }

    #[test]
    fn typed_namespace_isolation() {
        let svc = ConfigService::new_default();
        svc.set_typed("alpha", "key", ConfigValue::Text("a".into()), pid(1))
            .unwrap();
        svc.set_typed("beta", "key", ConfigValue::Text("b".into()), pid(1))
            .unwrap();

        let a = svc.get_typed("alpha", "key").unwrap();
        let b = svc.get_typed("beta", "key").unwrap();
        assert_eq!(a.value, ConfigValue::Text("a".into()));
        assert_eq!(b.value, ConfigValue::Text("b".into()));

        // list returns only matching namespace.
        assert_eq!(svc.list("alpha").len(), 1);
        assert_eq!(svc.list("beta").len(), 1);
        assert_eq!(svc.list("gamma").len(), 0);
    }

    #[test]
    fn typed_all_value_variants() {
        let svc = ConfigService::new_default();

        svc.set_typed("t", "text", ConfigValue::Text("hello".into()), pid(1))
            .unwrap();
        svc.set_typed("t", "int", ConfigValue::Integer(42), pid(1))
            .unwrap();
        svc.set_typed("t", "float", ConfigValue::Float(2.5), pid(1))
            .unwrap();
        svc.set_typed("t", "bool", ConfigValue::Boolean(false), pid(1))
            .unwrap();
        svc.set_typed(
            "t",
            "json",
            ConfigValue::Json(serde_json::json!({"a": 1})),
            pid(1),
        )
        .unwrap();

        assert_eq!(
            svc.get_typed("t", "text").unwrap().value,
            ConfigValue::Text("hello".into())
        );
        assert_eq!(
            svc.get_typed("t", "int").unwrap().value,
            ConfigValue::Integer(42)
        );
        assert_eq!(
            svc.get_typed("t", "float").unwrap().value,
            ConfigValue::Float(2.5)
        );
        assert_eq!(
            svc.get_typed("t", "bool").unwrap().value,
            ConfigValue::Boolean(false)
        );
        assert_eq!(
            svc.get_typed("t", "json").unwrap().value,
            ConfigValue::Json(serde_json::json!({"a": 1}))
        );
    }
    // ── card a134bcf7: secrets are sealed with an AEAD, not keyed XOR ──

    #[test]
    fn secret_same_plaintext_seals_differently() {
        let svc = ConfigService::new([0xAB; 32]);
        svc.set_secret("creds", "a", b"same-value", vec![]).unwrap();
        svc.set_secret("creds", "b", b"same-value", vec![]).unwrap();
        let a = svc.secrets.get("creds/a").unwrap().clone();
        let b = svc.secrets.get("creds/b").unwrap().clone();
        assert_ne!(a, b, "a fresh nonce per seal must change the ciphertext");
        svc.set_secret("creds", "a", b"same-value", vec![]).unwrap();
        assert_ne!(*svc.secrets.get("creds/a").unwrap(), a);
    }

    #[test]
    fn every_flipped_secret_byte_fails_to_open() {
        let svc = ConfigService::new([0xAB; 32]);
        svc.set_secret("creds", "k", b"tamper-me", vec![]).unwrap();
        let original = svc.secrets.get("creds/k").unwrap().clone();
        for i in 0..original.len() {
            let mut tampered = original.clone();
            tampered[i] ^= 0x01;
            svc.secrets.insert("creds/k".into(), tampered);
            assert!(
                svc.get_secret("creds", "k", pid(1)).is_err(),
                "flipping byte {i} must be detected"
            );
        }
        svc.secrets.insert("creds/k".into(), original);
        assert_eq!(svc.get_secret("creds", "k", pid(1)).unwrap(), b"tamper-me");
    }

    #[test]
    fn truncated_secret_fails_to_open() {
        let svc = ConfigService::new([0xAB; 32]);
        svc.set_secret("creds", "k", b"short", vec![]).unwrap();
        let original = svc.secrets.get("creds/k").unwrap().clone();
        for len in [0, 1, original.len() - 1] {
            svc.secrets.insert("creds/k".into(), original[..len].to_vec());
            assert!(svc.get_secret("creds", "k", pid(1)).is_err(), "len {len}");
        }
    }

    #[test]
    fn secret_sealed_under_other_key_does_not_open() {
        let a = ConfigService::new([0x11; 32]);
        let b = ConfigService::new([0x22; 32]);
        a.set_secret("creds", "k", b"cross-key", vec![]).unwrap();
        b.set_secret("creds", "k", b"placeholder", vec![]).unwrap();
        let sealed = a.secrets.get("creds/k").unwrap().clone();
        b.secrets.insert("creds/k".into(), sealed);
        assert!(b.get_secret("creds", "k", pid(1)).is_err());
    }

    #[test]
    fn sealed_secret_is_bound_to_its_slot() {
        let svc = ConfigService::new([0xAB; 32]);
        svc.set_secret("creds", "x", b"value-x", vec![]).unwrap();
        svc.set_secret("creds", "y", b"value-y", vec![]).unwrap();
        let x = svc.secrets.get("creds/x").unwrap().clone();
        svc.secrets.insert("creds/y".into(), x);
        assert!(
            svc.get_secret("creds", "y", pid(1)).is_err(),
            "a sealed value moved to another slot must not open"
        );
    }

    #[test]
    fn default_services_use_distinct_random_keys() {
        let a = ConfigService::new_default();
        let b = ConfigService::new_default();
        a.set_secret("creds", "k", b"per-instance", vec![]).unwrap();
        b.set_secret("creds", "k", b"placeholder", vec![]).unwrap();
        let sealed = a.secrets.get("creds/k").unwrap().clone();
        b.secrets.insert("creds/k".into(), sealed);
        assert!(
            b.get_secret("creds", "k", pid(1)).is_err(),
            "new_default must not share a fixed (all-zero) key"
        );
    }

    #[test]
    fn no_keyed_xor_left_in_config_service() {
        let src = include_str!("config_service.rs");
        assert!(!src.contains(concat!("xor", "_encrypt")));
        assert!(!src.contains(concat!("xor", "_decrypt")));
    }
}

#[cfg(test)]
#[path = "config_service_slot_tests.rs"]
mod slot_tests;
