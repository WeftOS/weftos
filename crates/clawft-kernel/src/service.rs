//! System service registry and lifecycle management.
//!
//! The [`ServiceRegistry`] manages named services that implement the
//! [`SystemService`] trait, providing start/stop lifecycle and health
//! check aggregation.

use std::sync::Arc;

use async_trait::async_trait;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use chrono::{DateTime, Utc};

use crate::health::HealthStatus;
use crate::process::Pid;

/// Type of system service.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ServiceType {
    /// Core kernel service (message bus, process table, etc.).
    Core,
    /// Plugin-provided service.
    Plugin,
    /// Cron/scheduler service.
    Cron,
    /// API/HTTP service.
    Api,
    /// Custom service with a user-defined label.
    Custom(String),
}

impl std::fmt::Display for ServiceType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServiceType::Core => write!(f, "core"),
            ServiceType::Plugin => write!(f, "plugin"),
            ServiceType::Cron => write!(f, "cron"),
            ServiceType::Api => write!(f, "api"),
            ServiceType::Custom(s) => write!(f, "custom({s})"),
        }
    }
}

// ── Service identity model (D1, D9, D19) ───────────────────────

/// How to reach a service at runtime.
///
/// A service can be backed by an in-kernel agent (most common),
/// an external system (Redis, HTTP endpoint), or a container.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ServiceEndpoint {
    /// Backed by an in-kernel agent (route to its inbox).
    AgentInbox(Pid),
    /// Backed by an external system (K3/K4 ServiceApi adapter required).
    External {
        /// URL or connection string for the external system.
        url: String,
    },
    /// Backed by a managed container (K4).
    Container {
        /// Container identifier.
        id: String,
    },
}

/// Audit level for service call witnessing (D9).
///
/// Controls how much of a service's activity is recorded in the
/// ExoChain. The default is `Full` -- every call is witnessed.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum ServiceAuditLevel {
    /// Witness every call (default, per D9).
    #[default]
    Full,
    /// Only log governance gate decisions (opt-out for high-frequency services).
    GateOnly,
}

/// First-class service identity in the registry (D1).
///
/// A `ServiceEntry` is metadata about a service -- who owns it, how
/// to reach it, and how deeply to audit it. It lives alongside (not
/// replacing) the `SystemService` trait implementations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceEntry {
    /// Service name (unique within the registry).
    pub name: String,
    /// PID of the agent that owns this service (`None` for external services).
    pub owner_pid: Option<Pid>,
    /// How to reach the service at runtime.
    pub endpoint: ServiceEndpoint,
    /// Audit depth for ExoChain witnessing.
    pub audit_level: ServiceAuditLevel,
    /// When this entry was registered.
    pub registered_at: DateTime<Utc>,
}

/// A system service managed by the kernel.
///
/// Services are started during boot and stopped during shutdown.
/// Each service provides a health check for monitoring.
#[async_trait]
pub trait SystemService: Send + Sync {
    /// Human-readable service name.
    fn name(&self) -> &str;

    /// Service type category.
    fn service_type(&self) -> ServiceType;

    /// Start the service.
    async fn start(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// Stop the service.
    async fn stop(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// Perform a health check.
    async fn health_check(&self) -> HealthStatus;

    /// Optional operator-facing detail (metrics, endpoints, …).
    ///
    /// Surfaced next to health in `kernel.services` / `weft status`
    /// without changing the Healthy/Degraded/Unhealthy classification.
    /// Default: `None` (no extra detail). WEFT-333.
    fn health_detail(&self) -> Option<String> {
        None
    }

    /// Liveness probe (K2b-G2, os-patterns).
    ///
    /// Returns whether the service process is alive. Default implementation
    /// returns `Live` for backward compatibility.
    #[cfg(feature = "os-patterns")]
    async fn liveness_check(&self) -> crate::health::ProbeResult {
        crate::health::ProbeResult::Live
    }

    /// Readiness probe (K2b-G2, os-patterns).
    ///
    /// Returns whether the service is ready to accept traffic. Default
    /// implementation returns `Ready` for backward compatibility.
    #[cfg(feature = "os-patterns")]
    async fn readiness_check(&self) -> crate::health::ProbeResult {
        crate::health::ProbeResult::Ready
    }
}

/// Longest one service's `stop()` may take during [`ServiceRegistry::stop_all`].
pub const SERVICE_STOP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// `Some(result)` when `stop()` finished in time, `None` when abandoned.
#[cfg(feature = "native")]
async fn bounded_stop(
    service: Arc<dyn SystemService>,
) -> Option<Result<(), Box<dyn std::error::Error + Send + Sync>>> {
    tokio::time::timeout(SERVICE_STOP_TIMEOUT, service.stop()).await.ok()
}

#[cfg(not(feature = "native"))]
async fn bounded_stop(
    service: Arc<dyn SystemService>,
) -> Option<Result<(), Box<dyn std::error::Error + Send + Sync>>> {
    Some(service.stop().await)
}

#[cfg(feature = "native")]
use futures::future::join_all;

/// Without `native` (no `futures`): the same contract, one at a time.
#[cfg(not(feature = "native"))]
async fn join_all<F: std::future::Future<Output = ()>>(futs: impl IntoIterator<Item = F>) {
    for f in futs {
        f.await;
    }
}

/// Registry of system services with lifecycle management.
///
/// Uses [`DashMap`] for concurrent access from multiple kernel
/// subsystems. Maintains two maps:
///
/// - `services`: `SystemService` trait object implementations (existing)
/// - `entries`: `ServiceEntry` metadata for service identity (D1, K2.1)
///
/// A service can have metadata before it has a running implementation
/// (useful for external services), and vice versa.
pub struct ServiceRegistry {
    services: DashMap<String, Arc<dyn SystemService>>,
    entries: DashMap<String, ServiceEntry>,
}

impl ServiceRegistry {
    /// Create a new, empty service registry.
    pub fn new() -> Self {
        Self {
            services: DashMap::new(),
            entries: DashMap::new(),
        }
    }

    /// Register a service.
    ///
    /// Returns an error if a service with the same name is already
    /// registered.
    pub fn register(
        &self,
        service: Arc<dyn SystemService>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let name = service.name().to_owned();
        if self.services.contains_key(&name) {
            return Err(format!("service already registered: {name}").into());
        }
        info!(service = %name, "registering service");
        self.services.insert(name, service);
        Ok(())
    }

    /// Unregister a service by name.
    pub fn unregister(&self, name: &str) -> Option<Arc<dyn SystemService>> {
        self.services.remove(name).map(|(_, s)| s)
    }

    /// Get a service by name.
    pub fn get(&self, name: &str) -> Option<Arc<dyn SystemService>> {
        self.services.get(name).map(|s| s.value().clone())
    }

    /// List all registered services with their types.
    pub fn list(&self) -> Vec<(String, ServiceType)> {
        self.services
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().service_type()))
            .collect()
    }

    /// Start all registered services.
    ///
    /// Individual service failures are logged as warnings but do not
    /// prevent other services from starting.
    pub async fn start_all(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // A snapshot, never a live iterator: no DashMap guard across an await.
        for (name, service) in self.snapshot() {
            info!(service = %name, "starting service");
            if let Err(e) = service.start().await {
                warn!(service = %name, error = %e, "service failed to start");
            }
        }
        Ok(())
    }

    /// Stop all registered services.
    ///
    /// Services stop concurrently (registry order was never defined), each
    /// bounded by [`SERVICE_STOP_TIMEOUT`] on native builds, so one stuck
    /// service cannot hold up kernel shutdown. Failures and timeouts are
    /// logged as warnings.
    pub async fn stop_all(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let stops = self.snapshot().into_iter().map(|(name, service)| async move {
            info!(service = %name, "stopping service");
            match bounded_stop(service).await {
                Some(Ok(())) => info!(service = %name, "service stopped"),
                Some(Err(e)) => warn!(service = %name, error = %e, "service failed to stop"),
                None => warn!(
                    service = %name,
                    timeout_s = SERVICE_STOP_TIMEOUT.as_secs(),
                    "service did not stop in time; abandoned"
                ),
            }
        });
        join_all(stops).await;
        Ok(())
    }

    /// Return a snapshot of all services as a `Vec`.
    ///
    /// This copies all `(name, Arc<dyn SystemService>)` pairs out of
    /// the `DashMap`, so the returned collection owns no DashMap refs
    /// and is safe to hold across await points and send across threads.
    pub fn snapshot(&self) -> Vec<(String, Arc<dyn SystemService>)> {
        self.services
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().clone()))
            .collect()
    }

    /// Run health checks on all registered services.
    pub async fn health_all(&self) -> Vec<(String, HealthStatus)> {
        let mut results = Vec::new();
        for (name, service) in self.snapshot() {
            let status = service.health_check().await;
            results.push((name, status));
        }
        results
    }

    // ── ServiceEntry metadata (D1, K2.1) ──────────────────────────

    /// Register a service entry (metadata, not a running implementation).
    ///
    /// Returns an error if an entry with the same name already exists.
    pub fn register_entry(
        &self,
        entry: ServiceEntry,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let name = entry.name.clone();
        if self.entries.contains_key(&name) {
            return Err(format!("service entry already registered: {name}").into());
        }
        info!(service = %name, "registering service entry");
        self.entries.insert(name, entry);
        Ok(())
    }

    /// Get a service entry by name.
    pub fn get_entry(&self, name: &str) -> Option<ServiceEntry> {
        self.entries.get(name).map(|e| e.value().clone())
    }

    /// Resolve a service name to its owning agent PID.
    ///
    /// Returns `None` if the service is not registered or has no
    /// `owner_pid` (e.g. external services).
    pub fn resolve_target(&self, name: &str) -> Option<Pid> {
        self.entries.get(name).and_then(|e| e.value().owner_pid)
    }

    /// List all registered service entries.
    pub fn list_entries(&self) -> Vec<ServiceEntry> {
        self.entries.iter().map(|e| e.value().clone()).collect()
    }

    /// Remove a service entry by name.
    pub fn unregister_entry(&self, name: &str) -> Option<ServiceEntry> {
        self.entries.remove(name).map(|(_, e)| e)
    }

    /// Register a service and create a resource tree node + chain event.
    ///
    /// When the exochain feature is enabled and a tree manager is provided,
    /// creates a node at `/kernel/services/{name}` in the resource tree
    /// and appends a corresponding chain event via `TreeManager`.
    #[cfg(feature = "exochain")]
    pub fn register_with_tree(
        &self,
        service: Arc<dyn SystemService>,
        tree_manager: &crate::tree_manager::TreeManager,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let name = service.name().to_owned();
        self.register(service)?;

        // Create tree node + chain event through the unified TreeManager path
        if let Err(e) = tree_manager.register_service(&name) {
            tracing::debug!(service = %name, error = %e, "failed to register service in tree");
        }

        Ok(())
    }

    /// Get the number of registered services.
    pub fn len(&self) -> usize {
        self.services.len()
    }

    /// Check whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.services.is_empty()
    }

    /// Register a service and automatically create a chain-anchored contract (C3).
    ///
    /// This is the recommended registration path for K4+ services that want
    /// immutable API contracts stored on the ExoChain. It combines service
    /// registration, contract creation, and chain logging in one call.
    #[cfg(feature = "exochain")]
    pub fn register_with_contract(
        &self,
        service: Arc<dyn SystemService>,
        methods: Vec<String>,
        chain: &crate::chain::ChainManager,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let name = service.name().to_owned();
        let stype = service.service_type().to_string();

        // Register the service first
        self.register(service)?;

        // Build canonical contract content and hash it
        let contract_content = serde_json::json!({
            "service": &name,
            "type": &stype,
            "methods": &methods,
        });
        let content_hash = {
            use sha2::{Digest, Sha256};
            let bytes = serde_json::to_string(&contract_content).unwrap_or_default();
            format!("{:x}", Sha256::digest(bytes.as_bytes()))
        };

        let contract = ServiceContract {
            service_name: name,
            version: "1.0.0".into(),
            methods,
            content_hash,
        };

        self.register_contract(&contract, chain)?;

        Ok(())
    }

    /// Register a service contract and log it to the chain (K2 C3, K4 G2).
    ///
    /// A service contract is a versioned interface declaration that is
    /// anchored in the ExoChain for immutability. Once a contract version
    /// is registered, it cannot be changed — only superseded by a new version.
    #[cfg(feature = "exochain")]
    pub fn register_contract(
        &self,
        contract: &ServiceContract,
        chain: &crate::chain::ChainManager,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        info!(
            service = %contract.service_name,
            version = %contract.version,
            "registering service contract on chain"
        );
        chain.append(
            &contract.service_name,
            "service.contract.register",
            Some(serde_json::json!({
                "service": contract.service_name,
                "version": contract.version,
                "methods": contract.methods,
                "hash": contract.content_hash,
            })),
        );
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// K4 G2: Service contracts (K2 C3)
// ---------------------------------------------------------------------------

/// A versioned service contract anchored in the ExoChain.
///
/// Contracts declare the interface a service exposes. Once registered
/// on-chain, a contract version is immutable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceContract {
    /// Name of the service this contract belongs to.
    pub service_name: String,
    /// Semantic version string (e.g. "1.0.0").
    pub version: String,
    /// Method names exposed by this contract version.
    pub methods: Vec<String>,
    /// SHAKE-256 hash of the canonical contract content.
    pub content_hash: String,
}

impl Default for ServiceRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ── ServiceApi trait (K3 C2) ─────────────────────────────────────

/// Internal API surface for protocol adapters (Shell, MCP, HTTP).
///
/// Protocol adapters bind to this trait to invoke kernel services
/// through a uniform interface. The kernel provides a concrete
/// implementation backed by the ServiceRegistry + A2ARouter.
#[async_trait]
pub trait ServiceApi: Send + Sync {
    /// Call a method on a named service.
    async fn call(
        &self,
        service: &str,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>>;

    /// List available services.
    async fn list_services(&self) -> Vec<ServiceInfo>;

    /// Get service health.
    async fn health(
        &self,
        service: &str,
    ) -> Result<HealthStatus, Box<dyn std::error::Error + Send + Sync>>;
}

/// Service info returned by [`ServiceApi::list_services`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceInfo {
    /// Service name.
    pub name: String,
    /// Service type label (e.g. "core", "plugin").
    pub service_type: String,
    /// Whether the service is currently healthy.
    pub healthy: bool,
}

/// Shell protocol adapter -- dispatches shell commands through [`ServiceApi`].
pub struct ShellAdapter {
    api: Arc<dyn ServiceApi>,
}

impl ShellAdapter {
    /// Create a new shell adapter bound to the given service API.
    pub fn new(api: Arc<dyn ServiceApi>) -> Self {
        Self { api }
    }

    /// Execute a shell-style command string through the service API.
    ///
    /// Parses `"service.method arg1 arg2"` format into a
    /// [`ServiceApi::call`].
    pub async fn execute(
        &self,
        command: &str,
    ) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
        let parts: Vec<&str> = command.splitn(2, ' ').collect();
        let (service_method, args_str) = match parts.as_slice() {
            [sm] => (*sm, ""),
            [sm, args] => (*sm, *args),
            _ => return Err("empty command".into()),
        };

        let (service, method) = service_method
            .split_once('.')
            .ok_or_else(|| format!("expected 'service.method', got '{service_method}'"))?;

        let params = if args_str.is_empty() {
            serde_json::Value::Null
        } else if args_str.starts_with('{') || args_str.starts_with('[') {
            serde_json::from_str(args_str)?
        } else {
            serde_json::json!({"args": args_str})
        };

        self.api.call(service, method, params).await
    }
}

/// MCP protocol adapter -- dispatches MCP tool calls through [`ServiceApi`].
pub struct McpAdapter {
    api: Arc<dyn ServiceApi>,
}

impl McpAdapter {
    /// Create a new MCP adapter bound to the given service API.
    pub fn new(api: Arc<dyn ServiceApi>) -> Self {
        Self { api }
    }

    /// Handle an MCP `tool_call` by routing through the service API.
    ///
    /// MCP tool names map to `service.method` via either underscore or
    /// dot separator (e.g. `"kernel_status"` -> `("kernel", "status")`).
    pub async fn handle_tool_call(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
        let (service, method) = tool_name
            .split_once('_')
            .or_else(|| tool_name.split_once('.'))
            .ok_or_else(|| format!("invalid tool name format: {tool_name}"))?;

        self.api.call(service, method, arguments).await
    }

    /// List available tools (mapped from services).
    pub async fn list_tools(&self) -> Vec<serde_json::Value> {
        let services = self.api.list_services().await;
        services
            .into_iter()
            .map(|s| {
                serde_json::json!({
                    "name": s.name,
                    "description": format!("WeftOS {} service", s.service_type),
                })
            })
            .collect()
    }
}

// ── Concrete ServiceApi backed by ServiceRegistry ──────────────────

/// Concrete [`ServiceApi`] implementation backed by a [`ServiceRegistry`].
///
/// Routes `call()` requests to the registered [`SystemService`] by name,
/// maps `list_services()` to the registry snapshot, and delegates
/// `health()` to each service's health check.
///
/// This is the production implementation that protocol adapters
/// (Shell, MCP, HTTP) hold via `Arc<dyn ServiceApi>`.
pub struct KernelServiceApi {
    registry: Arc<ServiceRegistry>,
}

impl KernelServiceApi {
    /// Create a new kernel service API backed by the given registry.
    pub fn new(registry: Arc<ServiceRegistry>) -> Self {
        Self { registry }
    }

    /// Get a reference to the underlying registry.
    pub fn registry(&self) -> &ServiceRegistry {
        &self.registry
    }
}

#[async_trait]
impl ServiceApi for KernelServiceApi {
    async fn call(
        &self,
        service: &str,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
        // Verify the service exists in the registry.
        let _svc = self
            .registry
            .get(service)
            .ok_or_else(|| format!("service not found: {service}"))?;

        // Build a JSON envelope describing the call. In a full integration
        // this would dispatch to the service's message inbox or method table.
        // For now, return an acknowledgement so the protocol wire path is
        // exercisable end-to-end.
        Ok(serde_json::json!({
            "service": service,
            "method": method,
            "params": params,
            "status": "dispatched",
        }))
    }

    async fn list_services(&self) -> Vec<ServiceInfo> {
        // Use snapshot() to avoid holding DashMap refs across the await.
        let snapshot = self.registry.snapshot();
        let mut infos = Vec::with_capacity(snapshot.len());
        for (name, svc) in &snapshot {
            let health = svc.health_check().await;
            infos.push(ServiceInfo {
                name: name.clone(),
                service_type: svc.service_type().to_string(),
                healthy: health == HealthStatus::Healthy,
            });
        }
        infos
    }

    async fn health(
        &self,
        service: &str,
    ) -> Result<HealthStatus, Box<dyn std::error::Error + Send + Sync>> {
        let svc = self
            .registry
            .get(service)
            .ok_or_else(|| format!("service not found: {service}"))?;
        Ok(svc.health_check().await)
    }
}

// ── Registry trait implementation ────────────────────────────────────

impl clawft_types::Registry for ServiceRegistry {
    type Value = Arc<dyn SystemService>;

    fn get(&self, key: &str) -> Option<Self::Value> {
        self.services.get(key).map(|s| s.value().clone())
    }

    fn list_keys(&self) -> Vec<String> {
        self.services.iter().map(|e| e.key().clone()).collect()
    }

    fn contains(&self, key: &str) -> bool {
        self.services.contains_key(key)
    }

    fn count(&self) -> usize {
        self.services.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A mock service for testing.
    struct MockService {
        name: String,
        service_type: ServiceType,
    }

    impl MockService {
        fn new(name: &str, stype: ServiceType) -> Self {
            Self {
                name: name.to_owned(),
                service_type: stype,
            }
        }
    }

    #[async_trait]
    impl SystemService for MockService {
        fn name(&self) -> &str {
            &self.name
        }

        fn service_type(&self) -> ServiceType {
            self.service_type.clone()
        }

        async fn start(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            Ok(())
        }

        async fn stop(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            Ok(())
        }

        async fn health_check(&self) -> HealthStatus {
            HealthStatus::Healthy
        }
    }

    #[test]
    fn register_and_get() {
        let registry = ServiceRegistry::new();
        let svc = Arc::new(MockService::new("test-svc", ServiceType::Core));
        registry.register(svc).unwrap();

        let retrieved = registry.get("test-svc");
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().name(), "test-svc");
    }

    #[test]
    fn register_duplicate_fails() {
        let registry = ServiceRegistry::new();
        let svc1 = Arc::new(MockService::new("dup-svc", ServiceType::Core));
        let svc2 = Arc::new(MockService::new("dup-svc", ServiceType::Plugin));

        registry.register(svc1).unwrap();
        let result = registry.register(svc2);
        assert!(result.is_err());
    }

    #[test]
    fn unregister() {
        let registry = ServiceRegistry::new();
        let svc = Arc::new(MockService::new("rm-svc", ServiceType::Core));
        registry.register(svc).unwrap();

        let removed = registry.unregister("rm-svc");
        assert!(removed.is_some());
        assert!(registry.get("rm-svc").is_none());
    }

    #[test]
    fn list_services() {
        let registry = ServiceRegistry::new();
        registry
            .register(Arc::new(MockService::new("svc-a", ServiceType::Core)))
            .unwrap();
        registry
            .register(Arc::new(MockService::new("svc-b", ServiceType::Cron)))
            .unwrap();

        let list = registry.list();
        assert_eq!(list.len(), 2);
    }

    #[tokio::test]
    async fn start_and_stop_all() {
        let registry = ServiceRegistry::new();
        registry
            .register(Arc::new(MockService::new("svc-1", ServiceType::Core)))
            .unwrap();
        registry
            .register(Arc::new(MockService::new("svc-2", ServiceType::Plugin)))
            .unwrap();

        registry.start_all().await.unwrap();
        registry.stop_all().await.unwrap();
    }

    /// `stop()` never returns, or unregisters itself from `registry` first.
    struct StuckService {
        name: String,
        registry: Option<Arc<ServiceRegistry>>,
    }

    #[async_trait]
    impl SystemService for StuckService {
        fn name(&self) -> &str {
            &self.name
        }
        fn service_type(&self) -> ServiceType {
            ServiceType::Core
        }
        async fn start(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            Ok(())
        }
        async fn stop(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            match &self.registry {
                Some(r) => {
                    r.unregister(&self.name);
                    Ok(())
                }
                None => std::future::pending().await,
            }
        }
        async fn health_check(&self) -> HealthStatus {
            HealthStatus::Healthy
        }
    }

    #[tokio::test(start_paused = true)]
    async fn stop_all_abandons_a_service_that_never_stops() {
        let registry = ServiceRegistry::new();
        registry
            .register(Arc::new(StuckService { name: "stuck".into(), registry: None }))
            .unwrap();
        registry
            .register(Arc::new(MockService::new("fine", ServiceType::Core)))
            .unwrap();
        let t = tokio::time::Instant::now();
        registry.stop_all().await.unwrap();
        assert!(t.elapsed() <= SERVICE_STOP_TIMEOUT + std::time::Duration::from_millis(100));
    }

    #[test]
    fn a_service_may_unregister_itself_while_stop_all_runs() {
        // The old loop held the registry's iterator guard across `stop()`, so
        // this write to the same map deadlocked the thread.
        let registry = Arc::new(ServiceRegistry::new());
        registry
            .register(Arc::new(StuckService { name: "self-rm".into(), registry: Some(registry.clone()) }))
            .unwrap();
        let (done, rx) = std::sync::mpsc::channel();
        let r = registry.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            let _ = done.send(rt.block_on(r.stop_all()).is_ok());
        });
        assert_eq!(rx.recv_timeout(std::time::Duration::from_secs(10)).ok(), Some(true));
        assert!(registry.get("self-rm").is_none());
    }

    #[tokio::test]
    async fn health_all() {
        let registry = ServiceRegistry::new();
        registry
            .register(Arc::new(MockService::new("svc-1", ServiceType::Core)))
            .unwrap();
        registry
            .register(Arc::new(MockService::new("svc-2", ServiceType::Plugin)))
            .unwrap();

        let health = registry.health_all().await;
        assert_eq!(health.len(), 2);
        for (_, status) in &health {
            assert_eq!(*status, HealthStatus::Healthy);
        }
    }

    #[test]
    fn len_and_is_empty() {
        let registry = ServiceRegistry::new();
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);

        registry
            .register(Arc::new(MockService::new("svc", ServiceType::Core)))
            .unwrap();
        assert!(!registry.is_empty());
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn service_type_display() {
        assert_eq!(ServiceType::Core.to_string(), "core");
        assert_eq!(ServiceType::Plugin.to_string(), "plugin");
        assert_eq!(ServiceType::Cron.to_string(), "cron");
        assert_eq!(ServiceType::Api.to_string(), "api");
        assert_eq!(
            ServiceType::Custom("webhook".into()).to_string(),
            "custom(webhook)"
        );
    }

    // ── ServiceEntry tests (K2.1 T3: D1) ───────────────────────

    #[test]
    fn register_and_get_entry() {
        let registry = ServiceRegistry::new();
        let entry = ServiceEntry {
            name: "auth".into(),
            owner_pid: Some(42),
            endpoint: ServiceEndpoint::AgentInbox(42),
            audit_level: ServiceAuditLevel::Full,
            registered_at: Utc::now(),
        };
        registry.register_entry(entry).unwrap();

        let retrieved = registry.get_entry("auth").unwrap();
        assert_eq!(retrieved.name, "auth");
        assert_eq!(retrieved.owner_pid, Some(42));
    }

    #[test]
    fn register_entry_duplicate_fails() {
        let registry = ServiceRegistry::new();
        let entry1 = ServiceEntry {
            name: "dup".into(),
            owner_pid: Some(1),
            endpoint: ServiceEndpoint::AgentInbox(1),
            audit_level: ServiceAuditLevel::Full,
            registered_at: Utc::now(),
        };
        let entry2 = ServiceEntry {
            name: "dup".into(),
            owner_pid: Some(2),
            endpoint: ServiceEndpoint::AgentInbox(2),
            audit_level: ServiceAuditLevel::Full,
            registered_at: Utc::now(),
        };
        registry.register_entry(entry1).unwrap();
        let result = registry.register_entry(entry2);
        assert!(result.is_err());
    }

    #[test]
    fn resolve_target_returns_owner_pid() {
        let registry = ServiceRegistry::new();
        let entry = ServiceEntry {
            name: "cache".into(),
            owner_pid: Some(99),
            endpoint: ServiceEndpoint::AgentInbox(99),
            audit_level: ServiceAuditLevel::Full,
            registered_at: Utc::now(),
        };
        registry.register_entry(entry).unwrap();

        assert_eq!(registry.resolve_target("cache"), Some(99));
        assert_eq!(registry.resolve_target("missing"), None);
    }

    #[test]
    fn resolve_target_external_returns_none() {
        let registry = ServiceRegistry::new();
        let entry = ServiceEntry {
            name: "redis".into(),
            owner_pid: None,
            endpoint: ServiceEndpoint::External {
                url: "redis://localhost:6379".into(),
            },
            audit_level: ServiceAuditLevel::GateOnly,
            registered_at: Utc::now(),
        };
        registry.register_entry(entry).unwrap();

        assert_eq!(registry.resolve_target("redis"), None);
    }

    #[test]
    fn unregister_entry() {
        let registry = ServiceRegistry::new();
        let entry = ServiceEntry {
            name: "temp".into(),
            owner_pid: Some(5),
            endpoint: ServiceEndpoint::AgentInbox(5),
            audit_level: ServiceAuditLevel::Full,
            registered_at: Utc::now(),
        };
        registry.register_entry(entry).unwrap();

        let removed = registry.unregister_entry("temp");
        assert!(removed.is_some());
        assert!(registry.get_entry("temp").is_none());
    }

    #[test]
    fn list_entries() {
        let registry = ServiceRegistry::new();
        registry
            .register_entry(ServiceEntry {
                name: "svc-a".into(),
                owner_pid: Some(1),
                endpoint: ServiceEndpoint::AgentInbox(1),
                audit_level: ServiceAuditLevel::Full,
                registered_at: Utc::now(),
            })
            .unwrap();
        registry
            .register_entry(ServiceEntry {
                name: "svc-b".into(),
                owner_pid: None,
                endpoint: ServiceEndpoint::Container { id: "c1".into() },
                audit_level: ServiceAuditLevel::GateOnly,
                registered_at: Utc::now(),
            })
            .unwrap();

        let list = registry.list_entries();
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn service_entry_serde_roundtrip() {
        let entry = ServiceEntry {
            name: "auth".into(),
            owner_pid: Some(42),
            endpoint: ServiceEndpoint::AgentInbox(42),
            audit_level: ServiceAuditLevel::Full,
            registered_at: Utc::now(),
        };
        let json = serde_json::to_string(&entry).unwrap();
        let restored: ServiceEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.name, "auth");
        assert_eq!(restored.owner_pid, Some(42));
        assert_eq!(restored.audit_level, ServiceAuditLevel::Full);
    }

    #[test]
    fn service_endpoint_variants_serde() {
        let endpoints = vec![
            ServiceEndpoint::AgentInbox(1),
            ServiceEndpoint::External {
                url: "https://api.example.com".into(),
            },
            ServiceEndpoint::Container {
                id: "container-abc".into(),
            },
        ];
        for ep in endpoints {
            let json = serde_json::to_string(&ep).unwrap();
            let _: ServiceEndpoint = serde_json::from_str(&json).unwrap();
        }
    }

    // ── ServiceApi tests (K3 C2) ──────────────────────

    struct MockServiceApi;

    #[async_trait]
    impl ServiceApi for MockServiceApi {
        async fn call(
            &self,
            service: &str,
            method: &str,
            _params: serde_json::Value,
        ) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
            Ok(serde_json::json!({"service": service, "method": method}))
        }
        async fn list_services(&self) -> Vec<ServiceInfo> {
            vec![ServiceInfo {
                name: "test".into(),
                service_type: "core".into(),
                healthy: true,
            }]
        }
        async fn health(
            &self,
            _service: &str,
        ) -> Result<HealthStatus, Box<dyn std::error::Error + Send + Sync>> {
            Ok(HealthStatus::Healthy)
        }
    }

    #[tokio::test]
    async fn shell_adapter_parses_command() {
        let api = Arc::new(MockServiceApi);
        let shell = ShellAdapter::new(api);
        let result = shell.execute("kernel.status").await.unwrap();
        assert_eq!(result["service"], "kernel");
        assert_eq!(result["method"], "status");
    }

    #[tokio::test]
    async fn shell_adapter_with_args() {
        let api = Arc::new(MockServiceApi);
        let shell = ShellAdapter::new(api);
        let result = shell
            .execute("agent.spawn {\"name\":\"test\"}")
            .await
            .unwrap();
        assert_eq!(result["service"], "agent");
    }

    #[tokio::test]
    async fn mcp_adapter_routes_tool_call() {
        let api = Arc::new(MockServiceApi);
        let mcp = McpAdapter::new(api);
        let result = mcp
            .handle_tool_call("kernel_status", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(result["service"], "kernel");
    }

    #[tokio::test]
    async fn mcp_adapter_list_tools() {
        let api = Arc::new(MockServiceApi);
        let mcp = McpAdapter::new(api);
        let tools = mcp.list_tools().await;
        assert_eq!(tools.len(), 1);
    }

    #[test]
    fn audit_level_default_is_full() {
        assert_eq!(ServiceAuditLevel::default(), ServiceAuditLevel::Full);
    }

    #[test]
    fn dual_registry_independent() {
        // SystemService and ServiceEntry registrations are independent
        let registry = ServiceRegistry::new();

        // Register a SystemService
        registry
            .register(Arc::new(MockService::new("both", ServiceType::Core)))
            .unwrap();

        // Register a ServiceEntry with the same name (independent map)
        registry
            .register_entry(ServiceEntry {
                name: "both".into(),
                owner_pid: Some(1),
                endpoint: ServiceEndpoint::AgentInbox(1),
                audit_level: ServiceAuditLevel::Full,
                registered_at: Utc::now(),
            })
            .unwrap();

        assert!(registry.get("both").is_some());
        assert!(registry.get_entry("both").is_some());
    }

    // --- K4 G2: Service contract tests ---

    #[test]
    fn service_contract_serde() {
        let contract = ServiceContract {
            service_name: "auth".into(),
            version: "1.0.0".into(),
            methods: vec!["login".into(), "logout".into()],
            content_hash: "abc123".into(),
        };
        let json = serde_json::to_string(&contract).unwrap();
        let restored: ServiceContract = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.service_name, "auth");
        assert_eq!(restored.methods.len(), 2);
    }

    #[test]
    #[cfg(feature = "exochain")]
    fn contract_on_chain() {
        let registry = ServiceRegistry::new();
        let chain = crate::chain::ChainManager::new(0, 1000);
        let contract = ServiceContract {
            service_name: "auth".into(),
            version: "1.0.0".into(),
            methods: vec!["login".into(), "logout".into()],
            content_hash: "abc123".into(),
        };
        registry.register_contract(&contract, &chain).unwrap();
        // Chain should have genesis + 1 contract event
        assert_eq!(chain.len(), 2);
        let tail = chain.tail(1);
        assert_eq!(tail[0].kind, "service.contract.register");
    }

    #[test]
    #[cfg(feature = "exochain")]
    fn contract_version_immutability() {
        let chain = crate::chain::ChainManager::new(0, 1000);
        let registry = ServiceRegistry::new();
        let v1 = ServiceContract {
            service_name: "auth".into(),
            version: "1.0.0".into(),
            methods: vec!["login".into()],
            content_hash: "v1hash".into(),
        };
        let v2 = ServiceContract {
            service_name: "auth".into(),
            version: "2.0.0".into(),
            methods: vec!["login".into(), "refresh".into()],
            content_hash: "v2hash".into(),
        };
        registry.register_contract(&v1, &chain).unwrap();
        registry.register_contract(&v2, &chain).unwrap();
        // Both versions recorded — chain is append-only so v1 cannot be mutated
        assert_eq!(chain.len(), 3); // genesis + v1 + v2
    }

    // --- C3: register_with_contract tests ---

    #[test]
    #[cfg(feature = "exochain")]
    fn register_with_contract_anchors_to_chain() {
        let chain = crate::chain::ChainManager::new(0, 1000);
        let registry = ServiceRegistry::new();
        let svc = Arc::new(MockService::new("api-v1", ServiceType::Api));

        registry
            .register_with_contract(
                svc,
                vec!["get".into(), "set".into(), "delete".into()],
                &chain,
            )
            .unwrap();

        // Service should be registered
        assert!(registry.get("api-v1").is_some());

        // Contract should be on chain
        let events = chain.tail(1);
        assert_eq!(events[0].kind, "service.contract.register");
    }

    #[test]
    #[cfg(feature = "exochain")]
    fn register_with_contract_duplicate_fails() {
        let chain = crate::chain::ChainManager::new(0, 1000);
        let registry = ServiceRegistry::new();
        let svc1 = Arc::new(MockService::new("dup-c3", ServiceType::Core));
        let svc2 = Arc::new(MockService::new("dup-c3", ServiceType::Core));

        registry
            .register_with_contract(svc1, vec!["ping".into()], &chain)
            .unwrap();
        // Second registration with same name should fail
        let result = registry.register_with_contract(svc2, vec!["ping".into()], &chain);
        assert!(result.is_err());
    }

    #[test]
    #[cfg(feature = "exochain")]
    fn register_with_contract_hash_deterministic() {
        let chain = crate::chain::ChainManager::new(0, 1000);
        let registry = ServiceRegistry::new();
        let svc = Arc::new(MockService::new("hash-svc", ServiceType::Plugin));

        registry
            .register_with_contract(svc, vec!["alpha".into(), "beta".into()], &chain)
            .unwrap();

        let events = chain.tail(1);
        let payload = events[0].payload.as_ref().unwrap();
        let hash = payload["hash"].as_str().unwrap();
        // Hash should be a 64-char hex string (SHA-256)
        assert_eq!(hash.len(), 64);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    // ── Sprint 09a: serde roundtrip tests ────────────────────────

    #[test]
    fn service_type_serde_roundtrip_core() {
        let st = ServiceType::Core;
        let json = serde_json::to_string(&st).unwrap();
        let restored: ServiceType = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, ServiceType::Core);
    }

    #[test]
    fn service_type_serde_roundtrip_plugin() {
        let st = ServiceType::Plugin;
        let json = serde_json::to_string(&st).unwrap();
        let restored: ServiceType = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, ServiceType::Plugin);
    }

    #[test]
    fn service_type_serde_roundtrip_custom() {
        let st = ServiceType::Custom("webhook".into());
        let json = serde_json::to_string(&st).unwrap();
        let restored: ServiceType = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, ServiceType::Custom("webhook".into()));
    }

    #[test]
    fn service_audit_level_serde_roundtrip() {
        for level in [ServiceAuditLevel::Full, ServiceAuditLevel::GateOnly] {
            let json = serde_json::to_string(&level).unwrap();
            let restored: ServiceAuditLevel = serde_json::from_str(&json).unwrap();
            assert_eq!(restored, level);
        }
    }

    #[test]
    fn service_info_serde_roundtrip() {
        let info = ServiceInfo {
            name: "cache".into(),
            service_type: "core".into(),
            healthy: true,
        };
        let json = serde_json::to_string(&info).unwrap();
        let restored: ServiceInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.name, "cache");
        assert_eq!(restored.service_type, "core");
        assert!(restored.healthy);
    }

    #[test]
    fn service_info_unhealthy_roundtrip() {
        let info = ServiceInfo {
            name: "broken".into(),
            service_type: "plugin".into(),
            healthy: false,
        };
        let json = serde_json::to_string(&info).unwrap();
        let restored: ServiceInfo = serde_json::from_str(&json).unwrap();
        assert!(!restored.healthy);
    }

    #[test]
    fn snapshot_returns_all_services() {
        let registry = ServiceRegistry::new();
        registry
            .register(Arc::new(MockService::new("svc-a", ServiceType::Core)))
            .unwrap();
        registry
            .register(Arc::new(MockService::new("svc-b", ServiceType::Plugin)))
            .unwrap();

        let snapshot = registry.snapshot();
        assert_eq!(snapshot.len(), 2);
        let names: Vec<&str> = snapshot.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"svc-a"));
        assert!(names.contains(&"svc-b"));
    }

    #[test]
    fn snapshot_is_independent_of_registry() {
        let registry = ServiceRegistry::new();
        registry
            .register(Arc::new(MockService::new("snap-svc", ServiceType::Core)))
            .unwrap();

        let snapshot = registry.snapshot();
        // Remove from registry -- snapshot should still have it
        registry.unregister("snap-svc");
        assert!(registry.get("snap-svc").is_none());
        assert_eq!(snapshot.len(), 1);
    }

    #[test]
    fn get_nonexistent_service_returns_none() {
        let registry = ServiceRegistry::new();
        assert!(registry.get("ghost").is_none());
    }

    #[test]
    fn get_nonexistent_entry_returns_none() {
        let registry = ServiceRegistry::new();
        assert!(registry.get_entry("ghost").is_none());
    }

    #[tokio::test]
    async fn shell_adapter_missing_dot_returns_error() {
        let api = Arc::new(MockServiceApi);
        let shell = ShellAdapter::new(api);
        let result = shell.execute("nodot").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn shell_adapter_json_array_args() {
        let api = Arc::new(MockServiceApi);
        let shell = ShellAdapter::new(api);
        let result = shell.execute("svc.method [1,2,3]").await.unwrap();
        assert_eq!(result["service"], "svc");
        assert_eq!(result["method"], "method");
    }

    #[tokio::test]
    async fn mcp_adapter_dot_separator() {
        let api = Arc::new(MockServiceApi);
        let mcp = McpAdapter::new(api);
        let result = mcp
            .handle_tool_call("kernel.status", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(result["service"], "kernel");
        assert_eq!(result["method"], "status");
    }

    #[tokio::test]
    async fn mcp_adapter_invalid_tool_name_returns_error() {
        let api = Arc::new(MockServiceApi);
        let mcp = McpAdapter::new(api);
        let result = mcp
            .handle_tool_call("noseparator", serde_json::json!({}))
            .await;
        assert!(result.is_err());
    }

    #[test]
    fn service_type_all_variants_serde() {
        let variants = vec![
            ServiceType::Core,
            ServiceType::Plugin,
            ServiceType::Cron,
            ServiceType::Api,
            ServiceType::Custom("special".into()),
        ];
        for v in variants {
            let json = serde_json::to_string(&v).unwrap();
            let _: ServiceType = serde_json::from_str(&json).unwrap();
        }
    }

    #[test]
    fn service_entry_external_endpoint_roundtrip() {
        let entry = ServiceEntry {
            name: "ext".into(),
            owner_pid: None,
            endpoint: ServiceEndpoint::External {
                url: "https://api.example.com/v2".into(),
            },
            audit_level: ServiceAuditLevel::GateOnly,
            registered_at: Utc::now(),
        };
        let json = serde_json::to_string(&entry).unwrap();
        let restored: ServiceEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.name, "ext");
        assert!(restored.owner_pid.is_none());
        assert_eq!(restored.audit_level, ServiceAuditLevel::GateOnly);
    }

    #[test]
    fn service_entry_container_endpoint_roundtrip() {
        let entry = ServiceEntry {
            name: "docker-svc".into(),
            owner_pid: None,
            endpoint: ServiceEndpoint::Container {
                id: "abc123".into(),
            },
            audit_level: ServiceAuditLevel::Full,
            registered_at: Utc::now(),
        };
        let json = serde_json::to_string(&entry).unwrap();
        let restored: ServiceEntry = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            restored.endpoint,
            ServiceEndpoint::Container { ref id } if id == "abc123"
        ));
    }

    // ── KernelServiceApi tests (ADR-035) ────────────────────────

    #[tokio::test]
    async fn kernel_api_call_dispatches_to_registered_service() {
        let registry = Arc::new(ServiceRegistry::new());
        registry
            .register(Arc::new(MockService::new("kernel", ServiceType::Core)))
            .unwrap();

        let api = KernelServiceApi::new(registry);
        let result = api
            .call("kernel", "status", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(result["service"], "kernel");
        assert_eq!(result["method"], "status");
        assert_eq!(result["status"], "dispatched");
    }

    #[tokio::test]
    async fn kernel_api_call_unknown_service_errors() {
        let registry = Arc::new(ServiceRegistry::new());
        let api = KernelServiceApi::new(registry);
        let result = api
            .call("nonexistent", "method", serde_json::Value::Null)
            .await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("service not found"));
    }

    #[tokio::test]
    async fn kernel_api_list_services_returns_registered() {
        let registry = Arc::new(ServiceRegistry::new());
        registry
            .register(Arc::new(MockService::new("auth", ServiceType::Core)))
            .unwrap();
        registry
            .register(Arc::new(MockService::new("cron", ServiceType::Cron)))
            .unwrap();

        let api = KernelServiceApi::new(registry);
        let list = api.list_services().await;
        assert_eq!(list.len(), 2);

        let names: Vec<&str> = list.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"auth"));
        assert!(names.contains(&"cron"));

        // MockService always returns Healthy
        for info in &list {
            assert!(info.healthy);
        }
    }

    #[tokio::test]
    async fn kernel_api_list_services_empty_registry() {
        let registry = Arc::new(ServiceRegistry::new());
        let api = KernelServiceApi::new(registry);
        let list = api.list_services().await;
        assert!(list.is_empty());
    }

    #[tokio::test]
    async fn kernel_api_health_returns_status() {
        let registry = Arc::new(ServiceRegistry::new());
        registry
            .register(Arc::new(MockService::new("cache", ServiceType::Core)))
            .unwrap();

        let api = KernelServiceApi::new(registry);
        let status = api.health("cache").await.unwrap();
        assert_eq!(status, HealthStatus::Healthy);
    }

    #[tokio::test]
    async fn kernel_api_health_unknown_service_errors() {
        let registry = Arc::new(ServiceRegistry::new());
        let api = KernelServiceApi::new(registry);
        let result = api.health("ghost").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn kernel_api_with_shell_adapter_end_to_end() {
        let registry = Arc::new(ServiceRegistry::new());
        registry
            .register(Arc::new(MockService::new("agent", ServiceType::Core)))
            .unwrap();

        let api: Arc<dyn ServiceApi> = Arc::new(KernelServiceApi::new(registry));
        let shell = ShellAdapter::new(api);
        let result = shell
            .execute("agent.spawn {\"name\":\"test\"}")
            .await
            .unwrap();
        assert_eq!(result["service"], "agent");
        assert_eq!(result["method"], "spawn");
        assert_eq!(result["status"], "dispatched");
    }

    #[tokio::test]
    async fn kernel_api_with_mcp_adapter_end_to_end() {
        let registry = Arc::new(ServiceRegistry::new());
        registry
            .register(Arc::new(MockService::new("kernel", ServiceType::Core)))
            .unwrap();

        let api: Arc<dyn ServiceApi> = Arc::new(KernelServiceApi::new(registry));
        let mcp = McpAdapter::new(api);
        let result = mcp
            .handle_tool_call("kernel_status", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(result["service"], "kernel");
        assert_eq!(result["method"], "status");
    }

    #[test]
    fn kernel_api_registry_accessor() {
        let registry = Arc::new(ServiceRegistry::new());
        registry
            .register(Arc::new(MockService::new("svc", ServiceType::Core)))
            .unwrap();

        let api = KernelServiceApi::new(registry);
        assert!(api.registry().get("svc").is_some());
    }
}
